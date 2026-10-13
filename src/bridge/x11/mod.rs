//! A minimal X11 server implementing enough for RealVNC to work and for Xlib
//! not to fail.
pub(crate) mod atom;
pub mod conn;
pub mod ext;
mod mit_shm;
mod property;
pub mod randr;
mod saver;
mod selection;
mod targets;
mod window;
mod xfixes;
pub mod xkb;

// we hardcode some ID choices for simplicity

/// First resource ID assigned to clients (each conn gets a non-overlapping
/// allocation after this, from [`ID_RANGES`]).
pub const CLIENT_RESOURCE_ID_BASE: u32 = 0x0040_0000;

/// The bits of a client's resource ids that are its own (`resource_id_mask`):
/// 2^21 ids each, as Xorg gives each of its clients.
pub const CLIENT_RESOURCE_ID_MASK: u32 = 0x001f_ffff;

/// How many connections can hold a resource-id range at once. X ids have 29
/// bits (the top three are zero, so clients can tell ids from other values)
/// and the ranges start above the server's own fixed ids, which leaves
/// (2^29 - [`CLIENT_RESOURCE_ID_BASE`]) / 2^21 = 254 of them: Xorg's limit
/// too (256 client slots, less its own).
pub const MAX_CLIENTS: usize =
    ((0x2000_0000 - CLIENT_RESOURCE_ID_BASE) / (CLIENT_RESOURCE_ID_MASK + 1)) as usize;

/// The resource-id ranges of the display's connections.
pub static ID_RANGES: IdRanges = IdRanges::new();

/// Hands out each connection's resource-id range and takes it back when the
/// connection ends, as Xorg reuses client slots: so ids stay within X's 29
/// bits and two live connections never share a range, however many have come
/// and gone. (A range per lifetime connection — a counter times the range
/// size — overflowed after 2046 connections, a panic in debug builds and the
/// server's own ids handed out in release ones, and left the 29-bit space
/// after 254.)
pub struct IdRanges {
    used: std::sync::Mutex<[u64; MAX_CLIENTS.div_ceil(64)]>,
}

impl IdRanges {
    pub const fn new() -> Self {
        Self {
            used: std::sync::Mutex::new([0; MAX_CLIENTS.div_ceil(64)]),
        }
    }

    /// The lowest free range, held until the returned guard drops; `None`
    /// while [`MAX_CLIENTS`] connections hold one.
    pub fn take(&self) -> Option<IdRange<'_>> {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = (0..MAX_CLIENTS).find(|&i| used[i / 64] & (1 << (i % 64)) == 0)?;
        used[slot / 64] |= 1 << (slot % 64);
        Some(IdRange { ranges: self, slot })
    }
}

/// One connection's resource-id range, given back on drop.
pub struct IdRange<'a> {
    ranges: &'a IdRanges,
    slot: usize,
}

impl IdRange<'_> {
    /// The range's `resource_id_base` (its mask is [`CLIENT_RESOURCE_ID_MASK`]).
    pub fn base(&self) -> u32 {
        CLIENT_RESOURCE_ID_BASE + self.slot as u32 * (CLIENT_RESOURCE_ID_MASK + 1)
    }
}

impl Drop for IdRange<'_> {
    fn drop(&mut self) {
        let mut used = self
            .ranges
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        used[self.slot / 64] &= !(1 << (self.slot % 64));
    }
}

/// Root window resource ID (arbitrary, must be outside client
/// `resource_id_base`/`mask` in [`conn::setup`]).
pub const ROOT_WINDOW: u32 = 0x0000_016b;

/// Default colormap resource ID (arbitrary, also outside client range).
pub const ROOT_COLORMAP: u32 = 0x0000_0020;

/// TrueColor 24-bit visual ID (arbitrary, also outside client range).
pub const ROOT_VISUAL: u32 = 0x0000_0021;

/// Window ID used to receive selections.
pub const SELECTION_FETCH_WINDOW: u32 = ROOT_WINDOW;

/// First RandR output ID (arbitrary, also outside client range) (leave enough
/// room).
const RANDR_OUTPUT_BASE: u32 = 0x0000_0040;

/// Depth of the root visual.
pub const ROOT_DEPTH: u8 = 24;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_ranges_fill_x_id_space_without_overlap() {
        assert_eq!(MAX_CLIENTS, 254);
        let ranges = IdRanges::new();
        let held: Vec<_> = (0..MAX_CLIENTS).map(|_| ranges.take().unwrap()).collect();
        assert!(ranges.take().is_none(), "full at MAX_CLIENTS");
        for (i, r) in held.iter().enumerate() {
            let (lo, hi) = (r.base(), r.base() | CLIENT_RESOURCE_ID_MASK);
            assert_eq!(lo & CLIENT_RESOURCE_ID_MASK, 0);
            assert!(lo >= CLIENT_RESOURCE_ID_BASE, "above the server's own ids");
            assert!(hi < 0x2000_0000, "range {i} within 29 bits: {hi:#x}");
            for fixed in [ROOT_WINDOW, ROOT_COLORMAP, ROOT_VISUAL, RANDR_OUTPUT_BASE] {
                assert!(!(lo..=hi).contains(&fixed));
            }
            if i > 0 {
                assert!(
                    lo > held[i - 1].base() | CLIENT_RESOURCE_ID_MASK,
                    "no overlap"
                );
            }
        }
    }

    #[test]
    fn setup_failed_is_a_well_formed_refusal() {
        use x11rb_protocol::protocol::xproto::SetupFailed;
        use x11rb_protocol::x11_utils::TryParse;
        let reason = b"Maximum number of clients reached";
        let buf = conn::setup_failed(reason);
        assert_eq!(buf.len() % 4, 0);
        let (f, rest) = SetupFailed::try_parse(&buf).unwrap();
        assert_eq!(
            rest.len(),
            buf.len() - 8 - reason.len(),
            "only the padding is left"
        );
        assert!(rest.len() < 4);
        assert_eq!(f.status, 0);
        assert_eq!(f.protocol_major_version, 11);
        assert_eq!(usize::from(f.length) * 4, buf.len() - 8);
        assert_eq!(f.reason, reason);
    }

    #[test]
    fn id_ranges_are_reused_after_disconnect() {
        let ranges = IdRanges::new();
        let first = ranges.take().unwrap();
        assert_eq!(first.base(), CLIENT_RESOURCE_ID_BASE);
        // a long-lived client keeps its range while thousands come and go
        let mut bases = std::collections::BTreeSet::new();
        for _ in 0..100_000 {
            let r = ranges.take().unwrap();
            assert_ne!(r.base(), first.base());
            bases.insert(r.base());
        }
        assert_eq!(bases.len(), 1, "a freed range is the next one handed out");
        // the lowest free range comes first
        let mut held: Vec<_> = (0..10).map(|_| ranges.take().unwrap()).collect();
        let freed = held.remove(3).base();
        assert_eq!(ranges.take().unwrap().base(), freed);
    }
}
