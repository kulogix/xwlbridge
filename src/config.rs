//! The runtime configuration: backend choices and the wrapped command, parsed
//! X11-style from the command line. The parser, help text and `ArgEnum` trait
//! live in `crate::util::args`; this module just defines the option enums and
//! the [`Config`] fields they're generated from.

use crate::util::{ArgEnum, Geometry};

define_config! {
    pub struct Config {
        /// display number
        display: Option<u32> = value("number"),
        /// write the display number to this fd once the server is ready
        displayfd: Option<u32> = value("fd"),
        /// compositor backend: "auto" picks gnome for mutter (its remote-desktop API on the bus, or a desktop named GNOME whose compositor lacks the wlroots capture protocols) and wlr otherwise; gnome needs the "gnome" build feature
        backend: BackendType = choice("auto"),
        /// X server: the built-in one, or a stock Xvfb (fixed size), Xvnc (TigerVNC; resizes) or rootful Xwayland on a private headless compositor (resizes) mirroring the desktop (complete X11; "xvfb" build feature; if that server dies, the command is stopped and xwlbridge exits 69)
        engine: EngineType = choice("builtin"),
        /// the X server program for -engine xvfb/xvnc/xwayland (default: Xvfb; for xvnc TigerVNC's Xtigervnc, or an Xvnc that is TigerVNC's: RealVNC's Xvnc and set-user-ID programs are refused; for xwayland, Xwayland)
        xserver: Option<String> = value("path"),
        /// screen capture protocol
        screen: ScreenType = choice("auto"),
        /// clipboard protocol
        clipboard: ClipboardType = choice("auto"),
        /// output configuration protocol (used for dynamic resolution; gnome: DisplayConfig, one of the monitor's own modes)
        outmgr: OutMgrType = choice("auto"),
        /// cursor source (if none, it's baked into the screen capture)
        cursor: CursorType = choice("auto"),
        /// output power management protocol (used to wake the screen while capturing; gnome: DisplayConfig PowerSaveMode)
        pwrmgr: PwrMgrType = choice("auto"),
        /// use the wayland seat with this name, or create a transient one
        seat: Option<String> = value("name|transient"),
        /// fallback screen size before outputs are known
        geometry: Option<Geometry> = value("WxH"),
        /// cap the screen capture frame rate (default: 30 for wlr, optimized for vncagent-x11; the monitor refresh rate for gnome, where frames only come on change)
        fps: Option<u32> = value("n"),
        /// show the X windows clients map on the stock engines' display natively on the desktop, with the desktop's input on them going to them only (experimental)
        showwindows: bool = flag,
        /// with -showwindows: the X system tray, its icons in the desktop's top bar (an XEmbed to StatusNotifierItem proxy; X tray apps need the AppIndicator extension, and the menu of each icon opens with the "-notray" form of their entry); off: -showwindows leaves the tray alone, as before 0.3
        tray: bool = noflag,
        /// with -showwindows: show the whole X screen as one window (its X windows composited in stacking order over a plain background, like a rootful X server), instead of one window per X window
        showscreen: bool = flag,
        /// with -showwindows: draw no decorations (title bar, buttons, resize edges) around X windows
        nodecorations: bool = flag,
        /// with -showwindows: use this GNOME button layout (e.g. ":minimize,maximize,close") instead of the desktop's live button-layout setting
        windowbuttons: Option<String> = value("layout"),
        /// with -showwindows: HiDPI/mixed-DPI scaling on (the default): the X screen is the desktop's logical layout times S (the largest monitor scale, rounded up), apps learn S through RandR/Xft.dpi/XSETTINGS, and each shown surface is scaled to its logical size; -nohidpi keeps the old behaviour (X pixels = logical pixels, scaled layouts turn the sync off)
        hidpi: bool = noflag,
        /// let clients change the resolution (RealVNC also needs the dynres hook); on GNOME, one of the monitor's own modes, temporarily: the layout it replaced comes back once the viewers have gone and at exit
        dynres: bool = flag,
        /// with -dynres on GNOME, keep the resolution X clients set when the viewers leave and at exit, instead of restoring the layout it replaced
        dynreskeep: bool = flag,
        /// don't advertise the DAMAGE extension (force clients to poll)
        nodamage: bool = flag,
        /// don't advertise the XKEYBOARD extension on the built-in engine (clients fall back to the core keyboard mapping)
        noxkb: bool = flag,
        /// drop all injected input
        viewonly: bool = flag,
        /// keep the keys/buttons any X client holds when it disconnects down, as Xorg does, until another client releases them (default: only a one-shot client's, for a while, see -xtestgrace; a client connected longer has its keys and buttons released when it leaves)
        xtestpersist: bool = flag,
        /// how long keys/buttons that a one-shot X client (one that disconnects within 1 s of connecting: xdotool keydown, xte, RustDesk's connection per key) leaves held stay down for another client to use and release, counted from the last XTest input of any client, and at most 10 s after it left (ms; default 1000; 0: released when it leaves)
        xtestgrace: Option<u32> = value("ms"),
        /// idle seconds after which a client holding a button stops keeping the pointer from other X clients; an owner that keeps sending pointer input keeps it (0: no ownership; default 10). Built-in engine only
        pointerowner: Option<u32> = value("secs"),
        /// programs whose XTest drives windows on this display and never reaches the desktop, comma-separated ("default": xembedsniproxy,xpra; "none")
        xtesthelpers: Option<String> = value("names"),
        /// programs whose XTest always reaches the desktop, even while a window on this display has the X focus or a grab, comma-separated ("default": the VNC servers and KVM clients; "none"; "all": everyone's; xvfb/xvnc engines)
        xtestdrivers: Option<String> = value("names"),
        /// stock engines (xvfb, xvnc, xwayland): give the display a private accessibility (AT-SPI) bus, so its apps' trees stay off the desktop's screen reader and reach a proxy (tools/x11-a11y-proxy) instead; the wrapped command gets AT_SPI_BUS_ADDRESS, and the root window the AT_SPI_BUS property
        a11ybus: bool = flag,
        /// stock engines (xvfb, xvnc, xwayland) with Composite: a second XInput2 master device ("xwlbridge-reverse") through which a desktop-side proxy (tools/x11-a11y-proxy) types and clicks into windows on this display without echoing to the desktop; the display's own pointer and keyboard then reach no X window (a grab and a focus instead of the input shield)
        reverseinput: bool = flag,
        /// recreate the remote desktop session even after it was stopped from GNOME's panel
        reclaim: bool = flag,
        /// GNOME backend: keep the remote desktop session (capture, input, and GNOME's sharing indicator) up this many seconds after the last X client watched (DAMAGE), polled the screen or injected input (default 10), or "always", from start
        warm: Option<Warm> = value("secs|always"),
        /// GNOME backend: keep the session up this many seconds after an X client that read the screen went away, so a burst of one-shot screenshots (scrot, xwd, Java's Robot) finds it running (default 30); input alone keeps it -warm seconds
        linger: Option<u32> = value("secs"),
        /// GNOME backend: run the remote desktop session from start to stop, for hosts that need instant one-shot captures (the sharing indicator stays up); the same as -warm always
        alwayson: bool = flag,
        /// GNOME backend, xvfb/xvnc engines: what an X client connecting while the session is stopped does: "none" (only its screen reads and input start the session) or "start" (start it at once, so a client that reads a little later gets the current desktop, not black) (default start)
        coldconnect: Option<ColdConnect> = value("none|start"),
        /// GNOME backend, built-in engine: milliseconds a client's screen read may wait while the session starts, so a one-shot reader gets the current desktop (default 2000; 0: never)
        coldwait: Option<u32> = value("ms"),
        /// GNOME backend, xvfb/xvnc engines: the desktop's Xwayland display that -clipboard xwayland relays selections through ("auto": the DISPLAY xwlbridge was started with)
        xwayland: Option<String> = value("display|auto"),
        /// capture GNOME monitors with RecordMonitor (damage metadata) instead of RecordArea; only safe on mutter >= 51, where disabling a recorded monitor no longer crashes the session (mutter#2718)
        recordmonitor: bool = flag,
        /// don't bridge the PRIMARY selection (middle-click paste of selected text)
        noprimary: bool = flag,
        /// where RealVNC's own UI process goes (vncserverui: tray icon, status, Options and Information Center dialogs, connection notices and prompts, chat, file transfer, and its clipboard): an X display such as the desktop's Xwayland, "auto" (the DISPLAY xwlbridge was started with; the default for vncserver-x11 started with a local DISPLAY) or "none" (our display, out of sight); runs vncserver-x11-core directly, without RealVNC's root helper (always with the builtin engine, on whose display RealVNC's UI would otherwise crash); until the display answers the UI waits on ours
        uidisplay: Option<String> = value("display|auto|none"),
        /// run until stopped without launching a command
        nowrap: bool = flag,
        /// profile the screen capture
        profile: bool = flag,
        /// trace requests to stderr
        xtrace: bool = flag,
        /// trace clipboard and selection handling to stderr
        cliptrace: bool = flag,
        /// suppress informational logging
        quiet: bool = flag,
        /// log extra detail
        verbose: bool = flag,
    }
}

