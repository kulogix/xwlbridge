//! Which X clients inject input meant for the desktop, and which drive
//! windows on the bridge's own display (`-xtesthelpers`, `-xtestdrivers`).
//!
//! A client is known by its program: X servers know a local client's pid
//! (the built-in one from the socket, Xvfb/Xvnc through X-Resource), and
//! `/proc` names the program. See `xvfb::attrib` for how the stock engines
//! use this; the built-in engine drops engine helpers' input.

/// Programs whose XTest drives windows on this display, never the desktop.
pub const DEFAULT_HELPERS: &[&str] = &["xembedsniproxy", "xpra"];

/// Programs whose XTest is the desktop's, whatever the X server does with it:
/// VNC servers (RealVNC's agent and cores, x11vnc, TigerVNC's scraping server,
/// GNOME's old Vino, KDE's krfb) and keyboard/mouse-sharing clients.
pub const DEFAULT_DRIVERS: &[&str] = &[
    "vncagent",
    "vncserver-x11",
    "vncserver-x11-core",
    "x11vnc",
    "x0vncserver",
    "vino-server",
    "krfb",
    "barrierc",
    "input-leapc",
    "deskflow-core",
    "synergyc",
    "synergy-core",
];

/// What kind of injector an X client is (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The agent itself (releases what departed clients left held; only the
    /// stock engines' agent has one).
    #[cfg_attr(not(feature = "xvfb"), allow(dead_code))]
    Agent,
    Helper,
    Driver,
    Other,
}

/// The `-xtesthelpers` and `-xtestdrivers` lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    helpers: Vec<String>,
    drivers: Vec<String>,
    /// `-xtestdrivers all`: nobody's input is judged by where it goes.
    all_drivers: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Rules::new(None, None)
    }
}

/// A comma-separated list: `none` (or nothing) is empty, `default` stands for
/// the built-in list.
fn parse_list(arg: Option<&str>, default: &[&str]) -> Vec<String> {
    let Some(arg) = arg else {
        return default.iter().map(|s| s.to_string()).collect();
    };
    let mut out = Vec::new();
    for name in arg.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match name {
            "none" => {}
            "default" => out.extend(default.iter().map(|s| s.to_string())),
            n => out.push(n.to_string()),
        }
    }
    out
}

impl Rules {
    pub fn new(helpers: Option<&str>, drivers: Option<&str>) -> Rules {
        let all_drivers = drivers.is_some_and(|d| d.split(',').any(|n| n.trim() == "all"));
        Rules {
            helpers: parse_list(helpers, DEFAULT_HELPERS),
            drivers: if all_drivers { Vec::new() } else { parse_list(drivers, DEFAULT_DRIVERS) },
            all_drivers,
        }
    }

    /// The class of a process going by any of `names` (see [`process_names`]).
    /// A helper is a helper even if it is also listed as a driver.
    pub fn classify(&self, names: &[String]) -> Class {
        if names.iter().any(|n| self.helpers.contains(n)) {
            Class::Helper
        } else if self.all_drivers || names.iter().any(|n| self.drivers.contains(n)) {
            Class::Driver
        } else {
            Class::Other
        }
    }

    /// Whether any helper is listed (otherwise there is nothing to look up on
    /// the builtin engine).
    pub fn has_helpers(&self) -> bool {
        !self.helpers.is_empty()
    }
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The names process `pid` goes by: its `comm` (the script's name, for a
/// script started through its #! line), its executable and its argv[0], and,
/// when argv[0] is an interpreter, the script it runs (`python3 helper.py`).
pub fn process_names(pid: u32) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        let n = n.trim();
        if !n.is_empty() && !names.iter().any(|m| m == n) {
            names.push(n.to_string());
        }
    };
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        add(&comm);
    }
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        let exe = exe.to_string_lossy();
        add(basename(exe.trim_end_matches(" (deleted)")));
    }
    if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let mut args = cmdline.split(|&b| b == 0).map(|a| String::from_utf8_lossy(a).into_owned());
        if let Some(argv0) = args.next() {
            let a0 = basename(&argv0).to_string();
            add(&a0);
            let interpreter = ["python", "perl", "ruby", "node", "sh", "bash", "dash"]
                .iter()
                .any(|i| a0.starts_with(i));
            if interpreter {
                // the first argument that isn't an option is the script
                if let Some(script) = args.find(|a| !a.starts_with('-')) {
                    add(basename(&script));
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_default_extend_and_empty() {
        let r = Rules::new(None, None);
        assert_eq!(r.classify(&["xembedsniproxy".into()]), Class::Helper);
        assert_eq!(r.classify(&["x11vnc".into()]), Class::Driver);
        assert_eq!(r.classify(&["xdotool".into()]), Class::Other);
        let r = Rules::new(Some("none"), Some("default,myvnc"));
        assert_eq!(r.classify(&["xembedsniproxy".into()]), Class::Other);
        assert_eq!(r.classify(&["myvnc".into()]), Class::Driver);
        assert_eq!(r.classify(&["x11vnc".into()]), Class::Driver);
        assert!(!r.has_helpers());
        let r = Rules::new(Some("tray, xdotool"), Some("all"));
        assert_eq!(r.classify(&["python3".into(), "xdotool".into()]), Class::Helper);
        assert_eq!(r.classify(&["anything".into()]), Class::Driver);
        let r = Rules::new(Some(""), Some("none"));
        assert_eq!(r.classify(&["x11vnc".into()]), Class::Other);
    }

    #[test]
    fn a_helper_listed_as_a_driver_too_is_a_helper() {
        let r = Rules::new(Some("x11vnc"), None);
        assert_eq!(r.classify(&["x11vnc".into()]), Class::Helper);
    }

    #[test]
    fn our_own_process_has_names() {
        let names = process_names(std::process::id());
        assert!(!names.is_empty());
        assert!(process_names(u32::MAX).is_empty());
    }

}
