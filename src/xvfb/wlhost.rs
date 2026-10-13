//! The private Wayland compositor that `-engine xwayland` runs a rootful
//! Xwayland on. Only that one Xwayland ever connects to it — through one end of
//! a socket pair handed over as `WAYLAND_SOCKET`; there is no listening socket —
//! so it never touches the user's session: no seat, no outputs, no D-Bus, no
//! GPU, nothing shown anywhere.
//!
//! It offers what a rootful Xwayland requires and nothing else:
//! `wl_compositor`, `wl_shm` and `xdg_wm_base`. Without a seat Xwayland keeps
//! its own core devices and the keymap it is given over XKB; without
//! linux-dmabuf it renders in software into shared memory; without a data
//! device it leaves the clipboard to the agent. Buffers are released as soon
//! as they are committed (their contents are never read: the X server's own
//! root window is the picture), and frame callbacks are answered at a 60 Hz
//! cadence, as a headless output's vblank would, so Present-synced X clients
//! keep their frame rate (Xwayland falls back to a 1 s timer while a frame
//! callback is outstanding).
//!
//! It also owns the toplevel: the X screen's size is whatever size it last
//! configured, which is how the engine follows the desktop's size.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wayland_backend::protocol::{Argument, Interface, Message};
use wayland_backend::server::{
    Backend, ClientData, ClientId, DisconnectReason, GlobalHandler, GlobalId, Handle, ObjectData,
    ObjectId,
};
use wayland_client::protocol::__interfaces::{
    WL_COMPOSITOR_INTERFACE, WL_SHM_INTERFACE,
};
use wayland_protocols::xdg::shell::client::__interfaces::XDG_WM_BASE_INTERFACE;

/// How long after a commit its frame callbacks are answered (60 Hz).
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// The frame-callback interval: [`FRAME_INTERVAL`], or for measurements
/// `XWLBRIDGE_XWAYLAND_FRAME_MS` (0 = never answer them).
fn frame_interval() -> Option<Duration> {
    match std::env::var("XWLBRIDGE_XWAYLAND_FRAME_MS").ok().and_then(|v| v.parse::<f64>().ok()) {
        Some(ms) if ms <= 0.0 => None,
        Some(ms) => Some(Duration::from_secs_f64(ms / 1000.0)),
        None => Some(FRAME_INTERVAL),
    }
}

/// `xdg_toplevel.state.activated`: without it Xwayland thinks it lost focus.
const XDG_TOPLEVEL_STATE_ACTIVATED: u32 = 4;

/// Counters, for logs and tests.
#[derive(Default)]
pub struct Stats {
    pub commits: AtomicU64,
    pub frames: AtomicU64,
    pub configures: AtomicU64,
}

/// What the engine asks of the compositor's thread.
struct Ctl {
    /// A size to configure the toplevel with, not yet sent.
    want: Mutex<Option<(u16, u16)>>,
    /// The size the client last acknowledged (0x0 before the first).
    acked: Mutex<(u16, u16)>,
    stop: AtomicBool,
    /// Whether Xwayland is (still) connected.
    connected: Arc<AtomicBool>,
    wake: OwnedFd,
    stats: Stats,
}

impl Ctl {
    fn poke(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }
}

/// The compositor, running on its own thread until dropped.
pub struct Host {
    ctl: Arc<Ctl>,
    thread: Option<JoinHandle<()>>,
}

/// Something that asks the compositor for a new screen size.
#[derive(Clone)]
pub struct Resizer(Arc<Ctl>);

impl Resizer {
    /// Configures the toplevel (and so Xwayland's root window) at `w`x`h`.
    /// Asynchronous: the X screen changes once Xwayland has handled it.
    pub fn resize(&self, w: u16, h: u16) {
        *self.0.want.lock().unwrap() = Some((w, h));
        self.0.poke();
    }

    /// The size Xwayland last acknowledged.
    pub fn acked(&self) -> (u16, u16) {
        *self.0.acked.lock().unwrap()
    }

    /// Whether Xwayland is still connected.
    pub fn connected(&self) -> bool {
        self.0.connected.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        let s = &self.0.stats;
        (
            s.commits.load(Ordering::Relaxed),
            s.frames.load(Ordering::Relaxed),
            s.configures.load(Ordering::Relaxed),
        )
    }
}

