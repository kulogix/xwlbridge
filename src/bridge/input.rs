//! Turns XTest `FakeInput` events into virtual-input requests, against whatever
//! backend `wayland::input` negotiated.
//!
//! Keys: X keycodes are evdev + 8, so we send `keycode - 8`, and the compositor
//! keymap goes to the virtual keyboard verbatim (see [`Input::set_keymap`]) so
//! they resolve to the keysyms the client meant. Pointer: absolute motion maps
//! through the output [`Layout`], X buttons 4-7 become wheel notches, and
//! anything else is a button.
//!
//! Several X clients can inject at once (RealVNC's agent and x11vnc on one
//! display, say), so input is accounted per client, the way VNC servers
//! account it per viewer (TigerVNC, RealVNC, libvncserver, GNOME Remote
//! Desktop):
//!
//! - each client can only release keys and buttons it pressed, and whatever it
//!   still holds is released when it disconnects (or crashes), so a dead client
//!   never leaves a key stuck. That matters doubly here: the compositor counts
//!   a key as down while *any* device holds it, so a stuck remote Ctrl would
//!   also swallow the local user's Ctrl. The exception is a *one-shot* client,
//!   one that disconnects within [`ONE_SHOT`] of connecting: `xdotool
//!   keydown shift`, `xte 'keydown a'`, or RustDesk, which injects every key
//!   event through a connection of its own. On Xorg what it leaves held stays
//!   down for a later connection to use and release (`xdotool keydown
//!   Shift_L; xdotool key 8` types `*`), so here it *lingers*: any client may
//!   release it, and it goes up by itself once X clients have injected nothing
//!   for [`InputPolicy::linger`] (`-xtestgrace`, default 1 s), and never later
//!   than [`LINGER_CAP`] after its client left ([`linger_due`]). The stock
//!   engines apply the same rule on their X server (src/xvfb/record.rs);
//! - a key or button goes down on the first press across all clients and up on
//!   the last release;
//! - the client that presses a button owns the pointer until it lets go (other
//!   clients' pointer events are dropped meanwhile, so two viewers can't fight
//!   over a drag), lapsing after a timeout so a dead owner can't freeze
//!   everyone: the next other client to move, scroll or click first releases
//!   the lapsed owner's buttons, so nobody drags under a press that isn't
//!   theirs. Outside a drag, the latest event wins. Keys are never arbitrated.
//!
//! `-xtestpersist` restores Xorg's semantics for every client: presses outlive
//! their client, however long it was connected, and nothing but another
//! client's release lets them go (`xdotool keydown ctrl; sleep 5; xdotool keyup
//! ctrl` as separate processes). `-xtestgrace 0` releases even a one-shot
//! client's keys at once. `-pointerowner 0` turns drag ownership off and
//! `-viewonly` drops all input.
//!
//! `wl-uinput-proxy` can sit between us and the compositor and serve these
//! protocols over uinput, for compositors whose virtual-input support is broken
//! or missing.
//!
//! A backend whose devices exist only on demand (GNOME's, whose remote-desktop
//! session shows the screen-sharing indicator) registers a hook with
//! [`Input::set_on_demand`]: input that arrives without devices then waits for
//! them, in order, and the hook asks for them. A client that only types
//! (`xdotool key`) is thereby served like one that watches the screen.

use std::collections::{HashMap, VecDeque};
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};

use crate::util::{Layout, OutputRect};
use xkbcommon_rs::xkb_state::{KeyDirection, StateComponent};
use xkbcommon_rs::{Context, Keymap, KeymapFormat, State};

/// A virtual pointer, abstracted over the protocol backing it. Methods mirror
/// the wl_pointer requests we emit; the implementor batches them into a `frame`.
pub trait VirtualPointer: Send + Sync {
    fn motion_absolute(&self, time: u32, x: u32, y: u32, extent_x: u32, extent_y: u32);
    fn motion(&self, time: u32, dx: f64, dy: f64);
    fn button(&self, time: u32, button: u32, state: ButtonState);
    fn axis(&self, time: u32, axis: Axis, value: f64);
    fn axis_source(&self, source: AxisSource);
    fn axis_discrete(&self, time: u32, axis: Axis, value: f64, discrete: i32);
    fn frame(&self);
}

/// A virtual keyboard, abstracted over the protocol backing it.
pub trait VirtualKeyboard: Send + Sync {
    fn keymap(&self, format: u32, fd: BorrowedFd, size: u32);
    fn key(&self, time: u32, key: u32, state: u32);
    fn modifiers(&self, depressed: u32, latched: u32, locked: u32, group: u32);
}

// XTest/core input event types (X.h)
pub const KEY_PRESS: u8 = 2;
pub const KEY_RELEASE: u8 = 3;
pub const BUTTON_PRESS: u8 = 4;
pub const BUTTON_RELEASE: u8 = 5;
pub const MOTION_NOTIFY: u8 = 6;

// evdev button codes (linux/input-event-codes.h)
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Wayland axis units per wheel notch, matching wlroots/libinput.
const WHEEL_NOTCH: f64 = 15.0;

pub struct Input {
    width: AtomicI32,
    height: AtomicI32,
    start: Instant, // epoch for protocol timestamps; only ordering matters
    /// Installed once the devices exist. Replaceable: a backend whose devices
    /// belong to a session that can end (GNOME's remote-desktop session closes on
    /// lock or when the last viewer leaves) clears and reinstalls them.
    backend: RwLock<Option<Backend>>,
    /// xkb state tracking the modifiers we've injected. A virtual keyboard, unlike
    /// a real one, does not derive modifier state from `key` events, so we replay
    /// each key here and forward the masks via `modifiers` — without it a chord
    /// like Ctrl+C arrives as a bare `c`.
    xkb: Mutex<Option<State>>,
    /// Physical-to-logical output layout for absolute motion. `None` until the
    /// first output is known, which falls back to a proportional mapping over
    /// the screen geometry.
    layout: Mutex<Option<Layout>>,
    /// Who holds what, and who owns the pointer: see the module docs.
    state: Mutex<InputState>,
    policy: InputPolicy,
    /// Input waiting for devices an on-demand backend is creating.
    on_demand: Mutex<OnDemand>,
    /// When an X client last pressed something or moved the pointer, as
    /// milliseconds since `start` plus one (0: never).
    last_demand_ms: AtomicU64,
    /// What the X server shows on each keycode (its level-1 keysym), as last
    /// told by [`set_x_key_syms`](Self::set_x_key_syms): a key that waits for
    /// devices is stamped with it, so that if the compositor turns out to be
    /// on another layout once they come, the key is replayed where that
    /// layout has the keysym the client meant.
    x_syms: Mutex<Option<std::sync::Arc<[u32]>>>,
    /// Keys a replay pressed on another keycode than the client did (after
    /// such a layout change), per (client, X keycode): where the client's
    /// release of the key has to go, 0 if the press was dropped.
    moved: Mutex<HashMap<(u32, u8), u8>>,
    /// When an X client last injected anything (any event type), as
    /// milliseconds since `start` plus one (0: never): what one-shot clients
    /// left held lingers while input keeps coming ([`linger_due`]).
    last_input_ms: AtomicU64,
    /// Wakes the linger thread ([`spawn_linger`](Self::spawn_linger)) when a
    /// hold starts lingering; paired with `state`.
    linger_cv: Condvar,
    /// Whether that thread runs: without it nothing would ever release a
    /// lingering hold, so departed clients' holds go up at once instead.
    linger_running: AtomicBool,
}

/// A client that disconnects within this long of connecting is a one-shot
/// injector: what it leaves held lingers (module docs). One connection per
/// event (RustDesk, `rdev::simulate`) lives about a millisecond, an
/// `xdotool`/`xte` process tens of milliseconds; a VNC server, or anything
/// else whose death should not leave a key down, lives on.
pub const ONE_SHOT: Duration = Duration::from_secs(1);

/// What a one-shot client left held goes up this long after it left at the
/// latest, however busy other clients are (or after `-xtestgrace`, if longer).
pub const LINGER_CAP: Duration = Duration::from_secs(10);

/// The default `-xtestgrace`, in milliseconds.
pub const LINGER_DEFAULT_MS: u32 = 1000;

/// When a hold that a one-shot client left at `left` goes up, input having
/// last come from any X client at `active`: `linger` after the later of the
/// two, and no later than [`LINGER_CAP`] (or `linger`, if longer) after
/// `left`. A RustDesk user holding Ctrl while clicking or scrolling keeps it
/// down, as does `xdotool keydown shift` followed by more xdotool commands; a
/// key nobody will release goes up within `linger` of input pausing.
pub fn linger_due(left: Instant, active: Instant, linger: Duration) -> Instant {
    (left.max(active) + linger).min(left + LINGER_CAP.max(linger))
}

/// At most this much input waits for devices (pointer motion is coalesced, and
/// releases always get in, so nothing queued is left pressed).
const PENDING_MAX: usize = 512;
/// Input that waited longer than this is dropped rather than replayed: a
/// session normally comes up in well under a second, and keys typed long ago
/// must not land in whatever has the focus by then.
const PENDING_MAX_AGE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(5)
};
/// A client whose waiting input was dropped as too old loses the rest of that
/// burst too: everything it sends until it pauses this long. Otherwise the end
/// of a word typed while the session was slow to come would be typed without
/// its beginning ("cdefgh" for "abcdefgh"),
/// which in a shell is another command.
const BURST_GAP: Duration = if cfg!(test) {
    Duration::from_millis(250)
} else {
    Duration::from_millis(500)
};

/// Something that arrived while there were no devices.
enum Pending {
    Event {
        at: Instant,
        client: u32,
        type_: u8,
        detail: u8,
        x: i16,
        y: i16,
        /// A key's level-1 keysym on the X keyboard when it was queued (0:
        /// not a key, or unknown).
        sym: u32,
    },
    /// The client disconnected after the input above, having been connected
    /// for `lived`: what it still holds once that is replayed is released
    /// (or lingers, see [`ONE_SHOT`]).
    Gone { client: u32, lived: Duration },
}

