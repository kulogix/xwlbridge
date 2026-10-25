use std::collections::VecDeque;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use x11rb_protocol::protocol::xfixes;

/// How much may be queued for one client before its replies wait. Events are
/// pushed from the Wayland thread and replies from the connection thread; a
/// writer thread per client drains the queue, so a client that stops reading
/// (stopped, or wedged) only ever costs memory, up to this much, rather than
/// stalling capture and the clipboard for every other client. Replies wait for
/// space here (the connection thread blocks, which applies backpressure to
/// that client); server events are dropped if they do not fit, because the
/// Wayland thread must never wait on a client. A full-screen `GetImage` reply
/// is tens of MB on a big screen, so a single write is never refused on its
/// own: the queue takes an oversize reply when it is otherwise empty.
const MAX_QUEUED_BYTES: usize = 64 << 20;

/// The outbound queue, plus the last sequence stamped into it (in full: the
/// wire carries its low 16 bits).
///
/// libxcb widens the wire sequence to 64-bit and assumes a wrap whenever it
/// appears to go *backwards*, bumping by 0x10000. An event from a background
/// thread carrying a sequence lower than a reply already written would therefore
/// be widened past the last request the client sent, and libxcb aborts with
/// `xcb_xlib_threads_sequence_lost`. So every write is stamped and queued under
/// this one lock, and the writer thread sends them in that order.
struct Outbox {
    /// Each write, with the file descriptor that goes with it, if any (a
    /// reply that hands the client an fd, `ShmCreateSegment`'s).
    queue: VecDeque<(Vec<u8>, Option<OwnedFd>)>,
    bytes: usize, // sum of the queued lengths
    last_seq: u64,
    /// No more will be queued: the connection ended, or the client was
    /// dropped. The writer thread exits once the queue is drained.
    closed: bool,
}

impl Outbox {
    /// Clamps `seq` so the wire sequence never decreases: if `seq` is behind
    /// `last_seq`, reuse `last_seq`. Compared in full, not on the wire's 16
    /// bits: a client can send 32768 or more requests without a reply in
    /// between (XTest input only), and a 16-bit comparison then took the
    /// next reply's sequence for one behind and stamped it with a stale one,
    /// which breaks the client's connection.
    fn monotonic(&self, seq: u64) -> u64 {
        seq.max(self.last_seq)
    }
}

/// The full sequence whose low 16 bits are `seq`, nearest `reference` (a
/// request's sequence is at most half the 16-bit range away from the latest).
fn widen(reference: u64, seq: u16) -> u64 {
    let d = i64::from(seq.wrapping_sub(reference as u16) as i16);
    (reference as i64 + d).max(0) as u64
}

/// One XFixes selection registration: X keeps one per `(selection, client,
/// window)` with its own event mask, so a client watching from two windows is
/// told twice, and clearing one window's mask leaves the other's alone.
struct SelReg {
    /// The selection watched, by its atom. Atoms are server-global, so this is
    /// the same number for every connection and for the event we send back.
    atom: u32,
    window: u32,
    mask: xfixes::SelectionEventMask,
}

/// The mask bit that lets `subtype` through: the bits are numbered like the
/// subtypes.
fn mask_for(subtype: xfixes::SelectionEvent) -> xfixes::SelectionEventMask {
    xfixes::SelectionEventMask::from(1u32 << u8::from(subtype))
}

/// One connection, as seen by [`EventSink`](super::EventSink): the outbound
/// queue to its socket, plus what the client has asked to be notified about.
pub struct Client {
    outbox: Mutex<Outbox>,
    outbox_ready: Condvar,
    /// A duplicate of the socket, only ever used to shut it down when the
    /// client is dropped, so the connection thread's blocking read ends.
    socket: UnixStream,
    seq: AtomicU64,           // the client's most recent request sequence, in full
    randr_window: AtomicU32,  // selected RandR ScreenChangeNotify, 0 if none
    cursor_window: AtomicU32, // selected XFixes cursor changes, 0 if none
    selections: Mutex<Vec<SelReg>>,
    dead: AtomicBool,
    /// Whether the client selected StructureNotify on the root window, and so
    /// wants a root ConfigureNotify on resize. This, not RandR SelectInput, is
    /// how vncagent notices the screen changed size.
    root_structure: AtomicBool,
    /// The XKB map components the client selected MapNotify for (0: none).
    /// Xlib selects them itself once it has read the XKB keymap, and re-reads
    /// what an XkbMapNotify says changed.
    xkb_map_details: AtomicU16,
    /// The NewKeyboardNotify details it selected (Xlib: keycodes, on which it
    /// reloads its whole copy of the keymap).
    xkb_nkn_details: AtomicU16,
}

