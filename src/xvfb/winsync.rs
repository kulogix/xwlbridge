//! `-showwindows` window sync with the desktop: the bridge side of the
//! bundled GNOME Shell extension `xwlbridge-windows@xwlbridge`, which serves
//! `org.xwlbridge.Windows1` on the session bus (research/release/
//! design_desktop_integration.md section 2, design_x_capability_gaps.md
//! sections A and B).
//!
//! A Wayland client cannot place its own toplevel, so a shown X window's
//! position reaches the desktop only through the extension, which finds the
//! window by its xdg_toplevel_tag_v1 tag (`tag`). This module holds:
//! - the decisions, pure and unit-tested: the tag, the per-window geometry
//!   sync (`Geometry`: coalescing, echo suppression, authority, clamping) and
//!   raise gating (`RaiseGate`);
//! - with `--features gnome`, the D-Bus client (`Client`): its own threads, so
//!   a request never blocks the reverse thread, and an eventfd plus a
//!   non-blocking drain for what comes back.
//!
//! The reverse thread is the only user: it feeds X events and the client's
//! events into `Geometry` and does what comes out (a MoveResize to send, or
//! an X ConfigureWindow plus the synthetic ConfigureNotify of ICCCM 4.1.5).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// An X window id.
pub type Xid = u32;

/// What every tag of ours starts with; the extension serves nothing else.
pub const TAG_PREFIX: &str = "xwlbridge:";

/// The tag of shown window `xid` of display `:display`:
/// `xwlbridge:<display>:0x<xid in lowercase hex>`. The display number keeps
/// several bridges' windows apart.
pub fn tag(display: u32, xid: Xid) -> String {
    format!("{TAG_PREFIX}{display}:{xid:#x}")
}

/// A tag's display and window, for tags exactly as `tag` writes them (no
/// leading zeros, lowercase, a non-zero window), so a tag has one spelling
/// and nothing else parses.
#[cfg_attr(not(feature = "gnome"), allow(dead_code))] // the D-Bus client's
pub fn parse_tag(s: &str) -> Option<(u32, Xid)> {
    let (display, xid) = s.strip_prefix(TAG_PREFIX)?.split_once(':')?;
    let canonical_decimal = !display.is_empty()
        && display.bytes().all(|b| b.is_ascii_digit())
        && (display == "0" || !display.starts_with('0'));
    let hex = xid.strip_prefix("0x")?;
    let canonical_hex = !hex.is_empty()
        && hex.len() <= 8
        && !hex.starts_with('0')
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !canonical_decimal || !canonical_hex {
        return None;
    }
    Some((display.parse().ok()?, Xid::from_str_radix(hex, 16).ok()?))
}

/// The window of `s` if it is a tag of display `display`.
#[cfg_attr(not(feature = "gnome"), allow(dead_code))] // the D-Bus client's
pub fn tag_window(s: &str, display: u32) -> Option<Xid> {
    parse_tag(s).filter(|t| t.0 == display).map(|t| t.1)
}

/// A window rectangle in the shared coordinate space (X root == the
/// desktop's logical layout): the X window's geometry, which is the
/// toplevel's xdg window geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
}

/// `MoveResize` flags.
pub mod mr {
    /// Keep the window inside the work area of the monitor it is (mostly) on.
    pub const CLAMP: u32 = 1 << 0;
    /// A user's action (mutter's `user_op`): set while a button is held on
    /// the window (the X client's own drag loop), or when re-asserting a
    /// user's change.
    pub const USER: u32 = 1 << 1;
}

/// Window state bits (`Changed`'s flags, `GetState`'s `flags`).
#[allow(dead_code)] // the extension's full set; the sync reads USER
pub mod st {
    /// The user is moving or resizing the window on the desktop (a mutter
    /// grab op is active on it).
    pub const USER: u32 = 1 << 0;
    pub const MINIMIZED: u32 = 1 << 1;
    pub const MAXIMIZED: u32 = 1 << 2;
    pub const FULLSCREEN: u32 = 1 << 3;
    pub const ABOVE: u32 = 1 << 4;
    pub const FOCUSED: u32 = 1 << 5;
}

/// `Changed`'s and `GetState`'s workspace of a window on every workspace.
#[allow(dead_code)] // the extension's API (workspaces are a later phase)
pub const ALL_WORKSPACES: i32 = -1;

/// Whether desktop serial `a` is later than `b` (the extension's counter is
/// a u32 that may wrap).
pub fn serial_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// How long after a user-initiated desktop change the desktop stays
/// authoritative over the X client's own moves of that window (as it is for
/// the whole of the user's grab): an X move then is put back rather than
/// fought over. Covers the end of a grab racing a client's move.
pub const DESKTOP_GRACE: Duration = Duration::from_millis(300);

/// A MoveResize to send for a window: at most one per window is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveResize {
    pub rect: Rect,
    pub flags: u32,
}

/// How many times in a row a client's own move is put back while the
/// desktop is busy with the window (`DESKTOP_GRACE`, a grab): a client that
/// re-asserts its position on every ConfigureNotify then gives up the fight
/// after this many rounds per desktop report instead of bouncing at X speed.
pub const PUT_BACKS: u8 = 3;

/// One shown toplevel's sync state.
#[derive(Debug, Default)]
struct Win {
    /// The X window's rectangle as we know it (its last ConfigureNotify, or
    /// what we configured it to).
    x: Rect,
    /// The desktop's rectangle as last reported (a MoveResize reply, a
    /// Changed, a resync); `None` until the desktop has said.
    desk: Option<Rect>,
    /// The order stamp of the newest desktop report taken into `desk` (the
    /// extension's message serial: replies and signals come from one
    /// connection, so their serials are the order the desktop sent them in,
    /// whatever order the client's threads deliver them in). Older reports
    /// are stale.
    serial: Option<u32>,
    /// X -> desktop: X's latest rectangle not sent yet (latest wins).
    pending: Option<Rect>,
    /// Extra MoveResize flags for `pending`.
    pending_flags: u32,
    /// The MoveResize sent and not answered yet (its requested rectangle).
    in_flight: Option<Rect>,
    /// A MoveResize that timed out: the desktop may still carry it out, and
    /// its late report is ours, not a desktop move.
    late: Option<Rect>,
    /// A ConfigureWindow we sent the X window: (its request's sequence
    /// number, the rectangle). The first ConfigureNotify generated at or
    /// after it settles it.
    x_echo: Option<(u64, Rect)>,
    /// A button is held on the window: X is authoritative for it.
    held: bool,
    /// When the last user-initiated desktop change came.
    user_at: Option<Instant>,
    /// The user has a grab on it on the desktop (`st::USER` in its latest
    /// state).
    grab: bool,
    /// A user-initiated desktop change that came while a MoveResize was in
    /// flight.
    overridden: Option<Rect>,
    /// The client's own moves put back since the desktop's last report
    /// (at most `PUT_BACKS`).
    put_backs: u8,
}

/// The geometry sync of every shown toplevel (design section 2's loop and
/// race rules):
/// 1. echoes are dropped on both sides: a desktop report of the position
///    the desktop already had while X's own move waits to go (ours landing,
///    or a state change), and an X ConfigureNotify of what we configured
///    from the desktop (by its request's sequence number); stale reports
///    (older order stamps, X events generated before our ConfigureWindow)
///    too;
/// 2. while a button is held on a window, X is authoritative for it (the
///    client's own drag loop); otherwise the desktop is;
/// 3. a user-initiated desktop change cancels a pending X -> desktop move,
///    and the client's own moves while the user has the window are put back
///    (position only, at most `PUT_BACKS` per desktop report);
/// 4. X -> desktop moves are coalesced: the latest wins, at most one in
///    flight per window, sent from the window's frame callback (`frame`);
/// 5. a clamped move (applied != requested) moves the X window back, with
///    the synthetic ConfigureNotify an X window manager sends at an edge.
///
/// Every method that returns `Some(rect)` means: ConfigureWindow the X
/// window to `rect`, send it the synthetic ConfigureNotify, and report the
/// request with `x_sent`.
#[derive(Debug, Default)]
pub struct Geometry {
    wins: HashMap<Xid, Win>,
}

/// Whether a report stamped `stamp` is newer than what `w` has (no stamp:
/// newer).
fn fresh(w: &Win, stamp: Option<u32>) -> bool {
    match (stamp, w.serial) {
        (Some(s), Some(cur)) => serial_newer(s, cur),
        _ => true,
    }
}

/// `desk`'s position with `x`'s size.
fn at_desk(desk: Rect, x: Rect) -> Rect {
    Rect {
        w: x.w,
        h: x.h,
        ..desk
    }
}

impl Geometry {
    pub fn new() -> Geometry {
        Geometry::default()
    }

    /// A toplevel is shown, at X rectangle `x`.
    pub fn track(&mut self, xid: Xid, x: Rect) {
        self.wins.entry(xid).or_default().x = x;
    }

    /// The toplevel is gone (unmapped, destroyed, or the desktop's
    /// WindowGone).
    pub fn forget(&mut self, xid: Xid) {
        self.wins.remove(&xid);
    }

    #[allow(dead_code)] // tests
    pub fn is_tracked(&self, xid: Xid) -> bool {
        self.wins.contains_key(&xid)
    }

    /// Whether a MoveResize for `xid` is waiting for its reply.
    #[allow(dead_code)] // tests
    pub fn in_flight(&self, xid: Xid) -> bool {
        self.wins.get(&xid).is_some_and(|w| w.in_flight.is_some())
    }

    /// Whether an X move of `xid` waits to be sent (by `frame`).
    pub fn pending(&self, xid: Xid) -> bool {
        self.wins.get(&xid).is_some_and(|w| w.pending.is_some())
    }

    /// The desktop's rectangle of `xid` as last reported (None: not yet).
    pub fn desk(&self, xid: Xid) -> Option<Rect> {
        self.wins.get(&xid)?.desk
    }

    /// The X window's rectangle as last known.
    pub fn x_rect(&self, xid: Xid) -> Option<Rect> {
        self.wins.get(&xid).map(|w| w.x)
    }

    /// Whether a desktop report about `xid` stamped `stamp` is newer than
    /// every one taken so far.
    pub fn is_fresh(&self, xid: Xid, stamp: u32) -> bool {
        self.wins.get(&xid).is_some_and(|w| fresh(w, Some(stamp)))
    }

    /// The bridge configured the X window to `rect` with the request of
    /// sequence number `seq` (for any desktop-originated reason: a `Some`
    /// from this type, or the toplevel's own configure): the ConfigureNotify
    /// it generates is not a move of the client's, and nothing generated
    /// before it is current.
    pub fn x_sent(&mut self, xid: Xid, rect: Rect, seq: u64) {
        if let Some(w) = self.wins.get_mut(&xid) {
            w.x = rect;
            w.x_echo = Some((seq, rect));
            w.pending = None;
            w.pending_flags = 0;
        }
    }

    /// A ConfigureNotify for the X window (`seq`: the sequence number it
    /// came with, as x11rb's `poll_for_event_with_sequence` gives it).
    pub fn x_configured(&mut self, xid: Xid, rect: Rect, seq: u64, now: Instant) -> Option<Rect> {
        let w = self.wins.get_mut(&xid)?;
        if let Some((sent, ours)) = w.x_echo {
            if seq < sent {
                return None; // generated before our ConfigureWindow overrode it
            }
            w.x_echo = None;
            if rect == ours {
                return None; // our own
            }
        }
        if rect == w.x {
            return None; // no geometry change (a restack, a border width)
        }
        w.x = rect;
        if w.in_flight.is_none() && w.desk == Some(rect) {
            w.pending = None; // the desktop is there already
            return None;
        }
        let desktop_busy = w.grab
            || w.user_at
                .is_some_and(|t| now.saturating_duration_since(t) < DESKTOP_GRACE);
        if !w.held && desktop_busy {
            // the user is moving it on the desktop: X goes back, to the
            // desktop's position (its size is the client's: a resize of its
            // own reaches the desktop with its next frame), a few times per
            // desktop report at most
            w.pending = None;
            w.pending_flags = 0;
            let desk = w.desk?;
            if (desk.x, desk.y) == (rect.x, rect.y) {
                return None;
            }
            if w.put_backs < PUT_BACKS {
                w.put_backs += 1;
                return Some(at_desk(desk, rect));
            }
            if !w.grab {
                // the grab is over and the client insists: it wins
                w.pending = Some(rect);
            }
            // during the grab, its end (a report) settles it
            return None;
        }
        w.pending = Some(rect);
        None
    }

    /// Whether a button is held on the window (any button of the desktop's
    /// pointer, touch or tablet on its surface).
    pub fn button(&mut self, xid: Xid, held: bool) -> Option<Rect> {
        let w = self.wins.get_mut(&xid)?;
        let was = std::mem::replace(&mut w.held, held);
        if held || !was || w.pending.is_some() || w.in_flight.is_some() {
            return None;
        }
        // released with X's last move landed: the desktop is authoritative
        // again, and a change it made meanwhile stands
        w.desk.filter(|d| *d != w.x)
    }

    /// The window's frame callback: the coalesced X -> desktop move to send
    /// now, if any (then it is in flight until `applied` or `failed`).
    pub fn frame(&mut self, xid: Xid) -> Option<MoveResize> {
        let w = self.wins.get_mut(&xid)?;
        if w.in_flight.is_some() {
            return None;
        }
        let rect = w.pending.take()?;
        let extra = std::mem::take(&mut w.pending_flags);
        if w.desk == Some(rect) {
            return None;
        }
        w.in_flight = Some(rect);
        w.late = None;
        // a held-button move is the client's own drag: mutter's user-move
        // constraints apply (it may cross monitors); otherwise it stays in
        // the work area of the monitor it goes to
        let flags = if w.held { mr::USER } else { mr::CLAMP };
        Some(MoveResize {
            rect,
            flags: flags | extra,
        })
    }

    /// The reply to the window's MoveResize of `requested`: the desktop
    /// applied `applied` (after clamping; for a new size, the size asked for
    /// until the client has drawn it, which a later `Changed` reports).
    /// `stamp`: the reply's order stamp (see `Win::serial`); a reply older
    /// than a report already taken settles the request and nothing else.
    pub fn applied(
        &mut self,
        xid: Xid,
        requested: Rect,
        applied: Rect,
        stamp: Option<u32>,
    ) -> Option<Rect> {
        let w = self.wins.get_mut(&xid)?;
        if w.in_flight != Some(requested) {
            return None; // not the request in flight (a reset dropped it)
        }
        w.in_flight = None;
        let user = w.overridden.take();
        if !fresh(w, stamp) {
            // the desktop reported a newer state before this reply came in
            // (the client's threads do not keep the desktop's order): that
            // state stands, and X follows it unless it moved on itself
            if w.pending.is_some() || w.held {
                return None;
            }
            let desk = at_desk(w.desk?, w.x);
            return (desk != w.x).then_some(desk);
        }
        if stamp.is_some() {
            w.serial = stamp;
        }
        w.desk = Some(applied);
        if let Some(user) = user {
            // the user changed it while ours was in flight, and ours landed
            // on top: X follows the user already, so put the user's result
            // back on the desktop
            if applied != user {
                w.pending = Some(user);
                w.pending_flags = mr::USER;
            }
            return None;
        }
        if w.pending.is_some() {
            return None; // X moved on: the next request settles it
        }
        // clamped at an edge or a size limit: X goes back, as under an X WM
        (applied != requested && applied != w.x).then_some(applied)
    }

    /// The window's MoveResize failed. `answered`: the desktop answered
    /// with an error (the move did nothing): forget it, X's next move tries
    /// again. Otherwise (a timeout, the connection went) the desktop may
    /// still carry it out: its late report is taken as ours, and X's
    /// position goes again with the next request.
    pub fn failed(&mut self, xid: Xid, answered: bool) {
        if let Some(w) = self.wins.get_mut(&xid) {
            let req = w.in_flight.take();
            w.overridden = None;
            if answered {
                w.pending = None;
                w.pending_flags = 0;
            } else {
                w.late = req;
                w.pending = Some(w.pending.unwrap_or(w.x));
            }
        }
    }

    /// The desktop's `Changed` for the window (`serial`: its order stamp).
    pub fn changed(
        &mut self,
        xid: Xid,
        rect: Rect,
        flags: u32,
        serial: u32,
        now: Instant,
    ) -> Option<Rect> {
        let w = self.wins.get_mut(&xid)?;
        if !fresh(w, Some(serial)) {
            return None; // stale, or a duplicate
        }
        w.serial = Some(serial);
        let prev = w.desk.replace(rect);
        w.grab = flags & st::USER != 0;
        w.put_backs = 0;
        if w.held {
            return None; // X is authoritative while a button is held
        }
        if w.grab {
            w.user_at = Some(now);
            w.pending = None; // a user's change beats a pending X move
            w.pending_flags = 0;
            w.late = None;
            if w.in_flight.is_some() {
                w.overridden = Some(rect);
            }
            return (rect != w.x).then_some(rect);
        }
        if w.in_flight.is_some() {
            return None; // ours landing, or overtaken by it: the reply settles it
        }
        if w.late == Some(rect) {
            // a request of ours that timed out, carried out late
            w.late = None;
            if w.pending.is_some() {
                return None;
            }
        }
        if w.pending.is_some() && prev.is_some_and(|p| (p.x, p.y) == (rect.x, rect.y)) {
            // the desktop did not move it (a state change, the echo of a
            // move of ours): X's own move goes next
            return None;
        }
        if rect == w.x {
            w.pending = None;
            return None;
        }
        // the desktop moved it on its own (a monitor went, mutter's
        // constraints, unmaximise, untile): the desktop is authoritative
        w.pending = None;
        w.pending_flags = 0;
        Some(rect)
    }

