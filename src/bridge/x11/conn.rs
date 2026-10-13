use std::io::{self, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use x11rb_protocol::RawFdContainer;
use x11rb_protocol::protocol::Request;
use x11rb_protocol::protocol::xproto::{
    AutoRepeatMode, BackingStore, Depth, EventMask, Format, ImageOrder, Screen, Setup, VisualClass,
    Visualtype,
};
use x11rb_protocol::protocol::{bigreq, damage, randr, render, shm, xfixes, xproto, xtest};
use x11rb_protocol::x11_utils::{RequestHeader, Serialize, TryParse};

use super::mit_shm::Shm;
use super::property::{Properties, Property};
use super::selection::{Fetch, IncrStep, Selection};
use super::targets;
use super::window::Windows;
use super::xfixes::Regions;
use super::xkb;
use super::{ROOT_COLORMAP, ROOT_DEPTH, ROOT_VISUAL, ROOT_WINDOW};
use crate::bridge::Server;
use crate::bridge::clipboard::{MAX_SELECTION_BYTES, Sel, Take, Value};
use crate::bridge::clipjobs::{Job, PendingConversion};
use crate::bridge::event::Client;
use crate::bridge::x11::atom::{XA_INTEGER, XA_PRIMARY, XA_STRING};
use crate::bridge::x11::ext::EXTENSIONS;
use crate::bridge::x11::randr as randr_screen;
use crate::bridge::x11::{CLIENT_RESOURCE_ID_BASE, SELECTION_FETCH_WINDOW};
use crate::bridge::xkbdesc::Desc;

/// How many property names the fetch ring cycles through, per selection. A
/// property that comes round again while the fetch that had it is still out
/// displaces that fetch, and its late answer would then be taken for the new
/// one's (see [`Selection::begin_fetch`]); so this has to be more fetches
/// than a client can plausibly start inside `FETCH_TIMEOUT`, after which the
/// old one is gone anyway. Atoms are never freed, so it is not unbounded.
const FETCH_PROPERTIES: u32 = 32;
use crate::util::{Geometry, bbox, mm};

/// The largest request we accept, in 4-byte units including the header, as
/// advertised by `BigreqEnable`. Anything bigger is a broken or hostile
/// client, and is disconnected rather than allocated for.
const MAX_REQUEST_LENGTH: u32 = 4_194_303;

/// How long `RRSetCrtcConfig` waits for the compositor to change an output's
/// mode (`-dynres`). Well past what a compositor needs, and short enough that
/// a client blocked in Xlib on a compositor that never answers gets its
/// failure rather than a hang.
const DYNRES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What answering one `ConvertSelection` came to.
enum Fill {
    /// The answer is ready: reply naming this property, or 0 to refuse.
    Now(u32),
    /// The value is on its way out of a Wayland app; the requestor is answered
    /// by the job fetching it, not here.
    Later,
}

pub struct Connection {
    reader: BufReader<UnixStream>,
    client: Arc<Client>,
    server: Arc<Server>,
    /// Per-connection id, never reused: for tracing and input accounting.
    id: u32,
    /// This connection's resource-id range, once the handshake handed it out;
    /// free for another connection once this one drops.
    ids: Option<super::IdRange<'static>>,
    /// Current request sequence number.
    seq: u16,
    /// Stored window properties for fake windows (i.e., clipboard/WM).
    ///
    /// Still per connection — one client's properties are not another's, which
    /// is what RealVNC's clipboard expects (`docs/troubleshooting.md`) — but
    /// behind a lock and an `Arc`, because a conversion answered out of a
    /// Wayland app finishes on the Wayland thread and has to store its result
    /// somewhere the connection thread will read it, possibly after the
    /// connection itself has gone.
    properties: Arc<Mutex<Properties>>,
    /// Clipboard/selection state machine (owners, in-flight fetches, INCR).
    selection: Selection,
    /// Cycles the property name each fetch uses, so an answer can be told
    /// from a stale one: a ring of [`FETCH_PROPERTIES`] rather than a fresh
    /// atom per fetch, since atoms are interned for good and never freed.
    fetch_seq: u32,
    /// MIT-SHM segments the client attached, for ShmGetImage.
    shm: Shm,
    /// XFixes regions (rectangle lists), keyed by region id.
    regions: Regions,
    /// Per-window selected event masks (for PropertyNotify routing + diagnostics).
    windows: Windows,
    /// XKB `PerClientFlags` the client set (kept and reported, nothing more).
    xkb_flags: u32,
    /// XKB minor opcodes this client has already had a warning logged for
    /// (refused, unknown or malformed requests, keymap loads): one line per
    /// client and request, so a client can't grow the log at request rate.
    xkb_warned: [u64; 4],
    /// Spare keycodes this client put keysyms on and is holding down, and
    /// the compositor keys pressed for them (shared with the stock engines'
    /// agent: [`keymap::Synth`](crate::bridge::keymap::Synth)).
    synth: crate::bridge::keymap::Synth,
    /// Whether a refused `ChangeKeyboardMapping` was logged for this client.
    mapping_refusal_logged: bool,
    /// The client is an engine helper (`-xtesthelpers`): its XTest drives
    /// windows on this display, never the desktop, so it is dropped. Its
    /// program, for the log, until logged.
    engine_helper: Option<String>,
    helper_logged: bool,
    /// It read the screen: when it goes, capture lingers for the next
    /// one-shot screenshot (`-linger`).
    active: bool,
    /// When the client connected: one gone within a second is a one-shot
    /// injector, whose held keys linger (see `Input::client_gone`).
    since: std::time::Instant,
}

/// The pid of the process at the other end of a unix socket.
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills in a ucred, and we pass its size
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// The program of an engine helper at the other end of `stream`, if it is one.
fn engine_helper(stream: &UnixStream, server: &Server) -> Option<String> {
    let rules = server.config.xtest_rules();
    if !rules.has_helpers() {
        return None;
    }
    let names = crate::bridge::injectors::process_names(peer_pid(stream)?);
    (rules.classify(&names) == crate::bridge::injectors::Class::Helper).then(|| names.join("/"))
}

impl Connection {
    pub fn new(stream: UnixStream, server: Arc<Server>) -> io::Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let client = Client::new(stream.try_clone()?)?;
        let engine_helper = engine_helper(&stream, &server);
        Ok(Self {
            reader: BufReader::new(stream),
            client,
            server,
            id,
            ids: None,
            seq: 0,
            properties: Arc::default(),
            selection: Selection::default(),
            fetch_seq: 0,
            shm: Shm::default(),
            regions: Regions::default(),
            windows: Windows::default(),
            xkb_flags: 0,
            xkb_warned: [0; 4],
            synth: Default::default(),
            active: false,
            mapping_refusal_logged: false,
            engine_helper,
            helper_logged: false,
            since: std::time::Instant::now(),
        })
    }

    pub fn run(mut self) -> io::Result<()> {
        self.handshake()?;
        // only now: an event fanned out from another thread before the setup
        // bytes were queued would land ahead of them on the wire
        self.server.events.register(&self.client);
        loop {
            // An X owner we have asked for its selection may never answer, and
            // an owner that is silent on that is usually silent altogether: so
            // while a fetch is out this thread waits for the next request no
            // longer than the fetch has left, and gives up on it in time
            // rather than leaving watchers untold and the previous owner's
            // value served on both sides for as long as the client says
            // nothing. Nothing else can: the fetch is this connection's.
            self.sweep_fetches();
            if let Some(deadline) = self.selection.next_deadline()
                && self.reader.buffer().is_empty()
                && !wait_readable(self.reader.get_ref(), deadline)?
            {
                continue;
            }
            let Some(raw) = read_request(&mut self.reader)? else {
                break;
            };
            self.seq = self.seq.wrapping_add(1);
            self.client.set_seq(self.seq);
            self.dispatch(raw)?;
        }
        Ok(())
    }

    fn handshake(&mut self) -> io::Result<()> {
        let mut head = [0u8; 12];
        self.reader.read_exact(&mut head)?;
        if head[0] != b'l' {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "big-endian X11 clients are not supported",
            ));
        }
        let name_len = u16::from_le_bytes([head[6], head[7]]) as usize;
        let data_len = u16::from_le_bytes([head[8], head[9]]) as usize;
        // Consume and ignore auth data.
        let mut auth = vec![0u8; pad4(name_len) + pad4(data_len)];
        self.reader.read_exact(&mut auth)?;
        // A resource-id range no other live connection holds; with every one
        // taken, refuse the client as Xorg does rather than hand out a shared
        // range or ids outside X's 29 bits.
        let Some(ids) = super::ID_RANGES.take() else {
            let max = super::MAX_CLIENTS;
            crate::warning!("refusing an X client: all {max} client slots are in use");
            self.client
                .write_setup(&setup_failed(b"Maximum number of clients reached"))?;
            return Err(io::Error::other("too many X clients"));
        };
        let bytes = setup(self.server.geometry(), ids.base());
        self.ids = Some(ids);
        self.client.write_setup(&bytes)
    }

    fn dispatch(&mut self, raw: RawRequest) -> io::Result<()> {
        let (major, minor) = (raw.major_opcode, raw.minor_opcode);
        let header = RequestHeader {
            major_opcode: raw.major_opcode,
            minor_opcode: raw.minor_opcode,
            remaining_length: raw.remaining_length,
        };
        if major == xkb::MAJOR_OPCODE {
            if self.server.config.xkb() {
                return self.dispatch_xkb(major, minor, &raw.body);
            }
            // `-noxkb`: as a server without the extension answers its opcode
            return self.send_error(xproto::REQUEST_ERROR, major, u16::from(minor));
        }
        let mut fds: Vec<RawFdContainer> = Vec::new();
        let req = match Request::parse(header, &raw.body, &mut fds, &EXTENSIONS) {
            Ok(req) => req,
            Err(e) => {
                // BadLength is what a real server answers a malformed request
                // with; without some reply a client waiting on one hangs forever
                crate::warning!("failed to parse request {major}.{minor}: {e:?}");
                return self.send_error(xproto::LENGTH_ERROR, major, u16::from(minor));
            }
        };

        // Full request trace, skipping noisy capture/input requests.
        if self.server.config.xtrace
            && !matches!(
                req,
                Request::ShmGetImage(_)
                    | Request::GetImage(_)
                    | Request::XtestFakeInput(_)
                    | Request::ChangeProperty(_)
            )
        {
            warning!("xtrace: [c{}] {major}.{minor}: {req:?}", self.id);
        }

        match req {
            // --- atoms ---
            Request::InternAtom(r) => {
                let atom = self
                    .server
                    .atoms
                    .lock()
                    .unwrap()
                    .intern(&r.name, r.only_if_exists);
                self.reply(&xproto::InternAtomReply {
                    sequence: 0,
                    length: 0,
                    atom,
                })?;
            }
            Request::GetAtomName(r) => {
                // an atom nobody interned is BadAtom, as in dix: answering
                // with an empty name instead makes a client walking the table
                // (xlsatoms does) walk it forever
                let Some(name) = self.atom_name(r.atom) else {
                    return self.send_error_value(
                        xproto::ATOM_ERROR,
                        r.atom,
                        major,
                        u16::from(minor),
                    );
                };
                self.reply(&xproto::GetAtomNameReply {
                    sequence: 0,
                    length: 0,
                    name,
                })?;
            }

            // --- extensions ---
            Request::QueryExtension(r) => {
                // Hide DAMAGE when disabled so clients fall back to polling.
                let e = EXTENSIONS.lookup(&r.name).filter(|e| self.advertised(e));
                if self.server.config.xtrace {
                    warning!(
                        "xtrace: [c{}] QueryExtension {:?} -> {}",
                        self.id,
                        String::from_utf8_lossy(&r.name),
                        if e.is_some() { "present" } else { "absent" }
                    );
                }
                self.reply(&xproto::QueryExtensionReply {
                    sequence: 0,
                    length: 0,
                    present: e.is_some(),
                    major_opcode: e.map_or(0, |e| e.major_opcode),
                    first_event: e.map_or(0, |e| e.first_event),
                    first_error: e.map_or(0, |e| e.first_error),
                })?;
            }
            Request::ListExtensions(_) => {
                let names = EXTENSIONS
                    .0
                    .iter()
                    .filter(|e| self.advertised(e))
                    .map(|e| xproto::Str {
                        name: e.name.as_bytes().to_vec(),
                    })
                    .collect();
                self.reply(&xproto::ListExtensionsReply {
                    sequence: 0,
                    length: 0,
                    names,
                })?;
            }

            // --- properties / selections (clipboard groundwork) ---
            Request::ChangeProperty(r) => {
                // INCR receive: the selection owner is feeding a large value in
                // chunks onto our fetch window. Each non-empty write is a chunk;
                // an empty write signals completion. We ack each by deleting the
                // property (which notifies the owner to send the next chunk).
                if self.selection.incr_chunk(r.window, r.property) {
                    match self.selection.incr_push(r.property, r.data.into_owned()) {
                        IncrStep::Done(fetch, data) => self.fetched(fetch, Some(data)),
                        IncrStep::More(property) => self.property_notify(
                            SELECTION_FETCH_WINDOW,
                            property,
                            xproto::Property::DELETE,
                        ),
                        // and no ack, so the owner sends no more of it
                        IncrStep::TooBig(fetch) => {
                            crate::cliplog!(
                                "{:?} owner {:#x} sent more than {MAX_SELECTION_BYTES} bytes; giving up on it",
                                fetch.kind,
                                fetch.owner
                            );
                            self.fetched(fetch, None);
                        }
                    }
                    return Ok(());
                }
                let mut value = Property {
                    type_: r.type_,
                    format: r.format,
                    data: r.data.into_owned(),
                };
                {
                    let mut properties = self.properties.lock().unwrap();
                    // Prepend/Append extend an existing value, which must agree
                    // on type and format (BadMatch otherwise); on a missing
                    // property they behave as Replace.
                    if r.mode != xproto::PropMode::REPLACE
                        && let Some(old) = properties.get(r.window, r.property)
                    {
                        if old.type_ != value.type_ || old.format != value.format {
                            drop(properties);
                            return self.send_error(xproto::MATCH_ERROR, major, u16::from(minor));
                        }
                        if r.mode == xproto::PropMode::APPEND {
                            let mut data = old.data.clone();
                            data.append(&mut value.data);
                            value.data = data;
                        } else {
                            value.data.extend_from_slice(&old.data);
                        }
                    }
                    properties.set(r.window, r.property, value);
                }
                self.property_notify(r.window, r.property, xproto::Property::NEW_VALUE);
            }
            Request::DeleteProperty(r) => {
                self.properties.lock().unwrap().remove(r.window, r.property);
                self.property_notify(r.window, r.property, xproto::Property::DELETE);
            }
            Request::GetProperty(r) => {
                let (reply, deleted) = self.get_property(&r);
                self.reply(&reply)?;
                if deleted {
                    self.property_notify(r.window, r.property, xproto::Property::DELETE);
                }
            }
            Request::ListProperties(r) => {
                let atoms = self.properties.lock().unwrap().list(r.window);
                self.reply(&xproto::ListPropertiesReply {
                    sequence: 0,
                    length: 0,
                    atoms,
                })?;
            }
            Request::SetSelectionOwner(r) => {
                // bridged selections are server-global (Wayland can revoke
                // them), the rest are per-connection
                let kind = self.selection_kind(r.selection);
                let time = if r.time == 0 {
                    crate::bridge::event::server_time_ms()
                } else {
                    r.time
                };
                crate::cliplog!(
                    "SetSelectionOwner {kind:?} owner {:#x} time {time}",
                    r.owner,
                );
                // X drops a request from the future, or from before the
                // selection last changed, without a word: the client believes
                // it owns the selection until it asks
                let stale = || {
                    crate::cliplog!("SetSelectionOwner at {time} is stale; ignored");
                    Ok(())
                };
                match (kind, r.owner) {
                    // a bridged selection: the clipboard state machine makes
                    // the transition and hands back the events it calls for
                    (Some(k), 0) => match self.server.clipboard.x_released(k, time) {
                        Take::Took(effects) => effects.apply(&self.server, k),
                        Take::Stale => return stale(),
                    },
                    (Some(k), owner) => {
                        match self.server.clipboard.x_took(k, owner, &self.client, time) {
                            Take::Took(effects) => {
                                effects.apply(&self.server, k);
                                if owner == crate::bridge::clipboard::OWNER_WINDOW {
                                    // our own synthetic window: there is nothing to
                                    // ask for its value, so nothing to wait for
                                    self.selection_owner_notify(kind, r.selection, owner, time);
                                } else {
                                    // pull its value into Wayland; watchers hear of
                                    // the acquisition once that resolves (`fetched`)
                                    self.start_fetch(k, r.selection, owner, time);
                                }
                            }
                            Take::Stale => return stale(),
                        }
                    }
                    // one we don't bridge: this connection's own owner table,
                    // and only this connection's watchers hear about it
                    (None, owner) => {
                        if !self.selection.set_owner(r.selection, owner, time) {
                            return stale();
                        }
                        self.selection_owner_notify(kind, r.selection, owner, time);
                    }
                }
            }
            Request::GetSelectionOwner(r) => {
                let owner = match self.selection_kind(r.selection) {
                    Some(k) => self.server.clipboard.owner_window(k),
                    None => self.selection.owner(r.selection),
                };
                self.reply(&xproto::GetSelectionOwnerReply {
                    sequence: 0,
                    length: 0,
                    owner: owner.unwrap_or(0),
                })?;
            }
            Request::ConvertSelection(r) => self.convert_selection(&r)?,
            Request::SendEvent(r) => self.handle_send_event(&r.event),
            Request::XfixesSelectSelectionInput(r) => {
                let known = xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
                let mask = u32::from(r.event_mask);
                if mask & !u32::from(known) != 0 {
                    return self.send_error_value(
                        xproto::VALUE_ERROR,
                        mask,
                        major,
                        u16::from(minor),
                    );
                }
                let kind = self.selection_kind(r.selection);
                crate::cliplog!(
                    "SelectSelectionInput window {:#x} {kind:?} (atom {}) mask {mask:#x}",
                    r.window,
                    r.selection,
                );
                self.client
                    .select_selection(r.selection, r.window, r.event_mask);
            }

            // --- window/drawable queries (we have no real windows) ---
            Request::GetInputFocus(_) => {
                self.reply(&xproto::GetInputFocusReply {
                    revert_to: xproto::InputFocus::POINTER_ROOT,
                    sequence: 0,
                    length: 0,
                    focus: ROOT_WINDOW,
                })?;
            }
            Request::GetGeometry(_) => {
                let g = self.server.geometry();
                self.reply(&xproto::GetGeometryReply {
                    depth: ROOT_DEPTH,
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: g.width,
                    height: g.height,
                    border_width: 0,
                })?;
            }
            Request::QueryTree(_) => {
                self.reply(&xproto::QueryTreeReply {
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    parent: 0,
                    children: vec![],
                })?;
            }
            Request::QueryPointer(_) => {
                let c = self.server.cursor.snapshot();
                self.reply(&xproto::QueryPointerReply {
                    same_screen: true,
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    child: 0,
                    root_x: c.x,
                    root_y: c.y,
                    win_x: c.x,
                    win_y: c.y,
                    mask: xproto::KeyButMask::from(0u16),
                })?;
            }
            Request::TranslateCoordinates(r) => {
                self.reply(&xproto::TranslateCoordinatesReply {
                    same_screen: true,
                    sequence: 0,
                    length: 0,
                    child: 0,
                    dst_x: r.src_x,
                    dst_y: r.src_y,
                })?;
            }
            Request::GetWindowAttributes(_) => {
                self.reply(&window_attributes())?;
            }

            // --- keyboard / pointer mapping ---
            Request::GetKeyboardMapping(r) => {
                let (per, keysyms) = match self.server.keymap.lock().unwrap().as_ref() {
                    // laid out as Xorg's with XKB, as before it without
                    Some(table) => {
                        let (per, syms) = if self.server.config.xkb() {
                            (table.per, &table.syms)
                        } else {
                            (crate::bridge::keymap::LEGACY_PER, &table.legacy)
                        };
                        (
                            per,
                            crate::bridge::keymap::mapping_slice(
                                syms,
                                per,
                                r.first_keycode,
                                r.count,
                            ),
                        )
                    }
                    // No compositor keymap yet: report one NoSymbol per keycode.
                    None => (1u8, vec![0u32; r.count as usize]),
                };
                self.reply(&xproto::GetKeyboardMappingReply {
                    keysyms_per_keycode: per,
                    sequence: 0,
                    keysyms,
                })?;
            }
            Request::ChangeKeyboardMapping(r) => self.change_keyboard_mapping(&r)?,
            Request::GetModifierMapping(_) => {
                let keycodes = self
                    .server
                    .keymap
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or_else(|| DEFAULT_MODIFIER_MAP.to_vec(), |t| t.modmap.clone());
                self.reply(&xproto::GetModifierMappingReply {
                    sequence: 0,
                    length: 0,
                    keycodes,
                })?;
            }
            Request::GetPointerControl(_) => {
                self.reply(&xproto::GetPointerControlReply {
                    sequence: 0,
                    length: 0,
                    acceleration_numerator: 2,
                    acceleration_denominator: 1,
                    threshold: 4,
                })?;
            }
            Request::GetScreenSaver(_) => {
                let s = super::saver::get();
                self.reply(&xproto::GetScreenSaverReply {
                    sequence: 0,
                    length: 0,
                    timeout: s.timeout as u16,
                    interval: s.interval as u16,
                    prefer_blanking: s.prefer_blanking,
                    allow_exposures: s.allow_exposures,
                })?;
            }
            Request::GetFontPath(_) => {
                self.reply(&xproto::GetFontPathReply {
                    sequence: 0,
                    length: 0,
                    path: vec![],
                })?;
            }
            Request::GetPointerMapping(_) => {
                // Report 7 buttons (identity mapping). vncagent only synthesizes
                // XTest button events for button numbers up to the pointer button
                // count it reads here, so buttons 4-7 (the scroll wheel and
                // horizontal wheel) must be present or scrolling never fires.
                self.reply(&xproto::GetPointerMappingReply {
                    sequence: 0,
                    length: 0,
                    map: vec![1, 2, 3, 4, 5, 6, 7],
                })?;
            }
            Request::QueryKeymap(_) => {
                self.reply(&xproto::QueryKeymapReply {
                    sequence: 0,
                    length: 0,
                    keys: [0; 32],
                })?;
            }
            Request::GetKeyboardControl(_) => {
                self.reply(&xproto::GetKeyboardControlReply {
                    global_auto_repeat: xproto::AutoRepeatMode::ON,
                    sequence: 0,
                    length: 0,
                    led_mask: 0,
                    key_click_percent: 0,
                    bell_percent: 0,
                    bell_pitch: 0,
                    bell_duration: 0,
                    auto_repeats: [0; 32],
                })?;
            }

            // --- colors (we have a static TrueColor visual) ---
            Request::AllocColor(r) => {
                let pixel = (u32::from(r.red >> 8) << 16)
                    | (u32::from(r.green >> 8) << 8)
                    | u32::from(r.blue >> 8);
                self.reply(&xproto::AllocColorReply {
                    sequence: 0,
                    length: 0,
                    red: r.red,
                    green: r.green,
                    blue: r.blue,
                    pixel,
                })?;
            }
            Request::LookupColor(_) => {
                // Stub: we have no color-name database, so resolve every name to
                // black. Not used by VNC; just enough for tools that call it not
                // to error. exact == visual (TrueColor needs no approximation).
                self.reply(&xproto::LookupColorReply {
                    sequence: 0,
                    length: 0,
                    exact_red: 0,
                    exact_green: 0,
                    exact_blue: 0,
                    visual_red: 0,
                    visual_green: 0,
                    visual_blue: 0,
                })?;
            }
            // --- screen capture ---
            Request::GetImage(r) => {
                // A rectangle larger than the whole screen can only be a bad
                // request (X answers BadMatch for one outside the drawable);
                // refusing it keeps one client from making us allocate up to
                // 16 GiB for its reply. Slightly-off rectangles (a read racing
                // a resize) are still served, zero-filled as before.
                let geom = self.server.geometry();
                let screen = u64::from(geom.width).max(1) * u64::from(geom.height).max(1);
                if u64::from(r.width) * u64::from(r.height) > screen {
                    return self.send_error(xproto::MATCH_ERROR, major, u16::from(minor));
                }
                self.await_fresh();
                let data = self.server.framebuffer.read_rect(
                    r.x.into(),
                    r.y.into(),
                    r.width.into(),
                    r.height.into(),
                );
                self.reply(&xproto::GetImageReply {
                    depth: ROOT_DEPTH,
                    sequence: 0,
                    visual: ROOT_VISUAL,
                    data,
                })?;
            }
            Request::ShmAttach(r) => self.shm.attach(r.shmseg, r.shmid),
            Request::ShmDetach(r) => self.shm.detach(r.shmseg),
            Request::ShmGetImage(r) => {
                // Read straight into the client's shared segment (vncagent
                // reads the whole screen this way ~20/sec).
                let image_size = r.width as usize * r.height as usize * 4;
                self.await_fresh();
                if let Some(seg) = self.shm.segment(r.shmseg) {
                    let off = (r.offset as usize).min(seg.size);
                    let n = image_size.min(seg.size - off);
                    if n > 0 {
                        let dst = unsafe { std::slice::from_raw_parts_mut(seg.ptr.add(off), n) };
                        self.server.framebuffer.read_rect_into(
                            r.x.into(),
                            r.y.into(),
                            r.width.into(),
                            r.height.into(),
                            dst,
                        );
                    }
                }
                self.reply(&shm::GetImageReply {
                    depth: ROOT_DEPTH,
                    sequence: 0,
                    length: 0,
                    visual: ROOT_VISUAL,
                    size: image_size as u32,
                })?;
            }
            Request::ShmCreateSegment(r) => {
                // MIT-SHM 1.2, which ShmQueryVersion advertises: the server
                // makes the segment and hands the client an fd for it
                // (imlib2, so scrot, asks this first; without it, it fell
                // back to GetImage)
                if r.size == 0 || r.size > super::mit_shm::MAX_CREATED_SEGMENT {
                    let code = if r.size == 0 {
                        xproto::VALUE_ERROR
                    } else {
                        xproto::ALLOC_ERROR
                    };
                    return self.send_error_value(code, r.size, major, u16::from(minor));
                }
                match self.shm.create(r.shmseg, r.size as usize) {
                    Ok(fd) => {
                        // reply, nfd = 1, no data beyond the 32 bytes
                        let mut buf = [0u8; 32];
                        buf[0] = 1;
                        buf[1] = 1;
                        self.client.send_reply_fd(self.seq, &mut buf, fd)?;
                    }
                    Err(e) => {
                        crate::warning!("can't create a {}-byte shared segment: {e}", r.size);
                        return self.send_error(xproto::ALLOC_ERROR, major, u16::from(minor));
                    }
                }
            }
            Request::ShmCreatePixmap(_) | Request::ShmAttachFd(_) => {}
            Request::QueryBestSize(r) => {
                self.reply(&xproto::QueryBestSizeReply {
                    sequence: 0,
                    length: 0,
                    width: r.width,
                    height: r.height,
                })?;
            }
            // --- fonts (we have no real fonts; report a fixed 6x13 metric) ---
            Request::QueryFont(_) => {
                let glyph = xproto::Charinfo {
                    left_side_bearing: 0,
                    right_side_bearing: 6,
                    character_width: 6,
                    ascent: 11,
                    descent: 2,
                    attributes: 0,
                };
                self.reply(&xproto::QueryFontReply {
                    sequence: 0,
                    length: 0,
                    min_bounds: glyph,
                    max_bounds: glyph,
                    min_char_or_byte2: 32,
                    max_char_or_byte2: 126,
                    default_char: 32,
                    draw_direction: xproto::FontDraw::LEFT_TO_RIGHT,
                    min_byte1: 0,
                    max_byte1: 0,
                    all_chars_exist: true,
                    font_ascent: 11,
                    font_descent: 2,
                    properties: vec![],
                    char_infos: vec![],
                })?;
            }
            Request::QueryTextExtents(r) => {
                let n = (r.string.len() * 6) as i32;
                self.reply(&xproto::QueryTextExtentsReply {
                    draw_direction: xproto::FontDraw::LEFT_TO_RIGHT,
                    sequence: 0,
                    length: 0,
                    font_ascent: 11,
                    font_descent: 2,
                    overall_ascent: 11,
                    overall_descent: 2,
                    overall_width: n,
                    overall_left: 0,
                    overall_right: n,
                })?;
            }
            Request::QueryColors(r) => {
                // TrueColor: the pixel value directly encodes the channels.
                let colors = r
                    .pixels
                    .iter()
                    .map(|&p| xproto::Rgb {
                        red: (((p >> 16) & 0xff) * 257) as u16,
                        green: (((p >> 8) & 0xff) * 257) as u16,
                        blue: ((p & 0xff) * 257) as u16,
                    })
                    .collect();
                self.reply(&xproto::QueryColorsReply {
                    sequence: 0,
                    length: 0,
                    colors,
                })?;
            }
            Request::ListInstalledColormaps(_) => {
                self.reply(&xproto::ListInstalledColormapsReply {
                    sequence: 0,
                    length: 0,
                    cmaps: vec![ROOT_COLORMAP],
                })?;
            }

            // --- grabs (always succeed; we own all input) ---
            Request::GrabPointer(_) => {
                self.reply(&xproto::GrabPointerReply {
                    status: xproto::GrabStatus::SUCCESS,
                    sequence: 0,
                    length: 0,
                })?;
            }
            Request::GrabKeyboard(_) => {
                self.reply(&xproto::GrabKeyboardReply {
                    status: xproto::GrabStatus::SUCCESS,
                    sequence: 0,
                    length: 0,
                })?;
            }
            // ... and so does letting go of them
            Request::UngrabPointer(_)
            | Request::UngrabKeyboard(_)
            | Request::ChangeActivePointerGrab(_)
            | Request::AllowEvents(_) => {}
            // Passive grabs (hotkey daemons: xbindkeys, sxhkd; push-to-talk)
            // are accepted and never fire: the desktop's keys and buttons
            // don't come through this server. Answering BadImplementation
            // killed them at start-up instead.
            Request::GrabKey(_)
            | Request::UngrabKey(_)
            | Request::GrabButton(_)
            | Request::UngrabButton(_) => {}

            // --- bell and screen saver (the desktop's to do) ---
            Request::Bell(_) => crate::bridge::bell::ring(),
            Request::SetScreenSaver(r) => {
                if let Err(value) = super::saver::set(
                    r.timeout,
                    r.interval,
                    r.prefer_blanking,
                    r.allow_exposures,
                ) {
                    return self.send_error_value(xproto::VALUE_ERROR, value, major, 0);
                }
            }
            Request::ForceScreenSaver(_) => {
                // no-op: the desktop's own idle handling decides, and there
                // is no saver here to start or reset (see `saver`)
            }

            // --- XTest: the whole point ---
            Request::XtestGetVersion(_) => {
                self.reply(&xtest::GetVersionReply {
                    major_version: 2,
                    sequence: 0,
                    length: 0,
                    minor_version: 2,
                })?;
            }
            Request::XtestCompareCursor(_) => {
                self.reply(&xtest::CompareCursorReply {
                    same: true,
                    sequence: 0,
                    length: 0,
                })?;
            }
            Request::XtestFakeInput(r) => self.fake_input(&r),
            Request::XtestGrabControl(_) => {}

            // --- extension version negotiation ---
            Request::BigreqEnable(_) => {
                self.reply(&bigreq::EnableReply {
                    sequence: 0,
                    length: 0,
                    maximum_request_length: MAX_REQUEST_LENGTH,
                })?;
            }
            Request::RandrQueryVersion(_) => {
                self.reply(&randr::QueryVersionReply {
                    sequence: 0,
                    length: 0,
                    major_version: 1,
                    minor_version: 6,
                })?;
            }
            Request::RandrGetScreenSizeRange(_) => {
                self.reply(&randr::GetScreenSizeRangeReply {
                    sequence: 0,
                    length: 0,
                    min_width: 8,
                    min_height: 8,
                    max_width: randr_screen::MAX_SCREEN,
                    max_height: randr_screen::MAX_SCREEN,
                })?;
            }
            Request::RandrGetScreenResources(_) => {
                let (timestamp, config_timestamp, crtcs, outputs, modes, names) =
                    self.screen_resources();
                self.reply(&randr::GetScreenResourcesReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    config_timestamp,
                    crtcs,
                    outputs,
                    modes,
                    names,
                })?;
            }
            Request::RandrGetScreenResourcesCurrent(_) => {
                let (timestamp, config_timestamp, crtcs, outputs, modes, names) =
                    self.screen_resources();
                self.reply(&randr::GetScreenResourcesCurrentReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    config_timestamp,
                    crtcs,
                    outputs,
                    modes,
                    names,
                })?;
            }
            Request::RandrGetScreenInfo(_) => {
                // RandR 1.0/1.1 single-screen view: report the whole virtual
                // screen as one configurable size at one refresh rate. (Modern
                // tools use GetScreenResources; this is for older ones.)
                let (width, height, timestamp, config_timestamp) = {
                    let s = self.server.screen.lock().unwrap();
                    (s.width, s.height, s.timestamp, s.config_timestamp)
                };
                // Same px->mm approximation as ScreenChangeNotify.
                let mwidth = (u32::from(width) * 254 / 960) as u16;
                let mheight = (u32::from(height) * 254 / 960) as u16;
                self.reply(&randr::GetScreenInfoReply {
                    rotations: randr::Rotation::ROTATE0,
                    sequence: 0,
                    length: 0,
                    root: ROOT_WINDOW,
                    timestamp,
                    config_timestamp,
                    size_id: 0,
                    rotation: randr::Rotation::ROTATE0,
                    rate: 60,
                    n_info: 2, // = n_sizes + rates.len()
                    sizes: vec![randr::ScreenSize {
                        width,
                        height,
                        mwidth,
                        mheight,
                    }],
                    rates: vec![randr::RefreshRates { rates: vec![60] }],
                })?;
            }
            Request::RandrGetOutputInfo(r) => {
                let reply = {
                    let s = self.server.screen.lock().unwrap();
                    match s.output_by_id(r.output) {
                        Some(o) => randr::GetOutputInfoReply {
                            status: randr::SetConfig::SUCCESS,
                            sequence: 0,
                            length: 0,
                            timestamp: s.timestamp,
                            crtc: if o.mode != 0 { o.crtc_id } else { 0 },
                            mm_width: o.mm_width,
                            mm_height: o.mm_height,
                            connection: if o.connected {
                                randr::Connection::CONNECTED
                            } else {
                                randr::Connection::DISCONNECTED
                            },
                            subpixel_order: render::SubPixel::UNKNOWN,
                            num_preferred: 1,
                            crtcs: vec![o.crtc_id],
                            modes: o.mode_ids.clone(),
                            clones: vec![],
                            name: o.name.clone(),
                        },
                        None => empty_output_info(s.timestamp),
                    }
                };
                self.reply(&reply)?;
            }
            Request::RandrGetCrtcInfo(r) => {
                let reply = {
                    let s = self.server.screen.lock().unwrap();
                    match s.output_by_crtc(r.crtc) {
                        Some(o) => randr::GetCrtcInfoReply {
                            status: randr::SetConfig::SUCCESS,
                            sequence: 0,
                            length: 0,
                            timestamp: s.timestamp,
                            x: o.x,
                            y: o.y,
                            width: o.width,
                            height: o.height,
                            mode: o.mode,
                            rotation: randr::Rotation::ROTATE0,
                            rotations: randr::Rotation::ROTATE0,
                            outputs: vec![o.output_id],
                            possible: vec![o.output_id],
                        },
                        None => empty_crtc_info(s.timestamp),
                    }
                };
                self.reply(&reply)?;
            }
            Request::RandrGetPanning(_) => {
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::GetPanningReply {
                    status: randr::SetConfig::SUCCESS,
                    sequence: 0,
                    length: 0,
                    timestamp,
                    left: 0,
                    top: 0,
                    width: 0,
                    height: 0,
                    track_left: 0,
                    track_top: 0,
                    track_width: 0,
                    track_height: 0,
                    border_left: 0,
                    border_top: 0,
                    border_right: 0,
                    border_bottom: 0,
                })?;
            }
            Request::RandrGetOutputPrimary(_) => {
                let output = self.server.screen.lock().unwrap().primary_output();
                self.reply(&randr::GetOutputPrimaryReply {
                    sequence: 0,
                    length: 0,
                    output,
                })?;
            }
            Request::RandrGetCrtcGammaSize(_) => {
                self.reply(&randr::GetCrtcGammaSizeReply {
                    sequence: 0,
                    length: 0,
                    size: randr_screen::GAMMA_SIZE,
                })?;
            }
            Request::RandrGetCrtcGamma(r) => {
                let [red, green, blue] = self.server.screen.lock().unwrap().gamma(r.crtc);
                self.reply(&randr::GetCrtcGammaReply {
                    sequence: 0,
                    length: 0,
                    red,
                    green,
                    blue,
                })?;
            }
            Request::RandrSetCrtcGamma(r) => {
                // kept to be read back, never applied: the desktop owns the
                // colours (night light), see `Screen::set_gamma`
                let result = self
                    .server
                    .screen
                    .lock()
                    .unwrap()
                    .set_gamma(r.crtc, &r.red, &r.green, &r.blue);
                match result {
                    Ok(()) => {}
                    Err(randr_screen::GammaError::Size) => {
                        return self.send_error(xproto::MATCH_ERROR, major, u16::from(minor));
                    }
                    Err(randr_screen::GammaError::UnknownCrtc) => {
                        let code = randr_error(randr::BAD_CRTC_ERROR);
                        return self.send_error_value(code, r.crtc, major, u16::from(minor));
                    }
                }
            }
            Request::RandrGetCrtcTransform(_) => {
                self.reply(&randr::GetCrtcTransformReply {
                    sequence: 0,
                    length: 0,
                    pending_transform: identity_transform(),
                    has_transforms: false,
                    current_transform: identity_transform(),
                    pending_filter_name: vec![],
                    pending_params: vec![],
                    current_filter_name: vec![],
                    current_params: vec![],
                })?;
            }
            Request::RandrListOutputProperties(_) => {
                self.reply(&randr::ListOutputPropertiesReply {
                    sequence: 0,
                    length: 0,
                    atoms: vec![],
                })?;
            }
            Request::RandrGetOutputProperty(_) => {
                self.reply(&randr::GetOutputPropertyReply {
                    format: 0,
                    sequence: 0,
                    length: 0,
                    type_: 0,
                    bytes_after: 0,
                    num_items: 0,
                    data: vec![],
                })?;
            }
            Request::RandrQueryOutputProperty(_) => {
                self.reply(&randr::QueryOutputPropertyReply {
                    sequence: 0,
                    pending: false,
                    range: false,
                    immutable: true,
                    valid_values: vec![],
                })?;
            }
            Request::RandrGetProviders(_) => {
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::GetProvidersReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    providers: vec![],
                })?;
            }
            Request::RandrGetMonitors(_) => {
                // Xorg's automatic monitors: one per active output, named
                // after it, the primary output's marked primary
                let (timestamp, mons) = {
                    let s = self.server.screen.lock().unwrap();
                    (s.timestamp, s.monitors())
                };
                let monitors: Vec<randr::MonitorInfo> = {
                    let mut atoms = self.server.atoms.lock().unwrap();
                    mons.iter()
                        .map(|m| randr::MonitorInfo {
                            name: atoms.intern(&m.name, false),
                            primary: m.primary,
                            automatic: true,
                            x: m.x,
                            y: m.y,
                            width: m.width,
                            height: m.height,
                            width_in_millimeters: m.mm_width,
                            height_in_millimeters: m.mm_height,
                            outputs: vec![m.output_id],
                        })
                        .collect()
                };
                self.reply(&randr::GetMonitorsReply {
                    sequence: 0,
                    length: 0,
                    timestamp,
                    n_outputs: monitors.len() as u32,
                    monitors,
                })?;
            }
            Request::RandrCreateMode(r) => {
                let mode = self
                    .server
                    .screen
                    .lock()
                    .unwrap()
                    .create_mode(r.mode_info, r.name.into_owned());
                self.reply(&randr::CreateModeReply {
                    sequence: 0,
                    length: 0,
                    mode,
                })?;
            }
            Request::RandrAddOutputMode(r) => {
                self.server
                    .screen
                    .lock()
                    .unwrap()
                    .add_output_mode(r.output, r.mode);
            }
            Request::RandrDeleteOutputMode(r) => {
                self.server
                    .screen
                    .lock()
                    .unwrap()
                    .delete_output_mode(r.output, r.mode);
            }
            Request::RandrDestroyMode(r) => {
                self.server.screen.lock().unwrap().destroy_mode(r.mode);
            }
            Request::RandrSetCrtcConfig(r) => {
                let status = self.set_crtc_config(&r);
                let timestamp = self.server.screen.lock().unwrap().timestamp;
                self.reply(&randr::SetCrtcConfigReply {
                    status,
                    sequence: 0,
                    length: 0,
                    timestamp,
                })?;
            }
            Request::RandrSetScreenSize(r) => {
                // The screen is the client's to size (the CRTCs must fit, which
                // is not enforced: a client that shrinks the screen before its
                // CRTC just gets clipped until it does), and the framebuffer,
                // the input mapping and the capture positions follow.
                if r.width == 0
                    || r.height == 0
                    || r.width > randr_screen::MAX_SCREEN
                    || r.height > randr_screen::MAX_SCREEN
                {
                    return self.send_error(xproto::VALUE_ERROR, major, u16::from(minor));
                }
                crate::vlog!("client set the screen size to {}x{}", r.width, r.height);
                self.server
                    .screen
                    .lock()
                    .unwrap()
                    .set_size(r.width, r.height);
                self.server.sync_screen(true);
            }
            Request::RandrSelectInput(r) => {
                // We only support ScreenChange; record the window so the event
                // sink can deliver RRScreenChangeNotify.
                let enabled = r.enable.contains(randr::NotifyMask::SCREEN_CHANGE);
                self.client.select_randr(if enabled { r.window } else { 0 });
            }
            Request::DamageQueryVersion(_) => {
                self.reply(&damage::QueryVersionReply {
                    sequence: 0,
                    length: 0,
                    major_version: 1,
                    minor_version: 1,
                })?;
            }
            Request::XfixesQueryVersion(_) => {
                self.reply(&xfixes::QueryVersionReply {
                    sequence: 0,
                    length: 0,
                    major_version: 5,
                    minor_version: 0,
                })?;
            }

            // --- DAMAGE ---
            Request::DamageCreate(r) => {
                if self.server.config.damage() {
                    crate::bridge::profile::damage_create(u8::from(r.level));
                    self.server.damage.create(
                        self.client.clone(),
                        r.damage,
                        r.drawable,
                        u8::from(r.level),
                    );
                }
            }
            Request::DamageDestroy(r) => {
                self.server.damage.destroy(&self.client, r.damage);
            }
            Request::DamageSubtract(r) => {
                crate::bridge::profile::damage_subtract();
                let repair = (r.repair != 0).then(|| self.regions.get(r.repair));
                let parts = self
                    .server
                    .damage
                    .subtract(&self.client, r.damage, repair.as_deref());
                if r.parts != 0 {
                    self.regions.set(r.parts, parts);
                }
            }

            // --- XFixes regions (enough for the DAMAGE -> region -> fetch flow) ---
            Request::XfixesCreateRegion(r) => {
                self.regions.set(r.region, r.rectangles.into_owned());
            }
            Request::XfixesSetRegion(r) => {
                self.regions.set(r.region, r.rectangles.into_owned());
            }
            Request::XfixesDestroyRegion(r) => {
                self.regions.destroy(r.region);
            }
            Request::XfixesCopyRegion(r) => {
                self.regions.copy(r.source, r.destination);
            }
            Request::XfixesRegionExtents(r) => {
                self.regions.extents(r.source, r.destination);
            }
            Request::XfixesGetCursorImage(_) => {
                let c = self.server.cursor.snapshot();
                let (w, h, image) = if c.serial == 0 {
                    (1, 1, vec![0u32])
                } else {
                    (c.width, c.height, c.image)
                };
                self.reply(&xfixes::GetCursorImageReply {
                    sequence: 0,
                    length: 0,
                    x: c.x,
                    y: c.y,
                    width: w,
                    height: h,
                    xhot: c.xhot,
                    yhot: c.yhot,
                    cursor_serial: c.serial.max(1),
                    cursor_image: image,
                })?;
            }
            Request::XfixesGetCursorImageAndName(_) => {
                let c = self.server.cursor.snapshot();
                let (w, h, image) = if c.serial == 0 {
                    (1, 1, vec![0u32])
                } else {
                    (c.width, c.height, c.image)
                };
                self.reply(&xfixes::GetCursorImageAndNameReply {
                    sequence: 0,
                    length: 0,
                    x: c.x,
                    y: c.y,
                    width: w,
                    height: h,
                    xhot: c.xhot,
                    yhot: c.yhot,
                    cursor_serial: c.serial.max(1),
                    cursor_atom: 0,
                    cursor_image: image,
                    name: vec![],
                })?;
            }
            Request::XfixesGetCursorName(_) => {
                self.reply(&xfixes::GetCursorNameReply {
                    sequence: 0,
                    length: 0,
                    atom: 0,
                    name: vec![],
                })?;
            }
            Request::XfixesGetClientDisconnectMode(_) => {
                self.reply(&xfixes::GetClientDisconnectModeReply {
                    sequence: 0,
                    length: 0,
                    disconnect_mode: xfixes::ClientDisconnectFlags::from(0u32),
                })?;
            }
            Request::XfixesFetchRegion(r) => {
                let rects = self.regions.get(r.region);
                let extents = bbox(&rects).unwrap_or(xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                });
                self.reply(&xfixes::FetchRegionReply {
                    sequence: 0,
                    extents,
                    rectangles: rects,
                })?;
            }
            Request::ShmQueryVersion(_) => {
                self.reply(&shm::QueryVersionReply {
                    shared_pixmaps: false,
                    sequence: 0,
                    length: 0,
                    major_version: 1,
                    minor_version: 2,
                    uid: 0,
                    gid: 0,
                    pixmap_format: 2, // ZPixmap
                })?;
            }

            // --- window attributes: we only track the event mask, to know which
            // windows want PropertyNotify (vncagent's clipboard needs it) ---
            Request::CreateWindow(r) => {
                if let Some(mask) = r.value_list.event_mask {
                    self.windows.set_mask(r.wid, mask);
                    self.track_root_structure(r.wid, mask);
                    self.windows.warn_unsupported(mask);
                }
            }
            Request::ChangeWindowAttributes(r) => {
                if let Some(mask) = r.value_list.event_mask {
                    self.windows.set_mask(r.window, mask);
                    self.track_root_structure(r.window, mask);
                    self.windows.warn_unsupported(mask);
                }
            }
            Request::DestroyWindow(r) => {
                self.windows.remove(r.window);
                // and with it its XFixes registrations and any selection it
                // owned, which reverts to nobody
                self.client.forget_window(r.window);
                for (atom, last_changed) in self.selection.forget_window(r.window) {
                    let regs = self
                        .client
                        .selections_for(atom, xfixes::SelectionEvent::SELECTION_WINDOW_DESTROY);
                    crate::bridge::event::selection_notify(
                        &self.client,
                        &regs,
                        xfixes::SelectionEvent::SELECTION_WINDOW_DESTROY,
                        0,
                        crate::bridge::event::server_time_ms(),
                        last_changed,
                    );
                }
                self.selection_owner_gone(
                    Some(r.window),
                    xfixes::SelectionEvent::SELECTION_WINDOW_DESTROY,
                );
            }
            Request::XfixesSelectCursorInput(r) => {
                let want = u32::from(r.event_mask)
                    & u32::from(xfixes::CursorNotifyMask::DISPLAY_CURSOR)
                    != 0;
                self.client.select_cursor(if want { r.window } else { 0 });
            }

            Request::ChangeKeyboardControl(r) => {
                if let Some(m) = r.value_list.auto_repeat_mode
                    && m == AutoRepeatMode::OFF
                {
                    crate::fixme!(
                        "keyboard auto-repeat cannot be disabled with current wayland interfaces"
                    )
                }
                // we don't care about bells or lights
            }

            Request::MapWindow(_) | Request::ConfigureWindow(_) | Request::UnmapWindow(_) => {
                // no-op: we don't support windows
            }
            Request::WarpPointer(r) => self.warp_pointer(&r),
            Request::GrabServer(_) | Request::UngrabServer(_) => {
                // no-op: there are no other clients to freeze out of a
                // reconfiguration (xrandr and vncagent bracket their RandR
                // changes in these), and nothing we do is affected by a grab
            }
            Request::CreateGC(_) | Request::ChangeGC(_) | Request::FreeGC(_) => {
                // no-op: we don't support drawing
            }
            // Offscreen pixmaps and drawing into them: accepted and dropped like
            // the GC requests above. RealVNC 6.11's vncserverui is a GTK 2 app,
            // and GTK aborts on any X error, so an error here takes down the
            // process that also routes RealVNC's clipboard.
            Request::CreatePixmap(_)
            | Request::FreePixmap(_)
            | Request::PutImage(_)
            | Request::CopyArea(_)
            | Request::CopyPlane(_)
            | Request::PolyPoint(_)
            | Request::PolyLine(_)
            | Request::PolySegment(_)
            | Request::PolyRectangle(_)
            | Request::PolyArc(_)
            | Request::FillPoly(_)
            | Request::PolyFillRectangle(_)
            | Request::PolyFillArc(_)
            | Request::ClearArea(_) => {}
            Request::OpenFont(_) => {
                // no-op: we don't support fonts
            }

            other => {
                // Anything not handled above is potentially something we need
                // to implement: fail it with a BadImplementation error and log
                // it as FIXME, and reply with an error if the request wants a
                // reply (so libxcb doesn't hang).
                let replied = other.reply_parser().is_some();
                self.send_error(xproto::IMPLEMENTATION_ERROR, major, u16::from(minor))?;
                crate::fixme!(
                    "request {major}.{minor} ({other:?}) is unhandled and needs a stub or implementation, sent error{}",
                    if replied {
                        " (instead of a reply)"
                    } else {
                        " (no reply expected)"
                    },
                );
            }
        }
        Ok(())
    }

    /// Builds the `GetProperty` reply, and whether the property was deleted as
    /// a result (`delete` set and the whole remainder returned), in which case
    /// the caller sends the PropertyNotify.
    fn get_property(&mut self, r: &xproto::GetPropertyRequest) -> (xproto::GetPropertyReply, bool) {
        let empty = xproto::GetPropertyReply {
            format: 0,
            sequence: 0,
            length: 0,
            type_: 0,
            bytes_after: 0,
            value_len: 0,
            value: vec![],
        };
        // worked out before taking the properties lock, which it would nest
        // the atom and keymap locks inside
        let rules = self.xkb_rules_property(r.window, r.property);
        let mut properties = self.properties.lock().unwrap();
        let (p, synthesized) = match (properties.get(r.window, r.property), &rules) {
            (Some(p), _) => (p, false),
            (None, Some(rules)) => (rules, true),
            (None, None) => return (empty, false),
        };
        // a type mismatch (with a specific type asked for) reports what the
        // property is, with no value and nothing deleted
        if r.type_ != 0 && r.type_ != p.type_ {
            let reply = xproto::GetPropertyReply {
                format: p.format,
                type_: p.type_,
                bytes_after: p.data.len() as u32,
                ..empty
            };
            return (reply, false);
        }
        let unit = (p.format / 8).max(1) as usize;
        let start = (r.long_offset as usize * 4).min(p.data.len());
        let want = (r.long_length as usize * 4).min(p.data.len() - start);
        // align to the property unit size
        let want = want - (want % unit);
        let value = p.data[start..start + want].to_vec();
        let bytes_after = (p.data.len() - start - want) as u32;
        let reply = xproto::GetPropertyReply {
            format: p.format,
            sequence: 0,
            length: 0,
            type_: p.type_,
            bytes_after,
            value_len: (value.len() / unit) as u32,
            value,
        };
        let deleted = r.delete && bytes_after == 0 && !synthesized;
        if deleted {
            properties.remove(r.window, r.property);
        }
        (reply, deleted)
    }

    /// `WarpPointer`. The pointer is the desktop's, so warping it moves it, as
    /// an XTest motion would (`xdotool mousemove`), through the same input
    /// path (`-viewonly` and the pointer-ownership policy apply): to
    /// `dst_x,dst_y` in `dst_window`, or by that offset with no `dst_window`.
    /// Every window of this server sits at the root's origin with the root's
    /// size (`GetGeometry`, `TranslateCoordinates`), so window coordinates
    /// are root coordinates. With a `src_window`, the move happens only if
    /// the pointer is inside the given rectangle of it (a width or height of
    /// 0 reaching to the window's edge), as X specifies.
    fn warp_pointer(&self, r: &xproto::WarpPointerRequest) {
        if r.src_window != 0 {
            let p = self.server.cursor.snapshot();
            if !warp_source_holds(r, (p.x, p.y), self.server.geometry()) {
                return;
            }
        }
        let relative = u8::from(r.dst_window == 0);
        self.server.input.fake_input(
            self.id,
            xproto::MOTION_NOTIFY_EVENT,
            relative,
            r.dst_x,
            r.dst_y,
        );
    }

    /// `ChangeKeyboardMapping`: keysyms on spare keycodes (ones the
    /// compositor keyboard leaves empty), as xdotool, x11vnc and pynput put
    /// a keysym they can't find there to type it; pressing one types the
    /// keysym where the compositor keyboard has it ([`Self::fake_input`]).
    /// Changing a keycode the compositor uses is refused (BadImplementation):
    /// its keyboard can't be changed from X.
    fn change_keyboard_mapping(
        &mut self,
        r: &xproto::ChangeKeyboardMappingRequest,
    ) -> io::Result<()> {
        let major = xproto::CHANGE_KEYBOARD_MAPPING_REQUEST;
        let last = usize::from(r.first_keycode) + usize::from(r.keycode_count);
        if r.first_keycode < crate::bridge::keymap::MIN_KEYCODE
            || last > usize::from(crate::bridge::keymap::MAX_KEYCODE) + 1
            || r.keysyms_per_keycode == 0
        {
            return self.send_error_value(
                xproto::VALUE_ERROR,
                u32::from(r.first_keycode),
                major,
                0,
            );
        }
        let legacy = !self.server.config.xkb();
        let result = match self.server.keymap.lock().unwrap().as_mut() {
            Some(table) => table
                .change_mapping(r.first_keycode, r.keysyms_per_keycode, &r.keysyms, legacy)
                .map(|changed| changed.then(|| table.xkb.types.len() as u8)),
            // no keyboard yet: every keycode is spare, but nothing types
            None => Ok(None),
        };
        match result {
            Ok(Some(xkb_types)) => {
                self.server.events.keyboard_mapping_changed(
                    r.first_keycode,
                    r.keycode_count,
                    xkb_types,
                );
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(crate::bridge::keymap::MappingRefused::InUse(kc)) => {
                if !std::mem::replace(&mut self.mapping_refusal_logged, true) {
                    crate::warning!(
                        "[c{}] client asked to change keycode {kc}, which the compositor keyboard \
                         uses; it can't be changed from X, sent BadImplementation (not logged again \
                         for this client)",
                        self.id
                    );
                }
                self.send_error(xproto::IMPLEMENTATION_ERROR, major, 0)
            }
        }
    }

    /// A screen read while capture is stopped starts it and waits for its
    /// first frame (`-coldwait`), so a one-shot reader (scrot, xwd, Java's
    /// Robot) gets the desktop as it is, not black or an old picture. Only
    /// this client waits; its later reads find capture running.
    fn await_fresh(&mut self) {
        use crate::bridge::fresh::Wait;
        self.active = true;
        match self.server.framebuffer.await_fresh(self.server.config.coldwait()) {
            Wait::NotNeeded => {}
            Wait::Fresh(d) => crate::vlog!("[c{}] screen read held {} ms until capture was live", self.id, d.as_millis()),
            Wait::TimedOut => crate::warning!(
                "[c{}] capture didn't start within {} ms; answering a screen read with the last frame",
                self.id,
                self.server.config.coldwait().as_millis()
            ),
        }
    }

    /// An XTest event. A key on a spare keycode the client put a keysym on
    /// becomes that keysym's key on the compositor keyboard, with the
    /// modifiers its level needs held around it (or, for a keysym only
    /// another layout has, that layout's key as part of a shortcut), the way
    /// [`keymap::Synth`](crate::bridge::keymap::Synth) and
    /// [`KeyTable::plan`](crate::bridge::keymap::KeyTable::plan) decide for
    /// every engine; everything else goes through as it is.
    fn fake_input(&mut self, r: &xtest::FakeInputRequest) {
        if let Some(name) = &self.engine_helper {
            if !std::mem::replace(&mut self.helper_logged, true) {
                crate::log!(
                    "input from X client {} ({name}) stays on this display: \
                     it drives windows here (an engine helper, -xtesthelpers)",
                    self.id
                );
            }
            return;
        }
        let input = self.server.input.clone();
        let press = r.type_ == xproto::KEY_PRESS_EVENT;
        if press || r.type_ == xproto::KEY_RELEASE_EVENT {
            let id = self.id;
            let shortcut = press && input.shortcut_held_or_queued();
            // (Input never takes the keymap lock, so holding it across
            // `holds` can't deadlock)
            let keymap = self.server.keymap.lock().unwrap();
            let events = self.synth.key(
                press,
                r.detail,
                || keymap.as_ref()?.overrides.get(&r.detail)?.first().copied(),
                |m| input.holds(id, m),
                keymap.as_ref(),
                shortcut,
                &format_args!("[c{id}]"),
            );
            drop(keymap);
            if let Some(events) = events {
                for (down, kc) in events {
                    let type_ = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
                    input.fake_input(self.id, type_, kc, 0, 0);
                }
                return;
            }
        }
        input.fake_input(self.id, r.type_, r.detail, r.root_x, r.root_y);
    }

    /// Whether `QueryExtension`/`ListExtensions` show the extension: DAMAGE
    /// and XKEYBOARD can be turned off (`-nodamage`, `-noxkb`).
    fn advertised(&self, e: &super::ext::ExtDef) -> bool {
        (self.server.config.damage() || e.name != "DAMAGE")
            && (self.server.config.xkb() || e.name != xkb::NAME)
    }

    /// The XKB description served: the compositor keymap's, or until there is
    /// one, a keyboard with no keysyms (as the core mapping then has).
    fn xkb_desc(&self) -> Arc<Desc> {
        if let Some(table) = self.server.keymap.lock().unwrap().as_ref() {
            return table.xkb.clone();
        }
        static NO_KEYMAP: std::sync::OnceLock<Arc<Desc>> = std::sync::OnceLock::new();
        NO_KEYMAP
            .get_or_init(|| Arc::new(Desc::without_keymap(&DEFAULT_MODIFIER_MAP)))
            .clone()
    }

    /// The root window's `_XKB_RULES_NAMES` (rules, model, layout, variant,
    /// options), which an X server with XKB publishes and `setxkbmap -query`
    /// and `-print` read: derived from the compositor keymap, never stored.
    fn xkb_rules_property(&self, window: u32, property: u32) -> Option<Property> {
        if window != ROOT_WINDOW || !self.server.config.xkb() {
            return None;
        }
        if self.atom_name(property)?.as_slice() != xkb::RULES_NAMES_PROPERTY.as_bytes() {
            return None;
        }
        let rules = self.xkb_desc().rules.clone()?;
        Some(Property {
            type_: XA_STRING,
            format: 8,
            data: rules.property(),
        })
    }

    /// Whether a warning about XKB request `minor` is still to be logged for
    /// this client (true the first time only).
    fn xkb_warn_once(&mut self, minor: u8) -> bool {
        let (word, bit) = (usize::from(minor / 64), 1u64 << (minor % 64));
        let first = self.xkb_warned[word] & bit == 0;
        self.xkb_warned[word] |= bit;
        first
    }

    /// An XKEYBOARD request (see [`xkb`]).
    fn dispatch_xkb(&mut self, major: u8, minor: u8, body: &[u8]) -> io::Result<()> {
        let Some(req) = xkb::parse(minor, body) else {
            if self.xkb_warn_once(minor) {
                crate::warning!(
                    "[c{}] failed to parse XKB request {} ({major}.{minor}), sent BadLength \
                     (not logged again for this client)",
                    self.id,
                    xkb::request_name(minor)
                );
            }
            return self.send_error(xproto::LENGTH_ERROR, major, u16::from(minor));
        };
        if self.server.config.xtrace {
            warning!(
                "xtrace: [c{}] {major}.{minor}: Xkb{} {req:?}",
                self.id,
                xkb::request_name(minor)
            );
        }
        let desc = self.xkb_desc();
        let reply = match req {
            xkb::Request::UseExtension { .. } => xkb::use_extension(),
            xkb::Request::SelectEvents(select) => {
                self.client.select_xkb(&select);
                return Ok(());
            }
            xkb::Request::GetState => xkb::get_state(),
            xkb::Request::GetControls => xkb::get_controls(&desc),
            xkb::Request::GetMap(r) => xkb::get_map(&desc, &r),
            xkb::Request::GetCompatMap {
                groups,
                all,
                first,
                count,
            } => xkb::get_compat_map(groups, all, first, count),
            xkb::Request::GetIndicatorState => xkb::get_indicator_state(),
            xkb::Request::GetIndicatorMap { which } => xkb::get_indicator_map(&desc, which),
            xkb::Request::GetNamedIndicator { indicator } => {
                let name = self.atom_name(indicator);
                let name = name.as_deref().and_then(|n| std::str::from_utf8(n).ok());
                xkb::get_named_indicator(&desc, indicator, name)
            }
            xkb::Request::GetNames { which } => {
                let mut intern = |name: &str| self.intern(name.as_bytes());
                xkb::get_names(&desc, which, &mut intern)
            }
            xkb::Request::GetGeometry { name } => xkb::get_geometry(name),
            xkb::Request::PerClientFlags { change, value } => {
                self.xkb_flags = xkb::per_client_value(self.xkb_flags, change, value);
                xkb::per_client_flags(self.xkb_flags)
            }
            xkb::Request::ListComponents => xkb::list_components(),
            xkb::Request::GetKbdByName { load } => {
                if load && self.xkb_warn_once(minor) {
                    crate::warning!(
                        "[c{}] client asked to load a new XKB keymap; the keyboard is the \
                         compositor's, so nothing is loaded (not logged again for this client)",
                        self.id
                    );
                }
                xkb::get_kbd_by_name()
            }
            xkb::Request::GetDeviceInfo {
                first_button,
                n_buttons,
            } => xkb::get_device_info(first_button, n_buttons),
            xkb::Request::SetDebuggingFlags => xkb::set_debugging_flags(),
            xkb::Request::Ignored(m) => {
                // XkbBell (GTK's gdk_display_beep, xkbbell) rings like the
                // core Bell, unless it asks for the event only (eventOnly,
                // request byte 12)
                if m == xkb::op::BELL && body.get(8) == Some(&0) {
                    crate::bridge::bell::ring();
                }
                return Ok(());
            }
            xkb::Request::Refused(m) => {
                if self.xkb_warn_once(m) {
                    crate::warning!(
                        "[c{}] client asked to change the keyboard with Xkb{}; it is the \
                         compositor's (read-only), sent BadImplementation (not logged again for \
                         this client)",
                        self.id,
                        xkb::request_name(m)
                    );
                }
                return self.send_error(xproto::IMPLEMENTATION_ERROR, major, u16::from(minor));
            }
            xkb::Request::Unknown(m) => {
                if self.xkb_warn_once(m) {
                    crate::fixme!(
                        "[c{}] unknown XKB request {major}.{m}, sent BadRequest (not logged again \
                         for this client)",
                        self.id
                    );
                }
                return self.send_error(xproto::REQUEST_ERROR, major, u16::from(minor));
            }
        };
        let mut buf = xkb::finish(reply);
        self.client.send_reply(self.seq, &mut buf)
    }

    /// `RRSetCrtcConfig`. Disabling a CRTC, or giving it a mode the output is
    /// already running at, only touches the model. Any other mode needs the
    /// compositor to change the output, which `-dynres` carries over and waits
    /// for (so the reply is truthful and the model already reflects what
    /// happened); without it the request fails, since claiming a size the
    /// capture will never have would leave the client reading a screen that
    /// does not exist.
    fn set_crtc_config(&self, r: &randr::SetCrtcConfigRequest) -> randr::SetConfig {
        enum Plan {
            Local,
            Compositor {
                wl_name: u32,
                name: String,
                width: u16,
                height: u16,
            },
        }
        let plan = {
            let s = self.server.screen.lock().unwrap();
            let Some(o) = s.output_by_crtc(r.crtc) else {
                crate::warning!("client configured unknown crtc {}", r.crtc);
                return randr::SetConfig::FAILED;
            };
            let name = String::from_utf8_lossy(&o.name).into_owned();
            if r.rotation != randr::Rotation::ROTATE0 {
                crate::warning!("client asked to rotate output {name:?}, which is unsupported");
                return randr::SetConfig::FAILED;
            }
            if r.mode == 0 {
                crate::vlog!("client disabled output {name:?}");
                Plan::Local
            } else {
                let Some((width, height)) = s.mode_size(r.mode) else {
                    crate::warning!(
                        "client configured output {name:?} with unknown mode {}",
                        r.mode
                    );
                    return randr::SetConfig::FAILED;
                };
                crate::vlog!(
                    "client set output {name:?} to {width}x{height} at {},{} (currently {}x{})",
                    r.x,
                    r.y,
                    o.width,
                    o.height
                );
                if !o.is_wayland() || (o.width == width && o.height == height) {
                    Plan::Local
                } else {
                    Plan::Compositor {
                        wl_name: o.wl_name,
                        name,
                        width,
                        height,
                    }
                }
            }
        };
        match plan {
            Plan::Local => {
                let ok = self
                    .server
                    .screen
                    .lock()
                    .unwrap()
                    .set_crtc(r.crtc, r.x, r.y, r.mode);
                if !ok {
                    return randr::SetConfig::FAILED;
                }
                // the framebuffer, the input mapping and the capture positions
                // all follow the screen model, exactly as for a change the
                // compositor made
                self.server.sync_screen(true);
                randr::SetConfig::SUCCESS
            }
            Plan::Compositor {
                wl_name,
                name,
                width,
                height,
            } => {
                let config = &self.server.config;
                if !config.dynres {
                    crate::warning!(
                        "client asked for {width}x{height} on output {name:?}, but \
                         dynamic resolution is off (see -dynres); refusing"
                    );
                    return randr::SetConfig::FAILED;
                }
                if !self.server.dynres.available() {
                    let why = if config.outmgr == crate::config::OutMgrType::None {
                        "output configuration is off (see -outmgr)"
                    } else {
                        "the compositor offers no way to change it"
                    };
                    crate::warning!(
                        "client asked for {width}x{height} on output {name:?}, but {why}; refusing"
                    );
                    return randr::SetConfig::FAILED;
                }
                crate::log!("client asked for {width}x{height} on output {name:?}");
                // blocks until the compositor has (or has not) changed the
                // output and the model has been rebuilt from what it did
                match self
                    .server
                    .dynres
                    .change_mode(wl_name, width, height, DYNRES_TIMEOUT)
                {
                    Ok(()) => {
                        // the model got a native mode of the new size from the
                        // wl_output sync; hand the CRTC the client's own mode
                        // so it reads back what it set
                        self.server
                            .screen
                            .lock()
                            .unwrap()
                            .adopt_mode(r.crtc, r.mode);
                        randr::SetConfig::SUCCESS
                    }
                    Err(reason) => {
                        crate::warning!(
                            "could not set {width}x{height} on output {name:?}: {reason}"
                        );
                        // the client may already have disabled the CRTC and
                        // resized the screen for a mode that never came; put
                        // both back to what the compositor is really showing
                        let changed = self.server.screen.lock().unwrap().restore_outputs();
                        if changed {
                            crate::log!("restored the screen to the compositor's outputs");
                            self.server.sync_screen(true);
                        }
                        randr::SetConfig::FAILED
                    }
                }
            }
        }
    }

    /// Shared between `GetScreenResources`/`GetScreenResourcesCurrent`.
    fn screen_resources(&self) -> (u32, u32, Vec<u32>, Vec<u32>, Vec<randr::ModeInfo>, Vec<u8>) {
        let s = self.server.screen.lock().unwrap();
        (
            s.timestamp,
            s.config_timestamp,
            s.crtc_ids(),
            s.output_ids(),
            s.mode_infos(),
            s.mode_names(),
        )
    }

    /// Pushes an RRScreenChangeNotify to all clients that selected RandR input.
    /// Tracks whether this client wants root ConfigureNotify (StructureNotify on
    /// the root window) so the event sink can deliver screen-resize notifications.
    fn track_root_structure(&self, window: u32, mask: xproto::EventMask) {
        if window == ROOT_WINDOW {
            self.client
                .select_root_structure(mask.contains(xproto::EventMask::STRUCTURE_NOTIFY));
        }
    }

    /// Interns an atom in the server's table (`InternAtom` with
    /// `only_if_exists` false).
    fn intern(&self, name: &[u8]) -> u32 {
        self.server.atoms.lock().unwrap().intern(name, false)
    }

    /// The name of an interned atom, copied out rather than borrowed: the
    /// table is shared, so the lock cannot be held while we use it.
    fn atom_name(&self, atom: u32) -> Option<Vec<u8>> {
        self.server
            .atoms
            .lock()
            .unwrap()
            .name(atom)
            .map(<[u8]>::to_vec)
    }

    /// Asks an X selection owner to convert its selection to UTF8_STRING so we
    /// can offer it on Wayland (X -> Wayland direction).
    fn start_fetch(&mut self, kind: Sel, selection: u32, owner: u32, time: u32) {
        let target = self.intern(b"UTF8_STRING");
        // one property per fetch, so transfers in flight at the same time
        // (RealVNC starts PRIMARY and CLIPBOARD at once) cannot land on top of
        // each other, INCR chunks included, and an answer can be matched to the
        // request it belongs to
        let which = match kind {
            Sel::Clipboard => "CLIPBOARD",
            Sel::Primary => "PRIMARY",
        };
        self.fetch_seq = self.fetch_seq.wrapping_add(1) % FETCH_PROPERTIES;
        let name = format!("XWLRVNC_FETCH_{which}_{}", self.fetch_seq);
        let property = self.intern(name.as_bytes());
        // whatever a late answer to the fetch that last had this property left
        // on it is not this owner's, and must not be read as its answer
        self.properties
            .lock()
            .unwrap()
            .remove(SELECTION_FETCH_WINDOW, property);
        crate::cliplog!("ConvertSelection {kind:?} to owner {owner:#x} time {time}");
        let displaced = self.selection.begin_fetch(Fetch {
            kind,
            selection,
            property,
            owner,
            time,
            since: Instant::now(),
        });
        for stale in displaced {
            crate::cliplog!(
                "{:?} owner {:#x} never answered; giving up on it",
                stale.kind,
                stale.owner
            );
            self.fetched(stale, None);
        }
        let request = xproto::SelectionRequestEvent {
            response_type: xproto::SELECTION_REQUEST_EVENT,
            sequence: self.seq,
            time,
            owner,
            requestor: SELECTION_FETCH_WINDOW,
            selection,
            target,
            property,
        };
        self.send_event(&request);
    }

    /// Completes an in-flight fetch when the owner replies with SelectionNotify.
    /// If the owner returned an INCR property, switches to chunked receive.
    fn handle_send_event(&mut self, event: &[u8; 32]) {
        if event[0] & 0x7f != xproto::SELECTION_NOTIFY_EVENT {
            return;
        }
        let Ok((notify, _)) = xproto::SelectionNotifyEvent::try_parse(&event[..]) else {
            return;
        };
        if notify.requestor != SELECTION_FETCH_WINDOW {
            return;
        }
        // matched by selection and property: PRIMARY and CLIPBOARD replies are
        // both in flight, and so can two for one selection taken twice, and
        // answering the wrong one hands a selection another's text
        let Some(fetch) = self.selection.take_fetch(notify.selection, notify.property) else {
            crate::cliplog!(
                "SelectionNotify for selection {} answers no fetch",
                notify.selection
            );
            // a fetch given up on: what its owner stored is not wanted, and
            // must not be there to be read as the next answer on that property
            if self.is_fetch_property(notify.property) {
                self.properties
                    .lock()
                    .unwrap()
                    .remove(SELECTION_FETCH_WINDOW, notify.property);
            }
            return;
        };
        let kind = fetch.kind;
        // property 0 is a refusal
        if notify.property != fetch.property {
            crate::cliplog!("{kind:?} owner refused the conversion");
            self.fetched(fetch, None);
            return;
        }
        let Some(p) = self
            .properties
            .lock()
            .unwrap()
            .remove(SELECTION_FETCH_WINDOW, fetch.property)
        else {
            crate::cliplog!("{kind:?} owner answered but stored no value");
            self.fetched(fetch, None);
            return;
        };
        if self.atom_name(p.type_).as_deref() == Some(b"INCR") {
            // Large value: the owner will feed it in chunks. Per ICCCM, the
            // requestor starts the transfer by deleting the INCR property
            // (which the owner, watching this window, reacts to with the first
            // chunk).
            self.selection.begin_incr(fetch);
            self.property_notify(
                SELECTION_FETCH_WINDOW,
                fetch.property,
                xproto::Property::DELETE,
            );
        } else {
            // ICCCM: the owner may wait for the property to go before it takes
            // the transfer to be done (Xt and GTK owners do, and hold the
            // conversion open until their own timeout otherwise)
            self.property_notify(
                SELECTION_FETCH_WINDOW,
                fetch.property,
                xproto::Property::DELETE,
            );
            self.fetched(fetch, Some(p.data));
        }
    }

    /// Whether `atom` names a property of the fetch ring (on any connection:
    /// atoms are global).
    fn is_fetch_property(&self, atom: u32) -> bool {
        self.atom_name(atom)
            .is_some_and(|name| name.starts_with(b"XWLRVNC_FETCH_"))
    }

    /// Gives up on every fetch whose owner has run out of time to answer, as
    /// though it had refused: see [`Selection::expired`].
    fn sweep_fetches(&mut self) {
        for fetch in self.selection.expired(Instant::now()) {
            crate::cliplog!(
                "{:?} owner {:#x} never answered; giving up on it",
                fetch.kind,
                fetch.owner
            );
            self.fetched(fetch, None);
        }
    }

    /// A fetch resolved: the owner gave us `data`, or with `None` would not.
    /// Either way, if that owner is still current, XFixes watchers now hear
    /// of its acquisition — with its window and the time it used, as X would
    /// have told them at `SetSelectionOwner`. Not before: X forwards their
    /// conversions to the owner, while we answer them from what we fetched,
    /// so the news is only good once there is something to fetch.
    fn fetched(&self, fetch: Fetch, data: Option<Vec<u8>>) {
        let clipboard = &self.server.clipboard;
        let effects = match data {
            Some(data) => clipboard.x_fetched(fetch.kind, fetch.owner, fetch.time, data),
            None => clipboard.x_fetch_failed(fetch.kind, fetch.owner, fetch.time),
        };
        if let Some(effects) = effects {
            effects.apply(&self.server, fetch.kind);
        }
    }

    /// Maps an X selection atom to the Wayland selection it bridges.
    fn selection_kind(&self, atom: u32) -> Option<Sel> {
        let kind = match self.atom_name(atom).as_deref() {
            _ if atom == XA_PRIMARY => Some(Sel::Primary), // predefined
            Some(b"CLIPBOARD") => Some(Sel::Clipboard),
            _ => None,
        };
        // With -noprimary, treat PRIMARY as an unknown selection so we neither
        // fetch it from nor offer it to Wayland.
        if self.server.config.noprimary && kind == Some(Sel::Primary) {
            return None;
        }
        kind
    }

    /// Handles ConvertSelection for a bridged selection: fills the requestor's
    /// property from the clipboard and replies with SelectionNotify — unless
    /// the value has to come out of a Wayland app, in which case the requestor
    /// is answered when it does, and this connection goes back to serving
    /// requests in the meantime.
    fn convert_selection(&mut self, r: &xproto::ConvertSelectionRequest) -> io::Result<()> {
        // any selection request is a chance to notice conversions the Wayland
        // thread has stopped answering; the client waiting on one of those is
        // by definition not asking us anything else
        self.server.clipboard.jobs().sweep();
        let kind = self.selection_kind(r.selection);
        // an X owner's value is ours to serve from the moment it took the
        // selection, before the compositor has echoed what we published for it
        let owned = kind.is_some_and(|k| self.server.clipboard.owner_window(k).is_some());
        let filled = if owned {
            self.fill_selection(kind.unwrap(), r)
        } else {
            Fill::Now(0) // we don't own it / can't convert
        };
        let Fill::Now(property) = filled else {
            return Ok(());
        };
        let notify = self.selection_notify(r, property);
        self.send_event(&notify);
        Ok(())
    }

    /// The `SelectionNotify` answering `r`, refusing it with `property` 0.
    fn selection_notify(
        &self,
        r: &xproto::ConvertSelectionRequest,
        property: u32,
    ) -> xproto::SelectionNotifyEvent {
        xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: self.seq, // stamped by the client's outbox
            time: r.time,
            requestor: r.requestor,
            selection: r.selection,
            target: r.target,
            property,
        }
    }

    /// Answers one conversion of `kind`: stores the value as the requestor's
    /// property and names it, refuses with 0, or hands the request to the
    /// Wayland thread and leaves it to answer.
    fn fill_selection(&mut self, kind: Sel, r: &xproto::ConvertSelectionRequest) -> Fill {
        let (requestor, property) = (r.requestor, r.property);
        let target_name = self.atom_name(r.target);
        if target_name.as_deref() == Some(b"TIMESTAMP") {
            // ICCCM: reply with the time this selection was acquired, as a single
            // 32-bit INTEGER. Polling clients (vncagent) convert TIMESTAMP and
            // re-read only when it changes.
            let ts = self.server.clipboard.last_changed(kind);
            self.properties.lock().unwrap().set(
                requestor,
                property,
                Property {
                    type_: XA_INTEGER,
                    format: 32,
                    data: ts.to_le_bytes().to_vec(),
                },
            );
            return Fill::Now(property);
        }
        if target_name.as_deref() == Some(b"TARGETS") {
            let targets = self.supported_targets(kind);
            let mut data = Vec::with_capacity(targets.len() * 4);
            for a in targets {
                data.extend_from_slice(&a.to_le_bytes());
            }
            self.properties.lock().unwrap().set(
                requestor,
                property,
                Property {
                    type_: 4,
                    format: 32,
                    data,
                },
            );
            return Fill::Now(property);
        }
        let (mimes, generation) = self.server.clipboard.convertible(kind);
        let Some(conv) = targets::resolve(target_name.as_deref().unwrap_or(b""), &mimes) else {
            return Fill::Now(0);
        };
        let type_ = match conv.type_ {
            Some(name) => self.intern(name),
            None => r.target,
        };
        match self.server.clipboard.value(kind) {
            Value::None => Fill::Now(0),
            Value::Here(data) => {
                Self::store_converted(
                    &self.properties,
                    requestor,
                    property,
                    type_,
                    conv.latin1,
                    data,
                );
                Fill::Now(property)
            }
            // out of a Wayland app, down a pipe only the Wayland thread may
            // ask for and read: this thread posts the job and moves on, and
            // the job answers the requestor whenever the app gets round to it
            // (or is dropped, which refuses it)
            Value::FromWayland => {
                let conversion = self.pending_conversion(r, type_, conv.latin1);
                crate::cliplog!(
                    "ConvertSelection {kind:?} to {} for window {requestor:#x}: asking the app",
                    conv.mime,
                );
                self.server.clipboard.jobs().post(Job::Receive {
                    sel: kind,
                    mime: conv.mime,
                    generation,
                    since: Instant::now(),
                    conversion,
                });
                Fill::Later
            }
        }
    }

    /// Stores a converted value as the requestor's property, transcoding it
    /// first if the target is one that asked for Latin-1. An empty value is a
    /// value: X selections can be empty, and refusing one would leave the
    /// requestor's previous paste in place.
    ///
    /// The properties lock is a leaf: nothing else is ever taken while it is
    /// held, on this thread or the Wayland one.
    fn store_converted(
        properties: &Mutex<Properties>,
        requestor: u32,
        property: u32,
        type_: u32,
        latin1: bool,
        data: Vec<u8>,
    ) {
        let data = if latin1 {
            targets::to_latin1(&data)
        } else {
            data
        };
        // never `unwrap`: this runs from a refusal in `Drop`, which the
        // `Draining` guard puts on every panic path, and panicking on a
        // poisoned lock during unwinding aborts the whole server. A poisoned
        // `Properties` still stores correctly.
        properties
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set(
                requestor,
                property,
                Property {
                    type_,
                    format: 8,
                    data,
                },
            );
    }

    /// The half of a conversion that runs once a Wayland app has handed over
    /// its value (or would not): store the property, then tell the requestor.
    /// That order is the one the requestor reads them in — it asks for the
    /// property only after the notification — and both happen on the thread
    /// that finished the transfer, so nothing can interleave between them.
    ///
    /// It carries the property store and the client rather than the connection
    /// because it may well outlive it: this thread goes straight back to
    /// serving requests, and may have finished with the client altogether by
    /// the time the value arrives. The property goes to *this* connection's
    /// store, exactly as it does when the answer is immediate; properties are
    /// per connection, so a requestor window belonging to another connection
    /// is out of reach either way.
    fn pending_conversion(
        &self,
        r: &xproto::ConvertSelectionRequest,
        type_: u32,
        latin1: bool,
    ) -> PendingConversion {
        pending_conversion(
            self.properties.clone(),
            &self.client,
            self.selection_notify(r, r.property),
            type_,
            latin1,
        )
    }

    /// The list of target atoms we can convert the given selection to.
    fn supported_targets(&mut self, kind: Sel) -> Vec<u32> {
        let mimes = self.server.clipboard.mimes(kind);
        let mut out = vec![self.intern(b"TARGETS"), self.intern(b"TIMESTAMP")];
        for name in targets::advertised(&mimes) {
            out.push(self.intern(&name));
        }
        out
    }

    /// XFixesSelectionNotify for an accepted `SetSelectionOwner` on
    /// `selection` that starts no fetch: to every client if it is bridged,
    /// else to this one alone.
    fn selection_owner_notify(&self, kind: Option<Sel>, selection: u32, owner: u32, time: u32) {
        let subtype = xfixes::SelectionEvent::SET_SELECTION_OWNER;
        let now = crate::bridge::event::server_time_ms();
        match kind {
            Some(k) => self.server.events.selection_changed(
                k,
                self.server.selection_atom(k),
                subtype,
                owner,
                now,
                time,
            ),
            None => {
                let regs = self.client.selections_for(selection, subtype);
                crate::bridge::event::selection_notify(
                    &self.client,
                    &regs,
                    subtype,
                    owner,
                    now,
                    time,
                );
            }
        }
    }

    /// Reverts every bridged selection this client owns (from `window`, if
    /// given) to no owner, telling XFixes watchers with `subtype`; X does the
    /// same when the owner window is destroyed or its client goes.
    fn selection_owner_gone(&self, window: Option<u32>, subtype: xfixes::SelectionEvent) {
        for kind in [Sel::Clipboard, Sel::Primary] {
            if let Some(effects) =
                self.server
                    .clipboard
                    .x_owner_gone(kind, &self.client, window, subtype)
            {
                effects.apply(&self.server, kind);
            }
        }
    }

    /// Sends a PropertyNotify for `(window, atom)` if the window selected
    /// PropertyChange. vncagent's clipboard `gotTime()` does a zero-length
    /// property append and blocks waiting for this event to read a server
    /// timestamp; without it, clipboard ownership/conversion never proceeds.
    fn property_notify(&self, window: u32, atom: u32, state: xproto::Property) {
        if !self.windows.wants_property_change(window) {
            return;
        }
        self.send_event(&xproto::PropertyNotifyEvent {
            response_type: xproto::PROPERTY_NOTIFY_EVENT,
            sequence: self.seq,
            window,
            atom,
            time: crate::bridge::event::server_time_ms(),
            state,
        });
    }

    /// Writes a 32-byte event through the guarded socket (which stamps a
    /// monotonic sequence number).
    fn send_event(&self, event: &impl Serialize) {
        let mut buf = Vec::with_capacity(32);
        event.serialize_into(&mut buf);
        if buf.len() < 32 {
            buf.resize(32, 0);
        }
        self.client.send_event_bytes(&mut buf);
    }

    fn reply(&mut self, reply: &impl Serialize) -> io::Result<()> {
        let mut buf = build_reply(reply);
        self.client.send_reply(self.seq, &mut buf)
    }

    /// Sends an X error of `code` for the current request, identifying the
    /// offending `major`/`minor` opcode. Used to fail requests we don't
    /// implement; for a reply-expecting request the error takes the reply's
    /// place so the client unblocks. The sequence is stamped by `send_reply`.
    fn send_error(&self, code: u8, major: u8, minor: u16) -> io::Result<()> {
        self.send_error_value(code, 0, major, minor)
    }

    /// [`send_error`](Self::send_error) with the offending value filled in,
    /// for the errors that name one (`BadValue`, `BadAtom`, ...).
    fn send_error_value(&self, code: u8, value: u32, major: u8, minor: u16) -> io::Result<()> {
        // 32-byte X error: [0]=0 (error), [1]=code, [2..4]=sequence (stamped by
        // send_reply), [4..8]=bad value, [8..10]=minor opcode, [10]=major.
        let mut buf = [0u8; 32];
        buf[1] = code;
        buf[4..8].copy_from_slice(&value.to_le_bytes());
        buf[8..10].copy_from_slice(&minor.to_le_bytes());
        buf[10] = major;
        self.client.send_reply(self.seq, &mut buf)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // lets the writer thread send what is queued and exit
        self.client.close();
        // X reverts a closing client's selections to nobody; the rest of the
        // display hears about it, this client no longer can
        self.selection_owner_gone(None, xfixes::SelectionEvent::SELECTION_CLIENT_CLOSE);
        // and nothing it was pressing stays pressed (unless -xtestpersist,
        // or for a while if it was a one-shot client)
        self.server.input.client_gone(self.id, self.since.elapsed());
        if self.active {
            self.server.framebuffer.note_oneshot();
        }
    }
}

