//! PipeWire capture of mutter's monitor streams into the shared framebuffer.
//!
//! mutter is asked for BGRx/BGRA *without* a DRM modifier, which makes it hand
//! out memfd buffers (already CPU-readable) whose byte order is exactly our
//! framebuffer's, so a frame is a plain row copy. Frames only arrive when the
//! screen changes, each carrying the damaged rectangles, so only those are
//! copied and only those are reported to XDamage. The cursor comes as metadata
//! (its own bitmap plus a position) and goes to XFixes rather than into the
//! pixels.
//!
//! Everything PipeWire is `!Send` and runs on the backend thread, whose poll
//! loop iterates the PipeWire loop.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;

use pipewire as pw;
use pw::spa;
use pw::spa::buffer::meta::{MetaCursor, MetaHeader, MetaHeaderFlags, MetaVideoDamage};
use pw::spa::param::video::VideoFormat;
use pw::spa::pod::Pod;
use x11rb_protocol::protocol::xproto::Rectangle;

use crate::bridge::Server;
use crate::bridge::capture::XRGB;

/// The PipeWire main loop and core connection, driven from the backend's poll.
pub struct Pw {
    mainloop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    core: pw::core::CoreRc,
}

impl Pw {
    pub fn new() -> Result<Pw, pw::Error> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        // mutter's streams live on the user's session PipeWire
        let core = context.connect_rc(None)?;
        // Keep the loop entered by this (the backend's) thread for its whole
        // life. Before PipeWire 1.2 a non-RT stream's process callback reaches
        // the main loop through pw_loop_invoke from PipeWire's data thread, and
        // invoke runs the callback right there, on the data thread, whenever
        // the loop is not entered (libspa loop.c: `impl->thread == 0`) — all
        // the time between iterate()s — racing this thread on !Send state
        // (seen on Ubuntu 24.04, PipeWire 1.0.5, as RefCell panics). Entered,
        // the call is queued and runs in our iterate().
        unsafe { mainloop.loop_().enter() };
        Ok(Pw {
            mainloop,
            _context: context,
            core,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.mainloop.loop_().fd().as_raw_fd()
    }

    /// Runs whatever is ready, without blocking.
    pub fn iterate(&self) {
        self.mainloop.loop_().iterate(pw::loop_::Timeout::None);
    }
}

impl Drop for Pw {
    fn drop(&mut self) {
        // pairs the enter() in new(); Pw is !Send, so this is the same thread
        unsafe { self.mainloop.loop_().leave() };
    }
}

/// Which stream currently has the pointer, and the last sprite each one sent.
/// mutter only resends a sprite when it changes, and only the stream the
/// pointer is over reports a cursor, so moving between monitors re-applies the
/// sprite that monitor sent last.
#[derive(Default)]
pub struct CursorTrack {
    owner: Option<u32>,
    sprites: HashMap<u32, Sprite>,
    applied: Option<(u32, u64)>, // (stream key, sprite generation) last given to X
    generation: u64,
}

#[derive(Clone)]
struct Sprite {
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    image: Vec<u32>,
    generation: u64,
}

/// Per-stream state the PipeWire callbacks share with the backend.
pub struct StreamState {
    server: Arc<Server>,
    key: u32,
    cursor: Rc<RefCell<CursorTrack>>,
    pub width: u32,
    pub height: u32,
    /// The next frame must be copied whole: the first one, or after a format
    /// change, since our framebuffer region holds nothing valid yet.
    need_full: bool,
    was_streaming: bool,
    /// The stream died under us (monitor reconfigured, mutter gone).
    pub lost: bool,
    /// The negotiated size, when it differs from what the layout said.
    pub size_mismatch: Option<(u32, u32)>,
    pub frames: u64,
}

/// One monitor's capture stream.
pub struct Capture {
    pub key: u32,
    /// Our hook on `stream`'s listener list. It must be removed while the
    /// stream is alive: `pw_stream_destroy` frees the list head the hook still
    /// points at, and the hook's removal then writes into freed memory. `Drop`
    /// takes it out first; it is also declared first, so the fields' own drop
    /// order agrees.
    listener: Option<pw::stream::StreamListener<Rc<RefCell<StreamState>>>>,
    /// The only reference to the stream: dropping it destroys the stream.
    stream: pw::stream::StreamRc,
    pub state: Rc<RefCell<StreamState>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // disconnect with the listener still registered (its Paused/
        // Unconnected callbacks only update our state), then unhook, then let
        // the stream go
        let _ = self.stream.disconnect();
        drop(self.listener.take());
    }
}

fn pod_bytes(obj: spa::pod::Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serialize pod")
    .0
    .into_inner()
}

