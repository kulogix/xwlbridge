//! Which X server binary a stock engine runs.
//!
//! "Xvnc" is not one program. On most distributions it is TigerVNC's, the one
//! `-engine xvnc` is written for (its private RFB socket, `-rfbunixpath`,
//! `-AcceptKeyEvents=0`, RandR resizing). RealVNC Server's package installs
//! its own at the same path, /usr/bin/Xvnc: a set-user-ID-root launcher for
//! RealVNC's virtual-mode server, which takes other arguments and must never be
//! started as our engine. So the binary is chosen deliberately:
//!
//! - `-engine xvnc`: TigerVNC's `Xtigervnc` on the search path first (Debian
//!   and Ubuntu install it there, and keep it when RealVNC holds /usr/bin/Xvnc),
//!   then an `Xvnc`, but only one that identifies as TigerVNC's;
//! - `-engine xvfb`: `Xvfb`;
//! - `-engine xwayland`: `Xwayland` (run rootful, on a private compositor of
//!   ours: see `wlhost`);
//! - never anything set-user-ID or set-group-ID;
//! - `-xserver PATH` names the binary instead, and is checked the same way.
//!
//! The check reads the file (it is never run to ask): TigerVNC's server
//! carries its version banner ("Xvnc TigerVNC 1.15.0 - built ...").

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::EngineType;

/// In every TigerVNC Xvnc: its version banner and copyright line.
const TIGERVNC: &[u8] = b"TigerVNC";
/// In RealVNC's launcher and server.
const REALVNC: &[u8] = b"RealVNC";
/// No X server binary is anywhere near this; a bound on what is read.
const MAX_READ: u64 = 256 << 20;

fn install_hint(engine: EngineType) -> &'static str {
    match engine {
        EngineType::Xvnc => {
            "install TigerVNC's X server (Debian/Ubuntu: sudo apt install tigervnc-standalone-server; \
             Fedora: sudo dnf install tigervnc-server-minimal; Arch: sudo pacman -S tigervnc), \
             or name it with -xserver PATH"
        }
        EngineType::Xwayland => {
            "install Xwayland (Debian/Ubuntu/Raspberry Pi OS: sudo apt install xwayland; RHEL/Fedora: \
             sudo dnf install xorg-x11-server-Xwayland; Arch: sudo pacman -S xorg-xwayland), or name it with \
             -xserver PATH"
        }
        _ => {
            "install Xvfb (Debian/Ubuntu: sudo apt install xvfb; Fedora: sudo dnf install \
             xorg-x11-server-Xvfb; Arch: sudo pacman -S xorg-server-xvfb), or name it with -xserver PATH"
        }
    }
}

/// The X server binary to run for `engine`: `explicit` (`-xserver`), or the
/// first acceptable one on `path` (`PATH`). The error says what was found,
/// why it was refused, and what to install.
pub fn resolve(engine: EngineType, explicit: Option<&OsStr>, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let names: &[&str] = match engine {
        EngineType::Xvnc => &["Xtigervnc", "Xvnc"],
        EngineType::Xvfb => &["Xvfb"],
        EngineType::Xwayland => &["Xwayland"],
        EngineType::Builtin => return Err("the built-in engine runs no X server binary".into()),
    };
    if let Some(given) = explicit {
        let found = lookup(Path::new(given), path)
            .ok_or_else(|| format!("-xserver {}: no such program", given.to_string_lossy()))?;
        return check(&found, engine).map_err(|why| format!("-xserver {}: {why}", found.display()));
    }
    let mut refused = Vec::new();
    for name in names {
        for candidate in on_path(name, path) {
            match check(&candidate, engine) {
                Ok(p) => return Ok(p),
                Err(why) => refused.push(format!("{} ({why})", candidate.display())),
            }
        }
    }
    let what = match engine {
        EngineType::Xvnc => "-engine xvnc needs TigerVNC's Xvnc (Xtigervnc)",
        EngineType::Xwayland => "-engine xwayland needs Xwayland",
        _ => "-engine xvfb needs Xvfb",
    };
    if refused.is_empty() {
        Err(format!("{what}, which isn't installed; {}", install_hint(engine)))
    } else {
        Err(format!("{what}; refused {}; {}", refused.join(", "), install_hint(engine)))
    }
}

/// `program` as given if it names a path, else its first match on `path`.
fn lookup(program: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    on_path(program.to_str()?, path).into_iter().next()
}

