use std::collections::BTreeMap;

use x11rb_protocol::protocol::randr::{ModeFlag, ModeInfo};

use crate::{
    bridge::x11::RANDR_OUTPUT_BASE,
    util::{OutputRect, mm},
};

/// Largest screen we will accept, matching the range reported by
/// `RRGetScreenSizeRange`. Screen bounds feed the framebuffer allocation, and a
/// client picks the screen size, so without a bound a bad request could ask us
/// to allocate an absurd framebuffer.
pub const MAX_SCREEN: u16 = 16384;

/// Entries per channel of a CRTC's gamma ramp (`RRGetCrtcGammaSize`).
pub const GAMMA_SIZE: u16 = 256;

/// Most modes listed for one output from the compositor's list (a real
/// monitor offers a few dozen; this only bounds a broken answer).
const MAX_LISTED_MODES: usize = 128;

/// A RandR screen backed by Wayland outputs.
pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub modes: Vec<Mode>,
    pub outputs: Vec<Output>,
    next_id: u32,
    pub timestamp: u32,
    pub config_timestamp: u32,
    /// The compositor's primary output (its `wl_name`), 0 if it has none or
    /// says none; the first output stands in then.
    pub primary: u32,
    /// Gamma ramps clients set, per CRTC (red, green, blue). Kept to be read
    /// back, and nothing more: the desktop owns the colours (night light),
    /// so they are never applied.
    gamma: BTreeMap<u32, [Vec<u16>; 3]>,
}

/// A mode the compositor offers for an output, for listing it beside the one
/// it drives (see [`Screen::set_listed_modes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: u16,
    pub height: u16,
    pub refresh_mhz: u32,
    /// The mode the compositor is driving.
    pub current: bool,
    pub preferred: bool,
    pub interlaced: bool,
}

/// Why a gamma ramp was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum GammaError {
    /// No such CRTC (BadRRCrtc).
    UnknownCrtc,
    /// Not [`GAMMA_SIZE`] entries (BadMatch).
    Size,
}

/// One RandR 1.5 monitor, as `RRGetMonitors` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorDesc {
    pub name: Vec<u8>,
    pub primary: bool,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub mm_width: u32,
    pub mm_height: u32,
    pub output_id: u32,
}

/// An output positioned on a screen.
pub struct Output {
    pub output_id: u32,
    pub crtc_id: u32,
    pub name: Vec<u8>,
    /// CRTC position in the virtual screen, in **physical** pixels. For Wayland
    /// outputs this is computed by [`Screen::relayout_physical`] from the logical
    /// layout so that scaled outputs' native-resolution buffers tile without
    /// overlap (see `logical_*` below). The width/height are the physical mode.
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    /// The output's rectangle in the compositor's **logical** coordinate space
    /// (from xdg-output, or wl_output geometry + mode/scale as a fallback). Used
    /// only to reconstruct the relative arrangement; physical positions are
    /// derived from it. Zero for the synthetic default output.
    pub lx: i32,
    pub ly: i32,
    pub lw: i32,
    pub lh: i32,
    pub mode: u32, // 0 is disabled
    pub mode_ids: Vec<u32>,
    pub mm_width: u32,
    pub mm_height: u32,
    pub wl_name: u32, // wayland object, or 0 if not
    pub connected: bool,
    /// The compositor's mode list as last listed for this output, and the
    /// CRTC mode it was listed with (`set_listed_modes` does nothing again
    /// for the same pair).
    listed: Option<(Vec<ModeSpec>, u32)>,
}

// A mode for an output.
pub struct Mode {
    pub info: ModeInfo,
    pub name: Vec<u8>,
    /// Allocated by us for an output's native resolution, and so freed once no
    /// output lists it; a client-created mode (`RandrCreateMode`) is the
    /// client's to destroy.
    ours: bool,
    /// Listed from the compositor's mode list for an output (not driven):
    /// replaced whenever that list is, and freed once no output lists it.
    listed: bool,
}

