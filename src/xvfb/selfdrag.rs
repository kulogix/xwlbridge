//! `-showwindows` with the window sync: where the desktop's pointer is during
//! a held-button self-drag (an X client moving its own window while a button
//! is held on it: Qt docks, Tk and Swing palettes), so that the X pointer is
//! where the desktop's is and the window follows it exactly.
//!
//! Wayland tells the pointer's position only relative to the surface it is
//! on (wl_pointer.motion, a tablet tool's or a touch point's motion), and
//! during such a drag the surface itself moves: the desktop carries out the
//! client's MoveResize requests (winsync.rs) while the pointer moves. The
//! pointer's desktop position is the surface's origin when the compositor
//! made the event plus the event's surface-local position, and which origin
//! that was is not on the wire. mutter computes the surface-local position
//! from the surface actor's layout, which follows a move at its next frame;
//! the move's reply comes over D-Bus, another connection than the Wayland
//! events made before and after it; and mutter sends no wl_pointer.motion at
//! all when the surface-local position did not change (a window that keeps up
//! with the pointer), only the pointer frame. Taking the newest origin the
//! bridge knows of (b743bee) made the X pointer step back and forth by one
//! move at high event rates and overshoot at the end; driving the drag by
//! relative motion only (def9584) froze it for absolute devices, which send
//! none, and lost the first motion's fraction. So (`SelfDrag`):
//! - a frame's position is always its surface-local position plus one of the
//!   window's origins, never relative motion added up: a frame counts once
//!   whether the device sent motion, relative motion or both, nothing drifts
//!   (at a screen edge the desktop's pointer stops while relative motion goes
//!   on), and fractions are kept, the first motion's included;
//! - the origins it may be are the drag's start and every MoveResize sent
//!   since, minus those the window has certainly left: older than the newest
//!   one it has been laid out at (a frame callback committed after that
//!   move's reply fired, `laid_out`; long after the reply, `SETTLE`; or an
//!   origin a relative frame proved, below);
//! - of those, the most likely: the one whose position is nearest where the
//!   pointer is expected, weighed by how good that expectation has been, with
//!   a cost for a move sent after the frame's own time (the compositor's
//!   event times are CLOCK_MONOTONIC). With relative motion (a mouse, a
//!   touchpad, RemoteDesktop relative motion: zwp_relative_pointer_v1 gives
//!   the desktop pointer's own accelerated motion) the pointer is expected
//!   where that motion takes it, and a match proves the origin, so older ones
//!   are out from then on (the desktop's moves only go forward). Without (a
//!   tablet, QEMU's usb-tablet, a VNC or RDP viewer's pointer through
//!   RemoteDesktop or EIS; touch), where its track leads
//!   (`Track::expectations`: on at its velocity, slowing down to a stop if it
//!   slows, and its last step again; either may be right, as event times can
//!   be jittered and steps irregular, so a candidate is scored against both,
//!   each as good as it has been lately);
//! - an origin whose MoveResize is unanswered is where the desktop will put
//!   the window, which is not always where it was asked to: mutter keeps part
//!   of a window on the monitors' work areas, user moves included, so a drag
//!   past an edge is held there on every request. The bounds the replies
//!   showed are learned (`Bounds`) and an unanswered origin is placed inside
//!   them; a reply that went past a bound as asked drops it (another monitor);
//! - the X pointer follows one account of the drag (the main `Path`), which
//!   never goes back to an origin older than its own last one: events are
//!   made in order and the window goes through the origins in order. Its
//!   predictions come from its own picks, and the X client's moves, which
//!   become the next origins, follow those picks too: a wrong pick followed
//!   by one wrong the other way grew without bound (an absolute pointer that
//!   repeats positions or steps unevenly: 6232 px), and the order rules that
//!   out. Beside it, the most likely account of the drag up to each origin is
//!   kept, each judged by its own predictions, with room for a sudden turn;
//!   the X pointer moves over to one only when it explains the frames clearly
//!   better than the main one (`SWITCH_MARGIN`).
//!
//! Model-checked against a simulation (tests below): the compositor (moves
//! taking effect once applied or once laid out at the next frame, events made
//! at once or per frame, compressed or not, a main loop that is sometimes
//! busy), random latencies on both connections, the X client moving its
//! window with the pointer, the bridge pacing its MoveResize requests. The
//! simulation reproduces what was measured with both earlier rules
//! (research/release/p1/p1-validate.md) and the review of the first version
//! of this one (p1-absdrag-review.md: a drag the desktop holds at an edge,
//! repeated and uneven absolute positions); `sweep` compares the rules over
//! event rates, slowing drags and irregular event times.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A position on the desktop, which with the window sync is the X screen's
/// space too, in fractional pixels.
pub type Pos = (f64, f64);

/// A pause after which the pointer is taken to start again from rest rather
/// than to keep its last velocity (ms).
const PAUSE_MS: u32 = 100;

/// A prediction looks at most this many of the last interval between two
/// timed positions ahead (an event after a gap).
const MAX_STEPS: u32 = 4;

/// How close a candidate must be to where a frame's relative motion moved the
/// pointer to prove its origin (px, per axis). Candidates of two different
/// origins are at least 1 px apart on some axis; the relative motion of one
/// frame is exact to wl_fixed's 1/256 px, unless the desktop's pointer was
/// held at a screen edge or a barrier.
const REL_MATCH: f64 = 0.5;

/// An event is hardly ever relative to a move sent more than this long after
/// the event's own time (ms): the compositor makes the event from its input
/// promptly, and a move takes effect once applied (or, with the frame clock,
/// once laid out at the next frame, later still). The event's time is in
/// whole milliseconds. Hardly ever: when gnome-shell's main loop was busy,
/// input and calls that waited are taken in any order.
const SENT_AFTER_MS: u32 = 2;

/// How unlikely such a move is as an event's origin, as a cost in the same
/// unit as a candidate's distance from the expected position (squared,
/// over twice the expectation's squared spread: a negative log-likelihood).
const SENT_AFTER_COST: f64 = 4.0;

/// The least spread of an expected position (px): what wl_fixed and the
/// pixel grid leave of even a perfect prediction.
const MIN_SPREAD: f64 = 0.25;

/// The spread of the position a frame's relative motion leads to (px).
const REL_SPREAD: f64 = 0.1;

/// An event's time is taken to be on our clock (CLOCK_MONOTONIC, as mutter's
/// event times are) only if the event reaches us at most this long after it
/// (ms), and not from the future.
const CLOCK_SLACK_MS: u32 = 1000;

/// After this long since its reply a move has surely been laid out, even if
/// no frame callback said so (a hidden window gets none).
pub const SETTLE: Duration = Duration::from_millis(250);

/// How much more likely another account of the drag must be than the main
/// one before the X pointer follows it instead (a negative log-likelihood
/// difference): enough that a coincidence does not move it, little enough
/// that a run of wrong picks does (each costs several).
const SWITCH_MARGIN: f64 = 10.0;

/// How an account of the drag weighs a frame that none of its predictions
/// expected (a sudden turn, an uneven step): per axis, a broad component of
/// this weight and spread (px) beside each prediction's own, so that one
/// such frame costs it a few units, not hundreds.
const TURN_WEIGHT: f64 = 0.05;
const TURN_SPREAD: f64 = 16.0;

/// The accounts of the drag kept beside the main one (one per origin, the
/// most likely first).
const MAX_PATHS: usize = 8;

/// ln(sqrt(2 pi)): the normal density's constant, per axis.
const LN_SQRT_2PI: f64 = 0.918_938_533_204_672_8;

/// The pointer's last positions, for predicting the next one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Track {
    pos: Option<Pos>,
    /// The last step.
    step: Option<Pos>,
    /// The last position whose event had the compositor's time (ms): a
    /// frame whose motion the compositor left out (unchanged surface-local
    /// position) has none.
    timed: Option<(Pos, u32)>,
    /// The velocity between the last two timed positions (px per ms) and
    /// their interval, unless a pause was between them.
    velocity: Option<(Pos, u32)>,
    /// How the velocity changed between the last two intervals (px per ms
    /// per ms).
    accel: Option<Pos>,
    /// Positions since the last timed one: the pointer kept moving.
    untimed: u32,
    /// How far off the two predictions have been lately (mean squared px,
    /// decaying): by velocity and time, and by the last step. Times can be
    /// jittered (a pointer over a network: the compositor stamps arrival),
    /// and steps can be irregular (a device whose events the compositor
    /// merges per frame); whichever has been right lately is used. Scored
    /// only where the position is certain (`score`).
    err_time: f64,
    err_step: f64,
    /// The same over every position of one account of the drag
    /// (`score_path`): how well that account explains its own frames.
    path_time: f64,
    path_step: f64,
}

/// The weight of the newest error in `Track`'s decaying means.
const ERR_WEIGHT: f64 = 0.3;

impl Track {
    /// The pointer is at `pos` (an event of compositor time `time`, in ms).
    pub fn update(&mut self, pos: Pos, time: Option<u32>) {
        if let Some(prev) = self.pos {
            self.step = Some((pos.0 - prev.0, pos.1 - prev.1));
        }
        self.pos = Some(pos);
        let Some(t) = time else {
            self.untimed = self.untimed.saturating_add(1);
            return;
        };
        let moving = std::mem::take(&mut self.untimed) > 0;
        if let Some((tp, tt)) = self.timed {
            match elapsed(Some(tt), Some(t)) {
                // a step out of a pause (the first of a drag, after the
                // press) says where the pointer goes, not how fast
                Some(dt) if dt > PAUSE_MS && !moving => (self.velocity, self.accel) = (None, None),
                Some(dt) if dt > 0 => {
                    let k = 1.0 / f64::from(dt);
                    let v = ((pos.0 - tp.0) * k, (pos.1 - tp.1) * k);
                    self.accel = self.velocity.map(|(v1, i1)| {
                        let span = (f64::from(i1) + f64::from(dt)) / 2.0;
                        ((v.0 - v1.0) / span, (v.1 - v1.1) / span)
                    });
                    self.velocity = Some((v, dt));
                }
                Some(_) => {} // the same millisecond
                None => (self.velocity, self.accel) = (None, None),
            }
        }
        self.timed = Some((pos, t));
    }

    pub fn pos(&self) -> Option<Pos> {
        self.pos
    }

    /// Where the pointer is likely to be at `time`: where it is after a
    /// pause; else by the prediction that has been closer lately: on at its
    /// velocity from the last timed position, or its last step once more.
    #[cfg(test)]
    pub fn predict(&self, time: Option<u32>) -> Option<Pos> {
        let pos = self.pos?;
        if self.paused(time) {
            return Some(pos);
        }
        match self.by_time(time) {
            Some(p) if self.err_time <= self.err_step => Some(p),
            _ => self.by_step(),
        }
    }

