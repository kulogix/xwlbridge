//! Fake X11 server that implements enough for RealVNC's vncagent-x11 (and most
//! other VNC server implementations) to work on wayland with screen capture,
//! virtual input, and clipboard synchronization.

/// Logging verbosity: 0 = quiet (warnings only), 1 = normal, 2 = verbose. Set
/// once at startup from `-quiet`/`-verbose`.
pub(crate) static LOG_LEVEL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Emits one `xwlbridge:`-prefixed line to stderr if the current level is at least
/// `$min`. Same args as `format!`.
macro_rules! logat {
    ($min:expr, $($arg:tt)*) => {
        if $crate::LOG_LEVEL.load(::std::sync::atomic::Ordering::Relaxed) >= $min {
            eprintln!("xwlbridge: {}", format_args!($($arg)*));
        }
    };
}
/// Informational log (shown unless `-quiet`).
macro_rules! log { ($($arg:tt)*) => { $crate::logat!(1, $($arg)*) }; }
/// Warning/error log (always shown).
macro_rules! warning {
    ($($arg:tt)*) => { eprintln!("xwlbridge: {}", format_args!($($arg)*)) };
}
/// Unimplemented (and not explicitly stubbed) request/feature (always shown).
macro_rules! fixme {
    ($($arg:tt)*) => { eprintln!("xwlbridge: fixme: {}", format_args!($($arg)*)) };
}
/// Verbose log (shown only with `-verbose`).
macro_rules! vlog { ($($arg:tt)*) => { $crate::logat!(2, $($arg)*) }; }