impl Screen {
    /// A default single output of the given size (used until Wayland outputs
    /// arrive, or if there's no compositor).
    pub fn new(width: u16, height: u16) -> Self {
        let mut s = Self {
            width,
            height,
            modes: Vec::new(),
            outputs: Vec::new(),
            next_id: RANDR_OUTPUT_BASE,
            timestamp: 1,
            config_timestamp: 1,
            primary: 0,
            gamma: BTreeMap::new(),
        };
        let mode = s.alloc_mode(width, height, 0);
        let (output_id, crtc_id) = (s.alloc_id(), s.alloc_id());
        s.outputs.push(Output {
            output_id,
            crtc_id,
            name: b"screen".to_vec(),
            x: 0,
            y: 0,
            width,
            height,
            lx: 0,
            ly: 0,
            lw: i32::from(width),
            lh: i32::from(height),
            mode,
            mode_ids: vec![mode],
            mm_width: mm(width),
            mm_height: mm(height),
            wl_name: 0,
            connected: true,
            listed: None,
        });
        s
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn alloc_mode(&mut self, width: u16, height: u16, refresh_mhz: u32) -> u32 {
        let id = self.alloc_id();
        self.modes.push(Mode::new(id, width, height, refresh_mhz));
        id
    }

    pub fn create_mode(&mut self, mut info: ModeInfo, name: Vec<u8>) -> u32 {
        let id = self.alloc_id();
        info.id = id;
        info.name_len = name.len() as u16;
        self.modes.push(Mode {
            info,
            name,
            ours: false,
            listed: false,
        });
        id
    }

    /// Allocates a mode standing for one of the compositor's offered modes.
    fn alloc_listed_mode(&mut self, spec: ModeSpec) -> u32 {
        let id = self.alloc_id();
        let mut mode = Mode::new(id, spec.width, spec.height, spec.refresh_mhz);
        mode.ours = false;
        mode.listed = true;
        if spec.interlaced {
            // as Xorg names them; and the compositor's rate is the field
            // rate, twice the frame rate the dot clock gives (xrandr doubles
            // it back for an interlaced mode)
            mode.name.push(b'i');
            mode.info.name_len = mode.name.len() as u16;
            mode.info.mode_flags = ModeFlag::INTERLACE;
            mode.info.dot_clock /= 2;
        }
        self.modes.push(mode);
        id
    }

    fn is_listed(&self, id: u32) -> bool {
        self.modes.iter().any(|m| m.info.id == id && m.listed)
    }

    /// Lists the compositor's modes for the output mirroring `wl_name`
    /// beside the one it drives, as `RRGetOutputInfo` shows a monitor's modes
    /// on Xorg: its preferred mode first, then in the compositor's order, the
    /// CRTC's own mode standing for the one the compositor is driving.
    /// Selecting another one is a resolution change like any other (refused
    /// unless `-dynres`). Modes a client added to the output stay listed.
    /// Returns whether the list changed.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend lists modes
    pub fn set_listed_modes(&mut self, wl_name: u32, specs: &[ModeSpec]) -> bool {
        let Some(i) = self
            .outputs
            .iter()
            .position(|o| o.wl_name == wl_name && o.is_wayland())
        else {
            return false;
        };
        let current = self.outputs[i].mode;
        if self.outputs[i]
            .listed
            .as_ref()
            .is_some_and(|(s, c)| s.as_slice() == specs && *c == current)
        {
            return false;
        }
        let keep: Vec<u32> = self.outputs[i]
            .mode_ids
            .iter()
            .copied()
            .filter(|&id| id == current || !self.is_listed(id))
            .collect();
        let current_size = self.mode_size(current);
        let mut ids: Vec<u32> = Vec::new();
        let mut seen: Vec<(u16, u16, u32, bool)> = Vec::new();
        let ordered = specs
            .iter()
            .filter(|s| s.preferred)
            .chain(specs.iter().filter(|s| !s.preferred));
        for s in ordered {
            if s.width == 0 || s.height == 0 || s.width > MAX_SCREEN || s.height > MAX_SCREEN {
                continue;
            }
            let key = (s.width, s.height, s.refresh_mhz, s.interlaced);
            if seen.contains(&key) || seen.len() >= MAX_LISTED_MODES {
                continue;
            }
            seen.push(key);
            if s.current
                && current != 0
                && current_size == Some((s.width, s.height))
                && !ids.contains(&current)
            {
                ids.push(current);
                continue;
            }
            ids.push(self.alloc_listed_mode(*s));
        }
        // then the CRTC's mode if the compositor's list didn't stand for it,
        // and modes a client added
        for id in keep {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let o = &mut self.outputs[i];
        o.mode_ids = ids;
        o.listed = Some((specs.to_vec(), current));
        self.prune_modes();
        self.bump();
        true
    }

    /// A CRTC's gamma ramp: what a client set, else the identity.
    pub fn gamma(&self, crtc: u32) -> [Vec<u16>; 3] {
        self.gamma.get(&crtc).cloned().unwrap_or_else(|| {
            let ramp: Vec<u16> = (0..u32::from(GAMMA_SIZE))
                .map(|i| (i * 65535 / (u32::from(GAMMA_SIZE) - 1)) as u16)
                .collect();
            [ramp.clone(), ramp.clone(), ramp]
        })
    }

    /// `RRSetCrtcGamma`: kept for [`gamma`](Self::gamma), never applied.
    pub fn set_gamma(
        &mut self,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> Result<(), GammaError> {
        if !self.outputs.iter().any(|o| o.crtc_id == crtc) {
            return Err(GammaError::UnknownCrtc);
        }
        let n = usize::from(GAMMA_SIZE);
        if red.len() != n || green.len() != n || blue.len() != n {
            return Err(GammaError::Size);
        }
        self.gamma
            .insert(crtc, [red.to_vec(), green.to_vec(), blue.to_vec()]);
        Ok(())
    }

    /// Drops the native modes no output lists any more, after an output's mode
    /// changed or the output went away; without this every resolution change
    /// leaves a mode behind forever.
    fn prune_modes(&mut self) {
        let outputs = &self.outputs;
        self.modes.retain(|m| {
            !(m.ours || m.listed) || outputs.iter().any(|o| o.mode_ids.contains(&m.info.id))
        });
    }

    pub fn add_output_mode(&mut self, output: u32, mode: u32) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.output_id == output)
            && !o.mode_ids.contains(&mode)
        {
            o.mode_ids.push(mode);
        }
    }

    pub fn delete_output_mode(&mut self, output: u32, mode: u32) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.output_id == output) {
            o.mode_ids.retain(|&m| m != mode);
        }
    }

    pub fn destroy_mode(&mut self, mode: u32) {
        self.modes.retain(|m| m.info.id != mode);
        for o in &mut self.outputs {
            o.mode_ids.retain(|&m| m != mode);
        }
    }

    pub fn output_by_id(&self, output: u32) -> Option<&Output> {
        self.outputs.iter().find(|o| o.output_id == output)
    }

    pub fn output_by_crtc(&self, crtc: u32) -> Option<&Output> {
        self.outputs.iter().find(|o| o.crtc_id == crtc)
    }

    pub fn output_by_wl_name(&self, wl_name: u32) -> Option<&Output> {
        self.outputs
            .iter()
            .find(|o| o.wl_name == wl_name && o.is_wayland())
    }

    pub fn mode_size(&self, id: u32) -> Option<(u16, u16)> {
        self.modes
            .iter()
            .find(|m| m.info.id == id)
            .map(|m| (m.info.width, m.info.height))
    }

    pub fn mode_infos(&self) -> Vec<ModeInfo> {
        self.modes.iter().map(|m| m.info).collect()
    }

    pub fn mode_names(&self) -> Vec<u8> {
        self.modes
            .iter()
            .flat_map(|m| m.name.iter().copied())
            .collect()
    }

    pub fn crtc_ids(&self) -> Vec<u32> {
        self.outputs.iter().map(|o| o.crtc_id).collect()
    }

    pub fn output_ids(&self) -> Vec<u32> {
        self.outputs.iter().map(|o| o.output_id).collect()
    }

    /// Follows the compositor's primary output (`wl_name`, 0 for none).
    /// Returns whether it changed.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only GNOME says which output is primary
    pub fn set_primary(&mut self, wl_name: u32) -> bool {
        if self.primary == wl_name {
            return false;
        }
        self.primary = wl_name;
        self.bump();
        true
    }

    /// `RRGetOutputPrimary`: the compositor's primary output while it is
    /// active, else the first active output (0 if there is none), as Xorg
    /// falls back to the first CRTC's output when no primary is set.
    pub fn primary_output(&self) -> u32 {
        let active = |o: &&Output| o.connected && o.mode != 0;
        self.outputs
            .iter()
            .filter(active)
            .find(|o| self.primary != 0 && o.wl_name == self.primary)
            .or_else(|| self.outputs.iter().find(active))
            .map_or(0, |o| o.output_id)
    }

    /// The refresh (mHz) of the primary output's current mode (see
    /// [`primary_output`](Self::primary_output)), 0 if unknown.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn primary_refresh_mhz(&self) -> u32 {
        let id = self.primary_output();
        self.outputs
            .iter()
            .find(|o| o.output_id == id)
            .and_then(|o| self.modes.iter().find(|m| m.info.id == o.mode))
            .map_or(0, |m| refresh_mhz(&m.info))
    }

    /// `RRGetMonitors`: one monitor per active output, named after it, as
    /// Xorg's automatic monitors (one per active CRTC) are; the primary
    /// output's is the primary monitor.
    pub fn monitors(&self) -> Vec<MonitorDesc> {
        let primary = self.primary_output();
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode != 0)
            .map(|o| MonitorDesc {
                name: o.name.clone(),
                primary: o.output_id == primary,
                x: o.x,
                y: o.y,
                width: o.width,
                height: o.height,
                mm_width: o.mm_width,
                mm_height: o.mm_height,
                output_id: o.output_id,
            })
            .collect()
    }

    /// The physical CRTC position of the output mirroring `wl_name`, for placing
    /// its capture buffer in the framebuffer.
    pub fn physical_pos(&self, wl_name: u32) -> Option<(i32, i32)> {
        self.outputs
            .iter()
            .find(|o| o.wl_name == wl_name)
            .map(|o| (i32::from(o.x), i32::from(o.y)))
    }

    /// Per-output physical (X11 screen) and logical (Wayland compositor)
    /// rectangles for the connected enabled outputs. Used to remap absolute
    /// pointer input from the physical-tiled screen back into logical space (so
    /// it is correct for scaled displays).
    pub fn layout_rects(&self) -> Vec<OutputRect> {
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode != 0)
            .map(|o| OutputRect {
                px: i32::from(o.x),
                py: i32::from(o.y),
                pw: i32::from(o.width),
                ph: i32::from(o.height),
                lx: o.lx,
                ly: o.ly,
                lw: o.lw,
                lh: o.lh,
            })
            .collect()
    }

    /// Reconfigures a CRTC (position + mode, 0 to disable) in the model only,
    /// as `RRSetCrtcConfig` does: the screen size is the client's to set with
    /// `RRSetScreenSize`, so the bounds are left alone. Returns false for an
    /// unknown CRTC or mode.
    ///
    /// For a Wayland output this is only right when the compositor is (or has
    /// just been) driving the output at the mode's size; see
    /// [`adopt_mode`](Self::adopt_mode) for the latter.
    pub fn set_crtc(&mut self, crtc: u32, x: i16, y: i16, mode: u32) -> bool {
        let size = if mode == 0 {
            None
        } else {
            match self.mode_size(mode) {
                Some(size) => Some(size),
                None => return false,
            }
        };
        let Some(o) = self.outputs.iter_mut().find(|o| o.crtc_id == crtc) else {
            return false;
        };
        o.mode = mode;
        o.x = x;
        o.y = y;
        if let Some((w, h)) = size {
            o.width = w;
            o.height = h;
        }
        self.bump();
        true
    }

    /// Puts every Wayland output back to what the compositor is actually
    /// driving, and the screen back to their bounds: re-enables CRTCs a client
    /// disabled, at a mode of the output's real size. For rolling back a
    /// client's reconfiguration that the compositor then refused (RealVNC
    /// disables the CRTC and resizes the screen *before* it asks for the mode,
    /// and does not undo either when that fails, which would leave the X
    /// screen a size the capture never fills and the output disabled).
    /// Returns whether anything changed.
    pub fn restore_outputs(&mut self) -> bool {
        let mut changed = false;
        for i in 0..self.outputs.len() {
            let o = &self.outputs[i];
            if !o.is_wayland() {
                continue;
            }
            let (width, height) = (o.width, o.height);
            let ok = self
                .mode_size(o.mode)
                .is_some_and(|size| size == (width, height));
            if ok {
                continue;
            }
            let listed = o
                .mode_ids
                .iter()
                .copied()
                .find(|&id| self.mode_size(id) == Some((width, height)));
            let mode = match listed {
                Some(id) => id,
                None => self.alloc_mode(width, height, 0),
            };
            let o = &mut self.outputs[i];
            o.mode = mode;
            if !o.mode_ids.contains(&mode) {
                o.mode_ids.push(mode);
            }
            changed = true;
        }
        // a CRTC a client moved goes back to its place too
        let before: Vec<(i16, i16)> = self.outputs.iter().map(|o| (o.x, o.y)).collect();
        self.relayout_physical();
        changed |= self.outputs.iter().map(|o| (o.x, o.y)).ne(before);
        let bounds = self.recompute_bounds();
        if changed && !bounds {
            self.bump();
        }
        changed || bounds
    }

    /// Whether the Wayland outputs are not as the compositor drives them: a
    /// client disabled a CRTC or set it to another size or position, or left
    /// the screen another size than their bounds — what
    /// [`restore_outputs`](Self::restore_outputs) puts back.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend checks
    pub fn outputs_diverged(&self) -> bool {
        if self
            .outputs
            .iter()
            .any(|o| o.is_wayland() && self.mode_size(o.mode) != Some((o.width, o.height)))
        {
            return true;
        }
        let xs: Vec<(i32, i32, i32)> = self
            .outputs
            .iter()
            .map(|o| (o.lx, o.lw, i32::from(o.width)))
            .collect();
        let ys: Vec<(i32, i32, i32)> = self
            .outputs
            .iter()
            .map(|o| (o.ly, o.lh, i32::from(o.height)))
            .collect();
        let (px, py) = (pack_axis(&xs), pack_axis(&ys));
        let max = i32::from(i16::MAX);
        let (mut w, mut h) = (0u16, 0u16);
        for (i, o) in self.outputs.iter().enumerate() {
            if o.is_wayland()
                && (i32::from(o.x), i32::from(o.y)) != (px[i].clamp(0, max), py[i].clamp(0, max))
            {
                return true;
            }
            if o.connected && o.mode != 0 {
                w = w.max((o.x.max(0) as u16).saturating_add(o.width));
                h = h.max((o.y.max(0) as u16).saturating_add(o.height));
            }
        }
        (w.clamp(1, MAX_SCREEN), h.clamp(1, MAX_SCREEN)) != (self.width, self.height)
    }

    /// After the compositor drove a Wayland output to a client's requested
    /// size, switches its CRTC to the mode the client actually asked for (the
    /// wl_output sync gave it a native mode of the same size), so the client
    /// reads back what it set. A no-op unless the sizes agree.
    pub fn adopt_mode(&mut self, crtc: u32, mode: u32) {
        let Some((w, h)) = self.mode_size(mode) else {
            return;
        };
        let Some(o) = self.outputs.iter_mut().find(|o| o.crtc_id == crtc) else {
            return;
        };
        if o.width == w && o.height == h && o.mode != mode {
            if !o.mode_ids.contains(&mode) {
                o.mode_ids.push(mode);
            }
            o.mode = mode;
            // the native mode of that size stood for what the compositor
            // drives, which the client's mode now does
            let native: Vec<u32> = self
                .modes
                .iter()
                .filter(|m| m.ours && m.info.id != mode && (m.info.width, m.info.height) == (w, h))
                .map(|m| m.info.id)
                .collect();
            self.outputs
                .iter_mut()
                .find(|o| o.crtc_id == crtc)
                .unwrap()
                .mode_ids
                .retain(|m| !native.contains(m));
            self.prune_modes();
            self.bump();
        }
    }

    /// Recomputes the X11 screen (i.e., bounding box of all outputs) size for
    /// connected enabled outputs, returning true if changed.
    pub fn recompute_bounds(&mut self) -> bool {
        let mut w = 0u16;
        let mut h = 0u16;
        for o in &self.outputs {
            if o.connected && o.mode != 0 {
                // saturating, since a client picks the crtc positions and
                // `position + size` can leave u16 behind
                w = w.max((o.x.max(0) as u16).saturating_add(o.width));
                h = h.max((o.y.max(0) as u16).saturating_add(o.height));
            }
        }
        let (w, h) = (w.clamp(1, MAX_SCREEN), h.clamp(1, MAX_SCREEN));
        let changed = w != self.width || h != self.height;
        self.width = w;
        self.height = h;
        if changed {
            self.bump();
        }
        changed
    }

    /// Derives each output's physical CRTC position from the logical layout so
    /// that native-resolution capture buffers tile without overlap, even when
    /// outputs have different (possibly fractional) scales.
    ///
    /// Logical coordinates only encode the arrangement; physical sizes differ
    /// by scale. We repack each axis independently, walking outputs
    /// left-to-right (top-to-bottom), so an output that begins where a
    /// neighbour's logical edge ended is placed where that neighbour's physical
    /// edge ended. This is exact for edge-aligned layouts (rows, columns,
    /// aligned grids); gaps are bridged 1:1.
    fn relayout_physical(&mut self) {
        let xs: Vec<(i32, i32, i32)> = self
            .outputs
            .iter()
            .map(|o| (o.lx, o.lw, i32::from(o.width)))
            .collect();
        let ys: Vec<(i32, i32, i32)> = self
            .outputs
            .iter()
            .map(|o| (o.ly, o.lh, i32::from(o.height)))
            .collect();
        let px = pack_axis(&xs);
        let py = pack_axis(&ys);
        let max = i32::from(i16::MAX);
        for (i, o) in self.outputs.iter_mut().enumerate() {
            o.x = px[i].clamp(0, max) as i16;
            o.y = py[i].clamp(0, max) as i16;
        }
    }

    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width.clamp(1, MAX_SCREEN);
        self.height = height.clamp(1, MAX_SCREEN);
        self.bump();
    }

    pub fn bump(&mut self) {
        self.timestamp = self.timestamp.wrapping_add(1);
        self.config_timestamp = self.config_timestamp.wrapping_add(1);
    }

    /// Creates or updates the output mirroring a given Wayland output, then
    /// recomputes the physical layout and screen bounds. Returns whether
    /// anything changed.
    ///
    /// `lx,ly,lw,lh` is the output's rectangle in the compositor's logical
    /// space; `width,height` is its physical mode resolution. The CRTC keeps the
    /// physical mode as its size, and its physical position is derived from the
    /// logical layout in [`relayout_physical`](Self::relayout_physical).
    #[allow(clippy::too_many_arguments)]
    pub fn sync_wayland_output(
        &mut self,
        wl_name: u32,
        name: &[u8],
        lx: i32,
        ly: i32,
        lw: i32,
        lh: i32,
        width: u16,
        height: u16,
        refresh_mhz: u32,
    ) -> bool {
        // Find or create the output and its native mode.
        let existing = self.outputs.iter().position(|o| o.wl_name == wl_name);
        match existing {
            Some(i) => {
                let o = &self.outputs[i];
                // a native mode stands for the compositor's size *and* refresh
                // (a client-created mode the compositor was driven to keeps the
                // client's own clock)
                let want_clock = Mode::clock(width, height, refresh_mhz);
                let fits = |m: &Mode| {
                    m.info.width == width
                        && m.info.height == height
                        && (!m.ours || refresh_mhz == 0 || m.info.dot_clock == want_clock)
                };
                let same_mode = self
                    .modes
                    .iter()
                    .find(|m| m.info.id == o.mode)
                    .is_some_and(fits);
                let unchanged = o.lx == lx
                    && o.ly == ly
                    && o.lw == lw
                    && o.lh == lh
                    && o.width == width
                    && o.height == height
                    && same_mode;
                if unchanged {
                    return false;
                }
                // keep the mode if it already has this size; else reuse one of
                // the output's listed modes with it (a client-created mode the
                // compositor was just driven to); else allocate a native one
                let listed = self.outputs[i]
                    .mode_ids
                    .iter()
                    .copied()
                    .filter(|&id| !self.is_listed(id))
                    .find(|&id| {
                        self.modes
                            .iter()
                            .find(|m| m.info.id == id)
                            .is_some_and(fits)
                    });
                let mode = if same_mode {
                    self.outputs[i].mode
                } else if let Some(id) = listed {
                    id
                } else {
                    self.alloc_mode(width, height, refresh_mhz)
                };
                let o = &mut self.outputs[i];
                o.lx = lx;
                o.ly = ly;
                o.lw = lw;
                o.lh = lh;
                o.width = width;
                o.height = height;
                o.mode = mode;
                if !o.mode_ids.contains(&mode) {
                    o.mode_ids.push(mode);
                }
                // Any native mode of another size is ours to retire: a native
                // mode only ever stands for what the compositor is driving now.
                // (Looked up by size rather than by the previous CRTC mode, which
                // is 0 when a client disabled the CRTC before changing it, as
                // RealVNC does.)
                let stale: Vec<u32> = self
                    .modes
                    .iter()
                    .filter(|m| {
                        m.ours && m.info.id != mode && self.outputs[i].mode_ids.contains(&m.info.id)
                    })
                    .map(|m| m.info.id)
                    .collect();
                if !stale.is_empty() {
                    self.outputs[i].mode_ids.retain(|m| !stale.contains(m));
                    self.prune_modes();
                }
            }
            None => {
                let mode = self.alloc_mode(width, height, refresh_mhz);
                let (output_id, crtc_id) = (self.alloc_id(), self.alloc_id());
                let name = if name.is_empty() {
                    format!("output-{wl_name}").into_bytes()
                } else {
                    name.to_vec()
                };
                self.outputs.push(Output {
                    output_id,
                    crtc_id,
                    name,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    lx,
                    ly,
                    lw,
                    lh,
                    mode,
                    mode_ids: vec![mode],
                    mm_width: mm(width),
                    mm_height: mm(height),
                    wl_name,
                    connected: true,
                    listed: None,
                });
                // drop the fake output, and its mode, now that we have a real one
                self.outputs.retain(|o| o.is_wayland());
                self.prune_modes();
            }
        }
        self.relayout_physical();
        self.recompute_bounds();
        true
    }

    pub fn remove_wayland_output(&mut self, wl_name: u32) -> bool {
        let before = self.outputs.len();
        self.outputs.retain(|o| o.wl_name != wl_name);
        if self.outputs.len() != before {
            let crtcs: Vec<u32> = self.outputs.iter().map(|o| o.crtc_id).collect();
            self.gamma.retain(|crtc, _| crtcs.contains(crtc));
            self.prune_modes();
            self.relayout_physical();
            self.recompute_bounds();
            true
        } else {
            false
        }
    }
}

