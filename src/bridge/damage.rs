//! DAMAGE extension: capture damage goes in as regions, DamageNotify comes out.
//!
//! Per-level semantics match the X.org server (`DamageReportDamage` in
//! miext/damage/damage.c, `DamageExtReport`/`DamageExtNotify` in
//! damageext/damageext.c):
//!
//! - RawRectangles: report every incoming rectangle.
//! - DeltaRectangles: report the newly-damaged part; accumulate into the region.
//! - BoundingBox: report the region extents, only when they grow.
//! - NonEmpty: report once (no area) when the region goes empty -> non-empty.
//!
//! One event per box, with `DamageNotifyMore` (0x80) on all but the last.
//! DamageSubtract clears the region and re-arms reporting.

use std::sync::{Arc, Mutex};

use x11rb_protocol::protocol::damage;
use x11rb_protocol::protocol::xproto::Rectangle;

use crate::bridge::event::{self, Client};
use crate::bridge::x11::ext::EXTENSIONS;
use crate::util::bbox;

const RAW: u8 = 0;
const DELTA: u8 = 1;
const BBOX: u8 = 2;
const NON_EMPTY: u8 = 3;
const MORE: u8 = 0x80;

/// One client's `XDamage` handle on the root drawable.
struct DamageObj {
    client: Arc<Client>,
    id: u32,
    drawable: u32,
    level: u8,
    region: Vec<Rectangle>, // accumulated, not yet subtracted
}