impl Host {
    /// Starts the compositor for one client, whose end of the connection is
    /// returned (close-on-exec; the caller passes it to Xwayland as
    /// `WAYLAND_SOCKET`). The toplevel is first configured at `w`x`h`.
    pub fn start(w: u16, h: u16) -> io::Result<(Host, OwnedFd)> {
        let (ours, theirs) = UnixStream::pair()?;
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let connected = Arc::new(AtomicBool::new(true));
        let ctl = Arc::new(Ctl {
            want: Mutex::new(None),
            acked: Mutex::new((0, 0)),
            stop: AtomicBool::new(false),
            connected: connected.clone(),
            wake: unsafe { OwnedFd::from_raw_fd(raw) },
            stats: Stats::default(),
        });
        let mut backend: Backend<State> =
            Backend::new().map_err(|e| io::Error::other(format!("wayland server: {e:?}")))?;
        let mut handle = backend.handle();
        let global: Arc<dyn GlobalHandler<State>> = Arc::new(Global);
        handle.create_global::<State>(&WL_COMPOSITOR_INTERFACE, 4, global.clone());
        handle.create_global::<State>(&WL_SHM_INTERFACE, 1, global.clone());
        handle.create_global::<State>(&XDG_WM_BASE_INTERFACE, 1, global);
        handle.insert_client(ours, Arc::new(Client(connected)))?;
        let mut state = State {
            ctl: ctl.clone(),
            frame_interval: frame_interval(),
            size: (w, h),
            serial: 0,
            surfaces: HashMap::new(),
            xdg_surfaces: HashMap::new(),
            toplevels: HashMap::new(),
            serials: Vec::new(),
            frames: Vec::new(),
        };
        let thread_ctl = ctl.clone();
        let thread = std::thread::Builder::new()
            .name("xwayland-host".into())
            .spawn(move || {
                if let Err(e) = run(&mut backend, &mut state, &thread_ctl) {
                    crate::warning!("xwayland: compositor stopped: {e}");
                }
                thread_ctl.connected.store(false, Ordering::Release);
            })?;
        let theirs: OwnedFd = theirs.into();
        Ok((Host { ctl, thread: Some(thread) }, theirs))
    }