/// See [`Connection::pending_conversion`]; free of the connection so that it
/// can outlive it, and so that it can be tested without one.
fn pending_conversion(
    properties: Arc<Mutex<Properties>>,
    client: &Arc<Client>,
    notify: xproto::SelectionNotifyEvent,
    type_: u32,
    latin1: bool,
) -> PendingConversion {
    let (requestor, property) = (notify.requestor, notify.property);
    // Weak, for the reason [`EventSink::clients`] is: a `Client` owns a
    // duplicate of the connection's socket, so holding it strongly would keep
    // that descriptor open for a connection that has already gone, for as long
    // as the conversion has left to run. If it is gone there is nobody to answer.
    let client = Arc::downgrade(client);
    PendingConversion::new(move |data| {
        let Some(client) = client.upgrade() else {
            return;
        };
        // A refusal runs from `Drop`, including while unwinding a panic, and
        // the `Draining` guard makes that a path every panic takes. Panicking
        // on a poisoned lock there would be a panic inside a drop during
        // unwinding, which aborts the whole server — every client, not just
        // this one. A poisoned `Properties` still answers `insert` correctly;
        // carrying on with it beats taking the process down.
        let mut notify = notify;
        match data {
            Some(data) => {
                Connection::store_converted(&properties, requestor, property, type_, latin1, data);
            }
            // no value to be had: a refusal, as from an owner that cannot
            // convert to the target it was asked for
            None => notify.property = 0,
        }
        crate::bridge::event::send(&client, &notify);
    })
}