/// Whether `-cliptrace` was given. A global like [`LOG_LEVEL`] rather than a
/// config lookup because the clipboard bridge spans the X connection threads,
/// the event sink and the Wayland thread, and the latter two hold no config.
pub(crate) static CLIP_TRACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One step of the clipboard/selection state machine (shown only with
/// `-cliptrace`). Enough of these to follow a copy end to end: who took a
/// selection, what we asked its owner for, what came back, and what we told
/// the compositor and the other X clients.
macro_rules! cliplog {
    ($($arg:tt)*) => {
        if $crate::CLIP_TRACE.load(::std::sync::atomic::Ordering::Relaxed) {
            eprintln!("xwlbridge: clip: {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use {cliplog, fixme, log, logat, vlog, warning};

#[macro_use]
mod util;
mod bridge;
mod childenv;
mod config;
mod uidisplay;
#[cfg(feature = "xvfb")]
mod xvfb;

use std::ffi::CString;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io, process, ptr, thread};

use crate::bridge::Server;
use crate::bridge::input::Input;
use crate::bridge::x11::conn::Connection;
use crate::bridge::x11::randr::Screen;
use crate::config::Config;

/// The wrapped child, for forwarding signals: 0 before it is spawned, `SPAWNING`
/// while the spawn is in progress, its pid after, `CLOSED` once it has been
/// reaped or will never be spawned (we are exiting). One word, so a signal
/// handler never sees a torn state (with the pid and a spawning flag as two
/// atomics, a handler that read pid 0 just before the spawn finished would
/// then read "not spawning" and exit under a live child).
static CHILD: AtomicI32 = AtomicI32::new(0);
const SPAWNING: i32 = -1;
const CLOSED: i32 = -2;
static PENDING_SIG: AtomicI32 = AtomicI32::new(0); // a signal that arrived while spawning, to forward
static SOCKET_PATH: AtomicPtr<libc::c_char> = AtomicPtr::new(ptr::null_mut()); // leaked C string, to unlink on termination, null until display is bound
/// A stock X server's private files for [`handle_term`] to remove if a stop
/// comes while we are still starting (the X server then dies with us, by its
/// parent-death signal, and can't): its authority file and RFB socket, and
/// the private directory holding them. Leaked C strings, null if none.
#[cfg_attr(not(feature = "xvfb"), allow(dead_code))]
static ENGINE_FILES: [AtomicPtr<libc::c_char>; 2] = [
    AtomicPtr::new(ptr::null_mut()),
    AtomicPtr::new(ptr::null_mut()),
];
#[cfg_attr(not(feature = "xvfb"), allow(dead_code))]
static ENGINE_DIR: AtomicPtr<libc::c_char> = AtomicPtr::new(ptr::null_mut());
/// Non-zero once [`shutdown`] runs: the status to exit with instead of the
/// wrapped command's (the first cause wins).
static SHUTDOWN: AtomicI32 = AtomicI32::new(0);
/// The first terminating signal we were sent (a stop someone asked for:
/// `systemctl stop`, Ctrl+C), 0 if none. A stop ends with status 0 (see
/// [`exit_status`]), however the parts we stop for it go down.
static STOP_SIG: AtomicI32 = AtomicI32::new(0);
/// With `-nowrap`: the write end of the pipe main waits on for a stop, -1
/// until main waits there.
static NOWRAP_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Exit status when the stock X server engine is gone for good (it died, or
/// its agent keeps failing): EX_UNAVAILABLE, for a service manager to restart
/// us (and the wrapped command, which we stop first).
#[cfg(feature = "xvfb")]
pub(crate) const EXIT_ENGINE_LOST: i32 = 69;
/// Exit status when the desktop backend failed (its thread panicked):
/// EX_SOFTWARE, for a service manager to restart us.
#[cfg(feature = "gnome")]
pub(crate) const EXIT_BACKEND_FAILED: i32 = 70;
/// Exit status when the desktop is gone for good under us (GNOME: the session
/// bus closed our connection and nobody asked us to stop): EX_UNAVAILABLE, as
/// for a lost engine.
#[cfg(feature = "gnome")]
pub(crate) const EXIT_DESKTOP_LOST: i32 = 69;
/// How long the wrapped command gets to exit on [`shutdown`]'s SIGTERM.
#[cfg_attr(not(any(feature = "gnome", feature = "xvfb")), allow(dead_code))]
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Sends `sig` to the wrapped child, or parks it for main to send while the
/// child is being spawned (any thread can take a signal, so no mask closes
/// that window). Returns false if there is no child to send it to.
/// Async-signal-safe (does not allocate).
fn forward_to_child(sig: libc::c_int) -> bool {
    let child = CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // SAFETY: does not allocate
        unsafe { libc::kill(child, sig) };
        return true;
    }
    if child == SPAWNING {
        PENDING_SIG.store(sig, Ordering::SeqCst);
        // The spawn may have finished between the load and the store, with
        // main having already swapped the (then empty) pending signal out; it
        // won't look again. So whoever swaps a non-zero signal out delivers
        // it, and exactly one of us does.
        let child = CHILD.load(Ordering::SeqCst);
        if child > 0 {
            let sig = PENDING_SIG.swap(0, Ordering::SeqCst);
            if sig != 0 {
                // SAFETY: does not allocate
                unsafe { libc::kill(child, sig) };
            }
        }
        return true;
    }
    false
}

/// Async-signal-safe (does not allocate) terminating-signal handler: a stop
/// was asked for. If the child exists, forward the signal to it and return:
/// the main thread is blocked in `child.wait()`, which then returns and runs
/// the normal cleanup (i.e. we wait for the child to exit before exiting
/// ourselves). While the child is being spawned the signal is parked for main
/// to forward once it has the pid. With `-nowrap`, main is woken to clean up
/// (stop the stock X server, remove the socket). If main has already reaped
/// the command (or [`shutdown`] is exiting), it is on its way out and will
/// exit as for a stop. Only while still starting up (nothing to wait for) do
/// we clean up and exit here directly.
extern "C" fn handle_term(sig: libc::c_int) {
    let _ = STOP_SIG.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    if forward_to_child(sig) {
        return;
    }
    let pipe = NOWRAP_PIPE.load(Ordering::SeqCst);
    if pipe >= 0 {
        let byte = sig as u8;
        // SAFETY: does not allocate; a full pipe already holds a wake-up
        unsafe { libc::write(pipe, (&byte as *const u8).cast(), 1) };
        return;
    }
    if CHILD.load(Ordering::SeqCst) == CLOSED {
        return;
    }
    let path = SOCKET_PATH.load(Ordering::Acquire);
    if !path.is_null() {
        // SAFETY: does not allocate, path is a leaked C string
        unsafe { libc::unlink(path) };
    }
    // a stop before anything ran (a stock X server we started dies with us:
    // its parent-death signal; its files are ours to remove)
    for file in &ENGINE_FILES {
        let path = file.load(Ordering::Acquire);
        if !path.is_null() {
            // SAFETY: as above
            unsafe { libc::unlink(path) };
        }
    }
    let path = ENGINE_DIR.load(Ordering::Acquire);
    if !path.is_null() {
        // SAFETY: as above
        unsafe { libc::rmdir(path) };
    }
    childenv::withdraw();
    // SAFETY: does not allocate
    unsafe { libc::_exit(0) };
}

/// Registers a stock X server's private files (its authority file and RFB
/// socket) and the private directory they are in for removal when a stop
/// cuts our startup short.
#[cfg(feature = "xvfb")]
pub(crate) fn engine_files(files: &[&std::path::Path], dir: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let leak = |p: &std::path::Path| {
        CString::new(p.as_os_str().as_bytes()).map_or(ptr::null_mut(), CString::into_raw)
    };
    for (slot, file) in ENGINE_FILES.iter().zip(files) {
        slot.store(leak(file), Ordering::Release);
    }
    ENGINE_DIR.store(leak(dir), Ordering::Release);
}

/// Whether a stop was asked for (we got SIGTERM, SIGINT or SIGHUP). Parts
/// going down meanwhile (a stock X server a service manager signalled along
/// with us, say) are then expected, not a failure.
pub(crate) fn stop_requested() -> bool {
    STOP_SIG.load(Ordering::SeqCst) != 0
}

/// The signals that ask us to stop (see [`install_cleanup`]).
fn is_stop_signal(sig: i32) -> bool {
    matches!(sig, libc::SIGINT | libc::SIGTERM | libc::SIGHUP)
}

/// The status to exit with, once the wrapped command has ended (`None`:
/// there was none, `-nowrap`). What a service manager acts on, so:
/// - a stop someone asked for exits 0 when the command ended because of it
///   (it exited 0, or died of / exited 128 + one of the stop signals); a
///   command that reports a failure of its own on the way still exits with it;
/// - otherwise, if a part we need died ([`shutdown`]), its status;
/// - otherwise the command's own status, 128 + n if it was killed by signal n.
fn exit_status(status: Option<&io::Result<std::process::ExitStatus>>) -> i32 {
    decide_exit(stop_requested(), SHUTDOWN.load(Ordering::SeqCst), status)
}

/// [`exit_status`] given whether a stop was asked for and [`shutdown`]'s
/// status (0: none).
fn decide_exit(
    stopped: bool,
    forced: i32,
    status: Option<&io::Result<std::process::ExitStatus>>,
) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if !stopped && forced != 0 {
        return forced;
    }
    match status {
        None => 0,
        Some(Err(_)) => 1,
        Some(Ok(s)) => match (s.code(), s.signal()) {
            (Some(0), _) => 0,
            (Some(c), _) if stopped && is_stop_signal(c - 128) => 0,
            (Some(c), _) => c,
            (None, Some(sig)) if stopped && is_stop_signal(sig) => 0,
            (None, Some(sig)) => 128 + sig,
            (None, None) => 1,
        },
    }
}

/// Ends the process with status `code` because it can no longer do its job
/// (the stock X server is gone, the desktop backend failed), from any thread.
/// The wrapped command is stopped first, as a terminating signal would stop
/// it — a RealVNC server, for one, outlives its X server and would keep
/// restarting against a dead display, and a service manager would never see
/// a failure — and killed if it hasn't exited within [`SHUTDOWN_GRACE`]; main
/// then stops the stock X server and exits with `code`. Without a command
/// (`-nowrap`, or not started yet) we do that here at once. The first cause
/// wins; a stop that was asked for wins over all (see [`exit_status`]).
#[cfg_attr(not(any(feature = "gnome", feature = "xvfb")), allow(dead_code))]
pub(crate) fn shutdown(code: i32) -> ! {
    let _ = SHUTDOWN.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
    loop {
        if forward_to_child(libc::SIGTERM) {
            break;
        }
        match CHILD.compare_exchange(0, CLOSED, Ordering::SeqCst, Ordering::SeqCst) {
            // there is no child, and now there never will be: we exit
            Ok(_) => {
                let path = SOCKET_PATH.load(Ordering::Acquire);
                if !path.is_null() {
                    unsafe { libc::unlink(path) };
                }
                childenv::withdraw();
                #[cfg(feature = "xvfb")]
                xvfb::stop();
                process::exit(exit_status(None));
            }
            // main has reaped it (or is exiting for a stop) and exits itself
            Err(CLOSED) => loop {
                thread::park();
            },
            // it was being spawned or has just been: forward to it
            Err(_) => {}
        }
    }
    thread::sleep(SHUTDOWN_GRACE);
    let child = CHILD.load(Ordering::SeqCst);
    if child > 0 {
        crate::warning!(
            "the command didn't exit within {}s; killing it",
            SHUTDOWN_GRACE.as_secs()
        );
        unsafe { libc::kill(child, libc::SIGKILL) };
    }
    loop {
        thread::park();
    }
}

/// With `-nowrap`: blocks until a stop is asked for (see [`handle_term`]).
fn wait_for_stop() {
    let mut fds = [0; 2];
    // SAFETY: fds has room for two
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        // can't happen in practice; a stop then exits from the handler
        crate::warning!("can't make a pipe: {}", io::Error::last_os_error());
        loop {
            thread::park();
        }
    }
    let (rd, wr) = (fds[0], fds[1]);
    // the read end blocks; the handler's write must never block
    unsafe { libc::fcntl(rd, libc::F_SETFL, 0) };
    NOWRAP_PIPE.store(wr, Ordering::SeqCst);
    // (a stop that came before the pipe was there: the handler saw no pipe;
    // it may then also exit at once, with the same status)
    while !stop_requested() {
        let mut byte = 0u8;
        // SAFETY: one byte into a local
        let n = unsafe { libc::read(rd, (&mut byte as *mut u8).cast(), 1) };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            crate::warning!("waiting for a signal: {}", io::Error::last_os_error());
            loop {
                thread::park();
            }
        }
    }
}

/// Removes our X display's socket file, for an exit that skips main's cleanup
/// (a backend giving up before the wrapped command starts).
#[allow(dead_code)] // only the gnome backend exits that way
pub(crate) fn remove_display_socket() {
    let path = SOCKET_PATH.load(Ordering::Acquire);
    if !path.is_null() {
        // SAFETY: path is a leaked C string
        unsafe { libc::unlink(path) };
    }
}

/// Registers our X display's filesystem socket for removal on a terminating
/// signal (empty: none to remove).
fn install_cleanup(socket_path: &str) {
    if !socket_path.is_empty() {
        let leaked = CString::new(socket_path)
            .expect("socket path has no NUL")
            .into_raw();
        SOCKET_PATH.store(leaked, Ordering::Release);
    }
}

/// Installs the terminating-signal handler: first thing, so that a stop that
/// comes while we are still starting (resolving the X server program reads
/// it, which takes a moment) is a stop, exiting 0, not death by the signal.
fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        // This only affects us since the child will reset signal dispositions
        // on exec.
        sa.sa_sigaction = handle_term as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaction(sig, &sa, ptr::null_mut());
        }
    }
}

