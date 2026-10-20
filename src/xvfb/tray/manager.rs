//! The tray manager: `_NET_SYSTEM_TRAY_S<screen>` on the engine display.
//!
//! One thread owns a connection of its own to the engine's X server (x11rb, like the rest of the agent). It
//! takes the tray selection, docks the icons that ask for it, and follows them: their DAMAGE (new pixels),
//! their `_XEMBED_INFO` and their names. What the desktop needs to know goes to the bus side as
//! [`Update`]s; what the desktop does to an icon comes back as a [`Command`], which this thread turns into
//! XTest on the icon (see [`super::clicks`]).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::damage;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, MapState, PropMode, StackMode, WindowClass,
};
use x11rb::protocol::{Event, xtest};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::balloons::Balloons;
use super::clicks::{self, Command, Step};
use super::dock::{CONTAINER, Icon, PARK};
use super::pixels::Image;
use super::{ICON_SIZE, MAX_ICON_UPDATES, selection_name};
use crate::bridge::input::{BUTTON_PRESS, BUTTON_RELEASE, MOTION_NOTIFY};
use crate::xvfb::{self, AnyError, Auth};

/// How often an icon's name is checked without a DAMAGE (a rename sends none).
const POLL: Duration = Duration::from_secs(1);

// `_NET_SYSTEM_TRAY_OPCODE`'s message types.
const REQUEST_DOCK: u32 = 0;
const BEGIN_MESSAGE: u32 = 1;
const MESSAGE_DATA: u32 = 2;
const CANCEL_MESSAGE: u32 = 3;

/// What the manager tells the bus side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// We own the selection.
    Started,
    /// An icon was docked (its container window).
    Docked(u32),
    /// The icon's pixels or its name changed.
    Changed(u32),
    /// The icon is gone.
    Undocked(u32),
    /// A balloon message, whole.
    Message {
        container: u32,
        timeout: i32,
        text: String,
    },
    /// The tray stopped (the message says why, if that was not asked for).
    Stopped(Option<String>),
}

/// What an icon looks like now, for the bus side.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub title: String,
    pub class: Option<String>,
    pub pixmap: Option<Image>,
}

/// The click handle the bus side uses.
#[derive(Clone, Default)]
pub struct Handle(Arc<HandleInner>);

#[derive(Default)]
struct HandleInner {
    commands: std::sync::Mutex<Option<Sender<Command>>>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tray::Handle")
    }
}

impl Handle {
    fn send(&self, command: Command) {
        if let Ok(commands) = self.0.commands.lock()
            && let Some(tx) = commands.as_ref()
        {
            let _ = tx.send(command);
        }
    }

    /// A click of `button` (1 left, 2 middle, 3 right) on the icon in `container`.
    pub fn click(&self, container: u32, button: u8, double: bool) {
        self.send(Command::Click {
            container,
            button,
            double,
        });
    }

    /// `steps` notches of the wheel over the icon.
    #[allow(dead_code)]
    pub fn scroll(&self, container: u32, steps: i32, horizontal: bool) {
        self.send(Command::Scroll {
            container,
            steps,
            horizontal,
        });
    }
}

