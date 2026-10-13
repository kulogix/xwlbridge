//! Resolution changes requested by an X client, on their way to the compositor
//! (`-dynres`).
//!
//! A RandR client (RealVNC with the `dynres/` hook, or plain `xrandr`)
//! reconfigures a CRTC from an X connection thread, but only the Wayland thread
//! may touch the `wlr-output-management` objects that carry the change to the
//! compositor. So the X side parks its request here, wakes the Wayland thread,
//! and blocks until that thread reports how the compositor answered — exactly
//! as `RRSetCrtcConfig` blocks in Xorg while the driver mode-sets. The reply the
//! client gets is therefore truthful, and the RandR model never says something
//! the compositor did not do: the change reaches the model the ordinary way, as
//! a `wl_output` change, before the X side is woken.
//!
//! One request is handled at a time: RealVNC drives resizes from a single
//! connection, and a compositor configuration is single-use anyway.
//!
//! A backend that puts the compositor back once nobody watches (GNOME: a
//! temporary monitor configuration is never undone by mutter itself) also
//! gets a last chance to do so when the process exits: see
//! [`DynRes::finish`].

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A mode change for one Wayland output, in the screen's (transformed)
/// physical pixels, i.e. what the X client asked for.
#[derive(Clone, Copy, Debug)]
pub struct ModeRequest {
    pub id: u64,
    pub wl_name: u32,
    pub width: u16,
    pub height: u16,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Parked by an X thread, not yet picked up by the Wayland thread.
    queued: Option<ModeRequest>,
    /// Picked up, and not yet completed.
    in_flight: Option<u64>,
    /// Requests whose callers still wait for the answer.
    waiting: Vec<u64>,
    /// Answers not yet collected by their callers. Each caller takes only its
    /// own: with several callers queued, the next one may run (and enqueue)
    /// before the previous one has woken up to read its answer.
    done: Vec<(u64, Result<(), String>)>,
    /// The process is exiting and wants the backend to undo its changes.
    finish_requested: bool,
    finish_done: bool,
}

pub struct DynRes {
    inner: Mutex<Inner>,
    cv: Condvar,
    /// An eventfd the Wayland thread polls, so a parked request is picked up at
    /// once rather than at the end of the capture interval.
    wake: OwnedFd,
    /// Whether the Wayland thread has an output manager to carry changes over.
    available: AtomicBool,
    /// Whether the backend has changed something it would put back on exit.
    undo_pending: AtomicBool,
}

impl Default for DynRes {
    fn default() -> Self {
        // SAFETY: eventfd takes no pointers; the fd is owned from here on
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(
            fd >= 0,
            "eventfd failed: {}",
            std::io::Error::last_os_error()
        );
        Self {
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
            // SAFETY: a fresh, valid fd nobody else owns
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            available: AtomicBool::new(false),
            undo_pending: AtomicBool::new(false),
        }
    }
}

impl DynRes {
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    /// Whether a request stands any chance: the Wayland thread has bound an
    /// output manager. Checked by the X side so a request never waits on a
    /// thread that will not answer.
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    /// Whether a request is parked or in progress (an X client's RandR change
    /// is half done meanwhile: RealVNC disables the CRTC before it asks).
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend asks
    pub fn busy(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.queued.is_some() || inner.in_flight.is_some()
    }

