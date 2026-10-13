//! The desktop's cursor sprite as the X server's cursor.
//!
//! The backend describes the compositor's sprite in
//! [`CursorState`](crate::bridge::cursor::CursorState) (on GNOME, from the
//! screencast's cursor metadata). The built-in engine answers XFixes from it
//! directly; a stock X server only knows the cursors its windows define. So the
//! sprite becomes an ARGB cursor (RENDER) defined on the window on top of
//! everything — the input shield, or the mirror without Composite — which the
//! pointer is always over, and on the root for any window briefly above it.
//! The X server then shows it, and XFixes clients (RealVNC's viewer cursor,
//! x11vnc) get the desktop's shape instead of Xorg's default "X".
//!
//! A hidden pointer, or a backend that has no sprite (the cursor drawn into the
//! frames: `-cursor none`, or mutter < 48), gets an invisible cursor, as the
//! built-in engine reports a transparent one, so no second cursor is drawn over
//! the desktop's. A cursor is made only when the sprite changes, and the one it
//! replaces is freed.

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::render::{self, PictType};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, Cursor, Gcontext, ImageFormat,
    ImageOrder, Rectangle, Window,
};
use x11rb::rust_connection::RustConnection;

use super::AnyError;
use crate::bridge::Server;

/// A sprite as given to the X server.
#[derive(PartialEq)]
enum Shape {
    Hidden,
    Sprite {
        width: u16,
        height: u16,
        xhot: u16,
        yhot: u16,
        image: Vec<u32>,
    },
}

/// RENDER's means to make ARGB cursors.
struct Argb {
    /// The a8r8g8b8 picture format.
    format: render::Pictformat,
    /// A GC for depth-32 pixmaps.
    gc: Gcontext,
}

pub struct CursorSync {
    root: Window,
    /// Where the cursor is defined: the window on top, and the root.
    windows: Vec<Window>,
    argb: Argb,
    big_endian: bool,
    /// A fully transparent cursor, for a hidden pointer; kept for good.
    invisible: Cursor,
    /// The ARGB cursor in use, if any.
    current: Option<Cursor>,
    /// The `CursorState` serial last looked at.
    serial: Option<u32>,
    /// What the X server shows now.
    applied: Option<Shape>,
}

