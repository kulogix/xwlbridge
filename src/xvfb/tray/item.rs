//! `org.kde.StatusNotifierItem`: what the desktop sees of one X tray icon.
//!
//! The item is what GNOME's AppIndicator extension (or any other StatusNotifier host) talks to. It holds
//! the icon's last [`Look`] and, for the rest, asks the tray thread ([`Shared`]): a click is an XTest click
//! on the icon where the user made it, a menu entry the same. See [`super::menu`] for why there is a menu
//! at all.

use std::sync::{Arc, Mutex, mpsc};

use zbus::interface;

use super::clicks::Command;
use crate::xvfb::tray::Look;

/// The item's `IconPixmap` and `ToolTip` icon type: `a(iiay)`.
pub type Pixmaps = Vec<(i32, i32, Vec<u8>)>;
/// The item's `ToolTip` and `ToolTip` property: `(name, icon, title, text)`.
pub type ToolTip = (String, Pixmaps, String, String);

/// What the item knows and does, shared with the tray thread.
pub struct Shared {
    pub container: u32,
    look: Mutex<Look>,
    commands: mpsc::Sender<Command>,
}

impl Shared {
    pub fn new(container: u32, look: Look, commands: mpsc::Sender<Command>) -> Arc<Shared> {
        Arc::new(Shared {
            container,
            look: Mutex::new(look),
            commands,
        })
    }

    pub fn look(&self) -> Look {
        self.look.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Takes a new look; `true` if anything the desktop shows changed.
    pub fn set_look(&self, look: Look) -> bool {
        match self.look.lock() {
            Ok(mut l) if *l != look => {
                *l = look;
                true
            }
            _ => false,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// A click at the desktop position `(x, y)` (see [`super::clicks`]).
    pub fn click(&self, button: u8, x: Option<i32>, y: Option<i32>) {
        self.send(Command::Click {
            container: self.container,
            button,
            x,
            y,
        });
    }
}

/// The item of one icon.
pub struct Item {
    pub shared: Arc<Shared>,
}

impl Item {
    pub fn new(shared: Arc<Shared>) -> Item {
        Item { shared }
    }
}

impl Item {
    pub fn look(&self) -> Look {
        self.shared.look()
    }

    pub fn pixmaps(&self) -> Pixmaps {
        self.look().pixmaps()
    }
}

/// The position a call carries: the desktop's, unless it has none.
pub fn position(x: i32, y: i32) -> Option<(i32, i32)> {
    (x >= 0 && y >= 0).then_some((x, y))
}

/// A call's position, as the tray takes it.
fn xy(x: i32, y: i32) -> (Option<i32>, Option<i32>) {
    match position(x, y) {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    }
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    /// A left double click: a double click on the icon.
    fn activate(&self, x: i32, y: i32) {
        let (x, y) = xy(x, y);
        self.shared.click(1, x, y);
    }

    /// A middle click.
    fn secondary_activate(&self, x: i32, y: i32) {
        let (x, y) = xy(x, y);
        self.shared.click(2, x, y);
    }

    /// A right click: the icon's own menu.
    fn context_menu(&self, x: i32, y: i32) {
        let (x, y) = xy(x, y);
        self.shared.click(3, x, y);
    }

    fn scroll(&self, delta: i32, orientation: String) {
        self.shared.send(Command::Scroll {
            container: self.shared.container,
            steps: delta,
            horizontal: orientation == "horizontal",
            x: None,
            y: None,
        });
    }

    #[zbus(property, name = "Category")]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property, name = "Id")]
    fn id(&self) -> String {
        self.look().id(self.shared.container)
    }

    #[zbus(property, name = "Title")]
    fn title(&self) -> String {
        self.look().title
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property, name = "WindowId")]
    fn window_id(&self) -> u32 {
        0
    }

    #[zbus(property, name = "IconPixmap")]
    fn icon_pixmap(&self) -> Pixmaps {
        self.pixmaps()
    }

    #[zbus(property, name = "ToolTip")]
    fn tool_tip(&self) -> ToolTip {
        (
            String::new(),
            self.pixmaps(),
            self.look().title,
            String::new(),
        )
    }

    #[zbus(property, name = "ItemIsMenu")]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property, name = "Menu")]
    fn menu(&self) -> String {
        self.menu_path()
    }

    #[zbus(property, name = "IconName")]
    fn icon_name(&self) -> &str {
        ""
    }

    #[zbus(property, name = "IconThemePath")]
    fn icon_theme_path(&self) -> String {
        // our icons come as pixels; a non-empty string, some hosts skip the pixmaps for
        String::new()
    }
}

impl Item {
    /// The object path of the icon's menu.
    pub fn menu_path(&self) -> String {
        format!("/StatusNotifierItem/{}/Menu", self.shared.container)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_position_is_the_desktops() {
        assert_eq!(position(10, 20), Some((10, 20)));
        assert_eq!(position(-1, 20), None);
        assert_eq!(position(10, -1), None);
    }

    #[test]
    fn a_click_reaches_the_tray() {
        let (tx, rx) = mpsc::channel();
        let shared = Shared::new(7, Look::default(), tx);
        let item = Item::new(shared);
        item.shared.click(1, Some(100), Some(6));
        assert_eq!(
            rx.recv().unwrap(),
            Command::Click {
                container: 7,
                button: 1,
                x: Some(100),
                y: Some(6)
            }
        );
    }

    #[test]
    fn a_new_look_is_news_once() {
        let (tx, _rx) = mpsc::channel();
        let shared = Shared::new(1, Look::default(), tx);
        let mut look = Look::default();
        assert!(!shared.set_look(look.clone()));
        look.title = "Icon".into();
        assert!(shared.set_look(look));
        assert_eq!(shared.look().title, "Icon");
    }
}
