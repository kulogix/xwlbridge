//! mutter's remote-access D-Bus APIs: `org.gnome.Mutter.DisplayConfig` for the
//! monitor layout, and the linked `RemoteDesktop` + `ScreenCast` sessions that
//! give us PipeWire capture streams and an EIS input socket.
//!
//! Every session call must come from the connection that created the session
//! (mutter rejects other senders, and closes the session when its creator's bus
//! name vanishes), so the backend keeps one connection for its whole life.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MsgType;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::Wake;
use crate::bridge::x11::randr::ModeSpec;

pub const RD: &str = "org.gnome.Mutter.RemoteDesktop";
pub const SC: &str = "org.gnome.Mutter.ScreenCast";
pub const DC: &str = "org.gnome.Mutter.DisplayConfig";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";

/// `RecordMonitor` cursor modes.
const CURSOR_MODE_EMBEDDED: u32 = 1;
const CURSOR_MODE_METADATA: u32 = 2;

/// One active monitor, as mutter lays it out.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub connector: String,
    /// Every mode mutter offers for it (the current one flagged), oriented
    /// like `width`/`height`, for X clients to see listed.
    pub modes: Vec<ModeSpec>,
    /// Logical rectangle in the compositor's layout.
    pub lx: i32,
    pub ly: i32,
    pub lw: i32,
    pub lh: i32,
    /// Physical size of what a monitor stream carries (the current mode,
    /// rotated if the monitor is).
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    /// GNOME's primary monitor (the top bar's, where new windows go).
    pub primary: bool,
}

pub(super) type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
pub(super) type MonitorInfo = (
    (String, String, String, String),
    Vec<Mode>,
    HashMap<String, OwnedValue>,
);
pub(super) type LogicalInfo = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);