/// A running tray: its handle, and the updates for the bus side.
pub struct Running {
    pub handle: Handle,
    pub updates: Receiver<Update>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts the tray on `display` (the agent's X server), for `screen`.
pub fn spawn(display: u32, auth: Auth, screen: u32) -> std::io::Result<Running> {
    let (tx, commands) = channel();
    let (updates, rx) = channel();
    let handle = Handle::default();
    if let Ok(mut c) = handle.0.commands.lock() {
        *c = Some(tx);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread =
        {
            let stop = stop.clone();
            let updates = updates.clone();
            std::thread::Builder::new().name("tray".into()).spawn(
                move || match Manager::connect(display, &auth, screen, updates.clone()) {
                    Ok(mut m) => {
                        let reason = m.run(&commands, &stop);
                        let _ = updates.send(Update::Stopped(reason));
                    }
                    Err(e) => {
                        let _ = updates.send(Update::Stopped(Some(format!("{e}"))));
                    }
                },
            )?
        };
    Ok(Running {
        handle,
        updates: rx,
        stop,
        thread: Some(thread),
    })
}

/// The atoms the tray uses.
struct Atoms {
    manager: u32,
    selection: u32,
    opcode: u32,
    orientation: u32,
    icon_size: u32,
    xembed_info: u32,
    wm_name: u32,
    wm_class: u32,
    container: u32,
}

impl Atoms {
    fn new(conn: &RustConnection, screen: u32) -> Result<Atoms, AnyError> {
        let atom = |name: &str| -> Result<u32, AnyError> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        Ok(Atoms {
            manager: atom("MANAGER")?,
            selection: atom(&selection_name(screen))?,
            opcode: atom("_NET_SYSTEM_TRAY_OPCODE")?,
            orientation: atom("_NET_SYSTEM_TRAY_ORIENTATION")?,
            icon_size: atom("_NET_SYSTEM_TRAY_ICON_SIZE")?,
            xembed_info: atom("_XEMBED_INFO")?,
            wm_name: atom("WM_NAME")?,
            wm_class: atom("WM_CLASS")?,
            container: atom(CONTAINER)?,
        })
    }
}

struct Manager {
    conn: RustConnection,
    root: u32,
    window: u32,
    atoms: Atoms,
    icons: HashMap<u32, Icon>,
    balloons: Balloons,
    updates: Sender<Update>,
    last_poll: Instant,
    /// When each icon's DAMAGE last reached the bus side (see [`MAX_ICON_UPDATES`]).
    read: HashMap<u32, Instant>,
    /// Icons whose DAMAGE came too soon for another read: their read is due.
    pending: Vec<u32>,
    /// The 32-bit ARGB visual to give a container whose icon has one, and the screen's default one.
    argb: Option<u32>,
    default_visual: u32,
}

impl Manager {
    fn connect(
        display: u32,
        auth: &Auth,
        screen: u32,
        updates: Sender<Update>,
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
        let manager = Manager {
            conn,
            root,
            window: 0,
            atoms,
            icons: HashMap::new(),
            balloons: Balloons::new(),
            updates,
            last_poll: Instant::now(),
            read: HashMap::new(),
            pending: Vec::new(),
            argb,
            default_visual,
        };
        Ok(manager)
    }

    fn send(&self, u: Update) {
        let _ = self.updates.send(u);
    }

    /// Takes the selection and runs until stopped. `Some(reason)` if it could not go on.
    fn run(&mut self, commands: &Receiver<Command>, stop: &AtomicBool) -> Option<String> {
        if let Err(e) = self.take_selection() {
            return Some(format!("{e}"));
        }
        let fd = self.conn.stream().as_raw_fd();
        loop {
            if stop.load(Ordering::SeqCst) {
                return None;
            }
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(event)) => {
                        if let Err(e) = self.event(event) {
                            crate::vlog!("-showwindows: tray: {e}");
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Some(format!("{e}")),
                }
            }
            if self.last_poll.elapsed() >= POLL {
                self.last_poll = Instant::now();
                if let Err(e) = self.poll() {
                    crate::vlog!("-showwindows: tray: {e}");
                }
            }
            self.conn.flush().ok();
            wait(fd, Duration::from_millis(250));
            loop {
                match commands.try_recv() {
                    Ok(command) => {
                        if let Err(e) = self.command(command) {
                            crate::vlog!("-showwindows: tray: {e}");
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return None,
                }
            }
        }
    }

    // ------------------------------------------------------------------ selection

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
            let atom = self
                .conn
                .intern_atom(false, b"_NET_SYSTEM_TRAY_VISUAL")?
                .reply()?
                .atom;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                atom,
                AtomEnum::VISUALID,
                &[visual],
            )?;
        }
        if let Some(argb) = self.argb {
            let visual = self
                .conn
                .intern_atom(false, b"_NET_SYSTEM_TRAY_VISUAL")?
                .reply()?
                .atom;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                visual,
                AtomEnum::VISUALID,
                &[argb],
            )?;
        }
        // tell every current client there is a tray (ICCCM: MANAGER, SelectionTime, owner)
        let mut data = [0u32; 5];
        data[..4].copy_from_slice(&[x11rb::CURRENT_TIME, self.atoms.selection, self.window, 0]);
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
        self.send(Update::Started);
        Ok(())
    }

    // ------------------------------------------------------------------ events

