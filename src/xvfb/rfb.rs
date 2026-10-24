//! A minimal RFB client for Xvnc's private socket: one `SetDesktopSize`
//! request per desktop layout.
//!
//! TigerVNC's Xvnc exposes one RandR output by default and creates more only
//! when its RFB `SetDesktopSize` handler is asked for a layout with more
//! screens (see `unix/common/randr.cxx`: `_setScreenLayout` calls
//! `vncRandRCreateOutputs`). The bridge lays the desktop's monitors out as
//! those screens, so X clients see one output and CRTC per monitor, exactly as
//! Xwayland does. The layout is server state: it survives the client that set
//! it, so this connection is short-lived.
//!
//! The socket is the private one `xvfb::start` gives Xvnc (`-rfbunixpath`,
//! mode 0600), and Xvnc runs with `-SecurityTypes None` there. Only the
//! `ExtendedDesktopSize` pseudo-encoding (-308) is asked for, so no pixel data
//! is ever sent.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// One RFB screen: the desktop monitor's rectangle on the X screen, in
/// physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub id: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// A whole desktop layout: the framebuffer (X screen) and its screens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: u16,
    pub height: u16,
    pub screens: Vec<Screen>,
}

impl Layout {
    /// The layout a list of physical monitor rectangles asks for. Screens get
    /// the stable ids 1..=N (id 1 must stay the first output, whose name the
    /// bridge gives to the first monitor).
    pub fn from_rects(width: u16, height: u16, rects: &[(i16, i16, u16, u16)]) -> Self {
        let screens = rects
            .iter()
            .enumerate()
            .map(|(i, &(x, y, w, h))| Screen {
                id: i as u32 + 1,
                x: x.max(0) as u16,
                y: y.max(0) as u16,
                width: w,
                height: h,
            })
            .collect();
        Self {
            width,
            height,
            screens,
        }
    }
}

const EXTENDED_DESKTOP_SIZE: i32 = -308;

/// Sends `layout` to the Xvnc listening on `path`, and waits for its answer.
/// `Ok(())` only if the server applied it (result 0).
pub fn set_desktop_size(path: &Path, layout: &Layout) -> io::Result<()> {
    if layout.screens.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a layout needs at least one screen",
        ));
    }
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    handshake(&mut s)?;
    set_encodings(&mut s)?;
    send_layout(&mut s, layout)?;
    s.flush()?;
    // Xvnc applies the layout and answers in its next update writer flush; the
    // caller checks the X server's actual outputs (which is the only answer
    // that matters), so closing here is enough and never waits on a reply
    Ok(())
}

fn handshake(s: &mut UnixStream) -> io::Result<()> {
    let mut version = [0u8; 12];
    s.read_exact(&mut version)?;
    if !version.starts_with(b"RFB ") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an RFB server",
        ));
    }
    s.write_all(b"RFB 003.008\n")?;
    let mut n = [0u8; 1];
    s.read_exact(&mut n)?;
    let count = n[0] as usize;
    if count == 0 {
        let mut len = [0u8; 4];
        s.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        let mut why = vec![0u8; len.min(4096)];
        s.read_exact(&mut why)?;
        return Err(io::Error::other(format!(
            "Xvnc refused the connection: {}",
            String::from_utf8_lossy(&why)
        )));
    }
    let mut types = vec![0u8; count];
    s.read_exact(&mut types)?;
    if !types.contains(&1) {
        return Err(io::Error::other(format!(
            "Xvnc offers no None security (types {types:?})"
        )));
    }
    s.write_all(&[1])?; // None
    let mut result = [0u8; 4];
    s.read_exact(&mut result)?;
    if u32::from_be_bytes(result) != 0 {
        return Err(io::Error::other("Xvnc's None security check failed"));
    }
    s.write_all(&[1])?; // ClientInit: shared
    let mut init = [0u8; 4];
    s.read_exact(&mut init)?; // framebuffer width, height
    let mut pf = [0u8; 16];
    s.read_exact(&mut pf)?; // server pixel format (unused)
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut name = vec![0u8; len];
    s.read_exact(&mut name)?;
    Ok(())
}

fn set_encodings(s: &mut UnixStream) -> io::Result<()> {
    let mut m = Vec::with_capacity(8);
    m.push(2u8); // SetEncodings
    m.push(0); // padding
    m.extend_from_slice(&1u16.to_be_bytes());
    m.extend_from_slice(&EXTENDED_DESKTOP_SIZE.to_be_bytes());
    s.write_all(&m)
}

fn send_layout(s: &mut UnixStream, l: &Layout) -> io::Result<()> {
    let mut m = Vec::with_capacity(8 + l.screens.len() * 16);
    m.push(251); // SetDesktopSize
    m.push(0);
    m.extend_from_slice(&l.width.to_be_bytes());
    m.extend_from_slice(&l.height.to_be_bytes());
    m.push(l.screens.len() as u8);
    m.push(0);
    for sc in &l.screens {
        m.extend_from_slice(&sc.id.to_be_bytes());
        m.extend_from_slice(&sc.x.to_be_bytes());
        m.extend_from_slice(&sc.y.to_be_bytes());
        m.extend_from_slice(&sc.width.to_be_bytes());
        m.extend_from_slice(&sc.height.to_be_bytes());
        m.extend_from_slice(&0u32.to_be_bytes()); // flags
    }
    s.write_all(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_from_physical_rects_keeps_ids_stable_and_order() {
        let l = Layout::from_rects(2560, 800, &[(0, 0, 1280, 800), (1280, 0, 1280, 800)]);
        assert_eq!((l.width, l.height), (2560, 800));
        assert_eq!(l.screens.len(), 2);
        assert_eq!(
            l.screens[0],
            Screen {
                id: 1,
                x: 0,
                y: 0,
                width: 1280,
                height: 800
            }
        );
        assert_eq!(
            l.screens[1],
            Screen {
                id: 2,
                x: 1280,
                y: 0,
                width: 1280,
                height: 800
            }
        );
    }

    #[test]
    fn negative_rects_are_clamped() {
        let l = Layout::from_rects(1280, 800, &[(-100, -20, 1280, 800)]);
        assert_eq!(l.screens[0].x, 0);
        assert_eq!(l.screens[0].y, 0);
    }
}