fn main() {
    // `xwlbridge run [...] CMD`: a tool on a running bridge's display (a program
    // that is itself called "run" is wrapped with `xwlbridge -- run`)
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|a| a == "run") {
        childenv::run_main(args.collect());
    }
    let mut config = Config::parse();
    install_signal_handlers();
    LOG_LEVEL.store(
        if config.quiet {
            0
        } else if config.verbose {
            2
        } else {
            1
        },
        Ordering::Relaxed,
    );
    CLIP_TRACE.store(config.cliptrace, Ordering::Relaxed);
    // Take the command out before `config` moves into the Server (which doesn't
    // need it). Without `-nowrap` a command is required.
    let command = std::mem::take(&mut config.command);
    if command.is_empty() && !config.nowrap {
        crate::warning!("no command given (use -nowrap to run without one); try -help");
        process::exit(2);
    }
    // -uidisplay (by default on for RealVNC in a desktop session): resolved
    // now, so a bad one stops us before anything starts
    let realvnc = command.first().is_some_and(|p| uidisplay::is_realvnc(p));
    let ui_choice = uidisplay::choose(config.uidisplay.as_deref(), realvnc).unwrap_or_else(|e| {
        crate::warning!("{e}");
        process::exit(2);
    });

    let xwayland_clip = xwayland_clipboard(&config);

    bridge::profile::start(config.profile);

    // With -engine xvfb/xvnc a stock X server is the X server (started once the
    // backend knows the screen size); otherwise we are.
    let xvfb_engine = config.engine != config::EngineType::Builtin;
    #[cfg(not(feature = "xvfb"))]
    if xvfb_engine {
        crate::warning!(
            "-engine xvfb/xvnc/xwayland needs xwlbridge built with the \"xvfb\" feature"
        );
        process::exit(2);
    }
    if config.xserver.is_some() && !xvfb_engine {
        crate::warning!("-xserver is for -engine xvfb, xvnc or xwayland");
        process::exit(2);
    }
    // the stock engines' display options do nothing on the built-in engine
    for (set, flag) in [
        (config.showwindows, "-showwindows"),
        (config.showscreen, "-showscreen"),
        (config.a11ybus, "-a11ybus"),
        (config.reverseinput, "-reverseinput"),
    ] {
        if set && !xvfb_engine {
            crate::warning!(
                "{flag} is for -engine xvfb, xvnc or xwayland; ignored with the built-in engine"
            );
        }
    }
    // which X server program: resolved now, so a missing (or refused) one stops
    // us before anything starts
    #[cfg(feature = "xvfb")]
    let xserver = xvfb_engine.then(|| {
        let path = std::env::var_os("PATH");
        xvfb::resolve_server(
            config.engine,
            config.xserver.as_deref().map(std::ffi::OsStr::new),
            path.as_deref(),
        )
        .unwrap_or_else(|e| {
            crate::warning!("{e}");
            process::exit(1);
        })
    });
    #[cfg_attr(not(feature = "xvfb"), allow(unused_mut))]
    let (mut display, listeners) = if xvfb_engine {
        (0, Vec::new())
    } else {
        match bind_display(config.display) {
            Ok(v) => v,
            Err(e) => {
                crate::warning!("failed to bind an X display: {e}");
                process::exit(1);
            }
        }
    };
    #[cfg_attr(not(feature = "xvfb"), allow(unused_mut))]
    let mut socket_path = format!("/tmp/.X11-unix/X{display}");
    // Clean up the filesystem socket on a terminating signal too, not just on the
    // normal child-exit path below.
    // (A stock engine owns its own socket; ours is a placeholder then, which
    // must never be unlinked — it could be a real X server's.)
    install_cleanup(if xvfb_engine { "" } else { &socket_path });

    let geom = config.geometry.unwrap_or_default();
    let displayfd = config.displayfd;
    let nowrap = config.nowrap;
    let dynres = config.dynres;
    if config.dynreskeep && !dynres {
        crate::warning!("-dynreskeep does nothing without -dynres");
    }
    let input = Arc::new(Input::new(geom.width, geom.height, config.input_policy()));
    if !xvfb_engine {
        // the stock engines' agent keeps their X server's lingering keys
        input.spawn_linger();
    }
    let atoms = bridge::x11::atom::Atoms::new(config.xkb());
    let server = Arc::new(Server {
        config,
        atoms: Mutex::new(atoms),
        screen: Mutex::new(Screen::new(geom.width, geom.height)),
        input,
        events: bridge::event::EventSink::default(),
        clipboard: bridge::clipboard::Clipboard::default(),
        framebuffer: Arc::new(bridge::capture::Framebuffer::default()),
        damage: bridge::damage::DamageSink::default(),
        cursor: bridge::cursor::CursorState::default(),
        dynres: bridge::dynres::DynRes::default(),
        keymap: Mutex::new(None),
        keymap_text: Mutex::new(None),
        keymap_gen: std::sync::atomic::AtomicU64::new(0),
        keymap_group: std::sync::atomic::AtomicU32::new(0),
        xwayland_clip,
    });

    bridge::backend::spawn(server.clone());

    // what the wrapped command needs to reach a stock engine (nothing while
    // our own user's clients are let in by uid)
    #[cfg(feature = "xvfb")]
    let mut engine_xauthority = None;
    #[cfg(feature = "xvfb")]
    let mut engine_a11y_bus = None;
    #[cfg(feature = "xvfb")]
    if let Some(program) = &xserver {
        match xvfb::start(&server, program) {
            Ok(engine) => {
                display = engine.display;
                // Xvfb owns (and cleans up) its socket; ours is not in use
                socket_path = String::new();
                engine_xauthority = engine.xauthority;
                engine_a11y_bus = engine.a11y_bus;
                xvfb::spawn_agent(server.clone(), display, engine.auth);
            }
            Err(e) => {
                crate::warning!("{e}");
                process::exit(1);
            }
        }
    }

    for listener in listeners {
        let server = server.clone();
        thread::spawn(move || accept_loop(listener, server));
    }

    if let Some(fd) = displayfd {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        let mut f = unsafe { fs::File::from_raw_fd(fd as i32) };
        if writeln!(f, "{display}").is_err() {
            crate::warning!("failed to write display number to fd {fd}");
        }
    }

    // for `xwlbridge run` (with the cookie, when the engine couldn't let our
    // own user in by uid)
    #[cfg(feature = "xvfb")]
    childenv::announce(display, engine_xauthority.as_deref());
    #[cfg(not(feature = "xvfb"))]
    childenv::announce(display, None);

    if nowrap {
        crate::log!("running without a command (-nowrap); waiting for a signal");
        wait_for_stop();
        // ours to finish, unless a part died meanwhile and shutdown() is
        // already exiting (as for a stop: see exit_status)
        if CHILD
            .compare_exchange(0, CLOSED, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            loop {
                thread::park();
            }
        }
        if !socket_path.is_empty() {
            let _ = fs::remove_file(&socket_path);
        }
        // undo what the backend changed on the compositor (-dynres, -pwrmgr)
        server.dynres.finish(std::time::Duration::from_secs(3));
        childenv::withdraw();
        #[cfg(feature = "xvfb")]
        xvfb::stop();
        process::exit(exit_status(None));
    }

    // Audio needs no special handling: vncserver-x11-core locates the
    // PulseAudio/PipeWire server itself via the runtime path (/run/user/$UID/pulse
    // etc.) and reads the cookie from disk, so we don't advertise the X11
    // PULSE_SERVER/PULSE_COOKIE root-window properties (module-x11-publish style).
    //
    // Point the child at our X display, and keep it and what it starts off the
    // compositor: unsetting WAYLAND_DISPLAY isn't enough (libwayland then
    // connects to wayland-0, and GTK does), so every toolkit is pinned to X11
    // as well (see childenv.rs).
    let (program, program_args) = command.split_first().expect("checked non-empty above");
    let mut program = program.clone();
    // RealVNC's UI on the desktop's display, and never crashing on the
    // built-in engine's (see uidisplay.rs): its setuid launcher would drop the
    // preload hook, so start the core it launches directly
    let ui_setup = setup_ui(
        ui_choice,
        realvnc,
        display,
        &mut program,
        program_args,
        &server,
    );
    let mut cmd = Command::new(&program);
    cmd.args(program_args);
    childenv::on_display(&mut cmd, display);
    // a stock engine lets our own user in without a cookie; only if it
    // couldn't does the command need the authority file
    #[cfg(feature = "xvfb")]
    if let Some(file) = &engine_xauthority {
        cmd.env("XAUTHORITY", file);
    }
    // -a11ybus: the display's own accessibility bus (GTK 4 finds it only here)
    #[cfg(feature = "xvfb")]
    if let Some(addr) = &engine_a11y_bus {
        cmd.env("AT_SPI_BUS_ADDRESS", addr);
    }
    if let Some(ui) = &ui_setup {
        let placed = ui
            .placed
            .as_ref()
            .map(|(d, gate)| (d, gate.as_ref().map(|g| g.path())));
        uidisplay::apply(&mut cmd, &ui.hook, placed);
    }
    if dynres {
        // what the dynres hook (see dynres/) looks for before it patches out
        // RealVNC's "-virtual" check and lets the feature turn on
        cmd.env("FORCE_DYNRES", "1");
    }
    if CHILD
        .compare_exchange(0, SPAWNING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // shutdown() got there first: it exits, and the command never starts
        loop {
            thread::park();
        }
    }
    let spawned = cmd.spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            CHILD.store(CLOSED, Ordering::SeqCst);
            crate::warning!("failed to spawn {program:?}: {e}");
            let _ = fs::remove_file(&socket_path);
            #[cfg(feature = "xvfb")]
            xvfb::stop();
            childenv::withdraw();
            process::exit(1);
        }
    };
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    // RealVNC's UI: move it when its display answers (or back if it stops),
    // and restart it if its tray icon helper dies (see uidisplay.rs)
    let gate_path = ui_setup
        .as_ref()
        .and_then(|ui| ui.placed.as_ref()?.1.as_ref().map(|g| g.path().to_owned()));
    if let Some(UiSetup {
        placed: Some((ui, gate)),
        ..
    }) = ui_setup
    {
        let server = server.clone();
        uidisplay::watch(child.id(), ui, gate, move || server.damage.active());
    }
    // a terminating signal that landed during the spawn goes to the child now,
    // as it would have a moment later (see handle_term for why the swap)
    let sig = PENDING_SIG.swap(0, Ordering::SeqCst);
    if sig != 0 {
        unsafe { libc::kill(child.id() as i32, sig) };
    }

    let status = child.wait();
    // the pid is reaped and could be reused; a signal from here on is only
    // noted (a stop, for the exit status): we are on our way out
    CHILD.store(CLOSED, Ordering::SeqCst);
    if let Err(e) = &status {
        crate::warning!("failed to wait for child: {e}");
    }
    let _ = fs::remove_file(&socket_path);
    // the command may have ended because the stock X server went away by
    // itself (an X client exits when its display goes) before the agent's
    // supervisor, waiting out its settle time, called that a loss: it still
    // is one, and a service manager must hear so
    #[cfg(feature = "xvfb")]
    if xvfb::lost_by_itself() {
        crate::warning!("xvfb: the X server is gone; exiting");
        let _ = SHUTDOWN.compare_exchange(0, EXIT_ENGINE_LOST, Ordering::SeqCst, Ordering::SeqCst);
    }
    // a resolution change (-dynres) or a woken screen (-pwrmgr) the backend
    // would put back once nobody watches: nobody will now
    server.dynres.finish(std::time::Duration::from_secs(3));
    if let Some(gate) = gate_path {
        let _ = fs::remove_file(gate);
    }
    childenv::withdraw();
    // process::exit skips destructors: stop the stock X server explicitly
    // (its parent-death signal alone is not reliable everywhere, e.g. SELinux)
    #[cfg(feature = "xvfb")]
    xvfb::stop();
    process::exit(exit_status(Some(&status)));
}

