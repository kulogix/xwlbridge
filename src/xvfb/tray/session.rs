//! The session bus side of the tray: one item and one menu per icon, and the tray's names.
//!
//! One thread owns the connection (the pattern of [`crate::xvfb::winsync::bus`]): it exports the well-known
//! names, the items and their menus, and keeps what it exports in step with the tray ([`Event`]). What the
//! desktop does with an icon arrives as a call on that icon's item or menu, which turns it into a
//! [`Command`] for the tray thread, at the position the call carried.
//!
//! # Names
//!
//! The first free of [`candidates`] (`org.kde.StatusNotifierItem-<pid>-<n>`), the objects
//! [`item_path`]`(container)` and [`menu_path`]`(container)` per icon. Each item is registered with the
//! StatusNotifierWatcher — again whenever the watcher's owner changes, and once it appears (the watcher is
//! looked for every tick) — and unregistered by dropping its name when its icon goes. Without a watcher
//! (GNOME without the AppIndicator extension) the items are exported all the same, and one log line says
//! that nothing shows them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::names::BusName;

use super::clicks::Command;
use super::item::{self, Shared};
use super::manager::Event;
use super::menu::Menu;
use super::notify;
use crate::xvfb::tray::Look;

pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
pub const WATCHER_PATH: &str = "/StatusNotifierWatcher";
pub const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";

/// How long the bus thread waits for the tray before it looks at the watcher again.
pub const TICK: Duration = Duration::from_millis(500);

/// The object of the icon docked in `container`.
pub fn item_path(container: u32) -> String {
    format!("/StatusNotifierItem/{container}")
}

/// The menu object of the icon docked in `container`.
pub fn menu_path(container: u32) -> String {
    format!("/StatusNotifierItem/{container}/Menu")
}

/// The names an item may take, in order (the first free one is used).
pub fn candidates(pid: u32) -> [String; 3] {
    std::array::from_fn(|i| format!("org.kde.StatusNotifierItem-{pid}-{}", i + 1))
}

/// A running bus thread, which ends with the tray's events (or when dropped).
pub struct Session {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // the thread is waiting on the tray: it hears the next clock, sees the flag and lets go
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts the bus side: the tray's `events` (none: nothing to show, the thread ends at once) and its
/// `commands`, on the session bus at `address` (`None`: the session's own).
pub fn start(
    events: Option<Receiver<Event>>,
    address: Option<String>,
    commands: Sender<Command>,
) -> Session {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let thread = std::thread::Builder::new()
        .name("tray-bus".into())
        .spawn(move || {
            let Some(events) = events else { return };
            let conn = match connect(address.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    crate::warning!("-showwindows: tray icons: no session bus ({e})");
                    return;
                }
            };
            Bus::new(conn, commands).run(events, &stop_thread);
        })
        .ok();
    Session { stop, thread }
}

/// Connects to the session bus (or the one at `address`: the tests' private bus).
pub fn connect(address: Option<&str>) -> zbus::Result<Connection> {
    let b = match address {
        Some(a) => zbus::blocking::connection::Builder::address(a)?,
        None => zbus::blocking::connection::Builder::session()?,
    };
    b.method_timeout(Duration::from_secs(5)).build()
}

/// The unique name of the StatusNotifierWatcher's owner, if there is one.
#[allow(dead_code)]
pub fn watcher(conn: &Connection) -> Option<String> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
    let name = BusName::try_from(WATCHER).ok()?;
    dbus.get_name_owner(name).ok().map(|o| o.to_string())
}

/// One exported icon.
struct Icon {
    shared: Arc<Shared>,
    /// The look the desktop has of it.
    #[allow(dead_code)]
    sent: Look,
}

struct Bus {
    conn: Connection,
    /// Our well-known name, once taken.
    name: Option<String>,
    items: HashMap<u32, Icon>,
    /// The watcher our items were registered with.
    registered: Option<String>,
    commands: Sender<Command>,
    reported: bool,
}

impl Bus {
    fn new(conn: Connection, commands: Sender<Command>) -> Bus {
        Bus {
            conn,
            name: None,
            items: HashMap::new(),
            registered: None,
            commands,
            reported: false,
        }
    }