#[derive(Default)]
struct OnDemand {
    /// Called (on the injecting thread) when input wants devices.
    hook: Option<Box<dyn Fn() + Send + Sync>>,
    queue: VecDeque<Pending>,
    /// Devices aren't coming for now (the screen is locked, or the local user
    /// stopped sharing): input without them is dropped, not queued.
    refused: bool,
    /// Clients whose waiting input was dropped as too old, and when they last
    /// sent anything: the rest of that burst is dropped too ([`BURST_GAP`]).
    cut: HashMap<u32, Instant>,
}

impl OnDemand {
    /// Drops input too old to replay: all of a client's waiting input once any
    /// of it is too old (never the end of a burst without its beginning), and
    /// marks the client cut off for the rest of that burst. Returns the clients
    /// whose disconnect markers went with it (their input was dropped, but
    /// what they held before still has to be let go), with how long each
    /// was connected, and how many events were dropped.
    fn expire(&mut self) -> (Vec<(u32, Duration)>, usize) {
        let stale: std::collections::HashSet<u32> = self
            .queue
            .iter()
            .filter_map(|p| match p {
                Pending::Event { at, client, .. } if at.elapsed() > PENDING_MAX_AGE => Some(*client),
                _ => None,
            })
            .collect();
        if stale.is_empty() {
            return (Vec::new(), 0);
        }
        let (mut gone, mut dropped) = (Vec::new(), 0);
        self.queue.retain(|p| match p {
            Pending::Event { client, at, .. } if stale.contains(client) => {
                let last = self.cut.entry(*client).or_insert(*at);
                *last = (*last).max(*at);
                dropped += 1;
                false
            }
            Pending::Gone { client, lived } if stale.contains(client) => {
                gone.push((*client, *lived));
                false
            }
            _ => true,
        });
        // a client that has gone has no burst left to cut
        for (c, _) in &gone {
            self.cut.remove(c);
        }
        (gone, dropped)
    }

    /// Whether this event from `client` belongs to a burst whose start was
    /// dropped (see [`expire`](Self::expire)): then it is dropped too, except
    /// a release, which is let through (it may end a press made before the
    /// burst; one of a dropped press does nothing).
    fn cut_off(&mut self, client: u32, type_: u8, now: Instant) -> bool {
        let Some(last) = self.cut.get_mut(&client) else { return false };
        if now.duration_since(*last) >= BURST_GAP {
            self.cut.remove(&client);
            return false;
        }
        *last = now;
        matches!(type_, KEY_PRESS | BUTTON_PRESS | MOTION_NOTIFY)
    }
}

/// How input from several X clients is combined (from the command line).
#[derive(Clone, Copy)]
pub struct InputPolicy {
    /// Keep a client's held keys and buttons down after it disconnects, as
    /// Xorg does, until any client releases them (`-xtestpersist`).
    pub persist: bool,
    /// How long what a one-shot client left held lingers after the last
    /// input from any X client (`-xtestgrace`; see [`linger_due`]). Zero:
    /// released when the client leaves, like any other client's.
    pub linger: Duration,
    /// How long a client holding a button may go without sending pointer
    /// input (motion, wheel or a press; keys don't count) and still keep the
    /// pointer from other clients: an idle timeout, not a cap, so an owner
    /// that keeps dragging keeps the pointer, as with TigerVNC. Zero disables
    /// ownership (`-pointerowner`).
    pub owner_timeout: Duration,
    /// Drop all injected input (`-viewonly`).
    pub view_only: bool,
}

impl Default for InputPolicy {
    fn default() -> Self {
        Self {
            persist: false,
            linger: Duration::from_millis(LINGER_DEFAULT_MS.into()),
            owner_timeout: Duration::from_secs(10),
            view_only: false,
        }
    }
}

/// What one X client holds.
#[derive(Default)]
struct Held {
    keys: [u64; 4],
    buttons: u16, // bit = index into BUTTON_CODES
}

struct InputState {
    /// Live clients only: an entry goes when its client does (connection ids
    /// are never reused, so a departed client's entry would never be looked
    /// up again, only pile up).
    clients: HashMap<u32, Held>,
    /// What departed clients left held: with `-xtestpersist` everything, else
    /// what one-shot clients left, lingering. Any client may release these,
    /// as in Xorg, where XTest state belongs to the device.
    orphan: Held,
    /// For each lingering orphan key (by X keycode) and button (by index into
    /// BUTTON_CODES), when its last holder left; `None` with `-xtestpersist`.
    /// One entry per key, so this stays bounded however many clients come.
    key_left: [Option<Instant>; 256],
    btn_left: [Option<Instant>; BUTTON_CODES.len()],
    /// How many clients hold each X keycode / button: the compositor sees a
    /// press on 0->1 and a release on 1->0.
    key_count: [u8; 256],
    btn_count: [u8; 16],
    /// The client dragging, and when it last sent pointer input.
    owner: Option<u32>,
    owner_last: Instant,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            clients: HashMap::new(),
            orphan: Held::default(),
            key_left: [None; 256],
            btn_left: [None; BUTTON_CODES.len()],
            key_count: [0; 256],
            btn_count: [0; 16],
            owner: None,
            owner_last: Instant::now(),
        }
    }
}

/// X buttons that are real buttons (4-7 are wheel notches), and their evdev codes.
const BUTTON_CODES: [(u8, u32); 5] = [
    (1, BTN_LEFT),
    (2, BTN_MIDDLE),
    (3, BTN_RIGHT),
    (8, BTN_SIDE),
    (9, BTN_EXTRA),
];

/// Locks that survive a panic elsewhere: the state behind them is plain
/// counters and handles, so one client's thread dying mid-update must not make
/// every other client's next event panic too.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Backend {
    /// Pushes queued requests to the compositor after each event.
    flush: Box<dyn Fn() + Send + Sync>,
    pointer: Box<dyn VirtualPointer>,
    keyboard: Box<dyn VirtualKeyboard>,
    keymap_set: AtomicBool, // the virtual keyboard rejects `key` until a keymap is set
}

impl Input {
    pub fn new(width: u16, height: u16, policy: InputPolicy) -> Self {
        Self {
            width: AtomicI32::new(width.into()),
            height: AtomicI32::new(height.into()),
            start: Instant::now(),
            backend: RwLock::new(None),
            xkb: Mutex::new(None),
            layout: Mutex::new(None),
            state: Mutex::new(InputState::default()),
            policy,
            on_demand: Mutex::new(OnDemand::default()),
            last_demand_ms: AtomicU64::new(0),
            x_syms: Mutex::new(None),
            moved: Mutex::new(HashMap::new()),
            last_input_ms: AtomicU64::new(0),
            linger_cv: Condvar::new(),
            linger_running: AtomicBool::new(false),
        }
    }

    /// Starts the thread that releases what one-shot clients left lingering
    /// once it is due (the built-in engine's; the stock engines' agent does
    /// this for their X server). Without it, departed clients' holds go up
    /// at once.
    pub fn spawn_linger(self: &Arc<Self>) {
        if self.policy.persist || self.policy.linger.is_zero() {
            return;
        }
        let input = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("xtest-linger".into())
            .spawn(move || {
                while let Some(i) = input.upgrade() {
                    i.linger_wait();
                    i.expire_lingering(Instant::now());
                }
            });
        match spawned {
            Ok(_) => self.linger_running.store(true, Ordering::Release),
            Err(e) => crate::warning!(
                "can't start the thread for keys one-shot X clients leave held ({e}); \
                 they are released when their client leaves"
            ),
        }
    }

    /// Waits until a lingering hold may be due, or one starts lingering.
    fn linger_wait(&self) {
        let st = lock(&self.state);
        let wait = match self.next_due(&st) {
            Some(due) => due.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(60),
        };
        if !wait.is_zero() {
            drop(self.linger_cv.wait_timeout(st, wait).unwrap_or_else(PoisonError::into_inner));
        }
    }

    /// When X clients last injected input (`start` if they never did).
    fn last_input(&self) -> Instant {
        let ms = self.last_input_ms.load(Ordering::Relaxed);
        self.start + Duration::from_millis(ms.saturating_sub(1))
    }

    /// When the first lingering hold is due to go up, if any is lingering.
    fn next_due(&self, st: &InputState) -> Option<Instant> {
        let active = self.last_input();
        st.key_left
            .iter()
            .chain(st.btn_left.iter())
            .flatten()
            .map(|&left| linger_due(left, active, self.policy.linger))
            .min()
    }