/// How long `-uidisplay` waits for its display to answer before starting
/// RealVNC with its UI on the engine display (a session's Xwayland starts on
/// demand, and the unit may start early; the UI moves once it answers).
const UI_DISPLAY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How RealVNC is run: with the hook (`hook`), and RealVNC's UI on the
/// desktop's display (`placed`: the display and the gate that says whether to
/// use it, see uidisplay.rs).
struct UiSetup {
    hook: std::path::PathBuf,
    placed: Option<(uidisplay::UiDisplay, Option<uidisplay::Gate>)>,
}

/// Sets up `-uidisplay`, and the hook RealVNC needs on the built-in engine:
/// checks the UI display, writes the hook and the gate, swaps RealVNC's setuid
/// launcher for its core. `None`: the command runs as it is (with a warning
/// where that is a problem). `display` is the engine's display number.
fn setup_ui(
    choice: uidisplay::Choice,
    realvnc: bool,
    display: u32,
    program: &mut std::ffi::OsString,
    args: &[std::ffi::OsString],
    server: &Server,
) -> Option<UiSetup> {
    let builtin = server.config.engine == config::EngineType::Builtin;
    let wrapped = uidisplay::wrapped_launcher(program, args);
    let mut ui = match choice {
        uidisplay::Choice::Display(ui) => Some(ui),
        uidisplay::Choice::Off(why) => {
            if let Some(why) = why {
                crate::log!(
                    "RealVNC's UI stays on the engine display, out of sight ({why}); -uidisplay names a display"
                );
            }
            None
        }
    };
    if let Some(u) = &ui
        && uidisplay::same_display(u, display)
    {
        crate::warning!(
            "-uidisplay: {} is xwlbridge's own display; RealVNC's UI stays on it, out of sight",
            u.display.to_string_lossy()
        );
        ui = None;
    }
    if builtin && wrapped {
        crate::warning!(
            "{program:?} runs vncserver-x11 itself, whose setuid launcher drops the hook RealVNC's UI needs: on the \
             built-in engine's display (no XKB) the UI then crashes, and RealVNC restarts it every 2 s, and it can't \
             go on the desktop; give vncserver-x11 as the command"
        );
    } else if wrapped && ui.is_some() {
        crate::warning!(
            "-uidisplay: {program:?} runs vncserver-x11 itself, whose setuid launcher drops the UI hook, so RealVNC's \
             UI would stay hidden on our display; give vncserver-x11 as the command"
        );
    }
    // the hook moves the UI (if there is a display for it) and, on the
    // built-in engine, keeps it alive where it stays on the engine display
    if ui.is_none() && !(builtin && realvnc) {
        return None;
    }
    let usable = ui.as_ref().map(|u| {
        let name = u.display.to_string_lossy();
        uidisplay::wait_usable(u, UI_DISPLAY_WAIT)
            .inspect_err(|e| {
                crate::warning!(
                    "-uidisplay: can't use X display {name} for RealVNC's UI yet ({e}); it starts on the engine display \
                     (out of sight) and moves to {name} once that answers. Check DISPLAY/XAUTHORITY, or start xwlbridge \
                     in the desktop session"
                )
            })
            .ok()
    });
    let hook = match uidisplay::install_hook() {
        Ok(hook) => hook,
        Err(e) if builtin => {
            crate::warning!(
                "can't write RealVNC's UI hook ({e}); running {program:?} without it: RealVNC's UI can't go on the \
                 desktop, and on the built-in engine's display (no XKB) it crashes, and RealVNC restarts it every 2 s"
            );
            return None;
        }
        Err(e) => {
            crate::warning!(
                "-uidisplay: can't write the UI hook ({e}); running {program:?} without -uidisplay"
            );
            return None;
        }
    };
    let path = std::env::var_os("PATH");
    let core = if let Some(core) = uidisplay::core_for(program, path.as_deref()) {
        crate::log!(
            "running {} directly (the setuid launcher would drop the UI hook), without RealVNC's root helper: remote \
             printing then needs the lpadmin group, and system authentication works for this user's own account only",
            core.display()
        );
        *program = core.into_os_string();
        true
    } else {
        if uidisplay::is_setid(program, path.as_deref()) {
            crate::warning!(
                "{program:?} is set-user/group-ID, which keeps the UI hook from what it starts"
            );
        }
        std::path::Path::new(program)
            .file_name()
            .is_some_and(|n| n == "vncserver-x11-core")
    };
    let Some(ui) = ui else {
        crate::log!(
            "RealVNC's UI stays on the built-in engine's display, out of sight (the hook keeps it from crashing there)"
        );
        return Some(UiSetup { hook, placed: None });
    };
    let name = ui.display.to_string_lossy().into_owned();
    if core {
        let params = uidisplay::command_line_params(args);
        if !params.is_empty() {
            crate::warning!(
                "-uidisplay: RealVNC forgets parameters given on its command line ({}) as soon as its Options dialog \
                 applies a change; keep them in ~/.vnc/config.d/vncserver-x11 (what the dialog edits) instead",
                params.join(", ")
            );
        }
    }
    if server.config.noprimary {
        crate::warning!(
            "-uidisplay: -noprimary covers the bridge's own clipboard only; RealVNC's UI on {name} reads and sets the \
             desktop's PRIMARY selection itself (RealVNC's SendPrimary=0 does not stop it; SendCutText=0 turns off all \
             clipboard to viewers)"
        );
    }
    let usable = usable.flatten();
    let dir = hook.parent().unwrap_or(std::path::Path::new("."));
    let gate = match uidisplay::Gate::create(dir, usable.is_some()) {
        Ok(gate) => Some(gate),
        Err(e) => {
            crate::warning!(
                "-uidisplay: can't write the UI gate in {} ({e}); RealVNC's UI goes to {name} whether it answers or not",
                dir.display()
            );
            None
        }
    };
    match usable {
        Some(uidisplay::Probe { tray: Some(owner) }) => crate::log!(
            "-uidisplay: RealVNC's UI goes on X display {name}; its status icon goes in the system tray there \
             (window {owner:#x})"
        ),
        Some(uidisplay::Probe { tray: None }) => {
            crate::log!("-uidisplay: RealVNC's UI goes on X display {name}");
            // the desktop's tray (GNOME: an extension) may come up after us
            let ui = ui.clone();
            thread::spawn(move || {
                thread::sleep(std::time::Duration::from_secs(20));
                if let Ok(uidisplay::Probe { tray: None }) =
                    uidisplay::probe(&ui, std::time::Duration::from_secs(5))
                {
                    crate::log!(
                        "-uidisplay: no system tray on X display {name}, so RealVNC's status icon can't show (on GNOME, \
                         enable the AppIndicator extension, e.g. ubuntu-appindicators@ubuntu.com or \
                         appindicatorsupport@rgcjonas.gmail.com, with its legacy tray on); `vncserver-x11 -showstatus` \
                         opens RealVNC's status window"
                    );
                }
            });
        }
        None => {}
    }
    Some(UiSetup {
        hook,
        placed: Some((ui, gate)),
    })
}

