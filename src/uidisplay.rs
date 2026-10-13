//! `-uidisplay`: RealVNC's own UI process on another X display than the one
//! it remotes — typically the GNOME session's Xwayland, where its tray icon
//! (shown by the AppIndicator extension's legacy tray), status dialog,
//! Options and Information Center, connection notices and prompts, chat and
//! file transfer dialogs show up as ordinary windows, and its clipboard rides
//! mutter's X ⇄ Wayland clipboard sync. On the engine display (offscreen,
//! behind the input shield with the stock engines) nobody can see or use them.
//! It is the default for RealVNC (`vncserver-x11`) started in a desktop
//! session (a local `DISPLAY`); `-uidisplay none` turns it off.
//!
//! RealVNC has no parameter for it: its core starts `vncserverui` itself, by
//! absolute path, with `DISPLAY` set to the remoted display. So the wrapped
//! command gets a preload hook (uihook/uihook.rs, embedded at build time)
//! that, in `vncserverui` alone, swaps `DISPLAY`/`XAUTHORITY` before `main`.
//! The setuid `vncserver-x11` launcher would drop `LD_PRELOAD` (glibc's
//! secure-execution mode), so it is replaced by the core it launches,
//! `vncserver-x11-core`, started directly — which then runs without RealVNC's
//! root helper (used for PAM system authentication via a root process and for
//! adding the viewer's printer queue).
//!
//! The UI must never run on the built-in engine's display as RealVNC leaves
//! it: that server has no XKEYBOARD, and `vncserverui` segfaults on the NULL
//! keyboard name it gets there, every 2 s, for as long as the server runs
//! (each crash also runs the system's crash reporter as root). The hook
//! also fixes that (see uihook.rs), so with the built-in engine RealVNC always
//! runs through it, with or without a UI display.
//!
//! The UI display is checked ([`probe`]) before RealVNC starts: reachable with
//! the authority we would hand RealVNC, and whether something owns the system
//! tray there. The hook moves the UI only while a gate file ([`Gate`]) says
//! the display is usable; [`watch`] keeps that file current — it moves the UI
//! to the display once it answers (a session's Xwayland may come late) and
//! back to the engine display (hidden, but working) if the UI keeps exiting
//! there — and keeps RealVNC's tray icon alive.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{env, fs};

use x11rb_protocol::connect::Connect;
use x11rb_protocol::errors::ConnectError;
use x11rb_protocol::parse_display::{ConnectAddress, ParsedDisplay, parse_display};
use x11rb_protocol::protocol::xproto::{
    GetSelectionOwnerReply, GetSelectionOwnerRequest, InternAtomReply, InternAtomRequest,
};
use x11rb_protocol::x11_utils::{Request, TryParse};

/// The hook, for the target this was built for.
static HOOK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libxwlrvnc_uihook.so"));

/// Where RealVNC's UI goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiDisplay {
    pub display: OsString,
    /// The display's authority file, if it needs one we know of.
    pub xauthority: Option<OsString>,
}

/// Resolves `-uidisplay`: a display name, or `auto` for the X display xwlrvnc
/// itself was started with (in a GNOME session, its Xwayland). The authority
/// file is xwlrvnc's `XAUTHORITY`, or else mutter's Xwayland cookie in the
/// runtime directory.
pub fn resolve(arg: &str) -> Result<UiDisplay, String> {
    let display = if arg.eq_ignore_ascii_case("auto") {
        match env::var_os("DISPLAY") {
            Some(d) if !d.is_empty() => d,
            _ => {
                return Err("-uidisplay auto: xwlrvnc has no DISPLAY to put RealVNC's UI on (run it from the \
                            desktop session, or name the display: -uidisplay :0)"
                    .into());
            }
        }
    } else {
        OsString::from(arg)
    };
    let xauthority = env::var_os("XAUTHORITY")
        .filter(|a| !a.is_empty())
        .or_else(|| env::var_os("XDG_RUNTIME_DIR").and_then(|dir| mutter_cookie(Path::new(&dir))));
    Ok(UiDisplay { display, xauthority })
}

/// What `-uidisplay` comes to, before anything starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// RealVNC's UI goes on this display (named, `auto`, or the default).
    Display(UiDisplay),
    /// The UI stays on the engine display: `-uidisplay none`, a command that is
    /// not RealVNC's, or (with the reason) the default outside a desktop session.
    Off(Option<String>),
}

/// Resolves `-uidisplay`: `arg` is its value (`None`: not given), `realvnc`
/// whether the command is RealVNC's server. Not given, RealVNC's UI goes on
/// the X display xwlrvnc was started with, if that is a local one (in a
/// desktop session: its Xwayland), as with `auto`. `Err` only for a value
/// given explicitly.
pub fn choose(arg: Option<&str>, realvnc: bool) -> Result<Choice, String> {
    Ok(match arg {
        Some(a) if a.eq_ignore_ascii_case("none") => Choice::Off(None),
        Some(a) => Choice::Display(resolve(a)?),
        None if !realvnc => Choice::Off(None),
        None => match env::var_os("DISPLAY").filter(|d| !d.is_empty()) {
            None => Choice::Off(Some("xwlrvnc has no DISPLAY: not started in a desktop session".into())),
            Some(d) if !is_local(&d) => {
                Choice::Off(Some(format!("DISPLAY {} is not a local display", d.to_string_lossy())))
            }
            Some(_) => Choice::Display(resolve("auto")?),
        },
    })
}

/// Whether the command is RealVNC Server's (`vncserver-x11` or its core).
pub fn is_realvnc(program: &OsStr) -> bool {
    Path::new(program).file_name().is_some_and(|n| n == "vncserver-x11" || n == "vncserver-x11-core")
}

/// Parses a display name as Xlib does, which also takes `unix:N` for the
/// local display N (x11rb reads it as a socket path).
fn parse(display: &str) -> Result<ParsedDisplay, String> {
    let local = display.strip_prefix("unix:").filter(|n| n.starts_with(|c: char| c.is_ascii_digit()));
    let name = local.map(|n| format!(":{n}"));
    parse_display(Some(name.as_deref().unwrap_or(display))).map_err(|e| format!("bad display name {display:?}: {e}"))
}

/// Whether `display` names an X server on this machine reached through a
/// unix socket (`:0`, `unix:0`, a socket path), not over the network.
pub fn is_local(display: &OsStr) -> bool {
    display
        .to_str()
        .and_then(|d| parse(d).ok())
        .is_some_and(|p| p.protocol.as_deref() == Some("unix") || (p.host.is_empty() && p.protocol.is_none()))
}

/// Whether `ui` is X display number `n` of this machine (xwlrvnc's own).
pub fn same_display(ui: &UiDisplay, n: u32) -> bool {
    let Some(p) = ui.display.to_str().and_then(|d| parse(d).ok()) else {
        return false;
    };
    match p.protocol.as_deref() {
        Some("unix") => Path::new(&p.host) == Path::new(&format!("/tmp/.X11-unix/X{n}")),
        None if p.host.is_empty() => u32::from(p.display) == n,
        _ => false,
    }
}

/// The newest `.mutter-Xwaylandauth.*` in `dir`: mutter's cookie for its
/// Xwayland, which GNOME puts in the session's XAUTHORITY.
fn mutter_cookie(dir: &Path) -> Option<OsString> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().as_bytes().starts_with(b".mutter-Xwaylandauth."))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max()
        .map(|(_, p)| p.into_os_string())
}

/// What [`probe`] found on the UI display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The window owning `_NET_SYSTEM_TRAY_S<screen>` (the system tray that
    /// embeds RealVNC's status icon), if any.
    pub tray: Option<u32>,
}