    /// Releases the lingering holds due by `now` (see [`linger_due`]): each
    /// goes up for the compositor unless a live client holds it too.
    fn expire_lingering(&self, now: Instant) {
        if self.policy.persist {
            return;
        }
        let guard = self.backend.read().unwrap_or_else(PoisonError::into_inner);
        let active = self.last_input();
        let due = |left: Option<Instant>| left.is_some_and(|l| now >= linger_due(l, active, self.policy.linger));
        let (keys, buttons) = {
            let mut st = lock(&self.state);
            let st = &mut *st;
            let mut keys = Vec::new();
            for kc in 0..=u8::MAX {
                if !due(st.key_left[kc as usize]) {
                    continue;
                }
                st.key_left[kc as usize] = None;
                let (w, bit) = ((kc / 64) as usize, 1u64 << (kc % 64));
                if st.orphan.keys[w] & bit != 0 {
                    st.orphan.keys[w] &= !bit;
                    let count = &mut st.key_count[kc as usize];
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        keys.push(kc);
                    }
                }
            }
            let mut buttons = Vec::new();
            for i in 0..BUTTON_CODES.len() {
                if !due(st.btn_left[i]) {
                    continue;
                }
                st.btn_left[i] = None;
                if st.orphan.buttons & (1 << i) != 0 {
                    st.orphan.buttons &= !(1 << i);
                    st.btn_count[i] = st.btn_count[i].saturating_sub(1);
                    if st.btn_count[i] == 0 {
                        buttons.push(i);
                    }
                }
            }
            (keys, buttons)
        };
        let Some(b) = guard.as_ref() else { return };
        for &kc in &keys {
            b.keyboard.key(self.time(), u32::from(kc).wrapping_sub(8), 0);
            self.update_modifiers(b, kc, false);
        }
        for &i in &buttons {
            b.pointer.button(self.time(), BUTTON_CODES[i].1, ButtonState::Released);
            b.pointer.frame();
        }
        let released = keys.len() + buttons.len();
        if released > 0 {
            crate::log!(
                "released {released} key(s)/button(s) a disconnected X client was holding \
                 (a one-shot client's; no X client released them in time)"
            );
            (b.flush)();
        }
    }

    /// Records what the X server shows on each keycode now: `syms[kc]` is the
    /// keysym keycode `kc` types without modifiers (0: none). The X side
    /// calls this whenever its keyboard changes (the built-in engine's served
    /// layout, a stock engine's uploaded one).
    #[allow(dead_code)] // only the gnome backend queues input
    pub fn set_x_key_syms(&self, syms: Option<std::sync::Arc<[u32]>>) {
        *lock(&self.x_syms) = syms;
    }

    fn x_sym(&self, kc: u8) -> u32 {
        lock(&self.x_syms)
            .as_ref()
            .and_then(|s| s.get(usize::from(kc)).copied())
            .unwrap_or(0)
    }

    /// Makes input that arrives while no devices are installed wait for them,
    /// calling `hook` so the backend creates them; see the module docs.
    #[allow(dead_code)] // only the gnome backend's devices come on demand
    pub fn set_on_demand(&self, hook: Box<dyn Fn() + Send + Sync>) {
        lock(&self.on_demand).hook = Some(hook);
    }

    /// Whether an X client pressed a key or button, or moved the pointer,
    /// within the last `ms` milliseconds (a backend keeps its devices for it).
    #[allow(dead_code)]
    pub fn demanded_within(&self, ms: u64) -> bool {
        let last = self.last_demand_ms.load(Ordering::Relaxed);
        last != 0 && (self.start.elapsed().as_millis() as u64 + 1).saturating_sub(last) < ms
    }

    /// Whether input is waiting for devices.
    #[allow(dead_code)]
    pub fn has_pending(&self) -> bool {
        !lock(&self.on_demand).queue.is_empty()
    }

    /// Injects the input that waited for devices, in order, now that they are
    /// installed; what waited too long is dropped. Input arriving meanwhile
    /// queues behind it, so nothing overtakes. Keys go where they were
    /// pressed (see [`replay_pending_with`](Self::replay_pending_with)).
    #[allow(dead_code)]
    pub fn replay_pending(&self) {
        self.replay_pending_with(&|kc, _, _| Some(kc));
    }

    /// [`replay_pending`](Self::replay_pending), with each key press
    /// re-resolved first: `resolve(keycode, keysym, shortcut)` gets the
    /// keycode the client pressed, the keysym the X keyboard showed there
    /// when it did (see [`set_x_key_syms`](Self::set_x_key_syms)) and whether
    /// a shortcut modifier is held, and says which keycode types that keysym
    /// on the compositor keyboard now (`None`: none does; the key is
    /// dropped). The compositor's layout is only learned with its devices,
    /// so input typed before them was mapped with the layout in force then,
    /// which a local input-source switch may have changed. The key's release
    /// follows its press, whenever it comes.
    #[allow(dead_code)]
    pub fn replay_pending_with(&self, resolve: &dyn Fn(u8, u32, bool) -> Option<u8>) {
        if self.backend.read().unwrap_or_else(PoisonError::into_inner).is_none() {
            return;
        }
        let mut od = lock(&self.on_demand);
        let (mut replayed, mut moved, mut dropped) = (0, 0, 0);
        // whole bursts of what waited too long go first
        let (gone, mut stale) = od.expire();
        for (c, lived) in gone {
            self.departed(c, lived);
        }
        while let Some(p) = od.queue.pop_front() {
            match p {
                Pending::Event { at, .. } if at.elapsed() > PENDING_MAX_AGE => stale += 1,
                Pending::Event {
                    client,
                    type_: KEY_PRESS,
                    detail,
                    sym,
                    ..
                } if sym != 0 => {
                    let to = resolve(detail, sym, self.shortcut_held());
                    match to {
                        Some(kc) if kc == detail => {}
                        Some(_) => moved += 1,
                        None => dropped += 1,
                    }
                    if to != Some(detail) {
                        lock(&self.moved).insert((client, detail), to.unwrap_or(0));
                    }
                    if let Some(kc) = to {
                        self.inject(client, KEY_PRESS, kc, 0, 0);
                    }
                    replayed += 1;
                }
                Pending::Event {
                    client,
                    type_,
                    detail,
                    x,
                    y,
                    ..
                } => {
                    self.inject(client, type_, detail, x, y);
                    replayed += 1;
                }
                Pending::Gone { client, lived } => self.departed(client, lived),
            }
        }
        drop(od);
        if replayed + stale > 0 {
            crate::vlog!(
                "replayed {replayed} input event(s) that arrived before the devices{}{}",
                if moved + dropped > 0 {
                    format!(
                        " ({moved} key(s) moved to where the active layout has them, {dropped} \
                         dropped that it lacks)"
                    )
                } else {
                    String::new()
                },
                if stale > 0 { format!(", dropped {stale} that waited too long") } else { String::new() }
            );
        }
    }

    /// Drops the input waiting for devices, and any that arrives without them
    /// until [`accept_pending`](Self::accept_pending): they aren't coming (the
    /// screen is locked, or the local user stopped sharing), and what X
    /// clients type meanwhile must never be typed into the desktop later.
    #[allow(dead_code)]
    pub fn drop_pending(&self) {
        let queue = {
            let mut od = lock(&self.on_demand);
            od.refused = true;
            std::mem::take(&mut od.queue)
        };
        let mut dropped = 0;
        for p in queue {
            match p {
                Pending::Event { .. } => dropped += 1,
                Pending::Gone { client, lived } => self.departed(client, lived),
            }
        }
        if dropped > 0 {
            crate::log!("dropped {dropped} input event(s) that arrived while input was unavailable");
        }
    }

    /// Devices are coming again (a session is starting): input that arrives
    /// before them waits for them again.
    #[allow(dead_code)]
    pub fn accept_pending(&self) {
        lock(&self.on_demand).refused = false;
    }

    /// Queues an event for devices still to come, if that is what it has to
    /// do: there are none (and they come on demand), or earlier input is still
    /// queued and must go first. Returns whether it was taken (queued or, for
    /// a release with nothing queued to pair with, dropped as before).
    fn wait_for_devices(&self, client: u32, type_: u8, detail: u8, x: i16, y: i16) -> bool {
        let mut od = lock(&self.on_demand);
        if od.hook.is_none() {
            return false;
        }
        let (gone, stale) = od.expire();
        if stale > 0 {
            crate::vlog!("dropped {stale} input event(s) of a burst that waited too long for devices");
        }
        for (c, lived) in gone {
            self.departed(c, lived);
        }
        if od.cut_off(client, type_, Instant::now()) {
            return true;
        }
        if od.queue.is_empty() && self.backend.read().unwrap_or_else(PoisonError::into_inner).is_some() {
            return false;
        }
        let demand = matches!(type_, KEY_PRESS | BUTTON_PRESS | MOTION_NOTIFY);
        if od.refused || (!demand && od.queue.is_empty()) {
            // refused for now, or a release of nothing anyone pressed since
            // the devices went
            if let Some(hook) = od.hook.as_ref() {
                hook();
            }
            return true;
        }
        let at = Instant::now();
        let sym = if matches!(type_, KEY_PRESS | KEY_RELEASE) { self.x_sym(detail) } else { 0 };
        let coalesced = type_ == MOTION_NOTIFY
            && detail == 0
            && match od.queue.back_mut() {
                Some(Pending::Event {
                    client: c,
                    type_: MOTION_NOTIFY,
                    detail: 0,
                    x: px,
                    y: py,
                    at: pat,
                    ..
                }) if *c == client => {
                    (*px, *py, *pat) = (x, y, at);
                    true
                }
                _ => false,
            };
        if !coalesced && (!demand || od.queue.len() < PENDING_MAX) {
            od.queue.push_back(Pending::Event {
                at,
                client,
                type_,
                detail,
                x,
                y,
                sym,
            });
        }
        if let Some(hook) = od.hook.as_ref() {
            hook();
        }
        true
    }

    /// Installs the virtual-input objects, once the seat and both backends
    /// exist, replacing any installed before. `flush` sends what an event queued.
    pub fn set_devices(
        &self,
        flush: Box<dyn Fn() + Send + Sync>,
        pointer: Box<dyn VirtualPointer>,
        keyboard: Box<dyn VirtualKeyboard>,
    ) {
        *self.backend.write().unwrap_or_else(PoisonError::into_inner) = Some(Backend {
            flush,
            pointer,
            keyboard,
            keymap_set: AtomicBool::new(false),
        });
    }

    /// Removes the virtual-input objects, when the session they belong to ends.
    /// Input is dropped until [`set_devices`](Self::set_devices) runs again, and
    /// held keys are forgotten (the compositor releases a destroyed device's
    /// keys itself), so a key held across the gap isn't left half-pressed here.
    #[allow(dead_code)] // only the gnome backend's devices can go away
    pub fn clear_devices(&self) {
        *self.backend.write().unwrap_or_else(PoisonError::into_inner) = None;
        // the compositor released the dead devices' keys and buttons itself;
        // start the accounting over (later releases of them are just dropped)
        *lock(&self.state) = InputState::default();
        lock(&self.moved).clear();
    }

    /// Forwards the compositor keymap to the virtual keyboard. Key events are
    /// dropped until this lands, since the protocol demands a keymap first.
    /// A no-op before [`set_devices`](Self::set_devices), which re-forwards.
    pub fn set_keymap(&self, format: u32, fd: BorrowedFd, size: u32) {
        if let Some(b) = self.backend.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
            b.keyboard.keymap(format, fd, size);
            b.keymap_set.store(true, Ordering::Release);
            (b.flush)();
        }
    }

    /// Builds the modifier-tracking xkb state from the keymap text. Called
    /// alongside [`set_keymap`](Self::set_keymap); see the `xkb` field.
    pub fn set_modifier_keymap(&self, text: &str) {
        let state = Context::new(0)
            .ok()
            .and_then(|ctx| Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok())
            .map(State::new);
        let Some(mut state) = state else { return };
        // replay held keys so a keymap change doesn't drop modifiers that are
        // still down
        let key_count = lock(&self.state).key_count;
        for kc in 0..=u8::MAX {
            if key_count[kc as usize] != 0 && state.key_get_layout(u32::from(kc)).is_some()
            {
                state.update_key(u32::from(kc), KeyDirection::Down);
            }
        }
        *lock(&self.xkb) = Some(state);
    }

    /// Whether a modifier that makes keys shortcuts rather than text is in
    /// effect from the keys X clients have pressed: Control, Mod1 (Alt), Mod3
    /// or Mod4 (Super, Hyper); not Shift, Lock, NumLock or AltGr, which pick
    /// a character. See [`KeyTable::plan`](crate::bridge::keymap::KeyTable::plan).
    pub fn shortcut_held(&self) -> bool {
        let guard = lock(&self.xkb);
        let Some(state) = guard.as_ref() else {
            return false;
        };
        ["Control", "Mod1", "Mod3", "Mod4"].iter().any(|m| {
            state
                .mod_name_is_active(m, StateComponent::MODS_EFFECTIVE)
                .unwrap_or(false)
        })
    }

    /// [`shortcut_held`](Self::shortcut_held), counting also the keys still
    /// waiting for devices (pressed and not yet released in the queue): with
    /// no session running, a client's `ctrl+c` queues its Control, and the
    /// `c` that follows (on a spare keycode, a keysym only another layout has)
    /// is a shortcut all the same. For the X
    /// side deciding a key at request time, not for the replay, which injects
    /// the queued keys in order.
    pub fn shortcut_held_or_queued(&self) -> bool {
        // collected first: Input takes `on_demand` before `xkb`, never both here
        let queued: Vec<u8> = {
            let od = lock(&self.on_demand);
            let mut down: Vec<u8> = Vec::new();
            for p in &od.queue {
                if let Pending::Event { type_, detail, .. } = p {
                    match *type_ {
                        KEY_PRESS if !down.contains(detail) => down.push(*detail),
                        KEY_RELEASE => down.retain(|k| k != detail),
                        _ => {}
                    }
                }
            }
            down
        };
        if queued.is_empty() {
            return self.shortcut_held();
        }
        if self.shortcut_held() {
            return true;
        }
        let guard = lock(&self.xkb);
        let Some(state) = guard.as_ref() else {
            return false;
        };
        // a queued key whose keysym is one of those modifiers' keys
        queued.iter().any(|&kc| {
            state.key_get_layout(u32::from(kc)).is_some()
                && state.key_get_syms(u32::from(kc)).iter().any(|s| {
                    matches!(
                        s.raw(),
                        0xffe3 | 0xffe4 // Control_L, Control_R
                            | 0xffe7..=0xffea // Meta_L/R, Alt_L/R
                            | 0xffeb..=0xffee // Super_L/R, Hyper_L/R
                    )
                })
        })
    }

    /// Whether `client` holds X keycode `x_keycode` down.
    pub fn holds(&self, client: u32, x_keycode: u8) -> bool {
        lock(&self.state)
            .clients
            .get(&client)
            .is_some_and(|h| h.keys[usize::from(x_keycode / 64)] & (1 << (x_keycode % 64)) != 0)
    }

    /// Records `client`'s key press or release, returning whether the
    /// compositor should see it: a repeated press, or a release of a key this
    /// client doesn't hold (and no departed client left held, under
    /// `-xtestpersist`), changes nothing; otherwise only the first press and
    /// last release across clients go through. A release also ends a departed
    /// client's hold on the key, as in Xorg, which keeps one state per key.
    fn track_key(&self, client: u32, x_keycode: u8, press: bool) -> bool {
        let mut st = lock(&self.state);
        let st = &mut *st;
        let w = (x_keycode / 64) as usize;
        let bit = 1u64 << (x_keycode % 64);
        let holds = st.clients.get(&client).is_some_and(|h| h.keys[w] & bit != 0);
        let count = &mut st.key_count[x_keycode as usize];
        if press {
            if holds {
                return false; // autorepeat
            }
            st.clients.entry(client).or_default().keys[w] |= bit;
            *count = count.saturating_add(1);
            return *count == 1;
        }
        // a release: its own hold and any a departed client left. (A stray
        // release creates no entry: only presses do.)
        let mut dropped = 0;
        if holds && let Some(h) = st.clients.get_mut(&client) {
            h.keys[w] &= !bit;
            dropped += 1;
        }
        if st.orphan.keys[w] & bit != 0 {
            st.orphan.keys[w] &= !bit;
            st.key_left[x_keycode as usize] = None;
            dropped += 1;
        }
        if dropped == 0 {
            return false; // not this client's to release
        }
        let count = &mut st.key_count[x_keycode as usize];
        *count = count.saturating_sub(dropped);
        *count == 0
    }

    /// Whether `client` may use the pointer now (no other client is dragging,
    /// or the drag has lapsed), refreshing the owner's liveness. A lapsed drag
    /// is ended first, its owner's buttons released, so the motion, wheel or
    /// press let through never runs under a button its client doesn't hold.
    fn pointer_allowed(&self, b: &Backend, st: &mut InputState, client: u32) -> bool {
        match st.owner {
            Some(o) if o == client => {
                st.owner_last = Instant::now();
                true
            }
            Some(o) => {
                if st.owner_last.elapsed() < self.policy.owner_timeout {
                    return false; // another client is dragging
                }
                self.end_lapsed_drag(b, st, o);
                true
            }
            None => true,
        }
    }

    /// Ends the drag of `owner`, which went quiet holding a button (its viewer
    /// hung mid-drag, say): its buttons go up where no other client holds
    /// them, at the pointer's current position, and ownership clears.
    fn end_lapsed_drag(&self, b: &Backend, st: &mut InputState, owner: u32) {
        crate::log!("pointer ownership of X client {owner} lapsed; releasing its buttons");
        if let Some(held) = st.clients.get_mut(&owner) {
            for (j, &(_, c)) in BUTTON_CODES.iter().enumerate() {
                if held.buttons & (1 << j) != 0 {
                    held.buttons &= !(1 << j);
                    st.btn_count[j] = st.btn_count[j].saturating_sub(1);
                    if st.btn_count[j] == 0 {
                        b.pointer.button(self.time(), c, ButtonState::Released);
                        b.pointer.frame();
                    }
                }
            }
        }
        st.owner = None;
    }

    /// Releases everything an X client held, when it disconnects after being
    /// connected for `lived`. Its keys and buttons go up for the compositor
    /// only where no other client still holds them; a one-shot client's
    /// ([`ONE_SHOT`]) linger instead, and `-xtestpersist` keeps them all.
    /// With its input still waiting for devices, that happens once the input
    /// has been replayed.
    pub fn client_gone(&self, client: u32, lived: Duration) {
        {
            let mut od = lock(&self.on_demand);
            od.cut.remove(&client);
            let queued = od.queue.iter().any(|p| matches!(p, Pending::Event { client: c, .. } if *c == client));
            if queued {
                od.queue.push_back(Pending::Gone { client, lived });
                return;
            }
        }
        self.departed(client, lived);
    }

    /// A client is gone (now, or its queued input has been replayed): its
    /// holds end as the policy says ([`client_gone`](Self::client_gone)):
    /// released, or (`-xtestpersist`, or a one-shot client's) kept as orphans.
    fn departed(&self, client: u32, lived: Duration) {
        let now = Instant::now();
        let linger = !self.policy.persist
            && !self.policy.linger.is_zero()
            && lived < ONE_SHOT
            && self.linger_running.load(Ordering::Acquire);
        if !self.policy.persist && !linger {
            self.release_client(client);
            return;
        }
        lock(&self.moved).retain(|&(c, _), _| c != client);
        let mut st = lock(&self.state);
        let st = &mut *st;
        if st.owner == Some(client) {
            st.owner = None;
        }
        // its presses stay, as orphans any client may release; where an
        // orphan already holds the same key or button, two holders become
        // one (lingering from now on)
        let Some(h) = st.clients.remove(&client) else { return };
        let mut kept = 0;
        for (w, &word) in h.keys.iter().enumerate() {
            let dup = st.orphan.keys[w] & word;
            st.orphan.keys[w] |= word;
            for bit in 0..64 {
                if word & (1 << bit) == 0 {
                    continue;
                }
                kept += 1;
                if dup & (1 << bit) != 0 {
                    let c = &mut st.key_count[w * 64 + bit];
                    *c = c.saturating_sub(1);
                }
                if linger {
                    st.key_left[w * 64 + bit] = Some(now);
                }
            }
        }
        let dup = st.orphan.buttons & h.buttons;
        st.orphan.buttons |= h.buttons;
        for i in 0..BUTTON_CODES.len() {
            if h.buttons & (1 << i) == 0 {
                continue;
            }
            kept += 1;
            if dup & (1 << i) != 0 {
                st.btn_count[i] = st.btn_count[i].saturating_sub(1);
            }
            if linger {
                st.btn_left[i] = Some(now);
            }
        }
        if linger && kept > 0 {
            crate::vlog!(
                "one-shot X client {client} left after {} ms: its {kept} key(s)/button(s) stay down \
                 for another client to release",
                lived.as_millis()
            );
            self.linger_cv.notify_all();
        }
    }

    /// Releases everything `client` held, as [`client_gone`](Self::client_gone)
    /// does but whatever the policy: for an input source that can never
    /// release anything again (the stock engines' agent, when it stops).
    pub fn release_client(&self, client: u32) {
        lock(&self.moved).retain(|&(c, _), _| c != client);
        let held = {
            let mut st = lock(&self.state);
            if st.owner == Some(client) {
                st.owner = None;
            }
            st.clients.remove(&client)
        };
        let Some(held) = held else { return };
        let guard = self.backend.read().unwrap_or_else(PoisonError::into_inner);
        let Some(b) = guard.as_ref() else { return };
        let mut released = 0;
        for kc in 0..=u8::MAX {
            if held.keys[(kc / 64) as usize] & (1 << (kc % 64)) == 0 {
                continue;
            }
            let last = {
                let mut st = lock(&self.state);
                let count = &mut st.key_count[kc as usize];
                *count = count.saturating_sub(1);
                *count == 0
            };
            if last {
                b.keyboard.key(self.time(), u32::from(kc).wrapping_sub(8), 0);
                self.update_modifiers(b, kc, false);
                released += 1;
            }
        }
        for (i, &(_, code)) in BUTTON_CODES.iter().enumerate() {
            if held.buttons & (1 << i) != 0 && self.drop_button_count(i) {
                b.pointer.button(self.time(), code, ButtonState::Released);
                b.pointer.frame();
                released += 1;
            }
        }
        if released > 0 {
            crate::log!("released {released} key(s)/button(s) a disconnected X client was holding");
            (b.flush)();
        }
    }

    /// Decrements a button's holder count, returning whether it hit zero.
    fn drop_button_count(&self, i: usize) -> bool {
        let mut st = lock(&self.state);
        let count = &mut st.btn_count[i];
        *count = count.saturating_sub(1);
        *count == 0
    }

    /// Replays a key into the xkb state and forwards the resulting masks via
    /// `modifiers`. Sent after every key; an unchanged mask is a no-op the
    /// compositor diffs away.
    fn update_modifiers(&self, b: &Backend, x_keycode: u8, press: bool) {
        let mut guard = lock(&self.xkb);
        let Some(state) = guard.as_mut() else { return };
        let dir = if press {
            KeyDirection::Down
        } else {
            KeyDirection::Up
        };
        // xkb keycode == X keycode == evdev + 8, so pass `detail` unshifted
        let kc = u32::from(x_keycode);
        // xkbcommon-rs panics ("Key has no valid layout") inside update_key for a
        // keycode that is in the keymap but has no symbol groups, which RealVNC
        // does inject. key_get_layout makes the same check it would panic on, and
        // such keys can't affect modifier state anyway.
        if state.key_get_layout(kc).is_none() {
            return;
        }
        state.update_key(kc, dir);
        let depressed = state.serialize_mods(StateComponent::MODS_DEPRESSED);
        let latched = state.serialize_mods(StateComponent::MODS_LATCHED);
        let locked = state.serialize_mods(StateComponent::MODS_LOCKED);
        let group = state.serialize_layout(StateComponent::LAYOUT_EFFECTIVE) as u32;
        b.keyboard.modifiers(depressed, latched, locked, group);
    }

    /// Updates the screen geometry absolute motion scales against, on resize.
    pub fn set_geometry(&self, width: u16, height: u16) {
        self.width.store(width.into(), Ordering::Relaxed);
        self.height.store(height.into(), Ordering::Relaxed);
    }

    /// Installs the output layout absolute motion remaps through, on any output
    /// change.
    pub fn set_layout(&self, outputs: Vec<OutputRect>) {
        *lock(&self.layout) = Layout::new(outputs);
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Handles one XTest `FakeInput` event from X client `client`.
    ///
    /// A `MOTION_NOTIFY` with `detail == 0` is absolute, otherwise relative.
    pub fn fake_input(&self, client: u32, type_: u8, detail: u8, x: i16, y: i16) {
        if self.policy.view_only {
            return;
        }
        let now_ms = self.start.elapsed().as_millis() as u64 + 1;
        if matches!(type_, KEY_PRESS | BUTTON_PRESS | MOTION_NOTIFY) {
            self.last_demand_ms.store(now_ms, Ordering::Relaxed);
        }
        self.last_input_ms.store(now_ms, Ordering::Relaxed);
        if self.wait_for_devices(client, type_, detail, x, y) {
            return;
        }
        self.inject(client, type_, detail, x, y);
    }

    fn inject(&self, client: u32, type_: u8, detail: u8, x: i16, y: i16) {
        let guard = self.backend.read().unwrap_or_else(PoisonError::into_inner);
        let Some(b) = guard.as_ref() else { return };
        match type_ {
            KEY_PRESS | KEY_RELEASE => {
                let press = type_ == KEY_PRESS;
                let detail = if press {
                    detail
                } else {
                    match lock(&self.moved).remove(&(client, detail)) {
                        Some(0) => return, // its press was dropped
                        Some(kc) => kc,
                        None => detail,
                    }
                };
                if b.keymap_set.load(Ordering::Acquire) && self.track_key(client, detail, press) {
                    let evdev = u32::from(detail).wrapping_sub(8);
                    b.keyboard.key(self.time(), evdev, press as u32);
                    self.update_modifiers(b, detail, press);
                }
            }
            BUTTON_PRESS | BUTTON_RELEASE => {
                self.button(b, client, detail, type_ == BUTTON_PRESS)
            }
            MOTION_NOTIFY => {
                if !self.pointer_allowed(b, &mut lock(&self.state), client) {
                    return;
                }
                if detail == 0 {
                    self.motion_abs(b, x, y);
                } else {
                    self.motion_rel(b, x.into(), y.into());
                }
            }
            _ => {
                crate::warning!("unknown fake input type {type_}");
                return;
            }
        }
        (b.flush)();
    }

    fn button(&self, b: &Backend, client: u32, button: u8, press: bool) {
        // X scroll buttons (4-7) are momentary, so emit one notch on press.
        // Values follow the wl_pointer convention of down/right positive.
        let scroll = match button {
            4 => Some((Axis::VerticalScroll, -1)),   // up
            5 => Some((Axis::VerticalScroll, 1)),    // down
            6 => Some((Axis::HorizontalScroll, -1)), // left
            7 => Some((Axis::HorizontalScroll, 1)),  // right
            _ => None,
        };
        if let Some((axis, discrete)) = scroll {
            if press && self.pointer_allowed(b, &mut lock(&self.state), client) {
                let t = self.time();
                b.pointer.axis_source(AxisSource::Wheel);
                b.pointer.axis(t, axis, f64::from(discrete) * WHEEL_NOTCH);
                b.pointer
                    .axis_discrete(t, axis, f64::from(discrete) * WHEEL_NOTCH, discrete);
                b.pointer.frame();
            }
            return;
        }
        let Some(i) = BUTTON_CODES.iter().position(|&(x, _)| x == button) else {
            crate::warning!("ignoring unsupported X button {button}");
            return;
        };
        let code = BUTTON_CODES[i].1;
        let mut st = lock(&self.state);
        let st = &mut *st;
        if press {
            if !self.pointer_allowed(b, st, client) {
                return; // another client is dragging
            }
            let held = st.clients.entry(client).or_default();
            if held.buttons & (1 << i) != 0 {
                return;
            }
            held.buttons |= 1 << i;
            st.btn_count[i] = st.btn_count[i].saturating_add(1);
            if !self.policy.owner_timeout.is_zero() {
                st.owner = Some(client);
                st.owner_last = Instant::now();
            }
            if st.btn_count[i] == 1 {
                b.pointer.button(self.time(), code, ButtonState::Pressed);
                b.pointer.frame();
            }
        } else {
            // its own hold and any a departed client left (see track_key)
            let mut dropped = 0;
            if let Some(held) = st.clients.get_mut(&client)
                && held.buttons & (1 << i) != 0
            {
                held.buttons &= !(1 << i);
                if held.buttons == 0 && st.owner == Some(client) {
                    st.owner = None;
                }
                dropped += 1;
            }
            if st.orphan.buttons & (1 << i) != 0 {
                st.orphan.buttons &= !(1 << i);
                st.btn_left[i] = None;
                dropped += 1;
            }
            if dropped == 0 {
                return; // not this client's to release
            }
            st.btn_count[i] = st.btn_count[i].saturating_sub(dropped);
            if st.btn_count[i] == 0 {
                b.pointer.button(self.time(), code, ButtonState::Released);
                b.pointer.frame();
            }
        }
    }

    fn motion_abs(&self, b: &Backend, x: i16, y: i16) {
        // Map the physical X-screen point back into logical space so motion
        // lands correctly under mixed or fractional output scaling: the virtual
        // pointer resolves value/extent over the logical layout, so physical
        // coords with a physical extent would misplace the cursor.
        if let Some((lx, ly, lw, lh)) = lock(&self.layout)
            .as_ref()
            .and_then(|l| l.to_logical(x, y))
        {
            b.pointer.motion_absolute(self.time(), lx, ly, lw, lh);
            b.pointer.frame();
            return;
        }
        // no layout yet, so fall back to a proportional mapping over the screen
        let w = self.width.load(Ordering::Relaxed).max(0);
        let h = self.height.load(Ordering::Relaxed).max(0);
        let cx = i32::from(x).clamp(0, w) as u32;
        let cy = i32::from(y).clamp(0, h) as u32;
        b.pointer
            .motion_absolute(self.time(), cx, cy, w as u32, h as u32);
        b.pointer.frame();
    }

    /// Forwarded as a relative wl_pointer motion, which the backend scales
    /// against the extents from the last absolute motion.
    fn motion_rel(&self, b: &Backend, dx: i32, dy: i32) {
        b.pointer.motion(self.time(), f64::from(dx), f64::from(dy));
        b.pointer.frame();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Records what reaches the compositor, as "k30+" / "b272-" / "m" lines.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);
    impl Log {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }
    struct Ptr(Log);
    struct Kbd(Log);
    impl VirtualPointer for Ptr {
        fn motion_absolute(&self, _: u32, _: u32, _: u32, _: u32, _: u32) {
            self.0.0.lock().unwrap().push("m".into());
        }
        fn motion(&self, _: u32, _: f64, _: f64) {
            self.0.0.lock().unwrap().push("r".into());
        }
        fn button(&self, _: u32, button: u32, state: ButtonState) {
            let s = if state == ButtonState::Pressed { "+" } else { "-" };
            self.0.0.lock().unwrap().push(format!("b{button}{s}"));
        }
        fn axis(&self, _: u32, _: Axis, _: f64) {}
        fn axis_source(&self, _: AxisSource) {}
        fn axis_discrete(&self, _: u32, _: Axis, _: f64, d: i32) {
            self.0.0.lock().unwrap().push(format!("s{d}"));
        }
        fn frame(&self) {}
    }
    impl VirtualKeyboard for Kbd {
        fn keymap(&self, _: u32, _: BorrowedFd, _: u32) {}
        fn key(&self, _: u32, key: u32, state: u32) {
            let s = if state != 0 { "+" } else { "-" };
            self.0.0.lock().unwrap().push(format!("k{key}{s}"));
        }
        fn modifiers(&self, _: u32, _: u32, _: u32, _: u32) {}
    }

    fn input(policy: InputPolicy) -> (Input, Log) {
        let log = Log::default();
        let i = Input::new(100, 100, policy);
        i.set_devices(Box::new(|| {}), Box::new(Ptr(log.clone())), Box::new(Kbd(log.clone())));
        let null = std::fs::File::open("/dev/null").unwrap();
        use std::os::fd::AsFd;
        i.set_keymap(1, null.as_fd(), 0);
        (i, log)
    }

    const A: u32 = 1;
    /// How long a client was connected when it left: a VNC server, say, or
    /// a one-shot injector (see ONE_SHOT).
    const LONG: Duration = Duration::from_secs(60);
    const SHORT: Duration = Duration::from_millis(5);
    const B: u32 = 2;
    const KC: u8 = 38; // X keycode for evdev 30 (KEY_A)

    #[test]
    fn shared_key_goes_up_on_last_release() {
        let (i, log) = input(InputPolicy::default());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30-"]);
    }

    #[test]
    fn foreign_release_and_repeat_press_are_dropped() {
        let (i, log) = input(InputPolicy::default());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_PRESS, KC, 0, 0); // autorepeat
        i.fake_input(B, KEY_RELEASE, KC, 0, 0); // not B's key
        assert_eq!(log.take(), vec!["k30+"]);
    }

    // review_xproto F1: under -xtestpersist, whatever departed clients left
    // held is one orphan hold, not an entry per client, and a session ending
    // (the gnome backend's devices going) forgets it with everything else
    #[test]
    fn persisted_holds_cost_no_entries_and_end_with_the_session() {
        let (i, log) = persist();
        for c in 0..5_000 {
            i.fake_input(c, KEY_PRESS, KC, 0, 0);
            i.fake_input(c, BUTTON_PRESS, 1, 0, 0);
            i.client_gone(c, LONG);
        }
        assert_eq!(
            log.take(),
            vec!["k30+", "b272+"],
            "one press each, still down"
        );
        {
            let st = lock(&i.state);
            assert!(st.clients.is_empty());
            assert_eq!((st.key_count[KC as usize], st.btn_count[0]), (1, 1));
        }
        i.clear_devices();
        {
            let st = lock(&i.state);
            assert_eq!((st.orphan.keys, st.orphan.buttons), ([0; 4], 0));
        }
        let null = std::fs::File::open("/dev/null").unwrap();
        use std::os::fd::AsFd;
        i.set_devices(
            Box::new(|| {}),
            Box::new(Ptr(log.clone())),
            Box::new(Kbd(log.clone())),
        );
        i.set_keymap(1, null.as_fd(), 0);
        // the new devices start with nothing down: a release changes nothing
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        i.fake_input(B, BUTTON_RELEASE, 1, 0, 0);
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
    }

    #[test]
    fn disconnect_releases_held_input() {
        let (i, log) = input(InputPolicy::default());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        i.client_gone(A, LONG);
        assert_eq!(log.take(), vec!["k30-", "b272-"]);
        // and the key is free again for anyone
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
    }

    #[test]
    fn disconnect_keeps_keys_another_client_holds() {
        let (i, log) = input(InputPolicy::default());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        log.take();
        i.client_gone(A, LONG);
        assert!(log.take().is_empty());
    }

    #[test]
    fn persist_keeps_input_after_disconnect() {
        let (i, log) = input(InputPolicy {
            persist: true,
            ..InputPolicy::default()
        });
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        log.take();
        i.client_gone(A, LONG);
        assert!(log.take().is_empty());
    }

    #[test]
    fn release_client_releases_even_with_persist() {
        // the stock engines' agent stopping: nothing can release its input
        // later, so it goes up whatever -xtestpersist says
        let (i, log) = input(InputPolicy {
            persist: true,
            ..InputPolicy::default()
        });
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        i.release_client(A);
        assert_eq!(log.take(), vec!["k30-", "b272-"]);
        // its later releases change nothing; a new press goes through
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        assert!(log.take().is_empty());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
    }

    #[test]
    fn drag_owner_blocks_other_pointers_but_not_keys() {
        let (i, log) = input(InputPolicy::default());
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        i.fake_input(B, MOTION_NOTIFY, 0, 5, 5); // dropped: A is dragging
        i.fake_input(B, BUTTON_PRESS, 3, 0, 0); // dropped
        i.fake_input(B, BUTTON_PRESS, 5, 0, 0); // wheel, dropped
        i.fake_input(B, KEY_PRESS, KC, 0, 0); // keys are never arbitrated
        i.fake_input(A, MOTION_NOTIFY, 0, 9, 9);
        assert_eq!(log.take(), vec!["b272+", "k30+", "m"]);
        i.fake_input(A, BUTTON_RELEASE, 1, 0, 0);
        i.fake_input(B, MOTION_NOTIFY, 0, 5, 5); // free again
        assert_eq!(log.take(), vec!["b272-", "m"]);
    }

    #[test]
    fn stale_drag_lapses_and_is_released_first() {
        let (i, log) = input(InputPolicy {
            owner_timeout: Duration::from_millis(30),
            ..InputPolicy::default()
        });
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        std::thread::sleep(Duration::from_millis(50));
        i.fake_input(B, BUTTON_PRESS, 3, 0, 0);
        assert_eq!(log.take(), vec!["b272-", "b273+"]);
    }

    #[test]
    fn ownership_off_lets_everyone_through() {
        let (i, log) = input(InputPolicy {
            owner_timeout: Duration::ZERO,
            ..InputPolicy::default()
        });
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        i.fake_input(B, MOTION_NOTIFY, 0, 5, 5);
        assert_eq!(log.take(), vec!["b272+", "m"]);
    }

    /// An input whose devices come on demand, not yet installed, and a count
    /// of how often it asked for them.
    fn on_demand() -> (Input, Log, Arc<AtomicU64>) {
        let log = Log::default();
        let asked = Arc::new(AtomicU64::new(0));
        let i = Input::new(100, 100, InputPolicy::default());
        let a = asked.clone();
        i.set_on_demand(Box::new(move || {
            a.fetch_add(1, Ordering::Relaxed);
        }));
        (i, log, asked)
    }

    fn install(i: &Input, log: &Log) {
        i.set_devices(Box::new(|| {}), Box::new(Ptr(log.clone())), Box::new(Kbd(log.clone())));
        let null = std::fs::File::open("/dev/null").unwrap();
        use std::os::fd::AsFd;
        i.set_keymap(1, null.as_fd(), 0);
    }

    #[test]
    fn without_on_demand_devices_input_before_them_is_dropped() {
        let log = Log::default();
        let i = Input::new(100, 100, InputPolicy::default());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        install(&i, &log);
        i.replay_pending();
        assert!(log.take().is_empty());
    }

    #[test]
    fn input_before_on_demand_devices_waits_for_them() {
        // xdotool key a: press, release, gone, all before any session exists
        let (i, log, asked) = on_demand();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        i.client_gone(A, LONG);
        assert!(asked.load(Ordering::Relaxed) >= 1, "the backend was asked for devices");
        assert!(i.demanded_within(1000));
        assert!(i.has_pending());
        install(&i, &log);
        // installed but not replayed yet: newer input queues behind the old
        i.fake_input(B, MOTION_NOTIFY, 0, 5, 5);
        assert!(log.take().is_empty());
        i.replay_pending();
        assert_eq!(log.take(), vec!["k30+", "k30-", "m"]);
        assert!(!i.has_pending());
        // and from now on input goes straight through
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
    }

    #[test]
    fn a_client_gone_before_replay_is_released_after_it() {
        let (i, log, _) = on_demand();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        i.client_gone(A, LONG);
        install(&i, &log);
        i.replay_pending();
        assert_eq!(log.take(), vec!["k30+", "b272+", "k30-", "b272-"]);
    }

    #[test]
    fn dropped_pending_input_is_never_typed() {
        // the screen locked (or the user stopped sharing) while it waited
        let (i, log, asked) = on_demand();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        i.drop_pending();
        // and what comes while it stays locked is refused too, not queued
        let n = asked.load(Ordering::Relaxed);
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        assert!(!i.has_pending());
        assert!(asked.load(Ordering::Relaxed) > n, "the backend still hears of it");
        // unlocked: a session starts, its devices come
        i.accept_pending();
        install(&i, &log);
        i.replay_pending();
        assert!(log.take().is_empty());
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
    }

    #[test]
    fn input_queues_again_once_accepted() {
        let (i, log, _) = on_demand();
        i.drop_pending();
        i.accept_pending();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        assert!(i.has_pending());
        install(&i, &log);
        i.replay_pending();
        assert_eq!(log.take(), vec!["k30+"]);
    }

    #[test]
    fn a_burst_that_waited_too_long_is_dropped_whole() {
        // "abcdefgh" typed while the session was slow
        // to come must not arrive as "cdefgh"
        let (i, log, _) = on_demand();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        std::thread::sleep(PENDING_MAX_AGE - Duration::from_millis(100));
        i.fake_input(A, KEY_PRESS, KC + 1, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC + 1, 0, 0);
        // another client's input waits too, and is younger
        std::thread::sleep(Duration::from_millis(150));
        i.fake_input(B, BUTTON_PRESS, 1, 0, 0);
        i.fake_input(B, BUTTON_RELEASE, 1, 0, 0);
        install(&i, &log);
        // the first key is too old: none of A's burst is typed, B's click is
        i.replay_pending();
        assert_eq!(log.take(), vec!["b272+", "b272-"]);
        // A goes on typing without a pause: still the same burst, dropped
        i.fake_input(A, KEY_PRESS, KC + 2, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC + 2, 0, 0);
        assert_eq!(log.take(), Vec::<String>::new());
        // after a pause, A types again
        std::thread::sleep(BURST_GAP + Duration::from_millis(50));
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+", "k30-"]);
    }

    #[test]
    fn stale_pending_input_is_never_typed() {
        let (i, log, _) = on_demand();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        std::thread::sleep(PENDING_MAX_AGE + Duration::from_millis(100));
        i.fake_input(B, BUTTON_PRESS, 1, 0, 0);
        install(&i, &log);
        i.replay_pending();
        assert_eq!(log.take(), vec!["b272+"]);
    }

    #[test]
    fn pending_motion_is_coalesced_and_lone_releases_dropped() {
        let (i, log, _) = on_demand();
        i.fake_input(A, KEY_RELEASE, KC, 0, 0); // nothing to release
        assert!(!i.has_pending());
        for n in 0..1000 {
            i.fake_input(A, MOTION_NOTIFY, 0, n, n);
        }
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        install(&i, &log);
        i.replay_pending();
        assert_eq!(log.take(), vec!["m", "b272+"]);
    }

    #[test]
    fn releases_get_into_a_full_queue() {
        let (i, log, _) = on_demand();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        for n in 0..PENDING_MAX {
            i.fake_input(B, BUTTON_PRESS, 4 + (n % 2) as u8, 0, 0); // wheel notches
        }
        i.fake_input(A, KEY_RELEASE, KC, 0, 0);
        install(&i, &log);
        i.replay_pending();
        let got = log.take();
        assert_eq!(got.first().map(String::as_str), Some("k30+"));
        assert_eq!(got.last().map(String::as_str), Some("k30-"), "the key must not stay down");
    }

    #[test]
    fn queued_keys_follow_a_layout_switch_made_before_the_session() {
        let (i, log, _) = on_demand();
        // the X keyboard showed "us": z on keycode 52, y on 29
        let mut syms = vec![0u32; 256];
        (syms[52], syms[29], syms[50], syms[60]) = (0x7a, 0x79, 0xffe1, 0xe9);
        i.set_x_key_syms(Some(syms.into()));
        i.fake_input(A, KEY_PRESS, 50, 0, 0); // Shift
        i.fake_input(A, KEY_PRESS, 52, 0, 0); // z
        i.fake_input(A, KEY_RELEASE, 52, 0, 0);
        i.fake_input(A, KEY_RELEASE, 50, 0, 0);
        i.fake_input(A, KEY_PRESS, 29, 0, 0); // y, still held after the replay
        i.fake_input(B, KEY_PRESS, 60, 0, 0); // a keysym the new layout lacks
        install(&i, &log);
        // the session says the compositor is on "de": z and y swapped
        i.replay_pending_with(&|kc, sym, _| match sym {
            0x7a => Some(29),
            0x79 => Some(52),
            0 => Some(kc),
            _ if sym == 0xffe1 => Some(kc),
            _ => None,
        });
        assert_eq!(log.take(), vec!["k42+", "k21+", "k21-", "k42-", "k44+"]);
        // the held key's release goes where its press went, and the dropped
        // key's release nowhere
        i.fake_input(A, KEY_RELEASE, 29, 0, 0);
        i.fake_input(B, KEY_RELEASE, 60, 0, 0);
        assert_eq!(log.take(), vec!["k44-"]);
        // input after the replay is the client's as it is
        i.fake_input(A, KEY_PRESS, 29, 0, 0);
        assert_eq!(log.take(), vec!["k21+"]);
    }

    #[test]
    fn a_disconnect_forgets_moved_keys() {
        let (i, log, _) = on_demand();
        let mut syms = vec![0u32; 256];
        syms[52] = 0x7a;
        i.set_x_key_syms(Some(syms.into()));
        i.fake_input(A, KEY_PRESS, 52, 0, 0);
        install(&i, &log);
        i.replay_pending_with(&|_, _, _| Some(29));
        i.client_gone(A, LONG);
        assert_eq!(log.take(), vec!["k21+", "k21-"], "released where it was pressed");
        i.fake_input(B, KEY_PRESS, 52, 0, 0);
        assert_eq!(log.take(), vec!["k44+"]);
    }

    #[test]
    fn shortcut_modifiers_are_tracked_from_injected_keys() {
        let (i, _log) = input(InputPolicy::default());
        i.set_modifier_keymap(include_str!("testdata/xkbcomp_us_ru_us.xkb"));
        assert!(!i.shortcut_held());
        i.fake_input(A, KEY_PRESS, 50, 0, 0); // Shift picks a character
        assert!(!i.shortcut_held());
        i.fake_input(A, KEY_PRESS, 37, 0, 0); // Control
        assert!(i.shortcut_held());
        i.fake_input(A, KEY_RELEASE, 37, 0, 0);
        assert!(!i.shortcut_held());
        i.fake_input(A, KEY_PRESS, 64, 0, 0); // Alt
        assert!(i.shortcut_held());
        i.fake_input(A, KEY_RELEASE, 64, 0, 0);
        i.fake_input(A, KEY_PRESS, 133, 0, 0); // Super
        assert!(i.shortcut_held());
    }

    /// With no session, a Control the client
    /// pressed is still queued when its next key is decided.
    #[test]
    fn queued_shortcut_modifiers_count_for_the_next_key() {
        let (i, _log, _asked) = on_demand();
        i.set_modifier_keymap(include_str!("testdata/xkbcomp_us_ru_us.xkb"));
        assert!(!i.shortcut_held_or_queued());
        i.fake_input(A, KEY_PRESS, 50, 0, 0); // Shift, queued
        assert!(!i.shortcut_held_or_queued());
        i.fake_input(A, KEY_PRESS, 37, 0, 0); // Control, queued
        assert!(!i.shortcut_held(), "nothing injected yet");
        assert!(i.shortcut_held_or_queued());
        i.fake_input(A, KEY_RELEASE, 37, 0, 0);
        assert!(!i.shortcut_held_or_queued());
        i.fake_input(A, KEY_PRESS, 64, 0, 0); // Alt
        assert!(i.shortcut_held_or_queued());
        i.fake_input(A, KEY_RELEASE, 64, 0, 0);
        i.fake_input(A, KEY_PRESS, 133, 0, 0); // Super
        assert!(i.shortcut_held_or_queued());
    }

    #[test]
    fn view_only_drops_everything() {
        let (i, log) = input(InputPolicy {
            view_only: true,
            ..InputPolicy::default()
        });
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        assert!(log.take().is_empty());
    }

    // --- review_stability S6: a lapsed drag ends before anyone else uses the
    // pointer (it used to be released only when another client *pressed*, so
    // another client's motion dragged with the stale button down) ---

    fn short_owner() -> (Input, Log) {
        input(InputPolicy {
            owner_timeout: Duration::from_millis(30),
            ..InputPolicy::default()
        })
    }

    #[test]
    fn s6_lapsed_drag_is_released_before_another_clients_motion() {
        let (i, log) = short_owner();
        i.fake_input(A, MOTION_NOTIFY, 0, 10, 10);
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        assert_eq!(log.take(), vec!["m", "b272+"]);
        i.fake_input(B, MOTION_NOTIFY, 0, 50, 50); // still A's drag
        assert!(log.take().is_empty());
        std::thread::sleep(Duration::from_millis(50));
        i.fake_input(B, MOTION_NOTIFY, 0, 60, 60);
        i.fake_input(B, MOTION_NOTIFY, 0, 70, 70);
        assert_eq!(log.take(), vec!["b272-", "m", "m"]);
        i.fake_input(B, BUTTON_PRESS, 3, 0, 0);
        assert_eq!(log.take(), vec!["b273+"]);
    }

    #[test]
    fn s6_lapsed_drag_is_released_before_another_clients_relative_motion() {
        let (i, log) = short_owner();
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        std::thread::sleep(Duration::from_millis(50));
        i.fake_input(B, MOTION_NOTIFY, 1, 5, 5);
        assert_eq!(log.take(), vec!["b272-", "r"]);
    }

    #[test]
    fn s6_lapsed_drag_is_released_before_another_clients_wheel() {
        let (i, log) = short_owner();
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        std::thread::sleep(Duration::from_millis(50));
        i.fake_input(B, BUTTON_PRESS, 5, 0, 0);
        assert_eq!(log.take(), vec!["b272-", "s1"]);
    }

    /// Once its drag has been ended, the old owner is like any client: it no
    /// longer reclaims the pointer, and its late release changes nothing.
    #[test]
    fn s6_owner_returning_after_its_drag_ended_holds_nothing() {
        let (i, log) = short_owner();
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        std::thread::sleep(Duration::from_millis(50));
        i.fake_input(B, MOTION_NOTIFY, 0, 60, 60);
        i.fake_input(A, MOTION_NOTIFY, 0, 20, 20);
        i.fake_input(B, MOTION_NOTIFY, 0, 70, 70);
        assert_eq!(log.take(), vec!["b272-", "m", "m", "m"]);
        i.fake_input(A, BUTTON_RELEASE, 1, 0, 0);
        i.client_gone(A, LONG);
        assert!(log.take().is_empty());
    }

    /// An owner that keeps sending never lapses, however long it holds.
    #[test]
    fn s6_active_owner_keeps_the_pointer() {
        let (i, log) = input(InputPolicy {
            owner_timeout: Duration::from_millis(60),
            ..InputPolicy::default()
        });
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        for k in 0..4 {
            std::thread::sleep(Duration::from_millis(25));
            i.fake_input(A, MOTION_NOTIFY, 0, 10 + k, 10);
            i.fake_input(B, MOTION_NOTIFY, 0, 60, 60); // dropped every time
        }
        assert_eq!(log.take(), vec!["b272+", "m", "m", "m", "m"]);
    }

    /// A button another client also holds stays down when the lapsed owner's
    /// drag ends.
    #[test]
    fn s6_lapse_keeps_a_button_another_client_holds() {
        let (i, log) = input(InputPolicy {
            owner_timeout: Duration::from_millis(30),
            persist: true,
            ..InputPolicy::default()
        });
        i.fake_input(B, BUTTON_PRESS, 1, 0, 0);
        i.client_gone(B, LONG); // persist: its button 1 is an orphan now
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0); // A holds it too
        log.take();
        std::thread::sleep(Duration::from_millis(50));
        i.fake_input(3, MOTION_NOTIFY, 0, 1, 1);
        assert_eq!(log.take(), vec!["m"]); // still down for the orphan
    }

    // --- review_stability S7: -xtestpersist kept every departed client's
    // entry (ids are never reused), and a departed client's key could not be
    // released by anyone ---

    /// One short-lived client per action (xdotool-style: connect, act,
    /// disconnect), ids never reused, as conn.rs hands them out. Returns how
    /// many per-client entries are left.
    fn entries_after(policy: InputPolicy, n: u32, kind: &str) -> usize {
        let (i, _log) = input(policy);
        for c in 100..100 + n {
            match kind {
                "key" => {
                    i.fake_input(c, KEY_PRESS, KC, 0, 0);
                    i.fake_input(c, KEY_RELEASE, KC, 0, 0);
                }
                "keydown" => i.fake_input(c, KEY_PRESS, KC, 0, 0),
                "click" => {
                    i.fake_input(c, BUTTON_PRESS, 1, 0, 0);
                    i.fake_input(c, BUTTON_RELEASE, 1, 0, 0);
                }
                "buttondown" => i.fake_input(c, BUTTON_PRESS, 1, 0, 0),
                "stray_release" => i.fake_input(c, KEY_RELEASE, KC, 0, 0),
                _ => unreachable!(),
            }
            i.client_gone(c, LONG);
        }
        lock(&i.state).clients.len()
    }

    #[test]
    fn s7_departed_clients_leave_no_entries() {
        for persist in [false, true] {
            let policy = InputPolicy {
                persist,
                ..InputPolicy::default()
            };
            for kind in ["key", "keydown", "click", "buttondown", "stray_release"] {
                assert_eq!(entries_after(policy, 2000, kind), 0, "persist={persist} {kind}");
            }
        }
    }

    fn persist() -> (Input, Log) {
        input(InputPolicy {
            persist: true,
            ..InputPolicy::default()
        })
    }

    /// `xdotool keydown ctrl; xdotool keyup ctrl` as two processes.
    #[test]
    fn s7_persist_lets_another_client_release_a_departed_ones_key() {
        let (i, log) = persist();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, LONG);
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        i.client_gone(B, LONG);
        assert_eq!(log.take(), vec!["k30+", "k30-"]);
        i.fake_input(3, KEY_PRESS, KC, 0, 0); // free again
        assert_eq!(log.take(), vec!["k30+"]);
    }

    /// `xdotool keydown ctrl; xdotool key ctrl`: the tap ends the hold, as in
    /// Xorg.
    #[test]
    fn s7_persist_a_tap_ends_a_departed_clients_hold() {
        let (i, log) = persist();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, LONG);
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30+", "k30-"]);
    }

    #[test]
    fn s7_persist_orphan_release_waits_for_live_holders() {
        let (i, log) = persist();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, LONG);
        i.fake_input(B, KEY_PRESS, KC, 0, 0); // B holds it too
        i.fake_input(3, KEY_RELEASE, KC, 0, 0); // ends only the orphan's hold
        assert_eq!(log.take(), vec!["k30+"]);
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30-"]);
    }

    #[test]
    fn s7_persist_two_departed_holders_of_one_key_merge() {
        let (i, log) = persist();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(B, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, LONG);
        i.client_gone(B, LONG);
        i.fake_input(3, KEY_RELEASE, KC, 0, 0); // one release frees it
        assert_eq!(log.take(), vec!["k30+", "k30-"]);
    }

    #[test]
    fn s7_persist_lets_another_client_release_a_departed_ones_button() {
        let (i, log) = persist();
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        i.client_gone(A, LONG);
        i.fake_input(B, MOTION_NOTIFY, 0, 5, 5); // no owner any more
        i.fake_input(B, BUTTON_RELEASE, 1, 0, 0);
        assert_eq!(log.take(), vec!["b272+", "m", "b272-"]);
    }

    #[test]
    fn s7_without_persist_a_foreign_release_is_still_dropped() {
        let (i, log) = input(InputPolicy::default());
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        i.fake_input(B, BUTTON_RELEASE, 1, 0, 0);
        assert_eq!(log.take(), vec!["k30+"]);
    }

    // --- vb_held_keys (remote_access B1): what a one-shot client leaves held
    // lingers for the next client, and nothing stays stuck ---

    const SHIFT: u8 = 50; // X keycode of Shift_L (evdev 42)
    const EIGHT: u8 = 17; // X keycode of the 8 key (evdev 9)

    /// The default policy with the linger thread's part played by the test
    /// (it calls `expire_lingering` with the time it wants).
    fn lingering() -> (Input, Log) {
        let (i, log) = input(InputPolicy::default());
        i.linger_running.store(true, Ordering::Release);
        (i, log)
    }

    fn after(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    /// `xdotool keydown Shift_L; xdotool key 8; xdotool keyup Shift_L`, or
    /// RustDesk's connection per key event: Shift stays down for the 8 and
    /// goes up with the third client's release.
    #[test]
    fn a_one_shot_clients_shift_holds_for_the_next_clients_key() {
        let (i, log) = lingering();
        i.fake_input(A, KEY_PRESS, SHIFT, 0, 0);
        i.client_gone(A, SHORT);
        assert_eq!(log.take(), vec!["k42+"], "Shift stays down");
        i.fake_input(B, KEY_PRESS, EIGHT, 0, 0);
        i.fake_input(B, KEY_RELEASE, EIGHT, 0, 0);
        i.client_gone(B, SHORT);
        i.expire_lingering(Instant::now());
        assert_eq!(log.take(), vec!["k9+", "k9-"]);
        i.fake_input(3, KEY_RELEASE, SHIFT, 0, 0);
        assert_eq!(log.take(), vec!["k42-"]);
        i.client_gone(3, SHORT);
        i.expire_lingering(after(60_000));
        assert!(log.take().is_empty());
        let st = lock(&i.state);
        assert!(st.clients.is_empty() && st.key_left.iter().all(Option::is_none));
    }

    /// Nothing stays stuck: what no client releases goes up once X clients
    /// have injected nothing for the linger time (1 s by default).
    #[test]
    fn a_one_shot_hold_nobody_releases_goes_up_when_input_pauses() {
        let (i, log) = lingering();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(B, BUTTON_PRESS, 1, 0, 0);
        i.client_gone(A, SHORT);
        i.client_gone(B, SHORT);
        assert_eq!(log.take(), vec!["k30+", "b272+"]);
        i.expire_lingering(after(500));
        assert!(log.take().is_empty());
        let due = i.next_due(&lock(&i.state)).expect("lingering");
        assert!(due > after(800) && due <= after(1000), "due within 1 s");
        i.expire_lingering(after(1001));
        assert_eq!(log.take(), vec!["k30-", "b272-"]);
        assert!(i.next_due(&lock(&i.state)).is_none());
    }

    /// Input from any client keeps a lingering hold down (Ctrl held in RustDesk
    /// while clicking), but never past LINGER_CAP after its client left.
    #[test]
    fn input_keeps_a_lingering_hold_down_up_to_the_cap() {
        let (i, log) = lingering();
        i.fake_input(A, KEY_PRESS, SHIFT, 0, 0);
        i.client_gone(A, SHORT);
        log.take();
        let left = Instant::now();
        // other clients' input every 500 ms for 12 s (simulated)
        for step in 1..=24u64 {
            let t = left + Duration::from_millis(step * 500);
            i.last_input_ms.store(
                t.duration_since(i.start).as_millis() as u64 + 1,
                Ordering::Relaxed,
            );
            i.expire_lingering(t);
            let got = log.take();
            if t < left + LINGER_CAP {
                assert!(got.is_empty(), "step {step}");
            } else {
                assert_eq!(got, vec!["k42-"], "step {step}");
                return;
            }
        }
        panic!("never released");
    }

    /// A client connected longer than ONE_SHOT (a VNC server, say) that goes
    /// away holding something has it released at once, as before, even when
    /// its press came just before it died.
    #[test]
    fn a_long_lived_clients_holds_go_up_at_once() {
        let (i, log) = lingering();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.fake_input(A, BUTTON_PRESS, 1, 0, 0);
        log.take();
        i.client_gone(A, ONE_SHOT);
        assert_eq!(log.take(), vec!["k30-", "b272-"]);
    }

    /// A lingering key another client presses too goes up with its last
    /// holder; a one-shot client leaving the same key again renews it.
    #[test]
    fn lingering_holds_count_as_holders() {
        let (i, log) = lingering();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, SHORT);
        i.fake_input(B, KEY_PRESS, KC, 0, 0); // B holds it too
        assert_eq!(log.take(), vec!["k30+"]);
        i.expire_lingering(after(1001));
        assert!(log.take().is_empty(), "B still holds it");
        i.fake_input(B, KEY_RELEASE, KC, 0, 0);
        assert_eq!(log.take(), vec!["k30-"]);
        // two one-shot clients leave the same key: one hold, renewed
        i.fake_input(3, KEY_PRESS, KC, 0, 0);
        i.client_gone(3, SHORT);
        i.fake_input(4, KEY_PRESS, KC, 0, 0);
        i.client_gone(4, SHORT);
        assert_eq!(log.take(), vec!["k30+"]);
        assert_eq!(lock(&i.state).key_count[KC as usize], 1);
        i.expire_lingering(after(1001));
        assert_eq!(log.take(), vec!["k30-"]);
        assert_eq!(lock(&i.state).key_count[KC as usize], 0);
    }

    /// `-xtestgrace 0`, or no linger thread: released at once (the old
    /// default); `-xtestpersist`: never released by time.
    #[test]
    fn zero_grace_releases_at_once_and_persist_never_expires() {
        let (i, log) = input(InputPolicy {
            linger: Duration::ZERO,
            ..InputPolicy::default()
        });
        i.linger_running.store(true, Ordering::Release);
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, SHORT);
        assert_eq!(log.take(), vec!["k30+", "k30-"]);
        let (i, log) = input(InputPolicy::default()); // no thread running
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, SHORT);
        assert_eq!(log.take(), vec!["k30+", "k30-"]);
        let (i, log) = persist();
        i.fake_input(A, KEY_PRESS, KC, 0, 0);
        i.client_gone(A, SHORT);
        i.expire_lingering(after(3_600_000));
        assert_eq!(log.take(), vec!["k30+"]);
        assert!(i.next_due(&lock(&i.state)).is_none());
    }

    /// The thread: a lingering hold goes up by itself, about when it is due.
    #[test]
    fn the_linger_thread_releases_what_is_due() {
        let log = Log::default();
        let i = Arc::new(Input::new(
            100,
            100,
            InputPolicy {
                linger: Duration::from_millis(100),
                ..InputPolicy::default()
            },
        ));
        i.set_devices(Box::new(|| {}), Box::new(Ptr(log.clone())), Box::new(Kbd(log.clone())));
        let null = std::fs::File::open("/dev/null").unwrap();
        use std::os::fd::AsFd;
        i.set_keymap(1, null.as_fd(), 0);
        i.spawn_linger();
        let t = Instant::now();
        i.fake_input(A, KEY_PRESS, SHIFT, 0, 0);
        i.client_gone(A, SHORT);
        assert_eq!(log.take(), vec!["k42+"]);
        while log.0.lock().unwrap().is_empty() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let took = t.elapsed();
        assert_eq!(log.take(), vec!["k42-"]);
        assert!(took >= Duration::from_millis(100) && took < Duration::from_millis(1000), "{took:?}");
    }
}