    /// A full resync from `GetState` (the extension came or came back, or
    /// the monitors changed): the desktop's rectangle, state bits and order
    /// stamp for the window. A window reported newer since is left alone,
    /// and a request in flight is settled by its reply.
    pub fn resync(&mut self, xid: Xid, rect: Rect, flags: u32, serial: u32) -> Option<Rect> {
        let w = self.wins.get_mut(&xid)?;
        if !fresh(w, Some(serial)) {
            return None; // a newer report came in first
        }
        w.serial = Some(serial);
        w.desk = Some(rect);
        w.grab = flags & st::USER != 0;
        w.put_backs = 0;
        w.late = None;
        if w.held {
            // X rules while held: push X's rectangle instead
            if rect != w.x {
                w.pending = Some(w.x);
            }
            return None;
        }
        if w.in_flight.is_some() {
            return None;
        }
        w.pending = None;
        w.pending_flags = 0;
        (rect != w.x).then_some(rect)
    }

    /// The extension went away: nothing is in flight any more, and the
    /// desktop's state is unknown until the next resync.
    pub fn reset(&mut self) {
        for w in self.wins.values_mut() {
            let x = w.x;
            *w = Win {
                x,
                held: w.held,
                ..Win::default()
            };
        }
    }
}

/// How recent a client's own input on one of its shown windows must be for
/// its raise or activation request to be honoured (the X equivalent of an
/// xdg_activation token from that input).
pub const RECENT_INPUT: Duration = Duration::from_secs(2);

/// An ungated raise or activation request of a window is forwarded (as
/// demands-attention) at most once per this long: no raise loop can flicker
/// or nag.
pub const UNGATED_INTERVAL: Duration = Duration::from_secs(3);

/// What becomes of an X client's raise or activation request
/// (XRaiseWindow, a ConfigureWindow restack to Above/TopIf,
/// _NET_RESTACK_WINDOW, _NET_ACTIVE_WINDOW): design_x_capability_gaps.md
/// rule A2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raise {
    /// Recent user input in that client: activate (raise and focus).
    Activate,
    /// No recent input: the window demands attention instead.
    DemandAttention,
    /// An ungated request again within `UNGATED_INTERVAL`: nothing.
    Ignore,
}

/// Raise gating: the clients' latest input on their shown windows, and each
/// window's last ungated request.
#[derive(Debug, Default)]
pub struct RaiseGate {
    input: HashMap<u32, Instant>,
    ungated: HashMap<Xid, Instant>,
}

impl RaiseGate {
    pub fn new() -> RaiseGate {
        RaiseGate::default()
    }

    /// The user pressed a key or button on a shown window of `client` (an X
    /// resource base).
    pub fn input(&mut self, client: u32, now: Instant) {
        let t = self.input.entry(client).or_insert(now);
        *t = (*t).max(now);
    }

    /// The desktop's keyboard went to something that is not `client`'s:
    /// its earlier input no longer vouches for a raise (the user is busy
    /// elsewhere).
    pub fn left(&mut self, client: u32) {
        self.input.remove(&client);
    }

    /// Window `xid` of `client` asks to be raised or activated.
    pub fn request(&mut self, xid: Xid, client: u32, now: Instant) -> Raise {
        let recent = self
            .input
            .get(&client)
            .is_some_and(|t| now.saturating_duration_since(*t) <= RECENT_INPUT);
        if recent {
            return Raise::Activate;
        }
        match self.ungated.get(&xid) {
            Some(t) if now.saturating_duration_since(*t) < UNGATED_INTERVAL => Raise::Ignore,
            _ => {
                self.ungated.insert(xid, now);
                Raise::DemandAttention
            }
        }
    }

    pub fn forget_window(&mut self, xid: Xid) {
        self.ungated.remove(&xid);
    }

    #[allow(dead_code)] // no client-gone signal reaches the reverse thread yet
    pub fn forget_client(&mut self, client: u32) {
        self.input.remove(&client);
    }
}

// ---- the integration's decisions (src/xvfb/reverse.rs carries them out) ----

/// A shown window's rectangle in the shared space from X's geometry: the
/// outer corner and the size including the border (the shown surface is the
/// named pixmap, border included, and has no other window geometry).
pub fn outer_rect(x: i16, y: i16, width: u16, height: u16, border: u16) -> Rect {
    Rect::new(
        i32::from(x),
        i32::from(y),
        i32::from(width) + 2 * i32::from(border),
        i32::from(height) + 2 * i32::from(border),
    )
}

/// The X window size (inside the border) of shared-space size `w`x`h`; at
/// least 1x1.
pub fn inner_size(w: i32, h: i32, border: u16) -> (u32, u32) {
    let b = 2 * i32::from(border);
    ((w - b).max(1) as u32, (h - b).max(1) as u32)
}

/// What the X window should be told of the desktop's rectangle `desk`: its
/// position, with X's own size `x`. A shown window's size on the desktop is
/// the size of the buffer the bridge committed last, so a desktop report can
/// only lag X's size, never lead it: the desktop's own resizes (the user's,
/// tiling, maximise) come as the toplevel's configure, which resizes the X
/// window, and a clamped size comes back in the MoveResize reply. Taking the
/// reported size would shrink a window that just grew itself back to the
/// size of its last frame.
pub fn desktop_position(desk: Rect, x: Rect) -> Rect {
    Rect {
        w: x.w,
        h: x.h,
        ..desk
    }
}

/// WM_NORMAL_HINTS flags.
const US_POSITION: u32 = 1 << 0;
const P_POSITION: u32 = 1 << 2;

/// Where a new toplevel at X position `(x, y)` asks to open on the desktop,
/// from its WM_NORMAL_HINTS (32-bit values): the user's position
/// (USPosition: -geometry, session restore) always; the program's
/// (PPosition: a saved layout) unless it is 0,0, which toolkits report for a
/// window nobody placed. None: the desktop places it (and X follows).
pub fn initial_position(normal_hints: &[u32], x: i32, y: i32) -> Option<(i32, i32)> {
    let flags = normal_hints.first().copied().unwrap_or(0);
    let user = flags & US_POSITION != 0;
    let program = flags & P_POSITION != 0 && (x, y) != (0, 0);
    (user || program).then_some((x, y))
}

/// WM_HINTS (32-bit values) has UrgencyHint set.
pub fn urgent(wm_hints: &[u32]) -> bool {
    wm_hints.first().is_some_and(|f| f & (1 << 8) != 0)
}

/// Where pointer positions on a shown toplevel's surface map from (root =
/// origin + surface-local): with the sync, where the desktop has the window
/// (`desk`, as last reported) - the surface's true position, also while the
/// X client's own drag loop moves the X window ahead of it, so the X pointer
/// stays where the desktop's is and the drag neither doubles nor stalls;
/// before the desktop has reported, and without the sync, X's position
/// (`x`; 0.1.2 holds it during a held-button self-drag).
pub fn pointer_origin(desk: Option<Rect>, x: (i16, i16)) -> (i16, i16) {
    match desk {
        Some(d) => (
            d.x.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            d.y.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        ),
        None => x,
    }
}

/// The least time between two MoveResize requests of a window that is not
/// painting (no frame callback to pace them): one per 60 Hz frame.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// A frame callback later than this (a hidden or minimised window's) does
/// not hold a MoveResize back.
pub const STALE_FRAME: Duration = Duration::from_millis(100);

/// When a window's coalesced MoveResize may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Now.
    Now,
    /// At the frame callback of the commit in flight.
    Frame,
    /// At this time (the window does not paint: paced by `FRAME_INTERVAL`).
    At(Instant),
}

/// One MoveResize per frame callback at most: while a commit (made at
/// `frame_wait`) waits for its frame callback the request goes with it;
/// otherwise at once, unless the last one went less than `FRAME_INTERVAL`
/// ago (a window moving itself without repainting has no frame callbacks).
pub fn send_due(frame_wait: Option<Instant>, last: Option<Instant>, now: Instant) -> Due {
    if frame_wait.is_some_and(|t| now.saturating_duration_since(t) < STALE_FRAME) {
        return Due::Frame;
    }
    match last {
        Some(t) if now.saturating_duration_since(t) < FRAME_INTERVAL => Due::At(t + FRAME_INTERVAL),
        _ => Due::Now,
    }
}

/// A restack: put a window directly above or below another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stack {
    Above(Xid),
    Below(Xid),
}

/// How to restack X so the shown toplevels are in the desktop's order (the
/// desktop is the only stacking authority: design_x_capability_gaps.md A1).
/// `desktop` and `x` are bottom to top; windows in only one of them are
/// left out. The longest run of windows already in the desktop's order stays
/// put and each other window is restacked once, in order (`(w, Stack)`: put
/// `w` there); empty when X agrees.
pub fn restack_plan(desktop: &[Xid], x: &[Xid]) -> Vec<(Xid, Stack)> {
    let mut desk: Vec<Xid> = Vec::new();
    for w in desktop {
        if x.contains(w) && !desk.contains(w) {
            desk.push(*w);
        }
    }
    let n = desk.len();
    // X's positions of the desktop's windows; the longest increasing run of
    // them is what is in the desktop's order already
    let at: Vec<usize> = desk
        .iter()
        .map(|w| x.iter().position(|u| u == w).expect("in both"))
        .collect();
    let (mut len, mut prev) = (vec![1usize; n], vec![usize::MAX; n]);
    for i in 0..n {
        for j in 0..i {
            if at[j] < at[i] && len[j] + 1 > len[i] {
                len[i] = len[j] + 1;
                prev[i] = j;
            }
        }
    }
    let mut keep = vec![false; n];
    let mut i = (0..n).max_by_key(|&i| (len[i], std::cmp::Reverse(i)));
    while let Some(k) = i {
        keep[k] = true;
        i = (prev[k] != usize::MAX).then_some(prev[k]);
    }
    let mut ops = Vec::new();
    for i in 0..n {
        if keep[i] {
            continue;
        }
        if i > 0 {
            // its desktop neighbour below is in place (kept, or put there)
            ops.push((desk[i], Stack::Above(desk[i - 1])));
        } else if let Some(k) = (0..n).find(|&k| keep[k]) {
            // the desktop's bottom one goes below the lowest kept window
            ops.push((desk[i], Stack::Below(desk[k])));
        }
    }
    ops
}

/// Whether a ConfigureNotify of a shown window may be a restack: its
/// geometry is the one it had (`(position, width, height, sibling below)`
/// before and after; a move or resize is not, and a configure of ours that
/// moved it is not either), or the sibling below it changed. The stacking
/// itself is then checked against the desktop's (`raised_over_desktop`).
pub fn restack_event(
    before: ((i16, i16), u16, u16, Option<Xid>),
    after: ((i16, i16), u16, u16, Xid),
) -> bool {
    let same_geometry = (before.0, before.1, before.2) == (after.0, after.1, after.2);
    same_geometry || before.3.is_some_and(|b| b != after.3)
}

/// Whether X's order `x` (after a client restacked `xid`) has `xid` above a
/// window the desktop has above it (`desktop`, bottom to top): the client
/// raised it (XRaiseWindow, a restack to Above or TopIf), which is a request
/// for the raise gating, not a fact. A lower is never a request: X just goes
/// back to the desktop's order.
pub fn raised_over_desktop(xid: Xid, desktop: &[Xid], x: &[Xid]) -> bool {
    let (Some(d), Some(xi)) = (
        desktop.iter().position(|&w| w == xid),
        x.iter().position(|&w| w == xid),
    ) else {
        return false;
    };
    desktop[d + 1..]
        .iter()
        .any(|v| x.iter().position(|u| u == v).is_some_and(|p| p < xi))
}

/// Keep-above of the shown toplevels (design_x_capability_gaps.md A3, A4):
/// a window is above when it asks (_NET_WM_STATE_ABOVE, or KDE's
/// _NET_WM_STATE_STAYS_ON_TOP), or when its transient parent is (a dialog of
/// an above window must not open behind it: no invisible modal); and what
/// the desktop was last told.
#[derive(Debug, Default)]
pub struct Layers {
    own: HashMap<Xid, bool>,
    parent: HashMap<Xid, Xid>,
    told: HashMap<Xid, bool>,
}

impl Layers {
    pub fn new() -> Layers {
        Layers::default()
    }

    /// Window `xid` asks to be above (`own`); `parent`: its shown transient
    /// parent.
    pub fn set(&mut self, xid: Xid, own: bool, parent: Option<Xid>) {
        self.own.insert(xid, own);
        match parent {
            Some(p) if p != xid => self.parent.insert(xid, p),
            _ => self.parent.remove(&xid),
        };
    }

    pub fn forget(&mut self, xid: Xid) {
        self.own.remove(&xid);
        self.parent.remove(&xid);
        self.told.remove(&xid);
    }

    /// Whether `xid` is above: its own wish or its parent chain's (a cycle
    /// of transients ends the walk).
    pub fn above(&self, xid: Xid) -> bool {
        let mut w = xid;
        for _ in 0..32 {
            if self.own.get(&w).copied().unwrap_or(false) {
                return true;
            }
            match self.parent.get(&w) {
                Some(&p) if p != xid => w = p,
                _ => return false,
            }
        }
        false
    }

    /// What to tell the desktop (SetAbove), by window: every known window
    /// whose state differs from what it was told (untold = not above).
    pub fn changes(&mut self) -> Vec<(Xid, bool)> {
        let mut xids: Vec<Xid> = self.own.keys().copied().collect();
        xids.sort_unstable();
        let mut out = Vec::new();
        for w in xids {
            let on = self.above(w);
            if self.told.get(&w).copied().unwrap_or(false) != on {
                self.told.insert(w, on);
                out.push((w, on));
            }
        }
        out
    }

    /// The desktop side came back: tell it again (the next `changes`).
    pub fn retell(&mut self) {
        self.told.clear();
    }

    /// The desktop reports `xid` above (`on`) or not. When that is not what
    /// it was told, the user changed it there (Always on Top): it becomes
    /// the window's own wish, so a retell repeats it. True: X's
    /// `_NET_WM_STATE` is to follow.
    pub fn desktop(&mut self, xid: Xid, on: bool) -> bool {
        if !self.own.contains_key(&xid) || self.told.get(&xid).copied().unwrap_or(false) == on {
            return false;
        }
        self.told.insert(xid, on);
        self.own.insert(xid, on);
        true
    }
}

/// Whether the desktop's monitors fit the X screen of size `screen` one to
/// one, which the window sync needs (it puts X windows where the desktop
/// has them, and pointer input reaches only the X screen): every monitor
/// unscaled (the X screen is in physical pixels, the desktop's layout in
/// logical ones) and inside the X screen. Otherwise why not.
pub fn layout_fits(monitors: &[MonitorInfo], screen: (u16, u16)) -> Result<(), String> {
    if monitors.is_empty() {
        return Err("the desktop reports no monitors".into());
    }
    let (sw, sh) = (i32::from(screen.0), i32::from(screen.1));
    for m in monitors {
        if (m.scale - 1.0).abs() > 1e-6 {
            return Err(format!(
                "monitor {} is scaled {}x: the sync needs unscaled monitors for now",
                m.index, m.scale
            ));
        }
        let r = m.rect;
        if r.x < 0 || r.y < 0 || r.x + r.w > sw || r.y + r.h > sh {
            return Err(format!(
                "monitor {} at {}x{}+{}+{} lies outside the {sw}x{sh} X screen",
                m.index, r.w, r.h, r.x, r.y
            ));
        }
    }
    Ok(())
}

/// Demands-attention of the shown toplevels from their hints (WM_HINTS
/// UrgencyHint, _NET_WM_STATE_DEMANDS_ATTENTION: design_x_capability_gaps.md
/// B), and what the desktop was told.
#[derive(Debug, Default)]
pub struct Attention {
    told: HashMap<Xid, bool>,
}

impl Attention {
    pub fn new() -> Attention {
        Attention::default()
    }

    /// The window's hints ask for attention or not: what to tell the
    /// desktop (SetDemandsAttention), if anything (untold = no attention).
    pub fn hint(&mut self, xid: Xid, on: bool) -> Option<bool> {
        let was = self.told.insert(xid, on).unwrap_or(false);
        (was != on).then_some(on)
    }

    pub fn forget(&mut self, xid: Xid) {
        self.told.remove(&xid);
    }

    /// The desktop side came back: the windows to tell again.
    pub fn retell(&self) -> Vec<Xid> {
        let mut v: Vec<Xid> = self
            .told
            .iter()
            .filter(|(_, on)| **on)
            .map(|(w, _)| *w)
            .collect();
        v.sort_unstable();
        v
    }
}

