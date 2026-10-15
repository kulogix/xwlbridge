//! Who holds which key or button down, per X client, from the XTest requests
//! themselves (RECORD extension), so that whatever a client leaves held when it
//! goes away can be released — and only that: a key another live client still
//! holds stays down, as with the built-in engine's per-client accounting.
//!
//! Xorg merges every client's XTest into one device and keeps it pressed when
//! the client that pressed it disconnects. A client can come and go in a few
//! milliseconds (`xdotool keydown a` does), far faster than anything could
//! poll for, so this listens on RECORD's own connection, which reports each
//! request with the client that made it and each client's death, in order.
//!
//! What a departed client held goes as the built-in engine's would (see
//! crate::bridge::input): released at once, unless the client was a one-shot
//! injector ([`ONE_SHOT`]: `xdotool keydown shift` then `xdotool key 8`, or
//! RustDesk's connection per key), whose holds linger for a later client to
//! use and release ([`linger_due`]), or `-xtestpersist` keeps them all until
//! some client releases them. The agent releases what is due
//! ([`Holds::expire`]), as XTest input of its own.
//!
//! The same context reports which clients read the root window's pixels
//! (`GetImage`, `ShmGetImage`): a client that polls the screen without a
//! DAMAGE object (xwd, import, scrot, ffmpeg's x11grab, x11vnc -noxdamage) is
//! watching it too. (`CopyArea` from the root would count as well, but every
//! toolkit draws with it, and nothing that captures the screen uses it.)
//!
//! And it says whose each injected event is (see [`attrib`](super::attrib)):
//! every FakeInput goes into the agent's ledger with its client's class, and
//! the agent's own NoOperation requests come back as markers, so the agent
//! knows when everything before one has arrived.
//!
//! And who connects: a client connecting while capture is stopped starts it
//! at once (`-coldconnect start`), so one that reads a moment later gets the
//! desktop; and who leaves after reading the screen, which keeps capture up a
//! while (`-linger`) for the next one-shot screenshot.
//!
//! And whose pointer grabs confine the pointer to a window (core
//! `GrabPointer`/`UngrabPointer`): the -showwindows thread locks the desktop's
//! pointer for relative motion while the cursor is hidden and one is held
//! (SDL2's relative mouse mode; GTK 4's XI2 grabs have no confine-to and do
//! not count), see [`confining_grab`].

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::record::{self, ConnectionExt as _, ExtRange, Range, Range8, Range16};
use x11rb::rust_connection::RustConnection;

use super::attrib::{self, Class, Fake, Ledger, Rules, Who};
use super::motion::{Motion, MotionLog};
use crate::bridge::input::{
    BUTTON_PRESS, BUTTON_RELEASE, InputPolicy, KEY_PRESS, KEY_RELEASE, MOTION_NOTIFY, ONE_SHOT,
    linger_due,
};

/// XTest's FakeInput minor opcode.
const FAKE_INPUT: u8 = 2;
/// The core request that reads a drawable's pixels.
const GET_IMAGE: u8 = 73;
/// MIT-SHM's ShmGetImage minor opcode.
const SHM_GET_IMAGE: u8 = 4;
/// The core NoOperation request: the agent's marker (see `attrib`).
const NO_OPERATION: u8 = 127;
/// The core requests that take (`GrabPointer`) and end (`UngrabPointer`) a
/// client's pointer grab.
const GRAB_POINTER: u8 = 26;
const UNGRAB_POINTER: u8 = 27;

/// RECORD's reply categories (the protocol numbers them; x11rb has no names).
const FROM_CLIENT: u8 = 1;
const CLIENT_STARTED: u8 = 2;
const CLIENT_DIED: u8 = 3;

/// An input event to inject to undo what a departed client left held:
/// (`KEY_RELEASE` or `BUTTON_RELEASE`, keycode or button).
pub type Release = (u8, u8);

#[derive(Default)]
struct Held {
    keys: BTreeSet<u8>,
    buttons: BTreeSet<u8>,
}

/// Who holds what: live clients, and what departed ones left down. Shared by
/// the RECORD thread, which follows the clients, and the agent, which
/// releases what is due.
pub struct Holds {
    /// Clients that have pressed something, until they die (resource bases
    /// are reused only after that).
    clients: HashMap<u32, Held>,
    /// When each live client connected (clients already there when RECORD
    /// started have none, and count as long-lived).
    started: HashMap<u32, Instant>,
    /// What departed clients left down that nobody has released yet (a
    /// [`Release`] undoes it), and when its last holder left. One entry per
    /// key or button, however many clients come and go.
    orphans: HashMap<Release, Instant>,
    /// When a client other than the agent last sent XTest input.
    active: Instant,
    persist: bool,
    linger: Duration,
    /// The agent's resource base: its own releases are no client's input.
    agent: u32,
}

impl Holds {
    pub fn new(policy: &InputPolicy, agent: u32) -> Holds {
        Holds {
            clients: HashMap::new(),
            started: HashMap::new(),
            orphans: HashMap::new(),
            active: Instant::now(),
            persist: policy.persist,
            linger: policy.linger,
            agent,
        }
    }

    /// A client with resource base `client` connected at `now`.
    fn started(&mut self, client: u32, now: Instant) {
        self.started.insert(client, now);
    }

    /// A FakeInput request from the client with resource base `client`, seen
    /// at `now`.
    fn input(&mut self, client: u32, type_: u8, detail: u8, now: Instant) {
        if client != self.agent {
            self.active = now;
        }
        match type_ {
            KEY_PRESS | BUTTON_PRESS => {
                let held = self.clients.entry(client).or_default();
                if type_ == KEY_PRESS {
                    held.keys.insert(detail);
                } else {
                    held.buttons.insert(detail);
                }
            }
            // X has one state per key: a release from anyone releases it, so
            // nobody holds it any more
            KEY_RELEASE => {
                self.clients.values_mut().for_each(|h| {
                    h.keys.remove(&detail);
                });
                self.orphans.remove(&(KEY_RELEASE, detail));
            }
            BUTTON_RELEASE => {
                self.clients.values_mut().for_each(|h| {
                    h.buttons.remove(&detail);
                });
                self.orphans.remove(&(BUTTON_RELEASE, detail));
            }
            _ => {}
        }
    }

    /// Whether a live client holds what `r` releases.
    fn live(&self, (type_, detail): Release) -> bool {
        self.clients.values().any(|h| {
            if type_ == KEY_RELEASE {
                h.keys.contains(&detail)
            } else {
                h.buttons.contains(&detail)
            }
        })
    }

