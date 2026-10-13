use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Manages the shared-memory segments clients use with MIT-SHM: SysV
/// segments they attach (`ShmAttach`), and segments created here for them
/// (`ShmCreateSegment`, MIT-SHM 1.2), handed over as a file descriptor.
#[derive(Default)]
pub(super) struct Shm {
    segments: HashMap<u32, ShmSeg>,
}

pub(super) struct ShmSeg {
    pub ptr: *mut u8,
    pub size: usize,
    /// Mapped from a memfd created here, so unmapped rather than detached.
    mapped: bool,
}

/// The largest segment `ShmCreateSegment` makes: a 16384x16384 32-bit image,
/// the biggest screen this server has. The memory is only committed as it is
/// written (ShmGetImage writes at most one image into it).
pub(super) const MAX_CREATED_SEGMENT: u32 = 1 << 30;

impl Shm {
    pub fn attach(&mut self, shmseg: u32, shmid: u32) {
        let ptr = unsafe { libc::shmat(shmid as i32, std::ptr::null(), 0) };
        if std::ptr::eq(ptr, libc::MAP_FAILED) {
            crate::warning!("shmat failed for shmid {shmid}");
            return;
        }
        let size = unsafe {
            let mut ds: libc::shmid_ds = std::mem::zeroed();
            if libc::shmctl(shmid as i32, libc::IPC_STAT, &mut ds) == 0 {
                ds.shm_segsz as usize
            } else {
                0
            }
        };
        self.insert(
            shmseg,
            ShmSeg {
                ptr: ptr.cast(),
                size,
                mapped: false,
            },
        );
    }

    /// `ShmCreateSegment`: a new segment of `size` bytes (1..=
    /// [`MAX_CREATED_SEGMENT`]), a memfd mapped here and registered as
    /// `shmseg`; returns the fd for the client, which maps it too.
    pub fn create(&mut self, shmseg: u32, size: usize) -> io::Result<OwnedFd> {
        // SAFETY: plain syscalls; the fd is owned from here on, and the
        // mapping is recorded and unmapped on detach/drop.
        unsafe {
            let raw = libc::memfd_create(c"xwlbridge-shm".as_ptr(), libc::MFD_CLOEXEC);
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(raw);
            if libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            self.insert(
                shmseg,
                ShmSeg {
                    ptr: ptr.cast(),
                    size,
                    mapped: true,
                },
            );
            Ok(fd)
        }
    }

    /// Registers a segment, releasing one the id named before (a client
    /// reusing an id without `ShmDetach` must not leak the old mapping).
    fn insert(&mut self, shmseg: u32, seg: ShmSeg) {
        if let Some(old) = self.segments.insert(shmseg, seg) {
            release(old);
        }
    }

    pub fn detach(&mut self, shmseg: u32) {
        if let Some(seg) = self.segments.remove(&shmseg) {
            release(seg);
        }
    }

    pub fn segment(&self, shmseg: u32) -> Option<&ShmSeg> {
        self.segments.get(&shmseg)
    }
}

fn release(seg: ShmSeg) {
    unsafe {
        if seg.mapped {
            libc::munmap(seg.ptr.cast(), seg.size);
        } else {
            libc::shmdt(seg.ptr.cast());
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        for (_, seg) in self.segments.drain() {
            release(seg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_created_segment_is_shared_with_the_fd_it_returns() {
        let mut shm = Shm::default();
        let fd = shm.create(7, 4096).unwrap();
        let seg = shm.segment(7).unwrap();
        assert_eq!(seg.size, 4096);
        // what the server writes, the client reads through its own mapping
        unsafe { seg.ptr.add(100).write(0x5a) };
        let theirs = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(theirs, libc::MAP_FAILED);
        assert_eq!(unsafe { *(theirs as *const u8).add(100) }, 0x5a);
        unsafe { libc::munmap(theirs, 4096) };
        // reusing the id replaces (and releases) the old segment
        let _fd2 = shm.create(7, 8192).unwrap();
        assert_eq!(shm.segment(7).unwrap().size, 8192);
        shm.detach(7);
        assert!(shm.segment(7).is_none());
    }
}