/// The extension's requests and events (pure data; the client is `bus`).
/// A request to the extension, about window `xid` of our display.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // the extension's full API; this phase uses part of it
pub enum Request {
    MoveResize { xid: Xid, rect: Rect, flags: u32 },
    SetInitialPosition { xid: Xid, x: i32, y: i32 },
    Activate { xid: Xid, timestamp: u32 },
    Raise { xid: Xid },
    SetAbove { xid: Xid, on: bool },
    SetDemandsAttention { xid: Xid, on: bool },
    Stick { xid: Xid, on: bool },
    SetWorkspace { xid: Xid, index: i32 },
    Minimize { xid: Xid, on: bool },
    GetState,
    GetMonitors,
}

#[cfg_attr(not(feature = "gnome"), allow(dead_code))] // the D-Bus client's
impl Request {
    pub fn method(&self) -> &'static str {
        match self {
            Request::MoveResize { .. } => "MoveResize",
            Request::SetInitialPosition { .. } => "SetInitialPosition",
            Request::Activate { .. } => "Activate",
            Request::Raise { .. } => "Raise",
            Request::SetAbove { .. } => "SetAbove",
            Request::SetDemandsAttention { .. } => "SetDemandsAttention",
            Request::Stick { .. } => "Stick",
            Request::SetWorkspace { .. } => "SetWorkspace",
            Request::Minimize { .. } => "Minimize",
            Request::GetState => "GetState",
            Request::GetMonitors => "GetMonitors",
        }
    }

    pub fn xid(&self) -> Option<Xid> {
        match *self {
            Request::MoveResize { xid, .. }
            | Request::SetInitialPosition { xid, .. }
            | Request::Activate { xid, .. }
            | Request::Raise { xid }
            | Request::SetAbove { xid, .. }
            | Request::SetDemandsAttention { xid, .. }
            | Request::Stick { xid, .. }
            | Request::SetWorkspace { xid, .. }
            | Request::Minimize { xid, .. } => Some(xid),
            Request::GetState | Request::GetMonitors => None,
        }
    }
}

/// A window as `GetState` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowState {
    pub xid: Xid,
    pub rect: Rect,
    pub monitor: i32,
    /// `ALL_WORKSPACES` when on all.
    pub workspace: i32,
    /// `st` bits.
    pub flags: u32,
    /// The serial of its last `Changed` (0: none yet).
    pub serial: u32,
    /// Mapped on the desktop yet.
    pub shown: bool,
}

/// A monitor as `GetMonitors` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub index: i32,
    pub rect: Rect,
    pub work_area: Rect,
    pub scale: f64,
    pub primary: bool,
}

/// What comes back from the extension.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(feature = "gnome"), allow(dead_code))] // the D-Bus client's
pub enum Event {
    /// The extension is there and compatible (at start, or it came
    /// back): resync with `GetState`.
    Available {
        version: u32,
        features: Vec<String>,
    },
    /// It went away (or the session bus did): fall back.
    Unavailable,
    /// The reply to a MoveResize.
    Applied {
        xid: Xid,
        requested: Rect,
        applied: Rect,
        /// The reply's order stamp (`Stamp`).
        wire: u32,
    },
    /// A request about `xid` failed. Every MoveResize ends in exactly
    /// one `Applied` or `Failed`. `answered`: the extension answered with
    /// an error (the request did nothing); otherwise it timed out or the
    /// connection went, and the desktop may still carry it out.
    Failed {
        xid: Xid,
        method: &'static str,
        error: String,
        answered: bool,
    },
    Changed {
        xid: Xid,
        rect: Rect,
        monitor: i32,
        workspace: i32,
        flags: u32,
        serial: u32,
        /// The signal's order stamp (`Stamp`).
        wire: u32,
    },
    /// Our display's windows, bottom to top, and the order stamp.
    Restacked(Vec<Xid>, u32),
    MonitorsChanged,
    WindowGone(Xid),
    /// Our display's windows, bottom to top, and the reply's order stamp.
    State(Vec<WindowState>, u32),
    Monitors(Vec<MonitorInfo>),
}

/// An order stamp: the extension's D-Bus message serial. Its replies and
/// signals all leave gnome-shell on one connection, so their serials are
/// the order the desktop sent them in; the client's threads deliver
/// replies (the worker) and signals (the signal thread) in either order,
/// so the reverse thread drops what is older than what it has already
/// taken (`serial_newer`).
#[allow(dead_code)] // documentation of the `wire` fields
pub type Stamp = u32;

/// Without `--features gnome` there is no D-Bus client: no window sync, the
/// 0.1.2 behaviour (`Client::spawn` says so).
#[cfg(not(feature = "gnome"))]
pub mod bus {
    pub use super::{Event, Request};

    /// Never made: `spawn` fails.
    pub enum Client {}

    impl Client {
        pub fn spawn(_display: u32) -> std::io::Result<Client> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this bridge was built without D-Bus (--features gnome)",
            ))
        }

        pub fn send(&self, _: Request) {
            match *self {}
        }

        pub fn fd(&self) -> std::os::fd::RawFd {
            match *self {}
        }

        pub fn events(&self) -> std::iter::Empty<Event> {
            match *self {}
        }
    }
}

/// The D-Bus client of the extension.
///
/// Wire contract, as contrib/gnome-shell/xwlbridge-windows@xwlbridge
/// serves it: bus name `BUS_NAME`, object `PATH`, interface `IFACE`.
/// - `Version() -> u` (at least `MIN_VERSION`), `Features() -> as` (empty
///   when mutter has no window tags; `move-resize` is required);
/// - `MoveResize(s tag, i x, i y, i w, i h, u flags) -> (iiii)`: the
///   rectangle applied (after clamping; a new size is the one asked for
///   until the client has drawn it);
/// - `SetInitialPosition(s tag, i x, i y)`, `Activate(s tag, u timestamp)`,
///   `Raise(s tag)`, `SetAbove(s tag, b)`, `SetDemandsAttention(s tag, b)`,
///   `Stick(s tag, b)`, `SetWorkspace(s tag, i index)`,
///   `Minimize(s tag, b)`;
/// - `GetState() -> a(sa{sv})`: the caller's windows bottom to top, each
///   with `rect` (iiii), `monitor` i, `workspace` i, `flags` u (`st` bits),
///   `serial` u (of its last `Changed`), `shown` b, `stack` u;
/// - `GetMonitors() -> a(i(iiii)(iiii)db)`: index, geometry, work area,
///   scale, primary;
/// - signals, sent only to the bus names of the process owning the window
///   (a caller becomes a recipient with its first call):
///   `Changed(s tag, (iiii) rect, i monitor, i workspace, u flags,
///   u serial)` (coalesced, one per window per main-loop turn, only when
///   something differs; the serial is one counter for all), `Restacked(as
///   tags)` (bottom to top), `MonitorsChanged()`, `WindowGone(s tag)`;
/// - errors: `org.xwlbridge.Windows1.Error.NoSuchWindow` for a tag that is
///   not a window of the caller's process.
#[cfg(feature = "gnome")]
pub mod bus {
    use std::collections::HashMap;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use zbus::blocking::{Connection, MessageIterator, Proxy};
    use zbus::message::Type as MsgType;
    use zbus::proxy::CacheProperties;
    use zbus::zvariant::OwnedValue;

    use super::{Rect, Xid, tag, tag_window};

    pub const BUS_NAME: &str = "org.xwlbridge.Windows";
    pub const PATH: &str = "/org/xwlbridge/Windows";
    pub const IFACE: &str = "org.xwlbridge.Windows1";
    /// The oldest interface revision this client works with; later ones only
    /// add (their additions are announced in `Features`).
    pub const MIN_VERSION: u32 = 1;
    /// The feature the window sync cannot do without.
    pub const REQUIRED_FEATURE: &str = "move-resize";

    /// Longest wait for a reply: gnome-shell answers in milliseconds, and a
    /// MoveResize stuck longer must not keep its window from syncing.
    const CALL_TIMEOUT: Duration = Duration::from_secs(2);

    pub use super::{Event, MonitorInfo, Request, WindowState};

    pub(super) type Quad = (i32, i32, i32, i32);
    pub(super) type ChangedBody = (String, Quad, i32, i32, u32, u32);
    pub(super) type StateBody = Vec<(String, HashMap<String, OwnedValue>)>;
    pub(super) type MonitorBody = (i32, Quad, Quad, f64, bool);

    fn rect(q: Quad) -> Rect {
        Rect::new(q.0, q.1, q.2, q.3)
    }

    /// `Changed` (of order stamp `wire`) as an event, if it is about a
    /// window of `display`.
    pub(super) fn changed_event(display: u32, b: ChangedBody, wire: u32) -> Option<Event> {
        let (t, r, monitor, workspace, flags, serial) = b;
        Some(Event::Changed {
            xid: tag_window(&t, display)?,
            rect: rect(r),
            monitor,
            workspace,
            flags,
            serial,
            wire,
        })
    }

    /// `GetState`'s reply as an event: our display's windows only (one
    /// without a `rect` is dropped; other missing keys read as 0/false).
    pub(super) fn state_event(display: u32, b: StateBody, wire: u32) -> Event {
        Event::State(
            b.into_iter()
                .filter_map(|(t, props)| {
                    let get = |k: &str| props.get(k).and_then(|v| v.try_clone().ok());
                    let int = |k: &str| get(k).and_then(|v| i32::try_from(v).ok()).unwrap_or(0);
                    let uint = |k: &str| get(k).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
                    Some(WindowState {
                        xid: tag_window(&t, display)?,
                        rect: rect(get("rect").and_then(|v| Quad::try_from(v).ok())?),
                        monitor: int("monitor"),
                        workspace: int("workspace"),
                        flags: uint("flags"),
                        serial: uint("serial"),
                        shown: get("shown")
                            .and_then(|v| bool::try_from(v).ok())
                            .unwrap_or(false),
                    })
                })
                .collect(),
            wire,
        )
    }

    pub(super) fn monitors_event(b: Vec<MonitorBody>) -> Event {
        Event::Monitors(
            b.into_iter()
                .map(|(index, r, wa, scale, primary)| MonitorInfo {
                    index,
                    rect: rect(r),
                    work_area: rect(wa),
                    scale,
                    primary,
                })
                .collect(),
        )
    }

    pub(super) fn restacked_event(display: u32, tags: Vec<String>, wire: u32) -> Event {
        Event::Restacked(
            tags.iter().filter_map(|t| tag_window(t, display)).collect(),
            wire,
        )
    }

    /// A message's order stamp (`super::Stamp`).
    fn wire(msg: &zbus::Message) -> u32 {
        msg.primary_header().serial_num().get()
    }

    /// An eventfd: readable while events wait.
    struct Wake(OwnedFd);

    impl Wake {
        fn new() -> std::io::Result<Wake> {
            let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Wake(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        fn wake(&self) {
            let one: u64 = 1;
            unsafe { libc::write(self.0.as_raw_fd(), (&one as *const u64).cast(), 8) };
        }

        fn clear(&self) {
            let mut v: u64 = 0;
            unsafe { libc::read(self.0.as_raw_fd(), (&mut v as *mut u64).cast(), 8) };
        }
    }

    /// The client's threads' inputs.
    enum Cmd {
        Req(Request),
        /// The bus name's owner changed (its new unique name, if any).
        Owner(Option<String>),
        /// The signal stream ended: the session bus is gone.
        BusLost,
        /// The client was dropped: close the connection (which ends the
        /// signal threads) and stop.
        Quit,
    }

    /// What the threads share.
    struct Shared {
        events: Sender<Event>,
        wake: Wake,
        available: AtomicBool,
        /// The extension's unique name while it is available: signals from
        /// anyone else are dropped (anyone may emit on our path).
        owner: Mutex<Option<String>>,
    }

    impl Shared {
        fn emit(&self, e: Event) {
            if self.events.send(e).is_ok() {
                self.wake.wake();
            }
        }
    }

    /// The client: `send` never blocks; events are drained with `events`
    /// when `fd` is readable (or at any time).
    pub struct Client {
        cmds: Sender<Cmd>,
        events: Receiver<Event>,
        shared: Arc<Shared>,
    }

    impl Client {
        /// Starts the client for display `:display` on the session bus. The
        /// connection is made on the client's thread, so this never waits for
        /// the bus.
        pub fn spawn(display: u32) -> std::io::Result<Client> {
            Client::spawn_at(display, None)
        }

        /// The same on the bus at `address` (tests: a private bus).
        pub fn spawn_at(display: u32, address: Option<String>) -> std::io::Result<Client> {
            let (cmds, cmd_rx) = channel();
            let (ev_tx, events) = channel();
            let shared = Arc::new(Shared {
                events: ev_tx,
                wake: Wake::new()?,
                available: AtomicBool::new(false),
                owner: Mutex::new(None),
            });
            let (s, tx) = (shared.clone(), cmds.clone());
            std::thread::Builder::new()
                .name("winsync".into())
                .spawn(move || {
                    Worker {
                        display,
                        shared: s,
                        conn: None,
                        proxy: None,
                        logged: Vec::new(),
                        announced: false,
                    }
                    .run(address, tx, cmd_rx)
                })?;
            Ok(Client {
                cmds,
                events,
                shared,
            })
        }

        /// Queues a request (dropped, or for a MoveResize answered with
        /// `Failed`, while the extension is unavailable).
        pub fn send(&self, r: Request) {
            let _ = self.cmds.send(Cmd::Req(r));
        }

        /// Whether the extension is there and compatible right now.
        #[allow(dead_code)] // the reverse thread follows Available/Unavailable
        pub fn available(&self) -> bool {
            self.shared.available.load(Ordering::Relaxed)
        }

        /// Readable while events wait (for the caller's poll).
        pub fn fd(&self) -> RawFd {
            self.shared.wake.0.as_raw_fd()
        }

        /// Every event waiting; never blocks.
        pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
            self.shared.wake.clear();
            self.events.try_iter()
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            let _ = self.cmds.send(Cmd::Quit);
        }
    }

    struct Worker {
        display: u32,
        shared: Arc<Shared>,
        conn: Option<Connection>,
        /// A proxy on the extension's unique name, while it is available.
        proxy: Option<Proxy<'static>>,
        /// Every reason it was unavailable that was logged: each is logged
        /// once (the extension goes and comes back with every screen lock),
        /// later ones only verbosely.
        logged: Vec<String>,
        /// Whether its being available was logged.
        announced: bool,
    }

    impl Worker {
        fn run(mut self, address: Option<String>, tx: Sender<Cmd>, rx: Receiver<Cmd>) {
            match connect(address.as_deref()) {
                Ok(conn) => {
                    // subscribe before asking, so an owner change can't
                    // fall between the answer and the subscription
                    match self.subscribe(&conn, tx) {
                        Ok(()) => {
                            let owner = name_owner(&conn);
                            self.conn = Some(conn);
                            self.owner_changed(owner);
                        }
                        Err(e) => self.unavailable(&format!("can't subscribe: {e}")),
                    }
                }
                Err(e) => self.unavailable(&format!("no session bus: {e}")),
            }
            for cmd in rx {
                match cmd {
                    Cmd::Req(r) => self.request(r),
                    Cmd::Owner(o) => self.owner_changed(o),
                    Cmd::BusLost => {
                        self.conn = None;
                        self.unavailable("lost the session bus");
                    }
                    Cmd::Quit => break,
                }
            }
            self.proxy = None;
            if let Some(conn) = self.conn.take() {
                let _ = conn.close();
            }
        }