    fn event(&mut self, event: Event) -> Result<(), AnyError> {
        match event {
            Event::ClientMessage(e) if e.type_ == self.atoms.opcode => self.client_message(&e)?,
            Event::DestroyNotify(e) => self.undock(e.window),
            Event::UnmapNotify(e) => {
                let ours = self.icons.contains_key(&e.window);
                let unmapped = self
                    .conn
                    .get_window_attributes(e.window)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|a| a.map_state == MapState::UNMAPPED);
                if ours && unmapped {
                    self.undock(e.window);
                }
            }
            Event::ReparentNotify(e) => {
                if self
                    .icons
                    .get(&e.window)
                    .is_some_and(|i| e.parent != i.container)
                {
                    self.undock(e.window);
                }
            }
            Event::PropertyNotify(e) if e.atom == self.atoms.xembed_info => {
                if self.icons.contains_key(&e.window) {
                    self.xembed_changed(e.window)?;
                }
            }
            Event::DamageNotify(e) => self.damaged(e.damage),
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
                self.balloons.begin(icon, &header);
            }
            MESSAGE_DATA => {
                let mut chunk = Vec::with_capacity(20);
                for v in &data[3..] {
                    chunk.extend_from_slice(&v.to_ne_bytes());
                }
                if let Some((header, text)) = self.balloons.data(icon, &chunk)
                    && let Some(icon) = self.icons.get(&icon)
                {
                    let container = icon.container;
                    self.send(Update::Message {
                        container,
                        timeout: header.timeout,
                        text,
                    });
                }
            }
            CANCEL_MESSAGE => self.balloons.cancel(icon),
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
        self.conn
            // keep the window's contents (and its events) when we go
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
        icon.info = self.read_info(w)?;
        let shown = icon.shown();
        self.icons.insert(w, icon);
        if shown {
            self.conn.map_window(w)?;
        }
        self.conn.flush()?;
        self.send(Update::Docked(container));
        Ok(())
    }

    fn undock(&mut self, w: u32) {
        if let Some(icon) = self.icons.remove(&w) {
            damage::destroy(&self.conn, icon.damage).ok();
            self.conn.destroy_window(icon.container).ok();
            self.conn.flush().ok();
            self.send(Update::Undocked(icon.container));
        }
    }

    fn xembed_changed(&mut self, w: u32) -> Result<(), AnyError> {
        let info = self.read_info(w)?;
        let Some(icon) = self.icons.get_mut(&w) else {
            return Ok(());
        };
        icon.info = info;
        let shown = icon.shown();
        if shown {
            self.conn.map_window(w)?;
        } else {
            self.conn.unmap_window(w)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn poll(&mut self) -> Result<(), AnyError> {
        self.flush_pending();
        Ok(())
    }

    /// A click or a wheel notch on the desktop's icon in `container`.
    ///
    /// The container goes above the agent's full-screen window (the only way the pointer can enter the icon
    /// at all), the pointer is moved to the middle of the icon and the buttons are pressed there, and the
    /// container goes back down: from then on the icon's own menu, if it has one, is an override-redirect
    /// window the reverse path shows on the desktop like any other popup.
    fn command(&mut self, command: Command) -> Result<(), AnyError> {
        let (container, steps) = match command {
            Command::Click {
                container,
                button,
                double,
            } => (container, clicks::click(button, double)),
            Command::Scroll {
                container,
                steps,
                horizontal,
            } => (container, clicks::scroll(steps, horizontal)),
        };
        let Some(icon) = self.icons.values().find(|i| i.container == container) else {
            return Ok(());
        };
        let (x, y) = clicks::middle(PARK.0, PARK.1, icon.size.0, icon.size.1);
        let pointer = self.conn.query_pointer(self.root)?.reply().ok();
        self.conn.configure_window(
            container,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        let button = |type_: u8, detail: u8| -> Result<(), AnyError> {
            xtest::fake_input(
                &self.conn,
                type_,
                detail,
                x11rb::CURRENT_TIME,
                self.root,
                x,
                y,
                0,
            )?;
            self.conn.flush()?;
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        };
        button(MOTION_NOTIFY, 0)?;
        for step in steps {
            match step {
                Step::Move(..) => {}
                Step::Press(b) => button(BUTTON_PRESS, b)?,
                Step::Release(b) => button(BUTTON_RELEASE, b)?,
            }
        }
        // the user's pointer goes back where it was, the icons back below
        if let Some(p) = pointer {
            self.conn
                .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, p.root_x, p.root_y)?;
        }
        self.conn.configure_window(
            container,
            &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn read_info(&self, w: u32) -> Result<Option<Vec<u32>>, AnyError> {
        let r = self
            .conn
            .get_property(false, w, self.atoms.xembed_info, AtomEnum::CARDINAL, 0, 2)?
            .reply()?;
        Ok(r.value32().map(|v| v.collect()))
    }

    /// An icon's pixels changed: the bus side hears about it when a read is due (at most
    /// [`MAX_ICON_UPDATES`] a second), once more for each icon whose read was too soon.
    fn damaged(&mut self, w: u32) {
        let now = Instant::now();
        if self.due(w, now) {
            if let Some(icon) = self.icons.get(&w) {
                let container = icon.container;
                self.send(Update::Changed(container));
            }
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
                let container = self.icons[&w].container;
                self.send(Update::Changed(container));
            } else {
                self.pending.push(w);
            }
        }
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
