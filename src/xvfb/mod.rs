//! The Xvfb engine (`-engine xvfb`): a stock Xorg server (Xvfb) speaks the X
//! protocol — every extension, for any X client — and an agent, an ordinary X
//! client of it, keeps it in step with the desktop:
//!
//! - damaged rectangles of the captured desktop are put, through MIT-SHM, onto
//!   the root window, so Xvfb's own DAMAGE/GetImage/XShm serve its clients the
//!   desktop. Every other client's window is redirected offscreen (Composite,
//!   manually, as a compositing manager does), so windows clients map (RealVNC's
//!   UI and its connection notices, when there is no tray) keep working but
//!   never cover it. Without Composite the desktop goes into a topmost window
//!   instead, which windows can briefly cover. X apps that should be *seen*
//!   belong on Xwayland; this display is for tools that watch and drive the
//!   desktop;
//! - input its clients inject (XTest) is seen as XInput2 raw events and
//!   forwarded through the same path as the built-in engine's XTest; a key on
//!   a spare keycode a client put a keysym on (xdotool, x11vnc and RealVNC do
//!   for keysyms they find on no key) is typed through the compositor's
//!   keyboard as the built-in engine types it ([`keymap::Synth`](crate::bridge::keymap::Synth)) —
//!   but only input meant for the desktop: each event is paired with its
//!   client's request through RECORD, and engine helpers' input, or input the
//!   X server sends to another client's window here, stays here (see [`attrib`]);
//! - the compositor keymap is uploaded with `xkbcomp`, so keycodes keep meaning
//!   evdev + 8 and keysyms match: its active layout only, so a keysym is found
//!   on the key that types it on the desktop now;
//! - capture runs only while one of its clients holds a DAMAGE object
//!   (checked with X-Resource) or reads the root's pixels (seen through
//!   RECORD), like the built-in engine's gate;
//! - keys and buttons left down by a client that went away are released, as
//!   the built-in engine releases them: at once, or (a one-shot client's, or
//!   anyone's under `-xtestpersist`) once nobody else has (record.rs);
//! - CLIPBOARD and PRIMARY are bridged like a clipboard manager would;
//! - the desktop's cursor sprite is the X server's cursor (XFixes clients see
//!   it), invisible while the desktop's pointer is hidden;
//! - on Xvnc, a screen size another client sets with RandR (RealVNC resizing
//!   to a viewer's window) is carried to the desktop's monitor with `-dynres`,
//!   and undone on the X side when it cannot be (or without `-dynres`), so the
//!   screen never claims a size the desktop does not have; Xvfb's size is
//!   fixed, so a client's resize is undone there; and an output a client
//!   turned off is turned back on once it is done.
//!
//! The compositor side (capture, input, keymap) is the same backend the
//! built-in engine uses.
//!
//! `-engine xwayland` runs the same agent against a stock *rootful* Xwayland
//! instead (the X server RHEL 10 ships, and the only one Raspberry Pi OS
//! installs; neither has Xvfb or Xvnc by default). Xwayland needs a Wayland
//! compositor: it gets a private one of ours (`wlhost`), on a socket pair
//! nobody else can reach, which shows nothing anywhere and through which the
//! screen follows the desktop's size.
//!
//! If the agent stops (an X error, a client killing its connection, a panic),
//! whatever it installed in the shared state is undone at once — input it
//! forwarded is released on the desktop, it stops counting as a watcher (so
//! the desktop session can end), nothing calls its wake-ups any more — and a
//! new agent takes over. If the X server itself is gone, or the agent keeps
//! failing, the process ends (stopping the wrapped command first) with
//! [`EXIT_ENGINE_LOST`](crate::EXIT_ENGINE_LOST), for its service manager to
//! restart: a RealVNC server, for one, outlives its X server and would
//! otherwise keep serving nothing.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Family,
    HostMode, ImageFormat, NotifyDetail, NotifyMode, StackMode, Window, WindowClass,
};
use x11rb::protocol::{Event, composite, randr, res, shm, xkb, xtest};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

mod a11ybus;
mod attrib;
mod clip;
mod cursor;
mod motion;
mod record;
mod reverse;
// -showwindows' pointer during a held self-drag of a synced window (reverse.rs)
mod selfdrag;
mod wlhost;
// -showwindows' window sync with the desktop (reverse.rs)
mod winsync;
pub mod xrelay;
mod xserver;

pub use xserver::resolve as resolve_server;

use crate::bridge::Server;
use crate::bridge::input::{BUTTON_PRESS, BUTTON_RELEASE, KEY_PRESS, KEY_RELEASE, MOTION_NOTIFY};
use crate::bridge::xkbdesc::one_layout;
use crate::config::EngineType;

/// The X client id the agent's forwarded input is accounted under. Xvfb
/// merges every client's XTest into one device, so per-client accounting is
/// Xvfb's (Xorg's) business here; see `release_orphaned_input`.
const AGENT_CLIENT: u32 = u32::MAX - 1;

/// The core pointer's XInput2 device id (Xorg's "Virtual core pointer").
const CORE_POINTER: u16 = 2;

/// The engine's private accessibility bus address (`-a11ybus`), once it runs:
/// each agent puts it on the root window as `AT_SPI_BUS`.
static A11Y_BUS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// How often to re-check who is watching and whether the keymap changed.
const HOUSEKEEPING: Duration = Duration::from_millis(500);

/// Whether two refresh rates (mHz) are the same for RandR's purposes: within
/// 0.5 % or 0.5 Hz, so Xvnc's own 59.96 Hz mode stands for a 60 Hz desktop.
fn same_refresh(a: u32, b: u32) -> bool {
    let tol = (a.max(b) / 200).max(500);
    a.abs_diff(b) <= tol
}

/// Whether to re-mode Xvnc's output (now stating `current` mHz) for a desktop
/// refreshing at `want` mHz on a `w`x`h` screen: only when the mode X can
/// state for it (RandR's 32-bit dot clock saturates for big fast screens) is
/// a different rate and closer to `want` than the current one: a 16384x16384
/// screen can say 16 Hz at most, so an output already stating 60 stays.
fn remode_for_refresh(current: u32, want: u32, w: u16, h: u16) -> bool {
    let clock = crate::bridge::x11::randr::dot_clock(w, h, want);
    let can = (u64::from(clock) * 1000 / (u64::from(w) * u64::from(h)).max(1)) as u32;
    !same_refresh(can, current) && can.abs_diff(want) < current.abs_diff(want)
}

/// The refresh (mHz) of the first CRTC's current mode, as the X server states
/// it (Xvnc's own modes: 59.96 Hz at 1920x1080, 59.38 at 640x480).
fn current_refresh_mhz(conn: &RustConnection, root: Window) -> Option<u32> {
    let res = randr::get_screen_resources_current(conn, root)
        .ok()?
        .reply()
        .ok()?;
    let crtc = *res.crtcs.first()?;
    let info = randr::get_crtc_info(conn, crtc, res.config_timestamp)
        .ok()?
        .reply()
        .ok()?;
    let mode = res.modes.iter().find(|m| m.id == info.mode)?;
    Some(crate::bridge::x11::randr::refresh_mhz(mode))
}
/// How long a client's resize may take to reach the desktop (as for the
/// built-in engine's `RRSetCrtcConfig`).
const DYNRES_TIMEOUT: Duration = Duration::from_secs(10);
/// How long after another client's last RandR change the screen is brought in
/// line with the desktop (size, output on). A resize takes a client several
/// requests (RealVNC: output off, screen size, new mode, output on) whose
/// config timestamps a change of ours in between would invalidate.
const SETTLE: Duration = Duration::from_secs(1);
/// How long a client's new screen size must stand before the desktop is asked
/// for it: xrandr passes through an intermediate size (the larger of old and
/// new in each direction), and a monitor that happens to have that mode would
/// otherwise be switched to it first.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// A client's screen size and what the desktop made of it.
type DynresResult = ((u16, u16), Result<(), String>);

/// A RandR 1.5 monitor as the desktop has it: its name (the output's), its
/// geometry, and whether it is the desktop's primary output.
#[derive(Clone, PartialEq, Debug)]
struct Monitor {
    name: Vec<u8>,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    primary: bool,
}

/// The index of the monitor that is RandR's primary: the desktop's primary
/// output, or the first monitor if the desktop names none.
fn primary_index(monitors: &[Monitor]) -> usize {
    monitors.iter().position(|m| m.primary).unwrap_or(0)
}

/// The desktop's primary output as RandR is to have it: `known` is the
/// last one RandR was given, `screen` the output that is the desktop's
/// now (0: it names none). A desktop that names none keeps `known` (RandR
/// always has a primary monitor, and a backend without one must not take
/// it away); the first answer then goes to RandR, whatever it is.
fn primary_to_set(known: u32, screen: u32) -> u32 {
    if screen != 0 { screen } else { known }
}

/// A screen size a client set, not handed to the desktop yet.
#[derive(Clone, Copy)]
struct DynresNext {
    size: (u16, u16),
    /// When the client set it.
    since: Instant,
    /// When to look at it (again).
    due: Instant,
}

/// How long a pointer move whose core event never came may wait for RECORD
/// to say whether it was relative (it has normally said so already).
const MOTION_KIND_WAIT: Duration = Duration::from_millis(250);

/// How long to wait for the RECORD thread to catch up with an injected event
/// (it normally takes well under a millisecond); after that the event is
/// forwarded unattributed, as before attribution existed.
const ATTRIB_WAIT: Duration = Duration::from_millis(500);

/// The X authorization the engine's server requires: an MIT-MAGIC-COOKIE-1
/// that only this process and its user know (in a 0600 file in a 0700
/// directory).
#[derive(Clone)]
pub struct Auth {
    cookie: [u8; 16],
    /// The authority file, for X clients we start (xkbcomp, or the wrapped
    /// command if it needs one).
    pub file: PathBuf,
}

const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";

impl Auth {
    /// A fresh random cookie, written as an authority file in `dir`.
    fn create(dir: &Path) -> io::Result<Auth> {
        let mut cookie = [0u8; 16];
        let mut got = 0;
        while got < cookie.len() {
            let n = unsafe {
                libc::getrandom(cookie[got..].as_mut_ptr().cast(), cookie.len() - got, 0)
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            got += n as usize;
        }
        let file = dir.join("Xauthority");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)?;
        f.write_all(&xauth_entry(&cookie))?;
        f.sync_all()?;
        Ok(Auth { cookie, file })
    }
}

/// One Xauthority entry (the libXau format: big-endian lengths) for any
/// display (FamilyWild, no display number): the server loads every cookie in
/// its -auth file, and our clients' libXau matches it whatever the display.
fn xauth_entry(cookie: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(2 + 2 + 2 + 2 + MIT_MAGIC_COOKIE.len() + 2 + cookie.len());
    e.extend_from_slice(&0xffffu16.to_be_bytes()); // FamilyWild
    for field in [&b""[..], b"", MIT_MAGIC_COOKIE, cookie] {
        e.extend_from_slice(&(field.len() as u16).to_be_bytes());
        e.extend_from_slice(field);
    }
    e
}

/// Connects to the engine's display with the cookie.
fn connect(display: u32, auth: &Auth) -> Result<(RustConnection, usize), AnyError> {
    // the abstract socket first, then the file (as libxcb does)
    let addr =
        x11rb_protocol::parse_display::ConnectAddress::Socket(format!("/tmp/.X11-unix/X{display}"));
    let (stream, _) = DefaultStream::connect(&addr)?;
    let conn = RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        MIT_MAGIC_COOKIE.to_vec(),
        auth.cookie.to_vec(),
    )?;
    Ok((conn, 0))
}
/// What the agent selects on the root window: windows mapped or restacked
/// (to stay on top) and the focus (input attribution judges a key by it, and sees the
/// keyboard grabbed through it). A client has one event mask per window, so
/// anything that selects on the root again must keep these.
fn root_events() -> EventMask {
    EventMask::SUBSTRUCTURE_NOTIFY | EventMask::FOCUS_CHANGE
}

/// After a failed keymap upload, retry after this, doubling up to the max.
const KEYMAP_RETRY: Duration = Duration::from_secs(2);
const KEYMAP_RETRY_MAX: Duration = Duration::from_secs(60);
/// How recently a client must have read the root's pixels to count as
/// watching (the built-in engine's read gate, likewise).
const READ_GATE_MS: u64 = 1000;

/// Milliseconds since the first call (never 0): the clock root reads are
/// stamped with.
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// A stopped agent is replaced after this long (the X server has dropped the
/// old one's connection, and its Composite redirection with it, by then).
const AGENT_RESTART_DELAY: Duration = Duration::from_secs(1);
/// More agent failures than this within [`AGENT_RESTART_WINDOW`] end the
/// process: something is wrong that a new agent won't fix.
const AGENT_RESTARTS: usize = 3;
const AGENT_RESTART_WINDOW: Duration = Duration::from_secs(60);
/// How long a vanished X server waits for a stop signal to explain it, before
/// it counts as lost (see `engine_lost`).
const ENGINE_LOSS_SETTLE: Duration = Duration::from_millis(200);
/// How long the X server gets to exit on SIGTERM before SIGKILL.
const ENGINE_STOP_GRACE: Duration = Duration::from_secs(5);

/// The running X server, here rather than with the caller so that whichever
/// path ends the process can stop it.
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
/// Set once [`stop`] runs: the X server going away is then expected, and the
/// agent stopping with it is no failure (nor a reason to change our exit
/// status).
static STOPPING: AtomicBool = AtomicBool::new(false);
/// While the agent runs xkbcomp, whose connection is ours and starts nothing
/// (see record.rs, `-coldconnect start`).
static XKBCOMP_RUNNING: AtomicBool = AtomicBool::new(false);

struct Engine {
    child: Child,
    /// `-a11ybus`: the display's private accessibility bus (stopped before
    /// the directory its socket is in goes).
    _a11y: Option<a11ybus::Bus>,
    /// Our private (0700) directory: the authority file, Xvnc's RFB socket.
    /// Removed with everything in it once the X server is gone (an Xvnc that
    /// was killed, which is how the engine usually ends early, can't remove
    /// its socket itself).
    _dir: PrivateDir,
    /// Xwayland's compositor (stopped after Xwayland, being dropped after it).
    host: Option<wlhost::Host>,
}

/// What [`start`] started: the display, its authorization, and what the
/// wrapped command needs in its environment to connect (nothing while our
/// user's clients are let in by uid, else the authority file).
pub struct Started {
    pub display: u32,
    pub auth: Auth,
    pub xauthority: Option<PathBuf>,
    /// `-a11ybus`: the private accessibility bus's address, for the wrapped
    /// command's `AT_SPI_BUS_ADDRESS`.
    pub a11y_bus: Option<String>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SIGTERM lets it remove its sockets; then reap it, killing it if it
        // won't go (a hung X server must not hang our exit)
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + ENGINE_STOP_GRACE;
            while matches!(self.child.try_wait(), Ok(None)) {
                if Instant::now() >= deadline {
                    crate::warning!("xvfb: the X server didn't exit on SIGTERM; killing it");
                    let _ = self.child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.wait();
        if let Some(h) = &self.host {
            let (commits, frames, configures) = h.resizer().stats();
            crate::vlog!(
                "xwayland: its compositor saw {commits} commits, answered {frames} frame callbacks, sent {configures} configures"
            );
        }
        // the directory goes after this, with `_dir`, and the compositor with `host`
    }
}

/// A directory only this user can enter, removed with everything in it.
struct PrivateDir(PathBuf);

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Xvnc's RFB socket, in the private directory.
const XVNC_SOCKET: &str = "rfb.sock";

/// Stops the X server [`start`] started, if it still runs, and waits for it.
pub fn stop() {
    STOPPING.store(true, Ordering::SeqCst);
    let engine = ENGINE.lock().unwrap_or_else(PoisonError::into_inner).take();
    drop(engine);
}

/// How the agent changes the screen size when the X server can't do it
/// itself through RandR (Xwayland: its compositor configures it).
fn engine_resizer() -> Option<wlhost::Resizer> {
    ENGINE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|e| e.host.as_ref().map(wlhost::Host::resizer))
}

/// Whether the X server [`start`] started is still running.
fn engine_running() -> bool {
    ENGINE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
        .is_some_and(|e| matches!(e.child.try_wait(), Ok(None)))
}

