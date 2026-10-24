//! GNOME backend: mirrors a mutter (GNOME Shell) desktop through mutter's own
//! remote-desktop D-Bus APIs, since mutter implements none of the Wayland
//! capture/virtual-input protocols the [`wayland`](crate::bridge::wayland)
//! backend uses.
//!
//! - Outputs come from `org.gnome.Mutter.DisplayConfig` (no session needed).
//! - The keymap comes from a plain `wl_keyboard` (no session needed), and from
//!   the EIS keyboard while a session exists.
//! - Capture and input need a linked `RemoteDesktop` + `ScreenCast` session,
//!   which makes gnome-shell show its screen-sharing indicator (with a stop
//!   button). So the session is created only while an X client is using the
//!   desktop: holding a DAMAGE object, reading the root's pixels, or injecting
//!   input (which waits for the session's devices, see
//!   [`Input::set_on_demand`](crate::bridge::input::Input::set_on_demand)),
//!   and closed shortly after the last one stops, like a real remote-desktop
//!   server.
//! - gnome-shell closes the session on the lock screen (it inhibits remote
//!   access there: we wait for unlock and resume) and from the indicator's stop
//!   button (the local user's say: we don't resume until the viewers using it
//!   have gone, unless `-reclaim`). Which of the two it was shows within a few
//!   seconds: the screen shield coming up (asked of org.gnome.ScreenSaver
//!   until then, since its state trails an animated lock by up to a second) is
//!   a lock, a monitor change just before is that change (the session is
//!   recreated), anything else is the user's stop. While locked the X side
//!   shows a padlock, not the last frame (the lock-screen extension in
//!   contrib/ lets viewers see and unlock the lock screen instead).
//! - Started before gnome-shell is up (a service racing the login), it waits
//!   for it; with no GNOME to mirror after that, it exits with an error, so the
//!   service manager tries again rather than a blank screen staying up.
//! - With `-dynres`, an X client's RandR resize switches the monitor to one of
//!   its own modes, temporarily (see [`display`]); the layout it replaced comes
//!   back once nobody watches any more, and when the process exits (not with
//!   `-dynreskeep`).
//! - Outputs in power save (DPMS) are turned on when a session starts and when
//!   they go to sleep while one runs (`-pwrmgr`), and put back to sleep when
//!   the viewers have gone if nobody touched the desktop in between.

mod capture;
mod clip;
mod dbus;
mod display;
mod eis;
mod keymap;
mod outbox;
mod primary;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;

use crate::bridge::Server;
use crate::bridge::dynres::ModeRequest;
use crate::bridge::placeholder;
use crate::config::CursorType;
use capture::{Capture, CursorTrack, Pw};
use dbus::{Monitor, Session, Signal};
use eis::Eis;
use keymap::KeymapWatch;

/// How recently a client must have read pixels for capture to stay on (the
/// wayland backend's gate, likewise).
const READ_GATE_MS: u64 = 1000;
// How long the session outlives the last watcher, so a viewer reconnecting
// (or vncagent pausing between polls) doesn't flap the sharing indicator, is
// `-warm` (10 s by default); after a one-shot client (a screenshot tool, an
// xdotool run) went away, `-linger` (30 s), so a burst of them finds it up.
/// How long a session started for a client that connected to a stock engine
/// (`-coldconnect start`) is kept for it to read the screen; past that, only
/// its reads and input keep it.
const PREFETCH_MS: u64 = 2000;
/// Retry delay while the session can't be created (locked screen: mutter
/// refuses until unlock).
const RETRY_LOCKED: Duration = Duration::from_secs(1);
/// How long after gnome-shell closes our session we look for the screen
/// shield before taking the close for the local user's "stop sharing". A
/// lock closes the session first and reports the shield only after its
/// animation (Super+L: a 300 ms fade, then the slide), so the first answer
/// right after the close is often still "not locked" (review S3).
const CLOSE_CLASSIFY: Duration = Duration::from_secs(3);
/// How often the lock state is polled meanwhile (`ActiveChanged` usually
/// answers first).
const LOCK_POLL: Duration = Duration::from_millis(250);
/// A close this soon after a monitor change is the change's doing.
const MONITOR_CLOSE_WINDOW: Duration = Duration::from_secs(2);
/// How long a `MonitorsChanged` burst is collected before it is acted on (a
/// colour-mode flip, a no-op apply, a mode set come as a burst of signals).
const MONITORS_DEBOUNCE: Duration = Duration::from_millis(250);
/// How long to wait before retrying a `GetCurrentState` that failed after a
/// `MonitorsChanged` (SE4): each further failure doubles the wait, up to one
/// of these; the list's length is also how many times it is retried (0.5, 1,
/// 2 and 4 s, then every 4 s for ~30 s), after which the layout in use stays
/// until the next change rather than a stale one being used for a session.
const REFRESH_BACKOFF: [Duration; 8] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(4),
    Duration::from_secs(4),
    Duration::from_secs(4),
    Duration::from_secs(4),
];

/// How long to wait before retry number `n` (0-based) of a failed
/// `GetCurrentState`, `None` once they are exhausted.
fn backoff(n: u32) -> Option<Duration> {
    REFRESH_BACKOFF.get(n as usize).copied()
}

/// When a capture stream or the input connection dies, how long to wait for
/// mutter's `Closed` signal before taking it for a stream failure (recreate at
/// once) rather than a close (lock, or the local user's "stop"): mutter tears
/// the streams and EIS down in the same moment it emits `Closed`, and the
/// PipeWire/EIS sockets usually tell us first.
const BROKEN_GRACE: Duration = Duration::from_millis(1500);
const RETRY_ERROR: Duration = Duration::from_secs(3);
/// How long after the session bus goes away we wait for a stop before ending
/// the process ourselves: at logout the bus and the service go down together,
/// and the service manager's SIGTERM arrives within this.
const BUS_LOST_GRACE: Duration = Duration::from_secs(10);
/// Upper bound on any single D-Bus call to mutter.
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Capture rate cap when -fps isn't given: the monitor's refresh rate, but no
/// more than this. VNC consumers can't use more, and each captured frame costs
/// mutter a paint and readback on the compositor thread (on a 144 Hz monitor
/// that would be 2.4x the work for nothing).
const MAX_DEFAULT_FPS: u32 = 60;
/// How long a new session's streams get to start streaming before the session
/// is recreated (mutter normally takes a few ms).
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(4);
/// How long input that arrived before the session's devices waits for the
/// second of them (keyboard or pointer) before it is replayed anyway.
const REPLAY_WAIT: Duration = Duration::from_millis(500);
/// How long the compositor's seat must have had a keyboard before keys go to
/// it. When the session's virtual keyboard is the seat's only one (no
/// physical keyboard: headless, some VMs and servers), the seat gains its
/// keyboard with it, and every app binds a new `wl_keyboard` only on hearing
/// so. A key the compositor emits before an app has bound one never reaches
/// that app (a key held at that moment shows up only in the keyboard's
/// `enter`, which toolkits don't type): the first keys of each session but the
/// first were lost that way. A seat that had a keyboard
/// already doesn't wait.
const KEYBOARD_SETTLE: Duration = Duration::from_millis(150);
/// Output keys are opaque non-zero ids; ours start here so they never look like
/// Wayland registry names in logs.
const FIRST_KEY: u32 = 0x1000;
/// How long startup waits for gnome-shell to own mutter's remote-desktop name
/// (a service started with the login can race it), and how often it looks.
const STARTUP_WAIT: Duration = Duration::from_secs(30);
const STARTUP_POLL: Duration = Duration::from_millis(250);
/// Exit status when there is no GNOME desktop to mirror (`EX_UNAVAILABLE`).
const EXIT_UNAVAILABLE: i32 = 69;
/// How long after a session's clipboard went GNOME's clipboard changes are
/// taken for that going (mutter drops the source we published with the
/// session), not for a GNOME app's copy.
const CLIP_TEARDOWN: Duration = Duration::from_millis(500);
/// How often to try again to watch GNOME's clipboard when mutter has no
/// session for us (it refuses them on the lock screen).
const RETRY_WATCH: Duration = Duration::from_secs(1);
/// How long a resolution change mutter accepted gets to show up in the
/// monitor layout (it normally has by the time the call returns).
const DYNRES_SETTLE: Duration = Duration::from_secs(3);
/// How long an X client may leave the built-in engine's outputs other than
/// the desktop's before they are put back (a resize takes RealVNC several
/// requests, the first of which turns the output off).
const X_OUTPUTS_SETTLE: Duration = Duration::from_secs(1);

/// An eventfd the signal pump writes to wake the backend's poll.
pub struct Wake(OwnedFd);

