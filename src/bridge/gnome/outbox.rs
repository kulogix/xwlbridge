//! What the bridge sends mutter over EIS, paced so that the socket never holds
//! part of a message and mutter is never handed a flood.
//!
//! `reis` queues every request in an unbounded buffer and writes it all with
//! one `sendmsg` on a non-blocking socket. With nothing waiting for the socket
//! to drain, input injected while mutter wasn't reading (a busy frame, a
//! modeset, a GC pause, or just an X client injecting faster than mutter
//! reads) piled up, and the tail of a burst sat in that buffer until some
//! unrelated input came along (a key's release among it: a stuck key). Worse,
//! the backlog then went out in pieces cut at arbitrary bytes: libeis (1.5.0,
//! GNOME 50) drops an incomplete message at the end of each read and parses
//! the next one from the middle, which lost input, and a misread length of 0
//! sends it round an endless loop inside mutter's main thread: the whole
//! desktop frozen, for good (review_stability S2).
//!
//! So nothing goes to `reis` while it still holds unsent bytes, and what does
//! go is written in batches of whole messages small enough that the kernel
//! takes each `sendmsg` whole or not at all (a Unix stream socket queues one
//! write of up to one skb, at least ~32 KiB, atomically). Meanwhile events wait
//! here, where pointer motion (and wheel notches) coalesce with the previous
//! unsent one, and a hard cap ([`MAX_QUEUE`]) first merges motion and wheel
//! between key and button transitions, then drops whole key and button taps,
//! never a lone press or release, so no key is ever left down. The backend
//! thread flushes on `POLLOUT`, so the tail of a burst never waits for
//! unrelated input.
//!
//! Pointer motion is also paced, whatever follows it, to [`MOTION_INTERVAL`]
//! (after a burst of at most [`MOTION_BURST`] when other input follows it):
//! mutter marks a window "not responding" when more than 400 motion events
//! reach it before it has answered a ping, and then delivers a drag's button
//! release nowhere. Events behind a held motion wait with it, so the order
//! (and every click's position) is kept.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

/// The most pointer motion events per second sent to the compositor (500 Hz,
/// faster than any VNC viewer sends; only floods are paced).
pub const MOTION_INTERVAL: Duration = Duration::from_millis(2);
/// How many motions may go faster than [`MOTION_INTERVAL`] when other input
/// follows them (a click right after a move goes at once), before the
/// interval holds for those too: far under mutter's 400 per ping.
pub const MOTION_BURST: u32 = 16;
/// The most unsent events (frames included) held: past it they are compacted
/// to at most half of it, so this is a hard bound (~2 MB) and compaction
/// runs at most once per `MAX_QUEUE / 2` pushes. Far more than a stall at any
/// human or sane automation rate leaves.
pub const MAX_QUEUE: usize = 65536;
/// The most a merged wheel event scrolls on each axis, in libei's 1/120
/// notch units: 120 notches, pages of scrolling. mutter turns a discrete
/// scroll into one event per notch, synchronously, so a stall's worth of an
/// automated wheel flood merged without a bound (100000 notches seen) floods
/// mutter and the app under the pointer for many seconds; what goes past
/// this is dropped.
pub const MAX_SCROLL: i32 = 120 * 120;
/// The largest message we send: a frame (16-byte header, serial, timestamp).
pub const OP_BYTES: usize = 28;

/// One request for the compositor, on device `D`.
#[derive(Clone, Debug, PartialEq)]
pub enum Op<D> {
    Abs { dev: D, x: f32, y: f32 },
    Rel { dev: D, dx: f32, dy: f32 },
    Button { dev: D, button: u32, press: bool },
    Scroll { dev: D, dx: i32, dy: i32 },
    Key { dev: D, key: u32, press: bool },
    Frame { dev: D },
}

impl<D> Op<D> {
    fn dev(&self) -> &D {
        match self {
            Op::Abs { dev, .. }
            | Op::Rel { dev, .. }
            | Op::Button { dev, .. }
            | Op::Scroll { dev, .. }
            | Op::Key { dev, .. }
            | Op::Frame { dev } => dev,
        }
    }

    fn is_frame(&self) -> bool {
        matches!(self, Op::Frame { .. })
    }

    fn is_motion(&self) -> bool {
        matches!(self, Op::Abs { .. } | Op::Rel { .. })
    }

    /// A key or button transition: (is a key, code, press).
    fn transition(&self) -> Option<(bool, u32, bool)> {
        match *self {
            Op::Key { key, press, .. } => Some((true, key, press)),
            Op::Button { button, press, .. } => Some((false, button, press)),
            _ => None,
        }
    }
}

/// Where ops are written: `reis`, or a test double.
pub trait Sink<D> {
    /// Queues one request (and nothing else) in the sink's buffer.
    fn write(&mut self, op: &Op<D>);
    /// Sends the buffer: all of it, or nothing and `WouldBlock`.
    fn flush(&mut self) -> io::Result<()>;
}

/// The outcome of [`Outbox::pump`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pump {
    /// Everything was sent.
    Idle,
    /// A paced motion waits until then.
    Hold(Instant),
    /// The socket is full: wait for `POLLOUT`.
    Blocked,
    /// The socket is gone.
    Dead,
}

/// Counts for one stretch of the compositor not reading, for the log.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Stall {
    pub since: Option<Instant>,
    /// Motion and wheel events merged into others.
    pub coalesced: u64,
    /// Key and button taps (a press with its release) dropped at the cap.
    pub dropped_taps: u64,
    pub warned: bool,
}

pub struct Outbox<D> {
    queue: VecDeque<Op<D>>,
    /// The sink holds bytes it couldn't send: nothing more goes in until a
    /// flush gets them out.
    blocked: bool,
    /// Since when the socket has been full without a break.
    blocked_since: Option<Instant>,
    dead: bool,
    /// Bytes per batch: one `sendmsg` the kernel takes whole.
    max_batch: usize,
    /// Motion pacing (GCRA): when the motion budget is full again; each motion
    /// sent moves it [`MOTION_INTERVAL`] on from now (or from itself, if
    /// later).
    motion_tat: Option<Instant>,
    hold_until: Option<Instant>,
    stall: Stall,
    /// How often [`compact`](Self::compact) ran.
    #[cfg(test)]
    compactions: usize,
}