        fn subscribe(&self, conn: &Connection, tx: Sender<Cmd>) -> zbus::Result<()> {
            let owners = zbus::MatchRule::builder()
                .msg_type(MsgType::Signal)
                .sender("org.freedesktop.DBus")?
                .interface("org.freedesktop.DBus")?
                .member("NameOwnerChanged")?
                .arg(0, BUS_NAME)?
                .build();
            let owners = MessageIterator::for_match_rule(owners, conn, Some(16))?;
            // the extension sends its signals to us alone; the rule also
            // takes broadcast ones (checked against the owner below)
            let signals = zbus::MatchRule::builder()
                .msg_type(MsgType::Signal)
                .path(PATH)?
                .interface(IFACE)?
                .build();
            let signals = MessageIterator::for_match_rule(signals, conn, Some(256))?;
            let tx2 = tx.clone();
            std::thread::Builder::new()
                .name("winsync-owner".into())
                .spawn(move || {
                    for msg in owners {
                        let Ok(msg) = msg else { continue };
                        let Ok((name, _old, new)) =
                            msg.body().deserialize::<(String, String, String)>()
                        else {
                            continue;
                        };
                        let new = (!new.is_empty()).then_some(new);
                        if name == BUS_NAME && tx2.send(Cmd::Owner(new)).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|e| zbus::Error::Failure(e.to_string()))?;
            let (shared, display) = (self.shared.clone(), self.display);
            std::thread::Builder::new()
                .name("winsync-signals".into())
                .spawn(move || {
                    for msg in signals {
                        let Ok(msg) = msg else { continue };
                        if let Some(e) = signal_event(&shared, display, &msg) {
                            shared.emit(e);
                        }
                    }
                    let _ = tx.send(Cmd::BusLost);
                })
                .map_err(|e| zbus::Error::Failure(e.to_string()))?;
            Ok(())
        }

        /// The extension's name has a new owner (or none): negotiate.
        fn owner_changed(&mut self, owner: Option<String>) {
            if self.shared.available.swap(false, Ordering::Relaxed) {
                self.shared.emit(Event::Unavailable);
            }
            *self.shared.owner.lock().unwrap() = None;
            self.proxy = None;
            let (Some(conn), Some(owner)) = (self.conn.as_ref(), owner) else {
                self.unavailable(
                    "the GNOME Shell extension xwlbridge-windows@xwlbridge is not running",
                );
                return;
            };
            // the name is free while the screen is locked (the extension is
            // disabled there), so any process may take it: only gnome-shell
            // itself is the extension
            match owner_is_the_shell(conn, &owner) {
                Ok(()) => {}
                Err(why) => return self.unavailable(&why),
            }
            let proxy = match zbus::blocking::proxy::Builder::<Proxy>::new(conn)
                .destination(owner.clone())
                .and_then(|b| b.path(PATH))
                .and_then(|b| b.interface(IFACE))
                .map(|b| b.cache_properties(CacheProperties::No))
                .and_then(|b| b.build())
            {
                Ok(p) => p,
                Err(e) => return self.unavailable(&format!("can't reach the extension: {e}")),
            };
            // the first call also makes us a recipient of its signals
            let version = match proxy.call::<_, _, u32>("Version", &()) {
                Ok(v) => v,
                Err(e) => return self.unavailable(&format!("can't read its Version: {e}")),
            };
            if version < MIN_VERSION {
                return self.unavailable(&format!(
                    "the extension is version {version}, this bridge needs {MIN_VERSION} or later"
                ));
            }
            let features = match proxy.call::<_, _, Vec<String>>("Features", &()) {
                Ok(f) => f,
                Err(e) => return self.unavailable(&format!("can't read its Features: {e}")),
            };
            if !features.iter().any(|f| f == REQUIRED_FEATURE) {
                return self.unavailable(&format!(
                    "the extension can't move windows here: its features are {}, mutter has no window tags?",
                    if features.is_empty() {
                        "none".to_string()
                    } else {
                        features.join(",")
                    }
                ));
            }
            let msg = format!(
                "-showwindows: window sync with the desktop: extension version {version}, features {}",
                features.join(",")
            );
            if std::mem::replace(&mut self.announced, true) {
                crate::vlog!("{msg}");
            } else {
                crate::log!("{msg}");
            }
            self.proxy = Some(proxy);
            self.shared.available.store(true, Ordering::Relaxed);
            // Available goes out before its signals are let through, so none
            // of them precedes it; what changes in between is in the
            // GetState the caller asks for on Available (this thread
            // answers it after this returns)
            self.shared.emit(Event::Available { version, features });
            *self.shared.owner.lock().unwrap() = Some(owner);
        }

        /// Not available (each reason logged once).
        fn unavailable(&mut self, why: &str) {
            if self.shared.available.swap(false, Ordering::Relaxed) {
                self.shared.emit(Event::Unavailable);
            }
            self.proxy = None;
            *self.shared.owner.lock().unwrap() = None;
            if !self.logged.iter().any(|l| l == why) {
                crate::log!(
                    "-showwindows: no window sync with the desktop ({why}): windows keep the 0.1.2 behaviour"
                );
                self.logged.push(why.to_string());
            } else {
                crate::vlog!("-showwindows: window sync still unavailable: {why}");
            }
        }

        fn request(&mut self, r: Request) {
            let method = r.method();
            let Some(proxy) = self.proxy.as_ref() else {
                if let Request::MoveResize { xid, .. } = r {
                    self.shared.emit(Event::Failed {
                        xid,
                        method,
                        error: "the extension is not available".into(),
                        answered: true,
                    });
                }
                return;
            };
            let d = self.display;
            let t = |xid: Xid| tag(d, xid);
            let none = |_: ()| None;
            let result: zbus::Result<Option<Event>> = match r {
                Request::MoveResize {
                    xid,
                    rect: req,
                    flags,
                } => proxy
                    .call_method(method, &(t(xid), req.x, req.y, req.w, req.h, flags))
                    .and_then(|m| {
                        let q: Quad = m.body().deserialize()?;
                        Ok(Some(Event::Applied {
                            xid,
                            requested: req,
                            applied: rect(q),
                            wire: wire(&m),
                        }))
                    }),
                Request::SetInitialPosition { xid, x, y } => {
                    proxy.call(method, &(t(xid), x, y)).map(none)
                }
                Request::Activate { xid, timestamp } => {
                    proxy.call(method, &(t(xid), timestamp)).map(none)
                }
                Request::Raise { xid } => proxy.call(method, &(t(xid),)).map(none),
                Request::SetAbove { xid, on }
                | Request::SetDemandsAttention { xid, on }
                | Request::Stick { xid, on }
                | Request::Minimize { xid, on } => proxy.call(method, &(t(xid), on)).map(none),
                Request::SetWorkspace { xid, index } => {
                    proxy.call(method, &(t(xid), index)).map(none)
                }
                Request::GetState => proxy.call_method(method, &()).and_then(|m| {
                    let b: StateBody = m.body().deserialize()?;
                    Ok(Some(state_event(d, b, wire(&m))))
                }),
                Request::GetMonitors => proxy
                    .call::<_, _, Vec<MonitorBody>>(method, &())
                    .map(|b| Some(monitors_event(b))),
            };
            match result {
                Ok(Some(e)) => self.shared.emit(e),
                Ok(None) => {}
                Err(e) => {
                    crate::vlog!("-showwindows: window sync: {method} failed: {e}");
                    if let Some(xid) = r.xid() {
                        self.shared.emit(Event::Failed {
                            xid,
                            method,
                            error: e.to_string(),
                            answered: matches!(
                                e,
                                zbus::Error::MethodError(..) | zbus::Error::FDO(_)
                            ),
                        });
                    }
                }
            }
        }
    }

    fn connect(address: Option<&str>) -> zbus::Result<Connection> {
        let b = match address {
            Some(a) => zbus::blocking::connection::Builder::address(a)?,
            None => zbus::blocking::connection::Builder::session()?,
        };
        b.method_timeout(CALL_TIMEOUT).build()
    }

    fn name_owner(conn: &Connection) -> Option<String> {
        owner_of(conn, BUS_NAME)
    }

    fn owner_of(conn: &Connection, name: &str) -> Option<String> {
        let dbus = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
        let name = zbus::names::BusName::try_from(name).ok()?;
        dbus.get_name_owner(name).ok().map(|n| n.to_string())
    }

    /// The name gnome-shell owns on the session bus.
    pub const SHELL_NAME: &str = "org.gnome.Shell";

    /// Whether `owner` (the unique name owning `BUS_NAME`) is gnome-shell's
    /// connection: the extension runs inside gnome-shell, and serves the
    /// name on gnome-shell's own connection. Otherwise the reason, naming
    /// the process that holds the name.
    fn owner_is_the_shell(conn: &Connection, owner: &str) -> Result<(), String> {
        let shell = owner_of(conn, SHELL_NAME);
        if shell.as_deref() == Some(owner) {
            return Ok(());
        }
        let pid = zbus::blocking::fdo::DBusProxy::new(conn)
            .ok()
            .and_then(|d| {
                let n = zbus::names::BusName::try_from(owner.to_string()).ok()?;
                d.get_connection_unix_process_id(n).ok()
            })
            .map_or_else(|| "unknown".to_string(), |p| p.to_string());
        Err(match shell {
            Some(_) => format!(
                "{BUS_NAME} is held by {owner} (pid {pid}), which is not gnome-shell: refusing it"
            ),
            None => format!(
                "{BUS_NAME} is held by {owner} (pid {pid}) and gnome-shell is not on this bus: refusing it"
            ),
        })
    }

    /// A signal of the extension as an event: only from the current owner,
    /// and only about our display's windows.
    fn signal_event(shared: &Shared, display: u32, msg: &zbus::Message) -> Option<Event> {
        let hdr = msg.header();
        let sender = hdr.sender()?.as_str();
        if shared.owner.lock().unwrap().as_deref() != Some(sender) {
            return None;
        }
        let body = msg.body();
        let stamp = wire(msg);
        match hdr.member()?.as_str() {
            "Changed" => changed_event(display, body.deserialize::<ChangedBody>().ok()?, stamp),
            "Restacked" => Some(restacked_event(
                display,
                body.deserialize::<Vec<String>>().ok()?,
                stamp,
            )),
            "MonitorsChanged" => Some(Event::MonitorsChanged),
            "WindowGone" => Some(Event::WindowGone(tag_window(
                &body.deserialize::<String>().ok()?,
                display,
            )?)),
            _ => None,
        }
    }
}

#[cfg(test)]
impl Geometry {
    /// The reply to the request in flight, with no order stamp.
    fn reply(&mut self, xid: Xid, applied: Rect) -> Option<Rect> {
        let requested = self.wins.get(&xid)?.in_flight?;
        self.applied(xid, requested, applied, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R0: Rect = Rect::new(100, 100, 400, 300);
    const XID: Xid = 0x1a0_0003;

    fn t0() -> Instant {
        Instant::now()
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    /// A tracked window whose desktop rectangle is known (`resync`).
    fn synced(rect: Rect) -> Geometry {
        let mut g = Geometry::new();
        g.track(XID, rect);
        assert_eq!(g.resync(XID, rect, 0, 10), None);
        g
    }

    fn r(x: i32, y: i32) -> Rect {
        Rect::new(x, y, R0.w, R0.h)
    }

    // ---- tags ----

    #[test]
    fn tag_is_display_and_hex_window() {
        assert_eq!(tag(2, 0x1a00003), "xwlbridge:2:0x1a00003");
        assert_eq!(tag(0, 0xffff_ffff), "xwlbridge:0:0xffffffff");
        assert_eq!(parse_tag("xwlbridge:2:0x1a00003"), Some((2, 0x1a00003)));
        for (d, x) in [
            (0, 1),
            (2, 0x200001),
            (99, 0xdeadbeef),
            (u32::MAX, u32::MAX),
        ] {
            assert_eq!(parse_tag(&tag(d, x)), Some((d, x)), "round trip");
        }
    }

    #[test]
    fn only_canonical_tags_parse() {
        for bad in [
            "",
            "xwlbridge:",
            "xwlbridge:2",
            "xwlbridge:2:",
            "xwlbridge::0x1",
            "xwlbridge:2:1a00003",     // no 0x
            "xwlbridge:2:0X1A00003",   // upper case
            "xwlbridge:2:0x1A00003",   // upper case digits
            "xwlbridge:2:0x01a00003",  // leading zero
            "xwlbridge:02:0x1a00003",  // leading zero in the display
            "xwlbridge:2:0x0",         // no window
            "xwlbridge:2:0x",          // empty
            "xwlbridge:2:0x100000000", // more than 32 bits
            "xwlbridge:-2:0x1",
            "xwlbridge:+2:0x1",
            "xwlbridge:2:0x1:extra",
            "xwlbridge:2:0x1 ",
            "xwlbridge:4294967296:0x1", // display out of range
            "Xwlbridge:2:0x1",
            "other:2:0x1",
            "x11:0x1a00003", // GNOME's own Xwayland windows
        ] {
            assert_eq!(parse_tag(bad), None, "{bad:?}");
        }
        assert_eq!(parse_tag("xwlbridge:0:0x1"), Some((0, 1)));
    }

    #[test]
    fn tags_of_other_displays_are_not_ours() {
        assert_eq!(tag_window("xwlbridge:2:0x1a00003", 2), Some(0x1a00003));
        assert_eq!(tag_window("xwlbridge:3:0x1a00003", 2), None);
        assert_eq!(tag_window("garbage", 2), None);
    }

    #[test]
    fn serials_wrap() {
        assert!(serial_newer(2, 1));
        assert!(!serial_newer(1, 2));
        assert!(!serial_newer(5, 5));
        assert!(serial_newer(0, u32::MAX), "0 follows u32::MAX");
        assert!(serial_newer(3, u32::MAX - 3));
        assert!(!serial_newer(u32::MAX, 0));
    }

    // ---- X -> desktop: coalescing ----

    #[test]
    fn x_moves_coalesce_latest_wins_one_in_flight() {
        let mut g = synced(R0);
        let t = t0();
        for (i, x) in [110, 120, 130].into_iter().enumerate() {
            assert_eq!(g.x_configured(XID, r(x, 100), i as u64 + 1, t), None);
        }
        // one request per frame, with the latest rectangle
        let m = g.frame(XID).expect("a move to send");
        assert_eq!(m.rect, r(130, 100));
        assert_eq!(m.flags, mr::CLAMP);
        assert!(g.in_flight(XID));
        // more X moves while it is in flight: nothing more goes out
        g.x_configured(XID, r(140, 100), 4, t);
        g.x_configured(XID, r(150, 100), 5, t);
        assert_eq!(g.frame(XID), None, "at most one in flight per window");
        assert_eq!(g.frame(XID), None);
        // the reply: applied as asked; the next frame sends the latest
        assert_eq!(g.reply(XID, r(130, 100)), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(150, 100)));
        assert_eq!(g.reply(XID, r(150, 100)), None);
        assert_eq!(g.frame(XID), None, "nothing left");
    }

    #[test]
    fn frames_without_moves_send_nothing() {
        let mut g = synced(R0);
        assert_eq!(g.frame(XID), None);
        assert_eq!(g.frame(0xdead), None, "untracked");
        // X moves and comes back before the frame: the desktop is there
        let t = t0();
        g.x_configured(XID, r(200, 100), 1, t);
        g.x_configured(XID, R0, 2, t);
        assert_eq!(g.frame(XID), None);
    }

    #[test]
    fn windows_coalesce_independently() {
        let mut g = synced(R0);
        g.track(7, R0);
        g.resync(7, R0, 0, 10);
        let t = t0();
        g.x_configured(XID, r(1, 1), 1, t);
        g.x_configured(7, r(2, 2), 2, t);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(1, 1)));
        assert_eq!(g.frame(7).map(|m| m.rect), Some(r(2, 2)), "one each");
    }

    #[test]
    fn a_held_button_makes_the_move_a_user_op() {
        let mut g = synced(R0);
        g.button(XID, true);
        g.x_configured(XID, r(300, 100), 1, t0());
        // mutter's user-move constraints, not the work-area clamp (which
        // would keep a self-drag from crossing to another monitor)
        assert_eq!(g.frame(XID).map(|m| m.flags), Some(mr::USER));
        g.button(XID, false);
        g.reply(XID, r(300, 100));
        g.x_configured(XID, r(310, 100), 2, t0());
        assert_eq!(g.frame(XID).map(|m| m.flags), Some(mr::CLAMP));
    }

    #[test]
    fn a_failed_move_frees_the_window() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(1, 1), 1, t);
        assert!(g.frame(XID).is_some());
        g.failed(XID, true);
        assert!(!g.in_flight(XID));
        assert_eq!(g.frame(XID), None, "dropped, not retried every frame");
        g.x_configured(XID, r(2, 2), 2, t);
        assert_eq!(
            g.frame(XID).map(|m| m.rect),
            Some(r(2, 2)),
            "the next move goes"
        );
        // a reply nobody waits for is ignored
        g.failed(XID, true);
        assert_eq!(g.reply(XID, r(2, 2)), None);
    }

    #[test]
    fn untracked_windows_are_ignored() {
        let mut g = Geometry::new();
        let t = t0();
        assert_eq!(g.x_configured(1, R0, 1, t), None);
        assert_eq!(g.changed(1, R0, st::USER, 1, t), None);
        assert_eq!(g.reply(1, R0), None);
        assert_eq!(g.button(1, true), None);
        assert_eq!(g.resync(1, R0, 0, 1), None);
        g.x_sent(1, R0, 1);
        g.failed(1, true);
        assert!(!g.is_tracked(1));
        g.track(1, R0);
        assert!(g.is_tracked(1));
        g.forget(1);
        assert!(!g.is_tracked(1));
    }

    // ---- echo suppression ----