    /// The pointer was in fact at `pos` at `time` (a frame with one possible
    /// origin): how far off each prediction was. Before `update`.
    pub fn score(&mut self, pos: Pos, time: Option<u32>) {
        if let Some((t, s)) = self.misses(pos, time) {
            self.err_time += ERR_WEIGHT * (t - self.err_time);
            self.err_step += ERR_WEIGHT * (s - self.err_step);
        }
    }

    /// The pointer was at `pos` at `time` on the account of the drag this
    /// track is (`SelfDrag`'s paths): how far off each prediction was along
    /// it. Before `update`.
    fn score_path(&mut self, pos: Pos, time: Option<u32>) {
        if let Some((t, s)) = self.misses(pos, time) {
            self.path_time += ERR_WEIGHT * (t - self.path_time);
            self.path_step += ERR_WEIGHT * (s - self.path_step);
        }
    }

    /// How far (squared px) each prediction is from `pos` at `time`, unless
    /// after a pause.
    fn misses(&self, pos: Pos, time: Option<u32>) -> Option<(f64, f64)> {
        if self.paused(time) {
            return None;
        }
        let off = |p: Option<Pos>| p.map(|p| (p.0 - pos.0).powi(2) + (p.1 - pos.1).powi(2));
        off(self.by_time(time)).zip(off(self.by_step()))
    }

    /// Where the pointer may be at `time`, each with how far off that
    /// prediction has been lately (px): by velocity and time, and by the last
    /// step; where it is after a pause.
    pub fn expectations(&self, time: Option<u32>) -> Vec<(Pos, f64)> {
        self.expect(time, (self.err_time, self.err_step))
    }

    /// The same, each as good as it has been along this account of the drag.
    fn path_expectations(&self, time: Option<u32>) -> Vec<(Pos, f64)> {
        self.expect(time, (self.path_time, self.path_step))
    }

    fn expect(&self, time: Option<u32>, (err_time, err_step): (f64, f64)) -> Vec<(Pos, f64)> {
        let Some(pos) = self.pos else {
            return Vec::new();
        };
        if self.paused(time) {
            return vec![(pos, MIN_SPREAD)];
        }
        let spread = |err: f64| err.sqrt().max(MIN_SPREAD);
        let mut v = Vec::with_capacity(2);
        if let Some(p) = self.by_time(time) {
            v.push((p, spread(err_time)));
        }
        if let Some(p) = self.by_step() {
            v.push((p, spread(err_step)));
        }
        v
    }

    fn paused(&self, time: Option<u32>) -> bool {
        self.untimed == 0
            && self
                .timed
                .and_then(|(_, tt)| elapsed(Some(tt), time))
                .is_some_and(|dt| dt > PAUSE_MS)
    }

    fn by_time(&self, time: Option<u32>) -> Option<Pos> {
        let (tp, tt) = self.timed?;
        let dt = elapsed(Some(tt), time)?;
        let (v, interval) = self.velocity?;
        let dt = f64::from(dt.min(interval.saturating_mul(MAX_STEPS)));
        let a = self.accel.unwrap_or_default();
        let half = f64::from(interval) / 2.0;
        Some((
            slowing(tp.0, v.0, a.0, half, dt),
            slowing(tp.1, v.1, a.1, half, dt),
        ))
    }

    fn by_step(&self) -> Option<Pos> {
        let pos = self.pos?;
        let step = self.step.unwrap_or_default();
        Some((pos.0 + step.0, pos.1 + step.1))
    }
}

/// Where one coordinate goes in `dt` ms from `p`, with mean velocity `v`
/// over the last interval (whose middle was `half` ms ago) and change of
/// velocity `a`: a pointer that slows down (`a` against `v`) is taken to go
/// on slowing, to a stop and not back (a drag's end); otherwise its
/// velocity holds.
fn slowing(p: f64, v: f64, a: f64, half: f64, dt: f64) -> f64 {
    if v * a >= 0.0 {
        return p + v * dt;
    }
    let now = v + a * half;
    if now * v <= 0.0 {
        return p; // stopped already
    }
    let t = dt.min(-now / a);
    p + now * t + 0.5 * a * t * t
}

/// Milliseconds from `from` to `to` on the compositor's (wrapping) clock.
fn elapsed(from: Option<u32>, to: Option<u32>) -> Option<u32> {
    let d = to?.wrapping_sub(from?);
    (d as i32 >= 0).then_some(d)
}

/// One pointer frame during the drag.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Frame {
    /// Its surface-local position: the frame's motion, or none when the
    /// compositor sent none (it does not when the position is unchanged).
    pub surface: Option<Pos>,
    /// Its relative motion, if the device is a relative one.
    pub rel: Option<Pos>,
    /// The compositor's time of the frame's events (ms).
    pub time: Option<u32>,
}

/// The parts of a wl_pointer frame as its events come in.
#[derive(Clone, Copy, Debug, Default)]
pub struct Parts {
    surface: Option<Pos>,
    rel: Option<Pos>,
    time: Option<u32>,
    /// A button, axis, enter or leave event: the frame is not a motion.
    other: bool,
}

impl Parts {
    pub fn motion(&mut self, surface: Pos, time: u32) {
        self.surface = Some(surface);
        self.time = Some(time);
    }

    /// Relative motion (several in one frame add up); `time` in ms.
    pub fn relative(&mut self, d: Pos, time: u32) {
        let (x, y) = self.rel.unwrap_or_default();
        self.rel = Some((x + d.0, y + d.1));
        self.time.get_or_insert(time);
    }

    /// Something else is in the frame: the motion so far, which goes before
    /// it, and the frame's end is no motion of its own.
    pub fn other(&mut self) -> Option<Frame> {
        let f = self.take_motion();
        self.other = true;
        f
    }

    /// wl_pointer.frame: the motion it carried. A frame of nothing at all is
    /// one: the compositor sends a frame for every pointer motion, and
    /// leaves the motion out when the surface-local position is unchanged.
    pub fn end(&mut self) -> Option<Frame> {
        let p = std::mem::take(self);
        if p.surface.is_none() && p.rel.is_none() && p.other {
            return None;
        }
        Some(Frame {
            surface: p.surface,
            rel: p.rel,
            time: p.time,
        })
    }

    fn take_motion(&mut self) -> Option<Frame> {
        if self.surface.is_none() && self.rel.is_none() {
            return None;
        }
        let f = Frame {
            surface: self.surface.take(),
            rel: self.rel.take(),
            time: self.time.take(),
        };
        Some(f)
    }
}

/// A position the window had or was asked to have during the drag.
#[derive(Clone, Copy, Debug)]
struct Origin {
    at: (i32, i32),
    /// `at` is the position asked for, not one the desktop told (the drag's
    /// start and an answered MoveResize are those).
    asked: bool,
    /// When its MoveResize was sent, on CLOCK_MONOTONIC (ms), if known.
    sent: Option<u32>,
    /// Its MoveResize was answered (when): `at` is what the desktop applied.
    reply: Option<Instant>,
    /// Its MoveResize was refused: the window never went there.
    refused: bool,
}

/// Where the desktop has held the window during the drag, per axis (x, y):
/// the least and the most position a reply put it at in place of one beyond
/// (mutter's constrain_partially_onscreen keeps part of a window on the
/// monitors' work areas, user moves included). A drag past an edge is held
/// there on every request, so a request not yet answered is placed inside
/// what the replies showed; a reply that went past a bound as asked drops it
/// (the window has gone where another monitor lets it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Bounds {
    least: [Option<i32>; 2],
    most: [Option<i32>; 2],
}

impl Bounds {
    /// The desktop put the window at `applied` for a request to `requested`.
    fn learn(&mut self, requested: (i32, i32), applied: (i32, i32)) {
        for (axis, (r, a)) in [(requested.0, applied.0), (requested.1, applied.1)]
            .into_iter()
            .enumerate()
        {
            if a > r {
                self.least[axis] = Some(a);
            } else if a < r {
                self.most[axis] = Some(a);
            } else {
                if self.least[axis].is_some_and(|b| r < b) {
                    self.least[axis] = None;
                }
                if self.most[axis].is_some_and(|b| r > b) {
                    self.most[axis] = None;
                }
            }
        }
    }

    /// Where a request to `at` will put the window, as far as the replies
    /// so far tell.
    fn place(&self, at: (i32, i32)) -> (i32, i32) {
        let one = |v: i32, axis: usize| {
            let v = self.least[axis].map_or(v, |b| v.max(b));
            self.most[axis].map_or(v, |b| v.min(b))
        };
        (one(at.0, 0), one(at.1, 1))
    }
}

/// One account of the drag so far: the pointer's track through one sequence
/// of the window's origins, the newest of them (its number `at`), and how
/// unlikely the account is (`cost`, a negative log-likelihood of its frames,
/// less that of the most likely account).
#[derive(Clone, Copy, Debug)]
struct Path {
    track: Track,
    at: usize,
    cost: f64,
}

/// Where the pointer is if a frame was made with origin number `n`.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    n: usize,
    pos: Pos,
    /// The origin's MoveResize was sent after the frame's event was made.
    late: bool,
}

/// The pointer of one held-button self-drag of a synced toplevel.
#[derive(Debug)]
pub struct SelfDrag {
    /// The window's possible origins, oldest first: the drag's start, then
    /// each MoveResize sent, in order; `origins[0]` is number `base`.
    origins: VecDeque<Origin>,
    base: usize,
    bounds: Bounds,
    /// The account of the drag the X pointer follows.
    main: Path,
    /// The most likely account up to each origin, the most likely first.
    paths: Vec<Path>,
    /// The last surface-local position a frame had.
    surface: Option<Pos>,
}

impl SelfDrag {
    /// A drag of a window at `origin` on the desktop (settled: no move of
    /// it under way), with the pointer's track so far and its last
    /// surface-local position on the window.
    pub fn new(origin: (i32, i32), track: Track, surface: Option<Pos>) -> SelfDrag {
        let main = Path {
            track,
            at: 0,
            cost: 0.0,
        };
        SelfDrag {
            origins: VecDeque::from([Origin {
                at: origin,
                asked: false,
                sent: None,
                reply: None,
                refused: false,
            }]),
            base: 0,
            bounds: Bounds::default(),
            main,
            paths: vec![main],
            surface,
        }
    }

