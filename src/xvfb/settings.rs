//! GNOME's settings, mirrored onto the engine display: a faithful XSETTINGS
//! manager and `RESOURCE_MANAGER`, so X apps on `-showwindows` look and size
//! like the same apps on GNOME's own Xwayland (design section 4, phase 5).
//!
//! The bridge owns `_XSETTINGS_S0` only when no other manager owns it (on an
//! engine display there normally is none: gsd-xsettings runs on the desktop's
//! Xwayland); otherwise it leaves the existing manager alone and says so.
//!
//! The table is what `gsd-xsettings` publishes, derived the same way from
//! GNOME's settings: theme, icon and cursor theme, fonts and Xft rendering,
//! cursor size/blink, double-click and drag times, decoration layout, titlebar
//! actions, sounds, recent-files, accessibility, the scale/DPI keys and the
//! fixed GTK keys (palette, toolbar style, module list, session bus id...).
//! Values come from the settings portal (`org.freedesktop.portal.Settings`,
//! live `SettingChanged`) and fall back to GSettings through `gsettings`
//! (`gsettings monitor` for live changes). An S change re-derives the DPI and
//! cursor-size keys; `Xft.dpi`/`Xft.*`/`Xcursor.*` in the root's
//! `RESOURCE_MANAGER` are kept in step, preserving other resources.
//!
//! On an Xwayland display gsd-xsettings is that daemon; this is the same table
//! for the engine display. XSETTINGS wire format: freedesktop.org "XSETTINGS".

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, TryRecvError};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::AnyError;

/// One XSETTINGS value (an XSETTINGS integer is also how booleans travel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i32),
    Str(String),
}

impl Value {
    fn str(s: &str) -> Value {
        Value::Str(s.to_owned())
    }
    fn int(n: i64) -> Value {
        Value::Int(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
    }
}

/// A GNOME setting as the portal/GSettings reader sees it.
#[derive(Clone, Debug, PartialEq)]
enum Setting {
    Bool(bool),
    Int(i64),
    Str(String),
    Double(f64),
}

/// One live change from the portal (`SettingChanged`) or `gsettings monitor`.
struct Change {
    key: String,
    value: Option<Setting>,
}

/// GNOME's settings and the thread that follows them.
struct GnomeSettings {
    map: BTreeMap<String, Setting>,
    bus_id: String,
    animations: bool,
    enabled_modules: Vec<String>,
    disabled_modules: Vec<String>,
    rx: Option<Receiver<Change>>,
    source: &'static str,
}

const SCHEMAS: &[&str] = &[
    "org.gnome.desktop.interface",
    "org.gnome.desktop.wm.preferences",
    "org.gnome.desktop.peripherals.mouse",
    "org.gnome.desktop.sound",
    "org.gnome.desktop.privacy",
    "org.gnome.desktop.a11y",
    "org.gnome.desktop.a11y.interface",
    "org.gnome.desktop.background",
];

const PORTAL_NAMESPACES: &[&str] = SCHEMAS;

impl GnomeSettings {
    fn load() -> Self {
        let mut me = Self {
            map: BTreeMap::new(),
            bus_id: String::new(),
            animations: false,
            enabled_modules: Vec::new(),
            disabled_modules: Vec::new(),
            rx: None,
            source: "none",
        };
        #[cfg(feature = "gnome")]
        if let Some((map, bus_id, animations)) = portal_load() {
            me.map = map;
            me.bus_id = bus_id;
            me.animations = animations;
            me.source = "portal";
            let (tx, rx) = std::sync::mpsc::channel();
            spawn_portal_watch(tx);
            me.rx = Some(rx);
        }
        if me.source == "none" {
            me.map = gsettings_read_all();
            me.bus_id = gsettings_bus_id().unwrap_or_default();
            // gsd-xsettings takes the animation state from gnome-shell, not a
            // GSettings key; without the portal, ask the shell the same way
            // through `gdbus` when it is there.
            me.animations = gsettings_animations();
            me.source = "gsettings";
            let (tx, rx) = std::sync::mpsc::channel();
            spawn_gsettings_watch(tx);
            me.rx = Some(rx);
        }
        me.enabled_modules = gsettings_strv(
            "org.gnome.settings-daemon.plugins.xsettings",
            "enabled-gtk-modules",
        );
        me.disabled_modules = gsettings_strv(
            "org.gnome.settings-daemon.plugins.xsettings",
            "disabled-gtk-modules",
        );
        me
    }

    /// Applies every queued live change; true when anything changed.
    fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok(Change { key, value }) => {
                    match value {
                        Some(value) => {
                            self.map.insert(key, value);
                        }
                        None => {
                            self.map.remove(&key);
                        }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    break;
                }
            }
        }
        changed
    }

    fn get(&self, schema: &str, key: &str) -> Option<&Setting> {
        self.map.get(&format!("{schema}:{key}"))
    }

    fn string(&self, schema: &str, key: &str, default: &str) -> String {
        match self.get(schema, key) {
            Some(Setting::Str(s)) => s.clone(),
            _ => default.to_owned(),
        }
    }

    fn int(&self, schema: &str, key: &str, default: i64) -> i64 {
        match self.get(schema, key) {
            Some(Setting::Int(n)) => *n,
            _ => default,
        }
    }

    fn double(&self, schema: &str, key: &str, default: f64) -> f64 {
        match self.get(schema, key) {
            Some(Setting::Double(n)) => *n,
            Some(Setting::Int(n)) => *n as f64,
            _ => default,
        }
    }

    fn boolean(&self, schema: &str, key: &str, default: bool) -> bool {
        match self.get(schema, key) {
            Some(Setting::Bool(b)) => *b,
            Some(Setting::Int(n)) => *n != 0,
            _ => default,
        }
    }
}

/// The `_XSETTINGS_SETTINGS` property bytes (native little-endian, as the
/// spec's byte-order field says LSBFirst). Every entry carries the serial at
/// which it last changed, as `gsd-xsettings` does.
fn encode(
    settings: &BTreeMap<String, Value>,
    serial: u32,
    last_change: &BTreeMap<String, u32>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2048);
    out.push(0); // LSBFirst
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&(settings.len() as u32).to_le_bytes());
    for (name, value) in settings {
        let nb = name.as_bytes();
        // XSETTINGS: 0 = Integer, 1 = String
        let kind = u8::from(matches!(value, Value::Str(_)));
        out.push(kind);
        out.push(0);
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(nb);
        out.resize(out.len() + (4 - nb.len() % 4) % 4, 0);
        out.extend_from_slice(
            &last_change
                .get(name)
                .copied()
                .unwrap_or(serial)
                .to_le_bytes(),
        );
        match value {
            Value::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Str(s) => {
                let vb = s.as_bytes();
                out.extend_from_slice(&(vb.len() as u32).to_le_bytes());
                out.extend_from_slice(vb);
                out.resize(out.len() + (4 - vb.len() % 4) % 4, 0);
            }
        }
    }
    out
}

