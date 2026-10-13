//! The X bell, rung on the compositor's system bell.
//!
//! An X client's `Bell` (scrot rings one per screenshot, terminals on `\a`)
//! goes to `xdg_system_bell_v1` where the compositor has it (mutter 50 does,
//! and plays GNOME's alert sound or flashes as its settings say); on one
//! without it the bell does nothing, as on a server with no speaker. Either
//! way the request succeeds: before this it failed with BadImplementation,
//! which kills an Xlib client with the default error handler.
//!
//! Bells are rare, so the Wayland connection for them is made by the first
//! one, on a thread of its own (an X client's request never waits on the
//! compositor), and kept. A compositor without the global is remembered; a
//! connection that breaks is made again by the next bell, and one that can't
//! be made is tried again at most every [`RETRY`]. Bells closer together
//! than [`MIN_INTERVAL`] are rung once.

use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1::{self, XdgSystemBellV1};

/// Bells closer together than this ring once (a client ringing in a loop
/// must not flood the compositor).
const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// How long after a failed connection a bell tries again.
const RETRY: Duration = Duration::from_secs(30);

/// Rings the bell, without waiting for it. (The request's volume has no
/// counterpart in the compositor's bell.)
pub fn ring() {
    static RINGER: OnceLock<Option<SyncSender<()>>> = OnceLock::new();
    let Some(tx) = RINGER.get_or_init(spawn) else {
        return;
    };
    // one pending bell is enough: rings that arrive while one is pending
    // are the same bell, and a gone thread has said why
    match tx.try_send(()) {
        Ok(()) | Err(TrySendError::Full(()) | TrySendError::Disconnected(())) => {}
    }
}

fn spawn() -> Option<SyncSender<()>> {
    let (tx, rx) = sync_channel(1);
    match std::thread::Builder::new()
        .name("bell".into())
        .spawn(move || run(rx))
    {
        Ok(_) => Some(tx),
        Err(e) => {
            crate::warning!("bell: can't start its thread: {e}");
            None
        }
    }
}

enum State {
    /// Not connected; the next bell tries (once `retry_at` has passed).
    Disconnected { retry_at: Option<Instant> },
    Ready {
        conn: Connection,
        bell: XdgSystemBellV1,
        /// Never dispatched (the bell has no events), but kept: the bell
        /// and the registry belong to it.
        _queue: EventQueue<Globals>,
    },
    /// The compositor has no system bell: every bell does nothing.
    Unsupported,
}

fn run(rx: Receiver<()>) {
    let mut state = State::Disconnected { retry_at: None };
    let mut last: Option<Instant> = None;
    while rx.recv().is_ok() {
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < MIN_INTERVAL) {
            continue;
        }
        last = Some(now);
        // a connection that was lost is made again at once, for this bell
        for _ in 0..2 {
            if let State::Disconnected { retry_at } = &state {
                if retry_at.is_some_and(|t| now < t) {
                    break;
                }
                state = connect();
            }
            let State::Ready { conn, bell, .. } = &state else {
                break;
            };
            bell.ring(None);
            match conn.flush() {
                Ok(()) => {
                    crate::vlog!("bell: rang the compositor's system bell");
                    break;
                }
                Err(e) => {
                    crate::vlog!("bell: the compositor connection is gone ({e})");
                    state = State::Disconnected { retry_at: None };
                }
            }
        }
    }
}

fn connect() -> State {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            crate::vlog!("bell: no compositor connection for the bell ({e})");
            return State::Disconnected {
                retry_at: Some(Instant::now() + RETRY),
            };
        }
    };
    let (globals, queue) = match registry_queue_init::<Globals>(&conn) {
        Ok(g) => g,
        Err(e) => {
            crate::vlog!("bell: can't list the compositor's globals ({e})");
            return State::Disconnected {
                retry_at: Some(Instant::now() + RETRY),
            };
        }
    };
    match globals.bind::<XdgSystemBellV1, _, _>(&queue.handle(), 1..=1, ()) {
        Ok(bell) => {
            crate::vlog!("bell: X bells ring the compositor's system bell (xdg_system_bell_v1)");
            State::Ready {
                conn,
                bell,
                _queue: queue,
            }
        }
        Err(_) => {
            crate::log!(
                "bell: the compositor has no system bell (xdg_system_bell_v1); X bells do nothing"
            );
            State::Unsupported
        }
    }
}

struct Globals;

impl Dispatch<WlRegistry, GlobalListContents> for Globals {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgSystemBellV1, ()> for Globals {
    fn event(
        _: &mut Self,
        _: &XdgSystemBellV1,
        _: xdg_system_bell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
