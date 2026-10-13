//! `-showwindows`: X windows on the engine display shown natively on the
//! desktop ("reverse Xwayland", backlog #22; experimental, off by default).
//! `-reverseinput` uses only this module's master device (below), for a
//! desktop-side proxy (tools/x11-a11y-proxy) that types and clicks into the
//! display's windows through it; it shows no windows.
//!
//! Two layouts:
//! - per window (default): each managed top-level X window is a Wayland
//!   `xdg_toplevel`, each override-redirect window (menu, tooltip, combo
//!   list) an `xdg_popup` on the shown window of the same client;
//! - `-showscreen`: the whole X screen is one `xdg_toplevel`, its windows
//!   composited in stacking order over a plain background (like a rootful X
//!   server without a window manager), with a minimal click-to-focus.
//!
//! - Contents: the agent already redirects every top-level window offscreen
//!   (Composite, manually). Per shown window this names its pixmap, watches it
//!   with DAMAGE, and reads it with MIT-SHM GetImage straight into a memfd
//!   that is also the `wl_shm` pool (two buffers). Commits follow the
//!   compositor's frame callbacks, so a fast client costs at most one copy per
//!   compositor frame.
//! - Window state: title from `_NET_WM_NAME`/`WM_NAME`, app id from `WM_CLASS`,
//!   `WM_TRANSIENT_FOR` as the toplevel's parent (a child's first frame waits
//!   for its parent's), `WM_HINTS` initial iconic state as a minimised
//!   toplevel, the compositor's close as `WM_DELETE_WINDOW`, and the
//!   compositor's size as the X window's size.
//! - Input: the desktop's pointer and keyboard on a shown window are injected
//!   with XTest on a separate XInput2 master device ("xwlbridge-reverse"), so
//!   they reach that window and nothing else: the agent ignores raw events
//!   from that master instead of forwarding them to the desktop, and the core
//!   (desktop) pointer is held by the agent's grab, so it can't reach a shown
//!   window. The window's owner gets that master as its ClientPointer, so its
//!   pointer grabs, warps and queries follow the desktop's pointer, and its
//!   own XTest goes to the reverse master's focus (an on-screen keyboard on
//!   the engine types into the X window that has that keyboard, inside X; the
//!   keyboard is given ICCCM-style, see `focus_x`). The X cursor of that master
//!   is the desktop's cursor over shown windows. Mouse-look: when the cursor
//!   is blank and the client warps the pointer, the desktop's pointer is
//!   locked (pointer-constraints) and its relative motion is injected as
//!   relative XTest motion, as Xwayland emulates warps.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection as WlConnection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use x11rb::connection::Connection;
use x11rb::protocol::render::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, MapState,
    StackMode, Window, WindowClass,
};
use x11rb::protocol::{Event, composite, damage, shm, xtest};
use x11rb::rust_connection::RustConnection;

use super::{AnyError, Auth};
use crate::bridge::input::{BUTTON_PRESS, BUTTON_RELEASE, KEY_PRESS, KEY_RELEASE, MOTION_NOTIFY};

/// The master device the desktop's input on shown windows (or a proxy's)
/// goes through.
pub const MASTER_NAME: &str = "xwlbridge-reverse";

/// Largest side of a shown window, and the most bytes its two buffers may
/// take: anything bigger is not shown (one client must not be able to break
/// the compositor connection every shown window shares; review F3).
const MAX_SIDE: u16 = 16384;
const MAX_POOL: usize = 1 << 30;

/// The plain background of `-showscreen` (no desktop picture: that would
/// show the desktop inside itself).
const SCREEN_BG: render::Color = render::Color {
    red: 0x3030,
    green: 0x3434,
    blue: 0x3838,
    alpha: 0xffff,
};

/// The resource base of the reverse thread's X connection (0: none yet). Its
/// XTest is on the reverse master, whose buttons and keys are its own, so
/// the RECORD thread must not count it among the core devices' holders.
static CONNECTION_BASE: AtomicU32 = AtomicU32::new(0);

/// Whether the reverse thread was started (it lives for the process).
static SPAWNED: AtomicBool = AtomicBool::new(false);

/// Whether `xid_base` is the reverse thread's X connection.
pub fn is_reverse_client(xid_base: u32) -> bool {
    xid_base != 0 && CONNECTION_BASE.load(Ordering::Relaxed) == xid_base
}

/// The X server dropped the client with `xid_base`: if it was the reverse
/// thread's, its slot goes to the next client, which must count again.
pub fn forget_client(xid_base: u32) {
    let _ = CONNECTION_BASE.compare_exchange(xid_base, 0, Ordering::Relaxed, Ordering::Relaxed);
}

/// The reverse master's devices: input from these must not go to the desktop.
#[derive(Clone, Debug)]
pub struct Devices {
    pub pointer: u16,
    pub keyboard: u16,
}

impl Devices {
    /// Whether an XI2 event from `deviceid` is the reverse direction's own.
    pub fn owns(&self, deviceid: u16) -> bool {
        deviceid == self.pointer || deviceid == self.keyboard
    }
}

/// Creates (or finds, if it is there already) the reverse master device. The
/// connection must have announced XI 2.
pub fn create_master(conn: &RustConnection) -> Result<Devices, AnyError> {
    if let Some(d) = find_master(conn)? {
        return Ok(d);
    }
    let name = MASTER_NAME.as_bytes().to_vec();
    conn.xinput_xi_change_hierarchy(&[xinput::HierarchyChange {
        len: add_master_len(name.len()),
        data: xinput::HierarchyChangeData::AddMaster(xinput::HierarchyChangeDataAddMaster {
            send_core: true,
            enable: true,
            name,
        }),
    }])?
    .check()?;
    find_master(conn)?.ok_or_else(|| "the reverse master device didn't appear".into())
}

/// XIChangeHierarchy's AddMaster length in 4-byte units: the 4-byte change
/// header, name_len + send_core + enable, and the name padded to 4.
fn add_master_len(name_len: usize) -> u16 {
    (4 + 4 + name_len).div_ceil(4) as u16
}

/// Where a window of `len` pixels at `p` goes so it is wholly on a screen of
/// `max` pixels (at 0 if it can't be).
fn fit(p: i16, len: u16, max: i32) -> i16 {
    if len as i32 >= max {
        0
    } else {
        (p as i32).clamp(0, max - len as i32) as i16
    }
}

/// The bytes of one frame of `w`x`h` pixels, if a window that size may be
/// shown at all.
fn frame_bytes(w: u16, h: u16) -> Option<usize> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    let frame = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    (frame.checked_mul(2)? <= MAX_POOL).then_some(frame)
}

/// An anchor rectangle inside a parent of `pw`x`ph` for a popup at `(dx, dy)`
/// from the parent's origin, and the offset from it (the anchor must lie in
/// the parent; the offset may point anywhere).
fn popup_anchor(dx: i32, dy: i32, pw: u16, ph: u16) -> ((i32, i32), (i32, i32)) {
    let ax = dx.clamp(0, (pw as i32 - 1).max(0));
    let ay = dy.clamp(0, (ph as i32 - 1).max(0));
    ((ax, ay), (dx - ax, dy - ay))
}

/// Whether a cursor image shows nothing (every pixel transparent).
fn blank_cursor(image: &[u32]) -> bool {
    image.iter().all(|p| p >> 24 == 0)
}

/// A Linux BTN_* code (Wayland's wl_pointer.button) as an X button.
fn x_button(code: u32) -> Option<u8> {
    Some(match code {
        0x110 => 1, // BTN_LEFT
        0x112 => 2, // BTN_MIDDLE
        0x111 => 3, // BTN_RIGHT
        0x113 => 8, // BTN_SIDE (back)
        0x114 => 9, // BTN_EXTRA (forward)
        _ => return None,
    })
}

fn find_master(conn: &RustConnection) -> Result<Option<Devices>, AnyError> {
    let infos = conn
        .xinput_xi_query_device(xinput::Device::ALL)?
        .reply()?
        .infos;
    let find = |suffix: &str, ty: xinput::DeviceType| {
        let want = format!("{MASTER_NAME} {suffix}");
        infos
            .iter()
            .find(|i| i.type_ == ty && i.name == want.as_bytes())
            .map(|i| i.deviceid)
    };
    Ok(
        match (
            find("pointer", xinput::DeviceType::MASTER_POINTER),
            find("keyboard", xinput::DeviceType::MASTER_KEYBOARD),
        ) {
            (Some(pointer), Some(keyboard)) => Some(Devices { pointer, keyboard }),
            _ => None,
        },
    )
}

/// Runs the reverse direction on its own thread (own X and Wayland
/// connections) for the life of the process; later calls (agent restarts)
/// do nothing.
pub fn spawn_once(display: u32, devs: Devices, auth: Auth, screen: bool) {
    if SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("xvfb-reverse".into())
        .spawn(move || {
            if let Err(e) = run(display, devs, &auth, screen) {
                crate::warning!("-showwindows: stopped: {e}");
            }
            // our X connection is gone (or never was): its slot is anyone's
            CONNECTION_BASE.store(0, Ordering::Relaxed);
        });
}