    /// Asks the Wayland thread to drive `wl_name` at `width`x`height` and waits
    /// for the outcome. `Err` carries why it did not happen: the compositor
    /// refused, nothing answered within `timeout`, or another request is still
    /// in progress. Called from an X connection thread, never with the screen
    /// lock held (the Wayland thread needs it to finish).
    pub fn change_mode(
        &self,
        wl_name: u32,
        width: u16,
        height: u16,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        // one at a time; a second client asking concurrently waits its turn
        while inner.queued.is_some() || inner.in_flight.is_some() {
            let now = Instant::now();
            if now >= deadline {
                return Err("another resolution change is still in progress".into());
            }
            inner = self.cv.wait_timeout(inner, deadline - now).unwrap().0;
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.queued = Some(ModeRequest {
            id,
            wl_name,
            width,
            height,
        });
        inner.waiting.push(id);
        self.wake_wayland();
        loop {
            if let Some(i) = inner.done.iter().position(|(d, _)| *d == id) {
                inner.waiting.retain(|&w| w != id);
                return inner.done.swap_remove(i).1;
            }
            let now = Instant::now();
            if now >= deadline {
                // if still queued, nobody will ever act on it; if in flight the
                // Wayland thread finishes on its own and the result is dropped
                if inner.queued.is_some_and(|r| r.id == id) {
                    inner.queued = None;
                }
                inner.waiting.retain(|&w| w != id);
                return Err("the compositor did not answer in time".into());
            }
            inner = self.cv.wait_timeout(inner, deadline - now).unwrap().0;
        }
    }

    fn wake_wayland(&self) {
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a u64 to an eventfd
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// The fd to poll for a parked request (Wayland thread).
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Clears the wake fd after it polled readable (Wayland thread).
    pub fn drain_wake(&self) {
        let mut buf = 0u64;
        // SAFETY: reading 8 bytes into a u64 from an eventfd; EAGAIN is fine
        unsafe {
            libc::read(
                self.wake.as_raw_fd(),
                (&mut buf as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// Takes the parked request, if any, marking it in flight (Wayland thread).
    pub fn take(&self) -> Option<ModeRequest> {
        let mut inner = self.inner.lock().unwrap();
        if inner.in_flight.is_some() {
            return None;
        }
        let req = inner.queued.take()?;
        inner.in_flight = Some(req.id);
        Some(req)
    }

    /// Says whether the compositor currently carries a change the backend
    /// would undo on exit (backend thread).
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend undoes
    pub fn set_undo_pending(&self, pending: bool) {
        self.undo_pending.store(pending, Ordering::Release);
    }

    /// Asks the backend to undo its changes before the process exits, and
    /// waits up to `timeout` for it to report back. Returns at once when there
    /// is nothing to undo. Called by the main thread after the wrapped command
    /// exited.
    pub fn finish(&self, timeout: Duration) {
        if !self.undo_pending.load(Ordering::Acquire) {
            return;
        }
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        inner.finish_requested = true;
        self.wake_wayland();
        while !inner.finish_done {
            let now = Instant::now();
            if now >= deadline {
                crate::warning!(
                    "the compositor's display configuration could not be restored in time"
                );
                return;
            }
            inner = self.cv.wait_timeout(inner, deadline - now).unwrap().0;
        }
    }

    /// Whether [`finish`](Self::finish) is waiting (backend thread, after its
    /// wake fd polled readable).
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend undoes
    pub fn finish_requested(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.finish_requested && !inner.finish_done
    }

    /// Reports that the changes were undone, or could not be (backend thread).
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))] // only the gnome backend undoes
    pub fn finished(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.finish_done = true;
        self.undo_pending.store(false, Ordering::Release);
        self.cv.notify_all();
    }

    /// Reports the outcome of the in-flight request and wakes its waiter
    /// (Wayland thread).
    pub fn complete(&self, id: u64, result: Result<(), String>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.in_flight == Some(id) {
            inner.in_flight = None;
        }
        // a caller that gave up waiting no longer collects its answer
        if inner.waiting.contains(&id) {
            inner.done.push((id, result));
        }
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Several X-side requesters at once (the stock engines' agent asks from
    /// helper threads) against a backend that answers each request at once:
    /// every requester must get its own answer, never time out because a
    /// later requester cleared it first.
    #[test]
    fn concurrent_requesters_each_get_their_answer() {
        let d = Arc::new(DynRes::default());
        d.set_available(true);
        let stop = Arc::new(AtomicBool::new(false));
        let backend = {
            let (d, stop) = (d.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match d.take() {
                        Some(req) => d.complete(req.id, Ok(())),
                        None => std::thread::yield_now(),
                    }
                }
            })
        };
        let failed = Arc::new(AtomicUsize::new(0));
        let requesters: Vec<_> = (0..6)
            .map(|t| {
                let (d, failed) = (d.clone(), failed.clone());
                std::thread::spawn(move || {
                    for i in 0..60u16 {
                        if d.change_mode(1, 640 + t, 480 + i, Duration::from_secs(2))
                            .is_err()
                        {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for r in requesters {
            r.join().unwrap();
        }
        stop.store(true, Ordering::Release);
        backend.join().unwrap();
        assert_eq!(
            failed.load(Ordering::Relaxed),
            0,
            "requests lost their answer"
        );
    }

    /// A refusal reaches the requester that asked, with the backend's reason.
    #[test]
    fn a_refusal_carries_its_reason() {
        let d = Arc::new(DynRes::default());
        let backend = {
            let d = d.clone();
            std::thread::spawn(move || {
                loop {
                    if let Some(req) = d.take() {
                        d.complete(req.id, Err(format!("no {}x{}", req.width, req.height)));
                        break;
                    }
                    std::thread::yield_now();
                }
            })
        };
        let r = d.change_mode(7, 1366, 768, Duration::from_secs(2));
        backend.join().unwrap();
        assert_eq!(r, Err("no 1366x768".into()));
    }

    /// Nobody picks the request up: it times out and is withdrawn, so a later
    /// backend pass does not act on it.
    #[test]
    fn an_unanswered_request_is_withdrawn() {
        let d = DynRes::default();
        let r = d.change_mode(7, 800, 600, Duration::from_millis(50));
        assert!(r.is_err());
        assert!(d.take().is_none());
    }
}