pub struct RawRequest {
    pub major_opcode: u8,
    pub minor_opcode: u8,
    pub remaining_length: u32,
    /// Everything after the 4/8-byte header for
    /// [`x11rb_protocol::protocol::Request::parse`].
    pub body: Vec<u8>,
}

/// Reads one request. Returns `Ok(None)` on a clean EOF at a request boundary.
pub fn read_request(r: &mut impl Read) -> io::Result<Option<RawRequest>> {
    let mut hdr = [0u8; 4];
    if !read_exact_or_eof(r, &mut hdr)? {
        return Ok(None);
    }
    let major_opcode = hdr[0];
    let minor_opcode = hdr[1];
    let short_len = u16::from_le_bytes([hdr[2], hdr[3]]);
    let remaining_length = if short_len == 0 {
        // BIG-REQUESTS: the real length follows as a u32 in 4-byte units,
        // including the now-2-unit header.
        let mut ext = [0u8; 4];
        r.read_exact(&mut ext)?;
        let len = u32::from_le_bytes(ext);
        if len > MAX_REQUEST_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request length {len} exceeds the maximum {MAX_REQUEST_LENGTH}"),
            ));
        }
        len.saturating_sub(2)
    } else {
        u32::from(short_len) - 1
    };
    let mut body = vec![0u8; remaining_length as usize * 4];
    r.read_exact(&mut body)?;
    Ok(Some(RawRequest {
        major_opcode,
        minor_opcode,
        remaining_length,
        body,
    }))
}