/// Connects to the UI display as RealVNC's UI will (same display name, same
/// authority file) and asks who owns the system tray. `Err` says why the
/// display is unusable.
pub fn probe(ui: &UiDisplay, timeout: Duration) -> Result<Probe, String> {
    let name = ui.display.to_str().ok_or("the display name is not UTF-8")?;
    let parsed = parse(name)?;
    let mut last = format!("no local socket for {name}");
    for addr in parsed.connect_instruction() {
        // Only local displays are checked; RealVNC's UI on a TCP display is
        // the user's business (and needs xhost/xauth we can't see).
        let ConnectAddress::Socket(path) = addr else {
            continue;
        };
        let stream = match SocketAddr::from_abstract_name(path.as_bytes())
            .and_then(|a| UnixStream::connect_addr(&a))
            .or_else(|_| UnixStream::connect(&path))
        {
            Ok(s) => s,
            Err(e) => {
                last = format!("can't connect to {name} ({path}): {e}");
                continue;
            }
        };
        stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let cookie = ui
            .xauthority
            .as_deref()
            .and_then(|f| fs::read(f).ok())
            .and_then(|data| find_cookie(&data, &hostname(), parsed.display));
        return handshake(stream, cookie, parsed.screen).map_err(|e| format!("{name}: {e}"));
    }
    Err(last)
}

/// [`probe`]s until the display answers or `total` has passed (a desktop
/// session's Xwayland may still be starting); the last error otherwise.
pub fn wait_usable(ui: &UiDisplay, total: Duration) -> Result<Probe, String> {
    let end = std::time::Instant::now() + total;
    loop {
        match probe(ui, Duration::from_secs(5)) {
            Ok(p) => return Ok(p),
            Err(e) if std::time::Instant::now() >= end => return Err(e),
            Err(e) => crate::vlog!("-uidisplay: {e}; trying again"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The connection setup and two requests (InternAtom, GetSelectionOwner) on
/// an open stream to the X server.
fn handshake(
    mut stream: impl Read + Write,
    cookie: Option<(Vec<u8>, Vec<u8>)>,
    screen: u16,
) -> Result<Probe, String> {
    let (auth_name, auth_data) = cookie.unwrap_or_default();
    let (mut connect, setup_request) = Connect::with_authorization(auth_name, auth_data);
    stream.write_all(&setup_request).map_err(|e| format!("sending the connection setup: {e}"))?;
    loop {
        let n = stream
            .read(connect.buffer())
            .map_err(|e| format!("no answer to the connection setup: {e}"))?;
        if n == 0 {
            return Err("the X server closed the connection during setup".into());
        }
        if connect.advance(n) {
            break;
        }
    }
    match connect.into_setup() {
        Ok(_) => {}
        Err(ConnectError::SetupFailed(f)) => {
            return Err(format!("refused the connection: {}", String::from_utf8_lossy(&f.reason).trim_end()));
        }
        Err(ConnectError::SetupAuthenticate(_)) => return Err("asked for more authentication".into()),
        Err(e) => return Err(format!("connection setup failed: {e}")),
    }
    let name = format!("_NET_SYSTEM_TRAY_S{screen}");
    let intern = InternAtomRequest { only_if_exists: true, name: name.as_bytes().into() };
    stream
        .write_all(&<InternAtomRequest as Request>::serialize(intern, 0).0)
        .map_err(|e| format!("sending InternAtom: {e}"))?;
    let reply = read_reply(&mut stream)?;
    let Ok((atom, _)) = InternAtomReply::try_parse(&reply) else {
        return Ok(Probe { tray: None });
    };
    if atom.atom == 0 {
        return Ok(Probe { tray: None });
    }
    let owner = GetSelectionOwnerRequest { selection: atom.atom };
    stream
        .write_all(&<GetSelectionOwnerRequest as Request>::serialize(owner, 0).0)
        .map_err(|e| format!("sending GetSelectionOwner: {e}"))?;
    let reply = read_reply(&mut stream)?;
    Ok(Probe {
        tray: GetSelectionOwnerReply::try_parse(&reply).ok().map(|(r, _)| r.owner).filter(|&o| o != 0),
    })
}

/// One 32-byte reply (or error; both requests have no variable part).
fn read_reply(stream: &mut impl Read) -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    stream.read_exact(&mut buf).map_err(|e| format!("no reply: {e}"))?;
    Ok(buf)
}

fn hostname() -> Vec<u8> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for its length
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return Vec::new();
    }
    buf.iter().take_while(|&&b| b != 0).copied().collect()
}

/// The MIT-MAGIC-COOKIE-1 for local display `display` in an Xauthority file's
/// contents, as Xlib picks it: a FamilyLocal entry for this host or a
/// FamilyWild one, with that display number or none.
fn find_cookie(data: &[u8], host: &[u8], display: u16) -> Option<(Vec<u8>, Vec<u8>)> {
    const LOCAL: u16 = 256;
    const WILD: u16 = 65535;
    let number = display.to_string();
    let mut rest = data;
    let field = |rest: &mut &[u8]| -> Option<Vec<u8>> {
        let (len, tail) = rest.split_first_chunk::<2>()?;
        let len = usize::from(u16::from_be_bytes(*len));
        let value = tail.get(..len)?.to_vec();
        *rest = &tail[len..];
        Some(value)
    };
    while let Some((family, tail)) = rest.split_first_chunk::<2>() {
        rest = tail;
        let family = u16::from_be_bytes(*family);
        let address = field(&mut rest)?;
        let num = field(&mut rest)?;
        let name = field(&mut rest)?;
        let cookie = field(&mut rest)?;
        let here = family == WILD || (family == LOCAL && address == host);
        if here && (num.is_empty() || num == number.as_bytes()) && name == b"MIT-MAGIC-COOKIE-1" {
            return Some((name, cookie));
        }
    }
    None
}

/// Writes the hook where the wrapped command can load it: the runtime
/// directory (private to the user), else `~/.cache/xwlrvnc`, under a name
/// derived from its contents, so runs share one copy and an older build's copy
/// is never reused.
///
/// A directory is used only if it is ours, not writable by anyone else and not
/// mounted noexec (the dynamic loader would skip the hook there, silently). An
/// existing copy is reused only if it is a regular file of ours that nobody
/// else can write and holds exactly these bytes; otherwise a fresh one, created
/// exclusively and never through a symlink, is renamed over it.
pub fn install_hook() -> io::Result<PathBuf> {
    let mut why = Vec::new();
    let runtime = env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    let cache = env::var_os("HOME").filter(|d| !d.is_empty()).map(|h| PathBuf::from(h).join(".cache/xwlrvnc"));
    for (dir, create) in [(runtime, false), (cache, true)] {
        let Some(dir) = dir else { continue };
        if create && let Err(e) = fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir) {
            why.push(format!("{}: {e}", dir.display()));
            continue;
        }
        match usable_dir(&dir).and_then(|()| install_in(&dir)) {
            Ok(path) => return Ok(path),
            Err(e) => why.push(format!("{}: {e}", dir.display())),
        }
    }
    Err(io::Error::other(if why.is_empty() {
        "neither XDG_RUNTIME_DIR nor HOME is set".to_string()
    } else {
        why.join("; ")
    }))
}

/// Whether `dir` is a safe home for the hook (see [`install_hook`]).
fn usable_dir(dir: &Path) -> io::Result<()> {
    let meta = fs::metadata(dir)?;
    // SAFETY: geteuid cannot fail
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() {
        return Err(io::Error::other("not a directory"));
    }
    if meta.uid() != uid {
        return Err(io::Error::other(format!("owned by uid {}, not by us ({uid})", meta.uid())));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!("writable by others (mode {:o})", meta.mode() & 0o7777)));
    }
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: c is NUL-terminated, st is writable
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 && st.f_flag & libc::ST_NOEXEC != 0 {
        return Err(io::Error::other("mounted noexec"));
    }
    Ok(())
}

