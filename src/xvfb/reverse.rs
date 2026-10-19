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
//!   keyboard is given ICCCM-style, see `focus_x`); when the desktop's
//!   keyboard leaves our windows, the X focus is cleared too, so X clients
//!   see FocusOut as on Xwayland while a Wayland window is active. The X
//!   cursor of that master is the desktop's cursor over shown windows.
//!   Mouse-look: when the cursor is blank and the client warps the pointer or
//!   holds a pointer grab (SDL2's relative mode), the desktop's pointer is
//!   locked (pointer-constraints) and its unaccelerated relative motion is
//!   injected as relative XTest motion, as Xwayland emulates warps.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{Connection as WlConnection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use wayland_protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2, zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2,
    zwp_tablet_pad_strip_v2, zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2,
    zwp_tablet_v2,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::dialog::v1::client::{xdg_dialog_v1, xdg_wm_dialog_v1};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols::xdg::toplevel_tag::v1::client::xdg_toplevel_tag_manager_v1;
use x11rb::connection::Connection;
use x11rb::protocol::render::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, CONFIGURE_NOTIFY_EVENT, ClientMessageEvent, ConfigureNotifyEvent, ConfigureWindowAux,
    ConnectionExt as _, EventMask, MapState, PropMode, StackMode, Window, WindowClass,
};
use x11rb::protocol::{Event, composite, damage, shm, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::selfdrag;
use super::winsync::{self, Due, Raise, Rect, Request};
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

/// Scroll distance (surface pixels) of one emulated wheel click for smooth
/// (touchpad, continuous) scrolling: Xwayland's step.
const SCROLL_STEP: f64 = 10.0;

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

/// Named X pixmaps include the border: their shown origin is one border
/// width before the client origin returned by TranslateCoordinates.
fn shown_origin(client: (i16, i16), border: u16) -> (i32, i32) {
    (
        i32::from(client.0) - i32::from(border),
        i32::from(client.1) - i32::from(border),
    )
}

/// Popup positions and xdg_popup.configure use the same shown-parent origin.
fn popup_position(parent: (i32, i32), offset: (i32, i32)) -> (i32, i32) {
    (parent.0 + offset.0, parent.1 + offset.1)
}

/// The relative pointer of a held button on a synced toplevel (its user
/// data): its motion picks where the desktop's pointer is while the client
/// moves its own window (`selfdrag`).
struct DragRel;

/// A frame callback of a held self-drag's window (its user data), committed
/// after the reply to one of the drag's MoveResize requests: when it fires,
/// the window has been laid out at that origin (`SelfDrag::laid_out`).
struct DragMark {
    drag: u64,
    origin: usize,
}

/// A held self-drag of a synced toplevel (`begin_move`): the window, its
/// number (for its frame callbacks), whether wl_pointer drives it (else a
/// touch point or a tablet tool does) and where the desktop's pointer is.
struct Drag {
    window: Window,
    id: u64,
    wl_pointer: bool,
    pointer: selfdrag::SelfDrag,
}

impl Drag {
    /// wl_pointer's events on `on` are this drag's.
    fn of_pointer(&self, on: Option<Window>) -> bool {
        self.wl_pointer && on == Some(self.window)
    }
}

/// CLOCK_MONOTONIC in milliseconds, wrapping: the clock of the compositor's
/// event times (libinput's, and mutter's own for virtual devices).
fn monotonic_ms() -> Option<u32> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes the timespec it is given.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    Some((ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32)
}

/// Keep the pointer mapping stable while the compositor owns a client drag.
fn mirror_position(moving: Option<Window>, w: Window, buttons: &[u8]) -> bool {
    moving != Some(w) || buttons.is_empty()
}

/// A globally active client (Wine included) needs both focus and its protocol
/// notification. A No Input window still leaves the previous focus alone.
fn focus_actions(input: bool, protocols: &[u32], take_focus: u32) -> (bool, bool) {
    let take = protocols.contains(&take_focus);
    (input || take, take)
}

/// Whether an override-redirect window is a toolkit's invisible helper (off
/// the screen, or at most 2x2) rather than something to show.
fn helper_window(x: i32, y: i32, w: i32, h: i32, screen: (u16, u16)) -> bool {
    let (sw, sh) = (i32::from(screen.0), i32::from(screen.1));
    x + w <= 0 || y + h <= 0 || x >= sw || y >= sh || (w <= 2 && h <= 2)
}

/// WM_NORMAL_HINTS (as 32-bit values) -> ((min w, h), (max w, h)), 0 = none;
/// values that are no window size (Qt's 16777215, negative) count as none.
fn size_limits(v: &[u32]) -> ((i32, i32), (i32, i32)) {
    let flags = v.first().copied().unwrap_or(0);
    let ok = |n: u32| {
        if n > 0 && n <= u32::from(MAX_SIDE) {
            n as i32
        } else {
            0
        }
    };
    let min = if flags & (1 << 4) != 0 && v.len() >= 7 {
        (ok(v[5]), ok(v[6]))
    } else {
        (0, 0)
    };
    let max = if flags & (1 << 5) != 0 && v.len() >= 9 {
        (ok(v[7]), ok(v[8]))
    } else {
        (0, 0)
    };
    (min, max)
}

/// xdg_toplevel configure states -> (maximised, fullscreen).
fn toplevel_states(states: &[u8]) -> (bool, bool) {
    let mut out = (false, false);
    for c in states.chunks_exact(4) {
        match u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) {
            1 => out.0 = true,
            2 => out.1 = true,
            _ => {}
        }
    }
    out
}

/// Adds `value` pixels of smooth scrolling to `rest` and takes whole
/// `SCROLL_STEP`s out of it as wheel clicks (signed, + = down/right).
fn scroll_steps(rest: &mut f64, value: f64) -> i32 {
    *rest += value;
    let steps = (*rest / SCROLL_STEP).trunc();
    *rest -= steps * SCROLL_STEP;
    steps as i32
}

/// Drops a scroll gesture's carried remainder and frame marks (the pointer
/// entered or left a shown window, or the seat lost its pointer): the next
/// gesture's wheel clicks are measured from nothing, not against the last
/// one's fraction of a step.
fn scroll_forgets(
    rest: &mut (f64, f64),
    source: &mut Option<wl_pointer::AxisSource>,
    discrete_seen: &mut bool,
) {
    *rest = (0.0, 0.0);
    *source = None;
    *discrete_seen = false;
}

/// Whether a cursor image shows nothing (every pixel transparent).
fn blank_cursor(image: &[u32]) -> bool {
    image.iter().all(|p| p >> 24 == 0)
}

/// wl_keyboard.enter: the keyboard is on shown window `w`, and a pending
/// leave is withdrawn (a leave followed by an enter on another of our
/// windows in the same batch keeps the focus where `focus_x` put or left it).
fn on_keyboard_enter(keyboard_left: &mut bool, keyboard_on: &mut Option<Window>, w: Window) {
    *keyboard_left = false;
    *keyboard_on = Some(w);
}

/// wl_keyboard.leave: the desktop took the keyboard from our windows.
fn on_keyboard_leave(keyboard_left: &mut bool, keyboard_on: &mut Option<Window>) {
    *keyboard_left = true;
    *keyboard_on = None;
}

/// Whether the X focus goes with the keyboard after a dispatch batch: the
/// desktop took the keyboard (leave) and, by the end of the batch, gave it to
/// none of our windows.
fn x_focus_clears(keyboard_left: bool, keyboard_on: Option<Window>) -> bool {
    keyboard_left && keyboard_on.is_none()
}

/// wl_touch.down on shown window `w`: whether the first point of a touch
/// sequence starts driving the reverse pointer (Xwayland's pointer emulation
/// for core clients: it moves the pointer and holds button 1 until the
/// lift). A second point down suppresses emulation, as does the cursor
/// surface (window 0). `down` is the count of points down, `id` the new
/// point's.
fn touch_emulation_starts(
    emulating: &mut Option<(i32, Window)>,
    down: u32,
    id: i32,
    w: Window,
) -> Option<Window> {
    if w == 0 || down != 1 {
        return None;
    }
    *emulating = Some((id, w));
    Some(w)
}

/// wl_touch.motion of point `id`: whether it moves the reverse pointer (only
/// the emulating point's motion does).
fn touch_motion_drives(emulating: Option<(i32, Window)>, id: i32) -> bool {
    emulating.is_some_and(|(t, _)| t == id)
}

/// wl_touch.up of point `id`: whether that was the emulating point (button 1
/// goes up and the emulation ends); another point's lift changes nothing.
fn touch_lift_ends(emulating: &mut Option<(i32, Window)>, id: i32) -> bool {
    if emulating.is_some_and(|(t, _)| t == id) {
        *emulating = None;
        true
    } else {
        false
    }
}

/// wl_touch.cancel, or the seat lost its touch capability: every point
/// lifted; button 1 goes up if the emulation held it.
fn touch_emulation_ends(emulating: &mut Option<(i32, Window)>) -> bool {
    emulating.take().is_some()
}

/// The presses of a tablet tool's frame that get injected, in frame order,
/// after updating the buttons the tool holds (BTN_* codes): a press of a
/// button it already holds, or a lift of one it doesn't, is dropped (the
/// frame repeated it, or the tool left proximity in between).
fn tool_frame_presses(buttons: &mut Vec<u32>, presses: &[(u32, bool)]) -> Vec<(u32, bool)> {
    let mut out = Vec::new();
    for &(code, pressed) in presses {
        if pressed {
            if buttons.contains(&code) {
                continue;
            }
            buttons.push(code);
        } else {
            if !buttons.contains(&code) {
                continue;
            }
            buttons.retain(|&c| c != code);
        }
        out.push((code, pressed));
    }
    out
}

/// A tablet tool left proximity (or was removed): everything it holds is
/// released, in the order it was pressed; the reverse pointer's window goes
/// too, unless the mouse's own pointer had entered a shown window (then the
/// pointer stays the mouse's).
fn tool_out_releases(buttons: &[u32], pointer_entered: bool) -> (Vec<u32>, bool) {
    (buttons.to_vec(), !pointer_entered)
}

/// Whether the reverse pointer switches to relative (locked) delivery: no
/// lock is held, the client hid the cursor and a client's pointer grab was
/// seen to confine the pointer to a window (SDL2's relative mode: a core
/// `GrabPointer` with confine-to, hidden cursor, XI2 raw motion, no warps; a
/// grab without confine-to — GTK 4's XI2 grabs, a pop-up grab — does not
/// count, and nothing does without RECORD, which sees the grabs).
fn relative_mode_due(no_lock: bool, blank: bool, confining: bool) -> bool {
    no_lock && blank && confining
}

/// Whether a held pointer lock is released and absolute motion resumes: the
/// cursor is visible again; or the lock was taken for a client's confining
/// grab and the grab is gone (a warp-triggered lock ends only when the
/// cursor shows).
fn relative_mode_ends(locked: bool, blank: bool, grab: bool, confining: bool) -> bool {
    locked && (!blank || (grab && !confining))
}

/// A Linux BTN_* code (Wayland's wl_pointer.button) as an X button, numbered
/// as Xwayland numbers them: left, middle and right are 1, 2, 3; 4-7 are the
/// wheel; every other button goes in order from BTN_SIDE = 8 (BTN_EXTRA 9,
/// BTN_FORWARD 10, BTN_BACK 11, BTN_TASK 12 ...). The reverse master's XTest
/// device has 10 buttons (the X server's core pointer), so 11 and up can't
/// be injected: those are dropped (logged once by `State::button`).
fn x_button(code: u32) -> Option<u8> {
    Some(match code {
        0x110 => 1,                                            // BTN_LEFT
        0x112 => 2,                                            // BTN_MIDDLE
        0x111 => 3,                                            // BTN_RIGHT
        0x113..=0x11f => u8::try_from(8 + code - 0x113).ok()?, // BTN_SIDE ... BTN_MOUSE + 15
        _ => return None,
    })
}

/// The X server's core (and XTest) pointer has 10 buttons.
const XTEST_BUTTONS: u8 = 10;

/// The X button a BTN_* code injects: `x_button`'s, if the XTest device has
/// one there (it has 10; buttons 11 and up are dropped).
fn injectable_button(code: u32) -> Option<u8> {
    x_button(code).filter(|&b| b <= XTEST_BUTTONS)
}