impl<D: Clone + PartialEq> Outbox<D> {
    pub fn new(max_batch: usize) -> Self {
        Outbox {
            queue: VecDeque::new(),
            blocked: false,
            blocked_since: None,
            dead: false,
            max_batch: max_batch.max(OP_BYTES * 8),
            motion_tat: None,
            hold_until: None,
            stall: Stall::default(),
            #[cfg(test)]
            compactions: 0,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn blocked(&self) -> bool {
        self.blocked
    }

    /// Since when the compositor hasn't taken anything (the socket has been
    /// full throughout), if it isn't now. A stall's counts run on while its
    /// backlog drains at the paced rate; this is only the not reading.
    pub fn blocked_since(&self) -> Option<Instant> {
        self.blocked_since
    }

    /// When the backend thread must pump next (a paced motion is held).
    pub fn deadline(&self) -> Option<Instant> {
        if self.blocked { None } else { self.hold_until }
    }

    pub fn stall(&mut self) -> &mut Stall {
        &mut self.stall
    }

    /// Adds an op, coalescing it into the last unsent event where that loses
    /// nothing a user would notice: a motion into the previous motion of its
    /// device, a wheel notch into the previous notches, a frame into the
    /// frame already closing the device's events.
    pub fn push(&mut self, op: Op<D>) {
        if self.dead {
            return;
        }
        if self.coalesce(&op) {
            return;
        }
        self.queue.push_back(op);
        if self.queue.len() > MAX_QUEUE {
            self.compact();
        }
    }

    fn coalesce(&mut self, op: &Op<D>) -> bool {
        if let Op::Frame { dev } = op {
            // a frame right after a frame of the same device adds nothing
            return matches!(self.queue.back(), Some(Op::Frame { dev: d }) if d == dev);
        }
        // the last unsent event, with nothing but frames after it
        let Some(i) = self.queue.iter().rposition(|o| !o.is_frame()) else {
            return false;
        };
        // a motion held for pacing that has nothing after it but frames is
        // still unsent, so it may coalesce too
        let merged = match (&mut self.queue[i], op) {
            (Op::Abs { dev: d, x: px, y: py }, Op::Abs { dev, x, y }) if d == dev => {
                *px = *x;
                *py = *y;
                true
            }
            (Op::Rel { dev: d, dx: pdx, dy: pdy }, Op::Rel { dev, dx, dy }) if d == dev => {
                *pdx += dx;
                *pdy += dy;
                true
            }
            (Op::Scroll { dev: d, dx: pdx, dy: pdy }, Op::Scroll { dev, dx, dy }) if d == dev => {
                *pdx = add_scroll(*pdx, *dx);
                *pdy = add_scroll(*pdy, *dy);
                true
            }
            _ => false,
        };
        if merged {
            self.stall.coalesced += 1;
        }
        merged
    }

    /// Makes room when the compositor has stopped reading for long (or input
    /// comes far faster than paced motion lets it go), down to at most half
    /// of [`MAX_QUEUE`], in steps that lose ever more:
    /// 1. motion and wheel between two key/button transitions merge into one
    ///    move (the last position, the sum of relative motion) and one wheel
    ///    event (the sum) per device: every press and release keeps its place
    ///    and position, only the path between them goes;
    /// 2. every key or button tap (a press and its release: a keystroke, a
    ///    click, a whole drag) still unsent goes, and step 1 runs again on
    ///    what that leaves;
    /// 3. motion and wheel merge across the transitions too, at the end.
    ///
    /// Lone presses and releases always stay, so whatever is down ends up
    /// down and whatever is up ends up up. At most ~2 transitions per key or
    /// button survive step 2, so step 3 always gets under the target.
    fn compact(&mut self) {
        #[cfg(test)]
        {
            self.compactions += 1;
        }
        // an episode for the log, as a stall is: compaction also happens with
        // the compositor reading, behind paced motion, and what it drops must
        // be reported then too
        if self.stall.since.is_none() {
            self.stall = Stall {
                since: Some(Instant::now()),
                ..Stall::default()
            };
        }
        let target = MAX_QUEUE / 2;
        // every event is followed by its device's frame (Input frames each
        // one), so work on the events and frame each survivor again
        let mut events: Vec<Op<D>> = self.queue.drain(..).filter(|o| !o.is_frame()).collect();
        self.merge_motion(&mut events, false);
        if events.len() * 2 > target {
            self.drop_taps(&mut events);
            self.merge_motion(&mut events, false);
        }
        if events.len() * 2 > target {
            self.merge_motion(&mut events, true);
        }
        for op in events {
            let dev = op.dev().clone();
            self.queue.push_back(op);
            self.queue.push_back(Op::Frame { dev });
        }
    }

    /// Drops every key or button tap: a press and the release that ends it.
    fn drop_taps(&mut self, events: &mut Vec<Op<D>>) {
        let mut keep = vec![true; events.len()];
        // unmatched presses so far: (device, is a key, code, index)
        let mut open: Vec<(D, bool, u32, usize)> = Vec::new();
        for (i, op) in events.iter().enumerate() {
            let Some((key, code, press)) = op.transition() else {
                continue;
            };
            let found = open
                .iter()
                .position(|(d, k, c, _)| d == op.dev() && *k == key && *c == code);
            match (press, found) {
                (true, None) => open.push((op.dev().clone(), key, code, i)),
                (false, Some(n)) => {
                    let (.., j) = open.swap_remove(n);
                    keep[j] = false;
                    keep[i] = false;
                    self.stall.dropped_taps += 1;
                }
                // (a press after a press can't come from Input; keep it)
                _ => {}
            }
        }
        let mut k = keep.into_iter();
        events.retain(|_| k.next().unwrap_or(true));
    }

    /// Merges the motion and wheel events of each device into one move and
    /// one wheel event per stretch between key/button transitions (or, with
    /// `across`, over the whole queue, placed at its end), each where that
    /// device's first one was.
    fn merge_motion(&mut self, events: &mut Vec<Op<D>>, across: bool) {
        let mut out: Vec<Op<D>> = Vec::with_capacity(events.len());
        // indexes into `out` of the current stretch's move and wheel event
        // per device: (device, move, wheel)
        let mut slots: Vec<(D, Option<usize>, Option<usize>)> = Vec::new();
        let mut tail: Vec<Op<D>> = Vec::new();
        for op in events.drain(..) {
            if op.transition().is_some() {
                out.push(op);
                if !across {
                    slots.clear();
                }
                continue;
            }
            let into = if across { &mut tail } else { &mut out };
            let slot = match slots.iter().position(|(d, ..)| d == op.dev()) {
                Some(n) => n,
                None => {
                    slots.push((op.dev().clone(), None, None));
                    slots.len() - 1
                }
            };
            let (_, mv, wheel) = &mut slots[slot];
            let at = if matches!(op, Op::Scroll { .. }) { wheel } else { mv };
            let merged = match at.map(|i| (&mut into[i], &op)) {
                Some((Op::Abs { x: px, y: py, .. }, Op::Abs { x, y, .. })) => {
                    *px = *x;
                    *py = *y;
                    true
                }
                Some((Op::Rel { dx: pdx, dy: pdy, .. }, Op::Rel { dx, dy, .. })) => {
                    *pdx += dx;
                    *pdy += dy;
                    true
                }
                Some((Op::Scroll { dx: pdx, dy: pdy, .. }, Op::Scroll { dx, dy, .. })) => {
                    *pdx = add_scroll(*pdx, *dx);
                    *pdy = add_scroll(*pdy, *dy);
                    true
                }
                _ => false,
            };
            if merged {
                self.stall.coalesced += 1;
            } else {
                *at = Some(into.len());
                into.push(op);
            }
        }
        out.append(&mut tail);
        *events = out;
    }

    /// Forgets queued ops for devices that went away (mutter released what
    /// they held itself).
    pub fn retain_devices(&mut self, mut alive: impl FnMut(&D) -> bool) {
        self.queue.retain(|op| alive(op.dev()));
    }

    /// Sends what can go now, in whole-message batches of at most
    /// `max_batch` bytes, each flushed before the next.
    pub fn pump(&mut self, sink: &mut impl Sink<D>, now: Instant) -> Pump {
        if self.dead {
            return Pump::Dead;
        }
        // first what the sink already holds: a batch that didn't fit before,
        // or requests the backend thread made itself (handshake, binding,
        // start_emulating, pongs), which go out even with nothing queued here
        match sink.flush() {
            Ok(()) => {
                self.blocked = false;
                self.blocked_since = None;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.block(now);
                return Pump::Blocked;
            }
            Err(_) => return self.die(),
        }
        self.hold_until = None;
        loop {
            let mut bytes = 0;
            let mut wrote = false;
            while let Some(op) = self.queue.front() {
                if bytes + OP_BYTES > self.max_batch {
                    break;
                }
                // motion is paced whatever follows it, and what follows waits
                // with it (order, and so every click's position, is kept)
                if op.is_motion()
                    && let Some(at) = self.motion_due(now)
                {
                    self.hold_until = Some(at);
                    break;
                }
                let op = self.queue.pop_front().expect("front exists");
                if op.is_motion() {
                    self.motion_tat = Some(self.motion_tat.map_or(now, |t| t.max(now)) + MOTION_INTERVAL);
                }
                sink.write(&op);
                bytes += OP_BYTES;
                wrote = true;
            }
            if wrote {
                match sink.flush() {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.block(now);
                        return Pump::Blocked;
                    }
                    Err(_) => return self.die(),
                }
            }
            if !wrote || self.hold_until.is_some() || self.queue.is_empty() {
                break;
            }
        }
        match self.hold_until {
            Some(t) => Pump::Hold(t),
            None => Pump::Idle,
        }
    }

    /// When the motion at the front may go, if not now. One with nothing but
    /// frames behind it waits a whole interval after the last one (meanwhile
    /// later motion merges into it); one that other input follows (a click
    /// after a move) may go sooner, within a burst of [`MOTION_BURST`]: so
    /// floods of any shape reach the compositor at no more than 500 Hz.
    fn motion_due(&self, now: Instant) -> Option<Instant> {
        let tat = self.motion_tat?;
        let followed = self.queue.iter().skip(1).any(|o| !o.is_frame());
        let slack = if followed { MOTION_INTERVAL * (MOTION_BURST - 1) } else { Duration::ZERO };
        let at = tat.checked_sub(slack).unwrap_or(now);
        (now < at).then_some(at)
    }

    /// The socket is full: a stall starts (its counts from zero: merges made
    /// while merely pacing motion aren't part of it) unless one is on.
    fn block(&mut self, now: Instant) {
        self.blocked = true;
        self.blocked_since.get_or_insert(now);
        if self.stall.since.is_none() {
            self.stall = Stall {
                since: Some(now),
                ..Stall::default()
            };
        }
    }

    fn die(&mut self) -> Pump {
        self.dead = true;
        self.queue.clear();
        Pump::Dead
    }

    /// Ends a stall episode once everything is out, returning its counts.
    pub fn take_stall_if_drained(&mut self) -> Option<Stall> {
        if self.blocked || !self.queue.is_empty() || self.stall.since.is_none() {
            return None;
        }
        Some(std::mem::take(&mut self.stall))
    }
}

/// Two wheel amounts merged, within [`MAX_SCROLL`].
fn add_scroll(a: i32, b: i32) -> i32 {
    a.saturating_add(b).clamp(-MAX_SCROLL, MAX_SCROLL)
}

/// The batch size for a socket whose kernel send buffer is `sndbuf` bytes
/// (`SO_SNDBUF` as read back): the kernel queues one `sendmsg` of up to
/// `sndbuf / 2 - 64` bytes (and at least ~32 KiB) as one unit, all or nothing,
/// so stay well under that, with room for the requests the backend thread
/// adds itself (handshake, pongs).
pub fn batch_for(sndbuf: usize) -> usize {
    (sndbuf / 2).saturating_sub(64 + 1024).clamp(OP_BYTES * 8, 4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that takes `room` more batches, then reports a full socket.
    #[derive(Default)]
    struct FakeSink {
        pending: Vec<Op<u8>>,
        sent: Vec<Vec<Op<u8>>>,
        room: usize,
    }

    impl Sink<u8> for FakeSink {
        fn write(&mut self, op: &Op<u8>) {
            self.pending.push(op.clone());
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.pending.is_empty() {
                return Ok(());
            }
            if self.room == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.room -= 1;
            self.sent.push(std::mem::take(&mut self.pending));
            Ok(())
        }
    }

    impl FakeSink {
        fn all(&self) -> Vec<Op<u8>> {
            self.sent.iter().flatten().cloned().collect()
        }
    }

    fn abs(x: f32) -> Op<u8> {
        Op::Abs { dev: 1, x, y: x }
    }
    fn key(k: u32, press: bool) -> Op<u8> {
        Op::Key { dev: 2, key: k, press }
    }
    fn btn(b: u32, press: bool) -> Op<u8> {
        Op::Button { dev: 1, button: b, press }
    }
    fn frame(dev: u8) -> Op<u8> {
        Op::Frame { dev }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    /// Pumps at each pacing deadline until everything is out; returns when.
    fn drain(o: &mut Outbox<u8>, s: &mut FakeSink, mut now: Instant) -> Instant {
        for _ in 0..1_000_000 {
            match o.pump(s, now) {
                Pump::Hold(t) => now = t,
                Pump::Idle => return now,
                p => panic!("{p:?}"),
            }
        }
        panic!("never drained");
    }

    #[test]
    fn flowing_input_goes_out_at_once_and_unchanged() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink { room: 100, ..Default::default() };
        let now = t0();
        o.push(key(30, true));
        o.push(frame(2));
        assert_eq!(o.pump(&mut s, now), Pump::Idle);
        o.push(key(30, false));
        o.push(frame(2));
        assert_eq!(o.pump(&mut s, now), Pump::Idle);
        assert_eq!(s.sent.len(), 2);
        assert_eq!(s.all(), vec![key(30, true), frame(2), key(30, false), frame(2)]);
    }

    /// S2: the tail of a burst must not wait for unrelated input once the
    /// socket drains, and nothing piles up beyond one batch in the sink.
    #[test]
    fn s2_backlog_waits_in_the_outbox_and_goes_out_on_pollout() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink { room: 1, ..Default::default() };
        let mut now = t0();
        // mutter stops reading: the first event fills the "socket"
        for i in 0..5000 {
            now += Duration::from_millis(1);
            o.push(abs(i as f32));
            o.push(frame(1));
            o.pump(&mut s, now);
        }
        o.push(key(45, true));
        o.push(frame(2));
        o.push(key(45, false));
        o.push(frame(2));
        assert_eq!(o.pump(&mut s, now), Pump::Blocked);
        // whatever the sink holds unsent is one batch at most
        assert!(s.pending.len() * OP_BYTES <= 4096, "{}", s.pending.len());
        // 5000 motions coalesced to one, plus the key's press and release
        assert!(o.len() <= 6, "{}", o.len());
        // the socket drains (POLLOUT): everything goes, the key included
        s.room = 100;
        now += Duration::from_millis(10);
        assert_eq!(o.pump(&mut s, now), Pump::Idle);
        let all = s.all();
        let keys: Vec<_> = all.iter().filter(|o| matches!(o, Op::Key { .. })).collect();
        assert_eq!(keys, vec![&key(45, true), &key(45, false)]);
        assert!(all.iter().filter(|o| o.is_motion()).count() <= 3);
        assert_eq!(all.iter().rev().find(|o| o.is_motion()), Some(&abs(4999.0)));
        let stall = o.take_stall_if_drained().expect("a stall was recorded");
        assert!(stall.coalesced >= 4990, "{stall:?}");
    }

    /// The order of events survives coalescing: a click lands where the
    /// pointer was when it was pressed.
    #[test]
    fn motion_never_coalesces_across_a_button() {
        let mut o = Outbox::new(4096);
        let now = t0();
        for op in [
            abs(1.0), frame(1), abs(2.0), frame(1), btn(272, true), frame(1),
            abs(3.0), frame(1), abs(4.0), frame(1), btn(272, false), frame(1),
        ] {
            o.push(op);
        }
        let mut s = FakeSink { room: 10, ..Default::default() };
        o.pump(&mut s, now);
        assert_eq!(
            s.all(),
            vec![abs(2.0), frame(1), btn(272, true), frame(1), abs(4.0), frame(1), btn(272, false), frame(1)]
        );
    }

    /// A hard cap: taps go, lone transitions stay, so nothing is left down.
    #[test]
    fn s2_overflow_drops_whole_taps_only() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink::default(); // never drains
        let now = t0();
        o.push(key(29, true)); // Ctrl down, never released while stalled
        o.push(frame(2));
        o.pump(&mut s, now);
        for i in 0..40_000u32 {
            o.push(key(30 + i % 10, true));
            o.push(frame(2));
            o.push(key(30 + i % 10, false));
            o.push(frame(2));
            o.push(abs(i as f32));
            o.push(frame(1));
        }
        o.push(key(42, true)); // Shift down at the end
        o.push(frame(2));
        assert!(o.len() <= MAX_QUEUE, "{}", o.len());
        // the socket drains: Ctrl (sent before the stall) first, then the rest
        // (paced motion holds what follows it: pump at its deadlines, as the
        // backend thread does)
        s.room = usize::MAX;
        drain(&mut o, &mut s, now);
        let keys: Vec<_> = s.all().into_iter().filter(|o| matches!(o, Op::Key { .. })).collect();
        // every press that went out was released, except Ctrl and Shift
        let mut down = std::collections::HashSet::new();
        for k in &keys {
            if let Op::Key { key, press, .. } = k {
                if *press {
                    assert!(down.insert(*key), "double press of {key}");
                } else {
                    assert!(down.remove(key), "release of {key} not down");
                }
            }
        }
        assert_eq!(down, std::collections::HashSet::from([29, 42]));
        assert!(o.len() == 0);
        assert!(o.stall().dropped_taps > 10_000, "{:?}", o.stall());
    }