    pub fn resizer(&self) -> Resizer {
        Resizer(self.ctl.clone())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Release);
        self.ctl.poke();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(backend: &mut Backend<State>, state: &mut State, ctl: &Ctl) -> io::Result<()> {
    loop {
        if ctl.stop.load(Ordering::Acquire) || !ctl.connected.load(Ordering::Acquire) {
            return Ok(());
        }
        let handle = backend.handle();
        if let Some((w, h)) = ctl.want.lock().unwrap().take() {
            state.size = (w, h);
            state.configure_all(&handle);
        }
        state.fire_frames(&handle, Instant::now());
        backend.flush(None)?;
        let timeout = state
            .frames
            .iter()
            .map(|(due, _)| due.saturating_duration_since(Instant::now()))
            .min()
            .map(|d| d.as_millis().clamp(1, 1000) as i32)
            .unwrap_or(1000);
        let mut pfds = [
            libc::pollfd { fd: backend.poll_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: ctl.wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if pfds[1].revents != 0 {
            let mut v: u64 = 0;
            unsafe { libc::read(ctl.wake.as_fd().as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
        }
        if pfds[0].revents != 0 {
            backend.dispatch_all_clients(state)?;
        }
    }
}

#[derive(Default)]
struct Surface {
    /// Attached since the last commit: `Some(None)` = detached.
    pending_buffer: Option<Option<ObjectId>>,
    /// Frame callbacks requested since the last commit.
    pending_frames: Vec<ObjectId>,
    /// The xdg_surface/xdg_toplevel it is, once it has that role.
    toplevel: Option<(ObjectId, ObjectId)>,
    /// Whether the first configure went out (sent on the first commit).
    configured: bool,
}

/// The compositor's state, handed to every request handler.
struct State {
    ctl: Arc<Ctl>,
    /// After how long frame callbacks are answered (`None`: never).
    frame_interval: Option<Duration>,
    size: (u16, u16),
    serial: u32,
    surfaces: HashMap<ObjectId, Surface>,
    /// xdg_surface -> its wl_surface
    xdg_surfaces: HashMap<ObjectId, ObjectId>,
    /// xdg_toplevel -> its xdg_surface
    toplevels: HashMap<ObjectId, ObjectId>,
    /// configure serials sent, with their size (to know what an ack means)
    serials: Vec<(u32, (u16, u16))>,
    /// frame callbacks to answer, and when
    frames: Vec<(Instant, ObjectId)>,
}

impl State {
    /// Sends the current size to every toplevel.
    fn configure_all(&mut self, handle: &Handle) {
        let targets: Vec<(ObjectId, ObjectId)> = self
            .surfaces
            .values()
            .filter(|s| s.configured)
            .filter_map(|s| s.toplevel.clone())
            .collect();
        for (xdg_surface, toplevel) in targets {
            self.configure(handle, &xdg_surface, &toplevel);
        }
    }

    fn configure(&mut self, handle: &Handle, xdg_surface: &ObjectId, toplevel: &ObjectId) {
        let (w, h) = self.size;
        let states: Vec<u8> = XDG_TOPLEVEL_STATE_ACTIVATED.to_ne_bytes().to_vec();
        let _ = handle.send_event(event(
            toplevel,
            "configure",
            vec![
                Argument::Int(i32::from(w)),
                Argument::Int(i32::from(h)),
                Argument::Array(Box::new(states)),
            ],
        ));
        self.serial = self.serial.wrapping_add(1);
        self.serials.push((self.serial, self.size));
        if self.serials.len() > 64 {
            self.serials.remove(0);
        }
        let _ = handle.send_event(event(xdg_surface, "configure", vec![Argument::Uint(self.serial)]));
        self.ctl.stats.configures.fetch_add(1, Ordering::Relaxed);
        crate::vlog!("xwayland: configured the X screen at {w}x{h} (serial {})", self.serial);
    }

    fn fire_frames(&mut self, handle: &Handle, now: Instant) {
        if self.frames.is_empty() {
            return;
        }
        let time = monotonic_ms();
        let mut i = 0;
        while i < self.frames.len() {
            if self.frames[i].0 <= now {
                let (_, cb) = self.frames.swap_remove(i);
                // done is a destructor event: the backend forgets the object
                let _ = handle.send_event(event(&cb, "done", vec![Argument::Uint(time)]));
                self.ctl.stats.frames.fetch_add(1, Ordering::Relaxed);
            } else {
                i += 1;
            }
        }
    }

    fn commit(&mut self, handle: &Handle, surface: &ObjectId) {
        self.ctl.stats.commits.fetch_add(1, Ordering::Relaxed);
        let Some(s) = self.surfaces.get_mut(surface) else { return };
        // the picture is never read: hand the buffer back at once
        if let Some(Some(buffer)) = s.pending_buffer.take() {
            let _ = handle.send_event(event(&buffer, "release", vec![]));
        }
        if let Some(interval) = self.frame_interval {
            let due = Instant::now() + interval;
            for cb in s.pending_frames.drain(..) {
                self.frames.push((due, cb));
            }
        } else {
            // never answered (the client keeps the objects until it drops them)
            s.pending_frames.clear();
        }
        if !s.configured
            && let Some((xdg_surface, toplevel)) = s.toplevel.clone()
        {
            s.configured = true;
            self.configure(handle, &xdg_surface, &toplevel);
        }
    }
}

fn monotonic_ms() -> u32 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

/// An event message by name (the opcode is its index in the interface).
fn event(id: &ObjectId, name: &str, args: Vec<Argument<ObjectId, std::os::fd::RawFd>>) -> Message<ObjectId, std::os::fd::RawFd> {
    let iface: &Interface = id.interface();
    let opcode = iface
        .events
        .iter()
        .position(|e| e.name == name)
        .unwrap_or_else(|| panic!("{} has no event {name}", iface.name)) as u16;
    Message { sender_id: id.clone(), opcode, args: args.into_iter().collect() }
}

struct Client(Arc<AtomicBool>);

impl ClientData for Client {
    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        crate::vlog!("xwayland: Xwayland left its compositor ({reason:?})");
        self.0.store(false, Ordering::Release);
    }
}

struct Global;

impl GlobalHandler<State> for Global {
    fn bind(
        self: Arc<Self>,
        handle: &Handle,
        _data: &mut State,
        _client_id: ClientId,
        _global_id: GlobalId,
        object_id: ObjectId,
    ) -> Arc<dyn ObjectData<State>> {
        if object_id.interface().name == WL_SHM_INTERFACE.name {
            // the two formats every wl_shm has
            for format in [0u32, 1] {
                let _ = handle.send_event(event(&object_id, "format", vec![Argument::Uint(format)]));
            }
        }
        Arc::new(Obj)
    }
}

/// Every object: requests are told apart by interface and request name.
struct Obj;

impl ObjectData<State> for Obj {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut State,
        _client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<State>>> {
        let id = msg.sender_id.clone();
        let iface = id.interface();
        let desc = iface.requests.get(msg.opcode as usize)?;
        let new_id = || {
            msg.args.iter().find_map(|a| match a {
                Argument::NewId(n) => Some(n.clone()),
                _ => None,
            })
        };
        let object = |i: usize| match msg.args.get(i) {
            Some(Argument::Object(o)) if !o.is_null() => Some(o.clone()),
            _ => None,
        };
        match (iface.name, desc.name) {
            ("wl_compositor", "create_surface") => {
                if let Some(s) = new_id() {
                    state.surfaces.insert(s, Surface::default());
                }
            }
            ("wl_surface", "attach") => {
                if let Some(s) = state.surfaces.get_mut(&id) {
                    s.pending_buffer = Some(object(0));
                }
            }
            ("wl_surface", "frame") => {
                if let (Some(cb), Some(s)) = (new_id(), state.surfaces.get_mut(&id)) {
                    s.pending_frames.push(cb);
                }
            }
            ("wl_surface", "commit") => state.commit(handle, &id),
            ("xdg_wm_base", "get_xdg_surface") => {
                if let (Some(xs), Some(surface)) = (new_id(), object(1)) {
                    state.xdg_surfaces.insert(xs, surface);
                }
            }
            ("xdg_surface", "get_toplevel") => {
                if let (Some(top), Some(surface)) = (new_id(), state.xdg_surfaces.get(&id).cloned()) {
                    state.toplevels.insert(top.clone(), id.clone());
                    if let Some(s) = state.surfaces.get_mut(&surface) {
                        s.toplevel = Some((id.clone(), top));
                    }
                }
            }
            ("xdg_surface", "ack_configure") => {
                if let Some(Argument::Uint(serial)) = msg.args.first()
                    && let Some(&(_, size)) = state.serials.iter().find(|(s, _)| s == serial)
                {
                    *state.ctl.acked.lock().unwrap() = size;
                }
            }
            ("xdg_toplevel", "set_title") => {
                if let Some(Argument::Str(Some(t))) = msg.args.first() {
                    crate::vlog!("xwayland: toplevel title {:?}", t.to_string_lossy());
                }
            }
            // everything else (shm pools and buffers, regions, damage, titles,
            // size hints, pongs) needs no answer
            _ => {}
        }
        // every request that creates an object gets one of ours
        new_id().map(|_| Arc::new(Obj) as Arc<dyn ObjectData<State>>)
    }

    fn destroyed(
        self: Arc<Self>,
        _handle: &Handle,
        state: &mut State,
        _client_id: ClientId,
        object_id: ObjectId,
    ) {
        match object_id.interface().name {
            "wl_surface" => {
                state.surfaces.remove(&object_id);
            }
            "xdg_surface" => {
                state.xdg_surfaces.remove(&object_id);
                for s in state.surfaces.values_mut() {
                    if s.toplevel.as_ref().is_some_and(|(xs, _)| *xs == object_id) {
                        s.toplevel = None;
                        s.configured = false;
                    }
                }
            }
            "xdg_toplevel" => {
                state.toplevels.remove(&object_id);
                for s in state.surfaces.values_mut() {
                    if s.toplevel.as_ref().is_some_and(|(_, t)| *t == object_id) {
                        s.toplevel = None;
                        s.configured = false;
                    }
                }
            }
            "wl_callback" => state.frames.retain(|(_, cb)| *cb != object_id),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The compositor driven by a real Wayland client (wayland-client), doing
    //! what a rootful Xwayland does: bind, make a toplevel, wait for its
    //! configure, attach a shm buffer with a frame callback, be resized.
    use super::Host;
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};
    use wayland_client::protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    #[derive(Default)]
    struct T {
        globals: Vec<String>,
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        wm: Option<xdg_wm_base::XdgWmBase>,
        formats: Vec<u32>,
        toplevel_sizes: Vec<(i32, i32, Vec<u8>)>,
        serials: Vec<u32>,
        released: u32,
        frame_done: Option<Instant>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for T {
        fn event(
            t: &mut Self,
            reg: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { name, interface, version } = event {
                match interface.as_str() {
                    "wl_compositor" => t.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                    "wl_shm" => t.shm = Some(reg.bind(name, 1, qh, ())),
                    "xdg_wm_base" => t.wm = Some(reg.bind(name, 1, qh, ())),
                    _ => {}
                }
                t.globals.push(interface);
            }
        }
    }
    impl Dispatch<wl_shm::WlShm, ()> for T {
        fn event(t: &mut Self, _: &wl_shm::WlShm, e: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_shm::Event::Format { format } = e {
                t.formats.push(u32::from(format));
            }
        }
    }
    impl Dispatch<wl_buffer::WlBuffer, ()> for T {
        fn event(t: &mut Self, _: &wl_buffer::WlBuffer, e: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_buffer::Event::Release = e {
                t.released += 1;
            }
        }
    }
    impl Dispatch<wl_callback::WlCallback, ()> for T {
        fn event(t: &mut Self, _: &wl_callback::WlCallback, e: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let wl_callback::Event::Done { .. } = e {
                t.frame_done = Some(Instant::now());
            }
        }
    }
    impl Dispatch<xdg_surface::XdgSurface, ()> for T {
        fn event(t: &mut Self, s: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_surface::Event::Configure { serial } = e {
                s.ack_configure(serial);
                t.serials.push(serial);
            }
        }
    }
    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for T {
        fn event(t: &mut Self, _: &xdg_toplevel::XdgToplevel, e: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            if let xdg_toplevel::Event::Configure { width, height, states } = e {
                t.toplevel_sizes.push((width, height, states));
            }
        }
    }
    delegate_noop!(T: ignore wl_compositor::WlCompositor);
    delegate_noop!(T: ignore wl_surface::WlSurface);
    delegate_noop!(T: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(T: ignore xdg_wm_base::XdgWmBase);

    #[test]
    fn a_rootful_xwayland_session() {
        let (host, fd) = Host::start(800, 600).unwrap();
        let resizer = host.resizer();
        let conn = Connection::from_socket(UnixStream::from(fd)).unwrap();
        let mut queue = conn.new_event_queue::<T>();
        let qh = queue.handle();
        let mut t = T::default();
        conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut t).unwrap();
        // what a rootful Xwayland needs, and nothing that reaches anything else
        let mut g = t.globals.clone();
        g.sort();
        assert_eq!(g, ["wl_compositor", "wl_shm", "xdg_wm_base"]);
        queue.roundtrip(&mut t).unwrap();
        assert_eq!(t.formats, [0, 1], "argb8888 and xrgb8888");

        // the toplevel is configured at the size asked for, activated, on its first commit
        let surface = t.compositor.as_ref().unwrap().create_surface(&qh, ());
        let xs = t.wm.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
        let _top = xs.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut t).unwrap();
        assert_eq!(t.toplevel_sizes.len(), 1);
        assert_eq!((t.toplevel_sizes[0].0, t.toplevel_sizes[0].1), (800, 600));
        assert_eq!(t.toplevel_sizes[0].2, 4u32.to_ne_bytes(), "activated");
        queue.roundtrip(&mut t).unwrap();
        assert_eq!(resizer.acked(), (800, 600));

        // a committed buffer comes back at once; its frame callback ~16 ms later
        let len = 800 * 600 * 4;
        let mfd = unsafe { libc::memfd_create(c"wlhost-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(mfd >= 0);
        let mfd = unsafe { OwnedFd::from_raw_fd(mfd) };
        assert_eq!(unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&mfd), len as i64) }, 0);
        let pool = t.shm.as_ref().unwrap().create_pool(mfd.as_fd(), len, &qh, ());
        let buffer = pool.create_buffer(0, 800, 600, 3200, wl_shm::Format::Xrgb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, 800, 600);
        surface.frame(&qh, ());
        let committed = Instant::now();
        surface.commit();
        queue.roundtrip(&mut t).unwrap();
        assert_eq!(t.released, 1, "released on commit");
        while t.frame_done.is_none() && committed.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(2));
            queue.roundtrip(&mut t).unwrap();
        }
        let waited = t.frame_done.expect("frame callback answered") - committed;
        assert!(waited >= Duration::from_millis(15) && waited < Duration::from_millis(500), "{waited:?}");

        // a resize is a new configure, at the new size
        resizer.resize(1024, 768);
        let start = Instant::now();
        while t.toplevel_sizes.len() < 2 && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(2));
            queue.roundtrip(&mut t).unwrap();
        }
        assert_eq!((t.toplevel_sizes[1].0, t.toplevel_sizes[1].1), (1024, 768));
        queue.roundtrip(&mut t).unwrap();
        assert_eq!(resizer.acked(), (1024, 768));
        let (commits, frames, configures) = resizer.stats();
        assert_eq!((commits, frames, configures), (2, 1, 2));

        // the client going away is noticed
        assert!(resizer.connected());
        drop(queue);
        drop(conn);
        let start = Instant::now();
        while resizer.connected() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!resizer.connected());
        drop(host);
    }
}
