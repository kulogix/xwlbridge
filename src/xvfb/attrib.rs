//! Whose XTest is it, and is it meant for the desktop?
//!
//! The agent forwards the input its clients inject — seen as XInput2 raw
//! events, which don't say who sent them — to the desktop. Not all of it is
//! meant for the desktop: a helper that drives windows living on this display
//! injects XTest too (KDE's xembedsniproxy clicking a tray icon window, Xpra's
//! server replaying its client's input, xdotool typing into a window it focused
//! here), and forwarding that lands clicks and keys on whatever is under the
//! person's pointer on the real desktop.
//!
//! RECORD reports every XTest FakeInput request with the client that made it,
//! before the X server turns it into input. The agent pairs each raw event with
//! its request (same type and key or button; same coordinates for motion), so
//! it knows the client. Each client is classified once, by its program name
//! (X-Resource gives a local client's pid):
//!
//! - **engine helpers** (`-xtesthelpers`): never forwarded;
//! - **desktop drivers** (`-xtestdrivers`, the VNC servers and KVM clients):
//!   always forwarded, wherever the X server sends it: a remote user sees and
//!   drives the desktop, not this display's offscreen windows, even when one
//!   of them took the X focus;
//! - **anyone else** (xdotool, PyAutoGUI, Java's Robot, scripts): forwarded
//!   unless the X server sends it to another client's window here — a key
//!   while such a window has the X focus or the keyboard is grabbed, a button
//!   or motion while the pointer is grabbed, a button over such a window.
//!
//! Releases always go through: the input layer drops a release of anything the
//! desktop never saw pressed, so a dropped press can't leave a key stuck and a
//! forwarded one is always released. Input that can't be paired with a
//! request (no RECORD, a source that isn't an XTest device) is forwarded, as
//! it always was.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use x11rb::protocol::res;
use x11rb::rust_connection::RustConnection;

pub use crate::bridge::injectors::{Class, Rules};
use crate::bridge::input::MOTION_NOTIFY;

/// Requests older than this (server time) can't be the source of an event
/// being handled now: the X server turns a FakeInput into input at once.
const STALE_MS: u32 = 5_000;

/// The pid of a local X client, from X-Resource (1.2).
pub fn client_pid(conn: &RustConnection, base: u32) -> Option<u32> {
    let spec = res::ClientIdSpec {
        client: base,
        mask: res::ClientIdMask::LOCAL_CLIENT_PID,
    };
    let reply = res::query_client_ids(conn, &[spec]).ok()?.reply().ok()?;
    reply
        .ids
        .iter()
        .find(|v| v.spec.mask.contains(res::ClientIdMask::LOCAL_CLIENT_PID))
        .and_then(|v| v.value.first().copied())
}

/// Who an injecting client is, as far as forwarding goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Who {
    pub base: u32,
    pub class: Class,
    /// The program, for the log ("?" if its pid couldn't be found).
    pub name: String,
    /// Its program's names (command, executable, argv[0], an interpreter's
    /// script), "/"-joined; "" if its pid couldn't be found.
    pub program: String,
    /// Its process, if it could be found.
    pub pid: Option<u32>,
}

/// One XTest FakeInput request, as RECORD reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fake {
    pub who: std::sync::Arc<Who>,
    pub type_: u8,
    pub detail: u8,
    pub x: i16,
    pub y: i16,
    /// Server time (ms) the request was recorded at.
    pub time: u32,
}

/// The parts of a raw event a request is matched on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sig {
    /// The core event type: `KEY_PRESS` .. `MOTION_NOTIFY`.
    pub type_: u8,
    /// Key or button (0 for motion).
    pub detail: u8,
    /// Server time of the event.
    pub time: u32,
    /// Motion: the first two valuators, processed and as given.
    pub xy: Option<(i32, i32)>,
    pub xy_raw: Option<(i32, i32)>,
}

impl Fake {
    /// Whether this request must never reach the desktop, whoever else
    /// `Rules::classify` would take it for: the agent's own input, which only
    /// ever drives windows on this display (its window management, its focus
    /// helper, and the tray's clicks on a status icon: see `xvfb::tray`).
    /// The forwarder asks this before anything else, so that no rule and no
    /// binding of the event can make it reach the desktop.
    pub fn stays_here(&self) -> bool {
        self.who.class == Class::Agent
    }

