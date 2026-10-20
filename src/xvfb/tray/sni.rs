//! The tray icons on the session bus: `org.kde.StatusNotifierItem` and `com.canonical.dbusmenu`.
//!
//! Each docked icon is exported as a StatusNotifierItem (what GNOME's AppIndicator extension shows in the top
//! bar) and has a small `com.canonical.dbusmenu`:
//!
//! | the desktop | the icon |
//! |---|---|
//! | left click | the menu: **Open** (a left click), **Menu** (a middle click) and **Middle click** |
//! | double click | `Activate`: a left double click on the icon |
//! | middle click | `SecondaryActivate`: a middle click |
//! | wheel | `Scroll` |
//!
//! GNOME's AppIndicator never calls `ContextMenu`, and its left click only opens the menu (and does nothing
//! for an item without one): hence the menu, with which both a plain click and the app's own X menu are one
//! click away, and `Activate` for a double click.
//!
//! Without a `StatusNotifierWatcher` on the bus (the AppIndicator extension not installed or not enabled)
//! the icons are still docked and kept up to date; one log line says that nothing shows them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, Value};

use super::manager::Handle as Tray;
use super::pixels::{self, Image};

pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
pub const WATCHER_PATH: &str = "/StatusNotifierWatcher";
pub const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
pub const ITEM_PATH: &str = "/StatusNotifierItem";
pub const MENU_IFACE: &str = "com.canonical.dbusmenu";
pub const MENU_PATH: &str = "/StatusNotifierItem/Menu";

/// What a menu entry does on the icon: `(id, label, button, double click)`.
pub const MENU: &[(i32, &str, u8, bool)] = &[
    (1, "Open", 1, false),
    (2, "Menu", 2, false),
    (3, "Middle click", 3, false),
];

/// How long a balloon stays up when the app leaves it to the tray.
pub const BALLOON_TIMEOUT: i32 = 10_000;

/// `Notify`'s `expire_timeout` for a balloon: one with no timeout of its own (0) goes away after
/// [`BALLOON_TIMEOUT`], one that asked to stay (negative) gets 0, the server's "never expire".
pub fn expire_timeout(timeout: i32) -> i32 {
    match timeout {
        0 => BALLOON_TIMEOUT,
        n if n < 0 => 0,
        n => n,
    }
}

/// A balloon, on its way to the notification server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Balloon {
    pub title: String,
    pub text: String,
    pub timeout: i32,
}

/// The well-known name of a tray icon's item.
pub fn name(pid: u32, n: u32) -> String {
    format!("org.kde.StatusNotifierItem-{pid}-{n}")
}

/// What the desktop shows of an icon.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub title: String,
    pub id: String,
    pub icon: Vec<pixels::Pixmap>,
}

impl Look {
    pub fn new(title: String, class: Option<String>, image: Option<&Image>) -> Look {
        let id = class
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| "xwlbridge-tray".to_string());
        Look {
            title,
            id,
            icon: pixels::pixmaps(image, [0; 3]),
        }
    }
}

/// The item of one icon.
#[derive(Clone)]
pub struct Item {
    pub container: u32,
    state: Arc<Mutex<Look>>,
    tray: Tray,
}

/// The tray's side of the session bus, once connected.
#[derive(Clone)]
pub struct Bus {
    pub tray: Tray,
    /// Whether the missing watcher was reported already.
    pub reported: Arc<AtomicBool>,
}

impl Bus {
    /// Notes that no watcher is on the bus, once.
    pub fn no_watcher(&self) {
        if !self.reported.swap(true, Ordering::SeqCst) {
            crate::log!(
                "-showwindows: no StatusNotifierWatcher on the session bus: tray icons of X apps are \
                 docked but nothing shows them (GNOME needs the AppIndicator extension, \
                 gnome-shell-extension-appindicator, for X tray icons); they appear as soon as one does"
            );
        }
    }
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn context_menu(&self, _x: i32, _y: i32) {
        // GNOME's AppIndicator does not call this (its right click opens the menu): the menu's entries
        // are how the icon's own menu is reached.
    }

    fn activate(&self, _x: i32, _y: i32) {
        self.tray.click(self.container, 1, true);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        self.tray.click(self.container, 2, false);
    }

