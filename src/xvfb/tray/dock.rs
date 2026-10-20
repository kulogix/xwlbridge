//! Docking one icon: its container, `_XEMBED_INFO`, and where it is (out of the way).
//!
//! The tray's container windows are mapped (the X server sends no events to, and draws nothing of, an
//! unmapped window's children) and at [`PARK`], off the screen to the left, behind the agent's full-screen
//! window like everything else on the engine display. [`CONTAINER`] marks them, so anything else that looks
//! at the display's windows (the reverse path above all: a tray icon is not a window to show on the
//! desktop) can tell them apart.
//!
//! An icon is shown only while its `_XEMBED_INFO` says `XEMBED_MAPPED` (a GTK `Gtk.StatusIcon` first docks
//! and then sets it; a KDE or Qt icon may simply map itself): [`shown`] is that decision, and a changed
//! `_XEMBED_INFO` is a `PropertyNotify` on the icon window the manager listens for.

use x11rb::protocol::xproto::Window;

/// Where container windows live: just off the top left of the usual screen sizes, so that a stray
/// screenshot of the screen is all a user could ever see of them.
pub const PARK: (i16, i16) = (-200, -200);

/// `_XEMBED_INFO`'s flags.
pub const XEMBED_MAPPED: u32 = 1 << 0;

/// The property every container carries (with its icon window as its value): this is ours, and it holds a
/// tray icon.
pub const CONTAINER: &str = "_XWLBridge_TRAY_CONTAINER";

/// Whether an icon whose `_XEMBED_INFO` is `info` (none: the property is unset) is to be shown.
pub fn shown(info: Option<&[u32]>) -> bool {
    match info {
        Some(info) => info.first().is_some_and(|f| f & XEMBED_MAPPED != 0),
        // no _XEMBED_INFO at all: a plain XEMBED 0 client, shown once it maps itself
        None => false,
    }
}

/// One docked icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icon {
    /// The icon's own window (a child of `container`).
    pub window: Window,
    /// Our container (see [`CONTAINER`]).
    pub container: Window,
    /// The icon's `_XEMBED_INFO`, as last read.
    pub info: Option<Vec<u32>>,
    /// The icon's DAMAGE object.
    pub damage: u32,
    /// The icon window's last size, as the desktop shows it.
    pub size: (u16, u16),
    /// The title last sent to the desktop.
    pub title: String,
}

impl Icon {
    pub fn new(window: Window, container: Window, damage: u32) -> Icon {
        Icon {
            window,
            container,
            info: None,
            damage,
            size: (0, 0),
            title: String::new(),
        }
    }

    /// Whether the icon should be on screen.
    pub fn shown(&self) -> bool {
        shown(self.info.as_deref())
    }

    /// The icon's `Id` on the desktop: its class (what a task bar would group it by), or its title, or its
    /// window.
    pub fn id(&self, class: Option<&str>, title: &str) -> String {
        let class = class.map(str::trim).filter(|c| !c.is_empty());
        let title = title.trim();
        match (class, title.is_empty()) {
            (Some(c), _) => c.to_string(),
            (None, false) => title.to_string(),
            (None, true) => format!("xwlbridge-tray-{}", self.window),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_mapped_xembed_icon_is_shown() {
        assert!(!shown(None));
        assert!(!shown(Some(&[0, 0])));
        assert!(shown(Some(&[XEMBED_MAPPED, 0])));
        assert!(shown(Some(&[XEMBED_MAPPED | 2, 0])));
        assert!(!shown(Some(&[])));
    }

    #[test]
    fn the_id_is_the_class_then_the_title() {
        let mut icon = Icon::new(7, 8, 9);
        assert_eq!(icon.id(Some("java"), "J"), "java");
        assert_eq!(icon.id(Some("  "), "J"), "J");
        assert_eq!(icon.id(None, " "), "xwlbridge-tray-7");
        icon.info = Some(vec![1, 0]);
        assert!(icon.shown());
    }
}