    fn matches(&self, s: &Sig) -> bool {
        if self.type_ != s.type_ {
            return false;
        }
        if s.type_ != MOTION_NOTIFY {
            return self.detail == s.detail;
        }
        // an absolute move carries the position, a relative one the delta;
        // a move to where the pointer already is made no event at all
        let want = (i32::from(self.x), i32::from(self.y));
        s.xy == Some(want) || s.xy_raw == Some(want)
    }

    /// Whether this request was made more than STALE_MS before `time`.
    fn stale_at(&self, time: u32) -> bool {
        let age = time.wrapping_sub(self.time);
        age < u32::MAX / 2 && age > STALE_MS
    }
}

#[derive(Default)]
struct LedgerState {
    fakes: VecDeque<Fake>,
    /// Markers (the agent's NoOperation requests) seen so far.
    markers: u64,
    /// The RECORD thread is gone: nothing more will arrive.
    closed: bool,
}

/// The FakeInput requests RECORD reported and no raw event has claimed yet,
/// in the order the X server handled them, shared between the RECORD thread
/// (which adds) and the agent (which takes).
#[derive(Default)]
pub struct Ledger {
    state: Mutex<LedgerState>,
    cond: Condvar,
}

/// Requests kept at most: a client injecting faster than the agent drains
/// can't grow this without bound (the oldest go; their events, if any, are
/// then forwarded as unattributed).
const MAX_FAKES: usize = 4096;