impl Wake {
    fn new() -> std::io::Result<Wake> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Wake(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn wake(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.0.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    fn drain(&self) {
        let mut v: u64 = 0;
        unsafe { libc::read(self.0.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
    }

    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Whether mutter's remote-desktop API is reachable on the session bus.
pub fn available() -> bool {
    session_bus()
        .map(|c| dbus::rd_available(&c))
        .unwrap_or(false)
}

/// The session bus, with every method call bounded: a hung gnome-shell must
/// not hang us (and with us, the X clients' input and clipboard).
fn session_bus() -> zbus::Result<Connection> {
    zbus::blocking::connection::Builder::session()?
        .method_timeout(DBUS_TIMEOUT)
        .build()
}

/// Ends the process: there is no GNOME desktop to mirror. A bridge that stayed
/// up would serve a blank screen with no input that no service manager ever
/// restarts; this way `Restart=on-failure` tries again.
fn give_up(why: &str) -> ! {
    crate::warning!(
        "gnome: {why}; exiting (status {EXIT_UNAVAILABLE}) so the service can try again"
    );
    crate::remove_display_socket();
    std::process::exit(EXIT_UNAVAILABLE)
}

/// Waits for gnome-shell if it isn't up yet, reads the layout and keymap
/// synchronously (so the X server starts with the real geometry and key
/// table), then runs the backend thread.
pub fn spawn(server: Arc<Server>) {
    let conn = match session_bus() {
        Ok(c) => c,
        Err(e) => give_up(&format!("no session bus ({e})")),
    };
    if !dbus::rd_available(&conn) {
        crate::log!(
            "gnome: waiting up to {}s for gnome-shell (org.gnome.Mutter.RemoteDesktop on the session bus)",
            STARTUP_WAIT.as_secs()
        );
        if !dbus::wait_for_rd(&conn, STARTUP_WAIT, STARTUP_POLL) {
            give_up(
                "org.gnome.Mutter.RemoteDesktop never appeared on the session bus \
                 (not GNOME, or run outside the user's session)",
            );
        }
        crate::log!("gnome: gnome-shell is up");
    }
    let wake = match Wake::new() {
        Ok(w) => Arc::new(w),
        Err(e) => give_up(&format!("eventfd: {e}")),
    };
    let (tx, rx) = channel();
    if let Err(e) = dbus::spawn_signal_pump(&conn, tx, wake.clone()) {
        give_up(&format!("can't subscribe to mutter signals: {e}"));
    }
    // input from an X client that only injects starts a session too (and
    // waits for its devices)
    let input_wake = wake.clone();
    server
        .input
        .set_on_demand(Box::new(move || input_wake.wake()));
    // a client reading the screen (or connecting to a stock engine) while
    // there is no session wakes us at once, and may wait for the first frame
    // (`-coldwait`, see crate::bridge::fresh)
    let fresh_wake = wake.clone();
    server
        .framebuffer
        .fresh
        .manage(Box::new(move || fresh_wake.wake()));
    // PipeWire's objects can't cross threads, so the backend is built on its
    // own thread; we wait for it to load the layout and keymap so the X server
    // reports them from the start.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let started = std::thread::Builder::new()
        .name("gnome".into())
        .spawn(move || {
            let mut backend = Backend::new(server, conn, rx, wake);
            if let Err(e) = backend.refresh_outputs(false) {
                crate::warning!("gnome: can't read the monitor layout: {e}");
            }
            backend.keymap_watch = KeymapWatch::connect();
            match backend.keymap_watch.as_mut().and_then(KeymapWatch::take) {
                Some(text) => backend.apply_keymap(text),
                None => crate::log!(
                    "gnome: no keymap yet (no wayland display?); it will come with the first session"
                ),
            }
            let _ = ready_tx.send(());
            backend.report_lock_screen_mode();
            // The X side can't do anything useful without the desktop, and a
            // process that stays up with a frozen picture and no input is never
            // restarted by its service manager: a panic here ends the process.
            // (X client threads are another matter: one client's panic must not
            // take the others down, so this is only for the backend.)
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| backend.run())).is_err() {
                crate::warning!("gnome: the backend thread panicked; exiting so the service can restart");
                crate::shutdown(crate::EXIT_BACKEND_FAILED);
            }
        });
    match started {
        Ok(_) => {
            if ready_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                crate::warning!("gnome: backend setup is slow; starting with default geometry");
            }
        }
        Err(e) => crate::warning!("gnome: can't start the backend thread: {e}"),
    }
}

/// A live session and everything hanging off it.
struct Active {
    session: Session,
    /// Recorded stream path -> output key, until its node arrives.
    pending: HashMap<String, u32>,
    captures: Vec<Capture>,
    eis: Option<Eis>,
    clip: Option<clip::Clip>,
    input_installed: bool,
    /// When EIS first had a device ready to emulate (input is installed once
    /// the seat's keyboard has settled: [`KEYBOARD_SETTLE`]).
    eis_ready_at: Option<Instant>,
    /// When input devices were installed.
    input_since: Option<Instant>,
    started: Instant,
    /// Every stream has streamed at least once.
    confirmed: bool,
    healthy: bool,
    /// A capture stream or the input connection died at this time; see
    /// [`BROKEN_GRACE`].
    broken: Option<(Instant, &'static str)>,
}

struct Backend {
    server: Arc<Server>,
    conn: Connection,
    signals: Receiver<Signal>,
    wake: Arc<Wake>,
    /// `-fps`: cap mutter's frame rate (default: the monitor's refresh rate).
    fps: Option<u32>,
    baked_cursor: bool,
    monitors: Vec<Monitor>,
    /// Which monitor GNOME's primary one is, from mutter's (stale) flags.
    primary: primary::PrimaryModel,
    /// Connector name -> output key, stable across sessions and layout changes
    /// so RandR output ids don't churn.
    keys: HashMap<String, u32>,
    next_key: u32,
    keymap_watch: Option<KeymapWatch>,
    keymap_hash: Option<u64>,
    keymap_text: Option<String>,
    active: Option<Active>,
    last_watched: Option<Instant>,
    retry_at: Instant,
    locked_logged: bool,
    /// Sessions in a row whose capture never started.
    failed_starts: u32,
    /// The local user stopped sharing from GNOME's panel: don't take the
    /// screen back until the current viewers have gone (unless -reclaim).
    stopped_by_user: bool,
    /// gnome-shell closed our session at this time and it isn't known yet
    /// whether the screen was locked or the local user stopped sharing.
    closed_pending: Option<Instant>,
    last_lock_poll: Option<Instant>,
    last_monitors_changed: Option<Instant>,
    /// A `MonitorsChanged` waiting for its burst to end, and when it
    /// arrived.
    monitors_pending: Option<Instant>,
    /// The layout the running session was started for ([`XLayout`]): the
    /// next session is restarted only if reading the layout again gives
    /// another one.
    session_layout: Option<XLayout>,
    /// A `GetCurrentState` after a `MonitorsChanged` failed: when to try
    /// again and how many times it has.
    refresh_retry: Option<(Instant, u32)>,
    /// The monitors' colour state (`color-mode`, `rgb-range`) as last read:
    /// not something the X screen depends on.
    colour: dbus::ColourState,
    /// The picture on the X screen while no session runs
    /// ([`placeholder::Kind`]), so that a new X framebuffer (a size change)
    /// is painted again instead of black.
    placeholder: Option<crate::bridge::placeholder::Kind>,
    /// The session bus closed our connection at this time; see
    /// [`BUS_LOST_GRACE`].
    bus_lost: Option<Instant>,
    /// The keymap layout (xkb group) mutter has active, as its EIS keyboard
    /// last said; kept across sessions (mutter keeps it too).
    group: u32,
    /// The CLIPBOARD ownership generation when the last session's clipboard
    /// went away (`None`: there hasn't been one), to tell whether an X client
    /// took the selection while GNOME couldn't hear of it.
    clip_generation: Option<u64>,
    /// When the last session's clipboard went away.
    clip_ended: Option<Instant>,
    /// Hears of GNOME's clipboard changing between sessions.
    clip_watch: Option<clip::ClipWatch>,
    /// When to try to make `clip_watch` again (`None`: never; this mutter
    /// can't).
    clip_watch_retry: Option<Instant>,
    /// When GNOME's clipboard last changed between sessions, as `clip_watch`
    /// heard: an X copy made before that is older than GNOME's.
    gnome_clip_changed: Option<Instant>,
    /// `-showwindows`: the clipboard bridged while no session runs (shown X
    /// windows are used without a viewer), on an unstarted session.
    idle_clip: Option<clip::Clip>,
    idle_clip_path: Option<String>,
    idle_clip_retry: Option<Instant>,
    /// The `clip_generation` the idle bridge started from: an X copy made
    /// after it went to GNOME through the idle session, and goes from GNOME
    /// with that session's clipboard, so a session taking over carries it.
    idle_clip_since: Option<u64>,
    pw: Option<Pw>,
    cursor: Rc<RefCell<CursorTrack>>,
    /// `-dynres` through DisplayConfig.
    dynres: bool,
    /// `-dynreskeep`: never put the replaced layout back.
    dynres_keep: bool,
    /// The layout our first resolution change replaced, the one we applied
    /// last, and when: put back once someone watched since and nobody does
    /// any more (unless the monitors changed some other way meanwhile).
    dynres_saved: Option<DynresSaved>,
    /// When a layout change of the user's (not ours) was last seen in
    /// `MonitorsChanged` (SE3).
    dynres_user_at: Option<Instant>,
    /// An X client's resolution change has been waiting for the user to
    /// confirm theirs (logged once).
    dynres_held_logged: bool,
    /// A change mutter accepted that the RandR model does not show yet.
    dynres_wait: Option<DynresWait>,
    /// `-pwrmgr`: wake outputs from power save for capture.
    pwrmgr: bool,
    /// The power-save mode we turned the outputs on from, and when.
    woke: Option<(i32, Instant)>,
    /// Since when an X client has left the built-in engine's outputs other
    /// than the desktop's (a CRTC off, moved or resized, the screen resized),
    /// and when that was last looked at.
    diverged_since: Option<Instant>,
    x_outputs_checked: Instant,
    /// The process is exiting: what was put back stays so.
    exiting: bool,
}

struct DynresSaved {
    /// The layout to put back: the user's.
    original: display::Layout,
    /// What the desktop shows because of the last resolution change (the
    /// user's own change on top of it otherwise).
    applied: display::Layout,
    at: Instant,
    /// When `original` was last read from a layout that was the user's (not
    /// one of ours). A layout change seen within
    /// [`USER_CHANGE_PENDING`] of the user's confirmed change is that
    /// confirmation, not the user changing their mind again.
    original_at: Instant,
}

impl DynresSaved {
    /// SE2: the layout changed to `layout` without us. If it is not the one
    /// our last change left, it is the user's (the end of a Settings change,
    /// a monitor plugged in, another scale, another primary) and becomes the
    /// one to restore. One that is again within [`USER_CHANGE_PENDING`] of
    /// our taking theirs over is a step of the same change, or its
    /// confirmation (GNOME's "Keep these display settings?" answers with a
    /// `MonitorsChanged` too), not a new one.
    fn note_user_layout(&mut self, layout: &display::Layout, now: Instant) {
        if layout == &self.applied {
            return;
        }
        if layout != &self.original && now.duration_since(self.original_at) > USER_CHANGE_PENDING {
            crate::vlog!("gnome: the user's layout is now {}", layout.describe());
            self.original = layout.clone();
            self.original_at = now;
        }
        self.applied = layout.clone();
    }
}

/// How long after the user's own change of the layout (GNOME's "Keep these
/// display settings?" was answered, or never appeared) a `MonitorsChanged` is
/// the confirmation of that change rather than another one.
const USER_CHANGE_PENDING: Duration = Duration::from_secs(30);

struct DynresWait {
    id: u64,
    key: u32,
    width: u16,
    height: u16,
    deadline: Instant,
}

/// Whether to have mutter draw the cursor into frames instead of describing
/// it in metadata. Before mutter 47 an area stream's cursor metadata can crash
/// mutter (46.2: `set_cursor_sprite_metadata` with a zero scale when the
/// sprite has no texture yet; 47.1 tested fine in a real session); monitor streams don't have that bug, but have
/// mutter#2718 (see `dbus::Session::start`). An unknown version gets the safe
/// choice.
fn cursor_metadata_unsafe(conn: &Connection, record_monitor: bool) -> bool {
    let major = dbus::mutter_major(conn);
    let unsafe_ = !record_monitor && major.is_none_or(|m| m < 47);
    match (major, unsafe_) {
        (Some(m), true) => crate::log!(
            "gnome: mutter {m}: drawing the cursor into frames (its cursor metadata is unsafe here)"
        ),
        (None, true) => crate::log!(
            "gnome: can't tell mutter's version: drawing the cursor into frames to be safe"
        ),
        (Some(m), false) => crate::vlog!("gnome: mutter {m}"),
        (None, false) => {}
    }
    unsafe_
}

impl Backend {
    fn new(
        server: Arc<Server>,
        conn: Connection,
        signals: Receiver<Signal>,
        wake: Arc<Wake>,
    ) -> Self {
        let config = &server.config;
        let mut dynres = config.outmgr_wants_gnome();
        if dynres && !display::apply_allowed(&conn) {
            crate::warning!(
                "gnome: mutter does not accept monitor configurations over D-Bus (monitors.xml \
                 policy); X clients cannot change the resolution"
            );
            dynres = false;
        } else if config.dynres && !dynres {
            crate::warning!(
                "gnome: -outmgr {:?} leaves -dynres without a way to change GNOME's monitors",
                config.outmgr
            );
        }
        if dynres {
            crate::log!(
                "gnome: X clients may switch a monitor to another of its modes (-dynres); {}",
                if config.dynreskeep {
                    "the change is kept after the viewers leave (-dynreskeep)"
                } else {
                    "the layout it replaces comes back once the viewers have gone, and at exit"
                }
            );
            server.dynres.set_available(true);
        }
        let pwrmgr = config.pwrmgr.wants_gnome();
        Backend {
            dynres,
            dynres_keep: config.dynreskeep,
            dynres_saved: None,
            dynres_user_at: None,
            dynres_held_logged: false,
            dynres_wait: None,
            pwrmgr,
            woke: None,
            diverged_since: None,
            x_outputs_checked: Instant::now(),
            exiting: false,
            fps: server.config.fps,
            baked_cursor: server.config.cursor == CursorType::Baked
                || cursor_metadata_unsafe(&conn, server.config.recordmonitor),
            server,
            conn,
            signals,
            wake,
            monitors: Vec::new(),
            primary: primary::PrimaryModel::new(),
            keys: HashMap::new(),
            next_key: FIRST_KEY,
            keymap_watch: None,
            keymap_hash: None,
            keymap_text: None,
            active: None,
            last_watched: None,
            retry_at: Instant::now(),
            locked_logged: false,
            failed_starts: 0,
            stopped_by_user: false,
            closed_pending: None,
            last_lock_poll: None,
            last_monitors_changed: None,
            monitors_pending: None,
            session_layout: None,
            refresh_retry: None,
            colour: Vec::new(),
            placeholder: None,
            bus_lost: None,
            group: 0,
            clip_generation: None,
            clip_ended: None,
            clip_watch: None,
            clip_watch_retry: Some(Instant::now()),
            gnome_clip_changed: None,
            idle_clip: None,
            idle_clip_path: None,
            idle_clip_retry: None,
            idle_clip_since: None,
            pw: None,
            cursor: Rc::new(RefCell::new(CursorTrack::default())),
        }
    }

    fn run(mut self) {
        loop {
            while let Ok(sig) = self.signals.try_recv() {
                self.on_signal(sig);
            }
            self.retry_refresh();
            self.monitors_changed();
            self.check_health();
            self.tick_clip_watch();
            self.tick_idle_clip();
            self.tick_session();
            self.report_liveness();
            self.install_input();
            self.replay_input();
            self.tick_dynres();

            let mut fds: Vec<libc::pollfd> = vec![libc::pollfd {
                fd: self.wake.fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let pw_idx = self.active.as_ref().and(self.pw.as_ref()).map(|pw| {
                fds.push(libc::pollfd {
                    fd: pw.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            });
            // input waiting for the EIS socket to drain, or a paced motion
            let eis_out = self
                .active
                .as_ref()
                .and_then(|a| a.eis.as_ref())
                .map(|e| e.shared.poll_needs());
            let eis_idx = self.active.as_ref().and_then(|a| a.eis.as_ref()).map(|e| {
                let blocked = eis_out.is_some_and(|(b, _)| b);
                fds.push(libc::pollfd {
                    fd: e.fd(),
                    events: libc::POLLIN | if blocked { libc::POLLOUT } else { 0 },
                    revents: 0,
                });
                fds.len() - 1
            });
            let clip_idx = (self.active.as_ref().is_some_and(|a| a.clip.is_some())
                || self.idle_clip.is_some())
            .then(|| {
                fds.push(libc::pollfd {
                    fd: self.server.clipboard.jobs().wake_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            });
            let dynres_idx = self.dynres.then(|| {
                fds.push(libc::pollfd {
                    fd: self.server.dynres.wake_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            });
            let km_idx = self
                .keymap_watch
                .as_mut()
                .and_then(KeymapWatch::prepare)
                .map(|fd| {
                    fds.push(libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    });
                    fds.len() - 1
                });
            // short while a session runs (to notice the watcher leaving), and
            // short enough idle to notice one arriving: the gate is a poll of
            // the X side's read time, with no fd to wake on; shorter still
            // while input waits for the seat's keyboard to settle
            let settling = self
                .active
                .as_ref()
                .is_some_and(|a| a.eis_ready_at.is_some() && !a.input_installed);
            let mut timeout = if settling { 10 } else { 100 };
            let deadline = eis_out.and_then(|(_, d)| d);
            if let Some(d) = deadline {
                let ms = d
                    .saturating_duration_since(Instant::now())
                    .as_micros()
                    .div_ceil(1000);
                timeout = timeout.min(ms as i32);
            }
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                if let Some(k) = self.keymap_watch.as_mut() {
                    k.dispatch(false);
                }
                continue;
            }
            if fds[0].revents != 0 {
                self.wake.drain();
                if let Some(c) = self.active.as_mut().and_then(|a| a.clip.as_mut()) {
                    c.poll_results();
                }
                if let Some(c) = self.idle_clip.as_mut() {
                    c.poll_results();
                }
            }
            if let Some(i) = clip_idx
                && fds[i].revents != 0
            {
                self.server.clipboard.jobs().drain_wake();
                if let Some(c) = self.active.as_mut().and_then(|a| a.clip.as_mut()) {
                    c.run_jobs();
                } else if let Some(c) = self.idle_clip.as_mut() {
                    c.run_jobs();
                }
            }
            if let (Some(i), Some(pw)) = (pw_idx, self.pw.as_ref())
                && fds[i].revents != 0
            {
                pw.iterate();
                self.check_captures();
                self.report_liveness();
            }
            if let Some(i) = eis_idx
                && fds[i].revents & !libc::POLLOUT != 0
            {
                self.dispatch_eis(); // (pumps too)
            } else if let Some(i) = eis_idx
                && (fds[i].revents & libc::POLLOUT != 0
                    || deadline.is_some_and(|d| Instant::now() >= d))
                && let Some(e) = self.active.as_ref().and_then(|a| a.eis.as_ref())
            {
                e.shared.pump();
            }
            if let Some(i) = dynres_idx
                && fds[i].revents != 0
            {
                // picked up by tick_dynres on the next pass, right away
                self.server.dynres.drain_wake();
            }
            if let Some(i) = km_idx {
                let readable = fds[i].revents & libc::POLLIN != 0;
                let alive = self
                    .keymap_watch
                    .as_mut()
                    .is_some_and(|k| k.dispatch(readable));
                if !alive {
                    crate::warning!("gnome: lost the wayland keymap connection");
                    self.keymap_watch = None;
                } else if let Some(text) = self.keymap_watch.as_mut().and_then(KeymapWatch::take) {
                    self.apply_keymap(text);
                }
            }
        }
    }

    /// Whether an X client is using the desktop: holding a DAMAGE object (on a
    /// stock engine, also reading the root's pixels), polling pixels recently,
    /// or injecting input.
    fn watched(&self) -> bool {
        self.server.damage.active()
            || self.server.framebuffer.read_within(READ_GATE_MS)
            || self.server.input.demanded_within(READ_GATE_MS)
    }

    fn tick_session(&mut self) {
        let now = Instant::now();
        if self.watched() {
            self.last_watched = Some(now);
        }
        if let Some(at) = self.bus_lost
            && now - at >= BUS_LOST_GRACE
            && !crate::stop_requested()
        {
            // Without the bus there is no desktop to capture or type into,
            // and nothing would ever restart a process that stays up like
            // that (a greeter's bus, say, gone with its login screen).
            crate::warning!(
                "gnome: the session bus is gone and no stop came; stopping the command and exiting with {}",
                crate::EXIT_DESKTOP_LOST
            );
            crate::shutdown(crate::EXIT_DESKTOP_LOST);
        }
        let fb = &self.server.framebuffer;
        let wanted = match self.server.config.warm() {
            // `<=`: with -warm 0, wanted exactly while watched
            crate::config::Warm::Secs(s) => self
                .last_watched
                .is_some_and(|t| now - t <= Duration::from_secs(s.into())),
            crate::config::Warm::Always => true,
        } || fb.oneshot_within(self.server.config.linger().as_millis() as u64)
            || fb.prefetch_within(PREFETCH_MS);
        self.classify_close();
        if self.closed_pending.is_some() {
            return;
        }
        if self.stopped_by_user {
            if wanted {
                return;
            }
            crate::log!("gnome: viewers gone; sharing will resume for the next one");
            self.stopped_by_user = false;
            self.server.input.accept_pending();
        }
        if let Some((at, why)) = self.active.as_ref().and_then(|a| a.broken)
            && now - at >= BROKEN_GRACE
        {
            // no Closed came: the stream (or EIS) failed by itself
            self.stop_session(why);
            self.retry_at = now;
            return;
        }
        match &self.active {
            None if wanted && now >= self.retry_at => self.start_session(),
            Some(a) if a.broken.is_some() => {}
            Some(_) if !wanted => {
                self.stop_session("nobody is watching");
                crate::bridge::placeholder::paint(
                    &self.server,
                    crate::bridge::placeholder::Kind::Idle,
                );
            }
            Some(a) if !a.healthy => {
                // capture never came up: something else in the PipeWire graph
                // or mutter is stuck; a fresh session gets fresh nodes. Back
                // off if it keeps happening.
                self.failed_starts += 1;
                let delay = if self.failed_starts <= 2 {
                    Duration::ZERO
                } else {
                    RETRY_ERROR * self.failed_starts.min(10)
                };
                crate::warning!(
                    "gnome: capture didn't start within {}s; recreating the session",
                    STREAM_START_TIMEOUT.as_secs()
                );
                self.stop_session("capture didn't start");
                self.retry_at = now + delay;
            }
            _ => {}
        }
        if !wanted {
            self.undo_for_viewers();
        }
    }

    /// Tells X-side readers whether the framebuffer is the desktop as it is
    /// now: a session runs and every stream has delivered a frame. Readers
    /// that arrive while it isn't wait for that (`-coldwait`), unless capture
    /// can't start for now.
    fn report_liveness(&mut self) {
        let live = self.active.as_ref().is_some_and(|a| {
            a.pending.is_empty()
                && !a.captures.is_empty()
                && a.captures.iter().all(|c| c.state.borrow().frames > 0)
        });
        let fresh = &self.server.framebuffer.fresh;
        if live
            && !fresh.is_live()
            && let Some(a) = self.active.as_ref()
        {
            crate::vlog!(
                "gnome: capture live {} ms after the session started",
                a.started.elapsed().as_millis()
            );
        }
        fresh.set_live(live);
        // the local user stopped sharing, the screen is locked (or gnome-shell
        // just closed the session and we don't know why yet), or the last
        // attempt failed and the next is a while off: a reader would only wait
        // out -coldwait for nothing
        let backing_off = self.active.is_none() && self.retry_at > Instant::now();
        fresh.set_unavailable(
            self.stopped_by_user
                || self.locked_logged
                || self.closed_pending.is_some()
                || backing_off,
        );
    }

    /// Nobody watches (any more): put back what was changed for the viewers.
    /// A resolution change is undone only once someone has watched since it
    /// was made, so a plain `xrandr` with no viewer around is not undone at
    /// once (it is when the next viewer leaves, or at exit).
    fn undo_for_viewers(&mut self) {
        if self.dynres_wait.is_none()
            && self
                .dynres_saved
                .as_ref()
                .is_some_and(|s| self.last_watched.is_some_and(|t| t >= s.at))
        {
            self.revert_dynres("nobody is watching");
        }
        self.restore_power_save("nobody is watching");
    }

    /// `-clipboard none`: GNOME's clipboard is neither bridged in a session
    /// nor watched between sessions. Likewise when the stock engine's
    /// selections are relayed with the desktop's Xwayland instead (which
    /// needs no session).
    fn clipboard_off(&self) -> bool {
        matches!(
            self.server.config.clipboard,
            crate::config::ClipboardType::None
        ) || self.server.xwayland_clip.is_some()
    }

    /// Makes the [`ClipWatch`](clip::ClipWatch) if there is none, when it is
    /// time to try.
    fn tick_clip_watch(&mut self) {
        let now = Instant::now();
        if self.clipboard_off()
            || self.clip_watch.is_some()
            || self.clip_watch_retry.is_none_or(|t| now < t)
        {
            return;
        }
        match clip::ClipWatch::start(&self.conn) {
            Ok(w) => {
                crate::vlog!(
                    "gnome: watching for GNOME clipboard changes between sessions ({})",
                    w.path
                );
                self.clip_watch = Some(w);
                self.clip_watch_retry = Some(now);
            }
            Err(clip::WatchError::Session(e)) => {
                if !dbus::is_inhibited(&e) {
                    crate::vlog!("gnome: can't watch GNOME's clipboard yet: {e}");
                }
                self.clip_watch_retry = Some(
                    now + if dbus::is_inhibited(&e) {
                        RETRY_WATCH
                    } else {
                        RETRY_ERROR * 10
                    },
                );
            }
            Err(clip::WatchError::Clipboard(e)) => {
                crate::warning!(
                    "gnome: can't hear of GNOME clipboard changes between sessions ({e}); an X \
                     copy made while no session runs will replace a GNOME copy made after it"
                );
                self.clip_watch_retry = None;
            }
        }
    }

    /// `-showwindows`: bridges the clipboard while no session's clipboard
    /// does, through an unstarted session (see [`clip::unstarted_session`]).
    fn tick_idle_clip(&mut self) {
        let want = self.server.config.showwindows
            && !self.clipboard_off()
            && self.active.as_ref().is_none_or(|a| a.clip.is_none());
        if !want {
            self.end_idle_clip();
            return;
        }
        let now = Instant::now();
        if self.idle_clip.is_some() || self.idle_clip_retry.is_some_and(|t| now < t) {
            return;
        }
        let path = match self.idle_clip_path.clone() {
            Some(p) => p,
            None => match clip::unstarted_session(&self.conn) {
                Ok(p) => {
                    self.idle_clip_path = Some(p.clone());
                    p
                }
                Err(e) => {
                    crate::vlog!("gnome: -showwindows: no session for the clipboard yet: {e}");
                    self.idle_clip_retry = Some(now + RETRY_WATCH);
                    return;
                }
            },
        };
        match clip::Clip::start(
            self.server.clone(),
            self.conn.clone(),
            path.clone(),
            self.wake.clone(),
            self.clip_generation,
            self.gnome_clip_changed,
        ) {
            Some(c) => {
                crate::vlog!(
                    "gnome: -showwindows: clipboard bridged while no session runs ({path})"
                );
                self.idle_clip = Some(c);
                self.idle_clip_since = self.clip_generation;
            }
            None => {
                // (e.g. "Already enabled": a fresh session next time)
                self.idle_clip_path = None;
                self.idle_clip_retry = Some(now + RETRY_WATCH * 5);
            }
        }
    }

    /// Hands the clipboard back from the unstarted session (a session's own
    /// clipboard takes over, or -showwindows is off).
    fn end_idle_clip(&mut self) {
        if let Some(c) = self.idle_clip.take() {
            // GNOME's copy of what X took while the idle bridge ran belonged
            // to the idle session and goes with its clipboard: the session
            // taking over publishes it again (as an X copy made while no
            // session ran), unless GNOME's clipboard changed after it. (With
            // -coldconnect start, every X client that connects while no
            // session runs starts one, so this handover is frequent.)
            self.clip_generation = self.idle_clip_since.take();
            self.clip_ended = Some(Instant::now());
            if let Some(p) = self.idle_clip_path.take() {
                // mutter 50 enables a session's clipboard once ("Already
                // enabled" after DisableClipboard, fd0f11a): the next idle
                // bridge takes a fresh unstarted session at once, rather
                // than after a refused attempt and 5 s
                let _ = clip::disable(&self.conn, &p);
            }
            drop(c);
        }
    }

    /// Marks the session unhealthy if a stream hasn't started in time, and
    /// resets the failure count once everything streams.
    fn check_health(&mut self) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        let all_streaming = a.pending.is_empty()
            && !a.captures.is_empty()
            && a.captures.iter().all(|c| c.state.borrow().streaming());
        if all_streaming {
            self.failed_starts = 0;
            a.healthy = true;
            a.confirmed = true;
        } else if !a.confirmed && a.started.elapsed() > STREAM_START_TIMEOUT {
            a.healthy = false;
        }
    }

    fn start_session(&mut self) {
        if self.monitors.is_empty() {
            if let Err(e) = self.refresh_outputs(true) {
                crate::warning!("gnome: can't read the monitor layout: {e}");
            }
            if self.monitors.is_empty() {
                self.retry_at = Instant::now() + RETRY_ERROR;
                return;
            }
        }
        // what this session is for: a layout read later is compared with it
        self.session_layout = Some(self.x_layout());
        if self.pw.is_none() {
            match Pw::new() {
                Ok(pw) => self.pw = Some(pw),
                Err(e) => {
                    crate::warning!("gnome: can't connect to PipeWire: {e}");
                    self.retry_at = Instant::now() + RETRY_ERROR;
                    return;
                }
            }
        }
        match Session::start(
            &self.conn,
            &self.monitors,
            self.baked_cursor,
            true,
            self.server.config.recordmonitor,
        ) {
            Ok(mut session) => {
                // the bus answers: whatever failed before, it isn't gone
                self.bus_lost = None;
                self.locked_logged = false;
                self.placeholder = None;
                // input from now until the devices are up waits for them
                self.server.input.accept_pending();
                let pending = session
                    .streams
                    .iter()
                    .filter_map(|s| self.keys.get(&s.connector).map(|&k| (s.path.clone(), k)))
                    .collect();
                // -showwindows: a running idle bridge (an unstarted session's
                // clipboard) stays the clipboard bridge while sessions come
                // and go. With -coldconnect start every X client that
                // connects while none runs (xclip itself) starts one, which
                // stops 2 s later; handing the clipboard over each way lost
                // what was copied around it
                let keep_idle = self.idle_clip.is_some();
                if !keep_idle {
                    self.end_idle_clip();
                }
                let clip = if self.clipboard_off() || keep_idle {
                    None
                } else {
                    clip::Clip::start(
                        self.server.clone(),
                        self.conn.clone(),
                        session.rd_path.clone(),
                        self.wake.clone(),
                        self.clip_generation,
                        self.gnome_clip_changed,
                    )
                };
                // which EIS region moves the pointer over which part of the
                // layout: mutter makes one absolute pointer per area stream
                let areas = session
                    .streams
                    .iter()
                    .filter_map(|s| {
                        let (x, y, w, h) = s.area;
                        Some(eis::StreamArea {
                            mapping_id: s.mapping_id.clone()?,
                            x,
                            y,
                            w,
                            h,
                        })
                    })
                    .collect();
                let eis = session.eis.take().and_then(|fd| {
                    Eis::new(fd, self.logical_origin(), areas, self.wake.clone())
                        .map_err(|e| crate::warning!("gnome: EIS socket: {e}"))
                        .ok()
                });
                crate::log!(
                    "gnome: remote desktop session started ({} monitor{})",
                    session.streams.len(),
                    if session.streams.len() == 1 { "" } else { "s" }
                );
                self.wake_outputs("a viewer started watching");
                self.active = Some(Active {
                    session,
                    pending,
                    captures: Vec::new(),
                    eis,
                    clip,
                    input_installed: false,
                    eis_ready_at: None,
                    input_since: None,
                    started: Instant::now(),
                    confirmed: false,
                    healthy: true,
                    broken: None,
                });
            }
            Err(e) if dbus::is_inhibited(&e) => {
                if !self.locked_logged {
                    crate::log!("gnome: screen is locked; waiting for unlock to capture");
                    self.locked_logged = true;
                }
                // whatever the X screen holds is from before the lock
                self.show_locked_placeholder();
                // what X clients type at a locked screen goes nowhere, and
                // never into the desktop once it unlocks
                self.server.input.drop_pending();
                self.retry_at = Instant::now() + RETRY_LOCKED;
            }
            Err(e) => {
                crate::warning!("gnome: can't start a remote desktop session: {e}");
                if dbus::connection_lost(&e) && self.bus_lost.is_none() {
                    // the connection itself failed (EPIPE): the bus is gone.
                    // (A call that took longer than DBUS_TIMEOUT is no sign
                    // of that: gnome-shell can stall for seconds, and the
                    // retry then serves.)
                    self.bus_lost = Some(Instant::now());
                }
                self.retry_at = Instant::now() + RETRY_ERROR;
            }
        }
    }

    fn stop_session(&mut self, why: &str) {
        self.session_layout = None;
        if let Some(active) = self.active.take() {
            crate::log!(
                "gnome: remote desktop session stopped after {:.1}s ({why})",
                active.started.elapsed().as_secs_f32()
            );
            if active.clip.is_some() {
                self.clip_generation = Some(
                    self.server
                        .clipboard
                        .generation(crate::bridge::clipboard::Sel::Clipboard),
                );
                self.clip_ended = Some(Instant::now());
            }
            self.server.input.clear_devices();
            self.server.framebuffer.fresh.set_live(false);
            drop(active); // captures disconnect, EIS closes, session stops
        }
        // Drop the PipeWire connection too. The streams' disconnects are only
        // sent when the loop runs, and nothing runs it between sessions; a
        // stream left half-gone stays in the graph as an unresponsive follower,
        // and the next stream mutter drives (ours or another program's) stalls
        // waiting on it. Closing the socket makes the daemon remove all of our
        // objects at once, and holds no PipeWire resources while idle.
        self.pw = None;
    }

    fn on_signal(&mut self, sig: Signal) {
        match sig {
            Signal::StreamAdded { path, node } => self.stream_added(&path, node),
            Signal::SessionClosed { path } => {
                if let Some(a) = self.active.as_mut()
                    && a.session.rd_path == path
                {
                    a.session.mark_closed();
                    // whichever it is, input waiting for this session must
                    // not be typed into the desktop later
                    self.server.input.drop_pending();
                    // gnome-shell closes sessions when the screen locks (we
                    // wait for unlock and resume) and when the local user
                    // clicks "stop" on the sharing indicator (which we honour);
                    // which of the two is only clear a moment later
                    self.stop_session("closed by gnome-shell");
                    self.closed_pending = Some(Instant::now());
                    self.last_lock_poll = None;
                    self.classify_close();
                }
                if self.idle_clip_path.as_deref() == Some(path.as_str()) {
                    crate::vlog!(
                        "gnome: -showwindows: the clipboard's session was closed; it is made again when mutter allows"
                    );
                    self.idle_clip = None;
                    self.idle_clip_path = None;
                    self.idle_clip_retry = Some(Instant::now() + RETRY_WATCH);
                }
                if self.clip_watch.as_ref().is_some_and(|w| w.path == path) {
                    // with every other session, on the lock screen
                    crate::vlog!(
                        "gnome: the clipboard watch was closed; it is made again when mutter allows"
                    );
                    self.clip_watch = None;
                    self.clip_watch_retry = Some(Instant::now() + RETRY_WATCH);
                }
            }
            Signal::ScreenLocked(true) => {
                crate::vlog!("gnome: screen shield up");
                if self.closed_pending.take().is_some() {
                    self.on_locked();
                } else if self.active.is_none() {
                    // nobody watching: don't keep the pre-lock desktop for the
                    // next viewer to see
                    self.show_locked_placeholder();
                }
            }
            Signal::ScreenLocked(false) => {
                crate::vlog!("gnome: screen shield down");
                if self.active.is_none() && !self.stopped_by_user {
                    self.retry_at = Instant::now();
                }
            }
            Signal::BusLost => {
                self.bus_lost.get_or_insert_with(Instant::now);
            }
            Signal::MonitorsChanged => {
                // acted on once the burst is over (monitors_changed)
                self.last_monitors_changed = Some(Instant::now());
                self.monitors_pending.get_or_insert_with(Instant::now);
            }
            Signal::SelectionOwnerChanged {
                path,
                mimes,
                session_is_owner,
                at,
            } => {
                if let Some(w) = self.clip_watch.as_ref()
                    && w.path == path
                {
                    // a session's clipboard hears of changes itself; right
                    // after one goes, the change is its source going
                    let live = self.active.as_ref().is_some_and(|a| a.clip.is_some())
                        || self.idle_clip.is_some();
                    let teardown = self
                        .clip_ended
                        .is_some_and(|t| at.saturating_duration_since(t) < CLIP_TEARDOWN);
                    if !live && !teardown && !session_is_owner && w.is_change(at) {
                        crate::cliplog!(
                            "gnome: GNOME's CLIPBOARD changed while no session runs ({mimes:?})"
                        );
                        self.gnome_clip_changed = Some(at);
                    }
                    return;
                }
                if let Some(c) = self.active.as_mut().and_then(|a| a.clip.as_mut())
                    && c.is_ours(&path)
                {
                    c.owner_changed(mimes, session_is_owner);
                } else if let Some(c) = self.idle_clip.as_mut()
                    && c.is_ours(&path)
                {
                    c.owner_changed(mimes, session_is_owner);
                }
            }
            Signal::SelectionTransfer { path, mime, serial } => {
                if let Some(c) = self.active.as_mut().and_then(|a| a.clip.as_mut())
                    && c.is_ours(&path)
                {
                    c.transfer(mime, serial);
                } else if let Some(c) = self.idle_clip.as_mut()
                    && c.is_ours(&path)
                {
                    c.transfer(mime, serial);
                }
            }
            Signal::PowerSaveMode(mode) => {
                crate::vlog!("gnome: power save mode is {mode}");
                // went to sleep under a watching client (the idle policy does
                // not count remote viewing as activity): wake it straight back
                if (1..=3).contains(&mode) && self.active.is_some() {
                    self.wake_outputs("it went into power save while a viewer is watching");
                }
            }
        }
    }

    fn stream_added(&mut self, path: &str, node: u32) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(key) = active.pending.remove(path) else {
            return;
        };
        let Some(pw) = self.pw.as_ref() else { return };
        let (w, h) = self
            .monitors
            .iter()
            .find(|m| self.keys.get(&m.connector) == Some(&key))
            .map_or((0, 0), |m| (m.width, m.height));
        match Capture::connect(
            pw,
            self.server.clone(),
            self.cursor.clone(),
            key,
            node,
            w,
            h,
            self.fps.or_else(|| {
                let hz = self
                    .monitors
                    .iter()
                    .find(|m| self.keys.get(&m.connector) == Some(&key))
                    .map_or(0, |m| m.refresh_mhz.div_ceil(1000));
                (hz > MAX_DEFAULT_FPS).then_some(MAX_DEFAULT_FPS)
            }),
        ) {
            Ok(c) => {
                crate::vlog!(
                    "gnome: capturing output {key:#x} from node {node} (mapping {:?})",
                    active
                        .session
                        .streams
                        .iter()
                        .find(|s| s.path == path)
                        .and_then(|s| s.mapping_id.as_deref())
                );
                active.captures.push(c);
            }
            Err(e) => crate::warning!("gnome: can't connect capture stream {node}: {e}"),
        }
    }

    /// After PipeWire ran: restart the session if a stream died, and adopt a
    /// negotiated size that differs from the layout's.
    fn check_captures(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let mut lost = false;
        let mut resized = Vec::new();
        for c in &active.captures {
            let mut st = c.state.borrow_mut();
            lost |= st.lost;
            if let Some(size) = st.size_mismatch.take() {
                resized.push((c.key, size));
            }
        }
        for (key, (w, h)) in resized {
            if let Some(m) = self
                .monitors
                .iter_mut()
                .find(|m| self.keys.get(&m.connector) == Some(&key))
            {
                crate::vlog!(
                    "gnome: output {key:#x} streams at {w}x{h}, not {}x{}",
                    m.width,
                    m.height
                );
                m.width = w;
                m.height = h;
            }
            self.apply_outputs(true);
        }
        if lost && let Some(a) = self.active.as_mut() {
            a.broken
                .get_or_insert((Instant::now(), "a capture stream ended"));
        }
    }

    fn dispatch_eis(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(eis) = active.eis.as_mut() else {
            return;
        };
        let up = eis.dispatch();
        if up.disconnected {
            crate::warning!("gnome: input connection (EIS) closed");
            active.eis = None;
            active.input_installed = false;
            active.eis_ready_at = None;
            self.server.input.clear_devices();
            // a session without input is no use: unless mutter closes it
            // (lock, user's stop), it is recreated after BROKEN_GRACE
            active
                .broken
                .get_or_insert((Instant::now(), "the input connection closed"));
            return;
        }
        // the keymap and the layout in effect first, so input waiting for the
        // devices is replayed against them
        if let Some(text) = up.keymap {
            self.apply_keymap(text);
        }
        if let Some(group) = up.group {
            self.set_group(group);
        }
        self.install_input();
        self.replay_input();
    }

    /// Hands the session's EIS devices to the X side once one is ready to
    /// emulate and the compositor's seat has had a keyboard for
    /// [`KEYBOARD_SETTLE`] (or for [`REPLAY_WAIT`] at most). Until then X
    /// clients' input waits for them, in order.
    fn install_input(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.input_installed {
            return;
        }
        let Some(eis) = active.eis.as_ref() else {
            return;
        };
        if !eis.ready() {
            active.eis_ready_at = None;
            return;
        }
        let ready_at = *active.eis_ready_at.get_or_insert_with(Instant::now);
        let settled = match self.keymap_watch.as_ref() {
            // the seat's keyboard as every app sees it
            Some(k) => k
                .keyboard_since()
                .is_some_and(|t| t.elapsed() >= KEYBOARD_SETTLE),
            // can't see the seat: give apps the same time from our devices
            None => ready_at.elapsed() >= KEYBOARD_SETTLE,
        };
        if !settled && ready_at.elapsed() < REPLAY_WAIT {
            return;
        }
        let (flush, pointer, keyboard) = eis.handles();
        self.server
            .input
            .set_devices(flush, Box::new(pointer), Box::new(keyboard));
        active.input_installed = true;
        active.input_since = Some(Instant::now());
        crate::vlog!(
            "gnome: input devices ready ({} ms after EIS{})",
            ready_at.elapsed().as_millis(),
            if settled {
                ""
            } else {
                "; the seat's keyboard never settled"
            }
        );
        self.arm_input_keymap();
    }

    /// Replays the input X clients sent before the session's devices were
    /// there, once a keyboard and a pointer both are (mutter adds them one
    /// after the other), or a moment after the first if one never comes.
    fn replay_input(&mut self) {
        let Some(a) = self.active.as_ref() else {
            return;
        };
        let (Some(since), Some(eis)) = (a.input_since, a.eis.as_ref()) else {
            return;
        };
        if !self.server.input.has_pending() {
            return;
        }
        if eis.complete() || since.elapsed() >= REPLAY_WAIT {
            // keys typed before the session were mapped with the layout the
            // X keyboard showed then; the compositor's may have changed while
            // no session told us (an input-source switch)
            let keymap = self.server.keymap.lock().unwrap();
            self.server.input.replay_pending_with(&|kc, sym, shortcut| {
                let Some(t) = keymap.as_ref() else {
                    return Some(kc);
                };
                if t.level1(kc) == sym {
                    return Some(kc);
                }
                // the keysym's own key now, if it needs no modifiers there
                // (the client holds the ones it pressed for its level)
                t.plan(sym, shortcut)
                    .filter(|(_, mods)| mods.is_empty())
                    .map(|(k, _)| k)
            });
        }
    }

    /// Installs a compositor keymap as the X keyboard mapping, telling X
    /// clients to re-read it (vncagent caches keycode -> keysym).
    fn apply_keymap(&mut self, text: String) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&text, &mut hasher);
        let hash = std::hash::Hasher::finish(&hasher);
        if self.keymap_hash == Some(hash) {
            return;
        }
        self.keymap_hash = Some(hash);
        self.server.input.set_modifier_keymap(&text);
        self.server.set_keymap_text(&text);
        self.keymap_text = Some(text);
        self.load_x_keymap("loaded compositor keymap");
        self.arm_input_keymap();
    }

    /// mutter switched layouts within its keymap (GNOME's input-source
    /// switch, Super+Space): the X keyboard becomes the new layout's, so X
    /// clients looking up a keysym find the key that types it now.
    fn set_group(&mut self, group: u32) {
        if group == self.group {
            return;
        }
        self.group = group;
        self.load_x_keymap(&format!("keyboard layout {group} is active"));
    }

    /// Builds the X keyboard mapping for the active layout of the current
    /// keymap and tells X clients (on a stock engine, the agent uploads it).
    fn load_x_keymap(&mut self, what: &str) {
        let Some(text) = self.keymap_text.as_deref() else {
            return;
        };
        self.server.set_keymap_group(self.group);
        if let Some(table) = crate::bridge::keymap::build_layout(text, self.group) {
            let xkb_types = table.xkb.types.len() as u8;
            if self.server.config.engine == crate::config::EngineType::Builtin {
                // a stock engine's agent says this once its X server has it
                let syms: Vec<u32> = (0..=u8::MAX).map(|kc| table.level1(kc)).collect();
                self.server.input.set_x_key_syms(Some(syms.into()));
            }
            *self.server.keymap.lock().unwrap() = Some(table);
            crate::log!("gnome: {what}");
            let count = crate::bridge::keymap::MAX_KEYCODE - crate::bridge::keymap::MIN_KEYCODE + 1;
            self.server.events.keyboard_mapping_changed(
                crate::bridge::keymap::MIN_KEYCODE,
                count,
                xkb_types,
            );
        }
    }

    /// `Input` drops keys until it has seen a keymap for the installed devices.
    fn arm_input_keymap(&self) {
        let installed = self.active.as_ref().is_some_and(|a| a.input_installed);
        if !installed {
            return;
        }
        if let Some((fd, size)) = self.keymap_text.as_deref().and_then(keymap::keymap_fd) {
            self.server.input.set_keymap(1, fd.as_fd(), size);
        }
    }

    /// Decides what a close by gnome-shell meant (see [`CLOSE_CLASSIFY`]): the
    /// screen shield coming up within the window is a lock; a monitor change
    /// just before is that change; anything else is the local user's "stop".
    fn classify_close(&mut self) {
        let Some(at) = self.closed_pending else {
            return;
        };
        let now = Instant::now();
        if self.last_lock_poll.is_none_or(|t| now - t >= LOCK_POLL) {
            self.last_lock_poll = Some(now);
            if self.screen_locked() {
                self.closed_pending = None;
                self.on_locked();
                return;
            }
        }
        if self
            .last_monitors_changed
            .is_some_and(|t| t + MONITOR_CLOSE_WINDOW >= at)
        {
            self.closed_pending = None;
            crate::log!("gnome: the session closed with a monitor change; recreating it");
            self.retry_at = now;
            return;
        }
        if now - at < CLOSE_CLASSIFY {
            return;
        }
        self.closed_pending = None;
        if self.server.config.reclaim {
            crate::log!("gnome: screen sharing was stopped from GNOME; taking it back (-reclaim)");
            self.retry_at = now;
            return;
        }
        crate::warning!(
            "gnome: screen sharing was stopped from GNOME; not resuming until \
             the current viewers disconnect (-reclaim overrides)"
        );
        self.stopped_by_user = true;
        if self.active.is_none() {
            crate::bridge::placeholder::paint(
                &self.server,
                crate::bridge::placeholder::Kind::Stopped,
            );
            self.placeholder = Some(crate::bridge::placeholder::Kind::Stopped);
        }
    }

    /// The screen locked under a live session: gnome-shell has closed remote
    /// access until someone unlocks at the computer (unless the lock-screen
    /// extension keeps it open, in which case the session isn't closed at all).
    fn on_locked(&mut self) {
        // The extension can be switched off behind our back after we started:
        // GNOME turns every user extension off when gnome-shell fails soon
        // after it starts (org.gnome.Shell-disable-extensions.service), and the
        // remote user then finds the padlock instead of the lock screen. Say
        // why, where the operator looks.
        match dbus::extension_state(&self.conn, dbus::LOCKSCREEN_EXTENSION) {
            Some(1) => crate::warning!(
                "gnome: the screen locked and GNOME closed remote access although {} is active; \
                 resuming after it is unlocked",
                dbus::LOCKSCREEN_EXTENSION
            ),
            Some(state) => crate::warning!(
                "gnome: the screen locked and GNOME closed remote access: {} is installed but not \
                 active (state {state}), so viewers can't see or unlock the lock screen; resuming after \
                 it is unlocked. GNOME turns user extensions off after a gnome-shell failure: check \
                 `gsettings get org.gnome.shell disable-user-extensions`",
                dbus::LOCKSCREEN_EXTENSION
            ),
            None => crate::log!(
                "gnome: the screen locked and GNOME closed remote access; resuming after it is \
                 unlocked (the xwlbridge lock-screen extension lets viewers see and unlock it)"
            ),
        }
        self.show_locked_placeholder();
        self.retry_at = Instant::now() + RETRY_LOCKED;
    }

    fn show_locked_placeholder(&mut self) {
        if self.placeholder != Some(crate::bridge::placeholder::Kind::Locked)
            && self.active.is_none()
        {
            crate::bridge::placeholder::paint(
                &self.server,
                crate::bridge::placeholder::Kind::Locked,
            );
            self.placeholder = Some(crate::bridge::placeholder::Kind::Locked);
            crate::vlog!("gnome: showing the locked placeholder");
        }
    }

    /// Says once, at start, what viewers will get while the screen is locked.
    fn report_lock_screen_mode(&self) {
        match dbus::extension_state(&self.conn, dbus::LOCKSCREEN_EXTENSION) {
            Some(1) => crate::log!(
                "gnome: lock screen: viewers see it and can unlock it ({} is active)",
                dbus::LOCKSCREEN_EXTENSION
            ),
            Some(state) => crate::warning!(
                "gnome: lock screen: {} is installed but not active (state {state}); viewers will see a \
                 padlock while the screen is locked. Enable it with `gnome-extensions enable {}` (GNOME turns \
                 user extensions off after a gnome-shell crash: `gsettings get org.gnome.shell \
                 disable-user-extensions`)",
                dbus::LOCKSCREEN_EXTENSION,
                dbus::LOCKSCREEN_EXTENSION
            ),
            None => crate::log!(
                "gnome: lock screen: viewers will see a padlock while the screen is locked (GNOME closes \
                 remote access there); the {} extension lets them see and unlock it",
                dbus::LOCKSCREEN_EXTENSION
            ),
        }
    }

    /// Whether gnome-shell's screen shield is up (an error, e.g. no
    /// gnome-shell at all, counts as not locked).
    fn screen_locked(&self) -> bool {
        zbus::blocking::Proxy::new(
            &self.conn,
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
        )
        .and_then(|p| p.call::<_, _, bool>("GetActive", &()))
        .unwrap_or(false)
    }

    fn logical_origin(&self) -> (i32, i32) {
        let x = self.monitors.iter().map(|m| m.lx).min().unwrap_or(0);
        let y = self.monitors.iter().map(|m| m.ly).min().unwrap_or(0);
        (x, y)
    }

    /// Re-reads mutter's layout into the RandR model. `Ok` whether the
    /// layout changed.
    fn refresh_outputs(&mut self, notify: bool) -> Result<bool, zbus::Error> {
        let (mut monitors, colour) = dbus::display_state_detailed(&self.conn)?;
        // mutter 50.1 leaves the old primary flagged after a change: what it
        // flags is a candidate, which one is primary is worked out
        let flags: Vec<(&str, bool)> = monitors
            .iter()
            .map(|m| (m.connector.as_str(), m.primary))
            .collect();
        let rects: Vec<(i32, i32, i32, i32)> =
            monitors.iter().map(|m| (m.lx, m.ly, m.lw, m.lh)).collect();
        let mut src = ShellPrimary(&self.conn, &rects);
        let primary = self.primary.update(&flags, &mut src);
        for (i, m) in monitors.iter_mut().enumerate() {
            m.primary = primary == Some(i);
        }
        let changed = monitors != self.monitors || colour != self.colour;
        if changed {
            for m in &monitors {
                crate::vlog!(
                    "gnome: monitor {} {}x{} at {},{} (logical {}x{})",
                    m.connector,
                    m.width,
                    m.height,
                    m.lx,
                    m.ly,
                    m.lw,
                    m.lh
                );
            }
        }
        self.monitors = monitors;
        self.colour = colour;
        self.apply_outputs(notify);
        Ok(changed)
    }

    /// Acts on the `MonitorsChanged` burst once it is over
    /// ([`MONITORS_DEBOUNCE`]), and retries a layout that could not be read.
    ///
    /// The session is restarted only if the layout the desktop reports now is
    /// not the one it was started for ([`XLayout`]): a colour mode (HDR), an
    /// RGB range, a refresh rate (a monitor stream is capped anyway), a
    /// no-op apply or any other change capture and input do not depend on
    /// leaves it running, so gnome-shell's sharing indicator does not blink,
    /// keys and buttons held stay down on the same devices, and a reader is
    /// not held up waiting for a new stream. A stream that renegotiates its
    /// format is handled by the stream (`capture.rs`).
    fn monitors_changed(&mut self) {
        if self
            .monitors_pending
            .is_none_or(|at| at.elapsed() < MONITORS_DEBOUNCE)
        {
            return;
        }
        self.monitors_pending = None;
        let was_active = self.active.is_some();
        let before = self.x_layout();
        match self.refresh_outputs(was_active) {
            Ok(_) => self.refresh_retry = None,
            Err(e) => {
                self.monitor_retry(&e);
                return;
            }
        }
        // -dynres: whose change was that?
        self.dynres_saw_change();
        if !was_active || self.active.is_none() {
            return;
        }
        let after = self.x_layout();
        if self.session_layout.as_ref() == Some(&after) {
            crate::vlog!(
                "gnome: monitors changed, the layout the session is for did not ({} monitor(s))",
                after.len()
            );
            self.dynres_check_wait();
            return;
        }
        crate::log!(
            "gnome: the monitor layout changed ({} -> {} monitor(s)); restarting the session",
            before.len(),
            after.len()
        );
        // monitor streams whose rect changed are gone; re-record them
        self.stop_session("monitors changed");
        self.retry_at = Instant::now();
    }

    /// A `GetCurrentState` after a `MonitorsChanged` failed: keep the layout
    /// in use (and the session running on it, which is still what the
    /// monitors show) and try again, backing off. The session is only ever
    /// rebuilt from a layout that was read (SE4): a failed read stops the
    /// retries but touches neither the layout in use nor the session, and a
    /// new `MonitorsChanged` starts over.
    fn monitor_retry(&mut self, e: &zbus::Error) {
        let now = Instant::now();
        let n = self.refresh_retry.map_or(0, |(_, n)| n);
        if n == 0 {
            crate::warning!(
                "gnome: can't read the monitor layout ({e}); trying again (the layout in use stays)"
            );
        }
        match backoff(n) {
            Some(delay) => self.refresh_retry = Some((now + delay, n + 1)),
            None => {
                crate::warning!(
                    "gnome: the monitor layout still can't be read ({e}); \
                     the layout in use stays until the next change"
                );
                self.refresh_retry = None;
            }
        }
    }

    /// Reads the layout again once a failed read is due to be retried.
    fn retry_refresh(&mut self) {
        if let Some((at, _)) = self.refresh_retry
            && Instant::now() >= at
        {
            self.refresh_retry = None;
            self.monitors_pending = Some(Instant::now() - MONITORS_DEBOUNCE);
        }
    }

    /// What a session is started for, and what capture and input depend on:
    /// the monitors' physical sizes and arrangement (so the X screen and
    /// every stream's rectangle), and which one is primary. Not their colour
    /// mode, refresh rate or name.
    fn x_layout(&self) -> XLayout {
        let mut v: Vec<_> = self
            .monitors
            .iter()
            .map(|m| {
                (
                    m.connector.clone(),
                    m.lx,
                    m.ly,
                    m.width,
                    m.height,
                    m.lw,
                    m.lh,
                )
            })
            .collect();
        v.sort();
        v
    }

    fn apply_outputs(&mut self, notify: bool) {
        for m in &self.monitors {
            if !self.keys.contains_key(&m.connector) {
                self.keys.insert(m.connector.clone(), self.next_key);
                self.next_key += 1;
            }
        }
        let live: Vec<u32> = self
            .monitors
            .iter()
            .filter_map(|m| self.keys.get(&m.connector).copied())
            .collect();
        let mut changed = false;
        {
            let mut screen = self.server.screen.lock().unwrap();
            for m in &self.monitors {
                let key = self.keys[&m.connector];
                changed |= screen.sync_wayland_output(
                    key,
                    m.connector.as_bytes(),
                    m.lx,
                    m.ly,
                    m.lw,
                    m.lh,
                    m.width.min(u16::MAX.into()) as u16,
                    m.height.min(u16::MAX.into()) as u16,
                    m.refresh_mhz,
                );
                changed |= screen.set_listed_modes(key, &m.modes);
            }
            for &key in self.keys.values() {
                if !live.contains(&key) {
                    changed |= screen.remove_wayland_output(key);
                }
            }
            let primary = self
                .monitors
                .iter()
                .find(|m| m.primary)
                .map_or(0, |m| self.keys[&m.connector]);
            changed |= screen.set_primary(primary);
        }
        let before = self.server.geometry();
        self.server.sync_screen(notify && changed);
        let origin = self.logical_origin();
        if let Some(eis) = self.active.as_ref().and_then(|a| a.eis.as_ref()) {
            eis.set_origin(origin);
        }
        // a new X framebuffer is black: the picture it showed is painted again
        let after = self.server.geometry();
        if (after.width, after.height) != (before.width, before.height) {
            self.paint_placeholder();
        }
    }

    /// Keeps the picture the X screen shows instead of the desktop (the
    /// padlock while the screen is locked, the pause sign after the local user
    /// stopped sharing) across a new X framebuffer, which is black (F5): a
    /// size change while it is shown paints it again.
    fn paint_placeholder(&mut self) {
        if let Some(kind) = self.placeholder {
            placeholder::paint(&self.server, kind);
        }
    }
}

/// GNOME Shell's own view of the monitors, for [`primary::PrimaryModel`].
/// GNOME Shell's view of the primary, for the monitors just read (their
/// logical rectangles, in the same order).
struct ShellPrimary<'a>(&'a Connection, &'a [(i32, i32, i32, i32)]);

impl primary::PrimarySource for ShellPrimary<'_> {
    fn shell_primary(&mut self) -> Option<usize> {
        let rect = dbus::shell_primary(self.0)?;
        self.1.iter().position(|r| *r == rect)
    }

    fn log(&mut self, msg: &str) {
        crate::warning!("{msg}");
    }
}

/// What a capture session depends on, one entry per monitor (see
/// [`Backend::x_layout`]).
// (not the primary flag: capture areas and input regions do not depend on which monitor is primary,
// and RandR's primary follows in apply_outputs without a new session)
type XLayout = Vec<(String, i32, i32, u32, u32, i32, i32)>;

/// `-dynres` and `-pwrmgr`.
impl Backend {
    /// Picks up a parked resolution change, enforces the deadline of the one
    /// in progress, and answers the exit-time request to undo our changes.
    fn tick_dynres(&mut self) {
        if self.server.dynres.finish_requested() {
            if let Some(w) = self.dynres_wait.take() {
                self.server
                    .dynres
                    .complete(w.id, Err("xwlbridge is exiting".into()));
            }
            self.revert_dynres("xwlbridge is exiting");
            self.restore_power_save("xwlbridge is exiting");
            // whatever happens until the process is gone (our own power save
            // coming back as a signal, a watcher still connected) changes
            // nothing any more
            self.exiting = true;
            self.server.dynres.finished();
        }
        self.tick_x_outputs();
        if !self.dynres {
            return;
        }
        self.dynres_check_wait();
        if self.dynres_wait.is_some() {
            return;
        }
        // SE3: not while the user may still be confirming a change of their
        // own (GNOME's "Keep these display settings?"): any ApplyMonitorsConfig
        // of ours in that time would answer it for them
        if self
            .dynres_user_at
            .is_some_and(|at| at.elapsed() < USER_CHANGE_PENDING)
            && self.server.dynres.busy()
        {
            if !self.dynres_held_logged {
                crate::log!(
                    "gnome: a resolution change requested by an X client waits: the display \
                     settings may still be being confirmed"
                );
                self.dynres_held_logged = true;
            }
            return;
        }
        self.dynres_held_logged = false;
        if let Some(req) = self.server.dynres.take() {
            self.dynres_start(req);
        }
    }

    fn dynres_start(&mut self, req: ModeRequest) {
        match self.dynres_apply(req.wl_name, req.width, req.height) {
            Ok(()) => {
                self.dynres_wait = Some(DynresWait {
                    id: req.id,
                    key: req.wl_name,
                    width: req.width,
                    height: req.height,
                    deadline: Instant::now() + DYNRES_SETTLE,
                });
                self.dynres_check_wait();
            }
            Err(e) => {
                crate::warning!(
                    "gnome: resolution change to {}x{} refused: {e}",
                    req.width,
                    req.height
                );
                self.server.dynres.complete(req.id, Err(e));
            }
        }
    }

    /// Applies the layout that shows output `key` at `width`x`height`, and
    /// re-reads the layout into the RandR model.
    fn dynres_apply(&mut self, key: u32, width: u16, height: u16) -> Result<(), String> {
        let connector = self
            .keys
            .iter()
            .find(|&(_, &k)| k == key)
            .map(|(c, _)| c.clone())
            .ok_or("the output no longer exists")?;
        let state = display::State::read(&self.conn)
            .map_err(|e| format!("can't read the monitor configuration: {e}"))?;
        let layout = match state.plan(&connector, width.into(), height.into())? {
            display::Plan::Unchanged => {
                // mutter runs it already; the model just had not caught up
                self.refresh_outputs(true).ok();
                return Ok(());
            }
            display::Plan::Apply(layout) => layout,
        };
        display::apply(&self.conn, &state, &layout)
            .map_err(|e| format!("mutter refused it: {e}"))?;
        crate::log!(
            "gnome: {connector} switched to {width}x{height} for an X client (temporary): {}",
            layout.describe()
        );
        let now = Instant::now();
        if self.dynres_keep {
            // nothing will be put back, so nothing to remember
            self.refresh_outputs(true).ok();
            return Ok(());
        }
        match self.dynres_saved.as_mut() {
            Some(saved) => {
                // SE2: the desktop is not where we left it: the user changed
                // the layout in between, and theirs is what is put back
                if state.layout != saved.applied {
                    crate::log!(
                        "gnome: the monitors were reconfigured since the last resolution change; \
                         {} is what will be restored",
                        state.layout.describe()
                    );
                    saved.original = state.layout;
                    saved.original_at = now;
                }
                saved.applied = layout;
                saved.at = now;
            }
            None => {
                self.dynres_saved = Some(DynresSaved {
                    original: state.layout,
                    applied: layout,
                    at: now,
                    original_at: now,
                })
            }
        }
        // back where we started: nothing left to undo
        if self
            .dynres_saved
            .as_ref()
            .is_some_and(|s| s.applied == s.original)
        {
            self.dynres_saved = None;
        }
        self.sync_undo_pending();
        self.refresh_outputs(true).ok();
        Ok(())
    }

    /// Completes the change in progress once the model shows it (or fails it
    /// at its deadline), waking the X client that asked.
    fn dynres_check_wait(&mut self) {
        let Some(w) = &self.dynres_wait else { return };
        let shown = self
            .server
            .screen
            .lock()
            .unwrap()
            .output_by_wl_name(w.key)
            .is_some_and(|o| (o.width, o.height) == (w.width, w.height));
        if shown {
            self.server.dynres.complete(w.id, Ok(()));
            self.dynres_wait = None;
        } else if Instant::now() >= w.deadline {
            crate::warning!(
                "gnome: mutter accepted {}x{} but the monitor layout does not show it",
                w.width,
                w.height
            );
            self.server
                .dynres
                .complete(w.id, Err("the monitor did not change".into()));
            self.dynres_wait = None;
        }
    }

    /// A `MonitorsChanged` was acted on: was it a change of the user's?
    ///
    /// SE2: what the user set is what is put back at the end. A layout that
    /// is not the one our last change left is theirs (the end of a Settings
    /// change, a monitor plugged in, another scale, another primary) and
    /// becomes the one to restore. One that is again within
    /// [`USER_CHANGE_PENDING`] of our taking theirs over is a step of the
    /// same change, or its confirmation (GNOME's "Keep these display
    /// settings?" answers with a `MonitorsChanged` too), not a new one.
    fn dynres_saw_change(&mut self) {
        if !self.dynres {
            return;
        }
        let Ok(state) = display::State::read(&self.conn) else {
            return;
        };
        let now = Instant::now();
        self.dynres_user_at = Some(now);
        if let Some(saved) = self.dynres_saved.as_mut() {
            saved.note_user_layout(&state.layout, now);
        }
    }

    /// Puts back the layout the user had before our resolution changes, unless
    /// they changed it themselves since (that one then stays:
    /// [`Self::dynres_saw_change`] kept track).
    fn revert_dynres(&mut self, why: &str) {
        let Some(saved) = self.dynres_saved.take() else {
            return;
        };
        self.sync_undo_pending();
        let state = match display::State::read(&self.conn) {
            Ok(s) => s,
            Err(e) => {
                crate::warning!("gnome: can't read the monitor configuration to restore it: {e}");
                return;
            }
        };
        if state.layout != saved.applied && state.layout != saved.original {
            // a change nobody told us about (SE2): theirs, and it stays
            crate::log!(
                "gnome: the monitors were reconfigured since an X client changed the resolution; \
                 leaving them as they are"
            );
            return;
        }
        if state.layout == saved.original {
            return; // already back where the user had it
        }
        if !state.can_apply(&saved.original) {
            crate::warning!(
                "gnome: the monitor modes from before the resolution change are gone; not restoring \
                 {}",
                saved.original.describe()
            );
            return;
        }
        match display::apply(&self.conn, &state, &saved.original) {
            Ok(()) => {
                crate::log!(
                    "gnome: restored the monitor configuration ({why}): {}",
                    saved.original.describe()
                );
                self.refresh_outputs(true).ok();
            }
            Err(e) => crate::warning!("gnome: can't restore the monitor configuration: {e}"),
        }
    }

    /// Puts the built-in engine's outputs back once an X client has left them
    /// other than the desktop's for a while with nothing in progress (an
    /// output turned off and never on again, `xrandr --off`), so no monitor
    /// stays disabled on the X side. Requests that reach the compositor are
    /// rolled back on refusal already (x11/conn.rs).
    fn tick_x_outputs(&mut self) {
        // only the built-in engine's clients reach the model's outputs (the
        // stock engines' agent looks after its own X server), and a few
        // looks a second are plenty against a one-second settle time
        if self.server.config.engine != crate::config::EngineType::Builtin
            || self.x_outputs_checked.elapsed() < Duration::from_millis(250)
        {
            return;
        }
        self.x_outputs_checked = Instant::now();
        let diverged = self.server.screen.lock().unwrap().outputs_diverged();
        if !diverged || self.dynres_wait.is_some() || self.server.dynres.busy() {
            self.diverged_since = None;
            return;
        }
        let since = *self.diverged_since.get_or_insert_with(Instant::now);
        if since.elapsed() < X_OUTPUTS_SETTLE {
            return;
        }
        self.diverged_since = None;
        if self.server.screen.lock().unwrap().restore_outputs() {
            crate::log!(
                "gnome: an X client left the X outputs other than the desktop's (off, moved or resized); \
                 restored them"
            );
            self.server.sync_screen(true);
        }
    }

    /// Turns the outputs on if mutter has them in power save.
    fn wake_outputs(&mut self, why: &str) {
        if !self.pwrmgr || self.exiting {
            return;
        }
        let Some(mode) = display::power_save_mode(&self.conn) else {
            return;
        };
        if !(1..=3).contains(&mode) {
            return; // on, or -1: unsupported
        }
        match display::set_power_save_mode(&self.conn, 0) {
            Ok(()) => {
                crate::log!(
                    "gnome: the screen was in power save (mode {mode}); turning it on ({why})"
                );
                if self.woke.is_none() {
                    self.woke = Some((mode, Instant::now()));
                }
                self.sync_undo_pending();
            }
            Err(e) => crate::warning!("gnome: can't wake the screen from power save: {e}"),
        }
    }

    /// The viewers have gone: if nobody used the desktop since we woke it (no
    /// input at all, local or remote), it goes back into the power save we
    /// took it out of; otherwise the idle policy has it from here.
    fn restore_power_save(&mut self, why: &str) {
        let Some((mode, at)) = self.woke.take() else {
            return;
        };
        self.sync_undo_pending();
        if display::power_save_mode(&self.conn) != Some(0) {
            return; // someone else has set it since
        }
        match display::idle_time(&self.conn) {
            Some(idle) if idle >= at.elapsed() => {
                match display::set_power_save_mode(&self.conn, mode) {
                    Ok(()) => crate::log!(
                        "gnome: nobody used the desktop since the screen was woken; back to \
                         power save (mode {mode}; {why})"
                    ),
                    Err(e) => {
                        crate::warning!("gnome: can't put the screen back into power save: {e}")
                    }
                }
            }
            Some(idle) => crate::vlog!(
                "gnome: the desktop was used {:.1}s ago, since the screen was woken; leaving it on",
                idle.as_secs_f32()
            ),
            None => crate::vlog!("gnome: no idle time from mutter; leaving the screen on"),
        }
    }

    fn sync_undo_pending(&self) {
        self.server
            .dynres
            .set_undo_pending(self.dynres_saved.is_some() || self.woke.is_some());
    }
}

#[cfg(test)]
mod monitor_tests {
    use super::{REFRESH_BACKOFF, backoff};
    use std::time::Duration;

    /// SE4: a `GetCurrentState` that failed after a `MonitorsChanged` is
    /// retried at 0.5, 1, 2, 4 s and then every 4 s, for ~30 s in all, and
    /// then left to the next change (never used to rebuild a session).
    #[test]
    fn a_failed_layout_read_is_retried_with_a_bounded_backoff() {
        let waits: Vec<Duration> = (0..).map_while(backoff).collect();
        assert_eq!(waits.len(), REFRESH_BACKOFF.len());
        let secs: Vec<f32> = waits.iter().map(|d| d.as_secs_f32()).collect();
        assert_eq!(secs, [0.5, 1.0, 2.0, 4.0, 4.0, 4.0, 4.0, 4.0]);
        // grows, then stays at the cap
        assert!(waits.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*waits.last().unwrap(), Duration::from_secs(4));
        let total: Duration = waits.iter().sum();
        assert!((Duration::from_secs(20)..=Duration::from_secs(30)).contains(&total));
        assert_eq!(backoff(waits.len() as u32), None);
        assert_eq!(backoff(u32::MAX), None);
    }
}

#[cfg(test)]
mod dynres_tests {
    use super::{DynresSaved, USER_CHANGE_PENDING};
    use crate::bridge::gnome::display::{Layout, Logical};
    use std::time::{Duration, Instant};

    fn layout(scale: f64) -> Layout {
        Layout(vec![Logical {
            x: 0,
            y: 0,
            scale,
            transform: 0,
            primary: true,
            monitors: vec![("Meta-0".into(), "1920x1080".into())],
            props: Vec::new(),
        }])
    }

    fn saved(original: Layout, applied: Layout, at: Instant) -> DynresSaved {
        DynresSaved {
            original,
            applied,
            at,
            original_at: at,
        }
    }

    /// SE2: the user's change after an X client's resolution change is what
    /// is put back once the viewers leave; so is the next one.
    #[test]
    fn the_users_layout_is_the_one_restored() {
        let t0 = Instant::now();
        let (l0, l1, l2, l3) = (layout(1.0), layout(1.25), layout(1.5), layout(2.0));
        // the user had l0, an X client's resolution change left l1
        let mut s = saved(l0.clone(), l1.clone(), t0);
        // a MonitorsChanged that is not a change: nothing
        s.note_user_layout(&l1.clone(), t0 + Duration::from_secs(1));
        assert_eq!(s.original, l0);
        // the user sets another scale in Settings: that is what is restored
        s.note_user_layout(&l2, t0 + Duration::from_secs(60));
        assert_eq!(s.original, l2);
        // our own change of the resolution again is not the user's
        let before = s.original.clone();
        s.note_user_layout(&l2, t0 + Duration::from_secs(70));
        assert_eq!(s.original, before);
        // and a later change of theirs again
        s.note_user_layout(&l3, t0 + Duration::from_secs(120));
        assert_eq!(s.original, l3);
    }

    /// The confirmation of the user's own change (GNOME's dialog goes away,
    /// or the change is kept) is not a new layout, even if it comes back to
    /// the one before: their change is still what the end restores.
    #[test]
    fn a_confirmation_within_the_window_is_not_a_new_layout() {
        let t0 = Instant::now();
        let (l0, l1, trial) = (layout(1.0), layout(1.25), layout(1.0));
        assert!(USER_CHANGE_PENDING >= Duration::from_secs(20));
        let mut s = saved(l0, l1, t0);
        s.note_user_layout(&trial, t0 + Duration::from_secs(60));
        // the trial ran out 20 s later: the layout before it is back, and it
        // is the user's
        s.note_user_layout(&layout(1.0), t0 + Duration::from_secs(80));
        assert_eq!(s.original, layout(1.0));
        assert_eq!(s.applied, layout(1.0));
    }
}