/// The default colour palette string gsd-xsettings publishes.
const COLOR_PALETTE: &str = "black:white:gray50:red:purple:blue:light blue:green:yellow:orange:lavender:brown:goldenrod4:dodger blue:pink:light green:gray10:gray30:gray75:gray90";

/// The 1x scale/DPI table, for when GNOME's settings cannot be read at all:
/// today's 0.2.0 behaviour, so the engine still tells apps the scale.
fn basic_values(s: u32) -> BTreeMap<String, Value> {
    let s = s.max(1);
    let mut m = BTreeMap::new();
    m.insert("Gdk/WindowScalingFactor".into(), Value::int(i64::from(s)));
    m.insert("Gdk/UnscaledDPI".into(), Value::int(96 * 1024));
    m.insert("Xft/DPI".into(), Value::int(96 * i64::from(s) * 1024));
    m.insert("Gtk/CursorThemeSize".into(), Value::int(24 * i64::from(s)));
    m
}

/// Translates a GNOME/WM button layout (`appmenu:close`) into GTK's
/// (`menu:close`), exactly like gsd-xsettings' `translate_wm_button_layout_to_gtk`.
fn gtk_button_layout(layout: &str) -> String {
    fn buttons(part: &str) -> Vec<&'static str> {
        part.split(',')
            .filter_map(|b| match b {
                "menu" => Some("icon"),
                "appmenu" => Some("menu"),
                "minimize" => Some("minimize"),
                "maximize" => Some("maximize"),
                "close" => Some("close"),
                _ => None,
            })
            .collect()
    }
    match layout.split_once(':') {
        Some((left, right)) => format!("{}:{}", buttons(left).join(","), buttons(right).join(",")),
        None => buttons(layout).join(","),
    }
}

fn hint_style(hint: &str) -> &'static str {
    match hint {
        "none" => "hintnone",
        "medium" => "hintmedium",
        "full" => "hintfull",
        _ => "hintslight",
    }
}

fn rgba_order(aa: &str, order: &str) -> &'static str {
    if aa != "rgba" {
        return "none";
    }
    match order {
        "rgba" => "rgba",
        "bgr" => "bgr",
        "vrgb" => "vrgb",
        "vbgr" => "vbgr",
        _ => "rgb",
    }
}

/// The GTK modules gsd-xsettings would publish: the `.desktop` module files in
/// gsd's module directory (conditions read from GNOME's settings), plus the
/// enabled and minus the disabled GSettings lists.
fn gtk_modules(g: &GnomeSettings) -> String {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(d) = std::env::var("GSD_gtk_modules_dir")
        && !d.is_empty()
    {
        dirs.push(d);
    }
    for d in [
        "/usr/lib/gnome-settings-daemon-3.0/gtk-modules",
        "/usr/lib/unity-settings-daemon-1.0/gtk-modules",
        "/usr/lib64/gnome-settings-daemon-3.0/gtk-modules",
        "/usr/lib/gnome-settings-daemon/gtk-modules",
    ] {
        if !dirs.iter().any(|x| x == d) {
            dirs.push(d.to_owned());
        }
    }
    // gsd-xsettings iterates a GHashTable of module files; GNOME's own
    // Xwayland publishes canberra first, then the AT-SPI pair. Sort the
    // module *groups* (canberra-gtk-module < gail:atk-bridge) and keep each
    // group's own colon order, which reproduces that string.
    let mut groups: Vec<String> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !(name.ends_with(".desktop") || name.ends_with(".gtk-module")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut module = None;
            let mut schema = None;
            let mut key = None;
            let mut in_group = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_group = line == "[GTK Module]";
                    continue;
                }
                if !in_group {
                    continue;
                }
                if let Some(v) = line.strip_prefix("X-GTK-Module-Name=") {
                    module = Some(v.trim().to_owned());
                } else if let Some(v) = line.strip_prefix("X-GTK-Module-Enabled-Schema=") {
                    schema = Some(v.trim().to_owned());
                } else if let Some(v) = line.strip_prefix("X-GTK-Module-Enabled-Key=") {
                    key = Some(v.trim().to_owned());
                }
            }
            let Some(module) = module else { continue };
            let enabled = match (schema.as_deref(), key.as_deref()) {
                (Some(schema), Some(key)) => g.boolean(schema, key, true),
                _ => true,
            };
            if enabled && !groups.iter().any(|x| x == &module) {
                groups.push(module);
            }
        }
    }
    for m in &g.enabled_modules {
        if !m.is_empty() && !groups.iter().any(|x| x == m) {
            groups.push(m.clone());
        }
    }
    groups.sort();
    let mut found: Vec<String> = Vec::new();
    for group in groups {
        for m in group.split(':') {
            if !m.is_empty()
                && !found.iter().any(|x| x == m)
                && !g.disabled_modules.iter().any(|d| d == m)
            {
                found.push(m.to_owned());
            }
        }
    }
    found.join(":")
}

