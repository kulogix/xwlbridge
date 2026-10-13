//! Input over libei: the EIS socket mutter hands out from `ConnectToEIS`, spoken
//! with the pure-Rust `reis` client.
//!
//! mutter offers a keyboard (with its current xkb keymap), a relative pointer
//! and absolute pointers over the recorded streams: one shared device whose
//! regions are the monitor streams in logical layout coordinates
//! (`RecordMonitor`), or one "standalone" device per area stream
//! (`RecordArea`, our default), whose single region is the stream in its own
//! pixels from 0,0. Each region carries its stream's mapping id, so an X
//! position is sent on the device and region over the stream that shows it
//! (see [`StreamArea`]). The protocol side (handshake, seat binding, device
//! lifecycle) runs on the backend thread; the X connection threads inject
//! through [`GnomePointer`]/[`GnomeKeyboard`], which share the live devices.
//! What they send waits in an [`Outbox`] and goes out in whole-message batches
//! that never overfill the socket (see `outbox.rs`: mutter's libeis can lose
//! input, or hang, on a message split across reads).

use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::EiHandshaker;
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};

use crate::bridge::input::{VirtualKeyboard, VirtualPointer};

use super::Wake;
use super::outbox::{self, Op, Outbox, Pump, Sink};

/// Our end's kernel send buffer: small, so a compositor that stops reading
/// pushes back after ~80 events (which then coalesce in the outbox) instead
/// of ~280 stale ones that it would get all at once afterwards.
const SNDBUF: libc::c_int = 32 * 1024;
/// How long the compositor may not read input before we say so.
const STALL_WARN: Duration = Duration::from_secs(2);

/// One wheel notch in libei's discrete scroll units.
const NOTCH_120: i32 = 120;

fn now_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

/// What happened on the EIS socket that the backend cares about.
#[derive(Default)]
pub struct EisUpdate {
    /// The keyboard device arrived with (or changed to) this xkb keymap text.
    pub keymap: Option<String>,
    /// The keyboard's active layout (xkb group) is now this one.
    pub group: Option<u32>,
    /// The usable devices changed (so input may have become ready, or not).
    pub devices_changed: bool,
    /// The socket is gone; input is dead until a new session.
    pub disconnected: bool,
}

/// Where a recorded stream sits in the compositor's logical layout, under the
/// mapping id mutter gives both the stream and the EIS region over it.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamArea {
    pub mapping_id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// The point moved inside (libeis drops absolute motion outside a region).
    fn clamp(&self, x: f32, y: f32) -> (f32, f32) {
        (
            x.clamp(self.x, self.x + self.w - 1.0),
            y.clamp(self.y, self.y + self.h - 1.0),
        )
    }

    fn dist2(&self, x: f32, y: f32) -> f32 {
        let (cx, cy) = self.clamp(x, y);
        (x - cx) * (x - cx) + (y - cy) * (y - cy)
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}+{}+{}", self.w, self.h, self.x, self.y)
    }
}

/// One region of an absolute pointer: the part of the logical layout it moves
/// the pointer over, and the region itself in the device's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Target {
    layout: Rect,
    region: Rect,
    /// Matched to a recorded stream by mapping id (else taken as layout
    /// coordinates).
    stream: bool,
}

impl Target {
    /// The region coordinates for a layout point, clamped into both.
    fn map(&self, gx: f32, gy: f32) -> (f32, f32) {
        let (lx, ly) = self.layout.clamp(gx, gy);
        let rx = self.region.x + (lx - self.layout.x) * self.region.w / self.layout.w;
        let ry = self.region.y + (ly - self.layout.y) * self.region.h / self.layout.h;
        self.region.clamp(rx, ry)
    }
}

