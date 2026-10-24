//! `-showwindows` decorations: a bridge-drawn Adwaita-like header for X
//! windows shown on the desktop, in its own `wl_subsurface`.
//!
//! Design: `research/release/design_decorations.md` (section 10 decisions
//! override the earlier text). This module holds the parts that do not know
//! about X or Wayland:
//!
//! - [`FrameGeom`]: the one conversion between the *content* rectangle (the X
//!   window and its border, which is the `xdg_surface` window geometry and the
//!   space winsync/P1/P2 work in) and the *surface* box (content + header +
//!   invisible resize margin);
//! - [`decide`]: the section 5.1 decision table, from what the client declares
//!   (`_NET_WM_WINDOW_TYPE`, `_MOTIF_WM_HINTS`, `_GTK_FRAME_EXTENTS`,
//!   override-redirect and WM_HINTS), never from pixels;
//! - [`parse_layout`]: `org.gnome.desktop.wm.preferences button-layout`;
//! - [`Painter`]: cairo + pango through `dlopen` (no new crates; libloading is
//!   already in `Cargo.lock`), painting into a `wl_shm` ARGB8888 buffer.
//!
//! Without cairo/pango the frame is still there (geometry, extents, input):
//! only the title and buttons are missing, and the doctor says why.

use std::ffi::{CString, c_char, c_int, c_void};

/// Adwaita header height in logical pixels (the GNOME 50 metric is measured
/// in M3; this is the design's value until then).
pub const HEADER_HEIGHT: i32 = 46;
/// Invisible resize margin around the content, logical pixels.
pub const RESIZE_MARGIN: i32 = 6;

/// The decoration around an X client: a header above and an invisible resize
/// margin on the other three sides. Both zero when a window is not framed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FrameGeom {
    pub header: i32,
    pub margin: i32,
}

impl FrameGeom {
    pub const NONE: FrameGeom = FrameGeom {
        header: 0,
        margin: 0,
    };
    pub const ADWAITA: FrameGeom = FrameGeom {
        header: HEADER_HEIGHT,
        margin: RESIZE_MARGIN,
    };

    pub fn is_framed(self) -> bool {
        self.header > 0 || self.margin > 0
    }

    /// The surface box: content plus both margins and the header.
    pub fn surface_size(self, content: (u16, u16)) -> (u16, u16) {
        let m = self.margin.max(0) as u32;
        let h = self.header.max(0) as u32;
        (
            (u32::from(content.0) + 2 * m).min(u32::from(u16::MAX)) as u16,
            (u32::from(content.1) + h + m).min(u32::from(u16::MAX)) as u16,
        )
    }

    /// The content rectangle inside a surface rectangle (the xdg window
    /// geometry is the surface box while a window is framed normally).
    pub fn content_rect(self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
        let m = self.margin.max(0);
        let hh = self.header.max(0);
        (x + m, y + hh, (w - 2 * m).max(0), (h - hh - m).max(0))
    }

    /// The surface rectangle around a content rectangle.
    pub fn surface_rect(self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
        let m = self.margin.max(0);
        let hh = self.header.max(0);
        (x - m, y - hh, w + 2 * m, h + hh + m)
    }

    /// Where the content's outer corner is inside the surface.
    pub fn content_origin(self) -> (i32, i32) {
        (self.margin.max(0), self.header.max(0))
    }

    /// What a surface point is over. `content` is the content size (the
    /// header/button layout is needed for buttons: [`Self::hit_area`]).
    pub fn hit(self, p: (f64, f64), content: (u16, u16)) -> Hit {
        let (w, h) = self.surface_size(content);
        let (x, y) = (p.0, p.1);
        let m = f64::from(self.margin.max(0));
        let hh = f64::from(self.header.max(0));
        if x < 0.0 || y < 0.0 || x >= f64::from(w) || y >= f64::from(h) {
            return Hit::Outside;
        }
        // the header's own top band resizes from the north edge, as on a
        // native GNOME window
        if self.header > 0 && y < hh {
            let west = x < m;
            let east = x >= f64::from(w) - m;
            let north = y < m.min(hh);
            return match (north, west, east) {
                (true, true, _) => Hit::Edge(Edge::NorthWest),
                (true, _, true) => Hit::Edge(Edge::NorthEast),
                (true, _, _) => Hit::Edge(Edge::North),
                (false, true, _) => Hit::Edge(Edge::West),
                (false, _, true) => Hit::Edge(Edge::East),
                _ => Hit::Title,
            };
        }
        if self.margin > 0 {
            let west = x < m;
            let east = x >= f64::from(w) - m;
            let south = y >= f64::from(h) - m;
            match (south, west, east) {
                (true, true, _) => return Hit::Edge(Edge::SouthWest),
                (true, _, true) => return Hit::Edge(Edge::SouthEast),
                (true, _, _) => return Hit::Edge(Edge::South),
                (false, true, _) => return Hit::Edge(Edge::West),
                (false, _, true) => return Hit::Edge(Edge::East),
                _ => {}
            }
        }
        Hit::Content
    }
}

/// What a press on a framed surface is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Outside,
    Content,
    Title,
    Edge(Edge),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    North,
    South,
    East,
    West,
    NorthWest,
    NorthEast,
    SouthWest,
    SouthEast,
}

/// The window types that get a frame (section 5.1). The names are
/// `_NET_WM_WINDOW_TYPE` atom names without the prefix, lowercased; `None` is
/// an untyped window (framed, like `NORMAL`).
pub fn frame_type_ok(kind: Option<&str>) -> bool {
    match kind {
        None => true,
        Some("normal" | "dialog" | "utility" | "toolbar" | "untyped" | "unknown") => true,
        Some(_) => false,
    }
}

/// Why a window is or is not framed (logged and shown by the doctor).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    Normal,
    WindowType,
    MotifNoDecorations,
    ClientSideDecorations,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::Normal => "NORMAL, no client decorations",
            Why::WindowType => "_NET_WM_WINDOW_TYPE",
            Why::MotifNoDecorations => "_MOTIF_WM_HINTS decorations 0",
            Why::ClientSideDecorations => "_GTK_FRAME_EXTENTS (client-side decorations)",
        }
    }
}

/// What the frame may do, from `_MOTIF_WM_HINTS.functions` (absent = all).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decision {
    pub framed: bool,
    pub why: Why,
    pub move_: bool,
    pub resize: bool,
    pub close: bool,
    pub minimize: bool,
    pub maximize: bool,
}

impl Decision {
    const NONE: Decision = Decision {
        framed: false,
        why: Why::WindowType,
        move_: false,
        resize: false,
        close: false,
        minimize: false,
        maximize: false,
    };
}

const MWM_HINTS_FUNCTIONS: u32 = 1 << 0;
const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
const MWM_FUNC_ALL: u32 = 1 << 0;
const MWM_FUNC_RESIZE: u32 = 1 << 1;
const MWM_FUNC_MOVE: u32 = 1 << 2;
const MWM_FUNC_MINIMIZE: u32 = 1 << 3;
const MWM_FUNC_MAXIMIZE: u32 = 1 << 4;
const MWM_FUNC_CLOSE: u32 = 1 << 5;