struct Atoms {
    wm_protocols: u32,
    wm_delete_window: u32,
    wm_take_focus: u32,
    net_wm_name: u32,
    net_wm_window_type: u32,
    utf8_string: u32,
}

/// A shown window's pixels: a memfd holding two frames, attached to the X
/// server (MIT-SHM) and the compositor (wl_shm) alike.
struct Buffers {
    seg: u32,
    pool: wl_shm_pool::WlShmPool,
    buffers: [wl_buffer::WlBuffer; 2],
    busy: [bool; 2],
    width: u16,
    height: u16,
    frame: usize,
}

enum Role {
    Toplevel(xdg_toplevel::XdgToplevel),
    Popup {
        popup: xdg_popup::XdgPopup,
        parent: Window,
    },
    /// `-showscreen`: the whole X screen.
    Screen {
        toplevel: xdg_toplevel::XdgToplevel,
        viewport: Option<wp_viewport::WpViewport>,
    },
}

struct Shown {
    /// Outer position and size (border included), as the X server has it.
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    depth: u8,
    damage: u32,
    pixmap: Option<u32>,
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    role: Role,
    configured: bool,
    buffers: Option<Buffers>,
    dirty: bool,
    frames: u64,
    /// A wl_surface.frame callback is outstanding: commit at most once per
    /// compositor frame, like Xwayland (games.md finding 1).
    frame_pending: bool,
    /// When the last frame was committed.
    committed_at: Option<std::time::Instant>,
    /// The X client (resource base) the window belongs to.
    client: u32,
    /// Asked to start iconic: minimise once the first frame is up.
    iconic: bool,
    /// The transient parent whose first frame ours waits for.
    parent: Option<Window>,
}

/// `-showscreen`: a window of the X screen, composited onto the canvas.
struct Member {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    depth: u8,
    format: u32,
    damage: u32,
    pixmap: Option<u32>,
    picture: Option<u32>,
    override_redirect: bool,
    no_input: bool,
}

struct Canvas {
    pixmap: u32,
    picture: u32,
    members: HashMap<Window, Member>,
    /// Bottom to top, as last queried.
    order: Vec<Window>,
    restacked: bool,
    /// Picture formats by visual, and the 32-bit one.
    formats: HashMap<u32, u32>,
    /// The X window the reverse keyboard types into.
    focus: Option<Window>,
    /// Surface pixels per X pixel.
    scale: f64,
}

/// The pointer lock of mouse-look.
struct Lock {
    locked: zwp_locked_pointer_v1::ZwpLockedPointerV1,
    relative: zwp_relative_pointer_v1::ZwpRelativePointerV1,
    window: Window,
    active: bool,
    /// Sub-pixel motion not sent yet.
    rest: (f64, f64),
}

struct CursorImage {
    surface: wl_surface::WlSurface,
    buffer: Option<(wl_shm_pool::WlShmPool, wl_buffer::WlBuffer)>,
    hot: (i32, i32),
    serial: u32,
    blank: bool,
}

struct State {
    x: RustConnection,
    root: Window,
    screen: (u16, u16),
    devs: Devices,
    atoms: Atoms,
    resource_mask: u32,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    windows: HashMap<Window, Shown>,
    /// Shown popups, oldest first (the last is the topmost).
    popups: Vec<Window>,
    /// Clients (resource bases) given the reverse master.
    adopted: Vec<u32>,
    /// Override-redirect windows the compositor dismissed: not shown again
    /// until they are mapped again.
    dismissed: Vec<Window>,
    /// `-showscreen`'s canvas (the screen itself is `windows[root]`).
    canvas: Option<Canvas>,
    /// The shown window under the desktop's pointer / with keyboard focus.
    pointer_on: Option<Window>,
    keyboard_on: Option<Window>,
    /// The last X window the reverse keyboard was focused on.
    x_focus: Option<Window>,
    enter_serial: u32,
    /// Where the reverse pointer was last put (root coordinates).
    last_pos: Option<(i16, i16)>,
    lock: Option<Lock>,
    cursor: Option<CursorImage>,
    keys_down: Vec<u8>,
    buttons_down: Vec<u8>,
    /// Released buffers to repaint into: (window, buffer index).
    released: Vec<(Window, usize)>,
    /// Windows whose frame callback fired.
    frame_done: Vec<Window>,
    throttle: bool,
    /// Toplevels the compositor asked to close.
    close: Vec<Window>,
    /// Sizes the compositor asked for.
    resize: Vec<(Window, i32, i32)>,
    /// Popups the compositor placed: (window, x, y) relative to the parent.
    placed: Vec<(Window, i32, i32)>,
    /// Popups the compositor dismissed.
    popup_done: Vec<Window>,
    /// Pointer-lock state changes: (window, locked).
    lock_events: Vec<bool>,
    /// Relative motion while locked.
    relative: Vec<(f64, f64)>,
    qh: QueueHandle<State>,
}

fn run(display: u32, devs: Devices, auth: &Auth, screen_mode: bool) -> Result<(), AnyError> {
    let (x, screen) = super::connect(display, auth)?;
    CONNECTION_BASE.store(x.setup().resource_id_base, Ordering::Relaxed);
    let resource_mask = x.setup().resource_id_mask;
    let s = &x.setup().roots[screen];
    let (root, sw, sh) = (s.root, s.width_in_pixels, s.height_in_pixels);
    x.xinput_xi_query_version(2, 2)?.reply()?;
    damage::query_version(&x, 1, 1)?.reply()?;
    composite::query_version(&x, 0, 4)?.reply()?;
    x.xfixes_query_version(5, 0)?.reply()?;
    let v = shm::query_version(&x)?.reply()?;
    if (v.major_version, v.minor_version) < (1, 2) {
        return Err("the X server's MIT-SHM can't take a file descriptor (needs 1.2)".into());
    }
    // our XTest goes through the reverse master's XTest devices, and our
    // cursor queries are about its pointer
    x.xinput_xi_set_client_pointer(x11rb::NONE, devs.pointer)?
        .check()?;
    x.change_window_attributes(
        root,
        &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
    )?;
    x.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    let atom =
        |name: &[u8]| -> Result<u32, AnyError> { Ok(x.intern_atom(false, name)?.reply()?.atom) };
    let atoms = Atoms {
        wm_protocols: atom(b"WM_PROTOCOLS")?,
        wm_delete_window: atom(b"WM_DELETE_WINDOW")?,
        wm_take_focus: atom(b"WM_TAKE_FOCUS")?,
        net_wm_name: atom(b"_NET_WM_NAME")?,
        net_wm_window_type: atom(b"_NET_WM_WINDOW_TYPE")?,
        utf8_string: atom(b"UTF8_STRING")?,
    };

    let wl = WlConnection::connect_to_env()
        .map_err(|e| format!("no Wayland display to show windows on: {e}"))?;
    let mut queue: EventQueue<State> = wl.new_event_queue();
    let qh = queue.handle();
    wl.display().get_registry(&qh, ());
    let mut state = State {
        x,
        root,
        screen: (sw, sh),
        devs,
        atoms,
        resource_mask,
        compositor: None,
        shm: None,
        wm_base: None,
        seat: None,
        pointer: None,
        keyboard: None,
        constraints: None,
        relative_manager: None,
        viewporter: None,
        windows: HashMap::new(),
        popups: Vec::new(),
        adopted: Vec::new(),
        dismissed: Vec::new(),
        canvas: None,
        pointer_on: None,
        keyboard_on: None,
        x_focus: None,
        enter_serial: 0,
        last_pos: None,
        lock: None,
        cursor: None,
        keys_down: Vec::new(),
        buttons_down: Vec::new(),
        released: Vec::new(),
        frame_done: Vec::new(),
        throttle: std::env::var_os("XWLBRIDGE_SHOW_NOTHROTTLE").is_none_or(|v| v.is_empty()),
        close: Vec::new(),
        resize: Vec::new(),
        placed: Vec::new(),
        popup_done: Vec::new(),
        lock_events: Vec::new(),
        relative: Vec::new(),
        qh: qh.clone(),
    };
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    if state.compositor.is_none() || state.shm.is_none() || state.wm_base.is_none() {
        return Err("the compositor lacks wl_compositor, wl_shm or xdg_wm_base".into());
    }
    crate::log!(
        "-showwindows: showing the engine display's {} on the desktop (reverse master {:?}; pointer lock {}, viewporter {})",
        if screen_mode {
            "screen as one window"
        } else {
            "windows"
        },
        state.devs,
        if state.constraints.is_some() && state.relative_manager.is_some() {
            "yes"
        } else {
            "no"
        },
        if state.viewporter.is_some() {
            "yes"
        } else {
            "no"
        },
    );
    if let Some(c) = state.compositor.as_ref() {
        let surface = c.create_surface(&qh, 0);
        state.cursor = Some(CursorImage {
            surface,
            buffer: None,
            hot: (0, 0),
            serial: 0,
            blank: false,
        });
    }
    state.update_cursor();

    if screen_mode {
        state.show_screen(&qh, display)?;
    }
    // what is already up
    let tree = state.x.query_tree(root)?.reply()?;
    for w in tree.children {
        state.consider(w, &qh);
    }

    let xfd = state.x.stream().as_raw_fd();
    loop {
        while let Some(ev) = state.x.poll_for_event()? {
            state.x_event(ev, &qh);
        }
        queue.dispatch_pending(&mut state)?;
        state.after_dispatch(&qh);
        // anything the dispatch did may have queued X events (replies we
        // waited for read them in)
        while let Some(ev) = state.x.poll_for_event()? {
            state.x_event(ev, &qh);
        }
        state.x.flush()?;
        match wl.flush() {
            Ok(()) => {}
            // the compositor is slow to read: try again next time (review F7)
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("Wayland connection lost: {e}").into()),
        }
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut pfds = [
            libc::pollfd {
                fd: xfd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, 1000) };
        if n > 0 && pfds[1].revents != 0 {
            if let Err(e) = guard.read()
                && !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
            {
                return Err(format!("Wayland connection lost: {e}").into());
            }
        } else {
            drop(guard);
        }
    }
}

