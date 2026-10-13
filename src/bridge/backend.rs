//! Picks the compositor-facing implementation: the Wayland-protocol client
//! ([`wayland`](crate::bridge::wayland), wlroots and friends) or, with the
//! `gnome` feature, mutter's D-Bus remote-desktop APIs
//! ([`gnome`](crate::bridge::gnome)).
//!
//! Both honour the same contract: block until the initial output geometry is
//! known (so the X server reports the real size before the wrapped binary
//! starts), then run on their own thread, writing into the shared [`Server`].

use std::sync::Arc;

use crate::bridge::Server;
use crate::config::BackendType;

pub fn spawn(server: Arc<Server>) {
    match server.config.backend {
        BackendType::Wlr => crate::bridge::wayland::spawn(server),
        BackendType::Gnome => spawn_gnome(server),
        BackendType::Auto => {
            if gnome_detected() {
                crate::log!("detected GNOME; using -backend gnome");
                spawn_gnome(server)
            } else {
                crate::bridge::wayland::spawn(server)
            }
        }
    }
}

/// What the Wayland compositor (`WAYLAND_DISPLAY`) offers: `Some(true)` if it
/// has a capture protocol the wayland backend uses (wlroots screencopy or
/// ext-image-copy-capture), `Some(false)` if it has none, `None` if there is
/// no compositor to ask (yet).
#[cfg_attr(not(feature = "gnome"), allow(dead_code))]
fn compositor_has_capture() -> Option<bool> {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, QueueHandle};
    struct Probe;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let conn = Connection::connect_to_env().ok()?;
    let (globals, _queue) = registry_queue_init::<Probe>(&conn).ok()?;
    Some(globals.contents().with_list(|list| {
        list.iter().any(|g| {
            g.interface == "zwlr_screencopy_manager_v1"
                || g.interface == "ext_image_copy_capture_manager_v1"
        })
    }))
}

#[cfg(feature = "gnome")]
const AUTO_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(feature = "gnome")]
const AUTO_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether `-backend auto` runs the gnome backend. mutter's remote-desktop
/// API on the session bus says GNOME. A desktop that only *names* GNOME
/// (`XDG_CURRENT_DESKTOP`) is GNOME too if its compositor has none of the
/// capture protocols the wayland backend needs (mutter has none), even before
/// gnome-shell is up (the gnome backend waits for it); but one whose
/// compositor has them is wlroots-based, whatever the desktop is called:
/// Phosh's phoc, in sessions that set `GNOME:Phosh`. With neither the name on
/// the bus nor a compositor to ask yet (a service racing the login), it looks
/// again until one shows up; after [`AUTO_WAIT`] it is taken for GNOME, whose
/// backend waits for gnome-shell as long again and then exits 69.
#[cfg(feature = "gnome")]
fn gnome_detected() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let names_gnome = desktop.split(':').any(|d| d.eq_ignore_ascii_case("GNOME"));
    let deadline = std::time::Instant::now() + AUTO_WAIT;
    let mut logged = false;
    loop {
        if crate::bridge::gnome::available() {
            return true;
        }
        if !names_gnome {
            return false;
        }
        match compositor_has_capture() {
            Some(true) => {
                crate::log!(
                    "XDG_CURRENT_DESKTOP is {desktop}, but the compositor has the wlroots \
                     capture protocols (not mutter); using -backend wlr"
                );
                return false;
            }
            Some(false) => return true,
            None if std::time::Instant::now() >= deadline => return true,
            None => {
                if !std::mem::replace(&mut logged, true) {
                    crate::log!(
                        "-backend auto: XDG_CURRENT_DESKTOP is {desktop}; waiting up to {}s for \
                         its compositor or gnome-shell",
                        AUTO_WAIT.as_secs()
                    );
                }
                std::thread::sleep(AUTO_POLL);
            }
        }
    }
}

#[cfg(feature = "gnome")]
fn spawn_gnome(server: Arc<Server>) {
    crate::bridge::gnome::spawn(server)
}

#[cfg(not(feature = "gnome"))]
fn spawn_gnome(server: Arc<Server>) {
    let _ = (server.config.reclaim, server.config.recordmonitor); // gnome-backend options
    crate::warning!(
        "-backend gnome needs xwlbridge built with the \"gnome\" feature \
         (cargo install xwlbridge --features gnome); falling back to the wayland backend"
    );
    crate::bridge::wayland::spawn(server)
}

#[cfg(not(feature = "gnome"))]
fn gnome_detected() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktop.split(':').any(|d| d.eq_ignore_ascii_case("GNOME")) {
        crate::warning!(
            "this looks like GNOME, whose compositor has none of the wayland capture/input \
             protocols; rebuild with --features gnome and use -backend gnome"
        );
    }
    false
}