fn prop(key: u32, value: spa::pod::Value) -> spa::pod::Property {
    spa::pod::Property {
        key,
        flags: spa::pod::PropertyFlags::empty(),
        value,
    }
}

fn int_range(default: i32, min: i32, max: i32) -> spa::pod::Value {
    spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
        spa::utils::ChoiceFlags::empty(),
        spa::utils::ChoiceEnum::Range { default, min, max },
    )))
}

/// What we accept: raw BGRx or BGRA (mutter's two SDR formats, both our byte
/// order) and any size (a monitor stream's is fixed by mutter anyway).
///
/// With `fps`, also a maximum frame rate, which mutter enforces by skipping
/// frames that come too soon and redrawing later to deliver their damage.
/// Without it mutter caps at the monitor's refresh rate and never skips:
/// measured on mutter 50.1, a 30 fps cap raised click-to-pixel latency from 41
/// to 55 ms (p50) and once left a change undelivered for over 3 s, while frames
/// still only come when something changed, so an idle screen costs nothing.
fn enum_format(width: u32, height: u32, fps: Option<u32>) -> Vec<u8> {
    use spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    let mut obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(F::MediaType, Id, MediaType::Video),
        spa::pod::property!(F::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            F::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        spa::pod::property!(
            F::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: width.max(1),
                height: height.max(1)
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        spa::pod::property!(
            F::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    if let Some(fps) = fps {
        let fps = fps.max(1);
        obj.properties.push(spa::pod::property!(
            F::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: fps, denom: 1 },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction { num: fps, denom: 1 }
        ));
    }
    pod_bytes(obj)
}

fn meta_param(kind: u32, size: spa::pod::Value) -> Vec<u8> {
    pod_bytes(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            prop(
                spa::sys::SPA_PARAM_META_type,
                spa::pod::Value::Id(spa::utils::Id(kind)),
            ),
            prop(spa::sys::SPA_PARAM_META_size, size),
        ],
    })
}

/// Sent once the format is fixed: memfd (or plain memory) buffers, and the
/// header, cursor and damage metadata.
fn buffer_and_meta_params() -> Vec<Vec<u8>> {
    let cursor_size = |w: usize, h: usize| {
        (std::mem::size_of::<spa::sys::spa_meta_cursor>()
            + std::mem::size_of::<spa::sys::spa_meta_bitmap>()
            + w * h * 4) as i32
    };
    let region = std::mem::size_of::<spa::sys::spa_meta_region>() as i32;
    vec![
        pod_bytes(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: spa::param::ParamType::Buffers.as_raw(),
            properties: vec![
                // a few buffers is plenty for a copying consumer, and capping the
                // max (not just the default) is what stops mutter allocating 8:
                // half the memfd memory charged to gnome-shell per stream
                prop(spa::sys::SPA_PARAM_BUFFERS_buffers, int_range(4, 2, 4)),
                prop(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    spa::pod::Value::Int(
                        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr),
                    ),
                ),
            ],
        }),
        meta_param(
            spa::sys::SPA_META_Header,
            spa::pod::Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
        ),
        // mutter offers exactly CURSOR_META_SIZE(384,384); the range must contain it
        meta_param(
            spa::sys::SPA_META_Cursor,
            int_range(cursor_size(384, 384), cursor_size(1, 1), cursor_size(384, 384)),
        ),
        // up to 32 damage rects per frame (more collapse into their bounding box)
        meta_param(
            spa::sys::SPA_META_VideoDamage,
            int_range(region * 32, region, region * 32),
        ),
    ]
}