impl Config {
    /// Whether X tray icons are shown on the desktop: the `-showwindows` tray, with the gnome and xvfb
    /// features (`-notray` turns it off; without them there is no tray at all).
    #[cfg_attr(not(all(feature = "gnome", feature = "xvfb")), allow(dead_code))]
    pub fn tray(&self) -> bool {
        let _ = self;
        cfg!(all(feature = "gnome", feature = "xvfb")) && self.tray
    }

    /// Whether HiDPI scaling is on: `-nohidpi` (config `hidpi=off`) or
    /// `XWLBRIDGE_HIDPI=off` turns it off. Only `-showwindows` uses it.
    pub fn hidpi(&self) -> bool {
        self.hidpi
            && std::env::var("XWLBRIDGE_HIDPI")
                .map_or(true, |v| !v.trim().eq_ignore_ascii_case("off"))
            // without the window sync there are no shown windows to scale:
            // keep 0.1.2's X screen (the extension=off fallback)
            && std::env::var("XWLBRIDGE_WINDOWS_EXTENSION").map_or(true, |v| {
                !v.trim().eq_ignore_ascii_case("off")
            })
    }
}

impl ArgEnum for EngineType {
    const VARIANTS: &'static [&'static str] = &["builtin", "xvfb", "xvnc", "xwayland"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "builtin" => Self::Builtin,
            "xvfb" => Self::Xvfb,
            "xvnc" => Self::Xvnc,
            "xwayland" => Self::Xwayland,
            _ => return None,
        })
    }
}

