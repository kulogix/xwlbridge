//! Balloon messages: `_NET_SYSTEM_TRAY_BEGIN_MESSAGE` and `_NET_SYSTEM_TRAY_MESSAGE_DATA`.
//!
//! A tray app shows a balloon (a notification) by sending the tray a `BEGIN_MESSAGE` with a 40-byte header
//! and then one `MESSAGE_DATA` per chunk of at most 20 bytes of text (the last one NUL-terminated). The
//! header holds a timeout and a length (32 bits each, native order), then padding. Icons interleave, a
//! message may be cancelled halfway, and the length is not always a multiple of 20: [`Balloons`] is given
//! segment after segment and hands back each message whole, with the header it started with.

use std::collections::HashMap;

/// The longest text a single message may have (anything longer is a client that lost track, not a balloon).
pub const MAX_TEXT: usize = 64 * 1024;

/// The 40-byte `BEGIN_MESSAGE` header: a timeout and a length, 32 bits each, native order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// How long the balloon should stay up, in milliseconds (0: the notification server's choice, negative:
    /// until dismissed).
    pub timeout: i32,
    /// How many bytes of text follow.
    pub length: u32,
}

impl Header {
    /// The header in `data`: the timeout at offset 0 and the length at offset 4 (what GTK's, Qt's and the
    /// other toolkits' tray code writes). A length that makes no sense (negative, or more text than a
    /// balloon has) is read as none.
    pub fn parse(data: &[u8; 40]) -> Header {
        let int = |at: usize| {
            let mut b = [0; 4];
            b.copy_from_slice(&data[at..at + 4]);
            i32::from_ne_bytes(b)
        };
        let timeout = int(0);
        let length = int(4);
        Header {
            timeout,
            length: length.clamp(0, MAX_TEXT as i32) as u32,
        }
    }
}

/// A message being assembled.
#[derive(Debug)]
struct Open {
    header: Header,
    text: Vec<u8>,
}

/// The messages in progress, by the window the app sent them for.
#[derive(Debug, Default)]
pub struct Balloons {
    open: HashMap<u32, Open>,
    /// What [`Balloons::max_text`] allows a single message.
    pub max_text: usize,
}

impl Balloons {
    pub fn new() -> Balloons {
        Balloons {
            open: HashMap::new(),
            max_text: MAX_TEXT,
        }
    }

    /// `BEGIN_MESSAGE` for `icon`. A message already open for it is dropped: the new one replaces it.
    pub fn begin(&mut self, icon: u32, data: &[u8; 40]) -> Header {
        let header = Header::parse(data);
        self.open.insert(
            icon,
            Open {
                header,
                text: Vec::new(),
            },
        );
        header
    }

    /// `MESSAGE_DATA` for `icon`. `None` while the message goes on; `Some((header, text))` once it is
    /// whole (its length arrived, or a NUL ended it). A segment for an icon with no message open is
    /// ignored.
    pub fn data(&mut self, icon: u32, data: &[u8]) -> Option<(Header, String)> {
        let open = self.open.get_mut(&icon)?;
        let room = (open.header.length as usize).saturating_sub(open.text.len());
        let max = self.max_text.max(1);
        let take = data.len().min(room).min(max - open.text.len().min(max));
        open.text.extend_from_slice(&data[..take]);
        let nul = open.text.iter().position(|&b| b == 0);
        if let Some(nul) = nul {
            open.text.truncate(nul);
        }
        let full = open.text.len() >= open.header.length as usize || open.text.len() >= max;
        if !full && nul.is_none() {
            return None;
        }
        let open = self.open.remove(&icon)?;
        Some((
            open.header,
            String::from_utf8_lossy(&open.text).into_owned(),
        ))
    }

    /// `SYSTEM_TRAY_CANCEL_MESSAGE`, or the icon going away: whatever it had started is dropped.
    pub fn cancel(&mut self, icon: u32) {
        self.open.remove(&icon);
    }

