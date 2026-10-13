//! CLIPBOARD bridging over mutter's remote-desktop clipboard API.
//!
//! mutter has no offer objects: a session is told the owner changed (with the
//! mime types), reads a value with `SelectionRead` (a pipe mutter fills), and
//! publishes with `SetSelection`, answering each paste's `SelectionTransfer`
//! by writing into the pipe `SelectionWrite` returns. PRIMARY is not exposed
//! at all, so only CLIPBOARD is bridged.
//!
//! Every pipe is pumped on a short-lived helper thread, so a slow or stuck app
//! can never stall the backend loop (or, by extension, capture and input):
//! a read has a size cap and a deadline, its result comes back over a channel;
//! a paste is written without blocking and abandoned once its reader stops
//! taking data for a while, and only so many are written at once. mutter
//! times nothing out once it has handed us a paste's pipe, so these limits
//! are all that bound what a paste that is never read can hold.
//!
//! Between sessions a [`ClipWatch`] hears *when* GNOME's clipboard changes
//! (never what it holds), so that a session starting can tell which of an X
//! copy and a GNOME copy made meanwhile is the newer.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::Value;

use super::Wake;
use super::dbus::RD;
use crate::bridge::Server;
use crate::bridge::clipboard::{DataOffer, MAX_SELECTION_BYTES, Sel, TEXT_MIMES};
use crate::bridge::clipjobs::{Job, PendingConversion};

const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
/// A paste that hasn't finished in this long is abandoned (mutter itself
/// cancels unanswered transfers after 15 s).
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
/// A paste whose reader takes nothing for this long is abandoned (its pipe
/// closed, so the app sees the end of the data). Generous, so a reader that is
/// slow but alive (a pager on a large value) still gets everything.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Pastes written at once; another one abandons the oldest (as the wlr
/// backend's sends do).
const MAX_WRITES: usize = 8;
/// How often a waiting write checks whether it was abandoned.
const WRITE_POLL: Duration = Duration::from_millis(250);

/// The pastes being written, oldest first, each with its abandon flag.
#[derive(Default)]
struct Writers {
    next_id: u64,
    live: VecDeque<(u64, Arc<AtomicBool>)>,
}