impl ArgEnum for BackendType {
    const VARIANTS: &'static [&'static str] = &["auto", "wlr", "gnome"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "wlr" => Self::Wlr,
            "gnome" => Self::Gnome,
            _ => return None,
        })
    }
}

impl ArgEnum for CursorType {
    const VARIANTS: &'static [&'static str] = &["auto", "none"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::Baked,
            _ => return None,
        })
    }
}

impl ArgEnum for ScreenType {
    const VARIANTS: &'static [&'static str] = &["auto", "none", "screencopy", "imagecopy"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            "screencopy" => Self::Screencopy,
            "imagecopy" => Self::Imagecopy,
            _ => return None,
        })
    }
}

impl ArgEnum for OutMgrType {
    const VARIANTS: &'static [&'static str] = &["auto", "none", "wlr", "gnome"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            "wlr" => Self::Wlr,
            "gnome" => Self::Gnome,
            _ => return None,
        })
    }
}

impl ArgEnum for PwrMgrType {
    const VARIANTS: &'static [&'static str] = &["auto", "none", "wlr", "gnome"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            "wlr" => Self::Wlr,
            "gnome" => Self::Gnome,
            _ => return None,
        })
    }
}

impl ArgEnum for ClipboardType {
    const VARIANTS: &'static [&'static str] =
        &["auto", "none", "wlr", "ext", "session", "xwayland"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            "wlr" => Self::Wlr,
            "ext" => Self::Ext,
            "session" => Self::Session,
            "xwayland" => Self::Xwayland,
            _ => return None,
        })
    }
}

/// `-warm`: how long the session outlives its last watcher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Warm {
    Secs(u32),
    /// From the start, and never stopped for lack of watchers.
    Always,
}

impl std::str::FromStr for Warm {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s.eq_ignore_ascii_case("always") {
            return Ok(Warm::Always);
        }
        s.trim()
            .parse()
            .map(Warm::Secs)
            .map_err(|_| "expected seconds or \"always\"".to_string())
    }
}

/// `-coldconnect`: what a client connecting to a stock engine while the
/// session is stopped does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColdConnect {
    /// Nothing: only its reads and input start the session.
    None,
    /// The session starts at once.
    Start,
}

