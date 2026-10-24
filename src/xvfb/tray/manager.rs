//! The tray manager: `_NET_SYSTEM_TRAY_S<screen>` on the engine display.
//!
//! One thread owns a connection of its own to the engine's X server (x11rb, like the rest of the agent). It
//! takes the tray selection, docks the icons that ask for it, and follows them: their DAMAGE (new pixels),
//! their `_XEMBED_INFO`, their names, their balloon messages and their menus. What the desktop shows goes to
//! the bus thread as an [`Event`]; what the desktop does to an icon comes back as a [`clicks::Command`],
//! which this thread turns into XTest on the icon ([`clicks`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::damage;
use x11rb::protocol::xinput::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, ImageFormat, MapState, PropMode, StackMode, WindowClass,
};
use x11rb::protocol::{Event as XEvent, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::balloons::Balloons;
use super::clicks::{self, Command, Step};
use super::dock::{CONTAINER, Icon, PARK};
use super::pixels::Image;
use super::{ICON_SIZE, Look, MAX_ICON_UPDATES, selection_name};
use crate::bridge::input::{BUTTON_PRESS, BUTTON_RELEASE, MOTION_NOTIFY};
use crate::xvfb::{self, AnyError, Auth};

/// The property of the tray's own window that says whose it is (the tests find the tray that runs).
const PID_ATOM: &str = "_XWLBridge_TRAY_PID";

/// How long to wait before connecting to the X server again (the tray lost its connection).
const RETRY: Duration = Duration::from_secs(2);

/// How often an icon's name and pixels are checked without a DAMAGE (a rename sends none).
const POLL: Duration = Duration::from_secs(1);

// `_NET_SYSTEM_TRAY_OPCODE`'s message types.
const REQUEST_DOCK: u32 = 0;
const BEGIN_MESSAGE: u32 = 1;
const MESSAGE_DATA: u32 = 2;
const CANCEL_MESSAGE: u32 = 3;

/// What the tray tells the bus thread.
#[derive(Clone, Debug)]
pub enum Event {
    /// An icon was docked, with its first look.
    Docked { container: u32, look: Look },
    /// The icon's look changed (new pixels, a new name, a menu or not).
    Look { container: u32, look: Box<Look> },
    /// The icon is gone.
    Undocked { container: u32 },
    /// A balloon message (`_NET_SYSTEM_TRAY_BEGIN_MESSAGE`), whole.
    Notify {
        container: u32,
        title: String,
        text: String,
        timeout: i32,
    },
}

/// Starts the tray on `display` (the agent's X server), for `screen`, its icons on the session bus at
/// `bus` (`None`: the session's own).
pub fn spawn(
    display: u32,
    auth: Auth,
    screen: u32,
    bus: Option<String>,
    pointer: Option<u16>,
) -> std::io::Result<Running> {
    let (tx, commands) = channel();
    let (ready, started) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                // the one thread that talks to the X server: every request of the tray is made here
                let previous = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    let here = format!("{:?}", std::thread::current().id());
                    crate::warning!("-showwindows: tray: panicked in {here}: {info}");
                    previous(info);
                }));
                // The tray's own connection to the X server. Should its thread ever end (it does not on a
                // lost connection: it reconnects, see `run`), another connection takes the selection back.
                let mut first = true;
                while !stop.load(Ordering::SeqCst) {
                    if !first {
                        crate::log!(
                            "-showwindows: tray: connecting again in {} s",
                            RETRY.as_secs()
                        );
                        let until = Instant::now() + RETRY;
                        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                    }
                    let manager = match Manager::connect(display, &auth, screen, pointer)
                        .and_then(|mut m| m.take_selection().map(|()| m))
                    {
                        Ok(m) => Rc::new(RefCell::new(m)),
                        Err(e) if !first => {
                            crate::vlog!("-showwindows: tray: {e}");
                            continue;
                        }
                        Err(e) => {
                            let _ = ready.send(Err(format!("{e}")));
                            return;
                        }
                    };
                    let events = manager.borrow_mut().start_bus();
                    let _bus = super::session::start(events, bus.clone(), tx.clone());
                    if first && ready.send(Ok(())).is_err() {
                        manager.borrow_mut().release();
                        return;
                    }
                    first = false;
                    Manager::run(manager.clone(), &commands, &stop);
                    manager.borrow_mut().release();
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    crate::log!("-showwindows: tray: lost the X connection; the tray reconnects");
                }
            })?
    };
    match started.recv() {
        Ok(Ok(())) => Ok(Running {
            stop,
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(std::io::Error::other(e))
        }
        Err(_) => Err(std::io::Error::other("the tray thread stopped")),
    }
}

