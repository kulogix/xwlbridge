//! An icon window's pixels, as a StatusNotifierItem's `IconPixmap`.
//!
//! The icon is read back with `GetImage` (ZPixmap, in the icon's own format) and turned into the one format
//! every StatusNotifier host understands: `a(iiay)`, a width, a height and 4 bytes per pixel, ARGB in
//! network byte order.
//!
//! - A 24- or 32-bit icon is opaque: its alpha is 255 everywhere (a 32-bit icon's fourth byte is not an
//!   alpha channel in the X protocol, and the tray's background does not show through an opaque icon);
//! - an icon with an alpha channel (a 32-bit ARGB visual, which is how GTK's and Qt's tray icons are drawn:
//!   they clear themselves to transparent) keeps it;
//! - a 1-bit icon (a bitmap the app draws into) draws in the foreground colour on a transparent background:
//!   its zero bits are "nothing drawn", not black.
//!
//! Anything else (a format we did not expect) is "no icon" rather than a wrong one.

/// An icon as read from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u16,
    pub height: u16,
    /// The icon window's depth.
    pub depth: u8,
    /// The `GetImage` data: 32 bits per pixel (or 1 bit per pixel for a bitmap).
    pub data: Vec<u8>,
}

/// One StatusNotifier `IconPixmap` entry: its width, height and ARGB bytes.
pub type Pixmap = (i32, i32, Vec<u8>);

impl Image {
    /// The icon as `IconPixmap` wants it: `(width, height, argb)`, network byte order, or `None` for
    /// nothing to show. A 1-bit icon (edge to edge, as `a(iiay)` has no separate mask) is drawn in the
    /// foreground colour; the rest of that icon is transparent.
    pub fn argb(&self, foreground: [u8; 3]) -> Option<(i32, i32, Vec<u8>)> {
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        if w == 0 || h == 0 {
            return None;
        }
        let mut out = Vec::with_capacity(w * h * 4);
        match self.depth {
            1 => {
                let stride = w.div_ceil(32) * 4;
                if self.data.len() < stride * h {
                    return None;
                }
                for y in 0..h {
                    for x in 0..w {
                        let byte = self.data[y * stride + x / 8];
                        let set = byte & (0x80 >> (x % 8)) != 0;
                        let [r, g, b] = foreground;
                        out.extend_from_slice(&if set { [255, r, g, b] } else { [0; 4] });
                    }
                }
            }
            24 | 32 => {
                if self.data.len() < w * h * 4 {
                    return None;
                }
                for p in self.data.chunks_exact(4).take(w * h) {
                    let a = if self.depth == 32 { p[3] } else { 255 };
                    out.extend_from_slice(&[a, p[2], p[1], p[0]]);
                }
            }
            _ => return None,
        }
        Some((i32::from(self.width), i32::from(self.height), out))
    }
}

/// The `(iiay)` sub-message of an image, or an empty list (an icon that can't be read is no icon, not a
/// black square).
pub fn pixmaps(image: Option<&Image>, foreground: [u8; 3]) -> Vec<Pixmap> {
    image.and_then(|i| i.argb(foreground)).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_24_bit_icon_is_opaque() {
        let img = Image {
            width: 2,
            height: 1,
            depth: 24,
            data: vec![0x10, 0x20, 0x30, 0xff, 0x01, 0x02, 0x03, 0xff],
        };
        assert_eq!(
            img.argb([0; 3]).unwrap(),
            (2, 1, vec![255, 0x30, 0x20, 0x10, 255, 0x03, 0x02, 0x01])
        );
    }

    #[test]
    fn a_32_bit_icon_keeps_its_alpha() {
        let img = Image {
            width: 1,
            height: 1,
            depth: 32,
            data: vec![0x10, 0x20, 0x30, 0x80],
        };
        assert_eq!(img.argb([0; 3]).unwrap().2, vec![0x80, 0x30, 0x20, 0x10]);
    }

    #[test]
    fn a_bitmap_draws_in_the_foreground_on_nothing() {
        let img = Image {
            width: 4,
            height: 1,
            depth: 1,
            data: vec![0b1010_0000; 4],
        };
        let (w, h, px) = img.argb([1, 2, 3]).unwrap();
        assert_eq!((w, h), (4, 1));
        assert_eq!(&px[0..4], &[255, 1, 2, 3]);
        assert_eq!(&px[4..8], &[0, 0, 0, 0]);
        assert_eq!(&px[8..12], &[255, 1, 2, 3]);
    }

    #[test]
    fn rows_of_a_bitmap_are_padded() {
        let img = Image {
            width: 3,
            height: 2,
            depth: 1,
            data: vec![0b1000_0000, 0, 0, 0, 0b0100_0000, 0, 0, 0],
        };
        let (_, _, px) = img.argb([9, 9, 9]).unwrap();
        assert_eq!(&px[0..4], &[255, 9, 9, 9]);
        assert_eq!(&px[4..8], &[0; 4]);
        assert_eq!(&px[12..16], &[0; 4]);
        assert_eq!(&px[16..20], &[255, 9, 9, 9]);
    }

    #[test]
    fn the_wrong_size_is_no_icon() {
        let img = Image {
            width: 3,
            height: 2,
            depth: 24,
            data: vec![0; 20],
        };
        assert!(img.argb([0; 3]).is_none());
    }

    #[test]
    fn an_unknown_depth_is_no_icon() {
        let img = Image {
            width: 1,
            height: 1,
            depth: 16,
            data: vec![0; 4],
        };
        assert!(pixmaps(Some(&img), [0; 3]).is_empty());
        assert!(pixmaps(None, [0; 3]).is_empty());
    }
}