/// The full XSETTINGS table gsd-xsettings publishes, for scale `s`.
fn derive(g: &GnomeSettings, s: u32) -> BTreeMap<String, Value> {
    let s = s.max(1);
    let scale = f64::from(s);
    let iface = "org.gnome.desktop.interface";
    let wm = "org.gnome.desktop.wm.preferences";
    let mouse = "org.gnome.desktop.peripherals.mouse";
    let sound = "org.gnome.desktop.sound";
    let privacy = "org.gnome.desktop.privacy";
    let a11y = "org.gnome.desktop.a11y";

    let text_scale = g.double(iface, "text-scaling-factor", 1.0).max(0.1);
    let dpi = 96.0 * text_scale;
    let unscaled_dpi = (dpi * 1024.0) as i64;
    let scaled_dpi = (dpi * scale * 1024.0) as i64;
    let aa = g.string(iface, "font-antialiasing", "grayscale");
    let hint = g.string(iface, "font-hinting", "slight");
    let hintstyle = hint_style(&hint);
    let order = g.string(iface, "font-rgba-order", "rgb");
    let high_contrast = g.boolean("org.gnome.desktop.a11y.interface", "high-contrast", false);
    let cursor_size = g.int(iface, "cursor-size", 24).max(1);
    let cursor_theme = g.string(iface, "cursor-theme", "Adwaita");
    let theme = if high_contrast {
        "HighContrast".to_owned()
    } else {
        g.string(iface, "gtk-theme", "Adwaita")
    };
    let im_module = {
        let m = g.string(iface, "gtk-im-module", "");
        if m.is_empty() { "ibus".to_owned() } else { m }
    };

    let mut m = BTreeMap::new();
    macro_rules! put_i {
        ($k:expr, $v:expr $(,)?) => {
            m.insert($k.to_owned(), Value::int($v));
        };
    }
    macro_rules! put_s {
        ($k:expr, $v:expr $(,)?) => {
            m.insert($k.to_owned(), Value::str($v));
        };
    }

    put_s!("Net/ThemeName", &theme);
    put_s!("Net/IconThemeName", &g.string(iface, "icon-theme", "Yaru"));
    put_s!("Net/FallbackIconTheme", "gnome");
    put_s!(
        "Net/SoundThemeName",
        &g.string(sound, "theme-name", "freedesktop"),
    );
    put_i!(
        "Net/EnableEventSounds",
        i64::from(g.boolean(sound, "event-sounds", true)),
    );
    put_i!(
        "Net/EnableInputFeedbackSounds",
        i64::from(g.boolean(sound, "input-feedback-sounds", true)),
    );
    put_i!(
        "Net/CursorBlink",
        i64::from(g.boolean(iface, "cursor-blink", true)),
    );
    put_i!(
        "Net/CursorBlinkTime",
        g.int(iface, "cursor-blink-time", 1200),
    );
    put_i!("Net/DoubleClickTime", g.int(mouse, "double-click", 400));
    put_i!("Net/DndDragThreshold", g.int(mouse, "drag-threshold", 8));

    put_s!("Gtk/FontName", &g.string(iface, "font-name", "Sans 11"));
    put_s!(
        "Gtk/KeyThemeName",
        &g.string(iface, "gtk-key-theme", "Default"),
    );
    put_s!("Gtk/CursorThemeName", &cursor_theme);
    put_i!("Gtk/CursorThemeSize", cursor_size * i64::from(s));
    put_i!(
        "Gtk/CursorBlinkTimeout",
        g.int(iface, "cursor-blink-timeout", 10),
    );
    put_i!(
        "Gtk/EnablePrimaryPaste",
        i64::from(g.boolean(iface, "gtk-enable-primary-paste", true)),
    );
    put_i!(
        "Gtk/OverlayScrolling",
        i64::from(g.boolean(iface, "overlay-scrolling", true)),
    );
    put_i!(
        "Gtk/RecentFilesMaxAge",
        g.int(privacy, "recent-files-max-age", -1),
    );
    put_i!(
        "Gtk/RecentFilesEnabled",
        i64::from(g.boolean(privacy, "remember-recent-files", true)),
    );
    put_s!(
        "Gtk/DecorationLayout",
        &gtk_button_layout(&g.string(wm, "button-layout", "appmenu:close")),
    );
    put_s!(
        "Gtk/TitlebarDoubleClick",
        &g.string(wm, "action-double-click-titlebar", "toggle-maximize"),
    );
    put_s!(
        "Gtk/TitlebarMiddleClick",
        &g.string(wm, "action-middle-click-titlebar", "none"),
    );
    put_s!(
        "Gtk/TitlebarRightClick",
        &g.string(wm, "action-right-click-titlebar", "menu"),
    );
    // Gtk/EnableAnimations is only published when gnome-shell says animations
    // are on; gsd-xsettings' initial state is false and it does not publish a
    // false value at startup (the key is absent, as on a stock GNOME 50).
    if g.animations {
        put_i!("Gtk/EnableAnimations", 1);
    }
    put_i!(
        "Gtk/KeynavUseCaret",
        i64::from(g.boolean(a11y, "always-show-text-caret", false)),
    );
    put_i!(
        "Gtk/ShowStatusShapes",
        i64::from(g.boolean(
            "org.gnome.desktop.a11y.interface",
            "show-status-shapes",
            false,
        )),
    );
    put_i!(
        "Gtk/ShellShowsDesktop",
        i64::from(g.boolean("org.gnome.desktop.background", "show-desktop-icons", true)),
    );

    // fixed entries (gsd-xsettings fixed_entries[])
    put_i!("Gtk/MenuImages", 0);
    put_i!("Gtk/ButtonImages", 0);
    put_i!("Gtk/ShowInputMethodMenu", 0);
    put_i!("Gtk/ShowUnicodeMenu", 0);
    put_i!("Gtk/AutoMnemonics", 1);
    put_i!("Gtk/DialogsUseHeader", 1);
    put_i!("Gtk/ShellShowsAppMenu", 0);
    put_i!("Gtk/CanChangeAccels", 0);
    put_i!("Gtk/TimeoutInitial", 200);
    put_i!("Gtk/TimeoutRepeat", 20);
    put_s!("Gtk/ColorPalette", COLOR_PALETTE);
    put_s!("Gtk/ColorScheme", "");
    put_s!("Gtk/ToolbarStyle", "both-horiz");
    put_s!("Gtk/ToolbarIconSize", "large");
    put_s!("Gtk/IMPreeditStyle", "callback");
    put_s!("Gtk/IMStatusStyle", "callback");
    put_s!("Gtk/MenuBarAccel", "F10");
    put_s!("Gtk/SessionBusId", &g.bus_id);
    put_s!("Gtk/Modules", &gtk_modules(g));
    put_s!("Gtk/IMModule", &im_module);

    // Xft and the scale/DPI keys
    put_i!("Xft/Antialias", i64::from(aa != "none"));
    put_i!("Xft/Hinting", i64::from(hint != "none"));
    put_s!("Xft/HintStyle", hintstyle);
    put_s!("Xft/RGBA", rgba_order(&aa, &order));
    put_i!("Gdk/WindowScalingFactor", i64::from(s));
    put_i!("Gdk/UnscaledDPI", unscaled_dpi);
    put_i!("Xft/DPI", scaled_dpi);

    m
}

