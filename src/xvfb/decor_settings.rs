//! GNOME's live settings for `-showwindows` decorations (design section 10
//! decision 3): `org.freedesktop.portal.Settings` (`Read` + `SettingChanged`)
//! with dconf (`ca.desrt.dconf`) as the fallback and the schema defaults last.
//!
//! The portal is the only interface that sees a per-application sandbox's view
//! of the settings; dconf is what GSettings itself reads, so a desktop without
//! a settings portal (`gnome` build on a non-GNOME session) still follows the
//! user. Both are D-Bus, through the `zbus` the `gnome` build already uses; no
//! GIO, no new crates.
//!
//! The watcher runs on its own thread and sends a whole [`Settings`] snapshot
//! on every change. The reverse thread applies it at its next dispatch (the
//! header is repainted only then, never per client frame).

use std::sync::mpsc::Sender;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

use super::decor::TitlebarAction;

/// The settings the header uses, as GNOME's defaults say them until a value
/// arrives.
#[derive(Clone, Debug)]
pub struct Settings {
    pub button_layout: String,
    pub double_click: TitlebarAction,
    pub middle_click: TitlebarAction,
    pub right_click: TitlebarAction,
    pub titlebar_font: Option<String>,
    pub titlebar_uses_system_font: bool,
    /// `org.gnome.desktop.interface font-name` (the system font).
    pub font_name: String,
    pub dark: bool,
    pub accent: String,
    /// Where the values came from: "settings portal", "dconf" or "defaults".
    pub origin: &'static str,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            button_layout: "appmenu:minimize,maximize,close".into(),
            double_click: TitlebarAction::ToggleMaximize,
            middle_click: TitlebarAction::Lower,
            right_click: TitlebarAction::Menu,
            titlebar_font: None,
            titlebar_uses_system_font: true,
            font_name: "Cantarell 11".into(),
            dark: false,
            accent: "blue".into(),
            origin: "defaults",
        }
    }
}

impl Settings {
    /// The font description for the title (None: the system font at the
    /// header's size).
    pub fn title_font(&self) -> Option<String> {
        let font = if self.titlebar_uses_system_font {
            self.font_name.clone()
        } else {
            self.titlebar_font.clone()?
        };
        Some(bold(font))
    }
}

const WM: &str = "org.gnome.desktop.wm.preferences";
const IFACE: &str = "org.gnome.desktop.interface";

/// Starts the watcher thread; every snapshot (the first one included) arrives
/// on `tx`. Failure to reach the session bus sends the defaults.
pub fn watch(tx: Sender<Settings>) {
    let _ = std::thread::Builder::new()
        .name("decor-settings".into())
        .spawn(move || run(tx));
}

fn run(tx: Sender<Settings>) {
    let Ok(conn) = Connection::session() else {
        let _ = tx.send(Settings::default());
        return;
    };
    let mut s = read_all(&conn);
    let _ = tx.send(s.clone());
    let Ok(proxy) = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    ) else {
        return;
    };
    let Ok(signals) = proxy.receive_signal("SettingChanged") else {
        return;
    };
    for msg in signals {
        let Ok((ns, key, value)) = msg.body().deserialize::<(String, String, OwnedValue)>() else {
            continue;
        };
        if apply(&mut s, &ns, &key, &value) {
            let _ = tx.send(s.clone());
        }
    }
}

/// Reads every key: the settings portal first, dconf for the keys the portal
/// does not answer (an older portal, a key it does not relay), defaults last.
fn read_all(conn: &Connection) -> Settings {
    let mut s = Settings::default();
    let mut portal = 0;
    let mut dconf = 0;
    for (ns, key) in [
        (WM, "button-layout"),
        (WM, "action-double-click-titlebar"),
        (WM, "action-middle-click-titlebar"),
        (WM, "action-right-click-titlebar"),
        (WM, "titlebar-font"),
        (WM, "titlebar-uses-system-font"),
        (IFACE, "font-name"),
        (IFACE, "color-scheme"),
        (IFACE, "accent-color"),
    ] {
        if let Some(v) = portal_read(conn, ns, key) {
            portal += 1;
            set(&mut s, key, &v);
        } else if let Some(v) = dconf_read(conn, ns, key) {
            dconf += 1;
            set(&mut s, key, &v);
        }
    }
    s.origin = if portal == 9 {
        "settings portal"
    } else if portal > 0 {
        "settings portal + dconf"
    } else if dconf > 0 {
        "dconf"
    } else {
        "defaults"
    };
    s
}

