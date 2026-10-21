//! Notifications: the balloon messages of X tray icons, on the desktop.
//!
//! An X tray icon shows a balloon with `_NET_SYSTEM_TRAY_BEGIN_MESSAGE` and `_NET_SYSTEM_TRAY_MESSAGE_DATA`
//! (reassembled by [`super::balloons`]); the desktop shows notifications from
//! `org.freedesktop.Notifications`. The icon's title is the notification's application name and summary, the
//! message its body, the balloon's timeout its expiry. (X apps that notify through the session bus
//! themselves need nothing from the tray: they reach GNOME's notification server directly.)

use std::collections::HashMap;

use zbus::blocking::Proxy;
use zbus::zvariant::Value;

/// The notification server.
pub const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// `Notify`'s arguments: an application name, an id (none: a new notification), an icon (none), a summary,
/// a body, no actions, no hints, and how long it stays up.
pub type Args<'a> = (
    &'a str,
    u32,
    &'a str,
    &'a str,
    &'a str,
    Vec<&'a str>,
    HashMap<&'a str, Value<'a>>,
    i32,
);

/// What a balloon message becomes.
pub fn notify_args<'a>(app: &'a str, summary: &'a str, body: &'a str, timeout: i32) -> Args<'a> {
    (
        app,
        0,
        "",
        summary,
        body,
        Vec::new(),
        HashMap::new(),
        timeout,
    )
}

fn proxy(conn: &zbus::blocking::Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(
        conn,
        NOTIFICATIONS,
        NOTIFICATIONS_PATH,
        "org.freedesktop.Notifications",
    )
}

/// Shows a balloon as a notification; the notification's id.
pub fn send(app: &str, summary: &str, body: &str, timeout: i32) -> zbus::Result<u32> {
    let conn = zbus::blocking::Connection::session()?;
    proxy(&conn)?.call("Notify", &notify_args(app, summary, body, timeout))
}

/// Closes a notification ([`send`]'s id).
#[allow(dead_code)]
pub fn close(id: u32) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::session()?;
    proxy(&conn)?.call::<_, _, ()>("CloseNotification", &(id,))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_balloon_is_a_notification() {
        let (app, id, icon, summary, body, actions, hints, timeout) =
            notify_args("Gtk icon", "Gtk icon", "Hello, tray", 3000);
        assert_eq!((app, id, icon), ("Gtk icon", 0, ""));
        assert_eq!((summary, body), ("Gtk icon", "Hello, tray"));
        assert!(actions.is_empty() && hints.is_empty());
        assert_eq!(timeout, 3000);
    }
}