/// A running tray: the icons' events go to the session bus (`-showwindows`), for as long as it runs.
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // it notices within its wait, or as soon as it is at its next event
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The atoms the tray uses.
struct Atoms {
    manager: u32,
    selection: u32,
    opcode: u32,
    /// `_NET_SYSTEM_TRAY_MESSAGE_DATA`: the message type GTK and Qt send balloon text with.
    message_data: u32,
    orientation: u32,
    icon_size: u32,
    visual: u32,
    xembed_info: u32,
    net_wm_name: u32,
    wm_name: u32,
    wm_class: u32,
    container: u32,
    /// findable by the tests (a tray that starts again must not depend on an earlier one's atoms)
    pid: u32,
}

impl Atoms {
    fn new(conn: &RustConnection, screen: u32) -> Result<Atoms, AnyError> {
        let atom = |name: &[u8]| -> Result<u32, AnyError> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        let atoms = Atoms {
            manager: atom(b"MANAGER")?,
            selection: atom(selection_name(screen).as_bytes())?,
            opcode: atom(b"_NET_SYSTEM_TRAY_OPCODE")?,
            message_data: atom(b"_NET_SYSTEM_TRAY_MESSAGE_DATA")?,
            orientation: atom(b"_NET_SYSTEM_TRAY_ORIENTATION")?,
            icon_size: atom(b"_NET_SYSTEM_TRAY_ICON_SIZE")?,
            visual: atom(b"_NET_SYSTEM_TRAY_VISUAL")?,
            xembed_info: atom(b"_XEMBED_INFO")?,
            net_wm_name: atom(b"_NET_WM_NAME")?,
            wm_name: atom(b"WM_NAME")?,
            wm_class: atom(b"WM_CLASS")?,
            container: atom(CONTAINER.as_bytes())?,
            pid: atom(PID_ATOM.as_bytes())?,
        };
        // (the requests are queued: this waits until the server has them all)
        conn.get_input_focus()?.reply()?;
        Ok(atoms)
    }
}

/// A balloon being reassembled (the timeout comes with its `BEGIN_MESSAGE`, the text after it).
struct Balloon {
    timeout: i32,
}

struct Manager {
    conn: RustConnection,
    screen: u32,
    root: u32,
    window: u32,
    atoms: Atoms,
    icons: HashMap<u32, Icon>,
    balloons: Balloons,
    /// The balloon each icon has open (by icon window).
    open: HashMap<u32, Balloon>,
    events: Option<Sender<Event>>,
    /// What the desktop has been told of each icon's look, by container.
    told: HashMap<u32, Look>,
    last_poll: Instant,
    /// When each icon's pixels last reached the desktop (see [`MAX_ICON_UPDATES`]).
    read: HashMap<u32, Instant>,
    /// Icons whose DAMAGE came too soon for a read.
    pending: Vec<u32>,
    /// Events already read from the server that are not handled yet (see [`Manager::undock`]).
    waiting: Vec<XEvent>,
    /// The 32-bit ARGB visual, and the screen's default one.
    argb: Option<u32>,
    default_visual: u32,
}