/// Where the stock engines' clipboard goes: relayed with the desktop's
/// Xwayland (`-clipboard xwayland`, and `auto` on GNOME when the desktop's X
/// display answers), or `None` for the backend's own clipboard.
fn xwayland_clipboard(config: &Config) -> Option<uidisplay::UiDisplay> {
    use config::{BackendType, ClipboardType, EngineType};
    let wanted = match config.clipboard {
        ClipboardType::Xwayland => true,
        ClipboardType::Auto => {
            let gnome = match config.backend {
                BackendType::Gnome => true,
                BackendType::Wlr => false,
                BackendType::Auto => std::env::var("XDG_CURRENT_DESKTOP")
                    .unwrap_or_default()
                    .split(':')
                    .any(|d| d.eq_ignore_ascii_case("GNOME")),
            };
            gnome && config.engine != EngineType::Builtin
        }
        _ => false,
    };
    if !wanted {
        return None;
    }
    if config.engine == EngineType::Builtin {
        crate::warning!(
            "-clipboard xwayland needs -engine xvfb or xvnc; using the session clipboard"
        );
        return None;
    }
    let arg = config.xwayland.as_deref().unwrap_or("auto");
    match uidisplay::resolve(arg) {
        // our own display, or a stale DISPLAY pointing at one of ours, would relay with itself
        Ok(d)
            if config.display.is_some_and(|n| {
                let s = d.display.to_string_lossy();
                s.trim_start_matches("unix")
                    .trim_start_matches(':')
                    .split('.')
                    .next()
                    == Some(&n.to_string())
            }) =>
        {
            crate::warning!(
                "-xwayland {}: that is the bridge's own display; using the session clipboard",
                d.display.to_string_lossy()
            );
            None
        }
        #[cfg(feature = "xvfb")]
        Ok(d) if config.clipboard == ClipboardType::Auto && !xvfb::xrelay::reachable(&d) => {
            crate::log!(
                "the desktop's X display {} doesn't answer; using the session clipboard",
                d.display.to_string_lossy()
            );
            None
        }
        Ok(d) => Some(d),
        Err(_) if config.clipboard == ClipboardType::Auto => {
            crate::log!(
                "no desktop X display to relay the clipboard with (no DISPLAY); using the session clipboard"
            );
            None
        }
        Err(e) => {
            crate::warning!("-clipboard xwayland: {e}");
            process::exit(2);
        }
    }
}

