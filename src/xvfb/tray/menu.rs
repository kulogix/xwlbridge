//! `com.canonical.dbusmenu`: the small menu of one icon.
//!
//! GNOME's AppIndicator extension never calls `ContextMenu`, and its single left click only opens the item's
//! menu (and does nothing at all for an item without one). So every icon gets a menu with what a click on
//! an X tray icon can do, each entry a click on the icon itself ([`super::clicks`]):
//!
//! | entry | on the icon |
//! |---|---|
//! | **Open** | a left click (what a single click does on an X panel) |
//! | **Menu** | a right click: the app's own menu, where the app puts it |
//! | **Middle click** | a middle click (some icons only listen to that) |
//!
//! The entries of a menu carry no position: the tray clicks the icon where the session's pointer is, or the
//! top right of the primary monitor without one ([`super::clicks::default_point`]).

use std::collections::HashMap;
use std::sync::Arc;

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

use super::item::Shared;

/// The menu's entries: `(id, label, button)`.
pub const ENTRIES: &[(i32, &str, u8)] = &[(1, "Open", 1), (2, "Menu", 3), (3, "Middle click", 2)];

/// The icon's menu.
pub struct Menu {
    shared: Arc<Shared>,
}

type Properties = HashMap<String, OwnedValue>;
type Node = (i32, Properties, Vec<OwnedValue>);

fn properties(label: &str) -> Properties {
    let mut props = Properties::new();
    for (key, value) in [
        ("label", Value::from(label)),
        ("enabled", Value::from(true)),
        ("visible", Value::from(true)),
    ] {
        if let Ok(value) = OwnedValue::try_from(value) {
            props.insert(key.to_string(), value);
        }
    }
    props
}

impl Menu {
    pub fn new(shared: Arc<Shared>) -> Menu {
        Menu { shared }
    }

    fn activate(&self, id: i32) {
        if let Some((_, _, button)) = ENTRIES.iter().find(|(i, ..)| *i == id) {
            self.shared.click(*button, None, None);
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
    ) -> (u32, Node) {
        let children = ENTRIES
            .iter()
            .filter_map(|(id, label, _)| {
                let node: Value = Value::from((*id, properties(label), Vec::<Value>::new()));
                OwnedValue::try_from(node).ok()
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
        let mut out: Vec<(i32, Properties)> = ENTRIES
            .iter()
            .filter(|(id, ..)| wanted(*id))
            .map(|(id, label, _)| (*id, properties(label)))
            .collect();
        if wanted(0) {
            let mut root = Properties::new();
            if let Ok(v) = OwnedValue::try_from(Value::from("submenu")) {
                root.insert("children-display".into(), v);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xvfb::tray::Look;
    use crate::xvfb::tray::clicks::Command;
    use std::sync::mpsc;

    #[test]
    fn every_entry_clicks_the_icon() {
        let (tx, rx) = mpsc::channel();
        let menu = Menu::new(Shared::new(3, Look::default(), tx));
        for (id, label, button) in ENTRIES {
            menu.activate(*id);
            assert_eq!(
                rx.recv().unwrap(),
                Command::Click {
                    container: 3,
                    button: *button,
                    x: None,
                    y: None
                },
                "{label}"
            );
        }
        assert_eq!(
            ENTRIES.iter().map(|(_, l, _)| *l).collect::<Vec<_>>(),
            ["Open", "Menu", "Middle click"]
        );
    }

    #[test]
    fn an_entry_has_a_label() {
        assert_eq!(properties("Open").len(), 3);
    }
}