impl State {
    fn screen_mode(&self) -> bool {
        self.canvas.is_some()
    }

    fn client_of(&self, w: Window) -> u32 {
        w & !self.resource_mask
    }

    /// WM_HINTS: (takes input, starts iconic).
    fn hints(&self, w: Window) -> (bool, bool) {
        let v: Vec<u32> = self
            .x
            .get_property(false, w, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, 0, 9)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        let flags = v.first().copied().unwrap_or(0);
        let input = if flags & 1 != 0 {
            v.get(1).copied().unwrap_or(1) != 0
        } else {
            true
        };
        let iconic = flags & 2 != 0 && v.get(2).copied() == Some(3);
        (input, iconic)
    }

    fn protocols(&self, w: Window) -> Vec<u32> {
        self.x
            .get_property(false, w, self.atoms.wm_protocols, AtomEnum::ATOM, 0, 64)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>()))
            .unwrap_or_default()
    }

    /// Whether `w` never takes the keyboard: WM_HINTS input false and no
    /// WM_TAKE_FOCUS (ICCCM's "No Input" model), as on-screen keyboards ask.
    fn takes_no_input(&self, w: Window) -> bool {
        !self.hints(w).0 && !self.protocols(w).contains(&self.atoms.wm_take_focus)
    }

    /// Shows `w` if it is a mapped top-level InputOutput window.
    fn consider(&mut self, w: Window, qh: &QueueHandle<State>) {
        if self.windows.contains_key(&w) || w == self.root {
            return;
        }
        let Ok(Ok(attrs)) = self.x.get_window_attributes(w).map(|c| c.reply()) else {
            return;
        };
        if attrs.map_state != MapState::VIEWABLE || attrs.class != WindowClass::INPUT_OUTPUT {
            return;
        }
        if self.screen_mode() {
            if let Err(e) = self.add_member(w, attrs.override_redirect, attrs.visual) {
                crate::vlog!("-showscreen: can't composite {w:#x}: {e}");
            }
            return;
        }
        let result = if attrs.override_redirect {
            if self.dismissed.contains(&w) {
                return;
            }
            self.show_popup(w, qh)
        } else {
            self.show(w, qh)
        };
        if let Err(e) = result {
            crate::warning!("-showwindows: can't show {w:#x}: {e}");
        }
    }

    /// The X-side part of showing `w`: its geometry, a DAMAGE object, and
    /// property changes. `(x, y, width, height, depth, damage)`.
    fn watch(&mut self, w: Window) -> Result<(i16, i16, u16, u16, u8, u32), AnyError> {
        let g = self.x.get_geometry(w)?.reply()?;
        let (width, height) = (g.width + 2 * g.border_width, g.height + 2 * g.border_width);
        if frame_bytes(width, height).is_none() {
            return Err(format!("{width}x{height} is too big to show").into());
        }
        let dmg = self.x.generate_id()?;
        damage::create(&self.x, dmg, w, damage::ReportLevel::NON_EMPTY)?;
        self.x.change_window_attributes(
            w,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        Ok((g.x, g.y, width, height, g.depth, dmg))
    }

    /// Gives `w`'s client the reverse master as its ClientPointer, so its
    /// pointer grabs, warps and queries follow the desktop's input on it.
    fn adopt_client(&mut self, w: Window) {
        let set = self.x.xinput_xi_set_client_pointer(w, self.devs.pointer);
        match set
            .map_err(AnyError::from)
            .and_then(|c| c.check().map_err(AnyError::from))
        {
            Ok(()) => {
                let client = self.client_of(w);
                if !self.adopted.contains(&client) {
                    self.adopted.push(client);
                    // (resource bases are reused: forget the oldest)
                    if self.adopted.len() > 256 {
                        self.adopted.remove(0);
                    }
                }
            }
            Err(e) => {
                crate::vlog!("-showwindows: can't give {w:#x}'s client the reverse pointer: {e}")
            }
        }
    }

    fn show(&mut self, w: Window, qh: &QueueHandle<State>) -> Result<(), AnyError> {
        let (mut x, mut y, width, height, depth, dmg) = self.watch(w)?;
        // Pointer input can only reach the part of the window on the screen:
        // without a window manager its position is the client's guess and
        // nobody sees it, so move it fully on the screen where it fits.
        let (sw, sh) = (self.screen.0 as i32, self.screen.1 as i32);
        let (fx, fy) = (fit(x, width, sw), fit(y, height, sh));
        if (fx, fy) != (x, y) {
            self.x.configure_window(
                w,
                &ConfigureWindowAux::new().x(i32::from(fx)).y(i32::from(fy)),
            )?;
            (x, y) = (fx, fy);
        }
        let (input, iconic) = self.hints(w);
        let no_input = !input && !self.protocols(w).contains(&self.atoms.wm_take_focus);
        self.adopt_client(w);
        let compositor = self.compositor.as_ref().expect("checked at start");
        let wm_base = self.wm_base.as_ref().expect("checked at start");
        let surface = compositor.create_surface(qh, w);
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, w);
        let toplevel = xdg_surface.get_toplevel(qh, w);
        let title = self.title(w);
        toplevel.set_title(title.clone());
        let class = self.class(w);
        if let Some(c) = &class {
            toplevel.set_app_id(c.clone());
        }
        let mut parent = None;
        if let Some(p) = self.transient_for(w)
            && let Some(ps) = self.windows.get(&p)
            && let Role::Toplevel(pt) = &ps.role
        {
            toplevel.set_parent(Some(pt));
            parent = Some(p);
        }
        surface.commit();
        crate::log!(
            "-showwindows: showing {w:#x} {:?} (class {:?}) {}x{}+{}+{} depth {}{}{}",
            title,
            class,
            width,
            height,
            x,
            y,
            depth,
            if iconic { ", starts iconic" } else { "" },
            if no_input {
                ", takes no keyboard input"
            } else {
                ""
            },
        );
        let client = self.client_of(w);
        self.windows.insert(
            w,
            Shown {
                x,
                y,
                width,
                height,
                depth,
                damage: dmg,
                pixmap: None,
                surface,
                xdg_surface,
                role: Role::Toplevel(toplevel),
                configured: false,
                buffers: None,
                dirty: true,
                frames: 0,
                frame_pending: false,
                committed_at: None,
                client,
                iconic,
                parent,
            },
        );
        Ok(())
    }

    /// An override-redirect window (menu, tooltip, combo list) as an
    /// xdg_popup on a shown window of the same client: the last popup it has
    /// up (a submenu), else its window under the pointer or with the
    /// keyboard, else its transient parent, else any of its windows.
    fn show_popup(&mut self, w: Window, qh: &QueueHandle<State>) -> Result<(), AnyError> {
        let client = self.client_of(w);
        let mine = |s: &&Window, windows: &HashMap<Window, Shown>| {
            windows
                .get(*s)
                .is_some_and(|sh| sh.client == client && sh.frames > 0)
        };
        let parent = self
            .popups
            .iter()
            .rev()
            .find(|p| mine(p, &self.windows))
            .copied()
            .or_else(|| self.pointer_on.filter(|p| mine(&p, &self.windows)))
            .or_else(|| self.keyboard_on.filter(|p| mine(&p, &self.windows)))
            .or_else(|| self.transient_for(w).filter(|p| mine(&p, &self.windows)))
            .or_else(|| {
                self.windows
                    .iter()
                    .filter(|(_, s)| {
                        s.client == client && s.frames > 0 && matches!(s.role, Role::Toplevel(_))
                    })
                    .map(|(w, _)| *w)
                    .min()
            });
        let Some(parent) = parent else {
            crate::vlog!(
                "-showwindows: {w:#x} is override-redirect with no shown window of its client: not shown"
            );
            return Ok(());
        };
        let (x, y, width, height, depth, dmg) = self.watch(w)?;
        let p = self.windows.get(&parent).expect("found above");
        let (dx, dy) = (i32::from(x) - i32::from(p.x), i32::from(y) - i32::from(p.y));
        let ((ax, ay), (ox, oy)) = popup_anchor(dx, dy, p.width, p.height);
        let wm_base = self.wm_base.as_ref().expect("checked at start");
        let positioner = wm_base.create_positioner(qh, ());
        positioner.set_size(i32::from(width), i32::from(height));
        positioner.set_anchor_rect(ax, ay, 1, 1);
        positioner.set_offset(ox, oy);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::empty());
        let compositor = self.compositor.as_ref().expect("checked at start");
        let surface = compositor.create_surface(qh, w);
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, w);
        let popup = xdg_surface.get_popup(Some(&p.xdg_surface), &positioner, qh, w);
        positioner.destroy();
        surface.commit();
        crate::log!(
            "-showwindows: showing {w:#x} (override-redirect, {}) as a popup of {parent:#x} at {dx},{dy} {}x{}",
            self.window_type(w),
            width,
            height
        );
        self.windows.insert(
            w,
            Shown {
                x,
                y,
                width,
                height,
                depth,
                damage: dmg,
                pixmap: None,
                surface,
                xdg_surface,
                role: Role::Popup { popup, parent },
                configured: false,
                buffers: None,
                dirty: true,
                frames: 0,
                frame_pending: false,
                committed_at: None,
                client,
                iconic: false,
                parent: None,
            },
        );
        self.popups.push(w);
        // it is on top on the X screen too, where the reverse pointer goes
        let _ = self
            .x
            .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        Ok(())
    }

    /// `_NET_WM_WINDOW_TYPE`'s first atom's name, for the log.
    fn window_type(&self, w: Window) -> String {
        self.x
            .get_property(
                false,
                w,
                self.atoms.net_wm_window_type,
                AtomEnum::ATOM,
                0,
                1,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .and_then(|a| self.x.get_atom_name(a).ok()?.reply().ok())
            .map(|r| {
                String::from_utf8_lossy(&r.name)
                    .trim_start_matches("_NET_WM_WINDOW_TYPE_")
                    .to_lowercase()
            })
            .unwrap_or_else(|| "untyped".into())
    }

    fn hide(&mut self, w: Window, qh: &QueueHandle<State>) {
        if self.screen_mode() {
            self.remove_member(w);
            return;
        }
        if !self.windows.contains_key(&w) {
            return;
        }
        // popups on it go first (xdg-shell: children before parents), and
        // come back on another parent if their X windows are still up
        let children: Vec<Window> = self
            .popups
            .iter()
            .rev()
            .filter(|p| matches!(self.windows.get(*p).map(|s| &s.role), Some(Role::Popup { parent, .. }) if *parent == w))
            .copied()
            .collect();
        for c in &children {
            self.hide(*c, qh);
        }
        let Some(s) = self.windows.remove(&w) else {
            return;
        };
        self.popups.retain(|p| *p != w);
        crate::log!("-showwindows: {w:#x} went away after {} frame(s)", s.frames);
        // the window may be gone already: errors are expected and harmless
        let _ = damage::destroy(&self.x, s.damage);
        if let Some(p) = s.pixmap {
            let _ = self.x.free_pixmap(p);
        }
        if let Some(b) = s.buffers {
            let _ = shm::detach(&self.x, b.seg);
            for buf in &b.buffers {
                buf.destroy();
            }
            b.pool.destroy();
        }
        if self.lock.as_ref().is_some_and(|l| l.window == w) {
            self.unlock();
        }
        match s.role {
            Role::Toplevel(t) | Role::Screen { toplevel: t, .. } => t.destroy(),
            Role::Popup { popup, .. } => popup.destroy(),
        }
        s.xdg_surface.destroy();
        s.surface.destroy();
        if self.pointer_on == Some(w) {
            self.release_buttons();
            self.pointer_on = None;
        }
        if self.keyboard_on == Some(w) {
            self.release_keys();
            self.keyboard_on = None;
        }
        for c in children {
            self.consider(c, qh);
        }
    }

    fn x_event(&mut self, ev: Event, qh: &QueueHandle<State>) {
        match ev {
            // A client's first top-level window: give it the reverse master
            // now, before it is mapped, as some clients pick their XInput
            // devices once, from their ClientPointer, when their window comes
            // up (onboard's XInput event source)
            Event::CreateNotify(e) if e.parent == self.root && !e.override_redirect => {
                let client = self.client_of(e.window);
                if !self.adopted.contains(&client) {
                    self.adopt_client(e.window);
                }
            }
            Event::MapNotify(e) if e.event == self.root => {
                self.dismissed.retain(|d| *d != e.window);
                self.consider(e.window, qh)
            }
            Event::UnmapNotify(e) if e.event == self.root => self.hide(e.window, qh),
            Event::DestroyNotify(e) => {
                self.dismissed.retain(|d| *d != e.window);
                self.hide(e.window, qh)
            }
            Event::ReparentNotify(e) if e.parent != self.root => self.hide(e.window, qh),
            Event::ConfigureNotify(e) if e.event == self.root => {
                let (w, h) = (e.width + 2 * e.border_width, e.height + 2 * e.border_width);
                if let Some(c) = self.canvas.as_mut() {
                    c.restacked = true;
                    if let Some(m) = c.members.get_mut(&e.window) {
                        (m.x, m.y) = (e.x, e.y);
                        if (w, h) != (m.width, m.height) {
                            (m.width, m.height) = (w, h);
                            Self::drop_pixmap(&self.x, m);
                        }
                    }
                    self.screen_dirty();
                    return;
                }
                if let Some(s) = self.windows.get_mut(&e.window) {
                    (s.x, s.y) = (e.x, e.y);
                    if (w, h) != (s.width, s.height) {
                        if frame_bytes(w, h).is_none() {
                            crate::warning!(
                                "-showwindows: {:#x} grew to {w}x{h}, too big to show",
                                e.window
                            );
                            self.hide(e.window, qh);
                            return;
                        }
                        (s.width, s.height) = (w, h);
                        // the window got a new pixmap
                        if let Some(p) = s.pixmap.take() {
                            let _ = self.x.free_pixmap(p);
                        }
                        s.dirty = true;
                        self.paint(e.window);
                    }
                }
            }
            Event::CirculateNotify(_) => {
                if let Some(c) = self.canvas.as_mut() {
                    c.restacked = true;
                    self.screen_dirty();
                }
            }
            Event::DamageNotify(e) => {
                let _ = damage::subtract(&self.x, e.damage, x11rb::NONE, x11rb::NONE);
                if self
                    .canvas
                    .as_ref()
                    .is_some_and(|c| c.members.contains_key(&e.drawable))
                {
                    self.screen_dirty();
                } else if let Some(s) = self.windows.get_mut(&e.drawable) {
                    s.dirty = true;
                    self.paint(e.drawable);
                }
            }
            Event::PropertyNotify(e)
                if (e.atom == u32::from(AtomEnum::WM_NAME) || e.atom == self.atoms.net_wm_name)
                    && self.windows.contains_key(&e.window) =>
            {
                let title = self.title(e.window);
                if let Some(Shown {
                    role: Role::Toplevel(t),
                    ..
                }) = self.windows.get(&e.window)
                {
                    t.set_title(title);
                }
            }
            Event::XfixesCursorNotify(_) => self.update_cursor(),
            Event::Error(e) => crate::vlog!("-showwindows: X error {e:?}"),
            _ => {}
        }
    }

    /// Work the Wayland handlers queued.
    fn after_dispatch(&mut self, qh: &QueueHandle<State>) {
        for w in std::mem::take(&mut self.frame_done) {
            if let Some(s) = self.windows.get_mut(&w) {
                s.frame_pending = false;
                if let Some(t) = s.committed_at {
                    let ms = t.elapsed().as_millis();
                    if ms > 30
                        && std::env::var_os("XWLBRIDGE_SHOW_FRAMELOG")
                            .is_some_and(|v| !v.is_empty())
                    {
                        crate::warning!(
                            "-showwindows: frame callback of {w:#x} {ms} ms after its commit (dirty: {})",
                            s.dirty
                        );
                    }
                }
            }
            self.paint(w);
        }
        for (w, i) in std::mem::take(&mut self.released) {
            if let Some(b) = self.windows.get_mut(&w).and_then(|s| s.buffers.as_mut()) {
                b.busy[i] = false;
            }
            self.paint(w);
        }
        for w in std::mem::take(&mut self.close) {
            self.close_window(w);
        }
        for (w, width, height) in std::mem::take(&mut self.resize) {
            if w == self.root {
                self.rescale(width, height);
                continue;
            }
            if let Some(s) = self.windows.get(&w)
                && width > 0
                && height > 0
                && (width as u16, height as u16) != (s.width, s.height)
                && frame_bytes(width as u16, height as u16).is_some()
            {
                let _ = self.x.configure_window(
                    w,
                    &ConfigureWindowAux::new()
                        .width(width as u32)
                        .height(height as u32),
                );
            }
        }
        for (w, px, py) in std::mem::take(&mut self.placed) {
            // the compositor put the popup elsewhere: move the X window there,
            // so the reverse pointer finds it where the desktop shows it
            if let Some(s) = self.windows.get(&w)
                && let Role::Popup { parent, .. } = s.role
                && let Some(p) = self.windows.get(&parent)
            {
                let (nx, ny) = (i32::from(p.x) + px, i32::from(p.y) + py);
                if (nx, ny) != (i32::from(s.x), i32::from(s.y)) {
                    crate::vlog!(
                        "-showwindows: popup {w:#x} placed at {nx},{ny}, not {},{}",
                        s.x,
                        s.y
                    );
                    let _ = self
                        .x
                        .configure_window(w, &ConfigureWindowAux::new().x(nx).y(ny));
                }
            }
        }
        for w in std::mem::take(&mut self.popup_done) {
            crate::log!("-showwindows: the desktop dismissed popup {w:#x}");
            self.hide(w, qh);
            self.dismissed.push(w);
        }
        for locked in std::mem::take(&mut self.lock_events) {
            if let Some(l) = self.lock.as_mut() {
                l.active = locked;
                crate::log!(
                    "-showwindows: pointer {} on {:#x}",
                    if locked { "locked" } else { "unlocked" },
                    l.window
                );
            }
        }
        for (dx, dy) in std::mem::take(&mut self.relative) {
            self.relative_motion(dx, dy);
        }
        let dirty: Vec<Window> = self
            .windows
            .iter()
            .filter(|(_, s)| s.dirty)
            .map(|(w, _)| *w)
            .collect();
        for w in dirty {
            self.paint(w);
        }
    }

    /// Copies the window's current contents into a free buffer and commits
    /// it, if it changed, the surface is configured and a buffer is free.
    fn paint(&mut self, w: Window) {
        if let Err(e) = self.try_paint(w) {
            crate::vlog!("-showwindows: can't paint {w:#x}: {e}");
        }
    }

    fn try_paint(&mut self, w: Window) -> Result<(), AnyError> {
        let shm_global = self.shm.clone().expect("checked at start");
        // a transient child's first frame waits for its parent's, so the
        // compositor can place it on the parent (review F4)
        if let Some(s) = self.windows.get(&w)
            && s.frames == 0
            && let Some(p) = s.parent
            && self.windows.get(&p).is_some_and(|p| p.frames == 0)
        {
            return Ok(());
        }
        let realloc = self.windows.get(&w).is_some_and(|s| {
            s.configured
                && s.dirty
                && s.buffers
                    .as_ref()
                    .is_none_or(|b| (b.width, b.height) != (s.width, s.height))
        });
        if realloc {
            // (re)allocate for the current size
            let s = self.windows.get_mut(&w).expect("checked");
            if let Some(b) = s.buffers.take() {
                let _ = shm::detach(&self.x, b.seg);
                for buf in &b.buffers {
                    buf.destroy();
                }
                b.pool.destroy();
            }
            let (width, height) = (s.width, s.height);
            let frame = frame_bytes(width, height).ok_or("too big")?;
            let fd = memfd(frame * 2)?;
            let seg = self.x.generate_id()?;
            shm::attach_fd(&self.x, seg, fd.try_clone()?, false)?;
            let format = if s.depth == 32 {
                wl_shm::Format::Argb8888
            } else {
                wl_shm::Format::Xrgb8888
            };
            let qh = self.qh.clone();
            let pool = shm_global.create_pool(fd.as_fd(), (frame * 2) as i32, &qh, ());
            let mk = |i: usize| {
                pool.create_buffer(
                    (frame * i) as i32,
                    width as i32,
                    height as i32,
                    width as i32 * 4,
                    format,
                    &qh,
                    (w, i),
                )
            };
            let buffers = [mk(0), mk(1)];
            s.buffers = Some(Buffers {
                seg,
                pool,
                buffers,
                busy: [false; 2],
                width,
                height,
                frame,
            });
        }
        let Some(s) = self.windows.get(&w) else {
            return Ok(());
        };
        if !s.configured || !s.dirty || s.frame_pending {
            return Ok(());
        }
        let Some(b) = s.buffers.as_ref() else {
            return Ok(());
        };
        let Some(i) = (0..2).find(|&i| !b.busy[i]) else {
            return Ok(()); // the compositor holds both; repaint on release
        };
        let named = s.pixmap;
        let source = if w == self.root {
            self.compose()?
        } else {
            match named {
                Some(p) => p,
                None => {
                    let p = self.x.generate_id()?;
                    composite::name_window_pixmap(&self.x, w, p)?.check()?;
                    self.windows.get_mut(&w).expect("checked").pixmap = Some(p);
                    p
                }
            }
        };
        let s = self.windows.get_mut(&w).expect("checked");
        let b = s.buffers.as_mut().expect("checked");
        s.dirty = false;
        let reply = shm::get_image(
            &self.x,
            source,
            0,
            0,
            b.width,
            b.height,
            !0,
            x11rb::protocol::xproto::ImageFormat::Z_PIXMAP.into(),
            b.seg,
            (b.frame * i) as u32,
        )?
        .reply();
        if let Err(e) = reply {
            // a pixmap of the old size (resized since); name it again next time
            if w != self.root
                && let Some(p) = s.pixmap.take()
            {
                let _ = self.x.free_pixmap(p);
            }
            s.dirty = true;
            return Err(e.into());
        }
        s.surface.attach(Some(&b.buffers[i]), 0, 0);
        s.surface
            .damage_buffer(0, 0, b.width as i32, b.height as i32);
        if self.throttle {
            s.surface.frame(&self.qh, w);
            s.frame_pending = true;
        }
        s.committed_at = Some(std::time::Instant::now());
        s.surface.commit();
        b.busy[i] = true;
        s.frames += 1;
        if s.frames == 1 {
            crate::log!("-showwindows: first frame of {w:#x} on the desktop");
            if s.iconic
                && let Role::Toplevel(t) = &s.role
            {
                // it asked to start iconic: minimised (review F2)
                t.set_minimized();
                crate::log!("-showwindows: {w:#x} asked to start iconic: minimised");
            }
        }
        Ok(())
    }

    fn close_window(&mut self, w: Window) {
        if w == self.root {
            crate::log!(
                "-showscreen: the desktop closed the screen window; it stays up while the bridge runs"
            );
            return;
        }
        if self.protocols(w).contains(&self.atoms.wm_delete_window) {
            crate::log!("-showwindows: closing {w:#x} (WM_DELETE_WINDOW)");
            let ev = ClientMessageEvent::new(
                32,
                w,
                self.atoms.wm_protocols,
                [self.atoms.wm_delete_window, x11rb::CURRENT_TIME, 0, 0, 0],
            );
            let _ = self.x.send_event(false, w, EventMask::NO_EVENT, ev);
        } else {
            crate::log!(
                "-showwindows: closing {w:#x} (no WM_DELETE_WINDOW: disconnecting its client)"
            );
            let _ = self.x.kill_client(w);
        }
    }

    fn title(&self, w: Window) -> String {
        let get = |prop: u32, ty: u32| {
            self.x
                .get_property(false, w, prop, ty, 0, 1024)
                .ok()
                .and_then(|c| c.reply().ok())
                .filter(|r| r.format == 8 && !r.value.is_empty())
                .map(|r| r.value)
        };
        if let Some(v) = get(self.atoms.net_wm_name, self.atoms.utf8_string) {
            return String::from_utf8_lossy(&v).into_owned();
        }
        // WM_NAME is usually STRING (Latin-1)
        get(AtomEnum::WM_NAME.into(), AtomEnum::ANY.into())
            .map(|v| v.iter().map(|&b| b as char).collect())
            .unwrap_or_default()
    }

    /// WM_CLASS's class part ("XClock"), the closest X has to an app id.
    fn class(&self, w: Window) -> Option<String> {
        let r = self
            .x
            .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        let mut parts = r.value.split(|&b| b == 0);
        let instance = parts.next().unwrap_or_default();
        let class = parts.next().filter(|c| !c.is_empty()).unwrap_or(instance);
        (!class.is_empty()).then(|| String::from_utf8_lossy(class).into_owned())
    }

    fn transient_for(&self, w: Window) -> Option<Window> {
        self.x
            .get_property(false, w, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
            .filter(|&p| p != 0)
    }

    /// Root coordinates of surface-local `(sx, sy)` on `w`'s surface.
    fn to_root(&self, w: Window, sx: f64, sy: f64) -> Option<(i16, i16)> {
        let (x, y) = if w == self.root {
            let scale = self.canvas.as_ref().map_or(1.0, |c| c.scale);
            ((sx / scale).floor() as i32, (sy / scale).floor() as i32)
        } else {
            let s = self.windows.get(&w)?;
            (
                s.x as i32 + sx.floor() as i32,
                s.y as i32 + sy.floor() as i32,
            )
        };
        Some((
            x.clamp(0, self.screen.0 as i32 - 1) as i16,
            y.clamp(0, self.screen.1 as i32 - 1) as i16,
        ))
    }

    /// Moves the reverse pointer to surface-local `(sx, sy)` of `w`.
    fn motion(&mut self, w: Window, sx: f64, sy: f64) {
        let Some((x, y)) = self.to_root(w, sx, sy) else {
            return;
        };
        // mouse-look: a client that hid the cursor and moved the pointer
        // itself (a warp to the centre, typically) wants relative motion
        if self.lock.is_none()
            && self.cursor.as_ref().is_some_and(|c| c.blank)
            && let Some(last) = self.last_pos
            && let Some(now) = self.query_pointer()
            && now != last
        {
            crate::log!(
                "-showwindows: the client moved the hidden pointer ({},{} -> {},{}): locking the desktop's pointer for relative motion",
                last.0,
                last.1,
                now.0,
                now.1
            );
            self.lock_pointer(w);
        }
        self.last_pos = Some((x, y));
        let _ = xtest::fake_input(&self.x, MOTION_NOTIFY, 0, 0, self.root, x, y, 0);
    }

    fn query_pointer(&self) -> Option<(i16, i16)> {
        let r = self
            .x
            .xinput_xi_query_pointer(self.root, self.devs.pointer)
            .ok()?
            .reply()
            .ok()?;
        Some(((r.root_x >> 16) as i16, (r.root_y >> 16) as i16))
    }

    fn lock_pointer(&mut self, w: Window) {
        let (Some(constraints), Some(manager), Some(pointer), Some(s)) = (
            self.constraints.as_ref(),
            self.relative_manager.as_ref(),
            self.pointer.as_ref(),
            self.windows.get(&w),
        ) else {
            return;
        };
        let locked = constraints.lock_pointer(
            &s.surface,
            pointer,
            None,
            zwp_pointer_constraints_v1::Lifetime::Persistent,
            &self.qh,
            (),
        );
        let relative = manager.get_relative_pointer(pointer, &self.qh, ());
        self.lock = Some(Lock {
            locked,
            relative,
            window: w,
            active: false,
            rest: (0.0, 0.0),
        });
    }

    fn unlock(&mut self) {
        let Some(l) = self.lock.take() else {
            return;
        };
        // leave the desktop's pointer where the X pointer is
        if let Some((x, y)) = self.query_pointer()
            && let Some(s) = self.windows.get(&l.window)
        {
            let (sx, sy) = if l.window == self.root {
                let scale = self.canvas.as_ref().map_or(1.0, |c| c.scale);
                (f64::from(x) * scale, f64::from(y) * scale)
            } else {
                (f64::from(x - s.x), f64::from(y - s.y))
            };
            l.locked.set_cursor_position_hint(sx, sy);
            s.surface.commit();
        }
        l.locked.destroy();
        l.relative.destroy();
        crate::log!("-showwindows: pointer lock on {:#x} released", l.window);
    }

    fn relative_motion(&mut self, dx: f64, dy: f64) {
        let Some(l) = self.lock.as_mut() else {
            return;
        };
        if !l.active {
            return;
        }
        let (rx, ry) = (l.rest.0 + dx, l.rest.1 + dy);
        let (ix, iy) = (rx.trunc(), ry.trunc());
        l.rest = (rx - ix, ry - iy);
        if ix == 0.0 && iy == 0.0 {
            return;
        }
        // XTest relative motion: detail 1
        let _ = xtest::fake_input(
            &self.x,
            MOTION_NOTIFY,
            1,
            0,
            self.root,
            ix as i16,
            iy as i16,
            0,
        );
        self.last_pos = None;
    }

    fn button(&mut self, button: u32, pressed: bool) {
        let Some(b) = x_button(button) else {
            return;
        };
        let ty = if pressed {
            BUTTON_PRESS
        } else {
            BUTTON_RELEASE
        };
        if pressed {
            self.buttons_down.push(b);
            if self.screen_mode() {
                self.click_to_focus();
            }
        } else {
            self.buttons_down.retain(|&x| x != b);
        }
        crate::vlog!(
            "-showwindows: button {b} {} on {:?}",
            if pressed { "down" } else { "up" },
            self.pointer_on
        );
        let _ = xtest::fake_input(&self.x, ty, b, 0, self.root, 0, 0, 0);
    }

    fn wheel(&mut self, axis: wl_pointer::Axis, steps: i32) {
        let (neg, pos) = match axis {
            wl_pointer::Axis::VerticalScroll => (4u8, 5u8),
            _ => (6, 7),
        };
        let b = if steps < 0 { neg } else { pos };
        for _ in 0..steps.unsigned_abs().min(20) {
            let _ = xtest::fake_input(&self.x, BUTTON_PRESS, b, 0, self.root, 0, 0, 0);
            let _ = xtest::fake_input(&self.x, BUTTON_RELEASE, b, 0, self.root, 0, 0, 0);
        }
    }

    fn key(&mut self, key: u32, pressed: bool) {
        let Ok(kc) = u8::try_from(key + 8) else {
            return;
        };
        if pressed {
            self.keys_down.push(kc);
        } else {
            self.keys_down.retain(|&k| k != kc);
        }
        let ty = if pressed { KEY_PRESS } else { KEY_RELEASE };
        let _ = xtest::fake_input(&self.x, ty, kc, 0, self.root, 0, 0, 0);
    }

    fn release_keys(&mut self) {
        for kc in std::mem::take(&mut self.keys_down) {
            let _ = xtest::fake_input(&self.x, KEY_RELEASE, kc, 0, self.root, 0, 0, 0);
        }
    }

    fn release_buttons(&mut self) {
        for b in std::mem::take(&mut self.buttons_down) {
            let _ = xtest::fake_input(&self.x, BUTTON_RELEASE, b, 0, self.root, 0, 0, 0);
        }
    }

    /// Gives `w` the reverse keyboard the way an ICCCM window manager gives
    /// the keyboard (§4.1.7): set the focus if its WM_HINTS input is true
    /// (Passive, Locally Active), and send WM_TAKE_FOCUS if it lists it
    /// (Locally and Globally Active: Java focuses itself then). A window with
    /// input false is never focused by us, so an on-screen keyboard (GTK's
    /// accept-focus false; it ignores WM_TAKE_FOCUS) leaves the keyboard with
    /// the window that had it, where its XTest keys then go.
    fn focus_x(&mut self, w: Window) {
        if self.x_focus == Some(w) {
            return;
        }
        let (input, _) = self.hints(w);
        if input {
            self.x_focus = Some(w);
            let _ = self
                .x
                .xinput_xi_set_focus(w, x11rb::CURRENT_TIME, self.devs.keyboard);
        }
        if self.protocols(w).contains(&self.atoms.wm_take_focus) {
            let ev = ClientMessageEvent::new(
                32,
                w,
                self.atoms.wm_protocols,
                [self.atoms.wm_take_focus, x11rb::CURRENT_TIME, 0, 0, 0],
            );
            let _ = self.x.send_event(false, w, EventMask::NO_EVENT, ev);
        }
    }

    /// The reverse master's X cursor (the pointer is on shown windows only,
    /// so its cursor is theirs), as the desktop's cursor over them.
    fn update_cursor(&mut self) {
        let Ok(Ok(img)) = self.x.xfixes_get_cursor_image().map(|c| c.reply()) else {
            return;
        };
        let Some(c) = self.cursor.as_mut() else {
            return;
        };
        if img.cursor_serial == c.serial && c.serial != 0 {
            return;
        }
        c.serial = img.cursor_serial;
        let blank = blank_cursor(&img.cursor_image);
        if blank != c.blank {
            crate::vlog!(
                "-showwindows: the X cursor is {}",
                if blank { "blank" } else { "visible" }
            );
        }
        c.blank = blank;
        if !blank && img.width > 0 && img.height > 0 && img.width <= 256 && img.height <= 256 {
            let (w, h) = (img.width as usize, img.height as usize);
            let shm = self.shm.as_ref().expect("checked at start");
            if let Ok(fd) = memfd(w * h * 4) {
                let bytes: Vec<u8> = img
                    .cursor_image
                    .iter()
                    .flat_map(|p| p.to_le_bytes())
                    .collect();
                let file = std::fs::File::from(fd.try_clone().expect("memfd dup"));
                let _ = std::os::unix::fs::FileExt::write_all_at(&file, &bytes, 0);
                let pool = shm.create_pool(fd.as_fd(), (w * h * 4) as i32, &self.qh, ());
                let buffer = pool.create_buffer(
                    0,
                    w as i32,
                    h as i32,
                    (w * 4) as i32,
                    wl_shm::Format::Argb8888,
                    &self.qh,
                    (0, 9),
                );
                c.surface.attach(Some(&buffer), 0, 0);
                c.surface.damage_buffer(0, 0, w as i32, h as i32);
                c.surface.commit();
                if let Some((p, b)) = c.buffer.replace((pool, buffer)) {
                    b.destroy();
                    p.destroy();
                }
                c.hot = (i32::from(img.xhot), i32::from(img.yhot));
            }
        }
        if blank && self.lock.is_none() {
            // nothing; a warp while blank locks (see motion)
        } else if !blank && self.lock.is_some() {
            self.unlock();
        }
        self.apply_cursor();
    }

    fn apply_cursor(&self) {
        let (Some(p), Some(c)) = (self.pointer.as_ref(), self.cursor.as_ref()) else {
            return;
        };
        if self.pointer_on.is_none() || c.serial == 0 {
            return;
        }
        if c.blank {
            p.set_cursor(self.enter_serial, None, 0, 0);
        } else if c.buffer.is_some() {
            p.set_cursor(self.enter_serial, Some(&c.surface), c.hot.0, c.hot.1);
        }
    }

    // ---- -showscreen ----

    fn show_screen(&mut self, qh: &QueueHandle<State>, display: u32) -> Result<(), AnyError> {
        let (sw, sh) = self.screen;
        if frame_bytes(sw, sh).is_none() {
            return Err("the X screen is too big to show".into());
        }
        let formats = self.x.render_query_pict_formats()?.reply()?;
        let mut by_visual = HashMap::new();
        for s in &formats.screens {
            for d in &s.depths {
                for v in &d.visuals {
                    by_visual.insert(v.visual, v.format);
                }
            }
        }
        let root_visual = self
            .x
            .setup()
            .roots
            .iter()
            .find(|s| s.root == self.root)
            .map(|s| s.root_visual);
        let canvas_format = root_visual
            .and_then(|v| by_visual.get(&v).copied())
            .ok_or("no picture format for the root")?;
        let pixmap = self.x.generate_id()?;
        let depth = self
            .x
            .setup()
            .roots
            .iter()
            .find(|s| s.root == self.root)
            .map_or(24, |s| s.root_depth);
        self.x.create_pixmap(depth, pixmap, self.root, sw, sh)?;
        let picture = self.x.generate_id()?;
        self.x.render_create_picture(
            picture,
            pixmap,
            canvas_format,
            &render::CreatePictureAux::new(),
        )?;
        self.canvas = Some(Canvas {
            pixmap,
            picture,
            members: HashMap::new(),
            order: Vec::new(),
            restacked: true,
            formats: by_visual,
            focus: None,
            scale: 1.0,
        });
        let compositor = self.compositor.as_ref().expect("checked at start");
        let wm_base = self.wm_base.as_ref().expect("checked at start");
        let surface = compositor.create_surface(qh, self.root);
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, self.root);
        let toplevel = xdg_surface.get_toplevel(qh, self.root);
        toplevel.set_title(format!("X display :{display} (xwlbridge)"));
        toplevel.set_app_id("xwlbridge-screen".into());
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ()));
        if viewport.is_some() {
            // fill the work area, scaled to fit (the X screen is the
            // desktop's size, which a window on it can't be)
            toplevel.set_maximized();
        }
        surface.commit();
        crate::log!("-showscreen: showing the {sw}x{sh} X screen as one window");
        self.windows.insert(
            self.root,
            Shown {
                x: 0,
                y: 0,
                width: sw,
                height: sh,
                depth: 24,
                damage: 0,
                pixmap: None,
                surface,
                xdg_surface,
                role: Role::Screen { toplevel, viewport },
                configured: false,
                buffers: None,
                dirty: true,
                frames: 0,
                frame_pending: false,
                committed_at: None,
                client: 0,
                iconic: false,
                parent: None,
            },
        );
        Ok(())
    }

    /// The compositor's size for the screen window: scale to fit it.
    fn rescale(&mut self, width: i32, height: i32) {
        let (sw, sh) = (f64::from(self.screen.0), f64::from(self.screen.1));
        let scale = if width > 0 && height > 0 {
            (f64::from(width) / sw).min(f64::from(height) / sh).min(1.0)
        } else {
            1.0
        };
        let Some(c) = self.canvas.as_mut() else {
            return;
        };
        if (scale - c.scale).abs() < 1e-6 {
            return;
        }
        c.scale = scale;
        if let Some(Shown {
            role: Role::Screen {
                viewport: Some(v), ..
            },
            ..
        }) = self.windows.get(&self.root)
        {
            let (dw, dh) = ((sw * scale).round() as i32, (sh * scale).round() as i32);
            v.set_destination(dw.max(1), dh.max(1));
            crate::log!("-showscreen: shown at {dw}x{dh} (scale {scale:.3})");
        }
        self.screen_dirty();
    }

    fn screen_dirty(&mut self) {
        if let Some(s) = self.windows.get_mut(&self.root) {
            s.dirty = true;
        }
        self.paint(self.root);
    }

    fn add_member(
        &mut self,
        w: Window,
        override_redirect: bool,
        visual: u32,
    ) -> Result<(), AnyError> {
        if self
            .canvas
            .as_ref()
            .is_some_and(|c| c.members.contains_key(&w))
        {
            return Ok(());
        }
        let format = self
            .canvas
            .as_ref()
            .and_then(|c| c.formats.get(&visual).copied())
            .ok_or("no picture format")?;
        let (mut x, mut y, mut width, mut height, depth, damage) = self.watch(w)?;
        if !override_redirect {
            // no window manager keeps a managed window on the screen: one
            // bigger than the screen (OBS asks for 1280x960 on 1280x720) or
            // off it has parts no pointer can reach, so fit it, as a window
            // manager would
            let (sw, sh) = self.screen;
            let (fw, fh) = (width.min(sw), height.min(sh));
            let (fx, fy) = (fit(x, fw, i32::from(sw)), fit(y, fh, i32::from(sh)));
            if (fx, fy, fw, fh) != (x, y, width, height) {
                let g = self.x.get_geometry(w)?.reply()?;
                let b = 2 * g.border_width;
                crate::log!(
                    "-showscreen: fitting {w:#x} {width}x{height}+{x}+{y} into the screen: {fw}x{fh}+{fx}+{fy}"
                );
                self.x.configure_window(
                    w,
                    &ConfigureWindowAux::new()
                        .x(i32::from(fx))
                        .y(i32::from(fy))
                        .width(u32::from(fw.saturating_sub(b).max(1)))
                        .height(u32::from(fh.saturating_sub(b).max(1))),
                )?;
                (x, y, width, height) = (fx, fy, fw, fh);
            }
        }
        let no_input = !override_redirect && self.takes_no_input(w);
        if !override_redirect {
            self.adopt_client(w);
        }
        crate::log!(
            "-showscreen: compositing {w:#x} {:?} {}x{}+{}+{}{}",
            self.title(w),
            width,
            height,
            x,
            y,
            if override_redirect {
                " (override-redirect)"
            } else {
                ""
            }
        );
        let c = self.canvas.as_mut().expect("screen mode");
        c.members.insert(
            w,
            Member {
                x,
                y,
                width,
                height,
                depth,
                format,
                damage,
                pixmap: None,
                picture: None,
                override_redirect,
                no_input,
            },
        );
        c.restacked = true;
        // a new managed window gets the keyboard, as under a window manager
        if !override_redirect && !no_input {
            c.focus = Some(w);
            if self.keyboard_on == Some(self.root) {
                self.focus_x(w);
            }
        }
        self.screen_dirty();
        Ok(())
    }

    fn drop_pixmap(x: &RustConnection, m: &mut Member) {
        if let Some(p) = m.picture.take() {
            let _ = x.render_free_picture(p);
        }
        if let Some(p) = m.pixmap.take() {
            let _ = x.free_pixmap(p);
        }
    }

    fn remove_member(&mut self, w: Window) {
        let Some(c) = self.canvas.as_mut() else {
            return;
        };
        let Some(mut m) = c.members.remove(&w) else {
            return;
        };
        let _ = damage::destroy(&self.x, m.damage);
        Self::drop_pixmap(&self.x, &mut m);
        c.restacked = true;
        if c.focus == Some(w) {
            // the topmost managed window left takes the keyboard
            c.focus = c
                .order
                .iter()
                .rev()
                .find(|o| {
                    c.members
                        .get(o)
                        .is_some_and(|m| !m.override_redirect && !m.no_input)
                })
                .copied();
            if let Some(f) = c.focus
                && self.keyboard_on == Some(self.root)
            {
                self.focus_x(f);
            }
        }
        if self.x_focus == Some(w) {
            self.x_focus = None;
        }
        self.screen_dirty();
    }

    /// Composites the screen's windows onto the canvas; the canvas pixmap.
    fn compose(&mut self) -> Result<u32, AnyError> {
        let (sw, sh) = self.screen;
        let restack = self.canvas.as_ref().is_some_and(|c| c.restacked);
        if restack {
            let order = self.x.query_tree(self.root)?.reply()?.children;
            let c = self.canvas.as_mut().expect("screen mode");
            c.order = order
                .into_iter()
                .filter(|w| c.members.contains_key(w))
                .collect();
            c.restacked = false;
        }
        let c = self.canvas.as_mut().expect("screen mode");
        self.x.render_fill_rectangles(
            render::PictOp::SRC,
            c.picture,
            SCREEN_BG,
            &[x11rb::protocol::xproto::Rectangle {
                x: 0,
                y: 0,
                width: sw,
                height: sh,
            }],
        )?;
        for w in c.order.clone() {
            let Some(m) = c.members.get_mut(&w) else {
                continue;
            };
            if m.picture.is_none() {
                let p = self.x.generate_id()?;
                if composite::name_window_pixmap(&self.x, w, p)?
                    .check()
                    .is_err()
                {
                    continue;
                }
                let pic = self.x.generate_id()?;
                self.x
                    .render_create_picture(pic, p, m.format, &render::CreatePictureAux::new())?;
                m.pixmap = Some(p);
                m.picture = Some(pic);
            }
            let op = if m.depth == 32 {
                render::PictOp::OVER
            } else {
                render::PictOp::SRC
            };
            self.x.render_composite(
                op,
                m.picture.expect("named above"),
                x11rb::NONE,
                c.picture,
                0,
                0,
                0,
                0,
                m.x,
                m.y,
                m.width,
                m.height,
            )?;
        }
        Ok(c.pixmap)
    }

    /// `-showscreen`: a click gives the keyboard to (and raises) the
    /// managed window under the pointer, as a click-to-focus window manager.
    fn click_to_focus(&mut self) {
        let Some((px, py)) = self.last_pos.or_else(|| self.query_pointer()) else {
            return;
        };
        let Some(c) = self.canvas.as_mut() else {
            return;
        };
        let hit = c.order.iter().rev().copied().find(|w| {
            c.members.get(w).is_some_and(|m| {
                px >= m.x
                    && py >= m.y
                    && i32::from(px) < i32::from(m.x) + i32::from(m.width)
                    && i32::from(py) < i32::from(m.y) + i32::from(m.height)
            })
        });
        let Some(w) = hit else {
            return;
        };
        let m = &c.members[&w];
        if m.override_redirect || m.no_input {
            return; // a menu, or a window that never takes the keyboard
        }
        c.focus = Some(w);
        let _ = self
            .x
            .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        self.focus_x(w);
    }
}

