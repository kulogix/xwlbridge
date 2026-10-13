use std::sync::Mutex;

/// Cursor image and position, written by the Wayland capture thread and read by
/// X connection threads for XFixes `GetCursorImage`/`GetCursorImageAndName`,
/// and by the stock engines' agent, which makes the image the X server's cursor.
#[derive(Default)]
pub struct CursorState {
    inner: Mutex<CursorInner>,
    /// Called (outside the lock) after each image change, to wake a consumer
    /// that isn't an X connection (the stock engines' agent).
    notify: Mutex<Option<Box<dyn Fn() + Send>>>,
}

#[derive(Default)]
struct CursorInner {
    serial: u32, // monotonic, 0 if nothing captured yet
    width: u16,
    height: u16,
    xhot: u16, // hotspot within the image
    yhot: u16,
    x: i16, // hotspot position in root coordinates
    y: i16,
    image: Vec<u32>, // ARGB, native endian (0xAARRGGBB on little-endian)
}

/// A consistent copy of the cursor, taken under the lock.
pub struct CursorSnapshot {
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    pub x: i16,
    pub y: i16,
    pub image: Vec<u32>,
}

impl CursorState {
    /// Replaces the image, returning the new serial for
    /// [`EventSink::cursor_changed`](crate::bridge::event::EventSink::cursor_changed).
    pub fn update_image(&self, w: u16, h: u16, xhot: u16, yhot: u16, image: Vec<u32>) -> u32 {
        let serial = {
            let mut g = self.inner.lock().unwrap();
            g.width = w;
            g.height = h;
            g.xhot = xhot;
            g.yhot = yhot;
            g.image = image;
            g.serial = g.serial.wrapping_add(1).max(1);
            g.serial
        };
        if let Some(f) = self.notify.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
            f();
        }
        serial
    }

    pub fn update_position(&self, x: i16, y: i16) {
        let mut g = self.inner.lock().unwrap();
        g.x = x;
        g.y = y;
    }

    pub fn snapshot(&self) -> CursorSnapshot {
        let g = self.inner.lock().unwrap();
        CursorSnapshot {
            serial: g.serial,
            width: g.width,
            height: g.height,
            xhot: g.xhot,
            yhot: g.yhot,
            x: g.x,
            y: g.y,
            image: g.image.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn serial(&self) -> u32 {
        self.inner.lock().unwrap().serial
    }

    /// Sets the function called after every image change (replacing any).
    #[allow(dead_code)]
    pub fn set_notify(&self, f: Box<dyn Fn() + Send>) {
        *self.notify.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(f);
    }

    /// Removes the function (its consumer is gone); no call of it is in
    /// progress once this returns.
    #[allow(dead_code)]
    pub fn clear_notify(&self) {
        let f = self.notify.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        drop(f);
    }
}

#[cfg(test)]
mod tests {
    use super::CursorState;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_cleared_notify_is_dropped_and_not_called() {
        let c = CursorState::default();
        let n = Arc::new(AtomicU32::new(0));
        let n2 = n.clone();
        c.set_notify(Box::new(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        }));
        c.update_image(1, 1, 0, 0, vec![0]);
        c.clear_notify();
        c.update_image(1, 1, 0, 0, vec![0]);
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&n), 1);
    }

    #[test]
    fn image_changes_notify_and_bump_the_serial() {
        let c = CursorState::default();
        assert_eq!(c.serial(), 0);
        let calls = Arc::new(AtomicU32::new(0));
        let n = calls.clone();
        c.set_notify(Box::new(move || {
            n.fetch_add(1, Ordering::Relaxed);
        }));
        let s1 = c.update_image(2, 1, 0, 0, vec![0xff00_0000, 0]);
        c.update_position(5, 6); // position alone doesn't notify
        let s2 = c.update_image(1, 1, 0, 0, vec![0]);
        assert!(s1 >= 1 && s2 > s1);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let snap = c.snapshot();
        assert_eq!((snap.serial, snap.width, snap.x, snap.y), (s2, 1, 5, 6));
    }
}