fn install_in(dir: &Path) -> io::Result<PathBuf> {
    let path = dir.join(format!("xwlrvnc-uihook-{:016x}.so", fnv1a(HOOK)));
    if is_our_copy(&path) {
        return Ok(path);
    }
    let tmp = dir.join(format!(".xwlrvnc-uihook-{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp); // a leftover of a crashed run with our pid
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    let written = f.write_all(HOOK).and_then(|()| f.sync_all());
    drop(f);
    written
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700)))
        .and_then(|()| fs::rename(&tmp, &path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
    Ok(path)
}

/// Whether `path` is a regular file of ours, writable only by us, holding
/// exactly the hook (opened without following a symlink).
fn is_our_copy(path: &Path) -> bool {
    let Ok(mut f) = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) else {
        return false;
    };
    let Ok(meta) = f.metadata() else { return false };
    // SAFETY: geteuid cannot fail
    let uid = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o022 != 0 || meta.len() != HOOK.len() as u64 {
        return false;
    }
    let mut have = Vec::with_capacity(HOOK.len());
    f.read_to_end(&mut have).is_ok() && have == HOOK
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The program to run instead of `program` so the hook reaches RealVNC's UI:
/// `vncserver-x11-core` beside a `vncserver-x11` (found on `path` if `program`
/// is a bare name). `None`: run `program` as it is.
pub fn core_for(program: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    let program = Path::new(program);
    if program.file_name()? != "vncserver-x11" {
        return None;
    }
    let launcher = if program.components().count() > 1 {
        program.to_path_buf()
    } else {
        env::split_paths(path?)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())?
    };
    let core = launcher.with_file_name("vncserver-x11-core");
    core.is_file().then_some(core)
}

/// Whether `program` (found on `path` if a bare name) is set-user-ID or
/// set-group-ID, which drops `LD_PRELOAD` before the hook can reach anything
/// it starts.
pub fn is_setid(program: &OsStr, path: Option<&OsStr>) -> bool {
    let program = Path::new(program);
    let found = if program.components().count() > 1 {
        Some(program.to_path_buf())
    } else {
        path.and_then(|p| env::split_paths(p).map(|dir| dir.join(program)).find(|p| p.is_file()))
    };
    found
        .and_then(|p| fs::metadata(p).ok())
        .is_some_and(|m| m.permissions().mode() & 0o6000 != 0)
}

/// Whether the command is a wrapper (`env`, a shell, `nice`, ...) that runs
/// `vncserver-x11` itself: the setuid launcher then drops the hook, and only
/// a `vncserver-x11` named as the command can be swapped for its core.
pub fn wrapped_launcher(program: &OsStr, args: &[OsString]) -> bool {
    const WRAPPERS: &[&[u8]] = &[
        b"env", b"sh", b"bash", b"dash", b"zsh", b"nice", b"ionice", b"setsid", b"stdbuf", b"taskset",
        b"chrt", b"nohup", b"timeout", b"systemd-run", b"sudo", b"runuser", b"su", b"dbus-run-session",
        b"systemd-inhibit", b"flatpak-spawn",
    ];
    let base = Path::new(program).file_name().map(OsStr::as_bytes).unwrap_or_default();
    WRAPPERS.contains(&base)
        && args.iter().any(|a| {
            let a = a.as_bytes();
            a.windows(13).enumerate().any(|(i, w)| w == b"vncserver-x11" && !a[i + 13..].starts_with(b"-core"))
        })
}

/// The RealVNC parameters given on its command line (`-Name=value`,
/// `Name=value` or `-Name value` style names), which RealVNC drops the first
/// time its Options dialog applies a change: the dialog writes the user's
/// config file and the server reloads its settings from the files alone.
pub fn command_line_params(args: &[OsString]) -> Vec<String> {
    args.iter()
        .filter_map(|a| a.to_str())
        .filter_map(|a| {
            let name = a.trim_start_matches('-').split('=').next()?;
            let looks = !name.is_empty()
                && name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            // vncserver-x11's own modes are not parameters
            let mode = matches!(name, "service" | "showstatus" | "virtual" | "help" | "version" | "rootHelper");
            (looks && !mode && (a.starts_with('-') || a.contains('='))).then(|| name.to_string())
        })
        .collect()
}

/// A process's parent pid and command line, from /proc.
fn proc_info(pid: u32) -> Option<(u32, Vec<Vec<u8>>)> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ...": comm may hold spaces and parentheses
    let rest = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    let ppid = std::str::from_utf8(rest).ok()?.split_whitespace().nth(1)?.parse().ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some((ppid, cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).map(<[u8]>::to_vec).collect()))
}

/// The children of `parent` running `vncserverui`, with their arguments.
fn ui_children(parent: u32) -> Vec<(u32, Vec<Vec<u8>>)> {
    let Ok(dir) = fs::read_dir("/proc") else { return Vec::new() };
    dir.filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let (ppid, argv) = proc_info(pid)?;
            let exe = Path::new(OsStr::from_bytes(argv.first()?)).file_name()?.as_bytes() == b"vncserverui";
            (ppid == parent && exe).then(|| (pid, argv[1..].to_vec()))
        })
        .collect()
}

/// What [`StatusIconWatch::poll`] wants done.
#[derive(Debug, PartialEq, Eq)]
pub enum IconAction {
    Nothing,
    /// The icon helper of UI process `pid` is gone; restart that UI process.
    Restart(u32),
    /// The same, but not now (a viewer is connected); said once.
    Deferred(u32),
    /// Too many restarts lately; said once.
    GiveUp(u32),
}

/// Keeps RealVNC's tray icon alive. The icon is a separate GTK2 helper
/// (`vncserverui -statusicon`) of RealVNC's UI process; on GNOME it has been
/// seen to die of an X error (BadWindow on QueryPointer) while its tray, GNOME
/// Shell's, re-arranged the top bar, and RealVNC's UI never starts it again —
/// the icon is then gone until the server restarts. RealVNC's core does restart
/// its UI process when that exits, which starts a new icon helper, so the
/// watch asks for that: only for a UI process that did have an icon, only while
/// no viewer is connected (so no transfer or chat in progress is cut), and at
/// most a few times an hour.
#[derive(Debug, Default)]
pub struct StatusIconWatch {
    ui: Option<u32>,
    had_icon: bool,
    missing: u32,
    said: bool,
    restarts: Vec<std::time::Instant>,
}

impl StatusIconWatch {
    const MISSES: u32 = 2;
    const MAX_RESTARTS: usize = 3;
    const WINDOW: Duration = Duration::from_secs(3600);

    /// One look: `ui` is RealVNC's UI process and `icon` whether its icon
    /// helper is running; `busy` whether a viewer is connected.
    pub fn poll(&mut self, ui: Option<u32>, icon: bool, busy: bool, now: std::time::Instant) -> IconAction {
        if ui != self.ui {
            *self = Self { ui, restarts: std::mem::take(&mut self.restarts), ..Self::default() };
        }
        let Some(pid) = ui else { return IconAction::Nothing };
        if icon {
            self.had_icon = true;
            self.missing = 0;
            self.said = false;
            return IconAction::Nothing;
        }
        if !self.had_icon {
            return IconAction::Nothing;
        }
        self.missing += 1;
        if self.missing < Self::MISSES {
            return IconAction::Nothing;
        }
        self.restarts.retain(|&t| now.duration_since(t) < Self::WINDOW);
        let said = std::mem::replace(&mut self.said, true);
        if self.restarts.len() >= Self::MAX_RESTARTS {
            return if said { IconAction::Nothing } else { IconAction::GiveUp(pid) };
        }
        if busy {
            return if said { IconAction::Nothing } else { IconAction::Deferred(pid) };
        }
        self.restarts.push(now);
        self.had_icon = false;
        IconAction::Restart(pid)
    }
}

