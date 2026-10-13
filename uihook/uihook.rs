//! RealVNC's UI hook: puts RealVNC's UI process on the desktop's X display,
//! and keeps it from crashing where it stays on the engine's.
//!
//! RealVNC Server's core (`vncserver-x11-core`) starts its UI process
//! (`vncserverui`: the status dialog and tray icon, connection notices, chat,
//! file transfer and the clipboard) itself, by absolute path, talking to it
//! over stdin/stdout, with `DISPLAY` set to the display it remotes — the
//! engine's, where nobody can see it. No RealVNC parameter separates the two.
//!
//! xwlrvnc preloads this into the core, whose children inherit it. In a
//! process running `vncserverui` it, before `main`:
//!
//! - points `DISPLAY` and `XAUTHORITY` at `XWLRVNC_UI_DISPLAY` /
//!   `XWLRVNC_UI_XAUTHORITY` (an empty value unsets it) — if `XWLRVNC_UI_GATE`
//!   names a file, only while that file starts with `1` (xwlrvnc keeps it at
//!   `0` while the UI display can't be used, so the UI stays where it works);
//! - puts `LD_PRELOAD` back to `XWLRVNC_UI_LD_PRELOAD` (what it was before
//!   xwlrvnc added this hook) and drops its own variables, so what the UI
//!   launches (its tray icon helper, a browser for a help link) inherits a
//!   plain environment for the display the UI is on.
//!
//! It also stands in for libX11's `XkbGetNames` in `vncserverui`: on an X
//! server without the XKEYBOARD extension (xwlrvnc's built-in engine) the UI
//! asks for the keyboard's symbols name, gets none, and builds a C++ string
//! from the NULL — a segfault on every start, which RealVNC answers by
//! starting it again every 2 s. There the hook names the symbols "unknown",
//! what RealVNC itself reports when it can't tell. Everywhere else it does
//! nothing.
//!
//! Built by build.rs with rustc directly (one file, no dependencies, libc only)
//! and embedded in xwlrvnc, which writes it out when it runs RealVNC.

#![no_std]

use core::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

#[link(name = "c")]
unsafe extern "C" {
    fn getenv(name: *const c_char) -> *mut c_char;
    fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
    fn unsetenv(name: *const c_char) -> c_int;
    fn readlink(path: *const c_char, buf: *mut c_char, len: usize) -> isize;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn abort() -> !;
}

/// Whether this process is RealVNC's UI (`vncserverui`).
static IN_UI: AtomicBool = AtomicBool::new(false);

#[used]
#[unsafe(link_section = ".init_array")]
static INIT: extern "C" fn() = init;