fn accept_loop(listener: UnixListener, server: Arc<Server>) {
    // SAFETY: getuid cannot fail
    let uid = unsafe { libc::getuid() };
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // There is no X authentication, and the abstract socket has no
                // filesystem permissions either, so the peer's uid is all that
                // keeps other local users off the display (and its screen,
                // input and clipboard). Only our own user, and root, get in.
                match peer_uid(&stream) {
                    Ok(peer) if peer == uid || peer == 0 => {}
                    Ok(peer) => {
                        crate::warning!("rejecting X connection from uid {peer}");
                        continue;
                    }
                    Err(e) => {
                        crate::warning!("rejecting X connection: cannot get peer credentials: {e}");
                        continue;
                    }
                }
                let server = server.clone();
                thread::spawn(move || {
                    if let Err(e) = Connection::new(stream, server).and_then(Connection::run) {
                        crate::vlog!("connection closed: {e}");
                    }
                });
            }
            Err(e) => crate::warning!("accept failed: {e}"),
        }
    }
}

/// The uid of the process at the other end of a unix socket.
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills in a ucred, and we pass its size
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

fn bind_display(forced: Option<u32>) -> io::Result<(u32, Vec<UnixListener>)> {
    let _ = fs::create_dir_all("/tmp/.X11-unix");
    let candidates: Vec<u32> = match forced {
        Some(n) => vec![n],
        None => (20..100).collect(),
    };
    for n in candidates {
        let path = format!("/tmp/.X11-unix/X{n}");
        match bind_sockets(&path) {
            Ok(listeners) => {
                crate::log!("serving X display :{n}");
                return Ok((n, listeners));
            }
            Err(e) if forced.is_some() => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("display :{n} is unavailable: {e}"),
                ));
            }
            Err(_) => continue,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no free X display",
    ))
}

