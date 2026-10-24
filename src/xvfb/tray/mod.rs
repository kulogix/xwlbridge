//! `-showwindows`: the XEmbed system tray, shown on the desktop.
//!
//! GNOME has no system tray of its own: what it shows is a StatusNotifierItem, through the AppIndicator
//! extension. An X application's tray icon (GTK's `Gtk.StatusIcon`, Java's `SystemTray`, Tk's `tk systray`,
//! Wine's notification area) is an XEmbed client instead: it asks the owner of `_NET_SYSTEM_TRAY_S0` to
//! dock it. [`start`] is that owner on the engine display and a StatusNotifierItem on the session bus at
//! the same time (what KDE's xembedsniproxy does), so an X icon shows in GNOME's top bar:
//!
//! - **the manager** ([`manager`]) takes `_NET_SYSTEM_TRAY_S<screen>` if nobody else has it (the X
//!   session's own tray then keeps it), answers `SYSTEM_TRAY_REQUEST_DOCK` and follows each icon;
//! - **docking** ([`dock`]): each icon is reparented into an override-redirect container of ours, which
//!   stays out of the way (parked off screen, behind the agent's full-screen window, and marked so that the
//!   reverse path never shows it on the desktop);
//! - **pixels** ([`pixels`]): the icon is read back with `GetImage` when its DAMAGE says it changed (at
//!   most ten times a second) and becomes the StatusNotifierItem's `IconPixmap`;
//! - **the bus** ([`bus`], [`item`], [`menu`]): `org.kde.StatusNotifierItem` for each icon, its
//!   `com.canonical.dbusmenu`, and the balloon messages as notifications ([`notify`]);
//! - **clicks** ([`clicks`]): a StatusNotifierItem only reports clicks, so they are turned into X button
//!   events on the icon window, the way a click on the icon would have been made on an X panel.

pub mod balloons;
pub mod clicks;
pub mod dock;
#[cfg(feature = "gnome")]
pub mod item;
pub mod manager;
#[cfg(feature = "gnome")]
pub mod menu;
#[cfg(feature = "gnome")]
pub mod notify;
pub mod pixels;
pub mod pointer;
#[cfg(feature = "gnome")]
pub mod session;

use crate::xvfb::Auth;

/// The X resource base of the tray's own connection (0: none yet). Its XTest clicks are the agent's input,
/// as the reverse path's are (`xvfb::record` classifies it so: it must never reach the desktop).
static CONNECTION_BASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// S (X pixels = logical pixels x S), as the reverse path last reported it. The StatusNotifierItem calls
/// (`Activate`, `Scroll`) and the extension's pointer are in the desktop's logical pixels; the icon, the
/// XTest click and its container are in X pixels. The tray reads this at every click (it changes live).
static SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Whether `xid_base` is the tray thread's X connection.
pub fn is_tray_client(xid_base: u32) -> bool {
    xid_base != 0 && CONNECTION_BASE.load(std::sync::atomic::Ordering::Relaxed) == xid_base
}

/// Records the tray's connection base (called by the tray thread once it is connected).
pub fn set_connection_base(xid_base: u32) {
    CONNECTION_BASE.store(xid_base, std::sync::atomic::Ordering::Relaxed);
}

/// The X <-> logical scale the tray converts with (the reverse path sets it on every sync loop).
pub fn set_scale(scale: u32) {
    SCALE.store(scale.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// The scale in force (1 without HiDPI).
pub fn scale() -> u32 {
    SCALE.load(std::sync::atomic::Ordering::Relaxed).max(1)
}

/// Starts the tray for `-showwindows` on the engine display: `None` (and a log line) if another tray has the
/// selection or the X server can't be reached.
///
/// `devs` is the reverse direction's master device once `-showwindows` has it: the pointer the icons are
/// clicked with (so that a click stays on the display, see [`clicks`]).
pub fn start(display: u32, auth: Auth, pointer: Option<u16>) -> Option<manager::Running> {
    let screen = 0;
    match manager::spawn(display, auth, screen, None, pointer) {
        Ok(running) => {
            crate::log!(
                "-showwindows: system tray: _NET_SYSTEM_TRAY_S{screen} taken, X tray icons are shown on the desktop"
            );
            Some(running)
        }
        Err(e) => {
            crate::warning!("-showwindows: no system tray ({e})");
            None
        }
    }
}

/// The size every icon is given, in pixels (`_NET_SYSTEM_TRAY_ICON_SIZE`; GNOME's and KDE's trays use 22
/// for a status icon too).
pub const ICON_SIZE: u16 = 22;

/// The most icon updates a second (a DAMAGE burst from an animated icon costs one read, and one D-Bus
/// signal, per frame).
pub const MAX_ICON_UPDATES: u32 = 10;

/// The tray selection of `screen`: `_NET_SYSTEM_TRAY_S0` and the like.
pub fn selection_name(screen: u32) -> String {
    format!("_NET_SYSTEM_TRAY_S{screen}")
}

/// What an icon looks like now: everything the desktop is told about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Look {
    /// `_NET_WM_NAME` (or `WM_NAME`) of the icon window.
    pub title: String,
    /// `WM_CLASS` of the icon window (provider and class): its class is the item's `Id`.
    pub class: Option<String>,
    /// The icon's pixels, as last read.
    pub image: Option<pixels::Image>,
    /// Whether the icon has a menu (a right click, or the menu of its StatusNotifierItem, is worth
    /// offering).
    pub menu: bool,
}

impl Look {
    /// The `Id` the desktop knows the icon by: its class, or its title.
    pub fn id(&self, window: u32) -> String {
        let class = self.class.as_deref().map(str::trim).unwrap_or("");
        if !class.is_empty() {
            return class.to_string();
        }
        let title = self.title.trim();
        if title.is_empty() {
            format!("xwlbridge-tray-{window}")
        } else {
            title.to_string()
        }
    }

    /// The icon as the StatusNotifierItem's `IconPixmap`.
    pub fn pixmaps(&self) -> Vec<pixels::Pixmap> {
        pixels::pixmaps(self.image.as_ref(), [0xff, 0xff, 0xff])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selection_is_named_after_the_screen() {
        assert_eq!(selection_name(0), "_NET_SYSTEM_TRAY_S0");
        assert_eq!(selection_name(3), "_NET_SYSTEM_TRAY_S3");
    }

    #[test]
    fn an_icon_is_known_by_its_class_then_its_title() {
        let mut look = Look {
            class: Some("gtk-icon".into()),
            title: "Title".into(),
            ..Look::default()
        };
        assert_eq!(look.id(7), "gtk-icon");
        look.class = Some("  ".into());
        assert_eq!(look.id(7), "Title");
        look.title = String::new();
        assert_eq!(look.id(7), "xwlbridge-tray-7");
        assert!(look.pixmaps().is_empty());
    }

    #[test]
    fn the_trays_connection_is_known_by_its_base() {
        // no connection yet: no base is the tray's
        set_connection_base(0);
        assert!(!is_tray_client(0));
        assert!(!is_tray_client(0x200000));
        set_connection_base(0x200000);
        assert!(is_tray_client(0x200000));
        assert!(!is_tray_client(0x400000));
        set_connection_base(0);
    }
}