/// A new directory only this user can enter (mkdtemp, so 0700 and a name no
/// one could have taken first), in the user's runtime directory, or else (if
/// there is none, or it can't be written) the temp dir. Its name carries our
/// pid (`xwlbridge-<pid>-XXXXXX`), so that a later start can tell one a bridge
/// that was killed outright left behind (see [`remove_stale_private_dirs`]).
fn private_dir() -> io::Result<PrivateDir> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty() && Path::new(d).is_dir())
        .map(PathBuf::from);
    let mut err = None;
    for base in runtime.into_iter().chain([std::env::temp_dir()]) {
        remove_stale_rfb_sockets(&base);
        remove_stale_private_dirs(&base);
        match mkdtemp(&base.join(format!("xwlbridge-{}-XXXXXX", std::process::id()))) {
            Ok(d) => return Ok(PrivateDir(d)),
            Err(e) => {
                err = Some(io::Error::new(
                    e.kind(),
                    format!("can't make a private directory in {}: {e}", base.display()),
                ))
            }
        }
    }
    Err(err.expect("the temp dir was tried"))
}

fn mkdtemp(template: &Path) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = template.as_os_str().as_bytes().to_vec();
    buf.push(0);
    if unsafe { libc::mkdtemp(buf.as_mut_ptr().cast()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    buf.pop();
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf)))
}

/// Starts the X server `program` (Xvfb, TigerVNC's Xvnc or Xwayland; see
/// `resolve_server`) at the current screen size, on `-display` or a free
/// display, and returns once it serves; [`stop`] stops it.
///
/// No other local user may reach it. With no authorization configured, an
/// Xorg server lets every local connection in (its sockets are world-writable,
/// and the abstract one has no permissions at all), and this display is the
/// user's desktop: its pixels, input forwarded into the session, the clipboard.
/// So the server requires a cookie only we hold, and then lets in, by the
/// peer's uid as the kernel reports it, this user's clients and root's —
/// exactly whom the built-in engine admits — so the wrapped command and the
/// user's own X tools need no cookie.
pub fn start(server: &Arc<Server>, program: &Path) -> io::Result<Started> {
    let geom = server.geometry();
    let dir = private_dir()?;
    let auth = Auth::create(&dir.0)?;
    let sock = dir.0.join(XVNC_SOCKET);
    // a stop that cuts our startup short removes these (the X server dies
    // with us then, by its parent-death signal, and can't)
    crate::engine_files(&[auth.file.as_path(), sock.as_path()], &dir.0);
    // -displayfd: the server picks a free display and writes its number once ready
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let wr_fd = wr.as_raw_fd();
    let (w, h) = (geom.width.max(64), geom.height.max(64));
    let engine = server.config.engine;
    let xvnc = engine == EngineType::Xvnc;
    let mut host = None;
    // Xwayland's end of its compositor connection, open until it has it
    let mut wl_sock: Option<OwnedFd> = None;
    let mut cmd = Command::new(program);
    crate::childenv::pin_x11(&mut cmd);
    if let Some(n) = server.config.display {
        cmd.arg(format!(":{n}"));
    }
    cmd.args([
        "-displayfd",
        &wr_fd.to_string(),
        "-nolisten",
        "tcp",
        "-noreset",
        "-auth",
    ])
    .arg(&auth.file);
    if engine == EngineType::Xwayland {
        // Rootful (no -rootless), software rendering into shared memory
        // (-shm: no GPU, and our compositor offers no dmabuf anyway), on our
        // compositor only: WAYLAND_SOCKET wins over WAYLAND_DISPLAY in
        // libwayland, and both of the user's are dropped so nothing can reach
        // the desktop's compositor; no libei socket either (rootful Xwayland
        // handles XTest itself).
        let (w, h) = (w.max(320), h.max(200));
        cmd.args(["-geometry", &format!("{w}x{h}"), "-shm"])
            .env_remove("LIBEI_SOCKET");
        match std::env::var("XWLBRIDGE_XWAYLAND_COMPOSITOR") {
            // For measurements only: an existing private compositor (e.g. a
            // weston --backend=headless) instead of ours; fixed size then
            Ok(other) if !other.is_empty() => {
                crate::warning!(
                    "xwayland: on the compositor {other:?} (XWLBRIDGE_XWAYLAND_COMPOSITOR); the screen keeps its size"
                );
                cmd.env("WAYLAND_DISPLAY", other)
                    .env_remove("WAYLAND_SOCKET");
            }
            _ => {
                let (compositor, sock) = wlhost::Host::start(w, h)?;
                cmd.env("WAYLAND_SOCKET", sock.as_raw_fd().to_string())
                    .env_remove("WAYLAND_DISPLAY");
                host = Some(compositor);
                wl_sock = Some(sock);
            }
        }
    } else if xvnc {
        // Xvnc insists on an RFB listener: give it a socket only this user can
        // reach (in our private directory), which accepts no input, clipboard
        // or resize requests either
        cmd.args([
            "-geometry",
            &format!("{w}x{h}"),
            "-depth",
            "24",
            "-rfbport",
            "-1",
        ])
        .arg("-rfbunixpath")
        .arg(&sock)
        .args([
            "-rfbunixmode",
            "0600",
            "-SecurityTypes",
            "None",
            "-AcceptKeyEvents=0",
            "-AcceptPointerEvents=0",
            "-AcceptCutText=0",
            "-SendCutText=0",
            "-AcceptSetDesktopSize=0",
        ]);
    } else {
        cmd.args(["-screen", "0", &format!("{w}x{h}x24")]);
    }
    let sock_fd = wl_sock.as_ref().map(|s| s.as_raw_fd());
    unsafe {
        cmd.pre_exec(move || {
            // the write end (and Xwayland's compositor socket) must survive
            // exec; and the server must not outlive us
            libc::fcntl(wr_fd, libc::F_SETFD, 0);
            if let Some(fd) = sock_fd {
                libc::fcntl(fd, libc::F_SETFD, 0);
            }
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("can't start {}: {e}", program.display())))?;
    drop(wr);
    drop(wl_sock);
    // from here on, a failure stops it again (and removes the directory)
    let mut engine = Engine {
        child,
        _a11y: None,
        _dir: dir,
        host,
    };
    let display = read_display(rd)?;
    let mut a11y_bus = None;
    if server.config.a11ybus {
        match a11ybus::start(&engine._dir.0, display) {
            Ok(bus) => {
                crate::log!(
                    "xvfb: private accessibility bus for display :{display} at {}",
                    bus.address
                );
                let _ = A11Y_BUS.set(bus.address.clone());
                a11y_bus = Some(bus.address.clone());
                engine._a11y = Some(bus);
            }
            Err(e) => crate::warning!(
                "xvfb: no private accessibility bus ({e}); apps on display :{display} will \
                 register on the desktop's, where a screen reader follows windows nobody sees"
            ),
        }
    }
    let xauthority = match allow_own_user(display, &auth) {
        Ok(()) => None,
        Err(e) => {
            crate::warning!(
                "xvfb: can't let our own user's X clients in without the cookie ({e}); \
                 they need XAUTHORITY={} (the wrapped command gets it)",
                auth.file.display()
            );
            Some(auth.file.clone())
        }
    };
    crate::log!(
        "xvfb: {} serving display :{display} ({w}x{h}); X clients need the cookie{}",
        program.display(),
        if xauthority.is_none() {
            " or to run as this user (or root)"
        } else {
            ""
        }
    );
    *ENGINE.lock().unwrap_or_else(PoisonError::into_inner) = Some(engine);
    Ok(Started {
        display,
        auth,
        xauthority,
        a11y_bus,
    })
}

/// Lets clients running as our own user, and root, in without the cookie
/// (ServerInterpreted "localuser", what `xhost +si:localuser:NAME` adds: the
/// server checks the connecting process's uid).
fn allow_own_user(display: u32, auth: &Auth) -> Result<(), AnyError> {
    let (conn, _) = connect(display, auth)?;
    let uid = unsafe { libc::getuid() };
    let mut uids = vec![uid];
    if uid != 0 {
        uids.push(0);
    }
    for u in uids {
        // a numeric uid ('#'), so no user database is needed
        let address = format!("localuser\0#{u}");
        conn.change_hosts(
            HostMode::INSERT,
            Family::SERVER_INTERPRETED,
            address.as_bytes(),
        )?
        .check()?;
    }
    // the hosts stay: with -noreset the server never resets its access list
    Ok(())
}

/// Removes the private directories (authority file, Xvnc's RFB socket) that
/// earlier bridges left in `dir`: a bridge killed outright (SIGKILL, the OOM
/// killer) can't remove its own, nor can a stop that cuts our startup short,
/// when Xvnc may still create its socket after we are gone. One is left alone
/// while a process with the pid in its name runs, or anything answers on its
/// socket.
fn remove_stale_private_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("xwlbridge-"))
            .and_then(|n| n.rsplit_once('-'))
            .filter(|(_, rand)| rand.len() == 6)
            .and_then(|(pid, _)| pid.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let path = entry.path();
        // only a real directory of ours (never through a link)
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) || !pid_gone(pid) {
            continue;
        }
        let sock = path.join(XVNC_SOCKET);
        if std::fs::symlink_metadata(&sock).is_ok()
            && std::os::unix::net::UnixStream::connect(&sock).is_ok()
        {
            continue;
        }
        crate::vlog!("xvfb: removing a stale engine directory {}", path.display());
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Whether no process with id `pid` exists (and it isn't ours).
fn pid_gone(pid: libc::pid_t) -> bool {
    if pid <= 0 || pid as u32 == std::process::id() {
        return false;
    }
    // SAFETY: signal 0 only checks for the process
    let r = unsafe { libc::kill(pid, 0) };
    r != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Removes the RFB sockets that bridges of earlier versions, which put Xvnc's
/// socket straight in the runtime directory named after their pid, left in
/// `dir` when killed outright (see [`remove_stale_private_dirs`] for today's).
/// One is left alone while a process with its pid runs, or anything answers on
/// it.
fn remove_stale_rfb_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("xwlbridge-xvnc-"))
            .and_then(|n| n.strip_suffix(".sock"))
            .and_then(|n| n.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let path = entry.path();
        if stale_rfb_socket(&path, pid) {
            crate::vlog!("xvfb: removing a stale Xvnc socket {}", path.display());
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether the RFB socket at `path`, made for the bridge with process id
/// `pid`, is left over: that process is gone and nothing listens on it.
fn stale_rfb_socket(path: &std::path::Path, pid: libc::pid_t) -> bool {
    use std::os::unix::fs::FileTypeExt;
    pid_gone(pid)
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
        && matches!(
            std::os::unix::net::UnixStream::connect(path),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused
        )
}

fn read_display(fd: OwnedFd) -> io::Result<u32> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 15_000) } <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the X server didn't start",
        ));
    }
    let mut s = String::new();
    std::fs::File::from(fd).read_to_string(&mut s)?;
    s.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the X server didn't start (display {s:?})"),
        )
    })
}

/// One MIT-SHM segment, attached by us and the X server; it goes away once
/// both detach: the X server when it is told to or our connection closes, we
/// when this is dropped.
struct Shm {
    seg: u32,
    ptr: *mut u8,
    len: usize,
}

impl Shm {
    /// A new segment of `len` bytes, attached on both sides.
    fn attach(conn: &RustConnection, len: usize) -> Result<Shm, AnyError> {
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let ptr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) } as *mut u8;
        if ptr as isize == -1 {
            let e = io::Error::last_os_error();
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(e.into());
        }
        // ours from here: an error below detaches it again
        let mut shm = Shm { seg: 0, ptr, len };
        let attached = (|| -> Result<(), AnyError> {
            shm.seg = conn.generate_id()?;
            shm::attach(conn, shm.seg, shmid as u32, true)?;
            conn.sync()?;
            Ok(())
        })();
        // once both are attached it goes when both detach
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        attached.map(|()| shm)
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.ptr.cast()) };
    }
}

/// A raw pointer motion not yet settled by its core event.
struct PendingMotion {
    /// Its place in the pairing with RECORD's reports, if RECORD is running.
    id: Option<u64>,
    /// Valuators 0 and 1 (each only if the event carries it): the position
    /// for an absolute move, the distance for a relative one.
    x: Option<i32>,
    y: Option<i32>,
    /// The same, as the client sent them (what RECORD reports).
    raw_x: Option<i32>,
    raw_y: Option<i32>,
    /// Its server time (its core event has the same) and whether it goes to
    /// the desktop (see `attrib`).
    time: u32,
    forward: bool,
}

/// What decides whether injected input is the desktop's (see [`attrib`]).
#[derive(Default)]
struct Target {
    own_base: u32,
    own_mask: u32,
    /// The XTest devices' ids (raw events from other devices aren't injected).
    xtest_devices: Vec<u16>,
    /// Another client has the pointer / the keyboard grabbed.
    pointer_grabbed: bool,
    keyboard_grabbed: bool,
    /// The X focus is on a window below the root (another client's: ours
    /// never take it), as the root's focus events say: they come in server
    /// order with the raw events, so a key is judged by the focus it was
    /// made under, not by where a client that changes the focus right after
    /// its key has put it by the time we ask.
    focus_inferior: bool,
    /// -showwindows / -reverseinput: the X focus is on the agent's own
    /// keyboard window (`kfocus`), as that window's focus events say. The
    /// root's events only tell that some window below it has the focus;
    /// this one is ours, so a key then reaches no other client's window.
    focus_ours: bool,
    /// Clients whose input stayed here, and why (logged once each).
    dropped: std::collections::HashSet<(u32, String, String)>,
    unattributed: u64,
    stats: AttribStats,
}

/// What attribution costs the agent's loop, logged now and then (-verbose).
#[derive(Default)]
struct AttribStats {
    events: u64,
    /// Events whose request hadn't arrived yet (a marker was sent).
    marked: u64,
    /// Time to find the request: <=0.1, <=1, <=5, <=20, >20 ms.
    wait_hist: [u64; 5],
    wait_max: Duration,
    /// Time spent asking the X server where an `Other` client's event goes.
    judged: u64,
    judge_max: Duration,
    logged: u64,
    last_log: Option<Instant>,
}

impl AttribStats {
    fn waited(&mut self, d: Duration, marked: bool) {
        self.events += 1;
        self.marked += u64::from(marked);
        let ms = d.as_secs_f64() * 1000.0;
        let i = [0.1, 1.0, 5.0, 20.0]
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(4);
        self.wait_hist[i] += 1;
        self.wait_max = self.wait_max.max(d);
    }

    fn judged(&mut self, d: Duration) {
        self.judged += 1;
        self.judge_max = self.judge_max.max(d);
    }

    /// A summary, at most every 10 s and only if anything happened.
    fn report(&mut self) {
        if self.events == self.logged
            || self
                .last_log
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        crate::vlog!(
            "xvfb: attribution: {} injected events, {} waited for RECORD; wait <=0.1/1/5/20/>20 ms: {:?}, max {:.2} ms; \
             {} judged by target, max {:.2} ms",
            self.events,
            self.marked,
            self.wait_hist,
            self.wait_max.as_secs_f64() * 1000.0,
            self.judged,
            self.judge_max.as_secs_f64() * 1000.0
        );
        self.logged = self.events;
        self.last_log = Some(Instant::now());
    }
}

impl Target {
    fn ours(&self, id: u32) -> bool {
        id & !self.own_mask == self.own_base
    }

    /// The root's focus event with `detail` (`focus_in`: FocusIn, else
    /// FocusOut), mode Normal or WhileGrabbed: where the X focus now is.
    /// A FocusIn on the root says where it arrived: below the root (Virtual,
    /// NonlinearVirtual: an inferior has it), or the root, PointerRoot or None
    /// (Inferior, Nonlinear, PointerRoot, None, Pointer). A FocusOut with
    /// Inferior is the focus going from the root itself to a window below it
    /// (the root gets no FocusIn then); other FocusOuts are followed by a
    /// FocusIn that settles it. A move between two windows below the root
    /// sends the root nothing, and changes nothing here either.
    fn focus_event(&mut self, focus_in: bool, detail: NotifyDetail) {
        if focus_in {
            self.focus_inferior = matches!(
                detail,
                NotifyDetail::VIRTUAL | NotifyDetail::NONLINEAR_VIRTUAL
            );
        } else if detail == NotifyDetail::INFERIOR {
            self.focus_inferior = true;
        }
    }