    /// A MoveResize of the window to `at` was sent (at `ms` on
    /// CLOCK_MONOTONIC, the clock of the compositor's event times): its
    /// number.
    pub fn sent(&mut self, at: (i32, i32), ms: Option<u32>) -> usize {
        self.origins.push_back(Origin {
            at,
            asked: true,
            sent: ms,
            reply: None,
            refused: false,
        });
        self.base + self.origins.len() - 1
    }

    /// The reply to the MoveResize to `requested`: the desktop put the window
    /// at `applied`. Its number, for the frame callback that tells when the
    /// window has been laid out there (`laid_out`).
    pub fn applied(
        &mut self,
        requested: (i32, i32),
        applied: (i32, i32),
        now: Instant,
    ) -> Option<usize> {
        let i = self
            .origins
            .iter()
            .rposition(|o| o.reply.is_none() && !o.refused && o.at == requested)?;
        if i == 0 && self.base == 0 {
            return None; // the start is not a request
        }
        let o = &mut self.origins[i];
        o.at = applied;
        o.asked = false;
        o.reply = Some(now);
        self.bounds.learn(requested, applied);
        Some(self.base + i)
    }

    /// The MoveResize in flight failed. `answered`: the desktop refused it
    /// (the window did not move); otherwise (a timeout) it may still land.
    pub fn failed(&mut self, answered: bool) {
        if !answered {
            return;
        }
        // not the oldest: the drag's start, or where the window has been
        // laid out
        if let Some(o) = self
            .origins
            .iter_mut()
            .skip(1)
            .rev()
            .find(|o| o.reply.is_none() && !o.refused)
        {
            o.refused = true;
        }
    }

    /// The window has been laid out at origin `n` (or a later one): every
    /// event the compositor makes from now on is relative to it or to a
    /// later one.
    pub fn laid_out(&mut self, n: usize) {
        if n <= self.base || n >= self.base + self.origins.len() {
            return;
        }
        if self.origins[n - self.base].refused {
            return;
        }
        self.origins.drain(..n - self.base);
        self.base = n;
    }

    /// The newest origin it has been laid out at by now.
    #[cfg(test)]
    pub fn laid_out_at(&self) -> usize {
        self.base
    }

    /// A pointer frame: where the pointer is on the desktop (None: no
    /// surface-local position known yet). `now_ms`: CLOCK_MONOTONIC now (ms),
    /// to tell whether the frame's time is on that clock.
    pub fn frame(&mut self, f: Frame, now: Instant, now_ms: Option<u32>) -> Option<Pos> {
        let s = f.surface.or(self.surface)?;
        self.surface = Some(s);
        self.settle(now);
        // a move sent after the event was made is an unlikely origin
        let made = f.time.filter(|&t| {
            now_ms.is_some_and(|n| {
                let d = n.wrapping_sub(t) as i32;
                (-(SENT_AFTER_MS as i32)..=CLOCK_SLACK_MS as i32).contains(&d)
            })
        });
        let late = |o: &Origin| {
            made.zip(o.sent).is_some_and(|(t, sent)| {
                elapsed(Some(t), Some(sent)).is_some_and(|d| d > SENT_AFTER_MS)
            })
        };
        let candidates: Vec<Candidate> = self
            .origins
            .iter()
            .enumerate()
            .filter(|(_, o)| !o.refused)
            .map(|(i, o)| {
                let at = if o.asked {
                    self.bounds.place(o.at)
                } else {
                    o.at
                };
                Candidate {
                    n: self.base + i,
                    pos: (f64::from(at.0) + s.0, f64::from(at.1) + s.1),
                    late: late(o),
                }
            })
            .collect();
        let late_cost = |c: &Candidate| if c.late { SENT_AFTER_COST } else { 0.0 };
        // the main account goes on to the most likely origin, not older than
        // its own last: its candidate near where the pointer is expected (a
        // mixture of the expectations, each as good as it has been lately),
        // and not sent after the event was made
        let main = self.main;
        let expects = expectations(&main.track, &f, false);
        let mut pick: Option<(&Candidate, f64)> = None;
        for any in [false, true] {
            for c in candidates.iter().filter(|c| any || c.n >= main.at) {
                let k = near(&expects, c.pos) + late_cost(c);
                // the oldest of equals; without an expectation, the newest
                if pick.is_none_or(|b| k < b.1 || (expects.is_empty() && k <= b.1)) {
                    pick = Some((c, k));
                }
            }
            if pick.is_some() {
                break;
            }
        }
        let (c, _) = pick?;
        let mut chosen = (
            Path {
                at: c.n,
                cost: main.cost
                    + unlikely(&expectations(&main.track, &f, true), c.pos)
                    + late_cost(c),
                ..main
            },
            c.pos,
        );
        // the most likely account up to each origin, the main one's included
        let mut next: Vec<(Path, Pos)> = Vec::new();
        for any in [false, true] {
            for p in self.paths.iter().chain(std::iter::once(&main)) {
                let e = expectations(&p.track, &f, true);
                for c in candidates.iter().filter(|c| any || c.n >= p.at) {
                    let cost = p.cost + unlikely(&e, c.pos) + late_cost(c);
                    let path = Path {
                        at: c.n,
                        cost,
                        ..*p
                    };
                    match next.iter_mut().find(|(q, _)| q.at == c.n) {
                        Some(q) if cost < q.0.cost => *q = (path, c.pos),
                        Some(_) => {}
                        None => next.push((path, c.pos)),
                    }
                }
            }
            if !next.is_empty() {
                break;
            }
        }
        // the X pointer moves over to another account only when it explains
        // the frames clearly better
        if let Some(best) = next
            .iter()
            .min_by(|a, b| a.0.cost.total_cmp(&b.0.cost).then(a.0.at.cmp(&b.0.at)))
            && best.0.cost + SWITCH_MARGIN < chosen.0.cost
        {
            chosen = *best;
        }
        let pos = chosen.1;
        // what a frame's relative motion says, which proves an origin
        let rel_match = |p: &Path, at: Pos| {
            f.rel.zip(p.track.pos()).is_some_and(|(d, q)| {
                (at.0 - (q.0 + d.0)).abs() < REL_MATCH && (at.1 - (q.1 + d.1)).abs() < REL_MATCH
            })
        };
        if rel_match(&chosen.0, pos) {
            // the relative motion proves the origin the compositor used
            self.laid_out(chosen.0.at);
        }
        let one = candidates.len() == 1;
        let least = next
            .iter()
            .map(|(p, _)| p.cost)
            .fold(chosen.0.cost, f64::min);
        for (p, at) in next.iter_mut().chain(std::iter::once(&mut chosen)) {
            if one || rel_match(p, *at) {
                p.track.score(*at, f.time);
            }
            p.track.score_path(*at, f.time);
            p.track.update(*at, f.time);
            p.cost -= least;
        }
        next.sort_by(|a, b| a.0.cost.total_cmp(&b.0.cost));
        next.truncate(MAX_PATHS);
        self.paths = next.into_iter().map(|(p, _)| p).collect();
        self.main = chosen.0;
        Some(pos)
    }

    /// Moves answered `SETTLE` ago have been laid out, frame callback or
    /// not.
    fn settle(&mut self, now: Instant) {
        let n = self.origins.iter().rposition(|o| {
            !o.refused
                && o.reply
                    .is_some_and(|t| now.saturating_duration_since(t) >= SETTLE)
        });
        if let Some(i) = n {
            self.laid_out(self.base + i);
        }
    }
}

/// A frame's expectations on track `t`: where its relative motion leads, or
/// where the track's predictions do, each as good as it has been where the
/// position was certain, or (`path`) along the track's own account.
fn expectations(t: &Track, f: &Frame, path: bool) -> Vec<(Pos, f64)> {
    match (f.rel, t.pos()) {
        (Some(d), Some(p)) => vec![((p.0 + d.0, p.1 + d.1), REL_SPREAD)],
        _ if path => t.path_expectations(f.time),
        _ => t.expectations(f.time),
    }
}

/// How unlikely position `c` is under `expects` (a negative log-likelihood
/// of a mixture of normal distributions): the pick of one frame's origin.
fn near(expects: &[(Pos, f64)], c: Pos) -> f64 {
    let terms: Vec<f64> = expects
        .iter()
        .map(|&(e, s)| {
            let d = (c.0 - e.0).powi(2) + (c.1 - e.1).powi(2);
            -d / (2.0 * s * s) - 2.0 * s.ln()
        })
        .collect();
    -log_sum(&terms)
}

/// The same for an account of the drag, which must survive a frame none of
/// its predictions expected (a sudden turn, an uneven step): per axis, each
/// prediction's normal distribution with a broad one beside it
/// (`TURN_WEIGHT`, `TURN_SPREAD`), the predictions weighed alike.
fn unlikely(expects: &[(Pos, f64)], c: Pos) -> f64 {
    let normal = |d: f64, s: f64| -d * d / (2.0 * s * s) - s.ln() - LN_SQRT_2PI;
    let axis = |d: f64, s: f64| {
        log_sum(&[
            (1.0 - TURN_WEIGHT).ln() + normal(d, s),
            TURN_WEIGHT.ln() + normal(d, TURN_SPREAD.max(s)),
        ])
    };
    let terms: Vec<f64> = expects
        .iter()
        .map(|&(e, s)| axis(c.0 - e.0, s) + axis(c.1 - e.1, s) - (expects.len() as f64).ln())
        .collect();
    -log_sum(&terms)
}

/// ln(sum(exp(terms))), 0 for none.
fn log_sum(terms: &[f64]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let top = terms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    top + terms.iter().map(|t| (t - top).exp()).sum::<f64>().ln()
}

