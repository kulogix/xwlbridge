use std::sync::{Arc, Mutex};

use crate::bridge::capture::Framebuffer;
use crate::bridge::clipboard::{Clipboard, Sel};
use crate::bridge::cursor::CursorState;
use crate::bridge::damage::DamageSink;
use crate::bridge::dynres::DynRes;
use crate::bridge::event::EventSink;
use crate::bridge::input::Input;
use crate::bridge::keymap::KeyTable;
use crate::bridge::x11::atom::{Atoms, XA_PRIMARY};
use crate::bridge::x11::randr::Screen;
use crate::config::Config;
use crate::util::Geometry;

/// Shared, connection-independent server state.
pub(crate) struct Server {
    pub config: Config,
    /// Interned atoms, shared by every connection as X shares them.
    pub atoms: Mutex<Atoms>,
    pub screen: Mutex<Screen>,
    pub input: Arc<Input>,
    pub events: EventSink,
    pub clipboard: Clipboard,
    pub framebuffer: Arc<Framebuffer>,
    pub damage: DamageSink,
    pub cursor: CursorState,
    /// Resolution changes an X client asked for, for the Wayland thread to carry
    /// to the compositor (`-dynres`).
    pub dynres: DynRes,
    /// X keysym and modifier tables built from the compositor keymap (`None`
    /// until received).
    pub keymap: Mutex<Option<KeyTable>>,
    /// The compositor keymap itself (xkb text), and a counter bumped on each
    /// change, for consumers that need the source (the Xvfb engine uploads it).
    pub keymap_text: Mutex<Option<Arc<str>>>,
    pub keymap_gen: std::sync::atomic::AtomicU64,
    /// Which of the keymap's layouts (xkb groups, from 0) the compositor has
    /// active; the X keyboard is that one alone. Changes bump `keymap_gen`.
    pub keymap_group: std::sync::atomic::AtomicU32,
    /// The desktop's Xwayland, when the stock engine's clipboard is relayed
    /// with it (`-clipboard xwayland`) instead of going through the backend's
    /// session clipboard. Decided before anything starts, so a session never
    /// enables the other.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend reads it
    pub xwayland_clip: Option<crate::uidisplay::UiDisplay>,
}

impl Server {
    /// Records a new compositor keymap text (backends call this alongside
    /// building [`keymap`](Self::keymap)).
    pub fn set_keymap_text(&self, text: &str) {
        *self.keymap_text.lock().unwrap() = Some(Arc::from(text));
        self.keymap_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Records which layout of the keymap is active (see `keymap_group`).
    #[allow(dead_code)] // only the gnome backend tells layouts apart
    pub fn set_keymap_group(&self, group: u32) {
        use std::sync::atomic::Ordering;
        if self.keymap_group.swap(group, Ordering::AcqRel) != group {
            self.keymap_gen.fetch_add(1, Ordering::Release);
        }
    }

    /// The atom naming a bridged selection. Every connection sees the same
    /// number for it, so it is also the one to put in an event.
    pub fn selection_atom(&self, sel: Sel) -> u32 {
        match sel {
            Sel::Primary => XA_PRIMARY,
            // interning is idempotent: whoever asks first fixes the number
            Sel::Clipboard => self.atoms.lock().unwrap().intern(b"CLIPBOARD", false),
        }
    }

    pub fn geometry(&self) -> Geometry {
        let s = self.screen.lock().unwrap();
        Geometry {
            width: s.width,
            height: s.height,
        }
    }

    /// Pushes the screen model out to everything derived from it (the
    /// framebuffer size, and the input geometry and output layout), and with
    /// `notify`, tells the X clients that asked. Call after any change to
    /// `screen`, whether the compositor's outputs moved or an X client
    /// reconfigured a CRTC. Returns the resulting screen size.
    pub fn sync_screen(&self, notify: bool) -> Geometry {
        let (geom, timestamp, config_timestamp, rects) = {
            let s = self.screen.lock().unwrap();
            (
                Geometry {
                    width: s.width,
                    height: s.height,
                },
                s.timestamp,
                s.config_timestamp,
                s.layout_rects(),
            )
        };
        self.framebuffer
            .ensure(u32::from(geom.width), u32::from(geom.height));
        self.input.set_layout(rects);
        self.input.set_geometry(geom.width, geom.height);
        if notify {
            self.events
                .screen_changed(geom.width, geom.height, timestamp, config_timestamp);
        }
        geom
    }
}