    /// A focus event on the agent's own keyboard window (-showwindows,
    /// -reverseinput). Grab and Ungrab are keyboard grabs (see
    /// `keyboard_grabbed`), and Pointer details are about the window under
    /// the pointer, not the focus: neither moves the focus.
    fn own_focus_event(&mut self, focus_in: bool, mode: NotifyMode, detail: NotifyDetail) {
        if matches!(mode, NotifyMode::NORMAL | NotifyMode::WHILE_GRABBED)
            && detail != NotifyDetail::POINTER
        {
            self.focus_ours = focus_in;
        }
    }
}

/// The XTest slave devices: their raw events are injected input.
/// Asks the engine's X server for its bells (XKB BellNotify). Without XKB
/// (no stock engine lacks it) bells stay silent, as they were.
fn select_bells(conn: &RustConnection) {
    use x11rb::protocol::xkb::ConnectionExt as _;
    let supported = conn
        .xkb_use_extension(1, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| r.supported);
    if !supported {
        crate::vlog!("xvfb: no XKB on the engine display: X bells stay silent");
        return;
    }
    if let Err(e) = conn.xkb_select_events(
        xkb::ID::USE_CORE_KBD.into(),
        xkb::EventType::from(0u16),
        xkb::EventType::BELL_NOTIFY,
        xkb::MapPart::from(0u16),
        xkb::MapPart::from(0u16),
        &xkb::SelectEventsAux::new(),
    ) {
        crate::warning!("xvfb: can't select X bells: {e}");
    }
}

/// Whether an X bell makes a sound: a client may ring for the event only
/// (XkbBell with event_only, to play its own sound), which must stay silent.
fn bell_sounds(e: &xkb::BellNotifyEvent) -> bool {
    !e.event_only
}

fn xtest_devices(conn: &RustConnection) -> Result<Vec<u16>, AnyError> {
    let infos = conn
        .xinput_xi_query_device(xinput::Device::ALL)?
        .reply()?
        .infos;
    Ok(infos
        .iter()
        .filter(|d| d.name.windows(5).any(|w| w == b"XTEST"))
        .map(|d| d.deviceid)
        .collect())
}

/// What an injected raw event is matched on.
fn raw_sig(
    type_: u8,
    detail: u32,
    time: u32,
    mask: &[u32],
    values: &[xinput::Fp3232],
    raw: &[xinput::Fp3232],
) -> attrib::Sig {
    let both = |(x, y): (Option<i32>, Option<i32>)| x.zip(y);
    attrib::Sig {
        type_,
        detail: detail as u8,
        time,
        xy: both(first_two_valuators(mask, values)),
        xy_raw: both(first_two_valuators(mask, raw)),
    }
}

/// Valuators 0 and 1 of an XI2 raw event, each if present.
fn first_two_valuators(mask: &[u32], values: &[xinput::Fp3232]) -> (Option<i32>, Option<i32>) {
    let (mut x, mut y) = (None, None);
    let mut values = values.iter();
    for bit in 0..mask.len() * 32 {
        if mask[bit / 32] & (1 << (bit % 32)) == 0 {
            continue;
        }
        let Some(v) = values.next() else { break };
        match bit {
            0 => x = Some(v.integral),
            1 => y = Some(v.integral),
            _ => break,
        }
    }
    (x, y)
}

/// Millimetres for `px` pixels at 96 dpi.
fn mm(px: u16) -> u32 {
    u32::from(px) * 254 / 960
}

/// Runs the agent on its own thread for the life of the process, replacing it
/// when it stops while the X server lives on; once the X server is gone (or
/// agents keep failing) the process ends, see [`crate::shutdown`].
pub fn spawn_agent(server: Arc<Server>, display: u32, auth: Auth) {
    // the clipboard relay outlives agents (it is a client of the X server and
    // of the desktop's Xwayland, nothing of the agent's): connected before the
    // first agent watches who connects, so it never counts as a client that
    // starts a session
    if let Some(desk) = server.xwayland_clip.clone() {
        match connect(display, &auth) {
            Ok(c) => xrelay::spawn(server.clone(), c, desk),
            Err(e) => crate::warning!("xvfb: no clipboard relay: {e}"),
        }
    }
    let spawned = std::thread::Builder::new()
        .name("xvfb-agent".into())
        .spawn(move || supervise(server, display, auth));
    if let Err(e) = spawned {
        crate::warning!("xvfb: can't start the agent thread: {e}");
        stop();
        crate::shutdown(crate::EXIT_ENGINE_LOST);
    }
}

/// The agent's recent failures, for deciding whether another one is worth
/// starting.
#[derive(Default)]
struct Failures(VecDeque<Instant>);

impl Failures {
    /// Counts a failure at `now`; whether a new agent may still be started
    /// (no more than [`AGENT_RESTARTS`] failures within [`AGENT_RESTART_WINDOW`]).
    fn record(&mut self, now: Instant) -> bool {
        self.0
            .retain(|t| now.duration_since(*t) < AGENT_RESTART_WINDOW);
        self.0.push_back(now);
        self.0.len() <= AGENT_RESTARTS
    }
}

fn supervise(server: Arc<Server>, display: u32, auth: Auth) {
    let mut failures = Failures::default();
    let mut restart = false;
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // undoes whatever this agent installs, however it ends: an error
            // here, a panic, or a failure part way through Agent::new
            let _teardown = Teardown(server.clone());
            let mut agent = Agent::new(server.clone(), display, auth.clone())?;
            if let Some(devs) = agent.reverse.as_ref().filter(|_| server.config.showwindows) {
                // one reverse thread for the process: it outlives agent restarts
                // (the master device stays, and the thread re-reads windows)
                reverse::spawn_once(
                    display,
                    devs.clone(),
                    auth.clone(),
                    server.config.showscreen,
                );
            }
            if restart {
                // the old agent's clients may have left keys down in the X
                // server that nobody will release now; the desktop side was
                // released when it stopped
                agent.release_orphaned_input()?;
            }
            agent.run()
        }));
        if stopping() {
            // we are exiting and took the X server down ourselves, or were
            // asked to stop (a service manager may signal the X server along
            // with us): no failure, and nothing to restart
            crate::vlog!("xvfb: agent stopped with the X server");
            return;
        }
        match result {
            Ok(Ok(())) => crate::warning!("xvfb: agent stopped"),
            Ok(Err(e)) => crate::warning!("xvfb: agent stopped: {e}"),
            Err(_) => crate::warning!("xvfb: agent panicked"),
        }
        if engine_lost() {
            crate::warning!("xvfb: the X server is gone; exiting");
            break;
        }
        if stopping() {
            return;
        }
        if !failures.record(Instant::now()) {
            crate::warning!(
                "xvfb: the agent stopped {} times in {}s; exiting",
                failures.0.len(),
                AGENT_RESTART_WINDOW.as_secs()
            );
            break;
        }
        std::thread::sleep(AGENT_RESTART_DELAY);
        if stopping() {
            return;
        }
        if engine_lost() {
            crate::warning!("xvfb: the X server is gone; exiting");
            break;
        }
        if stopping() {
            return;
        }
        crate::log!("xvfb: restarting the agent");
        restart = true;
    }
    if stopping() {
        return;
    }
    stop();
    crate::shutdown(crate::EXIT_ENGINE_LOST);
}

/// Whether we are on our way out: we stopped the X server ourselves, or a
/// stop was asked for. The X server and the agent going down are then
/// expected.
fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst) || crate::stop_requested()
}

/// Whether the X server [`start`] started has gone away by itself (not stopped
/// by us, nor explained by a stop that was asked for): for main, once the
/// wrapped command has ended. An X client exits as soon as its display goes
/// (x11vnc with 3, most with 0), often before the agent's supervisor has
/// waited out [`ENGINE_LOSS_SETTLE`] and called it a loss, and main must not
/// then take the command's own status for the cause.
pub fn lost_by_itself() -> bool {
    let started = ENGINE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some();
    started && !stopping() && engine_lost()
}

/// Whether the X server has gone away by itself. A service manager stopping
/// us may signal it along with us (systemd's default `KillMode`, which sends
/// SIGTERM to every process of the service, us first), so when it has gone,
/// our own signal gets a moment to be seen before this counts as a loss.
fn engine_lost() -> bool {
    if engine_running() {
        return false;
    }
    std::thread::sleep(ENGINE_LOSS_SETTLE);
    !stopping()
}

/// Undoes what an agent installed in the process-wide state, when it is
/// dropped (the agent stopped, by error or panic): nothing it forwarded stays
/// pressed on the desktop, it no longer counts as a watcher (so the desktop
/// session can end), and nothing calls its wake-ups any more.
struct Teardown(Arc<Server>);

impl Drop for Teardown {
    fn drop(&mut self) {
        let server = &self.0;
        server.input.release_client(AGENT_CLIENT);
        server.damage.clear_external();
        server.clipboard.clear_watcher();
        server.cursor.clear_notify();
    }
}

/// The RECORD thread's connection, shut down to end that thread when the
/// agent goes.
struct RecordStop {
    sock: OwnedFd,
    stop: Arc<AtomicBool>,
}