impl Client {
    /// Takes a duplicate of the connection's socket for writing, and starts
    /// the writer thread that drains the queue into it.
    pub fn new(writer: UnixStream) -> std::io::Result<Arc<Self>> {
        // no write timeout: a client that stops reading must not be dropped
        // (x11vnc is both the wrapped command and a client; an XIO would take
        // the bridge down with it). The queue bounds its memory, and replies
        // wait for space, so a slow client only slows itself.
        let socket = writer.try_clone()?;
        let client = Arc::new(Self {
            outbox: Mutex::new(Outbox {
                queue: VecDeque::new(),
                bytes: 0,
                last_seq: 0,
                closed: false,
            }),
            outbox_ready: Condvar::new(),
            socket,
            seq: AtomicU64::new(0),
            randr_window: AtomicU32::new(0),
            cursor_window: AtomicU32::new(0),
            selections: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
            root_structure: AtomicBool::new(false),
            xkb_map_details: AtomicU16::new(0),
            xkb_nkn_details: AtomicU16::new(0),
        });
        let for_writer = client.clone();
        std::thread::spawn(move || for_writer.writer_loop(writer));
        Ok(client)
    }

    /// The writer thread: sends queued buffers in order until the outbox is
    /// closed and drained. A failed write (the client went away, or timed out)
    /// drops the client.
    fn writer_loop(&self, mut stream: UnixStream) {
        loop {
            let next = {
                let mut o = self.outbox.lock().unwrap();
                loop {
                    if let Some(item) = o.queue.pop_front() {
                        o.bytes -= item.0.len();
                        // a reply waiting for queue space (see `enqueue`)
                        self.outbox_ready.notify_all();
                        break Some(item);
                    }
                    if o.closed {
                        break None;
                    }
                    o = self.outbox_ready.wait(o).unwrap();
                }
            };
            let Some((buf, fd)) = next else { return };
            let written = match fd {
                // the fd travels with the first bytes of its reply, as libxcb
                // expects it no later than the reply itself
                Some(fd) => send_with_fd(&stream, &buf, fd.as_raw_fd())
                    .and_then(|n| stream.write_all(&buf[n..])),
                None => stream.write_all(&buf),
            };
            if let Err(e) = written {
                crate::vlog!("dropping client: write failed: {e}");
                self.drop_client();
                return;
            }
        }
    }