/// Binds one display's filesystem socket and its abstract twin. A socket file
/// nothing answers on (left by a killed server) is reclaimed; anything else in
/// the way, or another server holding the abstract name, means the display is
/// in use.
fn bind_sockets(path: &str) -> io::Result<Vec<UnixListener>> {
    match fs::symlink_metadata(path) {
        Err(_) => {}
        Ok(m) if !m.file_type().is_socket() => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{path} exists and is not a socket"),
            ));
        }
        Ok(_) => match UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another server is listening",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                crate::log!("removing stale socket {path}");
                fs::remove_file(path)?;
            }
            // permission denied and the like: someone else's socket
            Err(e) => return Err(e),
        },
    }
    let mut listeners = vec![UnixListener::bind(path)?];
    // libxcb tries the abstract name first, so another server holding it owns
    // the display whatever the filesystem says (abstract names die with their
    // process, so this one is never stale)
    match SocketAddr::from_abstract_name(path.as_bytes())
        .and_then(|addr| UnixListener::bind_addr(&addr))
    {
        Ok(l) => listeners.push(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            let _ = fs::remove_file(path);
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another server holds the abstract socket",
            ));
        }
        Err(_) => {} // no abstract namespace; the filesystem socket suffices
    }
    Ok(listeners)
}