impl Writers {
    /// Registers a new write, abandoning the oldest if there are too many.
    fn add(&mut self) -> (u64, Arc<AtomicBool>) {
        while self.live.len() >= MAX_WRITES {
            if let Some((_, old)) = self.live.pop_front() {
                crate::warning!("gnome: too many pastes in flight; abandoning the oldest");
                old.store(true, Ordering::Release);
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let abandon = Arc::new(AtomicBool::new(false));
        self.live.push_back((id, abandon.clone()));
        (id, abandon)
    }

    fn remove(&mut self, id: u64) {
        self.live.retain(|(i, _)| *i != id);
    }
}

struct Receive {
    mime: String,
    generation: u64,
    since: Instant,
    conversion: PendingConversion,
}

pub struct Clip {
    server: Arc<Server>,
    conn: Connection,
    rd_path: String,
    wake: Arc<Wake>,
    next_token: u64,
    /// What we published (an X client's selection), served to local pastes.
    published: Option<Arc<[u8]>>,
    /// mutter allows one `SelectionRead` at a time per session.
    queue: VecDeque<Receive>,
    reading: Option<PendingConversion>,
    results_tx: Sender<Option<Vec<u8>>>,
    results_rx: Receiver<Option<Vec<u8>>>,
    /// Pastes still being written (they may outlive the session).
    writers: Arc<Mutex<Writers>>,
}

impl Clip {
    /// Enables the session's clipboard and tells the X side it may publish.
    ///
    /// GNOME's clipboard is reachable only while a session runs, so an X
    /// client may have taken CLIPBOARD while it wasn't (`since`: the ownership
    /// generation when the last session's clipboard went, `None` if there was
    /// none). Unless GNOME's clipboard changed after that copy was made
    /// (`gnome_changed`: when a GNOME app last took it, as far as a
    /// [`ClipWatch`] heard), the X copy is the newer and goes to GNOME in the
    /// call that enables the clipboard: mutter installs it before it starts
    /// reporting owners, so its announcement of its own older selection never
    /// arrives to displace the X owner, as it would race a `SetSelection`
    /// made afterwards. Otherwise GNOME's copy is the newer, and mutter's
    /// announcement of it goes to X as in a session.
    pub fn start(
        server: Arc<Server>,
        conn: Connection,
        rd_path: String,
        wake: Arc<Wake>,
        since: Option<u64>,
        gnome_changed: Option<Instant>,
    ) -> Option<Clip> {
        let rd = Proxy::new(&conn, RD, rd_path.clone(), RD_SESSION).ok()?;
        let carry = match server.clipboard.x_value_since(Sel::Clipboard, since) {
            Some((_, x_at)) if gnome_changed.is_some_and(|g| g > x_at) => {
                crate::cliplog!(
                    "gnome: an X client took CLIPBOARD while GNOME couldn't hear of it, but \
                     GNOME's clipboard changed {:.1}s later; keeping GNOME's",
                    gnome_changed.map_or(0.0, |g| (g - x_at).as_secs_f32())
                );
                None
            }
            other => other.map(|(data, _)| data),
        };
        let mut opts: HashMap<&str, Value> = HashMap::new();
        if carry.is_some() {
            opts.insert("mime-types", Value::from(TEXT_MIMES.to_vec()));
        }
        if let Err(e) = rd.call_method("EnableClipboard", &(opts,)) {
            crate::warning!("gnome: clipboard unavailable: {e}");
            return None;
        }
        if let Some(d) = &carry {
            crate::cliplog!(
                "gnome: an X client took CLIPBOARD while GNOME couldn't hear of it; published its {} bytes",
                d.len()
            );
            server.clipboard.mark_published(Sel::Clipboard);
        }
        server.clipboard.set_publishable(Sel::Clipboard, true);
        server.clipboard.set_publishable(Sel::Primary, false);
        server.clipboard.jobs().set_running(true);
        let (results_tx, results_rx) = channel();
        Some(Clip {
            server,
            conn,
            rd_path,
            wake,
            next_token: 0,
            published: carry,
            queue: VecDeque::new(),
            reading: None,
            results_tx,
            results_rx,
            writers: Arc::default(),
        })
    }

    fn rd(&self) -> zbus::Result<Proxy<'static>> {
        Proxy::new(&self.conn, RD, self.rd_path.clone(), RD_SESSION)
    }

    pub fn is_ours(&self, path: &str) -> bool {
        self.rd_path == path
    }

    /// The owner of CLIPBOARD changed. Our own publication comes back as
    /// `session_is_owner`, which is turned into the echo the state machine
    /// recognises (an offer carrying exactly `TEXT_MIMES`).
    pub fn owner_changed(&mut self, mimes: Option<Vec<String>>, session_is_owner: bool) {
        let (offer, mimes) = if session_is_owner {
            (
                Some(self.token()),
                TEXT_MIMES.iter().map(|m| (*m).to_string()).collect(),
            )
        } else {
            match mimes {
                Some(m) if !m.is_empty() => (Some(self.token()), m),
                _ => (None, Vec::new()),
            }
        };
        crate::cliplog!("gnome: CLIPBOARD owner changed (ours: {session_is_owner}, {mimes:?})");
        self.server
            .clipboard
            .wayland_announced(Sel::Clipboard, offer, mimes)
            .apply(&self.server, Sel::Clipboard);
    }

    fn token(&mut self) -> DataOffer {
        self.next_token += 1;
        DataOffer::Token(self.next_token)
    }

    /// Runs the X side's clipboard work.
    pub fn run_jobs(&mut self) {
        for job in self.server.clipboard.jobs().take() {
            match job {
                Job::Publish(Sel::Clipboard, data) => self.publish(data),
                Job::Publish(Sel::Primary, _) | Job::Destroy(_) => {}
                Job::Receive {
                    sel: Sel::Clipboard,
                    mime,
                    generation,
                    since,
                    conversion,
                } => self.queue.push_back(Receive {
                    mime,
                    generation,
                    since,
                    conversion,
                }),
                // PRIMARY isn't bridged on GNOME; dropping refuses the conversion
                Job::Receive { .. } => {}
            }
        }
        self.pump_reads();
    }

    fn publish(&mut self, data: Option<Arc<[u8]>>) {
        let mut opts: HashMap<&str, Value> = HashMap::new();
        if data.is_some() {
            opts.insert("mime-types", Value::from(TEXT_MIMES.to_vec()));
        }
        // empty options withdraw our selection
        self.published = data;
        match self.rd() {
            Ok(rd) => {
                if let Err(e) = rd.call_method("SetSelection", &(opts,)) {
                    crate::warning!("gnome: SetSelection failed: {e}");
                }
            }
            Err(e) => crate::warning!("gnome: clipboard: {e}"),
        }
    }

    /// A local app pastes from our selection: write it into mutter's pipe on
    /// a helper thread, then report done. With nothing to give, close the
    /// pipe at once (an empty paste) rather than leave the app waiting for
    /// mutter's 15 s cleanup. A paste that stops being read is abandoned
    /// after [`WRITE_STALL_TIMEOUT`], or sooner once [`MAX_WRITES`] newer ones
    /// are in flight.
    pub fn transfer(&mut self, mime: String, serial: u32) {
        let Ok(rd) = self.rd() else { return };
        let fd: OwnedFd =
            match rd.call::<_, _, zbus::zvariant::OwnedFd>("SelectionWrite", &(serial,)) {
                Ok(fd) => fd.into(),
                Err(e) => {
                    crate::cliplog!("gnome: SelectionWrite({serial}) failed: {e}");
                    return;
                }
            };
        let data = self.published.clone();
        crate::cliplog!(
            "gnome: serving paste #{serial} as {mime} ({} bytes)",
            data.as_ref().map_or(0, |d| d.len())
        );
        let conn = self.conn.clone();
        let path = self.rd_path.clone();
        let writers = self.writers.clone();
        let (id, abandon) = lock(&writers).add();
        let spawned = std::thread::Builder::new()
            .name("gnome-clip-write".into())
            .spawn({
                let writers = writers.clone();
                move || {
                    let ok = match &data {
                        Some(d) => {
                            let r = write_pipe(&fd, d, &abandon, WRITE_STALL_TIMEOUT);
                            if let Err(e) = &r {
                                crate::cliplog!("gnome: paste #{serial} abandoned: {e}");
                            }
                            r.is_ok()
                        }
                        None => false,
                    };
                    drop(fd);
                    lock(&writers).remove(id);
                    // the pipe is closed above (end of data); now say how it went
                    if let Ok(rd) = Proxy::new(&conn, RD, path, RD_SESSION) {
                        let _ = rd.call_method("SelectionWriteDone", &(serial, ok));
                    }
                }
            });
        if spawned.is_err() {
            lock(&writers).remove(id);
        }
    }

    /// Starts the next queued read if none is running.
    fn pump_reads(&mut self) {
        while self.reading.is_none() {
            let Some(r) = self.queue.pop_front() else {
                return;
            };
            // the owner changed since the X client asked: refuse (drop) it
            if self
                .server
                .clipboard
                .offer_at(Sel::Clipboard, r.generation)
                .is_none()
                || r.since.elapsed() > TRANSFER_TIMEOUT
            {
                continue;
            }
            let fd: OwnedFd = match self.rd().and_then(|rd| {
                rd.call::<_, _, zbus::zvariant::OwnedFd>("SelectionRead", &(r.mime.as_str(),))
            }) {
                Ok(fd) => fd.into(),
                Err(e) => {
                    crate::cliplog!("gnome: SelectionRead({}) failed: {e}", r.mime);
                    continue;
                }
            };
            let tx = self.results_tx.clone();
            let wake = self.wake.clone();
            let budget = TRANSFER_TIMEOUT.saturating_sub(r.since.elapsed());
            let spawned = std::thread::Builder::new()
                .name("gnome-clip-read".into())
                .spawn(move || {
                    let _ = tx.send(read_pipe(fd, budget));
                    wake.wake();
                });
            if spawned.is_ok() {
                self.reading = Some(r.conversion);
            }
        }
    }

    /// Collects finished reads; called when the backend wakes.
    pub fn poll_results(&mut self) {
        while let Ok(data) = self.results_rx.try_recv() {
            if let Some(conversion) = self.reading.take() {
                crate::cliplog!(
                    "gnome: read the clipboard ({:?} bytes)",
                    data.as_ref().map(Vec::len)
                );
                conversion.finish(data);
            }
        }
        self.pump_reads();
    }
}

/// Hears when GNOME's CLIPBOARD owner changes while no session runs, through
/// a remote-desktop session that is created and never started: mutter lets an
/// unstarted session enable the clipboard, and tells it of every owner
/// change. Only that something changed is used, and when: nothing is read,
/// nothing reaches X, and an unstarted session has no screen-sharing
/// indicator (gnome-shell makes one only on `Start`). mutter closes it with
/// every other session on the lock screen; it is made again after unlock.
pub struct ClipWatch {
    pub path: String,
    /// When `EnableClipboard` returned. mutter reports the owner it has as
    /// that call enables the clipboard, which is no change.
    enabled: Instant,
}

/// Owner changes heard within this long of [`ClipWatch::enabled`] are taken
/// for mutter's report of the owner it already had.
const WATCH_SETTLE: Duration = Duration::from_millis(200);

/// Why a [`ClipWatch`] couldn't be made.
pub enum WatchError {
    /// No session (mutter refuses them on the lock screen, or isn't there):
    /// try again later.
    Session(zbus::Error),
    /// A session, but its clipboard can't be enabled before it starts (a
    /// mutter that differs from 50's here): don't try again, as each try
    /// leaves an unstarted session behind until we disconnect.
    Clipboard(zbus::Error),
}

/// `-showwindows`: a remote-desktop session that is created and never
/// started (no screen-sharing indicator), for a [`Clip`] that bridges the
/// clipboard while no session runs: the X windows shown on the desktop are
/// used there and then, as on Xwayland. mutter 50 reads and sets the
/// clipboard of an unstarted session.
pub fn unstarted_session(conn: &Connection) -> zbus::Result<String> {
    let rd = Proxy::new(conn, RD, "/org/gnome/Mutter/RemoteDesktop", RD)?;
    let path: zbus::zvariant::OwnedObjectPath = rd.call("CreateSession", &())?;
    Ok(path.to_string())
}

/// Turns off the clipboard of the unstarted session at `path` (mutter allows
/// one `EnableClipboard` at a time; mutter 50 then refuses to enable it again
/// on that session, so the idle bridge takes a fresh one next time).
pub fn disable(conn: &Connection, path: &str) -> bool {
    match Proxy::new(conn, RD, path.to_string(), RD_SESSION)
        .and_then(|rd| rd.call_method("DisableClipboard", &()))
    {
        Ok(_) => true,
        Err(e) => {
            crate::vlog!("gnome: can't turn off the clipboard of {path}: {e}");
            false
        }
    }
}

impl ClipWatch {
    /// Makes the watching session.
    pub fn start(conn: &Connection) -> Result<ClipWatch, WatchError> {
        let session = || -> zbus::Result<String> {
            let rd = Proxy::new(conn, RD, "/org/gnome/Mutter/RemoteDesktop", RD)?;
            let path: zbus::zvariant::OwnedObjectPath = rd.call("CreateSession", &())?;
            Ok(path.to_string())
        };
        let path = session().map_err(WatchError::Session)?;
        // (an unstarted session can't be stopped: should this fail, mutter
        // drops the session with our connection)
        Proxy::new(conn, RD, path.clone(), RD_SESSION)
            .and_then(|s| s.call_method("EnableClipboard", &(HashMap::<&str, Value>::new(),)))
            .map_err(WatchError::Clipboard)?;
        Ok(ClipWatch {
            path,
            enabled: Instant::now(),
        })
    }

