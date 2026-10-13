//! The environment of the programs that use our X display: the command we
//! wrap, the stock X server and the tools started with `xwlrvnc run`. They
//! belong on our display, and a toolkit that can also speak Wayland would
//! otherwise go to the desktop's compositor instead: GTK 3 and 4 open
//! `wayland-0` even with `WAYLAND_DISPLAY` unset (libwayland's default name),
//! Qt 6 and SDL 3 prefer Wayland in a Wayland session, and the JDK's Robot
//! captures through the portal when it thinks it is on Wayland. So every
//! toolkit's own switch is set to X11.
//!
//! `WAYLAND_DISPLAY` itself is removed, not set to a name that is no socket
//! (which would also stop libwayland's `wayland-0` default): x11vnc (0.9.17)
//! exits ("Wayland display server detected") when the variable is set at all,
//! even empty, and winit >= 0.29 and the JDK take any non-empty value for a
//! Wayland session (winit then fails, with no X11 fallback). The switches
//! below cover every toolkit that would fall back to `wayland-0`.
//!
//! Programs whose UI is meant for the desktop (OBS, SimpleScreenRecorder, a VNC
//! viewer) must not get this environment: they run on the desktop and are only
//! pointed at our display for capture (docs/split-display.md).

use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::{env, fs, io, process, ptr};

/// Removed for our children: the compositor's socket name, and an inherited
/// Wayland connection (which libwayland and winit prefer to the name).
pub const WAYLAND_VARS: &[&str] = &["WAYLAND_DISPLAY", "WAYLAND_SOCKET"];

/// Each toolkit's own switch, set to X11.
pub const X11_ONLY: &[(&str, &str)] = &[
    // what toolkits and launchers consult first (Qt, Chromium/Electron >= 38,
    // SDL 3, GLFW 3.4, scripts)
    ("XDG_SESSION_TYPE", "x11"),
    ("GDK_BACKEND", "x11"),                  // GTK 3 and 4
    ("QT_QPA_PLATFORM", "xcb"),              // Qt 5 and 6
    ("SDL_VIDEODRIVER", "x11"),              // SDL 2 (SDL 3 reads it too)
    ("SDL_VIDEO_DRIVER", "x11"),             // SDL 3
    ("ELECTRON_OZONE_PLATFORM_HINT", "x11"), // Electron < 38
    ("MOZ_ENABLE_WAYLAND", "0"),             // Firefox, Thunderbird
    ("CLUTTER_BACKEND", "x11"),
    ("WINIT_UNIX_BACKEND", "x11"), // winit < 0.29, which tries wayland-0 first (newer: WAYLAND_DISPLAY unset = X11)
];

/// What `xwlrvnc run` adds for Java: AWT's Robot reads our screen with X11
/// requests, never the desktop portal.
pub const JAVA_SCREENSHOT: &str = "-Dawt.robot.screenshotMethod=x11";

/// The variable that tells a program which display the bridge serves (set for
/// the wrapped command and by `xwlrvnc run`, read by `xwlrvnc run`).
pub const DISPLAY_VAR: &str = "XWLRVNC_DISPLAY";

/// Keeps `cmd` (and what it starts) off the Wayland compositor.
pub fn pin_x11(cmd: &mut Command) -> &mut Command {
    for k in WAYLAND_VARS {
        cmd.env_remove(k);
    }
    for (k, v) in X11_ONLY {
        cmd.env(k, v);
    }
    cmd
}

/// [`pin_x11`], and `cmd` on X display `display`.
pub fn on_display(cmd: &mut Command, display: u32) -> &mut Command {
    pin_x11(cmd).env("DISPLAY", format!(":{display}")).env(DISPLAY_VAR, format!(":{display}"))
}