impl Manager {
    fn connect(
        display: u32,
        auth: &Auth,
        screen: u32,
        pointer: Option<u16>,
    ) -> Result<Manager, AnyError> {
        let (conn, screen_num) = xvfb::connect(display, auth)?;
        super::set_connection_base(conn.setup().resource_id_base);
        // Our XTest goes through the reverse master's XTest devices (as the reverse path's own does):
        // the core pointer is held by the agent's grab, and events injected on it from another client
        // reach no X window. With this connection's ClientPointer on the reverse master, the icons get
        // the clicks, and they are still the bridge's own input (`attrib`: the connection is ours).
        if let Some(device) = pointer {
            let set = (|| -> Result<(), AnyError> {
                conn.xinput_xi_query_version(2, 2)?.reply()?;
                conn.xinput_xi_set_client_pointer(x11rb::NONE, device)?
                    .check()?;
                Ok(())
            })();
            if let Err(e) = set {
                crate::warning!(
                    "-showwindows: tray: the reverse pointer {device} can't be the tray's ({e}): clicks may not reach the icons"
                );
            }
        }
        let root = conn.setup().roots[screen_num].root;
        let atoms = Atoms::new(&conn, screen)?;
        let (default_visual, argb) = {
            let root = &conn.setup().roots[screen_num];
            let argb = root
                .allowed_depths
                .iter()
                .find(|d| d.depth == 32)
                .and_then(|d| d.visuals.first())
                .map(|v| v.visual_id);
            (root.root_visual, argb)
        };
        Ok(Manager {
            conn,
            screen,
            root,
            window: 0,
            atoms,
            icons: HashMap::new(),
            balloons: Balloons::new(),
            open: HashMap::new(),
            events: None,
            told: HashMap::new(),
            last_poll: Instant::now(),
            read: HashMap::new(),
            pending: Vec::new(),
            waiting: Vec::new(),
            argb,
            default_visual,
        })
    }

    fn send(&self, event: Event) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    fn start_bus(&mut self) -> Option<Receiver<Event>> {
        let (tx, rx) = channel();
        self.events = Some(tx);
        Some(rx)
    }

    // ------------------------------------------------------------------ selection

