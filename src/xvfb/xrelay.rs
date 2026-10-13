//! `-clipboard xwayland`: the selections of the engine's clients relayed X to
//! X with the desktop's own Xwayland, which mutter keeps in step with the
//! Wayland clipboard and primary selection itself.
//!
//! Unlike the remote-desktop clipboard (mutter's `SetSelection`/
//! `SelectionRead`), this needs no session, so it works while capture is
//! stopped and shows no sharing indicator; and since both ends speak X, every
//! target goes through unchanged (images, HTML, URI lists, not only text), and
//! PRIMARY too.
//!
//! For each selection the relay is a proxy owner: when a client on one display
//! takes it, the relay takes it on the other, and answers each conversion
//! there by converting from the real owner on demand (nothing is copied ahead
//! of a paste). Atoms are per display, so target names, property types and
//! atom-valued data (TARGETS) are translated by name. Values larger than one
//! request go out incrementally (INCR), and INCR answers are read the same way.
//!
//! Our own ownership comes back from each display as an owner change naming
//! our window, which is how the relay tells it from a real one (no loop).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Property, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::AnyError;
use crate::bridge::clipboard::MAX_SELECTION_BYTES;
use crate::bridge::Server;
use crate::uidisplay::UiDisplay;

const ENGINE: usize = 0;
const DESK: usize = 1;
const SIDE_NAMES: [&str; 2] = ["engine", "xwayland"];

/// A conversion the real owner hasn't finished in this long is refused.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
/// A conversion the real owner hasn't answered at all in this long is asked
/// again (up to [`MAX_RETRIES`] times): an owner busy with an INCR transfer
/// to someone else may drop other requests meanwhile (xclip does, while a
/// clipboard manager such as CopyQ reads a large value), and the desktop
/// then keeps its old clipboard.
const RETRY_AFTER: Duration = Duration::from_millis(400);
const MAX_RETRIES: u32 = 20;
/// At most this many bytes of values in transit are held at once (INCR
/// answers being collected, values going out in INCR steps); a conversion
/// that would need more is refused, so a client that asks for a large
/// selection over and over without reading the answers can't make us hold
/// gigabytes.
const INFLIGHT_CAP: usize = 128 << 20;
/// How much of a large value goes out per INCR step.
const CHUNK: usize = 256 * 1024;
/// Transfer properties on our window per display: how many conversions can
/// be in flight at once.
const RING: usize = 16;
/// Reconnect delays for the desktop side (Xwayland restarted, not up yet).
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(10);
/// Atoms below this are predefined (the same number on every server).
const LAST_PREDEFINED: Atom = 68;

fn other(side: usize) -> usize {
    1 - side
}

/// One display's end of the relay.
struct Side {
    conn: RustConnection,
    win: Window,
    /// The selections, by index into [`Relay::sels`].
    sel_atoms: Vec<Atom>,
    ring: Vec<Atom>,
    targets: Atom,
    timestamp: Atom,
    multiple: Atom,
    incr: Atom,
    names: HashMap<Atom, Vec<u8>>,
    atoms: HashMap<Vec<u8>, Atom>,
    max_req: usize,
}