/// Every `name` on `path`, in order.
fn on_path(name: &str, path: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(path) = path else { return Vec::new() };
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .filter(|p| p.is_file())
        .collect()
}

/// `candidate` (after symlinks) if it may be run as `engine`'s server, or why not.
fn check(candidate: &Path, engine: EngineType) -> Result<PathBuf, String> {
    let real = fs::canonicalize(candidate).map_err(|e| e.to_string())?;
    let meta = fs::metadata(&real).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mode = meta.permissions().mode();
    if mode & 0o111 == 0 {
        return Err("not executable".into());
    }
    if mode & 0o6000 != 0 {
        return Err("set-user-ID or set-group-ID, as RealVNC's Xvnc is and no engine server should be; not run".into());
    }
    let mut data = Vec::new();
    fs::File::open(&real)
        .and_then(|f| f.take(MAX_READ).read_to_end(&mut data))
        .map_err(|e| format!("can't read it: {e}"))?;
    let tiger = contains(&data, TIGERVNC);
    let realvnc = contains(&data, REALVNC);
    match engine {
        EngineType::Xvnc if tiger => Ok(real),
        EngineType::Xvnc if realvnc => Err("RealVNC's Xvnc, not TigerVNC's".into()),
        EngineType::Xvnc => Err("not TigerVNC's Xvnc (no TigerVNC version banner)".into()),
        _ if realvnc && !tiger => Err(format!("RealVNC's X server, not {}", if engine == EngineType::Xwayland { "Xwayland" } else { "Xvfb" })),
        _ => Ok(real),
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else { return true };
    let mut at = 0;
    while let Some(i) = hay[at..].iter().position(|&b| b == first) {
        let start = at + i + 1;
        if hay.len() - start < rest.len() {
            return false;
        }
        if &hay[start..start + rest.len()] == rest {
            return true;
        }
        at = start;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn dir(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("xwlrvnc-xserver-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn exe(dir: &Path, name: &str, body: &[u8], mode: u32) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        p
    }
    fn canon(p: &Path) -> PathBuf {
        fs::canonicalize(p).unwrap()
    }
    fn path(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }
    const TIGER: &[u8] = b"\x7fELF...Xvnc %s - built %s\0TigerVNC 1.15.0\0";
    const REAL: &[u8] = b"\x7fELF...RealVNC Ltd\0";

    #[test]
    fn xtigervnc_is_preferred_over_any_xvnc() {
        let (a, b) = (dir("pref-a"), dir("pref-b"));
        exe(&a.0, "Xvnc", TIGER, 0o755); // a TigerVNC Xvnc earlier on the path
        let tiger = exe(&b.0, "Xtigervnc", TIGER, 0o755);
        let p = path(&[&a.0, &b.0]);
        assert_eq!(resolve(EngineType::Xvnc, None, Some(&p)), Ok(canon(&tiger)));
    }

    #[test]
    fn realvnc_xvnc_is_never_chosen() {
        // the host's layout: RealVNC's set-user-ID launcher at Xvnc, no TigerVNC
        let a = dir("real");
        exe(&a.0, "Xvnc", REAL, 0o4755);
        let p = path(&[&a.0]);
        let e = resolve(EngineType::Xvnc, None, Some(&p)).unwrap_err();
        assert!(e.contains("set-user-ID"), "{e}");
        assert!(e.contains("apt install tigervnc-standalone-server"), "{e}");
        // ... nor without the set-user-ID bit (e.g. RealVNC's Xvnc-core copied as Xvnc)
        exe(&a.0, "Xvnc", REAL, 0o755);
        let e = resolve(EngineType::Xvnc, None, Some(&p)).unwrap_err();
        assert!(e.contains("RealVNC's Xvnc, not TigerVNC's"), "{e}");
        // ... nor an unidentified one
        exe(&a.0, "Xvnc", b"\x7fELF something else", 0o755);
        let e = resolve(EngineType::Xvnc, None, Some(&p)).unwrap_err();
        assert!(e.contains("no TigerVNC version banner"), "{e}");
    }

    #[test]
    fn a_tigervnc_xvnc_after_a_realvnc_one_is_found() {
        let (a, b) = (dir("order-a"), dir("order-b"));
        exe(&a.0, "Xvnc", REAL, 0o4755);
        let tiger = exe(&b.0, "Xvnc", TIGER, 0o755);
        let p = path(&[&a.0, &b.0]);
        assert_eq!(resolve(EngineType::Xvnc, None, Some(&p)), Ok(canon(&tiger)));
    }

    #[test]
    fn a_symlinked_xvnc_is_followed_and_checked() {
        // Debian: /usr/bin/Xvnc -> /etc/alternatives/Xvnc -> /usr/bin/Xtigervnc
        let a = dir("link");
        let tiger = exe(&a.0, "Xtigervnc.real", TIGER, 0o755);
        std::os::unix::fs::symlink(&tiger, a.0.join("Xvnc")).unwrap();
        let p = path(&[&a.0]);
        assert_eq!(resolve(EngineType::Xvnc, None, Some(&p)), Ok(canon(&tiger)));
        // a link to a set-user-ID file is refused like the file
        let real = exe(&a.0, "realvnc", REAL, 0o4755);
        fs::remove_file(a.0.join("Xvnc")).unwrap();
        std::os::unix::fs::symlink(&real, a.0.join("Xvnc")).unwrap();
        assert!(resolve(EngineType::Xvnc, None, Some(&p)).is_err());
    }

    #[test]
    fn explicit_xserver_is_checked_too() {
        let a = dir("explicit");
        let real = exe(&a.0, "Xvnc", REAL, 0o4755);
        let tiger = exe(&a.0, "my-xvnc", TIGER, 0o755);
        let e = resolve(EngineType::Xvnc, Some(real.as_os_str()), None).unwrap_err();
        assert!(e.starts_with(&format!("-xserver {}:", real.display())), "{e}");
        assert_eq!(resolve(EngineType::Xvnc, Some(tiger.as_os_str()), None), Ok(canon(&tiger)));
        // a bare name is looked up on the path
        let p = path(&[&a.0]);
        assert_eq!(resolve(EngineType::Xvnc, Some(OsStr::new("my-xvnc")), Some(&p)), Ok(canon(&tiger)));
        let e = resolve(EngineType::Xvnc, Some(OsStr::new("nonesuch")), Some(&p)).unwrap_err();
        assert!(e.contains("no such program"), "{e}");
        // not executable
        let plain = exe(&a.0, "plain", TIGER, 0o644);
        assert!(resolve(EngineType::Xvnc, Some(plain.as_os_str()), None).unwrap_err().contains("not executable"));
    }

    #[test]
    fn xvfb() {
        let a = dir("xvfb");
        let p = path(&[&a.0]);
        let e = resolve(EngineType::Xvfb, None, Some(&p)).unwrap_err();
        assert!(e.contains("isn't installed") && e.contains("apt install xvfb"), "{e}");
        let xvfb = exe(&a.0, "Xvfb", b"\x7fELF Xvfb", 0o755);
        assert_eq!(resolve(EngineType::Xvfb, None, Some(&p)), Ok(canon(&xvfb)));
        fs::set_permissions(&xvfb, fs::Permissions::from_mode(0o2755)).unwrap();
        assert!(resolve(EngineType::Xvfb, None, Some(&p)).unwrap_err().contains("set-user-ID"));
        let real = exe(&a.0, "rv", REAL, 0o755);
        assert!(resolve(EngineType::Xvfb, Some(real.as_os_str()), None).unwrap_err().contains("RealVNC"));
    }

    #[test]
    fn xwayland() {
        let a = dir("xwayland");
        let p = path(&[&a.0]);
        let e = resolve(EngineType::Xwayland, None, Some(&p)).unwrap_err();
        assert!(e.contains("isn't installed") && e.contains("apt install xwayland"), "{e}");
        let xw = exe(&a.0, "Xwayland", b"\x7fELF Xwayland", 0o755);
        assert_eq!(resolve(EngineType::Xwayland, None, Some(&p)), Ok(canon(&xw)));
        fs::set_permissions(&xw, fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(resolve(EngineType::Xwayland, None, Some(&p)).unwrap_err().contains("set-user-ID"));
    }

    #[test]
    fn nothing_installed() {
        let e = resolve(EngineType::Xvnc, None, None).unwrap_err();
        assert!(e.contains("isn't installed") && e.contains("-xserver PATH"), "{e}");
    }

    #[test]
    fn substring_search() {
        assert!(contains(b"xxTigerVNC", b"TigerVNC"));
        assert!(contains(b"TTigerVNC 1", b"TigerVNC"));
        assert!(!contains(b"TigerVN", b"TigerVNC"));
        assert!(!contains(b"", b"TigerVNC"));
        assert!(contains(b"abc", b""));
    }
}