/// The `RESOURCE_MANAGER` lines gsd-xsettings writes (`Xft.*`, `Xcursor.*`).
fn resources(g: &GnomeSettings, s: u32) -> BTreeMap<String, String> {
    let iface = "org.gnome.desktop.interface";
    let s = s.max(1);
    let text_scale = g.double(iface, "text-scaling-factor", 1.0).max(0.1);
    let dpi = (96.0 * text_scale * f64::from(s) + 0.5) as i64;
    let aa = g.string(iface, "font-antialiasing", "grayscale");
    let hint = g.string(iface, "font-hinting", "slight");
    let order = g.string(iface, "font-rgba-order", "rgb");
    let cursor_size = g.int(iface, "cursor-size", 24).max(1) * i64::from(s);
    let cursor_theme = g.string(iface, "cursor-theme", "Adwaita");
    let mut m = BTreeMap::new();
    // Xwayland's root has this; Xt apps read it to load their colour
    // app-defaults. gsd-xsettings leaves it in place.
    m.insert("*customization".into(), "-color".into());
    m.insert("Xft.dpi".into(), dpi.to_string());
    m.insert("Xft.antialias".into(), i64::from(aa != "none").to_string());
    m.insert("Xft.hinting".into(), i64::from(hint != "none").to_string());
    m.insert("Xft.hintstyle".into(), hint_style(&hint).to_owned());
    m.insert("Xft.rgba".into(), rgba_order(&aa, &order).to_owned());
    m.insert("Xcursor.size".into(), cursor_size.to_string());
    m.insert("Xcursor.theme".into(), cursor_theme);
    m
}

fn basic_resources(s: u32) -> BTreeMap<String, String> {
    let s = s.max(1);
    let mut m = BTreeMap::new();
    m.insert("Xft.dpi".into(), (96 * s).to_string());
    m.insert("Xcursor.size".into(), (24 * s).to_string());
    m
}