    /// The client went away at `now`: `None` if it held nothing, else what
    /// to release at once (possibly nothing). Its holds become orphans if they
    /// linger or persist; otherwise what nobody else holds goes up (another
    /// client, or an orphan, holding a key keeps it down: X keeps one state
    /// per key, which goes up with its last holder).
    fn died(&mut self, client: u32, now: Instant) -> Option<Vec<Release>> {
        let started = self.started.remove(&client);
        let held = self.clients.remove(&client)?;
        if held.keys.is_empty() && held.buttons.is_empty() {
            return None;
        }
        let lived = started.map(|t| now.saturating_duration_since(t));
        let one_shot = lived.is_some_and(|l| l < ONE_SHOT);
        let keep = self.persist || (!self.linger.is_zero() && one_shot);
        if keep && !self.persist {
            crate::vlog!(
                "xvfb: one-shot X client {client:#x} left after {} ms: its {} key(s)/button(s) stay down \
                 for another client to release",
                lived.unwrap_or_default().as_millis(),
                held.keys.len() + held.buttons.len()
            );
        }
        let mut out = Vec::new();
        let keys = held.keys.into_iter().map(|k| (KEY_RELEASE, k));
        let buttons = held.buttons.into_iter().map(|b| (BUTTON_RELEASE, b));
        for r in keys.chain(buttons) {
            if keep {
                // a lingering hold another one-shot client left is renewed
                self.orphans.insert(r, now);
            } else if !self.orphans.contains_key(&r) && !self.live(r) {
                out.push(r);
            }
        }
        Some(out)
    }

    /// When a departed client's hold left at `left` is due to go up.
    fn due(&self, left: Instant) -> Option<Instant> {
        (!self.persist).then(|| linger_due(left, self.active, self.linger))
    }

    /// Ends the lingering holds due by `now`, returning those to release:
    /// the ones no live client holds too.
    pub fn expire(&mut self, now: Instant) -> Vec<Release> {
        let due: Vec<Release> = self
            .orphans
            .iter()
            .filter(|&(_, &left)| self.due(left).is_some_and(|d| now >= d))
            .map(|(&r, _)| r)
            .collect();
        for r in &due {
            self.orphans.remove(r);
        }
        let mut out: Vec<Release> = due.into_iter().filter(|&r| !self.live(r)).collect();
        out.sort_unstable();
        out
    }

    /// When the next lingering hold is due to go up, if any lingers.
    pub fn next_due(&self) -> Option<Instant> {
        self.orphans
            .values()
            .filter_map(|&left| self.due(left))
            .min()
    }

    /// How many departed clients' holds are down (lingering or persisting).
    #[cfg(test)]
    fn orphans(&self) -> usize {
        self.orphans.len()
    }
}

/// Locks the shared [`Holds`] (the state is plain sets: a panic elsewhere
/// leaves nothing half-done that matters).
pub fn lock(holds: &Mutex<Holds>) -> std::sync::MutexGuard<'_, Holds> {
    holds.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Which clients hold a core pointer grab that confines the pointer to a
/// window (its confine-to window), from the clients' `GrabPointer` and
/// `UngrabPointer` requests, one entry per client. Shared with the
/// -showwindows thread, which locks the desktop's pointer for relative motion
/// while the cursor is hidden and one is held (SDL2's relative mouse mode): a
/// grab without confine-to (GTK 4's XI2 grabs, a pop-up grab) does not count,
/// and with no RECORD thread none is ever seen, so no grab triggers the lock.
/// One RECORD thread at a time runs, and one starts and ends with the map
/// cleared (grabs seen before it started, or after it stopped, are none of
/// its business).
static CONFINES: LazyLock<Mutex<HashMap<u32, u32>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn confines() -> std::sync::MutexGuard<'static, HashMap<u32, u32>> {
    CONFINES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether some client holds a pointer grab that confines the pointer to a
/// window: the -showwindows relative-mode trigger, together with a hidden
/// cursor.
pub fn confining_grab() -> bool {
    !confines().is_empty()
}

/// Forgets every client's grab, at the start and end of a RECORD thread's
/// run (see [`CONFINES`]).
pub fn clear_grabs() {
    confines().clear();
}

/// A client's `GrabPointer` request: it now holds a grab that confines the
/// pointer to `confine` — or holds none, when `confine` is 0 (None): a grab
/// without confine-to (a pop-up grab), or a grab that failed (RECORD sees
/// the request, not its reply), replaces a held one either way.
fn grab_seen(confines: &mut HashMap<u32, u32>, client: u32, confine: u32) {
    if confine != 0 {
        confines.insert(client, confine);
    } else {
        confines.remove(&client);
    }
}

/// The client's `UngrabPointer` request, or the client itself (a grab ends
/// when its client does): it holds no confining grab.
fn grab_gone(confines: &mut HashMap<u32, u32>, client: u32) {
    confines.remove(&client);
}

/// Where root reads are reported: milliseconds on [`super::now_ms`]'s clock
/// of the last one by a client other than the agent (0: none yet).
pub struct RootReads {
    pub root: u32,
    pub agent_base: u32,
    pub last_ms: Arc<AtomicU64>,
    /// For starting capture when a client connects, and keeping it up after
    /// a one-shot client left.
    pub framebuffer: Arc<crate::bridge::capture::Framebuffer>,
    /// `-coldconnect start`.
    pub prefetch: bool,
    /// Set while a client of our own (xkbcomp) runs: its connection starts
    /// nothing.
    pub ours_running: &'static AtomicBool,
}

/// What the RECORD thread needs to say whose input is whose.
pub struct Attrib {
    pub ledger: Arc<Ledger>,
    pub rules: Rules,
    /// The agent's own connection (its releases, and its markers).
    pub agent_base: u32,
    /// A connection of the thread's own for X-Resource lookups (RECORD's is
    /// busy), connected like the agent's.
    pub lookup: Option<RustConnection>,
}

/// Classifies clients by their program, once each.
struct Classifier {
    conn: Option<RustConnection>,
    rules: Rules,
    agent_base: u32,
    known: HashMap<u32, Arc<Who>>,
}

impl Classifier {
    fn new(a: Attrib) -> Classifier {
        let conn = a.lookup.filter(|c| {
            x11rb::protocol::res::query_version(c, 1, 2)
                .ok()
                .and_then(|r| r.reply().ok())
                .is_some()
        });
        if conn.is_none() {
            crate::warning!(
                "xvfb: can't look up X clients' programs (X-Resource 1.2); no client counts as an engine helper or a desktop driver"
            );
        }
        Classifier {
            conn,
            rules: a.rules,
            agent_base: a.agent_base,
            known: HashMap::new(),
        }
    }