    #[test]
    fn s2_motion_is_paced() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink { room: 100_000, ..Default::default() };
        let start = t0();
        let mut held = None;
        // a flood: 1000 motions within 1 ms of each other
        for i in 0..1000u32 {
            let now = start + Duration::from_micros(i as u64);
            o.push(abs(i as f32));
            o.push(frame(1));
            if let Pump::Hold(t) = o.pump(&mut s, now) {
                held = Some(t);
            }
        }
        let sent = s.all().iter().filter(|o| o.is_motion()).count();
        assert_eq!(sent, 1, "only the first goes before the interval");
        let t = held.expect("the last one is held");
        assert_eq!(o.deadline(), Some(t));
        assert_eq!(o.pump(&mut s, t), Pump::Idle);
        assert_eq!(s.all().iter().rev().find(|o| o.is_motion()), Some(&abs(999.0)));
        // a click releases a held motion at once (and keeps it first)
        let now = t + Duration::from_micros(10);
        o.push(abs(5.0));
        o.push(frame(1));
        assert!(matches!(o.pump(&mut s, now), Pump::Hold(_)));
        o.push(btn(272, true));
        o.push(frame(1));
        assert_eq!(o.pump(&mut s, now), Pump::Idle);
        let tail: Vec<_> = s.all().into_iter().rev().take(4).collect();
        assert_eq!(tail, vec![frame(1), btn(272, true), frame(1), abs(5.0)]);
    }

    /// A stall's report counts only what happened during it.
    #[test]
    fn a_stall_counts_from_its_start() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink { room: 100, ..Default::default() };
        let start = t0();
        for i in 0..50u32 {
            o.push(abs(i as f32)); // paced: merged into the held one
            o.push(frame(1));
            o.pump(&mut s, start + Duration::from_micros(i as u64));
        }
        s.room = 0;
        o.push(btn(272, true));
        o.push(frame(1));
        assert_eq!(o.pump(&mut s, start + Duration::from_millis(5)), Pump::Blocked);
        for i in 0..10u32 {
            o.push(abs(i as f32));
            o.push(frame(1));
        }
        s.room = 100;
        o.pump(&mut s, start + Duration::from_millis(10));
        let st = o.take_stall_if_drained().expect("a stall");
        assert_eq!(st.coalesced, 9, "{st:?}");
    }

    /// The "hasn't read input" warning is about the socket staying full, not
    /// about a backlog draining at the paced rate after it.
    #[test]
    fn not_reading_is_only_while_the_socket_stays_full() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink { room: 0, ..Default::default() };
        let start = t0();
        o.push(key(1, true));
        o.push(frame(2));
        assert_eq!(o.pump(&mut s, start), Pump::Blocked);
        assert_eq!(o.blocked_since(), Some(start));
        // still full later: still the same stretch
        assert_eq!(o.pump(&mut s, start + Duration::from_secs(1)), Pump::Blocked);
        assert_eq!(o.blocked_since(), Some(start));
        for i in 0..40u32 {
            o.push(abs(i as f32));
            o.push(scroll(1));
        }
        // the compositor reads again: the backlog drains paced, the stall's
        // counts go on, but nothing is "not read" any more
        s.room = usize::MAX;
        assert!(matches!(o.pump(&mut s, start + Duration::from_secs(2)), Pump::Hold(_)));
        assert_eq!(o.blocked_since(), None);
        assert!(o.stall().since.is_some());
    }

    /// A wheel flood merged while held back scrolls at most MAX_SCROLL per
    /// merged event (mutter emits one event per notch), both ways.
    #[test]
    fn merged_wheel_is_capped() {
        let mut o = Outbox::new(4096);
        blocked_flood(&mut o, &[scroll(120), frame(1)], 100_000);
        blocked_flood(&mut o, &[abs(1.0), frame(1)], 1);
        blocked_flood(&mut o, &[scroll(-120), frame(1)], 100_000);
        let wheel: Vec<i32> = o.queue.iter().filter_map(|o| if let Op::Scroll { dy, .. } = o { Some(*dy) } else { None }).collect();
        assert_eq!(wheel, vec![MAX_SCROLL, -MAX_SCROLL]);
    }

    #[test]
    fn batches_respect_the_size_limit() {
        let mut o = Outbox::new(1024);
        let mut s = FakeSink { room: 1000, ..Default::default() };
        let now = t0();
        for i in 0..500u32 {
            o.push(key(i % 200, i % 2 == 0));
            o.push(frame(2));
        }
        // (pushed while "blocked" so they queue)
        o.blocked = true;
        o.pump(&mut s, now);
        assert!(s.sent.iter().all(|b| b.len() * OP_BYTES <= 1024));
        assert_eq!(s.all().len(), 1000);
    }

    /// What the sink already holds goes out even when nothing is queued here
    /// (the backend thread's own requests: without them no device ever came).
    #[test]
    fn the_sinks_own_requests_are_flushed_with_nothing_queued() {
        let mut o: Outbox<u8> = Outbox::new(4096);
        let mut s = FakeSink { room: 1, ..Default::default() };
        s.pending.push(frame(9)); // stands for start_emulating
        assert_eq!(o.pump(&mut s, t0()), Pump::Idle);
        assert_eq!(s.all(), vec![frame(9)]);
        // and a full socket then is a stall like any other
        s.pending.push(frame(9));
        assert_eq!(o.pump(&mut s, t0()), Pump::Blocked);
        assert!(o.blocked());
    }

    #[test]
    fn a_dead_socket_drops_everything() {
        struct Broken;
        impl Sink<u8> for Broken {
            fn write(&mut self, _: &Op<u8>) {}
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let mut o = Outbox::new(4096);
        o.push(key(1, true));
        assert_eq!(o.pump(&mut Broken, t0()), Pump::Dead);
        o.push(key(1, false));
        assert_eq!(o.len(), 0);
    }

    #[test]
    fn removed_devices_lose_their_ops() {
        let mut o: Outbox<u8> = Outbox::new(4096);
        o.blocked = true;
        o.push(abs(1.0));
        o.push(frame(1));
        o.push(key(1, true));
        o.push(frame(2));
        o.retain_devices(|d| *d == 2);
        assert_eq!(o.len(), 2);
    }

    /// A sink that takes everything and notes when each motion went.
    struct Timed {
        now: Instant,
        ops: Vec<(Instant, Op<u8>)>,
    }

    impl Sink<u8> for Timed {
        fn write(&mut self, op: &Op<u8>) {
            self.ops.push((self.now, op.clone()));
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn scroll(dy: i32) -> Op<u8> {
        Op::Scroll { dev: 1, dx: 0, dy }
    }

    /// Feeds `rounds` of `pattern` at 10 kHz (a pump after each event, as
    /// Input does), then drains at the pacing deadlines; returns what was sent
    /// and when.
    fn timed_flood(pattern: &[Op<u8>], rounds: u32) -> (Instant, Vec<(Instant, Op<u8>)>) {
        let mut o = Outbox::new(4096);
        let start = t0();
        let mut s = Timed { now: start, ops: Vec::new() };
        for i in 0..rounds {
            for op in pattern {
                let mut op = op.clone();
                if let Op::Abs { x, .. } = &mut op {
                    *x = i as f32;
                }
                let dev = *op.dev();
                o.push(op);
                o.push(frame(dev));
                s.now = start + Duration::from_micros(100 * u64::from(i));
                let now = s.now;
                o.pump(&mut s, now);
            }
        }
        for _ in 0..1_000_000 {
            let now = s.now;
            match o.pump(&mut s, now) {
                Pump::Hold(t) => s.now = t,
                Pump::Idle => break,
                p => panic!("{p:?}"),
            }
        }
        assert_eq!(o.len(), 0);
        (start, s.ops)
    }

    /// Pacing held only a motion with nothing but
    /// frames behind it, so motion interleaved with wheel notches or keys
    /// went out at their rate (1000 in 100 ms). Now no 400 motions ever reach
    /// the compositor within less than (400 - burst) intervals, whatever the
    /// flood's shape, and everything still arrives, in order.
    #[test]
    fn r3_motion_is_paced_whatever_follows() {
        for (name, other) in [
            ("motion only", None),
            ("motion+wheel", Some(scroll(120))),
            ("motion+key", Some(key(30, true))),
            ("motion+button", Some(btn(272, true))),
        ] {
            let mut pattern = vec![abs(0.0)];
            if let Some(op) = &other {
                pattern.push(op.clone());
                if let Some(rel) = match op {
                    Op::Key { .. } => Some(key(30, false)),
                    Op::Button { .. } => Some(btn(272, false)),
                    _ => None,
                } {
                    pattern.push(rel);
                }
            }
            let (start, sent) = timed_flood(&pattern, 1000);
            let motions: Vec<Instant> = sent.iter().filter(|(_, o)| o.is_motion()).map(|(t, _)| *t).collect();
            let early = motions.iter().filter(|t| **t < start + Duration::from_millis(100)).count();
            let bound = MOTION_BURST as usize + 100 / 2 + 1;
            assert!(early <= bound, "{name}: {early} motions in the first 100 ms (bound {bound})");
            for w in motions.windows(400) {
                let span = w[399] - w[0];
                assert!(
                    span >= MOTION_INTERVAL * (400 - MOTION_BURST),
                    "{name}: 400 motions within {span:?}"
                );
            }
            // nothing else was lost or reordered: every non-motion event
            // arrives, each after the motion that came before it
            let others: Vec<&Op<u8>> = sent.iter().map(|(_, o)| o).filter(|o| !o.is_motion() && !o.is_frame()).collect();
            assert_eq!(others.len(), 1000 * (pattern.len() - 1), "{name}");
            let mut last_x = -1.0;
            for (_, op) in &sent {
                if let Op::Abs { x, .. } = op {
                    assert!(*x > last_x, "{name}: motion out of order");
                    last_x = *x;
                }
            }
            assert_eq!(last_x, 999.0, "{name}: the last position");
            if other.is_some() {
                // interleaved: no motion could merge, all 1000 went
                assert_eq!(motions.len(), 1000, "{name}");
            }
        }
    }

    /// R3: a drag whose motion is interleaved with wheel notches (the e2e
    /// case that lost the release) ends with its release, at its position.
    #[test]
    fn r3_an_interleaved_drag_keeps_its_release_last() {
        let mut pattern = vec![abs(0.0), scroll(120)];
        let mut o = Outbox::new(4096);
        let now = t0();
        let mut s = Timed { now, ops: Vec::new() };
        o.push(btn(272, true));
        o.push(frame(1));
        for i in 0..2000u32 {
            pattern[0] = abs(i as f32);
            for op in &pattern {
                o.push(op.clone());
                o.push(frame(1));
                o.pump(&mut s, now);
            }
        }
        o.push(btn(272, false));
        o.push(frame(1));
        loop {
            let now = s.now;
            let Pump::Hold(t) = o.pump(&mut s, now) else { break };
            s.now = t;
        }
        let ops: Vec<&Op<u8>> = s.ops.iter().map(|(_, o)| o).filter(|o| !o.is_frame()).collect();
        assert_eq!(ops.first(), Some(&&btn(272, true)));
        assert_eq!(ops.last(), Some(&&btn(272, false)));
        assert_eq!(ops[ops.len() - 3], &abs(1999.0));
        // ~2000 motions at 500 Hz
        assert!(s.now - now >= MOTION_INTERVAL * (2000 - MOTION_BURST), "{:?}", s.now - now);
    }

    /// Blocks an outbox (the compositor isn't reading) and floods it.
    fn blocked_flood(o: &mut Outbox<u8>, pattern: &[Op<u8>], rounds: usize) -> usize {
        struct Never;
        impl Sink<u8> for Never {
            fn write(&mut self, _: &Op<u8>) {}
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        if !o.blocked() {
            o.push(key(1, true));
            assert_eq!(o.pump(&mut Never, t0()), Pump::Blocked);
        }
        let mut max = 0;
        for _ in 0..rounds {
            for op in pattern {
                o.push(op.clone());
                max = max.max(o.len());
            }
        }
        max
    }

    /// Only taps were dropped, so motion on two
    /// absolute devices (a pointer crossing a monitor boundary) or motion
    /// alternating with wheel notches grew the queue without bound, and
    /// every push past the cap reran a compaction that freed nothing
    /// (160000 pushes: 36 s). The cap is hard now, and compaction runs once
    /// per MAX_QUEUE / 2 pushes at most.
    #[test]
    fn r4_the_queue_cap_is_hard_and_compaction_amortized() {
        let alt_abs = [abs(1.0), frame(1), Op::Abs { dev: 3, x: 1.0, y: 1.0 }, frame(3)];
        let abs_wheel = [abs(1.0), frame(1), scroll(120), frame(1)];
        let key_taps = [key(30, true), frame(2), key(30, false), frame(2), abs(1.0), frame(1)];
        for (name, pattern) in [("alt-abs", &alt_abs[..]), ("abs+wheel", &abs_wheel[..]), ("key taps", &key_taps[..])] {
            let mut o = Outbox::new(4096);
            let rounds = 400_000 / pattern.len();
            let started = Instant::now();
            let max = blocked_flood(&mut o, pattern, rounds);
            let took = started.elapsed();
            let pushes = rounds * pattern.len();
            assert!(max <= MAX_QUEUE, "{name}: queue reached {max}");
            assert!(o.compactions <= pushes / (MAX_QUEUE / 2) + 1, "{name}: {} compactions", o.compactions);
            // (36 s for 160000 pushes before; generous for a loaded host)
            assert!(took < Duration::from_secs(20), "{name}: {took:?}");
        }
    }

    /// R4: a motion and wheel flood loses only the path between transitions:
    /// a drag queued before it keeps its press and release, each at its own
    /// position, and the wheel merges too (within its cap).
    #[test]
    fn r4_a_motion_flood_keeps_every_click_where_it_was() {
        let mut o = Outbox::new(4096);
        blocked_flood(&mut o, &[], 0);
        for op in [abs(10.0), frame(1), btn(272, true), frame(1), abs(500.0), frame(1), btn(272, false), frame(1)] {
            o.push(op);
        }
        blocked_flood(&mut o, &[abs(7.0), frame(1), scroll(120), frame(1), abs(8.0), frame(1)], 50_000);
        assert!(o.compactions > 0);
        let ops: Vec<Op<u8>> = o.queue.iter().filter(|o| !o.is_frame()).cloned().collect();
        let press = ops.iter().position(|o| *o == btn(272, true)).expect("press kept");
        let release = ops.iter().position(|o| *o == btn(272, false)).expect("release kept");
        assert_eq!(ops[press - 1], abs(10.0));
        assert_eq!(ops[release - 1], abs(500.0));
        let wheel: Vec<i32> = ops.iter().filter_map(|o| if let Op::Scroll { dy, .. } = o { Some(*dy) } else { None }).collect();
        // the wheel merged too, up to its cap
        assert!(!wheel.is_empty() && wheel.iter().all(|dy| *dy > 0 && *dy <= MAX_SCROLL), "{wheel:?}");
        assert_eq!(ops.iter().rev().find(|o| o.is_motion()), Some(&abs(8.0)));
        assert_eq!(o.stall().dropped_taps, 0);
    }

    /// R4: when even taps have to go, a press with no release (a key still
    /// held) stays, and so does a release with no press, wherever they are.
    #[test]
    fn r4_lone_transitions_survive_every_step() {
        let mut o = Outbox::new(4096);
        blocked_flood(&mut o, &[], 0);
        o.push(key(30, false)); // released: its press went out before the stall
        o.push(frame(2));
        let mut pattern = Vec::new();
        for k in 40..60u32 {
            pattern.extend([key(k, true), frame(2), key(k, false), frame(2), abs(k as f32), frame(1), scroll(1), frame(1)]);
        }
        blocked_flood(&mut o, &pattern, 2_000);
        o.push(key(29, true)); // held
        o.push(frame(2));
        assert!(o.len() <= MAX_QUEUE);
        let keys: Vec<Op<u8>> = o.queue.iter().filter(|o| matches!(o, Op::Key { key, .. } if *key == 29 || *key == 30)).cloned().collect();
        assert_eq!(keys, vec![key(30, false), key(29, true)]);
        assert!(o.stall().dropped_taps > 0);
    }

    /// F3: taps compaction drops while the compositor reads (a paced
    /// backlog, never a full socket) are reported once the queue drains.
    #[test]
    fn taps_dropped_behind_paced_motion_are_reported() {
        let mut o = Outbox::new(4096);
        let mut s = FakeSink { room: usize::MAX, ..Default::default() };
        let mut now = t0();
        for i in 0..40_000u32 {
            for op in [abs(i as f32), frame(1), key(42, true), frame(2), key(42, false), frame(2)] {
                o.push(op);
            }
            if let Pump::Hold(t) = o.pump(&mut s, now) {
                // the compositor reads, but motion is paced: time barely moves
                now = now.max(t - MOTION_INTERVAL * 100);
            }
        }
        assert!(!o.blocked());
        assert!(o.stall().dropped_taps > 0, "{:?}", o.stall());
        assert_eq!(o.take_stall_if_drained(), None, "not drained yet");
        drain(&mut o, &mut s, now);
        let st = o.take_stall_if_drained().expect("the episode is reported");
        assert!(st.since.is_some() && st.dropped_taps > 0, "{st:?}");
        assert_eq!(o.take_stall_if_drained(), None, "once");
    }

    #[test]
    fn batch_size_follows_the_send_buffer() {
        assert_eq!(batch_for(212_992), 4096);
        assert_eq!(batch_for(65_536), 4096);
        assert_eq!(batch_for(4608), 1216);
        assert_eq!(batch_for(0), OP_BYTES * 8);
    }
}

/// The same, over a real Unix socket, against a reader that does what
/// libeis 1.5.0 does: read 1 KiB chunks while they come full, parse the whole
/// messages, and throw away an incomplete one at the end.
#[cfg(test)]
mod socket_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// Writes ops as reis does: messages appended to a byte buffer, sent with
    /// one `sendmsg` of the whole buffer (both ring halves), drained by what
    /// the kernel took. Events are 24 bytes and frames 28, as on the wire.
    struct SockSink {
        fd: OwnedFd,
        buf: VecDeque<u8>,
    }

    fn encode(op: &Op<u8>) -> Vec<u8> {
        let (opcode, a, b, len) = match *op {
            Op::Abs { x, y, .. } => (1u32, x.to_bits(), y.to_bits(), 24u32),
            Op::Rel { dx, dy, .. } => (2, dx.to_bits(), dy.to_bits(), 24),
            Op::Button { button, press, .. } => (3, button, press as u32, 24),
            Op::Scroll { dx, dy, .. } => (4, dx as u32, dy as u32, 24),
            Op::Key { key, press, .. } => (5, key, press as u32, 24),
            Op::Frame { .. } => (6, 0, 0, 28),
        };
        let mut m = Vec::with_capacity(len as usize);
        m.extend_from_slice(&7u64.to_ne_bytes()); // object id
        m.extend_from_slice(&len.to_ne_bytes());
        m.extend_from_slice(&opcode.to_ne_bytes());
        m.extend_from_slice(&a.to_ne_bytes());
        m.extend_from_slice(&b.to_ne_bytes());
        m.resize(len as usize, 0);
        m
    }

    impl Sink<u8> for SockSink {
        fn write(&mut self, op: &Op<u8>) {
            self.buf.extend(encode(op));
        }
        fn flush(&mut self) -> io::Result<()> {
            while !self.buf.is_empty() {
                let (a, b) = self.buf.as_slices();
                let iov = [
                    libc::iovec { iov_base: a.as_ptr() as *mut _, iov_len: a.len() },
                    libc::iovec { iov_base: b.as_ptr() as *mut _, iov_len: b.len() },
                ];
                let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
                msg.msg_iov = iov.as_ptr() as *mut _;
                msg.msg_iovlen = 2;
                let n = unsafe {
                    libc::sendmsg(self.fd.as_raw_fd(), &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
                };
                if n < 0 {
                    return Err(io::Error::last_os_error());
                }
                self.buf.drain(..n as usize);
            }
            Ok(())
        }
    }

    /// One libeis-style dispatch: (whole messages, bytes thrown away).
    fn libeis_read(fd: &OwnedFd, carry: &mut Vec<u8>) -> (Vec<(u32, u32, u32)>, usize) {
        let mut data = std::mem::take(carry); // (always empty: libeis keeps nothing)
        loop {
            let mut chunk = [0u8; 1024];
            let n = unsafe {
                libc::recv(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len(), libc::MSG_DONTWAIT)
            };
            if n <= 0 {
                break;
            }
            data.extend_from_slice(&chunk[..n as usize]);
            if (n as usize) < chunk.len() {
                break;
            }
        }
        let mut msgs = Vec::new();
        let mut at = 0;
        while data.len() - at >= 16 {
            let len = u32::from_ne_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
            let opcode = u32::from_ne_bytes(data[at + 12..at + 16].try_into().unwrap());
            if len < 16 || data.len() - at < len {
                break;
            }
            let a = u32::from_ne_bytes(data[at + 16..at + 20].try_into().unwrap());
            let b = u32::from_ne_bytes(data[at + 20..at + 24].try_into().unwrap());
            msgs.push((opcode, a, b));
            at += len;
        }
        (msgs, data.len() - at)
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) }, 0);
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let size: libc::c_int = 32 * 1024;
        unsafe {
            libc::setsockopt(a.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&size as *const libc::c_int).cast(), 4)
        };
        (a, b)
    }

    fn sndbuf(fd: &OwnedFd) -> usize {
        let mut got: libc::c_int = 0;
        let mut len = 4;
        unsafe {
            libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&mut got as *mut libc::c_int).cast(), &mut len)
        };
        got as usize
    }

    /// Drives a stall-then-flood through an outbox of `batch` bytes and returns
    /// (bytes the reader threw away, key transitions it got).
    fn run(batch: usize) -> (usize, Vec<(u32, bool)>) {
        let (tx, rx) = pair();
        let mut o = Outbox::new(batch);
        o.max_batch = batch; // (the control's "unbounded" batch too)
        let mut sink = SockSink { fd: tx, buf: VecDeque::new() };
        let mut now = Instant::now();
        let mut lost = 0;
        let mut keys = Vec::new();
        let mut carry = Vec::new();
        let take = |msgs: Vec<(u32, u32, u32)>, keys: &mut Vec<(u32, bool)>| {
            for (opcode, a, b) in msgs {
                if opcode == 5 {
                    keys.push((a, b != 0));
                }
            }
        };
        // mutter doesn't read: 20000 motions and a few taps go in
        for i in 0..20_000u32 {
            now += Duration::from_micros(300);
            o.push(Op::Abs { dev: 1, x: i as f32, y: 1.0 });
            o.push(Op::Frame { dev: 1 });
            if i % 1000 == 999 {
                for press in [true, false] {
                    o.push(Op::Key { dev: 2, key: i / 1000, press });
                    o.push(Op::Frame { dev: 2 });
                }
            }
            o.pump(&mut sink, now);
        }
        // it reads again, at whatever moments, while we flush on POLLOUT
        for _ in 0..10_000 {
            let (msgs, dropped) = libeis_read(&rx, &mut carry);
            lost += dropped;
            take(msgs, &mut keys);
            now += Duration::from_millis(3);
            if o.pump(&mut sink, now) == Pump::Idle && sink.buf.is_empty() && o.len() == 0 {
                let (msgs, dropped) = libeis_read(&rx, &mut carry);
                lost += dropped;
                take(msgs, &mut keys);
                break;
            }
        }
        (lost, keys)
    }

    /// S2: whatever moment the reader picks, it finds only whole messages,
    /// and every key press and release arrives, in order.
    #[test]
    fn s2_reader_never_sees_a_split_message() {
        let (_, probe) = pair();
        let batch = batch_for(sndbuf(&probe));
        let (lost, keys) = run(batch);
        assert_eq!(lost, 0, "the reader threw away {lost} bytes of a split message");
        let want: Vec<(u32, bool)> = (0..20).flat_map(|k| [(k, true), (k, false)]).collect();
        assert_eq!(keys, want);
    }

    /// The control: the old way, every event straight into reis's buffer and
    /// a flush whose WouldBlock is ignored, so the backlog goes out in pieces
    /// the kernel cuts anywhere, and the reader loses input.
    #[test]
    fn s2_control_unpaced_flush_splits_messages() {
        let (tx, rx) = pair();
        let mut sink = SockSink { fd: tx, buf: VecDeque::new() };
        let send = |sink: &mut SockSink, op: Op<u8>| {
            sink.write(&op);
            let _ = sink.flush();
        };
        for i in 0..20_000u32 {
            send(&mut sink, Op::Abs { dev: 1, x: i as f32, y: 1.0 });
            send(&mut sink, Op::Frame { dev: 1 });
        }
        let mut lost = 0;
        let mut carry = Vec::new();
        // mutter reads again; each later X event flushes some more
        for i in 0..10_000u32 {
            lost += libeis_read(&rx, &mut carry).1;
            send(&mut sink, Op::Abs { dev: 1, x: i as f32, y: 2.0 });
            send(&mut sink, Op::Frame { dev: 1 });
            if sink.buf.is_empty() {
                break;
            }
        }
        lost += libeis_read(&rx, &mut carry).1;
        assert!(lost > 0, "expected the unpaced flush to split a message");
    }
}