/// The targets of a device's regions, given as `(rect, mapping id)`. A region
/// whose mapping id names a recorded stream covers that stream's area: mutter
/// puts a standalone area device's region at 0,0 in the stream's own
/// (physical) pixels and adds the area's origin back, and puts a monitor
/// stream's region at its place in the layout. A region with no or an unknown
/// mapping id is taken to be in layout coordinates already (what mutter did
/// before mapping ids, and does for a remote-desktop session without streams).
fn targets<'a>(
    regions: impl IntoIterator<Item = (Rect, Option<&'a str>)>,
    areas: &[StreamArea],
) -> Vec<Target> {
    regions
        .into_iter()
        .filter(|(r, _)| r.w >= 1.0 && r.h >= 1.0)
        .map(|(region, id)| {
            let area = id
                .and_then(|id| areas.iter().find(|a| a.mapping_id == id))
                .filter(|a| a.w >= 1 && a.h >= 1);
            match area {
                Some(a) => Target {
                    layout: Rect {
                        x: a.x as f32,
                        y: a.y as f32,
                        w: a.w as f32,
                        h: a.h as f32,
                    },
                    region,
                    stream: true,
                },
                None => Target {
                    layout: region,
                    region,
                    stream: false,
                },
            }
        })
        .collect()
}

/// An emulating absolute pointer and where its regions put the pointer.
struct AbsDevice<D = Device> {
    dev: D,
    targets: Vec<Target>,
    /// It has no regions at all, so libeis takes any point.
    regionless: bool,
}

/// Which device and region point a layout point goes to: the region over the
/// part of the layout containing it, else the nearest (a point in a gap
/// between monitors, or on a monitor that appeared after the session
/// started), clamped. A device without regions takes any point as it is.
fn route<D>(abs: &[AbsDevice<D>], gx: f32, gy: f32) -> Option<(usize, f32, f32)> {
    let all = || {
        abs.iter()
            .enumerate()
            .flat_map(|(i, a)| a.targets.iter().map(move |t| (i, t)))
    };
    let hit = all()
        .find(|(_, t)| t.layout.contains(gx, gy))
        .or_else(|| {
            all().min_by(|a, b| a.1.layout.dist2(gx, gy).total_cmp(&b.1.layout.dist2(gx, gy)))
        });
    if let Some((i, t)) = hit {
        let (x, y) = t.map(gx, gy);
        return Some((i, x, y));
    }
    abs.iter().position(|a| a.regionless).map(|i| (i, gx, gy))
}

#[derive(Default)]
struct Devices {
    /// Every emulating absolute pointer: one per area stream, or one shared
    /// by the monitor streams.
    abs: Vec<AbsDevice>,
    rel: Option<Device>,
    kbd: Option<Device>,
    /// The absolute pointer that moved the pointer last. Buttons and the wheel
    /// go to it, as with the one device a user holds.
    last_abs: Option<Device>,
    /// Devices with events not yet closed by a frame.
    dirty: Vec<Device>,
    /// The device each held button was pressed on: its release must go to the
    /// same one, or mutter ignores it and the button stays down.
    pressed_on: HashMap<u32, Device>,
    /// Buttons held on a device that went away: mutter released them with the
    /// device, so their release from X has nowhere to go.
    released_with_device: Vec<u32>,
}

impl Devices {
    /// The device for buttons and the wheel: the absolute pointer that moved
    /// last, else any absolute pointer, else the relative one.
    fn pointer_dev(&self) -> Option<Device> {
        self.last_abs
            .clone()
            .or_else(|| self.abs.first().map(|a| a.dev.clone()))
            .or_else(|| self.rel.clone())
    }