/// `JAVA_TOOL_OPTIONS` with [`JAVA_SCREENSHOT`] added, unless the user's
/// options already choose a screenshot method.
pub fn java_tool_options(existing: Option<&OsStr>) -> OsString {
    match existing {
        Some(e) if e.as_bytes().windows(26).any(|w| w == b"awt.robot.screenshotMethod") => e.to_os_string(),
        Some(e) if !e.as_bytes().iter().all(u8::is_ascii_whitespace) => {
            let mut s = e.to_os_string();
            s.push(" ");
            s.push(JAVA_SCREENSHOT);
            s
        }
        _ => JAVA_SCREENSHOT.into(),
    }
}

/// A display number from `20`, `:20` or `:20.0`.
pub fn parse_display(s: &str) -> Option<u32> {
    let s = s.strip_prefix("unix").unwrap_or(s);
    let s = s.strip_prefix(':').unwrap_or(s);
    let s = s.split_once('.').map_or(s, |(n, _)| n);
    s.parse().ok()
}

// --- the running bridges, so `xwlrvnc run` can find "the" display ---

/// Where running bridges list their displays: one file per display, named by
/// its number. Private to the user (the runtime directory); no list without
/// one.
fn registry_dir() -> Option<PathBuf> {
    env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| Path::new(d).is_absolute())
        .map(|d| PathBuf::from(d).join("xwlrvnc-displays"))
}

/// Our entry, a leaked C string so a signal handler can remove it.
static ANNOUNCED: AtomicPtr<libc::c_char> = AtomicPtr::new(ptr::null_mut());

/// A running bridge, as its registry entry says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    display: u32,
    pid: i32,
    /// The cookie file a client needs for the display, when the engine
    /// couldn't let our own user in by uid (the wrapped command gets the
    /// same file as XAUTHORITY).
    xauthority: Option<PathBuf>,
}

/// A process's start time (clock ticks since boot, /proc/PID/stat field 22):
/// with the pid, it names one process, not whatever reuses the pid later.
fn start_time(pid: i32) -> Option<u64> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    // the command name (field 2) is in parentheses and may hold anything
    let rest = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    let field = rest.split(|b| b.is_ascii_whitespace()).filter(|f| !f.is_empty()).nth(19)?;
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// The text of an entry: "PID STARTTIME", then "XAUTHORITY=FILE" when a
/// cookie is needed.
fn entry_text(pid: i32, start: Option<u64>, xauthority: Option<&Path>) -> Vec<u8> {
    let mut text = match start {
        Some(t) => format!("{pid} {t}\n").into_bytes(),
        None => format!("{pid}\n").into_bytes(),
    };
    if let Some(a) = xauthority {
        text.extend_from_slice(b"XAUTHORITY=");
        text.extend_from_slice(a.as_os_str().as_bytes());
        text.push(b'\n');
    }
    text
}

/// Parses an entry's text (see [`entry_text`]): pid, start time, cookie file.
fn parse_entry(text: &[u8]) -> Option<(i32, Option<u64>, Option<PathBuf>)> {
    let mut lines = text.split(|&b| b == b'\n');
    let first = std::str::from_utf8(lines.next()?).ok()?;
    let mut words = first.split_whitespace();
    let pid = words.next()?.parse::<i32>().ok().filter(|&p| p > 0)?;
    let start = words.next().and_then(|w| w.parse().ok());
    let xauthority = lines
        .find_map(|l| l.strip_prefix(b"XAUTHORITY="))
        .filter(|f| !f.is_empty())
        .map(|f| PathBuf::from(OsStr::from_bytes(f)));
    Some((pid, start, xauthority))
}

/// Lists `display` as served by this process (best effort: without a runtime
/// directory, `xwlrvnc run` needs `-display`), with the cookie file a client
/// needs for it, if any.
pub fn announce(display: u32, xauthority: Option<&Path>) {
    let Some(dir) = registry_dir() else { return };
    let path = dir.join(display.to_string());
    let pid = process::id() as i32;
    let written = fs::create_dir_all(&dir).and_then(|()| {
        let tmp = dir.join(format!(".{display}.{pid}"));
        fs::write(&tmp, entry_text(pid, start_time(pid), xauthority))?;
        fs::rename(&tmp, &path)
    });
    match written {
        Ok(()) => {
            if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
                let old = ANNOUNCED.swap(c.into_raw(), Ordering::AcqRel);
                if !old.is_null() {
                    // SAFETY: from CString::into_raw above
                    drop(unsafe { CString::from_raw(old) });
                }
            }
        }
        Err(e) => crate::vlog!("can't list display :{display} in {}: {e}", dir.display()),
    }
}