    /// Whether an owner change heard at `at` is a change (not mutter's report
    /// of the owner it had when the watch began).
    pub fn is_change(&self, at: Instant) -> bool {
        at.saturating_duration_since(self.enabled) > WATCH_SETTLE
    }
}

impl Drop for Clip {
    fn drop(&mut self) {
        // nobody drains the X side's jobs any more: refuse them up front
        self.server.clipboard.jobs().set_running(false);
        self.server.clipboard.set_publishable(Sel::Clipboard, false);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a paste wasn't written whole.
#[derive(Debug, PartialEq)]
enum WriteEnd {
    /// The reader took nothing for the whole stall timeout.
    Stalled,
    /// Too many newer pastes came along.
    Abandoned,
    /// The pipe failed (the reader closed it, or the fd is unusable).
    Failed(i32),
}

impl std::fmt::Display for WriteEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteEnd::Stalled => write!(f, "the app stopped reading"),
            WriteEnd::Abandoned => write!(f, "too many newer pastes"),
            WriteEnd::Failed(e) => write!(f, "{}", std::io::Error::from_raw_os_error(*e)),
        }
    }
}

/// Writes all of `data` into the pipe `fd` without ever blocking for longer
/// than [`WRITE_POLL`]: gives up once the reader has taken nothing for
/// `stall`, or when `abandon` is set. mutter hands over the blocking end of
/// its pipe, so the fd is made non-blocking first.
fn write_pipe(
    fd: &OwnedFd,
    data: &[u8],
    abandon: &AtomicBool,
    stall: Duration,
) -> Result<(), WriteEnd> {
    let raw = fd.as_raw_fd();
    // SAFETY: plain fcntl on an fd we own; only our end of the pipe changes
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(WriteEnd::Failed(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ));
    }
    let mut off = 0;
    let mut progress = Instant::now();
    while off < data.len() {
        if abandon.load(Ordering::Acquire) {
            return Err(WriteEnd::Abandoned);
        }
        // SAFETY: writes from a live slice; at most its remaining length
        let n = unsafe { libc::write(raw, data[off..].as_ptr().cast(), data.len() - off) };
        if n > 0 {
            off += n as usize;
            progress = Instant::now();
            continue;
        }
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if n < 0 && err == libc::EINTR {
            continue;
        }
        if n == 0 || err != libc::EAGAIN {
            return Err(WriteEnd::Failed(err));
        }
        let Some(left) = stall
            .checked_sub(progress.elapsed())
            .filter(|d| !d.is_zero())
        else {
            return Err(WriteEnd::Stalled);
        };
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ms = left.min(WRITE_POLL).as_millis().max(1) as i32;
        // SAFETY: one valid pollfd
        unsafe { libc::poll(&mut pfd, 1, ms) };
    }
    Ok(())
}