/// Waits until `stream` has something to read or `deadline` passes, saying
/// which. A hangup or error counts as readable: the read that follows sees
/// it. Unlike a read timeout on the socket, this consumes nothing, so a
/// request that straddles the deadline is read whole afterwards.
fn wait_readable(stream: &UnixStream, deadline: Instant) -> io::Result<bool> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let timeout = i32::try_from(left.as_millis() + 1).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, for the count given
        match unsafe { libc::poll(&mut pfd, 1, timeout) } {
            0 => return Ok(false),
            n if n > 0 => return Ok(true),
            _ => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
}

/// Like `read_exact`, but distinguishes a clean EOF (no bytes read) from a
/// truncated read.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    Ok(true)
}

/// Serializes an x11rb reply, pads to the 32-byte wire minimum, and patches in
/// the length field. The sequence number at `[2..4]` is left zero and stamped by
/// the writer just before sending (see [`crate::bridge::event::Client::send_reply`]).
///
/// x11rb's reply `serialize` only emits the meaningful bytes (e.g. 12 for
/// `QueryExtension`); the real wire format is always at least 32 bytes with
/// `length` counting the extra 4-byte units beyond that. Patching `[4..8]` from
/// the final length works for every reply because that field is always the reply
/// length.
pub fn build_reply(reply: &impl Serialize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    reply.serialize_into(&mut buf);
    // Replies are at least 32 bytes, and their trailing variable data must be
    // padded to a 4-byte boundary (x11rb's serialize doesn't always do this).
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    let pad = (4 - buf.len() % 4) % 4;
    buf.resize(buf.len() + pad, 0);
    let length = ((buf.len() - 32) / 4) as u32;
    buf[4..8].copy_from_slice(&length.to_le_bytes());
    buf
}

