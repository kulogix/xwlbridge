//! Docking one icon: its container, `_XEMBED_INFO`, and where it is (out of the way).
//!
//! The tray's container windows are mapped (the X server sends no events to, and draws nothing of, an
//! unmapped window's children) and at [`PARK`], off the screen to the left, like the agent's own windows on
//! the engine display. [`CONTAINER`] marks them, so anything else that looks at the display's windows (the
//! reverse path above all: a tray icon is not a window to show on the desktop) can tell them apart; a
//! container never has the shape of a window the desktop could show (it is `ICON_SIZE` square).
//!
//! An icon is shown only as far as the app shows it: an icon that had no `_XEMBED_INFO` when it docked (a
//! plain XEMBED 0 client, or one still on its way to `XEMBED_MAPPED`) is followed when it maps and unmaps
//! itself, one that sets `_XEMBED_MAPPED` is shown only while the flag is set.

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
    /// Whether the app has docked an icon without `_XEMBED_INFO` (an XEMBED 0 client): it shows itself, so
    /// its own map state is followed.
    pub self_managed: bool,
}

impl Icon {
    pub fn new(window: Window, container: Window, damage: u32) -> Icon {
        Icon {
            window,
            container,
            info: None,
            damage,
            size: (0, 0),
            self_managed: true,
        }
    }

    /// Whether the icon should be on screen, given whether its window is mapped on the X side.
    pub fn shown(&self) -> bool {
        if self.self_managed {
            return true;
        }
        shown(self.info.as_deref())
    }

    /// The icon's `Id` on the desktop: its class (what a task bar would group it by), or its title, or its
    /// window.
    #[allow(dead_code)]
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
    fn an_icon_without_xembed_info_is_followed() {
        let mut icon = Icon::new(7, 8, 9);
        icon.self_managed = false;
        assert!(!icon.shown());
        icon.info = Some(vec![XEMBED_MAPPED, 0]);
        assert!(icon.shown());
        icon.self_managed = true;
        assert!(icon.shown());
    }

    #[test]
    fn the_id_is_the_class_then_the_title() {
        let icon = Icon::new(7, 8, 9);
        assert_eq!(icon.id(Some("java"), "J"), "java");
        assert_eq!(icon.id(Some("  "), "J"), "J");
        assert_eq!(icon.id(None, " "), "xwlbridge-tray-7");
    }
}