    /// Whether a message is being assembled for `icon`.
    #[allow(dead_code)]
    pub fn busy(&self, icon: u32) -> bool {
        self.open.contains_key(&icon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(timeout: i32, length: u32) -> [u8; 40] {
        let mut h = [0; 40];
        h[0..4].copy_from_slice(&timeout.to_ne_bytes());
        h[4..8].copy_from_slice(&length.to_ne_bytes());
        h
    }

    #[test]
    fn a_header_reads_back() {
        let h = Header::parse(&header(5000, 13));
        assert_eq!((h.timeout, h.length), (5000, 13));
        assert_eq!(Header::parse(&header(-1, 0)).timeout, -1);
        // a nonsense length is no text at all
        assert_eq!(Header::parse(&header(5, u32::MAX)).length, 0);
    }

    #[test]
    fn a_message_arrives_whole() {
        let mut b = Balloons::new();
        b.begin(1, &header(3000, 28));
        assert!(b.data(1, b"hello ").is_none());
        let (h, text) = b.data(1, b"world, how are you the").unwrap();
        assert_eq!(
            (h.timeout, text.as_str()),
            (3000, "hello world, how are you the")
        );
    }

    #[test]
    fn a_length_that_is_not_a_multiple_of_twenty_ends_at_the_length() {
        let mut b = Balloons::new();
        b.begin(1, &header(1000, 13));
        assert!(b.data(1, b"0123456789").is_none());
        assert_eq!(b.data(1, b"abc").unwrap().1, "0123456789abc");
        // and the client may pad the last segment: only the announced length counts
        let mut b = Balloons::new();
        b.begin(1, &header(1000, 5));
        assert_eq!(b.data(1, b"hel\0lo...........").unwrap().1, "hel");
    }

    #[test]
    fn a_nul_ends_a_message_early() {
        let mut b = Balloons::new();
        b.begin(1, &header(1000, 40));
        let (_, text) = b.data(1, b"short\0").unwrap();
        assert_eq!(text, "short");
        assert!(!b.busy(1));
    }

    #[test]
    fn a_cancel_forgets_an_incomplete_message() {
        let mut b = Balloons::new();
        b.begin(7, &header(1000, 40));
        assert!(b.data(7, b"half").is_none());
        b.cancel(7);
        assert!(!b.busy(7));
        assert!(b.data(7, b"ignored").is_none());
    }

    #[test]
    fn two_icons_interleave() {
        let mut b = Balloons::new();
        b.begin(1, &header(1000, 9));
        b.begin(2, &header(2000, 6));
        assert!(b.data(1, b"aaa").is_none());
        assert!(b.data(2, b"bbb").is_none());
        assert!(b.data(1, b"ccc").is_none());
        let (h, text) = b.data(2, b"ddd").unwrap();
        assert_eq!((h.timeout, text.as_str()), (2000, "bbbddd"));
        assert_eq!(b.data(1, b"zzz").unwrap().1, "aaaccczzz");
    }

    #[test]
    fn a_split_character_survives() {
        let text = "é".as_bytes();
        let mut b = Balloons::new();
        b.begin(1, &header(0, text.len() as u32));
        assert!(b.data(1, &text[..1]).is_none());
        assert_eq!(b.data(1, &text[1..]).unwrap().1, "é");
    }

    #[test]
    fn data_without_a_message_is_ignored() {
        let mut b = Balloons::new();
        assert!(b.data(9, b"nobody").is_none());
    }

    #[test]
    fn a_message_that_never_ends_stops_at_the_cap() {
        let mut b = Balloons::new();
        b.max_text = 25;
        b.begin(1, &header(0, 1_000_000));
        assert!(b.data(1, &[b'x'; 20]).is_none());
        assert_eq!(b.data(1, &[b'y'; 10]).unwrap().1.len(), 25);
    }
}
