//! The tray manager: `_NET_SYSTEM_TRAY_S<screen>` on the engine display.
//!
//! One thread owns a connection of its own to the engine's X server (x11rb, like the rest of the agent). It
//! takes the tray selection, docks the icons that ask for it, and follows them: their DAMAGE (new pixels),
//! their `_XEMBED_INFO`, their names, their balloon messages and their menus. What the desktop shows goes to
//! the bus thread as an [`Event`]; what the desktop does to an icon comes back as a [`clicks::Command`],
//! which this thread turns into XTest on the icon ([`clicks`]).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
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
use crate::xvfb::reverse::Devices;
use crate::xvfb::{self, AnyError, Auth};

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
    devs: Option<Devices>,
    bus: Option<String>,
) -> std::io::Result<Running> {
    let (tx, commands) = channel();
    let (ready, started) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                let mut manager = match Manager::connect(display, &auth, screen, devs) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = ready.send(Err(format!("{e}")));
                        return;
                    }
                };
                if let Err(e) = manager.take_selection() {
                    let _ = ready.send(Err(format!("{e}")));
                    return;
                }
                let bus = super::session::start(manager.start_bus(), bus, tx);
                if let Err(e) = ready.send(Ok(())) {
                    let _ = e;
                    manager.release();
                    return;
                }
                manager.run(&commands, &stop);
                drop(bus);
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
    orientation: u32,
    icon_size: u32,
    visual: u32,
    xembed_info: u32,
    net_wm_name: u32,
    wm_name: u32,
    wm_class: u32,
    container: u32,
}

impl Atoms {
    fn new(conn: &RustConnection, screen: u32) -> Result<Atoms, AnyError> {
        let atom = |name: &[u8]| -> Result<u32, AnyError> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        Ok(Atoms {
            manager: atom(b"MANAGER")?,
            selection: atom(selection_name(screen).as_bytes())?,
            opcode: atom(b"_NET_SYSTEM_TRAY_OPCODE")?,
            orientation: atom(b"_NET_SYSTEM_TRAY_ORIENTATION")?,
            icon_size: atom(b"_NET_SYSTEM_TRAY_ICON_SIZE")?,
            visual: atom(b"_NET_SYSTEM_TRAY_VISUAL")?,
            xembed_info: atom(b"_XEMBED_INFO")?,
            net_wm_name: atom(b"_NET_WM_NAME")?,
            wm_name: atom(b"WM_NAME")?,
            wm_class: atom(b"WM_CLASS")?,
            container: atom(CONTAINER.as_bytes())?,
        })
    }
}

/// A balloon being reassembled (the timeout comes with its `BEGIN_MESSAGE`, the text after it).
struct Balloon {
    timeout: i32,
}

struct Manager {
    conn: RustConnection,
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
    /// The 32-bit ARGB visual, and the screen's default one.
    argb: Option<u32>,
    default_visual: u32,
    /// The screen's size, for the clicks' positions.
    screen: (u16, u16),
}