/// The section 5.1 decision table. `motif` is `_MOTIF_WM_HINTS` as 32-bit
/// values (empty: none), `gtk_extents` a present `_GTK_FRAME_EXTENTS`, and
/// `no_input` WM_HINTS `input == False` without `WM_TAKE_FOCUS`.
pub fn decide(
    kind: Option<&str>,
    motif: &[u32],
    gtk_extents: Option<[i32; 4]>,
    no_input: bool,
) -> Decision {
    // WM_HINTS `input == False` alone does not mean "no frame": an X window
    // manager frames xclock and display-only tools too (the toolkit matrix
    // measures this); only an override-redirect window has no frame.
    let _ = no_input;
    if gtk_extents.is_some() {
        return Decision {
            why: Why::ClientSideDecorations,
            ..Decision::NONE
        };
    }
    if !frame_type_ok(kind) {
        return Decision {
            why: Why::WindowType,
            ..Decision::NONE
        };
    }
    let flags = motif.first().copied().unwrap_or(0);
    if flags & MWM_HINTS_DECORATIONS != 0 && motif.len() >= 3 && motif[2] == 0 {
        return Decision {
            why: Why::MotifNoDecorations,
            ..Decision::NONE
        };
    }
    let funcs = if flags & MWM_HINTS_FUNCTIONS != 0 && motif.len() >= 2 {
        motif[1]
    } else {
        u32::MAX
    };
    let all = funcs == u32::MAX || funcs & MWM_FUNC_ALL != 0;
    let may = |bit: u32| all || funcs & bit != 0;
    Decision {
        framed: true,
        why: Why::Normal,
        move_: may(MWM_FUNC_MOVE),
        resize: may(MWM_FUNC_RESIZE),
        close: may(MWM_FUNC_CLOSE),
        minimize: may(MWM_FUNC_MINIMIZE),
        maximize: may(MWM_FUNC_MAXIMIZE),
    }
}

/// A titlebar click action (`org.gnome.desktop.wm.preferences
/// action-*-click-titlebar`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TitlebarAction {
    None,
    Menu,
    Lower,
    Minimize,
    Maximize,
    ToggleMaximize,
}

impl TitlebarAction {
    pub fn parse(s: &str) -> Option<TitlebarAction> {
        Some(match s {
            "none" => TitlebarAction::None,
            "menu" => TitlebarAction::Menu,
            "lower" => TitlebarAction::Lower,
            "minimize" | "minimise" => TitlebarAction::Minimize,
            "maximize" | "maximise" => TitlebarAction::Maximize,
            "toggle-maximize" => TitlebarAction::ToggleMaximize,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TitlebarAction::None => "none",
            TitlebarAction::Menu => "menu",
            TitlebarAction::Lower => "lower",
            TitlebarAction::Minimize => "minimize",
            TitlebarAction::Maximize => "maximize",
            TitlebarAction::ToggleMaximize => "toggle-maximize",
        }
    }
}

/// A header button. `Menu` is GNOME's window menu (`appmenu`/`menu` in
/// `button-layout`); `Spacer` takes room but draws nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Close,
    Minimize,
    Maximize,
    Menu,
    Spacer,
}

/// `button-layout` split into the two sides.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Layout {
    pub left: Vec<Button>,
    pub right: Vec<Button>,
}

impl Layout {
    /// The buttons as mutter/libdecor read the string: `(none)` and an empty
    /// string mean none, a missing `:` puts everything on the left, unknown
    /// names are ignored (as mutter ignores them).
    pub fn parse(s: &str) -> Layout {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("(none)") {
            return Layout::default();
        }
        let (left, right) = match s.split_once(':') {
            Some((l, r)) => (l, r),
            None => (s, ""),
        };
        let side = |part: &str| {
            part.split(',')
                .filter_map(|name| match name.trim() {
                    "close" => Some(Button::Close),
                    "minimize" | "minimise" => Some(Button::Minimize),
                    "maximize" | "maximise" => Some(Button::Maximize),
                    "appmenu" | "menu" | "icon" => Some(Button::Menu),
                    "spacer" => Some(Button::Spacer),
                    _ => None,
                })
                .collect()
        };
        Layout {
            left: side(left),
            right: side(right),
        }
    }

    /// The buttons the frame's hints allow, in layout order.
    pub fn allowed(&self, d: &Decision) -> Layout {
        let keep = |b: Button| match b {
            Button::Close => d.close,
            Button::Minimize => d.minimize,
            Button::Maximize => d.maximize,
            Button::Menu | Button::Spacer => true,
        };
        Layout {
            left: self.left.iter().copied().filter(|b| keep(*b)).collect(),
            right: self.right.iter().copied().filter(|b| keep(*b)).collect(),
        }
    }
}

/// The Adwaita header metrics of one GNOME version and colour scheme. The
/// GNOME 50 row is measured in M3 (design section 10 decision 4); these are
/// the design's starting values.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub height: i32,
    /// The frame's invisible resize margin (settings/look, M3).
    #[allow(dead_code)]
    pub margin: i32,
    pub radius: f64,
    /// The visible window-control circle (Adwaita `> image`, 24 px).
    pub button: f64,
    /// The clickable control around it (24 px + 2*5 px padding, 34 px).
    pub control: f64,
    pub icon: f64,
    pub pad: f64,
    pub spacing: f64,
    pub title_pt: f64,
    pub title_weight: f64,
    pub bg_active: [f64; 4],
    pub bg_backdrop: [f64; 4],
    pub fg_active: [f64; 4],
    pub fg_backdrop: [f64; 4],
    pub border: [f64; 4],
    pub close_hover: [f64; 4],
    /// GNOME's accent colour (M3); kept in the table for the measured look.
    #[allow(dead_code)]
    pub accent: [f64; 4],
}

const fn rgba(r: u8, g: u8, b: u8, a: f64) -> [f64; 4] {
    [r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, a]
}

impl Metrics {
    pub fn adwaita(_gnome_major: u32, dark: bool) -> Metrics {
        // Yaru's libadwaita 1.9.1 override (the theme of the GNOME 50.1
        // test session), `default-light.css`/`default-dark.css`:
        // light headerbar #fafafa active and backdrop, fg #3d3d3d;
        // dark active #393939, backdrop #2c2c2c, fg #f7f7f7.  Backdrop is
        // `headerbar:backdrop > windowhandle { filter: opacity(.5) }`, i.e.
        // the 50 % foreground below, not a second hand-picked colour.
        let (bg_active, bg_backdrop, fg, fg_back) = if dark {
            (
                rgba(0x2c, 0x2c, 0x2c, 1.0),
                rgba(0x2c, 0x2c, 0x2c, 1.0),
                rgba(0xf7, 0xf7, 0xf7, 1.0),
                rgba(0xf7, 0xf7, 0xf7, 0.50),
            )
        } else {
            (
                rgba(0xfa, 0xfa, 0xfa, 1.0),
                rgba(0xfa, 0xfa, 0xfa, 1.0),
                rgba(0x3d, 0x3d, 0x3d, 1.0),
                rgba(0x3d, 0x3d, 0x3d, 0.50),
            )
        };
        Metrics {
            height: HEADER_HEIGHT,
            margin: RESIZE_MARGIN,
            // libadwaita 1.9.1 `headerbar { min-height: 46px }`,
            // `windowcontrols > button { min-width: 24px; padding: 5px }`,
            // `windowcontrols { border-spacing: 3px }`,
            // `headerbar > windowhandle > box { padding: 6px 7px 7px 7px }`,
            // measured from the native Adw window's top row on GNOME 50.1:
            // the first opaque pixel is 14 px in at y=0, i.e. the CSS
            // `--window-radius: 15px` (the earlier "10" came from a crop
            // that was 14 px off); `.title { font-weight: bold }`
            radius: 15.0,
            button: 24.0,
            control: 34.0,
            icon: 16.0,
            pad: 7.0,
            spacing: 3.0,
            title_pt: 11.0,
            title_weight: 700.0,
            bg_active,
            bg_backdrop,
            fg_active: fg,
            fg_backdrop: fg_back,
            border: if dark {
                rgba(0xff, 0xff, 0xff, 0.10)
            } else {
                rgba(0x00, 0x00, 0x00, 0.08)
            },
            close_hover: rgba(0xc0, 0x1c, 0x28, 1.0),
            accent: rgba(0x35, 0x84, 0xe4, 1.0),
        }
    }
}