/// The X root position of desktop position `pos`, on the X screen.
pub fn root_position(pos: Pos, screen: (u16, u16)) -> (i16, i16) {
    let clamp = |v: f64, size: u16| {
        let max = i32::from(size).max(1) - 1;
        (v.floor().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32).clamp(0, max) as i16
    };
    (clamp(pos.0, screen.0), clamp(pos.1, screen.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn at(t: Instant, ms: u64) -> Instant {
        t + Duration::from_millis(ms)
    }

    fn moving(pos: Pos, step: Pos, dt: u32) -> Track {
        let mut tr = Track::default();
        tr.update((pos.0 - step.0, pos.1 - step.1), Some(1000));
        tr.update(pos, Some(1000 + dt));
        tr
    }

    #[test]
    fn the_track_goes_on_at_its_velocity() {
        let tr = moving((100.0, 50.0), (2.0, 1.0), 4);
        assert_eq!(tr.predict(Some(1008)), Some((102.0, 51.0)));
        assert_eq!(
            tr.predict(Some(1012)),
            Some((104.0, 52.0)),
            "two steps' time"
        );
        assert_eq!(tr.predict(None), Some((102.0, 51.0)), "no time: one step");
        assert_eq!(tr.predict(Some(1004 + 200)), Some((100.0, 50.0)), "a pause");
        assert_eq!(
            tr.predict(Some(1004 + 60)),
            Some((108.0, 54.0)),
            "a gap: at most 4 intervals"
        );
        // a frame without a time (the compositor left its motion out) in
        // between: the velocity still holds from the last timed position
        let mut tr = moving((100.0, 50.0), (8.0, 4.0), 16);
        tr.update((108.0, 54.0), None);
        tr.update((116.0, 58.0), Some(1048));
        assert_eq!(
            tr.predict(Some(1052)),
            Some((118.0, 59.0)),
            "a short last step"
        );
        // frames without a time keep the pointer moving: no pause
        let mut tr = moving((100.0, 50.0), (8.0, 4.0), 16);
        for i in 1..=8 {
            tr.update(
                (100.0 + 8.0 * f64::from(i), 50.0 + 4.0 * f64::from(i)),
                None,
            );
        }
        tr.update((172.0, 86.0), Some(1016 + 16 * 9));
        assert_eq!(tr.predict(Some(1016 + 16 * 9 + 8)), Some((176.0, 88.0)));
        // the first step out of a pause: its direction and size, not its time
        let mut tr = Track::default();
        tr.update((10.0, 10.0), Some(500));
        tr.update((15.0, 13.0), Some(1000));
        assert_eq!(tr.predict(Some(1030)), Some((20.0, 16.0)));
        let mut still = Track::default();
        assert_eq!(still.predict(Some(5)), None);
        still.update((3.5, 4.5), None);
        assert_eq!(still.predict(Some(5)), Some((3.5, 4.5)), "one position");
    }

    #[test]
    fn the_compositors_clock_wraps() {
        let mut tr = Track::default();
        tr.update((0.0, 0.0), Some(u32::MAX - 1));
        tr.update((2.0, 0.0), Some(2)); // 4 ms later
        assert_eq!(tr.predict(Some(6)), Some((4.0, 0.0)));
    }

    #[test]
    fn a_frame_is_its_motion_its_relative_motion_or_nothing() {
        let mut p = Parts::default();
        p.motion((10.0, 20.0), 7);
        p.relative((1.0, 2.0), 7);
        p.relative((0.5, 0.25), 7);
        assert_eq!(
            p.end(),
            Some(Frame {
                surface: Some((10.0, 20.0)),
                rel: Some((1.5, 2.25)),
                time: Some(7),
            })
        );
        // the compositor sent no motion: the surface-local position is the
        // last one
        assert_eq!(p.end(), Some(Frame::default()));
        // a button frame is no motion
        assert_eq!(p.other(), None);
        assert_eq!(p.end(), None);
        // a motion before a button in one frame goes first
        p.motion((1.0, 1.0), 9);
        assert_eq!(
            p.other(),
            Some(Frame {
                surface: Some((1.0, 1.0)),
                rel: None,
                time: Some(9),
            })
        );
        assert_eq!(p.end(), None);
    }

    /// Finding 1 of p1-validate: an absolute device sends no relative
    /// motion; its motion must move the drag.
    #[test]
    fn absolute_motion_moves_the_drag() {
        let t = t0();
        let mut d = SelfDrag::new((300, 200), moving((410.0, 280.0), (0.0, 0.0), 30), None);
        let f = |x: f64, y: f64, ms| Frame {
            surface: Some((x, y)),
            rel: None,
            time: Some(ms),
        };
        assert_eq!(d.frame(f(115.0, 82.5, 1060), t, None), Some((415.0, 282.5)));
        assert_eq!(d.sent((305, 202), None), 1);
        // before the move took effect, and after
        assert_eq!(d.frame(f(120.0, 85.0, 1090), t, None), Some((420.0, 285.0)));
        assert_eq!(d.applied((305, 202), (305, 202), t), Some(1));
        assert_eq!(d.frame(f(120.0, 85.5, 1120), t, None), Some((425.0, 287.5)));
        d.laid_out(1);
        assert_eq!(d.laid_out_at(), 1);
        assert_eq!(d.frame(f(125.0, 88.0, 1150), t, None), Some((430.0, 290.0)));
    }

    /// A relative device's frame counts once (its motion and relative motion
    /// are one move of the pointer), keeps fractions, and proves the origin.
    #[test]
    fn relative_motion_picks_the_origin_and_counts_once() {
        let t = t0();
        let mut tr = Track::default();
        tr.update((651.84375, 420.796875), Some(1000));
        let mut d = SelfDrag::new((540, 340), tr, Some((111.84375, 80.796875)));
        assert_eq!(d.sent((541, 340), None), 1);
        assert_eq!(d.sent((546, 343), None), 2);
        // made relative to origin 1 (the window there, not yet at 2)
        let p = d.frame(
            Frame {
                surface: Some((115.6875, 83.59375)),
                rel: Some((4.84375, 2.796875)),
                time: Some(1030),
            },
            t,
            None,
        );
        assert_eq!(p, Some((656.6875, 423.59375)), "not added to the motion");
        assert_eq!(d.laid_out_at(), 1, "proved");
        // no motion (unchanged surface-local position), relative motion: the
        // window moved with the pointer, to origin 2
        let p = d.frame(
            Frame {
                surface: None,
                rel: Some((4.84375, 2.796875)),
                time: Some(1060),
            },
            t,
            None,
        );
        assert_eq!(p, Some((661.6875, 426.59375)));
        assert_eq!(d.laid_out_at(), 2);
    }

    /// A move sent after the event was made is an unlikely origin: it loses
    /// to an older one unless the prediction has been reliable and says so
    /// clearly (gnome-shell's main loop may have been busy).
    #[test]
    fn a_move_sent_after_the_event_is_an_unlikely_origin() {
        let t = t0();
        let track = |err: f64| Track {
            pos: Some((410.0, 280.0)),
            step: Some((9.8, 0.0)),
            timed: Some(((410.0, 280.0), 1030)),
            velocity: Some(((9.8 / 30.0, 0.0), 30)),
            accel: None,
            untimed: 0,
            err_time: err,
            err_step: err + 5.0,
            path_time: err,
            path_step: err + 5.0,
        };
        // both predictions say 419.8: the newer origin's candidate (420)
        let drag = |err: f64, sent: u32| {
            let mut d = SelfDrag::new((300, 200), track(err), None);
            d.sent((305, 200), Some(sent));
            d
        };
        let f = Frame {
            surface: Some((115.0, 80.0)),
            rel: None,
            time: Some(1060),
        };
        // predictions 5 px off lately: the timing decides
        assert_eq!(
            drag(25.0, 1063).frame(f, t, Some(1064)),
            Some((415.0, 280.0))
        );
        // sent in time, or no times (or another clock): the nearer one
        assert_eq!(
            drag(25.0, 1062).frame(f, t, Some(1064)),
            Some((420.0, 280.0))
        );
        assert_eq!(drag(25.0, 1063).frame(f, t, None), Some((420.0, 280.0)));
        assert_eq!(
            drag(25.0, 1063).frame(f, t, Some(900_000)),
            Some((420.0, 280.0))
        );
        // exact predictions lately: the prediction decides
        assert_eq!(
            drag(0.0, 1063).frame(f, t, Some(1064)),
            Some((420.0, 280.0))
        );
    }

    /// Either prediction may be the right one (event times jittered or not):
    /// a candidate one of them expects wins over one neither does.
    #[test]
    fn a_candidate_either_prediction_expects_wins() {
        let t = t0();
        // stepping 2 px; this event came late (9 ms, steps were 4 ms): by
        // time 422.5 (nearer the newer origin's 424), by the last step 420
        let mut tr = Track::default();
        tr.update((414.0, 280.0), Some(1000));
        tr.update((416.0, 280.0), Some(1004));
        tr.update((418.0, 280.0), Some(1008));
        let mut d = SelfDrag::new((300, 200), tr, None);
        d.sent((304, 200), Some(1009));
        let f = Frame {
            surface: Some((120.0, 80.0)),
            rel: None,
            time: Some(1017),
        };
        assert_eq!(d.frame(f, t, Some(1018)), Some((420.0, 280.0)));
    }

    /// Jittered times (the compositor stamps a network pointer's arrival):
    /// the step prediction takes over once it has been closer.
    #[test]
    fn the_better_prediction_lately_is_used() {
        let mut tr = Track::default();
        let times = [1000, 1010, 1011, 1040, 1050, 1051, 1080, 1090];
        for (i, &t) in times.iter().enumerate() {
            let p = (100.0 + 5.0 * i as f64, 50.0);
            tr.score(p, Some(t));
            tr.update(p, Some(t));
        }
        let next = tr.predict(Some(1091)).unwrap();
        assert_eq!(next, (140.0, 50.0), "the step, not the time");
        // steady times: the time prediction
        let mut tr = Track::default();
        for i in 0..8 {
            let p = (100.0 + 5.0 * f64::from(i), 50.0);
            tr.score(p, Some(1000 + 10 * i));
            tr.update(p, Some(1000 + 10 * i));
        }
        assert_eq!(tr.predict(Some(1090)), Some((145.0, 50.0)));
    }

    #[test]
    fn a_laid_out_move_leaves_older_origins_out() {
        let t = t0();
        let mut d = SelfDrag::new(
            (0, 0),
            moving((50.0, 50.0), (0.0, 0.0), 10),
            Some((50.0, 50.0)),
        );
        let one = d.sent((10, 0), None);
        let two = d.sent((20, 0), None);
        assert_eq!(d.applied((10, 0), (10, 0), t), Some(one));
        d.laid_out(two); // not applied yet: a later frame callback can still say so
        assert_eq!(d.laid_out_at(), two);
        d.laid_out(one); // older: nothing
        assert_eq!(d.laid_out_at(), two);
        // only origin 2 is left: an unexpected position is taken as it is
        let p = d.frame(
            Frame {
                surface: Some((40.0, 50.0)),
                rel: None,
                time: Some(1020),
            },
            t,
            None,
        );
        assert_eq!(p, Some((60.0, 50.0)));
    }

    #[test]
    fn a_move_answered_long_ago_is_laid_out() {
        let t = t0();
        let mut d = SelfDrag::new((0, 0), moving((5.0, 5.0), (0.0, 0.0), 10), Some((5.0, 5.0)));
        d.sent((3, 0), None);
        d.applied((3, 0), (3, 0), t);
        let still = Frame::default();
        d.frame(still, at(t, 100), None);
        assert_eq!(d.laid_out_at(), 0);
        d.frame(still, at(t, 260), None);
        assert_eq!(d.laid_out_at(), 1);
    }

    #[test]
    fn a_refused_move_is_no_origin() {
        let t = t0();
        let mut d = SelfDrag::new((0, 0), moving((5.0, 5.0), (1.0, 0.0), 10), Some((5.0, 5.0)));
        d.sent((6, 0), None);
        d.failed(true);
        // the prediction (6,5) would match origin 1 exactly; it is refused
        let p = d.frame(
            Frame {
                surface: Some((6.0, 5.0)),
                rel: None,
                time: Some(1020),
            },
            t,
            None,
        );
        assert_eq!(p, Some((6.0, 5.0)));
        d.laid_out(1);
        assert_eq!(d.laid_out_at(), 0, "never laid out there");
        assert_eq!(d.applied((6, 0), (6, 0), t), None);
        // where the window has been laid out is never refused
        let mut d = SelfDrag::new((0, 0), Track::default(), Some((5.0, 5.0)));
        d.sent((6, 0), None);
        d.laid_out(1);
        d.failed(true);
        assert_eq!(
            d.frame(Frame::default(), t, None),
            Some((11.0, 5.0)),
            "still a candidate"
        );
    }

    #[test]
    fn a_clamped_move_is_where_the_desktop_put_it() {
        let t = t0();
        let mut d = SelfDrag::new((0, 0), Track::default(), Some((5.0, 5.0)));
        d.sent((-30, 0), None);
        assert_eq!(d.applied((-30, 0), (0, 0), t), Some(1));
        d.laid_out(1);
        let p = d.frame(
            Frame {
                surface: Some((5.0, 5.0)),
                rel: None,
                time: None,
            },
            t,
            None,
        );
        assert_eq!(p, Some((5.0, 5.0)));
    }

    /// What the replies showed of the desktop's constraints (p1-absdrag-
    /// review F1), and a bound another monitor lifts.
    #[test]
    fn the_desktop_holds_a_window_where_its_replies_showed() {
        let mut b = Bounds::default();
        assert_eq!(b.place((10, -30)), (10, -30));
        // the top edge held the window at y 32 (asked -30)
        b.learn((10, -30), (10, 32));
        assert_eq!(b.place((14, -40)), (14, 32));
        assert_eq!(b.place((14, 50)), (14, 50), "below the edge: as asked");
        // the right edge, as far as one monitor goes
        b.learn((900, 40), (780, 40));
        assert_eq!(b.place((950, 0)), (780, 32));
        // another monitor lets it further right, as asked: that bound is gone
        b.learn((820, 40), (820, 40));
        assert_eq!(b.place((950, 0)), (950, 32));
        // a move inside the bound left keeps it
        b.learn((100, 100), (100, 100));
        assert_eq!(b.place((14, -40)), (14, 32));
    }

    /// A move not yet answered is where the desktop will hold the window,
    /// not where it was asked to go.
    #[test]
    fn an_unanswered_move_past_an_edge_is_where_the_desktop_holds_it() {
        let t = t0();
        let mut d = SelfDrag::new((300, 130), Track::default(), Some((110.0, 80.0)));
        d.sent((310, 110), None);
        assert_eq!(d.applied((310, 110), (310, 120), t), Some(1), "held at 120");
        d.sent((315, 100), None);
        d.laid_out(2); // the only origin left
        let f = Frame {
            surface: Some((110.0, 70.0)),
            rel: None,
            time: None,
        };
        assert_eq!(d.frame(f, t, None), Some((425.0, 190.0)), "not 425,170");
        assert_eq!(d.applied((315, 100), (315, 120), t), Some(2));
        // the drag's start is where the desktop had the window, bounds or not
        let mut still = Track::default();
        still.update((410.0, 180.0), Some(1000));
        let mut d = SelfDrag::new((300, 100), still, Some((110.0, 80.0)));
        d.sent((300, 90), None);
        d.applied((300, 90), (300, 120), t);
        let f = Frame {
            surface: None,
            rel: None,
            time: Some(2000),
        };
        assert_eq!(d.frame(f, t, None), Some((410.0, 180.0)), "not 410,200");
    }

    /// The window goes through its origins in order and events are made in
    /// order: once the main account has a frame at origin 2, a later frame
    /// is not taken to be made with origin 1, however near its candidate
    /// lies (an account that went back and forth could follow its own
    /// errors without bound: p1-absdrag-review F2).
    #[test]
    fn the_main_account_does_not_go_back_to_an_older_origin() {
        let t = t0();
        let mut d = SelfDrag::new(
            (0, 0),
            moving((40.0, 50.0), (10.0, 0.0), 10),
            Some((40.0, 50.0)),
        );
        d.sent((10, 0), None);
        d.sent((20, 0), None);
        let f = |x: f64, ms| Frame {
            surface: Some((x, 50.0)),
            rel: None,
            time: Some(ms),
        };
        // 30, 40 or 50: on at 10 px per frame, origin 2
        assert_eq!(d.frame(f(30.0, 1020), t, None), Some((50.0, 50.0)));
        // 48, 58 or 68 for an expected 60: 58 would be origin 1
        assert_eq!(d.frame(f(48.0, 1030), t, None), Some((68.0, 50.0)));
    }

    #[test]
    fn root_positions_are_on_the_x_screen() {
        let s = (1280, 720);
        assert_eq!(root_position((130.75, 89.999), s), (130, 89));
        assert_eq!(root_position((-0.5, 700.0), s), (0, 700));
        assert_eq!(root_position((5000.0, 1e12), s), (1279, 719));
        assert_eq!(root_position((f64::NAN, -1e12), s), (0, 0));
    }
}

/// A simulation of a held self-drag end to end: the compositor (mutter's
/// rules for pointer events, moves over D-Bus, frame callbacks), the bridge
/// (this module's rule, or one of the two earlier ones) and the X client
/// (moving its window with the X pointer, as selfdrag.py and a palette's
/// drag handle do).
#[cfg(test)]
mod sim {
    use super::*;
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    /// When a move takes effect for pointer events, and when they are made.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Model {
        /// Events are made as input comes; a move counts from the moment it
        /// is applied.
        Immediate,
        /// mutter's frame clock: input is processed at the start of a frame
        /// (each event, or only the last motion of a device per frame), with
        /// the layout of the frame before; a move counts once laid out.
        Frames { compress: bool },
        /// Immediate, but now and then gnome-shell's main loop is busy
        /// (3 to 12 ms): input and MoveResize calls that came meanwhile are
        /// then taken in any order.
        Stalling,
    }

    #[derive(Clone, Copy, Debug)]
    pub enum Device {
        /// To `to` in equal steps.
        Absolute { from: Pos, to: Pos },
        /// From `from`, `step` (already accelerated) per event.
        Relative { from: Pos, step: Pos },
        /// From `from` to `to`, slowing down to a stop (ease-out), as a
        /// person's drag ends; relative motion or absolute positions.
        Easing { from: Pos, to: Pos, relative: bool },
        /// Event i of n at `f(i, n)` (i = 0: the start), in wl_fixed;
        /// relative motion or absolute positions.
        Path {
            f: fn(usize, usize) -> Pos,
            relative: bool,
        },
    }

    /// Where the desktop puts a window asked to go to a position.
    pub type Constraint = fn((i32, i32)) -> (i32, i32);

    /// The bridge's rule for the X pointer during the drag.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Rule {
        /// `SelfDrag`.
        Origins,
        /// b743bee: the newest origin the desktop reported plus each motion.
        Newest,
        /// def9584: once the drag began, its start plus the relative motion
        /// only.
        RelativeOnly,
    }

    #[derive(Clone, Copy, Debug)]
    pub struct Run {
        pub model: Model,
        pub device: Device,
        pub steps: usize,
        /// Between two device events (ms).
        pub period: f64,
        pub rule: Rule,
        pub seed: u64,
        /// Irregular event times (a VNC viewer's pointer over a network):
        /// each up to half a period early or late, some in bursts of the
        /// same millisecond.
        pub jitter: bool,
        /// Where the desktop puts the window asked to go somewhere (mutter's
        /// constraints: an edge it holds the window at).
        pub constrain: Option<Constraint>,
    }

    #[derive(Debug)]
    pub struct Outcome {
        /// The X pointer at each motion the bridge made.
        pub path: Vec<(i32, i32)>,
        /// Where the desktop's pointer was when the compositor made the
        /// event each of those came from.
        pub truth: Vec<(i32, i32)>,
        /// The desktop's pointer moved by (whole pixels).
        pub pointer_moved: (i32, i32),
        pub x_pointer_moved: (i32, i32),
        pub x_window_moved: (i32, i32),
        pub desk_window_moved: (i32, i32),
        pub start: (i32, i32),
    }

    impl Outcome {
        /// X motions that go back on either axis (the drag only goes right
        /// and down).
        pub fn back_steps(&self) -> usize {
            let mut prev = self.start;
            let mut n = 0;
            for &p in &self.path {
                if p.0 < prev.0 || p.1 < prev.1 {
                    n += 1;
                }
                prev = p;
            }
            n
        }

        /// The farthest any X motion was from where the desktop's pointer was
        /// (px, either axis).
        pub fn worst_off(&self) -> i32 {
            self.path
                .iter()
                .zip(&self.truth)
                .map(|(a, b)| (a.0 - b.0).abs().max((a.1 - b.1).abs()))
                .max()
                .unwrap_or(0)
        }

        /// How far the X pointer ended from the desktop's (px, either axis).
        pub fn end_off(&self) -> i32 {
            (self.x_pointer_moved.0 - self.pointer_moved.0)
                .abs()
                .max((self.x_pointer_moved.1 - self.pointer_moved.1).abs())
        }

        /// The farthest the X pointer went past the desktop pointer's end.
        pub fn overshoot(&self) -> (i32, i32) {
            let end = (
                self.start.0 + self.pointer_moved.0,
                self.start.1 + self.pointer_moved.1,
            );
            self.path
                .iter()
                .fold((0, 0), |m, p| (m.0.max(p.0 - end.0), m.1.max(p.1 - end.1)))
        }
    }

    /// splitmix64
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }

        fn range(&mut self, a: f64, b: f64) -> f64 {
            a + (b - a) * self.unit()
        }
    }

    /// wl_fixed_t (libwayland's wl_fixed_from_double truncates).
    fn fixed(v: f64) -> f64 {
        (v * 256.0).trunc() / 256.0
    }

    fn floor(p: Pos) -> (i32, i32) {
        (p.0.floor() as i32, p.1.floor() as i32)
    }

    enum Msg {
        /// wl_pointer.motion; the true pointer then (test bookkeeping).
        Motion(Pos, u32, (i32, i32)),
        Relative(Pos, u32, (i32, i32)),
        Frame((i32, i32)),
        /// A frame callback of ours fired (the origin it marks).
        Done(usize),
    }

    enum Ev {
        /// Device event i reaches the compositor.
        Input(usize),
        /// A compositor frame.
        Frame,
        /// MoveResize k reaches gnome-shell.
        Request(usize),
        /// A commit of ours with a frame callback reaches the compositor.
        Commit(usize),
        /// A Wayland event reaches the bridge.
        Wayland(Msg),
        /// The reply to MoveResize k reaches the bridge.
        Reply(usize),
        /// The X client's move of its window (to this position) reaches the
        /// bridge.
        Moved((i32, i32)),
        /// The bridge's pacing timer.
        Timer,
        /// The compositor's main loop is free again.
        StallEnd,
    }

    struct Item {
        t: f64,
        seq: u64,
        ev: Ev,
    }

    impl PartialEq for Item {
        fn eq(&self, o: &Self) -> bool {
            self.cmp(o) == Ordering::Equal
        }
    }
    impl Eq for Item {}
    impl PartialOrd for Item {
        fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for Item {
        // earliest first (a max-heap): by time, then by order of scheduling
        fn cmp(&self, o: &Self) -> Ordering {
            o.t.total_cmp(&self.t).then(o.seq.cmp(&self.seq))
        }
    }

    const FRAME_MS: f64 = 1000.0 / 60.0;

    thread_local! {
        /// Print what happens (debugging a run: set it in a test, then
        /// `cargo test NAME -- --nocapture`).
        pub static TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub fn run(r: Run) -> Outcome {
        let mut rng = Rng(r.seed);
        let trace = TRACE.with(|t| t.get());
        let mut q = BinaryHeap::new();
        let mut seq = 0u64;
        macro_rules! at {
            ($t:expr, $ev:expr) => {{
                seq += 1;
                q.push(Item {
                    t: $t,
                    seq,
                    ev: $ev,
                });
            }};
        }
        let base = Instant::now();
        let instant = |t: f64| base + Duration::from_secs_f64(t.max(0.0) / 1000.0);

        // the drag: the window at 300,200, the pointer 110,80 into it
        let w0 = (300, 200);
        let relative = matches!(
            r.device,
            Device::Relative { .. }
                | Device::Easing { relative: true, .. }
                | Device::Path { relative: true, .. }
        );
        let constrain = |w: (i32, i32)| r.constrain.map_or(w, |c| c(w));
        let (p0, inputs): (Pos, Vec<Pos>) = match r.device {
            Device::Absolute { from, to } => (
                from,
                (1..=r.steps)
                    .map(|i| {
                        let k = i as f64 / r.steps as f64;
                        (from.0 + (to.0 - from.0) * k, from.1 + (to.1 - from.1) * k)
                    })
                    .collect(),
            ),
            Device::Relative { from, step } => (from, vec![step; r.steps]),
            Device::Easing { from, to, relative } => {
                let at = |i: usize| {
                    let k = 1.0 - (1.0 - i as f64 / r.steps as f64).powi(2);
                    (
                        fixed(from.0 + (to.0 - from.0) * k),
                        fixed(from.1 + (to.1 - from.1) * k),
                    )
                };
                let points = (1..=r.steps).map(|i| {
                    if relative {
                        let (a, b) = (at(i - 1), at(i));
                        (b.0 - a.0, b.1 - a.1)
                    } else {
                        at(i)
                    }
                });
                (from, points.collect())
            }
            Device::Path { f, .. } => {
                let at = |i: usize| {
                    let p = f(i, r.steps);
                    (fixed(p.0), fixed(p.1))
                };
                let points = (1..=r.steps).map(|i| {
                    if relative {
                        let (a, b) = (at(i - 1), at(i));
                        (b.0 - a.0, b.1 - a.1)
                    } else {
                        at(i)
                    }
                });
                (at(0), points.collect())
            }
        };
        let start_ms = 1000.0;
        let mut times: Vec<f64> = (0..inputs.len())
            .map(|i| start_ms + r.period * (i + 1) as f64)
            .collect();
        if r.jitter {
            let mut last = start_ms;
            for t in times.iter_mut() {
                *t = if rng.unit() < 0.2 {
                    last // a burst
                } else {
                    (*t + r.period * rng.range(-0.5, 0.5)).max(last)
                };
                last = *t;
            }
        }
        let input_time = |i: usize| times[i];
        // input reaches the compositor in order
        let mut arrived = 0.0f64;
        for i in 0..inputs.len() {
            arrived = (input_time(i) + rng.range(0.0, 0.5)).max(arrived);
            at!(arrived, Ev::Input(i));
        }
        let end = times.last().copied().unwrap_or(start_ms) + 1500.0;
        let phase = rng.range(0.0, FRAME_MS);
        at!(start_ms + phase, Ev::Frame);
        let mut stalls: Vec<(f64, f64)> = Vec::new();
        if r.model == Model::Stalling {
            let mut t = start_ms;
            while t < end {
                t += rng.range(30.0, 150.0);
                let len = rng.range(3.0, 12.0);
                stalls.push((t, t + len));
                t += len;
            }
        }
        let stalled_at = |t: f64| stalls.iter().find(|s| t >= s.0 && t < s.1).map(|s| s.1);
        let mut stalled: (VecDeque<usize>, VecDeque<usize>) = Default::default();
        let mut stall_end: Option<f64> = None;

        // compositor
        let mut pointer = p0;
        let mut window = w0; // as applied
        let mut layout = w0; // as laid out (Frames)
        let mut last_s: Option<Pos> = Some((p0.0 - w0.0 as f64, p0.1 - w0.1 as f64));
        let mut queued: Vec<usize> = Vec::new(); // input waiting for a frame
        let mut commits: Vec<usize> = Vec::new(); // frame callbacks waiting
        let mut wl_last = 0.0f64;
        // the X connection and our Wayland requests are in order too
        let mut x_last = 0.0f64;
        let mut up_last = 0.0f64;
        // bridge
        let x_start = floor(p0);
        let mut track = Track::default();
        track.update(p0, Some(start_ms as u32 - 300));
        let mut drag = SelfDrag::new(w0, track, last_s);
        let mut parts = Parts::default();
        let mut x_pointer = x_start;
        let mut x_window = w0;
        let mut reported = w0; // the newest origin the desktop reported
        let mut began = false; // def9584: the drag (relative only) began
        let mut rel_start = (0, 0);
        let mut rel_acc = (0.0, 0.0);
        let mut requests: Vec<(i32, i32)> = vec![w0];
        let mut in_flight = false;
        let mut pending: Option<(i32, i32)> = None;
        let mut target = w0;
        let mut last_send: Option<f64> = None;
        let mut timer = false;
        let mut path = Vec::new();
        let mut truth = Vec::new();

        while let Some(Item { t, ev, .. }) = q.pop() {
            if t > end {
                break;
            }
            // what the compositor sends us in this step: flushed together,
            // after what it sent before
            let mut out: Vec<Msg> = Vec::new();
            // the compositor makes the pointer events of input `ins`
            let process = |ins: &[usize],
                           origin: (i32, i32),
                           pointer: &mut Pos,
                           last_s: &mut Option<Pos>,
                           out: &mut Vec<Msg>| {
                if ins.is_empty() {
                    return;
                }
                let mut rel = (0.0, 0.0);
                for &i in ins {
                    if relative {
                        *pointer = (pointer.0 + inputs[i].0, pointer.1 + inputs[i].1);
                        rel = (rel.0 + inputs[i].0, rel.1 + inputs[i].1);
                    } else {
                        *pointer = inputs[i];
                    }
                }
                let ms = input_time(*ins.last().unwrap()) as u32;
                let tr = floor(*pointer);
                let s = (
                    fixed(pointer.0 - f64::from(origin.0)),
                    fixed(pointer.1 - f64::from(origin.1)),
                );
                // mutter: no motion when the surface-local position is
                // unchanged, relative motion for relative devices, a frame
                // always
                if *last_s != Some(s) {
                    *last_s = Some(s);
                    out.push(Msg::Motion(s, ms, tr));
                }
                if relative {
                    out.push(Msg::Relative((fixed(rel.0), fixed(rel.1)), ms, tr));
                }
                out.push(Msg::Frame(tr));
            };
            match ev {
                Ev::Input(i) => match r.model {
                    Model::Stalling if stalled_at(t).is_some() => {
                        let e = stalled_at(t).unwrap();
                        stalled.0.push_back(i);
                        if stall_end.is_none() {
                            stall_end = Some(e);
                            at!(e, Ev::StallEnd);
                        }
                    }
                    Model::Immediate | Model::Stalling => {
                        process(&[i], window, &mut pointer, &mut last_s, &mut out)
                    }
                    Model::Frames { .. } => queued.push(i),
                },
                Ev::StallEnd => {
                    stall_end = None;
                    // what came meanwhile, input and calls in any order
                    while !stalled.0.is_empty() || !stalled.1.is_empty() {
                        let input =
                            stalled.1.is_empty() || (!stalled.0.is_empty() && rng.unit() < 0.5);
                        if input {
                            let i = stalled.0.pop_front().unwrap();
                            process(&[i], window, &mut pointer, &mut last_s, &mut out);
                        } else {
                            let k = stalled.1.pop_front().unwrap();
                            window = constrain(requests[k]);
                            let lat = rng.range(0.2, 8.0);
                            at!(t + lat, Ev::Reply(k));
                        }
                    }
                }
                Ev::Frame if stalled_at(t).is_some() => at!(t + FRAME_MS, Ev::Frame),
                Ev::Frame => {
                    if let Model::Frames { compress } = r.model {
                        let ins = std::mem::take(&mut queued);
                        if compress {
                            process(&ins, layout, &mut pointer, &mut last_s, &mut out);
                        } else {
                            for i in ins {
                                process(&[i], layout, &mut pointer, &mut last_s, &mut out);
                            }
                        }
                        layout = window;
                    }
                    for c in std::mem::take(&mut commits) {
                        out.push(Msg::Done(c));
                    }
                    at!(t + FRAME_MS, Ev::Frame);
                }
                Ev::Request(k) if r.model == Model::Stalling && stalled_at(t).is_some() => {
                    stalled.1.push_back(k);
                    if stall_end.is_none() {
                        let e = stalled_at(t).unwrap();
                        stall_end = Some(e);
                        at!(e, Ev::StallEnd);
                    }
                }
                Ev::Request(k) => {
                    window = constrain(requests[k]);
                    if trace {
                        eprintln!("{t:9.3} applied {k} {:?}", window);
                    }
                    let lat = rng.range(0.2, 8.0);
                    at!(t + lat, Ev::Reply(k));
                }
                Ev::Commit(n) => commits.push(n),
                Ev::Reply(k) => {
                    in_flight = false;
                    reported = constrain(requests[k]);
                    if r.rule == Rule::Origins
                        && let Some(n) = drag.applied(requests[k], reported, instant(t))
                    {
                        up_last = (t + rng.range(0.05, 1.0)).max(up_last);
                        at!(up_last, Ev::Commit(n));
                    }
                }
                Ev::Moved(w) => {
                    x_window = w;
                    if !began {
                        // begin_move: def9584 starts from the X pointer
                        began = true;
                        rel_start = x_pointer;
                    }
                    pending = Some(w);
                }
                Ev::Timer => timer = false,
                Ev::Wayland(m) => {
                    let mut to: Option<((i32, i32), (i32, i32))> = None;
                    match (r.rule, m) {
                        (_, Msg::Done(n)) => drag.laid_out(n),
                        (Rule::Origins, Msg::Motion(s, ms, _)) => parts.motion(s, ms),
                        (Rule::Origins, Msg::Relative(d, ms, _)) => parts.relative(d, ms),
                        (Rule::Origins, Msg::Frame(tr)) => {
                            if let Some(f) = parts.end()
                                && let Some(p) = drag.frame(f, instant(t), Some(t as u32))
                            {
                                if trace {
                                    eprintln!(
                                        "{t:9.3} frame {f:?} -> {p:?} truth {tr:?} laid out {} of {:?}",
                                        drag.laid_out_at(),
                                        drag.origins.iter().map(|o| o.at).collect::<Vec<_>>()
                                    );
                                }
                                to = Some((floor(p), tr));
                            }
                        }
                        (Rule::Newest, Msg::Motion(s, _, tr)) => {
                            let p = (f64::from(reported.0) + s.0, f64::from(reported.1) + s.1);
                            to = Some((floor(p), tr));
                        }
                        (Rule::RelativeOnly, Msg::Motion(s, _, tr)) if !began => {
                            let p = (f64::from(reported.0) + s.0, f64::from(reported.1) + s.1);
                            to = Some((floor(p), tr));
                        }
                        (Rule::RelativeOnly, Msg::Relative(d, _, tr)) if began => {
                            rel_acc = (rel_acc.0 + d.0, rel_acc.1 + d.1);
                            let p = (
                                f64::from(rel_start.0) + rel_acc.0,
                                f64::from(rel_start.1) + rel_acc.1,
                            );
                            to = Some((floor(p), tr));
                        }
                        _ => {}
                    }
                    if let Some((x, tr)) = to
                        && x != x_pointer
                    {
                        x_pointer = x;
                        path.push(x);
                        truth.push(tr);
                        // the client moves its window with the pointer
                        let w = (w0.0 + x.0 - x_start.0, w0.1 + x.1 - x_start.1);
                        x_last = (t + rng.range(0.2, 1.0)).max(x_last);
                        at!(x_last, Ev::Moved(w));
                    }
                }
            }
            if !out.is_empty() {
                let d = (t + rng.range(0.05, 1.5)).max(wl_last);
                wl_last = d;
                for m in out {
                    at!(d, Ev::Wayland(m));
                }
            }
            // the bridge's MoveResize: one in flight, at most one per 16 ms
            if pending == Some(target) {
                pending = None;
            }
            if let Some(w) = pending
                && !in_flight
            {
                let due = last_send.map_or(t, |l| l + 16.0);
                if due <= t {
                    pending = None;
                    requests.push(w);
                    let k = requests.len() - 1;
                    if r.rule == Rule::Origins {
                        assert_eq!(drag.sent(w, Some(t as u32)), k);
                    }
                    in_flight = true;
                    target = w;
                    last_send = Some(t);
                    let lat = rng.range(0.2, 3.0);
                    at!(t + lat, Ev::Request(k));
                } else if !timer {
                    timer = true;
                    at!(due, Ev::Timer);
                }
            }
        }
        let p1 = floor(pointer);
        let moved = |a: (i32, i32), b: (i32, i32)| (a.0 - b.0, a.1 - b.1);
        Outcome {
            path,
            truth,
            pointer_moved: moved(p1, x_start),
            x_pointer_moved: moved(x_pointer, x_start),
            x_window_moved: moved(x_window, w0),
            desk_window_moved: moved(window, w0),
            start: x_start,
        }
    }

    const MODELS: [Model; 4] = [
        Model::Immediate,
        Model::Frames { compress: false },
        Model::Frames { compress: true },
        Model::Stalling,
    ];

    /// p1-validate's drags: RemoteDesktop absolute and the uinput tablet,
    /// +120,+70 in 24 events at 33 Hz; the tablet at 250 Hz, +240,+120 in
    /// 120 events (and +120,+70); the uinput mouse, whose accelerated motion
    /// took the desktop pointer +116,+67 from a fractional start.
    fn absolute(dx: f64, dy: f64) -> Device {
        Device::Absolute {
            from: (410.0, 280.0),
            to: (410.0 + dx, 280.0 + dy),
        }
    }

    fn mouse() -> Device {
        // 24 steps of 4.84375,2.796875 (exact in wl_fixed): +116.25,+67.125
        Device::Relative {
            from: (410.0, 280.0),
            step: (4.84375, 2.796875),
        }
    }

    fn check_exact(r: Run, want: (i32, i32)) {
        let o = run(r);
        let what = format!("{r:?}");
        assert_eq!(o.pointer_moved, want, "{what}: the desktop pointer");
        assert_eq!(
            o.path, o.truth,
            "{what}: the X pointer is the desktop's at every motion"
        );
        assert_eq!(o.back_steps(), 0, "{what}");
        assert_eq!(o.overshoot(), (0, 0), "{what}");
        assert_eq!(o.x_pointer_moved, want, "{what}: X pointer");
        assert_eq!(o.x_window_moved, want, "{what}: X window");
        assert_eq!(o.desk_window_moved, want, "{what}: desktop window");
    }

    #[test]
    fn an_absolute_drag_at_33_hz_follows_exactly() {
        for model in MODELS {
            for seed in 0..40 {
                let r = Run {
                    model,
                    device: absolute(120.0, 70.0),
                    steps: 24,
                    period: 30.0,
                    rule: Rule::Origins,
                    seed,
                    jitter: false,
                    constrain: None,
                };
                check_exact(r, (120, 70));
                assert_eq!(run(r).path.len(), 24, "every event moves the X pointer");
            }
        }
    }

    #[test]
    fn an_absolute_drag_at_250_hz_follows_exactly() {
        for model in MODELS {
            for seed in 0..40 {
                for (d, want) in [((240.0, 120.0), (240, 120)), ((120.0, 70.0), (120, 70))] {
                    let r = Run {
                        model,
                        device: absolute(d.0, d.1),
                        steps: 120,
                        period: 4.0,
                        rule: Rule::Origins,
                        seed,
                        jitter: false,
                        constrain: None,
                    };
                    check_exact(r, want);
                }
            }
        }
    }

    #[test]
    fn a_relative_drag_follows_exactly() {
        for model in MODELS {
            for seed in 0..40 {
                for (steps, period, d) in [(24, 30.0, mouse()), (120, 4.0, mouse())] {
                    let want = match steps {
                        24 => (116, 67),
                        _ => (581, 335), // 120 * 4.84375, 120 * 2.796875
                    };
                    let r = Run {
                        model,
                        device: d,
                        steps,
                        period,
                        rule: Rule::Origins,
                        seed,
                        jitter: false,
                        constrain: None,
                    };
                    check_exact(r, want);
                }
            }
        }
    }

    /// Runs of `device` at 33, 125 and 250 Hz (or the given rates) in every
    /// model over `seeds`: the worst X motion and the worst end (px).
    fn worst(
        device: Device,
        rates: &[(usize, f64)],
        constrain: Option<Constraint>,
        seeds: u64,
        models: &[Model],
    ) -> (i32, i32) {
        let mut w = (0, 0);
        for &model in models {
            for &(steps, period) in rates {
                for seed in 0..seeds {
                    let o = run(Run {
                        model,
                        device,
                        steps,
                        period,
                        rule: Rule::Origins,
                        seed,
                        jitter: false,
                        constrain,
                    });
                    w = (w.0.max(o.worst_off()), w.1.max(o.end_off()));
                }
            }
        }
        w
    }

    const RATES: [(usize, f64); 3] = [(24, 30.0), (90, 8.0), (120, 4.0)];

    /// mutter keeps part of a window on the work areas, user moves included
    /// (constraints.c, constrain_partially_onscreen): here the window's top
    /// at y 120, its right side at x 500.
    fn top_edge(w: (i32, i32)) -> (i32, i32) {
        (w.0, w.1.max(120))
    }

    fn right_edge(w: (i32, i32)) -> (i32, i32) {
        (w.0.min(500), w.1)
    }

    /// Up and right, on past the top edge (the window would go to y 0).
    fn up_right(i: usize, n: usize) -> Pos {
        let k = i as f64 / n as f64;
        (410.0 + 120.0 * k, 280.0 - 200.0 * k)
    }

    /// Right and down, on past the right edge (the window would go to x 700).
    fn right_down(i: usize, n: usize) -> Pos {
        let k = i as f64 / n as f64;
        (410.0 + 400.0 * k, 280.0 + 150.0 * k)
    }

    /// A palette dragged past an edge the desktop holds it at (p1-absdrag-
    /// review F1): until its reply, a move counted as where it was asked to
    /// go, so on the held axis every candidate but the oldest was wrong, the
    /// X pointer swung ever wider (1814 px) and the drag ended up to 478 px
    /// off. The bounds the replies show place it: the drag ends where the
    /// pointer does, every motion close to it.
    #[test]
    fn a_drag_the_desktop_holds_at_an_edge_ends_where_the_pointer_does() {
        for (f, edge) in [
            (up_right as fn(usize, usize) -> Pos, top_edge as Constraint),
            (right_down, right_edge),
        ] {
            for relative in [false, true] {
                let (motion, end) = worst(
                    Device::Path { f, relative },
                    &RATES,
                    Some(edge),
                    20,
                    &MODELS,
                );
                println!(
                    "held at an edge, relative {relative}: worst motion {motion} px, end {end} px"
                );
                assert_eq!(end, 0, "relative {relative}");
                assert!(motion <= 12, "relative {relative}: {motion} px");
            }
        }
    }

    /// Two monitors side by side, the left one taller: the window's bottom
    /// is held at y 500 on the left and at 300 on the right. Dragged right
    /// onto the short one, down past its bottom (held at 300), then left onto
    /// the tall one, where it may go lower: the bound learned on the right
    /// no longer holds, and the first reply that shows it drops it.
    fn monitors(w: (i32, i32)) -> (i32, i32) {
        (w.0, w.1.min(if w.0 < 450 { 500 } else { 300 }))
    }

    fn right_down_left(i: usize, n: usize) -> Pos {
        let k = i as f64 / n as f64 * 3.0;
        if k < 1.0 {
            (410.0 + 200.0 * k, 280.0)
        } else if k < 2.0 {
            (610.0, 280.0 + 200.0 * (k - 1.0))
        } else {
            (610.0 - 300.0 * (k - 2.0), 480.0)
        }
    }

    #[test]
    fn a_bound_another_monitor_lifts_is_dropped() {
        for relative in [false, true] {
            let device = Device::Path {
                f: right_down_left,
                relative,
            };
            let (motion, end) = worst(device, &RATES, Some(monitors), 20, &MODELS);
            println!("two monitors, relative {relative}: worst motion {motion} px, end {end} px");
            assert_eq!(end, 0, "relative {relative}");
            // one frame may be off by the 100 px the window drops by when the
            // tall monitor takes it, until the reply that shows it
            assert!(motion <= 100, "relative {relative}: {motion} px");
        }
    }

    /// Deterministic noise in [0, 1).
    fn noise(i: usize) -> f64 {
        let mut z = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x1234_5678;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn along(k: f64) -> Pos {
        (410.0 + 240.0 * k, 280.0 + 120.0 * k)
    }

    /// Every position twice (a remote pointer whose viewer events map to one
    /// remote pixel; the bridge's own XTest -> EIS path forwards every
    /// motion).
    fn pairs(i: usize, n: usize) -> Pos {
        along((i / 2 * 2) as f64 / n as f64)
    }

    /// A quarter of the events repeat the position before.
    fn repeats(i: usize, n: usize) -> Pos {
        let moves = |upto: usize| (1..=upto).filter(|&j| noise(j) >= 0.25).count() as f64;
        along(moves(i) / moves(n))
    }

    /// Steps of 0.2 to 1.8 times the mean.
    fn uneven(i: usize, n: usize) -> Pos {
        let sum = |upto: usize| (1..=upto).map(|j| 0.2 + 1.6 * noise(j)).sum::<f64>();
        along(sum(i) / sum(n))
    }

    /// Absolute positions that repeat or step unevenly (p1-absdrag-review
    /// F2): the predictions come from the rule's own picks and the X
    /// client's moves follow them, so a wrong pick followed by one wrong the
    /// other way grew without bound (each position twice at 125 Hz: 6232 px,
    /// the drag ending 138 px off). The main account never goes back to an
    /// older origin and gives way to a clearly better one: the error stays
    /// within a few steps.
    #[test]
    fn repeated_and_uneven_positions_stay_close() {
        let rates = [(37, 16.0), (75, 8.0), (150, 4.0)];
        for (name, f) in [
            ("pairs", pairs as fn(usize, usize) -> Pos),
            ("repeats", repeats),
            ("uneven", uneven),
        ] {
            let device = Device::Path { f, relative: false };
            let (motion, end) = worst(device, &rates, None, 20, &MODELS);
            println!("{name}: worst motion {motion} px, end {end} px");
            assert!(motion <= 40, "{name}: {motion} px");
            assert!(end <= 30, "{name}: end {end} px");
        }
    }

    /// Right 120 px, back, and right again, drifting down: a drag that turns
    /// back. At the turn the main account may take a newer origin than the
    /// frame was made with, and its order then keeps it there; an account
    /// that took the older one explains the frames after the turn better,
    /// and the X pointer moves over to it (without: drags ending 42 px off
    /// with the frame clock).
    fn back_and_forth(i: usize, n: usize) -> Pos {
        let k = i as f64 / n as f64 * 3.0;
        let x = if k <= 1.0 {
            k
        } else if k <= 2.0 {
            2.0 - k
        } else {
            k - 2.0
        };
        (410.0 + 120.0 * x, 280.0 + 40.0 * i as f64 / n as f64)
    }

    #[test]
    fn a_drag_that_turns_back_ends_where_the_pointer_does() {
        let rates = [(37, 16.0), (75, 8.0), (150, 4.0)];
        for relative in [false, true] {
            let device = Device::Path {
                f: back_and_forth,
                relative,
            };
            let (motion, end) = worst(device, &rates, None, 20, &MODELS);
            println!("turning back, relative {relative}: worst motion {motion} px, end {end} px");
            assert_eq!(end, 0, "relative {relative}");
            assert!(motion <= 45, "relative {relative}: {motion} px");
        }
    }

    /// A person's drag: slow to start, fastest midway, slowing to a stop
    /// (minimum jerk). A guard that bounds each step by the steps seen at
    /// the start (where only one origin is possible) trips on it later.
    fn person(i: usize, n: usize) -> Pos {
        let t = i as f64 / n as f64;
        along(10.0 * t.powi(3) - 15.0 * t.powi(4) + 6.0 * t.powi(5))
    }

    #[test]
    fn a_person_like_drag_ends_where_the_pointer_does() {
        let rates = [(20, 30.0), (75, 8.0), (150, 4.0)];
        let models = [
            Model::Immediate,
            Model::Frames { compress: false },
            Model::Stalling,
        ];
        for relative in [false, true] {
            let device = Device::Path {
                f: person,
                relative,
            };
            let (motion, end) = worst(device, &rates, None, 20, &models);
            println!(
                "a person's drag, relative {relative}: worst motion {motion} px, end {end} px"
            );
            assert_eq!(end, 0, "relative {relative}");
            assert!(motion <= 10, "relative {relative}: {motion} px");
        }
    }

    /// Beyond the measured drags: other event rates, and drags that slow down
    /// to a stop. Prints what each rule does (`cargo test -- --ignored
    /// --nocapture sweep`); the report quotes it.
    #[test]
    #[ignore]
    fn sweep() {
        let rules = [Rule::Origins, Rule::Newest, Rule::RelativeOnly];
        println!(
            "model | device | Hz | rule | drags with back-steps | back-steps | motions off | end off (drags, max px) | worst motion off (px)"
        );
        for model in MODELS {
            for (name, relative) in [("absolute", false), ("relative", true)] {
                for period in [4.0, 5.0, 8.0, 16.0, 30.0] {
                    for (ease, jitter, how) in [
                        (false, false, "steady"),
                        (true, false, "slowing"),
                        (false, true, "steady, irregular times"),
                        (true, true, "slowing, irregular times"),
                    ] {
                        for rule in rules {
                            let (mut drags_back, mut back, mut off, mut ends, mut worst) =
                                (0, 0, 0, 0, 0);
                            let mut worst_motion = 0;
                            let steps = (480.0 / period) as usize;
                            for seed in 0..50 {
                                let device = match (ease, relative) {
                                    (false, false) => absolute(240.0, 120.0),
                                    (false, true) => Device::Relative {
                                        from: (410.0, 280.0),
                                        step: (
                                            fixed(240.0 / steps as f64),
                                            fixed(120.0 / steps as f64),
                                        ),
                                    },
                                    (true, _) => Device::Easing {
                                        from: (410.0, 280.0),
                                        to: (650.0, 400.0),
                                        relative,
                                    },
                                };
                                let o = run(Run {
                                    model,
                                    device,
                                    steps,
                                    period,
                                    rule,
                                    seed,
                                    jitter,
                                    constrain: None,
                                });
                                let b = o.back_steps();
                                back += b;
                                drags_back += usize::from(b > 0);
                                off += o.path.iter().zip(&o.truth).filter(|(a, b)| a != b).count();
                                worst_motion = worst_motion.max(o.worst_off());
                                let e = (o.x_window_moved.0 - o.pointer_moved.0)
                                    .abs()
                                    .max((o.x_window_moved.1 - o.pointer_moved.1).abs());
                                ends += usize::from(e > 0);
                                worst = worst.max(e);
                            }
                            println!(
                                "{model:?} | {name} {how} | {:.0} | {rule:?} | {drags_back}/50 | {back} | {off} | {ends}/50, {worst} | {worst_motion}",
                                1000.0 / period
                            );
                        }
                    }
                }
            }
        }
    }

    /// The simulation shows what p1-validate measured with the two earlier
    /// rules, so its passes above mean something.
    #[test]
    fn the_simulation_reproduces_the_measured_failures() {
        let run_with = |rule, device, steps, period, model, seed| {
            run(Run {
                model,
                device,
                steps,
                period,
                rule,
                seed,
                jitter: false,
                constrain: None,
            })
        };
        for model in MODELS {
            for seed in 0..10 {
                // def9584, absolute: one motion, then frozen (+5,+2 measured)
                let o = run_with(
                    Rule::RelativeOnly,
                    absolute(120.0, 70.0),
                    24,
                    30.0,
                    model,
                    seed,
                );
                assert!(
                    o.x_window_moved.0 <= 10,
                    "{model:?}: {:?}",
                    o.x_window_moved
                );
                assert_eq!(o.pointer_moved, (120, 70));
                // def9584, relative: the first motion's fraction is lost
                // (+115,+66 for +116,+67 measured)
                let o = run_with(Rule::RelativeOnly, mouse(), 24, 30.0, model, seed);
                assert_eq!(o.pointer_moved, (116, 67));
                assert_eq!(o.x_window_moved, (115, 66), "{model:?}");
                // b743bee, absolute at 33 Hz: exact, as measured, where moves
                // take effect at once (with input processed only at fixed
                // 60 Hz frames it would have erred at 33 Hz too, which the
                // VM did not show: its mutter laid the window out sooner)
                if model == Model::Immediate {
                    let o = run_with(Rule::Newest, absolute(120.0, 70.0), 24, 30.0, model, seed);
                    assert_eq!(o.x_window_moved, (120, 70), "seed {seed}");
                    assert_eq!(o.back_steps(), 0, "seed {seed}");
                }
            }
            // b743bee, the tablet at 250 Hz: back-steps (26 and 23 measured)
            let back: usize = (0..10)
                .map(|seed| {
                    run_with(Rule::Newest, absolute(240.0, 120.0), 120, 4.0, model, seed)
                        .back_steps()
                })
                .sum();
            assert!(back >= 10, "{model:?}: {back} back-steps in 10 drags");
        }
    }
}