/// Serializes a refused connection's setup reply (status Failed + reason),
/// for a little-endian client.
pub fn setup_failed(reason: &[u8]) -> Vec<u8> {
    let reason = &reason[..reason.len().min(255)];
    let mut buf = vec![0, reason.len() as u8];
    buf.extend_from_slice(&11u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&((pad4(reason.len()) / 4) as u16).to_le_bytes());
    buf.extend_from_slice(reason);
    buf.resize(8 + pad4(reason.len()), 0);
    buf
}

/// Serializes the setup reply (status byte + version + length + body) for a
/// single TrueColor 24-bit screen of the given size. `resource_id_base` must be
/// unique per connection so clients don't allocate colliding IDs.
pub fn setup(geom: Geometry, resource_id_base: u32) -> Vec<u8> {
    debug_assert!(
        resource_id_base >= CLIENT_RESOURCE_ID_BASE
            && u64::from(resource_id_base) + u64::from(super::CLIENT_RESOURCE_ID_MASK)
                < 0x2000_0000,
        "a range from ID_RANGES: above the server's ids, within 29 bits"
    );
    let visual = Visualtype {
        visual_id: ROOT_VISUAL,
        class: VisualClass::TRUE_COLOR,
        bits_per_rgb_value: 8,
        colormap_entries: 256,
        red_mask: 0x00ff_0000,
        green_mask: 0x0000_ff00,
        blue_mask: 0x0000_00ff,
    };
    let screen = Screen {
        root: ROOT_WINDOW,
        default_colormap: ROOT_COLORMAP,
        white_pixel: 0x00ff_ffff,
        black_pixel: 0x0000_0000,
        current_input_masks: EventMask::NO_EVENT,
        width_in_pixels: geom.width,
        height_in_pixels: geom.height,
        width_in_millimeters: mm(geom.width) as u16,
        height_in_millimeters: mm(geom.height) as u16,
        min_installed_maps: 1,
        max_installed_maps: 1,
        root_visual: ROOT_VISUAL,
        backing_stores: BackingStore::NOT_USEFUL,
        save_unders: false,
        root_depth: ROOT_DEPTH,
        allowed_depths: vec![Depth {
            depth: ROOT_DEPTH,
            visuals: vec![visual],
        }],
    };
    let setup = Setup {
        status: 1, // success
        protocol_major_version: 11,
        protocol_minor_version: 0,
        length: 0, // patched below
        release_number: 0,
        resource_id_base,
        resource_id_mask: super::CLIENT_RESOURCE_ID_MASK,
        motion_buffer_size: 256,
        maximum_request_length: 65535,
        image_byte_order: ImageOrder::LSB_FIRST,
        bitmap_format_bit_order: ImageOrder::LSB_FIRST,
        bitmap_format_scanline_unit: 32,
        bitmap_format_scanline_pad: 32,
        min_keycode: 8,
        max_keycode: 255,
        vendor: b"xwlrvnc".to_vec(),
        pixmap_formats: vec![
            Format {
                depth: 1,
                bits_per_pixel: 1,
                scanline_pad: 32,
            },
            Format {
                depth: 24,
                bits_per_pixel: 32,
                scanline_pad: 32,
            },
            Format {
                depth: 32,
                bits_per_pixel: 32,
                scanline_pad: 32,
            },
        ],
        roots: vec![screen],
    };
    let mut buf = Vec::new();
    setup.serialize_into(&mut buf);
    // `length` counts the 4-byte units after the 8-byte header.
    let length = ((buf.len() - 8) / 4) as u16;
    buf[6..8].copy_from_slice(&length.to_le_bytes());
    buf
}