#[derive(Default)]
pub struct DamageSink {
    objects: Mutex<Vec<DamageObj>>,
    /// Watchers outside our own X server (the Xvfb engine's agent, on behalf
    /// of the clients of its Xvfb): they count for [`active`](Self::active).
    external: std::sync::atomic::AtomicBool,
    /// A consumer outside our own X server (the Xvfb engine's agent) is
    /// registered: it gets every damage rect through
    /// [`take_external`](Self::take_external), whatever made capture run (a
    /// watcher, a client that connected or left a moment ago, `-alwayson`),
    /// so its root never lags the desktop while a session runs.
    external_sink: std::sync::atomic::AtomicBool,
    external_rects: Mutex<Vec<Rectangle>>,
    external_wake: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl DamageSink {
    /// DamageCreate. As in the X.org server (`ProcDamageCreate` damages a
    /// window's whole `borderClip` right away), the new object starts with the
    /// entire drawable damaged and reports it at its level: clients such as
    /// x11vnc fetch the screen when that first report comes and otherwise
    /// trust damage alone, so without it a screen that changed before they
    /// watched stays stale for them until something else changes.
    pub fn create(&self, client: Arc<Client>, id: u32, drawable: u32, level: u8, geom: (u16, u16)) {
        let mut objs = self.objects.lock().unwrap();
        let was_active = objs.iter().any(|o| !o.client.is_dead());
        objs.push(DamageObj {
            client,
            id,
            drawable,
            level,
            region: Vec::new(),
        });
        if geom.0 > 0 && geom.1 > 0 {
            let whole = Rectangle {
                x: 0,
                y: 0,
                width: geom.0,
                height: geom.1,
            };
            let o = objs.last_mut().expect("just pushed");
            report(o, &[whole], geom);
        }
        if !was_active {
            crate::vlog!("damage tracking started");
        }
    }

    /// Whether any client is watching the screen. Gates the capture loop.
    pub fn active(&self) -> bool {
        self.external.load(std::sync::atomic::Ordering::Relaxed)
            || self
                .objects
                .lock()
                .unwrap()
                .iter()
                .any(|o| !o.client.is_dead())
    }

    pub fn destroy(&self, client: &Arc<Client>, id: u32) {
        let mut objs = self.objects.lock().unwrap();
        objs.retain(|o| !(Arc::ptr_eq(&o.client, client) && o.id == id));
        let still_active = objs.iter().any(|o| !o.client.is_dead());
        if !still_active {
            crate::vlog!("damage tracking stopped");
        }
    }

    /// DamageSubtract: clears the damage and returns it for the `parts` region.
    pub fn subtract(
        &self,
        client: &Arc<Client>,
        id: u32,
        repair: Option<&[Rectangle]>,
    ) -> Vec<Rectangle> {
        let mut objs = self.objects.lock().unwrap();
        let Some(o) = objs
            .iter_mut()
            .find(|o| Arc::ptr_eq(&o.client, client) && o.id == id)
        else {
            return Vec::new();
        };
        if o.level == RAW {
            return Vec::new(); // no-op at the raw level
        }
        // only repair == None is exact, which is the path VNC takes
        // (XDamageSubtract(d, None, parts)); anything else is treated as a full
        // subtract, which over-clears but never loses real damage
        let _ = repair;
        std::mem::take(&mut o.region)
    }

    /// Marks an external watcher present or gone; see the `external` field.
    #[allow(dead_code)] // only the xvfb engine watches from outside
    pub fn set_external(&self, watching: bool, wake: Option<Box<dyn Fn() + Send>>) {
        self.external
            .store(watching, std::sync::atomic::Ordering::Relaxed);
        if wake.is_some() {
            *self
                .external_wake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = wake;
            self.external_sink
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The external watcher is gone for good (the agent stopped): it no longer
    /// counts as watching, its wake is dropped (no later damage calls it) and
    /// its pending damage is discarded. Once this returns, no call of the old
    /// wake is in progress.
    #[allow(dead_code)]
    pub fn clear_external(&self) {
        self.external
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.external_sink
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let wake = self
            .external_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.external_rects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        drop(wake);
    }

    /// The damage accumulated for the external watcher since the last call.
    #[allow(dead_code)]
    pub fn take_external(&self) -> Vec<Rectangle> {
        std::mem::take(&mut *self.external_rects.lock().unwrap())
    }

    /// Feeds a new screen-damage region, in root coordinates, to every object.
    pub fn add_damage(&self, new: &[Rectangle], geom: (u16, u16)) {
        let new: Vec<Rectangle> = new
            .iter()
            .copied()
            .filter(|r| r.width > 0 && r.height > 0)
            .collect();
        if new.is_empty() {
            return;
        }
        if self
            .external_sink
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let mut ext = self.external_rects.lock().unwrap();
            ext.extend_from_slice(&new);
            if ext.len() > 256 {
                // too fragmented to be worth tracking: the whole screen
                ext.clear();
                ext.push(Rectangle {
                    x: 0,
                    y: 0,
                    width: geom.0,
                    height: geom.1,
                });
            }
            drop(ext);
            if let Some(wake) = self
                .external_wake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                wake();
            }
        }
        let mut objs = self.objects.lock().unwrap();
        objs.retain(|o| !o.client.is_dead());
        for o in objs.iter_mut() {
            report(o, &new, geom);
        }
    }
}

/// Adds `new` (root coordinates) to one object's damage and reports it as the
/// object's level asks.
fn report(o: &mut DamageObj, new: &[Rectangle], geom: (u16, u16)) {
    match o.level {
        RAW => {
            // nothing accumulates: subtract is a no-op at this level, so a
            // region here would only ever grow (x11vnc uses RAW)
            notify(o, new, geom);
        }
        DELTA => {
            // exact delta is `new - region`, but reporting all of `new`
            // over-reports safely and needs no region algebra
            o.region.extend_from_slice(new);
            notify(o, new, geom);
        }
        BBOX => {
            let old = bbox(&o.region).map(tuple);
            o.region.extend_from_slice(new);
            let now = bbox(&o.region);
            if old != now.map(tuple)
                && let Some(now) = now
            {
                notify(o, &[now], geom);
            }
        }
        NON_EMPTY => {
            let was_empty = o.region.is_empty();
            o.region.extend_from_slice(new);
            if was_empty && !o.region.is_empty() {
                notify_non_empty(o, geom);
            }
        }
        _ => o.region.extend_from_slice(new),
    }
}

/// One DamageNotify per box, with `DamageNotifyMore` on all but the last.
fn notify(o: &DamageObj, boxes: &[Rectangle], geom: (u16, u16)) {
    let geometry = Rectangle {
        x: 0,
        y: 0,
        width: geom.0,
        height: geom.1,
    };
    let n = boxes.len();
    for (i, area) in boxes.iter().enumerate() {
        let level = if i + 1 < n { o.level | MORE } else { o.level };
        send_notify(o, level, *area, geometry);
    }
}

/// A NonEmpty report: one event whose area is the whole drawable.
fn notify_non_empty(o: &DamageObj, geom: (u16, u16)) {
    let geometry = Rectangle {
        x: 0,
        y: 0,
        width: geom.0,
        height: geom.1,
    };
    send_notify(o, o.level, geometry, geometry);
}

fn send_notify(o: &DamageObj, level: u8, area: Rectangle, geometry: Rectangle) {
    crate::bridge::profile::damage_notify(1);
    let first_event = EXTENSIONS.lookup(b"DAMAGE").map_or(0, |e| e.first_event);
    let event = damage::NotifyEvent {
        response_type: first_event, // + XDamageNotify (0)
        level: damage::ReportLevel::from(level),
        sequence: o.client.seq(),
        drawable: o.drawable,
        damage: o.id,
        timestamp: crate::bridge::event::server_time_ms(),
        area,
        geometry,
    };
    event::send(&o.client, &event);
}

/// `Rectangle` is not `PartialEq`, so compare bounding boxes as tuples.
fn tuple(r: Rectangle) -> (i16, i16, u16, u16) {
    (r.x, r.y, r.width, r.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_cleared_external_watcher_is_neither_active_nor_woken() {
        let sink = DamageSink::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        sink.set_external(
            false,
            Some(Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            })),
        );
        let r = [Rectangle {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        }];
        assert!(!sink.active());
        sink.add_damage(&r, (64, 64));
        // not watching, but capture runs (something else started it): the
        // agent still gets the damage, so its root stays current
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a registered agent gets damage while not watching"
        );
        assert_eq!(sink.take_external().len(), 1);
        sink.set_external(true, None);
        assert!(sink.active());
        sink.add_damage(&r, (64, 64));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        sink.clear_external();
        assert!(
            !sink.active(),
            "a stopped agent no longer counts as watching"
        );
        assert!(sink.take_external().is_empty());
        sink.add_damage(&r, (64, 64));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "its wake is never called again"
        );
        assert_eq!(Arc::strong_count(&calls), 1, "and was dropped");
    }

    // --- a new object starts with the whole drawable damaged (xwlrvnc 3ef1511)

    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn client() -> (Arc<Client>, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        (Client::new(a).unwrap(), b)
    }

    /// The DamageNotify events waiting on the client's socket: (level, damage
    /// id, area x, y, w, h).
    fn events(b: &mut UnixStream) -> Vec<(u8, u32, i16, i16, u16, u16)> {
        let mut out = Vec::new();
        let mut buf = [0u8; 32];
        while b.read_exact(&mut buf).is_ok() {
            let first = EXTENSIONS.lookup(b"DAMAGE").unwrap().first_event;
            assert_eq!(buf[0], first);
            let le16 = |o: usize| u16::from_le_bytes([buf[o], buf[o + 1]]);
            out.push((
                buf[1],
                u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
                le16(16) as i16,
                le16(18) as i16,
                le16(20),
                le16(22),
            ));
        }
        out
    }

    #[test]
    fn create_reports_the_whole_drawable_at_every_level() {
        for level in [RAW, DELTA, BBOX, NON_EMPTY] {
            let sink = DamageSink::default();
            let (c, mut b) = client();
            sink.create(c, 0x2c, 0x1e, level, (1280, 720));
            let ev = events(&mut b);
            assert_eq!(ev, vec![(level, 0x2c, 0, 0, 1280, 720)], "level {level}");
        }
    }

    #[test]
    fn after_the_initial_report_damage_follows_the_level() {
        let sink = DamageSink::default();
        let (raw, mut braw) = client();
        let (ne, mut bne) = client();
        sink.create(raw.clone(), 1, 0x1e, RAW, (800, 600));
        sink.create(ne.clone(), 2, 0x1e, NON_EMPTY, (800, 600));
        assert_eq!(events(&mut braw).len(), 1);
        assert_eq!(events(&mut bne).len(), 1);
        let r = Rectangle {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        sink.add_damage(&[r], (800, 600));
        // RAW reports every rectangle; NonEmpty stays quiet until subtracted
        assert_eq!(events(&mut braw), vec![(RAW, 1, 10, 20, 30, 40)]);
        assert!(events(&mut bne).is_empty());
        sink.subtract(&ne, 2, None);
        sink.add_damage(&[r], (800, 600));
        assert_eq!(events(&mut bne), vec![(NON_EMPTY, 2, 0, 0, 800, 600)]);
    }

    #[test]
    fn a_new_object_reports_the_whole_drawable_even_when_the_screen_is_static() {
        // a second viewer (a VNC server starting late) makes its object while
        // nothing changes: it must still be told to fetch the screen
        let sink = DamageSink::default();
        let (a, mut ba) = client();
        let (b, mut bb) = client();
        sink.create(a, 1, 0x1e, RAW, (640, 480));
        assert_eq!(events(&mut ba), vec![(RAW, 1, 0, 0, 640, 480)]);
        sink.create(b, 2, 0x1e, RAW, (640, 480));
        assert_eq!(events(&mut bb), vec![(RAW, 2, 0, 0, 640, 480)]);
    }
}
