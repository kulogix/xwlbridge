//! Whether the framebuffer holds the desktop as it is now, for X clients that
//! read the screen while capture is stopped.
//!
//! Capture runs only while someone watches (a DAMAGE object, recent reads), so
//! a tool that reads the screen once (scrot, xwd, import, Java's Robot, a
//! `pyautogui.screenshot()`) arrives while it is stopped. Its read starts
//! capture, but on its own would be answered from what the framebuffer holds:
//! black on a fresh bridge, an old picture after a stop. A backend that reports
//! liveness here lets such a read wait (bounded) until every output has
//! delivered a frame of the new capture; one that doesn't is never waited for.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Freshness {
    state: Mutex<State>,
    changed: Condvar,
    /// Tells the backend someone wants the desktop now (it would otherwise
    /// notice the read on its next poll).
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct State {
    /// A backend reports liveness; without one, nobody waits.
    managed: bool,
    /// Capture runs and every output has delivered a frame since it started.
    live: bool,
    /// Capture can't start for now (sharing stopped from GNOME's panel, the
    /// screen locked): waiting would only delay the reader.
    unavailable: bool,
    /// Bumped whenever `live` or `unavailable` changes.
    epoch: u64,
    /// The epoch in which a wait ran out: later readers in the same epoch don't
    /// wait again (one stall per failed start, not one per read).
    timed_out: Option<u64>,
}

/// How a wait for a fresh frame ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Already live, or nothing to wait for (no backend reports liveness,
    /// capture unavailable, waiting disabled, or an earlier wait ran out).
    NotNeeded,
    /// Waited this long for capture to come up.
    Fresh(Duration),
    /// Capture didn't come up within the limit.
    TimedOut,
}

impl Freshness {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Called by a backend that reports liveness; `waker` makes it look at the
    /// capture gate right away.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend reports liveness
    pub fn manage(&self, waker: Box<dyn Fn() + Send + Sync>) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker);
        self.state().managed = true;
    }

    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend reports liveness
    pub fn set_live(&self, live: bool) {
        let mut st = self.state();
        if st.live != live {
            st.live = live;
            st.epoch += 1;
            self.changed.notify_all();
        }
    }

    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend reports liveness
    pub fn set_unavailable(&self, unavailable: bool) {
        let mut st = self.state();
        if st.unavailable != unavailable {
            st.unavailable = unavailable;
            st.epoch += 1;
            self.changed.notify_all();
        }
    }

    pub fn is_live(&self) -> bool {
        self.state().live
    }

    /// Whether a reader arriving now would wait (see [`Wait::NotNeeded`]).
    pub fn would_wait(&self) -> bool {
        let st = self.state();
        st.managed && !st.live && !st.unavailable && st.timed_out != Some(st.epoch)
    }

    pub fn wake_backend(&self) {
        if let Some(w) = self.waker.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            w();
        }
    }

    /// Blocks until capture is live, at most `limit`. The caller has already
    /// asked for capture (counted a read and woken the backend).
    pub fn wait(&self, limit: Duration) -> Wait {
        if limit.is_zero() || !self.would_wait() {
            return Wait::NotNeeded;
        }
        let t0 = Instant::now();
        let deadline = t0 + limit;
        let mut st = self.state();
        loop {
            if st.live {
                return Wait::Fresh(t0.elapsed());
            }
            if st.unavailable {
                return Wait::NotNeeded;
            }
            let now = Instant::now();
            if now >= deadline {
                st.timed_out = Some(st.epoch);
                return Wait::TimedOut;
            }
            st = self
                .changed
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn unmanaged_never_waits() {
        let f = Freshness::default();
        assert_eq!(f.wait(Duration::from_secs(5)), Wait::NotNeeded);
    }

    #[test]
    fn a_read_waits_for_the_first_frame() {
        let f = Arc::new(Freshness::default());
        f.manage(Box::new(|| {}));
        let g = f.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            g.set_live(true);
        });
        match f.wait(Duration::from_secs(5)) {
            Wait::Fresh(d) => assert!(d >= Duration::from_millis(40), "{d:?}"),
            w => panic!("{w:?}"),
        }
        t.join().unwrap();
        // live: no wait at all
        assert_eq!(f.wait(Duration::from_secs(5)), Wait::NotNeeded);
    }

    #[test]
    fn one_stall_per_failed_start() {
        let f = Freshness::default();
        f.manage(Box::new(|| {}));
        assert_eq!(f.wait(Duration::from_millis(20)), Wait::TimedOut);
        let t0 = Instant::now();
        assert_eq!(f.wait(Duration::from_secs(5)), Wait::NotNeeded);
        assert!(t0.elapsed() < Duration::from_millis(100));
        // capture came and went: the next cold read waits again
        f.set_live(true);
        f.set_live(false);
        assert!(f.would_wait());
    }

    #[test]
    fn unavailable_capture_is_not_waited_for() {
        let f = Arc::new(Freshness::default());
        f.manage(Box::new(|| {}));
        f.set_unavailable(true);
        assert_eq!(f.wait(Duration::from_secs(5)), Wait::NotNeeded);
        // and becoming unavailable releases a waiter
        f.set_unavailable(false);
        let g = f.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            g.set_unavailable(true);
        });
        assert_eq!(f.wait(Duration::from_secs(5)), Wait::NotNeeded);
        t.join().unwrap();
    }

    #[test]
    fn zero_limit_disables_waiting() {
        let f = Freshness::default();
        f.manage(Box::new(|| {}));
        assert_eq!(f.wait(Duration::ZERO), Wait::NotNeeded);
    }
}