/// The file the hook reads in each new UI process to decide where the UI
/// goes: `1`, the UI display; anything else, the engine display. It lives
/// next to the hook (a directory only we can write) and is replaced whole on
/// each change, so a UI process never reads half of one.
#[derive(Debug)]
pub struct Gate {
    path: PathBuf,
    on: bool,
}

impl Gate {
    /// Creates the gate for this xwlrvnc process in `dir`.
    pub fn create(dir: &Path, on: bool) -> io::Result<Gate> {
        let path = dir.join(format!("xwlrvnc-ui-{}.gate", std::process::id()));
        let mut gate = Gate { path, on: !on };
        gate.set(on)?;
        Ok(gate)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn set(&mut self, on: bool) -> io::Result<()> {
        if on == self.on {
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(format!(".xwlrvnc-ui-{}.gate.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        let written = f.write_all(if on { b"1\n" } else { b"0\n" });
        drop(f);
        written.and_then(|()| fs::rename(&tmp, &self.path)).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        self.on = on;
        Ok(())
    }
}

/// What [`Placement::poll`] wants done. The gate follows [`Placement::on`].
#[derive(Debug, PartialEq, Eq)]
pub enum PlaceAction {
    Nothing,
    /// The UI display answers: new UI processes go there; end UI process
    /// `Some(pid)` (on the engine display) so RealVNC's core starts it there.
    On(Option<u32>),
    /// The UI process `pid` should move but a viewer is connected (said once).
    Deferred(u32),
    /// The move [`PlaceAction::Deferred`] put off: end UI process `pid` now.
    Restart(u32),
    /// UI processes keep exiting on the UI display (none up, or a new one,
    /// at this many of the last looks): back to the engine display, where
    /// the UI works unseen.
    Off(usize),
    /// Moved too often lately; the UI stays on the engine display for now
    /// (said once).
    GiveUp,
}

/// Where new RealVNC UI processes go: the UI display while it is usable, else
/// the engine display (unseen, but the server's clipboard and prompts keep
/// working, and with the built-in engine the hook keeps the UI from
/// crashing). Off: the display is probed every little while and the UI moved
/// there when it answers — but not while a viewer is connected (a move
/// restarts the UI, cutting a transfer or chat), and at most a few times an
/// hour. On: if UI processes keep exiting there (the display went away, or
/// refuses the UI), it is off again.
#[derive(Debug)]
pub struct Placement {
    on: bool,
    /// on, but the running UI process is still on the engine display
    pending: bool,
    said: bool,
    last_ui: Option<u32>,
    /// looks while on that found no UI process, or a new one (one event: a
    /// look that finds none and the next one finding its successor count
    /// once; the first UI process ever seen not at all)
    unsteady: Vec<std::time::Instant>,
    /// the last look found no UI process (already counted)
    gap: bool,
    next_probe: std::time::Instant,
    moves: Vec<std::time::Instant>,
}

impl Placement {
    /// This many looks within [`Self::CHURN_WINDOW`] that found no UI process,
    /// or a new one: the UI does not stay up on the UI display. (RealVNC
    /// restarts a UI that exits after 2 s; one that can't connect lives a few
    /// milliseconds, so the watch, looking every 5 s, mostly finds none.)
    pub const CHURN: usize = 4;
    const CHURN_WINDOW: Duration = Duration::from_secs(60);
    /// Off: how often the display is probed, and the pause after [`Self::CHURN`].
    pub const RETRY: Duration = Duration::from_secs(15);
    const BACKOFF: Duration = Duration::from_secs(60);
    pub const MAX_MOVES: usize = 4;
    const MOVE_WINDOW: Duration = Duration::from_secs(3600);

    /// `on`: whether the display was usable when RealVNC started.
    pub fn new(on: bool, now: std::time::Instant) -> Self {
        Self {
            on,
            pending: false,
            said: false,
            last_ui: None,
            unsteady: Vec::new(),
            gap: false,
            next_probe: now + Self::RETRY,
            moves: Vec::new(),
        }
    }

    /// Whether new UI processes go on the UI display (the gate's value).
    pub fn on(&self) -> bool {
        self.on
    }

    /// Whether the running UI process is on the UI display (as far as we know).
    pub fn settled(&self) -> bool {
        self.on && !self.pending
    }

    /// One look: `ui` is RealVNC's UI process (if one runs), `busy` whether a
    /// viewer is connected; `usable` probes the UI display (called only when
    /// off and due).
    pub fn poll(
        &mut self,
        ui: Option<u32>,
        busy: bool,
        now: std::time::Instant,
        usable: impl FnOnce() -> bool,
    ) -> PlaceAction {
        let fresh = ui.is_some_and(|p| self.last_ui != Some(p));
        let first = fresh && self.last_ui.is_none();
        if ui.is_some() {
            self.last_ui = ui;
        }
        if self.on {
            if fresh {
                // a new UI process: on the UI display (the gate said so)
                self.pending = false;
            }
            if self.pending {
                let Some(pid) = ui else {
                    // it went by itself; the next one goes to the display
                    self.pending = false;
                    return PlaceAction::Nothing;
                };
                if busy {
                    return if std::mem::replace(&mut self.said, true) {
                        PlaceAction::Nothing
                    } else {
                        PlaceAction::Deferred(pid)
                    };
                }
                self.pending = false;
                return PlaceAction::Restart(pid);
            }
            // a restart counts once, whether a look caught the gap or only
            // the new process; RealVNC's first UI process is no restart
            if ui.is_none() {
                self.gap = true;
                self.unsteady.push(now);
            } else if std::mem::take(&mut self.gap) {
                // its successor: the gap was counted
            } else if fresh && !first {
                self.unsteady.push(now);
            }
            self.unsteady.retain(|&t| now.duration_since(t) < Self::CHURN_WINDOW);
            if self.unsteady.len() >= Self::CHURN {
                let n = self.unsteady.len();
                self.on = false;
                self.unsteady.clear();
                self.said = false;
                self.next_probe = now + Self::BACKOFF;
                return PlaceAction::Off(n);
            }
            return PlaceAction::Nothing;
        }
        if now < self.next_probe {
            return PlaceAction::Nothing;
        }
        self.next_probe = now + Self::RETRY;
        self.moves.retain(|&t| now.duration_since(t) < Self::MOVE_WINDOW);
        if self.moves.len() >= Self::MAX_MOVES {
            return if std::mem::replace(&mut self.said, true) { PlaceAction::Nothing } else { PlaceAction::GiveUp };
        }
        if !usable() {
            return PlaceAction::Nothing;
        }
        self.on = true;
        self.said = false;
        self.unsteady.clear();
        self.moves.push(now);
        match ui {
            None => PlaceAction::On(None),
            Some(pid) if busy => {
                self.pending = true;
                self.said = true;
                PlaceAction::Deferred(pid)
            }
            Some(pid) => {
                self.pending = true;
                PlaceAction::On(Some(pid))
            }
        }
    }
}

/// The UI process proper among the children of RealVNC's core `core` (not a
/// connection prompt, `-QueryConnectApp`), and whether its tray icon helper
/// (`vncserverui -statusicon`) runs.
fn ui_process(core: u32) -> (Option<u32>, bool) {
    let ui = ui_children(core).into_iter().find(|(_, args)| !args.iter().any(|a| a.starts_with(b"-")));
    let icon = ui
        .as_ref()
        .is_some_and(|(pid, _)| ui_children(*pid).iter().any(|(_, args)| args.iter().any(|a| a == b"-statusicon")));
    (ui.map(|(pid, _)| pid), icon)
}

/// Watches RealVNC's UI process (a child of RealVNC's core `core`) every few
/// seconds, for as long as the core lives: keeps the gate for `ui_display`
/// current ([`Placement`]; without a gate the UI always goes there) and the
/// UI's tray icon alive ([`StatusIconWatch`]); `busy` tells whether a viewer
/// is connected.
pub fn watch(core: u32, ui_display: UiDisplay, gate: Option<Gate>, busy: impl Fn() -> bool + Send + 'static) {
    std::thread::spawn(move || {
        let name = ui_display.display.to_string_lossy().into_owned();
        let mut gate = gate;
        let mut place = Placement::new(gate.as_ref().is_none_or(Gate::is_on), std::time::Instant::now());
        let mut icons = StatusIconWatch::default();
        let mut gate_failed = false;
        let end_ui = |pid: u32| {
            // SAFETY: plain kill(2); RealVNC's core starts a new UI process
            // when this one exits (its tray icon helper exits with it)
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        };
        while Path::new(&format!("/proc/{core}")).exists() {
            std::thread::sleep(Duration::from_secs(5));
            let (ui, icon) = ui_process(core);
            let busy = busy();
            let now = std::time::Instant::now();
            let action = if gate.is_none() {
                PlaceAction::Nothing
            } else {
                place.poll(ui, busy, now, || match probe(&ui_display, Duration::from_secs(5)) {
                    Ok(_) => true,
                    Err(e) => {
                        crate::vlog!("-uidisplay: {e}");
                        false
                    }
                })
            };
            if let Some(gate) = gate.as_mut()
                && gate.is_on() != place.on()
            {
                match gate.set(place.on()) {
                    Ok(()) => gate_failed = false,
                    Err(e) if !std::mem::replace(&mut gate_failed, true) => crate::warning!(
                        "-uidisplay: can't update {} ({e}); RealVNC's UI stays where it is",
                        gate.path().display()
                    ),
                    Err(_) => {}
                }
            }
            match action {
                PlaceAction::Nothing => {}
                PlaceAction::On(pid) => {
                    crate::log!("-uidisplay: X display {name} answers now; RealVNC's UI goes there");
                    if let Some(pid) = pid {
                        end_ui(pid);
                    }
                }
                PlaceAction::Deferred(pid) => crate::log!(
                    "-uidisplay: X display {name} answers now; RealVNC's UI process ({pid}) moves there once no \
                     viewer is connected"
                ),
                PlaceAction::Restart(pid) => {
                    crate::log!("-uidisplay: moving RealVNC's UI process ({pid}) to X display {name}");
                    end_ui(pid);
                }
                PlaceAction::Off(n) => crate::warning!(
                    "-uidisplay: RealVNC's UI does not stay up on X display {name} ({n} restarts within a minute); it \
                     goes on the engine display (unseen) until {name} answers again"
                ),
                PlaceAction::GiveUp => crate::warning!(
                    "-uidisplay: RealVNC's UI moved to X display {name} {} times within the hour; it stays on the \
                     engine display for now",
                    Placement::MAX_MOVES
                ),
            }
            // the tray icon only matters where it can be seen
            let ui = ui.filter(|_| place.settled());
            match icons.poll(ui, icon, busy, now) {
                IconAction::Nothing => {}
                IconAction::Restart(pid) => {
                    crate::warning!(
                        "-uidisplay: RealVNC's tray icon process exited; restarting RealVNC's UI process ({pid}) to \
                         bring the icon back"
                    );
                    end_ui(pid);
                }
                IconAction::Deferred(pid) => crate::warning!(
                    "-uidisplay: RealVNC's tray icon process exited; RealVNC's UI process ({pid}) will be restarted to \
                     bring it back once no viewer is connected"
                ),
                IconAction::GiveUp(pid) => crate::warning!(
                    "-uidisplay: RealVNC's tray icon process exited again; not restarting RealVNC's UI ({pid}) more than \
                     {} times an hour (`vncserver-x11 -showstatus` opens its status window)",
                    StatusIconWatch::MAX_RESTARTS
                ),
            }
        }
    });
}

/// Adds the hook and its instructions to the wrapped command's environment:
/// the UI display, if any, and the gate that says when to use it.
pub fn apply(cmd: &mut Command, hook: &Path, ui: Option<(&UiDisplay, Option<&Path>)>) {
    let previous = env::var_os("LD_PRELOAD").unwrap_or_default();
    let mut preload = hook.as_os_str().to_os_string();
    if !previous.is_empty() {
        preload.push(":");
        preload.push(&previous);
    }
    cmd.env("LD_PRELOAD", preload).env("XWLRVNC_UI_LD_PRELOAD", previous);
    // what the user may have exported must not steer the hook
    for var in ["XWLRVNC_UI_DISPLAY", "XWLRVNC_UI_XAUTHORITY", "XWLRVNC_UI_GATE"] {
        cmd.env_remove(var);
    }
    if let Some((ui, gate)) = ui {
        cmd.env("XWLRVNC_UI_DISPLAY", &ui.display);
        if let Some(auth) = &ui.xauthority {
            cmd.env("XWLRVNC_UI_XAUTHORITY", auth);
        }
        if let Some(gate) = gate {
            cmd.env("XWLRVNC_UI_GATE", gate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("xwlrvnc-uidisplay-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).unwrap();
        dir
    }

    #[test]
    fn hook_is_a_shared_object() {
        assert_eq!(&HOOK[..4], b"\x7fELF");
        // ET_DYN
        assert_eq!(u16::from_le_bytes([HOOK[16], HOOK[17]]), 3);
    }

    #[test]
    fn core_beside_the_launcher() {
        let dir = tmpdir("core");
        let launcher = dir.join("vncserver-x11");
        fs::write(&launcher, b"").unwrap();
        // no core next to it: nothing to substitute
        assert_eq!(core_for(launcher.as_os_str(), None), None);
        fs::write(dir.join("vncserver-x11-core"), b"").unwrap();
        assert_eq!(core_for(launcher.as_os_str(), None), Some(dir.join("vncserver-x11-core")));
        // by name, through the search path
        let path = env::join_paths([Path::new("/nonexistent"), &dir]).unwrap();
        assert_eq!(core_for(OsStr::new("vncserver-x11"), Some(&path)), Some(dir.join("vncserver-x11-core")));
        // other programs are left alone
        assert_eq!(core_for(OsStr::new("x11vnc"), Some(&path)), None);
        assert_eq!(core_for(OsStr::new("vncserver-x11-core"), Some(&path)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setid_programs() {
        let dir = tmpdir("setid");
        let p = dir.join("prog");
        fs::write(&p, b"").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!is_setid(p.as_os_str(), None));
        fs::set_permissions(&p, fs::Permissions::from_mode(0o2755)).unwrap();
        assert!(is_setid(p.as_os_str(), None));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mutter_cookie_is_found() {
        let dir = tmpdir("cookie");
        assert_eq!(mutter_cookie(&dir), None);
        fs::write(dir.join(".mutter-Xwaylandauth.ABC123"), b"").unwrap();
        fs::write(dir.join("unrelated"), b"").unwrap();
        assert_eq!(mutter_cookie(&dir), Some(dir.join(".mutter-Xwaylandauth.ABC123").into_os_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_keeps_an_existing_preload() {
        let ui = UiDisplay { display: ":0".into(), xauthority: Some("/run/user/1000/.mutter-Xwaylandauth.X".into()) };
        let envs = |cmd: &Command| -> Vec<(OsString, Option<OsString>)> {
            cmd.get_envs().map(|(k, v)| (k.to_os_string(), v.map(OsStr::to_os_string))).collect()
        };
        let get = |envs: &[(OsString, Option<OsString>)], k: &str| {
            envs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let hook = Path::new("/run/user/1000/xwlrvnc-uihook-0.so");
        let mut cmd = Command::new("true");
        apply(&mut cmd, hook, Some((&ui, Some(Path::new("/run/user/1000/xwlrvnc-ui-7.gate")))));
        let e = envs(&cmd);
        let preload = get(&e, "LD_PRELOAD").flatten().unwrap();
        assert!(preload.as_bytes().starts_with(b"/run/user/1000/xwlrvnc-uihook-0.so"));
        assert_eq!(get(&e, "XWLRVNC_UI_DISPLAY").flatten().unwrap(), ":0");
        assert_eq!(get(&e, "XWLRVNC_UI_XAUTHORITY").flatten().unwrap(), "/run/user/1000/.mutter-Xwaylandauth.X");
        assert_eq!(get(&e, "XWLRVNC_UI_GATE").flatten().unwrap(), "/run/user/1000/xwlrvnc-ui-7.gate");
        // the hook restores this in the UI process
        assert_eq!(get(&e, "XWLRVNC_UI_LD_PRELOAD").flatten().unwrap(), env::var_os("LD_PRELOAD").unwrap_or_default());
        // no UI display (the built-in engine's crash guard alone): the hook, and none of the inherited instructions
        let mut cmd = Command::new("true");
        apply(&mut cmd, hook, None);
        let e = envs(&cmd);
        assert!(get(&e, "LD_PRELOAD").flatten().is_some());
        for var in ["XWLRVNC_UI_DISPLAY", "XWLRVNC_UI_XAUTHORITY", "XWLRVNC_UI_GATE"] {
            assert_eq!(get(&e, var), Some(None), "{var} must be removed");
        }
    }

    #[test]
    fn choose_defaults_to_the_desktop_for_realvnc() {
        // one test touches DISPLAY, so they can't race
        let saved = (env::var_os("DISPLAY"), env::var_os("XAUTHORITY"));
        // SAFETY: no other test reads or writes DISPLAY/XAUTHORITY
        let put = |k: &str, v: Option<&str>| unsafe {
            match v {
                Some(v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        };
        put("XAUTHORITY", Some("/run/user/1000/.mutter-Xwaylandauth.Q"));
        put("DISPLAY", Some(":0"));
        let desktop = Choice::Display(UiDisplay {
            display: ":0".into(),
            xauthority: Some("/run/user/1000/.mutter-Xwaylandauth.Q".into()),
        });
        assert_eq!(choose(None, true), Ok(desktop.clone()));
        assert_eq!(choose(Some("auto"), true), Ok(desktop.clone()));
        assert_eq!(choose(Some("AUTO"), false), Ok(desktop));
        assert_eq!(choose(Some("none"), true), Ok(Choice::Off(None)));
        // not RealVNC: nothing unless asked
        assert_eq!(choose(None, false), Ok(Choice::Off(None)));
        // a forwarded (network) display is not the desktop's
        put("DISPLAY", Some("localhost:10.0"));
        assert!(matches!(choose(None, true), Ok(Choice::Off(Some(why))) if why.contains("not a local")));
        // explicit is explicit
        assert!(matches!(choose(Some("auto"), true), Ok(Choice::Display(_))));
        put("DISPLAY", None);
        assert!(matches!(choose(None, true), Ok(Choice::Off(Some(why))) if why.contains("no DISPLAY")));
        assert!(choose(Some("auto"), true).is_err());
        put("DISPLAY", saved.0.as_deref().and_then(OsStr::to_str));
        put("XAUTHORITY", saved.1.as_deref().and_then(OsStr::to_str));
    }

    #[test]
    fn realvnc_and_local_displays() {
        assert!(is_realvnc(OsStr::new("vncserver-x11")));
        assert!(is_realvnc(OsStr::new("/usr/bin/vncserver-x11-core")));
        assert!(!is_realvnc(OsStr::new("x11vnc")));
        assert!(!is_realvnc(OsStr::new("env")));
        for local in [":0", ":1.0", "unix:0"] {
            assert!(is_local(OsStr::new(local)), "{local}");
        }
        for remote in ["localhost:10.0", "otherhost:0", "tcp/host:0", "", "garbage"] {
            assert!(!is_local(OsStr::new(remote)), "{remote}");
        }
        let d = |s: &str| UiDisplay { display: s.into(), xauthority: None };
        assert!(same_display(&d(":20"), 20));
        assert!(same_display(&d(":20.0"), 20));
        assert!(!same_display(&d(":0"), 20));
        assert!(!same_display(&d("host:20"), 20));
    }

    #[test]
    fn gate_is_replaced_whole() {
        let dir = tmpdir("gate");
        let mut g = Gate::create(&dir, false).unwrap();
        assert_eq!(fs::read(g.path()).unwrap(), b"0\n");
        assert_eq!(fs::metadata(g.path()).unwrap().mode() & 0o777, 0o600);
        g.set(true).unwrap();
        assert!(g.is_on());
        assert_eq!(fs::read(g.path()).unwrap(), b"1\n");
        // a symlink at the temporary name is not written through
        let tmp = dir.join(format!(".xwlrvnc-ui-{}.gate.tmp", std::process::id()));
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        symlink(&victim, &tmp).unwrap();
        g.set(false).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(fs::read(g.path()).unwrap(), b"0\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn placement_moves_the_ui_when_the_display_answers() {
        use std::time::Instant;
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let never = || -> bool { panic!("must not probe now") };
        // the display did not answer at start: the UI starts on the engine display
        let mut p = Placement::new(false, t0);
        assert!(!p.on());
        assert_eq!(p.poll(Some(10), false, at(5), never), PlaceAction::Nothing);
        assert_eq!(p.poll(Some(10), false, at(10), never), PlaceAction::Nothing);
        // probed every RETRY: still nothing there
        assert_eq!(p.poll(Some(10), false, at(15), || false), PlaceAction::Nothing);
        assert_eq!(p.poll(Some(10), false, at(20), never), PlaceAction::Nothing);
        // it answers: gate on, the running UI is ended so RealVNC starts it there
        assert_eq!(p.poll(Some(10), false, at(30), || true), PlaceAction::On(Some(10)));
        assert!(p.on() && !p.settled());
        // the new UI process is there
        assert_eq!(p.poll(Some(11), false, at(35), never), PlaceAction::Nothing);
        assert!(p.settled());
        // it stays: nothing more
        for s in (40..400).step_by(5) {
            assert_eq!(p.poll(Some(11), false, at(s), never), PlaceAction::Nothing);
        }
    }

    #[test]
    fn placement_takes_a_few_restarts_on_a_healthy_display_in_stride() {
        // the UI killed at 10 s and 20 s on a
        // healthy display (RealVNC restarts it within 2 s) read as churn at
        // 25 s, because the first UI counted and a restart could count twice
        let t0 = std::time::Instant::now();
        let mut p = Placement::new(true, t0);
        let s = |n: u64| t0 + Duration::from_secs(n);
        let looks = [(0, Some(100)), (5, Some(100)), (10, None), (15, Some(101)), (20, None), (25, Some(102)), (30, Some(102)), (35, Some(102))];
        for (t, ui) in looks {
            assert_eq!(p.poll(ui, false, s(t), || true), PlaceAction::Nothing, "look at {t} s");
        }
        assert!(p.on());
    }

    #[test]
    fn placement_waits_for_viewers_and_falls_back_on_churn() {
        use std::time::Instant;
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let never = || -> bool { panic!("must not probe now") };
        let mut p = Placement::new(false, t0);
        // answers while a viewer is connected: gate on, the move waits (said once)
        assert_eq!(p.poll(Some(10), true, at(15), || true), PlaceAction::Deferred(10));
        assert!(p.on());
        assert_eq!(p.poll(Some(10), true, at(20), never), PlaceAction::Nothing);
        // the viewer leaves: now
        assert_eq!(p.poll(Some(10), false, at(25), never), PlaceAction::Restart(10));
        assert_eq!(p.poll(Some(11), false, at(30), never), PlaceAction::Nothing);
        assert!(p.settled());
        // the display goes away: UI processes keep exiting there (RealVNC restarts them every 2 s)
        // (a look that finds none and the next one's new process are one restart)
        assert_eq!(p.poll(Some(12), false, at(35), never), PlaceAction::Nothing);
        assert_eq!(p.poll(None, false, at(40), never), PlaceAction::Nothing);
        assert_eq!(p.poll(Some(13), false, at(45), never), PlaceAction::Nothing);
        assert_eq!(p.poll(None, false, at(50), never), PlaceAction::Off(4));
        assert!(!p.on());
        // back off a minute before probing again
        for s in [55, 80, 100, 105] {
            assert_eq!(p.poll(Some(14), false, at(s), never), PlaceAction::Nothing);
        }
        assert_eq!(p.poll(Some(14), false, at(110), || false), PlaceAction::Nothing);
        assert_eq!(p.poll(Some(14), false, at(125), || true), PlaceAction::On(Some(14)));
        // a UI that went by itself before the move: nothing to end, the next one goes there
        let mut q = Placement::new(false, t0);
        assert_eq!(q.poll(Some(20), false, at(15), || true), PlaceAction::On(Some(20)));
        let mut r = Placement::new(false, t0);
        assert_eq!(r.poll(Some(30), true, at(15), || true), PlaceAction::Deferred(30));
        assert_eq!(r.poll(None, true, at(20), never), PlaceAction::Nothing);
        assert!(r.settled());
    }

    #[test]
    fn placement_notices_a_ui_that_never_stays_up() {
        use std::time::Instant;
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let never = || -> bool { panic!("must not probe now") };
        // on from the start, the first UI up for a while
        let mut p = Placement::new(true, t0);
        for s in (5..60).step_by(5) {
            assert_eq!(p.poll(Some(7), false, at(s), never), PlaceAction::Nothing);
        }
        // the display dies: RealVNC's restarts can't connect and live milliseconds, so the looks mostly find none
        assert_eq!(p.poll(None, false, at(60), never), PlaceAction::Nothing);
        assert_eq!(p.poll(None, false, at(65), never), PlaceAction::Nothing);
        assert_eq!(p.poll(None, false, at(70), never), PlaceAction::Nothing);
        assert_eq!(p.poll(None, false, at(75), never), PlaceAction::Off(4));
        // a slow look now and then is not churn
        let mut q = Placement::new(true, t0);
        for s in (5..600).step_by(5) {
            let ui = if s % 120 == 0 { None } else { Some(9) };
            assert_eq!(q.poll(ui, false, at(s), never), PlaceAction::Nothing, "at {s}");
        }
    }

    #[test]
    fn placement_moves_a_few_times_an_hour_at_most() {
        use std::time::Instant;
        let t0 = Instant::now();
        let mut p = Placement::new(false, t0);
        let mut s = 0;
        let mut pid = 100;
        let mut moves = 0;
        let mut gave_up = 0;
        // a display that answers every probe but never keeps the UI
        while s < 3000 {
            s += 5;
            pid += 1;
            match p.poll(Some(pid), false, t0 + Duration::from_secs(s), || true) {
                PlaceAction::On(_) => moves += 1,
                PlaceAction::GiveUp => gave_up += 1,
                _ => {}
            }
        }
        assert_eq!(moves, Placement::MAX_MOVES);
        assert_eq!(gave_up, 1);
        assert!(!p.on());
    }

    #[test]
    fn placement_on_from_the_start() {
        use std::time::Instant;
        let t0 = Instant::now();
        let mut p = Placement::new(true, t0);
        assert!(p.settled());
        // RealVNC's first UI process, then a restart now and then (icon watch, Options): no churn
        assert_eq!(p.poll(Some(1), false, t0 + Duration::from_secs(5), || panic!()), PlaceAction::Nothing);
        assert_eq!(p.poll(Some(2), false, t0 + Duration::from_secs(30), || panic!()), PlaceAction::Nothing);
        assert_eq!(p.poll(Some(3), false, t0 + Duration::from_secs(95), || panic!()), PlaceAction::Nothing);
        assert!(p.on());
    }

    fn xauth_entry(family: u16, address: &[u8], number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = family.to_be_bytes().to_vec();
        for f in [address, number, name, data] {
            v.extend_from_slice(&(f.len() as u16).to_be_bytes());
            v.extend_from_slice(f);
        }
        v
    }

    #[test]
    fn cookie_matching() {
        let m = b"MIT-MAGIC-COOKIE-1";
        let mut file = xauth_entry(256, b"otherhost", b"0", m, b"other");
        file.extend(xauth_entry(256, b"myhost", b"1", m, b"display1"));
        file.extend(xauth_entry(256, b"myhost", b"0", b"XDM-AUTHORIZATION-1", b"xdm"));
        file.extend(xauth_entry(256, b"myhost", b"0", m, b"mine"));
        assert_eq!(find_cookie(&file, b"myhost", 0), Some((m.to_vec(), b"mine".to_vec())));
        assert_eq!(find_cookie(&file, b"myhost", 1), Some((m.to_vec(), b"display1".to_vec())));
        assert_eq!(find_cookie(&file, b"myhost", 2), None);
        // mutter writes FamilyWild entries too: any host, and an empty number is any display
        let wild = xauth_entry(65535, b"", b"", m, b"wild");
        assert_eq!(find_cookie(&wild, b"whatever", 7), Some((m.to_vec(), b"wild".to_vec())));
        // a truncated file is no cookie, not a panic
        assert_eq!(find_cookie(&file[..9], b"myhost", 0), None);
        assert_eq!(find_cookie(&[], b"myhost", 0), None);
    }

    /// A fake X server end: answers the connection setup (accepting the cookie
    /// `want`, refusing others) and InternAtom/GetSelectionOwner.
    fn fake_server(mut s: UnixStream, want: &'static [u8], atom: u32, owner: u32) {
        use crate::bridge::x11::conn::{build_reply, setup};
        use x11rb_protocol::protocol::xproto::SetupFailed;
        use x11rb_protocol::x11_utils::Serialize;
        let mut head = [0u8; 12];
        s.read_exact(&mut head).unwrap();
        let nlen = usize::from(u16::from_le_bytes([head[6], head[7]]));
        let dlen = usize::from(u16::from_le_bytes([head[8], head[9]]));
        let mut rest = vec![0u8; nlen.div_ceil(4) * 4 + dlen.div_ceil(4) * 4];
        s.read_exact(&mut rest).unwrap();
        let data = &rest[nlen.div_ceil(4) * 4..][..dlen];
        if data != want {
            let reason = b"Authorization required, but no authorization protocol specified\n".to_vec();
            let failed = SetupFailed {
                status: 0,
                protocol_major_version: 11,
                protocol_minor_version: 0,
                length: 0,
                reason,
            };
            let mut buf = Vec::new();
            failed.serialize_into(&mut buf);
            let length = ((buf.len() - 8) / 4) as u16;
            buf[6..8].copy_from_slice(&length.to_le_bytes());
            s.write_all(&buf).unwrap();
            return;
        }
        s.write_all(&setup(crate::util::Geometry::default(), 0x0020_0000)).unwrap();
        let mut req = [0u8; 4];
        // InternAtom: header + name
        s.read_exact(&mut req).unwrap();
        let len = usize::from(u16::from_le_bytes([req[2], req[3]])) * 4;
        let mut body = vec![0u8; len - 4];
        s.read_exact(&mut body).unwrap();
        assert_eq!(req[0], 16);
        assert!(body[4..].starts_with(b"_NET_SYSTEM_TRAY_S0"));
        s.write_all(&build_reply(&InternAtomReply { sequence: 1, length: 0, atom })).unwrap();
        if atom == 0 {
            return;
        }
        let mut req = [0u8; 8];
        s.read_exact(&mut req).unwrap();
        assert_eq!(req[0], 23); // GetSelectionOwner
        assert_eq!(u32::from_le_bytes(req[4..8].try_into().unwrap()), atom);
        s.write_all(&build_reply(&GetSelectionOwnerReply { sequence: 2, length: 0, owner })).unwrap();
    }

    fn probe_fake(cookie: Option<&'static [u8]>, atom: u32, owner: u32) -> Result<Probe, String> {
        let (client, server) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || fake_server(server, b"sesame", atom, owner));
        let cookie = cookie.map(|c| (b"MIT-MAGIC-COOKIE-1".to_vec(), c.to_vec()));
        let r = handshake(client, cookie, 0);
        t.join().unwrap();
        r
    }

    #[test]
    fn probe_finds_the_tray() {
        assert_eq!(probe_fake(Some(b"sesame"), 300, 0x60_0009), Ok(Probe { tray: Some(0x60_0009) }));
        // the atom exists but nobody owns the selection
        assert_eq!(probe_fake(Some(b"sesame"), 300, 0), Ok(Probe { tray: None }));
        // nobody ever interned the atom
        assert_eq!(probe_fake(Some(b"sesame"), 0, 0), Ok(Probe { tray: None }));
    }

    #[test]
    fn probe_reports_a_refusal() {
        let e = probe_fake(Some(b"wrong"), 300, 1).unwrap_err();
        assert!(e.contains("Authorization required"), "{e}");
        let e = probe_fake(None, 300, 1).unwrap_err();
        assert!(e.contains("refused"), "{e}");
    }

    #[test]
    fn probe_of_a_missing_display() {
        let ui = UiDisplay { display: ":4093".into(), xauthority: None };
        let e = probe(&ui, Duration::from_secs(1)).unwrap_err();
        assert!(e.contains(":4093"), "{e}");
    }

    #[test]
    fn hook_dir_must_be_private_and_ours() {
        let dir = tmpdir("dirperm");
        assert!(usable_dir(&dir).is_ok());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(usable_dir(&dir).unwrap_err().to_string().contains("writable by others"));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hook_install_never_trusts_what_is_there() {
        let dir = tmpdir("install");
        let path = install_in(&dir).unwrap();
        assert_eq!(fs::read(&path).unwrap(), HOOK);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert!(is_our_copy(&path));
        // reused as it is
        let ino = fs::metadata(&path).unwrap().ino();
        assert_eq!(install_in(&dir).unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().ino(), ino);
        // right bytes but writable by others: replaced
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!is_our_copy(&path));
        install_in(&dir).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        // wrong bytes: replaced
        fs::write(&path, b"planted").unwrap();
        install_in(&dir).unwrap();
        assert_eq!(fs::read(&path).unwrap(), HOOK);
        // a symlink (to a copy of the right bytes elsewhere) is not followed: replaced by a file of ours
        let elsewhere = dir.join("elsewhere.so");
        fs::write(&elsewhere, HOOK).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&elsewhere, &path).unwrap();
        assert!(!is_our_copy(&path));
        install_in(&dir).unwrap();
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(fs::read(&elsewhere).unwrap(), HOOK);
        // a symlink planted at the temporary name is not written through
        let tmp = dir.join(format!(".xwlrvnc-uihook-{}.tmp", std::process::id()));
        let target = dir.join("victim");
        fs::write(&target, b"keep").unwrap();
        symlink(&target, &tmp).unwrap();
        fs::remove_file(&path).unwrap();
        install_in(&dir).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert_eq!(fs::read(&path).unwrap(), HOOK);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wrappers_hide_the_launcher() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(wrapped_launcher(OsStr::new("env"), &args(&["A=1", "vncserver-x11", "-RfbPort=5900"])));
        assert!(wrapped_launcher(OsStr::new("/bin/sh"), &args(&["-c", "exec /usr/bin/vncserver-x11 -x"])));
        // the core itself is fine behind a wrapper (no setuid launcher in between)
        assert!(!wrapped_launcher(OsStr::new("env"), &args(&["vncserver-x11-core"])));
        assert!(!wrapped_launcher(OsStr::new("vncserver-x11"), &args(&[])));
        assert!(!wrapped_launcher(OsStr::new("env"), &args(&["x11vnc"])));
    }

    #[test]
    fn status_icon_watch() {
        use std::time::Instant;
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = StatusIconWatch::default();
        // a UI that never had an icon (no GTK2, say) is left alone
        assert_eq!(w.poll(Some(10), false, false, at(0)), IconAction::Nothing);
        assert_eq!(w.poll(Some(10), false, false, at(5)), IconAction::Nothing);
        assert_eq!(w.poll(Some(10), false, false, at(10)), IconAction::Nothing);
        // icon up, then gone: one miss is tolerated, the second asks for a restart
        assert_eq!(w.poll(Some(10), true, false, at(15)), IconAction::Nothing);
        assert_eq!(w.poll(Some(10), false, false, at(20)), IconAction::Nothing);
        assert_eq!(w.poll(Some(10), false, false, at(25)), IconAction::Restart(10));
        // nothing more for that pid until it is replaced
        assert_eq!(w.poll(Some(10), false, false, at(30)), IconAction::Nothing);
        // the new UI's icon dies while a viewer is connected: deferred (said once), then restarted when idle
        assert_eq!(w.poll(Some(11), true, false, at(35)), IconAction::Nothing);
        assert_eq!(w.poll(Some(11), false, true, at(40)), IconAction::Nothing);
        assert_eq!(w.poll(Some(11), false, true, at(45)), IconAction::Deferred(11));
        assert_eq!(w.poll(Some(11), false, true, at(50)), IconAction::Nothing);
        assert_eq!(w.poll(Some(11), false, false, at(55)), IconAction::Restart(11));
        // a third restart is allowed, a fourth within the hour is not
        assert_eq!(w.poll(Some(12), true, false, at(60)), IconAction::Nothing);
        w.poll(Some(12), false, false, at(65));
        assert_eq!(w.poll(Some(12), false, false, at(70)), IconAction::Restart(12));
        assert_eq!(w.poll(Some(13), true, false, at(75)), IconAction::Nothing);
        w.poll(Some(13), false, false, at(80));
        assert_eq!(w.poll(Some(13), false, false, at(85)), IconAction::GiveUp(13));
        assert_eq!(w.poll(Some(13), false, false, at(90)), IconAction::Nothing);
        // an hour after the first restarts there is room again
        assert_eq!(w.poll(Some(13), false, false, at(3700)), IconAction::Restart(13));
        // no UI process at all
        assert_eq!(w.poll(None, false, false, at(3705)), IconAction::Nothing);
    }

    #[test]
    fn proc_info_of_ourselves() {
        let (ppid, argv) = proc_info(std::process::id()).unwrap();
        // SAFETY: getppid cannot fail
        assert_eq!(ppid, unsafe { libc::getppid() } as u32);
        assert!(!argv.is_empty());
    }

    #[test]
    fn realvnc_parameters_on_the_command_line() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            command_line_params(&args(&["-RfbPort=5900", "Encryption=AlwaysOn", "-Log=*:stderr:30"])),
            ["RfbPort", "Encryption", "Log"]
        );
        assert_eq!(command_line_params(&args(&["-Authentication", "VncAuth"])), ["Authentication"]);
        assert!(command_line_params(&args(&["-service", "-showstatus"])).is_empty());
        assert!(command_line_params(&args(&[])).is_empty());
    }
}