impl Side {
    fn new(conn: RustConnection, screen: usize, sels: &[&str]) -> Result<Side, AnyError> {
        let root = conn.setup().roots[screen].root;
        conn.xfixes_query_version(5, 0)?.reply()?;
        let win = conn.generate_id()?;
        conn.create_window(
            0,
            win,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let intern = |n: &str| -> Result<Atom, AnyError> { Ok(conn.intern_atom(false, n.as_bytes())?.reply()?.atom) };
        let mut sel_atoms = Vec::new();
        for s in sels {
            sel_atoms.push(intern(s)?);
        }
        let mut ring = Vec::new();
        for i in 0..RING {
            ring.push(intern(&format!("_XWLBRIDGE_RELAY_{i}"))?);
        }
        let (targets, timestamp, multiple, incr) =
            (intern("TARGETS")?, intern("TIMESTAMP")?, intern("MULTIPLE")?, intern("INCR")?);
        for &a in &sel_atoms {
            conn.xfixes_select_selection_input(
                win,
                a,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?;
        }
        let max_req = conn.maximum_request_bytes();
        conn.flush()?;
        Ok(Side {
            conn,
            win,
            sel_atoms,
            ring,
            targets,
            timestamp,
            multiple,
            incr,
            names: HashMap::new(),
            atoms: HashMap::new(),
            max_req,
        })
    }

    fn name(&mut self, atom: Atom) -> Result<Vec<u8>, AnyError> {
        if let Some(n) = self.names.get(&atom) {
            return Ok(n.clone());
        }
        let n = self.conn.get_atom_name(atom)?.reply()?.name;
        self.names.insert(atom, n.clone());
        self.atoms.insert(n.clone(), atom);
        Ok(n)
    }

    fn atom(&mut self, name: &[u8]) -> Result<Atom, AnyError> {
        if let Some(&a) = self.atoms.get(name) {
            return Ok(a);
        }
        let a = self.conn.intern_atom(false, name)?.reply()?.atom;
        self.atoms.insert(name.to_vec(), a);
        self.names.insert(a, name.to_vec());
        Ok(a)
    }

    fn sel_index(&self, atom: Atom) -> Option<usize> {
        self.sel_atoms.iter().position(|&a| a == atom)
    }
}

/// Translates `atom` of display `from` to the same-named atom of `to`.
fn translate(sides: &mut [Side; 2], from: usize, atom: Atom) -> Result<Atom, AnyError> {
    if atom <= LAST_PREDEFINED {
        return Ok(atom);
    }
    let name = sides[from].name(atom)?;
    sides[other(from)].atom(&name)
}

/// A conversion asked of the real owner on `src`, for a requestor on the
/// other display.
struct Forward {
    sel: usize,
    src: usize,
    prop: Atom,
    /// The target asked of the owner (an atom of `src`).
    target: Atom,
    req: SelectionRequestEvent,
    since: Instant,
    /// When it was (last) asked, and how often it was asked again.
    asked: Instant,
    retries: u32,
    /// The value so far, once the owner answered with INCR: (type, format, bytes).
    incr: Option<(Atom, u8, Vec<u8>)>,
}

/// A value going out to a requestor in INCR steps.
struct Outgoing {
    side: usize,
    requestor: Window,
    property: Atom,
    type_: Atom,
    format: u8,
    data: Vec<u8>,
    offset: usize,
    /// The final zero-length step was written.
    finished: bool,
    since: Instant,
}

#[derive(Default, Clone, Copy)]
struct SelState {
    /// The display whose client really owns it; we own it on the other.
    source: Option<usize>,
    /// When (each display's X time) we became the owner there, for TIMESTAMP.
    since: [Option<u32>; 2],
}

struct Relay {
    sides: [Side; 2],
    sels: Vec<SelState>,
    sel_names: Vec<&'static str>,
    forwards: Vec<Forward>,
    outgoing: Vec<Outgoing>,
    /// Transfer properties of conversions that were asked more than once:
    /// a late answer to an earlier ask may still come, so they are not used
    /// again for a while, and what arrives on them is thrown away (an INCR
    /// answer step by step, so the owner isn't left waiting).
    cooling: Vec<(usize, Atom, Instant)>,
    /// Conversions refused for [`INFLIGHT_CAP`] since the last log line, and
    /// when that was.
    capped: (u64, Option<Instant>),
}

/// Starts the relay on its own thread. `engine` is a connection to the
/// engine's display made before the agent watches who connects (so the
/// relay's own connection never counts as a client to start capture for).
pub fn spawn(server: Arc<Server>, engine: (RustConnection, usize), desk: UiDisplay) {
    let mut sels = vec!["CLIPBOARD"];
    if !server.config.noprimary {
        sels.push("PRIMARY");
    }
    let _ = std::thread::Builder::new().name("xwayland-clip".into()).spawn(move || {
        let (conn, screen) = engine;
        let mut engine = match Side::new(conn, screen, &sels) {
            Ok(s) => Some(s),
            Err(e) => {
                crate::warning!("xvfb: clipboard relay: engine side: {e}; no clipboard");
                return;
            }
        };
        let mut delay = RETRY_MIN;
        let mut warned = false;
        loop {
            let desk_side = connect(&desk).and_then(|(c, s)| Side::new(c, s, &sels));
            match desk_side {
                Ok(d) => {
                    crate::log!(
                        "xvfb: clipboard relayed with the desktop's Xwayland ({}): {}",
                        desk.display.to_string_lossy(),
                        sels.join(", ")
                    );
                    delay = RETRY_MIN;
                    warned = false;
                    let mut relay = Relay {
                        sides: [engine.take().expect("kept between runs"), d],
                        sels: vec![SelState::default(); sels.len()],
                        sel_names: sels.clone(),
                        forwards: Vec::new(),
                        outgoing: Vec::new(),
                        cooling: Vec::new(),
                        capped: (0, None),
                    };
                    let err = relay.run();
                    let [e, _] = relay.sides;
                    match err {
                        Failed::Engine(err) => {
                            crate::warning!("xvfb: clipboard relay: lost the engine display: {err}");
                            return;
                        }
                        Failed::Desk(err) => {
                            crate::warning!("xvfb: clipboard relay: lost Xwayland ({err}); reconnecting");
                            // nothing on the engine side may keep pointing at a
                            // desktop owner we can no longer reach
                            for &a in &e.sel_atoms {
                                if e.conn.get_selection_owner(a).ok().and_then(|c| c.reply().ok()).map(|r| r.owner)
                                    == Some(e.win)
                                {
                                    let _ = e.conn.set_selection_owner(NONE, a, CURRENT_TIME);
                                }
                            }
                            let _ = e.conn.flush();
                            engine = Some(e);
                        }
                    }
                }
                Err(err) => {
                    if !warned {
                        crate::warning!(
                            "xvfb: clipboard relay: can't reach the desktop's Xwayland {} ({err}); retrying",
                            desk.display.to_string_lossy()
                        );
                        warned = true;
                    }
                }
            }
            // keep the engine connection's events drained meanwhile
            if let Some(e) = engine.as_ref() {
                while let Ok(Some(_)) = e.conn.poll_for_event() {}
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(RETRY_MAX);
        }
    });
}

/// Whether the desktop's X display answers (mutter starts its Xwayland on
/// the first connection, which this may be).
pub fn reachable(d: &UiDisplay) -> bool {
    connect(d).is_ok()
}

/// Connects to `d` with its own authority file (not necessarily ours).
fn connect(d: &UiDisplay) -> Result<(RustConnection, usize), AnyError> {
    let name = d.display.to_string_lossy().into_owned();
    let parsed = x11rb_protocol::parse_display::parse_display(Some(&name))?;
    let screen = parsed.screen.into();
    let cookie = d
        .xauthority
        .as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|data| cookie_for(&data, parsed.display));
    let mut last: Option<std::io::Error> = None;
    for addr in parsed.connect_instruction() {
        match DefaultStream::connect(&addr) {
            Ok((stream, _)) => {
                let (n, c) = cookie.clone().unwrap_or_default();
                return Ok((RustConnection::connect_to_stream_with_auth_info(stream, screen, n, c)?, screen));
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or_else(|| "no address to connect to".into(), Into::into))
}

/// The MIT-MAGIC-COOKIE-1 for local display `number` in an Xauthority file.
fn cookie_for(mut data: &[u8], number: u16) -> Option<(Vec<u8>, Vec<u8>)> {
    fn field<'a>(d: &mut &'a [u8]) -> Option<&'a [u8]> {
        let n = u16::from_be_bytes([*d.first()?, *d.get(1)?]) as usize;
        let f = d.get(2..2 + n)?;
        *d = &d[2 + n..];
        Some(f)
    }
    let want = number.to_string();
    while data.len() >= 2 {
        let family = u16::from_be_bytes([data[0], data[1]]);
        data = &data[2..];
        let _address = field(&mut data)?;
        let num = field(&mut data)?;
        let name = field(&mut data)?;
        let cookie = field(&mut data)?;
        // 256 = FamilyLocal, 65535 = FamilyWild
        if (family == 256 || family == 65535)
            && (num.is_empty() || num == want.as_bytes())
            && name == b"MIT-MAGIC-COOKIE-1"
        {
            return Some((name.to_vec(), cookie.to_vec()));
        }
    }
    None
}

/// Why the relay loop ended.
enum Failed {
    Engine(AnyError),
    Desk(AnyError),
}

impl Relay {
    fn run(&mut self) -> Failed {
        if let Err(e) = self.initial_sync() {
            return Failed::Desk(e);
        }
        loop {
            for side in [ENGINE, DESK] {
                loop {
                    let ev = match self.sides[side].conn.poll_for_event() {
                        Ok(Some(ev)) => ev,
                        Ok(None) => break,
                        Err(e) => return fail(side, e.into()),
                    };
                    if let Err(e) = self.handle(side, ev) {
                        // a reply error names the display it came from; any
                        // other failure is a lost connection: tell them apart
                        // by which connection still answers
                        let bad = if self.sides[DESK].conn.get_input_focus().is_ok_and(|c| c.reply().is_ok()) {
                            ENGINE
                        } else {
                            DESK
                        };
                        if self.sides[bad].conn.get_input_focus().is_ok_and(|c| c.reply().is_ok()) {
                            crate::cliplog!("relay: {e} (continuing)");
                            continue;
                        }
                        return fail(bad, e);
                    }
                }
            }
            self.expire();
            for side in [ENGINE, DESK] {
                if let Err(e) = self.sides[side].conn.flush() {
                    return fail(side, e.into());
                }
            }
            let mut pfds = [
                libc::pollfd { fd: self.sides[ENGINE].conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.sides[DESK].conn.stream().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            // while a conversion waits for its owner, wake often enough to ask
            // it again (see RETRY_AFTER)
            let timeout = if self.forwards.iter().any(|f| f.incr.is_none()) { 50 } else { 1000 };
            unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout) };
            for (side, p) in pfds.iter().enumerate() {
                if p.revents & (libc::POLLHUP | libc::POLLERR) != 0 && p.revents & libc::POLLIN == 0 {
                    return fail(side, "connection closed".into());
                }
            }
        }
    }

    /// Whoever owns a selection now (the desktop first: that is what the
    /// person copied last) gets proxied on the other display.
    fn initial_sync(&mut self) -> Result<(), AnyError> {
        for i in 0..self.sels.len() {
            for side in [DESK, ENGINE] {
                let s = &self.sides[side];
                let owner = s.conn.get_selection_owner(s.sel_atoms[i])?.reply()?.owner;
                if owner != NONE && owner != s.win {
                    self.took(side, i)?;
                    break;
                }
            }
        }
        Ok(())
    }

    fn handle(&mut self, side: usize, ev: Event) -> Result<(), AnyError> {
        match ev {
            Event::XfixesSelectionNotify(e) => {
                let Some(i) = self.sides[side].sel_index(e.selection) else { return Ok(()) };
                if e.owner == self.sides[side].win {
                    // our own proxy ownership coming back
                    self.sels[i].since[side] = Some(e.selection_timestamp);
                } else if e.owner == NONE {
                    if self.sels[i].source == Some(side) {
                        crate::cliplog!("relay: {} {}: owner gone", SIDE_NAMES[side], self.sel_names[i]);
                        self.sels[i].source = None;
                        self.disown(other(side), i)?;
                    }
                } else {
                    crate::cliplog!(
                        "relay: {} {}: taken by window {:#x}",
                        SIDE_NAMES[side],
                        self.sel_names[i],
                        e.owner
                    );
                    self.took(side, i)?;
                }
            }
            Event::SelectionRequest(e) if e.owner == self.sides[side].win => self.request(side, e)?,
            Event::SelectionNotify(e) if e.requestor == self.sides[side].win => self.answered(side, e)?,
            Event::PropertyNotify(e) => {
                if e.window == self.sides[side].win && e.state == Property::NEW_VALUE {
                    self.incr_chunk(side, e.atom)?;
                } else if e.state == Property::DELETE {
                    self.incr_next(side, e.window, e.atom)?;
                }
            }
            Event::Error(e) => crate::cliplog!("relay: {} X error {:?}", SIDE_NAMES[side], e.error_kind),
            _ => {}
        }
        Ok(())
    }

    /// A client on `side` took selection `i`: own it on the other display.
    fn took(&mut self, side: usize, i: usize) -> Result<(), AnyError> {
        self.sels[i].source = Some(side);
        let o = &self.sides[other(side)];
        o.conn.set_selection_owner(o.win, o.sel_atoms[i], CURRENT_TIME)?;
        Ok(())
    }

    /// Gives up our proxy ownership of selection `i` on `side`, if we still hold it.
    fn disown(&mut self, side: usize, i: usize) -> Result<(), AnyError> {
        let s = &self.sides[side];
        let owner = s.conn.get_selection_owner(s.sel_atoms[i])?.reply()?.owner;
        if owner == s.win {
            s.conn.set_selection_owner(NONE, s.sel_atoms[i], CURRENT_TIME)?;
        }
        Ok(())
    }

    /// A requestor on `side` converts a selection we proxy there.
    fn request(&mut self, side: usize, e: SelectionRequestEvent) -> Result<(), AnyError> {
        let Some(i) = self.sides[side].sel_index(e.selection) else { return refuse(&self.sides[side].conn, &e) };
        let src = other(side);
        if self.sels[i].source != Some(src) {
            return refuse(&self.sides[side].conn, &e);
        }
        let mut e = e;
        if e.property == NONE {
            // an obsolete requestor: the target doubles as the property
            e.property = e.target;
        }
        let s = &self.sides[side];
        if e.target == s.multiple {
            // MULTIPLE would need its atom-pair list relayed both ways; no
            // client of either display was seen to use it
            return refuse(&s.conn, &e);
        }
        if e.target == s.timestamp {
            let since = self.sels[i].since[side].unwrap_or(e.time);
            s.conn.change_property32(PropMode::REPLACE, e.requestor, e.property, AtomEnum::INTEGER, &[since])?;
            return notify(&s.conn, &e, e.property);
        }
        let in_use: Vec<Atom> = self
            .forwards
            .iter()
            .filter(|f| f.src == src)
            .map(|f| f.prop)
            .chain(self.cooling.iter().filter(|c| c.0 == src).map(|c| c.1))
            .collect();
        let Some(&prop) = self.sides[src].ring.iter().find(|p| !in_use.contains(p)) else {
            crate::cliplog!("relay: too many conversions in flight; refusing one");
            return refuse(&self.sides[side].conn, &e);
        };
        let target = translate(&mut self.sides, side, e.target)?;
        let s = &self.sides[src];
        s.conn.delete_property(s.win, prop)?;
        s.conn.convert_selection(s.win, s.sel_atoms[i], target, prop, CURRENT_TIME)?;
        crate::cliplog!(
            "relay: {} asks {} for {:?}",
            SIDE_NAMES[side],
            SIDE_NAMES[src],
            String::from_utf8_lossy(&self.sides[src].names.get(&target).cloned().unwrap_or_default())
        );
        let now = Instant::now();
        self.forwards.push(Forward { sel: i, src, prop, target, req: e, since: now, asked: now, retries: 0, incr: None });
        Ok(())
    }

    /// The real owner on `side` answered one of our conversions.
    fn answered(&mut self, side: usize, n: SelectionNotifyEvent) -> Result<(), AnyError> {
        let i = self.sides[side].sel_index(n.selection);
        let pos = if n.property != NONE {
            self.forwards
                .iter()
                .position(|f| f.src == side && f.prop == n.property && f.target == n.target && f.incr.is_none())
        } else {
            // a refusal names no property: the oldest matching conversion
            let target_name = self.sides[side].name(n.target)?;
            let mut found = None;
            for (k, f) in self.forwards.iter().enumerate() {
                if f.src == side && Some(f.sel) == i && f.incr.is_none() {
                    let dst = other(side);
                    if self.sides[dst].names.get(&f.req.target).is_some_and(|t| *t == target_name)
                        || f.req.target <= LAST_PREDEFINED && f.req.target == n.target
                    {
                        found = Some(k);
                        break;
                    }
                }
            }
            found
        };
        let Some(pos) = pos else {
            // a late answer to an ask that was repeated: throw it away (an
            // INCR answer step by step, see incr_chunk)
            if n.property != NONE && self.cooling.iter().any(|c| c.0 == side && c.1 == n.property) {
                let s = &self.sides[side];
                let r = s
                    .conn
                    .get_property(true, s.win, n.property, AtomEnum::ANY, 0, (MAX_SELECTION_BYTES / 4) as u32)?
                    .reply()?;
                crate::cliplog!(
                    "relay: {} answered an earlier ask late{}; dropped",
                    SIDE_NAMES[side],
                    if r.type_ == s.incr { " (INCR, drained)" } else { "" }
                );
            }
            return Ok(());
        };
        if self.forwards[pos].retries > 0 {
            // earlier asks of this one may still be answered
            self.cooling.push((side, n.property, Instant::now()));
        }
        if n.property == NONE {
            let f = self.forwards.remove(pos);
            crate::cliplog!("relay: {} refused", SIDE_NAMES[side]);
            return refuse(&self.sides[other(side)].conn, &f.req);
        }
        let s = &self.sides[side];
        let r = s
            .conn
            .get_property(true, s.win, n.property, AtomEnum::ANY, 0, (MAX_SELECTION_BYTES / 4) as u32)?
            .reply()?;
        if r.type_ == s.incr {
            // the value comes in steps, each a new value of the (now deleted) property
            self.forwards[pos].incr = Some((NONE, 8, Vec::new()));
            return Ok(());
        }
        if r.bytes_after > 0 {
            let f = self.forwards.remove(pos);
            crate::warning!("xvfb: clipboard relay: a value over {MAX_SELECTION_BYTES} bytes; refused");
            return refuse(&self.sides[other(side)].conn, &f.req);
        }
        let f = self.forwards.remove(pos);
        self.deliver(f, r.type_, r.format, r.value)
    }

    /// One INCR step of an answer arrived on our window on `side`.
    fn incr_chunk(&mut self, side: usize, prop: Atom) -> Result<(), AnyError> {
        let Some(pos) = self.forwards.iter().position(|f| f.src == side && f.prop == prop && f.incr.is_some()) else {
            if self.cooling.iter().any(|c| c.0 == side && c.1 == prop) {
                // a step of a late INCR answer we don't want: delete it, so
                // the owner goes on to the end
                let s = &self.sides[side];
                // (a delete happens only once everything is read)
                s.conn.get_property(true, s.win, prop, AtomEnum::ANY, 0, (MAX_SELECTION_BYTES / 4) as u32)?.reply()?;
            }
            return Ok(());
        };
        let held = self.held_bytes();
        let s = &self.sides[side];
        let r = s.conn.get_property(true, s.win, prop, AtomEnum::ANY, 0, (MAX_SELECTION_BYTES / 4) as u32)?.reply()?;
        let f = &mut self.forwards[pos];
        f.since = Instant::now();
        let (type_, format, buf) = f.incr.as_mut().expect("checked above");
        if !r.value.is_empty() {
            if buf.len() + r.value.len() > MAX_SELECTION_BYTES {
                let f = self.forwards.remove(pos);
                crate::warning!("xvfb: clipboard relay: a value over {MAX_SELECTION_BYTES} bytes; refused");
                // its remaining steps are thrown away
                self.cooling.push((side, prop, Instant::now()));
                return refuse(&self.sides[other(side)].conn, &f.req);
            }
            if held + r.value.len() > INFLIGHT_CAP {
                let f = self.forwards.remove(pos);
                self.over_cap();
                self.cooling.push((side, prop, Instant::now()));
                return refuse(&self.sides[other(side)].conn, &f.req);
            }
            if buf.is_empty() {
                (*type_, *format) = (r.type_, r.format);
            }
            buf.extend_from_slice(&r.value);
            return Ok(());
        }
        let mut f = self.forwards.remove(pos);
        let (type_, format, data) = f.incr.take().expect("checked above");
        self.deliver(f, type_, format, data)
    }

    /// Hands a value from the owner on `f.src` to the requestor on the other
    /// display, translated to that display's atoms.
    fn deliver(&mut self, f: Forward, type_: Atom, format: u8, mut data: Vec<u8>) -> Result<(), AnyError> {
        let (src, dst) = (f.src, other(f.src));
        if type_ == NONE {
            return refuse(&self.sides[dst].conn, &f.req);
        }
        let atom_valued = format == 32
            && (type_ == Atom::from(AtomEnum::ATOM) || self.sides[src].name(type_)? == b"ATOM_PAIR");
        if atom_valued {
            let mut out = Vec::with_capacity(data.len());
            let drop_multiple = f.req.target == self.sides[dst].targets;
            for w in data.chunks_exact(4) {
                let a = u32::from_ne_bytes([w[0], w[1], w[2], w[3]]);
                if a == NONE {
                    out.extend_from_slice(&a.to_ne_bytes());
                    continue;
                }
                let t = translate(&mut self.sides, src, a)?;
                if drop_multiple && t == self.sides[dst].multiple {
                    continue;
                }
                out.extend_from_slice(&t.to_ne_bytes());
            }
            data = out;
        }
        let type_d = translate(&mut self.sides, src, type_)?;
        let d = &self.sides[dst];
        let unit = usize::from(format.max(8) / 8);
        crate::cliplog!(
            "relay: {} bytes from {} to {} (format {format})",
            data.len(),
            SIDE_NAMES[src],
            SIDE_NAMES[dst]
        );
        if data.len() + 1024 > d.max_req {
            // a new conversion into a property we are still filling: the
            // requestor gave up on the old one
            self.outgoing
                .retain(|o| !(o.side == dst && o.requestor == f.req.requestor && o.property == f.req.property));
            if self.held_bytes() + data.len() > INFLIGHT_CAP {
                self.over_cap();
                return refuse(&self.sides[dst].conn, &f.req);
            }
            let d = &self.sides[dst];
            // too big for one request: INCR, one step per deletion of the
            // property
            d.conn.change_window_attributes(
                f.req.requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?;
            d.conn
                .change_property32(PropMode::REPLACE, f.req.requestor, f.req.property, d.incr, &[data.len() as u32])?;
            notify(&d.conn, &f.req, f.req.property)?;
            self.outgoing.push(Outgoing {
                side: dst,
                requestor: f.req.requestor,
                property: f.req.property,
                type_: type_d,
                format,
                data,
                offset: 0,
                finished: false,
                since: Instant::now(),
            });
            return Ok(());
        }
        d.conn.change_property(
            PropMode::REPLACE,
            f.req.requestor,
            f.req.property,
            type_d,
            format,
            (data.len() / unit) as u32,
            &data,
        )?;
        notify(&d.conn, &f.req, f.req.property)
    }

    /// A requestor on `side` deleted a property: if it is one we are filling
    /// incrementally, the next step.
    fn incr_next(&mut self, side: usize, window: Window, prop: Atom) -> Result<(), AnyError> {
        let Some(pos) = self
            .outgoing
            .iter()
            .position(|o| o.side == side && o.requestor == window && o.property == prop)
        else {
            return Ok(());
        };
        let o = &mut self.outgoing[pos];
        o.since = Instant::now();
        let conn = &self.sides[side].conn;
        if o.finished {
            let o = self.outgoing.remove(pos);
            if !self.outgoing.iter().any(|x| x.side == side && x.requestor == o.requestor) {
                let _ = conn.change_window_attributes(o.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT));
            }
            return Ok(());
        }
        let unit = usize::from(o.format.max(8) / 8);
        let end = (o.offset + CHUNK).min(o.data.len());
        let chunk = &o.data[o.offset..end];
        conn.change_property(
            PropMode::REPLACE,
            o.requestor,
            o.property,
            o.type_,
            o.format,
            (chunk.len() / unit) as u32,
            chunk,
        )?;
        o.finished = chunk.is_empty();
        o.offset = end;
        Ok(())
    }

    /// Logs a conversion refused for [`INFLIGHT_CAP`]: the first, then a
    /// count at most every 10 s.
    fn over_cap(&mut self) {
        self.capped.0 += 1;
        if self.capped.1.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
            crate::warning!(
                "xvfb: clipboard relay: {} MiB of values already in transit; refused {} conversion(s)",
                INFLIGHT_CAP >> 20,
                self.capped.0
            );
            self.capped = (0, Some(Instant::now()));
        }
    }

    /// Bytes of values in transit: INCR answers being collected and values
    /// going out in INCR steps.
    fn held_bytes(&self) -> usize {
        let incoming: usize = self.forwards.iter().filter_map(|f| f.incr.as_ref()).map(|(_, _, b)| b.len()).sum();
        let outgoing: usize = self.outgoing.iter().map(|o| o.data.len()).sum();
        incoming + outgoing
    }

    /// Refuses conversions whose owner went quiet, asks again those it hasn't
    /// answered at all yet, and drops stalled INCR transfers.
    fn expire(&mut self) {
        let mut k = 0;
        while k < self.forwards.len() {
            let f = &mut self.forwards[k];
            if f.since.elapsed() > TRANSFER_TIMEOUT {
                let f = self.forwards.remove(k);
                crate::cliplog!("relay: {} didn't answer in time", SIDE_NAMES[f.src]);
                if f.retries > 0 {
                    self.cooling.push((f.src, f.prop, Instant::now()));
                }
                let _ = refuse(&self.sides[other(f.src)].conn, &f.req);
                continue;
            }
            if f.incr.is_none() && f.retries < MAX_RETRIES && f.asked.elapsed() >= RETRY_AFTER {
                let s = &self.sides[f.src];
                f.asked = Instant::now();
                f.retries += 1;
                crate::cliplog!(
                    "relay: {} hasn't answered in {} ms; asking again ({})",
                    SIDE_NAMES[f.src],
                    f.since.elapsed().as_millis(),
                    f.retries
                );
                let _ = s.conn.convert_selection(s.win, s.sel_atoms[f.sel], f.target, f.prop, CURRENT_TIME);
            }
            k += 1;
        }
        self.outgoing.retain(|o| o.since.elapsed() <= TRANSFER_TIMEOUT);
        self.cooling.retain(|c| c.2.elapsed() <= TRANSFER_TIMEOUT);
    }
}

fn fail(side: usize, e: AnyError) -> Failed {
    if side == ENGINE { Failed::Engine(e) } else { Failed::Desk(e) }
}

fn notify(conn: &RustConnection, e: &SelectionRequestEvent, property: Atom) -> Result<(), AnyError> {
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: e.time,
        requestor: e.requestor,
        selection: e.selection,
        target: e.target,
        property,
    };
    conn.send_event(false, e.requestor, EventMask::NO_EVENT, ev)?;
    Ok(())
}