/// The error code of RandR's error `n` (`randr::BAD_*_ERROR`) on this server.
fn randr_error(n: u8) -> u8 {
    EXTENSIONS
        .lookup(randr::X11_EXTENSION_NAME.as_bytes())
        .map_or(xproto::IMPLEMENTATION_ERROR, |e| e.first_error + n)
}

/// A failed `GetOutputInfo` reply for an unknown output id.
fn empty_output_info(timestamp: u32) -> randr::GetOutputInfoReply {
    randr::GetOutputInfoReply {
        status: randr::SetConfig::FAILED,
        sequence: 0,
        length: 0,
        timestamp,
        crtc: 0,
        mm_width: 0,
        mm_height: 0,
        connection: randr::Connection::DISCONNECTED,
        subpixel_order: render::SubPixel::UNKNOWN,
        num_preferred: 0,
        crtcs: vec![],
        modes: vec![],
        clones: vec![],
        name: vec![],
    }
}

/// A failed `GetCrtcInfo` reply for an unknown crtc id.
fn empty_crtc_info(timestamp: u32) -> randr::GetCrtcInfoReply {
    randr::GetCrtcInfoReply {
        status: randr::SetConfig::FAILED,
        sequence: 0,
        length: 0,
        timestamp,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        mode: 0,
        rotation: randr::Rotation::ROTATE0,
        rotations: randr::Rotation::ROTATE0,
        outputs: vec![],
        possible: vec![],
    }
}