    /// Queues `buf` for the writer thread. Call with the outbox locked.
    ///
    /// A reply (`wait`) waits until the queue has room: the connection thread
    /// is the only producer of replies, so blocking it slows that client's
    /// requests and X does not let it run ahead of what it reads. An event
    /// (no `wait`) is dropped if it does not fit: the Wayland/capture thread
    /// pushes those for every client and must never wait on one, and events
    /// are recoverable (the next damage or configure carries the state again)
    /// whereas dropping the client could take a wrapped VNC server with it.
    /// Neither path drops the client: a client that misbehaves or disconnects
    /// must never end the bridge.
    fn enqueue<'a>(
        &self,
        mut o: MutexGuard<'a, Outbox>,
        buf: Vec<u8>,
        fd: Option<OwnedFd>,
        wait: bool,
    ) -> MutexGuard<'a, Outbox> {
        while o.bytes > 0 && o.bytes + buf.len() > MAX_QUEUED_BYTES {
            if o.closed || !wait {
                // an event does not fit: drop that event, not the client
                if !o.closed {
                    crate::vlog!(
                        "dropping server event for a client with {} bytes queued",
                        o.bytes
                    );
                }
                return o;
            }
            o = self.outbox_ready.wait(o).unwrap();
        }
        if o.closed {
            return o;
        }
        o.bytes += buf.len();
        o.queue.push_back((buf, fd));
        self.outbox_ready.notify_one();
        o
    }

    /// Marks the client dead and shuts its socket down, so the connection
    /// thread's next read ends and it cleans up, and discards anything still
    /// queued.
    fn drop_client(&self) {
        self.mark_dead();
        let mut o = self.outbox.lock().unwrap();
        o.closed = true;
        o.queue.clear();
        o.bytes = 0;
        let _ = self.socket.shutdown(Shutdown::Both);
        self.outbox_ready.notify_one();
    }

    /// Ends the queue once the connection is done: whatever is queued still
    /// goes out, then the writer thread exits. Called from the connection
    /// thread when it finishes.
    pub fn close(&self) {
        self.mark_dead();
        let mut o = self.outbox.lock().unwrap();
        o.closed = true;
        self.outbox_ready.notify_one();
    }

    /// Queues the connection-setup bytes, the only write with no sequence.
    pub fn write_setup(&self, bytes: &[u8]) -> std::io::Result<()> {
        let o = self.outbox.lock().unwrap();
        let o = self.enqueue(o, bytes.to_vec(), None, true);
        self.check_open(&o)
    }

    /// Queues a framed reply or error, stamping `seq` (clamped monotonic) into
    /// its sequence field at `[2..4]`. Fails once the client is gone, which is
    /// the connection thread's cue to stop.
    pub fn send_reply(&self, seq: u16, buf: &mut [u8]) -> std::io::Result<()> {
        let mut o = self.outbox.lock().unwrap();
        let seq = o.monotonic(widen(self.seq.load(Ordering::Relaxed), seq));
        buf[2..4].copy_from_slice(&(seq as u16).to_le_bytes());
        o.last_seq = seq;
        let o = self.enqueue(o, buf.to_vec(), None, true);
        self.check_open(&o)
    }

    /// [`send_reply`](Self::send_reply), handing the client `fd` with it
    /// (SCM_RIGHTS on the reply's first bytes).
    pub fn send_reply_fd(&self, seq: u16, buf: &mut [u8], fd: OwnedFd) -> std::io::Result<()> {
        let mut o = self.outbox.lock().unwrap();
        let seq = o.monotonic(widen(self.seq.load(Ordering::Relaxed), seq));
        buf[2..4].copy_from_slice(&(seq as u16).to_le_bytes());
        o.last_seq = seq;
        let o = self.enqueue(o, buf.to_vec(), Some(fd), true);
        self.check_open(&o)
    }

    /// Queues a server-initiated event, stamping the client's current request
    /// sequence under the outbox lock so it can't race a concurrent reply.
    pub fn send_event_bytes(&self, buf: &mut [u8]) {
        // Refusing a clipboard conversion sends through here from a `Drop`,
        // which happens while unwinding a panic, so a poisoned outbox must not
        // panic again: that would be a panic inside a drop during unwinding,
        // which aborts the process. The queue is still consistent enough to
        // take one more buffer, and the writer thread drops it if it is not.
        let mut o = self
            .outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = o.monotonic(self.seq.load(Ordering::Relaxed));
        buf[2..4].copy_from_slice(&(seq as u16).to_le_bytes());
        o.last_seq = seq;
        drop(self.enqueue(o, buf.to_vec(), None, false));
    }

    fn check_open(&self, o: &Outbox) -> std::io::Result<()> {
        if o.closed {
            Err(std::io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }

    /// The connection thread's sequence for the request it is about to
    /// handle (one more than the last: it is the only writer).
    pub fn set_seq(&self, seq: u16) {
        let prev = self.seq.load(Ordering::Relaxed);
        self.seq.store(widen(prev, seq), Ordering::Relaxed);
    }

    pub fn seq(&self) -> u16 {
        self.seq.load(Ordering::Relaxed) as u16
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub fn select_randr(&self, window: u32) {
        self.randr_window.store(window, Ordering::Relaxed);
    }

    pub fn select_cursor(&self, window: u32) {
        self.cursor_window.store(window, Ordering::Relaxed);
    }

    pub fn select_root_structure(&self, enable: bool) {
        self.root_structure.store(enable, Ordering::Relaxed);
    }

    /// Sets the XFixes selection event mask for `window` on the selection
    /// `atom`; an empty mask drops that registration and no other.
    pub fn select_selection(&self, atom: u32, window: u32, mask: xfixes::SelectionEventMask) {
        let mut sels = self.selections.lock().unwrap();
        sels.retain(|s| !(s.atom == atom && s.window == window));
        if u32::from(mask) != 0 {
            sels.push(SelReg { atom, window, mask });
        }
    }

    /// Drops every registration made from `window`, which was destroyed.
    pub fn forget_window(&self, window: u32) {
        self.selections
            .lock()
            .unwrap()
            .retain(|s| s.window != window);
    }

    /// The window selected for RandR ScreenChangeNotify, 0 if none.
    pub(super) fn randr_window(&self) -> u32 {
        self.randr_window.load(Ordering::Relaxed)
    }

    /// The window selected for XFixes cursor changes, 0 if none.
    pub(super) fn cursor_window(&self) -> u32 {
        self.cursor_window.load(Ordering::Relaxed)
    }

    pub(super) fn wants_root_structure(&self) -> bool {
        self.root_structure.load(Ordering::Relaxed)
    }

    /// Applies an XKB SelectEvents to the keymap-change events the client
    /// hears.
    pub fn select_xkb(&self, select: &crate::bridge::x11::xkb::Select) {
        let update = |a: &AtomicU16, c: &crate::bridge::x11::xkb::Change| {
            let _ = a.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(c.apply(v)));
        };
        update(&self.xkb_nkn_details, &select.nkn);
        update(&self.xkb_map_details, &select.map);
    }

    /// The (NewKeyboardNotify, MapNotify) details the client selected.
    pub fn xkb_details(&self) -> (u16, u16) {
        (
            self.xkb_nkn_details.load(Ordering::Relaxed),
            self.xkb_map_details.load(Ordering::Relaxed),
        )
    }

    /// The `(window, atom)` of each registration for the selection `atom`
    /// whose mask lets `subtype` through.
    pub fn selections_for(&self, atom: u32, subtype: xfixes::SelectionEvent) -> Vec<(u32, u32)> {
        let bit = mask_for(subtype);
        self.selections
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.atom == atom && r.mask.contains(bit))
            .map(|r| (r.window, r.atom))
            .collect()
    }
}

