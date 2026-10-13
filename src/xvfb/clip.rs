//! The clipboard between the external X server's clients and the desktop,
//! bridged the way an X clipboard manager does it, over the same state
//! machine the built-in engine uses ([`Clipboard`](crate::bridge::clipboard)):
//!
//! - an X client takes a selection (XFixes tells us): we convert it to
//!   UTF8_STRING and the value is published to the compositor;
//! - a Wayland app takes it (the backend announces it): we own the X selection
//!   and serve conversions by piping the value out of the compositor.
//!
//! Text only, as with the built-in engine.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::AnyError;
use crate::bridge::Server;
use crate::bridge::clipboard::{MAX_SELECTION_BYTES, OWNER_WINDOW, Sel, Value};
use crate::bridge::clipjobs::{Job, PendingConversion};

/// What we ask the compositor for, best first, among what its owner offers.
const MIME_PREFERENCE: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

struct Atoms {
    targets: Atom,
    timestamp: Atom,
    utf8: Atom,
    text: Atom,
    incr: Atom,
    plain_utf8: Atom,
    plain: Atom,
}

/// One selection, as this side sees it.
struct Side {
    sel: Sel,
    atom: Atom,
    /// The property our conversions of it land in.
    prop: Atom,
    /// The clipboard generation we last acted on.
    seen: u64,
    /// When (X time) we became its owner, while we are.
    ours_since: Option<u32>,
    fetch: Option<Fetch>,
}

/// A conversion we asked an X owner for.
struct Fetch {
    window: Window,
    /// The bridge-clock time its acquisition was recorded under.
    time: u32,
    /// Chunks so far, once it answered with INCR.
    incr: Option<Vec<u8>>,
}