pub(super) fn flag(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

/// The major version of the mutter serving `SC`, from the libmutter it has
/// loaded (libmutter-N is mutter N + 32: 14 is 46, 18 is 50). Its D-Bus API
/// versions don't move between releases, but its bugs do.
pub fn mutter_major(conn: &Connection) -> Option<u32> {
    let bus = Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .ok()?;
    let pid: u32 = bus.call("GetConnectionUnixProcessID", &(SC,)).ok()?;
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    maps.split(|c: char| c == '/' || c.is_whitespace())
        .filter_map(|f| {
            f.strip_prefix("libmutter-")?
                .split(".so")
                .next()?
                .parse::<u32>()
                .ok()
        })
        .next()
        .map(|soname| soname + 32)
}

/// The lock-screen extension shipped in `contrib/gnome-shell/`.
pub const LOCKSCREEN_EXTENSION: &str = "xwlbridge-lockscreen@xwlbridge";

/// A GNOME Shell extension's state (`org.gnome.Shell.Extensions`): `None` if
/// it isn't installed (or the shell can't be asked), else gnome-shell's
/// `ExtensionState` number (1 = active).
pub fn extension_state(conn: &Connection, uuid: &str) -> Option<u32> {
    let p = Proxy::new(
        conn,
        "org.gnome.Shell",
        "/org/gnome/Shell",
        "org.gnome.Shell.Extensions",
    )
    .ok()?;
    let info: HashMap<String, OwnedValue> = p.call("GetExtensionInfo", &(uuid,)).ok()?;
    let state = info.get("state")?;
    f64::try_from(state.try_clone().ok()?)
        .ok()
        .map(|v| v as u32)
}

/// The `xwlbridge-windows` shell extension (`contrib/gnome-shell/`): its
/// `GetMonitors` is GNOME Shell's own view of the monitors, primary flag
/// included, and answers anyone.
const WINDOWS_BUS_NAME: &str = "org.xwlbridge.Windows";
const WINDOWS_PATH: &str = "/org/xwlbridge/Windows";
const WINDOWS_IFACE: &str = "org.xwlbridge.Windows1";

/// One monitor of `GetMonitors`: its index and GNOME Shell's `primary` flag.
type ShellMonitor = (i32, ((i32, i32, i32, i32), (i32, i32, i32, i32), f64, bool));

/// The primary monitor according to GNOME Shell itself (the
/// `xwlbridge-windows` extension), or `None` if the extension is not there
/// or does not answer: it tells apart what mutter's stale primary flags
/// cannot (see [`super::primary`]).
pub fn shell_primary(conn: &Connection) -> Option<usize> {
    let p = Proxy::new(conn, WINDOWS_BUS_NAME, WINDOWS_PATH, WINDOWS_IFACE).ok()?;
    let monitors: Vec<ShellMonitor> = p
        .call("GetMonitors", &())
        .map_err(|e| crate::vlog!("gnome: GetMonitors: {e}"))
        .ok()?;
    let mut primaries = monitors.iter().filter(|(_, m)| m.3).map(|(i, _)| *i);
    match (primaries.next(), primaries.next()) {
        (Some(one), None) => usize::try_from(one).ok(),
        _ => None,
    }
}

/// What the X screen does not depend on: the monitor's colour state and its
/// RGB range (`color-mode`, `rgb-range`), as `GetCurrentState` gives them.
pub(super) type ColourState = Vec<(String, Option<i32>, Option<i32>)>;

/// Reads the current monitor layout (mirrored monitors share one logical
/// monitor; only the first is mirrored into X, since they show the same
/// thing), and the monitors' colour state, to tell a change of it (HDR, RGB
/// range) from a change of the layout.
pub fn display_state_detailed(conn: &Connection) -> zbus::Result<(Vec<Monitor>, ColourState)> {
    display_state_everything(conn)
}

fn display_state_everything(conn: &Connection) -> zbus::Result<(Vec<Monitor>, ColourState)> {
    let dc = Proxy::new(conn, DC, "/org/gnome/Mutter/DisplayConfig", DC)?;
    let (_serial, monitors, logical, props): (
        u32,
        Vec<MonitorInfo>,
        Vec<LogicalInfo>,
        HashMap<String, OwnedValue>,
    ) = dc.call("GetCurrentState", &())?;
    // layout-mode 2 = physical (logical coordinates are physical pixels)
    let physical_layout = props.get("layout-mode").and_then(|v| u32::try_from(v).ok()) == Some(2);
    let mut out = Vec::new();
    let mut colour: ColourState = monitors
        .iter()
        .map(|m| {
            let num = |k: &str| m.2.get(k).and_then(|v| i32::try_from(v).ok());
            (m.0.0.clone(), num("color-mode"), num("rgb-range"))
        })
        .collect();
    for (x, y, scale, transform, primary, mons, _) in logical {
        let Some((connector, ..)) = mons.first() else {
            continue;
        };
        let Some(info) = monitors.iter().find(|m| m.0.0 == *connector) else {
            continue;
        };
        let Some(mode) = info.1.iter().find(|md| flag(&md.6, "is-current")) else {
            continue;
        };
        let (mut w, mut h) = (mode.1.max(1) as u32, mode.2.max(1) as u32);
        if transform % 2 == 1 {
            std::mem::swap(&mut w, &mut h); // 90/270, plain or flipped
        }
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let (lw, lh) = if physical_layout {
            (w as i32, h as i32)
        } else {
            (
                (f64::from(w) / scale).round() as i32,
                (f64::from(h) / scale).round() as i32,
            )
        };
        out.push(Monitor {
            connector: connector.clone(),
            modes: offered_modes(&info.1, transform),
            lx: x,
            ly: y,
            lw,
            lh,
            width: w,
            height: h,
            refresh_mhz: (mode.3 * 1000.0).round() as u32,
            primary,
        });
    }
    colour.sort();
    Ok((out, colour))
}

/// A monitor's modes (`GetCurrentState`'s per-monitor list) as X lists
/// them: sizes swapped for a monitor turned 90 or 270 degrees, as its
/// current mode is, and nothing an X screen can't be.
fn offered_modes(modes: &[Mode], transform: u32) -> Vec<ModeSpec> {
    let max = i32::from(crate::bridge::x11::randr::MAX_SCREEN);
    modes
        .iter()
        .filter(|m| (1..=max).contains(&m.1) && (1..=max).contains(&m.2))
        .map(|m| {
            let (mut width, mut height) = (m.1 as u16, m.2 as u16);
            if transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            ModeSpec {
                width,
                height,
                refresh_mhz: (m.3.max(0.0) * 1000.0).round() as u32,
                current: flag(&m.6, "is-current"),
                preferred: flag(&m.6, "is-preferred"),
                interlaced: flag(&m.6, "is-interlaced"),
            }
        })
        .collect()
}

#[cfg(test)]
mod offered_modes_tests {
    use super::*;

    fn mode(w: i32, h: i32, hz: f64, flags: &[&str]) -> Mode {
        let props = flags
            .iter()
            .map(|f| (f.to_string(), Value::from(true).try_to_owned().unwrap()))
            .collect();
        (format!("{w}x{h}@{hz}"), w, h, hz, 1.0, vec![1.0], props)
    }

    #[test]
    fn modes_keep_their_flags_and_turn_with_the_monitor() {
        let modes = vec![
            mode(1920, 1080, 60.0, &["is-preferred"]),
            mode(1280, 720, 59.940, &["is-current"]),
            mode(1920, 1080, 60.0, &["is-interlaced"]),
            mode(0, 0, 60.0, &[]),
            mode(40000, 100, 60.0, &[]),
        ];
        let got = offered_modes(&modes, 0);
        assert_eq!(got.len(), 3, "nothing an X screen can't be");
        assert_eq!(
            (got[0].width, got[0].height, got[0].refresh_mhz),
            (1920, 1080, 60_000)
        );
        assert!(got[0].preferred && !got[0].current && !got[0].interlaced);
        assert_eq!(
            (got[1].width, got[1].height, got[1].refresh_mhz),
            (1280, 720, 59_940)
        );
        assert!(got[1].current && !got[1].preferred);
        assert!(got[2].interlaced);
        // turned 90 degrees (transform 1) or 270 flipped (7): portrait sizes
        for t in [1, 3, 5, 7] {
            assert_eq!(
                (
                    offered_modes(&modes, t)[1].width,
                    offered_modes(&modes, t)[1].height
                ),
                (720, 1280)
            );
        }
        assert_eq!(offered_modes(&modes, 2)[1].width, 1280);
    }
}

/// Whether mutter's remote-desktop API is on the session bus.
pub fn rd_available(conn: &Connection) -> bool {
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(conn) else {
        return false;
    };
    zbus::names::BusName::try_from(RD)
        .ok()
        .and_then(|n| dbus.name_has_owner(n).ok())
        .unwrap_or(false)
}

/// `org.freedesktop.DBus.Properties.PropertiesChanged`: interface, changed
/// properties, invalidated properties.
type PropertiesChanged = (String, HashMap<String, OwnedValue>, Vec<String>);

/// mutter signals the backend acts on, decoded on the pump thread.
#[derive(Debug)]
pub enum Signal {
    /// A remote-desktop session ended (lock screen, the indicator's stop
    /// button, gnome-shell restart).
    SessionClosed { path: String },
    /// A recorded stream's PipeWire node is ready.
    StreamAdded { path: String, node: u32 },
    /// Any monitor reconfiguration; monitor streams whose rect changed die.
    MonitorsChanged,
    /// gnome-shell's screen shield came up (true) or went away (false):
    /// `org.gnome.ScreenSaver.ActiveChanged`.
    ScreenLocked(bool),
    /// The CLIPBOARD owner changed. `None` mimes: cleared.
    SelectionOwnerChanged {
        path: String,
        mimes: Option<Vec<String>>,
        session_is_owner: bool,
        /// When the signal came in.
        at: std::time::Instant,
    },
    /// A local paste wants our published selection as `mime`.
    SelectionTransfer {
        path: String,
        mime: String,
        serial: u32,
    },
    /// The session bus closed our connection (the signal stream ended): the
    /// desktop session is gone, or its bus was restarted under us.
    BusLost,
    /// DisplayConfig's `PowerSaveMode` changed (gsd-power blanking the
    /// screen on idle, or anyone else on the bus).
    PowerSaveMode(i32),
}

/// Starts the one thread that receives every mutter signal (under
/// `/org/gnome/Mutter`) for the backend's lifetime, and forwards the ones we
/// act on. One subscription instead of one per session, so sessions coming
/// and going leave no blocked watcher threads behind, and nothing emitted
/// during `Start` can race a late subscription.
pub fn spawn_signal_pump(
    conn: &Connection,
    tx: Sender<Signal>,
    wake: Arc<Wake>,
) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(MsgType::Signal)
        .path_namespace("/org/gnome/Mutter")?
        .build();
    let iter = MessageIterator::for_match_rule(rule, conn, Some(256))?;
    let (tx2, wake2) = (tx.clone(), wake.clone());
    std::thread::Builder::new()
        .name("gnome-signals".into())
        .spawn(move || {
            for msg in iter {
                let Ok(msg) = msg else { continue };
                let hdr = msg.header();
                let (Some(path), Some(member)) = (hdr.path(), hdr.member()) else {
                    continue;
                };
                let path = path.to_string();
                let body = msg.body();
                let sig = match member.as_str() {
                    "Closed" if path.starts_with("/org/gnome/Mutter/RemoteDesktop/") => {
                        Signal::SessionClosed { path }
                    }
                    "PipeWireStreamAdded" => match body.deserialize::<u32>() {
                        Ok(node) => Signal::StreamAdded { path, node },
                        Err(_) => continue,
                    },
                    "MonitorsChanged" => Signal::MonitorsChanged,
                    "SelectionOwnerChanged" => {
                        let Ok(opts) = body.deserialize::<HashMap<String, OwnedValue>>() else {
                            continue;
                        };
                        // mutter packs the list as a 1-tuple, `(as)`, inside the variant
                        let mimes = opts.get("mime-types").and_then(|v| {
                            <(Vec<String>,)>::try_from(v.clone())
                                .map(|t| t.0)
                                .or_else(|_| Vec::<String>::try_from(v.clone()))
                                .ok()
                        });
                        Signal::SelectionOwnerChanged {
                            path,
                            mimes,
                            session_is_owner: flag(&opts, "session-is-owner"),
                            at: std::time::Instant::now(),
                        }
                    }
                    "SelectionTransfer" => match body.deserialize::<(String, u32)>() {
                        Ok((mime, serial)) => Signal::SelectionTransfer { path, mime, serial },
                        Err(_) => continue,
                    },
                    "PropertiesChanged" if path == "/org/gnome/Mutter/DisplayConfig" => {
                        let Ok((iface, changed, _)) = body.deserialize::<PropertiesChanged>()
                        else {
                            continue;
                        };
                        let mode = changed.get("PowerSaveMode");
                        match mode.and_then(|v| i32::try_from(v).ok()) {
                            Some(mode) if iface == DC => Signal::PowerSaveMode(mode),
                            _ => continue,
                        }
                    }
                    _ => continue,
                };
                if tx.send(sig).is_err() {
                    return;
                }
                wake.wake();
            }
            crate::warning!("gnome: lost the D-Bus signal stream");
            if tx.send(Signal::BusLost).is_ok() {
                wake.wake();
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    spawn_screensaver_pump(conn, tx2, wake2)
}

/// Forwards gnome-shell's lock-screen changes. A separate subscription: the
/// screen shield lives under `/org/gnome/ScreenSaver`, not mutter's namespace.
fn spawn_screensaver_pump(
    conn: &Connection,
    tx: Sender<Signal>,
    wake: Arc<Wake>,
) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(MsgType::Signal)
        .path("/org/gnome/ScreenSaver")?
        .interface("org.gnome.ScreenSaver")?
        .member("ActiveChanged")?
        .build();
    let iter = MessageIterator::for_match_rule(rule, conn, Some(16))?;
    std::thread::Builder::new()
        .name("gnome-lock-signals".into())
        .spawn(move || {
            for msg in iter {
                let Ok(msg) = msg else { continue };
                let Ok(active) = msg.body().deserialize::<bool>() else {
                    continue;
                };
                if tx.send(Signal::ScreenLocked(active)).is_err() {
                    return;
                }
                wake.wake();
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    Ok(())
}

/// A stream we asked mutter to record.
pub struct RecordedStream {
    pub connector: String,
    pub path: String,
    /// The logical rectangle it records (the monitor's, when recorded).
    pub area: (i32, i32, i32, i32),
    /// Its `mapping-id`: the EIS region that moves the pointer over it says
    /// the same (`None` from a mutter without mapping ids).
    pub mapping_id: Option<String>,
}

/// A linked RemoteDesktop + ScreenCast session.
pub struct Session {
    conn: Connection,
    pub rd_path: String,
    pub eis: Option<OwnedFd>,
    pub streams: Vec<RecordedStream>,
    stopped: bool,
}

impl Session {
    /// Creates, links and starts a session recording `monitors`. The streams'
    /// `PipeWireStreamAdded` arrive through the signal pump.
    ///
    /// `disable_animations` turns gnome-shell's animations off while the
    /// session lives, which saves capture work and bandwidth (it is global to
    /// the user's session, so only do it while someone is watching).
    pub fn start(
        conn: &Connection,
        monitors: &[Monitor],
        baked_cursor: bool,
        disable_animations: bool,
        record_monitor: bool,
    ) -> zbus::Result<Session> {
        let rd = Proxy::new(conn, RD, "/org/gnome/Mutter/RemoteDesktop", RD)?;
        let rd_path: OwnedObjectPath = rd.call("CreateSession", &())?;
        let mut session = Session {
            conn: conn.clone(),
            rd_path: rd_path.to_string(),
            eis: None,
            streams: Vec::new(),
            stopped: false,
        };
        let rds = session.rd_proxy()?;
        let session_id: String = rds.get_property("SessionId")?;

        // keyboard + pointer; we never send touch
        let mut eis_opts: HashMap<&str, Value> = HashMap::new();
        eis_opts.insert("device-types", Value::U32(3));
        match rds.call::<_, _, zbus::zvariant::OwnedFd>("ConnectToEIS", &(eis_opts,)) {
            Ok(fd) => session.eis = Some(fd.into()),
            Err(e) => crate::warning!("gnome: ConnectToEIS failed, no input: {e}"),
        }

        let sc = Proxy::new(conn, SC, "/org/gnome/Mutter/ScreenCast", SC)?;
        let mut sc_opts: HashMap<&str, Value> = HashMap::new();
        sc_opts.insert(
            "remote-desktop-session-id",
            Value::from(session_id.as_str()),
        );
        sc_opts.insert("disable-animations", Value::Bool(disable_animations));
        let sc_path: OwnedObjectPath = sc.call("CreateSession", &(sc_opts,))?;
        let scs = Proxy::new(conn, SC, sc_path.as_str(), SC_SESSION)?;
        let cursor_mode = if baked_cursor {
            CURSOR_MODE_EMBEDDED
        } else {
            CURSOR_MODE_METADATA
        };
        for m in monitors {
            let mut rec: HashMap<&str, Value> = HashMap::new();
            rec.insert("cursor-mode", Value::U32(cursor_mode));
            // RecordArea over the monitor's rectangle carries the same pixels,
            // painted the same way into our (memfd) buffers. RecordMonitor adds
            // damage metadata, but its cursor tracking dereferences the
            // monitor's mode on every frame: disabling a recorded monitor
            // crashes mutter up to 50.x (mutter#2718, fixed in 51 by !5316),
            // which is the user's whole session.
            let res = if record_monitor {
                scs.call::<_, _, OwnedObjectPath>("RecordMonitor", &(m.connector.as_str(), rec))
            } else {
                scs.call::<_, _, OwnedObjectPath>("RecordArea", &(m.lx, m.ly, m.lw, m.lh, rec))
            };
            match res {
                Ok(p) => {
                    // set when the stream is created, so it can be read now:
                    // input needs it before the first pointer motion
                    let mapping_id = session.mapping_id(p.as_str());
                    session.streams.push(RecordedStream {
                        connector: m.connector.clone(),
                        path: p.to_string(),
                        area: (m.lx, m.ly, m.lw, m.lh),
                        mapping_id,
                    })
                }
                Err(e) => crate::warning!("gnome: recording {} failed: {e}", m.connector),
            }
        }
        // a linked session is started through RemoteDesktop, which also starts
        // every stream recorded so far
        rds.call_method("Start", &())?;
        Ok(session)
    }

    fn rd_proxy(&self) -> zbus::Result<Proxy<'static>> {
        Proxy::new(&self.conn, RD, self.rd_path.clone(), RD_SESSION)
    }

    /// The `mapping-id` mutter gives a stream: the same string as the EIS
    /// region covering it, which is how input finds the right region.
    pub fn mapping_id(&self, stream_path: &str) -> Option<String> {
        let st = Proxy::new(&self.conn, SC, stream_path.to_string(), SC_STREAM).ok()?;
        let params: HashMap<String, OwnedValue> = st.get_property("Parameters").ok()?;
        params
            .get("mapping-id")
            .and_then(|v| String::try_from(v.clone()).ok())
    }

    /// Marks the session as already gone, so dropping it doesn't call `Stop`.
    pub fn mark_closed(&mut self) {
        self.stopped = true;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        // An un-started or already-closed session errors here; either way it's gone.
        if let Ok(rds) = self.rd_proxy() {
            let _ = rds.call_method("Stop", &());
        }
    }
}

/// Whether a session-creation error means the screen is locked (gnome-shell
/// inhibits remote access on the lock screen and greeter), so the right move is
/// to wait rather than report a failure.
pub fn is_inhibited(e: &zbus::Error) -> bool {
    e.to_string().contains("inhibited")
}

/// Whether `e` says our connection to the bus itself is gone (the bus closed
/// it, or it broke), as opposed to a call that failed or took longer than the
/// connection's method timeout: zbus reports that timeout as an I/O error too
/// (`ErrorKind::TimedOut`), while the bus is alive and well.
pub fn connection_lost(e: &zbus::Error) -> bool {
    use std::io::ErrorKind;
    match e {
        zbus::Error::InputOutput(io) => matches!(
            io.kind(),
            ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

/// Waits up to `limit` for something to own mutter's remote-desktop name,
/// checking every `every`. True once it is owned.
pub fn wait_for_rd(
    conn: &Connection,
    limit: std::time::Duration,
    every: std::time::Duration,
) -> bool {
    let until = std::time::Instant::now() + limit;
    loop {
        if rd_available(conn) {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(every);
    }
}

#[cfg(test)]
mod tests {
    use super::connection_lost;
    use std::io::{Error, ErrorKind};
    use std::sync::Arc;

    #[test]
    fn only_a_broken_connection_is_a_lost_bus() {
        let io = |k: ErrorKind| zbus::Error::InputOutput(Arc::new(Error::from(k)));
        // what zbus returns when a call outlives the method timeout: the bus
        // is still there (seen: a 6 s gnome-shell stall)
        assert!(!connection_lost(&io(ErrorKind::TimedOut)));
        assert!(!connection_lost(&io(ErrorKind::WouldBlock)));
        assert!(!connection_lost(&zbus::Error::Failure("refused".into())));
        for k in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(connection_lost(&io(k)), "{k:?}");
        }
    }
}