/// Removes our entry, unless it no longer names us (a bridge started on the
/// same display after we let it go has replaced it). Async-signal-safe: no
/// allocation, only open/read/close/unlink.
pub fn withdraw() {
    let p = ANNOUNCED.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    let mut buf = [0u8; 32];
    // SAFETY: a leaked C string, never freed while set; buf is local
    let n = unsafe {
        let fd = libc::open(p, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW);
        if fd < 0 {
            return; // already gone
        }
        let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
        libc::close(fd);
        n
    };
    if n > 0 && leading_number(&buf[..n as usize]) != Some(u64::from(process::id())) {
        return;
    }
    // SAFETY: as above
    unsafe { libc::unlink(p) };
}

/// The decimal number `buf` starts with (no allocation).
fn leading_number(buf: &[u8]) -> Option<u64> {
    let digits = buf.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 19 {
        return None;
    }
    Some(buf[..digits].iter().fold(0u64, |n, &d| n * 10 + u64::from(d - b'0')))
}

/// The running bridges listed in `dir`, sorted by display. Entries of
/// bridges that are gone (or whose pid now belongs to another process) are
/// removed.
fn live_entries(dir: &Path) -> Vec<Entry> {
    let mut found = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return found };
    for e in rd.filter_map(Result::ok) {
        let Some(display) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let parsed = fs::read(e.path()).ok().and_then(|t| parse_entry(&t));
        match parsed {
            Some((pid, start, xauthority))
                if alive(pid) && start.is_none_or(|t| start_time(pid).is_none_or(|now| now == t)) =>
            {
                found.push(Entry { display, pid, xauthority })
            }
            _ => {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    found.sort_by_key(|e| e.display);
    found
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether X display `n` has a socket file (a bridge's display has one).
fn has_socket(n: u32) -> bool {
    Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

/// Which display `xwlrvnc run` uses, and the cookie file it needs if any:
/// `-display`, else `$XWLRVNC_DISPLAY` (set under a bridge), else the only
/// running bridge. A `$XWLRVNC_DISPLAY` that names no running bridge and no X
/// display at all (left over from an `eval "$(xwlrvnc run -print)"` in a
/// shell that outlived its bridge) is refused rather than run against.
fn resolve(
    explicit: Option<u32>,
    from_env: Option<&OsStr>,
    dir: Option<&Path>,
    socket: impl Fn(u32) -> bool,
) -> Result<(u32, Option<PathBuf>), String> {
    let live = dir.map(live_entries).unwrap_or_default();
    let auth = |n: u32| live.iter().find(|e| e.display == n).and_then(|e| e.xauthority.clone());
    let listed = || live.iter().map(|e| format!(":{} pid {}", e.display, e.pid)).collect::<Vec<_>>().join(", ");
    if let Some(n) = explicit {
        return Ok((n, auth(n)));
    }
    if let Some(v) = from_env.filter(|v| !v.is_empty()) {
        let n = parse_display(&v.to_string_lossy())
            .ok_or_else(|| format!("{DISPLAY_VAR}={} is not an X display", v.to_string_lossy()))?;
        if !socket(n) && !live.iter().any(|e| e.display == n) {
            return Err(format!(
                "{DISPLAY_VAR}=:{n} names no running bridge (left over from an earlier one?){}; \
                 unset it or name a display: -display N",
                if live.is_empty() { String::new() } else { format!("; running: {}", listed()) }
            ));
        }
        return Ok((n, auth(n)));
    }
    match live.as_slice() {
        [e] => Ok((e.display, e.xauthority.clone())),
        [] => Err("no running bridge found; start one, or name its display: -display N".into()),
        _ => Err(format!("several bridges are running ({}); name one: -display N", listed())),
    }
}

const RUN_HELP: &str = "\
use: xwlrvnc run [-display N] [--] command [args ...]
     xwlrvnc run [-display N] -print

Runs an X tool on a running bridge's display, with the environment of the
programs the bridge starts itself: DISPLAY set to the bridge, WAYLAND_DISPLAY
removed, every toolkit pinned to X11 (GDK_BACKEND=x11 QT_QPA_PLATFORM=xcb
SDL_VIDEODRIVER=x11 ELECTRON_OZONE_PLATFORM_HINT=x11 MOZ_ENABLE_WAYLAND=0
CLUTTER_BACKEND=x11 XDG_SESSION_TYPE=x11 ...), and Java's Robot on X11
(JAVA_TOOL_OPTIONS=-Dawt.robot.screenshotMethod=x11).

Not for programs whose own windows belong on the desktop (OBS, SimpleScreen-
Recorder, VNC viewers): run those normally and point their capture at the
bridge's display (docs/split-display.md).

-display N    the bridge's display (default: $XWLRVNC_DISPLAY, or else the only
              running bridge)
-print        print the environment as shell commands instead of running
              anything (for a shell that only runs bridge tools)
";

/// `xwlrvnc run ...` (`args` without the program and `run`). Never returns.
pub fn run_main(args: Vec<OsString>) -> ! {
    fn fail(msg: &str, code: i32) -> ! {
        eprintln!("xwlrvnc run: {msg}");
        process::exit(code);
    }
    let mut explicit = None;
    let mut print = false;
    let mut i = 0;
    while i < args.len() {
        let Some(a) = args[i].to_str() else { break };
        if a == "--" {
            i += 1;
            break;
        }
        let Some(body) = a.strip_prefix('-').filter(|b| !b.is_empty()) else { break };
        let (name, inline) = match body.split_once('=') {
            Some((n, v)) => (n.to_ascii_lowercase(), Some(v.to_string())),
            None => (body.trim_start_matches('-').to_ascii_lowercase(), None),
        };
        match name.trim_start_matches('-') {
            "help" | "h" => {
                print!("{RUN_HELP}");
                process::exit(0);
            }
            "print" => print = true,
            "display" => {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i).and_then(|v| v.to_str()).map(str::to_string).unwrap_or_default()
                    }
                };
                explicit =
                    Some(parse_display(&v).unwrap_or_else(|| fail(&format!("-display {v:?} is not an X display"), 2)));
            }
            other => fail(&format!("unknown option -{other} (try: xwlrvnc run -help)"), 2),
        }
        i += 1;
    }
    let command = &args[i..];
    if command.is_empty() && !print {
        fail("no command given (try: xwlrvnc run -help)", 2);
    }
    let dir = registry_dir();
    let (display, xauthority) =
        resolve(explicit, env::var_os(DISPLAY_VAR).as_deref(), dir.as_deref(), has_socket).unwrap_or_else(|e| fail(&e, 2));
    let java = java_tool_options(env::var_os("JAVA_TOOL_OPTIONS").as_deref());

    if print {
        let mut out = String::new();
        let mut export = |k: &str, v: &OsStr| out.push_str(&format!("export {k}={}\n", sh_quote(v)));
        export("DISPLAY", OsStr::new(&format!(":{display}")));
        export(DISPLAY_VAR, OsStr::new(&format!(":{display}")));
        for (k, v) in X11_ONLY {
            export(k, OsStr::new(v));
        }
        export("JAVA_TOOL_OPTIONS", &java);
        if let Some(a) = &xauthority {
            export("XAUTHORITY", a.as_os_str());
        }
        out.push_str(&format!("unset {}\n", WAYLAND_VARS.join(" ")));
        print!("{out}");
        process::exit(0);
    }

    if !has_socket(display) {
        // only a hint: the abstract socket may still be there
        eprintln!("xwlrvnc run: warning: display :{display} has no socket /tmp/.X11-unix/X{display}");
    }
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    on_display(&mut cmd, display).env("JAVA_TOOL_OPTIONS", &java);
    // the engine's cookie, when it couldn't let our own user in by uid
    if let Some(a) = &xauthority {
        cmd.env("XAUTHORITY", a);
    }
    let e = cmd.exec();
    let code = if e.kind() == io::ErrorKind::NotFound { 127 } else { 126 };
    fail(&format!("can't run {}: {e}", command[0].to_string_lossy()), code);
}

/// `v` quoted for a POSIX shell.
fn sh_quote(v: &OsStr) -> String {
    let s = v.to_string_lossy();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:=,+@%".contains(&b)) {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envs(cmd: &Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect()
    }
    fn get(e: &[(String, Option<String>)], k: &str) -> Option<Option<String>> {
        e.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn children_are_pinned_to_x11() {
        let mut cmd = Command::new("true");
        on_display(&mut cmd, 21);
        let e = envs(&cmd);
        assert_eq!(get(&e, "DISPLAY"), Some(Some(":21".into())));
        assert_eq!(get(&e, "XWLRVNC_DISPLAY"), Some(Some(":21".into())));
        // removed (x11vnc quits when it is set at all); the toolkit switches
        // below keep libwayland's wayland-0 default from mattering
        assert_eq!(get(&e, "WAYLAND_DISPLAY"), Some(None));
        assert_eq!(get(&e, "WAYLAND_SOCKET"), Some(None));
        for (k, v) in [
            ("GDK_BACKEND", "x11"),
            ("QT_QPA_PLATFORM", "xcb"),
            ("SDL_VIDEODRIVER", "x11"),
            ("ELECTRON_OZONE_PLATFORM_HINT", "x11"),
            ("MOZ_ENABLE_WAYLAND", "0"),
            ("CLUTTER_BACKEND", "x11"),
            ("XDG_SESSION_TYPE", "x11"),
            ("SDL_VIDEO_DRIVER", "x11"),
            ("WINIT_UNIX_BACKEND", "x11"),
        ] {
            assert_eq!(get(&e, k), Some(Some(v.into())), "{k}");
        }
        // only `run` adds the Java option (the JVM announces it on stderr)
        assert_eq!(get(&e, "JAVA_TOOL_OPTIONS"), None);
    }

    #[test]
    fn java_options_are_merged() {
        assert_eq!(java_tool_options(None), JAVA_SCREENSHOT);
        assert_eq!(java_tool_options(Some(OsStr::new("  "))), JAVA_SCREENSHOT);
        assert_eq!(
            java_tool_options(Some(OsStr::new("-Xmx1g"))),
            format!("-Xmx1g {JAVA_SCREENSHOT}").as_str()
        );
        // the user's own choice stays
        let own = OsStr::new("-Dawt.robot.screenshotMethod=dbusScreencast");
        assert_eq!(java_tool_options(Some(own)), own);
    }

    #[test]
    fn display_names() {
        assert_eq!(parse_display("20"), Some(20));
        assert_eq!(parse_display(":20"), Some(20));
        assert_eq!(parse_display(":20.0"), Some(20));
        assert_eq!(parse_display("unix:7"), Some(7));
        assert_eq!(parse_display("host:1"), None);
        assert_eq!(parse_display(""), None);
    }

    #[test]
    fn which_display_run_uses() {
        let dir = env::temp_dir().join(format!("xwlrvnc-childenv-test-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let me = process::id() as i32;
        let no_socket = |_| false;
        // nothing running
        assert!(resolve(None, None, Some(&dir), no_socket).unwrap_err().contains("no running bridge"));
        // one live bridge (us), one gone (pid that cannot exist), junk
        fs::write(dir.join("23"), entry_text(me, start_time(me), None)).unwrap();
        fs::write(dir.join("24"), "2147483646\n").unwrap();
        fs::write(dir.join("notes"), "x").unwrap();
        assert_eq!(resolve(None, None, Some(&dir), no_socket), Ok((23, None)));
        assert!(!dir.join("24").exists(), "a dead bridge's entry is removed");
        // -display and $XWLRVNC_DISPLAY come first
        assert_eq!(resolve(Some(5), Some(OsStr::new(":6")), Some(&dir), no_socket), Ok((5, None)));
        assert_eq!(resolve(None, Some(OsStr::new(":6")), Some(&dir), |n| n == 6), Ok((6, None)));
        assert!(resolve(None, Some(OsStr::new("nope")), Some(&dir), no_socket).is_err());
        // a leftover $XWLRVNC_DISPLAY (no bridge, no X display) is refused
        let e = resolve(None, Some(OsStr::new(":6")), Some(&dir), no_socket).unwrap_err();
        assert!(e.contains("no running bridge") && e.contains(":23"), "{e}");
        assert_eq!(resolve(None, Some(OsStr::new(":23")), Some(&dir), no_socket), Ok((23, None)));
        // a pid reused by another process (other start time): stale, removed
        fs::write(dir.join("26"), format!("{me} 1\n")).unwrap();
        assert_eq!(resolve(None, None, Some(&dir), no_socket), Ok((23, None)));
        assert!(!dir.join("26").exists(), "an entry naming another process is removed");
        // the cookie a display needs comes with it
        let cookie = dir.join("cookie");
        fs::write(dir.join("23"), entry_text(me, start_time(me), Some(&cookie))).unwrap();
        assert_eq!(resolve(None, None, Some(&dir), no_socket), Ok((23, Some(cookie.clone()))));
        assert_eq!(resolve(Some(23), None, Some(&dir), no_socket), Ok((23, Some(cookie.clone()))));
        // two live ones: ambiguous
        fs::write(dir.join("25"), entry_text(me, None, None)).unwrap();
        let e = resolve(None, None, Some(&dir), no_socket).unwrap_err();
        assert!(e.contains(":23") && e.contains(":25"), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entries_round_trip() {
        let t = entry_text(42, Some(7), Some(Path::new("/run/user/1000/xwlrvnc-1-x/auth")));
        assert_eq!(
            parse_entry(&t),
            Some((42, Some(7), Some(PathBuf::from("/run/user/1000/xwlrvnc-1-x/auth"))))
        );
        // an entry written by an older bridge: the pid alone
        assert_eq!(parse_entry(b"42\n"), Some((42, None, None)));
        assert_eq!(parse_entry(b"x\n"), None);
        assert_eq!(parse_entry(b"0 5\n"), None);
        assert_eq!(leading_number(b"123 456"), Some(123));
        assert_eq!(leading_number(b" 1"), None);
        assert!(start_time(process::id() as i32).is_some());
    }

    #[test]
    fn withdraw_removes_only_our_own_entry() {
        let dir = env::temp_dir().join(format!("xwlrvnc-childenv-withdraw-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("31");
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        let old = ANNOUNCED.swap(c.into_raw(), Ordering::AcqRel);
        // replaced by another bridge on the same display: left alone
        fs::write(&path, "2147483646 5\n").unwrap();
        withdraw();
        assert!(path.exists(), "another bridge's entry was removed");
        // ours: removed
        let me = process::id() as i32;
        fs::write(&path, entry_text(me, start_time(me), None)).unwrap();
        withdraw();
        assert!(!path.exists(), "our entry was left behind");
        let ours = ANNOUNCED.swap(old, Ordering::AcqRel);
        drop(unsafe { CString::from_raw(ours) });
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote(OsStr::new(":20")), ":20");
        assert_eq!(sh_quote(OsStr::new("")), "''");
        assert_eq!(sh_quote(OsStr::new("-Xmx1g -Dawt.robot.screenshotMethod=x11")), "'-Xmx1g -Dawt.robot.screenshotMethod=x11'");
        assert_eq!(sh_quote(OsStr::new("it's")), r"'it'\''s'");
    }
}
