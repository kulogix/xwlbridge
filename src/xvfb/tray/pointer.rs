//! Where the desktop's pointer is.
//!
//! A click from the icon's menu (`com.canonical.dbusmenu`) carries no position (the StatusNotifierItem
//! calls do): the session's pointer is where the user's click landed, under the top bar's icon. The
//! window extension ([`crate::xvfb::reverse`]'s counterpart, `contrib/gnome-shell`) answers
//! `GetPointer` for anyone on the session bus; an extension from before that call has none (`Features`
//! does not list `pointer`), and then the top right corner of the primary monitor is used, where the
//! status icons are.

use std::time::Duration;

use zbus::blocking::Proxy;

use super::clicks::default_point;

/// The extension's name, object and interface.
pub const BUS_NAME: &str = "org.xwlbridge.Windows";
pub const OBJECT_PATH: &str = "/org/xwlbridge/Windows";
pub const INTERFACE: &str = "org.xwlbridge.Windows1";

/// How long the session bus has to answer.
pub const TIMEOUT: Duration = Duration::from_millis(1000);

/// The desktop's pointer, from the extension.
pub fn current() -> Option<(i32, i32)> {
    query().ok().flatten()
}

fn query() -> Result<Option<(i32, i32)>, zbus::Error> {
    let conn = zbus::blocking::connection::Builder::session()?
        .method_timeout(TIMEOUT)
        .build()?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)?;
    let features: Vec<String> = proxy.call("Features", &())?;
    if !features.iter().any(|f| f == "pointer") {
        return Ok(None);
    }
    let (x, y, _pressed): (i32, i32, bool) = proxy.call("GetPointer", &())?;
    Ok(Some((x, y)))
}

/// The extension's monitors, as `(x, y, width, height)`: their geometry and work area, primary first.
pub fn monitors() -> Vec<(i32, i32, i32, i32)> {
    monitors_on().unwrap_or_default()
}

fn monitors_on() -> Option<Vec<(i32, i32, i32, i32)>> {
    let conn = zbus::blocking::connection::Builder::session()
        .ok()?
        .method_timeout(TIMEOUT)
        .build()
        .ok()?;
    let proxy = Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE).ok()?;
    type Monitor = (i32, i32, i32, i32);
    let monitors: Vec<(u32, Monitor, Monitor, f64, bool)> = proxy.call("GetMonitors", &()).ok()?;
    Some(monitors.into_iter().map(|m| m.1).collect())
}

/// Where a click without a position lands: the pointer, else the top right corner of the primary monitor.
pub fn point() -> (i32, i32) {
    match current() {
        Some(p) => p,
        None => default_point(&monitors(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_without_a_position_stays_on_the_screen() {
        // the pointer, when the extension answers; the primary monitor's corner (the default one without
        // monitors) otherwise
        let (x, y) = point();
        let (w, h) = monitors()
            .first()
            .map(|m| (m.2, m.3))
            .unwrap_or((1024, 768));
        assert!((0..w).contains(&x) && (0..h).contains(&y), "{x},{y}");
    }
}