    #[test]
    fn the_desktops_echo_of_our_move_is_dropped() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(200, 100), 1, t);
        g.frame(XID).unwrap();
        // mutter reports the move before the reply: ours, in flight
        assert_eq!(g.changed(XID, r(200, 100), 0, 11, t), None);
        assert_eq!(g.reply(XID, r(200, 100)), None);
        // and again after it (a later size-changed, same rectangle)
        assert_eq!(g.changed(XID, r(200, 100), 0, 12, t), None);
        assert_eq!(g.frame(XID), None, "converged after one round trip");
    }

    #[test]
    fn an_unflagged_echo_is_dropped_by_its_rectangle() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(200, 100), 1, t);
        g.frame(XID).unwrap();
        g.reply(XID, r(200, 100));
        // X moved on, then the late echo of the applied rectangle comes
        g.x_configured(XID, r(250, 100), 2, t);
        assert_eq!(
            g.changed(XID, r(200, 100), 0, 12, t),
            None,
            "not a desktop move"
        );
        assert_eq!(
            g.frame(XID).map(|m| m.rect),
            Some(r(250, 100)),
            "X's move still goes"
        );
    }

    #[test]
    fn stale_and_duplicate_desktop_reports_are_dropped() {
        let mut g = synced(R0);
        let t = t0();
        assert_eq!(g.changed(XID, r(5, 5), 0, 20, t), Some(r(5, 5)));
        g.x_sent(XID, r(5, 5), 1);
        assert_eq!(g.changed(XID, r(9, 9), 0, 20, t), None, "duplicate serial");
        assert_eq!(g.changed(XID, r(9, 9), st::USER, 19, t), None, "older");
        assert_eq!(g.changed(XID, r(9, 9), 0, 21, t), Some(r(9, 9)), "newer");
    }

    #[test]
    fn our_configure_notify_is_dropped() {
        let mut g = synced(R0);
        let t = t0();
        // the user moved it on the desktop; we configure X (request 50)
        assert_eq!(
            g.changed(XID, r(300, 300), st::USER, 11, t),
            Some(r(300, 300))
        );
        g.x_sent(XID, r(300, 300), 50);
        // the ConfigureNotify our request generates
        assert_eq!(g.x_configured(XID, r(300, 300), 50, ms(t, 500)), None);
        assert_eq!(g.frame(XID), None, "no echo back to the desktop");
    }

    #[test]
    fn x_events_from_before_our_configure_are_stale() {
        let mut g = synced(R0);
        let t = t0();
        // the client moved itself (request A, not yet seen by us) while we
        // configured it from the desktop (request 50): A's event is older
        g.changed(XID, r(300, 300), 0, 11, t);
        g.x_sent(XID, r(300, 300), 50);
        assert_eq!(g.x_configured(XID, r(120, 100), 49, t), None);
        assert_eq!(g.frame(XID), None, "a frame between them sends nothing");
        assert_eq!(g.x_configured(XID, r(300, 300), 50, t), None);
        assert_eq!(g.frame(XID), None, "X's state is ours: nothing to send");
    }

    #[test]
    fn a_client_move_after_our_configure_goes_to_the_desktop() {
        let mut g = synced(R0);
        let t = t0();
        g.changed(XID, r(300, 300), 0, 11, t);
        g.x_sent(XID, r(300, 300), 50);
        g.x_configured(XID, r(300, 300), 50, t);
        assert_eq!(g.x_configured(XID, r(310, 300), 51, t), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(310, 300)));
    }

    #[test]
    fn a_configure_from_the_desktop_supersedes_a_pending_x_move() {
        // the toplevel's configure resizes the X window (the bridge's own
        // ConfigureWindow) before the frame: X is where the desktop wants it
        let mut g = synced(R0);
        g.x_configured(XID, r(150, 100), 1, t0());
        let big = Rect::new(100, 100, 800, 600);
        g.x_sent(XID, big, 2);
        assert_eq!(g.frame(XID), None);
        assert_eq!(g.x_configured(XID, big, 2, t0()), None, "its echo");
    }

    #[test]
    fn a_missing_echo_does_not_swallow_the_next_move() {
        // the X server generates nothing for a no-op ConfigureWindow: the
        // first event after it is the client's
        let mut g = synced(R0);
        let t = t0();
        g.x_sent(XID, R0, 50);
        assert_eq!(g.x_configured(XID, r(1, 2), 52, t), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(1, 2)));
    }

    #[test]
    fn restacks_and_border_changes_are_no_moves() {
        let mut g = synced(R0);
        assert_eq!(g.x_configured(XID, R0, 1, t0()), None);
        assert_eq!(g.frame(XID), None);
    }

    // ---- authority ----

    #[test]
    fn x_is_authoritative_while_a_button_is_held() {
        let mut g = synced(R0);
        let t = t0();
        g.button(XID, true);
        g.x_configured(XID, r(150, 100), 1, t);
        let m = g.frame(XID).unwrap();
        assert_eq!(m.rect, r(150, 100));
        // a desktop change while held (even a user one) does not move X
        assert_eq!(g.changed(XID, r(0, 0), 0, 11, t), None);
        assert_eq!(g.changed(XID, r(0, 0), st::USER, 12, t), None);
        assert_eq!(g.reply(XID, r(150, 100)), None);
        g.x_configured(XID, r(160, 100), 2, t);
        assert_eq!(
            g.frame(XID).map(|m| m.rect),
            Some(r(160, 100)),
            "X keeps driving"
        );
    }

    #[test]
    fn on_release_the_desktop_rules_again() {
        let mut g = synced(R0);
        let t = t0();
        g.button(XID, true);
        g.x_configured(XID, r(150, 100), 1, t);
        g.frame(XID).unwrap();
        g.reply(XID, r(150, 100));
        // a desktop change while held is kept for the release
        assert_eq!(g.changed(XID, r(500, 500), 0, 12, t), None);
        assert_eq!(g.button(XID, false), Some(r(500, 500)));
        // a release with nothing pending and the desktop where X is: nothing
        let mut g = synced(R0);
        g.button(XID, true);
        assert_eq!(g.button(XID, false), None);
        assert_eq!(g.button(XID, false), None, "a second release is no release");
    }

    #[test]
    fn a_release_with_a_move_still_to_land_waits_for_it() {
        let mut g = synced(R0);
        let t = t0();
        g.button(XID, true);
        g.x_configured(XID, r(150, 100), 1, t);
        g.frame(XID).unwrap();
        assert_eq!(g.button(XID, false), None, "in flight");
        assert_eq!(g.reply(XID, r(150, 100)), None);
        g.button(XID, true);
        g.x_configured(XID, r(160, 100), 2, t);
        assert_eq!(g.button(XID, false), None, "pending");
        assert_eq!(
            g.frame(XID).map(|m| m.flags),
            Some(mr::CLAMP),
            "released: no user op"
        );
    }

    #[test]
    fn a_user_change_cancels_a_pending_x_move() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(150, 100), 1, t);
        // before the frame, the user tiles the window
        let tiled = Rect::new(0, 32, 960, 1048);
        assert_eq!(g.changed(XID, tiled, st::USER, 11, t), Some(tiled));
        assert_eq!(g.frame(XID), None, "the X move was cancelled");
        g.x_sent(XID, tiled, 60);
        assert_eq!(g.frame(XID), None);
        assert_eq!(g.x_configured(XID, tiled, 60, t), None);
    }

    #[test]
    fn x_moves_during_a_user_drag_are_put_back() {
        let mut g = synced(R0);
        let t = t0();
        assert_eq!(
            g.changed(XID, r(400, 400), st::USER, 11, t),
            Some(r(400, 400))
        );
        g.x_sent(XID, r(400, 400), 60);
        g.x_configured(XID, r(400, 400), 60, t);
        // the client moves itself mid-drag: the desktop wins
        assert_eq!(
            g.x_configured(XID, r(10, 10), 61, ms(t, 100)),
            Some(r(400, 400))
        );
        g.x_sent(XID, r(400, 400), 62);
        assert_eq!(g.frame(XID), None);
        g.x_configured(XID, r(400, 400), 62, ms(t, 150));
        // the grab ends; just after, the desktop still wins
        assert_eq!(g.changed(XID, r(400, 400), 0, 12, ms(t, 200)), None);
        assert_eq!(
            g.x_configured(XID, r(10, 10), 63, ms(t, 250)),
            Some(r(400, 400))
        );
        g.x_sent(XID, r(400, 400), 64);
        // after the grace, the client's moves go through again
        assert_eq!(g.x_configured(XID, r(10, 10), 65, ms(t, 600)), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(10, 10)));
    }

    #[test]
    fn a_user_change_while_ours_is_in_flight_is_restored_if_ours_lands_on_top() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(150, 100), 1, t);
        g.frame(XID).unwrap();
        // the user drags it on the desktop (a grab) before our move lands
        let max = Rect::new(0, 32, 1920, 1048);
        assert_eq!(g.changed(XID, max, st::USER, 11, t), Some(max));
        g.x_sent(XID, max, 70);
        assert_eq!(g.reply(XID, r(150, 100)), None, "X stays with the user");
        // the user's result goes back to the desktop
        assert_eq!(
            g.frame(XID),
            Some(MoveResize {
                rect: max,
                flags: mr::CLAMP | mr::USER
            })
        );
        assert_eq!(g.reply(XID, max), None);
        assert_eq!(g.frame(XID), None, "converged");
    }

    #[test]
    fn a_desktop_change_before_ours_landed_is_overtaken() {
        // the extension's reply is newer than any Changed before it
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(150, 100), 1, t);
        g.frame(XID).unwrap();
        assert_eq!(g.changed(XID, r(20, 20), 0, 11, t), None, "held back");
        assert_eq!(g.reply(XID, r(150, 100)), None, "ours is on top");
        assert_eq!(g.changed(XID, r(150, 100), 0, 12, t), None, "its echo");
        assert_eq!(g.frame(XID), None);
    }

    #[test]
    fn a_grab_keeps_the_desktop_authoritative_until_it_ends() {
        let mut g = synced(R0);
        let t = t0();
        // grab begins (state only), the user drags
        assert_eq!(g.changed(XID, R0, st::USER, 11, t), None);
        assert_eq!(
            g.changed(XID, r(300, 300), st::USER, 12, t),
            Some(r(300, 300))
        );
        g.x_sent(XID, r(300, 300), 5);
        // the client moves itself long after the last report: still put back
        assert_eq!(
            g.x_configured(XID, r(1, 1), 6, ms(t, 5000)),
            Some(r(300, 300))
        );
        g.x_sent(XID, r(300, 300), 7);
        // the grab ends: the client's moves go through again
        assert_eq!(g.changed(XID, r(300, 300), 0, 13, ms(t, 5000)), None);
        assert_eq!(g.x_configured(XID, r(1, 1), 8, ms(t, 6000)), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(1, 1)));
    }

    #[test]
    fn a_desktop_change_on_its_own_moves_x() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(150, 100), 1, t);
        // not the user, not ours: mutter moved it (a monitor went)
        assert_eq!(g.changed(XID, r(0, 0), 0, 11, t), Some(r(0, 0)));
        g.x_sent(XID, r(0, 0), 80);
        assert_eq!(g.frame(XID), None, "the pending X move gave way");
        // one that only confirms X's rectangle does nothing
        assert_eq!(g.changed(XID, r(0, 0), 0, 12, t), None);
    }

    // ---- clamping ----

    #[test]
    fn a_clamped_move_puts_x_back() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, r(-50, 100), 1, t);
        g.frame(XID).unwrap();
        // the desktop kept it inside the work area
        assert_eq!(g.reply(XID, r(0, 100)), Some(r(0, 100)));
        g.x_sent(XID, r(0, 100), 90);
        assert_eq!(g.x_configured(XID, r(0, 100), 90, t), None, "our own");
        assert_eq!(g.frame(XID), None);
        assert_eq!(g.changed(XID, r(0, 100), 0, 12, t), None, "the echo");
    }

    #[test]
    fn a_clamped_size_puts_x_back() {
        let mut g = synced(R0);
        let t = t0();
        g.x_configured(XID, Rect::new(100, 100, 5000, 300), 1, t);
        g.frame(XID).unwrap();
        let fit = Rect::new(100, 100, 1820, 300);
        assert_eq!(g.reply(XID, fit), Some(fit));
    }

    #[test]
    fn a_clamped_move_with_a_newer_x_move_waits_for_that() {
        // a client drag pushing past the edge: no back-and-forth per frame,
        // the latest request is clamped in turn
        let mut g = synced(R0);
        let t = t0();
        g.button(XID, true);
        g.x_configured(XID, r(-10, 100), 1, t);
        g.frame(XID).unwrap();
        g.x_configured(XID, r(-20, 100), 2, t);
        assert_eq!(g.reply(XID, r(0, 100)), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(-20, 100)));
        // the last one: X goes back to the edge, as under an X WM
        assert_eq!(g.reply(XID, r(0, 100)), Some(r(0, 100)));
    }

    #[test]
    fn an_unclamped_reply_moves_nothing() {
        let mut g = synced(R0);
        g.x_configured(XID, r(1, 1), 1, t0());
        g.frame(XID).unwrap();
        assert_eq!(g.reply(XID, r(1, 1)), None);
    }

    // ---- resync ----

    #[test]
    fn a_resync_makes_x_follow_the_desktop_unless_held() {
        let mut g = Geometry::new();
        g.track(XID, R0);
        assert_eq!(g.resync(XID, r(7, 7), 0, 100), Some(r(7, 7)));
        g.x_sent(XID, r(7, 7), 1);
        assert_eq!(g.resync(XID, r(7, 7), 0, 101), None);
        // a move in flight across a resync waits for its reply
        g.x_configured(XID, r(8, 8), 2, t0());
        g.frame(XID).unwrap();
        assert_eq!(g.resync(XID, r(7, 7), 0, 102), None);
        assert!(g.in_flight(XID));
        assert_eq!(g.reply(XID, r(8, 8)), None);
        // a reconnect voids it, and the stamps restart
        g.x_configured(XID, r(6, 6), 3, t0());
        g.frame(XID).unwrap();
        g.reset();
        assert_eq!(
            g.resync(XID, r(7, 7), 0, 1),
            Some(r(7, 7)),
            "stamps restart"
        );
        assert!(!g.in_flight(XID));
        // held: X's rectangle is pushed instead
        g.x_sent(XID, r(7, 7), 3);
        g.button(XID, true);
        g.x_configured(XID, r(9, 9), 4, t0());
        assert_eq!(g.resync(XID, r(7, 7), 0, 5), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(9, 9)));
    }

    // ---- review fixes (research/release/p1/p1-fix.md) ----

    /// The desktop restores the rectangle our last MoveResize got (an
    /// unmaximise, an untile): X follows it (B1).
    #[test]
    fn a_desktop_restore_to_our_last_rectangle_moves_x() {
        let t = t0();
        let a = Rect::new(300, 200, 212, 192);
        let mut g = synced(Rect::new(50, 60, 212, 192));
        assert_eq!(g.x_configured(XID, a, 10, t), None);
        let m = g.frame(XID).expect("sent");
        assert_eq!(g.reply(XID, m.rect), None);
        assert_eq!(g.changed(XID, a, 0, 11, t), None, "its echo");
        // maximised: the configure resized X, the Changed moves it
        let max = Rect::new(0, 32, 1280, 688);
        g.x_sent(XID, max, 20);
        assert_eq!(g.x_configured(XID, max, 20, t), None);
        assert_eq!(
            g.changed(XID, max, st::MAXIMIZED, 12, t),
            None,
            "X is there"
        );
        // unmaximised: the size comes back first, then the position
        let back = Rect::new(0, 32, 212, 192);
        g.x_sent(XID, back, 30);
        assert_eq!(g.x_configured(XID, back, 30, t), None);
        assert_eq!(g.changed(XID, a, 0, 13, t), Some(a), "X follows it back");
    }

    /// A state-only report (focus, keep-above) while X's own move waits
    /// does not put X back.
    #[test]
    fn a_state_change_does_not_undo_a_pending_x_move() {
        let t = t0();
        let mut g = synced(R0);
        assert_eq!(g.x_configured(XID, r(300, 100), 1, t), None);
        assert_eq!(g.changed(XID, R0, st::FOCUSED, 11, t), None);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(300, 100)));
    }

    /// A GetState computed before a Changed and delivered after it does
    /// not move X back (M6).
    #[test]
    fn an_older_resync_does_not_rewind_a_newer_changed() {
        let t = t0();
        let mut g = synced(r(50, 60));
        let newer = r(700, 300);
        assert_eq!(g.changed(XID, newer, 0, 20, t), Some(newer));
        g.x_sent(XID, newer, 5);
        assert_eq!(g.resync(XID, r(600, 300), 0, 19), None);
        assert_eq!(g.desk(XID), Some(newer));
    }

    /// The reply to our move delivered after the echo of a later move:
    /// the later state stands (M6).
    #[test]
    fn a_reply_older_than_a_report_settles_only_its_request() {
        let t = t0();
        let mut g = synced(R0);
        g.x_configured(XID, r(200, 100), 1, t);
        let m = g.frame(XID).unwrap();
        // the desktop applied ours (stamp 20), then moved it on its own
        // (stamp 21); the signal thread is quicker than the worker
        assert_eq!(g.changed(XID, r(0, 0), 0, 21, t), None, "ours in flight");
        assert_eq!(
            g.applied(XID, m.rect, m.rect, Some(20)),
            Some(r(0, 0)),
            "X follows the newer report"
        );
        assert!(!g.in_flight(XID));
        // the echo of an earlier position after a newer reply is stale
        g.x_sent(XID, r(0, 0), 2);
        g.x_configured(XID, r(40, 0), 3, t);
        let m = g.frame(XID).unwrap();
        assert_eq!(g.applied(XID, m.rect, m.rect, Some(30)), None);
        assert_eq!(g.changed(XID, r(0, 0), 0, 29, t), None, "stale");
        assert_eq!(g.desk(XID), Some(r(40, 0)));
    }

    /// A reply is matched to its request: one that is not in flight is
    /// ignored.
    #[test]
    fn a_reply_to_another_request_is_ignored() {
        let mut g = synced(R0);
        g.x_configured(XID, r(10, 0), 1, t0());
        let m = g.frame(XID).unwrap();
        assert_eq!(g.applied(XID, r(99, 99), r(99, 99), None), None);
        assert!(g.in_flight(XID), "still waiting for its own");
        assert_eq!(g.applied(XID, m.rect, m.rect, None), None);
        assert!(!g.in_flight(XID));
    }

    /// While the user has the window, a client that re-asserts its
    /// position is put back a few times per desktop report, not at X
    /// speed, and only its position (M8).
    #[test]
    fn put_backs_during_a_grab_are_bounded_and_keep_the_clients_size() {
        let t = t0();
        let mut g = synced(r(400, 300));
        let user = r(600, 300);
        assert_eq!(g.changed(XID, user, st::USER, 11, t), Some(user));
        g.x_sent(XID, user, 1);
        let mut n = 0;
        for i in 0..1000u64 {
            if let Some(back) = g.x_configured(XID, r(400, 300), 2 + 2 * i, t) {
                n += 1;
                g.x_sent(XID, back, 3 + 2 * i);
            }
        }
        assert_eq!(n, usize::from(PUT_BACKS));
        // the next report allows a few more
        assert_eq!(
            g.changed(XID, r(610, 300), st::USER, 12, t),
            Some(r(610, 300))
        );
        g.x_sent(XID, r(610, 300), 3000);
        // a resize of its own during the user's move keeps its size
        let grown = Rect::new(400, 300, R0.w + 50, R0.h);
        assert_eq!(
            g.x_configured(XID, grown, 3001, t),
            Some(Rect::new(610, 300, R0.w + 50, R0.h))
        );
        // the grab ends where the client left X: X follows the desktop
        g.x_sent(XID, Rect::new(610, 300, R0.w + 50, R0.h), 3002);
        for i in 0..10u64 {
            if let Some(back) = g.x_configured(XID, r(400, 300), 3003 + 2 * i, t) {
                g.x_sent(XID, back, 3004 + 2 * i);
            }
        }
        assert_eq!(g.x_rect(XID), Some(r(400, 300)), "it gave up");
        assert_eq!(g.changed(XID, r(610, 300), 0, 13, t), Some(r(610, 300)));
        // after the grace, a client that insists wins
        g.x_sent(XID, r(610, 300), 4000);
        let later = ms(t, 50);
        let mut back = 0;
        for i in 0..10u64 {
            if let Some(b) = g.x_configured(XID, r(400, 300), 4001 + 2 * i, later) {
                back += 1;
                g.x_sent(XID, b, 4002 + 2 * i);
            }
        }
        assert!(back <= usize::from(PUT_BACKS));
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(400, 300)));
    }

    /// A MoveResize that times out may still land: its late report is
    /// ours, and X's newer position goes next (fallback review).
    #[test]
    fn a_timed_out_move_that_lands_late_is_ours() {
        let t = t0();
        let mut g = synced(R0);
        g.x_configured(XID, r(100, 300), 1, t);
        let a = g.frame(XID).unwrap();
        g.x_configured(XID, r(500, 300), 2, t);
        g.failed(XID, false); // timed out
        assert_eq!(g.changed(XID, a.rect, 0, 11, t), None, "no X move");
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(500, 300)));
        // an error reply did nothing: forget it
        let mut g = synced(R0);
        g.x_configured(XID, r(100, 300), 1, t);
        g.frame(XID).unwrap();
        g.failed(XID, true);
        assert!(!g.pending(XID) && !g.in_flight(XID));
        // a timeout with X still where it asked: sent again (a no-op if it
        // landed)
        let mut g = synced(R0);
        g.x_configured(XID, r(100, 300), 1, t);
        g.frame(XID).unwrap();
        g.failed(XID, false);
        assert_eq!(g.frame(XID).map(|m| m.rect), Some(r(100, 300)));
    }

    #[test]
    fn the_layout_must_be_unscaled_and_on_the_x_screen() {
        let m = |index, rect, scale| MonitorInfo {
            index,
            rect,
            work_area: rect,
            scale,
            primary: index == 0,
        };
        let one = m(0, Rect::new(0, 0, 1280, 720), 1.0);
        assert_eq!(layout_fits(std::slice::from_ref(&one), (1280, 720)), Ok(()));
        assert_eq!(
            layout_fits(
                &[one.clone(), m(1, Rect::new(1280, 0, 1280, 720), 1.0)],
                (2560, 720)
            ),
            Ok(())
        );
        // a monitor added and the X screen could not grow (Xvfb)
        assert!(
            layout_fits(
                &[one.clone(), m(1, Rect::new(1280, 0, 1280, 720), 1.0)],
                (1280, 720)
            )
            .is_err()
        );
        // scaled: the X screen is physical, the layout logical
        assert!(layout_fits(&[m(0, Rect::new(0, 0, 1280, 720), 2.0)], (2560, 1440)).is_err());
        assert!(layout_fits(&[m(0, Rect::new(0, 0, 1280, 720), 1.25)], (1600, 900)).is_err());
        assert!(layout_fits(&[], (1280, 720)).is_err());
    }

    #[test]
    fn always_on_top_from_the_desktop_becomes_the_windows_own() {
        let mut l = Layers::new();
        l.set(1, true, None);
        assert_eq!(l.changes(), vec![(1, true)]);
        assert!(!l.desktop(1, true), "what it was told");
        assert!(l.desktop(1, false), "the user turned it off");
        assert!(!l.above(1));
        assert_eq!(l.changes(), vec![], "nothing to tell back");
        l.retell();
        assert_eq!(l.changes(), vec![], "a retell repeats the user's choice");
        assert!(l.desktop(1, true));
        l.retell();
        assert_eq!(l.changes(), vec![(1, true)]);
        assert!(!l.desktop(2, true), "unknown windows are not taken");
    }

    #[test]
    fn a_reset_forgets_what_was_in_flight() {
        let mut g = synced(R0);
        g.x_configured(XID, r(1, 1), 1, t0());
        g.frame(XID).unwrap();
        g.reset();
        assert!(!g.in_flight(XID));
        assert_eq!(g.reply(XID, r(1, 1)), None, "a late reply is ignored");
        assert_eq!(g.frame(XID), None);
        assert!(g.is_tracked(XID));
    }

    // ---- a longer run: a client dragging its own window to an edge ----

    #[test]
    fn a_client_drag_converges() {
        let mut g = synced(R0);
        let t = t0();
        let mut seq = 0;
        let mut desk = R0;
        let mut x = R0;
        let mut sent = 0;
        g.button(XID, true);
        for _ in 0..60 {
            // the client moves itself left 10 px per motion, 2 motions a frame
            for _ in 0..2 {
                seq += 1;
                x.x -= 10;
                assert_eq!(g.x_configured(XID, x, seq, t), None);
            }
            if let Some(m) = g.frame(XID) {
                sent += 1;
                // the desktop clamps at x = 0 and replies at once
                desk = Rect {
                    x: m.rect.x.max(0),
                    ..m.rect
                };
                if let Some(back) = g.reply(XID, desk) {
                    seq += 1;
                    g.x_sent(XID, back, seq);
                    x = back;
                }
            }
        }
        assert_eq!(sent, 60, "one request per frame");
        g.button(XID, false);
        if let Some(m) = g.frame(XID) {
            desk = Rect {
                x: m.rect.x.max(0),
                ..m.rect
            };
            if let Some(back) = g.reply(XID, desk) {
                x = back;
            }
        }
        assert_eq!(desk.x, 0, "held at the edge");
        assert_eq!(x, desk, "X and the desktop agree");
    }

    // ---- raise gating ----

    #[test]
    fn a_raise_after_input_in_that_client_activates() {
        let mut g = RaiseGate::new();
        let t = t0();
        g.input(1, t);
        assert_eq!(g.request(XID, 1, ms(t, 500)), Raise::Activate);
        assert_eq!(g.request(XID, 1, ms(t, 2000)), Raise::Activate);
        assert_eq!(g.request(XID, 1, ms(t, 600)), Raise::Activate, "repeatable");
    }

    #[test]
    fn a_raise_without_input_demands_attention_once_per_interval() {
        let mut g = RaiseGate::new();
        let t = t0();
        assert_eq!(g.request(XID, 1, t), Raise::DemandAttention);
        assert_eq!(g.request(XID, 1, ms(t, 100)), Raise::Ignore);
        assert_eq!(g.request(XID, 1, ms(t, 2999)), Raise::Ignore);
        assert_eq!(g.request(XID, 1, ms(t, 3000)), Raise::DemandAttention);
        // per window: another window of the client gets its own attempt
        assert_eq!(g.request(7, 1, ms(t, 3001)), Raise::DemandAttention);
    }

    #[test]
    fn input_vouches_only_for_its_own_client_and_only_while_recent() {
        let mut g = RaiseGate::new();
        let t = t0();
        g.input(1, t);
        assert_eq!(
            g.request(XID, 2, ms(t, 10)),
            Raise::DemandAttention,
            "another client"
        );
        assert_eq!(
            g.request(9, 1, ms(t, 2001)),
            Raise::DemandAttention,
            "too old"
        );
        // newer input renews it; an older report does not roll it back
        g.input(1, ms(t, 5000));
        g.input(1, ms(t, 4000));
        assert_eq!(g.request(9, 1, ms(t, 6500)), Raise::Activate);
    }

    #[test]
    fn leaving_the_client_withdraws_its_input() {
        let mut g = RaiseGate::new();
        let t = t0();
        g.input(1, t);
        g.left(1);
        assert_eq!(g.request(XID, 1, ms(t, 10)), Raise::DemandAttention);
        g.input(1, t);
        g.forget_client(1);
        g.forget_window(XID);
        assert_eq!(
            g.request(XID, 1, ms(t, 20)),
            Raise::DemandAttention,
            "forgotten"
        );
    }

    #[test]
    fn a_self_raise_loop_cannot_flicker() {
        // a timer re-raising every 50 ms for a minute, nobody touching it
        let mut g = RaiseGate::new();
        let t = t0();
        let forwarded = (0..1200)
            .filter(|i| g.request(XID, 1, ms(t, i * 50)) != Raise::Ignore)
            .count();
        assert_eq!(forwarded, 20, "one per 3 s");
    }

    // ---- the integration's decisions ----

    #[test]
    fn the_shared_rectangle_includes_the_border() {
        assert_eq!(outer_rect(10, 20, 100, 50, 0), Rect::new(10, 20, 100, 50));
        assert_eq!(outer_rect(10, 20, 100, 50, 2), Rect::new(10, 20, 104, 54));
        assert_eq!(outer_rect(-5, -7, 1, 1, 0), Rect::new(-5, -7, 1, 1));
        assert_eq!(inner_size(104, 54, 2), (100, 50));
        assert_eq!(inner_size(100, 50, 0), (100, 50));
        assert_eq!(inner_size(3, 3, 2), (1, 1), "never below 1x1");
        // round trip
        let r = outer_rect(1, 2, 640, 480, 1);
        assert_eq!(inner_size(r.w, r.h, 1), (640, 480));
    }

    #[test]
    fn desktop_reports_carry_the_position_not_the_size() {
        // the client grew to 500x400; the desktop still reports its last
        // frame (400x300) at a new position: X keeps its size
        let x = Rect::new(100, 100, 500, 400);
        let desk = Rect::new(150, 120, 400, 300);
        assert_eq!(desktop_position(desk, x), Rect::new(150, 120, 500, 400));
        // fed to Geometry, the stale size does not shrink X back
        let mut g = synced(Rect::new(100, 100, 400, 300));
        let t = t0();
        assert_eq!(g.x_configured(XID, x, 1, t), None);
        let m = g.frame(XID).expect("the client's resize goes out");
        assert_eq!(m.rect, x);
        assert_eq!(g.reply(XID, x), None);
        let stale = Rect::new(100, 100, 400, 300);
        assert_eq!(
            g.changed(
                XID,
                desktop_position(stale, g.x_rect(XID).unwrap()),
                0,
                11,
                t
            ),
            None,
            "the frame before the new buffer is no desktop resize"
        );
        assert_eq!(g.x_rect(XID), Some(x));
    }

    #[test]
    fn a_user_or_meaningful_program_position_opens_there() {
        let flags = |f: u32| vec![f, 0, 0, 0, 0];
        assert_eq!(
            initial_position(&flags(1), 300, 200),
            Some((300, 200)),
            "USPosition"
        );
        assert_eq!(
            initial_position(&flags(1), 0, 0),
            Some((0, 0)),
            "the user's 0,0 too"
        );
        assert_eq!(
            initial_position(&flags(4), 300, 200),
            Some((300, 200)),
            "PPosition"
        );
        assert_eq!(
            initial_position(&flags(4), 0, 0),
            None,
            "a program's 0,0 is no position"
        );
        assert_eq!(initial_position(&flags(5), 0, 0), Some((0, 0)));
        assert_eq!(
            initial_position(&flags(2 | 8), 300, 200),
            None,
            "sizes only"
        );
        assert_eq!(initial_position(&flags(0), 300, 200), None);
        assert_eq!(initial_position(&[], 300, 200), None, "no WM_NORMAL_HINTS");
    }

    #[test]
    fn urgency_is_wm_hints_bit_8() {
        assert!(urgent(&[1 << 8, 1, 0]));
        assert!(urgent(&[(1 << 8) | 3, 1, 1]));
        assert!(!urgent(&[3, 1, 1]));
        assert!(!urgent(&[]));
    }

    #[test]
    fn the_pointer_maps_from_the_desktops_position_once_known() {
        assert_eq!(
            pointer_origin(None, (5, 6)),
            (5, 6),
            "not reported yet: X's"
        );
        assert_eq!(
            pointer_origin(Some(Rect::new(70, 80, 1, 1)), (5, 6)),
            (70, 80)
        );
        assert_eq!(
            pointer_origin(Some(Rect::new(1 << 20, -(1 << 20), 1, 1)), (0, 0)),
            (i16::MAX, i16::MIN)
        );
    }

    /// The X client's own drag loop (window = pointer - grab offset) with
    /// the pointer mapped from the desktop's position: the desktop window
    /// stays under the pointer step for step.
    #[test]
    fn a_self_drag_follows_the_pointer_with_the_desktops_origin() {
        let start = Rect::new(100, 100, 200, 150);
        let mut g = synced(start);
        let t = t0();
        let grab = 10; // the pointer is 10 px into the window
        let (mut p, mut desk_actual, mut seq) = (110, 100, 1u64);
        assert!(g.button(XID, true).is_none());
        for _ in 0..40 {
            p += 7; // the desktop's pointer
            let sx = p - desk_actual; // wl_pointer.motion, surface-local
            let origin = i32::from(pointer_origin(g.desk(XID), (0, 0)).0);
            let root = origin + sx;
            let x = Rect::new(root - grab, 100, 200, 150);
            seq += 1;
            assert_eq!(g.x_configured(XID, x, seq, t), None);
            if let Some(m) = g.frame(XID) {
                assert_ne!(m.flags & mr::USER, 0, "a held drag is the user's");
                desk_actual = m.rect.x; // mutter moves it in the call
                assert_eq!(g.reply(XID, m.rect), None);
            }
            assert_eq!(root, p, "the X pointer is where the desktop's is");
        }
        assert_eq!(desk_actual, p - grab, "the desktop window followed");
        assert_eq!(g.x_rect(XID).unwrap().x, p - grab);
        assert!(g.button(XID, false).is_none(), "nothing to put back");
    }

    /// Why 0.1.2's held mapping is not kept with the sync: once the surface
    /// moves under the pointer, a fixed origin reads only part of the
    /// pointer's motion and the window falls behind it.
    #[test]
    fn a_held_origin_would_stall_a_drag_the_desktop_follows() {
        let (fixed_origin, grab) = (100, 10);
        let (mut p, mut desk_actual) = (110, 100);
        let mut window = 100;
        for _ in 0..20 {
            p += 7;
            let root = fixed_origin + (p - desk_actual);
            window = root - grab;
            desk_actual = window; // the desktop follows X
        }
        // half the pointer's speed: 70 px behind after 20 steps of 7
        assert_eq!(p - grab - window, 70, "window {window}, pointer {p}");
    }

    #[test]
    fn move_resizes_go_with_frames_or_every_16_ms() {
        let t = t0();
        assert_eq!(send_due(None, None, t), Due::Now);
        assert_eq!(
            send_due(Some(t), None, ms(t, 5)),
            Due::Frame,
            "a commit waits"
        );
        assert_eq!(
            send_due(Some(t), Some(t), ms(t, 150)),
            Due::Now,
            "a frame callback this late does not hold it"
        );
        assert_eq!(send_due(None, Some(t), ms(t, 5)), Due::At(ms(t, 16)));
        assert_eq!(send_due(None, Some(t), ms(t, 16)), Due::Now);
        assert_eq!(send_due(None, Some(t), ms(t, 40)), Due::Now);
    }

    /// Applies `restack_plan`'s operations to a whole X stack.
    fn restacked(stack: &[Xid], ops: &[(Xid, Stack)]) -> Vec<Xid> {
        let mut s = stack.to_vec();
        for &(w, to) in ops {
            s.retain(|&v| v != w);
            match to {
                Stack::Above(below) => {
                    let i = s.iter().position(|&v| v == below).unwrap();
                    s.insert(i + 1, w);
                }
                Stack::Below(above) => {
                    let i = s.iter().position(|&v| v == above).unwrap();
                    s.insert(i, w);
                }
            }
        }
        s
    }

    #[test]
    fn x_restacks_to_the_desktops_order() {
        let (a, b, c, d) = (1, 2, 3, 4);
        assert!(restack_plan(&[a, b, c], &[a, b, c]).is_empty(), "agrees");
        assert!(
            restack_plan(&[a, b, c], &[90, a, 91, b, c, 92]).is_empty(),
            "other windows in between don't matter"
        );
        // the client raised a: one restack puts it back
        let ops = restack_plan(&[a, b, c], &[b, c, a]);
        assert_eq!(ops.len(), 1);
        assert_eq!(restacked(&[b, c, a], &ops), vec![a, b, c]);
        // reversed: n-1 restacks
        let ops = restack_plan(&[a, b, c, d], &[d, c, b, a]);
        assert_eq!(ops.len(), 3);
        assert_eq!(restacked(&[d, c, b, a], &ops), vec![a, b, c, d]);
        // windows in only one list are left out; duplicates count once
        let ops = restack_plan(&[a, 77, b, b, c], &[c, 88, b, a]);
        let x = restacked(&[c, 88, b, a], &ops);
        let ours: Vec<Xid> = x.into_iter().filter(|v| [a, b, c].contains(v)).collect();
        assert_eq!(ours, vec![a, b, c]);
        assert!(restack_plan(&[a], &[a]).is_empty());
        assert!(restack_plan(&[], &[a, b]).is_empty());
    }

    #[test]
    fn every_order_restacks_to_the_desktops() {
        // all permutations of 4 windows, with others interleaved
        let ws = [1, 2, 3, 4];
        let mut perms = vec![vec![]];
        for _ in 0..ws.len() {
            perms = perms
                .into_iter()
                .flat_map(|p: Vec<Xid>| {
                    ws.iter()
                        .filter(|w| !p.contains(w))
                        .map(|w| {
                            let mut q = p.clone();
                            q.push(*w);
                            q
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
        }
        for desk in &perms {
            for x in &perms {
                let stack: Vec<Xid> = x.iter().flat_map(|w| [100 + w, *w]).collect();
                let ops = restack_plan(desk, &stack);
                let got: Vec<Xid> = restacked(&stack, &ops)
                    .into_iter()
                    .filter(|v| ws.contains(v))
                    .collect();
                assert_eq!(&got, desk, "x {x:?}");
                assert!(ops.len() < ws.len());
                // one restack per window out of the longest run in order
                let pos: Vec<usize> = desk
                    .iter()
                    .map(|w| x.iter().position(|u| u == w).unwrap())
                    .collect();
                let longest = (0..1u32 << ws.len())
                    .filter(|m| {
                        let p: Vec<usize> = (0..ws.len())
                            .filter(|i| m & (1 << i) != 0)
                            .map(|i| pos[i])
                            .collect();
                        p.windows(2).all(|q| q[0] < q[1])
                    })
                    .map(|m| m.count_ones() as usize)
                    .max()
                    .unwrap();
                assert_eq!(ops.len(), ws.len() - longest, "x {x:?} desk {desk:?}");
                if desk == x {
                    assert!(ops.is_empty());
                }
            }
        }
    }

    #[test]
    fn restacks_are_told_from_moves() {
        let before = ((10, 20), 100, 50, Some(7));
        assert!(
            restack_event(before, ((10, 20), 100, 50, 7)),
            "XRaiseWindow with the same sibling below"
        );
        assert!(
            restack_event(before, ((10, 20), 100, 50, 9)),
            "a new sibling below"
        );
        assert!(!restack_event(before, ((11, 20), 100, 50, 7)), "a move");
        assert!(!restack_event(before, ((10, 20), 101, 50, 7)), "a resize");
        assert!(
            restack_event(before, ((11, 20), 100, 50, 9)),
            "a move and a restack"
        );
        assert!(
            restack_event(((10, 20), 100, 50, None), ((10, 20), 100, 50, 7)),
            "first seen"
        );
        assert!(!restack_event(
            ((10, 20), 100, 50, None),
            ((0, 0), 100, 50, 7)
        ));
    }

    #[test]
    fn only_a_raise_over_the_desktops_order_is_a_request() {
        let (a, b, c) = (1, 2, 3);
        let desk = [a, b, c];
        assert!(raised_over_desktop(a, &desk, &[b, c, a]), "XRaiseWindow");
        assert!(raised_over_desktop(a, &desk, &[b, 9, a, c]), "over b");
        assert!(!raised_over_desktop(c, &desk, &[a, b, c]), "on top already");
        assert!(
            !raised_over_desktop(c, &desk, &[c, a, b]),
            "a lower is no request"
        );
        assert!(!raised_over_desktop(b, &desk, &[a, b, c]), "unchanged");
        assert!(
            !raised_over_desktop(7, &desk, &[a, b, c, 7]),
            "not on the desktop yet"
        );
        assert!(!raised_over_desktop(a, &desk, &[b, c]), "not in X");
    }

    #[test]
    fn keep_above_is_told_on_change_only() {
        let mut l = Layers::new();
        l.set(1, false, None);
        assert!(l.changes().is_empty(), "untold means not above");
        l.set(1, true, None);
        assert_eq!(l.changes(), vec![(1, true)]);
        assert!(l.changes().is_empty());
        l.set(1, true, None);
        assert!(l.changes().is_empty(), "the same again");
        l.set(1, false, None);
        assert_eq!(l.changes(), vec![(1, false)]);
    }

    #[test]
    fn a_child_of_an_above_window_is_above_too() {
        let (top, dialog, sub) = (1, 2, 3);
        let mut l = Layers::new();
        l.set(top, true, None);
        l.set(dialog, false, Some(top));
        l.set(sub, false, Some(dialog));
        assert_eq!(l.changes(), vec![(top, true), (dialog, true), (sub, true)]);
        assert!(l.above(sub), "through the chain");
        l.set(top, false, None);
        assert_eq!(
            l.changes(),
            vec![(top, false), (dialog, false), (sub, false)]
        );
        // a child above of its own stays so
        l.set(top, true, None);
        l.set(dialog, true, Some(top));
        let _ = l.changes();
        l.set(top, false, None);
        assert_eq!(l.changes(), vec![(top, false)]);
        assert!(l.above(sub), "its own parent still is");
        // the parent goes: the inheritance with it
        l.set(dialog, false, Some(top));
        l.set(top, true, None);
        let _ = l.changes();
        l.forget(top);
        assert_eq!(l.changes(), vec![(dialog, false), (sub, false)]);
    }

    #[test]
    fn transient_cycles_end() {
        let mut l = Layers::new();
        l.set(1, false, Some(2));
        l.set(2, false, Some(1));
        l.set(3, false, Some(3));
        assert!(!l.above(1) && !l.above(2) && !l.above(3));
        assert!(l.changes().is_empty());
    }

    #[test]
    fn keep_above_is_told_again_when_the_extension_comes_back() {
        let mut l = Layers::new();
        l.set(1, true, None);
        l.set(2, false, None);
        let _ = l.changes();
        l.retell();
        assert_eq!(l.changes(), vec![(1, true)], "not-above needs no telling");
    }

    #[test]
    fn attention_is_told_on_change_and_again_after_a_comeback() {
        let mut a = Attention::new();
        assert_eq!(a.hint(1, false), None, "untold means none");
        assert_eq!(a.hint(1, true), Some(true));
        assert_eq!(a.hint(1, true), None);
        assert_eq!(a.hint(2, true), Some(true));
        assert_eq!(a.retell(), vec![1, 2]);
        assert_eq!(a.hint(1, false), Some(false));
        assert_eq!(a.retell(), vec![2]);
        a.forget(2);
        assert!(a.retell().is_empty());
        assert_eq!(a.hint(2, true), Some(true), "forgotten: told afresh");
    }
}

#[cfg(all(test, feature = "gnome"))]
mod bus_tests {
    use super::bus::*;
    use super::*;

    #[test]
    fn signals_and_replies_decode_for_our_display_only() {
        let ours = tag(2, 0x400001);
        let theirs = tag(3, 0x400001);
        assert_eq!(
            changed_event(
                2,
                (ours.clone(), (1, 2, 3, 4), 1, ALL_WORKSPACES, st::USER, 9),
                44
            ),
            Some(Event::Changed {
                xid: 0x400001,
                rect: Rect::new(1, 2, 3, 4),
                monitor: 1,
                workspace: ALL_WORKSPACES,
                flags: st::USER,
                serial: 9,
                wire: 44,
            })
        );
        assert_eq!(
            changed_event(2, (theirs.clone(), (1, 2, 3, 4), 0, 0, 0, 9), 45),
            None
        );
        assert_eq!(
            restacked_event(2, vec![ours, theirs, "junk".into(), tag(2, 5)], 46),
            Event::Restacked(vec![0x400001, 5], 46)
        );
        assert_eq!(
            monitors_event(vec![(
                0,
                (0, 0, 1920, 1080),
                (0, 32, 1920, 1048),
                1.0,
                true
            )]),
            Event::Monitors(vec![MonitorInfo {
                index: 0,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 32, 1920, 1048),
                scale: 1.0,
                primary: true
            }])
        );
    }
}

/// The client against a stand-in extension served by GDBus (gjs), with the
/// extension's interface and encodings, on a private bus (skipped where
/// there is no dbus-daemon or gjs).
#[cfg(all(test, feature = "gnome"))]
mod live_tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    use super::bus::*;
    use super::*;

    const DISPLAY: u32 = 5;
    const WAIT: Duration = Duration::from_secs(5);

    /// The stand-in: the window `xwlbridge:5:0x400001` is the caller's;
    /// MoveResize with CLAMP keeps x >= 0. `org.xwlbridge.Test` at
    /// /org/xwlbridge/Test: `Calls() -> as` (what it was asked), `Emit()`
    /// (a set of signals to the last caller of Version).
    const FAKE: &str = r#"
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import System from 'system';

const [address, version, features, squat] = System.programArgs;
const IFACE = `<node><interface name="org.xwlbridge.Windows1">
 <method name="Version"><arg type="u" direction="out"/></method>
 <method name="Features"><arg type="as" direction="out"/></method>
 <method name="MoveResize"><arg type="s" direction="in"/><arg type="i" direction="in"/>
  <arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="i" direction="in"/>
  <arg type="u" direction="in"/><arg type="(iiii)" name="applied" direction="out"/></method>
 <method name="SetInitialPosition"><arg type="s" direction="in"/><arg type="i" direction="in"/>
  <arg type="i" direction="in"/></method>
 <method name="Activate"><arg type="s" direction="in"/><arg type="u" direction="in"/></method>
 <method name="Raise"><arg type="s" direction="in"/></method>
 <method name="SetAbove"><arg type="s" direction="in"/><arg type="b" direction="in"/></method>
 <method name="SetDemandsAttention"><arg type="s" direction="in"/><arg type="b" direction="in"/></method>
 <method name="Stick"><arg type="s" direction="in"/><arg type="b" direction="in"/></method>
 <method name="SetWorkspace"><arg type="s" direction="in"/><arg type="i" direction="in"/></method>
 <method name="Minimize"><arg type="s" direction="in"/><arg type="b" direction="in"/></method>
 <method name="GetState"><arg type="a(sa{sv})" direction="out"/></method>
 <method name="GetMonitors"><arg type="a(i(iiii)(iiii)db)" direction="out"/></method>
 <signal name="Changed"><arg type="s"/><arg type="(iiii)"/><arg type="i"/><arg type="i"/>
  <arg type="u"/><arg type="u"/></signal>
 <signal name="Restacked"><arg type="as"/></signal>
 <signal name="MonitorsChanged"/>
 <signal name="WindowGone"><arg type="s"/></signal>
</interface></node>`;
const TEST = `<node><interface name="org.xwlbridge.Test">
 <method name="Calls"><arg type="as" direction="out"/></method>
 <method name="Emit"/>
</interface></node>`;
const MINE = 'xwlbridge:5:0x400001';
const calls = [];
let subscriber = null;
const conn = Gio.DBusConnection.new_for_address_sync(address,
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
    null, null);
const mine = (tag, inv) => {
    if (tag === MINE)
        return true;
    inv.return_dbus_error('org.xwlbridge.Windows1.Error.NoSuchWindow', `no window ${tag} of the calling process`);
    return false;
};
const plain = name => function (args, inv) {
    calls.push([name, ...args].join(' '));
    if (mine(args[0], inv))
        inv.return_value(null);
};
const windows = {
    VersionAsync(_, inv) {
        subscriber = inv.get_sender();
        inv.return_value(new GLib.Variant('(u)', [Number(version)]));
    },
    FeaturesAsync(_, inv) {
        inv.return_value(new GLib.Variant('(as)', [features ? features.split(',') : []]));
    },
    MoveResizeAsync([tag, x, y, w, h, flags], inv) {
        calls.push(`MoveResize ${tag} ${x},${y} ${w}x${h} ${flags}`);
        if (!mine(tag, inv))
            return;
        if (flags & 1)
            x = Math.max(0, x);
        inv.return_value(new GLib.Variant('((iiii))', [[x, y, w, h]]));
    },
    SetInitialPositionAsync([tag, x, y], inv) {
        calls.push(`SetInitialPosition ${tag} ${x},${y}`);
        if (mine(tag, inv))
            inv.return_value(null);
    },
    ActivateAsync: plain('Activate'),
    RaiseAsync: plain('Raise'),
    SetAboveAsync: plain('SetAbove'),
    SetDemandsAttentionAsync: plain('SetDemandsAttention'),
    StickAsync: plain('Stick'),
    SetWorkspaceAsync: plain('SetWorkspace'),
    MinimizeAsync: plain('Minimize'),
    GetStateAsync(_, inv) {
        const v = (t, x) => new GLib.Variant(t, x);
        inv.return_value(new GLib.Variant('(a(sa{sv}))', [[
            ['xwlbridge:6:0x400001', {rect: v('(iiii)', [0, 0, 10, 10])}],
            [MINE, {rect: v('(iiii)', [10, 20, 300, 200]), monitor: v('i', 1), workspace: v('i', -1),
                flags: v('u', 32), serial: v('u', 7), shown: v('b', true), stack: v('u', 1),
                minimized: v('b', false)}],
            ['xwlbridge:5:0x400002', {monitor: v('i', 0)}],
        ]]));
    },
    GetMonitorsAsync(_, inv) {
        inv.return_value(new GLib.Variant('(a(i(iiii)(iiii)db))', [[
            [0, [0, 0, 1920, 1080], [0, 32, 1920, 1048], 1.0, true],
            [1, [1920, 0, 2560, 1440], [1920, 0, 2560, 1440], 2.0, false],
        ]]));
    },
};
const test = {
    CallsAsync(_, inv) {
        inv.return_value(new GLib.Variant('(as)', [calls]));
    },
    EmitAsync(_, inv) {
        const e = (name, sig, vals) => conn.emit_signal(subscriber, '/org/xwlbridge/Windows',
            'org.xwlbridge.Windows1', name, sig ? new GLib.Variant(sig, vals) : null);
        e('Changed', '(s(iiii)iiuu)', ['xwlbridge:6:0x400001', [1, 1, 1, 1], 0, 0, 0, 102]);
        e('Changed', '(s(iiii)iiuu)', [MINE, [50, 60, 300, 200], 1, -1, 1, 103]);
        e('Restacked', '(as)', [['xwlbridge:5:0x1', 'xwlbridge:6:0x2', 'xwlbridge:5:0x3']]);
        e('MonitorsChanged', null, null);
        e('WindowGone', '(s)', [MINE]);
        inv.return_value(null);
    },
};
Gio.DBusExportedObject.wrapJSObject(IFACE, windows).export(conn, '/org/xwlbridge/Windows');
Gio.DBusExportedObject.wrapJSObject(TEST, test).export(conn, '/org/xwlbridge/Test');
// the extension runs in gnome-shell: the bridge accepts only the owner of org.gnome.Shell
// ("squat": a process that took the name but is not gnome-shell)
const own = () => Gio.bus_own_name_on_connection(conn, 'org.xwlbridge.Windows', Gio.BusNameOwnerFlags.NONE,
    () => print('OWNED'), null);
if (squat === 'squat')
    own();
else
    Gio.bus_own_name_on_connection(conn, 'org.gnome.Shell', Gio.BusNameOwnerFlags.NONE, own, null);
new GLib.MainLoop(null, false).run();
"#;

    struct Bus {
        child: Child,
        address: String,
        dir: std::path::PathBuf,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn private_bus() -> Option<Bus> {
        let dir = std::env::temp_dir().join(format!("xwlbridge-winsync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(dir.join("fake.js"), FAKE).ok()?;
        Command::new("gjs").arg("--version").output().ok()?;
        let mut child = Command::new("dbus-daemon")
            .arg("--session")
            .arg("--nofork")
            .arg("--print-address=1")
            .arg(format!("--address=unix:path={}", dir.join("bus").display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        Some(Bus {
            child,
            address: line.trim().to_string(),
            dir,
        })
    }

    /// The stand-in, once it owns the name.
    struct Fake(Child);

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn serve(bus: &Bus, version: u32, features: &str) -> Fake {
        serve_as(bus, version, features, "shell")
    }

    /// `who`: "shell" (it is gnome-shell's connection) or "squat" (not).
    fn serve_as(bus: &Bus, version: u32, features: &str, who: &str) -> Fake {
        let mut child = Command::new("gjs")
            .arg("-m")
            .arg(bus.dir.join("fake.js"))
            .args([bus.address.as_str(), &version.to_string(), features, who])
            .stdout(Stdio::piped())
            .spawn()
            .expect("gjs");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "OWNED", "the stand-in did not start");
        Fake(child)
    }

    fn connect(bus: &Bus) -> zbus::blocking::Connection {
        zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .unwrap()
    }

    fn test_call(conn: &zbus::blocking::Connection, method: &str) -> zbus::Message {
        conn.call_method(
            Some(BUS_NAME),
            "/org/xwlbridge/Test",
            Some("org.xwlbridge.Test"),
            method,
            &(),
        )
        .unwrap()
    }

    /// The next event that `want` accepts, within `WAIT` (others are kept
    /// in `seen`).
    fn wait_for(c: &Client, seen: &mut Vec<Event>, want: impl Fn(&Event) -> bool) -> Option<Event> {
        let end = Instant::now() + WAIT;
        while Instant::now() < end {
            for e in c.events() {
                if want(&e) {
                    return Some(e);
                }
                seen.push(e);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// Every event within `d`.
    fn settle(c: &Client, d: Duration) -> Vec<Event> {
        std::thread::sleep(d);
        c.events().collect()
    }

    /// `e` with its order stamp 0 (stamps are the bus's serials).
    fn unstamped(e: Event) -> Event {
        match e {
            Event::Applied {
                xid,
                requested,
                applied,
                ..
            } => Event::Applied {
                xid,
                requested,
                applied,
                wire: 0,
            },
            Event::Changed {
                xid,
                rect,
                monitor,
                workspace,
                flags,
                serial,
                ..
            } => Event::Changed {
                xid,
                rect,
                monitor,
                workspace,
                flags,
                serial,
                wire: 0,
            },
            Event::Restacked(o, _) => Event::Restacked(o, 0),
            Event::State(s, _) => Event::State(s, 0),
            e => e,
        }
    }

    #[test]
    fn negotiates_calls_and_receives_from_the_extension() {
        let Some(bus) = private_bus() else {
            eprintln!("no dbus-daemon or gjs: skipped");
            return;
        };
        let c = Client::spawn_at(DISPLAY, Some(bus.address.clone())).unwrap();
        let mut seen = Vec::new();
        let mine = 0x400001;

        // no extension: unavailable, and a MoveResize still gets its answer
        assert!(settle(&c, Duration::from_millis(300)).is_empty());
        assert!(!c.available());
        let rect = Rect::new(-5, 10, 100, 100);
        c.send(Request::MoveResize {
            xid: mine,
            rect,
            flags: mr::CLAMP,
        });
        c.send(Request::Raise { xid: mine }); // dropped
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::Failed { .. }));
        assert!(
            matches!(
                e,
                Some(Event::Failed {
                    xid: 0x400001,
                    method: "MoveResize",
                    ..
                })
            ),
            "{e:?}"
        );

        // the extension appears: negotiated
        let ext = serve(&bus, 1, "move-resize,state");
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::Available { .. }));
        assert_eq!(
            e,
            Some(Event::Available {
                version: 1,
                features: vec!["move-resize".into(), "state".into()]
            })
        );
        assert!(c.available());

        // calls
        c.send(Request::MoveResize {
            xid: mine,
            rect,
            flags: mr::CLAMP,
        });
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::Applied { .. }));
        assert_eq!(
            e.map(unstamped),
            Some(Event::Applied {
                xid: mine,
                requested: rect,
                applied: Rect::new(0, 10, 100, 100),
                wire: 0,
            }),
            "the (iiii) reply"
        );
        c.send(Request::Raise { xid: 0x400002 });
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::Failed { .. }));
        assert!(
            matches!(&e, Some(Event::Failed { xid: 0x400002, method: "Raise", error, answered: true }) if error.contains("NoSuchWindow")),
            "{e:?}"
        );
        for r in [
            Request::SetInitialPosition {
                xid: mine,
                x: 40,
                y: -30,
            },
            Request::Activate {
                xid: mine,
                timestamp: 1234,
            },
            Request::SetAbove {
                xid: mine,
                on: true,
            },
            Request::SetDemandsAttention {
                xid: mine,
                on: false,
            },
            Request::Stick {
                xid: mine,
                on: true,
            },
            Request::SetWorkspace {
                xid: mine,
                index: 2,
            },
            Request::Minimize {
                xid: mine,
                on: true,
            },
            Request::GetState,
        ] {
            c.send(r);
        }
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::State(..)));
        assert_eq!(
            e.map(unstamped),
            Some(Event::State(
                vec![WindowState {
                    xid: mine,
                    rect: Rect::new(10, 20, 300, 200),
                    monitor: 1,
                    workspace: ALL_WORKSPACES,
                    flags: st::FOCUSED,
                    serial: 7,
                    shown: true,
                }],
                0
            )),
            "only display 5's windows, and only with a rectangle"
        );
        c.send(Request::GetMonitors);
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::Monitors(_)));
        let Some(Event::Monitors(m)) = e else {
            panic!("{e:?}")
        };
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].rect, Rect::new(1920, 0, 2560, 1440));
        assert_eq!(m[1].scale, 2.0);
        assert!(m[0].primary && !m[1].primary);
        assert_eq!(m[0].work_area, Rect::new(0, 32, 1920, 1048));
        assert!(seen.is_empty(), "nothing else came: {seen:?}");
        let test = connect(&bus);
        let calls: Vec<String> = test_call(&test, "Calls").body().deserialize().unwrap();
        let t = tag(DISPLAY, mine);
        assert_eq!(
            calls,
            vec![
                format!("MoveResize {t} -5,10 100x100 1"),
                format!("Raise {}", tag(DISPLAY, 0x400002)),
                format!("SetInitialPosition {t} 40,-30"),
                format!("Activate {t} 1234"),
                format!("SetAbove {t} true"),
                format!("SetDemandsAttention {t} false"),
                format!("Stick {t} true"),
                format!("SetWorkspace {t} 2"),
                format!("Minimize {t} true"),
            ],
            "the tags and arguments on the wire"
        );

        // signals: a spoofer's (dropped), then the extension's
        test.emit_signal(
            None::<&str>,
            PATH,
            IFACE,
            "Changed",
            &(t.clone(), (9, 9, 9, 9), 0, 0, st::USER, 999u32),
        )
        .unwrap();
        test.emit_signal(None::<&str>, PATH, IFACE, "WindowGone", &(t.clone(),))
            .unwrap();
        test_call(&test, "Emit");
        let got = settle(&c, Duration::from_millis(500));
        let stamps: Vec<u32> = got
            .iter()
            .filter_map(|e| match e {
                Event::Changed { wire, .. } | Event::Restacked(_, wire) => Some(*wire),
                _ => None,
            })
            .collect();
        assert!(
            stamps.len() == 2 && serial_newer(stamps[1], stamps[0]),
            "stamps in the extension's order: {stamps:?}"
        );
        assert_eq!(
            got.into_iter().map(unstamped).collect::<Vec<_>>(),
            vec![
                Event::Changed {
                    xid: mine,
                    rect: Rect::new(50, 60, 300, 200),
                    monitor: 1,
                    workspace: ALL_WORKSPACES,
                    flags: st::USER,
                    serial: 103,
                    wire: 0,
                },
                Event::Restacked(vec![1, 3], 0),
                Event::MonitorsChanged,
                Event::WindowGone(mine),
            ],
            "only the extension's, only ours, in order"
        );

        // the extension goes away
        drop(ext);
        let e = wait_for(&c, &mut seen, |e| *e == Event::Unavailable);
        assert_eq!(e, Some(Event::Unavailable));
        assert!(!c.available());

        // incompatible ones come and go: still unavailable
        for (version, features) in [(0, "move-resize"), (1, ""), (1, "state")] {
            let other = serve(&bus, version, features);
            let got = settle(&c, Duration::from_millis(400));
            assert!(
                !got.iter().any(|e| matches!(e, Event::Available { .. })),
                "version {version} features {features:?}: {got:?}"
            );
            assert!(!c.available());
            drop(other);
            std::thread::sleep(Duration::from_millis(100));
        }

        // a compatible one that is not gnome-shell (the name is free while
        // the screen is locked): refused
        let squatter = serve_as(&bus, 1, "move-resize", "squat");
        let got = settle(&c, Duration::from_millis(400));
        assert!(
            !got.iter().any(|e| matches!(e, Event::Available { .. })),
            "a squatter: {got:?}"
        );
        assert!(!c.available());
        drop(squatter);
        std::thread::sleep(Duration::from_millis(100));

        // a compatible one again: available again
        let _ext = serve(&bus, 2, "move-resize");
        let e = wait_for(&c, &mut seen, |e| matches!(e, Event::Available { .. }));
        assert!(
            matches!(e, Some(Event::Available { version: 2, .. })),
            "{e:?}"
        );
        assert!(c.available());
    }
}