pub struct Clip {
    win: Window,
    atoms: Atoms,
    sides: Vec<Side>,
    pending: HashMap<u64, SelectionRequestEvent>,
    next_id: u64,
    tx: Sender<(u64, Option<Vec<u8>>)>,
    rx: Receiver<(u64, Option<Vec<u8>>)>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, AnyError> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

impl Clip {
    pub fn new(
        conn: &RustConnection,
        root: Window,
        server: &Arc<Server>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Clip, AnyError> {
        xfixes::query_version(conn, 5, 0)?.reply()?;
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
        let atoms = Atoms {
            targets: intern(conn, "TARGETS")?,
            timestamp: intern(conn, "TIMESTAMP")?,
            utf8: intern(conn, "UTF8_STRING")?,
            text: intern(conn, "TEXT")?,
            incr: intern(conn, "INCR")?,
            plain_utf8: intern(conn, "text/plain;charset=utf-8")?,
            plain: intern(conn, "text/plain")?,
        };
        let mut sides = vec![Side {
            sel: Sel::Clipboard,
            atom: intern(conn, "CLIPBOARD")?,
            prop: intern(conn, "_XWLBRIDGE_CLIPBOARD")?,
            seen: u64::MAX,
            ours_since: None,
            fetch: None,
        }];
        if !server.config.noprimary {
            sides.push(Side {
                sel: Sel::Primary,
                atom: AtomEnum::PRIMARY.into(),
                prop: intern(conn, "_XWLBRIDGE_PRIMARY")?,
                seen: u64::MAX,
                ours_since: None,
                fetch: None,
            });
        }
        for s in &sides {
            conn.xfixes_select_selection_input(
                win,
                s.atom,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?;
        }
        let w = wake.clone();
        server.clipboard.set_watcher(Box::new(move || w()));
        let (tx, rx) = mpsc::channel();
        Ok(Clip {
            win,
            atoms,
            sides,
            pending: HashMap::new(),
            next_id: 0,
            tx,
            rx,
            wake,
        })
    }

    fn side(&mut self, atom: Atom) -> Option<&mut Side> {
        self.sides.iter_mut().find(|s| s.atom == atom)
    }

    /// Catches up with the desktop: owns the X selections a Wayland app owns,
    /// lets go of those nobody does, and answers conversions whose value came.
    pub fn poll(&mut self, conn: &RustConnection, server: &Arc<Server>) -> Result<(), AnyError> {
        for s in &mut self.sides {
            let generation = server.clipboard.generation(s.sel);
            if generation == s.seen {
                continue;
            }
            s.seen = generation;
            match server.clipboard.owner_window(s.sel) {
                Some(OWNER_WINDOW) => {
                    crate::cliplog!(
                        "xvfb: {:?}: a wayland app owns it; taking the X selection",
                        s.sel
                    );
                    conn.set_selection_owner(self.win, s.atom, CURRENT_TIME)?;
                }
                None if s.ours_since.is_some() => {
                    conn.set_selection_owner(NONE, s.atom, CURRENT_TIME)?;
                    s.ours_since = None;
                }
                _ => {}
            }
        }
        while let Ok((id, data)) = self.rx.try_recv() {
            if let Some(req) = self.pending.remove(&id) {
                self.answer(conn, &req, data)?;
            }
        }
        Ok(())
    }

    /// Handles `ev` if it is clipboard business; returns whether it was.
    pub fn handle(
        &mut self,
        conn: &RustConnection,
        server: &Arc<Server>,
        ev: &x11rb::protocol::Event,
    ) -> Result<bool, AnyError> {
        use x11rb::protocol::Event as E;
        match ev {
            E::XfixesSelectionNotify(e) => {
                let win = self.win;
                let Some(s) = self.side(e.selection) else {
                    return Ok(true);
                };
                if e.owner == win {
                    s.ours_since = Some(e.selection_timestamp);
                    return Ok(true);
                }
                s.ours_since = None;
                s.fetch = None;
                let sel = s.sel;
                if e.owner == NONE {
                    if let Some(fx) = server.clipboard.x_external_gone(sel) {
                        fx.apply(server, sel);
                    }
                    return Ok(true);
                }
                let (fx, time) = server.clipboard.x_took_external(sel, e.owner);
                fx.apply(server, sel);
                let utf8 = self.atoms.utf8;
                let s = self.side(e.selection).expect("looked up above");
                s.seen = server.clipboard.generation(sel);
                s.fetch = Some(Fetch {
                    window: e.owner,
                    time,
                    incr: None,
                });
                conn.convert_selection(win, s.atom, utf8, s.prop, e.selection_timestamp)?;
            }
            E::SelectionNotify(e) if e.requestor == self.win => {
                let Some(s) = self.side(e.selection) else {
                    return Ok(true);
                };
                let Some(f) = &s.fetch else { return Ok(true) };
                let (sel, window, time, prop) = (s.sel, f.window, f.time, s.prop);
                if e.property == NONE {
                    s.fetch = None;
                    crate::cliplog!("xvfb: {sel:?}: owner {window:#x} refused UTF8_STRING");
                    if let Some(fx) = server.clipboard.x_fetch_failed(sel, window, time) {
                        fx.apply(server, sel);
                    }
                    return Ok(true);
                }
                let r = conn
                    .get_property(
                        true,
                        self.win,
                        prop,
                        AtomEnum::ANY,
                        0,
                        (MAX_SELECTION_BYTES / 4) as u32,
                    )?
                    .reply()?;
                let incr = self.atoms.incr;
                let s = self.side(e.selection).expect("looked up above");
                if r.type_ == incr {
                    // the value comes in chunks, each announced by a new
                    // value on the (just deleted) property
                    if let Some(f) = &mut s.fetch {
                        f.incr = Some(Vec::new());
                    }
                    return Ok(true);
                }
                s.fetch = None;
                self.fetched(server, sel, window, time, r.value);
            }
            E::PropertyNotify(e) if e.window == self.win && e.state == Property::NEW_VALUE => {
                let Some(s) = self.sides.iter_mut().find(|s| s.prop == e.atom) else {
                    return Ok(false);
                };
                let Some(Fetch { incr: Some(_), .. }) = &s.fetch else {
                    return Ok(true);
                };
                let r = conn
                    .get_property(
                        true,
                        self.win,
                        s.prop,
                        AtomEnum::ANY,
                        0,
                        (MAX_SELECTION_BYTES / 4) as u32,
                    )?
                    .reply()?;
                let f = s.fetch.as_mut().expect("checked above");
                let buf = f.incr.as_mut().expect("checked above");
                if !r.value.is_empty() && buf.len() + r.value.len() <= MAX_SELECTION_BYTES {
                    buf.extend_from_slice(&r.value);
                    return Ok(true);
                }
                let f = s.fetch.take().expect("checked above");
                let sel = s.sel;
                if r.value.is_empty() {
                    self.fetched(server, sel, f.window, f.time, f.incr.unwrap_or_default());
                } else if let Some(fx) = server.clipboard.x_fetch_failed(sel, f.window, f.time) {
                    crate::warning!(
                        "xvfb: {sel:?} is over {MAX_SELECTION_BYTES} bytes; not bridged"
                    );
                    fx.apply(server, sel);
                }
            }
            E::SelectionRequest(e) if e.owner == self.win => self.request(conn, server, e)?,
            E::SelectionClear(e) if e.owner == self.win => {
                if let Some(s) = self.side(e.selection) {
                    s.ours_since = None;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn fetched(&self, server: &Arc<Server>, sel: Sel, window: Window, time: u32, data: Vec<u8>) {
        crate::cliplog!("xvfb: {sel:?}: owner {window:#x} gave {} bytes", data.len());
        if let Some(fx) = server.clipboard.x_fetched(sel, window, time, data) {
            fx.apply(server, sel);
        }
    }

    /// An X client converts a selection we own.
    fn request(
        &mut self,
        conn: &RustConnection,
        server: &Arc<Server>,
        e: &SelectionRequestEvent,
    ) -> Result<(), AnyError> {
        let a = &self.atoms;
        let Some(s) = self.sides.iter().find(|s| s.atom == e.selection) else {
            return refuse(conn, e);
        };
        let (sel, since) = (s.sel, s.ours_since.unwrap_or(e.time));
        // obsolete clients name no property: the target doubles as one
        let prop = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        if e.target == a.targets {
            let targets = [
                a.targets,
                a.timestamp,
                a.utf8,
                AtomEnum::STRING.into(),
                a.text,
                a.plain_utf8,
                a.plain,
            ];
            conn.change_property32(
                PropMode::REPLACE,
                e.requestor,
                prop,
                AtomEnum::ATOM,
                &targets,
            )?;
            return notify(conn, e, prop);
        }
        if e.target == a.timestamp {
            conn.change_property32(
                PropMode::REPLACE,
                e.requestor,
                prop,
                AtomEnum::INTEGER,
                &[since],
            )?;
            return notify(conn, e, prop);
        }
        let text = [
            a.utf8,
            a.text,
            a.plain_utf8,
            a.plain,
            AtomEnum::STRING.into(),
        ];
        if !text.contains(&e.target) {
            return refuse(conn, e);
        }
        match server.clipboard.value(sel) {
            Value::None => refuse(conn, e),
            Value::Here(data) => self.answer(conn, e, Some(data)),
            Value::FromWayland => {
                let (mimes, generation) = server.clipboard.convertible(sel);
                let Some(mime) = MIME_PREFERENCE
                    .iter()
                    .find(|m| mimes.iter().any(|x| x == *m))
                else {
                    return refuse(conn, e);
                };
                let id = self.next_id;
                self.next_id += 1;
                let mut req = *e;
                req.property = prop;
                self.pending.insert(id, req);
                let (tx, wake) = (self.tx.clone(), self.wake.clone());
                // a job the queue refuses is dropped, which refuses the
                // conversion through the same channel
                server.clipboard.jobs().post(Job::Receive {
                    sel,
                    mime: (*mime).to_string(),
                    generation,
                    since: Instant::now(),
                    conversion: PendingConversion::new(move |data| {
                        let _ = tx.send((id, data));
                        wake();
                    }),
                });
                Ok(())
            }
        }
    }

    /// Stores `data` for the requestor and tells it, or refuses with `None`.
    fn answer(
        &self,
        conn: &RustConnection,
        e: &SelectionRequestEvent,
        data: Option<Vec<u8>>,
    ) -> Result<(), AnyError> {
        let Some(mut data) = data else {
            return refuse(conn, e);
        };
        let prop = if e.property == NONE {
            e.target
        } else {
            e.property
        };
        // leave room for the request header
        if data.len() + 64 > conn.maximum_request_bytes() {
            crate::warning!(
                "xvfb: a {}-byte selection is too big to hand over in one piece; refused",
                data.len()
            );
            return refuse(conn, e);
        }
        let type_: Atom = if e.target == Atom::from(AtomEnum::STRING) {
            // Latin-1, by definition
            data = String::from_utf8_lossy(&data)
                .chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                .collect();
            AtomEnum::STRING.into()
        } else {
            self.atoms.utf8
        };
        conn.change_property8(PropMode::REPLACE, e.requestor, prop, type_, &data)?;
        notify(conn, e, prop)
    }
}

fn notify(
    conn: &RustConnection,
    e: &SelectionRequestEvent,
    property: Atom,
) -> Result<(), AnyError> {
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