impl Drop for RecordStop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        unsafe { libc::shutdown(self.sock.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

type AnyError = Box<dyn std::error::Error + Send + Sync>;

struct Agent {
    server: Arc<Server>,
    display: u32,
    auth: Auth,
    conn: RustConnection,
    root: Window,
    /// Where the desktop is drawn: the root, or (without Composite) a window
    /// kept topmost.
    mirror: Window,
    gc: u32,
    width: u16,
    height: u16,
    /// The refresh (mHz) the one RandR output states: the desktop's primary
    /// monitor's (Xvnc; it starts with its own ~60 Hz mode).
    refresh_mhz: u32,
    shm: Shm,
    /// What wakes our poll: the backend's thread on new damage, the RECORD
    /// thread, the clipboard and cursor. Shared with the closures they hold,
    /// so it stays open as long as any of them can still write to it (they may
    /// outlive us).
    wake: Arc<OwnedFd>,
    damage_atom: u32,
    own_base: u32,
    clients: Vec<u32>,
    watching: bool,
    /// A client other than us holds a DAMAGE object.
    damage_watching: bool,
    /// When a client last read the root's pixels (from the RECORD thread).
    root_reads: Arc<AtomicU64>,
    /// The keymap generation last uploaded.
    keymap_gen: u64,
    /// After a failed upload: of which generation, when to try again, and
    /// the delay used.
    keymap_retry: Option<(u64, Instant, Duration)>,
    /// The window on top of everything: the mirror, or (with Composite) an
    /// input-only shield, so that pointer input reaches no other client's
    /// (offscreen, so invisible) window.
    top: Window,
    /// Where the X server's pointer is as of the events handled so far — not
    /// where it is now: a client's next move may already have been processed
    /// when we get to its click.
    pos: Option<(i16, i16)>,
    /// Whether `pos` moved since the desktop was told.
    pos_dirty: bool,
    /// A raw motion whose core event (the server's word on where the pointer
    /// went) hasn't come yet; see motion.rs.
    pending: Option<PendingMotion>,
    /// XTest moves RECORD saw, and the agent's pairing of them with raw motion.
    motions: Arc<motion::MotionLog>,
    pairing: motion::Pairing,
    /// Which client each injected event came from (filled by the RECORD
    /// thread), and what is known about where input goes now.
    ledger: Arc<attrib::Ledger>,
    target: Target,
    /// XWLBRIDGE_XVFB_DEBUG: log every event and where the pointer is sent.
    debug: bool,
    /// What departed clients left held to release at once, from the RECORD
    /// thread.
    releases: Receiver<Vec<record::Release>>,
    /// Who holds what (kept by the RECORD thread): what lingers is released
    /// from here once it is due.
    holds: Arc<Mutex<record::Holds>>,
    /// Whether that thread is running; without it, see `release_orphaned_input`.
    recording: Arc<AtomicBool>,
    /// The clipboard bridge, unless it couldn't be set up.
    clip: Option<clip::Clip>,
    /// The desktop's cursor sprite as the X server's cursor, unless it
    /// couldn't be set up.
    cursor: Option<cursor::CursorSync>,
    /// Whether the X server can change its screen size (Xvnc, Xwayland; Xvfb can't).
    resizable: bool,
    /// Xwayland's compositor, which resizes the screen (RandR can't there).
    resizer: Option<wlhost::Resizer>,
    /// The last size asked of it that it did not take (not asked again).
    refused: Option<(u16, u16)>,
    /// Xvfb's size, which clients may shrink but nothing can grow back past.
    fixed: (u16, u16),
    /// The desktop's primary output as last seen (`primary_to_set`), when
    /// it named one.
    primary: u32,
    /// Whether RandR 1.5 (monitors, screen change events) is there.
    randr15: bool,
    /// The desktop's outputs as last given to RandR: name, geometry and
    /// whether each is the desktop's primary.
    monitors: Vec<Monitor>,
    /// RandR monitors we created, to delete when the layout changes.
    monitor_atoms: Vec<u32>,
    warned_fixed: bool,
    /// Ends the RECORD thread when we go.
    _record: Option<RecordStop>,
    /// The X server's keyboard mapping as last read: keysyms per keycode and
    /// the keysyms from keycode 8 on.
    x_map: (u8, Vec<u32>),
    /// Keys on spare keycodes, typed through the compositor's keyboard.
    synth: crate::bridge::keymap::Synth,
    /// Resizes by other clients handed to the backend (`-dynres`), answered
    /// from a helper thread, one at a time.
    dynres_results: (mpsc::Sender<DynresResult>, Receiver<DynresResult>),
    /// The size asked for and not answered yet: meanwhile the desktop still
    /// has its old size, which the screen must not follow.
    dynres_out: Option<(u16, u16)>,
    /// The newest size a client set, and when, not asked for yet: once it has
    /// stood for [`DEBOUNCE`] and nothing is out, it is; sizes it replaced (a
    /// burst of resizes, as when a viewer window is dragged) are dropped.
    dynres_next: Option<DynresNext>,
    /// When to bring the screen in line with the desktop after other
    /// clients' RandR changes (see [`SETTLE`]).
    settle_at: Option<Instant>,
    /// With -showwindows or -reverseinput: the master device the desktop's
    /// own input on this display's windows comes through, which must not
    /// echo back to the desktop.
    reverse: Option<reverse::Devices>,
    /// Raw events from that master, not forwarded.
    reverse_ignored: u64,
    /// With -showwindows or -reverseinput: the window the core keyboard is
    /// focused on, so it types into no X window (the input shield's job for
    /// the keyboard).
    kfocus: Option<Window>,
}

// the shm mapping is only touched from the agent's own thread
unsafe impl Send for Agent {}

impl Agent {
    fn new(server: Arc<Server>, display: u32, auth: Auth) -> Result<Agent, AnyError> {
        let server_engine = server.config.engine;
        let (conn, screen) = connect(display, &auth)?;
        let s = &conn.setup().roots[screen];
        let (root, width, height) = (s.root, s.width_in_pixels, s.height_in_pixels);
        let own_base = conn.setup().resource_id_base;
        let own_mask = conn.setup().resource_id_mask;

        // MIT-SHM: one segment the size of the screen
        shm::query_version(&conn)?.reply()?;
        let shm = Shm::attach(&conn, width as usize * height as usize * 4)?;

        // Every other client's window goes offscreen; the desktop is drawn on
        // the root, which nothing else then paints.
        let composited = composite::query_version(&conn, 0, 4)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some()
            && composite::redirect_subwindows(&conn, root, composite::Redirect::MANUAL)
                .ok()
                .and_then(|c| c.check().ok())
                .is_some();
        let (mirror, top) = if composited {
            conn.change_window_attributes(
                root,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .background_pixmap(x11rb::NONE)
                    // focus: to see the keyboard grabbed (see `attrib`)
                    .event_mask(root_events()),
            )?;
            let shield = conn.generate_id()?;
            conn.create_window(
                0,
                shield,
                root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    // crossing: to see the pointer grabbed (see `attrib`)
                    .event_mask(
                        EventMask::POINTER_MOTION
                            | EventMask::ENTER_WINDOW
                            | EventMask::LEAVE_WINDOW,
                    ),
            )?;
            // -showwindows / -reverseinput: shown windows (or a desktop-side
            // proxy's reverse pointer) must reach this display's windows, so
            // the shield (which takes every device's input) stays down and
            // the core pointer is grabbed instead (see below)
            if !(server.config.showwindows || server.config.reverseinput) {
                conn.map_window(shield)?;
            }
            (root, shield)
        } else {
            crate::warning!(
                "xvfb: can't redirect windows offscreen (no Composite, or another \
                 compositing manager); other clients' windows may cover the desktop"
            );
            let m = Self::create_mirror(&conn, s, width, height)?;
            (m, m)
        };
        if server_engine == EngineType::Xwayland && composited {
            Self::stop_xwayland_commits(&conn, root);
        }
        let resizer = engine_resizer();
        let gc = conn.generate_id()?;
        conn.create_gc(gc, mirror, &CreateGCAux::new())?;

        // input the clients inject, as raw events from every device
        conn.xinput_xi_query_version(2, 2)?.reply()?;
        conn.xinput_xi_select_events(
            root,
            &[
                xinput::EventMask {
                    deviceid: xinput::Device::ALL_MASTER.into(),
                    mask: vec![
                        xinput::XIEventMask::RAW_KEY_PRESS
                            | xinput::XIEventMask::RAW_KEY_RELEASE
                            | xinput::XIEventMask::RAW_BUTTON_PRESS
                            | xinput::XIEventMask::RAW_BUTTON_RELEASE
                            | xinput::XIEventMask::RAW_MOTION,
                    ],
                },
                // devices coming and going (a new master brings its own
                // XTest devices): see `xtest_devices`
                xinput::EventMask {
                    deviceid: xinput::Device::ALL.into(),
                    mask: vec![xinput::XIEventMask::HIERARCHY],
                },
            ],
        )?;
        // the X bell: XKB reports every bell (a core Bell or an XkbBell) as
        // BellNotify; Xvfb and Xvnc have no speaker, so it rings the
        // desktop's system bell instead (see `bell_sounds`)
        select_bells(&conn);
        let mut reverse = None;
        let mut kfocus = None;
        // -showwindows shows this display's windows on the desktop, which
        // needs the reverse master; -reverseinput sets up only the master
        // (for a desktop-side proxy, tools/x11-a11y-proxy)
        let want_reverse = server.config.showwindows || server.config.reverseinput;
        let flag = if server.config.showwindows {
            "-showwindows"
        } else {
            "-reverseinput"
        };
        if want_reverse && !composited {
            if server.config.showwindows {
                crate::warning!(
                    "-showwindows needs Composite on the X server; not showing windows"
                );
            } else {
                crate::warning!(
                    "xvfb: -reverseinput needs Composite on the X server; the input shield stays"
                );
            }
        } else if want_reverse {
            match Self::setup_reverse(&conn, root) {
                Ok((devs, focus)) => {
                    crate::log!(
                        "xvfb: {flag}: reverse input on master {:?} (pointer {}, keyboard {}); the core pointer is \
                         grabbed and the core keyboard focused away from every X window",
                        reverse::MASTER_NAME,
                        devs.pointer,
                        devs.keyboard
                    );
                    reverse = Some(devs);
                    kfocus = Some(focus);
                }
                Err(e) => {
                    if server.config.showwindows {
                        crate::warning!("-showwindows: can't set up ({e}); not showing windows");
                    } else {
                        crate::warning!(
                            "xvfb: -reverseinput: can't set up ({e}); the input shield stays"
                        );
                    }
                    conn.map_window(top)?;
                }
            }
        }
        // -a11ybus: where this display's apps find their accessibility bus
        if let Some(addr) = A11Y_BUS.get() {
            let atom = conn.intern_atom(false, b"AT_SPI_BUS")?.reply()?.atom;
            conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                root,
                atom,
                x11rb::protocol::xproto::AtomEnum::STRING,
                addr.as_bytes(),
            )?;
        }
        res::query_version(&conn, 1, 2)?.reply()?;
        // Xorg registers damage objects under this resource-type name
        let damage_atom = conn.intern_atom(false, b"DamageExt")?.reply()?.atom;
        conn.flush()?;

        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        // The backend thread wakes us when it adds damage, and others below.
        // Each holds the eventfd itself: they can call this after we are gone
        // (a call already under way when we stop), and a bare fd number would
        // by then be closed, or reused by some unrelated pipe or socket that
        // the 8 bytes would land in.
        let poke = {
            let wake = wake.clone();
            move || {
                let one: u64 = 1;
                unsafe { libc::write(wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
            }
        };
        server
            .damage
            .set_external(false, Some(Box::new(poke.clone())));
        // and when the desktop's cursor sprite changes
        let cursor = match cursor::CursorSync::new(&conn, root, top) {
            Ok(c) => {
                server.cursor.set_notify(Box::new(poke.clone()));
                Some(c)
            }
            Err(e) => {
                crate::warning!("xvfb: the desktop's cursor won't be the X cursor: {e}");
                None
            }
        };

        // the clipboard: relayed with the desktop's Xwayland (a thread of its
        // own for the life of the process, see spawn_agent), or bridged
        // through the backend's session like the built-in engine's
        let clip = if server.xwayland_clip.is_some() {
            None
        } else {
            match clip::Clip::new(&conn, root, &server, Arc::new(poke.clone())) {
                Ok(c) => Some(c),
                Err(e) => {
                    crate::warning!("xvfb: no clipboard bridge: {e}");
                    None
                }
            }
        };

        let (tx, releases) = mpsc::channel();
        let holds = Arc::new(Mutex::new(record::Holds::new(
            &server.config.input_policy(),
            own_base,
        )));
        let rec_holds = holds.clone();
        let recording = Arc::new(AtomicBool::new(true));
        let rec = recording.clone();
        let root_reads = Arc::new(AtomicU64::new(0));
        let reads = record::RootReads {
            root,
            agent_base: own_base,
            last_ms: root_reads.clone(),
            framebuffer: server.framebuffer.clone(),
            prefetch: server.config.coldconnect() == crate::config::ColdConnect::Start,
            ours_running: &XKBCOMP_RUNNING,
        };
        // RECORD on a connection of its own, which we shut down to end that
        // thread when we go
        let motions = Arc::new(motion::MotionLog::default());
        let rec_motions = motions.clone();
        let xtest_devices = xtest_devices(&conn)?;
        // where the X focus is now; the root's focus events (selected above)
        // keep it up to date from here on
        // (below the root, ours included: only -showwindows / -reverseinput
        // focus a window of ours, and `focus_ours` says when it is that one,
        // as the root's events would from here on)
        let focus_now = conn
            .get_input_focus()
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|f| f.focus);
        let focus_inferior = focus_now.is_some_and(|f| f > 1 && f != root);
        let focus_ours = kfocus.is_some() && focus_now == kfocus;
        let ledger = Arc::new(attrib::Ledger::default());
        let attrib = record::Attrib {
            ledger: ledger.clone(),
            rules: server.config.xtest_rules(),
            agent_base: own_base,
            lookup: connect(display, &auth).ok().map(|(c, _)| c),
        };
        let rec_ledger = ledger.clone();
        let record = connect(display, &auth).and_then(|(rconn, _)| {
            let sock = rconn.stream().as_raw_fd();
            let dup = unsafe { libc::fcntl(sock, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                return Err(io::Error::last_os_error().into());
            }
            let stop = RecordStop {
                sock: unsafe { OwnedFd::from_raw_fd(dup) },
                stop: Arc::new(AtomicBool::new(false)),
            };
            let flag = stop.stop.clone();
            std::thread::Builder::new()
                .name("xvfb-record".into())
                .spawn(move || {
                    if let Err(e) = record::run(
                        rconn,
                        &rec_holds,
                        tx,
                        reads,
                        &rec_motions,
                        poke,
                        &flag,
                        attrib,
                    ) && !flag.load(Ordering::Acquire)
                        && !STOPPING.load(Ordering::SeqCst)
                    {
                        crate::warning!(
                            "xvfb: can't follow which client holds which key ({e}); \
                             falling back to releasing input when clients are seen to leave, \
                             and forwarding every client's input to the desktop"
                        );
                    }
                    // its grab sightings go with it (no RECORD, no grab trigger)
                    record::clear_grabs();
                    rec_ledger.close();
                    rec.store(false, Ordering::Release);
                })?;
            Ok(stop)
        });
        let record = match record {
            Ok(r) => Some(r),
            Err(e) => {
                crate::warning!(
                    "xvfb: can't follow which client holds which key ({e}); \
                     falling back to releasing input when clients are seen to leave, \
                     and forwarding every client's input to the desktop"
                );
                ledger.close();
                recording.store(false, Ordering::Release);
                None
            }
        };
        // the refresh the server's one output states now, 0 if unknown
        let refresh_mhz = current_refresh_mhz(&conn, root).unwrap_or(0);
        Ok(Agent {
            server,
            display,
            auth,
            conn,
            root,
            mirror,
            gc,
            width,
            height,
            refresh_mhz,
            shm,
            wake,
            damage_atom,
            own_base,
            clients: Vec::new(),
            watching: false,
            damage_watching: false,
            root_reads,
            keymap_gen: u64::MAX,
            keymap_retry: None,
            top,
            pos: None,
            pos_dirty: false,
            pending: None,
            motions,
            pairing: motion::Pairing::default(),
            ledger,
            target: Target {
                own_base,
                own_mask,
                xtest_devices,
                focus_inferior,
                focus_ours,
                ..Target::default()
            },
            debug: std::env::var_os("XWLBRIDGE_XVFB_DEBUG").is_some_and(|v| !v.is_empty()),
            releases,
            holds,
            recording,
            clip,
            cursor,
            resizable: server_engine == EngineType::Xvnc || resizer.is_some(),
            resizer,
            refused: None,
            fixed: (width, height),
            primary: 0,
            randr15: false,
            monitors: Vec::new(),
            monitor_atoms: Vec::new(),
            warned_fixed: false,
            _record: record,
            x_map: (0, Vec::new()),
            synth: Default::default(),
            dynres_results: mpsc::channel(),
            dynres_out: None,
            dynres_next: None,
            settle_at: None,
            reverse,
            reverse_ignored: 0,
            kfocus,
        })
    }

    /// Re-reads the X server's keyboard mapping (at start, after an upload,
    /// and when a client changed it), and tells [`Input`](crate::bridge::input::Input)
    /// what each keycode shows.
    fn read_mapping(&mut self) {
        use crate::bridge::keymap::MIN_KEYCODE;
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode.max(MIN_KEYCODE), setup.max_keycode);
        let Some(reply) = self
            .conn
            .get_keyboard_mapping(min, max.saturating_sub(min).saturating_add(1))
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return;
        };
        let per = reply.keysyms_per_keycode;
        let mut syms = vec![0u32; usize::from(min - MIN_KEYCODE) * usize::from(per)];
        syms.extend(reply.keysyms);
        self.x_map = (per, syms);
        let level1: Vec<u32> = (0..=u8::MAX).map(|kc| self.x_sym(kc)).collect();
        self.server.input.set_x_key_syms(Some(level1.into()));
    }

    /// The keysym keycode `kc` types without modifiers on the X server.
    fn x_sym(&self, kc: u8) -> u32 {
        let (per, syms) = &self.x_map;
        if kc < crate::bridge::keymap::MIN_KEYCODE || *per == 0 {
            return 0;
        }
        let at = usize::from(kc - crate::bridge::keymap::MIN_KEYCODE) * usize::from(*per);
        syms.get(at).copied().unwrap_or(0)
    }

    /// A key a client pressed or released: on a spare keycode of the
    /// compositor's keyboard that the client put a keysym on, that keysym
    /// typed through the compositor's keyboard; anything else as it is.
    fn key(&mut self, press: bool, kc: u8) {
        let input = &self.server.input;
        let shortcut = press && input.shortcut_held_or_queued();
        let keymap = self.server.keymap.lock().unwrap();
        let x_sym = self.x_sym(kc);
        let events = self.synth.key(
            press,
            kc,
            || {
                keymap
                    .as_ref()
                    .filter(|t| t.spare(kc))
                    .and((x_sym != 0).then_some(x_sym))
            },
            |m| input.holds(AGENT_CLIENT, m),
            keymap.as_ref(),
            shortcut,
            &"xvfb:",
        );
        drop(keymap);
        match events {
            Some(events) => {
                for (down, k) in events {
                    input.fake_input(
                        AGENT_CLIENT,
                        if down { KEY_PRESS } else { KEY_RELEASE },
                        k,
                        0,
                        0,
                    );
                }
            }
            None => input.fake_input(
                AGENT_CLIENT,
                if press { KEY_PRESS } else { KEY_RELEASE },
                kc,
                0,
                0,
            ),
        }
    }

    /// Rootful Xwayland copies the root window's damage into a Wayland buffer
    /// on every frame, for a compositor that shows it; ours never looks. A
    /// window manager can stop that (`_XWAYLAND_ALLOW_COMMITS` = 0, as mutter
    /// does while it syncs a window), and Xwayland takes whoever selects
    /// SubstructureRedirect on the root for the window manager: so become one
    /// for as long as it takes to set the property, then stop. Present still
    /// paces its clients (at 60 Hz, by timer, when no frame is pending).
    /// `XWLBRIDGE_XWAYLAND_COMMITS=1` keeps the commits (for measurements).
    fn stop_xwayland_commits(conn: &RustConnection, root: Window) {
        if std::env::var_os("XWLBRIDGE_XWAYLAND_COMMITS").is_some_and(|v| v == "1") {
            return;
        }
        use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, PropMode};
        let result = (|| -> Result<(), AnyError> {
            let atom = conn
                .intern_atom(false, b"_XWAYLAND_ALLOW_COMMITS")?
                .reply()?
                .atom;
            // an event mask replaces this client's whole selection on the
            // root: keep the agent's own (`root_events`: its focus events are
            // what input attribution judges keys by) around the redirect, and put it back
            conn.change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new()
                    .event_mask(root_events() | EventMask::SUBSTRUCTURE_REDIRECT),
            )?
            .check()?;
            let set = conn
                .change_property32(PropMode::REPLACE, root, atom, AtomEnum::CARDINAL, &[0])?
                .check();
            conn.change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(root_events()),
            )?
            .check()?;
            set?;
            Ok(())
        })();
        match result {
            Ok(()) => crate::vlog!("xwayland: commits off (the root window is never shown)"),
            Err(e) => crate::warning!(
                "xwayland: can't stop Xwayland's commits ({e}); it copies every frame for nothing"
            ),
        }
    }

    /// -showwindows / -reverseinput: the reverse master device, an active
    /// grab of the core pointer (per device, unlike the shield, so the reverse
    /// pointer still reaches shown windows, or a proxy's, while the desktop's
    /// own pointer, driven by the display's clients, reaches none), and the
    /// core keyboard focused on an input-only window of ours, so it types
    /// into no X window either.
    fn setup_reverse(
        conn: &RustConnection,
        root: Window,
    ) -> Result<(reverse::Devices, Window), AnyError> {
        let devs = reverse::create_master(conn)?;
        let kfocus = conn.generate_id()?;
        conn.create_window(
            0,
            kfocus,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            // its focus events tell input attribution that the focus is ours
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::FOCUS_CHANGE),
        )?;
        conn.map_window(kfocus)?;
        Self::grab_core(conn, root, kfocus)?;
        Ok((devs, kfocus))
    }

    fn grab_core(conn: &RustConnection, root: Window, kfocus: Window) -> Result<(), AnyError> {
        // An XInput2 grab, with the raw pointer events in its mask: Xorg gives
        // a grabbing client the grabbed device's raw events only through the
        // grab (a core grab can't carry them, and they'd be lost). XI_Motion
        // stands in for the shield's MotionNotify.
        let mask = xinput::XIEventMask::MOTION
            | xinput::XIEventMask::RAW_BUTTON_PRESS
            | xinput::XIEventMask::RAW_BUTTON_RELEASE
            | xinput::XIEventMask::RAW_MOTION;
        let status = conn
            .xinput_xi_grab_device(
                root,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                CORE_POINTER,
                x11rb::protocol::xproto::GrabMode::ASYNC,
                x11rb::protocol::xproto::GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[u32::from(mask)],
            )?
            .reply()?
            .status;
        if status != x11rb::protocol::xproto::GrabStatus::SUCCESS {
            return Err(format!("can't grab the core pointer: {status:?}").into());
        }
        conn.set_input_focus(
            x11rb::protocol::xproto::InputFocus::PARENT,
            kfocus,
            x11rb::CURRENT_TIME,
        )?;
        Ok(())
    }

    /// Keeps the core pointer grabbed and the core keyboard on our window
    /// (a client may have changed its focus).
    fn keep_reverse(&mut self) -> Result<(), AnyError> {
        let Some(kfocus) = self.kfocus else {
            return Ok(());
        };
        let focus = self.conn.get_input_focus()?.reply()?.focus;
        if focus != kfocus {
            crate::vlog!("-showwindows: core keyboard focus was {focus:#x}; taking it back");
            if let Err(e) = Self::grab_core(&self.conn, self.root, kfocus) {
                crate::vlog!("-showwindows: {e}");
            }
        }
        Ok(())
    }

    /// The fallback: a full-screen override-redirect window, kept on top.
    fn create_mirror(
        conn: &RustConnection,
        s: &x11rb::protocol::xproto::Screen,
        width: u16,
        height: u16,
    ) -> Result<Window, AnyError> {
        let root = s.root;
        let mirror = conn.generate_id()?;
        conn.create_window(
            s.root_depth,
            mirror,
            root,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            s.root_visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(s.black_pixel)
                // the pointer's position, beside raw motion; crossing, to see
                // the pointer grabbed (see `attrib`)
                .event_mask(
                    EventMask::POINTER_MOTION | EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW,
                ),
        )?;
        conn.map_window(mirror)?;
        // hear about other windows being mapped or restacked, to stay on top,
        // and about the keyboard being grabbed
        conn.change_window_attributes(
            root,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new().event_mask(root_events()),
        )?;
        Ok(mirror)
    }

    fn run(&mut self) -> Result<(), AnyError> {
        let xfd: RawFd = self.conn.stream().as_raw_fd();
        let mut last_house = Instant::now() - HOUSEKEEPING;
        if let Err(e) = randr::query_version(&self.conn, 1, 5)?.reply() {
            crate::warning!("xvfb: no RandR 1.5 ({e}); the screen won't follow the desktop");
            self.warned_fixed = true;
        } else {
            self.randr15 = true;
            // to notice another client resizing the screen or turning its
            // output off (every RandR change is reported as a screen change)
            randr::select_input(&self.conn, self.root, randr::NotifyMask::SCREEN_CHANGE)?;
        }
        self.sync_screen()?;
        self.sync_cursor()?;
        self.read_mapping();
        // the desktop as it is now (possibly blank until the first capture)
        self.push(&[(0, 0, self.width, self.height)])?;
        loop {
            if last_house.elapsed() >= HOUSEKEEPING {
                self.housekeeping()?;
                last_house = Instant::now();
            }
            while let Some(ev) = self.conn.poll_for_event()? {
                self.handle(ev);
            }
            if self.dynres_out.is_none()
                && let Some(next) = self.dynres_next
                && Instant::now() >= next.due
            {
                self.dynres_next = None;
                if let Err(e) = self.dynres_ask(next) {
                    crate::warning!("xvfb: can't hand the resize to the desktop: {e}");
                }
            }
            if self.dynres_out.is_none()
                && self.dynres_next.is_none()
                && self.settle_at.is_some_and(|t| Instant::now() >= t)
            {
                self.settle_at = None;
                if let Err(e) = self.settle() {
                    crate::warning!("xvfb: can't put the screen back: {e}");
                }
            }
            self.flush_motion();
            if self.recording.load(Ordering::Acquire) {
                self.pairing.settle(&self.motions);
            }
            self.conn.flush()?;
            let mut pfds = [
                libc::pollfd {
                    fd: xfd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let mut timeout = HOUSEKEEPING;
            if let Some(due) = record::lock(&self.holds).next_due() {
                timeout = timeout.min(due.saturating_duration_since(Instant::now()));
            }
            if self.dynres_out.is_none() {
                let due = match self.dynres_next {
                    Some(next) => Some(next.due),
                    None => self.settle_at,
                };
                if let Some(t) = due {
                    timeout = timeout.min(t.saturating_duration_since(Instant::now()));
                }
            }
            // (rounded up: a lingering hold is due on the millisecond)
            let ms = timeout.as_micros().div_ceil(1000) as i32;
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, ms) };
            if n < 0 {
                continue;
            }
            self.release_departed()?;
            if pfds[1].revents != 0 {
                let mut v: u64 = 0;
                unsafe { libc::read(self.wake.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
                self.update_watching()?;
                self.dynres_answers()?;
                if let Some(c) = self.clip.as_mut() {
                    c.poll(&self.conn, &self.server)?;
                }
                self.sync_cursor()?;
                let rects: Vec<(i16, i16, u16, u16)> = self
                    .server
                    .damage
                    .take_external()
                    .into_iter()
                    .map(|r| (r.x, r.y, r.width, r.height))
                    .collect();
                if !rects.is_empty() {
                    self.push(&rects)?;
                }
            }
        }
    }

    /// Input Xvfb's clients injected, forwarded to the compositor. Motion is
    /// coalesced and sent as the pointer's absolute position.
    fn handle(&mut self, ev: Event) {
        if self.debug {
            crate::warning!("xvfb: event {ev:?}");
        }
        if let Some(c) = self.clip.as_mut() {
            match c.handle(&self.conn, &self.server, &ev) {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => crate::warning!("xvfb: clipboard: {e}"),
            }
        }
        if let Some(devs) = &self.reverse {
            let from = match &ev {
                Event::XinputRawKeyPress(e) | Event::XinputRawKeyRelease(e) => Some(e.deviceid),
                Event::XinputRawButtonPress(e)
                | Event::XinputRawButtonRelease(e)
                | Event::XinputRawMotion(e) => Some(e.deviceid),
                _ => None,
            };
            if let Some(d) = from
                && devs.owns(d)
                && std::env::var_os("XWLBRIDGE_REVERSE_ECHO").is_none_or(|v| v.is_empty())
            {
                // the desktop's own input on a window here: never back to it
                self.reverse_ignored += 1;
                if !matches!(ev, Event::XinputRawMotion(_))
                    && (self.reverse_ignored < 4 || self.reverse_ignored.is_multiple_of(500))
                {
                    crate::vlog!(
                        "xvfb: not forwarding the reverse master's raw events ({} so far)",
                        self.reverse_ignored
                    );
                }
                return;
            }
        }
        match ev {
            Event::XinputRawKeyPress(e) => {
                let sig = raw_sig(KEY_PRESS, e.detail, e.time, &[], &[], &[]);
                if self.forward(e.sourceid, sig) {
                    self.key(true, e.detail as u8)
                }
            }
            // releases always go through: the input layer drops a release of
            // a key the desktop never saw pressed
            Event::XinputRawKeyRelease(e) => self.key(false, e.detail as u8),
            // a client remapped keys (a keysym onto a spare keycode, usually,
            // right before pressing it), or we uploaded a keymap
            Event::MappingNotify(e) if e.request == x11rb::protocol::xproto::Mapping::KEYBOARD => {
                self.read_mapping()
            }
            Event::XinputRawButtonPress(e) => {
                let sig = raw_sig(BUTTON_PRESS, e.detail, e.time, &[], &[], &[]);
                if !self.forward(e.sourceid, sig) {
                    return;
                }
                // A client that finds the pointer already where it wants to
                // click sends no motion (RealVNC does this), and the desktop
                // pointer may have moved since: always say where this is.
                self.pos_dirty = true;
                self.flush_motion();
                self.server
                    .input
                    .fake_input(AGENT_CLIENT, BUTTON_PRESS, e.detail as u8, 0, 0)
            }
            Event::XinputRawButtonRelease(e) => {
                self.flush_motion();
                self.server
                    .input
                    .fake_input(AGENT_CLIENT, BUTTON_RELEASE, e.detail as u8, 0, 0)
            }
            // XTest's absolute moves carry the position (screen coordinates)
            // as the first two valuators, its relative moves the distance;
            // the core motion that follows on the shield says which it was,
            // and else RECORD does (see motion.rs)
            Event::XinputRawMotion(e) => {
                // the one before had no core event either
                self.resolve_pending();
                let (x, y) = first_two_valuators(&e.valuator_mask, &e.axisvalues);
                let (raw_x, raw_y) = first_two_valuators(&e.valuator_mask, &e.axisvalues_raw);
                let sig = raw_sig(
                    MOTION_NOTIFY,
                    0,
                    e.time,
                    &e.valuator_mask,
                    &e.axisvalues,
                    &e.axisvalues_raw,
                );
                let forward = self.forward(e.sourceid, sig);
                let id = self
                    .recording
                    .load(Ordering::Acquire)
                    .then(|| self.pairing.saw(raw_x.unwrap_or(0), raw_y.unwrap_or(0)));
                self.pending = Some(PendingMotion {
                    id,
                    x,
                    y,
                    raw_x,
                    raw_y,
                    time: e.time,
                    forward,
                });
                self.pos_dirty |= forward;
            }
            // -showwindows / -reverseinput: the core pointer's motion, through
            // our grab (the shield's MotionNotify, which is down)
            Event::XinputMotion(e) if self.reverse.is_some() && e.deviceid == CORE_POINTER => {
                let forward = match self.pending.take() {
                    Some(p) if p.time == e.time => p.forward,
                    _ => true,
                };
                self.pos = Some(((e.root_x >> 16) as i16, (e.root_y >> 16) as i16));
                self.pos_dirty |= forward;
            }
            Event::MotionNotify(e) => {
                // where the server put the pointer: settles the raw motion,
                // and goes where it went (same server time); otherwise a
                // client warped the pointer (no raw event), which goes to the
                // desktop as it always did
                let forward = match self.pending.take() {
                    Some(p) if p.time == e.time => p.forward,
                    _ => true,
                };
                self.pos = Some((e.root_x, e.root_y));
                self.pos_dirty |= forward;
            }
            // another client grabbed the pointer: crossing events take it away
            // from the window on top (ours) and give it back on release
            Event::LeaveNotify(e) if e.event == self.top && e.mode == NotifyMode::GRAB => {
                self.target.pointer_grabbed = true;
            }
            Event::EnterNotify(e) if e.event == self.top && e.mode == NotifyMode::UNGRAB => {
                self.target.pointer_grabbed = false;
            }
            // likewise the keyboard: focus leaves the root (or the pointer's
            // window under PointerRoot) while it is grabbed
            Event::FocusOut(e) if e.event == self.root && e.mode == NotifyMode::GRAB => {
                self.target.keyboard_grabbed = true;
            }
            Event::FocusIn(e) if e.event == self.root && e.mode == NotifyMode::UNGRAB => {
                self.target.keyboard_grabbed = false;
            }
            // and where the focus is (see Target::focus_event)
            Event::FocusIn(e) if e.event == self.root => self.target.focus_event(true, e.detail),
            Event::FocusOut(e) if e.event == self.root => self.target.focus_event(false, e.detail),
            // -showwindows / -reverseinput: whether it is on our keyboard window
            Event::FocusIn(e) if Some(e.event) == self.kfocus => {
                self.target.own_focus_event(true, e.mode, e.detail)
            }
            Event::FocusOut(e) if Some(e.event) == self.kfocus => {
                self.target.own_focus_event(false, e.mode, e.detail)
            }
            Event::XkbBellNotify(e) => {
                if bell_sounds(&e) {
                    crate::bridge::bell::ring();
                }
            }
            Event::XinputHierarchy(_) => match xtest_devices(&self.conn) {
                Ok(d) => {
                    if d != self.target.xtest_devices {
                        crate::vlog!("xvfb: XTest devices now {d:?}");
                    }
                    self.target.xtest_devices = d;
                }
                Err(e) => crate::warning!("xvfb: can't list input devices: {e}"),
            },
            Event::RandrScreenChangeNotify(_) => {
                if let Err(e) = self.screen_changed() {
                    crate::warning!("xvfb: following a client's resize: {e}");
                }
            }
            // someone else's window came up or moved: go back on top
            // asked of us only in the moment we were Xwayland's window
            // manager (see stop_xwayland_commits): do as asked
            Event::MapRequest(e) => {
                let _ = self.conn.map_window(e.window);
            }
            Event::ConfigureRequest(e) => {
                let _ = self
                    .conn
                    .configure_window(e.window, &ConfigureWindowAux::from_configure_request(&e));
            }
            Event::MapNotify(e) if e.window != self.top => self.raise(),
            Event::ConfigureNotify(e) if e.window != self.top => self.raise(),
            _ => {}
        }
    }

    /// Whether the injected event `sig` (from device `source`) goes to the
    /// desktop: see [`attrib`].
    fn forward(&mut self, source: u16, sig: attrib::Sig) -> bool {
        if !self.target.xtest_devices.contains(&source) {
            return true; // not injected through XTest: nothing to pair it with
        }
        let t0 = Instant::now();
        let (fake, marked) = self.attribute(&sig);
        self.target.stats.waited(t0.elapsed(), marked);
        let Some(fake) = fake else {
            self.target.unattributed += 1;
            if self.target.unattributed == 1 || self.target.unattributed.is_multiple_of(1000) {
                crate::vlog!(
                    "xvfb: {} injected event(s) not paired with a client's request; forwarded",
                    self.target.unattributed
                );
            }
            return true;
        };
        use attrib::Class;
        let why = match fake.who.class {
            Class::Agent | Class::Driver => return true,
            Class::Helper => {
                "it drives windows on this display (an engine helper, -xtesthelpers)".to_string()
            }
            Class::Other => {
                let t0 = Instant::now();
                let bound = self.engine_bound(sig.type_);
                self.target.stats.judged(t0.elapsed());
                match bound {
                    Some(why) => why,
                    None => return true,
                }
            }
        };
        // once per client (the program and pid tell reused ids apart) and reason
        if self.target.dropped.len() >= 1024 {
            self.target.dropped.clear();
        }
        if self
            .target
            .dropped
            .insert((fake.who.base, fake.who.name.clone(), why.clone()))
        {
            crate::log!(
                "xvfb: input from X client {:#x} ({}) stays on this display: {why}",
                fake.who.base,
                fake.who.name
            );
        }
        false
    }

    /// The request that made `sig`: if the RECORD thread hasn't reported it
    /// yet, send a marker behind it and wait for that to come back.
    /// Also says whether it needed the marker.
    fn attribute(&self, sig: &attrib::Sig) -> (Option<attrib::Fake>, bool) {
        if let Some(f) = self.ledger.take(sig) {
            return (Some(f), false);
        }
        if !self.ledger.is_open() {
            return (None, false);
        }
        let n = self.ledger.markers() + 1;
        // The X server sends RECORD's buffered data only along with other
        // output (it flushes everything when some client has output
        // pending), and NoOperation has no reply: the round trip after it is
        // what makes the marker go out now rather than with whatever output
        // comes next (measured: up to ~40 ms later without it).
        if self.conn.no_operation().is_err() || self.conn.sync().is_err() {
            return (None, false);
        }
        self.ledger.wait_markers(n, ATTRIB_WAIT);
        (self.ledger.take(sig), true)
    }

    /// Whether the X server sends an event of `type_` to another client's
    /// window on this display rather than the desktop's (the root or ours),
    /// and why.
    fn engine_bound(&self, type_: u8) -> Option<String> {
        let t = &self.target;
        match type_ {
            KEY_PRESS => {
                if t.keyboard_grabbed {
                    return Some("another client has the keyboard grabbed".into());
                }
                // None, PointerRoot or the root: the desktop; with
                // -showwindows / -reverseinput, our own keyboard window too
                (t.focus_inferior && !t.focus_ours)
                    .then(|| "the X focus is on another client's window".into())
            }
            // -showwindows / -reverseinput: the core pointer is ours
            // (grabbed), so its clicks and moves reach no other client's
            // window
            BUTTON_PRESS | MOTION_NOTIFY if self.reverse.is_some() => None,
            BUTTON_PRESS => {
                if t.pointer_grabbed {
                    return Some("another client has the pointer grabbed".into());
                }
                // our window is kept on top; another one can be above it only
                // until we restack
                let child = self.conn.query_pointer(self.root).ok()?.reply().ok()?.child;
                (child != x11rb::NONE && child != self.top && !t.ours(child))
                    .then(|| format!("the pointer is over window {child:#x}, another client's"))
            }
            MOTION_NOTIFY => t
                .pointer_grabbed
                .then(|| "another client has the pointer grabbed".into()),
            _ => None,
        }
    }

    /// Gives the X server the desktop's cursor sprite if it changed.
    fn sync_cursor(&mut self) -> Result<(), AnyError> {
        match self.cursor.as_mut() {
            Some(c) => c.sync(&self.conn, &self.server),
            None => Ok(()),
        }
    }

    fn raise(&self) {
        if self.reverse.is_some() {
            return; // no shield up: the core pointer is grabbed instead
        }
        let _ = self.conn.configure_window(
            self.top,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        );
    }

    /// A raw motion went without its core event (another client holds a
    /// pointer grab, or the core event is still to come): works out where it
    /// put the pointer from what kind of move it was.
    fn resolve_pending(&mut self) {
        let Some(m) = self.pending.take() else { return };
        let recording = &self.recording;
        let relative = m.id.and_then(|id| {
            self.pairing
                .kind(&self.motions, id, Instant::now() + MOTION_KIND_WAIT, || {
                    recording.load(Ordering::Acquire)
                })
        });
        let clamp = |v: i32, len: u16| v.clamp(0, i32::from(len.max(1)) - 1) as i16;
        self.pos = match (relative, self.pos) {
            (Some(true), Some((px, py))) => Some((
                clamp(i32::from(px) + m.raw_x.unwrap_or(0), self.width),
                clamp(i32::from(py) + m.raw_y.unwrap_or(0), self.height),
            )),
            // nothing to add it to: where the pointer is now is all there is
            (Some(true), None) => None,
            // absolute, or unknown: the valuators are the position (unknown is
            // nearly always absolute: VNC servers move the pointer that way)
            (_, base) => {
                if relative.is_none() && m.id.is_some() {
                    crate::vlog!("xvfb: a pointer move of unknown kind; taken as absolute");
                }
                let (bx, by) = base.unwrap_or((0, 0));
                Some((
                    m.x.map_or(bx, |v| clamp(v, self.width)),
                    m.y.map_or(by, |v| clamp(v, self.height)),
                ))
            }
        };
        self.pos_dirty |= m.forward;
    }

    /// Tells the desktop where the pointer went, before a button and after
    /// each batch of events (so motion is coalesced).
    fn flush_motion(&mut self) {
        self.resolve_pending();
        if !std::mem::take(&mut self.pos_dirty) {
            return;
        }
        if self.pos.is_none() {
            // nothing seen yet: where it is now is all there is
            self.pos = self
                .conn
                .query_pointer(self.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|p| (p.root_x, p.root_y));
        }
        if let Some((x, y)) = self.pos {
            if self.debug {
                crate::warning!("xvfb: pointer to ({x}, {y})");
            }
            self.server
                .input
                .fake_input(AGENT_CLIENT, MOTION_NOTIFY, 0, x, y);
        }
    }

    /// Copies damaged rects of the captured desktop onto Xvfb's root window.
    fn push(&mut self, rects: &[(i16, i16, u16, u16)]) -> Result<(), AnyError> {
        let mut offset = 0usize;
        for &(x, y, w, h) in rects {
            // clip to Xvfb's screen
            let x0 = (x as i32).clamp(0, self.width as i32);
            let y0 = (y as i32).clamp(0, self.height as i32);
            let x1 = (x as i32 + w as i32).clamp(0, self.width as i32);
            let y1 = (y as i32 + h as i32).clamp(0, self.height as i32);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
            let len = w as usize * h as usize * 4;
            if offset + len > self.shm.len {
                // the server must be done reading before we overwrite
                self.conn.sync()?;
                offset = 0;
            }
            let dst = unsafe { std::slice::from_raw_parts_mut(self.shm.ptr.add(offset), len) };
            self.server.framebuffer.copy_rect_into(x0, y0, w, h, dst);
            shm::put_image(
                &self.conn,
                self.mirror,
                self.gc,
                w as u16,
                h as u16,
                0,
                0,
                w as u16,
                h as u16,
                x0 as i16,
                y0 as i16,
                24,
                ImageFormat::Z_PIXMAP.into(),
                false,
                self.shm.seg,
                offset as u32,
            )?;
            offset += len;
        }
        // every put has been read before the next batch reuses the segment
        self.conn.sync()?;
        Ok(())
    }

    /// Follows the desktop's size and outputs: the screen is resized (Xvnc)
    /// and RandR monitors carry the desktop's output names and geometry, so X
    /// tools see "DP-1" where the desktop has it.
    fn sync_screen(&mut self) -> Result<(), AnyError> {
        let (w, h, hz, monitors, primary) = {
            let s = self.server.screen.lock().unwrap();
            // the desktop's primary output (with GNOME, the one of its top
            // bar), wherever it is in the list: RandR's primary monitor
            let primary = s.primary_output();
            let m: Vec<Monitor> = s
                .outputs
                .iter()
                .map(|o| Monitor {
                    name: o.name.clone(),
                    x: o.x,
                    y: o.y,
                    width: o.width,
                    height: o.height,
                    // only an attached output can be the primary monitor's
                    primary: o.connected && o.mode != 0 && o.output_id == primary,
                })
                .collect();
            (s.width, s.height, s.primary_refresh_mhz(), m, primary)
        };
        let primary = primary_to_set(self.primary, primary);
        if primary != 0 {
            self.primary = primary;
        }
        // while a client's resize is on its way to the desktop (or another
        // client is still in the middle of one), the desktop's size is the
        // old one: following it would undo the client's resize
        let busy =
            self.dynres_out.is_some() || self.dynres_next.is_some() || self.settle_at.is_some();
        // Xvnc's one output states the primary monitor's refresh (X clients
        // pacing themselves read it), so a refresh change alone re-modes it
        let refresh_changed = self.resizable
            && self.resizer.is_none()
            && hz != 0
            && remode_for_refresh(self.refresh_mhz, hz, w, h);
        if ((w, h) != (self.width, self.height) || refresh_changed)
            && w > 0
            && h > 0
            && !busy
            && self.refused != Some((w, h))
        {
            if self.resizable {
                self.resize(w, h, hz)?;
                if hz != 0 {
                    self.refresh_mhz = hz;
                }
            } else if !self.warned_fixed {
                crate::warning!(
                    "xvfb: the desktop is now {w}x{h} but Xvfb stays {}x{}; use -engine xvnc to follow it",
                    self.width,
                    self.height
                );
                self.warned_fixed = true;
            }
        }
        // the primary is part of what makes a change (a Settings change of
        // the primary alone changes nothing else), and so it is of `Monitor`
        if monitors != self.monitors && self.randr15 {
            if let Err(e) = self.set_monitors(&monitors) {
                crate::vlog!("xvfb: can't name RandR monitors: {e}");
            }
            self.monitors = monitors;
        }
        Ok(())
    }

    /// Sets the screen to `w`x`h`: by having Xwayland's compositor configure
    /// it, or through RandR (Xvnc).
    fn resize(&mut self, w: u16, h: u16, refresh_mhz: u32) -> Result<(), AnyError> {
        if let Some(r) = self.resizer.clone() {
            return match self.resize_by_host(&r, w, h)? {
                Some((w, h)) => {
                    crate::log!("xvfb: screen resized to {w}x{h}");
                    self.adopt_size(w, h)
                }
                None => Ok(()),
            };
        }
        self.resize_randr(w, h, refresh_mhz)
    }

    /// Asks Xwayland's compositor for `w`x`h` and waits (up to 3 s) for the
    /// root window to take it. The size it took, or `None` if it stayed.
    fn resize_by_host(
        &mut self,
        r: &wlhost::Resizer,
        w: u16,
        h: u16,
    ) -> Result<Option<(u16, u16)>, AnyError> {
        r.resize(w, h);
        let start = Instant::now();
        loop {
            let g = self.conn.get_geometry(self.root)?.reply()?;
            if (g.width, g.height) == (w, h) {
                self.refused = None;
                return Ok(Some((w, h)));
            }
            if !r.connected() {
                return Err("Xwayland left its compositor".into());
            }
            if start.elapsed() > Duration::from_secs(3) {
                // Xwayland clamps (320x200 .. 32767x32767): take what it did
                let (aw, ah) = r.acked();
                crate::warning!(
                    "xwayland: asked the X screen for {w}x{h} (Xwayland acknowledged {aw}x{ah}), it stays {}x{}",
                    g.width,
                    g.height
                );
                self.refused = Some((w, h));
                if (g.width, g.height) != (self.width, self.height) {
                    return Ok(Some((g.width, g.height)));
                }
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Sets the screen (and its one output) to `w`x`h` at `refresh_mhz` (0:
    /// any) through RandR, creating the mode. A listed mode of that size
    /// within half a percent (or 0.5 Hz) of the refresh X can state for it is
    /// reused (Xvnc's own ~60 Hz modes for a 60 Hz desktop).
    fn resize_randr(&mut self, w: u16, h: u16, refresh_mhz: u32) -> Result<(), AnyError> {
        let res = randr::get_screen_resources_current(&self.conn, self.root)?.reply()?;
        let (Some(&output), Some(&crtc)) = (res.outputs.first(), res.crtcs.first()) else {
            return Err("the X server has no RandR output".into());
        };
        let info = randr::get_output_info(&self.conn, output, res.config_timestamp)?.reply()?;
        let want_clock = crate::bridge::x11::randr::dot_clock(w, h, refresh_mhz);
        let want_mhz = (u64::from(want_clock) * 1000 / (u64::from(w) * u64::from(h)).max(1)) as u32;
        let fits = |m: &randr::ModeInfo| {
            refresh_mhz == 0 || same_refresh(crate::bridge::x11::randr::refresh_mhz(m), want_mhz)
        };
        let mode = match res
            .modes
            .iter()
            .find(|m| m.width == w && m.height == h && info.modes.contains(&m.id) && fits(m))
        {
            Some(m) => m.id,
            None => {
                let name = if refresh_mhz == 0 {
                    format!("{w}x{h}")
                } else {
                    format!("{w}x{h}_{}.{:02}", want_mhz / 1000, want_mhz % 1000 / 10)
                };
                let mi = randr::ModeInfo {
                    id: 0,
                    width: w,
                    height: h,
                    dot_clock: want_clock,
                    hsync_start: w,
                    hsync_end: w,
                    htotal: w,
                    hskew: 0,
                    vsync_start: h,
                    vsync_end: h,
                    vtotal: h,
                    name_len: name.len() as u16,
                    mode_flags: 0u32.into(),
                };
                let id = randr::create_mode(&self.conn, self.root, mi, name.as_bytes())?
                    .reply()?
                    .mode;
                randr::add_output_mode(&self.conn, output, id)?;
                id
            }
        };
        // The screen has to hold the CRTC at every step. Growing: the screen
        // first; shrinking: the CRTC first. When one side grows and the other
        // shrinks (a rotation: 1920x1080 -> 1080x1920), switch the CRTC off,
        // size the screen, switch it on, as xrandr and RealVNC do -- growing to
        // the union first showed X clients a 1920x1920 screen the desktop never
        // had, and x11vnc -xrandr once crashed on it (display matrix,
        // live_rotate_90).
        let set_crtc = |mode: u32, outputs: &[u32]| -> Result<(), AnyError> {
            randr::set_crtc_config(
                &self.conn,
                crtc,
                res.timestamp,
                res.config_timestamp,
                0,
                0,
                mode,
                randr::Rotation::ROTATE0,
                outputs,
            )?
            .reply()?;
            Ok(())
        };
        if (w > self.width && h < self.height) || (w < self.width && h > self.height) {
            set_crtc(0, &[])?;
            randr::set_screen_size(&self.conn, self.root, w, h, mm(w), mm(h))?;
            set_crtc(mode, &[output])?;
        } else {
            let (gw, gh) = (self.width.max(w), self.height.max(h));
            if (gw, gh) != (self.width, self.height) {
                randr::set_screen_size(&self.conn, self.root, gw, gh, mm(gw), mm(gh))?;
            }
            set_crtc(mode, &[output])?;
            if (gw, gh) != (w, h) {
                randr::set_screen_size(&self.conn, self.root, w, h, mm(w), mm(h))?;
            }
        }
        crate::log!("xvfb: screen resized to {w}x{h}");
        self.adopt_size(w, h)
    }

    /// The screen is `w`x`h` now (we or another client set it): size the
    /// shield and the shm segment for it, and draw the desktop on it.
    fn adopt_size(&mut self, w: u16, h: u16) -> Result<(), AnyError> {
        self.conn.configure_window(
            self.top,
            &ConfigureWindowAux::new()
                .width(u32::from(w))
                .height(u32::from(h)),
        )?;
        let len = w as usize * h as usize * 4;
        if len > self.shm.len {
            shm::detach(&self.conn, self.shm.seg)?;
            self.conn.sync()?;
            // the old mapping goes once the new one is in place
            self.shm = Shm::attach(&self.conn, len)?;
        }
        self.conn.sync()?;
        (self.width, self.height) = (w, h);
        self.push(&[(0, 0, w, h)])
    }

    /// A RandR change: ours (already applied by the time its events are
    /// read, so the size matches), or another client's. A resize goes to the
    /// desktop with `-dynres` (Xvnc); whatever the desktop can't follow, and
    /// an output left off, is put right once the client is done
    /// ([`settle`](Self::settle)).
    fn screen_changed(&mut self) -> Result<(), AnyError> {
        let g = self.conn.get_geometry(self.root)?.reply()?;
        let (w, h) = (g.width, g.height);
        if (w, h) == (self.width, self.height) {
            // the size is ours, but a client may have turned the output off
            // (RealVNC does, first thing in a resize)
            if !self.output_on()? {
                self.settle_at = Some(Instant::now() + SETTLE);
            }
            return Ok(());
        }
        crate::log!("xvfb: an X client resized the screen to {w}x{h}");
        self.adopt_size(w, h)?;
        self.settle_at = Some(Instant::now() + SETTLE);
        if !self.resizable {
            crate::warning!(
                "xvfb: Xvfb's screen size is fixed ({}x{}); putting it back (use -engine xvnc to let clients resize)",
                self.fixed.0,
                self.fixed.1
            );
            return Ok(());
        }
        if let Err(why) = self.dynres_target() {
            crate::warning!("xvfb: the desktop can't follow: {why}; putting the screen back");
            return Ok(());
        }
        // asked for once it stands (and the previous one is answered); a
        // newer size replaces it
        let now = Instant::now();
        self.dynres_next = Some(DynresNext {
            size: (w, h),
            since: now,
            due: now + DEBOUNCE,
        });
        Ok(())
    }

    /// Hands a client's screen size to the desktop, unless it has it already
    /// (then the screen just settles).
    fn dynres_ask(&mut self, next: DynresNext) -> Result<(), AnyError> {
        let (w, h) = next.size;
        if (w, h) == self.desktop_size() {
            self.settle_at = Some(Instant::now());
            return Ok(());
        }
        if !self.output_on()? {
            // the client may not have turned its output on yet (RealVNC sets
            // the screen size first); if it never does, this is no size
            // anyone asked to see (xrandr --off shrinks the screen to its
            // minimum), and the screen settles with its output on
            if next.since.elapsed() < SETTLE {
                self.dynres_next = Some(DynresNext {
                    due: Instant::now() + DEBOUNCE,
                    ..next
                });
            } else {
                self.settle_at = Some(Instant::now());
            }
            return Ok(());
        }
        match self.dynres_target() {
            Ok(key) => self.dynres_request(key, w, h),
            Err(why) => {
                crate::warning!("xvfb: the desktop can't follow: {why}");
                self.settle_at = Some(Instant::now());
                Ok(())
            }
        }
    }

    /// The desktop's size as the backend last reported it.
    fn desktop_size(&self) -> (u16, u16) {
        let s = self.server.screen.lock().unwrap();
        (s.width, s.height)
    }

    /// The desktop monitor a screen size can be carried to, or why none.
    fn dynres_target(&self) -> Result<u32, String> {
        if !self.server.config.dynres {
            return Err("dynamic resolution is off (see -dynres)".into());
        }
        let keys: Vec<u32> = {
            let s = self.server.screen.lock().unwrap();
            s.outputs
                .iter()
                .filter(|o| o.is_wayland())
                .map(|o| o.wl_name)
                .collect()
        };
        if keys.len() != 1 {
            return Err(format!(
                "the desktop has {} monitors, and a screen size names no one of them",
                keys.len()
            ));
        }
        if !self.server.dynres.available() {
            return Err("the compositor offers no way to change it".into());
        }
        Ok(keys[0])
    }

    /// Asks the backend for `w`x`h` on a helper thread (it blocks until the
    /// desktop has, or has not, changed; this thread carries the input).
    fn dynres_request(&mut self, key: u32, w: u16, h: u16) -> Result<(), AnyError> {
        let (server, tx) = (self.server.clone(), self.dynres_results.0.clone());
        // its own handle on the wake eventfd, valid however long it takes
        let wake = self.wake.try_clone()?;
        std::thread::Builder::new()
            .name("xvfb-dynres".into())
            .spawn(move || {
                let r = server.dynres.change_mode(key, w, h, DYNRES_TIMEOUT);
                let _ = tx.send(((w, h), r));
                let one: u64 = 1;
                unsafe { libc::write(wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
            })?;
        self.dynres_out = Some((w, h));
        Ok(())
    }

    /// What the desktop made of a client's resize. A newer size set meanwhile
    /// is asked for next (by the loop); otherwise the screen settles, ending
    /// up as the desktop is, whatever came of it.
    fn dynres_answers(&mut self) -> Result<(), AnyError> {
        while let Ok(((w, h), r)) = self.dynres_results.1.try_recv() {
            self.dynres_out = None;
            match r {
                Ok(()) => crate::log!("xvfb: the desktop is now {w}x{h} too"),
                Err(e) => crate::warning!("xvfb: the desktop can't be {w}x{h} ({e})"),
            }
            if self.dynres_next.is_none() {
                self.settle_at = Some(self.settle_at.unwrap_or_else(Instant::now));
            }
        }
        Ok(())
    }

    /// Whether the screen's output is on and shows the whole screen.
    fn output_on(&self) -> Result<bool, AnyError> {
        let res = randr::get_screen_resources_current(&self.conn, self.root)?.reply()?;
        let Some(&crtc) = res.crtcs.first() else {
            return Ok(true); // nothing to turn on
        };
        let c = randr::get_crtc_info(&self.conn, crtc, res.config_timestamp)?.reply()?;
        Ok(c.mode != 0
            && !c.outputs.is_empty()
            && (c.x, c.y, c.width, c.height) == (0, 0, self.width, self.height))
    }

    /// After other clients' RandR changes: the screen goes back to the
    /// desktop's size (Xvfb: its fixed size) unless the desktop took the
    /// client's, and an output left off is turned back on.
    fn settle(&mut self) -> Result<(), AnyError> {
        let target = if self.resizable {
            self.desktop_size()
        } else {
            self.fixed
        };
        if target.0 == 0 || target.1 == 0 {
            return Ok(());
        }
        if (self.width, self.height) != target {
            crate::log!(
                "xvfb: the screen is {}x{} but {} {}x{}; putting it back",
                self.width,
                self.height,
                if self.resizable {
                    "the desktop is"
                } else {
                    "Xvfb's size is"
                },
                target.0,
                target.1
            );
        } else if !self.output_on()? {
            crate::log!("xvfb: an X client left the screen's output off; turning it back on");
        } else {
            return Ok(());
        }
        self.resize(target.0, target.1, self.refresh_mhz)
    }

    /// RandR 1.5 monitors named and placed like the desktop's outputs; the
    /// desktop's primary output is the primary monitor (`xrandr --listmonitors`
    /// puts the `*` there), the first one if it names none.
    fn set_monitors(&mut self, monitors: &[Monitor]) -> Result<(), AnyError> {
        for atom in self.monitor_atoms.drain(..) {
            randr::delete_monitor(&self.conn, self.root, atom)?;
        }
        let res = randr::get_screen_resources_current(&self.conn, self.root)?.reply()?;
        let primary = primary_index(monitors);
        for (i, m) in monitors.iter().enumerate() {
            let Monitor {
                name,
                x,
                y,
                width: w,
                height: h,
                ..
            } = m;
            let atom = self.conn.intern_atom(false, name)?.reply()?.atom;
            randr::set_monitor(
                &self.conn,
                self.root,
                randr::MonitorInfo {
                    name: atom,
                    primary: i == primary,
                    automatic: false,
                    x: *x,
                    y: *y,
                    width: *w,
                    height: *h,
                    width_in_millimeters: mm(*w),
                    height_in_millimeters: mm(*h),
                    // the one output goes with the first monitor, so RandR
                    // doesn't add an automatic one for it beside ours
                    outputs: if i == 0 {
                        res.outputs.first().copied().into_iter().collect()
                    } else {
                        Vec::new()
                    },
                },
            )?;
            self.monitor_atoms.push(atom);
        }
        self.conn.sync()?;
        Ok(())
    }

    fn housekeeping(&mut self) -> Result<(), AnyError> {
        self.target.stats.report();
        self.sync_screen()?;
        self.keep_reverse()?;
        self.sync_cursor()?;
        if let Some(c) = self.clip.as_mut() {
            c.poll(&self.conn, &self.server)?;
        }
        // who is connected, and is anyone watching (holding a DAMAGE object)?
        let clients = res::query_clients(&self.conn)?.reply()?.clients;
        let mut bases = Vec::with_capacity(clients.len());
        let mut watching = false;
        for c in clients {
            if c.resource_base == self.own_base {
                continue;
            }
            bases.push(c.resource_base);
            // -showwindows: the reverse thread's DAMAGE objects watch the shown
            // windows, not the desktop picture (else showing any window would
            // start the whole capture session; games.md finding 2, review F5)
            if reverse::is_reverse_client(c.resource_base) {
                continue;
            }
            if let Ok(r) = res::query_client_resources(&self.conn, c.resource_base)?.reply() {
                if std::env::var_os("XWLBRIDGE_XRES_DEBUG").is_some_and(|v| !v.is_empty()) {
                    let names: Vec<String> = r
                        .types
                        .iter()
                        .map(|t| {
                            let n = self
                                .conn
                                .get_atom_name(t.resource_type)
                                .ok()
                                .and_then(|c| c.reply().ok())
                                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                                .unwrap_or_default();
                            format!("{n}={}", t.count)
                        })
                        .collect();
                    crate::warning!("xvfb: client {:#x}: {}", c.resource_base, names.join(" "));
                }
                watching |= r
                    .types
                    .iter()
                    .any(|t| t.resource_type == self.damage_atom && t.count > 0);
            }
        }
        self.damage_watching = watching;
        self.update_watching()?;
        if bases.iter().any(|b| !self.clients.contains(b)) || bases.len() < self.clients.len() {
            if self.clients.iter().any(|b| !bases.contains(b))
                && !self.recording.load(Ordering::Acquire)
                && !self.server.config.xtestpersist
            {
                self.release_orphaned_input()?;
            }
            self.clients = bases;
        }
        // the compositor keymap changed (or a failed upload is due a retry: a
        // transient failure must not leave the X server on the wrong layout)
        let generation = self.server.keymap_gen.load(Ordering::Acquire);
        let due = self
            .keymap_retry
            .is_none_or(|(g, at, _)| g != generation || Instant::now() >= at);
        if generation != self.keymap_gen && due {
            // loud for a new keymap's first failure, quiet for its retries
            let first = self.keymap_retry.is_none_or(|(g, ..)| g != generation);
            if self.upload_keymap(first) {
                self.keymap_gen = generation;
                self.keymap_retry = None;
                self.read_mapping();
            } else {
                let delay = match self.keymap_retry {
                    Some((g, _, d)) if g == generation => (d * 2).min(KEYMAP_RETRY_MAX),
                    _ => KEYMAP_RETRY,
                };
                self.keymap_retry = Some((generation, Instant::now() + delay, delay));
            }
        }
        Ok(())
    }

    /// Starts or stops capture for our clients: one holds a DAMAGE object, or
    /// read the root's pixels lately. The desktop is copied onto the root
    /// only meanwhile, so once one starts watching the whole of it goes
    /// there (it may have changed unseen, a session being up for input).
    fn update_watching(&mut self) -> Result<(), AnyError> {
        let last_read = self.root_reads.load(Ordering::Relaxed);
        let reading = last_read != 0 && now_ms().saturating_sub(last_read) < READ_GATE_MS;
        let watching = self.damage_watching || reading;
        if watching == self.watching {
            return Ok(());
        }
        crate::vlog!(
            "xvfb: {}",
            match (watching, self.damage_watching) {
                (true, true) => "a client is watching",
                (true, false) => "a client is reading the screen",
                (false, _) => "nobody is watching",
            }
        );
        self.watching = watching;
        self.server.damage.set_external(watching, None);
        if watching {
            self.push(&[(0, 0, self.width, self.height)])?;
        }
        Ok(())
    }

    /// Releases what the RECORD thread found departed clients left held: at
    /// once, or once a one-shot client's lingering holds are due.
    fn release_departed(&mut self) -> Result<(), AnyError> {
        let mut now = Vec::new();
        while let Ok(list) = self.releases.try_recv() {
            now.extend(list);
        }
        let lingered = record::lock(&self.holds).expire(Instant::now());
        for &(type_, detail) in now.iter().chain(&lingered) {
            xtest::fake_input(&self.conn, type_, detail, 0, self.root, 0, 0, 0)?;
        }
        if !now.is_empty() || !lingered.is_empty() {
            self.conn.flush()?;
        }
        if !now.is_empty() {
            crate::log!(
                "released {} key(s)/button(s) a disconnected X client was holding",
                now.len()
            );
        }
        if !lingered.is_empty() {
            crate::log!(
                "released {} key(s)/button(s) a disconnected X client was holding \
                 (a one-shot client's; no X client released them in time)",
                lingered.len()
            );
        }
        Ok(())
    }

    /// Without RECORD: a client went away: Xorg keeps whatever it held down via XTest, which
    /// would stay down on the desktop too. Release it (the releases come back
    /// as raw events and reach the compositor like any other). Who held what
    /// is unknown here, so everything goes, at once (nothing lingers; under
    /// `-xtestpersist` this isn't called for a departure). Also called by a
    /// restarted agent, for what nobody can release any more.
    fn release_orphaned_input(&mut self) -> Result<(), AnyError> {
        let keys = self.conn.query_keymap()?.reply()?.keys;
        let mut released = 0;
        for (i, byte) in keys.iter().enumerate() {
            for bit in 0..8 {
                if byte & (1 << bit) != 0 {
                    let kc = (i * 8 + bit) as u8;
                    xtest::fake_input(&self.conn, KEY_RELEASE, kc, 0, self.root, 0, 0, 0)?;
                    released += 1;
                }
            }
        }
        let mask = u16::from(self.conn.query_pointer(self.root)?.reply()?.mask);
        for (button, bit) in [(1u8, 8u16), (2, 9), (3, 10), (4, 11), (5, 12)] {
            if mask & (1 << bit) != 0 {
                xtest::fake_input(&self.conn, BUTTON_RELEASE, button, 0, self.root, 0, 0, 0)?;
                released += 1;
            }
        }
        if released > 0 {
            crate::log!(
                "xvfb: released {released} key(s)/button(s) held by a client that went away"
            );
        }
        Ok(())
    }

    /// Makes Xvfb's keyboard the compositor's, so keycodes and keysyms agree.
    /// Returns whether it did (or there is nothing to upload); a failure is
    /// logged as a warning when `loud`, else only with -verbose (retries).
    fn upload_keymap(&self, loud: bool) -> bool {
        let Some(text) = self.server.keymap_text.lock().unwrap().clone() else {
            return true;
        };
        // A compositor keymap (libxkbcommon) can't go to an X server verbatim:
        // it has keycodes up to ~700 and features XKB's protocol rejects. It
        // names the xkeyboard-config components it was built from, though, and
        // compiling those from the X server's own data (what setxkbmap does)
        // gives the same keys, X-compatible.
        let group = self.server.keymap_group.load(Ordering::Acquire);
        let keymap = include_keymap(&text, group).unwrap_or_else(|| {
            if group != 0 {
                crate::warning!(
                    "xvfb: the compositor keymap names no components, so its layout {group} \
                     can't be picked out; X clients see its first"
                );
            }
            text.to_string()
        });
        XKBCOMP_RUNNING.store(true, Ordering::Release);
        let ran = run_xkbcomp(&keymap, self.display, &self.auth.file);
        XKBCOMP_RUNNING.store(false, Ordering::Release);
        match ran {
            Ok(()) => {
                crate::log!("xvfb: uploaded the compositor keymap");
                self.publish_rules_names();
                true
            }
            Err(e) => {
                if loud {
                    crate::warning!(
                        "xvfb: can't upload the compositor keymap, so X clients may type \
                         the wrong characters for a non-US layout (retrying): {e}"
                    );
                } else {
                    crate::vlog!("xvfb: keymap upload failed again: {e}");
                }
                false
            }
        }
    }
}

impl Agent {
    /// `_XKB_RULES_NAMES` on the root, as setxkbmap leaves it: xkbcomp loads a
    /// keymap without touching it, so `setxkbmap -query` (and whatever else
    /// reads it) would go on naming the X server's default layout ("us") for
    /// any desktop layout. Left as it is when the layout isn't known.
    fn publish_rules_names(&self) {
        use x11rb::protocol::xproto::{AtomEnum, PropMode};
        let rules = self
            .server
            .keymap
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|t| t.xkb.rules.clone());
        let Some(r) = rules else {
            crate::vlog!(
                "xvfb: the desktop's layout has no known rules names; _XKB_RULES_NAMES left as it is"
            );
            return;
        };
        let result = (|| -> Result<(), AnyError> {
            let atom = self
                .conn
                .intern_atom(
                    false,
                    crate::bridge::x11::xkb::RULES_NAMES_PROPERTY.as_bytes(),
                )?
                .reply()?
                .atom;
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    self.root,
                    atom,
                    AtomEnum::STRING,
                    &r.property(),
                )?
                .check()?;
            Ok(())
        })();
        match result {
            Ok(()) => crate::vlog!(
                "xvfb: _XKB_RULES_NAMES {}, {}, {}, {:?}",
                r.rules,
                r.model,
                r.layout,
                r.variant
            ),
            Err(e) => crate::vlog!("xvfb: can't set _XKB_RULES_NAMES: {e}"),
        }
    }
}

/// Compiles `keymap` into X display `:display` with xkbcomp (connecting with
/// the engine's cookie in `xauthority`), the source fed on stdin: no temp
/// file, so a full or hostile temp dir can't make it fail or block it (another
/// local user could take a predictable name first) and nothing is left behind.
fn run_xkbcomp(keymap: &str, display: u32, xauthority: &Path) -> io::Result<()> {
    use std::process::Stdio;
    let mut child = crate::childenv::pin_x11(&mut Command::new("xkbcomp"))
        .args(["-w", "0", "-"])
        .arg(format!(":{display}"))
        .env("XAUTHORITY", xauthority)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("can't run xkbcomp (install x11-xkb-utils): {e}"),
            )
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let source = keymap.to_owned();
    // a writer thread: xkbcomp may fill its stderr before it has read all of
    // a large keymap, so writing and reading must not wait on each other
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        stdin.write_all(source.as_bytes())
        // stdin drops here: xkbcomp sees the end of the source
    });
    let out = child.wait_with_output()?;
    let wrote = writer
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("keymap writer panicked")));
    if !out.status.success() {
        // (a write error then is just xkbcomp having quit early)
        let why = String::from_utf8_lossy(&out.stderr)
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(io::Error::other(format!(
            "xkbcomp failed ({}): {why}",
            out.status
        )));
    }
    wrote
}

