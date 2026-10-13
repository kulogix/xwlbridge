//! The core screen-saver requests (`SetScreenSaver`, `GetScreenSaver`,
//! `ForceScreenSaver`).
//!
//! The desktop blanks and locks the screen, not this server, so nothing here
//! ever activates a saver: the settings a client makes are kept, so that it
//! reads back what it set (`xset s 300 600` then `xset q`), and forcing the
//! saver on or resetting it does nothing. Before these were answered,
//! `xset s` and anything calling `XSetScreenSaver`/`XResetScreenSaver`/
//! `XForceScreenSaver` (screen lockers, video players keeping the screen on)
//! failed with BadImplementation, which kills an Xlib client with the default
//! error handler.

use std::sync::{Mutex, PoisonError};

use x11rb_protocol::protocol::xproto::{Blanking, Exposures};

/// What `GetScreenSaver` reports, as `SetScreenSaver` left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Seconds of idle time before the saver would start; 0 is off.
    pub timeout: i16,
    /// Seconds between saver pattern changes.
    pub interval: i16,
    pub prefer_blanking: Blanking,
    pub allow_exposures: Exposures,
}

impl Settings {
    /// This server's defaults, which `-1` / `Default` in a request restore:
    /// no saver of its own (the desktop has one).
    pub const DEFAULT: Self = Self {
        timeout: 0,
        interval: 0,
        prefer_blanking: Blanking::NOT_PREFERRED,
        allow_exposures: Exposures::NOT_ALLOWED,
    };

    /// Applies one `SetScreenSaver`, as X does: `-1` restores the default
    /// time, `Default` the default choice, and anything else out of range is
    /// a BadValue (`Err`, naming the value), with nothing changed.
    pub fn set(
        &mut self,
        timeout: i16,
        interval: i16,
        prefer_blanking: Blanking,
        allow_exposures: Exposures,
    ) -> Result<(), u32> {
        let time = |v: i16, default: i16| match v {
            -1 => Ok(default),
            // X reports the value sign-extended, as Xorg does
            v if v < -1 => Err(i32::from(v) as u32),
            v => Ok(v),
        };
        let timeout = time(timeout, Self::DEFAULT.timeout)?;
        let interval = time(interval, Self::DEFAULT.interval)?;
        let prefer_blanking = match prefer_blanking {
            Blanking::DEFAULT => Self::DEFAULT.prefer_blanking,
            b if b == Blanking::NOT_PREFERRED || b == Blanking::PREFERRED => b,
            b => return Err(u32::from(u8::from(b))),
        };
        let allow_exposures = match allow_exposures {
            Exposures::DEFAULT => Self::DEFAULT.allow_exposures,
            e if e == Exposures::NOT_ALLOWED || e == Exposures::ALLOWED => e,
            e => return Err(u32::from(u8::from(e))),
        };
        *self = Self {
            timeout,
            interval,
            prefer_blanking,
            allow_exposures,
        };
        Ok(())
    }
}

/// The server's settings (there is one screen, and one server per process).
static SETTINGS: Mutex<Settings> = Mutex::new(Settings::DEFAULT);

/// The current settings.
pub fn get() -> Settings {
    *SETTINGS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `SetScreenSaver`: see [`Settings::set`].
pub fn set(
    timeout: i16,
    interval: i16,
    prefer_blanking: Blanking,
    allow_exposures: Exposures,
) -> Result<(), u32> {
    SETTINGS.lock().unwrap_or_else(PoisonError::into_inner).set(
        timeout,
        interval,
        prefer_blanking,
        allow_exposures,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_and_defaults_restore() {
        let mut s = Settings::DEFAULT;
        // xset s 300 600; xset s blank; xset s expose
        s.set(300, 600, Blanking::PREFERRED, Exposures::ALLOWED)
            .unwrap();
        assert_eq!(
            s,
            Settings {
                timeout: 300,
                interval: 600,
                prefer_blanking: Blanking::PREFERRED,
                allow_exposures: Exposures::ALLOWED,
            }
        );
        // xset s default
        s.set(-1, -1, Blanking::DEFAULT, Exposures::DEFAULT)
            .unwrap();
        assert_eq!(s, Settings::DEFAULT);
        // xset s off
        s.set(0, 0, Blanking::DEFAULT, Exposures::DEFAULT).unwrap();
        assert_eq!(s.timeout, 0);
    }

    #[test]
    fn out_of_range_values_are_refused_and_change_nothing() {
        let mut s = Settings::DEFAULT;
        s.set(10, 20, Blanking::PREFERRED, Exposures::ALLOWED)
            .unwrap();
        let before = s;
        assert_eq!(
            s.set(-2, 0, Blanking::DEFAULT, Exposures::DEFAULT),
            Err(0xffff_fffe)
        );
        assert_eq!(
            s.set(0, -5, Blanking::DEFAULT, Exposures::DEFAULT),
            Err(0xffff_fffb)
        );
        assert_eq!(s.set(0, 0, Blanking::from(3u8), Exposures::DEFAULT), Err(3));
        assert_eq!(s.set(0, 0, Blanking::DEFAULT, Exposures::from(7u8)), Err(7));
        assert_eq!(s, before);
    }
}