fn memfd(len: usize) -> Result<OwnedFd, AnyError> {
    let fd = unsafe { libc::memfd_create(c"xwlbridge-window".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(fd)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &WlConnection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(2), qh, ())),
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(5), qh, ()))
                }
                "zwp_pointer_constraints_v1" => {
                    state.constraints = Some(registry.bind(name, 1, qh, ()))
                }
                "zwp_relative_pointer_manager_v1" => {
                    state.relative_manager = Some(registry.bind(name, 1, qh, ()))
                }
                "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &WlConnection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            // The seat's devices come and go (a pointer is a capability only
            // while some pointer device exists: on a headless or remote
            // desktop that may be a remote-desktop session's virtual one), and
            // a wl_pointer of a capability that went away gets no events any
            // more: let it go, and take a new one when it is back.
            if caps.contains(wl_seat::Capability::Pointer) {
                if state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
            } else if let Some(p) = state.pointer.take() {
                crate::vlog!("-showwindows: the seat has no pointer now");
                state.release_buttons();
                state.pointer_on = None;
                if state.lock.is_some() {
                    state.unlock();
                }
                if p.version() >= 3 {
                    p.release();
                }
            }
            if caps.contains(wl_seat::Capability::Keyboard) {
                if state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            } else if let Some(k) = state.keyboard.take() {
                crate::vlog!("-showwindows: the seat has no keyboard now");
                state.release_keys();
                state.keyboard_on = None;
                if k.version() >= 3 {
                    k.release();
                }
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, Window> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        w: &Window,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if let Some(s) = state.windows.get_mut(w) {
                s.configured = true;
                // an ack must be followed by a commit; a fresh frame does it
                s.dirty = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, Window> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        w: &Window,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Close => state.close.push(*w),
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.resize.push((*w, width, height))
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, Window> for State {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        w: &Window,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure { x, y, .. } => state.placed.push((*w, x, y)),
            xdg_popup::Event::PopupDone => state.popup_done.push(*w),
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (Window, usize)> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &(Window, usize),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && data.1 < 2
        {
            state.released.push(*data);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, Window> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        data: &Window,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_done.push(*data);
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                let Some(&w) = surface.data::<Window>() else {
                    return;
                };
                if w == 0 {
                    return; // the cursor surface
                }
                state.pointer_on = Some(w);
                state.enter_serial = serial;
                if w != state.root {
                    // the reverse pointer must hit this window, not one
                    // stacked above it on the X screen; the popups shown stay
                    // above it
                    let _ = state.x.configure_window(
                        w,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    );
                    for p in state.popups.clone() {
                        if p != w {
                            let _ = state.x.configure_window(
                                p,
                                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                            );
                        }
                    }
                }
                state.motion(w, surface_x, surface_y);
                state.apply_cursor();
            }
            wl_pointer::Event::Leave { .. } => {
                state.release_buttons();
                state.pointer_on = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(w) = state.pointer_on {
                    state.motion(w, surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(bs),
                ..
            } if state.pointer_on.is_some() => {
                state.button(button, bs == wl_pointer::ButtonState::Pressed);
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } if state.pointer_on.is_some() => {
                state.wheel(axis, discrete);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                let Some(&w) = surface.data::<Window>() else {
                    return;
                };
                state.keyboard_on = Some(w);
                if w == state.root {
                    if let Some(f) = state.canvas.as_ref().and_then(|c| c.focus) {
                        state.focus_x(f);
                    }
                } else {
                    state.focus_x(w);
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                state.release_keys();
                state.keyboard_on = None;
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(ks),
                ..
            } if state.keyboard_on.is_some() => {
                state.key(key, ks == wl_keyboard::KeyState::Pressed);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.lock_events.push(true),
            zwp_locked_pointer_v1::Event::Unlocked => state.lock_events.push(false),
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            state.relative.push((dx, dy));
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore xdg_positioner::XdgPositioner);
wayland_client::delegate_noop!(State: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
wayland_client::delegate_noop!(State: ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
wayland_client::delegate_noop!(State: ignore wp_viewporter::WpViewporter);
wayland_client::delegate_noop!(State: ignore wp_viewport::WpViewport);

impl Dispatch<wl_surface::WlSurface, Window> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &Window,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        // enter/leave (which output) matter for HiDPI, not yet handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_master_length_counts_header_and_padding() {
        // "xwlbridge-reverse": 4 + 4 + 17 = 25 bytes, padded to 28
        assert_eq!(add_master_len(MASTER_NAME.len()), 7);
        assert_eq!(add_master_len(0), 2);
        assert_eq!(add_master_len(4), 3);
        assert_eq!(add_master_len(5), 4);
    }

    #[test]
    fn windows_are_moved_wholly_on_screen() {
        assert_eq!(fit(40, 320, 1280), 40);
        assert_eq!(fit(1200, 320, 1280), 960);
        assert_eq!(fit(-30, 320, 1280), 0);
        assert_eq!(fit(100, 2000, 1280), 0); // bigger than the screen: its origin at least
    }

    #[test]
    fn linux_buttons_map_to_x_buttons() {
        assert_eq!(x_button(0x110), Some(1));
        assert_eq!(x_button(0x111), Some(3));
        assert_eq!(x_button(0x112), Some(2));
        assert_eq!(x_button(0x113), Some(8));
        assert_eq!(x_button(0x114), Some(9));
        assert_eq!(x_button(0x115), None);
    }

    #[test]
    fn the_reverse_master_owns_only_its_devices() {
        let d = Devices {
            pointer: 8,
            keyboard: 9,
        };
        assert!(d.owns(8) && d.owns(9));
        assert!(!d.owns(2) && !d.owns(3) && !d.owns(10));
    }

    #[test]
    fn oversized_windows_are_refused_without_overflow() {
        assert_eq!(frame_bytes(1280, 720), Some(1280 * 720 * 4));
        assert_eq!(frame_bytes(16384, 16384), None); // 2 x 1 GiB: over the cap (review F3)
        assert_eq!(frame_bytes(32767, 32767), None);
        assert_eq!(frame_bytes(0, 10), None);
        assert!(frame_bytes(8192, 8192).is_some()); // 2 x 256 MiB
    }

    #[test]
    fn popup_anchors_lie_in_the_parent() {
        // a menu below a 300x200 window's bottom edge
        assert_eq!(popup_anchor(10, 220, 300, 200), ((10, 199), (0, 21)));
        // a submenu left of it
        assert_eq!(popup_anchor(-50, 30, 300, 200), ((0, 30), (-50, 0)));
        assert_eq!(popup_anchor(40, 50, 300, 200), ((40, 50), (0, 0)));
    }

    #[test]
    fn cursor_blankness() {
        assert!(blank_cursor(&[0, 0x00ff_ffff]));
        assert!(!blank_cursor(&[0, 0xff00_0000]));
    }

    #[test]
    fn a_dead_reverse_connection_is_forgotten() {
        CONNECTION_BASE.store(0x600000, Ordering::Relaxed);
        assert!(is_reverse_client(0x600000));
        forget_client(0x800000);
        assert!(is_reverse_client(0x600000));
        forget_client(0x600000);
        assert!(!is_reverse_client(0x600000));
        assert!(!is_reverse_client(0));
    }
}
