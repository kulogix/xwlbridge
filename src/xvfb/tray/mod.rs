//! `-showwindows`: the XEmbed system tray, shown on the desktop.
//!
//! GNOME has no system tray of its own: what it shows is a StatusNotifierItem, through the AppIndicator
//! extension. An X application's tray icon (GTK's `Gtk.StatusIcon`, Java's `SystemTray`, Tk's `tk systray`,
//! Wine's notification area) is an XEmbed client instead: it asks the owner of `_NET_SYSTEM_TRAY_S0` to
//! dock it. [`spawn`] is that owner on the engine display and a StatusNotifierItem on the session bus at
//! the same time (what KDE's xembedsniproxy does), so an X icon shows in GNOME's top bar:
//!
//! - **the manager** ([`manager`]) takes `_NET_SYSTEM_TRAY_S<screen>` if nobody else has it (the X
//!   session's own tray then keeps it), answers `SYSTEM_TRAY_REQUEST_DOCK` and follows each icon;
//! - **docking** ([`dock`]): each icon is reparented into an override-redirect container of ours, which
//!   stays out of the way (parked off screen, behind the agent's full-screen window, and marked so that the
//!   reverse path never shows it on the desktop);
//! - **pixels** ([`pixels`]): the icon is read back with `GetImage` when its DAMAGE says it changed (at
//!   most ten times a second) and becomes the StatusNotifierItem's `IconPixmap`;
//! - **the bus** ([`sni`]): `org.kde.StatusNotifierItem` for each icon, its `com.canonical.dbusmenu`, and
//!   the balloon messages as notifications;
//! - **clicks** ([`clicks`]): a StatusNotifierItem only reports clicks, so they are turned into X button
//!   events on the icon window, the way a click on the icon would have been made on an X panel.
//!
//! # State
//!
//! The manager (docking, `_XEMBED_INFO`, balloons, DAMAGE throttling, clicks) is in place; the session-bus
//! side ([`sni`]) only has its data types and unit tests so far: nothing exports the items yet, so the
//! manager's updates are logged and no icon reaches the desktop. The item types are `dead_code` until the
//! thread that exports them exists.
#![allow(dead_code)]

pub mod balloons;
pub mod clicks;
pub mod dock;
pub mod manager;
pub mod pixels;
#[cfg(feature = "gnome")]
pub mod sni;

/// Starts the tray for `-showwindows` on the engine display: `None` (and a log line) if another tray has the
/// selection or the X server can't be reached.
///
/// The tray outlives an agent restart (it is a client of the engine's X server, not of the desktop): the
/// caller keeps the returned thread handle for as long as the display lives.
pub fn start(display: u32, auth: crate::xvfb::Auth) -> Option<manager::Running> {
    let screen = 0;
    match manager::spawn(display, auth, screen) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selection_is_named_after_the_screen() {
        assert_eq!(selection_name(0), "_NET_SYSTEM_TRAY_S0");
        assert_eq!(selection_name(3), "_NET_SYSTEM_TRAY_S3");
    }
}