#[cfg(test)]
mod tests {
    use super::decide_exit;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn exited(code: i32) -> Option<io::Result<ExitStatus>> {
        Some(Ok(ExitStatus::from_raw(code << 8)))
    }
    fn killed(sig: i32) -> Option<io::Result<ExitStatus>> {
        Some(Ok(ExitStatus::from_raw(sig)))
    }

    #[test]
    fn a_stop_that_was_asked_for_exits_0() {
        // the command ended because of the stop: whichever way it says so
        for st in [
            exited(0),
            killed(libc::SIGTERM),
            killed(libc::SIGINT),
            killed(libc::SIGHUP),
            exited(143),
            exited(130),
            exited(129),
        ] {
            assert_eq!(decide_exit(true, 0, st.as_ref()), 0, "{st:?}");
        }
        // -nowrap
        assert_eq!(decide_exit(true, 0, None), 0);
        // a part that went down during the stop (a stock X server signalled
        // along with us) is no failure
        assert_eq!(decide_exit(true, 69, exited(0).as_ref()), 0);
        assert_eq!(decide_exit(true, 69, None), 0);
        assert_eq!(decide_exit(true, 70, killed(libc::SIGTERM).as_ref()), 0);
    }

    #[test]
    fn a_failure_during_a_stop_is_still_reported() {
        assert_eq!(decide_exit(true, 0, exited(3).as_ref()), 3);
        assert_eq!(
            decide_exit(true, 0, killed(libc::SIGSEGV).as_ref()),
            128 + libc::SIGSEGV
        );
        assert_eq!(
            decide_exit(true, 0, killed(libc::SIGKILL).as_ref()),
            128 + libc::SIGKILL
        );
        assert_eq!(
            decide_exit(true, 0, Some(Err(io::Error::other("x"))).as_ref()),
            1
        );
    }

    #[test]
    fn without_a_stop_the_command_or_the_lost_part_decides() {
        assert_eq!(decide_exit(false, 0, exited(0).as_ref()), 0);
        assert_eq!(decide_exit(false, 0, exited(3).as_ref()), 3);
        // killed by someone else: a failure, with the signal in it
        assert_eq!(decide_exit(false, 0, killed(libc::SIGTERM).as_ref()), 143);
        assert_eq!(decide_exit(false, 0, killed(libc::SIGSEGV).as_ref()), 139);
        // a part we need died: its status, whatever the command did when we
        // stopped it
        assert_eq!(decide_exit(false, 69, exited(0).as_ref()), 69);
        assert_eq!(decide_exit(false, 69, killed(libc::SIGTERM).as_ref()), 69);
        assert_eq!(decide_exit(false, 70, exited(3).as_ref()), 70);
        assert_eq!(decide_exit(false, 69, None), 69);
    }
}