/// An X-compatible keymap that includes the components `text` (a libxkbcommon
/// keymap) was compiled from, with only its layout `group` (from 0), or `None`
/// if it doesn't name them all.
fn include_keymap(text: &str, group: u32) -> Option<String> {
    let name = |section: &str| crate::bridge::xkbdesc::component_name(text, section);
    Some(format!(
        "xkb_keymap {{\n  xkb_keycodes {{ include \"{}\" }};\n  xkb_types {{ include \"{}\" }};\n  \
         xkb_compat {{ include \"{}\" }};\n  xkb_symbols {{ include \"{}\" }};\n}};\n",
        name("xkb_keycodes")?,
        name("xkb_types")?,
        name("xkb_compatibility")?,
        one_layout(&name("xkb_symbols")?, group),
    ))
}

#[cfg(test)]
mod tests {
    use super::{Monitor, primary_index, primary_to_set};

    /// The desktop's monitors, as `sync_screen` reads them: output 0 and 1
    /// side by side, `primary` naming the one GNOME shows its top bar on.
    fn two_heads(primary: u32) -> Vec<Monitor> {
        [(7, 0, "DP-1"), (9, 1280, "HDMI-1")]
            .into_iter()
            .map(|(id, x, name)| Monitor {
                name: name.into(),
                x,
                y: 0,
                width: 1280,
                height: 800,
                primary: id == primary,
            })
            .collect()
    }

