//! Telling a client's relative XTest pointer moves from its absolute ones.
//!
//! The agent follows the X server's pointer through the XInput2 raw motion
//! each XTest move makes, and the core MotionNotify that follows it on the
//! agent's full-screen input shield. The raw event is the same for both kinds
//! of move: its valuators are the position for an absolute move and the
//! distance for a relative one. The core event normally settles it, but while
//! another client holds a pointer grab (an explicit one, or the implicit grab a
//! button press gives a client that selects it on the root) the core event
//! goes to that client instead — and a relative move's distance would be taken
//! for a position: the click lands near the top-left corner.
//!
//! RECORD, which already reports each XTest FakeInput request (record.rs), says
//! which kind each move was, in the order the server took them. The agent pairs
//! those reports with its raw events (in the same order, each checked by its
//! values) whenever a raw event goes without its core event.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

/// One XTest FakeInput MotionNotify request, as RECORD saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Motion {
    pub relative: bool,
    pub x: i16,
    pub y: i16,
}

/// The moves RECORD saw that the agent hasn't paired yet, oldest first.
#[derive(Default)]
pub struct MotionLog {
    moves: Mutex<VecDeque<Motion>>,
    added: Condvar,
}

/// More unpaired moves than this means the agent isn't pairing; the oldest go.
const LOG_CAP: usize = 4096;

impl MotionLog {
    /// RECORD's thread: a move request was taken.
    pub fn push(&self, m: Motion) {
        let mut q = self.moves.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= LOG_CAP {
            q.pop_front();
        }
        q.push_back(m);
        drop(q);
        self.added.notify_all();
    }
}

/// A raw motion the agent saw: its id and its raw valuators 0 and 1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Raw {
    id: u64,
    x: i32,
    y: i32,
}

/// The agent's side: raw motions not yet paired with their request.
#[derive(Default)]
pub struct Pairing {
    owed: VecDeque<Raw>,
    next: u64,
}

/// More unpaired raw motions than this means RECORD isn't reporting; the
/// oldest go.
const OWED_CAP: usize = 1024;

impl Pairing {
    /// A raw motion with raw valuators `x`, `y` (the request's own values);
    /// returns its id, for [`Pairing::kind`].
    pub fn saw(&mut self, x: i32, y: i32) -> u64 {
        let id = self.next;
        self.next += 1;
        if self.owed.len() >= OWED_CAP {
            self.owed.pop_front();
        }
        self.owed.push_back(Raw { id, x, y });
        id
    }

    /// Pairs what can be paired now, without waiting.
    pub fn settle(&mut self, log: &MotionLog) {
        let mut q = log.moves.lock().unwrap_or_else(|e| e.into_inner());
        pair(&mut self.owed, &mut q, |_, _| {});
    }