/// A tablet tool button (BTN_STYLUS, BTN_STYLUS2, BTN_STYLUS3) as the BTN_*
/// code of the mouse button Xwayland's stylus device reports: middle, right,
/// and side (8).
fn tool_button(code: u32) -> Option<u32> {
    Some(match code {
        0x14b => 0x112, // BTN_STYLUS -> middle
        0x14c => 0x111, // BTN_STYLUS2 -> right
        0x149 => 0x113, // BTN_STYLUS3 -> 8
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
    net_wm_state: u32,
    net_wm_state_modal: u32,
    net_wm_state_max_vert: u32,
    net_wm_state_max_horz: u32,
    net_wm_state_fullscreen: u32,
    net_wm_state_hidden: u32,
    motif_wm_hints: u32,
    wm_change_state: u32,
    net_wm_moveresize: u32,
    net_supported: u32,
    net_supporting_wm_check: u32,
    net_active_window: u32,
    net_wm_state_above: u32,
    net_wm_state_stays_on_top: u32,
    net_wm_state_demands_attention: u32,
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
    /// Where pointer positions on the surface map from (root = this +
    /// surface-local): the outer position as the X server has it, held
    /// during a held-button self-drag (0.1.2); with the window sync, where
    /// the desktop has the window (`winsync::pointer_origin`).
    x: i16,
    y: i16,
    /// Outer size (border included), as the X server has it.
    width: u16,
    height: u16,
    /// The outer position as the X server has it (always).
    at: (i16, i16),
    border: u16,
    /// The sibling below it in its last ConfigureNotify (None: none seen).
    above: Option<Window>,
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
    /// A modal dialog's xdg_dialog_v1 (GNOME attaches it to its parent).
    dialog: Option<xdg_dialog_v1::XdgDialogV1>,
    /// The compositor's last maximised / fullscreen state, as told to X.
    maximized: bool,
    fullscreen: bool,
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
    /// Taken for a client's confining pointer grab (rather than for a warp
    /// of a hidden pointer): it ends when the grab does.
    grab: bool,
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

/// `-showwindows`' window sync with the desktop, through the bundled GNOME
/// Shell extension `xwlbridge-windows@xwlbridge` (`winsync`; design:
/// research/release/design_desktop_integration.md section 2,
/// design_x_capability_gaps.md sections A and B). Everything it changes
/// happens only while the extension is `available`; otherwise (no
/// extension, an old one, the lock screen, a build without `gnome`, or
/// `XWLBRIDGE_WINDOWS_EXTENSION=off`) the bridge behaves as 0.1.2.
struct Sync {
    client: winsync::bus::Client,
    /// The extension is there and compatible.
    available: bool,
    /// The sync is on: `available`, and the X screen holds the desktop's
    /// monitor layout one to one (`winsync::layout_fits`).
    on: bool,
    /// The desktop's monitors as last reported (None: not yet).
    monitors: Option<Vec<winsync::MonitorInfo>>,
    /// Why the layout keeps the sync off, as last logged.
    layout_why: Option<String>,
    /// The order stamp of the stacking order in `order`.
    order_stamp: Option<u32>,
    geo: winsync::Geometry,
    gate: winsync::RaiseGate,
    layers: winsync::Layers,
    attention: winsync::Attention,
    /// The desktop's stacking order of our shown windows, bottom to top.
    order: Vec<Window>,
    /// Our restacks of X windows not seen back yet: (window -> the request's
    /// sequence number). The window's first restack at or after it is ours.
    stack_echo: HashMap<Window, u64>,
    /// When each window's last MoveResize went, and the windows whose next
    /// one waits for its pacing time (`winsync::send_due`).
    sent_at: HashMap<Window, Instant>,
    due: HashMap<Window, Instant>,
}

/// `XWLBRIDGE_WINDOWS_EXTENSION=off` (what `xwlbridge-ctl run` exports for
/// the setting `extension=off`) turns the window sync off.
fn sync_turned_off(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.trim().eq_ignore_ascii_case("off"))
}

/// Starts the window sync's D-Bus client, unless it is turned off or cannot
/// work here (each case is logged; the bridge then keeps the 0.1.2
/// behaviour).
fn start_sync(display: u32, tags: bool) -> Option<Sync> {
    let keep = "windows keep the 0.1.2 behaviour";
    if sync_turned_off(std::env::var("XWLBRIDGE_WINDOWS_EXTENSION").ok().as_deref()) {
        crate::log!("-showwindows: window sync with the desktop is off (extension=off): {keep}");
        return None;
    }
    if !tags {
        crate::log!(
            "-showwindows: no window sync with the desktop (the compositor has no xdg_toplevel_tag_manager_v1): {keep}"
        );
        return None;
    }
    match winsync::bus::Client::spawn(display) {
        Ok(client) => Some(Sync {
            client,
            available: false,
            on: false,
            monitors: None,
            layout_why: None,
            order_stamp: None,
            geo: winsync::Geometry::new(),
            gate: winsync::RaiseGate::new(),
            layers: winsync::Layers::new(),
            attention: winsync::Attention::new(),
            order: Vec::new(),
            stack_echo: HashMap::new(),
            sent_at: HashMap::new(),
            due: HashMap::new(),
        }),
        Err(e) => {
            crate::log!("-showwindows: no window sync with the desktop ({e}): {keep}");
            None
        }
    }
}

/// The synthetic ConfigureNotify an X window manager sends after moving or
/// resizing a client's window (ICCCM 4.1.5): `w`'s geometry in root
/// coordinates (`x`, `y` the outer corner; `width`, `height` inside the
/// border).
fn synthetic_configure(
    w: Window,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    border: u16,
) -> ConfigureNotifyEvent {
    let i16_of = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    let u16_of = |v: u32| v.min(u32::from(u16::MAX)) as u16;
    ConfigureNotifyEvent {
        response_type: CONFIGURE_NOTIFY_EVENT,
        sequence: 0,
        event: w,
        window: w,
        above_sibling: x11rb::NONE,
        x: i16_of(x),
        y: i16_of(y),
        width: u16_of(width),
        height: u16_of(height),
        border_width: border,
        override_redirect: false,
    }
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
    /// The queue xdg_wm_base's pings are answered on (`spawn_pinger`).
    ping_qh: QueueHandle<Pinger>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    dialogs: Option<xdg_wm_dialog_v1::XdgWmDialogV1>,
    /// xdg_toplevel_tag_v1: every shown toplevel is tagged
    /// `xwlbridge:<display>:<xid>` (`winsync::tag`), the name the desktop
    /// side of the window sync finds it by.
    tags: Option<xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1>,
    /// The engine display's number (for the tags).
    display: u32,
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
    /// The desktop took the keyboard from our windows (wl_keyboard.leave)
    /// and, by the end of the dispatch, gave it to none of them: the X
    /// focus goes too (see `after_dispatch`).
    keyboard_left: bool,
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
    /// Popups the compositor dismissed.
    popup_done: Vec<Window>,
    /// Pointer-lock state changes: (window, locked).
    lock_events: Vec<bool>,
    /// Relative motion while locked.
    relative: Vec<(f64, f64)>,
    /// Toplevel states the compositor configured: (window, maximised, fullscreen).
    states: Vec<(Window, bool, bool)>,
    /// Serial of the last button press on a shown window (for an interactive move).
    press_serial: u32,
    /// The shown window the first of the held buttons went down on: X is
    /// authoritative for its geometry until the last comes up.
    held_on: Option<Window>,
    /// While a button is held on a synced toplevel: a relative pointer, and
    /// once the client moves the window itself, the drag (`selfdrag`): the
    /// surface moves under the pointer while the desktop carries out the
    /// client's moves, so which of the window's positions an event is
    /// relative to has to be worked out. The relative pointer's motion picks
    /// it for relative devices.
    drag_rel: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    drag: Option<Drag>,
    /// The current wl_pointer frame's events during a drag.
    drag_parts: selfdrag::Parts,
    /// Drags so far (numbers them).
    drags: u64,
    /// The reverse pointer's fractional desktop positions, a drag's start.
    track: selfdrag::Track,
    /// The last surface-local position wl_pointer gave (mutter sends no
    /// motion when it is unchanged).
    pointer_s: Option<(f64, f64)>,
    /// A window handed to the desktop's interactive move: its X position is
    /// not mirrored into the pointer mapping while a button is held.
    moving: Option<Window>,
    /// Latest X position suppressed during the hand-over, restored on release.
    moving_pos: Option<(i16, i16)>,
    /// The current pointer frame's axis source, and whether it had discrete steps.
    axis_source: Option<wl_pointer::AxisSource>,
    axis_discrete_seen: bool,
    /// Smooth scroll not yet sent as wheel clicks (horizontal, vertical).
    axis_rest: (f64, f64),
    /// wl_pointer is entered on a shown surface (as against a pointer_on a
    /// touch point or a tablet tool is driving).
    pointer_entered: bool,
    /// The touch screen's device, the touch point that drives the reverse
    /// pointer (pointer emulation, as Xwayland does for core clients:
    /// (id, window)), and how many points are down.
    touch: Option<wl_touch::WlTouch>,
    touch_emulating: Option<(i32, Window)>,
    touch_down: u32,
    /// The tablet manager and seat (a tool's events come only to a client
    /// that has a tablet seat: mutter leaves wl_pointer while a tool is in
    /// proximity).
    tablet_manager: Option<zwp_tablet_manager_v2::ZwpTabletManagerV2>,
    tablet_seat: Option<zwp_tablet_seat_v2::ZwpTabletSeatV2>,
    /// The tablet tool in proximity of a shown window (one at a time drives
    /// the pointer): its id and window, the buttons it holds (BTN_* codes),
    /// and its frame's motion and presses so far.
    tool_on: Option<(wayland_client::backend::ObjectId, Window)>,
    tool_buttons: Vec<u32>,
    tool_motion: Option<(f64, f64)>,
    tool_presses: Vec<(u32, bool)>,
    /// Buttons the XTest device has none for, already logged once.
    dropped_button_logged: bool,
    /// The window sync with the desktop (per-window mode only).
    sync: Option<Sync>,
    /// This thread announced itself as the engine's window manager (EWMH).
    wm_announced: bool,
    /// The compositor time of the latest key or button press on a shown
    /// window (an activation's timestamp).
    input_time: u32,
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
            .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::STRUCTURE_NOTIFY),
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
        net_wm_state: atom(b"_NET_WM_STATE")?,
        net_wm_state_modal: atom(b"_NET_WM_STATE_MODAL")?,
        net_wm_state_max_vert: atom(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
        net_wm_state_max_horz: atom(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
        net_wm_state_fullscreen: atom(b"_NET_WM_STATE_FULLSCREEN")?,
        net_wm_state_hidden: atom(b"_NET_WM_STATE_HIDDEN")?,
        motif_wm_hints: atom(b"_MOTIF_WM_HINTS")?,
        wm_change_state: atom(b"WM_CHANGE_STATE")?,
        net_wm_moveresize: atom(b"_NET_WM_MOVERESIZE")?,
        net_supported: atom(b"_NET_SUPPORTED")?,
        net_supporting_wm_check: atom(b"_NET_SUPPORTING_WM_CHECK")?,
        net_active_window: atom(b"_NET_ACTIVE_WINDOW")?,
        net_wm_state_above: atom(b"_NET_WM_STATE_ABOVE")?,
        net_wm_state_stays_on_top: atom(b"_NET_WM_STATE_STAYS_ON_TOP")?,
        net_wm_state_demands_attention: atom(b"_NET_WM_STATE_DEMANDS_ATTENTION")?,
    };

    let wl = WlConnection::connect_to_env()
        .map_err(|e| format!("no Wayland display to show windows on: {e}"))?;
    let mut queue: EventQueue<State> = wl.new_event_queue();
    let qh = queue.handle();
    let ping_qh = spawn_pinger(&wl);
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
        ping_qh,
        seat: None,
        pointer: None,
        keyboard: None,
        constraints: None,
        relative_manager: None,
        viewporter: None,
        dialogs: None,
        tags: None,
        display,
        windows: HashMap::new(),
        popups: Vec::new(),
        adopted: Vec::new(),
        dismissed: Vec::new(),
        canvas: None,
        pointer_on: None,
        keyboard_on: None,
        x_focus: None,
        keyboard_left: false,
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
        popup_done: Vec::new(),
        lock_events: Vec::new(),
        relative: Vec::new(),
        states: Vec::new(),
        press_serial: 0,
        held_on: None,
        drag_rel: None,
        drag: None,
        drag_parts: selfdrag::Parts::default(),
        drags: 0,
        track: selfdrag::Track::default(),
        pointer_s: None,
        moving: None,
        moving_pos: None,
        axis_source: None,
        axis_discrete_seen: false,
        axis_rest: (0.0, 0.0),
        pointer_entered: false,
        touch: None,
        touch_emulating: None,
        touch_down: 0,
        tablet_manager: None,
        tablet_seat: None,
        tool_on: None,
        tool_buttons: Vec::new(),
        tool_motion: None,
        tool_presses: Vec::new(),
        dropped_button_logged: false,
        sync: None,
        wm_announced: false,
        input_time: 0,
        qh: qh.clone(),
    };
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    if state.compositor.is_none() || state.shm.is_none() || state.wm_base.is_none() {
        return Err("the compositor lacks wl_compositor, wl_shm or xdg_wm_base".into());
    }
    crate::log!(
        "-showwindows: showing the engine display's {} on the desktop (reverse master {:?}; pointer lock {}, viewporter {}, toplevel tags {})",
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
        if state.tags.is_some() { "yes" } else { "no" },
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
    } else {
        if let Err(e) = state.announce_wm() {
            crate::warning!(
                "-showwindows: can't announce the window-manager requests it carries out: {e}"
            );
        }
        state.sync = start_sync(display, state.tags.is_some());
    }
    // what is already up
    let tree = state.x.query_tree(root)?.reply()?;
    for w in tree.children {
        state.consider(w, &qh);
    }

    let xfd = state.x.stream().as_raw_fd();
    let sync_fd = state.sync.as_ref().map(|s| s.client.fd());
    loop {
        // (with their full sequence numbers: the window sync tells our own
        // configures' notifications from the clients' by them)
        while let Some((ev, seq)) = state.x.poll_for_event_with_sequence()? {
            state.x_event(ev, seq, &qh);
        }
        // the desktop's reports first: a pointer motion read in the same
        // wake-up is relative to where the desktop has the window now
        if state.sync.is_some() {
            state.sync_poll();
        }
        queue.dispatch_pending(&mut state)?;
        state.after_dispatch(&qh);
        // anything the dispatch did may have queued X events (replies we
        // waited for read them in)
        while let Some((ev, seq)) = state.x.poll_for_event_with_sequence()? {
            state.x_event(ev, seq, &qh);
        }
        if state.sync.is_some() {
            state.sync_poll();
            while let Some((ev, seq)) = state.x.poll_for_event_with_sequence()? {
                state.x_event(ev, seq, &qh);
            }
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
            libc::pollfd {
                fd: sync_fd.unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds = if sync_fd.is_some() { 3 } else { 2 };
        let timeout = state.sync_timeout_ms().unwrap_or(1000);
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, timeout) };
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

    /// WM_HINTS as 32-bit values (empty: none).
    fn wm_hints(&self, w: Window) -> Vec<u32> {
        self.x
            .get_property(false, w, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, 0, 9)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }

    /// WM_NORMAL_HINTS as 32-bit values (empty: none).
    fn normal_hints(&self, w: Window) -> Vec<u32> {
        self.x
            .get_property(
                false,
                w,
                AtomEnum::WM_NORMAL_HINTS,
                AtomEnum::WM_SIZE_HINTS,
                0,
                18,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }

    /// WM_HINTS: (takes input, starts iconic).
    fn hints(&self, w: Window) -> (bool, bool) {
        let v = self.wm_hints(w);
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
    /// property changes. `(x, y, width, height, depth, damage, border)`.
    #[allow(clippy::type_complexity)]
    fn watch(&mut self, w: Window) -> Result<(i16, i16, u16, u16, u8, u32, u16), AnyError> {
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
        Ok((g.x, g.y, width, height, g.depth, dmg, g.border_width))
    }

    /// Query rather than use a ConfigureNotify that may still be queued (for
    /// example, the parent's position just after maximising).
    fn shown_origin(&self, w: Window) -> Result<(i32, i32), AnyError> {
        let g = self.x.get_geometry(w)?.reply()?;
        let p = self.x.translate_coordinates(w, self.root, 0, 0)?.reply()?;
        Ok(shown_origin((p.dst_x, p.dst_y), g.border_width))
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

    /// Tags a new toplevel for the desktop's window sync (before its first
    /// commit, as the protocol asks), if the compositor offers tags.
    fn tag(&self, toplevel: &xdg_toplevel::XdgToplevel, w: Window) {
        if let Some(tags) = &self.tags {
            tags.set_toplevel_tag(toplevel, super::winsync::tag(self.display, w));
        }
    }

    fn show(&mut self, w: Window, qh: &QueueHandle<State>) -> Result<(), AnyError> {
        let (mut x, mut y, width, height, depth, dmg, border) = self.watch(w)?;
        // where the client asked for it, before it is fitted on the screen
        let asked = (i32::from(x), i32::from(y));
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
        self.tag(&toplevel, w);
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
        // WM_NORMAL_HINTS' min/max size, as a window manager honours them
        self.apply_size_hints(w, &toplevel);
        // a modal dialog (_NET_WM_STATE_MODAL, or Motif's modal input mode)
        // with a parent: GNOME attaches it to the parent, as it does on X
        let dialog = match (&parent, self.dialogs.as_ref()) {
            (Some(_), Some(d)) if self.is_modal(w) => {
                let x = d.get_xdg_dialog(&toplevel, qh, ());
                x.set_modal();
                crate::log!("-showwindows: {w:#x} is a modal dialog of its parent");
                Some(x)
            }
            _ => None,
        };
        // with the window sync, a position the client asked for (USPosition,
        // PPosition) is where the desktop opens it: asked before the first
        // commit, applied at the window's first show (no jump)
        if self.synced()
            && let Some((ix, iy)) =
                winsync::initial_position(&self.normal_hints(w), asked.0, asked.1)
            && let Some(sync) = self.sync.as_ref()
        {
            sync.client.send(Request::SetInitialPosition {
                xid: w,
                x: ix,
                y: iy,
            });
            crate::log!(
                "-showwindows: {w:#x} asks to open at {ix},{iy}: the desktop places it there"
            );
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
                at: (x, y),
                border,
                above: None,
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
                dialog,
                maximized: false,
                fullscreen: false,
            },
        );
        if let Some(sync) = self.sync.as_mut() {
            let r = Rect::new(
                i32::from(x),
                i32::from(y),
                i32::from(width),
                i32::from(height),
            );
            sync.geo.track(w, r);
        }
        self.sync_flags(w);
        Ok(())
    }

    /// An override-redirect window (menu, tooltip, combo list) as an
    /// xdg_popup on a shown window of the same client: the last popup it has
    /// up (a submenu), else its window under the pointer or with the
    /// keyboard, else its transient parent, else any of its windows.
    fn show_popup(&mut self, w: Window, qh: &QueueHandle<State>) -> Result<(), AnyError> {
        // toolkits' invisible helpers (GTK's drag-and-drop IPC window: 1x1 at
        // -99,-99) are not popups: the compositor dismisses such a popup, and
        // the pointer leaving the parent then ends the drag
        let g = self.x.get_geometry(w)?.reply()?;
        let (gw, gh) = (
            i32::from(g.width + 2 * g.border_width),
            i32::from(g.height + 2 * g.border_width),
        );
        if helper_window(i32::from(g.x), i32::from(g.y), gw, gh, self.screen) {
            crate::vlog!(
                "-showwindows: {w:#x} is an override-redirect window off the screen or {gw}x{gh}: not shown"
            );
            return Ok(());
        }
        // a drag-and-drop icon sits under the pointer: as a popup it would take
        // the pointer from the window the drag started in (the pointer leaves
        // it, its buttons are released, and the drag ends at once); the
        // desktop's cursor shows the drag (the X cursor is shown) instead
        let kind = self.window_type(w);
        if kind == "dnd" {
            crate::vlog!("-showwindows: {w:#x} is a drag-and-drop icon: not shown");
            return Ok(());
        }
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
        let (x, y, width, height, depth, dmg, border) = self.watch(w)?;
        let origin = self.shown_origin(parent)?;
        let p = self.windows.get(&parent).expect("found above");
        let (dx, dy) = (i32::from(x) - origin.0, i32::from(y) - origin.1);
        let ((ax, ay), (ox, oy)) = popup_anchor(dx, dy, p.width, p.height);
        let wm_base = self.wm_base.as_ref().expect("checked at start");
        let positioner = wm_base.create_positioner(qh, ());
        positioner.set_size(i32::from(width), i32::from(height));
        positioner.set_anchor_rect(ax, ay, 1, 1);
        positioner.set_offset(ox, oy);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        // No constraint adjustment: the X client has already placed the popup
        // to fit the X screen, which is the size of the desktop, so the
        // position that keeps the popup at the parent's shown offset — the
        // invariant the reverse pointer mapping and `place_popup` assume —
        // fits the desktop too. Letting the compositor slide it (a popup
        // taller than the work area: a 720-line menu in a 688-px work area)
        // breaks that invariant: the compositor shows the popup 25 px lower
        // and reports the slid offset, the X window follows there, but the
        // client re-asserts its own position and the mapping is then off by
        // the adjustment — a maximised LibreOffice's Format > Paragraph...
        // click landed one menu row off (Character opened). 0.1.0 showed
        // these popups unadjusted and is correct; a popup that would overflow
        // the desktop is clipped instead of misplaced.
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::empty());
        let compositor = self.compositor.as_ref().expect("checked at start");
        let surface = compositor.create_surface(qh, w);
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, w);
        let popup = xdg_surface.get_popup(Some(&p.xdg_surface), &positioner, qh, w);
        positioner.destroy();
        if kind == "tooltip" || kind == "notification" {
            let region = compositor.create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        surface.commit();
        crate::log!(
            "-showwindows: showing {w:#x} (override-redirect, {}) as a popup of {parent:#x} at {dx},{dy} {}x{}",
            kind,
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
                at: (x, y),
                border,
                above: None,
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
                dialog: None,
                maximized: false,
                fullscreen: false,
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
        self.sync_forget(w);
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
        if let Some(d) = s.dialog {
            d.destroy();
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

    fn x_event(&mut self, ev: Event, seq: u64, qh: &QueueHandle<State>) {
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
            // the X screen itself was resized (xrandr, a VNC client's size):
            // the pointer reaches the new size, and the sync checks the
            // desktop's layout against it
            Event::ConfigureNotify(e) if e.window == self.root => {
                if !self.screen_mode() && (e.width, e.height) != self.screen {
                    crate::vlog!(
                        "-showwindows: the X screen is {}x{} now (was {}x{})",
                        e.width,
                        e.height,
                        self.screen.0,
                        self.screen.1
                    );
                    self.screen = (e.width, e.height);
                    self.sync_layout();
                }
            }
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
                // a client moving its own top-level while a button is held on
                // it (a drag handle, a toolkit's emulated move without a
                // window manager): the client carries its own drag; keep the
                // pointer mapping stable while it does (`begin_move`), but do
                // not hand the drag to the desktop's interactive move — its
                // grab takes the pointer off the window's surface mid-drag
                if !self.screen_mode()
                    && self.moving.is_none()
                    && !self.buttons_down.is_empty()
                    && self.pointer_on == Some(e.window)
                    && self.windows.get(&e.window).is_some_and(|s| {
                        matches!(s.role, Role::Toplevel(_))
                            && (e.x, e.y) != (s.x, s.y)
                            && (w, h) == (s.width, s.height)
                    })
                {
                    self.begin_move(e.window);
                }
                if !self.windows.contains_key(&e.window)
                    && e.override_redirect
                    && !self.screen_mode()
                {
                    // a popup mapped off the screen and moved onto it
                    self.consider(e.window, qh);
                    return;
                }
                // A queued notification can predate a compositor popup slide.
                // Use the server's current position, not that stale event.
                let pos = if self
                    .windows
                    .get(&e.window)
                    .is_some_and(|s| matches!(s.role, Role::Popup { .. }))
                {
                    self.shown_origin(e.window)
                        .map(|(x, y)| (x as i16, y as i16))
                        .unwrap_or((e.x, e.y))
                } else {
                    (e.x, e.y)
                };
                let synced = self.synced_toplevel(e.window);
                let desk = self.sync.as_ref().and_then(|sync| sync.geo.desk(e.window));
                let mut restacked = false;
                if let Some(s) = self.windows.get_mut(&e.window) {
                    // a restack (or nothing): the same geometry, or a new
                    // sibling below (an XRaiseWindow keeps the sibling when
                    // the window that was on top is the one below it now)
                    restacked = winsync::restack_event(
                        (s.at, s.width, s.height, s.above),
                        (pos, w, h, e.above_sibling),
                    );
                    s.at = pos;
                    s.border = e.border_width;
                    s.above = Some(e.above_sibling);
                    if synced {
                        // the pointer maps from where the desktop has it
                        (s.x, s.y) = winsync::pointer_origin(desk, pos);
                    } else if mirror_position(self.moving, e.window, &self.buttons_down) {
                        (s.x, s.y) = pos;
                    } else {
                        self.moving_pos = Some(pos);
                    }
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
                self.sync_x_configured(&e, seq, restacked);
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
            Event::PropertyNotify(e)
                if e.atom == u32::from(AtomEnum::WM_NORMAL_HINTS)
                    && self.windows.contains_key(&e.window) =>
            {
                if let Some(Shown {
                    role: Role::Toplevel(t),
                    ..
                }) = self.windows.get(&e.window)
                {
                    self.apply_size_hints(e.window, t);
                }
                if let Some(s) = self.windows.get_mut(&e.window) {
                    s.dirty = true;
                }
                self.paint(e.window);
            }
            // with the window sync: keep-above and attention hints
            Event::PropertyNotify(e)
                if (e.atom == self.atoms.net_wm_state
                    || e.atom == u32::from(AtomEnum::WM_HINTS))
                    && self.synced_toplevel(e.window) =>
            {
                self.sync_flags(e.window)
            }
            // a client asking the window manager (EWMH / ICCCM requests to the root)
            Event::ClientMessage(e) if e.format == 32 => self.client_message(e),
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
            // the window's frame: its coalesced MoveResize, if one waits
            self.sync_send(w);
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
                let synced = self.synced_toplevel(w);
                // with the sync the size is the window geometry's, border
                // included (0.1.2 sets it as the size inside the border)
                let (iw, ih) = if synced {
                    winsync::inner_size(width, height, s.border)
                } else {
                    (width as u32, height as u32)
                };
                let at = s.at;
                let cookie = self
                    .x
                    .configure_window(w, &ConfigureWindowAux::new().width(iw).height(ih));
                // the desktop's resize: its notification is not the client's
                // (unless a move of the client's waits to go out: then the
                // notification carries that move to the desktop, size and all)
                if synced
                    && let Ok(c) = cookie
                    && let Some(sync) = self.sync.as_mut()
                    && !sync.geo.pending(w)
                {
                    let r = Rect::new(i32::from(at.0), i32::from(at.1), width, height);
                    sync.geo.x_sent(w, r, c.sequence_number());
                }
            }
        }
        for (w, maximized, fullscreen) in std::mem::take(&mut self.states) {
            let changed = self.windows.get_mut(&w).is_some_and(|s| {
                let c = (s.maximized, s.fullscreen) != (maximized, fullscreen);
                (s.maximized, s.fullscreen) = (maximized, fullscreen);
                c
            });
            if changed {
                crate::log!(
                    "-showwindows: {w:#x} is {}{}",
                    if maximized {
                        "maximised"
                    } else {
                        "not maximised"
                    },
                    if fullscreen { ", fullscreen" } else { "" }
                );
                self.set_net_wm_state(w, maximized, fullscreen);
            }
        }
        for w in std::mem::take(&mut self.popup_done) {
            crate::log!("-showwindows: the desktop dismissed popup {w:#x}");
            self.hide(w, qh);
            self.dismissed.push(w);
        }
        // The keyboard went to a desktop window that isn't ours: take the X
        // focus away too, as Xwayland's window manager does, so X clients see
        // FocusOut (games pause; SDL stops passing joystick input to an
        // unfocused game unless it asked for background events). A leave
        // followed by an enter on another of our windows in the same batch
        // (another X window, or an on-screen keyboard that never takes the
        // focus) keeps the focus where `focus_x` put or left it.
        if x_focus_clears(std::mem::take(&mut self.keyboard_left), self.keyboard_on) {
            self.unfocus_x();
            self.set_active(None);
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

    /// Apply a popup configure before any later pointer event in this batch.
    fn place_popup(&mut self, w: Window, px: i32, py: i32) {
        if let Some(s) = self.windows.get(&w)
            && let Role::Popup { parent, .. } = s.role
            && let Ok(origin) = self.shown_origin(parent)
        {
            let (nx, ny) = popup_position(origin, (px, py));
            if (nx, ny) != (i32::from(s.x), i32::from(s.y)) {
                crate::vlog!(
                    "-showwindows: popup {w:#x} placed at {nx},{ny}, not {},{}",
                    s.x,
                    s.y
                );
                let _ = self
                    .x
                    .configure_window(w, &ConfigureWindowAux::new().x(nx).y(ny));
                if let Some(s) = self.windows.get_mut(&w) {
                    (s.x, s.y) = (nx as i16, ny as i16);
                }
            }
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

    /// Root coordinates of surface-local `(sx, sy)` on `w`'s surface, before
    /// they are whole pixels on the X screen.
    fn to_desk(&self, w: Window, sx: f64, sy: f64) -> Option<selfdrag::Pos> {
        if w == self.root {
            let scale = self.canvas.as_ref().map_or(1.0, |c| c.scale);
            return Some((sx / scale, sy / scale));
        }
        let s = self.windows.get(&w)?;
        Some((f64::from(s.x) + sx, f64::from(s.y) + sy))
    }

    /// Moves the reverse pointer to surface-local `(sx, sy)` of `w` (an event
    /// of compositor time `time`, ms).
    fn motion(&mut self, w: Window, sx: f64, sy: f64, time: Option<u32>) {
        if self.drag.as_ref().is_some_and(|d| d.window == w) {
            // the held self-drag's (a tablet tool's or a touch point's:
            // wl_pointer's frames go through `drag_parts`)
            self.drag_frame(selfdrag::Frame {
                surface: Some((sx, sy)),
                rel: None,
                time,
            });
            return;
        }
        let Some(pos) = self.to_desk(w, sx, sy) else {
            return;
        };
        let (x, y) = selfdrag::root_position(pos, self.screen);
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
            self.lock_pointer(w, false);
        } else if self.lock.is_none() {
            // no warp was seen: a client's confining pointer grab (a core
            // GrabPointer with confine-to, seen on RECORD; GTK 4's XI2 grabs
            // don't confine) plus a hidden cursor is the other relative-mode
            // trigger, checked here and on cursor change
            let blank = self.cursor.as_ref().is_some_and(|c| c.blank);
            if relative_mode_due(true, blank, blank && super::record::confining_grab()) {
                crate::log!(
                    "-showwindows: the client hid the cursor and its pointer grab confines the pointer (relative mode): locking the desktop's pointer for relative motion"
                );
                self.lock_pointer(w, true);
                if self.lock.is_some() {
                    return; // the X pointer stays put; relative motion follows once locked
                }
            }
        }
        self.track.update(pos, time);
        self.last_pos = Some((x, y));
        let _ = xtest::fake_input(&self.x, MOTION_NOTIFY, 0, 0, self.root, x, y, 0);
    }

    /// A pointer frame during a held self-drag: the X pointer goes where the
    /// desktop's pointer is (`selfdrag::SelfDrag`).
    fn drag_frame(&mut self, f: selfdrag::Frame) {
        let now_ms = monotonic_ms();
        let Some(d) = self.drag.as_mut() else {
            return;
        };
        let Some(pos) = d.pointer.frame(f, Instant::now(), now_ms) else {
            return;
        };
        self.track.update(pos, f.time);
        let (x, y) = selfdrag::root_position(pos, self.screen);
        if self.last_pos == Some((x, y)) {
            return;
        }
        self.last_pos = Some((x, y));
        let _ = xtest::fake_input(&self.x, MOTION_NOTIFY, 0, 0, self.root, x, y, 0);
    }

    /// Smooth scrolling (touchpad fingers, continuous devices) as wheel
    /// clicks, one per `SCROLL_STEP` pixels, as Xwayland emulates them for
    /// core clients.
    fn smooth_scroll(&mut self, axis: wl_pointer::Axis, value: f64) {
        let rest = if axis == wl_pointer::Axis::VerticalScroll {
            &mut self.axis_rest.1
        } else {
            &mut self.axis_rest.0
        };
        let steps = scroll_steps(rest, value);
        if steps != 0 {
            self.wheel(axis, steps);
        }
    }

    /// A tablet tool frame (of compositor time `time`, ms): the motion goes
    /// first, then the tip and buttons.
    fn tool_frame(&mut self, time: u32) {
        let Some((_, w)) = self.tool_on.clone() else {
            self.tool_motion = None;
            self.tool_presses.clear();
            return;
        };
        if let Some((x, y)) = self.tool_motion.take() {
            self.motion(w, x, y, Some(time));
        }
        let presses = std::mem::take(&mut self.tool_presses);
        for (code, pressed) in tool_frame_presses(&mut self.tool_buttons, &presses) {
            self.button(code, pressed);
        }
    }

    /// The tool left proximity: let go of what it holds; the pointer stays
    /// the mouse's if the mouse's own pointer had entered a shown window.
    fn tool_gone(&mut self) {
        if self.tool_on.take().is_none() {
            return;
        }
        self.tool_motion = None;
        self.tool_presses.clear();
        let (releases, drop_pointer) = tool_out_releases(&self.tool_buttons, self.pointer_entered);
        self.tool_buttons.clear();
        for code in releases {
            self.button(code, false);
        }
        if drop_pointer {
            self.pointer_on = None;
        }
    }

    /// A client that moves its own X window while a button is held on it (a
    /// drag handle, a toolkit's emulated move without a window manager)
    /// carries its own drag: keep the pointer mapping stable while it does
    /// (its X position is not mirrored into it until the button comes up, so
    /// its own move loop follows the drag once, not twice), and do not hand
    /// the drag to the desktop's interactive move. That hand-over was tried
    /// (0.1.1..59996af) and loses the drag: the desktop's move grab takes the
    /// pointer off the window's surface mid-drag, the held buttons are
    /// released under the pointer's last position, and a client running its
    /// own move loop (Qt docks: OBS) sees a drag that ends after one step at
    /// the drag's start point — over its own dock area — and re-docks: the
    /// dock's window went away 5 frames into the drag, the rest of the drag
    /// ran on the main window and dragged it off-screen.
    ///
    /// With the window sync the desktop window follows the X window (one
    /// MoveResize per frame), still with no grab: the surface moves under the
    /// pointer, and the X pointer goes where the desktop's pointer is, worked
    /// out from the window's positions on the desktop meanwhile
    /// (`selfdrag::SelfDrag`, `drag_frame`).
    fn begin_move(&mut self, w: Window) {
        if self.moving.is_some() {
            return;
        }
        self.moving = Some(w);
        self.moving_pos = None;
        if self.synced_toplevel(w)
            && self.track.pos().is_some()
            && let Some(s) = self.windows.get(&w)
        {
            // so far the pointer mapped from where the desktop has it
            let origin = (i32::from(s.x), i32::from(s.y));
            self.drags += 1;
            // wl_pointer's press, or a touch point's or tablet tool's (they
            // drive the pointer without it)
            let wl_pointer =
                self.pointer_entered && self.touch_emulating.is_none() && self.tool_on.is_none();
            let surface = self.pointer_s.filter(|_| wl_pointer);
            self.drag = Some(Drag {
                window: w,
                id: self.drags,
                wl_pointer,
                pointer: selfdrag::SelfDrag::new(origin, self.track, surface),
            });
            self.drag_parts = selfdrag::Parts::default();
        }
        if self.synced_toplevel(w) {
            crate::log!(
                "-showwindows: {w:#x} moves itself with the pointer: its desktop window follows"
            );
        } else {
            crate::log!(
                "-showwindows: {w:#x} moves itself with the pointer: its own move carries it"
            );
        }
    }

    fn end_move(&mut self) {
        self.drag = None;
        self.drag_parts = selfdrag::Parts::default();
        if let Some(w) = self.moving.take()
            && let Some((x, y)) = self.moving_pos.take()
            && let Some(s) = self.windows.get_mut(&w)
        {
            (s.x, s.y) = (x, y);
        }
    }

    /// Tells clients (EWMH) which window-manager requests the desktop carries
    /// out for shown windows, so toolkits ask (`_NET_WM_MOVERESIZE` for their
    /// drag handles, size grips and title bars, `_NET_WM_STATE` for maximise
    /// and fullscreen) instead of emulating them with root coordinates that
    /// mean nothing on the desktop (GTK 3's emulated move snaps a window at
    /// the top of the X screen to maximised). Not done when a window manager
    /// already runs on the engine.
    fn announce_wm(&mut self) -> Result<(), AnyError> {
        let a = &self.atoms;
        let existing = self
            .x
            .get_property(
                false,
                self.root,
                a.net_supporting_wm_check,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?
            .value32()
            .and_then(|mut v| v.next());
        if existing.is_some_and(|w| w != 0) {
            crate::log!(
                "-showwindows: a window manager runs on the engine display: not announcing one"
            );
            return Ok(());
        }
        let check = self.x.generate_id()?;
        self.x.create_window(
            0,
            check,
            self.root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &x11rb::protocol::xproto::CreateWindowAux::new(),
        )?;
        self.x.change_property32(
            PropMode::REPLACE,
            check,
            a.net_supporting_wm_check,
            AtomEnum::WINDOW,
            &[check],
        )?;
        self.x.change_property8(
            PropMode::REPLACE,
            check,
            a.net_wm_name,
            a.utf8_string,
            b"xwlbridge",
        )?;
        let supported = self.supported(false);
        self.x.change_property32(
            PropMode::REPLACE,
            self.root,
            a.net_supported,
            AtomEnum::ATOM,
            &supported,
        )?;
        self.x.change_property32(
            PropMode::REPLACE,
            self.root,
            a.net_supporting_wm_check,
            AtomEnum::WINDOW,
            &[check],
        )?;
        self.wm_announced = true;
        crate::log!(
            "-showwindows: announced _NET_WM_MOVERESIZE and _NET_WM_STATE (maximise, fullscreen) to clients"
        );
        Ok(())
    }

    /// `_NET_SUPPORTED`: what 0.1.2 carries out, plus (`sync`: while the
    /// window sync is available) activation, keep-above and attention.
    fn supported(&self, sync: bool) -> Vec<u32> {
        let a = &self.atoms;
        let mut v = vec![
            a.net_supported,
            a.net_supporting_wm_check,
            a.net_wm_name,
            a.net_wm_moveresize,
            a.net_wm_state,
            a.net_wm_state_max_vert,
            a.net_wm_state_max_horz,
            a.net_wm_state_fullscreen,
            a.net_wm_state_modal,
        ];
        if sync {
            v.extend([
                a.net_active_window,
                a.net_wm_state_above,
                a.net_wm_state_demands_attention,
            ]);
        }
        v
    }

    fn net_wm_state(&self, w: Window) -> Vec<u32> {
        self.x
            .get_property(false, w, self.atoms.net_wm_state, AtomEnum::ATOM, 0, 64)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>()))
            .unwrap_or_default()
    }

    /// Whether `w` asks to be modal: `_NET_WM_STATE_MODAL`, or Motif's
    /// `_MOTIF_WM_HINTS` with a modal input mode.
    fn is_modal(&self, w: Window) -> bool {
        if self
            .net_wm_state(w)
            .contains(&self.atoms.net_wm_state_modal)
        {
            return true;
        }
        let v: Vec<u32> = self
            .x
            .get_property(false, w, self.atoms.motif_wm_hints, AtomEnum::ANY, 0, 5)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        v.len() >= 4 && v[0] & 4 != 0 && v[3] != 0
    }

    /// WM_NORMAL_HINTS' minimum and maximum size on the shown toplevel.
    fn apply_size_hints(&self, w: Window, t: &xdg_toplevel::XdgToplevel) {
        let v = self.normal_hints(w);
        let ((minw, minh), (maxw, maxh)) = size_limits(&v);
        t.set_min_size(minw, minh);
        t.set_max_size(maxw, maxh);
        if (minw, minh, maxw, maxh) != (0, 0, 0, 0) {
            crate::vlog!("-showwindows: {w:#x} size limits {minw}x{minh} .. {maxw}x{maxh}");
        }
    }

    /// The compositor's maximised / fullscreen state in `_NET_WM_STATE`, so
    /// the client knows (the rest of the property is the client's own).
    fn set_net_wm_state(&self, w: Window, maximized: bool, fullscreen: bool) {
        let a = &self.atoms;
        let mut v = self.net_wm_state(w);
        v.retain(|x| {
            ![
                a.net_wm_state_max_vert,
                a.net_wm_state_max_horz,
                a.net_wm_state_fullscreen,
            ]
            .contains(x)
        });
        if maximized {
            v.extend([a.net_wm_state_max_vert, a.net_wm_state_max_horz]);
        }
        if fullscreen {
            v.push(a.net_wm_state_fullscreen);
        }
        let _ = self
            .x
            .change_property32(PropMode::REPLACE, w, a.net_wm_state, AtomEnum::ATOM, &v);
    }

    /// A client's request to the window manager: `_NET_WM_STATE` (maximise,
    /// fullscreen, hidden) and ICCCM's `WM_CHANGE_STATE` (iconify), for a
    /// shown toplevel.
    fn client_message(&mut self, e: ClientMessageEvent) {
        if self.synced_toplevel(e.window) && self.sync_client_message(&e) {
            return;
        }
        let d = e.data.as_data32();
        let a = &self.atoms;
        let Some(s) = self.windows.get(&e.window) else {
            return;
        };
        let Role::Toplevel(t) = &s.role else {
            return;
        };
        if e.type_ == a.net_wm_state {
            crate::vlog!(
                "-showwindows: {:#x} _NET_WM_STATE request {:?}",
                e.window,
                d
            );
            let want = |cur: bool| match d[0] {
                0 => false,
                1 => true,
                _ => !cur,
            };
            let props = [d[1], d[2]];
            if props.contains(&a.net_wm_state_fullscreen) {
                if want(s.fullscreen) {
                    t.set_fullscreen(None);
                } else {
                    t.unset_fullscreen();
                }
                crate::log!(
                    "-showwindows: {:#x} asks for fullscreen {}",
                    e.window,
                    want(s.fullscreen)
                );
            }
            if props.contains(&a.net_wm_state_max_vert) || props.contains(&a.net_wm_state_max_horz)
            {
                if want(s.maximized) {
                    t.set_maximized();
                } else {
                    t.unset_maximized();
                }
                crate::log!(
                    "-showwindows: {:#x} asks to be maximised {}",
                    e.window,
                    want(s.maximized)
                );
            }
            if props.contains(&a.net_wm_state_hidden) && want(false) {
                t.set_minimized();
            }
        } else if e.type_ == a.wm_change_state && d[0] == 3 {
            t.set_minimized();
            crate::log!("-showwindows: {:#x} iconifies itself: minimised", e.window);
        } else if e.type_ == a.net_wm_moveresize {
            // [x_root, y_root, direction, button, source]: an interactive move
            // or resize for the press that is down now, as the desktop does it
            let Some(seat) = self.seat.as_ref() else {
                return;
            };
            if self.buttons_down.is_empty() {
                return;
            }
            let edge = match d[2] {
                0 => xdg_toplevel::ResizeEdge::TopLeft,
                1 => xdg_toplevel::ResizeEdge::Top,
                2 => xdg_toplevel::ResizeEdge::TopRight,
                3 => xdg_toplevel::ResizeEdge::Right,
                4 => xdg_toplevel::ResizeEdge::BottomRight,
                5 => xdg_toplevel::ResizeEdge::Bottom,
                6 => xdg_toplevel::ResizeEdge::BottomLeft,
                7 => xdg_toplevel::ResizeEdge::Left,
                8 => {
                    t._move(seat, self.press_serial);
                    crate::log!(
                        "-showwindows: {:#x} asks for a move: the desktop moves its window",
                        e.window
                    );
                    self.moving = Some(e.window);
                    self.moving_pos = None;
                    return;
                }
                _ => return,
            };
            t.resize(seat, self.press_serial, edge);
            crate::log!(
                "-showwindows: {:#x} asks for a resize ({edge:?}): the desktop resizes its window",
                e.window
            );
        }
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

    fn lock_pointer(&mut self, w: Window, grab: bool) {
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
            grab,
            rest: (0.0, 0.0),
        });
    }

    /// Whether the held pointer lock ends now: the cursor is visible again,
    /// or the lock was taken for a client's confining grab and the grab is
    /// gone (see `relative_mode_ends`).
    fn lock_ended(&self, blank: bool) -> bool {
        relative_mode_ends(
            self.lock.is_some(),
            blank,
            self.lock.as_ref().is_some_and(|l| l.grab),
            super::record::confining_grab(),
        )
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
        // the lock ends when the cursor shows again, or (for one taken for
        // a client's confining grab) when the grab does; a warp-triggered
        // lock keeps until the cursor shows
        let blank = self.cursor.as_ref().is_some_and(|c| c.blank);
        if self.lock_ended(blank) {
            self.unlock();
        }
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
        let Some(b) = injectable_button(button) else {
            if !self.dropped_button_logged {
                self.dropped_button_logged = true;
                crate::log!(
                    "-showwindows: button code {button:#x} has no X button the reverse pointer can press (XTest has 10)"
                );
            }
            return;
        };
        let ty = if pressed {
            BUTTON_PRESS
        } else {
            BUTTON_RELEASE
        };
        let was_held = !self.buttons_down.is_empty();
        if pressed {
            self.buttons_down.push(b);
            if self.screen_mode() {
                self.click_to_focus();
            }
            self.input_on(self.pointer_on);
        } else {
            self.buttons_down.retain(|&x| x != b);
            if self.buttons_down.is_empty() {
                self.end_move();
            }
        }
        if was_held == self.buttons_down.is_empty() {
            // the window the press went on, also when the release comes
            // over another one
            let w = if was_held {
                self.drop_drag_pointer();
                self.held_on.take()
            } else {
                self.held_on = self.pointer_on;
                if let Some(w) = self.pointer_on
                    && self.synced_toplevel(w)
                    && self.pointer_entered
                    && let (Some(m), Some(p)) =
                        (self.relative_manager.as_ref(), self.pointer.as_ref())
                {
                    self.drag_rel = Some(m.get_relative_pointer(p, &self.qh, DragRel));
                }
                self.pointer_on
            };
            self.sync_held(w, !was_held);
        }
        crate::vlog!(
            "-showwindows: button {b} {} on {:?}",
            if pressed { "down" } else { "up" },
            self.pointer_on
        );
        let _ = xtest::fake_input(&self.x, ty, b, 0, self.root, 0, 0, 0);
    }

    fn drop_drag_pointer(&mut self) {
        self.drag = None;
        self.drag_parts = selfdrag::Parts::default();
        if let Some(r) = self.drag_rel.take() {
            r.destroy();
        }
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

    /// The keyboard went to another window (the person clicked it): close the
    /// client's open menus, as on an X desktop, where the menu's grab sees
    /// that click. The popups have no grab on the desktop (an X client's
    /// grabs are its own), so nothing else would close them: one Escape per
    /// menu-like popup still up (a menu's keyboard grab takes it).
    fn dismiss_menus(&mut self) {
        let menus = self
            .popups
            .iter()
            .filter(|p| {
                matches!(
                    self.window_type(**p).as_str(),
                    "popup_menu" | "dropdown_menu" | "menu" | "combo"
                )
            })
            .count();
        if menus == 0 {
            return;
        }
        crate::log!(
            "-showwindows: the keyboard left for another window: closing {menus} open menu(s) with Escape"
        );
        for _ in 0..menus.min(4) {
            // KEY_ESC
            self.key(1, true);
            self.key(1, false);
        }
    }

    fn release_keys(&mut self) {
        for kc in std::mem::take(&mut self.keys_down) {
            let _ = xtest::fake_input(&self.x, KEY_RELEASE, kc, 0, self.root, 0, 0, 0);
        }
    }

    fn release_buttons(&mut self) {
        let was_held = !self.buttons_down.is_empty();
        for b in std::mem::take(&mut self.buttons_down) {
            let _ = xtest::fake_input(&self.x, BUTTON_RELEASE, b, 0, self.root, 0, 0, 0);
        }
        self.end_move();
        self.drop_drag_pointer();
        if was_held {
            let w = self.held_on.take();
            self.sync_held(w, false);
        }
    }

    /// Gives `w` the reverse keyboard the way an ICCCM window manager gives
    /// the keyboard (§4.1.7): focus input-taking windows, including globally
    /// active clients, and send WM_TAKE_FOCUS only when advertised. No Input
    /// windows (on-screen keyboards) leave the previous focus alone.
    fn focus_x(&mut self, w: Window) {
        if self.x_focus == Some(w) {
            return;
        }
        let (input, _) = self.hints(w);
        let (set, take) = focus_actions(input, &self.protocols(w), self.atoms.wm_take_focus);
        if set {
            self.x_focus = Some(w);
            let _ = self
                .x
                .xinput_xi_set_focus(w, x11rb::CURRENT_TIME, self.devs.keyboard);
        }
        if take {
            let ev = ClientMessageEvent::new(
                32,
                w,
                self.atoms.wm_protocols,
                [self.atoms.wm_take_focus, x11rb::CURRENT_TIME, 0, 0, 0],
            );
            let _ = self.x.send_event(false, w, EventMask::NO_EVENT, ev);
        }
    }

    /// No X window has the reverse keyboard (the desktop's keyboard is on a
    /// window that isn't ours): focus None, so the window that had it gets
    /// FocusOut and keys from engine-side XTest on this master go nowhere,
    /// as on Xwayland while a Wayland window is active.
    fn unfocus_x(&mut self) {
        let had = self.x_focus.take();
        crate::vlog!(
            "-showwindows: the desktop's keyboard left our windows; X focus None (was {had:#x?})"
        );
        let _ = self
            .x
            .xinput_xi_set_focus(x11rb::NONE, x11rb::CURRENT_TIME, self.devs.keyboard);
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
            // a warp while blank locks (see motion); so does a client's
            // confining pointer grab (relative mode), checked here and on
            // motion
            if let Some(w) = self.pointer_on
                && super::record::confining_grab()
            {
                crate::log!(
                    "-showwindows: the client hid the cursor and its pointer grab confines the pointer (relative mode): locking the desktop's pointer for relative motion"
                );
                self.lock_pointer(w, true);
            }
        } else if self.lock_ended(blank) {
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

    // ---- the window sync with the desktop (winsync) ----

    /// The window sync is on: the extension is there and compatible, and
    /// the X screen holds the desktop's layout (`Sync::on`).
    fn synced(&self) -> bool {
        self.sync.as_ref().is_some_and(|s| s.on)
    }

    /// The window sync is on and `w` is a shown toplevel.
    fn synced_toplevel(&self, w: Window) -> bool {
        self.synced()
            && self
                .windows
                .get(&w)
                .is_some_and(|s| matches!(s.role, Role::Toplevel(_)))
    }

    /// The user pressed a key or button on shown window `w`: its client's
    /// raise requests are honoured for a while (`winsync::RaiseGate`).
    fn input_on(&mut self, w: Option<Window>) {
        let client = w.and_then(|w| self.windows.get(&w)).map(|s| s.client);
        if let (Some(sync), Some(c)) = (self.sync.as_mut(), client) {
            sync.gate.input(c, Instant::now());
        }
    }

    /// The first button went down on, or the last came up from, shown
    /// window `w`: X is authoritative for its geometry while one is held.
    fn sync_held(&mut self, w: Option<Window>, held: bool) {
        let (Some(w), Some(sync)) = (w, self.sync.as_mut()) else {
            return;
        };
        if let Some(r) = sync.geo.button(w, held) {
            self.apply_x(w, r);
        }
    }

    /// A ConfigureNotify (of sequence number `seq`) of shown window `e.window`
    /// was handled: with the sync, a toplevel the client moved or resized
    /// itself goes to the desktop (coalesced, at most one MoveResize per
    /// frame), and a restack of its own goes through the raise gating.
    fn sync_x_configured(&mut self, e: &ConfigureNotifyEvent, seq: u64, restacked: bool) {
        let toplevel = self
            .windows
            .get(&e.window)
            .is_some_and(|s| matches!(s.role, Role::Toplevel(_)));
        let synced = self.synced();
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        if !toplevel {
            return;
        }
        let rect = winsync::outer_rect(e.x, e.y, e.width, e.height, e.border_width);
        if !synced {
            // kept current for the next resync; nothing else changes
            sync.geo.track(e.window, rect);
            return;
        }
        // our own restack (or one from before it, which it overrides)
        let ours = restacked
            && match sync.stack_echo.get(&e.window) {
                Some(&sent) => {
                    if seq >= sent {
                        sync.stack_echo.remove(&e.window);
                    }
                    true
                }
                None => false,
            };
        let back = sync.geo.x_configured(e.window, rect, seq, Instant::now());
        let pending = sync.geo.pending(e.window);
        if let Some(r) = back {
            self.apply_x(e.window, r);
        } else if pending {
            self.sync_kick(e.window);
        }
        if restacked && !ours {
            self.x_restacked(e.window);
        }
    }

    /// Configures X window `w` to `r` for the desktop (its move, a clamp, a
    /// resync), sends the synthetic ConfigureNotify an X window manager
    /// sends (ICCCM 4.1.5), and tells the sync it was ours.
    fn apply_x(&mut self, w: Window, r: Rect) {
        if !self.synced_toplevel(w) {
            return;
        }
        let Some(s) = self.windows.get(&w) else {
            return;
        };
        let border = s.border;
        let size = (i32::from(s.width), i32::from(s.height));
        let (iw, ih) = winsync::inner_size(r.w, r.h, border);
        let (iw, ih) = (iw.min(u32::from(MAX_SIDE)), ih.min(u32::from(MAX_SIDE)));
        let mut aux = ConfigureWindowAux::new().x(r.x).y(r.y);
        if (r.w, r.h) != size {
            aux = aux.width(iw).height(ih);
        }
        let seq = match self.x.configure_window(w, &aux) {
            Ok(c) => c.sequence_number(),
            Err(e) => {
                crate::vlog!("-showwindows: can't configure {w:#x}: {e}");
                return;
            }
        };
        let ev = synthetic_configure(w, r.x, r.y, iw, ih, border);
        let _ = self.x.send_event(false, w, EventMask::STRUCTURE_NOTIFY, ev);
        if let Some(sync) = self.sync.as_mut() {
            sync.geo.x_sent(w, r, seq);
        }
        crate::vlog!(
            "-showwindows: {w:#x} follows the desktop: {}x{}+{}+{}",
            r.w,
            r.h,
            r.x,
            r.y
        );
    }

    /// The pointer on `w` maps from where the desktop has it
    /// (`winsync::pointer_origin`).
    fn set_origin(&mut self, w: Window) {
        let desk = self.sync.as_ref().and_then(|s| s.geo.desk(w));
        if let Some(s) = self.windows.get_mut(&w)
            && matches!(s.role, Role::Toplevel(_))
        {
            (s.x, s.y) = winsync::pointer_origin(desk, s.at);
        }
    }

    /// The reply to a MoveResize of a held self-drag's window: the drag
    /// takes its origin, and a frame callback committed now tells when the
    /// window has been laid out there (`DragMark`).
    fn drag_applied(&mut self, w: Window, requested: Rect, applied: Rect) {
        let Some(d) = self.drag.as_mut().filter(|d| d.window == w) else {
            return;
        };
        let Some(origin) = d.pointer.applied(
            (requested.x, requested.y),
            (applied.x, applied.y),
            Instant::now(),
        ) else {
            return;
        };
        let mark = DragMark { drag: d.id, origin };
        if let Some(s) = self.windows.get(&w)
            && s.configured
        {
            s.surface.frame(&self.qh, mark);
            s.surface.commit();
        }
    }

    /// A MoveResize of `w` waits: it goes now, with the frame callback of the
    /// commit in flight, or at its pacing time (`winsync::send_due`).
    fn sync_kick(&mut self, w: Window) {
        let frame_wait = self
            .windows
            .get(&w)
            .filter(|s| self.throttle && s.frame_pending)
            .and_then(|s| s.committed_at);
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        match winsync::send_due(frame_wait, sync.sent_at.get(&w).copied(), Instant::now()) {
            Due::Now => self.sync_send(w),
            Due::Frame => {
                // with the frame callback, or when it is late (the window
                // was hidden right after its commit): no move waits forever
                if let Some(t) = frame_wait {
                    sync.due.entry(w).or_insert(t + winsync::STALE_FRAME);
                }
            }
            Due::At(t) => {
                sync.due.entry(w).or_insert(t);
            }
        }
    }

    /// `w`'s frame callback (or pacing time) came: its coalesced
    /// MoveResize, if one waits.
    fn sync_send(&mut self, w: Window) {
        let synced = self.synced();
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        sync.due.remove(&w);
        // nothing goes before the desktop has shown the window: its first
        // report places it (where it was asked to open, or where GNOME puts
        // it) and X follows that
        if !synced || sync.geo.desk(w).is_none() {
            return;
        }
        if let Some(mr) = sync.geo.frame(w) {
            sync.client.send(Request::MoveResize {
                xid: w,
                rect: mr.rect,
                flags: mr.flags,
            });
            sync.sent_at.insert(w, Instant::now());
            if let Some(d) = self.drag.as_mut().filter(|d| d.window == w) {
                d.pointer.sent((mr.rect.x, mr.rect.y), monotonic_ms());
            }
            crate::vlog!(
                "-showwindows: {w:#x} moved itself: the desktop follows to {}x{}+{}+{} (flags {:#x})",
                mr.rect.w,
                mr.rect.h,
                mr.rect.x,
                mr.rect.y,
                mr.flags
            );
        }
    }

    /// How long the main loop may sleep before a paced MoveResize is due.
    fn sync_timeout_ms(&self) -> Option<i32> {
        let now = Instant::now();
        let next = self.sync.as_ref()?.due.values().min()?;
        let us = next.saturating_duration_since(now).as_micros();
        Some(us.div_ceil(1000).min(1000) as i32)
    }

    /// What the extension sent, and the paced MoveResize requests now due.
    fn sync_poll(&mut self) {
        let Some(sync) = self.sync.as_ref() else {
            return;
        };
        let events: Vec<winsync::Event> = sync.client.events().collect();
        for e in events {
            self.sync_event(e);
        }
        let now = Instant::now();
        let due: Vec<Window> = self.sync.as_ref().map_or_else(Vec::new, |s| {
            s.due
                .iter()
                .filter(|(_, t)| **t <= now)
                .map(|(w, _)| *w)
                .collect()
        });
        for w in due {
            self.sync_send(w);
        }
    }

    fn sync_event(&mut self, e: winsync::Event) {
        use winsync::Event as E;
        match e {
            E::Available { .. } => self.sync_available(true),
            E::Unavailable => self.sync_available(false),
            E::Monitors(list) => {
                let Some(sync) = self.sync.as_mut() else {
                    return;
                };
                if sync.available {
                    sync.monitors = Some(list);
                    self.sync_layout();
                }
            }
            E::Failed {
                xid,
                method,
                error,
                answered,
            } => {
                let mut kick = false;
                if method == "MoveResize"
                    && let Some(sync) = self.sync.as_mut()
                {
                    sync.geo.failed(xid, answered);
                    kick = sync.on && sync.geo.pending(xid);
                    if let Some(d) = self.drag.as_mut().filter(|d| d.window == xid) {
                        d.pointer.failed(answered);
                    }
                }
                crate::vlog!("-showwindows: window sync: {method} of {xid:#x} failed: {error}");
                if kick {
                    self.sync_kick(xid);
                }
            }
            _ if !self.synced() => {}
            E::Applied {
                xid,
                requested,
                applied,
                wire,
            } => {
                let Some(sync) = self.sync.as_mut() else {
                    return;
                };
                let back = sync.geo.applied(xid, requested, applied, Some(wire));
                let pending = sync.geo.pending(xid);
                self.set_origin(xid);
                self.drag_applied(xid, requested, applied);
                if let Some(r) = back {
                    crate::vlog!(
                        "-showwindows: the desktop has {xid:#x} at {}x{}+{}+{} (an edge, a size limit, or a later change)",
                        r.w,
                        r.h,
                        r.x,
                        r.y
                    );
                    self.apply_x(xid, r);
                }
                if pending {
                    self.sync_kick(xid);
                }
            }
            E::Changed {
                xid,
                rect,
                flags,
                wire,
                ..
            } => {
                let Some(sync) = self.sync.as_mut() else {
                    return;
                };
                let Some(x) = sync.geo.x_rect(xid) else {
                    return;
                };
                let fresh = sync.geo.is_fresh(xid, wire);
                let rect = winsync::desktop_position(rect, x);
                let back = sync.geo.changed(xid, rect, flags, wire, Instant::now());
                // the user's Always on Top on the desktop is the window's
                // keep-above (_NET_WM_STATE_ABOVE), so a retell repeats it
                let above = fresh && sync.layers.desktop(xid, flags & winsync::st::ABOVE != 0);
                self.set_origin(xid);
                if let Some(r) = back {
                    self.apply_x(xid, r);
                }
                if above {
                    let on = flags & winsync::st::ABOVE != 0;
                    let atom = self.atoms.net_wm_state_above;
                    self.set_state_atom(xid, atom, on);
                    crate::vlog!(
                        "-showwindows: {xid:#x} {} on the desktop",
                        if on {
                            "was made always on top"
                        } else {
                            "is no longer always on top"
                        }
                    );
                }
            }
            E::State(list, wire) => {
                for ws in list.iter().filter(|w| w.shown) {
                    let Some(sync) = self.sync.as_mut() else {
                        return;
                    };
                    let Some(x) = sync.geo.x_rect(ws.xid) else {
                        continue;
                    };
                    let rect = winsync::desktop_position(ws.rect, x);
                    let back = sync.geo.resync(ws.xid, rect, ws.flags, wire);
                    self.set_origin(ws.xid);
                    if let Some(r) = back {
                        self.apply_x(ws.xid, r);
                    }
                }
                if let Some(sync) = self.sync.as_mut()
                    && sync
                        .order_stamp
                        .is_none_or(|s| winsync::serial_newer(wire, s))
                {
                    sync.order = list.iter().filter(|w| w.shown).map(|w| w.xid).collect();
                    sync.order_stamp = Some(wire);
                }
                self.restack_x();
            }
            E::Restacked(order, wire) => {
                if let Some(sync) = self.sync.as_mut()
                    && sync
                        .order_stamp
                        .is_none_or(|s| winsync::serial_newer(wire, s))
                {
                    sync.order = order;
                    sync.order_stamp = Some(wire);
                }
                self.restack_x();
            }
            E::MonitorsChanged => {
                if let Some(sync) = self.sync.as_ref() {
                    // the layout first (its reply comes first: one worker)
                    sync.client.send(Request::GetMonitors);
                    sync.client.send(Request::GetState);
                }
            }
            E::WindowGone(xid) => {
                crate::vlog!("-showwindows: the desktop's window of {xid:#x} is gone")
            }
        }
    }

    /// The extension came (`on`; at start, or back after the lock screen or
    /// a shell restart) or went. When it comes, the desktop's monitors are
    /// asked for: the sync goes on once they fit the X screen
    /// (`sync_layout`).
    fn sync_available(&mut self, on: bool) {
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        sync.available = on;
        sync.monitors = None;
        if on {
            sync.client.send(Request::GetMonitors);
        } else {
            self.sync_turn(false);
        }
    }

    /// Whether the desktop's monitors (as last reported) fit the X screen
    /// one to one: the sync goes on or off accordingly (each reason to keep
    /// it off is logged when it changes).
    fn sync_layout(&mut self) {
        let screen = self.screen;
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        let Some(monitors) = sync.monitors.as_ref().filter(|_| sync.available) else {
            return;
        };
        let fits = winsync::layout_fits(monitors, screen);
        match &fits {
            Ok(()) => sync.layout_why = None,
            Err(why) => {
                if sync.layout_why.as_ref() != Some(why) {
                    crate::log!(
                        "-showwindows: no window sync with the desktop ({why}): windows keep the 0.1.2 behaviour"
                    );
                    sync.layout_why = Some(why.clone());
                }
            }
        }
        let on = fits.is_ok();
        if on != sync.on {
            self.sync_turn(on);
        }
    }

    /// The sync goes on (a full resync) or off (back to 0.1.2).
    fn sync_turn(&mut self, on: bool) {
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        let was = std::mem::replace(&mut sync.on, on);
        if on {
            sync.layers.retell();
            for w in sync.attention.retell() {
                sync.client
                    .send(Request::SetDemandsAttention { xid: w, on: true });
            }
            sync.client.send(Request::GetState);
            self.announce_sync(true);
            let tops: Vec<Window> = self
                .windows
                .iter()
                .filter(|(_, s)| matches!(s.role, Role::Toplevel(_)))
                .map(|(w, _)| *w)
                .collect();
            for w in tops {
                self.sync_flags(w);
            }
            crate::vlog!("-showwindows: window sync on: windows and the desktop follow each other");
        } else {
            sync.geo.reset();
            sync.due.clear();
            sync.sent_at.clear();
            sync.stack_echo.clear();
            sync.order.clear();
            sync.order_stamp = None;
            if !was {
                return;
            }
            self.announce_sync(false);
            // back to 0.1.2: the pointer maps from X's positions
            for s in self.windows.values_mut() {
                if matches!(s.role, Role::Toplevel(_)) {
                    (s.x, s.y) = s.at;
                }
            }
            crate::vlog!("-showwindows: window sync off: the 0.1.2 behaviour until it is back");
        }
    }

    /// Adds or removes `atom` in `w`'s `_NET_WM_STATE` (the bridge keeps
    /// the property, as the window manager).
    fn set_state_atom(&self, w: Window, atom: u32, on: bool) {
        let mut v = self.net_wm_state(w);
        if v.contains(&atom) == on {
            return;
        }
        if on {
            v.push(atom);
        } else {
            v.retain(|a| *a != atom);
        }
        let _ = self.x.change_property32(
            PropMode::REPLACE,
            w,
            self.atoms.net_wm_state,
            AtomEnum::ATOM,
            &v,
        );
    }

    /// The desktop's keyboard went to shown window `w`: it has the user's
    /// attention, so its demands-attention state ends (EWMH makes removing
    /// it the window manager's job; the desktop clears its own flag).
    fn sync_focused(&mut self, w: Window) {
        if !self.synced_toplevel(w) {
            return;
        }
        let atom = self.atoms.net_wm_state_demands_attention;
        self.set_state_atom(w, atom, false);
        self.sync_flags(w);
    }

    /// `_NET_SUPPORTED` with or without what the sync carries out, and
    /// `_NET_ACTIVE_WINDOW` (kept while it is announced).
    fn announce_sync(&self, on: bool) {
        if !self.wm_announced {
            return;
        }
        let v = self.supported(on);
        let _ = self.x.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_supported,
            AtomEnum::ATOM,
            &v,
        );
        if on {
            self.set_active(self.keyboard_on);
        } else {
            let _ = self
                .x
                .delete_property(self.root, self.atoms.net_active_window);
        }
    }

    /// With the sync, the root's `_NET_ACTIVE_WINDOW` is the shown toplevel
    /// with the desktop's keyboard (None while it is elsewhere), as EWMH asks
    /// of a window manager that announces it.
    fn set_active(&self, w: Option<Window>) {
        if !self.wm_announced || !self.synced() {
            return;
        }
        let w = w
            .filter(|w| self.synced_toplevel(*w))
            .unwrap_or(x11rb::NONE);
        let _ = self.x.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms.net_active_window,
            AtomEnum::WINDOW,
            &[w],
        );
    }

    /// Restacks the shown toplevels in X to the desktop's order (the desktop
    /// is the only stacking authority), the popups staying on top.
    fn restack_x(&mut self) {
        if !self.synced() {
            return;
        }
        let Some(sync) = self.sync.as_ref() else {
            return;
        };
        let desk: Vec<Window> = sync
            .order
            .iter()
            .copied()
            .filter(|w| {
                self.windows
                    .get(w)
                    .is_some_and(|s| matches!(s.role, Role::Toplevel(_)))
            })
            .collect();
        if desk.len() < 2 {
            return;
        }
        let Ok(Ok(tree)) = self.x.query_tree(self.root).map(|c| c.reply()) else {
            return;
        };
        let ops = winsync::restack_plan(&desk, &tree.children);
        if ops.is_empty() {
            return;
        }
        for (w, to) in &ops {
            let aux = match *to {
                winsync::Stack::Above(s) => ConfigureWindowAux::new()
                    .sibling(s)
                    .stack_mode(StackMode::ABOVE),
                winsync::Stack::Below(s) => ConfigureWindowAux::new()
                    .sibling(s)
                    .stack_mode(StackMode::BELOW),
            };
            if let Ok(c) = self.x.configure_window(*w, &aux) {
                let seq = c.sequence_number();
                if let Some(sync) = self.sync.as_mut() {
                    sync.stack_echo.insert(*w, seq);
                }
            }
        }
        for p in self.popups.clone() {
            let _ = self
                .x
                .configure_window(p, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        }
        crate::vlog!(
            "-showwindows: X's stacking follows the desktop's ({} restack(s))",
            ops.len()
        );
    }

    /// The client restacked its own toplevel `w`. A raise over what the
    /// desktop has above it is a request for the raise gating, and X goes
    /// back to the desktop's order.
    fn x_restacked(&mut self, w: Window) {
        let Some(sync) = self.sync.as_ref() else {
            return;
        };
        let Ok(Ok(tree)) = self.x.query_tree(self.root).map(|c| c.reply()) else {
            return;
        };
        if winsync::raised_over_desktop(w, &sync.order, &tree.children) {
            self.raise_request(w, "raises itself");
        }
        // back to the desktop's order even when the raise activates the
        // window: the desktop may keep something above it (a keep-above
        // palette, its own modal dialog) and then sends no new order; if it
        // does raise it, its Restacked brings X along
        self.restack_x();
    }

    /// An X client asks to raise or activate `w` (design_x_capability_gaps.md
    /// A2): honoured after recent input in that client, else the window
    /// demands attention (at most once per interval).
    fn raise_request(&mut self, w: Window, why: &str) -> Raise {
        let Some(client) = self.windows.get(&w).map(|s| s.client) else {
            return Raise::Ignore;
        };
        let timestamp = self.input_time;
        let Some(sync) = self.sync.as_mut() else {
            return Raise::Ignore;
        };
        let r = sync.gate.request(w, client, Instant::now());
        match r {
            Raise::Activate => {
                sync.client.send(Request::Activate { xid: w, timestamp });
                crate::vlog!("-showwindows: {w:#x} {why} after input in its app: activated");
            }
            Raise::DemandAttention => {
                sync.client
                    .send(Request::SetDemandsAttention { xid: w, on: true });
                crate::vlog!(
                    "-showwindows: {w:#x} {why} with no recent input in its app: it demands attention instead"
                );
            }
            Raise::Ignore => crate::vlog!("-showwindows: {w:#x} {why} again: ignored"),
        }
        r
    }

    /// Keep-above and attention of shown toplevel `w` from its hints, told
    /// to the desktop on change (a transient child of an above window is
    /// above too: `winsync::Layers`).
    fn sync_flags(&mut self, w: Window) {
        if !self.synced_toplevel(w) {
            return;
        }
        let parent = self.windows.get(&w).and_then(|s| s.parent);
        let state = self.net_wm_state(w);
        let a = &self.atoms;
        let above =
            state.contains(&a.net_wm_state_above) || state.contains(&a.net_wm_state_stays_on_top);
        // a window with the desktop's keyboard has the user's attention
        let attention = self.keyboard_on != Some(w)
            && (state.contains(&a.net_wm_state_demands_attention)
                || winsync::urgent(&self.wm_hints(w)));
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        sync.layers.set(w, above, parent);
        for (x, on) in sync.layers.changes() {
            sync.client.send(Request::SetAbove { xid: x, on });
            crate::log!(
                "-showwindows: {x:#x} {}",
                if on {
                    "stays above other windows"
                } else {
                    "no longer stays above other windows"
                }
            );
        }
        if let Some(on) = sync.attention.hint(w, attention) {
            sync.client
                .send(Request::SetDemandsAttention { xid: w, on });
            crate::log!(
                "-showwindows: {w:#x} {}",
                if on {
                    "demands attention"
                } else {
                    "no longer demands attention"
                }
            );
        }
    }

    /// With the sync: `_NET_ACTIVE_WINDOW` (through the raise gating), and
    /// `_NET_WM_STATE`'s keep-above and attention (the bridge, as the window
    /// manager, keeps the property). True: nothing more to do with it.
    fn sync_client_message(&mut self, e: &ClientMessageEvent) -> bool {
        let a = &self.atoms;
        let (active, state_atom) = (a.net_active_window, a.net_wm_state);
        let ours = [
            a.net_wm_state_above,
            a.net_wm_state_stays_on_top,
            a.net_wm_state_demands_attention,
        ];
        if e.type_ == active {
            self.raise_request(e.window, "asks to be activated");
            return true;
        }
        if e.type_ != state_atom {
            return false;
        }
        let d = e.data.as_data32();
        let props = [d[1], d[2]];
        if !props.iter().any(|p| ours.contains(p)) {
            return false;
        }
        let mut v = self.net_wm_state(e.window);
        let mut changed = false;
        for atom in ours {
            if !props.contains(&atom) {
                continue;
            }
            let cur = v.contains(&atom);
            let want = match d[0] {
                0 => false,
                1 => true,
                _ => !cur,
            };
            if want != cur {
                if want {
                    v.push(atom);
                } else {
                    v.retain(|x| *x != atom);
                }
                changed = true;
            }
        }
        if changed {
            let _ = self.x.change_property32(
                PropMode::REPLACE,
                e.window,
                state_atom,
                AtomEnum::ATOM,
                &v,
            );
        }
        self.sync_flags(e.window);
        false // its maximise and fullscreen parts, if any, as before
    }

    /// Shown window `w` went away.
    fn sync_forget(&mut self, w: Window) {
        if self.held_on == Some(w) {
            self.held_on = None;
        }
        let synced = self.synced();
        let Some(sync) = self.sync.as_mut() else {
            return;
        };
        sync.geo.forget(w);
        sync.gate.forget_window(w);
        sync.attention.forget(w);
        sync.layers.forget(w);
        sync.stack_echo.remove(&w);
        sync.sent_at.remove(&w);
        sync.due.remove(&w);
        sync.order.retain(|x| *x != w);
        if synced {
            // its transient children no longer inherit its keep-above
            for (x, on) in sync.layers.changes() {
                sync.client.send(Request::SetAbove { xid: x, on });
            }
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
        self.tag(&toplevel, self.root);
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
                at: (0, 0),
                border: 0,
                above: None,
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
                dialog: None,
                maximized: false,
                fullscreen: false,
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
        let (mut x, mut y, mut width, mut height, depth, damage, _) = self.watch(w)?;
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
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind(name, version.min(2), &state.ping_qh, ()))
                }
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
                "xdg_wm_dialog_v1" => state.dialogs = Some(registry.bind(name, 1, qh, ())),
                "xdg_toplevel_tag_manager_v1" => state.tags = Some(registry.bind(name, 1, qh, ())),
                "zwp_tablet_manager_v2" => {
                    state.tablet_manager = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            }
            // tablets: a tool's events come only to a client that has a
            // tablet seat (mutter leaves wl_pointer when a tool comes near),
            // and the seat and the manager may arrive in either order
            if state.tablet_seat.is_none()
                && let (Some(m), Some(seat)) = (state.tablet_manager.as_ref(), state.seat.as_ref())
            {
                state.tablet_seat = Some(m.get_tablet_seat(seat, qh, ()));
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
                state.pointer_entered = false;
                scroll_forgets(
                    &mut state.axis_rest,
                    &mut state.axis_source,
                    &mut state.axis_discrete_seen,
                );
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
            if caps.contains(wl_seat::Capability::Touch) {
                if state.touch.is_none() {
                    state.touch = Some(seat.get_touch(qh, ()));
                }
            } else if let Some(t) = state.touch.take() {
                crate::vlog!("-showwindows: the seat has no touch now");
                state.touch_down = 0;
                if touch_emulation_ends(&mut state.touch_emulating) {
                    state.button(0x110, false);
                }
                if t.version() >= 3 {
                    t.release();
                }
            }
        }
    }
}

/// Answers the compositor's xdg_wm_base pings on a queue and thread of its
/// own (see `spawn_pinger`).
struct Pinger;

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Pinger {
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
            crate::vlog!("-showwindows: answered the compositor's ping {serial}");
        }
    }
}

/// The compositor pings this client to see that it is alive, and offers to
/// force-quit one that does not answer within seconds - which would kill the
/// bridge and every X client on its display at once. The main loop can wait
/// on the X server (a reply held up while another X client grabs the
/// server, e.g. a screenshot tool selecting a region), so pings are answered
/// here, on a queue of their own: xdg_wm_base is bound to it (objects made
/// from it name their own queue, the main one). Meanwhile shown windows just
/// do not update, as every X window freezes on Xorg during a server grab.
fn spawn_pinger(wl: &WlConnection) -> QueueHandle<Pinger> {
    let mut queue: EventQueue<Pinger> = wl.new_event_queue();
    let qh = queue.handle();
    let conn = wl.clone();
    let _ = std::thread::Builder::new()
        .name("showwindows-ping".into())
        .spawn(move || {
            while queue.blocking_dispatch(&mut Pinger).is_ok() {
                if conn.flush().is_err() {
                    break;
                }
            }
        });
    qh
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
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                state.resize.push((*w, width, height));
                if *w != state.root {
                    let (m, f) = toplevel_states(&states);
                    state.states.push((*w, m, f));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1,
        _: xdg_toplevel_tag_manager_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_dialog_v1::XdgWmDialogV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_wm_dialog_v1::XdgWmDialogV1,
        _: xdg_wm_dialog_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_dialog_v1::XdgDialogV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_dialog_v1::XdgDialogV1,
        _: xdg_dialog_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
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
            xdg_popup::Event::Configure { x, y, .. } => state.place_popup(*w, x, y),
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

impl Dispatch<wl_callback::WlCallback, DragMark> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        data: &DragMark,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && let Some(d) = state.drag.as_mut().filter(|d| d.id == data.drag)
        {
            d.pointer.laid_out(data.origin);
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
        // during a held self-drag the motion of a frame goes at its end
        // (with its relative motion), before anything else the frame has
        if state
            .drag
            .as_ref()
            .is_some_and(|d| d.of_pointer(state.pointer_on))
            && !matches!(
                event,
                wl_pointer::Event::Motion { .. } | wl_pointer::Event::Frame
            )
            && let Some(f) = state.drag_parts.other()
        {
            state.drag_frame(f);
        }
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
                state.pointer_s = Some((surface_x, surface_y));
                state.pointer_on = Some(w);
                state.pointer_entered = true;
                state.enter_serial = serial;
                scroll_forgets(
                    &mut state.axis_rest,
                    &mut state.axis_source,
                    &mut state.axis_discrete_seen,
                );
                if w != state.root {
                    // the reverse pointer must hit this window, not one
                    // stacked above it on the X screen; the popups shown stay
                    // above it. With the window sync too: X's stacking
                    // follows the desktop's only as fast as D-Bus brings it,
                    // and the desktop's order also holds windows that are
                    // not showing (minimised, on another workspace), which
                    // stay mapped in X. The raise is ours, not the client's
                    // (`stack_echo`), and the desktop's next order puts X
                    // back in line.
                    if let Ok(c) = state.x.configure_window(
                        w,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    ) && state.synced_toplevel(w)
                        && let Some(sync) = state.sync.as_mut()
                    {
                        sync.stack_echo.insert(w, c.sequence_number());
                    }
                    for p in state.popups.clone() {
                        if p != w {
                            let _ = state.x.configure_window(
                                p,
                                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                            );
                        }
                    }
                }
                state.motion(w, surface_x, surface_y, None);
                state.apply_cursor();
            }
            wl_pointer::Event::Leave { .. } => {
                state.release_buttons();
                state.pointer_on = None;
                state.pointer_entered = false;
                scroll_forgets(
                    &mut state.axis_rest,
                    &mut state.axis_source,
                    &mut state.axis_discrete_seen,
                );
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                state.pointer_s = Some((surface_x, surface_y));
                if state
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.of_pointer(state.pointer_on))
                {
                    state.drag_parts.motion((surface_x, surface_y), time);
                } else if let Some(w) = state.pointer_on {
                    state.motion(w, surface_x, surface_y, Some(time));
                }
            }
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: WEnum::Value(bs),
            } if state.pointer_on.is_some() => {
                let pressed = bs == wl_pointer::ButtonState::Pressed;
                if pressed {
                    state.press_serial = serial;
                    state.input_time = time;
                }
                state.button(button, pressed);
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(src),
            } => state.axis_source = Some(src),
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } if state.pointer_on.is_some()
                && !state.axis_discrete_seen
                && !matches!(
                    state.axis_source,
                    Some(wl_pointer::AxisSource::Wheel | wl_pointer::AxisSource::WheelTilt)
                ) =>
            {
                state.smooth_scroll(axis, value);
            }
            wl_pointer::Event::AxisStop {
                axis: WEnum::Value(axis),
                ..
            } => {
                if axis == wl_pointer::Axis::VerticalScroll {
                    state.axis_rest.1 = 0.0;
                } else {
                    state.axis_rest.0 = 0.0;
                }
            }
            wl_pointer::Event::Frame => {
                state.axis_source = None;
                state.axis_discrete_seen = false;
                if state
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.of_pointer(state.pointer_on))
                    && let Some(f) = state.drag_parts.end()
                {
                    state.drag_frame(f);
                }
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } if state.pointer_on.is_some() => {
                state.axis_discrete_seen = true;
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
                on_keyboard_enter(&mut state.keyboard_left, &mut state.keyboard_on, w);
                state.set_active(Some(w));
                state.sync_focused(w);
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
                let had = state.keyboard_on;
                on_keyboard_leave(&mut state.keyboard_left, &mut state.keyboard_on);
                // its client's earlier input no longer vouches for a raise
                if let Some(w) = had {
                    let client = state.windows.get(&w).map(|s| s.client);
                    if let (Some(sync), Some(c)) = (state.sync.as_mut(), client) {
                        sync.gate.left(c);
                    }
                }
                state.dismiss_menus();
            }
            wl_keyboard::Event::Key {
                key,
                time,
                state: WEnum::Value(ks),
                ..
            } if state.keyboard_on.is_some() => {
                let pressed = ks == wl_keyboard::KeyState::Pressed;
                if pressed {
                    state.input_time = time;
                    state.input_on(state.keyboard_on);
                }
                state.key(key, pressed);
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

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, DragRel> for State {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &DragRel,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        // accelerated, as the desktop's pointer moves; part of the
        // wl_pointer frame it comes in
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx,
            dy,
            ..
        } = event
            && state
                .drag
                .as_ref()
                .is_some_and(|d| d.of_pointer(state.pointer_on))
        {
            let us = (u64::from(utime_hi) << 32) | u64::from(utime_lo);
            state.drag_parts.relative((dx, dy), (us / 1000) as u32);
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
        // unaccelerated, as Xwayland's raw events: mouse-look reads raw
        // motion (SDL's relative mode, XI2 RawMotion), and the X server
        // accelerates the XTest motion for the core pointer itself
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            state.relative.push((dx_unaccel, dy_unaccel));
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
wayland_client::delegate_noop!(State: ignore zwp_tablet_manager_v2::ZwpTabletManagerV2);
wayland_client::delegate_noop!(State: ignore zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2);
wayland_client::delegate_noop!(State: ignore zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2);

/// The touch screen drives the reverse pointer, as Xwayland's pointer
/// emulation does for core and XI 1 clients: the first touch point moves the
/// pointer and holds button 1 until it lifts. (The engines have no XI 2.2
/// touch device for real touch events.)
impl Dispatch<wl_touch::WlTouch, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down {
                surface,
                id,
                x,
                y,
                time,
                ..
            } => {
                state.touch_down += 1;
                let Some(&w) = surface.data::<Window>() else {
                    return;
                };
                // only the first point of a touch sequence emulates
                if let Some(w) =
                    touch_emulation_starts(&mut state.touch_emulating, state.touch_down, id, w)
                {
                    state.pointer_on = Some(w);
                    state.input_time = time;
                    state.motion(w, x, y, Some(time));
                    state.button(0x110, true);
                }
            }
            wl_touch::Event::Motion { time, id, x, y } => {
                if touch_motion_drives(state.touch_emulating, id)
                    && let Some((_, w)) = state.touch_emulating
                {
                    state.motion(w, x, y, Some(time));
                }
            }
            wl_touch::Event::Up { id, .. } => {
                state.touch_down = state.touch_down.saturating_sub(1);
                if touch_lift_ends(&mut state.touch_emulating, id) {
                    state.button(0x110, false);
                }
            }
            wl_touch::Event::Cancel => {
                state.touch_down = 0;
                if touch_emulation_ends(&mut state.touch_emulating) {
                    state.button(0x110, false);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for State {
    fn event(
        _: &mut Self,
        tablet: &zwp_tablet_v2::ZwpTabletV2,
        event: zwp_tablet_v2::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_v2::Event::Removed = event {
            tablet.destroy();
        }
    }
}

impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
        _: zwp_tablet_seat_v2::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(State, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, ()),
    ]);
}

impl Dispatch<zwp_tablet_pad_v2::ZwpTabletPadV2, ()> for State {
    fn event(
        _: &mut Self,
        pad: &zwp_tablet_pad_v2::ZwpTabletPadV2,
        event: zwp_tablet_pad_v2::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_pad_v2::Event::Removed = event {
            pad.destroy();
        }
    }

    wayland_client::event_created_child!(State, zwp_tablet_pad_v2::ZwpTabletPadV2, [
        zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()),
    ]);
}

impl Dispatch<zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
        _: zwp_tablet_pad_group_v2::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(State, zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, [
        zwp_tablet_pad_group_v2::EVT_RING_OPCODE => (zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ()),
        zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => (
            zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
            ()
        ),
    ]);
}

/// A pen or eraser on a shown window drives the reverse pointer the way
/// Xwayland's stylus device moves the core pointer: the tip is button 1,
/// BTN_STYLUS / BTN_STYLUS2 / BTN_STYLUS3 are 2 / 3 / 8. Pressure and tilt
/// can't go through XTest; an engine with a real tablet device would be
/// needed for them.
impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for State {
    fn event(
        state: &mut Self,
        tool: &zwp_tablet_tool_v2::ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_tablet_tool_v2::Event::ProximityIn { surface, .. } => {
                let Some(&w) = surface.data::<Window>() else {
                    return;
                };
                if w == 0 || state.tool_on.is_some() {
                    return; // another tool drives the pointer
                }
                state.tool_on = Some((tool.id(), w));
                state.pointer_on = Some(w);
            }
            _ if state
                .tool_on
                .as_ref()
                .is_none_or(|(id, _)| *id != tool.id()) =>
            {
                if matches!(event, zwp_tablet_tool_v2::Event::Removed) {
                    tool.destroy();
                }
            }
            zwp_tablet_tool_v2::Event::ProximityOut => state.tool_gone(),
            zwp_tablet_tool_v2::Event::Motion { x, y } => state.tool_motion = Some((x, y)),
            zwp_tablet_tool_v2::Event::Down { .. } => state.tool_presses.push((0x110, true)),
            zwp_tablet_tool_v2::Event::Up => state.tool_presses.push((0x110, false)),
            zwp_tablet_tool_v2::Event::Button {
                button,
                state: WEnum::Value(bs),
                ..
            } => {
                if let Some(code) = tool_button(button) {
                    state
                        .tool_presses
                        .push((code, bs == zwp_tablet_tool_v2::ButtonState::Pressed));
                }
            }
            zwp_tablet_tool_v2::Event::Frame { time } => {
                if state.tool_presses.iter().any(|(_, pressed)| *pressed) {
                    state.input_time = time;
                }
                state.tool_frame(time);
            }
            zwp_tablet_tool_v2::Event::Removed => {
                state.tool_gone();
                tool.destroy();
            }
            _ => {}
        }
    }
}

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
    #[test]
    fn the_window_sync_is_turned_off_by_extension_off_only() {
        assert!(super::sync_turned_off(Some("off")));
        assert!(super::sync_turned_off(Some(" OFF\n")));
        assert!(!super::sync_turned_off(Some("auto")));
        assert!(!super::sync_turned_off(Some("")));
        assert!(!super::sync_turned_off(None));
    }

    #[test]
    fn the_synthetic_configure_notify_is_icccm_4_1_5s() {
        let e = super::synthetic_configure(0x400001, -20, 30, 640, 480, 1);
        assert_eq!(
            e.response_type,
            x11rb::protocol::xproto::CONFIGURE_NOTIFY_EVENT
        );
        assert_eq!((e.event, e.window), (0x400001, 0x400001));
        assert_eq!(e.above_sibling, x11rb::NONE);
        assert_eq!(
            (e.x, e.y, e.width, e.height, e.border_width),
            (-20, 30, 640, 480, 1)
        );
        assert!(!e.override_redirect);
        // root coordinates beyond X's 16 bits are clamped, not wrapped
        let e = super::synthetic_configure(1, 70000, -70000, 1 << 20, 5, 0);
        assert_eq!((e.x, e.y, e.width), (i16::MAX, i16::MIN, u16::MAX));
        // the wire form: a ConfigureNotify (22) the server forwards as is
        let bytes: [u8; 32] = e.into();
        assert_eq!(bytes[0], 22);
    }

    #[test]
    fn size_hints_become_limits() {
        use super::size_limits;
        // flags PMinSize|PMaxSize, min 640x480, max 1800x1200
        let mut v = [0u32; 18];
        v[0] = (1 << 4) | (1 << 5);
        (v[5], v[6], v[7], v[8]) = (640, 480, 1800, 1200);
        assert_eq!(size_limits(&v), ((640, 480), (1800, 1200)));
        // Qt's "no maximum" (16777215) is none
        (v[7], v[8]) = (16_777_215, 16_777_215);
        assert_eq!(size_limits(&v), ((640, 480), (0, 0)));
        // flags off: none
        v[0] = 0;
        assert_eq!(size_limits(&v), ((0, 0), (0, 0)));
        assert_eq!(size_limits(&[]), ((0, 0), (0, 0)));
    }

    #[test]
    fn configure_states_are_read() {
        use super::toplevel_states;
        let b = |v: &[u32]| v.iter().flat_map(|x| x.to_ne_bytes()).collect::<Vec<u8>>();
        assert_eq!(toplevel_states(&b(&[4, 1])), (true, false));
        assert_eq!(toplevel_states(&b(&[2, 4])), (false, true));
        assert_eq!(toplevel_states(&b(&[])), (false, false));
    }

    #[test]
    fn helpers_are_not_popups() {
        use super::helper_window;
        // GTK 3's drag-and-drop IPC window: 1x1 at -99,-99
        assert!(helper_window(-99, -99, 1, 1, (1600, 900)));
        // wholly off the screen
        assert!(helper_window(-200, 10, 100, 30, (1600, 900)));
        assert!(helper_window(1600, 10, 100, 30, (1600, 900)));
        // a normal menu is shown
        assert!(!helper_window(570, 32, 90, 356, (1600, 900)));
        // hanging off one edge is still shown
        assert!(!helper_window(-10, 10, 100, 30, (1600, 900)));
        // a 24x24 on-screen window passes this check: it is skipped later,
        // only if its _NET_WM_WINDOW_TYPE says it is a drag-and-drop icon
        assert!(!helper_window(100, 100, 24, 24, (1600, 900)));
    }

    #[test]
    fn smooth_scroll_becomes_clicks() {
        use super::scroll_steps;
        let mut rest = 0.0;
        assert_eq!(scroll_steps(&mut rest, 4.0), 0);
        assert_eq!(scroll_steps(&mut rest, 7.0), 1);
        assert!((rest - 1.0).abs() < 1e-9);
        assert_eq!(scroll_steps(&mut rest, -31.0), -3);
        assert_eq!(scroll_steps(&mut rest, 25.0), 2);
        assert!((rest - 5.0).abs() < 1e-9);
        // a direction change reverses the carried remainder first: nothing is
        // sent until the new direction reaches a whole step of its own
        let mut rest = 8.0;
        assert_eq!(scroll_steps(&mut rest, -8.0), 0);
        assert!((rest - 0.0).abs() < 1e-9);
        let mut rest = -15.0;
        assert_eq!(scroll_steps(&mut rest, 15.0), 0);
        assert_eq!(scroll_steps(&mut rest, 10.0), 1);
    }

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
        // BTN_SIDE .. BTN_MOUSE + 15 in order from 8, as Xwayland numbers
        // them (BTN_SIDE 8, BTN_EXTRA 9, BTN_FORWARD 10, BTN_BACK 11 ...)
        for (code, b) in (0x113..=0x11f).zip(8..) {
            assert_eq!(x_button(code), Some(b), "code {code:#x}");
        }
        assert_eq!(x_button(0x11f), Some(20));
        assert_eq!(x_button(0x120), None); // BTN_TOOL_PEN and up: no button
        // the reverse master's XTest device has 10 buttons: 11 and up are
        // not injectable (dropped, logged once, by State::button)
        assert_eq!(injectable_button(0x114), Some(9));
        assert_eq!(injectable_button(0x115), Some(10));
        assert_eq!(injectable_button(0x116), None);
        assert_eq!(injectable_button(0x11f), None);
        assert_eq!(injectable_button(0x120), None);
    }

    #[test]
    fn touch_emulation_first_point_drives_and_lift_releases() {
        let mut emulating = None;
        // the first point of a sequence on a shown window drives the pointer
        assert_eq!(
            touch_emulation_starts(&mut emulating, 1, 0, 0x2001),
            Some(0x2001)
        );
        assert_eq!(emulating, Some((0, 0x2001)));
        // its motion drives; another point's does not
        assert!(touch_motion_drives(emulating, 0));
        assert!(!touch_motion_drives(emulating, 1));
        // a second point down suppresses emulation (nothing new drives)
        assert_eq!(touch_emulation_starts(&mut emulating, 2, 1, 0x2001), None);
        assert_eq!(emulating, Some((0, 0x2001)));
        // another point's lift changes nothing; the emulating point's lift
        // releases button 1 and ends the emulation
        assert!(!touch_lift_ends(&mut emulating, 1));
        assert_eq!(emulating, Some((0, 0x2001)));
        assert!(touch_lift_ends(&mut emulating, 0));
        assert_eq!(emulating, None);
        // a new sequence starts from nothing
        assert_eq!(
            touch_emulation_starts(&mut emulating, 1, 7, 0x2002),
            Some(0x2002)
        );
        // cancel (or the seat losing touch) ends it
        assert!(touch_emulation_ends(&mut emulating));
        assert_eq!(emulating, None);
        assert!(!touch_emulation_ends(&mut emulating));
        // the cursor surface (window 0) never emulates
        assert_eq!(touch_emulation_starts(&mut emulating, 1, 0, 0), None);
        assert_eq!(emulating, None);
    }

    #[test]
    fn tool_buttons_map_to_mouse_buttons() {
        assert_eq!(tool_button(0x14b), Some(0x112)); // BTN_STYLUS -> middle
        assert_eq!(tool_button(0x14c), Some(0x111)); // BTN_STYLUS2 -> right
        assert_eq!(tool_button(0x149), Some(0x113)); // BTN_STYLUS3 -> side (8)
        assert_eq!(tool_button(0x110), None); // the tip is the Down event
        assert_eq!(tool_button(0x14d), None);
    }

    #[test]
    fn a_tool_frame_presses_in_order_without_repeats() {
        let mut buttons = Vec::new();
        // the tip and a stylus button go out in frame order
        assert_eq!(
            tool_frame_presses(&mut buttons, &[(0x110, true), (0x14b, true)]),
            [(0x110, true), (0x14b, true)]
        );
        assert_eq!(buttons, [0x110, 0x14b]);
        // a repeated press, and a lift of a button not held, are dropped
        assert_eq!(
            tool_frame_presses(&mut buttons, &[(0x110, true), (0x111, false)]),
            []
        );
        assert_eq!(buttons, [0x110, 0x14b]);
        // the lift of a held button goes out
        assert_eq!(
            tool_frame_presses(&mut buttons, &[(0x110, false)]),
            [(0x110, false)]
        );
        assert_eq!(buttons, [0x14b]);
        // so does its press again
        assert_eq!(
            tool_frame_presses(&mut buttons, &[(0x14b, false), (0x14b, true)]),
            [(0x14b, false), (0x14b, true)]
        );
        assert_eq!(buttons, [0x14b]);
    }

    #[test]
    fn a_tool_out_releases_all_and_keeps_the_mouse_pointer() {
        // proximity-out releases everything it holds, in press order, and
        // drops the pointer's window it drove
        assert_eq!(
            tool_out_releases(&[0x110, 0x14b, 0x111], false),
            (vec![0x110, 0x14b, 0x111], true)
        );
        // but the mouse's own pointer had entered a shown window: the
        // pointer's window stays (the buttons still go up)
        assert_eq!(tool_out_releases(&[0x110], true), (vec![0x110], false));
        assert_eq!(tool_out_releases(&[], false), (Vec::<u32>::new(), true));
    }

    #[test]
    fn a_scroll_gesture_is_forgotten_when_the_pointer_changes_window() {
        let mut rest = (3.0, -2.0);
        let mut source = Some(wl_pointer::AxisSource::Finger);
        let mut discrete_seen = true;
        scroll_forgets(&mut rest, &mut source, &mut discrete_seen);
        assert_eq!(rest, (0.0, 0.0));
        assert_eq!(source, None);
        assert!(!discrete_seen);
        // a fresh gesture starts from nothing
        let mut rest = (0.0, 0.0);
        assert_eq!(scroll_steps(&mut rest.1, 4.0), 0);
        assert!((rest.1 - 4.0).abs() < 1e-9);
        scroll_forgets(&mut rest, &mut source, &mut discrete_seen);
        assert_eq!(rest, (0.0, 0.0));
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

    /// A held self-drag compares the compositor's event times with when its
    /// moves were sent: both on CLOCK_MONOTONIC, in milliseconds.
    #[test]
    fn the_drags_clock_is_the_compositors() {
        let a = monotonic_ms().expect("CLOCK_MONOTONIC");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = monotonic_ms().unwrap();
        let d = b.wrapping_sub(a);
        assert!((5..1000).contains(&d), "{a} -> {b}");
    }

    #[test]
    fn handover_suspends_only_the_held_windows_position_mirroring() {
        let w = 0x1001;
        assert!(mirror_position(None, w, &[1]));
        assert!(!mirror_position(Some(w), w, &[1]));
        assert!(!mirror_position(Some(w), w, &[1, 3]));
        assert!(mirror_position(Some(w), 0x1002, &[1]));
        // Even if the hand-over marker remains, release resumes mirroring.
        assert!(mirror_position(Some(w), w, &[]));
    }

    #[test]
    fn keyboard_enter_focuses_globally_active_clients_and_notifies_only_if_advertised() {
        let (mut left, mut on) = (true, None);
        on_keyboard_enter(&mut left, &mut on, 0x1001);
        assert_eq!(on, Some(0x1001));
        assert!(!x_focus_clears(left, on));
        let take = 42;
        // Wine may set input false and advertise WM_TAKE_FOCUS: do both.
        assert_eq!(focus_actions(false, &[7, take], take), (true, true));
        assert_eq!(focus_actions(true, &[take], take), (true, true));
        assert_eq!(focus_actions(true, &[7], take), (true, false));
        assert_eq!(focus_actions(true, &[], take), (true, false));
        // A No Input window must not steal focus from its typing target.
        assert_eq!(focus_actions(false, &[], take), (false, false));
    }

    #[test]
    fn popup_anchor_uses_current_shown_origin_including_border() {
        // Maximising moved the parent from its old floating position to 0,0.
        // TranslateCoordinates returns the client origin, inside its border.
        let origin = shown_origin((2, 2), 2);
        assert_eq!(origin, (0, 0));
        let popup = (120, 48);
        let offset = (popup.0 - origin.0, popup.1 - origin.1);
        assert_eq!(
            popup_anchor(offset.0, offset.1, 1920, 1080),
            ((120, 48), (0, 0))
        );
        assert_eq!(popup_position(origin, offset), popup);
        // A bordered floating parent and a nested popup use the same rule.
        assert_eq!(shown_origin((103, 203), 3), (100, 200));
        assert_eq!(shown_origin((-8, 32), 2), (-10, 30));
    }

    #[test]
    fn popup_follows_compositor_slide_in_parent_coordinates() {
        let origin = shown_origin((102, 202), 2);
        let requested = (700, 500);
        let adjusted = (650, 450);
        assert_eq!(popup_position(origin, requested), (800, 700));
        assert_eq!(popup_position(origin, adjusted), (750, 650));
        // A submenu follows its already adjusted parent's shown origin.
        assert_eq!(popup_position((750, 650), (-100, 24)), (650, 674));
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

    #[test]
    fn x_focus_clears_only_after_a_leave_with_no_enter() {
        let (mut left, mut on) = (false, None);
        // a leave without any enter before it keeps nothing to clear
        assert!(!x_focus_clears(false, None));
        // enter: the keyboard is on one of our windows
        on_keyboard_enter(&mut left, &mut on, 0x1001);
        assert_eq!(on, Some(0x1001));
        assert!(!left);
        assert!(!x_focus_clears(left, on));
        // leave marks the pending clear
        on_keyboard_leave(&mut left, &mut on);
        assert!(on.is_none());
        assert!(x_focus_clears(left, on));
        // a leave followed by an enter in the same batch withdraws the clear
        on_keyboard_enter(&mut left, &mut on, 0x1002);
        assert!(!left);
        assert!(!x_focus_clears(left, on));
        // a leave with no later enter clears the X focus
        on_keyboard_leave(&mut left, &mut on);
        assert!(x_focus_clears(left, on));
    }

    #[test]
    fn relative_mode_needs_a_confining_grab_and_no_lock() {
        // a client's confining grab + hidden cursor + no lock: relative
        // delivery
        assert!(relative_mode_due(true, true, true));
        // already locked: no new lock
        assert!(!relative_mode_due(false, true, true));
        // cursor visible: absolute motion
        assert!(!relative_mode_due(true, false, true));
        // no confining grab: a grab without confine-to (GTK 4's XI2 grabs,
        // which RECORD never sees confine) or none at all: absolute motion
        assert!(!relative_mode_due(true, true, false));
    }

    #[test]
    fn a_lock_ends_when_the_cursor_shows_or_its_confining_grab_does() {
        // the cursor is visible again: absolute motion resumes, however the
        // lock was taken
        assert!(relative_mode_ends(true, false, true, true));
        assert!(relative_mode_ends(true, false, false, false));
        // still blank: a grab-triggered lock ends with the grab
        assert!(!relative_mode_ends(true, true, true, true));
        assert!(relative_mode_ends(true, true, true, false));
        // a warp-triggered lock ends only when the cursor shows, grab or
        // no grab
        assert!(!relative_mode_ends(true, true, false, false));
        assert!(!relative_mode_ends(true, true, false, true));
        // not locked: nothing to end
        assert!(!relative_mode_ends(false, true, true, false));
        assert!(!relative_mode_ends(false, false, false, false));
    }
}