/// The client against the real extension in a real GNOME Shell: this test is
/// a Wayland client with one tagged toplevel (the extension serves only the
/// windows of the calling process), and drives the extension through
/// `bus::Client`, feeding what comes back into `Geometry`. Needs
/// gnome-shell with xwlbridge-windows@xwlbridge enabled on this session bus
/// and WAYLAND_DISPLAY (research/release/p1/p1-client.md has a container
/// recipe): `cargo test --features gnome,xvfb e2e -- --ignored`.
#[cfg(all(test, feature = "gnome"))]
mod e2e_tests {
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};

    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    };
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
    use wayland_protocols::xdg::toplevel_tag::v1::client::xdg_toplevel_tag_manager_v1;

    use super::bus::*;
    use super::*;

    const DISPLAY: u32 = 77;
    const XID: Xid = 0x2a00001;
    const WAIT: Duration = Duration::from_secs(10);

    #[derive(Default)]
    struct Wl {
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        wm_base: Option<xdg_wm_base::XdgWmBase>,
        tags: Option<xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1>,
        configured: bool,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for Wl {
        fn event(
            s: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name, interface, ..
            } = event
            {
                match interface.as_str() {
                    "wl_compositor" => s.compositor = Some(registry.bind(name, 4, qh, ())),
                    "wl_shm" => s.shm = Some(registry.bind(name, 1, qh, ())),
                    "xdg_wm_base" => s.wm_base = Some(registry.bind(name, 1, qh, ())),
                    "xdg_toplevel_tag_manager_v1" => s.tags = Some(registry.bind(name, 1, qh, ())),
                    _ => {}
                }
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Wl {
        fn event(
            _: &mut Self,
            base: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                base.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for Wl {
        fn event(
            s: &mut Self,
            xs: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                xs.ack_configure(serial);
                s.configured = true;
            }
        }
    }

    delegate_noop!(Wl: ignore wl_compositor::WlCompositor);
    delegate_noop!(Wl: ignore wl_surface::WlSurface);
    delegate_noop!(Wl: ignore wl_shm::WlShm);
    delegate_noop!(Wl: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(Wl: ignore wl_buffer::WlBuffer);
    delegate_noop!(Wl: ignore xdg_toplevel::XdgToplevel);
    delegate_noop!(Wl: ignore xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1);

    /// Answers the compositor while waiting for the client's next event
    /// that `want` accepts.
    fn wait(
        q: &mut EventQueue<Wl>,
        wl: &mut Wl,
        c: &Client,
        mut want: impl FnMut(&Event) -> bool,
    ) -> Event {
        let end = Instant::now() + WAIT;
        let mut seen = Vec::new();
        while Instant::now() < end {
            q.roundtrip(wl).unwrap();
            for e in c.events() {
                if want(&e) {
                    return e;
                }
                seen.push(e);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out; other events: {seen:?}")
    }

    #[test]
    #[ignore = "needs gnome-shell with xwlbridge-windows@xwlbridge on this session"]
    fn the_real_extension_moves_clamps_and_reports_our_window() {
        let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
        let mut q = conn.new_event_queue();
        let qh = q.handle();
        let mut wl = Wl::default();
        conn.display().get_registry(&qh, ());
        q.roundtrip(&mut wl).unwrap();
        let tags = wl.tags.clone().expect("xdg_toplevel_tag_manager_v1");
        let surface = wl.compositor.as_ref().unwrap().create_surface(&qh, ());
        let xs = wl
            .wm_base
            .as_ref()
            .unwrap()
            .get_xdg_surface(&surface, &qh, ());
        let top = xs.get_toplevel(&qh, ());
        tags.set_toplevel_tag(&top, tag(DISPLAY, XID));
        top.set_title("winsync e2e".into());
        surface.commit();
        while !wl.configured {
            q.blocking_dispatch(&mut wl).unwrap();
        }
        let (w, h) = (200, 150);
        let size = (w * h * 4) as usize;
        let fd = unsafe {
            let fd = libc::memfd_create(c"winsync-e2e".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, size as libc::off_t), 0);
            OwnedFd::from_raw_fd(fd)
        };
        let pool = wl
            .shm
            .as_ref()
            .unwrap()
            .create_pool(fd.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        q.roundtrip(&mut wl).unwrap();

        let c = Client::spawn(DISPLAY).unwrap();
        let e = wait(&mut q, &mut wl, &c, |e| {
            matches!(e, Event::Available { .. })
        });
        eprintln!("e2e: {e:?}");
        // the window as the desktop has it, once shown
        let state = loop {
            c.send(Request::GetState);
            let Event::State(s, swire) =
                wait(&mut q, &mut wl, &c, |e| matches!(e, Event::State(..)))
            else {
                unreachable!()
            };
            if let Some(ws) = s.into_iter().find(|ws| ws.xid == XID && ws.shown) {
                break (ws, swire);
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let (state, swire) = state;
        eprintln!("e2e: shown as {state:?}");
        assert_eq!((state.rect.w, state.rect.h), (w, h));
        c.send(Request::GetMonitors);
        let Event::Monitors(monitors) =
            wait(&mut q, &mut wl, &c, |e| matches!(e, Event::Monitors(_)))
        else {
            unreachable!()
        };
        eprintln!("e2e: monitors {monitors:?}");
        let mon = monitors
            .iter()
            .find(|m| m.index == state.monitor)
            .expect("its monitor");
        let wa = mon.work_area;

        // the decision logic on the real replies: X pushes the window past
        // the left edge of the work area; the desktop clamps; X goes back
        let mut g = Geometry::new();
        g.track(XID, state.rect);
        assert_eq!(g.resync(XID, state.rect, state.flags, swire), None);
        let past = Rect::new(wa.x - 50, wa.y + 40, w, h);
        let t = Instant::now();
        assert_eq!(g.x_configured(XID, past, 1, t), None);
        let m = g.frame(XID).expect("a move to send");
        c.send(Request::MoveResize {
            xid: XID,
            rect: m.rect,
            flags: m.flags,
        });
        let Event::Applied {
            applied,
            requested,
            wire: awire,
            ..
        } = wait(&mut q, &mut wl, &c, |e| matches!(e, Event::Applied { .. }))
        else {
            unreachable!()
        };
        eprintln!("e2e: asked {past:?}, applied {applied:?}");
        assert_eq!(
            applied,
            Rect::new(wa.x, wa.y + 40, w, h),
            "clamped to the work area"
        );
        assert_eq!(
            g.applied(XID, requested, applied, Some(awire)),
            Some(applied),
            "X goes back"
        );
        g.x_sent(XID, applied, 2);
        // the desktop's report of it is an echo
        let Event::Changed {
            rect, flags, wire, ..
        } = wait(
            &mut q,
            &mut wl,
            &c,
            |e| matches!(e, Event::Changed { xid: XID, rect, .. } if *rect == applied),
        )
        else {
            unreachable!()
        };
        assert_eq!(g.changed(XID, rect, flags, wire, Instant::now()), None);
        assert_eq!(g.frame(XID), None, "converged after one round trip");

        // state requests come back as state bits
        c.send(Request::SetAbove { xid: XID, on: true });
        wait(
            &mut q,
            &mut wl,
            &c,
            |e| matches!(e, Event::Changed { xid: XID, flags, .. } if flags & st::ABOVE != 0),
        );
        // a window that is not ours (no such tag in this process)
        c.send(Request::Raise { xid: XID + 1 });
        let e = wait(&mut q, &mut wl, &c, |e| matches!(e, Event::Failed { .. }));
        assert!(
            matches!(&e, Event::Failed { xid, error, .. } if *xid == XID + 1 && error.contains("NoSuchWindow")),
            "{e:?}"
        );
        // and gone
        top.destroy();
        xs.destroy();
        surface.destroy();
        let e = wait(&mut q, &mut wl, &c, |e| matches!(e, Event::WindowGone(_)));
        assert_eq!(e, Event::WindowGone(XID));
    }
}