    fn scroll(&self, delta: i32, orientation: String) {
        self.tray
            .scroll(self.container, delta, orientation == "horizontal");
    }

    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> String {
        self.state.lock().map(|l| l.id.clone()).unwrap_or_default()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.state
            .lock()
            .map(|l| l.title.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.state
            .lock()
            .map(|l| l.icon.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    #[allow(clippy::type_complexity)]
    fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        let icon = self.icon_pixmap();
        (String::new(), icon, self.title(), String::new())
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
}

/// The menu of one item.
#[derive(Clone)]
pub struct Menu {
    container: u32,
    tray: Tray,
}

type Properties = HashMap<String, Value<'static>>;

impl Menu {
    fn properties(label: &str) -> Properties {
        let mut props = Properties::new();
        props.insert("label".into(), Value::from(label.to_string()));
        props.insert("enabled".into(), Value::from(true));
        props.insert("visible".into(), Value::from(true));
        props
    }

    fn activate(&self, id: i32) {
        if let Some((_, _, button, double)) = MENU.iter().find(|(i, ..)| *i == id) {
            self.tray.click(self.container, *button, *double);
        }
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[allow(clippy::type_complexity)]
    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, (i32, Properties, Vec<zvariant::OwnedValue>)) {
        let children = MENU
            .iter()
            .filter_map(|(id, label, ..)| {
                let props = Menu::properties(label);
                zvariant::OwnedValue::try_from(Value::from((*id, props, Vec::<Value>::new()))).ok()
            })
            .collect();
        (1, (0, Properties::new(), children))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, Properties)> {
        let wanted = |id: i32| ids.is_empty() || ids.contains(&id);
        let mut out: Vec<(i32, Properties)> = MENU
            .iter()
            .filter(|(id, ..)| wanted(*id))
            .map(|(id, label, ..)| (*id, Menu::properties(label)))
            .collect();
        if wanted(0) {
            let mut root = Properties::new();
            root.insert("children-display".into(), Value::from("submenu"));
            out.insert(0, (0, root));
        }
        out
    }

    fn get_property(&self, _id: i32, _name: String) -> Value<'static> {
        Value::from("")
    }

    fn event(&self, id: i32, event_id: String, _data: Value<'_>, _timestamp: u32) {
        if event_id == "clicked" || event_id == "activated" {
            self.activate(id);
        }
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The menu of the icon in `container` (for the tests, and the manager).
pub fn menu(container: u32, tray: Tray) -> Menu {
    Menu { container, tray }
}

/// The click of a menu entry (see [`MENU`]).
pub fn menu_click(id: i32) -> Option<(u8, bool)> {
    MENU.iter()
        .find(|(i, ..)| *i == id)
        .map(|(_, _, button, double)| (*button, *double))
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;

    #[test]
    fn a_balloon_keeps_its_timeout() {
        assert_eq!(expire_timeout(3000), 3000);
        assert_eq!(expire_timeout(0), BALLOON_TIMEOUT);
        assert_eq!(expire_timeout(-1), 0);
    }

    #[test]
    fn the_menu_entries_are_the_clicks() {
        assert_eq!(menu_click(1), Some((1, false)));
        assert_eq!(menu_click(2), Some((2, false)));
        assert_eq!(menu_click(3), Some((3, false)));
        assert_eq!(menu_click(9), None);
        // the menu's own order, which is what a user sees
        assert_eq!(
            MENU.iter().map(|(_, label, ..)| *label).collect::<Vec<_>>(),
            ["Open", "Menu", "Middle click"]
        );
    }

    #[test]
    fn a_name_is_per_process_and_icon() {
        assert_eq!(name(42, 1), "org.kde.StatusNotifierItem-42-1");
        assert_ne!(name(42, 1), name(42, 2));
    }

    #[test]
    fn the_look_of_an_icon() {
        let img = Image {
            width: 1,
            height: 1,
            depth: 24,
            data: vec![1, 2, 3, 0],
        };
        let look = Look::new("T".into(), Some("app".into()), Some(&img));
        assert_eq!(look.id, "app");
        assert_eq!(look.icon.len(), 1);
        assert_eq!(look.icon[0].0, 1);
        let look = Look::new("T".into(), None, None);
        assert!(!look.id.is_empty());
        assert!(look.icon.is_empty());
    }
}