impl CursorSync {
    /// Fails if the X server can't make ARGB cursors (RENDER < 0.5); the X
    /// server's own cursor then stays (Xvfb and Xvnc have RENDER 0.11).
    pub fn new(conn: &RustConnection, root: Window, top: Window) -> Result<CursorSync, AnyError> {
        let argb = Self::argb(conn, root)?.ok_or("the X server can't make ARGB cursors")?;
        // invisible: a 1x1 core cursor whose mask is clear
        let pix = conn.generate_id()?;
        conn.create_pixmap(1, pix, root, 1, 1)?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, pix, &CreateGCAux::new().foreground(0))?;
        conn.poly_fill_rectangle(pix, gc, &[Rectangle { x: 0, y: 0, width: 1, height: 1 }])?;
        let invisible = conn.generate_id()?;
        conn.create_cursor(invisible, pix, pix, 0, 0, 0, 0, 0, 0, 0, 0)?;
        conn.free_gc(gc)?;
        conn.free_pixmap(pix)?;
        let mut windows = vec![top];
        if root != top {
            windows.push(root);
        }
        Ok(CursorSync {
            root,
            windows,
            argb,
            big_endian: conn.setup().image_byte_order == ImageOrder::MSB_FIRST,
            invisible,
            current: None,
            serial: None,
            applied: None,
        })
    }

    /// RENDER ≥ 0.5 (ARGB cursors) and its a8r8g8b8 format, or `None`.
    fn argb(conn: &RustConnection, root: Window) -> Result<Option<Argb>, AnyError> {
        let v = render::query_version(conn, 0, 11)?.reply()?;
        if (v.major_version, v.minor_version) < (0, 5) {
            return Ok(None);
        }
        let formats = render::query_pict_formats(conn)?.reply()?;
        let Some(format) = formats.formats.iter().find(|f| {
            let d = &f.direct;
            f.type_ == PictType::DIRECT
                && f.depth == 32
                && (d.alpha_shift, d.alpha_mask) == (24, 0xff)
                && (d.red_shift, d.red_mask) == (16, 0xff)
                && (d.green_shift, d.green_mask) == (8, 0xff)
                && (d.blue_shift, d.blue_mask) == (0, 0xff)
        }) else {
            return Ok(None);
        };
        let pix = conn.generate_id()?;
        conn.create_pixmap(32, pix, root, 1, 1)?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, pix, &CreateGCAux::new())?;
        conn.free_pixmap(pix)?;
        Ok(Some(Argb { format: format.id, gc }))
    }

    /// Makes the desktop's current sprite the X server's cursor, if it changed
    /// since the last call. Cheap when it didn't (a serial comparison).
    pub fn sync(&mut self, conn: &RustConnection, server: &Server) -> Result<(), AnyError> {
        let serial = server.cursor.serial();
        if self.serial == Some(serial) {
            return Ok(());
        }
        self.serial = Some(serial);
        let snap = server.cursor.snapshot();
        let shape = if serial == 0 || is_hidden(&snap.image) {
            // no sprite yet, none at all (drawn into the frames), or hidden
            Shape::Hidden
        } else {
            Shape::Sprite {
                width: snap.width,
                height: snap.height,
                xhot: snap.xhot,
                yhot: snap.yhot,
                image: snap.image,
            }
        };
        // the same sprite again (the pointer went back to a monitor that sent
        // it before): nothing to do
        if self.applied.as_ref() == Some(&shape) {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let cursor = match &shape {
            Shape::Hidden => None,
            Shape::Sprite { width, height, xhot, yhot, image } => {
                match self.make(conn, *width, *height, *xhot, *yhot, image) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        crate::warning!("xvfb: can't make a {width}x{height} cursor: {e}");
                        return Ok(());
                    }
                }
            }
        };
        let aux = ChangeWindowAttributesAux::new().cursor(cursor.unwrap_or(self.invisible));
        for &w in &self.windows {
            conn.change_window_attributes(w, &aux)?;
        }
        // the windows hold their own reference
        if let Some(old) = std::mem::replace(&mut self.current, cursor) {
            conn.free_cursor(old)?;
        }
        conn.flush()?;
        let us = started.elapsed().as_micros();
        match &shape {
            Shape::Hidden => crate::vlog!("xvfb: cursor hidden ({us} µs)"),
            Shape::Sprite { width, height, xhot, yhot, .. } => {
                crate::vlog!("xvfb: cursor {width}x{height}, hotspot {xhot},{yhot} ({us} µs)")
            }
        }
        self.applied = Some(shape);
        Ok(())
    }

    /// An ARGB cursor of `image` (premultiplied 0xAARRGGBB).
    fn make(
        &self,
        conn: &RustConnection,
        width: u16,
        height: u16,
        xhot: u16,
        yhot: u16,
        image: &[u32],
    ) -> Result<Cursor, AnyError> {
        if width == 0 || height == 0 || image.len() < usize::from(width) * usize::from(height) {
            return Err("bad sprite size".into());
        }
        let argb = &self.argb;
        let pix = conn.generate_id()?;
        conn.create_pixmap(32, pix, self.root, width, height)?;
        // in rows, each request within the X server's limit
        let row = usize::from(width) * 4;
        let rows = ((conn.maximum_request_bytes().saturating_sub(64)) / row).clamp(1, usize::from(height));
        let data = argb_bytes(&image[..usize::from(width) * usize::from(height)], self.big_endian);
        for (i, chunk) in data.chunks(rows * row).enumerate() {
            conn.put_image(
                ImageFormat::Z_PIXMAP,
                pix,
                argb.gc,
                width,
                (chunk.len() / row) as u16,
                0,
                (i * rows) as i16,
                0,
                32,
                chunk,
            )?;
        }
        let pic = conn.generate_id()?;
        render::create_picture(conn, pic, pix, argb.format, &Default::default())?;
        let cursor = conn.generate_id()?;
        let (x, y) = clamp_hotspot(width, height, xhot, yhot);
        let made = render::create_cursor(conn, cursor, pic, x, y)?;
        render::free_picture(conn, pic)?;
        conn.free_pixmap(pix)?;
        // a round trip, but only when the sprite changes
        made.check()?;
        Ok(cursor)
    }
}

/// Whether nothing of `image` shows (no sprite, or the pointer hidden).
fn is_hidden(image: &[u32]) -> bool {
    image.iter().all(|&p| p >> 24 == 0)
}

/// The hotspot within the image, as RENDER requires.
fn clamp_hotspot(width: u16, height: u16, xhot: u16, yhot: u16) -> (u16, u16) {
    (xhot.min(width.saturating_sub(1)), yhot.min(height.saturating_sub(1)))
}

/// 32-bit Z-pixmap data in the X server's byte order.
fn argb_bytes(image: &[u32], big_endian: bool) -> Vec<u8> {
    image
        .iter()
        .flat_map(|&p| if big_endian { p.to_be_bytes() } else { p.to_le_bytes() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_means_nothing_shows() {
        assert!(is_hidden(&[0]));
        assert!(is_hidden(&[]));
        assert!(is_hidden(&[0x00ff_ffff, 0])); // colour without alpha shows nothing
        assert!(!is_hidden(&[0, 0x0100_0000]));
    }

    #[test]
    fn hotspot_stays_inside() {
        assert_eq!(clamp_hotspot(24, 24, 3, 1), (3, 1));
        assert_eq!(clamp_hotspot(24, 24, 30, 24), (23, 23));
        assert_eq!(clamp_hotspot(1, 1, 5, 5), (0, 0));
    }

    #[test]
    fn pixels_in_server_byte_order() {
        assert_eq!(argb_bytes(&[0x8040_2010], false), vec![0x10, 0x20, 0x40, 0x80]);
        assert_eq!(argb_bytes(&[0x8040_2010], true), vec![0x80, 0x40, 0x20, 0x10]);
    }
}