    #[test]
    fn the_desktops_primary_is_randrs_wherever_it_is() {
        // p2_fix item 1: the primary is the desktop's (with GNOME, where
        // its top bar is), not the first output
        assert_eq!(primary_index(&two_heads(7)), 0);
        assert_eq!(primary_index(&two_heads(9)), 1);
        // a desktop that names none: the first monitor
        assert_eq!(primary_index(&two_heads(0)), 0);
        assert_eq!(primary_index(&[]), 0);
    }

    #[test]
    fn a_primary_only_change_reaches_randr() {
        // p2_fix E: the change detection compares these monitors; the
        // primary alone is part of them (before, the tuple was
        // (name, x, y, w, h) and a primary-only change was lost)
        let before = two_heads(7);
        let after = two_heads(9);
        assert_ne!(before, after);
        // and only the primary differs: same names and rectangles
        let geometry = |v: &[Monitor]| {
            v.iter()
                .map(|m| (m.name.clone(), m.x, m.y, m.width, m.height))
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(&before), geometry(&after));
        // a primary that does not exist keeps the one RandR has
        assert_eq!(primary_to_set(9, 0), 9);
        assert_eq!(primary_to_set(9, 7), 7);
        // nothing known yet
        assert_eq!(primary_to_set(0, 0), 0);
    }