impl Capture {
    /// Connects to mutter's PipeWire node for one monitor stream.
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        pw: &Pw,
        server: Arc<Server>,
        cursor: Rc<RefCell<CursorTrack>>,
        key: u32,
        node: u32,
        width: u32,
        height: u32,
        fps: Option<u32>,
    ) -> Result<Capture, pw::Error> {
        let stream = pw::stream::StreamRc::new(
            pw.core.clone(),
            "xwlbridge",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )?;
        let state = Rc::new(RefCell::new(StreamState {
            server,
            key,
            cursor,
            width,
            height,
            need_full: true,
            was_streaming: false,
            lost: false,
            size_mismatch: None,
            frames: 0,
        }));
        let listener = stream
            .add_local_listener_with_user_data(state.clone())
            .state_changed(|_, st, old, new| {
                let mut st = st.borrow_mut();
                crate::vlog!("gnome: stream {} {old:?} -> {new:?}", st.key);
                match new {
                    pw::stream::StreamState::Streaming => {
                        st.was_streaming = true;
                        // mutter repaints everything on (re)start; take it whole
                        st.need_full = true;
                    }
                    pw::stream::StreamState::Error(e) => {
                        crate::warning!("gnome: capture stream {} failed: {e}", st.key);
                        st.lost = true;
                    }
                    // a monitor stream that stops streaming was closed by mutter
                    // (its monitor changed); only a new RecordMonitor brings it back
                    pw::stream::StreamState::Paused | pw::stream::StreamState::Unconnected
                        if st.was_streaming =>
                    {
                        st.lost = true;
                    }
                    _ => {}
                }
            })
            .param_changed(|stream, st, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = spa::param::video::VideoInfoRaw::default();
                if info.parse(param).is_err() {
                    return;
                }
                let mut st = st.borrow_mut();
                let (w, h) = (info.size().width, info.size().height);
                if !matches!(info.format(), VideoFormat::BGRx | VideoFormat::BGRA) {
                    crate::warning!(
                        "gnome: stream {} negotiated unsupported format {:?}",
                        st.key,
                        info.format()
                    );
                }
                crate::vlog!(
                    "gnome: stream {} format {:?} {w}x{h} max {}/{} fps",
                    st.key,
                    info.format(),
                    info.max_framerate().num,
                    info.max_framerate().denom
                );
                if (w, h) != (st.width, st.height) {
                    st.size_mismatch = Some((w, h));
                }
                st.width = w;
                st.height = h;
                st.need_full = true;
                let bytes = buffer_and_meta_params();
                let mut pods: Vec<&Pod> =
                    bytes.iter().filter_map(|b| Pod::from_bytes(b)).collect();
                if let Err(e) = stream.update_params(&mut pods) {
                    crate::warning!("gnome: stream {} buffer params: {e}", st.key);
                }
            })
            .process(|stream, st| {
                let mut st = st.borrow_mut();
                while let Some(mut buf) = stream.dequeue_buffer() {
                    let corrupted = buf
                        .find_meta::<MetaHeader>()
                        .is_some_and(|h| h.flags().contains(MetaHeaderFlags::CORRUPTED));
                    if let Some(cur) = buf.find_meta::<MetaCursor>() {
                        st.cursor_meta(cur);
                    }
                    let damage: Vec<(i32, i32, u32, u32)> = buf
                        .find_meta::<MetaVideoDamage>()
                        .map(|d| {
                            d.iter()
                                .map(|r| {
                                    (r.position().x, r.position().y, r.size().width, r.size().height)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let datas = buf.datas_mut();
                    let Some(d) = datas.first_mut() else { continue };
                    let chunk = d.chunk();
                    let (size, offset, stride) = (chunk.size(), chunk.offset(), chunk.stride());
                    // size 0: a cursor-only (or failed) buffer, whose damage meta
                    // is stale; the pixels we already have stay valid
                    if corrupted || size == 0 || stride <= 0 {
                        continue;
                    }
                    let Some(src) = d.data() else { continue };
                    let Some(src) = src.get(offset as usize..) else {
                        continue;
                    };
                    st.frame(src, stride as u32, &damage);
                }
            })
            .register()?;
        let fmt = enum_format(width, height, fps);
        let mut params = [Pod::from_bytes(&fmt).expect("valid pod")];
        stream.connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;
        Ok(Capture {
            key,
            listener: Some(listener),
            stream,
            state,
        })
    }
}

impl StreamState {
    /// Whether the stream has reached the streaming state since it connected.
    pub fn streaming(&self) -> bool {
        self.was_streaming
    }

    /// Copies a frame's damaged rectangles (all of it when needed) into the
    /// framebuffer at this monitor's place, and reports them as X damage.
    fn frame(&mut self, src: &[u8], stride: u32, damage: &[(i32, i32, u32, u32)]) {
        let (w, h) = (self.width, self.height);
        let Some((ox, oy)) = self.server.screen.lock().unwrap().physical_pos(self.key) else {
            return;
        };
        let diffed = !self.need_full && damage.is_empty();
        let rects: Vec<(u32, u32, u32, u32)> = if diffed {
            // no damage metadata (an area stream): the diff finds it, and
            // has copied what changed
            self.server.framebuffer.blit_tiles(ox, oy, w, h, stride, src)
        } else if self.need_full {
            vec![(0, 0, w, h)]
        } else {
            damage
                .iter()
                .filter_map(|&(x, y, rw, rh)| {
                    let x0 = x.clamp(0, w as i32) as u32;
                    let y0 = y.clamp(0, h as i32) as u32;
                    let x1 = (i64::from(x) + i64::from(rw)).clamp(0, i64::from(w)) as u32;
                    let y1 = (i64::from(y) + i64::from(rh)).clamp(0, i64::from(h)) as u32;
                    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
                })
                .collect()
        };
        if rects.is_empty() {
            return;
        }
        self.need_full = false;
        self.frames += 1;
        if !diffed {
            let (wait_ns, work_ns) = self
                .server
                .framebuffer
                .blit_rects(ox, oy, w, h, stride, src, &rects, XRGB);
            crate::bridge::profile::frame(wait_ns, work_ns, 0);
        }
        let root: Vec<Rectangle> = rects
            .iter()
            .map(|&(x, y, rw, rh)| Rectangle {
                x: (ox + x as i32).clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                y: (oy + y as i32).clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                width: rw.min(u16::MAX.into()) as u16,
                height: rh.min(u16::MAX.into()) as u16,
            })
            .collect();
        let geom = self.server.geometry();
        self.server
            .damage
            .add_damage(&root, (geom.width, geom.height));
    }

    /// Feeds cursor metadata to XFixes: the position always, the sprite when it
    /// changed or the pointer moved onto this monitor.
    fn cursor_meta(&mut self, cur: &MetaCursor) {
        let mut track = self.cursor.borrow_mut();
        if !cur.is_valid() {
            // not on this monitor (or hidden); hide it if it was ours
            if track.owner == Some(self.key) {
                track.owner = None;
                track.applied = None;
                let serial = self.server.cursor.update_image(1, 1, 0, 0, vec![0]);
                self.server.events.cursor_changed(serial);
            }
            return;
        }
        track.owner = Some(self.key);
        if let Some((ox, oy)) = self.server.screen.lock().unwrap().physical_pos(self.key) {
            let p = cur.position();
            self.server
                .cursor
                .update_position(clamp_i16(ox + p.x), clamp_i16(oy + p.y));
        }
        if let Some(bm) = cur.bitmap() {
            let size = bm.size();
            let hot = cur.hotspot();
            let sprite = if size.width == 0 || size.height == 0 {
                // an empty sprite: the pointer is hidden (e.g. while typing)
                Some((1, 1, 0, 0, vec![0u32]))
            } else {
                bm.bitmap_data().map(|data| {
                    let bgra = bm.format() == VideoFormat::BGRA;
                    (
                        size.width.min(u16::MAX.into()) as u16,
                        size.height.min(u16::MAX.into()) as u16,
                        hot.x.clamp(0, u16::MAX.into()) as u16,
                        hot.y.clamp(0, u16::MAX.into()) as u16,
                        to_argb(data, size.width, size.height, bm.stride(), bgra),
                    )
                })
            };
            if let Some((width, height, xhot, yhot, image)) = sprite {
                track.generation += 1;
                let generation = track.generation;
                track.sprites.insert(
                    self.key,
                    Sprite {
                        width,
                        height,
                        xhot,
                        yhot,
                        image,
                        generation,
                    },
                );
            }
        }
        let Some(sprite) = track.sprites.get(&self.key).cloned() else {
            return;
        };
        if track.applied != Some((self.key, sprite.generation)) {
            track.applied = Some((self.key, sprite.generation));
            let serial = self.server.cursor.update_image(
                sprite.width,
                sprite.height,
                sprite.xhot,
                sprite.yhot,
                sprite.image,
            );
            self.server.events.cursor_changed(serial);
        }
    }
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

/// mutter's premultiplied RGBA (or BGRA) sprite to XFixes' premultiplied ARGB
/// (`0xAARRGGBB`): a byte shuffle, no alpha math, since both are premultiplied.
fn to_argb(data: &[u8], width: u32, height: u32, stride: i32, bgra: bool) -> Vec<u32> {
    let stride = stride.unsigned_abs() as usize;
    let mut out = Vec::with_capacity((width * height) as usize);
    for row in 0..height as usize {
        for col in 0..width as usize {
            let i = row * stride + col * 4;
            let Some(px) = data.get(i..i + 4) else {
                out.push(0);
                continue;
            };
            let (r, g, b, a) = if bgra {
                (px[2], px[1], px[0], px[3])
            } else {
                (px[0], px[1], px[2], px[3])
            };
            out.push(u32::from(a) << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::to_argb;

    #[test]
    fn rgba_to_argb() {
        // one premultiplied half-transparent red pixel, then opaque blue
        let px = [0x80, 0, 0, 0x80, 0, 0, 0xff, 0xff];
        assert_eq!(to_argb(&px, 2, 1, 8, false), vec![0x8080_0000, 0xff00_00ff]);
        let bgra = [0, 0, 0x80, 0x80, 0xff, 0, 0, 0xff];
        assert_eq!(to_argb(&bgra, 2, 1, 8, true), vec![0x8080_0000, 0xff00_00ff]);
    }

    #[test]
    fn short_bitmap_pads_transparent() {
        assert_eq!(to_argb(&[1, 2, 3, 4], 2, 1, 8, false), vec![0x0401_0203, 0]);
    }
}