    fn who(&mut self, base: u32) -> Arc<Who> {
        if let Some(w) = self.known.get(&base) {
            return w.clone();
        }
        let who = if base == self.agent_base {
            Who {
                base,
                class: Class::Agent,
                name: "xwlbridge".into(),
                program: String::new(),
                pid: None,
            }
        } else {
            let pid = self.conn.as_ref().and_then(|c| attrib::client_pid(c, base));
            let names = pid
                .map(crate::bridge::injectors::process_names)
                .unwrap_or_default();
            let class = self.rules.classify(&names);
            let name = match (names.first(), pid) {
                (Some(n), Some(p)) => format!("{n}, pid {p}"),
                _ => "?".into(),
            };
            crate::vlog!("xvfb: X client {base:#x} is {name}: {class:?}");
            // all its names: an interpreter's script tells it from other
            // scripts of the same interpreter
            let program = names.join("/");
            Who {
                base,
                class,
                name,
                program,
                pid,
            }
        };
        let who = Arc::new(who);
        self.known.insert(base, who.clone());
        who
    }
}

/// Runs on its own thread, on its own connection `conn`, for the life of the
/// agent: who holds what goes to `holds`, what a departed client's leaving
/// releases at once to `tx`, root reads to `reads`, each XTest pointer move to
/// `motions` (see motion.rs), and `wake` is called after a client that held
/// something left (to release it, or to wait for its lingering holds), and on
/// a root read after a pause. Ends quietly once `stop` is set and the
/// connection is shut down (the agent stopping), or once nobody receives on
/// `tx` any more. Each injected event's request goes into `attrib`'s ledger.
// (one more argument than clippy likes since held_keys' shared `holds` met
// `attrib`'s input attribution: each is a different party's state)
#[allow(clippy::too_many_arguments)]
pub fn run(
    conn: RustConnection,
    holds: &Mutex<Holds>,
    tx: Sender<Vec<Release>>,
    reads: RootReads,
    motions: &MotionLog,
    wake: impl Fn(),
    stop: &AtomicBool,
    attrib: Attrib,
) -> Result<(), super::AnyError> {
    // what a RECORD thread of ours saw before, or a dead one left behind
    clear_grabs();
    let xtest = conn
        .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
        .ok_or("no XTEST")?
        .major_opcode;
    // without MIT-SHM (Xvfb -noshm) there are no ShmGetImage requests to see
    let shm = conn
        .extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME)?
        .map(|e| e.major_opcode);
    conn.extension_information(record::X11_EXTENSION_NAME)?
        .ok_or("no RECORD extension")?;
    record::query_version(&conn, 1, 13)?.reply()?;
    let none8 = Range8 { first: 0, last: 0 };
    let none_ext = ExtRange {
        major: none8,
        minor: Range16 { first: 0, last: 0 },
    };
    let range = Range {
        // the agent's markers
        core_requests: Range8 {
            first: NO_OPERATION,
            last: NO_OPERATION,
        },
        core_replies: none8,
        ext_requests: ExtRange {
            major: Range8 {
                first: xtest,
                last: xtest,
            },
            minor: Range16 {
                first: FAKE_INPUT.into(),
                last: FAKE_INPUT.into(),
            },
        },
        ext_replies: none_ext,
        delivered_events: none8,
        device_events: none8,
        errors: none8,
        // classify each client while it is surely still there, and start
        // capture for one that connects while it is stopped (-coldconnect)
        client_started: true,
        client_died: true,
    };
    let reads_of = |core: Range8, ext: Option<u8>, minor: u8| Range {
        core_requests: core,
        ext_requests: match ext {
            Some(major) => ExtRange {
                major: Range8 {
                    first: major,
                    last: major,
                },
                minor: Range16 {
                    first: minor.into(),
                    last: minor.into(),
                },
            },
            None => none_ext,
        },
        client_started: false,
        client_died: false,
        ..range
    };
    // whose pointer grabs confine (SDL2's relative mode): see `grab_confine`
    let grabs = Range {
        core_requests: Range8 {
            first: GRAB_POINTER,
            last: UNGRAB_POINTER,
        },
        ext_requests: none_ext,
        client_started: false,
        client_died: false,
        ..range
    };
    // The ranges with core requests go first: Xorg (record.c,
    // RecordConvertRangesToIntervals) sizes the one interval list core and
    // extension requests share as twice the ranges from the first one with a
    // core request on, and asserts (the server aborts) when the extension
    // requests then overflow it, which XTest's range ahead of this one did.
    // (All three ranges have core requests now: three core and two extension
    // intervals, in a list sized for six.)
    let ranges = [
        reads_of(
            Range8 {
                first: GET_IMAGE,
                last: GET_IMAGE,
            },
            shm,
            SHM_GET_IMAGE,
        ),
        grabs,
        range,
    ];
    let ctx = conn.generate_id()?;
    // each request comes with the server time it was handled at
    let header = record::HType::FROM_CLIENT_TIME;
    conn.record_create_context(
        ctx,
        header.into(),
        &[record::CS::ALL_CLIENTS.into()],
        &ranges,
    )?
    .check()?;
    let agent_base = attrib.agent_base;
    let ledger = attrib.ledger.clone();
    let mut classifier = Classifier::new(attrib);
    // clients that read the root, and that injected input, until they go
    let mut active: std::collections::HashSet<u32> = Default::default();
    let mut injected: std::collections::HashSet<u32> = Default::default();
    // clients whose connection started capture (-coldconnect start), with
    // their program; and the programs whose last such connection did nothing
    let mut prefetched: HashMap<u32, (String, Option<u32>)> = HashMap::new();
    let mut idle = IdlePrograms::default();
    for reply in conn.record_enable_context(ctx)? {
        let r = match reply {
            Ok(r) => r,
            Err(_) if stop.load(Ordering::Acquire) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match r.category {
            // -showwindows: the desktop's input on shown windows, on its own master device
            FROM_CLIENT if super::reverse::is_reverse_client(r.xid_base) => {}
            FROM_CLIENT => {
                // the client's own byte order; RECORD says whether it is not ours
                let little = cfg!(target_endian = "little") != r.client_swapped;
                let timed = r.element_header & u8::from(record::HType::FROM_CLIENT_TIME) != 0;
                let mut read_root = false;
                let now = Instant::now();
                for_each_request(&r.data, little, timed, |op, minor, body, time| {
                    if op == xtest
                        && minor == FAKE_INPUT
                        && body.len() >= 2
                        && fake_delay(body, little) != 0
                    {
                        // a delayed FakeInput: the server sleeps for `time` ms
                        // and then runs the request again with no delay, which
                        // RECORD reports again, in its real place among the
                        // other clients' input; this first sight is no input
                        // yet
                    } else if op == xtest && minor == FAKE_INPUT && body.len() >= 2 {
                        if injected.insert(r.xid_base)
                            && let Some(who) = classifier.known.get(&r.xid_base)
                        {
                            idle.used(&who.program, who.pid);
                        }
                        lock(holds).input(r.xid_base, body[0], body[1], now);
                        if let Some(m) = fake_motion(body, little) {
                            motions.push(m);
                        }
                        let i16_at = |b: &[u8]| {
                            (if little {
                                u16::from_le_bytes([b[0], b[1]])
                            } else {
                                u16::from_be_bytes([b[0], b[1]])
                            }) as i16
                        };
                        let (x, y) = if body.len() >= 24 {
                            (i16_at(&body[20..]), i16_at(&body[22..]))
                        } else {
                            (0, 0)
                        };
                        let who = classifier.who(r.xid_base);
                        ledger.push(Fake {
                            who,
                            type_: body[0],
                            detail: body[1],
                            x,
                            y,
                            time,
                        });
                    } else if op == NO_OPERATION {
                        if r.xid_base == agent_base {
                            ledger.marker();
                        }
                    } else if op == UNGRAB_POINTER {
                        grab_gone(&mut confines(), r.xid_base);
                    } else if let Some(confine) = grab_confine(op, body, little) {
                        grab_seen(&mut confines(), r.xid_base, confine);
                    } else if r.xid_base != reads.agent_base
                        && reads_pixels(op, minor, shm)
                        && body.len() >= 4
                        && u32_at(&body[..4], little) == reads.root
                    {
                        read_root = true;
                    }
                });
                if read_root {
                    if active.insert(r.xid_base)
                        && let Some(who) = classifier.known.get(&r.xid_base)
                    {
                        idle.used(&who.program, who.pid);
                    }
                    let now = super::now_ms();
                    let before = reads.last_ms.swap(now, Ordering::Relaxed);
                    // tell the agent the moment a client starts reading
                    if now.saturating_sub(before) >= ROOT_READ_WAKE_MS {
                        wake();
                    }
                }
            }
            CLIENT_STARTED => {
                lock(holds).started(r.xid_base, Instant::now());
                // look it up while it is surely still there: `xdotool click 1`
                // is gone a few milliseconds after it connects
                let who = classifier.who(r.xid_base);
                if reads.prefetch
                    && r.xid_base != reads.agent_base
                    && !reads.ours_running.load(Ordering::Acquire)
                    && reads.framebuffer.fresh.would_wait()
                {
                    if who.class == Class::Helper {
                        // its input never reaches the desktop, and it isn't
                        // reading yet: nothing to start the session for
                        crate::vlog!(
                            "xvfb: engine helper {:#x} ({}) connected; not starting capture for it",
                            r.xid_base,
                            who.name
                        );
                    } else if idle.idle(&who.program, std::time::Instant::now()) {
                        crate::vlog!(
                            "xvfb: client {:#x} ({}) connected while capture is stopped; not starting it: \
                             that program started it {} times within {} s and neither read the screen nor injected input",
                            r.xid_base,
                            who.name,
                            IdlePrograms::IDLE_AFTER,
                            IdlePrograms::WINDOW.as_secs()
                        );
                    } else {
                        crate::vlog!(
                            "xvfb: client {:#x} connected while capture is stopped; starting it",
                            r.xid_base
                        );
                        reads.framebuffer.prefetch();
                        prefetched.insert(r.xid_base, (who.program.clone(), who.pid));
                    }
                }
            }
            CLIENT_DIED => {
                // a connection that started the session for nothing (a
                // poller such as xprop or xclip in a loop): see IdlePrograms
                // (seen in testing: the sharing indicator flapped for a
                // client that only polled)
                if let Some((program, pid)) = prefetched.remove(&r.xid_base)
                    && !active.contains(&r.xid_base)
                    && !injected.contains(&r.xid_base)
                {
                    idle.unused(program, pid, std::time::Instant::now());
                }
                injected.remove(&r.xid_base);
                if active.remove(&r.xid_base) && r.xid_base != reads.agent_base {
                    reads.framebuffer.note_oneshot();
                }
                classifier.known.remove(&r.xid_base);
                // its grab went with it (see `grab_gone`)
                grab_gone(&mut confines(), r.xid_base);
                // the reverse thread's connection is gone: its X slot (and so
                // its resource base) goes to the next client, whose input must
                // count again (review F1)
                super::reverse::forget_client(r.xid_base);
                let died = lock(holds).died(r.xid_base, Instant::now());
                if let Some(released) = died {
                    if !released.is_empty() && tx.send(released).is_err() {
                        return Ok(());
                    }
                    wake();
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Programs that start the session by connecting (`-coldconnect start`) for
/// nothing, again and again: once [`IDLE_AFTER`](Self::IDLE_AFTER) of a
/// program's processes within [`WINDOW`](Self::WINDOW) had a connection start
/// it and none of their connections read the screen or injected input (a
/// poller such as xprop, or xclip, in a loop), that program's connections stop
/// starting it; each of its connections meanwhile keeps it so, and one that
/// reads or injects ends it. A program that runs now and then (Wine's
/// explorer.exe, whose connection comes before the one that reads) keeps
/// starting the session early. Counted per process, not per connection: Java,
/// Wine and ffmpeg open a connection that never reads beside the one that
/// does. Bounded; an unknown program ("") is never counted.
#[derive(Default)]
struct IdlePrograms {
    /// Program -> when its idle processes (and suppressed connections) were seen.
    runs: HashMap<String, std::collections::VecDeque<std::time::Instant>>,
    /// Processes one of whose connections read or injected, and processes
    /// already counted idle.
    used_pids: BTreeSet<u32>,
    counted_pids: BTreeSet<u32>,
}

impl IdlePrograms {
    const IDLE_AFTER: usize = 3;
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    const MAX: usize = 4096;

    fn recent(&mut self, program: &str, now: std::time::Instant) -> usize {
        let Some(q) = self.runs.get_mut(program) else {
            return 0;
        };
        while q
            .front()
            .is_some_and(|&t| now.duration_since(t) > Self::WINDOW)
        {
            q.pop_front();
        }
        q.len()
    }

    /// Whether `program` connecting now should start nothing; if so, the
    /// connection counts as one more idle run (a poller stays idle).
    fn idle(&mut self, program: &str, now: std::time::Instant) -> bool {
        if program.is_empty() || self.recent(program, now) < Self::IDLE_AFTER {
            return false;
        }
        self.push(program.to_string(), now);
        true
    }

    /// A connection of `program` (process `pid`) read the screen or injected.
    fn used(&mut self, program: &str, pid: Option<u32>) {
        self.runs.remove(program);
        if let Some(p) = pid {
            self.bound();
            self.used_pids.insert(p);
        }
    }

    /// A connection of `program` that started the session went away having
    /// done neither.
    fn unused(&mut self, program: String, pid: Option<u32>, now: std::time::Instant) {
        if program.is_empty() {
            return;
        }
        self.bound();
        match pid {
            // another connection of its process did
            Some(p) if self.used_pids.contains(&p) => {
                self.runs.remove(&program);
            }
            // its process is counted already
            Some(p) if !self.counted_pids.insert(p) => {}
            _ => self.push(program, now),
        }
    }

    fn push(&mut self, program: String, now: std::time::Instant) {
        let q = self.runs.entry(program).or_default();
        q.push_back(now);
        if q.len() > Self::IDLE_AFTER {
            q.pop_front();
        }
    }

    fn bound(&mut self) {
        if self.runs.len() >= Self::MAX {
            self.runs.clear();
        }
        if self.used_pids.len() >= Self::MAX {
            self.used_pids.clear();
        }
        if self.counted_pids.len() >= Self::MAX {
            self.counted_pids.clear();
        }
    }
}

/// The delay (ms) an XTest FakeInput request's `body` (after the 4-byte
/// header) asks for: its `time` field. The server runs a delayed request
/// again once the time has passed, with the field zeroed.
fn fake_delay(body: &[u8], little: bool) -> u32 {
    // type, detail, pad, time, ...
    body.get(4..8).map_or(0, |b| u32_at(b, little))
}

/// The pointer move an XTest FakeInput request's `body` (after the 4-byte
/// header) asks for: its detail says whether it is relative, rootX and rootY
/// give the position, or the distance if relative. `None` for other input, or
/// a request too short to hold the fields.
fn fake_motion(body: &[u8], little: bool) -> Option<Motion> {
    // type, detail, pad, time, root, 8 bytes pad, rootX, rootY, ...
    if body.len() < 24 || body[0] != MOTION_NOTIFY {
        return None;
    }
    let i16_at = |b: &[u8]| {
        (if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }) as i16
    };
    Some(Motion {
        relative: body[1] != 0,
        x: i16_at(&body[20..]),
        y: i16_at(&body[22..]),
    })
}

/// The confine-to window of a core `GrabPointer` request's `body` (after the
/// 4-byte header), in the client's byte order: grab-window, event-mask,
/// pointer- and keyboard-mode, confine-to, cursor, time (20 bytes whole).
/// `None` for other requests, or one too short to hold the field.
fn grab_confine(op: u8, body: &[u8], little: bool) -> Option<u32> {
    (op == GRAB_POINTER && body.len() >= 12).then(|| u32_at(&body[8..12], little))
}

/// A root read this long after the previous one wakes the agent.
const ROOT_READ_WAKE_MS: u64 = 250;

/// Whether request `op`/`minor` reads the pixels of the drawable its first
/// field names.
fn reads_pixels(op: u8, minor: u8, shm: Option<u8>) -> bool {
    op == GET_IMAGE || (Some(op) == shm && minor == SHM_GET_IMAGE)
}

fn u32_at(b: &[u8], little: bool) -> u32 {
    let a = [b[0], b[1], b[2], b[3]];
    if little {
        u32::from_le_bytes(a)
    } else {
        u32::from_be_bytes(a)
    }
}

/// Calls `f(major, minor, body, time)` for each request in `data`, a run of
/// whole requests in the client's byte order: `minor` is the header's second
/// byte (an extension's minor opcode), `body` what follows the header, `time`
/// the server time RECORD put before the request if `timed` (in our byte
/// order), else 0.
fn for_each_request(
    mut data: &[u8],
    little: bool,
    timed: bool,
    mut f: impl FnMut(u8, u8, &[u8], u32),
) {
    let u16_at = |b: &[u8]| {
        if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    loop {
        let mut time = 0;
        if timed {
            if data.len() < 4 {
                return;
            }
            time = u32::from_ne_bytes([data[0], data[1], data[2], data[3]]);
            data = &data[4..];
        }
        if data.len() < 4 {
            return;
        }
        let mut len = u16_at(&data[2..4]) as usize * 4;
        let mut body = 4;
        if len == 0 {
            // BIG-REQUESTS: the length follows the header
            if data.len() < 8 {
                return;
            }
            len = u32_at(&data[4..8], little) as usize * 4;
            body = 8;
        }
        if len < body || len > data.len() {
            return;
        }
        f(data[0], data[1], &data[body..len], time);
        data = &data[len..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: u32 = 0x40_0000;
    const SHIFT: u8 = 50;

    fn holds(persist: bool, linger_ms: u64) -> Holds {
        let policy = InputPolicy {
            persist,
            linger: Duration::from_millis(linger_ms),
            ..InputPolicy::default()
        };
        Holds::new(&policy, AGENT)
    }

    fn ms(t: Instant, ms: u64) -> Instant {
        t + Duration::from_millis(ms)
    }

    /// Clients connected longer than [`ONE_SHOT`] (VNC servers, say) that go
    /// away holding something: released at once, unless a live client holds it.
    #[test]
    fn a_departed_client_releases_only_what_it_alone_holds() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.input(1, KEY_PRESS, 38, t);
        h.input(1, KEY_PRESS, 50, t);
        h.input(1, BUTTON_PRESS, 1, t);
        h.input(2, KEY_PRESS, 50, t); // shared
        let later = ms(t, 5000);
        assert_eq!(
            h.died(1, later),
            Some(vec![(KEY_RELEASE, 38), (BUTTON_RELEASE, 1)])
        );
        assert_eq!(h.died(2, later), Some(vec![(KEY_RELEASE, 50)]));
        assert_eq!(h.died(3, later), None);
        assert_eq!((h.orphans(), h.next_due()), (0, None));
    }

    #[test]
    fn a_release_from_anyone_ends_the_hold() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.input(1, KEY_PRESS, 38, t);
        h.input(2, KEY_RELEASE, 38, t);
        assert_eq!(h.died(1, ms(t, 5000)), None);
    }

    /// B1: `xdotool keydown Shift_L; xdotool key 8; xdotool keyup Shift_L`,
    /// or RustDesk's connection per key event: the Shift a one-shot client
    /// left stays down for the next client's 8, and goes up with the third
    /// client's release, not before.
    #[test]
    fn a_one_shot_clients_hold_lingers_for_the_next_client() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.started(1, t);
        h.input(1, KEY_PRESS, SHIFT, t);
        assert_eq!(
            h.died(1, ms(t, 2)),
            Some(vec![]),
            "nothing released at once"
        );
        assert_eq!(h.next_due(), Some(ms(t, 1002)));
        assert!(h.expire(ms(t, 400)).is_empty());
        h.input(2, KEY_PRESS, 17, ms(t, 416));
        h.input(2, KEY_RELEASE, 17, ms(t, 417));
        assert_eq!(h.died(2, ms(t, 418)), None);
        // the 8 was input: the Shift lingers 1 s from there
        assert_eq!(h.next_due(), Some(ms(t, 1417)));
        assert!(h.expire(ms(t, 1054)).is_empty());
        h.input(3, KEY_RELEASE, SHIFT, ms(t, 1055));
        assert_eq!(h.died(3, ms(t, 1056)), None);
        assert_eq!((h.orphans(), h.next_due()), (0, None));
        assert!(h.expire(ms(t, 60_000)).is_empty());
    }

    /// Nothing stays stuck: what nobody releases goes up once X clients have
    /// sent nothing for the linger time (the agent's own releases don't
    /// count), and never later than LINGER_CAP after its client left.
    #[test]
    fn what_nobody_releases_goes_up_when_input_pauses_or_at_the_cap() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.started(1, t);
        h.input(1, KEY_PRESS, 38, t);
        h.input(1, BUTTON_PRESS, 1, t);
        assert_eq!(h.died(1, t), Some(vec![]));
        h.input(AGENT, KEY_RELEASE, 99, ms(t, 900)); // not a client's input
        assert!(h.expire(ms(t, 999)).is_empty());
        assert_eq!(
            h.expire(ms(t, 1000)),
            vec![(KEY_RELEASE, 38), (BUTTON_RELEASE, 1)]
        );
        // busy other clients keep a hold down, up to the cap
        h.started(2, ms(t, 1990));
        h.input(2, KEY_PRESS, 38, ms(t, 2000));
        assert_eq!(h.died(2, ms(t, 2000)), Some(vec![]));
        for step in 1..=20 {
            h.input(3, MOTION_NOTIFY, 0, ms(t, 2000 + step * 500));
            let due = h.expire(ms(t, 2000 + step * 500));
            if step * 500 < 10_000 {
                assert!(due.is_empty(), "step {step}");
            } else {
                assert_eq!(due, vec![(KEY_RELEASE, 38)], "step {step}");
                break;
            }
        }
        assert_eq!(h.orphans(), 0);
    }

    /// A lingering key a live client presses too goes up with its last
    /// holder: not when the lingering hold ends, but with that client.
    #[test]
    fn a_lingering_hold_shared_with_a_live_client_goes_up_with_the_last() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.started(1, t);
        h.input(1, KEY_PRESS, SHIFT, t);
        h.died(1, t);
        h.started(2, ms(t, 50));
        h.input(2, KEY_PRESS, SHIFT, ms(t, 100));
        assert!(h.expire(ms(t, 1100)).is_empty(), "client 2 still holds it");
        assert_eq!((h.orphans(), h.next_due()), (0, None));
        // client 2 is long-lived by now: its hold goes up when it leaves
        assert_eq!(h.died(2, ms(t, 5000)), Some(vec![(KEY_RELEASE, SHIFT)]));
    }

    /// A long-lived client leaving while a one-shot client's hold of the same
    /// key lingers: the key stays down with the lingering hold.
    #[test]
    fn a_long_lived_leaver_does_not_cut_a_lingering_hold_short() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.started(1, t);
        h.input(1, KEY_PRESS, SHIFT, t); // long-lived: connected at t, leaves at +5 s
        h.started(2, ms(t, 4980));
        h.input(2, KEY_PRESS, SHIFT, ms(t, 4990));
        assert_eq!(h.died(2, ms(t, 4995)), Some(vec![]));
        assert_eq!(h.died(1, ms(t, 5000)), Some(vec![]));
        assert_eq!(h.expire(ms(t, 5995)), vec![(KEY_RELEASE, SHIFT)]);
    }

    /// -xtestpersist: every departed client's holds stay until a client
    /// releases them; -xtestgrace 0: none lingers.
    #[test]
    fn persist_keeps_everything_and_zero_grace_nothing() {
        let t = Instant::now();
        let mut p = holds(true, 1000);
        p.input(1, KEY_PRESS, SHIFT, t);
        p.input(2, BUTTON_PRESS, 1, t);
        assert_eq!(p.died(1, ms(t, 1)), Some(vec![]));
        assert_eq!(
            p.died(2, ms(t, 60_000)),
            Some(vec![]),
            "even a long-lived client's"
        );
        assert_eq!((p.orphans(), p.next_due()), (2, None));
        assert!(p.expire(ms(t, 3_600_000)).is_empty());
        p.input(3, KEY_RELEASE, SHIFT, ms(t, 3_600_000));
        p.input(3, BUTTON_RELEASE, 1, ms(t, 3_600_000));
        assert_eq!(p.orphans(), 0);

        let mut z = holds(false, 0);
        z.started(1, t);
        z.input(1, KEY_PRESS, SHIFT, t);
        assert_eq!(z.died(1, ms(t, 1)), Some(vec![(KEY_RELEASE, SHIFT)]));
        assert_eq!(z.orphans(), 0);
    }

    /// One entry per key however many one-shot clients come and go, and none
    /// per departed client.
    #[test]
    fn many_one_shot_clients_cost_one_entry_per_key() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        for c in 0..10_000u32 {
            h.started(c, t);
            h.input(c, KEY_PRESS, SHIFT, t);
            h.input(c, BUTTON_PRESS, 1, t);
            h.died(c, t);
        }
        assert_eq!((h.clients.len(), h.started.len(), h.orphans()), (0, 0, 2));
        // clients that never press anything leave nothing behind either
        for c in 20_000..30_000u32 {
            h.started(c, t);
            h.input(c, MOTION_NOTIFY, 0, t);
            assert_eq!(h.died(c, t), None);
        }
        assert_eq!((h.clients.len(), h.started.len()), (0, 0));
    }

    /// What counts is how long the client was connected, not how long it held
    /// the key: a VNC server connected for minutes that dies right after its
    /// viewer pressed Ctrl has Ctrl released at once; so does a client that
    /// was there before RECORD started (its start unknown).
    #[test]
    fn a_long_connected_client_dying_right_after_a_press_releases_at_once() {
        let t = Instant::now();
        let mut h = holds(false, 1000);
        h.started(1, t);
        h.input(1, KEY_PRESS, 37, ms(t, 120_000));
        assert_eq!(h.died(1, ms(t, 120_001)), Some(vec![(KEY_RELEASE, 37)]));
        h.input(2, KEY_PRESS, 37, ms(t, 120_002)); // never seen starting
        assert_eq!(h.died(2, ms(t, 120_003)), Some(vec![(KEY_RELEASE, 37)]));
        assert_eq!(h.orphans(), 0);
    }

    /// The (type, detail) of every FakeInput request in `data`.
    fn fake_inputs(data: &[u8], little: bool, xtest: u8) -> Vec<(u8, u8)> {
        let mut seen = Vec::new();
        for_each_request(data, little, false, |op, minor, body, _| {
            if op == xtest && minor == FAKE_INPUT && body.len() >= 2 {
                seen.push((body[0], body[1]));
            }
        });
        seen
    }

    #[test]
    fn a_program_that_only_polls_stops_starting_capture() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut idle = IdlePrograms::default();
        // xprop polling: a new process each time, none reads
        for (i, pid) in (1..IdlePrograms::IDLE_AFTER as u32).enumerate() {
            idle.unused("xprop".into(), Some(pid), at(i as u64 * 3));
            assert!(!idle.idle("xprop", at(i as u64 * 3 + 1)));
        }
        idle.unused("xprop".into(), Some(100), at(9));
        assert!(idle.idle("xprop", at(10)));
        assert!(!idle.idle("scrot", at(10)));
        // while it goes on polling it stays idle, long after its runs
        for s in (12..200).step_by(2) {
            assert!(idle.idle("xprop", at(s)), "{s}");
        }
        // left alone for longer than the window, it starts the session again
        assert!(!idle.idle("xprop", at(200 + 61)));
        // one of its connections read the screen: it starts it again at once
        for pid in 300..303 {
            idle.unused("xprop".into(), Some(pid), at(300));
        }
        assert!(idle.idle("xprop", at(301)));
        idle.used("xprop", Some(400));
        assert!(!idle.idle("xprop", at(302)));
        // a client whose program is unknown is never counted
        for _ in 0..5 {
            idle.unused(String::new(), None, at(0));
        }
        assert!(!idle.idle("", at(1)));
    }

    #[test]
    fn programs_that_run_now_and_then_keep_starting_capture() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut idle = IdlePrograms::default();
        // Wine's explorer.exe: idle, but one run every 70 s
        for run in 0..5u32 {
            let now = t0 + Duration::from_secs(u64::from(run) * 70);
            assert!(!idle.idle("explorer.exe", now), "run {run}");
            idle.unused("explorer.exe".into(), Some(500 + run), now);
        }
        // Java's Robot: one connection that never reads beside the one that
        // does, in either order, run after run
        for run in 0..5u32 {
            let (pid, now) = (200 + run, t0 + Duration::from_secs(u64::from(run)));
            if run % 2 == 0 {
                idle.used("java", Some(pid));
                idle.unused("java".into(), Some(pid), now);
            } else {
                idle.unused("java".into(), Some(pid), now);
                idle.unused("java".into(), Some(pid), now); // a second idle connection
                idle.used("java", Some(pid));
            }
            assert!(!idle.idle("java", now), "run {run}");
        }
        // one process with three idle connections counts once
        for _ in 0..3 {
            idle.unused("winemenubuilder".into(), Some(300), t0);
        }
        assert!(!idle.idle("winemenubuilder", t0));
    }

    #[test]
    fn a_delayed_fake_input_is_seen() {
        // FakeInput body: type, detail, pad, time (ms of delay), ...
        let mut le = vec![KEY_PRESS, 38, 0, 0, 0xf4, 0x01, 0, 0];
        le.resize(32, 0);
        assert_eq!(fake_delay(&le, true), 500);
        let mut be = vec![KEY_PRESS, 38, 0, 0, 0, 0, 0x01, 0xf4];
        be.resize(32, 0);
        assert_eq!(fake_delay(&be, false), 500);
        le[4] = 0;
        le[5] = 0;
        assert_eq!(fake_delay(&le, true), 0);
        assert_eq!(fake_delay(&[KEY_PRESS, 38], true), 0);
    }

    #[test]
    fn fake_input_requests_are_found_in_either_byte_order() {
        // FakeInput: opcode 140, minor 2, length 9 words, KeyPress 38
        let mut le = vec![140u8, 2, 9, 0, KEY_PRESS, 38];
        le.resize(36, 0);
        let mut be = vec![140u8, 2, 0, 9, KEY_PRESS, 39];
        be.resize(36, 0);
        let mut seen = fake_inputs(&le.repeat(2), true, 140);
        seen.extend(fake_inputs(&be, false, 140));
        assert_eq!(
            seen,
            vec![(KEY_PRESS, 38), (KEY_PRESS, 38), (KEY_PRESS, 39)]
        );
    }

    #[test]
    fn root_reads_are_recognised() {
        const ROOT: u32 = 0x3dd;
        // GetImage(ZPixmap, root, 0,0 8x8, planes): 5 words, little-endian
        let mut get_image = vec![GET_IMAGE, 2, 5, 0];
        get_image.extend(ROOT.to_le_bytes());
        get_image.extend([0, 0, 0, 0, 8, 0, 8, 0, 0xff, 0xff, 0xff, 0xff]);
        // ShmGetImage on another window, big-endian (MIT-SHM is major 130 here)
        let mut shm_other = vec![130u8, SHM_GET_IMAGE, 0, 8];
        shm_other.extend(0x0040_0001u32.to_be_bytes());
        shm_other.resize(32, 0);
        let mut reads = Vec::new();
        for (data, little) in [(get_image, true), (shm_other, false)] {
            for_each_request(&data, little, false, |op, minor, body, _| {
                if reads_pixels(op, minor, Some(130)) {
                    reads.push(u32_at(&body[..4], little));
                }
            });
        }
        assert_eq!(reads, vec![ROOT, 0x0040_0001]);
        // a PutImage (72), a CopyArea (62) or another extension's minor 4 is
        // no read
        assert!(!reads_pixels(72, 0, Some(130)));
        assert!(!reads_pixels(62, 0, Some(130)));
        assert!(!reads_pixels(131, SHM_GET_IMAGE, Some(130)));
        assert!(!reads_pixels(130, 3, Some(130)));
    }

    /// An XTest FakeInput request as libXtst sends it (xXTestFakeInputReq).
    fn fake_motion_req(little: bool, relative: bool, x: i16, y: i16, big: bool) -> Vec<u8> {
        let w16 = |v: u16| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut r = vec![140u8, 2];
        if big {
            // BIG-REQUESTS form: length 0, then the length in 32-bit words
            r.extend_from_slice(&w16(0));
            r.extend_from_slice(&if little {
                10u32.to_le_bytes()
            } else {
                10u32.to_be_bytes()
            });
        } else {
            r.extend_from_slice(&w16(9));
        }
        let body = r.len();
        r.extend_from_slice(&[MOTION_NOTIFY, relative as u8]);
        r.resize(body + 20, 0); // pad, time, root, pad
        r.extend_from_slice(&w16(x as u16));
        r.extend_from_slice(&w16(y as u16));
        r.resize(body + 32, 0);
        r
    }

    /// The pointer moves of every FakeInput request in `data`.
    fn motions_in(data: &[u8], little: bool) -> Vec<Motion> {
        let mut seen = Vec::new();
        for_each_request(data, little, false, |op, minor, body, _| {
            if op == 140
                && minor == FAKE_INPUT
                && let Some(m) = fake_motion(body, little)
            {
                seen.push(m);
            }
        });
        seen
    }

    /// A core GrabPointer request as libX11 sends it (xGrabPointerReq):
    /// opcode 26, owner-events, length 6, grab-window, event-mask, the
    /// pointer- and keyboard-modes, the confine-to window (0 for None), the
    /// cursor and the time, in the client's byte order.
    fn grab_pointer_req(little: bool, confine: u32) -> Vec<u8> {
        let w16 = |v: u16| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let w32 = |v: u32| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut r = vec![GRAB_POINTER, 0];
        r.extend_from_slice(&w16(6));
        r.extend_from_slice(&w32(0x4000_0001)); // grab-window
        r.extend([0, 4]); // event-mask: button 3
        r.extend([0, 0]); // pointer- and keyboard-mode
        r.extend_from_slice(&w32(confine));
        r.extend_from_slice(&w32(0)); // cursor
        r.extend_from_slice(&w32(0)); // time
        r
    }

    /// The confine-to windows of every GrabPointer request in `data`.
    fn confines_in(data: &[u8], little: bool) -> Vec<u32> {
        let mut seen = Vec::new();
        for_each_request(data, little, false, |op, _, body, _| {
            if let Some(c) = grab_confine(op, body, little) {
                seen.push(c);
            }
        });
        seen
    }

    #[test]
    fn grab_pointer_requests_carry_their_confine_to() {
        for little in [true, false] {
            // SDL2's relative mode grabs its window
            let data = grab_pointer_req(little, 0x4000_0002);
            assert_eq!(
                confines_in(&data, little),
                vec![0x4000_0002],
                "little {little}"
            );
            // a pop-up grab (or a failed one): no confine-to
            assert_eq!(confines_in(&grab_pointer_req(little, 0), little), vec![0]);
        }
        // another request's body is not read for a confine-to
        let body = grab_pointer_req(true, 0x4000_0002);
        assert!(grab_confine(GET_IMAGE, &body[4..], true).is_none());
        assert!(grab_confine(UNGRAB_POINTER, &[0, 0, 0, 0], true).is_none());
        // a request too short to hold the field
        assert!(grab_confine(GRAB_POINTER, &[0, 0, 0, 0, 0, 0, 0, 0], true).is_none());
    }

    #[test]
    fn clients_confining_grabs_come_and_go_per_client() {
        let mut g = HashMap::new();
        // SDL2's grab: its confine-to window
        grab_seen(&mut g, 1, 0x4000_0001);
        assert_eq!(g.get(&1), Some(&0x4000_0001));
        // another client's grab without confine-to adds nothing
        grab_seen(&mut g, 2, 0);
        assert!(!g.contains_key(&2));
        // a re-grab without confine-to, and an UngrabPointer, end a held one
        grab_seen(&mut g, 1, 0);
        assert!(!g.contains_key(&1));
        grab_seen(&mut g, 1, 0x4000_0001);
        grab_gone(&mut g, 1);
        assert!(!g.contains_key(&1));
        // a client dying ends its grab, not another client's
        grab_seen(&mut g, 1, 0x4000_0001);
        grab_seen(&mut g, 2, 0x4000_0002);
        grab_gone(&mut g, 2);
        assert_eq!(g.get(&1), Some(&0x4000_0001));
        grab_gone(&mut g, 1);
        assert!(g.is_empty());
    }

    /// The grab state is shared (with the -showwindows thread) and a RECORD
    /// thread's start and end forget it.
    #[test]
    fn the_grab_state_is_shared_and_forgotten() {
        clear_grabs();
        assert!(!confining_grab());
        grab_seen(&mut confines(), 3, 0x4000_0003);
        assert!(confining_grab());
        clear_grabs();
        assert!(!confining_grab());
    }

    #[test]
    fn motion_requests_carry_their_kind_and_values() {
        for little in [true, false] {
            for big in [false, true] {
                let mut data = fake_motion_req(little, false, 400, 300, big);
                data.extend(fake_motion_req(little, true, -100, -50, big));
                assert_eq!(
                    motions_in(&data, little),
                    vec![
                        Motion {
                            relative: false,
                            x: 400,
                            y: 300
                        },
                        Motion {
                            relative: true,
                            x: -100,
                            y: -50
                        },
                    ],
                    "little {little} big {big}"
                );
            }
        }
    }

    #[test]
    fn timed_requests_carry_their_time() {
        // [time][FakeInput motion][time][NoOperation]
        let mut data = 1234u32.to_ne_bytes().to_vec();
        data.extend(fake_motion_req(true, false, -5, 700, false));
        data.extend(99u32.to_ne_bytes());
        data.extend([NO_OPERATION, 0, 1, 0]);
        let mut seen = Vec::new();
        for_each_request(&data, true, true, |op, minor, body, time| {
            seen.push((op, minor, time, fake_motion(body, true)));
        });
        assert_eq!(
            seen,
            vec![
                (
                    140,
                    FAKE_INPUT,
                    1234,
                    Some(Motion {
                        relative: false,
                        x: -5,
                        y: 700
                    })
                ),
                (NO_OPERATION, 0, 99, None)
            ]
        );
    }

    #[test]
    fn short_requests_are_not_read_past() {
        // a FakeInput claiming 1 word: too short to hold the fields
        assert!(motions_in(&[140u8, 2, 1, 0], true).is_empty());
        // a motion request cut short of rootX/rootY
        let mut short = vec![140u8, 2, 4, 0, MOTION_NOTIFY, 1];
        short.resize(16, 0);
        assert!(motions_in(&short, true).is_empty());
    }
}