    #[test]
    fn a_bell_sounds_unless_the_client_asked_for_the_event_only() {
        let bell = |event_only| super::xkb::BellNotifyEvent {
            response_type: 0,
            xkb_type: 8,
            sequence: 0,
            time: 0,
            device_id: 3,
            bell_class: super::xkb::BellClassResult::KBD_FEEDBACK_CLASS,
            bell_id: 0,
            percent: 50,
            pitch: 400,
            duration: 100,
            name: 0,
            window: 0,
            event_only,
        };
        assert!(super::bell_sounds(&bell(false)));
        assert!(!super::bell_sounds(&bell(true)));
    }

    /// Held by the tests that spawn processes while they bind sockets: a child
    /// forked by one test holds the other's listening socket until it execs, and
    /// a connect() in that window is answered, so the socket seemed alive (the
    /// sweep tests failed now and then when they ran in parallel).
    static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_root_selection_keeps_the_focus_events() {
        // -engine xwayland's stop_xwayland_commits selects on the root again;
        // without FocusChange input attribution sends every key to the desktop (echo)
        use x11rb::protocol::xproto::EventMask;
        let m = u32::from(super::root_events());
        assert_ne!(m & u32::from(EventMask::FOCUS_CHANGE), 0);
        assert_ne!(m & u32::from(EventMask::SUBSTRUCTURE_NOTIFY), 0);
    }

