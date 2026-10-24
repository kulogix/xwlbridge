//! The session bus side of the tray: one item and one menu per icon, and the tray's names.
//!
//! One thread owns the tray's connection (the pattern of [`crate::xvfb::winsync::bus`]): it keeps what is
//! exported in step with the tray ([`Event`]). What the desktop does with an icon arrives as a call on that
//! icon's item or menu, which turns it into a [`Command`] for the tray thread, at the position the call
//! carried.
//!
//! # One connection per icon
//!
//! Each icon is exported on a connection of its own, which owns the icon's well-known name
//! (`org.kde.StatusNotifierItem-<pid>-<n>`, [`candidates`]) and serves the objects [`ITEM_PATH`] and
//! [`MENU_PATH`]. The StatusNotifierWatcher knows an item by its owner's *unique* bus name plus the object
//! path, so a connection per icon is what makes an item go away: dropping the connection (the icon undocked)
//! is a `NameOwnerChanged` the desktop sees, and GNOME's AppIndicator extension destroys the icon with it.
//! (The watcher has no `UnregisterStatusNotifierItem`, and releasing a well-known name does not change the
//! unique name's owner, so one connection for all icons would leave every exited icon in the top bar.)
//!
//! Each item is registered with the StatusNotifierWatcher — again whenever the watcher's owner changes, and
//! once it appears (the watcher is looked for every tick). Without a watcher (GNOME without the AppIndicator
//! extension) the items are exported all the same, and one log line says that nothing shows them.

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

/// The object every icon is exported at (each on its own connection, see the module comment).
pub const ITEM_PATH: &str = "/StatusNotifierItem";

/// The object of every icon's menu.
pub const MENU_PATH: &str = "/StatusNotifierItem/Menu";

/// The names an item may take, in order (the first free one is used per icon).
pub fn candidates(pid: u32) -> impl Iterator<Item = String> {
    (1..).map(move |n| format!("org.kde.StatusNotifierItem-{pid}-{n}"))
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
            Bus::new(conn, address, commands).run(events, &stop_thread);
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
    /// The icon's own connection: it owns the icon's well-known name and its unique name is what the
    /// watcher knows the item by, so ending the connection takes the icon off the desktop.
    conn: Connection,
    shared: Arc<Shared>,
    /// The well-known name it owns (`org.kde.StatusNotifierItem-<pid>-<n>`).
    #[allow(dead_code)]
    name: String,
    /// The look the desktop has of it.
    #[allow(dead_code)]
    sent: Look,
}

struct Bus {
    /// The tray's own connection: the watcher is looked for on it.
    conn: Connection,
    /// The bus the icons get connections of their own on (`None`: the session's own).
    address: Option<String>,
    items: HashMap<u32, Icon>,
    /// The watcher our items were registered with.
    registered: Option<String>,
    commands: Sender<Command>,
    reported: bool,
}

impl Bus {
    fn new(conn: Connection, address: Option<String>, commands: Sender<Command>) -> Bus {
        Bus {
            conn,
            address,
            items: HashMap::new(),
            registered: None,
            commands,
            reported: false,
        }
    }

    fn run(mut self, events: Receiver<Event>, stop: &AtomicBool) {
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
        self.registered = Some(watcher.to_string());
        for (container, icon) in &self.items {
            register_one(*container, icon, watcher);
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
        // the icon's own connection: the watcher knows the item by its unique name, which goes with it
        let conn = match connect(self.address.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                crate::warning!(
                    "-showwindows: tray: can't export icon {container:#x}: no session bus ({e})"
                );
                return;
            }
        };
        let Some(name) = free_name(&conn) else {
            crate::warning!(
                "-showwindows: tray: no free org.kde.StatusNotifierItem-<pid>-<n> name for icon {container:#x}"
            );
            return;
        };
        let shared = Shared::new(container, look.clone(), self.commands.clone());
        {
            let server = conn.object_server();
            let _ = server.at(ITEM_PATH, item::Item::new(shared.clone()));
            let _ = server.at(MENU_PATH, Menu::new(shared.clone()));
        }
        self.items.insert(
            container,
            Icon {
                conn,
                shared,
                name: name.clone(),
                sent: look,
            },
        );
        crate::vlog!("-showwindows: tray icon {container:#x} exported as {name}");
        if let Some(watcher) = session_watcher(&self.conn)
            && let Some(icon) = self.items.get(&container)
        {
            register_one(container, icon, &watcher);
        }
    }