fn refuse(conn: &RustConnection, e: &SelectionRequestEvent) -> Result<(), AnyError> {
    notify(conn, e, NONE)
}

#[cfg(test)]
mod tests {
    use super::cookie_for;

    fn entry(family: u16, number: &str, name: &str, data: &[u8]) -> Vec<u8> {
        let mut v = family.to_be_bytes().to_vec();
        for f in [b"host".as_slice(), number.as_bytes(), name.as_bytes(), data] {
            v.extend((f.len() as u16).to_be_bytes());
            v.extend_from_slice(f);
        }
        v
    }

    #[test]
    fn the_cookie_for_the_display_is_found() {
        let mut file = entry(256, "1", "MIT-MAGIC-COOKIE-1", &[1, 2]);
        file.extend(entry(0, "0", "MIT-MAGIC-COOKIE-1", &[3, 4])); // an internet entry
        file.extend(entry(256, "0", "XDM-AUTHORIZATION-1", &[5, 6]));
        file.extend(entry(256, "0", "MIT-MAGIC-COOKIE-1", &[7, 8]));
        assert_eq!(cookie_for(&file, 0), Some((b"MIT-MAGIC-COOKIE-1".to_vec(), vec![7, 8])));
        assert_eq!(cookie_for(&file, 1).map(|c| c.1), Some(vec![1, 2]));
        assert_eq!(cookie_for(&file, 5), None);
        // a wildcard entry with no number matches any display
        let wild = entry(65535, "", "MIT-MAGIC-COOKIE-1", &[9]);
        assert_eq!(cookie_for(&wild, 7).map(|c| c.1), Some(vec![9]));
        // truncated data ends the search instead of panicking
        assert_eq!(cookie_for(&file[..5], 0), None);
    }
}