    fn mark_dirty(&mut self, dev: &Device) {
        if !self.dirty.contains(dev) {
            self.dirty.push(dev.clone());
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// State shared between the backend thread and the X threads' input handles.
pub struct Shared {
    ctx: ei::Context,
    /// The last serial mutter sent; every `frame`/`start_emulating` cites it.
    serial: AtomicU32,
    devices: Mutex<Devices>,
    /// Logical layout origin: X-side absolute motion arrives relative to it,
    /// EIS regions are absolute.
    origin: Mutex<(i32, i32)>,
    /// The session's recorded streams, for matching regions to the layout.
    areas: Vec<StreamArea>,
    /// Events on their way out (lock order: `devices` before `out`).
    out: Mutex<Outbox<Device>>,
    /// Wakes the backend thread to flush on `POLLOUT` or at a pacing deadline.
    wake: Arc<Wake>,
    /// Messages mutter rejected as for an unknown object (logged sparingly).
    invalid: AtomicU64,
}

struct Dev {
    dev: Device,
    emulating: bool,
}

pub struct Eis {
    ctx: ei::Context,
    handshaker: Option<EiHandshaker<'static>>,
    conv: Option<EiEventConverter>,
    devices: Vec<Dev>,
    sequence: u32,
    pub shared: Arc<Shared>,
    /// Protocol problems logged so far (logged sparingly).
    problems: u64,
}

impl Eis {
    pub fn new(
        fd: OwnedFd,
        origin: (i32, i32),
        areas: Vec<StreamArea>,
        wake: Arc<Wake>,
    ) -> std::io::Result<Eis> {
        let batch = outbox::batch_for(set_sndbuf(&fd, SNDBUF));
        crate::vlog!("gnome: EIS writes in batches of up to {batch} bytes");
        let ctx = ei::Context::new(UnixStream::from(fd))?;
        Ok(Eis {
            handshaker: Some(EiHandshaker::new(
                "xwlbridge",
                ei::handshake::ContextType::Sender,
            )),
            conv: None,
            devices: Vec::new(),
            sequence: 0,
            shared: Arc::new(Shared {
                ctx: ctx.clone(),
                serial: AtomicU32::new(0),
                devices: Mutex::new(Devices::default()),
                origin: Mutex::new(origin),
                areas,
                out: Mutex::new(Outbox::new(batch)),
                wake,
                invalid: AtomicU64::new(0),
            }),
            ctx,
            problems: 0,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.ctx.as_fd().as_raw_fd()
    }

    pub fn set_origin(&self, origin: (i32, i32)) {
        *lock(&self.shared.origin) = origin;
    }

    /// Whether a keyboard or pointer is ready to emulate.
    pub fn ready(&self) -> bool {
        let d = lock(&self.shared.devices);
        !d.abs.is_empty() || d.kbd.is_some()
    }

    /// Whether both a keyboard and a pointer are ready to emulate.
    pub fn complete(&self) -> bool {
        let d = lock(&self.shared.devices);
        d.kbd.is_some() && (!d.abs.is_empty() || d.rel.is_some())
    }

    /// Reads and handles whatever the socket has.
    pub fn dispatch(&mut self) -> EisUpdate {
        let mut up = EisUpdate::default();
        if self.ctx.read().is_err() {
            up.disconnected = true;
            return up;
        }
        while let Some(res) = self.ctx.pending_event() {
            let ev = match res {
                reis::PendingRequestResult::Request(ev) => ev,
                other => {
                    self.problems += 1;
                    if self.problems.is_power_of_two() {
                        crate::warning!("gnome: EIS protocol problem ({}x so far): {other:?}", self.problems);
                    }
                    continue;
                }
            };
            // mutter got a message for an object it doesn't know: normal for
            // a device it just removed, a sign of trouble in numbers
            if let ei::Event::Connection(_, ei::connection::Event::InvalidObject { invalid_id, .. }) = &ev {
                let n = self.shared.invalid.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() && n >= 4 {
                    crate::warning!(
                        "gnome: mutter rejected {n} input messages for unknown objects (last: {invalid_id:#x})"
                    );
                } else {
                    crate::vlog!("gnome: mutter: invalid object {invalid_id:#x}");
                }
                continue;
            }
            if let Some(h) = self.handshaker.as_mut() {
                match h.handle_event(ev) {
                    Ok(Some(resp)) => {
                        crate::vlog!("gnome: EIS connected ({:?})", resp.negotiated_interfaces);
                        self.conv = Some(EiEventConverter::new(&self.ctx, resp));
                        self.handshaker = None;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::warning!("gnome: EIS handshake failed: {e}");
                        up.disconnected = true;
                        return up;
                    }
                }
                continue;
            }
            let Some(conv) = self.conv.as_mut() else { continue };
            if let Err(e) = conv.handle_event(ev) {
                crate::warning!("gnome: EIS event error: {e:?}");
            }
            while let Some(ev) = conv.next_event() {
                match ev {
                    EiEvent::SeatAdded(e) => e.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll,
                    ),
                    EiEvent::DeviceAdded(e) => {
                        if let Some(km) = e.device.keymap()
                            && let Some(text) = read_keymap(&km.fd, km.size)
                        {
                            // mutter recreates the keyboard when the keymap
                            // changes (other layouts or options); switching
                            // between the layouts of one keymap, which is what
                            // GNOME's input-source switch does, only changes
                            // the group (below)
                            up.keymap = Some(text);
                        }
                        self.devices.push(Dev {
                            dev: e.device.clone(),
                            emulating: false,
                        });
                    }
                    EiEvent::DeviceResumed(e) => {
                        if let Some(d) = self.devices.iter_mut().find(|d| d.dev == e.device) {
                            d.dev.device().start_emulating(e.serial, self.sequence);
                            self.sequence = self.sequence.wrapping_add(1);
                            d.emulating = true;
                            up.devices_changed = true;
                        }
                    }
                    EiEvent::DevicePaused(e) => {
                        if let Some(d) = self.devices.iter_mut().find(|d| d.dev == e.device) {
                            d.emulating = false;
                            up.devices_changed = true;
                        }
                    }
                    EiEvent::DeviceRemoved(e) => {
                        self.devices.retain(|d| d.dev != e.device);
                        up.devices_changed = true;
                    }
                    // the layout in effect: sent once the keyboard exists,
                    // and on every input-source switch
                    EiEvent::KeyboardModifiers(m) => up.group = Some(m.group),
                    EiEvent::Disconnected(d) => {
                        crate::vlog!("gnome: EIS disconnected: {d:?}");
                        up.disconnected = true;
                    }
                    _ => {}
                }
            }
            self.shared
                .serial
                .store(conv.connection().serial(), Ordering::Release);
        }
        if up.devices_changed {
            self.publish_devices();
        }
        // our replies (handshake, pongs, start_emulating), with any input
        self.shared.pump();
        up
    }

    /// Picks the emulating devices for each role and hands them to the input
    /// handles.
    fn publish_devices(&self) {
        let pick = |cap: DeviceCapability| {
            self.devices
                .iter()
                .find(|d| d.emulating && d.dev.has_capability(cap))
                .map(|d| d.dev.clone())
        };
        let abs: Vec<AbsDevice> = self
            .devices
            .iter()
            .filter(|d| d.emulating && d.dev.has_capability(DeviceCapability::PointerAbsolute))
            .map(|d| AbsDevice {
                targets: targets(
                    d.dev.regions().iter().map(|r| {
                        let rect = Rect {
                            x: r.x as f32,
                            y: r.y as f32,
                            w: r.width as f32,
                            h: r.height as f32,
                        };
                        (rect, r.mapping_id.as_deref())
                    }),
                    &self.shared.areas,
                ),
                regionless: d.dev.regions().is_empty(),
                dev: d.dev.clone(),
            })
            .collect();
        let exists = |dev: &Device| self.devices.iter().any(|d| d.dev == *dev);
        let emulating = |dev: &Device| self.devices.iter().any(|d| d.emulating && d.dev == *dev);
        let mut d = lock(&self.shared.devices);
        let same = d.abs.len() == abs.len()
            && d.abs.iter().zip(&abs).all(|(a, b)| a.dev == b.dev && a.targets == b.targets);
        if !same {
            for a in &abs {
                let name = a.dev.name().unwrap_or("?");
                if a.targets.is_empty() {
                    crate::vlog!("gnome: EIS absolute pointer '{name}': no regions");
                }
                for t in &a.targets {
                    crate::vlog!(
                        "gnome: EIS absolute pointer '{name}': region {} covers {}{}",
                        t.region,
                        t.layout,
                        if t.stream { "" } else { " (no stream of ours: taken as layout coordinates)" }
                    );
                }
            }
        }
        d.abs = abs;
        d.rel = pick(DeviceCapability::Pointer);
        d.kbd = pick(DeviceCapability::Keyboard);
        if d.last_abs.as_ref().is_some_and(|dev| !emulating(dev)) {
            d.last_abs = None;
        }
        d.dirty.retain(|dev| emulating(dev));
        // events for a device that went away (or paused) can't go anywhere
        // (mutter released what it held itself)
        lock(&self.shared.out).retain_devices(|dev| emulating(dev));
        let gone: Vec<u32> = d
            .pressed_on
            .iter()
            .filter(|(_, dev)| !exists(dev))
            .map(|(&b, _)| b)
            .collect();
        for b in gone {
            d.pressed_on.remove(&b);
            d.released_with_device.push(b);
        }
    }

    /// The handles to install into [`Input`](crate::bridge::input::Input).
    pub fn handles(&self) -> (Box<dyn Fn() + Send + Sync>, GnomePointer, GnomeKeyboard) {
        let shared = self.shared.clone();
        (
            Box::new(move || {
                shared.pump();
            }),
            GnomePointer(self.shared.clone()),
            GnomeKeyboard(self.shared.clone()),
        )
    }
}

/// Sets a socket's send buffer, returning the size the kernel then uses (it
/// doubles the request for bookkeeping), or the default if that fails.
fn set_sndbuf(fd: &OwnedFd, size: libc::c_int) -> usize {
    let raw = fd.as_raw_fd();
    let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    unsafe {
        libc::setsockopt(raw, libc::SOL_SOCKET, libc::SO_SNDBUF, (&size as *const libc::c_int).cast(), len)
    };
    let mut got: libc::c_int = 0;
    let mut got_len = len;
    let rc = unsafe {
        libc::getsockopt(raw, libc::SOL_SOCKET, libc::SO_SNDBUF, (&mut got as *mut libc::c_int).cast(), &mut got_len)
    };
    if rc == 0 && got > 0 { got as usize } else { 212_992 }
}

/// Writes ops as `reis` requests.
struct ReisSink<'a> {
    ctx: &'a ei::Context,
    serial: u32,
}

impl Sink<Device> for ReisSink<'_> {
    fn write(&mut self, op: &Op<Device>) {
        let key_state = |press: bool| {
            if press {
                ei::keyboard::KeyState::Press
            } else {
                ei::keyboard::KeyState::Released
            }
        };
        match op {
            Op::Abs { dev, x, y } => {
                if let Some(p) = dev.interface::<ei::PointerAbsolute>() {
                    p.motion_absolute(*x, *y);
                }
            }
            Op::Rel { dev, dx, dy } => {
                if let Some(p) = dev.interface::<ei::Pointer>() {
                    p.motion_relative(*dx, *dy);
                }
            }
            Op::Button { dev, button, press } => {
                if let Some(b) = dev.interface::<ei::Button>() {
                    let st = if *press {
                        ei::button::ButtonState::Press
                    } else {
                        ei::button::ButtonState::Released
                    };
                    b.button(*button, st);
                }
            }
            Op::Scroll { dev, dx, dy } => {
                if let Some(s) = dev.interface::<ei::Scroll>() {
                    s.scroll_discrete(*dx, *dy);
                }
            }
            Op::Key { dev, key, press } => {
                if let Some(k) = dev.interface::<ei::Keyboard>() {
                    k.key(*key, key_state(*press));
                }
            }
            Op::Frame { dev } => dev.device().frame(self.serial, now_us()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.ctx.flush().map_err(std::io::Error::from)
    }
}

impl Shared {
    fn push(&self, op: Op<Device>) {
        lock(&self.out).push(op);
    }

    /// Sends what can go now (from any thread). If that leaves something
    /// waiting for the socket or a pacing deadline, the backend thread is
    /// woken to poll for it.
    pub fn pump(&self) -> Pump {
        let mut out = lock(&self.out);
        let waiting = |o: &Outbox<Device>| o.blocked() || o.deadline().is_some();
        let was = waiting(&out);
        let mut sink = ReisSink {
            ctx: &self.ctx,
            serial: self.serial.load(Ordering::Acquire),
        };
        let r = out.pump(&mut sink, Instant::now());
        if let Some(st) = out.take_stall_if_drained() {
            let held = st.since.map_or(0.0, |t| t.elapsed().as_secs_f32());
            if held >= 1.0 || st.dropped_taps > 0 {
                crate::warning!(
                    "gnome: input came faster than the compositor read it (held back {held:.1}s): \
                     {} motion/wheel events merged, {} key/button taps dropped",
                    st.coalesced,
                    st.dropped_taps
                );
            }
        }
        if !was && waiting(&out) {
            self.wake.wake();
        }
        r
    }

    /// What the backend thread's poll needs: `POLLOUT`, and a deadline.
    pub fn poll_needs(&self) -> (bool, Option<Instant>) {
        let mut out = lock(&self.out);
        let not_reading = out.blocked_since();
        let stall = out.stall();
        if let Some(t) = not_reading
            && !stall.warned
            && t.elapsed() >= STALL_WARN
        {
            stall.warned = true;
            crate::warning!(
                "gnome: the compositor hasn't read input for {:.0}s; holding it (motion coalesced)",
                t.elapsed().as_secs_f32()
            );
        }
        (out.blocked(), out.deadline())
    }
}

fn read_keymap(fd: &OwnedFd, size: u32) -> Option<String> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::from(fd.try_clone().ok()?);
    let mut buf = vec![0u8; size as usize];
    file.read_exact_at(&mut buf, 0).ok()?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

pub struct GnomePointer(Arc<Shared>);
pub struct GnomeKeyboard(Arc<Shared>);

impl VirtualPointer for GnomePointer {
    fn motion_absolute(&self, _time: u32, x: u32, y: u32, _extent_x: u32, _extent_y: u32) {
        let (ox, oy) = *lock(&self.0.origin);
        let mut d = lock(&self.0.devices);
        let (gx, gy) = (x as f32 + ox as f32, y as f32 + oy as f32);
        let Some((i, rx, ry)) = route(&d.abs, gx, gy) else {
            return;
        };
        let dev = d.abs[i].dev.clone();
        if dev.interface::<ei::PointerAbsolute>().is_none() {
            return;
        }
        self.0.push(Op::Abs { dev: dev.clone(), x: rx, y: ry });
        d.mark_dirty(&dev);
        d.last_abs = Some(dev);
    }

    fn motion(&self, _time: u32, dx: f64, dy: f64) {
        let mut d = lock(&self.0.devices);
        let Some(dev) = d.rel.clone() else { return };
        if dev.interface::<ei::Pointer>().is_some() {
            self.0.push(Op::Rel { dev: dev.clone(), dx: dx as f32, dy: dy as f32 });
            d.mark_dirty(&dev);
        }
    }

    fn button(&self, _time: u32, button: u32, state: ButtonState) {
        let mut d = lock(&self.0.devices);
        // a press goes to the absolute pointer that last moved (mutter has one
        // per monitor here, and they all drive the one on-screen pointer); a
        // release goes wherever the press went
        let dev = match state {
            ButtonState::Pressed => {
                d.released_with_device.retain(|&b| b != button);
                d.pointer_dev()
            }
            _ => match d.pressed_on.remove(&button) {
                Some(dev) => Some(dev),
                None if d.released_with_device.contains(&button) => {
                    d.released_with_device.retain(|&b| b != button);
                    return;
                }
                None => d.pointer_dev(),
            },
        };
        let Some(dev) = dev else { return };
        if state == ButtonState::Pressed {
            d.pressed_on.insert(button, dev.clone());
        }
        if dev.interface::<ei::Button>().is_some() {
            self.0.push(Op::Button {
                dev: dev.clone(),
                button,
                press: state == ButtonState::Pressed,
            });
            d.mark_dirty(&dev);
        }
    }

    // A wheel notch arrives as axis_source + axis + axis_discrete; libei wants
    // only the discrete notch (mutter turns it into one v120 wheel event).
    fn axis(&self, _time: u32, _axis: Axis, _value: f64) {}
    fn axis_source(&self, _source: AxisSource) {}

    fn axis_discrete(&self, _time: u32, axis: Axis, _value: f64, discrete: i32) {
        let mut d = lock(&self.0.devices);
        let Some(dev) = d.pointer_dev() else {
            return;
        };
        if dev.interface::<ei::Scroll>().is_some() {
            let n = discrete.saturating_mul(NOTCH_120);
            let (dx, dy) = match axis {
                Axis::HorizontalScroll => (n, 0),
                _ => (0, n),
            };
            self.0.push(Op::Scroll { dev: dev.clone(), dx, dy });
            d.mark_dirty(&dev);
        }
    }

    fn frame(&self) {
        let mut d = lock(&self.0.devices);
        for dev in std::mem::take(&mut d.dirty) {
            self.0.push(Op::Frame { dev });
        }
    }
}

impl VirtualKeyboard for GnomeKeyboard {
    // mutter owns the keymap (it sends it to us) and derives modifier state from
    // the keys themselves, so neither needs forwarding
    fn keymap(&self, _format: u32, _fd: std::os::fd::BorrowedFd, _size: u32) {}
    fn modifiers(&self, _depressed: u32, _latched: u32, _locked: u32, _group: u32) {}

    fn key(&self, _time: u32, key: u32, state: u32) {
        let d = lock(&self.0.devices);
        let Some(dev) = &d.kbd else { return };
        if dev.interface::<ei::Keyboard>().is_some() {
            self.0.push(Op::Key { dev: dev.clone(), key, press: state != 0 });
            self.0.push(Op::Frame { dev: dev.clone() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    fn area(id: &str, x: i32, y: i32, w: i32, h: i32) -> StreamArea {
        StreamArea {
            mapping_id: id.into(),
            x,
            y,
            w,
            h,
        }
    }

    fn dev(regions: &[(Rect, Option<&str>)], areas: &[StreamArea]) -> AbsDevice<()> {
        AbsDevice {
            dev: (),
            targets: targets(regions.iter().copied(), areas),
            regionless: regions.is_empty(),
        }
    }

    /// mutter 50 with RecordArea: one standalone device per stream, its region
    /// at 0,0 in the stream's own pixels (1280x720 at 0,0 and 1024x768 right
    /// of it, as in the two-monitor test).
    fn side_by_side() -> (Vec<StreamArea>, Vec<AbsDevice<()>>) {
        let areas = vec![area("a", 0, 0, 1280, 720), area("b", 1280, 0, 1024, 768)];
        let devs = vec![
            dev(&[(rect(0.0, 0.0, 1280.0, 720.0), Some("a"))], &areas),
            dev(&[(rect(0.0, 0.0, 1024.0, 768.0), Some("b"))], &areas),
        ];
        (areas, devs)
    }

    #[test]
    fn area_devices_each_take_their_own_monitor() {
        let (_, devs) = side_by_side();
        assert_eq!(route(&devs, 100.0, 100.0), Some((0, 100.0, 100.0)));
        assert_eq!(route(&devs, 1279.0, 719.0), Some((0, 1279.0, 719.0)));
        // the second monitor: its own device, in its own coordinates (this
        // used to clamp to 1279,300 on the first)
        assert_eq!(route(&devs, 1600.0, 300.0), Some((1, 320.0, 300.0)));
        assert_eq!(route(&devs, 1280.0, 0.0), Some((1, 0.0, 0.0)));
        assert_eq!(route(&devs, 2303.0, 767.0), Some((1, 1023.0, 767.0)));
        // below the shorter first monitor only the second is there
        assert_eq!(route(&devs, 1500.0, 740.0), Some((1, 220.0, 740.0)));
    }

    #[test]
    fn device_order_does_not_matter() {
        let (_, mut devs) = side_by_side();
        devs.reverse();
        assert_eq!(route(&devs, 1600.0, 300.0), Some((0, 320.0, 300.0)));
        assert_eq!(route(&devs, 100.0, 100.0), Some((1, 100.0, 100.0)));
    }

    #[test]
    fn points_off_every_monitor_go_to_the_nearest_clamped() {
        let (_, devs) = side_by_side();
        // in the gap below the first monitor, and beyond the right edge
        assert_eq!(route(&devs, 100.0, 740.0), Some((0, 100.0, 719.0)));
        assert_eq!(route(&devs, 5000.0, 10.0), Some((1, 1023.0, 10.0)));
        assert_eq!(route(&devs, -5.0, -5.0), Some((0, 0.0, 0.0)));
    }

    #[test]
    fn vertical_layout() {
        let areas = vec![area("top", 0, 0, 1280, 720), area("bottom", 128, 720, 1024, 768)];
        let devs = vec![
            dev(&[(rect(0.0, 0.0, 1280.0, 720.0), Some("top"))], &areas),
            dev(&[(rect(0.0, 0.0, 1024.0, 768.0), Some("bottom"))], &areas),
        ];
        assert_eq!(route(&devs, 500.0, 1000.0), Some((1, 372.0, 280.0)));
        assert_eq!(route(&devs, 500.0, 719.0), Some((0, 500.0, 719.0)));
    }

    #[test]
    fn scaled_area_region_is_in_physical_pixels() {
        // a 2x monitor: mutter sizes the region area * scale and divides by
        // the scale again, so the layout point must be multiplied by it
        let areas = vec![area("hi", 0, 0, 1280, 720)];
        let devs = vec![dev(&[(rect(0.0, 0.0, 2560.0, 1440.0), Some("hi"))], &areas)];
        assert_eq!(route(&devs, 640.0, 360.0), Some((0, 1280.0, 720.0)));
        let (_, x, y) = route(&devs, 1279.0, 719.0).unwrap();
        assert!(x < 2560.0 && y < 1440.0, "{x},{y} must stay inside the region");
    }

    #[test]
    fn shared_monitor_device_is_already_in_layout_coordinates() {
        // RecordMonitor: one device, a region per monitor at its layout place
        let areas = vec![area("a", 0, 0, 1280, 720), area("b", 1280, 0, 1024, 768)];
        let devs = vec![dev(
            &[
                (rect(0.0, 0.0, 1280.0, 720.0), Some("a")),
                (rect(1280.0, 0.0, 1024.0, 768.0), Some("b")),
            ],
            &areas,
        )];
        assert_eq!(route(&devs, 1600.0, 300.0), Some((0, 1600.0, 300.0)));
        assert_eq!(route(&devs, 10.0, 700.0), Some((0, 10.0, 700.0)));
    }

    #[test]
    fn unknown_mapping_ids_are_taken_as_layout_coordinates() {
        // no mapping ids (or a stream we didn't record): the old behaviour
        let devs = vec![dev(&[(rect(0.0, 0.0, 1280.0, 720.0), None)], &[])];
        assert_eq!(route(&devs, 1600.0, 300.0), Some((0, 1279.0, 300.0)));
        let devs = vec![dev(&[(rect(0.0, 0.0, 1280.0, 720.0), Some("x"))], &[area("y", 9, 9, 9, 9)])];
        assert_eq!(route(&devs, 100.0, 100.0), Some((0, 100.0, 100.0)));
    }

    #[test]
    fn regionless_device_takes_any_point() {
        let devs = vec![dev(&[], &[])];
        assert_eq!(route(&devs, 1600.0, 300.0), Some((0, 1600.0, 300.0)));
        assert_eq!(route::<()>(&[], 1.0, 1.0), None);
    }

    #[test]
    fn empty_regions_are_ignored() {
        let areas = vec![area("a", 0, 0, 1280, 720)];
        let devs = vec![
            dev(&[(rect(0.0, 0.0, 0.0, 0.0), Some("a"))], &areas),
            dev(&[(rect(0.0, 0.0, 1280.0, 720.0), Some("a"))], &areas),
        ];
        assert_eq!(route(&devs, 5.0, 5.0), Some((1, 5.0, 5.0)));
    }
}