    fn look(&mut self, container: u32, look: Look) {
        if let Some(icon) = self.items.get_mut(&container) {
            icon.shared.set_look(look);
        }
    }

    fn undock(&mut self, container: u32) {
        // dropping the icon's connection ends its unique name: the watcher (and the desktop) drops the item
        if let Some(icon) = self.items.remove(&container) {
            crate::vlog!(
                "-showwindows: tray icon {container:#x} unexported ({})",
                icon.name
            );
        }
    }

    fn notify(&mut self, container: u32, title: &str, text: &str, timeout: i32) {
        let _ = container;
        if let Err(e) = notify::send(title, title, text, timeout) {
            crate::vlog!("-showwindows: tray: no notification ({e}): {title}: {text}");
        }
    }
}

/// The first free `org.kde.StatusNotifierItem-<pid>-<n>` on `conn`.
fn free_name(conn: &Connection) -> Option<String> {
    // no AllowReplacement/ReplaceExisting (the defaults of request_name): two icons would take the same
    // name from each other; DoNotQueue: a name that is taken is the next one's turn
    let flags = zbus::fdo::RequestNameFlags::DoNotQueue.into();
    candidates(std::process::id())
        .find(|name| conn.request_name_with_flags(name.as_str(), flags).is_ok())
}

/// Registers one icon's item with `watcher` from the icon's own connection (which is the item's service).
fn register_one(container: u32, icon: &Icon, watcher: &str) {
    let Ok(proxy) = zbus::blocking::Proxy::new(&icon.conn, watcher, WATCHER_PATH, WATCHER_IFACE)
    else {
        return;
    };
    if let Err(e) = proxy.call::<_, _, ()>("RegisterStatusNotifierItem", &(ITEM_PATH,)) {
        crate::vlog!("-showwindows: tray: can't register icon {container:#x}, {e}");
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

    /// A fake StatusNotifierWatcher: it records every registration (its name is what the tray looks for).
    struct FakeWatcher {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeWatcher {
        fn register_status_notifier_item(&self, service: &str) {
            self.calls.lock().unwrap().push(service.to_string());
        }
    }

    fn serve_watcher(conn: &Connection, calls: Arc<std::sync::Mutex<Vec<String>>>) {
        conn.object_server()
            .at(WATCHER_PATH, FakeWatcher { calls })
            .unwrap();
        conn.request_name(WATCHER).unwrap();
        assert!(watcher(conn).is_some());
    }

    fn owner_of(conn: &Connection, name: &str) -> Option<String> {
        let dbus = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
        dbus.get_name_owner(BusName::try_from(name).ok()?)
            .ok()
            .map(|o| o.to_string())
    }

    #[test]
    fn an_icon_is_registered_from_its_own_connection_that_ends_with_it() {
        let Some(bus) = bus() else {
            eprintln!("no dbus-daemon: skipped");
            return;
        };
        let watcher_conn = connect(Some(&bus.address)).unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        serve_watcher(&watcher_conn, calls.clone());
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut tray = Bus::new(
            connect(Some(&bus.address)).unwrap(),
            Some(bus.address.clone()),
            tx,
        );
        tray.dock(0x1234, Look::default());
        tray.dock(0x5678, Look::default());
        // each icon registers the same object path, from a connection of its own
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[ITEM_PATH.to_string(), ITEM_PATH.to_string()]
        );
        let names: Vec<String> = [&tray.items[&0x1234], &tray.items[&0x5678]]
            .iter()
            .map(|icon| icon.name.clone())
            .collect();
        assert_ne!(names[0], names[1]);
        for name in &names {
            assert!(owner_of(&watcher_conn, name).is_some(), "{name}");
        }
        let (first_name, first_owner) = (names[0].clone(), owner_of(&watcher_conn, &names[0]));
        // undocking ends the icon's connection: its name goes (the watcher then drops the item)
        tray.undock(0x1234);
        let mut gone = false;
        for _ in 0..150 {
            if owner_of(&watcher_conn, &first_name) != first_owner {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            gone,
            "the icon's connection is still on the bus as {first_name}"
        );
        assert!(owner_of(&watcher_conn, &names[1]).is_some());
    }
}