impl Manager {
    fn connect(
        display: u32,
        auth: &Auth,
        screen: u32,
        devs: Option<Devices>,
    ) -> Result<Manager, AnyError> {
        let (conn, screen_num) = xvfb::connect(display, auth)?;
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
        // the desktop's pointer is not ours to move: clicks are made with the reverse direction's device,
        // as every X client of the display gets it (and with our own XTest only without one)
        if let Some(devs) = &devs
            && let Err(e) = conn.xinput_xi_set_client_pointer(x11rb::NONE, devs.pointer)
        {
            crate::vlog!(
                "-showwindows: tray: no reverse pointer ({e}), clicking with the core pointer"
            );
        }
        let geometry = conn
            .get_geometry(root)?
            .reply()
            .map(|g| (g.width, g.height))
            .unwrap_or((0, 0));
        let screen_size = geometry;
        Ok(Manager {
            screen: screen_size,
            conn,
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
    fn run(&mut self, commands: &Receiver<Command>, stop: &AtomicBool) {
        let fd = self.conn.stream().as_raw_fd();
        while !stop.load(Ordering::SeqCst) {
            if let Err(e) = self.pump(commands) {
                crate::vlog!("-showwindows: tray: {e}");
                break;
            }
            wait(fd, Duration::from_millis(100));
        }
        self.release();
    }

    /// Handles what is there, and the commands.
    fn pump(&mut self, commands: &Receiver<Command>) -> Result<(), AnyError> {
        loop {
            match self.conn.poll_for_event() {
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
        let (kind, icon) = (data[1], data[2]);
        match kind {
            REQUEST_DOCK => self.dock(icon)?,
            BEGIN_MESSAGE => {
                let mut header = [0u8; 40];
                for (i, v) in data[3..].iter().enumerate() {
                    header[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
                }
                let h = self.balloons.begin(icon, &header);
                self.open.insert(icon, Balloon { timeout: h.timeout });
            }
            MESSAGE_DATA => {
                let mut chunk = Vec::with_capacity(20);
                for v in &data[3..] {
                    chunk.extend_from_slice(&v.to_ne_bytes());
                }
                if let Some((_, text)) = self.balloons.data(icon, &chunk) {
                    let timeout = self.open.get(&icon).map_or(0, |b| b.timeout);
                    if let Some(container) = self.icons.get(&icon).map(|i| i.container) {
                        let title = self
                            .told
                            .get(&container)
                            .map(|l| l.title.clone())
                            .unwrap_or_default();
                        self.send(Event::Notify {
                            container,
                            title,
                            text,
                            timeout,
                        });
                    }
                }
            }
            CANCEL_MESSAGE => {
                self.balloons.cancel(icon);
                self.open.remove(&icon);
            }
            _ => {}
        }
        Ok(())
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
        self.told.insert(container, look.clone());
        self.send(Event::Docked { container, look });
        Ok(())
    }

    fn undock(&mut self, w: u32) {
        if let Some(icon) = self.icons.remove(&w) {
            self.balloons.cancel(w);
            self.open.remove(&w);
            damage::destroy(&self.conn, icon.damage).ok();
            self.conn.destroy_window(icon.container).ok();
            self.conn.flush().ok();
            self.told.remove(&icon.container);
            self.send(Event::Undocked {
                container: icon.container,
            });
        }
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
        let (w, (width, height)) = (icon.window, icon.size);
        let (px, py) = (i32::from(width) / 2, i32::from(height) / 2);
        let (px, py) = (px as i16, py as i16);
        let (steps, at) = match command {
            Command::Click {
                button,
                x: Some(x),
                y: Some(y),
                ..
            } => {
                let at = self.place(x, y, px, py);
                (clicks::click(px, py, button), at)
            }
            Command::Scroll {
                steps,
                horizontal,
                x: Some(x),
                y: Some(y),
                ..
            } => {
                let at = self.place(x, y, px, py);
                (clicks::scroll(px, py, steps, horizontal), at)
            }
            // no position in the call: the desktop's pointer, where the menu was
            Command::Click { button, .. } => {
                let at = self.pointer();
                (clicks::click(px, py, button), at)
            }
            Command::Scroll {
                steps, horizontal, ..
            } => {
                let at = self.pointer();
                (clicks::scroll(px, py, steps, horizontal), at)
            }
        };
        {
            let _ = w;
            self.conn.configure_window(
                container,
                &ConfigureWindowAux::new()
                    .x(at.0)
                    .y(at.1)
                    .stack_mode(StackMode::ABOVE),
            )?;
            self.conn.flush()?;
            let result = self.play(&steps);
            // the container goes back out of the way, below the agent's window
            let _ = self.conn.configure_window(
                container,
                &ConfigureWindowAux::new()
                    .x(i32::from(PARK.0))
                    .y(i32::from(PARK.1))
                    .stack_mode(StackMode::BELOW),
            );
            self.conn.flush()?;
            result
        }
    }

    /// Where a click at desktop `(x, y)`, `(px, py)` inside the icon, finds its icon: the icon's top left
    /// corner, on the screen (the click's position under the top bar is not a place for a window).
    fn place(&self, x: i32, y: i32, px: i16, py: i16) -> (i32, i32) {
        let (mut ox, mut oy) = clicks::origin(x, y, px.into(), py.into());
        let (sw, sh) = (i32::from(self.screen.0), i32::from(self.screen.1));
        // under the top bar (a negative position): the corner is as close as a window goes
        ox = ox.clamp(0, (sw - i32::from(px) * 2 - 1).max(0));
        oy = oy.clamp(0, (sh - i32::from(py) * 2 - 1).max(0));
        (ox, oy)
    }

    /// The desktop's pointer, where the session shows it, or the top right corner of the primary monitor
    /// (the status icons' end) without the extension.
    fn pointer(&self) -> (i32, i32) {
        crate::xvfb::tray::pointer::point()
    }

    /// The XTest events of a click: the core pointer, whose `ClientPointer` is the reverse direction's
    /// device where there is one — never sent to the desktop, which has its own device
    /// (`xvfb::attrib`: the agent's own input stays on this display).
    fn play(&self, steps: &[Step]) -> Result<(), AnyError> {
        for step in steps {
            let (type_, detail) = match *step {
                Step::Move(..) => (MOTION_NOTIFY, 0),
                Step::Press(b) => (BUTTON_PRESS, b),
                Step::Release(b) => (BUTTON_RELEASE, b),
            };
            xtest::fake_input(
                &self.conn,
                type_,
                detail,
                x11rb::CURRENT_TIME,
                self.root,
                0,
                0,
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