impl std::str::FromStr for ColdConnect {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(ColdConnect::None),
            "start" => Ok(ColdConnect::Start),
            _ => Err("expected none or start".to_string()),
        }
    }
}

/// Which X server clients talk to: xwlbridge's own (lean, implements what VNC
/// servers need) or a stock Xorg-based server that an agent keeps in sync with
/// the desktop (every X extension, for any X client): Xvfb, which is everywhere
/// but has a fixed screen size, TigerVNC's Xvnc (its RFB listener confined
/// to a private socket), which resizes with the desktop, or a rootful Xwayland
/// on a private headless compositor of ours (what RHEL 10 ships, and the only
/// one Raspberry Pi OS installs), which resizes too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineType {
    Builtin,
    Xvfb,
    Xvnc,
    Xwayland,
}

/// Which compositor-facing implementation to run. `Wlr` is the Wayland-protocol
/// client (wlroots and friends); `Gnome` talks to mutter's D-Bus remote-desktop
/// APIs instead, since mutter implements none of the wlr capture/input protocols.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendType {
    Auto,
    Wlr,
    Gnome,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorType {
    Auto,
    Baked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenType {
    Auto,
    None,
    Screencopy,
    Imagecopy,
}

/// `wlr`/`ext`: the Wayland data-control protocols (wlr backend). `session`:
/// mutter's remote-desktop clipboard (GNOME backend; CLIPBOARD text only, and
/// only while a session runs). `xwayland`: the GNOME backend's xvfb/xvnc
/// engines relay selections with the desktop's Xwayland, which mutter keeps in
/// step with Wayland (every target, PRIMARY too, no session needed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardType {
    Auto,
    None,
    Wlr,
    Ext,
    Session,
    Xwayland,
}

/// How `-dynres` reaches the compositor: wlr-output-management (wayland
/// backend) or mutter's DisplayConfig (gnome backend).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutMgrType {
    Auto,
    None,
    Wlr,
    Gnome,
}

/// How sleeping outputs are woken for capture: wlr-output-power-management
/// (wayland backend) or DisplayConfig's `PowerSaveMode` (gnome backend).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PwrMgrType {
    Auto,
    None,
    Wlr,
    Gnome,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeatChoice<'a> {
    First,
    Named(&'a str),
    Transient,
}

impl OutMgrType {
    pub fn wants_wlr(self) -> bool {
        matches!(self, Self::Auto | Self::Wlr)
    }
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn wants_gnome(self) -> bool {
        matches!(self, Self::Auto | Self::Gnome)
    }
}

impl PwrMgrType {
    pub fn wants_wlr(self) -> bool {
        matches!(self, Self::Auto | Self::Wlr)
    }
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn wants_gnome(self) -> bool {
        matches!(self, Self::Auto | Self::Gnome)
    }
}

impl ScreenType {
    pub fn wants_screencopy(self) -> bool {
        matches!(self, Self::Auto | Self::Screencopy)
    }
    pub fn wants_imagecopy(self) -> bool {
        matches!(self, Self::Auto | Self::Imagecopy)
    }
}

impl ClipboardType {
    pub fn wants_wlr(self) -> bool {
        matches!(self, Self::Auto | Self::Wlr)
    }
    pub fn wants_ext(self) -> bool {
        matches!(self, Self::Auto | Self::Ext)
    }
}

impl Config {
    pub fn input_policy(&self) -> crate::bridge::input::InputPolicy {
        crate::bridge::input::InputPolicy {
            persist: self.xtestpersist,
            linger: std::time::Duration::from_millis(
                self.xtestgrace
                    .unwrap_or(crate::bridge::input::LINGER_DEFAULT_MS)
                    .into(),
            ),
            owner_timeout: std::time::Duration::from_secs(self.pointerowner.unwrap_or(10).into()),
            view_only: self.viewonly,
        }
    }

    pub fn xkb(&self) -> bool {
        !self.noxkb
    }

    /// Whose injected input reaches the desktop (`-xtesthelpers`,
    /// `-xtestdrivers`).
    pub fn xtest_rules(&self) -> crate::bridge::injectors::Rules {
        crate::bridge::injectors::Rules::new(
            self.xtesthelpers.as_deref(),
            self.xtestdrivers.as_deref(),
        )
    }