impl Output {
    /// Whether this output mirrors a real Wayland output (`wl_name` 0 is the
    /// synthetic default we start with before any output is known).
    pub fn is_wayland(&self) -> bool {
        self.wl_name != 0
    }
}

/// The dot clock of a blanking-free `width`x`height` mode at `refresh_mhz`
/// (60 Hz when unknown). RandR's dot clock is 32-bit: a big fast mode
/// (7680x4320@144 needs 4.78 GHz) can't be expressed, so this saturates to the
/// fastest refresh X can state for that size rather than wrapping to a
/// nonsense one (14.5 Hz).
pub fn dot_clock(width: u16, height: u16, refresh_mhz: u32) -> u32 {
    let px = u64::from(width) * u64::from(height);
    let clock = if refresh_mhz > 0 {
        px * u64::from(refresh_mhz) / 1000
    } else {
        px * 60
    };
    u32::try_from(clock).unwrap_or(u32::MAX)
}

/// A mode's refresh in millihertz (0 when it has no timings).
pub fn refresh_mhz(info: &x11rb_protocol::protocol::randr::ModeInfo) -> u32 {
    let total = u64::from(info.htotal) * u64::from(info.vtotal);
    if total == 0 {
        return 0;
    }
    let mut mhz = u64::from(info.dot_clock) * 1000 / total;
    let flags = u32::from(info.mode_flags);
    if flags & 0x10 != 0 {
        mhz /= 2; // doublescan
    }
    if flags & 0x20 != 0 {
        mhz *= 2; // interlace
    }
    mhz as u32
}

