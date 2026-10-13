//! `-a11ybus`: a private accessibility (AT-SPI) bus for the engine display.
//!
//! Every toolkit with an AT-SPI bridge finds its bus through the session
//! D-Bus (`org.a11y.Bus`), not through its X display, so without this the apps
//! on the engine display register on the desktop's accessibility bus: the
//! desktop's screen reader then follows windows nobody can see, and reads them
//! while the keyboard goes elsewhere.
//!
//! With it the engine gets a bus of its own (a `dbus-daemon` run with
//! at-spi2-core's `accessibility.conf`, which activates the registry on it), in
//! the engine's private 0700 directory. Apps find it through the `AT_SPI_BUS`
//! property on the engine's root window (GTK 3, Qt, Electron, Java/ATK) and
//! through `AT_SPI_BUS_ADDRESS` in their environment (GTK 4 reads only that),
//! which the wrapped command gets. A GNOME-side proxy
//! (tools/x11-a11y-proxy) reads the trees there and republishes them on the
//! desktop's bus, where they are visible and in step with the keyboard.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where at-spi2-core installs the bus configuration (`$datadir/defaults/at-spi2`).
const CONFIGS: &[&str] = &[
    "/usr/share/defaults/at-spi2/accessibility.conf",
    "/usr/local/share/defaults/at-spi2/accessibility.conf",
];

/// How long the bus gets to put its socket up.
const READY: Duration = Duration::from_secs(5);

/// The running bus; stopped (and its socket removed) when dropped.
pub struct Bus {
    child: Child,
    socket: PathBuf,
    /// The D-Bus address, `unix:path=...`.
    pub address: String,
}

impl Drop for Bus {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(self.child.try_wait(), Ok(None)) {
                if Instant::now() >= deadline {
                    let _ = self.child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The bus configuration file, if at-spi2-core is installed.
fn config() -> Option<&'static Path> {
    CONFIGS.iter().map(Path::new).find(|p| p.is_file())
}

/// Starts the bus with its socket in `dir` (private to this user); the
/// registry it activates talks to display `:display`.
pub fn start(dir: &Path, display: u32) -> io::Result<Bus> {
    let conf = config().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no at-spi2-core bus configuration (accessibility.conf); is at-spi2-core installed?",
        )
    })?;
    let socket = dir.join("a11y-bus");
    let address = format!("unix:path={}", socket.display());
    let mut cmd = Command::new("dbus-daemon");
    cmd.arg(format!("--config-file={}", conf.display()))
        .arg(format!("--address={address}"))
        .args(["--nofork", "--nopidfile"])
        // the registry it activates finds its bus here (and the engine's
        // display for the legacy device event controller), not the desktop's
        .env("AT_SPI_BUS_ADDRESS", &address)
        .env("DISPLAY", format!(":{display}"))
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            // the bus must not outlive us
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("can't start dbus-daemon: {e}")))?;
    let mut bus = Bus { child, socket, address };
    let deadline = Instant::now() + READY;
    while !bus.socket.exists() {
        if let Ok(Some(status)) = bus.child.try_wait() {
            return Err(io::Error::other(format!("dbus-daemon exited ({status})")));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("dbus-daemon didn't put its socket up"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(bus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bus_lives_in_the_given_directory() {
        let Some(_) = config() else { return }; // at-spi2-core not installed here
        if Command::new("dbus-daemon").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("xwlbridge-a11ybus-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let bus = start(&dir, 99).unwrap();
            assert!(bus.address.starts_with("unix:path="));
            assert!(bus.address.ends_with("/a11y-bus"));
            assert!(dir.join("a11y-bus").exists());
        }
        assert!(!dir.join("a11y-bus").exists(), "the socket goes with the bus");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