/// Reads a pipe to EOF within `budget`, capped at the selection size limit.
fn read_pipe(fd: OwnedFd, budget: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + budget;
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return None; // timed out (or failed): refuse rather than hand over a fragment
        }
        match file.read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => {
                if out.len() + n > MAX_SELECTION_BYTES {
                    return None;
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_paste_nobody_reads_is_given_up_after_the_stall_timeout() {
        let (_rd, wr) = pipe();
        let data = vec![b'x'; 1 << 20]; // far more than a pipe holds
        let t = Instant::now();
        let r = write_pipe(
            &wr,
            &data,
            &AtomicBool::new(false),
            Duration::from_millis(300),
        );
        assert_eq!(r, Err(WriteEnd::Stalled));
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }

    #[test]
    fn a_slow_reader_gets_everything() {
        let (rd, wr) = pipe();
        let data: Vec<u8> = (0..(1u32 << 20)).map(|i| (i % 251) as u8).collect();
        let reader = std::thread::spawn(move || {
            let mut f = std::fs::File::from(rd);
            let mut out = Vec::new();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                // slower than the writer, but never idle for the stall timeout
                std::thread::sleep(Duration::from_millis(2));
                match f.read(&mut buf) {
                    Ok(0) => return out,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let r = write_pipe(
            &wr,
            &data,
            &AtomicBool::new(false),
            Duration::from_millis(500),
        );
        drop(wr);
        assert_eq!(r, Ok(()));
        assert!(reader.join().unwrap() == data);
    }

    #[test]
    fn an_abandoned_paste_stops_at_once() {
        let (_rd, wr) = pipe();
        let data = vec![b'x'; 1 << 20];
        let abandon = Arc::new(AtomicBool::new(false));
        let a = abandon.clone();
        let t = Instant::now();
        let h = std::thread::spawn(move || write_pipe(&wr, &data, &a, Duration::from_secs(60)));
        std::thread::sleep(Duration::from_millis(100));
        abandon.store(true, Ordering::Release);
        assert_eq!(h.join().unwrap(), Err(WriteEnd::Abandoned));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_closed_reader_ends_the_write() {
        let (rd, wr) = pipe();
        drop(rd);
        let r = write_pipe(
            &wr,
            b"hello",
            &AtomicBool::new(false),
            Duration::from_secs(60),
        );
        assert_eq!(r, Err(WriteEnd::Failed(libc::EPIPE)));
    }

    #[test]
    fn only_so_many_writes_are_kept() {
        let mut w = Writers::default();
        let flags: Vec<_> = (0..MAX_WRITES + 2).map(|_| w.add()).collect();
        assert_eq!(w.live.len(), MAX_WRITES);
        // the two oldest were abandoned, the rest not
        for (i, (_, f)) in flags.iter().enumerate() {
            assert_eq!(f.load(Ordering::Acquire), i < 2, "write {i}");
        }
        w.remove(flags[5].0);
        assert_eq!(w.live.len(), MAX_WRITES - 1);
    }
}