    fn run(mut self, events: Receiver<Event>, stop: &AtomicBool) {
        match self.acquire() {
            Some(name) => crate::log!("-showwindows: tray icons: {name}"),
            None => crate::warning!(
                "-showwindows: tray icons: no free org.kde.StatusNotifierItem-<pid>-<n> name on the session bus"
            ),
        }
        let mut watcher = session_watcher(&self.conn);
        if watcher.is_none() {
            self.no_watcher();
        }
        loop {
            self.register(watcher.as_deref());
            match events.recv_timeout(TICK) {
                Ok(event) => self.event(event),
                Err(RecvTimeoutError::Timeout) if stop.load(Ordering::SeqCst) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            loop {
                match events.try_recv() {
                    Ok(event) => self.event(event),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
            if stop.load(Ordering::SeqCst) {
                break;
            }
            // the watcher coming or going: the items register with the new one
            let now = session_watcher(&self.conn);
            if now != watcher {
                watcher = now;
                self.registered = None;
            }
        }
        self.items.clear();
    }

    fn acquire(&mut self) -> Option<String> {
        for name in candidates(std::process::id()) {
            if self.conn.request_name(name.as_str()).is_ok() {
                self.name = Some(name.clone());
                return Some(name);
            }
        }
        None
    }

    fn no_watcher(&mut self) {
        if !self.reported {
            self.reported = true;
            crate::log!(
                "-showwindows: no StatusNotifierWatcher on the session bus: the tray icons of X apps are docked but nothing shows them (GNOME needs the AppIndicator extension, gnome-shell-extension-appindicator); they appear as soon as one is there"
            );
        }
    }

    /// Registers every item with the watcher, once per watcher.
    fn register(&mut self, watcher: Option<&str>) {
        let Some(watcher) = watcher else { return };
        if self.registered.as_deref() == Some(watcher) {
            return;
        }
        if self.name.is_none() {
            return;
        }
        let Ok(proxy) =
            zbus::blocking::Proxy::new(&self.conn, watcher, WATCHER_PATH, WATCHER_IFACE)
        else {
            return;
        };
        self.registered = Some(watcher.to_string());
        for container in self.items.keys().copied().collect::<Vec<u32>>() {
            if let Err(e) = proxy.call::<_, _, ()>(
                "RegisterStatusNotifierItem",
                &(item_path(container).as_str(),),
            ) {
                crate::vlog!("-showwindows: tray: can't register an icon, {e}");
            }
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Docked { container, look } => self.dock(container, look),
            Event::Look { container, look } => self.look(container, *look),
            Event::Undocked { container } => self.undock(container),
            Event::Notify {
                container,
                title,
                text,
                timeout,
            } => self.notify(container, &title, &text, timeout),
        }
    }

    fn dock(&mut self, container: u32, look: Look) {
        let shared = Shared::new(container, look.clone(), self.commands.clone());
        let (ipath, mpath) = (item_path(container), menu_path(container));
        {
            let server = self.conn.object_server();
            let _ = server.at(&*ipath, item::Item::new(shared.clone()));
            let _ = server.at(&*mpath, Menu::new(shared.clone()));
        }
        self.items.insert(container, Icon { shared, sent: look });
        let watcher = session_watcher(&self.conn);
        self.registered = None;
        self.register(watcher.as_deref());
    }

    fn look(&mut self, container: u32, look: Look) {
        if let Some(icon) = self.items.get_mut(&container) {
            icon.shared.set_look(look);
        }
    }

    fn undock(&mut self, container: u32) {
        self.items.remove(&container);
    }

    fn notify(&mut self, container: u32, title: &str, text: &str, timeout: i32) {
        let _ = container;
        if let Err(e) = notify::send(title, title, text, timeout) {
            crate::vlog!("-showwindows: tray: no notification ({e}): {title}: {text}");
        }
    }
}

/// [`watcher`] (a free function here so that the names above read the same everywhere).
fn session_watcher(conn: &Connection) -> Option<String> {
    watcher(conn)
}

/// The tray's icons on a private bus, for the tests: a `dbus-daemon` with a watcher and the icon of one
/// tray.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    /// A private session bus (`dbus-daemon --session --print-address`).
    pub struct TestBus {
        child: Child,
        pub address: String,
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    pub fn bus() -> Option<TestBus> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let address = line.trim().to_string();
        (!address.is_empty()).then_some(TestBus { child, address })
    }

    #[test]
    fn the_watcher_is_found_when_it_is_there() {
        let Some(bus) = bus() else {
            eprintln!("no dbus-daemon: skipped");
            return;
        };
        let conn = connect(Some(&bus.address)).unwrap();
        assert_eq!(watcher(&conn), None);
    }
}