fn apply(s: &mut Settings, ns: &str, key: &str, value: &Value) -> bool {
    let known = matches!(ns, WM | IFACE);
    if !known {
        return false;
    }
    set(s, key, value)
}

fn set(s: &mut Settings, key: &str, value: &Value) -> bool {
    match key {
        "button-layout" => {
            let Some(v) = as_string(value) else {
                return false;
            };
            if s.button_layout == v {
                return false;
            }
            s.button_layout = v;
        }
        "action-double-click-titlebar" => {
            let Some(v) = as_string(value).and_then(|v| TitlebarAction::parse(&v)) else {
                return false;
            };
            if s.double_click == v {
                return false;
            }
            s.double_click = v;
        }
        "action-middle-click-titlebar" => {
            let Some(v) = as_string(value).and_then(|v| TitlebarAction::parse(&v)) else {
                return false;
            };
            if s.middle_click == v {
                return false;
            }
            s.middle_click = v;
        }
        "action-right-click-titlebar" => {
            let Some(v) = as_string(value).and_then(|v| TitlebarAction::parse(&v)) else {
                return false;
            };
            if s.right_click == v {
                return false;
            }
            s.right_click = v;
        }
        "titlebar-font" => {
            let Some(v) = as_string(value) else {
                return false;
            };
            if s.titlebar_font.as_deref() == Some(v.as_str()) {
                return false;
            }
            s.titlebar_font = Some(v);
        }
        "titlebar-uses-system-font" => {
            let Some(v) = as_bool(value) else {
                return false;
            };
            if s.titlebar_uses_system_font == v {
                return false;
            }
            s.titlebar_uses_system_font = v;
        }
        "font-name" => {
            let Some(v) = as_string(value) else {
                return false;
            };
            if s.font_name == v {
                return false;
            }
            s.font_name = v;
        }
        "color-scheme" => {
            let Some(v) = as_string(value) else {
                return false;
            };
            let dark = v == "prefer-dark";
            if s.dark == dark {
                return false;
            }
            s.dark = dark;
        }
        "accent-color" => {
            let Some(v) = as_string(value) else {
                return false;
            };
            if s.accent == v {
                return false;
            }
            s.accent = v;
        }
        _ => return false,
    }
    true
}

fn as_string(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        Value::Value(inner) => as_string(inner),
        _ => None,
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Value(inner) => as_bool(inner),
        _ => None,
    }
}

/// `org.freedesktop.portal.Settings.Read(namespace, key) -> v`.
fn portal_read(conn: &Connection, ns: &str, key: &str) -> Option<OwnedValue> {
    let proxy = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .ok()?;
    let reply = proxy.call_method("Read", &(ns, key)).ok()?;
    let (v,): (OwnedValue,) = reply.body().deserialize().ok()?;
    Some(v)
}

/// dconf's `ca.desrt.dconf.Writer.Read(key) -> ay`, the value GSettings
/// stores for `ns.key` (the key path is the schema path).
fn dconf_read(conn: &Connection, ns: &str, key: &str) -> Option<OwnedValue> {
    let proxy = Proxy::new(
        conn,
        "ca.desrt.dconf",
        "/ca/desrt/dconf/Writer/user",
        "ca.desrt.dconf.Writer",
    )
    .ok()?;
    let path = format!("/{}/{}/", ns.replace('.', "/"), key);
    let reply = proxy.call_method("Read", &(path,)).ok()?;
    let (bytes,): (Vec<u8>,) = reply.body().deserialize().ok()?;
    // dconf returns the GVariant serialisation of the value: a string is
    // UTF-8 bytes without a length prefix, a boolean is one byte.
    match key {
        "titlebar-uses-system-font" => {
            OwnedValue::try_from(Value::Bool(bytes.first().copied()? != 0)).ok()
        }
        _ => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            OwnedValue::try_from(Value::Str(
                std::str::from_utf8(&bytes[..end]).ok()?.to_string().into(),
            ))
            .ok()
        }
    }
}

/// Makes a pango font description bold by putting `Bold` before its size
/// (`Ubuntu 11` -> `Ubuntu Bold 11`); an already bold description is kept.
fn bold(font: String) -> String {
    let mut parts: Vec<&str> = font.split_whitespace().collect();
    if parts.iter().any(|p| p.eq_ignore_ascii_case("bold")) {
        return font;
    }
    if let Some(last) = parts.last()
        && last.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        let size = parts.pop().unwrap_or_default();
        parts.push("Bold");
        parts.push(size);
        return parts.join(" ");
    }
    format!("{font} Bold")
}