    #[test]
    fn the_reverse_keyboard_window_keeps_count_of_its_focus() {
        use super::{NotifyDetail as D, NotifyMode as M, Target};
        let mut t = Target::default();
        // keep_reverse focuses it: FocusIn(Nonlinear), mode Normal
        t.own_focus_event(true, M::NORMAL, D::NONLINEAR);
        assert!(t.focus_ours);
        // a keyboard grab and its end move nothing (keyboard_grabbed does)
        t.own_focus_event(false, M::GRAB, D::NONLINEAR);
        assert!(t.focus_ours);
        t.own_focus_event(true, M::UNGRAB, D::NONLINEAR);
        assert!(t.focus_ours);
        // the pointer's window under PointerRoot is not the focus
        t.own_focus_event(false, M::NORMAL, D::POINTER);
        assert!(t.focus_ours);
        // a client takes the focus to its window: FocusOut(Nonlinear)
        t.own_focus_event(false, M::NORMAL, D::NONLINEAR);
        assert!(!t.focus_ours);
        // and while grabbed
        t.own_focus_event(true, M::WHILE_GRABBED, D::NONLINEAR);
        assert!(t.focus_ours);
    }

    #[test]
    fn the_focus_follows_the_roots_focus_events() {
        use super::{NotifyDetail as D, Target};
        let mut t = Target::default();
        // PointerRoot -> window W below the root: FocusOut(Pointer...),
        // FocusOut(PointerRoot), FocusIn(NonlinearVirtual) on the root
        t.focus_event(false, D::POINTER);
        t.focus_event(false, D::POINTER_ROOT);
        t.focus_event(true, D::NONLINEAR_VIRTUAL);
        assert!(t.focus_inferior);
        // W -> another window below the root: nothing reaches the root
        assert!(t.focus_inferior);
        // W -> PointerRoot: FocusOut(NonlinearVirtual), FocusIn(PointerRoot),
        // FocusIn(Pointer)
        t.focus_event(false, D::NONLINEAR_VIRTUAL);
        t.focus_event(true, D::POINTER_ROOT);
        t.focus_event(true, D::POINTER);
        assert!(!t.focus_inferior);
        // the root itself -> a child: FocusOut(Inferior) only
        t.focus_event(false, D::INFERIOR);
        assert!(t.focus_inferior);
        // a child -> the root: FocusIn(Inferior)
        t.focus_event(true, D::INFERIOR);
        assert!(!t.focus_inferior);
        // a child of a child -> its parent: FocusIn(Virtual) on the root's
        // side... and None
        t.focus_event(true, D::VIRTUAL);
        assert!(t.focus_inferior);
        t.focus_event(false, D::NONLINEAR_VIRTUAL);
        t.focus_event(true, D::NONE);
        assert!(!t.focus_inferior);
    }

    use super::{
        AGENT_RESTART_WINDOW, AGENT_RESTARTS, Auth, Failures, PrivateDir, XVNC_SOCKET,
        include_keymap, mkdtemp, one_layout, remove_stale_private_dirs, remove_stale_rfb_sockets,
        xauth_entry,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    #[test]
    fn authority_file_is_private_and_well_formed() {
        let dir = PrivateDir(mkdtemp(&std::env::temp_dir().join("xwlbridge-test-XXXXXX")).unwrap());
        assert_eq!(
            std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let a = Auth::create(&dir.0).unwrap();
        let b = Auth::create(&mkdtemp(&dir.0.join("b-XXXXXX")).unwrap()).unwrap();
        assert_ne!(a.cookie, b.cookie, "a fresh cookie each time");
        assert_ne!(a.cookie, [0; 16]);
        let meta = std::fs::metadata(&a.file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // libXau's format: family, address, number, name, data; big-endian lengths
        let data = std::fs::read(&a.file).unwrap();
        assert_eq!(data, xauth_entry(&a.cookie));
        let mut want = vec![0xff, 0xff, 0, 0, 0, 0, 0, 18];
        want.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        want.extend_from_slice(&[0, 16]);
        want.extend_from_slice(&a.cookie);
        assert_eq!(data, want);
        // never written through an existing file (or a planted link)
        assert!(Auth::create(&dir.0).is_err());
        drop(dir);
    }

    #[test]
    fn only_dead_bridges_private_dirs_are_removed() {
        let _spawning = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::net::UnixListener;
        use std::process::Command;
        let base =
            PrivateDir(mkdtemp(&std::env::temp_dir().join("xwlbridge-test-sweep-XXXXXX")).unwrap());
        let mut done = Command::new("true").spawn().unwrap();
        let dead = done.id();
        done.wait().unwrap();
        let mut live = Command::new("sleep").arg("30").spawn().unwrap();
        let mk = |name: String| {
            let d = base.0.join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("Xauthority"), b"x").unwrap();
            d
        };
        // a dead bridge's (Xvfb: no socket; Xvnc: a socket nothing listens on)
        let gone = mk(format!("xwlbridge-{dead}-abcdef"));
        let gone_xvnc = mk(format!("xwlbridge-{}-abcdef", dead + 1_000_000));
        drop(UnixListener::bind(gone_xvnc.join(XVNC_SOCKET)).unwrap());
        // a live bridge's, ours, and a dead pid's whose socket answers
        let alive = mk(format!("xwlbridge-{}-abcdef", live.id()));
        let own = mk(format!("xwlbridge-{}-abcdef", std::process::id()));
        let answering = mk(format!("xwlbridge-{}-abcdef", dead + 2_000_000));
        let _listening = UnixListener::bind(answering.join(XVNC_SOCKET)).unwrap();
        // not ours: another name, a plain file, a link to a directory
        let other = mk("xwlbridge-ui-abcdef".into());
        let plain = base
            .0
            .join(format!("xwlbridge-{}-zzzzzz", dead + 3_000_000));
        std::fs::write(&plain, b"").unwrap();
        let target = mk("elsewhere".into());
        let link = base
            .0
            .join(format!("xwlbridge-{}-yyyyyy", dead + 4_000_000));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        remove_stale_private_dirs(&base.0);
        assert!(!gone.exists());
        assert!(!gone_xvnc.exists());
        for kept in [&alive, &own, &answering, &other, &plain, &link, &target] {
            assert!(
                std::fs::symlink_metadata(kept).is_ok(),
                "{} kept",
                kept.display()
            );
        }
        assert!(target.join("Xauthority").exists());
        let _ = live.kill();
        let _ = live.wait();
    }

    #[test]
    fn only_dead_bridges_rfb_sockets_are_removed() {
        let _spawning = SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::net::UnixListener;
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("xwlbridge-test-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let mut done = Command::new("true").spawn().unwrap();
        let dead = done.id();
        done.wait().unwrap();
        let mut live = Command::new("sleep").arg("30").spawn().unwrap();
        let sock = |pid: u32| dir.join(format!("xwlbridge-xvnc-{pid}.sock"));
        // left by a dead bridge: a socket nothing listens on
        drop(UnixListener::bind(sock(dead)).unwrap());
        // a live bridge's (its Xvnc may not listen yet)
        drop(UnixListener::bind(sock(live.id())).unwrap());
        // ours
        drop(UnixListener::bind(sock(std::process::id())).unwrap());
        // a dead pid, but something answers on it
        let other = dir.join(format!("xwlbridge-xvnc-{}.sock", dead + 1_000_000));
        let _listening = UnixListener::bind(&other).unwrap();
        // not a socket, or not our name
        std::fs::write(dir.join(format!("xwlbridge-xvnc-{}.sock.bak", dead)), b"").unwrap();
        let plain = dir.join("xwlbridge-xvnc-1999999999.sock");
        std::fs::write(&plain, b"").unwrap();
        remove_stale_rfb_sockets(&dir);
        assert!(!sock(dead).exists());
        assert!(sock(live.id()).exists());
        assert!(sock(std::process::id()).exists());
        assert!(other.exists());
        assert!(plain.exists());
        let _ = live.kill();
        let _ = live.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_restarts_are_limited_per_window() {
        let mut f = Failures::default();
        let t0 = Instant::now();
        for i in 0..AGENT_RESTARTS {
            assert!(
                f.record(t0 + Duration::from_secs(i as u64)),
                "failure {i} may restart"
            );
        }
        assert!(
            !f.record(t0 + Duration::from_secs(10)),
            "one too many within the window"
        );
        // once the earlier ones are out of the window, restarting is allowed again
        assert!(f.record(t0 + AGENT_RESTART_WINDOW + Duration::from_secs(20)));
    }

    #[test]
    fn one_layout_is_picked_out() {
        let s = "pc+us+de(nodeadkeys):2+us:3+inet(evdev)+group(alts_toggle)";
        assert_eq!(one_layout(s, 0), "pc+us+inet(evdev)+group(alts_toggle)");
        assert_eq!(
            one_layout(s, 1),
            "pc+de(nodeadkeys)+inet(evdev)+group(alts_toggle)"
        );
        assert_eq!(one_layout(s, 2), "pc+us+inet(evdev)+group(alts_toggle)");
        // past the last layout: wraps, as xkb does
        assert_eq!(one_layout(s, 4), one_layout(s, 1));
        // one layout: unchanged
        assert_eq!(one_layout("pc+us+inet(evdev)", 0), "pc+us+inet(evdev)");
        assert_eq!(one_layout("pc+us+inet(evdev)", 1), "pc+us+inet(evdev)");
        // a per-layout option part goes with its layout
        assert_eq!(
            one_layout("pc+us+ru:2+level3(ralt_switch):2", 1),
            "pc+ru+level3(ralt_switch)"
        );
        assert_eq!(one_layout("pc+us+ru:2+level3(ralt_switch):2", 0), "pc+us");
    }

    #[test]
    fn the_active_layout_is_uploaded() {
        let text = "xkb_keymap {\nxkb_keycodes \"evdev_aliases(qwerty)\" {\n};\nxkb_types \"complete\" {\n};\n\
                    xkb_compatibility \"complete\" {\n};\nxkb_symbols \"pc_us_de_2_us_3_inet(evdev)\" {\n};\n};";
        let k = include_keymap(text, 1).unwrap();
        assert!(k.contains("include \"pc+de+inet(evdev)\""), "{k}");
        let k = include_keymap(text, 0).unwrap();
        assert!(k.contains("include \"pc+us+inet(evdev)\""), "{k}");
    }

    #[test]
    fn keymap_component_names_are_restored() {
        let text = "xkb_keymap {\nxkb_keycodes \"evdev_aliases(qwerty)\" {\n};\nxkb_types \"complete\" {\n};\n\
                    xkb_compatibility \"complete\" {\n};\nxkb_symbols \"pc_us_fr(oss_latin9):2_inet(evdev)\" {\n};\n};";
        let k = include_keymap(text, 1).unwrap();
        assert!(k.contains("include \"evdev+aliases(qwerty)\""));
        assert!(k.contains("include \"pc+fr(oss_latin9)+inet(evdev)\""));
    }

    #[test]
    fn group_indices_are_restored() {
        let text = "xkb_keymap {\nxkb_keycodes \"evdev_aliases(qwerty)\" {\n};\nxkb_types \"complete\" {\n};\n\
                    xkb_compatibility \"complete\" {\n};\nxkb_symbols \"pc_us_us_2_inet(evdev)\" {\n};\n};";
        let k = include_keymap(text, 1).unwrap();
        assert!(k.contains("include \"pc+us+inet(evdev)\""), "{k}");
    }

    #[test]
    fn unnamed_components_give_up() {
        let text = "xkb_keymap {\nxkb_keycodes \"(unnamed)\" {\n};\nxkb_types \"(unnamed)\" {\n};\n\
                    xkb_compatibility \"(unnamed)\" {\n};\nxkb_symbols \"(unnamed)\" {\n};\n};";
        assert!(include_keymap(text, 0).is_none());
    }

    #[test]
    fn missing_sections_give_up() {
        assert!(include_keymap("xkb_keymap { xkb_keycodes \"evdev\" {}; };", 0).is_none());
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::{remode_for_refresh, same_refresh};

    #[test]
    fn xvnc_modes_stand_for_the_desktop_refresh_within_tolerance() {
        assert!(same_refresh(59_963, 60_000)); // Xvnc's 1920x1080
        assert!(same_refresh(60_000, 59_940)); // a 59.94 Hz monitor
        assert!(!same_refresh(59_375, 60_000)); // Xvnc's 640x480: 0.625 Hz off
        assert!(same_refresh(144_000, 143_500)); // 0.5 % at 144 Hz is 0.72 Hz
        assert!(!same_refresh(120_000, 144_000));
        assert!(!same_refresh(0, 60_000));
    }

    #[test]
    fn remode_only_towards_the_desktop_refresh() {
        assert!(remode_for_refresh(59_375, 60_000, 640, 480)); // Xvnc's 640x480 -> 60
        assert!(!remode_for_refresh(59_963, 60_000, 1920, 1080)); // close enough
        assert!(remode_for_refresh(59_963, 144_000, 1920, 1080));
        // 8K@144 can only say 129.45 Hz, still closer than 60
        assert!(remode_for_refresh(60_000, 144_000, 7680, 4320));
        assert!(!remode_for_refresh(129_453, 144_000, 7680, 4320));
        // 16384x16384 can say at most 16 Hz: never re-mode a 60 Hz output to that
        assert!(!remode_for_refresh(60_000, 60_000, 16384, 16384));
        assert!(!remode_for_refresh(59_000, 60_000, 16384, 16384));
    }
}