    /// Whether raw motion `id` — the newest seen (what the agent asks about:
    /// earlier ones were settled or asked about already) — was a relative
    /// move, waiting until `deadline` for RECORD to report it (it normally has
    /// already: the server records a request before it makes the event).
    /// `None`: unknown (no report in time, or none will come).
    pub fn kind(
        &mut self,
        log: &MotionLog,
        id: u64,
        deadline: Instant,
        alive: impl Fn() -> bool,
    ) -> Option<bool> {
        let mut q = log.moves.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let mut found = None;
            pair(&mut self.owed, &mut q, |paired, kind| {
                if paired == id {
                    found = Some(kind);
                }
            });
            if let Some(kind) = found {
                return kind;
            }
            if !self.owed.iter().any(|r| r.id == id) {
                return None;
            }
            let now = Instant::now();
            if now >= deadline || !alive() {
                return None;
            }
            q = log
                .added
                .wait_timeout(q, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Pairs raw motions (`owed`, in the order the agent saw them) with requests
/// (`log`, in the order the server took them), calling `paired(id, kind)` for
/// each: `Some(relative)`, or `None` for a raw motion no request accounts for.
/// What can't be paired yet stays.
fn pair(
    owed: &mut VecDeque<Raw>,
    log: &mut VecDeque<Motion>,
    mut paired: impl FnMut(u64, Option<bool>),
) {
    while let Some(&raw) = owed.front() {
        // its request is the first with its values: those before it made no
        // event (a request the server refused, or one from before we looked).
        // If its request hasn't been reported yet, no later one has either.
        if let Some(i) = log
            .iter()
            .position(|m| i32::from(m.x) == raw.x && i32::from(m.y) == raw.y)
        {
            log.drain(..i);
            let m = log.pop_front().expect("found above");
            owed.pop_front();
            paired(raw.id, Some(m.relative));
            continue;
        }
        // the next request reported is a later raw motion's: this one came
        // from no request we saw
        match log.front() {
            Some(m)
                if owed
                    .iter()
                    .skip(1)
                    .any(|r| r.x == i32::from(m.x) && r.y == i32::from(m.y)) =>
            {
                owed.pop_front();
                paired(raw.id, None);
            }
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn abs(x: i16, y: i16) -> Motion {
        Motion {
            relative: false,
            x,
            y,
        }
    }
    fn rel(x: i16, y: i16) -> Motion {
        Motion {
            relative: true,
            x,
            y,
        }
    }
    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(50)
    }

    #[test]
    fn moves_pair_in_order() {
        let log = MotionLog::default();
        let mut p = Pairing::default();
        // xte 'mousemove 400 300' 'mousermove 10 10' 'mousermove 20 0' 'mousermove 20 0'
        for m in [abs(400, 300), rel(10, 10), rel(20, 0), rel(20, 0)] {
            log.push(m);
        }
        let kinds: Vec<_> = [(400, 300), (10, 10), (20, 0), (20, 0)]
            .iter()
            .map(|&(x, y)| {
                let id = p.saw(x, y);
                p.kind(&log, id, soon(), || true)
            })
            .collect();
        assert_eq!(kinds, vec![Some(false), Some(true), Some(true), Some(true)]);
        assert!(p.owed.is_empty() && log.moves.lock().unwrap().is_empty());
    }

    #[test]
    fn same_values_different_kinds() {
        // an absolute move to (10, 10), then a relative one by (10, 10)
        let log = MotionLog::default();
        let mut p = Pairing::default();
        log.push(abs(10, 10));
        log.push(rel(10, 10));
        let a = p.saw(10, 10);
        assert_eq!(p.kind(&log, a, soon(), || true), Some(false));
        let b = p.saw(10, 10);
        assert_eq!(p.kind(&log, b, soon(), || true), Some(true));
    }

    #[test]
    fn requests_that_made_no_event_are_skipped() {
        let log = MotionLog::default();
        let mut p = Pairing::default();
        log.push(abs(5000, 5000)); // refused (say, a bad root window)
        log.push(rel(3, -2));
        let id = p.saw(3, -2);
        assert_eq!(p.kind(&log, id, soon(), || true), Some(true));
        assert!(log.moves.lock().unwrap().is_empty());
    }

    #[test]
    fn a_raw_motion_no_request_accounts_for_is_given_up() {
        let log = MotionLog::default();
        let mut p = Pairing::default();
        let stray = p.saw(77, 77);
        let id = p.saw(3, -2);
        log.push(rel(3, -2));
        assert_eq!(p.kind(&log, id, soon(), || true), Some(true));
        assert_eq!(p.kind(&log, stray, soon(), || true), None);
        assert!(p.owed.is_empty());
    }

    #[test]
    fn raw_motions_ahead_of_record_wait_for_it() {
        let log = std::sync::Arc::new(MotionLog::default());
        let mut p = Pairing::default();
        let first = p.saw(400, 300);
        let id = p.saw(10, 10);
        p.settle(&log);
        assert_eq!(p.owed.len(), 2, "nothing reported yet: both stay owed");
        let l = log.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            l.push(abs(400, 300));
            l.push(rel(10, 10));
        });
        assert_eq!(
            p.kind(&log, id, Instant::now() + Duration::from_secs(5), || true),
            Some(true)
        );
        t.join().unwrap();
        // the earlier one was paired on the way
        assert_eq!(p.kind(&log, first, soon(), || true), None);
        assert!(p.owed.is_empty());
    }

    #[test]
    fn record_ahead_of_raw_motions_is_kept() {
        let log = MotionLog::default();
        let mut p = Pairing::default();
        log.push(abs(1, 1));
        log.push(rel(2, 2));
        let a = p.saw(1, 1);
        p.settle(&log);
        assert_eq!(
            log.moves.lock().unwrap().len(),
            1,
            "the later move waits for its raw motion"
        );
        let b = p.saw(2, 2);
        assert_eq!(p.kind(&log, b, soon(), || true), Some(true));
        assert_eq!(p.kind(&log, a, soon(), || true), None, "already paired");
    }

    #[test]
    fn no_report_in_time_is_unknown() {
        let log = MotionLog::default();
        let mut p = Pairing::default();
        let id = p.saw(1, 2);
        let t = Instant::now();
        assert_eq!(
            p.kind(&log, id, t + Duration::from_millis(20), || true),
            None
        );
        assert!(t.elapsed() >= Duration::from_millis(20));
        // and without RECORD running, no waiting at all
        let t = Instant::now();
        assert_eq!(p.kind(&log, id, t + Duration::from_secs(5), || false), None);
        assert!(t.elapsed() < Duration::from_secs(1));
        // a late report still pairs it, so later ones stay in step
        log.push(abs(1, 2));
        log.push(rel(5, 5));
        let next = p.saw(5, 5);
        assert_eq!(p.kind(&log, next, soon(), || true), Some(true));
    }

    #[test]
    fn caps() {
        let log = MotionLog::default();
        for i in 0..LOG_CAP + 10 {
            log.push(abs(i as i16, 0));
        }
        assert_eq!(log.moves.lock().unwrap().len(), LOG_CAP);
        let mut p = Pairing::default();
        for _ in 0..OWED_CAP + 10 {
            p.saw(-1, -1);
        }
        assert_eq!(p.owed.len(), OWED_CAP);
    }
}