/// Sends (some of) `buf` with `fd` attached (SCM_RIGHTS), returning how many
/// bytes went; the rest is the caller's to write.
fn send_with_fd(stream: &UnixStream, buf: &[u8], fd: RawFd) -> std::io::Result<usize> {
    let fd_len = std::mem::size_of::<RawFd>() as u32;
    // SAFETY: CMSG_SPACE/CMSG_LEN only compute sizes.
    let space = unsafe { libc::CMSG_SPACE(fd_len) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives the
    // sendmsg call, and the control buffer has room for one cmsg of one fd.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_len) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        loop {
            let n = libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::Duration;

    use super::*;

    /// A 32-byte frame with a marker byte, to tell frames apart on the wire.
    fn frame(marker: u8) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[0] = marker;
        b
    }

    fn seq_of(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[2], frame[3]])
    }

    #[test]
    fn writes_arrive_in_order_with_a_monotonic_sequence() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        client.write_setup(&[0xaa; 8]).unwrap();
        // a reply at sequence 5, then an event stamped from a stale request
        // sequence of 3, which must not go backwards on the wire
        let mut reply = frame(1);
        client.send_reply(5, &mut reply).unwrap();
        client.set_seq(3);
        let mut event = frame(2);
        client.send_event_bytes(&mut event);
        // and a reply that has moved on
        let mut reply2 = frame(3);
        client.send_reply(6, &mut reply2).unwrap();

        let mut got = vec![0u8; 8 + 32 * 3];
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got[..8], &[0xaa; 8]);
        let frames: Vec<&[u8]> = got[8..].chunks(32).collect();
        assert_eq!(frames.iter().map(|f| f[0]).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(
            frames.iter().map(|f| seq_of(f)).collect::<Vec<_>>(),
            [5, 5, 6]
        );
        assert!(!client.is_dead());
    }

    #[test]
    fn replies_keep_their_sequence_after_many_requests_without_one() {
        // a client that sends 32768-65535 requests
        // with no reply in between (XTest input) got its next reply stamped
        // with the last reply's sequence
        for n in [
            100u32, 30_000, 40_000, 60_000, 65_533, 65_534, 65_535, 70_000, 200_000,
        ] {
            let (a, mut b) = UnixStream::pair().unwrap();
            let client = Client::new(a).unwrap();
            let mut seq = 1u16;
            client.set_seq(seq);
            client.send_reply(seq, &mut frame(1)).unwrap();
            for _ in 0..n {
                seq = seq.wrapping_add(1);
                client.set_seq(seq);
            }
            seq = seq.wrapping_add(1);
            client.set_seq(seq);
            client.send_reply(seq, &mut frame(2)).unwrap();
            let mut got = vec![0u8; 64];
            b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            b.read_exact(&mut got).unwrap();
            assert_eq!(
                seq_of(&got[32..]),
                seq,
                "after {n} requests without a reply"
            );
        }
    }

    #[test]
    fn a_reply_can_hand_the_client_an_fd() {
        use std::io::{Seek, SeekFrom, Write as _};
        use std::os::fd::FromRawFd;
        let (a, b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        let mut file = std::fs::File::from(test_memfd());
        file.write_all(b"segment").unwrap();
        let mut reply = frame(1);
        reply[1] = 1; // nfd
        client
            .send_reply_fd(9, &mut reply, OwnedFd::from(file.try_clone().unwrap()))
            .unwrap();
        // receive the 32 bytes and the fd
        let mut got = [0u8; 32];
        let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(4) } as usize];
        let mut iov = libc::iovec {
            iov_base: got.as_mut_ptr().cast(),
            iov_len: got.len(),
        };
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let fd = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = control.len() as _;
            let n = libc::recvmsg(b.as_raw_fd(), &mut msg, libc::MSG_WAITALL);
            assert_eq!(n, 32);
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            assert!(!cmsg.is_null(), "no fd came with the reply");
            assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
            OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                libc::CMSG_DATA(cmsg).cast::<RawFd>(),
            ))
        };
        assert_eq!((got[0], got[1], seq_of(&got)), (1, 1, 9));
        // it is the same file
        let mut theirs = std::fs::File::from(fd);
        theirs.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut theirs, &mut s).unwrap();
        assert_eq!(s, "segment");
    }

    fn test_memfd() -> OwnedFd {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::memfd_create(c"xwlbridge-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create");
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn close_sends_what_is_queued_then_ends_the_stream() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        let mut f = frame(7);
        client.send_reply(1, &mut f).unwrap();
        client.close();
        assert!(client.is_dead());
        assert!(client.send_reply(2, &mut frame(8)).is_err());
        // once every holder of the socket is gone, the reader sees EOF after
        // the queued frame
        drop(client);
        let mut got = Vec::new();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        b.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), 32);
        assert_eq!(got[0], 7);
    }

    #[test]
    fn a_slow_client_gets_backpressure_instead_of_being_dropped() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        // nobody reads `b` at first, so the socket buffer fills and the queue
        // grows to the cap; the reply that does not fit must wait there.
        let chunk = vec![0u8; 1 << 20];
        let sender = client.clone();
        let t = std::thread::spawn(move || {
            let mut n = 0;
            for _ in 0..70 {
                if sender.send_reply(1, &mut chunk.clone()).is_err() {
                    return n;
                }
                n += 1;
            }
            n
        });
        let mut at_cap = false;
        for _ in 0..500 {
            if client.outbox.lock().unwrap().bytes >= MAX_QUEUED_BYTES {
                at_cap = true;
                break;
            }
            if t.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            at_cap,
            "the sender never filled the {MAX_QUEUED_BYTES}-byte queue"
        );
        assert!(!t.is_finished(), "the reply did not wait for queue space");
        // a server event that does not fit is dropped; the client is not
        let mut ev = frame(1);
        client.send_event_bytes(&mut ev);
        assert!(!client.is_dead(), "a full queue dropped the client");
        // drain: the blocked reply completes and the client stays usable
        b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut got = 0usize;
        let mut sink = vec![0u8; 1 << 16];
        while got < 70 << 20 {
            let n = b.read(&mut sink).unwrap();
            assert!(n > 0);
            got += n;
        }
        assert_eq!(t.join().unwrap(), 70);
        assert!(
            !client.is_dead(),
            "the slow client was dropped after draining"
        );
        assert!(client.send_reply(1, &mut frame(2)).is_ok());
    }

    const CLIP: u32 = 83;
    const PRI: u32 = 1;
    const OWNER: xfixes::SelectionEventMask = xfixes::SelectionEventMask::SET_SELECTION_OWNER;
    const CLOSE: xfixes::SelectionEventMask = xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;

    fn client() -> Arc<Client> {
        let (a, _b) = UnixStream::pair().unwrap();
        Client::new(a).unwrap()
    }

    #[test]
    fn mask_bits_are_numbered_like_the_subtypes() {
        assert_eq!(mask_for(xfixes::SelectionEvent::SET_SELECTION_OWNER), OWNER);
        assert_eq!(
            mask_for(xfixes::SelectionEvent::SELECTION_WINDOW_DESTROY),
            xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
        );
        assert_eq!(
            mask_for(xfixes::SelectionEvent::SELECTION_CLIENT_CLOSE),
            CLOSE
        );
    }

    #[test]
    fn registrations_are_per_window_and_selection() {
        let c = client();
        c.select_selection(CLIP, 0x10, OWNER);
        c.select_selection(CLIP, 0x11, OWNER | CLOSE);
        c.select_selection(PRI, 0x10, OWNER);
        let owner = xfixes::SelectionEvent::SET_SELECTION_OWNER;
        assert_eq!(
            c.selections_for(CLIP, owner),
            [(0x10, CLIP), (0x11, CLIP)],
            "both windows are told"
        );
        assert_eq!(c.selections_for(PRI, owner), [(0x10, PRI)]);
        // clearing one window's mask leaves the other's registration alone
        c.select_selection(CLIP, 0x10, 0u32.into());
        assert_eq!(c.selections_for(CLIP, owner), [(0x11, CLIP)]);
        // re-selecting replaces the mask rather than adding a second record
        c.select_selection(CLIP, 0x11, OWNER);
        assert_eq!(c.selections_for(CLIP, owner), [(0x11, CLIP)]);
    }

    #[test]
    fn the_mask_picks_the_subtypes() {
        let c = client();
        c.select_selection(CLIP, 0x10, CLOSE);
        assert!(
            c.selections_for(CLIP, xfixes::SelectionEvent::SET_SELECTION_OWNER)
                .is_empty()
        );
        assert_eq!(
            c.selections_for(CLIP, xfixes::SelectionEvent::SELECTION_CLIENT_CLOSE),
            [(0x10, CLIP)]
        );
    }

    #[test]
    fn a_destroyed_window_takes_its_registrations_with_it() {
        let c = client();
        c.select_selection(CLIP, 0x10, OWNER);
        c.select_selection(200, 0x10, OWNER);
        c.select_selection(CLIP, 0x11, OWNER);
        c.forget_window(0x10);
        let owner = xfixes::SelectionEvent::SET_SELECTION_OWNER;
        assert_eq!(c.selections_for(CLIP, owner), [(0x11, CLIP)]);
        assert!(c.selections_for(200, owner).is_empty());
    }

    #[test]
    fn every_selection_is_found_by_its_atom() {
        // atoms are the server's, so a registration needs nothing else to
        // identify the selection it is for, bridged or not
        let c = client();
        c.select_selection(200, 0x10, OWNER);
        c.select_selection(CLIP, 0x10, OWNER);
        let owner = xfixes::SelectionEvent::SET_SELECTION_OWNER;
        assert_eq!(c.selections_for(200, owner), [(0x10, 200)]);
        assert_eq!(c.selections_for(CLIP, owner), [(0x10, CLIP)]);
        assert!(c.selections_for(999, owner).is_empty());
    }
}