extern "C" fn init() {
    let mut buf = [0u8; 4096];
    // SAFETY: the buffer is writable for its length
    let n = unsafe { readlink(c"/proc/self/exe".as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 || n as usize >= buf.len() {
        return;
    }
    let exe = &buf[..n as usize];
    let name = exe.rsplit(|&b| b == b'/').next().unwrap_or(exe);
    if name != b"vncserverui" {
        return;
    }
    IN_UI.store(true, Ordering::Relaxed);
    // SAFETY: getenv/setenv/unsetenv in a constructor, before any other thread
    // of this process exists; the strings are NUL-terminated
    unsafe {
        let display = getenv(c"XWLRVNC_UI_DISPLAY".as_ptr());
        let gate = getenv(c"XWLRVNC_UI_GATE".as_ptr());
        let set_gate = !gate.is_null() && *gate != 0;
        if !display.is_null() && *display != 0 && (!set_gate || gate_open(gate)) {
            set(c"DISPLAY", display);
            set(c"XAUTHORITY", getenv(c"XWLRVNC_UI_XAUTHORITY".as_ptr()));
        }
        let preload = getenv(c"XWLRVNC_UI_LD_PRELOAD".as_ptr());
        if preload.is_null() || *preload == 0 {
            unsetenv(c"LD_PRELOAD".as_ptr());
        } else {
            setenv(c"LD_PRELOAD".as_ptr(), preload, 1);
        }
        for var in [c"XWLRVNC_UI_DISPLAY", c"XWLRVNC_UI_XAUTHORITY", c"XWLRVNC_UI_LD_PRELOAD", c"XWLRVNC_UI_GATE"] {
            unsetenv(var.as_ptr());
        }
    }
}

/// Whether the gate file `path` says the UI display is usable (starts with
/// `1`). Unreadable or missing: no, the UI stays on the engine display.
unsafe fn gate_open(path: *const c_char) -> bool {
    // SAFETY: path is NUL-terminated; the buffer is writable for its length
    unsafe {
        let fd = open(path, 0); // O_RDONLY
        if fd < 0 {
            return false;
        }
        let mut b = 0u8;
        let n = read(fd, (&raw mut b).cast(), 1);
        close(fd);
        n == 1 && b == b'1'
    }
}

/// Sets `name` to `value`; unsets it if `value` is empty. Leaves it alone if
/// `value` is null (the variable was not given).
unsafe fn set(name: &CStr, value: *const c_char) {
    if value.is_null() {
        return;
    }
    unsafe {
        if *value == 0 {
            unsetenv(name.as_ptr());
        } else {
            setenv(name.as_ptr(), value, 1);
        }
    }
}

/// The head of libX11's `XkbNamesRec` (Atom = unsigned long).
#[repr(C)]
pub struct XkbNamesRec {
    keycodes: c_ulong,
    geometry: c_ulong,
    symbols: c_ulong,
}

/// The head of libX11's `XkbDescRec`, up to `names`.
#[repr(C)]
pub struct XkbDescRec {
    dpy: *mut c_void,
    flags: u16,
    device_spec: u16,
    min_key_code: u8,
    max_key_code: u8,
    ctrls: *mut c_void,
    server: *mut c_void,
    map: *mut c_void,
    indicators: *mut c_void,
    names: *mut XkbNamesRec,
}

const XKB_SYMBOLS_NAME_MASK: c_uint = 1 << 2;
const SUCCESS: c_int = 0;
const BAD_IMPLEMENTATION: c_int = 17;
const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;
const RTLD_DEFAULT: *mut c_void = core::ptr::null_mut();

/// libX11's `XkbGetNames`, except that in `vncserverui` a request for the
/// symbols name that leaves none (no XKEYBOARD on the display) gets the name
/// "unknown" instead of NULL, which RealVNC's UI would crash on.
///
/// # Safety
/// As libX11's: `dpy` an open display, `xkb` from `XkbAllocKeyboard`/`XkbGetMap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn XkbGetNames(dpy: *mut c_void, which: c_uint, xkb: *mut XkbDescRec) -> c_int {
    type GetNames = unsafe extern "C" fn(*mut c_void, c_uint, *mut XkbDescRec) -> c_int;
    type AllocNames = unsafe extern "C" fn(*mut XkbDescRec, c_uint, c_int, c_int) -> c_int;
    type InternAtom = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_ulong;
    // SAFETY: the symbols are libX11's, with these signatures; the caller
    // passes what libX11's own function expects
    unsafe {
        let real = dlsym(RTLD_NEXT, c"XkbGetNames".as_ptr());
        let status = if real.is_null() {
            BAD_IMPLEMENTATION
        } else {
            core::mem::transmute::<*mut c_void, GetNames>(real)(dpy, which, xkb)
        };
        if !IN_UI.load(Ordering::Relaxed) || which & XKB_SYMBOLS_NAME_MASK == 0 || xkb.is_null() {
            return status;
        }
        if !(*xkb).names.is_null() && (*(*xkb).names).symbols != 0 {
            return status;
        }
        let alloc = dlsym(RTLD_DEFAULT, c"XkbAllocNames".as_ptr());
        let intern = dlsym(RTLD_DEFAULT, c"XInternAtom".as_ptr());
        if alloc.is_null() || intern.is_null() {
            return status;
        }
        if (*xkb).names.is_null() {
            core::mem::transmute::<*mut c_void, AllocNames>(alloc)(xkb, XKB_SYMBOLS_NAME_MASK, 0, 0);
            if (*xkb).names.is_null() {
                return status;
            }
        }
        let atom = core::mem::transmute::<*mut c_void, InternAtom>(intern)(dpy, c"unknown".as_ptr(), 0);
        if atom == 0 {
            return status;
        }
        (*(*xkb).names).symbols = atom;
        SUCCESS
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: always safe to call
    unsafe { abort() }
}