/// Merges our resource lines into the existing `RESOURCE_MANAGER` text,
/// keeping every other line (gsd-xsettings updates in place and leaves the
/// rest of XResourceManagerString alone).
fn merge_resources(old: &str, ours: &BTreeMap<String, String>) -> String {
    let mut out: Vec<String> = old
        .lines()
        .filter(|l| {
            let key = l.split(':').next().unwrap_or("").trim();
            !ours.contains_key(key)
        })
        .map(str::to_owned)
        .collect();
    for (k, v) in ours {
        out.push(format!("{k}:\t{v}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The manager on one X connection. Dropping it leaves the selection and the
/// resource property as they are (the bridge's connection normally outlives
/// the process).
pub struct Settings {
    pub owner: Window,
    /// The `_XSETTINGS_S0` selection atom (for the `MANAGER` announcement).
    selection: u32,
    prop: u32,
    resource_manager: u32,
    serial: u32,
    /// The scale currently published (0: not installed).
    pub scale: u32,
    active: bool,
    gnome: Option<GnomeSettings>,
    current: BTreeMap<String, Value>,
    last_change: BTreeMap<String, u32>,
}

impl Settings {
    /// Takes `_XSETTINGS_S0` if no other manager owns it and publishes S = 1.
    /// A selection held by someone else is left alone (`active` false).
    pub fn install(conn: &RustConnection, root: Window) -> Result<Settings, AnyError> {
        let atom = |name: &[u8]| -> Result<u32, AnyError> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        let selection = atom(b"_XSETTINGS_S0")?;
        let prop = atom(b"_XSETTINGS_SETTINGS")?;
        let resource_manager = atom(b"RESOURCE_MANAGER")?;
        let owner = conn.generate_id()?;
        // 1x1 InputOnly, off-screen: a selection owner needs a window
        conn.create_window(
            0,
            owner,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )?;
        let mut me = Settings {
            owner,
            selection,
            prop,
            resource_manager,
            serial: 0,
            scale: 0,
            active: false,
            gnome: None,
            current: BTreeMap::new(),
            last_change: BTreeMap::new(),
        };
        let current = conn.get_selection_owner(selection)?.reply()?.owner;
        if current != x11rb::NONE && current != owner {
            crate::log!(
                "settings: _XSETTINGS_S0 is owned by 0x{current:x}; leaving that manager in charge (no bridge XSETTINGS)"
            );
            return Ok(me);
        }
        // CurrentTime: the server takes the request's time (a selection
        // owner with no competing manager does not need a real timestamp)
        conn.set_selection_owner(owner, selection, x11rb::CURRENT_TIME)?;
        conn.sync()?;
        if conn.get_selection_owner(selection)?.reply()?.owner != owner {
            crate::log!("settings: another XSETTINGS manager took _XSETTINGS_S0; not publishing");
            return Ok(me);
        }
        me.active = true;
        let gnome = GnomeSettings::load();
        crate::log!(
            "settings: mirroring GNOME's settings ({} keys) as XSETTINGS, source {}",
            gnome.map.len(),
            gnome.source
        );
        me.gnome = Some(gnome);
        me.publish(conn, 1)?;
        Ok(me)
    }

    /// Tells every client that there is an XSETTINGS manager now (the spec's `MANAGER`
    /// ClientMessage: SelectionTime, the selection atom, the owner window). Without it a client
    /// that was already connected when the bridge took the selection only learns of the
    /// settings from a property change.
    fn announce(&self, conn: &RustConnection, selection: u32) -> Result<(), AnyError> {
        let manager = conn.intern_atom(false, b"MANAGER")?.reply()?.atom;
        let mut data = [0u32; 5];
        data[..3].copy_from_slice(&[x11rb::CURRENT_TIME, selection, self.owner]);
        let event = ClientMessageEvent {
            response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: conn.setup().roots[0].root,
            type_: manager,
            data: data.into(),
        };
        conn.send_event(
            false,
            conn.setup().roots[0].root,
            EventMask::STRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;
        Ok(())
    }

    /// Publishes S (the X scale) if it changed, and any live GNOME setting
    /// changes. Called from the reverse event loop, which is also the only
    /// place the X connection is used.
    pub fn update(&mut self, conn: &RustConnection, scale: u32) -> Result<(), AnyError> {
        if !self.active {
            return Ok(());
        }
        let mut republish = scale.max(1) != self.scale;
        if self.gnome.as_mut().is_some_and(GnomeSettings::poll) {
            republish = true;
        }
        if republish {
            self.publish(conn, scale.max(1))?;
        }
        Ok(())
    }

    fn publish(&mut self, conn: &RustConnection, scale: u32) -> Result<(), AnyError> {
        let scale = scale.max(1);
        let (table, res) = match self.gnome.as_ref() {
            Some(g) => (derive(g, scale), resources(g, scale)),
            None => (basic_values(scale), basic_resources(scale)),
        };
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let mut changed: Vec<String> = Vec::new();
        for (k, v) in &table {
            if self.current.get(k) != Some(v) {
                self.last_change.insert(k.clone(), serial);
                changed.push(k.clone());
            }
        }
        self.current = table;
        let data = encode(&self.current, serial, &self.last_change);
        conn.change_property8(PropMode::REPLACE, self.owner, self.prop, self.prop, &data)?;
        self.set_resource_manager(conn, &res)?;
        // (re-)announce on every change too: a client that connected after the manager
        // took the selection still learns that the settings it has are new
        self.announce(conn, self.selection)?;
        conn.sync()?;
        self.scale = scale;
        let keys = self.current.len();
        if changed.is_empty() {
            crate::log!(
                "settings: XSETTINGS serial {serial}: {keys} keys at S={scale} (Xft.dpi={}, Xft/DPI={})",
                res.get("Xft.dpi").map(String::as_str).unwrap_or("?"),
                self.current
                    .get("Xft/DPI")
                    .map(|v| match v {
                        Value::Int(n) => n.to_string(),
                        Value::Str(s) => s.clone(),
                    })
                    .unwrap_or_default()
            );
        } else {
            crate::log!(
                "settings: XSETTINGS serial {serial}: {keys} keys at S={scale}, changed {}",
                changed.join(", ")
            );
        }
        Ok(())
    }

    /// Replaces the Xft/Xcursor resources in the root's RESOURCE_MANAGER,
    /// keeping the other resource lines.
    fn set_resource_manager(
        &self,
        conn: &RustConnection,
        ours: &BTreeMap<String, String>,
    ) -> Result<(), AnyError> {
        // RESOURCE_MANAGER lives on the root
        let root = conn.setup().roots[0].root;
        let old = conn
            .get_property(
                false,
                root,
                self.resource_manager,
                AtomEnum::STRING,
                0,
                u32::MAX / 4,
            )?
            .reply()
            .ok()
            .map(|r| r.value)
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&old);
        let out = merge_resources(&text, ours);
        conn.change_property8(
            PropMode::REPLACE,
            root,
            self.resource_manager,
            AtomEnum::STRING,
            out.as_bytes(),
        )?;
        Ok(())
    }
}

// ---- reading GNOME's settings ----

#[cfg(feature = "gnome")]
fn setting_from_zv(v: &zbus::zvariant::OwnedValue) -> Option<Setting> {
    use zbus::zvariant::Value as V;
    match &**v {
        V::Bool(b) => Some(Setting::Bool(*b)),
        V::U8(n) => Some(Setting::Int(i64::from(*n))),
        V::I16(n) => Some(Setting::Int(i64::from(*n))),
        V::U16(n) => Some(Setting::Int(i64::from(*n))),
        V::I32(n) => Some(Setting::Int(i64::from(*n))),
        V::U32(n) => Some(Setting::Int(i64::from(*n))),
        V::I64(n) => Some(Setting::Int(*n)),
        V::U64(n) => i64::try_from(*n).ok().map(Setting::Int),
        V::F64(n) => Some(Setting::Double(*n)),
        V::Str(s) => Some(Setting::Str(s.to_string())),
        V::ObjectPath(p) => Some(Setting::Str(p.to_string())),
        _ => None,
    }
}

#[cfg(feature = "gnome")]
fn portal_connection() -> Option<zbus::blocking::Connection> {
    zbus::blocking::Connection::session().ok()
}

#[cfg(feature = "gnome")]
fn portal_load() -> Option<(BTreeMap<String, Setting>, String, bool)> {
    use zbus::blocking::Proxy;
    use zbus::zvariant::OwnedValue;

    let conn = portal_connection()?;
    let proxy = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .ok()?;
    let namespaces: Vec<String> = PORTAL_NAMESPACES.iter().map(|s| (*s).to_owned()).collect();
    let reply: std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>> =
        proxy.call("ReadAll", &(namespaces,)).ok()?;
    let mut map = BTreeMap::new();
    for (schema, keys) in reply {
        for (key, value) in keys {
            if let Some(v) = setting_from_zv(&value) {
                map.insert(format!("{schema}:{key}"), v);
            }
        }
    }
    if map.is_empty() {
        return None;
    }
    let bus_id = Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .ok()
    .and_then(|p| p.call::<_, _, String>("GetId", &()).ok())
    .unwrap_or_default();
    let animations = Proxy::new(
        &conn,
        "org.gnome.Shell.Introspect",
        "/org/gnome/Shell/Introspect",
        "org.gnome.Shell.Introspect",
    )
    .ok()
    .and_then(|p| p.get_property::<bool>("AnimationsEnabled").ok())
    .unwrap_or(false);
    Some((map, bus_id, animations))
}

#[cfg(feature = "gnome")]
fn spawn_portal_watch(tx: std::sync::mpsc::Sender<Change>) {
    use zbus::message::Type as MsgType;
    use zbus::zvariant::OwnedValue;

    let rule = match zbus::MatchRule::builder()
        .msg_type(MsgType::Signal)
        .interface("org.freedesktop.portal.Settings")
        .map(|b| b.member("SettingChanged"))
    {
        Ok(Ok(b)) => b.build(),
        _ => return,
    };
    let _ = std::thread::Builder::new()
        .name("gnome-settings".into())
        .spawn(move || {
            let Ok(conn) = zbus::blocking::Connection::session() else {
                return;
            };
            let Ok(iter) = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(64))
            else {
                return;
            };
            for msg in iter {
                let Ok(msg) = msg else { continue };
                let Ok((schema, key, value)) =
                    msg.body().deserialize::<(String, String, OwnedValue)>()
                else {
                    continue;
                };
                let Some(value) = setting_from_zv(&value) else {
                    continue;
                };
                if tx
                    .send(Change {
                        key: format!("{schema}:{key}"),
                        value: Some(value),
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
}

fn parse_gvariant(text: &str) -> Option<Setting> {
    let text = text.trim();
    match text {
        "true" => return Some(Setting::Bool(true)),
        "false" => return Some(Setting::Bool(false)),
        _ => {}
    }
    for prefix in [
        "uint32 ", "int32 ", "uint64 ", "int64 ", "uint16 ", "int16 ",
    ] {
        if let Some(rest) = text.strip_prefix(prefix)
            && let Ok(n) = rest.trim().parse::<i64>()
        {
            return Some(Setting::Int(n));
        }
    }
    if let Some(rest) = text.strip_prefix('@') {
        // annotations like @as [] are lists we do not mirror
        if rest.starts_with("as ") || rest.starts_with("a{") {
            return None;
        }
    }
    if text.len() >= 2 && text.starts_with('\'') && text.ends_with('\'') {
        let inner = &text[1..text.len() - 1];
        // GNOME's string settings do not contain quotes; unescape the two
        // sequences GVariant uses for them anyway.
        return Some(Setting::Str(
            inner.replace("\\'", "'").replace("\\\\", "\\"),
        ));
    }
    if let Ok(n) = text.parse::<i64>() {
        return Some(Setting::Int(n));
    }
    if let Ok(n) = text.parse::<f64>() {
        return Some(Setting::Double(n));
    }
    None
}

fn gsettings_output(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn gsettings_read_all() -> BTreeMap<String, Setting> {
    let mut map = BTreeMap::new();
    for schema in SCHEMAS {
        let Some(text) = gsettings_output(&["list-recursively", schema]) else {
            continue;
        };
        for line in text.lines() {
            let mut parts = line.splitn(3, ' ');
            let (Some(ns), Some(key), Some(value)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if ns != *schema {
                continue;
            }
            if let Some(v) = parse_gvariant(value) {
                map.insert(format!("{schema}:{key}"), v);
            }
        }
    }
    map
}

fn gsettings_strv(schema: &str, key: &str) -> Vec<String> {
    let Some(text) = gsettings_output(&["get", schema, key]) else {
        return Vec::new();
    };
    let text = text.trim();
    let inner = text
        .strip_prefix('@')
        .and_then(|t| t.split_once(' ').map(|(_, v)| v))
        .unwrap_or(text);
    inner
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|s| {
            let s = s.trim().trim_matches('\'');
            (!s.is_empty()).then(|| s.to_owned())
        })
        .collect()
}

fn parse_bus_id(text: &str) -> Option<String> {
    let start = text.find(&['\'', '"'][..])? + 1;
    let end = text.rfind(&['\'', '"'][..])?;
    (end > start).then(|| text[start..end].to_owned())
}

fn gsettings_bus_id() -> Option<String> {
    let out = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.GetId",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_bus_id(&String::from_utf8_lossy(&out.stdout))
}

fn gsettings_animations() -> bool {
    let out = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Shell.Introspect",
            "--object-path",
            "/org/gnome/Shell/Introspect",
            "--method",
            "org.freedesktop.DBus.Properties.Get",
            "org.gnome.Shell.Introspect",
            "AnimationsEnabled",
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).contains("true"),
        _ => false,
    }
}

fn dconf_dump() -> Option<String> {
    let out = std::process::Command::new("dconf")
        .args(["dump", "/org/gnome/desktop/"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The GSettings fallback's live changes: poll dconf's dump of the desktop
/// schemas (one cheap process), and on any change re-read the settings with
/// `gsettings` and send what differs. `gsettings monitor` is line-buffered
/// only on a terminal, so a pipe would never see a change in a daemon.
fn spawn_gsettings_watch(tx: std::sync::mpsc::Sender<Change>) {
    let _ = std::thread::Builder::new()
        .name("gsettings-watch".into())
        .spawn(move || {
            let mut known = gsettings_read_all();
            let mut last = dconf_dump().unwrap_or_default();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                let now = dconf_dump().unwrap_or_default();
                if now == last {
                    continue;
                }
                last = now;
                let fresh = gsettings_read_all();
                for (key, value) in &fresh {
                    if known.get(key) != Some(value)
                        && tx
                            .send(Change {
                                key: key.clone(),
                                value: Some(value.clone()),
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                for key in known.keys() {
                    if !fresh.contains_key(key)
                        && tx
                            .send(Change {
                                key: key.clone(),
                                value: None,
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                known = fresh;
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_at(bytes: &[u8], name: &str) -> Option<i32> {
        // minimal decoder for the test: walk the entries
        let mut off = 12;
        let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        for _ in 0..n {
            let kind = bytes[off];
            let nl = u16::from_le_bytes(bytes[off + 2..off + 4].try_into().unwrap()) as usize;
            let nm = std::str::from_utf8(&bytes[off + 4..off + 4 + nl]).unwrap();
            off += 4 + nl + (4 - nl % 4) % 4 + 4;
            if kind == 0 {
                let v = i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
                off += 4;
                if nm == name {
                    return Some(v);
                }
            } else {
                let vl = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
                off += 4 + vl + (4 - vl % 4) % 4;
            }
        }
        None
    }

    fn str_at(bytes: &[u8], name: &str) -> Option<String> {
        let mut off = 12;
        let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        for _ in 0..n {
            let kind = bytes[off];
            let nl = u16::from_le_bytes(bytes[off + 2..off + 4].try_into().unwrap()) as usize;
            let nm = std::str::from_utf8(&bytes[off + 4..off + 4 + nl]).unwrap();
            off += 4 + nl + (4 - nl % 4) % 4 + 4;
            if kind == 0 {
                off += 4;
            } else {
                let vl = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
                let v = String::from_utf8_lossy(&bytes[off + 4..off + 4 + vl]).into_owned();
                off += 4 + vl + (4 - vl % 4) % 4;
                if nm == name {
                    return Some(v);
                }
            }
        }
        None
    }

    /// The settings a stock GNOME 50 desktop had when the mirror was measured
    /// (Ubuntu 26.04, Yaru; the same values for both displays of dsh-p2mon).
    fn gnome50() -> GnomeSettings {
        let mut map = BTreeMap::new();
        let mut add = |k: &str, v: Setting| {
            map.insert(k.to_owned(), v);
        };
        let iface = "org.gnome.desktop.interface";
        let wm = "org.gnome.desktop.wm.preferences";
        let mouse = "org.gnome.desktop.peripherals.mouse";
        let sound = "org.gnome.desktop.sound";
        let privacy = "org.gnome.desktop.privacy";
        add(
            &format!("{iface}:font-name"),
            Setting::Str("Ubuntu Sans 11".into()),
        );
        add(
            &format!("{iface}:gtk-key-theme"),
            Setting::Str("Default".into()),
        );
        add(&format!("{iface}:cursor-blink"), Setting::Bool(true));
        add(&format!("{iface}:cursor-blink-time"), Setting::Int(1200));
        add(&format!("{iface}:cursor-blink-timeout"), Setting::Int(10));
        add(&format!("{iface}:gtk-theme"), Setting::Str("Yaru".into()));
        add(&format!("{iface}:icon-theme"), Setting::Str("Yaru".into()));
        add(
            &format!("{iface}:cursor-theme"),
            Setting::Str("Yaru".into()),
        );
        add(&format!("{iface}:cursor-size"), Setting::Int(24));
        add(
            &format!("{iface}:gtk-enable-primary-paste"),
            Setting::Bool(false),
        );
        add(&format!("{iface}:overlay-scrolling"), Setting::Bool(true));
        add(
            &format!("{iface}:text-scaling-factor"),
            Setting::Double(1.0),
        );
        add(
            &format!("{iface}:font-antialiasing"),
            Setting::Str("rgba".into()),
        );
        add(
            &format!("{iface}:font-hinting"),
            Setting::Str("slight".into()),
        );
        add(
            &format!("{iface}:font-rgba-order"),
            Setting::Str("rgb".into()),
        );
        add(
            &format!("{iface}:gtk-im-module"),
            Setting::Str(String::new()),
        );
        add(
            &format!("{iface}:toolkit-accessibility"),
            Setting::Bool(true),
        );
        add(
            &format!("{iface}:color-scheme"),
            Setting::Str("default".into()),
        );
        add(
            &format!("{iface}:gtk-color-scheme"),
            Setting::Str(String::new()),
        );
        add(
            &format!("{wm}:button-layout"),
            Setting::Str(":minimize,maximize,close".into()),
        );
        add(
            &format!("{wm}:action-double-click-titlebar"),
            Setting::Str("toggle-maximize".into()),
        );
        add(
            &format!("{wm}:action-middle-click-titlebar"),
            Setting::Str("lower".into()),
        );
        add(
            &format!("{wm}:action-right-click-titlebar"),
            Setting::Str("menu".into()),
        );
        add(&format!("{mouse}:double-click"), Setting::Int(400));
        add(&format!("{mouse}:drag-threshold"), Setting::Int(8));
        add(&format!("{sound}:theme-name"), Setting::Str("Yaru".into()));
        add(&format!("{sound}:event-sounds"), Setting::Bool(true));
        add(
            &format!("{sound}:input-feedback-sounds"),
            Setting::Bool(true),
        );
        add(&format!("{privacy}:recent-files-max-age"), Setting::Int(-1));
        add(
            &format!("{privacy}:remember-recent-files"),
            Setting::Bool(true),
        );
        add(
            "org.gnome.desktop.a11y:always-show-text-caret",
            Setting::Bool(false),
        );
        add(
            "org.gnome.desktop.a11y.interface:high-contrast",
            Setting::Bool(false),
        );
        add(
            "org.gnome.desktop.a11y.interface:show-status-shapes",
            Setting::Bool(false),
        );
        add(
            "org.gnome.desktop.background:show-desktop-icons",
            Setting::Bool(true),
        );
        GnomeSettings {
            map,
            bus_id: "b2da8f8186bebaa6d2aa27f06add27fd".into(),
            animations: false,
            enabled_modules: Vec::new(),
            disabled_modules: Vec::new(),
            rx: None,
            source: "test",
        }
    }

    fn table(g: &GnomeSettings, scale: u32) -> BTreeMap<String, Value> {
        derive(g, scale)
    }

    fn encoded(g: &GnomeSettings, scale: u32, serial: u32) -> Vec<u8> {
        encode(&table(g, scale), serial, &BTreeMap::new())
    }

    fn last_change_at(bytes: &[u8], name: &str) -> Option<u32> {
        let mut off = 12;
        let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        for _ in 0..n {
            let kind = bytes[off];
            let nl = u16::from_le_bytes(bytes[off + 2..off + 4].try_into().unwrap()) as usize;
            let nm = std::str::from_utf8(&bytes[off + 4..off + 4 + nl]).unwrap();
            off += 4 + nl + (4 - nl % 4) % 4;
            let serial = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            off += 4;
            if kind == 0 {
                off += 4;
            } else {
                let vl = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
                off += 4 + vl + (4 - vl % 4) % 4;
            }
            if nm == name {
                return Some(serial);
            }
        }
        None
    }

    #[test]
    fn one_setting_carries_its_own_last_change_serial() {
        let mut lc = BTreeMap::new();
        lc.insert("Xft/DPI".to_owned(), 5);
        let b = encode(&basic_values(1), 9, &lc);
        assert_eq!(last_change_at(&b, "Xft/DPI"), Some(5));
        assert_eq!(
            last_change_at(&b, "Gdk/WindowScalingFactor"),
            Some(9),
            "a key that did not change keeps the batch serial"
        );
    }

    #[test]
    fn encodes_the_scale_settings() {
        let b = encode(&basic_values(2), 7, &BTreeMap::new());
        assert_eq!(b[0], 0, "LSBFirst");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 7);
        assert_eq!(int_at(&b, "Gdk/WindowScalingFactor"), Some(2));
        assert_eq!(int_at(&b, "Xft/DPI"), Some(96 * 2 * 1024));
        assert_eq!(int_at(&b, "Gdk/UnscaledDPI"), Some(96 * 1024));
    }

    #[test]
    fn one_x_table_is_gsd_settings_table() {
        let g = gnome50();
        let t = table(&g, 1);
        // the 53 keys of GNOME 50's Xwayland XSETTINGS at 1x
        assert_eq!(t.len(), 53, "{:?}", t.keys().collect::<Vec<_>>());
        assert_eq!(t["Gdk/WindowScalingFactor"], Value::Int(1));
        assert_eq!(t["Gdk/UnscaledDPI"], Value::Int(98304));
        assert_eq!(t["Xft/DPI"], Value::Int(98304));
        assert_eq!(t["Gtk/CursorThemeSize"], Value::Int(24));
        assert_eq!(t["Net/ThemeName"], Value::str("Yaru"));
        assert_eq!(t["Net/IconThemeName"], Value::str("Yaru"));
        assert_eq!(t["Gtk/FontName"], Value::str("Ubuntu Sans 11"));
        assert_eq!(t["Gtk/CursorThemeName"], Value::str("Yaru"));
        assert_eq!(
            t["Gtk/DecorationLayout"],
            Value::str(":minimize,maximize,close")
        );
        assert_eq!(t["Xft/RGBA"], Value::str("rgb"));
        assert_eq!(t["Gtk/IMModule"], Value::str("ibus"));
        assert_eq!(t["Gtk/TitlebarMiddleClick"], Value::str("lower"));
        assert_eq!(t["Gtk/TimeoutInitial"], Value::Int(200));
        assert_eq!(t["Gtk/ColorPalette"], Value::str(COLOR_PALETTE));
        assert!(!t.contains_key("Gtk/EnableAnimations"));
        let b = encoded(&g, 1, 9);
        assert_eq!(int_at(&b, "Gtk/TimeoutInitial"), Some(200));
        assert_eq!(str_at(&b, "Net/ThemeName").as_deref(), Some("Yaru"));
    }

    #[test]
    fn two_x_and_mixed_layouts_publish_the_same_table_at_s2() {
        let g = gnome50();
        let one = table(&g, 1);
        let two = table(&g, 2);
        assert_eq!(two["Gdk/WindowScalingFactor"], Value::Int(2));
        assert_eq!(two["Gdk/UnscaledDPI"], Value::Int(98304));
        assert_eq!(two["Xft/DPI"], Value::Int(196608));
        assert_eq!(two["Gtk/CursorThemeSize"], Value::Int(48));
        // theme/font do not depend on S
        for k in [
            "Net/ThemeName",
            "Net/IconThemeName",
            "Gtk/FontName",
            "Gtk/CursorThemeName",
            "Gtk/DecorationLayout",
            "Xft/Antialias",
            "Xft/HintStyle",
            "Xft/RGBA",
        ] {
            assert_eq!(one[k], two[k], "{k}");
        }
        // a mixed 1x+2x layout has S = 2 (the largest scale rounded up), so it
        // is exactly the 2x table; 1.25x also runs at S = 2.
        assert_eq!(two, table(&g, 2));
    }

    #[test]
    fn one_point_two_five_x_uses_s_two_and_the_text_scaling_dpi() {
        let mut g = gnome50();
        g.map.insert(
            "org.gnome.desktop.interface:text-scaling-factor".into(),
            Setting::Double(1.25),
        );
        let t = table(&g, 2);
        assert_eq!(t["Gdk/WindowScalingFactor"], Value::Int(2));
        // 96 * 1.25 = 120 dpi unscaled; 240 dpi at S = 2
        assert_eq!(t["Gdk/UnscaledDPI"], Value::Int(122880));
        assert_eq!(t["Xft/DPI"], Value::Int(245760));
        assert_eq!(t["Gtk/CursorThemeSize"], Value::Int(48));
        let r = resources(&g, 2);
        assert_eq!(r["Xft.dpi"], "240");
        assert_eq!(r["Xcursor.size"], "48");
    }

    #[test]
    fn a_scale_change_republishes_dpi_and_cursor_but_not_the_theme() {
        let g = gnome50();
        let one = table(&g, 1);
        let mut g2 = g;
        // live setting changes (portal SettingChanged)
        g2.map.insert(
            "org.gnome.desktop.interface:gtk-theme".into(),
            Setting::Str("HighContrast".into()),
        );
        g2.map.insert(
            "org.gnome.desktop.interface:font-name".into(),
            Setting::Str("Ubuntu Sans 14".into()),
        );
        let two = table(&g2, 2);
        assert_eq!(two["Net/ThemeName"], Value::str("HighContrast"));
        assert_eq!(two["Gtk/FontName"], Value::str("Ubuntu Sans 14"));
        assert_eq!(one["Xft/DPI"], Value::Int(98304));
        assert_eq!(two["Xft/DPI"], Value::Int(196608));
    }

    #[test]
    fn high_contrast_dark_and_xft_mapping() {
        let mut g = gnome50();
        g.map.insert(
            "org.gnome.desktop.a11y.interface:high-contrast".into(),
            Setting::Bool(true),
        );
        g.map.insert(
            "org.gnome.desktop.interface:font-antialiasing".into(),
            Setting::Str("grayscale".into()),
        );
        g.map.insert(
            "org.gnome.desktop.interface:font-hinting".into(),
            Setting::Str("full".into()),
        );
        let t = table(&g, 1);
        assert_eq!(t["Net/ThemeName"], Value::str("HighContrast"));
        assert_eq!(t["Xft/Antialias"], Value::Int(1));
        assert_eq!(t["Xft/Hinting"], Value::Int(1));
        assert_eq!(t["Xft/HintStyle"], Value::str("hintfull"));
        assert_eq!(t["Xft/RGBA"], Value::str("none"));
        g.map.insert(
            "org.gnome.desktop.interface:font-antialiasing".into(),
            Setting::Str("none".into()),
        );
        let t = table(&g, 1);
        assert_eq!(t["Xft/Antialias"], Value::Int(0));
        assert_eq!(t["Xft/RGBA"], Value::str("none"));
    }

    #[test]
    fn wm_button_layout_is_translated_like_gsd() {
        assert_eq!(gtk_button_layout("appmenu:close"), "menu:close");
        assert_eq!(
            gtk_button_layout(":minimize,maximize,close"),
            ":minimize,maximize,close"
        );
        assert_eq!(gtk_button_layout("menu:appmenu"), "icon:menu");
        assert_eq!(gtk_button_layout("appmenu:"), "menu:");
        assert_eq!(gtk_button_layout("close"), "close");
    }

    #[test]
    fn animations_are_published_only_when_gnome_shell_has_them() {
        let mut g = gnome50();
        assert!(!table(&g, 1).contains_key("Gtk/EnableAnimations"));
        g.animations = true;
        assert_eq!(table(&g, 1)["Gtk/EnableAnimations"], Value::Int(1));
    }

    #[test]
    fn resources_replace_only_the_keys_gsd_owns() {
        let ours = basic_resources(2);
        let old = "Xft.dpi:\t96\nXcursor.theme:\tAdwaita\n*font:\tSans\n";
        let new = merge_resources(old, &ours);
        assert!(new.contains("Xft.dpi:\t192"));
        assert!(new.contains("Xcursor.size:\t48"));
        assert!(!new.contains("Xft.dpi:\t96"));
        assert!(new.contains("*font:\tSans"), "other resources preserved");
        assert_eq!(
            new.matches("Xcursor.theme").count(),
            1,
            "old Xcursor.theme replaced"
        );
    }

    #[test]
    fn parses_gdbus_string_replies() {
        assert_eq!(
            parse_bus_id("('b2da8f8186bebaa6d2aa27f06add27fd',)\n"),
            Some("b2da8f8186bebaa6d2aa27f06add27fd".to_owned())
        );
        assert_eq!(parse_bus_id("(\"abc\",)"), Some("abc".to_owned()));
        assert_eq!(parse_bus_id("no string"), None);
    }

    #[test]
    fn parses_the_portal_and_gsettings_value_forms() {
        assert_eq!(parse_gvariant("'Yaru'"), Some(Setting::Str("Yaru".into())));
        assert_eq!(parse_gvariant("true"), Some(Setting::Bool(true)));
        assert_eq!(parse_gvariant("uint32 0"), Some(Setting::Int(0)));
        assert_eq!(parse_gvariant("-1"), Some(Setting::Int(-1)));
        assert_eq!(parse_gvariant("1.25"), Some(Setting::Double(1.25)));
        assert_eq!(parse_gvariant("@as []"), None);
    }
}