/// What the header shows.
#[derive(Clone, Debug)]
pub struct HeaderState {
    pub title: String,
    /// The window has the desktop's keyboard focus.
    pub active: bool,
    pub dark: bool,
    pub layout: Layout,
    /// The pointer is over this button (null when not on a button).
    pub hover: Option<Button>,
    /// This button is held down.
    pub pressed: Option<Button>,
    /// The titlebar font description from GNOME (`titlebar-font`), when the
    /// user's setting overrides the system font.
    pub font: Option<String>,
}

impl Default for HeaderState {
    fn default() -> Self {
        HeaderState {
            title: String::new(),
            active: true,
            dark: false,
            layout: Layout::default(),
            hover: None,
            pressed: None,
            font: None,
        }
    }
}

/// The rectangle of each drawn button in surface coordinates, left to right
/// order: (button, x, y, w, h).
pub fn button_rects(
    m: &Metrics,
    surface_w: f64,
    layout: &Layout,
) -> Vec<(Button, f64, f64, f64, f64)> {
    let mut out = Vec::with_capacity(layout.left.len() + layout.right.len());
    // `windowcontrols > button { min-width: 24px; padding: 5px }` makes the
    // clickable control 34 px and the visible circle 24 px; `border-spacing:
    // 3px` then gives the native 37 px pitch.  GTK snaps the circle to
    // half-pixels (native columns 685..708 at width 720), hence the +0.5.
    let y = ((f64::from(m.height) - m.control) / 2.0).round();
    let mut x = m.pad + 0.5;
    for &b in &layout.left {
        out.push((b, x, y, m.control, m.control));
        x += m.control + m.spacing;
    }
    let mut x = surface_w - m.pad - m.control + 0.5;
    for &b in layout.right.iter().rev() {
        out.push((b, x, y, m.control, m.control));
        x -= m.control + m.spacing;
    }
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

/// The button under a surface point, if any.
pub fn hit_button(m: &Metrics, surface_w: f64, layout: &Layout, p: (f64, f64)) -> Option<Button> {
    button_rects(m, surface_w, layout)
        .into_iter()
        .find(|&(_, x, y, w, h)| p.0 >= x && p.0 < x + w && p.1 >= y && p.1 < y + h)
        .map(|(b, ..)| b)
}

// ---------------------------------------------------------------- painting

type CairoSurface = *mut c_void;
type Cairo = *mut c_void;
type PangoLayout = *mut c_void;
type PangoFontDescription = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PangoRectangle {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
}

const CAIRO_FORMAT_ARGB32: c_int = 0;
const CAIRO_OPERATOR_OVER: c_int = 2;
const CAIRO_OPERATOR_SOURCE: c_int = 1;
const CAIRO_FILL_RULE_EVEN_ODD: c_int = 1;
const PANGO_ELLIPSIZE_END: c_int = 3;
const PANGO_ALIGN_CENTER: c_int = 1;
const PANGO_SCALE: c_int = 1024;

/// cairo + pango + gobject, opened by `dlopen` at start. Every symbol is
/// resolved once; a missing library or symbol is an error the caller logs and
/// turns into "frames without titles" (geometry and input still work).
pub struct Painter {
    metrics: Metrics,
    dark: bool,
    cairo: libloading::Library,
    pango: libloading::Library,
    gobject: libloading::Library,
    image_surface_create_for_data:
        unsafe extern "C" fn(*mut u8, c_int, c_int, c_int, c_int) -> CairoSurface,
    create: unsafe extern "C" fn(CairoSurface) -> Cairo,
    destroy: unsafe extern "C" fn(Cairo),
    surface_destroy: unsafe extern "C" fn(CairoSurface),
    surface_flush: unsafe extern "C" fn(CairoSurface),
    surface_mark_dirty: unsafe extern "C" fn(CairoSurface),
    set_source_rgba: unsafe extern "C" fn(Cairo, f64, f64, f64, f64),
    set_operator: unsafe extern "C" fn(Cairo, c_int),
    paint: unsafe extern "C" fn(Cairo),
    rectangle: unsafe extern "C" fn(Cairo, f64, f64, f64, f64),
    fill: unsafe extern "C" fn(Cairo),
    set_fill_rule: unsafe extern "C" fn(Cairo, c_int),
    new_sub_path: unsafe extern "C" fn(Cairo),
    arc: unsafe extern "C" fn(Cairo, f64, f64, f64, f64, f64),
    close_path: unsafe extern "C" fn(Cairo),
    move_to: unsafe extern "C" fn(Cairo, f64, f64),
    line_to: unsafe extern "C" fn(Cairo, f64, f64),
    curve_to: unsafe extern "C" fn(Cairo, f64, f64, f64, f64, f64, f64),
    stroke: unsafe extern "C" fn(Cairo),
    set_line_width: unsafe extern "C" fn(Cairo, f64),
    set_antialias: unsafe extern "C" fn(Cairo, c_int),
    save: unsafe extern "C" fn(Cairo),
    restore: unsafe extern "C" fn(Cairo),
    pango_create_layout: unsafe extern "C" fn(Cairo) -> PangoLayout,
    layout_set_text: unsafe extern "C" fn(PangoLayout, *const c_char, c_int),
    layout_set_font_description: unsafe extern "C" fn(PangoLayout, PangoFontDescription),
    layout_set_ellipsize: unsafe extern "C" fn(PangoLayout, c_int),
    layout_set_width: unsafe extern "C" fn(PangoLayout, c_int),
    layout_set_alignment: unsafe extern "C" fn(PangoLayout, c_int),
    layout_get_pixel_extents:
        unsafe extern "C" fn(PangoLayout, *mut PangoRectangle, *mut PangoRectangle),
    pango_show_layout: unsafe extern "C" fn(Cairo, PangoLayout),
    font_description_from_string: unsafe extern "C" fn(*const c_char) -> PangoFontDescription,
    font_description_free: unsafe extern "C" fn(PangoFontDescription),
    g_object_unref: unsafe extern "C" fn(*mut c_void),
}

unsafe fn symbol<T: Copy>(lib: &libloading::Library, name: &str) -> Result<T, String> {
    let bytes = name.as_bytes();
    // SAFETY: `get` copies the symbol address; the caller stores it with the
    // exact C signature of that symbol.
    unsafe { lib.get::<T>(bytes) }
        .map(|s| *s)
        .map_err(|e| format!("{name}: {e}"))
}

impl Painter {
    /// Loads cairo/pango and picks the Adwaita metrics.
    pub fn load(dark: bool) -> Result<Painter, String> {
        // SAFETY: the libraries are opened for the whole life of `Painter`
        // (declared after them in the struct, so they are dropped last), and
        // every use below goes through the function pointers resolved here.
        unsafe {
            let cairo = libloading::Library::new("libcairo.so.2")
                .map_err(|e| format!("libcairo.so.2: {e}"))?;
            let pango = libloading::Library::new("libpangocairo-1.0.so.0")
                .map_err(|e| format!("libpangocairo-1.0.so.0: {e}"))?;
            let gobject = libloading::Library::new("libgobject-2.0.so.0")
                .map_err(|e| format!("libgobject-2.0.so.0: {e}"))?;
            Ok(Painter {
                metrics: Metrics::adwaita(50, dark),
                dark,
                image_surface_create_for_data: symbol(
                    &cairo,
                    "cairo_image_surface_create_for_data",
                )?,
                create: symbol(&cairo, "cairo_create")?,
                destroy: symbol(&cairo, "cairo_destroy")?,
                surface_destroy: symbol(&cairo, "cairo_surface_destroy")?,
                surface_flush: symbol(&cairo, "cairo_surface_flush")?,
                surface_mark_dirty: symbol(&cairo, "cairo_surface_mark_dirty")?,
                set_source_rgba: symbol(&cairo, "cairo_set_source_rgba")?,
                set_operator: symbol(&cairo, "cairo_set_operator")?,
                paint: symbol(&cairo, "cairo_paint")?,
                rectangle: symbol(&cairo, "cairo_rectangle")?,
                fill: symbol(&cairo, "cairo_fill")?,
                set_fill_rule: symbol(&cairo, "cairo_set_fill_rule")?,
                new_sub_path: symbol(&cairo, "cairo_new_sub_path")?,
                arc: symbol(&cairo, "cairo_arc")?,
                close_path: symbol(&cairo, "cairo_close_path")?,
                move_to: symbol(&cairo, "cairo_move_to")?,
                line_to: symbol(&cairo, "cairo_line_to")?,
                curve_to: symbol(&cairo, "cairo_curve_to")?,
                stroke: symbol(&cairo, "cairo_stroke")?,
                set_line_width: symbol(&cairo, "cairo_set_line_width")?,
                set_antialias: symbol(&cairo, "cairo_set_antialias")?,
                save: symbol(&cairo, "cairo_save")?,
                restore: symbol(&cairo, "cairo_restore")?,
                pango_create_layout: symbol(&pango, "pango_cairo_create_layout")?,
                layout_set_text: symbol(&pango, "pango_layout_set_text")?,
                layout_set_font_description: symbol(&pango, "pango_layout_set_font_description")?,
                layout_set_ellipsize: symbol(&pango, "pango_layout_set_ellipsize")?,
                layout_set_width: symbol(&pango, "pango_layout_set_width")?,
                layout_set_alignment: symbol(&pango, "pango_layout_set_alignment")?,
                layout_get_pixel_extents: symbol(&pango, "pango_layout_get_pixel_extents")?,
                pango_show_layout: symbol(&pango, "pango_cairo_show_layout")?,
                font_description_from_string: symbol(&pango, "pango_font_description_from_string")?,
                font_description_free: symbol(&pango, "pango_font_description_free")?,
                g_object_unref: symbol(&gobject, "g_object_unref")?,
                cairo,
                pango,
                gobject,
            })
        }
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    #[allow(dead_code)] // settings/look changes (M3)
    pub fn set_metrics(&mut self, m: Metrics) {
        self.metrics = m;
    }

    /// Paints a header of `width`x`height` into `pixels` (ARGB8888,
    /// premultiplied, `stride` bytes per row). `height` is normally
    /// [`HEADER_HEIGHT`].
    pub fn paint(
        &self,
        pixels: &mut [u8],
        width: u16,
        height: u16,
        stride: usize,
        st: &HeaderState,
    ) {
        let (w, h) = (f64::from(width), f64::from(height));
        if w <= 0.0 || h <= 0.0 || stride < width as usize * 4 {
            return;
        }
        let m = if st.dark == self.dark {
            self.metrics
        } else {
            Metrics::adwaita(50, st.dark)
        };
        // SAFETY: all pointers are the cairo/pango entry points resolved in
        // `load`; `pixels` is the caller's mapped wl_shm buffer, at least
        // `stride * height` bytes (checked by the caller), and every cairo
        // object created here is destroyed below.
        unsafe {
            let surface = (self.image_surface_create_for_data)(
                pixels.as_mut_ptr(),
                CAIRO_FORMAT_ARGB32,
                c_int::from(width),
                c_int::from(height),
                stride as c_int,
            );
            if surface.is_null() {
                return;
            }
            let cr = (self.create)(surface);
            if cr.is_null() {
                (self.surface_destroy)(surface);
                return;
            }
            (self.set_antialias)(cr, 6); // CAIRO_ANTIALIAS_BEST

            // Rounded top corners, square bottom (the header joins the
            // content); outside the radius is transparent.
            let r = m.radius.min(h / 2.0).min(w / 2.0);
            (self.new_sub_path)(cr);
            // quarter circles as cubic Beziers (k = 4*(sqrt(2)-1)/3):
            // cairo_arc is avoided here because on some cairo builds the
            // arc's implicit line-to produced a notched right edge.
            const K: f64 = 0.552_284_749_830_793_4;
            (self.move_to)(cr, r, 0.0);
            (self.line_to)(cr, w - r, 0.0);
            (self.curve_to)(cr, w - r + K * r, 0.0, w, r - K * r, w, r);
            (self.line_to)(cr, w, h);
            (self.line_to)(cr, 0.0, h);
            (self.line_to)(cr, 0.0, r);
            (self.curve_to)(cr, 0.0, r - K * r, r - K * r, 0.0, r, 0.0);
            (self.close_path)(cr);
            let (bg, fg) = if st.active {
                (m.bg_active, m.fg_active)
            } else {
                (m.bg_backdrop, m.fg_backdrop)
            };
            (self.set_operator)(cr, CAIRO_OPERATOR_SOURCE);
            (self.set_source_rgba)(cr, bg[0], bg[1], bg[2], bg[3]);
            (self.fill)(cr);
            // 1 px border while active
            if st.active {
                (self.set_operator)(cr, CAIRO_OPERATOR_OVER);
                (self.set_source_rgba)(cr, m.border[0], m.border[1], m.border[2], m.border[3]);
                (self.set_line_width)(cr, 1.0);
                const K: f64 = 0.552_284_749_830_793_4;
                (self.new_sub_path)(cr);
                (self.move_to)(cr, r, 0.5);
                (self.line_to)(cr, w - r, 0.5);
                (self.curve_to)(cr, w - r + K * r, 0.5, w - 0.5, r - K * r, w - 0.5, r);
                (self.line_to)(cr, w - 0.5, h);
                (self.line_to)(cr, 0.5, h);
                (self.line_to)(cr, 0.5, r);
                (self.curve_to)(cr, 0.5, r - K * r, r - K * r, 0.5, r, 0.5);
                (self.stroke)(cr);
            }

            let layout = st.layout.allowed(&Decision {
                framed: true,
                why: Why::Normal,
                move_: true,
                resize: true,
                close: true,
                minimize: true,
                maximize: true,
            });
            let rects = button_rects(&m, w, &layout);
            for &(b, bx, by, bw, bh) in &rects {
                if matches!(b, Button::Spacer) {
                    continue;
                }
                let hovered = st.hover == Some(b);
                let pressed = st.pressed == Some(b);
                // libadwaita's windowcontrols draw a 24 px circle always
                // (`currentColor 10%`), 15% on hover and 30% when active;
                // close turns red on hover
                let color = if b == Button::Close && (hovered || pressed) {
                    m.close_hover
                } else if pressed {
                    [fg[0], fg[1], fg[2], 0.30]
                } else if hovered {
                    [fg[0], fg[1], fg[2], 0.15]
                } else {
                    [fg[0], fg[1], fg[2], 0.10]
                };
                (self.set_operator)(cr, CAIRO_OPERATOR_OVER);
                (self.set_source_rgba)(cr, color[0], color[1], color[2], color[3]);
                let (cx, cy) = (bx + bw / 2.0, by + bh / 2.0);
                (self.new_sub_path)(cr);
                (self.arc)(cr, cx, cy, m.button / 2.0, 0.0, 2.0 * std::f64::consts::PI);
                (self.fill)(cr);
                let glyph = if b == Button::Close && (hovered || pressed) {
                    [1.0, 1.0, 1.0, 1.0]
                } else {
                    fg
                };
                self.glyph(cr, b, cx, cy, m.icon, glyph);
            }
            // `headerbar { box-shadow: inset 0 -1px headerbar-shade }`
            (self.set_operator)(cr, CAIRO_OPERATOR_OVER);
            let shade = if st.dark {
                [0.0, 0.0, 0.0, 0.36]
            } else {
                [0.0, 0.0, 0.0, 0.12]
            };
            (self.set_source_rgba)(cr, shade[0], shade[1], shade[2], shade[3]);
            (self.rectangle)(cr, 0.0, h - 1.0, w, 1.0);
            (self.fill)(cr);

            // Title: GtkHeaderBar centres it in the *window*, so the
            // available band is between the left/right button groups (plus
            // the CSS 6 px box margin and 12 px title padding) but the text
            // is centred at `w / 2`, not at the band's centre (measured on
            // the native header: 'xterm' ink 330..389 in a 720 px window).
            let left_group = rects.iter().filter(|r| r.1 < w / 2.0).map(|r| r.1 + r.3);
            let right_group = rects.iter().filter(|r| r.1 >= w / 2.0).map(|r| r.1);
            let left_end = left_group.fold(0.0f64, f64::max) + 6.0 + 12.0;
            let right_start = right_group.fold(w, f64::min) - 6.0 - 12.0;
            let tw = (right_start - left_end).max(10.0);
            // The titlebar font from GNOME (`titlebar-font` /
            // `titlebar-uses-system-font` + `font-name`), already carrying
            // Bold; the design's Cantarell only when no setting arrived.
            let font = st.font.clone().unwrap_or_else(|| {
                if m.title_weight >= 600.0 {
                    format!("Cantarell Bold {}", m.title_pt)
                } else {
                    format!("Cantarell {}", m.title_pt)
                }
            });
            let font = CString::new(font).unwrap_or_default();
            let desc = (self.font_description_from_string)(font.as_ptr());
            let pl = (self.pango_create_layout)(cr);
            if !desc.is_null() && !pl.is_null() {
                (self.layout_set_font_description)(pl, desc);
                (self.layout_set_text)(
                    pl,
                    st.title.as_ptr().cast::<c_char>(),
                    st.title.len().min(i32::MAX as usize) as c_int,
                );
                (self.layout_set_ellipsize)(pl, PANGO_ELLIPSIZE_END);
                (self.layout_set_width)(pl, (tw * PANGO_SCALE as f64) as c_int);
                (self.layout_set_alignment)(pl, PANGO_ALIGN_CENTER);
                let mut ink = PangoRectangle::default();
                let mut logical = PangoRectangle::default();
                (self.layout_get_pixel_extents)(pl, &mut ink, &mut logical);
                let _ = ink;
                let ty = (h - f64::from(logical.height)) / 2.0;
                (self.set_operator)(cr, CAIRO_OPERATOR_OVER);
                (self.set_source_rgba)(cr, fg[0], fg[1], fg[2], fg[3]);
                (self.move_to)(cr, (w - tw) / 2.0, ty);
                (self.pango_show_layout)(cr, pl);
            }
            if !pl.is_null() {
                (self.g_object_unref)(pl.cast::<c_void>());
            }
            if !desc.is_null() {
                (self.font_description_free)(desc);
            }

            (self.surface_flush)(surface);
            (self.surface_mark_dirty)(surface);
            (self.destroy)(cr);
            (self.surface_destroy)(surface);
        }
    }

    /// One symbolic glyph, centred at `(cx, cy)` and `size` px across.
    ///
    /// The shapes are the Yaru (Ubuntu's Adwaita) 16 px symbolic icons the
    /// native header uses -- `window-close-symbolic` (a filled 10-point
    /// outline), `window-minimize-symbolic` (a filled 8x1 bar) and
    /// `window-maximize-symbolic` (an 8x8 outline, even-odd) -- mapped from
    /// the icon's 16 px viewBox, not hand-tuned strokes.  That is why the
    /// glyph ink now matches GTK's instead of being a thinner approximation.
    unsafe fn glyph(&self, cr: Cairo, b: Button, cx: f64, cy: f64, size: f64, color: [f64; 4]) {
        let s = size / 16.0;
        let ox = cx - size / 2.0;
        let oy = cy - size / 2.0;
        let x = |v: f64| ox + v * s;
        let y = |v: f64| oy + v * s;
        unsafe {
            (self.set_source_rgba)(cr, color[0], color[1], color[2], color[3]);
            match b {
                Button::Close => {
                    // Yaru `window-close-symbolic.svg` outline verbatim
                    let pts = [
                        (4.795, 3.912),
                        (3.912, 4.795),
                        (4.059, 4.941),
                        (7.117, 8.0),
                        (4.060, 11.059),
                        (3.913, 11.205),
                        (4.796, 12.088),
                        (4.942, 11.941),
                        (8.000, 8.883),
                        (11.059, 11.941),
                        (11.205, 12.088),
                        (12.088, 11.205),
                        (11.941, 11.059),
                        (8.883, 8.0),
                        (11.941, 4.941),
                        (12.088, 4.795),
                        (11.205, 3.912),
                        (11.059, 4.059),
                        (8.000, 7.117),
                        (4.941, 4.059),
                    ];
                    (self.set_fill_rule)(cr, 0); // CAIRO_FILL_RULE_WINDING
                    (self.new_sub_path)(cr);
                    (self.move_to)(cr, x(pts[0].0), y(pts[0].1));
                    for (px, py) in &pts[1..] {
                        (self.line_to)(cr, x(*px), y(*py));
                    }
                    (self.close_path)(cr);
                    (self.fill)(cr);
                }
                Button::Minimize => {
                    // Yaru `window-minimize-symbolic.svg`: `m4 10v1h8v-1z`,
                    // an 8x1 bar (the old 2 px rectangle was visibly thick).
                    (self.set_fill_rule)(cr, 0);
                    (self.new_sub_path)(cr);
                    (self.rectangle)(cr, x(4.0), y(10.0), x(12.0) - x(4.0), y(11.0) - y(10.0));
                    (self.fill)(cr);
                }
                Button::Maximize => {
                    (self.set_fill_rule)(cr, CAIRO_FILL_RULE_EVEN_ODD);
                    (self.new_sub_path)(cr);
                    (self.rectangle)(cr, x(4.0), y(4.0), x(12.0) - x(4.0), y(12.0) - y(4.0));
                    (self.rectangle)(cr, x(5.0), y(5.0), x(11.0) - x(5.0), y(11.0) - y(5.0));
                    (self.fill)(cr);
                }
                Button::Menu => {
                    // `open-menu-symbolic`: three 1 px bars, 8 px wide
                    (self.set_fill_rule)(cr, 0);
                    for i in -1..=1 {
                        let yy = 8.0 + f64::from(i) * 3.0;
                        (self.new_sub_path)(cr);
                        (self.rectangle)(cr, x(4.0), y(yy - 0.5), x(12.0) - x(4.0), s);
                    }
                    (self.fill)(cr);
                }
                Button::Spacer => {}
            }
        }
    }

    /// The library versions, for the doctor.
    pub fn versions(&self) -> (String, String) {
        // SAFETY: both libraries stay open for `self`'s life.
        unsafe {
            let cairo = self
                .cairo
                .get::<unsafe extern "C" fn() -> c_int>(b"cairo_version\0")
                .map(|f| *f)
                .ok()
                .map(|f| {
                    let v = f();
                    format!("{}.{}.{}", v / 10000, (v / 100) % 100, v % 100)
                })
                .unwrap_or_else(|| "?".into());
            let pango = self
                .pango
                .get::<unsafe extern "C" fn() -> c_int>(b"pango_version\0")
                .map(|f| *f)
                .ok()
                .map(|f| {
                    let v = f();
                    format!("{}.{}.{}", v / 10000, (v / 100) % 100, v % 100)
                })
                .unwrap_or_else(|| "?".into());
            (cairo, pango)
        }
    }
}

// Silence "field is never read": the libraries are held to keep the symbols
// valid (dropping them would unmap the code the function pointers point at).
const _: fn() = || {
    fn assert_fields(p: &Painter) {
        let _ = &p.pango;
        let _ = &p.gobject;
        let _ = &p.set_operator;
        let _ = &p.set_antialias;
        let _ = &p.save;
        let _ = &p.restore;
        let _ = &p.paint;
        let _ = &p.set_line_width;
    }
    let _ = assert_fields;
};

/// Rearranges a freshly read X pixmap (packed rows of `content` width) at
/// `base` in a surface buffer whose rows are `surface_w` wide: moves each row
/// to the surface stride (bottom-up, the destination is longer), clears the
/// transparent margins and, for a depth-24 client, makes the content opaque
/// for ARGB32. The parent buffer's header band is above `base` and is left
/// alone on purpose: it belongs to the header's own surface.
pub fn arrange_content(
    buf: &mut [u8],
    base: usize,
    surface_w: u16,
    content: (u16, u16),
    margin: u16,
    opaque: bool,
    radius: f64,
) {
    let sw = surface_w as usize;
    let cw = content.0 as usize;
    let ch = content.1 as usize;
    let m = margin as usize;
    let sstride = sw * 4;
    let cstride = cw * 4;
    if cw == 0 || ch == 0 || sstride < cstride || base + ch * cstride > buf.len() {
        return;
    }
    for r in (0..ch).rev() {
        let from = base + r * cstride;
        let to = base + r * sstride;
        if to + cstride > buf.len() {
            continue;
        }
        buf.copy_within(from..from + cstride, to);
    }
    for r in 0..ch {
        // `base` is the content's first row at its x offset; the surface row
        // starts `margin` pixels before it
        let row = base + r * sstride - m * 4;
        if row + sstride > buf.len() {
            break;
        }
        for b in &mut buf[row..row + m * 4] {
            *b = 0;
        }
        for b in &mut buf[row + (m + cw) * 4..row + sstride] {
            *b = 0;
        }
        if opaque {
            for px in 0..cw {
                buf[row + (m + px) * 4 + 3] = 0xff;
            }
        }
    }
    round_bottom(
        &mut buf[base - m * 4..],
        content.0,
        content.1,
        sstride,
        (m, 0),
        radius,
    );
}

/// Rounds the bottom corners of the content buffer (the window's own bottom
/// corners, as an Adwaita window has them) by making the pixels outside the
/// radius transparent. `pixels` is the content region, `origin` its offset in
/// the surface buffer and `stride` the surface stride.
pub fn round_bottom(
    pixels: &mut [u8],
    width: u16,
    height: u16,
    stride: usize,
    origin: (usize, usize),
    radius: f64,
) {
    let (w, h) = (width as usize, height as usize);
    let r = radius.max(0.0) as usize;
    if r == 0 || w < 2 * r || h < r || stride < w * 4 {
        return;
    }
    for y in h - r..h {
        let dy = (h - 1 - y) as f64 + 0.5;
        let inset = r as f64
            - (r as f64 * r as f64 - (r as f64 - dy) * (r as f64 - dy))
                .max(0.0)
                .sqrt();
        let n = inset.round() as usize;
        for x in 0..n.min(w) {
            if let Some(px) = pixels.get_mut((origin.1 + y) * stride + (origin.0 + x) * 4 + 3) {
                *px = 0;
            }
        }
        for x in w.saturating_sub(n)..w {
            if let Some(px) = pixels.get_mut((origin.1 + y) * stride + (origin.0 + x) * 4 + 3) {
                *px = 0;
            }
        }
    }
}

// ------------------------------------------------------------- drop shadow

/// Space reserved around the window geometry for the client-side drop shadow,
/// in logical pixels.  GTK's extents for `0 3px 9px 1px` at scale 1 are 15 px
/// sideways and 18 px down (GSK's triple-box blur); 20 covers both and is one
/// value for active and backdrop, so a focus change never reallocates.
pub const SHADOW_PAD: i32 = 26;

/// One rendered drop shadow.  The CSS values are GTK's `window.csd`
/// (`0 3px 9px 1px rgba(0,0,0,.5)` and the backdrop
/// `0 2px 6px 2px rgba(0,0,0,.2)`); `sigma` is the Gaussian fitted to the
/// field a native libadwaita window actually renders on GNOME 50.1 (the
/// measured field is symmetric top/bottom, see the report).  The 1 px ring is
/// the CSS `0 0 0 1px` shadow.
#[derive(Clone, Copy, Debug)]
pub struct ShadowSpec {
    /// CSS blur radius (the fitted `sigma` is what is rendered; kept so the
    /// table reads as GTK's CSS).
    #[allow(dead_code)]
    pub blur: f64,
    /// Opacity of the blurred layer at the window edge.
    pub alpha: f64,
    /// The crisp 1 px ring just outside the window shape.
    pub ring: f64,
    pub sigma: f64,
}

/// The measured active/backdrop shadow of GNOME 50.1's libadwaita window.
pub fn shadow_spec(active: bool) -> ShadowSpec {
    if active {
        ShadowSpec {
            blur: 9.0,
            alpha: 0.39,
            ring: 0.23,
            sigma: 6.0,
        }
    } else {
        ShadowSpec {
            blur: 6.0,
            alpha: 0.22,
            ring: 0.18,
            sigma: 4.5,
        }
    }
}

/// `erfc` (A&S 7.1.26, |error| < 1.5e-7): the Gaussian-blurred edge profile.
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let y = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let y = y * (-z * z).exp();
    if x >= 0.0 { y } else { 2.0 - y }
}

/// Signed distance to a `w`x`h` rounded rectangle of radius `r` whose top
/// left is (0,0); negative inside.  Used for the window shape and its shadow.
pub fn rounded_sdf(x: f64, y: f64, w: f64, h: f64, r: f64) -> f64 {
    let r = r.min(w / 2.0).min(h / 2.0);
    let qx = (x - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (y - h / 2.0).abs() - (h / 2.0 - r);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
}

/// Paints the window's drop shadow into an ARGB8888 buffer of
/// `width`x`height`: the window geometry is inset by [`SHADOW_PAD`] on every
/// side, and the shadow lives outside its rounded shape (the visible corner
/// cuts show it).  The buffer is transparent inside the window: the parent
/// surface covers it there.  Painted once per size, focus or scheme change,
/// never per client frame.
pub fn paint_shadow(
    pixels: &mut [u8],
    width: u16,
    height: u16,
    stride: usize,
    active: bool,
    radius: f64,
) {
    let (bw, bh) = (width as usize, height as usize);
    if bw == 0 || bh == 0 || stride < bw * 4 || pixels.len() < stride * bh {
        return;
    }
    let pad = f64::from(SHADOW_PAD);
    let (ww, wh) = (bw as f64 - 2.0 * pad, bh as f64 - 2.0 * pad);
    if ww <= 1.0 || wh <= 1.0 {
        return;
    }
    let spec = shadow_spec(active);
    let cut = 4.0 * spec.sigma + 2.0;
    pixels[..stride * bh].fill(0);
    for y in 0..bh {
        let row = y * stride;
        let py = y as f64 + 0.5 - pad;
        for x in 0..bw {
            let i = row + x * 4;
            // the parent covers the window; only the outside is ours
            let px = x as f64 + 0.5 - pad;
            let d = rounded_sdf(px, py, ww, wh, radius);
            if d <= 0.0 || d > cut {
                continue;
            }
            // a crisp 1 px band just outside the shape (the CSS
            // `0 0 0 1px` ring): full at the first outside pixel, gone by
            // the second
            let ring = spec.ring * (1.5 - d).clamp(0.0, 1.0);
            // the measured blurred field starts just outside that ring
            let blur = if d >= 1.0 {
                spec.alpha * 0.5 * erfc(d / (spec.sigma * std::f64::consts::SQRT_2))
            } else {
                0.0
            };
            let a = 1.0 - (1.0 - ring) * (1.0 - blur);
            let byte = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            pixels[i] = 0;
            pixels[i + 1] = 0;
            pixels[i + 2] = 0;
            pixels[i + 3] = byte;
        }
    }
}

/// The shadow band's outer size for a surface box.
pub fn shadow_size(surface: (u16, u16)) -> (u16, u16) {
    let p = SHADOW_PAD.max(0) as u32;
    (
        (u32::from(surface.0) + 2 * p).min(u32::from(u16::MAX)) as u16,
        (u32::from(surface.1) + 2 * p).min(u32::from(u16::MAX)) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_geom_round_trip() {
        for &(w, h) in &[(1u16, 1u16), (640, 480), (1920, 1080), (16384, 4000)] {
            for g in [FrameGeom::NONE, FrameGeom::ADWAITA] {
                let (sw, sh) = g.surface_size((w, h));
                let (ox, oy) = g.content_origin();
                assert_eq!(
                    (i32::from(sw) - 2 * ox, i32::from(sh) - oy - g.margin.max(0)),
                    (i32::from(w), i32::from(h)),
                    "{g:?} {w}x{h}"
                );
                let (cx, cy, cw, ch) = g.content_rect(0, 0, i32::from(sw), i32::from(sh));
                assert_eq!((cx, cy), (ox, oy));
                assert_eq!((cw, ch), (i32::from(w), i32::from(h)));
                let (sx, sy, ssw, ssh) = g.surface_rect(0, 0, i32::from(w), i32::from(h));
                assert_eq!((sx, sy), (-ox, -oy));
                assert_eq!((ssw, ssh), (i32::from(sw), i32::from(sh)));
            }
        }
    }

    #[test]
    fn unframed_is_p123() {
        let g = FrameGeom::NONE;
        assert_eq!(g.surface_size((100, 50)), (100, 50));
        assert_eq!(g.content_origin(), (0, 0));
        assert_eq!(g.hit((99.9, 49.9), (100, 50)), Hit::Content);
        assert_eq!(g.hit((100.0, 10.0), (100, 50)), Hit::Outside);
    }

    #[test]
    fn hit_edges_title_content() {
        let g = FrameGeom::ADWAITA;
        let content = (100u16, 50u16);
        assert_eq!(g.hit((50.0, 10.0), content), Hit::Title);
        assert_eq!(g.hit((1.0, 20.0), content), Hit::Edge(Edge::West));
        assert_eq!(g.hit((112.0 - 1.0, 20.0), content), Hit::Edge(Edge::East));
        assert_eq!(g.hit((50.0, 2.0), content), Hit::Edge(Edge::North));
        assert_eq!(g.hit((1.0, 2.0), content), Hit::Edge(Edge::NorthWest));
        assert_eq!(g.hit((111.0, 2.0), content), Hit::Edge(Edge::NorthEast));
        assert_eq!(
            g.hit((50.0, f64::from(g.header) + 25.0), content),
            Hit::Content
        );
        assert_eq!(
            g.hit((50.0, f64::from(g.header) + 50.0), content),
            Hit::Edge(Edge::South)
        );
        assert_eq!(
            g.hit((1.0, f64::from(g.header) + 50.0), content),
            Hit::Edge(Edge::SouthWest)
        );
        assert_eq!(
            g.hit((111.0, f64::from(g.header) + 50.0), content),
            Hit::Edge(Edge::SouthEast)
        );
    }

    #[test]
    fn hit_button_layout() {
        let m = Metrics::adwaita(50, false);
        let l = Layout::parse("appmenu:minimize,maximize,close");
        let rects = button_rects(&m, 400.0, &l);
        assert_eq!(rects.len(), 4);
        let close = rects.iter().find(|r| r.0 == Button::Close).unwrap();
        let p = (close.1 + 1.0, close.2 + 1.0);
        assert_eq!(hit_button(&m, 400.0, &l, p), Some(Button::Close));
        assert_eq!(hit_button(&m, 400.0, &l, (200.0, 10.0)), None);
    }

    #[test]
    fn buttons_match_libadwaita_geometry() {
        // Native GNOME 50.1 at width 720: circles at x 685..708, 648..671,
        // 611..634 (37 px pitch) -> centres 696.5/659.5/622.5, visible
        // diameter 24, clickable control 34 (`min-width 24` + `padding 5`),
        // right edge 720 - 7 (`headerbar` padding) = 713.
        let m = Metrics::adwaita(50, false);
        let l = Layout::parse(":minimize,maximize,close");
        let rects = button_rects(&m, 720.0, &l);
        let close = rects.iter().find(|r| r.0 == Button::Close).unwrap();
        assert_eq!(close.1, 679.0 + 0.5);
        assert_eq!(close.1 + close.3, 713.0 + 0.5);
        assert_eq!(close.2 + close.3 / 2.0, 23.0); // circle centre row (11..34)
        let max = rects.iter().find(|r| r.0 == Button::Maximize).unwrap();
        let min = rects.iter().find(|r| r.0 == Button::Minimize).unwrap();
        assert_eq!(max.1 - min.1, 37.0); // control 34 + border-spacing 3
        // the visible circle is inset 5 px in the control
        assert_eq!(min.1 + 5.0 + m.button, min.1 + m.control - 5.0);
        // and the left side mirrors it
        let left = button_rects(&m, 720.0, &Layout::parse("appmenu:minimize,close"));
        let menu = left.iter().find(|r| r.0 == Button::Menu).unwrap();
        assert_eq!(menu.1, 7.0 + 0.5);
        assert_eq!(menu.1 + 5.0, 12.0 + 0.5);
    }

    #[test]
    fn decision_table_every_type() {
        for (kind, framed) in [
            (None, true),
            (Some("normal"), true),
            (Some("dialog"), true),
            (Some("utility"), true),
            (Some("toolbar"), true),
            (Some("dock"), false),
            (Some("desktop"), false),
            (Some("splash"), false),
            (Some("menu"), false),
            (Some("dropdown_menu"), false),
            (Some("popup_menu"), false),
            (Some("tooltip"), false),
            (Some("notification"), false),
            (Some("combo"), false),
            (Some("dnd"), false),
        ] {
            let d = decide(kind, &[], None, false);
            assert_eq!(d.framed, framed, "{kind:?}");
            if framed {
                assert!(d.close && d.minimize && d.maximize && d.move_ && d.resize);
            }
        }
    }

    #[test]
    fn decision_motif_and_csd_and_input() {
        // decorations 0 (flags: functions|decorations, funcs all, decor 0)
        let d = decide(Some("normal"), &[3, 1, 0, 0, 0], None, false);
        assert!(!d.framed);
        assert_eq!(d.why, Why::MotifNoDecorations);
        // decorations 1, functions without close/maximize
        let funcs = MWM_FUNC_MOVE | MWM_FUNC_RESIZE | MWM_FUNC_MINIMIZE;
        let d = decide(Some("normal"), &[3, funcs, 1, 0, 0], None, false);
        assert!(d.framed && d.move_ && d.resize && d.minimize);
        assert!(!d.close && !d.maximize);
        // functions absent: everything
        let d = decide(Some("normal"), &[2, 0, 1, 0, 0], None, false);
        assert!(d.framed && d.close && d.maximize && d.move_);
        // client-side decorations win over the type
        let d = decide(Some("normal"), &[], Some([0, 0, 40, 0]), false);
        assert!(!d.framed);
        assert_eq!(d.why, Why::ClientSideDecorations);
        // WM_HINTS input false alone still gets a frame (xclock), as an X WM
        let d = decide(Some("normal"), &[], None, true);
        assert!(d.framed);
        // a frameless dialog whose parent is frameless (GIMP docks) inherits
        let d = decide(Some("dialog"), &[3, 1, 0, 0, 0], None, false);
        assert!(!d.framed);
    }

    #[test]
    fn layout_parser() {
        assert!(Layout::parse("").left.is_empty() && Layout::parse("").right.is_empty());
        assert!(
            Layout::parse("(none)").left.is_empty() && Layout::parse("(none)").right.is_empty()
        );
        assert!(Layout::parse(":").left.is_empty() && Layout::parse(":").right.is_empty());
        let l = Layout::parse(":minimize,close");
        assert!(l.left.is_empty());
        assert_eq!(l.right, vec![Button::Minimize, Button::Close]);
        let l = Layout::parse("appmenu:minimize,maximize,close");
        assert_eq!(l.left, vec![Button::Menu]);
        assert_eq!(
            l.right,
            vec![Button::Minimize, Button::Maximize, Button::Close]
        );
        // no colon: everything on the left, as mutter reads it
        let l = Layout::parse("close,minimize");
        assert_eq!(l.left, vec![Button::Close, Button::Minimize]);
        assert!(l.right.is_empty());
        // unknown names are ignored, not invented
        let l = Layout::parse(":close,bogus,maximize");
        assert_eq!(l.left, Vec::new());
        assert_eq!(l.right, vec![Button::Close, Button::Maximize]);
    }

    #[test]
    fn button_allowed_by_functions() {
        let l = Layout::parse(":minimize,maximize,close");
        let d = decide(
            Some("normal"),
            &[3, MWM_FUNC_MOVE | MWM_FUNC_RESIZE | MWM_FUNC_CLOSE, 1, 0, 0],
            None,
            false,
        );
        let a = l.allowed(&d);
        assert_eq!(a.right, vec![Button::Close]);
    }

    #[test]
    fn opaque_and_corners() {
        let mut px = vec![0xffu8; 10 * 5 * 4];
        // rounded bottom corners clear exactly the outside pixels
        round_bottom(&mut px, 10, 5, 40, (0, 0), 2.0);
        assert_eq!(px[(4 * 40) + 3], 0, "bottom-left corner");
        assert_eq!(px[(4 * 40) + 9 * 4 + 3], 0, "bottom-right corner");
        assert_eq!(px[(4 * 40) + 5 * 4 + 3], 0xff, "middle bottom");
    }

    fn shadow_alpha(px: &[u8], stride: usize, x: usize, y: usize) -> u8 {
        px[y * stride + x * 4 + 3]
    }

    #[test]
    fn shadow_is_outside_the_window_only() {
        let (bw, bh) = (120u16, 120u16);
        let stride = bw as usize * 4;
        let mut px = vec![0u8; stride * bh as usize];
        paint_shadow(&mut px, bw, bh, stride, true, 15.0);
        let p = SHADOW_PAD as usize;
        let ww = bw as usize - 2 * p;
        // inside the window geometry (covered by the parent) is transparent
        assert_eq!(shadow_alpha(&px, stride, p + ww / 2, p + ww / 2), 0);
        assert_eq!(shadow_alpha(&px, stride, p + ww / 2, p + 2), 0);
        // 1 px outside the straight edge: the CSS ring
        let ring = shadow_alpha(&px, stride, p + ww / 2, p - 1);
        assert!((55..=62).contains(&ring), "ring alpha {ring}");
        // 2 px outside: the blurred layer starts, and is lighter than the ring
        let near = shadow_alpha(&px, stride, p + ww / 2, p - 2);
        assert!(near > 0 && near <= ring, "near {near} ring {ring}");
        // monotonic falloff along the edge
        let mut last = 0u8;
        for d in 1..18 {
            let a = shadow_alpha(&px, stride, p + ww / 2, p - d);
            assert!(a <= last.max(ring), "d {d}: {a}");
            last = a;
        }
        // the field is symmetric: left/right and top/bottom
        for d in 1..18 {
            assert_eq!(
                shadow_alpha(&px, stride, p - d, p + ww / 2),
                shadow_alpha(&px, stride, p + ww - 1 + d, p + ww / 2),
                "side {d}"
            );
            assert_eq!(
                shadow_alpha(&px, stride, p + ww / 2, p - d),
                shadow_alpha(&px, stride, p + ww / 2, p + ww - 1 + d),
                "top/bottom {d}"
            );
        }
        // nothing beyond the cut
        assert_eq!(shadow_alpha(&px, stride, p + ww / 2, 0), 0);
        assert_eq!(shadow_alpha(&px, stride, 0, p + ww / 2), 0);
        // inside the rounded corner (outside the shape) is shadowed
        assert!(shadow_alpha(&px, stride, p + 2, p + 2) > 0);
    }

    #[test]
    fn painted_header_is_opaque_to_the_right_edge() {
        let Ok(p) = Painter::load(false) else {
            return; // no cairo/pango on this machine; the VM covers it
        };
        let (w, h, stride) = (720u16, 46u16, 720usize * 4);
        let mut px = vec![0u8; stride * usize::from(h)];
        p.paint(&mut px, w, h, stride, &HeaderState::default());
        for x in [0usize, 1, 700, 709, 710, 715, 719] {
            let a = px[23 * stride + x * 4 + 3];
            assert_eq!(a, 0xff, "alpha at x={x}");
        }
        // and the top-right corner is the only transparent part of the top row
        assert_eq!(px[719 * 4 + 3], 0, "rounded top-right corner");
        assert_eq!(
            px[23 * stride + 719 * 4 + 3],
            0xff,
            "right edge at mid height"
        );
    }

    #[test]
    fn backdrop_shadow_is_lighter_and_has_the_css_values() {
        let a = shadow_spec(true);
        let b = shadow_spec(false);
        assert!(b.alpha < a.alpha && b.ring < a.ring && b.blur < a.blur);
        assert!((a.blur - 9.0).abs() < f64::EPSILON);
        assert!((b.blur - 6.0).abs() < f64::EPSILON);
        // the pad covers the rendered field (4 sigma + the 1 px ring)
        assert!(f64::from(SHADOW_PAD) >= 4.0 * a.sigma + 1.0);
    }

    #[test]
    fn rounded_sdf_matches_the_window_shape() {
        assert!(rounded_sdf(50.0, 50.0, 100.0, 100.0, 15.0) < 0.0);
        assert!((rounded_sdf(50.0, 0.0, 100.0, 100.0, 15.0) - 0.0).abs() < 1e-9);
        assert!((rounded_sdf(50.0, -3.0, 100.0, 100.0, 15.0) - 3.0).abs() < 1e-9);
        // the corner is 15 px in from the square corner
        assert!(rounded_sdf(1.0, 1.0, 100.0, 100.0, 15.0) > 0.0);
        assert!(rounded_sdf(15.0, 15.0, 100.0, 100.0, 15.0) < 0.0);
    }

    #[test]
    fn content_rows_move_to_the_surface_stride() {
        // 3x2 content at (1,1) in a 5x4 surface, stride 20 bytes
        let mut buf = vec![0xeeu8; 5 * 4 * 4];
        let base = 6 * 4;
        // the X server wrote the content packed: row 0 = 1,2,3; row 1 = 4,5,6
        for (i, v) in [1u8, 2, 3, 4, 5, 6].iter().enumerate() {
            buf[base + i * 4] = *v;
        }
        arrange_content(&mut buf, base, 5, (3, 2), 1, true, 0.0);
        for (row, vals) in [[1u8, 2, 3], [4, 5, 6]].iter().enumerate() {
            let start = (1 + row) * 5 * 4;
            // left margin transparent
            assert_eq!(&buf[start..start + 4], &[0, 0, 0, 0]);
            for (px, v) in vals.iter().enumerate() {
                let p = start + (1 + px) * 4;
                assert_eq!(buf[p], *v, "row {row} px {px}");
                assert_eq!(buf[p + 3], 0xff, "alpha row {row} px {px}");
            }
            // right margin transparent
            assert_eq!(&buf[start + 4 * 4..start + 5 * 4], &[0, 0, 0, 0]);
        }
    }
}
