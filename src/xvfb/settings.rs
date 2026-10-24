//! A minimal XSETTINGS manager and `Xft.dpi` for the engine display (design
//! section 4, phase 5 M3): apps learn S so their UI is physically the right
//! size on a HiDPI desktop.
//!
//! The bridge owns `_XSETTINGS_S0` only when no other manager owns it (on an
//! engine display there normally is none: gsd-xsettings runs on the desktop's
//! Xwayland); otherwise it leaves the existing manager alone and says so. It
//! publishes
//! - `Gdk/WindowScalingFactor` = S,
//! - `Gdk/UnscaledDPI` = 96 * 1024,
//! - `Xft/DPI` = 96 * S * 1024,
//!
//! and keeps `Xft.dpi` in the root's `RESOURCE_MANAGER` in step (Qt, Xft
//! toolkits and Java's `gnome.*` properties read it). Other resource lines
//! are preserved. This is the scale/DPI part of `tools/x11-a11y-proxy`'s
//! settings mirror, in Rust, owned by the bridge.
//!
//! XSETTINGS wire format: freedesktop.org "XSETTINGS" spec.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, CreateWindowAux, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::AnyError;

/// One XSETTINGS value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value<'a> {
    Int(i32),
    #[allow(dead_code)] // string settings (theme/font) are a later addition
    Str(&'a str),
}

/// The `_XSETTINGS_SETTINGS` property bytes (native little-endian, as the
/// spec's byte-order field says LSBFirst).
fn encode(settings: &[(&str, Value<'_>)], serial: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.push(0); // LSBFirst
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&(settings.len() as u32).to_le_bytes());
    let mut entries: Vec<&(&str, Value<'_>)> = settings.iter().collect();
    entries.sort_by_key(|(name, _)| *name);
    for (name, value) in entries {
        let nb = name.as_bytes();
        // XSETTINGS: 0 = Integer, 1 = String
        let kind = u8::from(matches!(value, Value::Str(_)));
        out.push(kind);
        out.push(0);
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(nb);
        out.resize(out.len() + (4 - nb.len() % 4) % 4, 0);
        out.extend_from_slice(&0u32.to_le_bytes()); // last-change serial
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

/// What the manager publishes at scale `s`.
fn values(s: u32) -> Vec<(&'static str, Value<'static>)> {
    let s = s.max(1) as i32;
    vec![
        ("Gdk/WindowScalingFactor", Value::Int(s)),
        ("Gdk/UnscaledDPI", Value::Int(96 * 1024)),
        ("Xft/DPI", Value::Int(96 * s * 1024)),
        ("Gtk/CursorThemeSize", Value::Int(24 * s)),
    ]
}

/// The manager on one X connection. Dropping it leaves the selection and the
/// resource property as they are (the bridge's connection normally outlives
/// the process).
pub struct Settings {
    pub owner: Window,
    prop: u32,
    resource_manager: u32,
    serial: u32,
    /// The scale currently published (0: not installed).
    pub scale: u32,
    active: bool,
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
            prop,
            resource_manager,
            serial: 0,
            scale: 0,
            active: false,
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
        me.publish(conn, 1)?;
        Ok(me)
    }

    /// Publishes S (the X scale): the XSETTINGS values and `Xft.dpi`.
    pub fn set_scale(&mut self, conn: &RustConnection, scale: u32) -> Result<(), AnyError> {
        if !self.active || self.scale == scale.max(1) {
            return Ok(());
        }
        self.publish(conn, scale)
    }

    fn publish(&mut self, conn: &RustConnection, scale: u32) -> Result<(), AnyError> {
        let scale = scale.max(1);
        self.serial = self.serial.wrapping_add(1);
        let data = encode(&values(scale), self.serial);
        conn.change_property8(PropMode::REPLACE, self.owner, self.prop, self.prop, &data)?;
        self.set_resource_manager(conn, scale)?;
        conn.sync()?;
        self.scale = scale;
        crate::log!(
            "settings: XSETTINGS Gdk/WindowScalingFactor={scale}, Xft/DPI={}, Xft.dpi={}",
            96 * scale * 1024,
            96 * scale
        );
        Ok(())
    }

    /// Replaces `Xft.dpi` in the root's RESOURCE_MANAGER, keeping the other
    /// resource lines.
    fn set_resource_manager(&self, conn: &RustConnection, scale: u32) -> Result<(), AnyError> {
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
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("Xft.dpi:"))
            .map(str::to_owned)
            .collect();
        lines.insert(0, format!("Xft.dpi:\t{}", 96 * scale));
        let mut out = lines.join("\n");
        out.push('\n');
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

    #[test]
    fn encodes_the_scale_settings() {
        let b = encode(&values(2), 7);
        assert_eq!(b[0], 0, "LSBFirst");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 7);
        assert_eq!(int_at(&b, "Gdk/WindowScalingFactor"), Some(2));
        assert_eq!(int_at(&b, "Xft/DPI"), Some(96 * 2 * 1024));
        assert_eq!(int_at(&b, "Gdk/UnscaledDPI"), Some(96 * 1024));
        // 1.25x runs at S = 2: the destination stays logical, DPI is 192
        let b = encode(&values(2), 8);
        assert_eq!(int_at(&b, "Xft/DPI"), Some(196_608));
    }
}