    /// Takes the selection and announces the tray. `Err` if another tray has it.
    fn take_selection(&mut self) -> Result<(), AnyError> {
        // an earlier tray may have gone with the display this one starts on (a restarted X server keeps
        // the numbers), so every atom is interned again: x11rb interns them without waiting for the reply,
        // and a round trip first makes sure the server knows them all
        self.atoms = Atoms::new(&self.conn, self.screen)?;
        let window = self.conn.generate_id()?;
        self.conn.create_window(
            0,
            window,
            self.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?;
        // whose tray it is (the tests find the tray that runs)
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.atoms.pid,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )?;
        self.conn
            .set_selection_owner(window, self.atoms.selection, x11rb::CURRENT_TIME)?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.selection)?
            .reply()?
            .owner;
        if owner != window {
            return Err("another system tray owns the selection".into());
        }
        self.window = window;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.atoms.orientation,
            AtomEnum::CARDINAL,
            &[0], // _NET_SYSTEM_TRAY_ORIENTATION_HORZ
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            self.atoms.icon_size,
            AtomEnum::CARDINAL,
            &[u32::from(ICON_SIZE)],
        )?;
        if let Some(visual) = self.argb {
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms.visual,
                AtomEnum::VISUALID,
                &[visual],
            )?;
        }
        // tell every current client there is a tray (ICCCM: MANAGER, SelectionTime, owner)
        let mut data = [0u32; 5];
        data[..3].copy_from_slice(&[x11rb::CURRENT_TIME, self.atoms.selection, window]);
        let announce = ClientMessageEvent {
            response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: self.root,
            type_: self.atoms.manager,
            data: data.into(),
        };
        self.conn
            .send_event(false, self.root, EventMask::STRUCTURE_NOTIFY, announce)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Gives the selection back: every icon is undocked and the desktop told.
    fn release(&mut self) {
        let containers: Vec<u32> = self.icons.values().map(|i| i.container).collect();
        for container in containers {
            self.send(Event::Undocked { container });
        }
        self.icons.clear();
        if self.window != 0 {
            self.conn
                .set_selection_owner(x11rb::NONE, self.atoms.selection, x11rb::CURRENT_TIME)
                .ok();
            self.conn.destroy_window(self.window).ok();
            self.conn.flush().ok();
            self.window = 0;
        }
    }

    // ------------------------------------------------------------------ the loop

    /// Runs until stopped.
    ///
    /// Every request of the tray is made from here (the thread that owns the connection): the bus thread
    /// only queues its commands, so nothing it does can hold up the X server's replies to the tray.
    fn run(manager: Rc<RefCell<Manager>>, commands: &Receiver<Command>, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            let fd = manager.borrow().conn.stream().as_raw_fd();
            if let Err(e) = manager.borrow_mut().pump(commands) {
                // the X connection is gone (the engine restarted): a new one takes the selection back
                crate::log!("-showwindows: tray: lost the X connection ({e})");
                break;
            }
            wait(fd, Duration::from_millis(100));
        }
        manager.borrow_mut().release();
    }

    /// Handles what is there, and the commands.
    fn pump(&mut self, commands: &Receiver<Command>) -> Result<(), AnyError> {
        loop {
            let event = match self.waiting.pop() {
                Some(event) => Ok(Some(event)),
                None => self.conn.poll_for_event(),
            };
            match event {
                Ok(Some(event)) => {
                    if let Err(e) = self.event(event) {
                        crate::vlog!("-showwindows: tray: {e}");
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(e.into()),
            }
        }
        if self.last_poll.elapsed() >= POLL {
            self.last_poll = Instant::now();
            if let Err(e) = self.poll() {
                crate::vlog!("-showwindows: tray: {e}");
            }
        }
        while let Ok(command) = commands.try_recv() {
            if let Err(e) = self.command(command) {
                crate::vlog!("-showwindows: tray: {e}");
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    // ------------------------------------------------------------------ events

    fn event(&mut self, event: XEvent) -> Result<(), AnyError> {
        match event {
            XEvent::ClientMessage(e) if e.type_ == self.atoms.opcode => self.client_message(&e)?,
            XEvent::ClientMessage(e) if e.type_ == self.atoms.message_data => {
                // the text of a balloon: the icon it belongs to is the message's window, the text its data
                let bytes = e.data.as_data8();
                self.message_data(e.window, &bytes);
            }
            XEvent::DestroyNotify(e) => self.undock(e.window),
            XEvent::UnmapNotify(e) => {
                // an XEMBED 0 client (or one that has not set _XEMBED_INFO yet) shows itself
                if self.icons.get(&e.window).is_some_and(|i| i.self_managed) {
                    let unmapped = self
                        .conn
                        .get_window_attributes(e.window)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .is_some_and(|a| a.map_state == MapState::UNMAPPED);
                    if unmapped {
                        self.undock(e.window);
                    }
                }
            }
            XEvent::ReparentNotify(e) => {
                if let Some(icon) = self.icons.get(&e.window)
                    && e.parent != icon.container
                {
                    self.undock(e.window);
                }
            }
            XEvent::ConfigureNotify(e) => {
                if let Some(icon) = self.icons.get_mut(&e.window)
                    && (e.width, e.height) != (0, 0)
                {
                    icon.size = (e.width, e.height);
                }
            }
            XEvent::PropertyNotify(e)
                if e.atom == self.atoms.xembed_info && self.icons.contains_key(&e.window) =>
            {
                self.xembed_changed(e.window)?
            }
            XEvent::DamageNotify(e) => self.damaged(e.damage),
            _ => {}
        }
        Ok(())
    }

    fn client_message(&mut self, e: &ClientMessageEvent) -> Result<(), AnyError> {
        let data = e.data.as_data32();
        match data[1] {
            REQUEST_DOCK => self.dock(data[2])?,
            BEGIN_MESSAGE => {
                // data[2] is the timeout, data[3] the length, data[4] the id; the icon is the message's
                // window (as GTK's and Qt's tray code send it), not the manager's
                let mut header = [0u8; 40];
                header[0..4].copy_from_slice(&data[2].to_ne_bytes());
                header[4..8].copy_from_slice(&data[3].to_ne_bytes());
                let h = self.balloons.begin(e.window, &header);
                self.open.insert(e.window, Balloon { timeout: h.timeout });
            }
            // (some clients send the text with the opcode type instead of _NET_SYSTEM_TRAY_MESSAGE_DATA)
            MESSAGE_DATA => {
                let bytes = e.data.as_data8();
                self.message_data(e.window, &bytes);
            }
            // data[2] is the balloon's id, not an icon: the icon is the message's window
            CANCEL_MESSAGE => {
                self.balloons.cancel(e.window);
                self.open.remove(&e.window);
            }
            _ => {}
        }
        Ok(())
    }

    /// Text of a balloon: one 20-byte segment (`_NET_SYSTEM_TRAY_MESSAGE_DATA`, or `MESSAGE_DATA` sent with
    /// the opcode type). The message goes out as a notification once the length announced is there.
    fn message_data(&mut self, icon: u32, bytes: &[u8]) {
        if !self.icons.contains_key(&icon) {
            return;
        }
        let Some((_, text)) = self.balloons.data(icon, bytes) else {
            return;
        };
        let timeout = self.open.get(&icon).map_or(0, |b| b.timeout);
        let container = self.icons.get(&icon).map(|i| i.container);
        let title = container
            .and_then(|c| self.told.get(&c))
            .map(|l| l.title.clone())
            .unwrap_or_default();
        if let Some(container) = container {
            self.send(Event::Notify {
                container,
                title,
                text,
                timeout,
            });
        }
    }

    // ------------------------------------------------------------------ docking

    fn dock(&mut self, w: u32) -> Result<(), AnyError> {
        if self.icons.contains_key(&w) {
            return Ok(());
        }
        let geometry = self.conn.get_geometry(w)?.reply()?;
        // an ARGB icon gets an ARGB container (its transparency is what makes it look right on the desktop)
        let (depth, visual) = match (geometry.depth, self.argb) {
            (32, Some(visual)) => (32, visual),
            _ => (self.conn.setup().roots[0].root_depth, self.default_visual),
        };
        let container = self.conn.generate_id()?;
        self.conn.create_window(
            depth,
            container,
            self.root,
            PARK.0,
            PARK.1,
            ICON_SIZE,
            ICON_SIZE,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(0)
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            container,
            self.atoms.container,
            AtomEnum::WINDOW,
            &[w],
        )?;
        // the window's contents (and its events) stay when we go
        self.conn
            .change_save_set(x11rb::protocol::xproto::SetMode::INSERT, w)
            .ok();
        self.conn.reparent_window(w, container, 0, 0)?;
        self.conn.change_window_attributes(
            w,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        self.conn.configure_window(
            w,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(ICON_SIZE))
                .height(u32::from(ICON_SIZE)),
        )?;
        self.conn.map_window(container)?;
        let damage = self.conn.generate_id()?;
        damage::create(&self.conn, damage, w, damage::ReportLevel::NON_EMPTY)?;
        let mut icon = Icon::new(w, container, damage);
        icon.size = (ICON_SIZE, ICON_SIZE);
        icon.info = self.read_info(w)?;
        icon.self_managed = icon.info.is_none();
        let shown = icon.shown();
        self.icons.insert(w, icon);
        if shown {
            self.conn.map_window(w)?;
        }
        self.conn.flush()?;
        let look = self.look(w);
        crate::log!(
            "-showwindows: tray icon {w:#x} docked ({}x{} {}, id {:?})",
            look.image.as_ref().map_or(0, |i| i.width),
            look.image.as_ref().map_or(0, |i| i.height),
            if depth == 32 { "ARGB" } else { "opaque" },
            look.id(w)
        );
        self.told.insert(container, look.clone());
        self.send(Event::Docked { container, look });
        Ok(())
    }

    fn undock(&mut self, w: u32) {
        let Some(icon) = self.icons.remove(&w) else {
            return;
        };
        self.balloons.cancel(w);
        self.open.remove(&w);
        damage::destroy(&self.conn, icon.damage).ok();
        self.conn.destroy_window(icon.container).ok();
        self.conn.flush().ok();
        self.told.remove(&icon.container);
        // the server still owes us this window's events: they are not the next icon's (a window number is
        // used again soon), and neither are the ones for the container that went with them
        let mut below: Vec<XEvent> = Vec::new();
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match &event {
                XEvent::DestroyNotify(e) if e.window == w || e.window == icon.container => {}
                XEvent::UnmapNotify(e) if e.window == w || e.window == icon.container => {}
                XEvent::ReparentNotify(e) if e.window == w || e.window == icon.container => {}
                _ => below.push(event),
            }
        }
        below.reverse();
        self.waiting.extend(below);
        crate::log!(
            "-showwindows: tray icon {w:#x} undocked (its window is gone or was taken away)"
        );
        self.send(Event::Undocked {
            container: icon.container,
        });
    }

    fn xembed_changed(&mut self, w: u32) -> Result<(), AnyError> {
        let info = self.read_info(w)?;
        let Some(icon) = self.icons.get_mut(&w) else {
            return Ok(());
        };
        icon.info = info;
        icon.self_managed = false;
        let shown = icon.shown();
        if shown {
            self.conn.map_window(w)?;
        } else {
            self.conn.unmap_window(w)?;
        }
        self.conn.flush()?;
        let look = self.look(w);
        self.change(w, look);
        Ok(())
    }

    // ------------------------------------------------------------------ looks

    /// What the icon in `w` looks like now: its name, its class, its pixels.
    fn look(&self, w: u32) -> Look {
        let title = self.title(w).unwrap_or_default();
        let class = self.class(w);
        let image = self.read_pixels(w);
        Look {
            title,
            class,
            image,
            menu: true,
        }
    }

    /// Tells the desktop about `look` if that is news.
    fn change(&mut self, w: u32, look: Look) {
        let Some(icon) = self.icons.get(&w) else {
            return;
        };
        let container = icon.container;
        if self.told.get(&container) == Some(&look) {
            return;
        }
        self.told.insert(container, look.clone());
        self.send(Event::Look {
            container,
            look: Box::new(look),
        });
    }

    fn title(&self, w: u32) -> Option<String> {
        for atom in [self.atoms.net_wm_name, self.atoms.wm_name] {
            let r = self
                .conn
                .get_property(false, w, atom, AtomEnum::ANY, 0, 1024)
                .ok()?
                .reply()
                .ok()?;
            let s = String::from_utf8_lossy(&r.value).into_owned();
            if !s.trim().is_empty() {
                return Some(s.trim_end_matches('\0').to_string());
            }
        }
        None
    }

    /// `WM_CLASS`'s class (the second of its two strings), then its instance.
    fn class(&self, w: u32) -> Option<String> {
        let r = self
            .conn
            .get_property(false, w, self.atoms.wm_class, AtomEnum::STRING, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        let mut parts = r
            .value
            .split(|&b| b == 0)
            .map(|p| String::from_utf8_lossy(p).trim().to_string());
        let instance = parts.next().unwrap_or_default();
        let class = parts.next().unwrap_or_default();
        match (class.is_empty(), instance.is_empty()) {
            (false, _) => Some(class),
            (true, false) => Some(instance),
            (true, true) => None,
        }
    }

    /// The icon's pixels, unless they are still those the desktop has.
    fn read_pixels(&self, w: u32) -> Option<Image> {
        let icon = self.icons.get(&w)?;
        let (width, height) = icon.size;
        if width == 0 || height == 0 {
            return None;
        }
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, w, 0, 0, width, height, !0)
            .ok()?
            .reply()
            .ok()?;
        Some(Image {
            width,
            height,
            depth: reply.depth,
            data: reply.data,
        })
    }

    fn read_info(&self, w: u32) -> Result<Option<Vec<u32>>, AnyError> {
        let r = self
            .conn
            .get_property(false, w, self.atoms.xembed_info, AtomEnum::CARDINAL, 0, 2)?
            .reply()?;
        Ok(r.value32().map(|v| v.collect()))
    }

    /// The names and pixels change without a DAMAGE too: once a second.
    fn poll(&mut self) -> Result<(), AnyError> {
        self.flush_pending();
        for w in self.icons.keys().copied().collect::<Vec<_>>() {
            let look = self.look(w);
            self.change(w, look);
        }
        Ok(())
    }

    fn damaged(&mut self, w: u32) {
        if !self.icons.contains_key(&w) {
            return;
        }
        let now = Instant::now();
        if self.due(w, now) {
            let look = self.look(w);
            self.change(w, look);
        } else if !self.pending.contains(&w) {
            self.pending.push(w);
        }
    }

    /// Whether an icon may be read now; if so, its read is marked.
    fn due(&mut self, w: u32, now: Instant) -> bool {
        const INTERVAL: Duration = Duration::from_millis(1000 / MAX_ICON_UPDATES as u64);
        if self
            .read
            .get(&w)
            .is_some_and(|t| now.duration_since(*t) < INTERVAL)
        {
            return false;
        }
        self.read.insert(w, now);
        true
    }

    /// The reads that came too soon: what the poll is for.
    fn flush_pending(&mut self) {
        let now = Instant::now();
        let pending = std::mem::take(&mut self.pending);
        for w in pending {
            if !self.icons.contains_key(&w) {
                continue;
            }
            if self.due(w, now) {
                let look = self.look(w);
                self.change(w, look);
            } else {
                self.pending.push(w);
            }
        }
    }

    // ------------------------------------------------------------------ clicks

    /// A click or a wheel notch on the desktop's icon in `container`, at the desktop position `(x, y)`.
    ///
    /// The container goes to that position (the middle of the icon under the pointer when the call had no
    /// position: the session's pointer, or the top right corner; the screen's top left corner under the top
    /// bar, where no window goes), the container is raised over the agent's window for the length of the
    /// click and lowered again: the icon's menu, if the click opens it, is an override-redirect window the
    /// reverse path shows on the desktop where the app put it (see [`clicks`]).
    fn command(&mut self, command: Command) -> Result<(), AnyError> {
        let container = command.container();
        let Some(icon) = self.icons.values().find(|i| i.container == container) else {
            return Ok(());
        };
        let (w, size) = (icon.window, icon.size);
        let target = clicks::target(size);
        // the click's root point for a container at `(ox, oy)`: the icon's click point inside it
        let inside = |ox: i32, oy: i32| {
            (
                (ox + i32::from(target.0)).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                (oy + i32::from(target.1)).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            )
        };
        #[allow(clippy::type_complexity)]
        let (steps, at, restore): (Vec<Step>, (i32, i32), Option<clicks::Restore>) = match command {
            Command::Click {
                button,
                x: Some(x),
                y: Some(y),
                ..
            } => match clicks::origin(x, y, target, super::scale()) {
                // the click's X point is the desktop's point: the container moves under it
                clicks::Origin::Container(ox, oy) => {
                    let (px, py) = inside(ox, oy);
                    (
                        clicks::click(px, py, button),
                        (ox, oy),
                        Some(clicks::Restore::Window(container, PARK.0, PARK.1)),
                    )
                }
                // under the top bar: the icon stays parked, the click goes to the point itself
                clicks::Origin::Asked(x, y) => (
                    clicks::click(x, y, button),
                    (PARK.0.into(), PARK.1.into()),
                    Some(clicks::Restore::Pointer(x, y, w)),
                ),
            },
            Command::Scroll {
                steps,
                horizontal,
                x: Some(x),
                y: Some(y),
                ..
            } => match clicks::origin(x, y, target, super::scale()) {
                clicks::Origin::Container(ox, oy) => {
                    let (px, py) = inside(ox, oy);
                    (
                        clicks::scroll(px, py, steps, horizontal),
                        (ox, oy),
                        Some(clicks::Restore::Window(container, PARK.0, PARK.1)),
                    )
                }
                clicks::Origin::Asked(x, y) => (
                    clicks::scroll(x, y, steps, horizontal),
                    (PARK.0.into(), PARK.1.into()),
                    Some(clicks::Restore::Pointer(x, y, w)),
                ),
            },
            // no position in the call: the icon is in front of the pointer, where the desktop's pointer is
            Command::Click { button, .. } => {
                let at = self.pointer();
                let (px, py) = inside(at.0, at.1);
                (clicks::click(px, py, button), at, None)
            }
            Command::Scroll {
                steps, horizontal, ..
            } => {
                let at = self.pointer();
                let (px, py) = inside(at.0, at.1);
                (clicks::scroll(px, py, steps, horizontal), at, None)
            }
        };
        let _ = w;
        crate::vlog!(
            "-showwindows: tray: click {:?}: container to {:?}, {} step(s)",
            command_kind(&command),
            at,
            steps.len()
        );
        self.conn.configure_window(
            container,
            &ConfigureWindowAux::new()
                .x(at.0)
                .y(at.1)
                .stack_mode(StackMode::ABOVE),
        )?;
        self.conn.flush()?;
        let result = self.play(&steps);
        // the icon goes back out of the way (and so does the pointer)
        if let Some(restore) = restore {
            match restore {
                clicks::Restore::Window(w, x, y) => {
                    self.conn.configure_window(
                        w,
                        &ConfigureWindowAux::new()
                            .x(i32::from(x))
                            .y(i32::from(y))
                            .stack_mode(StackMode::BELOW),
                    )?;
                    self.conn.flush()?;
                }
                clicks::Restore::Pointer(x, y, icon) => {
                    // the pointer cannot be put back where no window is: a 1x1 click-through window
                    // on the point for the length of the motion
                    if let Err(e) = self.point_at(icon, x, y) {
                        crate::vlog!(
                            "-showwindows: tray: the pointer stays where it was clicked: {e}"
                        );
                    }
                }
            }
        }
        result
    }

    /// Puts the X pointer at root `(x, y)`, under the top bar included: a 1x1 window over the point takes
    /// the motion (XTest's absolute motion is confined to the windows there are: a point no window covers,
    /// such as the desktop's top bar, takes none) and is gone again before the button goes down.
    fn point_at(&self, icon: u32, x: i16, y: i16) -> Result<(), AnyError> {
        let point = self.conn.generate_id()?;
        self.conn.create_window(
            x11rb::COPY_FROM_PARENT as u8,
            point,
            self.root,
            x,
            y,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
        )?;
        self.conn.map_window(point)?;
        self.conn.flush()?;
        for _ in 0..50 {
            if self.conn.query_pointer(self.root)?.reply()?.child == point {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = icon;
        let result = xtest::fake_input(
            &self.conn,
            MOTION_NOTIFY,
            0,
            x11rb::CURRENT_TIME,
            self.root,
            x,
            y,
            0,
        );
        self.conn.destroy_window(point)?;
        self.conn.flush()?;
        result?;
        Ok(())
    }

    /// The desktop's pointer, where the session shows it, or the top right corner of the primary monitor
    /// (the status icons' end) without the extension.
    fn pointer(&self) -> (i32, i32) {
        // the extension (and the fallback) answer in the desktop's logical
        // pixels; the XTest click is X pixels
        let (x, y) = crate::xvfb::tray::pointer::point();
        let s = super::scale() as i32;
        (x.saturating_mul(s), y.saturating_mul(s))
    }

    /// The XTest events of a click: this connection's XTest devices, which are the reverse master's when
    /// the tray has one (see [`Manager::connect`]) — never sent to the desktop, which has its own device
    /// (`xvfb::attrib`: the agent's own input stays on this display).
    fn play(&self, steps: &[Step]) -> Result<(), AnyError> {
        for step in steps {
            let (type_, detail, x, y) = match *step {
                Step::Move(x, y) => (MOTION_NOTIFY, 0, x, y),
                Step::Press(b) => (BUTTON_PRESS, b, 0, 0),
                Step::Release(b) => (BUTTON_RELEASE, b, 0, 0),
            };
            // a motion is absolute (the root window given): to the step's root point
            let root = if type_ == MOTION_NOTIFY { self.root } else { 0 };
            xtest::fake_input(
                &self.conn,
                type_,
                detail,
                x11rb::CURRENT_TIME,
                root,
                x,
                y,
                0,
            )?;
        }
        self.conn.flush()?;
        std::thread::sleep(Duration::from_millis(20));
        Ok(())
    }
}

/// Waits until `fd` is readable or `timeout` passes.
fn wait(fd: std::os::fd::RawFd, timeout: Duration) {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd
    unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
}

/// A short name of a command, for the log.
fn command_kind(command: &Command) -> String {
    match command {
        Command::Click { button, x, y, .. } => format!("button {button} at {x:?},{y:?}"),
        Command::Scroll { steps, x, y, .. } => format!("scroll {steps} at {x:?},{y:?}"),
    }
}