impl Ledger {
    fn lock(&self) -> MutexGuard<'_, LedgerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push(&self, f: Fake) {
        let mut st = self.lock();
        if st.fakes.len() >= MAX_FAKES {
            st.fakes.pop_front();
        }
        st.fakes.push_back(f);
        drop(st);
        self.cond.notify_all();
    }

    /// The agent's marker request got through the RECORD stream.
    pub fn marker(&self) {
        self.lock().markers += 1;
        self.cond.notify_all();
    }

    pub fn close(&self) {
        self.lock().closed = true;
        self.cond.notify_all();
    }

    pub fn is_open(&self) -> bool {
        !self.lock().closed
    }

    pub fn markers(&self) -> u64 {
        self.lock().markers
    }

    /// Waits (at most `limit`) until `n` markers were seen; whether they were.
    pub fn wait_markers(&self, n: u64, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut st = self.lock();
        while st.markers < n && !st.closed {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .cond
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        st.markers >= n
    }

    /// The request that made the raw event `s`, if it has arrived. Requests
    /// before it made no event of their own (a press of a modifier already
    /// down, a move to where the pointer already was) and are dropped.
    pub fn take(&self, s: &Sig) -> Option<Fake> {
        let mut st = self.lock();
        while st.fakes.front().is_some_and(|f| f.stale_at(s.time)) {
            st.fakes.pop_front();
        }
        let i = st.fakes.iter().position(|f| f.matches(s))?;
        st.fakes.drain(..i);
        st.fakes.pop_front()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().fakes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::input::{BUTTON_PRESS, KEY_PRESS, KEY_RELEASE};
    use std::sync::Arc;

    fn who(base: u32, class: Class) -> Arc<Who> {
        Arc::new(Who {
            base,
            class,
            name: "t".into(),
            program: "t".into(),
            pid: None,
        })
    }

    fn fake(w: &Arc<Who>, type_: u8, detail: u8, x: i16, y: i16, time: u32) -> Fake {
        Fake {
            who: w.clone(),
            type_,
            detail,
            x,
            y,
            time,
        }
    }

    fn sig(type_: u8, detail: u8, time: u32, xy: Option<(i32, i32)>) -> Sig {
        Sig {
            type_,
            detail,
            time,
            xy,
            xy_raw: None,
        }
    }

    #[test]
    fn the_agents_own_input_never_goes_to_the_desktop() {
        for (class, stays) in [
            (Class::Agent, true),
            (Class::Helper, false),
            (Class::Driver, false),
            (Class::Other, false),
        ] {
            let mut w = (*who(1, Class::Agent)).clone();
            w.class = class;
            let f = fake(&Arc::new(w), BUTTON_PRESS, 1, 0, 0, 100);
            assert_eq!(f.stays_here(), stays, "{class:?}");
        }
    }

    #[test]
    fn events_claim_their_own_requests_in_order() {
        let (a, b) = (who(1, Class::Driver), who(2, Class::Helper));
        let l = Ledger::default();
        l.push(fake(&a, KEY_PRESS, 38, 0, 0, 100));
        l.push(fake(&b, KEY_PRESS, 38, 0, 0, 101));
        assert_eq!(l.take(&sig(KEY_PRESS, 38, 101, None)).unwrap().who.base, 1);
        assert_eq!(l.take(&sig(KEY_PRESS, 38, 102, None)).unwrap().who.base, 2);
        assert!(l.take(&sig(KEY_PRESS, 38, 102, None)).is_none());
    }

    #[test]
    fn requests_that_made_no_event_are_skipped() {
        let (a, b) = (who(1, Class::Other), who(2, Class::Helper));
        let l = Ledger::default();
        // a's move to where the pointer was, and a release of a key that was up
        l.push(fake(&a, MOTION_NOTIFY, 0, 10, 10, 100));
        l.push(fake(&a, KEY_RELEASE, 40, 0, 0, 100));
        l.push(fake(&b, MOTION_NOTIFY, 0, 600, 300, 101));
        l.push(fake(&b, BUTTON_PRESS, 1, 0, 0, 101));
        let m = l
            .take(&sig(MOTION_NOTIFY, 0, 101, Some((600, 300))))
            .unwrap();
        assert_eq!(m.who.base, 2);
        assert_eq!(
            l.take(&sig(BUTTON_PRESS, 1, 101, None)).unwrap().who.base,
            2
        );
        assert_eq!(l.len(), 0);
    }

    #[test]
    fn an_event_without_its_request_yet_takes_nothing() {
        let a = who(1, Class::Other);
        let l = Ledger::default();
        l.push(fake(&a, KEY_PRESS, 38, 0, 0, 100));
        assert!(l.take(&sig(BUTTON_PRESS, 1, 100, None)).is_none());
        // nothing was dropped: the key's own event still finds it
        assert_eq!(l.take(&sig(KEY_PRESS, 38, 100, None)).unwrap().who.base, 1);
    }

    #[test]
    fn relative_motion_matches_its_delta_as_given() {
        let a = who(1, Class::Other);
        let l = Ledger::default();
        l.push(fake(&a, MOTION_NOTIFY, 1, 5, -3, 100));
        let s = Sig {
            type_: MOTION_NOTIFY,
            detail: 0,
            time: 100,
            xy: Some((645, 357)),
            xy_raw: Some((5, -3)),
        };
        assert!(l.take(&s).is_some());
    }

    #[test]
    fn stale_requests_are_dropped() {
        let a = who(1, Class::Helper);
        let l = Ledger::default();
        l.push(fake(&a, KEY_PRESS, 38, 0, 0, 100));
        assert!(
            l.take(&sig(KEY_PRESS, 38, 100 + STALE_MS + 1, None))
                .is_none()
        );
        assert_eq!(l.len(), 0);
        // and the server clock wrapping around is not "stale"
        l.push(fake(&a, KEY_PRESS, 38, 0, 0, u32::MAX - 5));
        assert!(l.take(&sig(KEY_PRESS, 38, 3, None)).is_some());
    }

    #[test]
    fn markers_are_counted_and_awaited() {
        let l = Arc::new(Ledger::default());
        assert!(!l.wait_markers(1, Duration::from_millis(10)));
        let l2 = l.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            l2.marker();
        });
        assert!(l.wait_markers(1, Duration::from_secs(5)));
        t.join().unwrap();
        l.close();
        assert!(!l.wait_markers(2, Duration::from_secs(5)));
        assert!(!l.is_open());
    }

    #[test]
    fn the_ledger_is_bounded() {
        let a = who(1, Class::Other);
        let l = Ledger::default();
        for i in 0..(MAX_FAKES + 10) {
            l.push(fake(&a, KEY_PRESS, (i % 200) as u8, 0, 0, 100));
        }
        assert_eq!(l.len(), MAX_FAKES);
    }
}