    /// `-warm`, with its default; `-alwayson` is `-warm always`.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn warm(&self) -> Warm {
        if self.alwayson {
            return Warm::Always;
        }
        self.warm.unwrap_or(Warm::Secs(10))
    }

    /// `-linger`, with its default.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn linger(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.linger.unwrap_or(30).into())
    }

    /// `-coldconnect`, with its default.
    #[cfg_attr(not(feature = "xvfb"), allow(dead_code))]
    pub fn coldconnect(&self) -> ColdConnect {
        self.coldconnect.unwrap_or(ColdConnect::Start)
    }

    /// `-coldwait`, with its default.
    pub fn coldwait(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.coldwait.unwrap_or(2000).into())
    }

    pub fn damage(&self) -> bool {
        !self.nodamage
    }

    /// The `-seat` option, decoded. The literal `transient` asks for a seat of
    /// our own rather than one called that.
    pub fn seat_choice(&self) -> SeatChoice<'_> {
        match self.seat.as_deref() {
            None => SeatChoice::First,
            Some(s) if s.eq_ignore_ascii_case("transient") => SeatChoice::Transient,
            Some(s) => SeatChoice::Named(s),
        }
    }

    /// Whether to bind wlr-output-management: dynamic resolution is on, and the
    /// protocol choice allows it.
    pub fn outmgr_wants_wlr(&self) -> bool {
        self.dynres && self.outmgr.wants_wlr()
    }

    /// Whether the gnome backend changes resolutions through DisplayConfig.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn outmgr_wants_gnome(&self) -> bool {
        self.dynres && self.outmgr.wants_gnome()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn repeated_engine_with_different_values_is_an_error() {
        let e = Config::parse_args("xwlbridge", &argv(&["-engine", "xvfb", "-engine", "xvnc"]))
            .unwrap_err();
        assert!(e.contains("-engine"), "{e}");
        assert!(e.contains("xvfb") && e.contains("xvnc"), "{e}");
    }

    #[test]
    fn repeated_engine_with_the_same_value_warns_once() {
        let (c, w) = Config::parse_args(
            "xwlbridge",
            &argv(&["-engine", "xvfb", "-engine", "Xvfb", "-engine", "xvfb"]),
        )
        .unwrap();
        assert_eq!(c.engine, EngineType::Xvfb);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("-engine") && w[0].contains("xvfb"),
            "{}",
            w[0]
        );
    }

    #[test]
    fn repeated_backend_with_different_values_is_an_error() {
        let e = Config::parse_args(
            "xwlbridge",
            &argv(&["-backend", "gnome", "-backend", "wlr"]),
        )
        .unwrap_err();
        assert!(e.contains("-backend"), "{e}");
        assert!(e.contains("gnome") && e.contains("wlr"), "{e}");
    }

    #[test]
    fn repeated_value_with_different_values_is_an_error() {
        let e =
            Config::parse_args("xwlbridge", &argv(&["-display", "1", "-display=2"])).unwrap_err();
        assert!(e.contains("-display"), "{e}");
        assert!(e.contains('1') && e.contains('2'), "{e}");
    }

    #[test]
    fn repeated_value_with_an_equal_value_warns_once() {
        let (c, w) = Config::parse_args(
            "xwlbridge",
            &argv(&["-fps", "30", "-fps", "30", "-fps", "30"]),
        )
        .unwrap();
        assert_eq!(c.fps, Some(30));
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn repeated_string_value_compares_case_sensitively() {
        let e = Config::parse_args(
            "xwlbridge",
            &argv(&["-xserver", "Xvfb", "-xserver", "xvfb"]),
        )
        .unwrap_err();
        assert!(
            e.contains("-xserver") && e.contains("Xvfb") && e.contains("xvfb"),
            "{e}"
        );
    }

    #[test]
    fn options_once_and_a_command_stay_as_they_were() {
        let (c, w) = Config::parse_args(
            "xwlbridge",
            &argv(&[
                "-engine", "xvfb", "-backend", "gnome", "--", "cmd", "-engine",
            ]),
        )
        .unwrap();
        assert_eq!(c.engine, EngineType::Xvfb);
        assert_eq!(c.backend, BackendType::Gnome);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.command, ["cmd", "-engine"].map(std::ffi::OsString::from));
    }
}