impl Mode {
    /// The dot clock of a blanking-free `width`x`height` mode at `refresh_mhz`
    /// (60 Hz when unknown). RandR's dot clock is 32-bit: a big fast mode
    /// (7680x4320@144 needs 4.78 GHz) can't be expressed, so this saturates to
    /// the fastest refresh X can state for that size rather than wrapping to a
    /// nonsense one (14.5 Hz).
    fn clock(width: u16, height: u16, refresh_mhz: u32) -> u32 {
        dot_clock(width, height, refresh_mhz)
    }

    fn new(id: u32, width: u16, height: u16, refresh_mhz: u32) -> Self {
        let name = format!("{width}x{height}").into_bytes();
        let dot_clock = Self::clock(width, height, refresh_mhz);
        let info = ModeInfo {
            id,
            width,
            height,
            dot_clock,
            hsync_start: 0,
            hsync_end: 0,
            htotal: width,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: height,
            name_len: name.len() as u16,
            mode_flags: ModeFlag::from(0u32),
        };
        Self {
            info,
            name,
            ours: true,
            listed: false,
        }
    }
}

/// Repacks one axis, given each output's `(logical_start, logical_len,
/// physical_len)`, returning its physical start. See
/// [`Screen::relayout_physical`].
fn pack_axis(items: &[(i32, i32, i32)]) -> Vec<i32> {
    let mut out = vec![0i32; items.len()];
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0);

    // map a logical coordinate (output edge) to its assigned physical coordinate
    let mut bound: BTreeMap<i32, i32> = BTreeMap::new();
    if let Some(min_start) = items.iter().map(|&(s, _, _)| s).min() {
        bound.insert(min_start, 0);
    }
    for &i in &order {
        let (ls, llen, plen) = items[i];
        let px = match bound.get(&ls) {
            Some(&p) => p,
            // no exact boundary (i.e., gap), extend from the nearest known edge
            // 1:1
            None => match bound.range(..=ls).next_back() {
                Some((&b, &pb)) => pb + (ls - b),
                None => 0,
            },
        };
        bound.entry(ls).or_insert(px);
        out[i] = px;
        bound
            .entry(ls + llen)
            .and_modify(|v| *v = (*v).max(px + plen))
            .or_insert(px + plen);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen mirroring one 1280x800 Wayland output (registry name 7).
    fn screen() -> Screen {
        let mut s = Screen::new(640, 480);
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 60_000));
        s
    }

    fn the_output(s: &Screen) -> &Output {
        s.output_by_wl_name(7).expect("the wayland output")
    }

    fn native_modes(s: &Screen) -> Vec<(u16, u16)> {
        s.modes
            .iter()
            .filter(|m| m.ours)
            .map(|m| (m.info.width, m.info.height))
            .collect()
    }

    #[test]
    fn primary_follows_the_compositor_and_falls_back_to_the_first() {
        let mut s = screen();
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 1920, 1080, 1920, 1080, 60_000));
        let first = s.output_by_wl_name(7).unwrap().output_id;
        let second = s.output_by_wl_name(9).unwrap().output_id;
        assert_eq!(s.primary_output(), first);
        let before = s.config_timestamp;
        assert!(s.set_primary(9));
        assert!(s.config_timestamp != before);
        assert!(!s.set_primary(9));
        assert_eq!(s.primary_output(), second);
        let mons = s.monitors();
        assert_eq!(
            mons.iter()
                .map(|m| (m.name.as_slice(), m.primary))
                .collect::<Vec<_>>(),
            vec![(&b"HEADLESS-1"[..], false), (&b"HEADLESS-2"[..], true)]
        );
        // the primary output goes away: the first active one stands in
        assert!(s.remove_wayland_output(9));
        assert_eq!(s.primary_output(), first);
        assert!(s.monitors()[0].primary);
    }

    #[test]
    fn the_desktops_primary_output_is_the_primary_monitor() {
        // p2_fix item 1: GNOME's primary is not necessarily the first
        // output; the RandR monitor of the output it names is the primary
        // one, and it follows the desktop back
        let mut s = screen();
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 1024, 768, 1024, 768, 60_000));
        let (first, second) = (s.outputs[0].output_id, s.outputs[1].output_id);
        let primary = |s: &Screen| {
            let m = s.monitors();
            assert_eq!(m.iter().filter(|m| m.primary).count(), 1);
            m.iter().find(|m| m.primary).unwrap().output_id
        };
        assert_eq!(primary(&s), first);
        assert!(s.set_primary(9));
        assert_eq!(primary(&s), second);
        assert!(s.set_primary(7));
        assert_eq!(primary(&s), first);
    }

    #[test]
    fn monitors_mirror_the_active_outputs() {
        let mut s = screen();
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 1024, 768, 2048, 1536, 60_000));
        let mons = s.monitors();
        assert_eq!(mons.len(), 2);
        let m = &mons[1];
        assert_eq!((m.x, m.y, m.width, m.height), (1280, 0, 2048, 1536));
        assert_eq!(m.output_id, s.output_by_wl_name(9).unwrap().output_id);
        assert_eq!((m.mm_width, m.mm_height), (mm(2048), mm(1536)));
    }

    fn refresh_hz(s: &Screen, wl_name: u32) -> f64 {
        let o = s.output_by_wl_name(wl_name).unwrap();
        let m = s.modes.iter().find(|m| m.info.id == o.mode).unwrap();
        f64::from(m.info.dot_clock) / (f64::from(m.info.htotal) * f64::from(m.info.vtotal))
    }

    #[test]
    fn primary_refresh() {
        let mut s = Screen::new(640, 480);
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 1920, 1080, 1920, 1080, 144_000));
        // no GNOME primary yet: the first active output stands for it
        assert_eq!(s.primary_refresh_mhz(), 144_000);
        let mut s = Screen::new(640, 480);
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 60_000));
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 1920, 1080, 1920, 1080, 144_000));
        assert_eq!(s.primary_refresh_mhz(), 60_000);
        s.set_primary(9);
        assert_eq!(s.primary_refresh_mhz(), 144_000);
        // what X can state at 8K is less than asked: the saturated clock's refresh
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 7680, 4320, 7680, 4320, 144_000));
        assert_eq!(s.primary_refresh_mhz(), 129_453);
    }

    #[test]
    fn refresh_saturates_instead_of_wrapping() {
        let mut s = Screen::new(640, 480);
        assert!(s.sync_wayland_output(7, b"DP-1", 0, 0, 7680, 4320, 7680, 4320, 144_000));
        let hz = refresh_hz(&s, 7);
        // 2^32-1 / (7680*4320) = 129.45 Hz: the most a RandR mode can say
        assert!((hz - 129.45).abs() < 0.01, "{hz}");
        let mut s = Screen::new(640, 480);
        assert!(s.sync_wayland_output(7, b"DP-1", 0, 0, 5120, 2880, 5120, 2880, 144_000));
        assert!((refresh_hz(&s, 7) - 144.0).abs() < 0.01);
    }

    #[test]
    fn a_refresh_change_at_the_same_size_is_followed() {
        let mut s = screen();
        assert!((refresh_hz(&s, 7) - 60.0).abs() < 0.01);
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 144_000));
        assert!((refresh_hz(&s, 7) - 144.0).abs() < 0.01);
        // only the current native mode is left, and a repeat is no change
        assert_eq!(native_modes(&s), vec![(1280, 800)]);
        assert!(!s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 144_000));
    }

    #[test]
    fn wayland_output_replaces_the_default() {
        let s = screen();
        assert_eq!(s.outputs.len(), 1);
        assert_eq!((s.width, s.height), (1280, 800));
        assert_eq!(native_modes(&s), vec![(1280, 800)]);
        assert_eq!(s.mode_size(the_output(&s).mode), Some((1280, 800)));
    }

    #[test]
    fn set_crtc_leaves_the_screen_size_alone() {
        // RRSetCrtcConfig never resizes the screen; that is RRSetScreenSize's
        // job (and RealVNC disables the CRTC before it shrinks the screen)
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        let ts = s.config_timestamp;
        assert!(s.set_crtc(crtc, 0, 0, 0));
        assert_eq!(the_output(&s).mode, 0);
        assert_eq!((s.width, s.height), (1280, 800));
        assert_ne!(s.config_timestamp, ts);
        assert!(
            s.layout_rects().is_empty(),
            "a disabled CRTC takes no input"
        );
        assert!(!s.set_crtc(crtc, 0, 0, 12345), "unknown mode");
        assert!(!s.set_crtc(999, 0, 0, 0), "unknown crtc");
    }

    #[test]
    fn set_size_is_clamped() {
        let mut s = screen();
        s.set_size(0, 40_000);
        assert_eq!((s.width, s.height), (1, MAX_SCREEN));
    }

    #[test]
    fn compositor_change_reuses_the_clients_mode_and_retires_the_native_one() {
        let mut s = screen();
        let (output, crtc) = (the_output(&s).output_id, the_output(&s).crtc_id);
        // the client's sequence: create + add a mode, disable, resize, set
        let info = ModeInfo {
            id: 0,
            width: 1024,
            height: 640,
            dot_clock: 0,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 1024,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 640,
            name_len: 0,
            mode_flags: ModeFlag::from(0u32),
        };
        let client_mode = s.create_mode(info, b"1024x640_vnc".to_vec());
        s.add_output_mode(output, client_mode);
        assert!(s.set_crtc(crtc, 0, 0, 0));
        s.set_size(1024, 640);
        // ... and the compositor's answer, as a wl_output change
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1024, 640, 1024, 640, 60_000));
        let o = the_output(&s);
        assert_eq!((o.width, o.height), (1024, 640));
        assert_eq!(
            o.mode, client_mode,
            "the listed mode of that size is reused"
        );
        assert_eq!((s.width, s.height), (1024, 640));
        assert!(
            native_modes(&s).is_empty(),
            "the old native mode is retired even though the CRTC was disabled"
        );
        assert!(
            s.modes.iter().any(|m| m.info.id == client_mode),
            "the client's mode is the client's to destroy"
        );
        // adopting is then a no-op
        let ts = s.config_timestamp;
        s.adopt_mode(crtc, client_mode);
        assert_eq!(s.config_timestamp, ts);
    }

    #[test]
    fn adopt_mode_switches_to_the_clients_mode_of_the_same_size() {
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        // the compositor changed first (a native mode was allocated), then the
        // client's own mode of that size is adopted
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 800, 600, 800, 600, 60_000));
        assert_eq!(native_modes(&s), vec![(800, 600)]);
        let client_mode = s.alloc_mode(800, 600, 0);
        s.modes.last_mut().unwrap().ours = false;
        s.adopt_mode(crtc, client_mode);
        assert_eq!(the_output(&s).mode, client_mode);
        assert!(
            native_modes(&s).is_empty(),
            "the native mode of that size is pruned"
        );
        // a mode of another size is not adopted
        let other = s.alloc_mode(640, 480, 0);
        s.adopt_mode(crtc, other);
        assert_eq!(the_output(&s).mode, client_mode);
    }

    #[test]
    fn outputs_diverge_when_a_client_leaves_them_off_moved_or_resized() {
        let mut s = screen();
        assert!(s.sync_wayland_output(9, b"HEADLESS-2", 1280, 0, 1024, 768, 1024, 768, 60_000));
        assert!(!s.outputs_diverged(), "as the compositor drives them");
        let crtc = s.output_by_wl_name(9).unwrap().crtc_id;
        let mode = s.output_by_wl_name(9).unwrap().mode;
        assert!(s.set_crtc(crtc, 0, 0, 0));
        assert!(s.outputs_diverged(), "a CRTC left off");
        assert!(s.restore_outputs());
        assert!(!s.outputs_diverged());
        assert!(s.set_crtc(crtc, 100, 0, mode));
        assert!(s.outputs_diverged(), "a CRTC moved");
        assert!(s.restore_outputs());
        assert!(!s.outputs_diverged());
        s.set_size(4000, 2000);
        assert!(s.outputs_diverged(), "a screen larger than the outputs");
        assert!(s.restore_outputs());
        assert!(!s.outputs_diverged());
    }

    #[test]
    fn restore_outputs_undoes_a_refused_reconfiguration() {
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        let mode = the_output(&s).mode;
        assert!(!s.restore_outputs(), "nothing to restore");
        // the client disabled the CRTC and shrank the screen for a mode the
        // compositor then refused
        assert!(s.set_crtc(crtc, 0, 0, 0));
        s.set_size(1000, 700);
        assert!(s.restore_outputs());
        let o = the_output(&s);
        assert_eq!(o.mode, mode, "re-enabled at the output's real mode");
        assert_eq!((o.width, o.height), (1280, 800));
        assert_eq!(
            (s.width, s.height),
            (1280, 800),
            "the screen is the outputs' bounds again"
        );
        assert_eq!(s.layout_rects().len(), 1);
    }

    #[test]
    fn unchanged_compositor_state_is_a_no_op() {
        let mut s = screen();
        let ts = s.config_timestamp;
        assert!(!s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 60_000));
        assert_eq!(s.config_timestamp, ts);
    }

    #[test]
    fn removing_the_output_prunes_its_native_mode() {
        let mut s = screen();
        assert!(s.remove_wayland_output(7));
        assert!(s.outputs.is_empty());
        assert!(native_modes(&s).is_empty());
        assert_eq!((s.width, s.height), (1, 1));
        assert!(!s.remove_wayland_output(7));
    }

    fn spec(width: u16, height: u16, refresh_mhz: u32) -> ModeSpec {
        ModeSpec {
            width,
            height,
            refresh_mhz,
            current: false,
            preferred: false,
            interlaced: false,
        }
    }

    /// The output's modes as `WxH@Hz` names, in its order.
    fn listed(s: &Screen) -> Vec<String> {
        the_output(s)
            .mode_ids
            .iter()
            .map(|&id| {
                let m = s.modes.iter().find(|m| m.info.id == id).unwrap();
                let mut hz = u64::from(m.info.dot_clock)
                    / (u64::from(m.info.htotal) * u64::from(m.info.vtotal));
                if m.info.mode_flags.contains(ModeFlag::INTERLACE) {
                    hz *= 2; // as xrandr shows it: the field rate
                }
                format!("{}@{hz}", String::from_utf8_lossy(&m.name))
            })
            .collect()
    }

    /// A monitor's modes as mutter lists them: 1920x1080 preferred, the
    /// driven 1280x800, and a few more (one interlaced, one duplicate).
    fn compositor_modes() -> Vec<ModeSpec> {
        vec![
            ModeSpec {
                preferred: true,
                ..spec(1920, 1080, 60_000)
            },
            spec(1920, 1080, 50_000),
            ModeSpec {
                interlaced: true,
                ..spec(1920, 1080, 60_000)
            },
            ModeSpec {
                current: true,
                ..spec(1280, 800, 60_000)
            },
            spec(1024, 768, 60_000),
            spec(1024, 768, 60_000),
        ]
    }

    #[test]
    fn gamma_is_kept_per_crtc_and_read_back() {
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        let identity = s.gamma(crtc);
        assert_eq!(identity[0].len(), usize::from(GAMMA_SIZE));
        assert_eq!((identity[0][0], identity[0][255]), (0, 65535));
        let half: Vec<u16> = identity[0].iter().map(|v| v / 2).collect();
        assert_eq!(s.set_gamma(crtc, &half, &identity[1], &half), Ok(()));
        assert_eq!(
            s.gamma(crtc),
            [half.clone(), identity[1].clone(), half.clone()]
        );
        assert_eq!(
            s.set_gamma(crtc, &half[..10], &half[..10], &half[..10]),
            Err(GammaError::Size)
        );
        assert_eq!(
            s.set_gamma(999, &half, &half, &half),
            Err(GammaError::UnknownCrtc)
        );
        // the output going takes its ramp with it
        assert!(s.remove_wayland_output(7));
        assert!(s.gamma.is_empty());
    }

    #[test]
    fn compositor_modes_are_listed_preferred_first() {
        let mut s = screen();
        let current = the_output(&s).mode;
        assert!(s.set_listed_modes(7, &compositor_modes()));
        assert_eq!(
            listed(&s),
            [
                "1920x1080@60",
                "1920x1080@50",
                "1920x1080i@60",
                "1280x800@60",
                "1024x768@60"
            ]
        );
        let o = the_output(&s);
        assert_eq!(o.mode, current, "the driven mode is unchanged");
        assert_eq!(
            o.mode_ids[3], current,
            "and stands for the compositor's current one"
        );
        let interlaced = s.modes.iter().find(|m| m.name == b"1920x1080i").unwrap();
        assert_eq!(interlaced.info.mode_flags, ModeFlag::INTERLACE);
        assert_eq!(interlaced.info.name_len, 10);
        // the same list again is a no-op: no new modes, no new timestamp
        let (n, ts) = (s.modes.len(), s.config_timestamp);
        assert!(!s.set_listed_modes(7, &compositor_modes()));
        assert_eq!((s.modes.len(), s.config_timestamp), (n, ts));
        // an unknown output is ignored
        assert!(!s.set_listed_modes(8, &compositor_modes()));
    }

    #[test]
    fn a_compositor_mode_change_relists_without_leaking_modes() {
        let mut s = screen();
        assert!(s.set_listed_modes(7, &compositor_modes()));
        let n = s.modes.len();
        // the compositor now drives 1920x1080@60
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1920, 1080, 1920, 1080, 60_000));
        assert_eq!(
            native_modes(&s),
            vec![(1920, 1080)],
            "a native mode, not a listed one"
        );
        let after: Vec<ModeSpec> = compositor_modes()
            .into_iter()
            .map(|m| ModeSpec {
                current: m.preferred,
                ..m
            })
            .collect();
        assert!(s.set_listed_modes(7, &after));
        assert_eq!(
            listed(&s),
            [
                "1920x1080@60",
                "1920x1080@50",
                "1920x1080i@60",
                "1280x800@60",
                "1024x768@60"
            ]
        );
        let o = the_output(&s);
        assert_eq!(
            o.mode_ids[0], o.mode,
            "the driven mode stands for the preferred one"
        );
        assert_eq!((o.width, o.height), (1920, 1080));
        assert_eq!(s.modes.len(), n, "every replaced mode was freed");
        // the output going frees its listed modes too
        assert!(s.remove_wayland_output(7));
        assert!(s.modes.is_empty());
    }

    #[test]
    fn listing_keeps_the_clients_modes_and_a_mode_it_was_driven_to() {
        let mut s = screen();
        let (output, crtc) = (the_output(&s).output_id, the_output(&s).crtc_id);
        let client = s.alloc_mode(1000, 700, 0);
        s.modes.last_mut().unwrap().ours = false;
        s.add_output_mode(output, client);
        assert!(s.set_listed_modes(7, &compositor_modes()));
        assert_eq!(listed(&s).last().unwrap(), "1000x700@60");
        // -dynres drove the output to the listed 1920x1080@50: the sync
        // allocates a native mode, and the client's pick is adopted
        let pick = the_output(&s).mode_ids[1];
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1920, 1080, 1920, 1080, 50_000));
        s.adopt_mode(crtc, pick);
        assert_eq!(the_output(&s).mode, pick);
        assert!(native_modes(&s).is_empty());
        let after: Vec<ModeSpec> = compositor_modes()
            .into_iter()
            .map(|m| ModeSpec {
                current: (m.width, m.refresh_mhz, m.interlaced) == (1920, 50_000, false),
                ..m
            })
            .collect();
        assert!(s.set_listed_modes(7, &after));
        let o = the_output(&s);
        assert_eq!(o.mode, pick, "still the client's pick");
        assert_eq!(
            o.mode_ids[1], pick,
            "standing for the compositor's current mode"
        );
        assert_eq!(listed(&s).len(), 6, "5 offered + the client's own");
        assert!(o.mode_ids.contains(&client));
    }
}
