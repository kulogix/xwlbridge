//! Tracks the compositor keymap over plain Wayland: mutter sends `wl_keyboard`
//! keymaps to any client with a seat, and resends on every layout switch. This
//! gives the X keymap from startup on, without holding a remote-desktop
//! session (which would show gnome-shell's screen-sharing indicator); the EIS
//! keyboard's keymap is used too once a session exists. It also tells when
//! the seat has had a keyboard since, which every app needs before keys reach
//! it (see the gnome backend's `KEYBOARD_SETTLE`).

use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    latest: Option<String>,
    /// Since when the seat has a keyboard (`None`: it has none now). A seat
    /// whose only keyboard is a remote-desktop session's gains and loses it
    /// with each session.
    keyboard_since: Option<Instant>,
}

pub struct KeymapWatch {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    guard: Option<wayland_client::backend::ReadEventsGuard>,
}

impl KeymapWatch {
    /// Connects and waits for the first keymap. `None` without a reachable
    /// Wayland display.
    pub fn connect() -> Option<KeymapWatch> {
        let conn = Connection::connect_to_env().ok()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        // registry -> seat -> capabilities -> keyboard -> keymap
        for _ in 0..4 {
            queue.roundtrip(&mut state).ok()?;
            if state.latest.is_some() {
                break;
            }
        }
        // a keyboard the seat has at our start has been there for apps all
        // along
        if state.keyboard_since.is_some() {
            state.keyboard_since = Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .or(state.keyboard_since);
        }
        Some(KeymapWatch {
            conn,
            queue,
            state,
            guard: None,
        })
    }

    /// A new keymap, if one arrived since the last call.
    pub fn take(&mut self) -> Option<String> {
        self.state.latest.take()
    }

    /// Since when the compositor's seat has had a keyboard, as far as this
    /// connection has heard (`None`: it has none).
    pub fn keyboard_since(&self) -> Option<Instant> {
        self.state.keyboard_since
    }

    /// Readies the socket for the poll loop and returns its fd.
    pub fn prepare(&mut self) -> Option<RawFd> {
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
        if self.guard.is_none() {
            self.guard = self.queue.prepare_read();
        }
        self.guard.as_ref().map(|g| g.connection_fd().as_raw_fd())
    }

    /// Reads (if `readable`) and dispatches. Returns false once the
    /// connection is gone.
    pub fn dispatch(&mut self, readable: bool) -> bool {
        if let Some(g) = self.guard.take()
            && readable
            && g.read().is_err()
        {
            return false;
        }
        self.queue.dispatch_pending(&mut self.state).is_ok()
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wl_seat"
            && state.seat.is_none()
        {
            state.seat = Some(registry.bind(name, version.min(5), qh, ()));
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Keyboard) {
            if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            state.keyboard_since.get_or_insert_with(Instant::now);
        } else {
            // gone with the last keyboard device: the next one (a new
            // session's) comes with a new keymap on a new wl_keyboard
            if let Some(k) = state.keyboard.take()
                && k.version() >= 3
            {
                k.release();
            }
            state.keyboard_since = None;
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event
            && format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
            && let Some(text) = read_keymap(fd, size)
        {
            state.latest = Some(text);
        }
    }
}

fn read_keymap(fd: OwnedFd, size: u32) -> Option<String> {
    let file = std::fs::File::from(fd);
    let mut buf = vec![0u8; size as usize];
    file.read_exact_at(&mut buf, 0).ok()?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// The keymap text in a sealed-off memfd, for [`Input::set_keymap`](crate::bridge::input::Input::set_keymap),
/// which wants an fd (the wlr virtual keyboard sends it on; ours ignores it).
pub fn keymap_fd(text: &str) -> Option<(OwnedFd, u32)> {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let raw = unsafe { libc::memfd_create(c"xwlbridge-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut file = std::fs::File::from(fd.as_fd().try_clone_to_owned().ok()?);
    file.write_all(text.as_bytes()).ok()?;
    file.write_all(&[0]).ok()?;
    Some((fd, text.len() as u32 + 1))
}