/// The RENDER identity transform (1.0 on the diagonal in 16.16 fixed point).
fn identity_transform() -> render::Transform {
    let one: render::Fixed = 1 << 16;
    render::Transform {
        matrix11: one,
        matrix12: 0,
        matrix13: 0,
        matrix21: 0,
        matrix22: one,
        matrix23: 0,
        matrix31: 0,
        matrix32: 0,
        matrix33: one,
    }
}

/// Plausible attributes for our (nonexistent) root/child windows.
fn window_attributes() -> xproto::GetWindowAttributesReply {
    xproto::GetWindowAttributesReply {
        backing_store: xproto::BackingStore::NOT_USEFUL,
        sequence: 0,
        length: 0,
        visual: ROOT_VISUAL,
        class: xproto::WindowClass::INPUT_OUTPUT,
        bit_gravity: xproto::Gravity::NORTH_WEST,
        win_gravity: xproto::Gravity::NORTH_WEST,
        backing_planes: 0,
        backing_pixel: 0,
        save_under: false,
        map_is_installed: true,
        map_state: xproto::MapState::VIEWABLE,
        override_redirect: false,
        colormap: ROOT_COLORMAP,
        all_event_masks: xproto::EventMask::NO_EVENT,
        your_event_mask: xproto::EventMask::NO_EVENT,
        do_not_propagate_mask: xproto::EventMask::NO_EVENT,
    }
}

