//! What the X side shows while the desktop can't be captured: a dark screen
//! with a symbol in the middle of each monitor — a padlock while the screen is
//! locked and the compositor has closed remote access (GNOME does that unless
//! the lock-screen extension in `contrib/gnome-shell/` keeps it open), a pause
//! sign after the local user stopped screen sharing, plain black once the
//! session stopped because nobody watched (the next reader of a stock engine
//! is answered before capture is back: black is plainly not the desktop, a
//! frame from minutes ago would pass for it).
//!
//! Without it the X screen keeps the last captured frame: a locked desktop's
//! contents (an open document, a mail) would stay readable to every viewer that
//! connects until someone unlocks at the computer, and a stopped share would
//! look like a hung one.

use x11rb_protocol::protocol::xproto::Rectangle;

use crate::bridge::Server;

/// Background and padlock colours, as framebuffer bytes (B, G, R, x).
const BG: [u8; 4] = [0x24, 0x1e, 0x1e, 0];
const FG: [u8; 4] = [0xc8, 0xc8, 0xc8, 0];

/// Which picture to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The screen is locked.
    Locked,
    /// The local user stopped sharing the screen.
    Stopped,
    /// Nobody watches: capture stopped.
    Idle,
}

/// Paints the picture over the whole X screen and reports it as damage, so
/// every engine (and every watching client) shows it.
pub fn paint(server: &Server, kind: Kind) {
    let rects = server.screen.lock().unwrap().layout_rects();
    server.framebuffer.paint(|w, h, data| {
        if kind == Kind::Idle {
            data.fill(0);
            return;
        }
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&BG);
        }
        let mut canvas = Canvas {
            w: w as i32,
            h: h as i32,
            data,
        };
        let mut centres: Vec<(i32, i32, i32)> = rects
            .iter()
            .map(|r| {
                (
                    r.px + r.pw / 2,
                    r.py + r.ph / 2,
                    (r.pw.min(r.ph) / 6).max(24),
                )
            })
            .collect();
        if centres.is_empty() {
            centres.push((w as i32 / 2, h as i32 / 2, (w.min(h) as i32 / 6).max(24)));
        }
        for (cx, cy, s) in centres {
            match kind {
                Kind::Locked => canvas.padlock(cx, cy, s),
                Kind::Stopped => canvas.pause(cx, cy, s),
                Kind::Idle => {}
            }
        }
    });
    let geom = server.geometry();
    server.damage.add_damage(
        &[Rectangle {
            x: 0,
            y: 0,
            width: geom.width,
            height: geom.height,
        }],
        (geom.width, geom.height),
    );
}

struct Canvas<'a> {
    w: i32,
    h: i32,
    data: &'a mut [u8],
}

impl Canvas<'_> {
    fn put(&mut self, x: i32, y: i32, c: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = (y as usize * self.w as usize + x as usize) * 4;
        if let Some(px) = self.data.get_mut(i..i + 4) {
            px.copy_from_slice(&c);
        }
    }

    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]) {
        for y in y0.max(0)..y1.min(self.h) {
            for x in x0.max(0)..x1.min(self.w) {
                self.put(x, y, c);
            }
        }
    }

    /// A pause sign (two bars) `s` pixels tall centred on (`cx`, `cy`).
    fn pause(&mut self, cx: i32, cy: i32, s: i32) {
        let (bw, gap, h) = (s / 4, s / 6, s * 3 / 4);
        self.rect(cx - gap / 2 - bw, cy - h / 2, cx - gap / 2, cy + h / 2, FG);
        self.rect(cx + gap / 2, cy - h / 2, cx + gap / 2 + bw, cy + h / 2, FG);
    }

    /// A padlock `s` pixels tall centred on (`cx`, `cy`): a shackle (the upper
    /// half of a ring, with legs) over a body with a keyhole.
    fn padlock(&mut self, cx: i32, cy: i32, s: i32) {
        let body_w = s * 3 / 4;
        let body_h = s * 9 / 16;
        let body_top = cy - s / 2 + s * 7 / 16;
        let ro = body_w * 3 / 8; // shackle outer radius
        let ri = ro - (s / 10).max(3); // inner radius
        let ring_cy = body_top - s * 3 / 16;
        // shackle: upper half ring, then straight legs down into the body
        for y in ring_cy - ro..=ring_cy {
            for x in cx - ro..=cx + ro {
                let d2 = (x - cx) * (x - cx) + (y - ring_cy) * (y - ring_cy);
                if d2 <= ro * ro && d2 >= ri * ri {
                    self.put(x, y, FG);
                }
            }
        }
        self.rect(cx - ro, ring_cy, cx - ri, body_top, FG);
        self.rect(cx + ri + 1, ring_cy, cx + ro + 1, body_top, FG);
        // body
        self.rect(
            cx - body_w / 2,
            body_top,
            cx + body_w / 2,
            body_top + body_h,
            FG,
        );
        // keyhole: a disc and a slot below it
        let kr = (s / 16).max(2);
        let ky = body_top + body_h * 2 / 5;
        for y in ky - kr..=ky + kr {
            for x in cx - kr..=cx + kr {
                if (x - cx) * (x - cx) + (y - ky) * (y - ky) <= kr * kr {
                    self.put(x, y, BG);
                }
            }
        }
        self.rect(cx - kr / 2, ky, cx + kr / 2 + 1, ky + kr * 3, BG);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padlock_stays_inside_and_draws() {
        let (w, h) = (64, 48);
        let mut data = vec![0u8; w * h * 4];
        let mut c = Canvas {
            w: w as i32,
            h: h as i32,
            data: &mut data,
        };
        // partly off-screen on purpose: must clip, not panic
        c.padlock(60, 4, 40);
        c.padlock(32, 24, 30);
        c.pause(-5, 40, 40);
        let fg = data.chunks_exact(4).filter(|p| *p == FG).count();
        assert!(fg > 100, "padlock pixels drawn: {fg}");
    }
}