/// The modifier map (2 keycodes per modifier) served until the compositor
/// keymap is known, from which the real one is derived: the pc105 layout,
/// using our X keycodes (= evdev code + 8): Shift, Lock, Control, Mod1(Alt),
/// Mod2(Num), Mod3, Mod4(Super), Mod5.
#[rustfmt::skip]
static DEFAULT_MODIFIER_MAP: [u8; 16] = [
    50, 62,   // Shift: LeftShift(42), RightShift(54)
    66, 0,    // Lock: CapsLock(58)
    37, 105,  // Control: LeftCtrl(29), RightCtrl(97)
    64, 108,  // Mod1: LeftAlt(56), RightAlt(100)
    77, 0,    // Mod2: NumLock(69)
    0, 0,     // Mod3
    133, 134, // Mod4: LeftMeta(125), RightMeta(126)
    0, 0,     // Mod5
];

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// `WarpPointer`'s source condition for a request with a `src_window`: the
/// pointer is inside the rectangle `src_x,src_y` `src_width`x`src_height` of
/// that window (here: of the screen, where every window is), a width or
/// height of 0 meaning "to the window's edge".
fn warp_source_holds(
    r: &xproto::WarpPointerRequest,
    pointer: (i16, i16),
    screen: crate::util::Geometry,
) -> bool {
    let span = |origin: i16, len: u16, limit: u16| {
        let start = i32::from(origin);
        let end = if len == 0 {
            i32::from(limit)
        } else {
            start + i32::from(len)
        };
        start..end
    };
    span(r.src_x, r.src_width, screen.width).contains(&i32::from(pointer.0))
        && span(r.src_y, r.src_height, screen.height).contains(&i32::from(pointer.1))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    use x11rb_protocol::x11_utils::TryParse;

    use super::*;

    const REQUESTOR: u32 = 0x0060_0005;
    const PROPERTY: u32 = 321;
    const TYPE: u32 = 654;

    /// A conversion answered later, as one out of a Wayland app is, plus the
    /// property store it writes to and the socket its client reads.
    fn pending(latin1: bool) -> (PendingConversion, Arc<Mutex<Properties>>, UnixStream) {
        let (peer, ours) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let properties: Arc<Mutex<Properties>> = Arc::default();
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: 42,
            requestor: REQUESTOR,
            selection: 1,
            target: 7,
            property: PROPERTY,
        };
        let client = Client::new(ours).unwrap();
        let c = pending_conversion(properties.clone(), &client, notify, TYPE, latin1);
        (c, properties, peer)
    }

    /// The `SelectionNotify` the client was sent, once its writer thread has
    /// got to it.
    fn notified(peer: &mut UnixStream) -> xproto::SelectionNotifyEvent {
        let mut buf = [0u8; 32];
        peer.read_exact(&mut buf).expect("no event was sent");
        let (notify, _) = xproto::SelectionNotifyEvent::try_parse(&buf[..]).unwrap();
        notify
    }

    #[test]
    fn warp_pointer_source_rectangle() {
        let screen = crate::util::Geometry {
            width: 1280,
            height: 720,
        };
        let warp = |x: i16, y: i16, w: u16, h: u16| xproto::WarpPointerRequest {
            src_window: ROOT_WINDOW,
            dst_window: 0,
            src_x: x,
            src_y: y,
            src_width: w,
            src_height: h,
            dst_x: 200,
            dst_y: 200,
        };
        // inside, on the near edges, and past the far edges (exclusive)
        assert!(warp_source_holds(&warp(0, 0, 10, 10), (5, 5), screen));
        assert!(warp_source_holds(&warp(5, 5, 1, 1), (5, 5), screen));
        assert!(!warp_source_holds(&warp(0, 0, 1, 1), (100, 100), screen));
        assert!(!warp_source_holds(&warp(0, 0, 10, 10), (10, 5), screen));
        assert!(!warp_source_holds(&warp(0, 0, 10, 10), (5, 10), screen));
        // 0 reaches to the window's (= the screen's) edge
        assert!(warp_source_holds(
            &warp(100, 100, 0, 0),
            (1279, 719),
            screen
        ));
        assert!(!warp_source_holds(&warp(100, 100, 0, 0), (99, 719), screen));
        assert!(!warp_source_holds(&warp(0, 0, 0, 0), (1280, 0), screen));
        // a rectangle reaching past i16 doesn't wrap
        assert!(warp_source_holds(
            &warp(i16::MAX - 1, 0, u16::MAX, 0),
            (i16::MAX, 3),
            screen
        ));
    }

    #[test]
    fn a_finished_conversion_stores_the_property_then_notifies() {
        // the requestor reads them in that order — it asks for the property
        // only once the notification names it — so the value has to be there
        // by the time the event goes out
        let (c, properties, mut peer) = pending(false);
        c.finish(Some(b"hello".to_vec()));
        let notify = notified(&mut peer);
        assert_eq!(notify.property, PROPERTY, "the property is named");
        assert_eq!(notify.requestor, REQUESTOR);
        let p = properties.lock().unwrap();
        let p = p.get(REQUESTOR, PROPERTY).expect("stored before the event");
        assert_eq!(p.data, b"hello");
        assert_eq!((p.type_, p.format), (TYPE, 8));
    }

    #[test]
    fn a_refused_conversion_names_no_property() {
        let (c, properties, mut peer) = pending(false);
        c.finish(None);
        assert_eq!(notified(&mut peer).property, 0, "0 is the refusal");
        assert!(
            properties
                .lock()
                .unwrap()
                .get(REQUESTOR, PROPERTY)
                .is_none(),
            "and nothing is left behind for the requestor to read"
        );
    }

    #[test]
    fn a_latin1_target_is_transcoded_on_the_way_out() {
        let (c, properties, mut peer) = pending(true);
        c.finish(Some("é".as_bytes().to_vec()));
        assert_eq!(notified(&mut peer).property, PROPERTY);
        let p = properties.lock().unwrap();
        assert_eq!(p.get(REQUESTOR, PROPERTY).unwrap().data, [0xe9]);
    }

    #[test]
    fn a_conversion_nobody_finishes_refuses_the_requestor() {
        // a job dropped on a full queue, or with no wayland thread to run it
        let (c, _properties, mut peer) = pending(false);
        drop(c);
        assert_eq!(notified(&mut peer).property, 0);
    }

    #[test]
    fn answering_a_connection_that_has_gone_does_nothing() {
        // A conversion holds the client weakly, so that it cannot keep the
        // duplicate socket of a dead connection open. Once the connection and
        // its writer thread are done there is nobody to answer, so the late
        // value is dropped rather than stored: the property belongs to a
        // connection that no longer exists to read it.
        let (peer, ours) = UnixStream::pair().unwrap();
        let properties: Arc<Mutex<Properties>> = Arc::default();
        let client = Client::new(ours).unwrap();
        let gone = Arc::downgrade(&client);
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: 1,
            requestor: REQUESTOR,
            selection: 1,
            target: 7,
            property: PROPERTY,
        };
        let c = pending_conversion(properties.clone(), &client, notify, TYPE, false);
        client.close();
        drop(client);
        drop(peer);
        // the writer thread holds the last strong reference until it drains and
        // exits; waiting for it is what makes this deterministic rather than a
        // race between that thread and the assertion
        for _ in 0..1000 {
            if gone.upgrade().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            gone.upgrade().is_none(),
            "the client outlived its connection"
        );

        c.finish(Some(b"late".to_vec()));
        assert!(
            properties
                .lock()
                .unwrap()
                .get(REQUESTOR, PROPERTY)
                .is_none(),
            "answered a connection that no longer exists"
        );
    }
}
