//! XKEYBOARD, read-only: the compositor keymap as an XKB description
//! ([`Desc`]), enough for Xlib's own XKB keysym handling (every Xlib client
//! turns XKB on at connect), xdotool (`XkbGetMap`, `XkbGetState`,
//! `XkbLockGroup`), `setxkbmap -query`/`-print` (the `_XKB_RULES_NAMES` root
//! property, served by the connection), `xkbcomp -xkb $DISPLAY` (the whole map,
//! names, compat, indicators, controls) and xev.
//!
//! Nothing can change the keyboard: it is the compositor's. Requests that
//! would (SetMap and friends) fail with BadImplementation, as the core
//! `ChangeKeyboardMapping` does; ones that only adjust state we don't keep
//! (LatchLockState, SetControls, indicator changes) are accepted and
//! ignored, as `ChangeKeyboardControl` is (Bell rings the core bell, see
//! `crate::bridge::bell`); `GetKbdByName` loads nothing and
//! finds nothing, which is also what Xorg answers when asked for the current
//! keyboard by empty names.
//!
//! Requests are parsed here from their fixed headers rather than by
//! `x11rb_protocol`, so a variable part we don't need can't make a request
//! fail; replies are encoded here too (tests cross-check them against
//! x11rb's xcb-proto-generated parsers). Ranges a client asks for are clamped
//! rather than refused: an X error is fatal to an Xlib client with the default
//! error handler, and these clients ask for what an earlier reply told them.

use crate::bridge::keymap::{MAX_KEYCODE, MIN_KEYCODE};
use crate::bridge::xkbdesc::{Action, Desc, NUM_KEYS};

pub const NAME: &str = "XKEYBOARD";

/// The extension's place in [`EXTENSIONS`](super::ext::EXTENSIONS): its
/// requests are recognised by this opcode ahead of `Request::parse`.
pub const MAJOR_OPCODE: u8 = 136;
pub const FIRST_EVENT: u8 = 95;
pub const FIRST_ERROR: u8 = 160;

/// The root window property naming the keymap's rules, model, layout, variant
/// and options (`XkbRF_GetNamesProp`).
pub const RULES_NAMES_PROPERTY: &str = "_XKB_RULES_NAMES";

/// The device id we report for the one keyboard (Xorg's core keyboard is 3).
pub const DEVICE_ID: u8 = 3;

/// Minor opcodes.
pub mod op {
    pub const USE_EXTENSION: u8 = 0;
    pub const SELECT_EVENTS: u8 = 1;
    pub const BELL: u8 = 3;
    pub const GET_STATE: u8 = 4;
    pub const LATCH_LOCK_STATE: u8 = 5;
    pub const GET_CONTROLS: u8 = 6;
    pub const SET_CONTROLS: u8 = 7;
    pub const GET_MAP: u8 = 8;
    pub const SET_MAP: u8 = 9;
    pub const GET_COMPAT_MAP: u8 = 10;
    pub const SET_COMPAT_MAP: u8 = 11;
    pub const GET_INDICATOR_STATE: u8 = 12;
    pub const GET_INDICATOR_MAP: u8 = 13;
    pub const SET_INDICATOR_MAP: u8 = 14;
    pub const GET_NAMED_INDICATOR: u8 = 15;
    pub const SET_NAMED_INDICATOR: u8 = 16;
    pub const GET_NAMES: u8 = 17;
    pub const SET_NAMES: u8 = 18;
    pub const GET_GEOMETRY: u8 = 19;
    pub const SET_GEOMETRY: u8 = 20;
    pub const PER_CLIENT_FLAGS: u8 = 21;
    pub const LIST_COMPONENTS: u8 = 22;
    pub const GET_KBD_BY_NAME: u8 = 23;
    pub const GET_DEVICE_INFO: u8 = 24;
    pub const SET_DEVICE_INFO: u8 = 25;
    pub const SET_DEBUGGING_FLAGS: u8 = 101;
}

/// Map components (`XkbKeyTypesMask`, ...), in `GetMap`/`MapNotify` `changed`.
pub mod part {
    pub const KEY_TYPES: u16 = 1 << 0;
    pub const KEY_SYMS: u16 = 1 << 1;
    pub const MODIFIER_MAP: u16 = 1 << 2;
    pub const EXPLICIT: u16 = 1 << 3;
    pub const KEY_ACTIONS: u16 = 1 << 4;
    pub const KEY_BEHAVIORS: u16 = 1 << 5;
    pub const VIRTUAL_MODS: u16 = 1 << 6;
    pub const VIRTUAL_MOD_MAP: u16 = 1 << 7;
    pub const ALL: u16 = 0xff;
}

/// Event types (`XkbMapNotify`, ...), as bit numbers in SelectEvents and
/// the second byte of an XKB event.
pub mod event {
    pub const NEW_KEYBOARD_NOTIFY: u16 = 0;
    pub const MAP_NOTIFY: u16 = 1;
}

/// NewKeyboardNotify details (`XkbNKN_KeycodesMask`, ...).
pub mod nkn {
    pub const KEYCODES: u16 = 1 << 0;
    pub const ALL: u16 = 0x7;
}

/// The `PerClientFlags` we support (detectable auto-repeat, grabs/lookups
/// using XKB state, auto-reset controls): we keep them, they change nothing
/// here.
const PER_CLIENT_SUPPORTED: u32 = 0x1f;

/// Explicit components (`XkbExplicitKeyType1Mask`, ...).
const EXPLICIT_KEY_TYPE1: u8 = 1 << 0;
const EXPLICIT_INTERPRET: u8 = 1 << 4;
const EXPLICIT_AUTO_REPEAT: u8 = 1 << 5;
const EXPLICIT_VMOD_MAP: u8 = 1 << 7;

/// `GetMap`'s ranges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapRequest {
    pub full: u16,
    pub partial: u16,
    pub first_type: u8,
    pub n_types: u8,
    pub first_key_sym: u8,
    pub n_key_syms: u8,
    pub first_key_act: u8,
    pub n_key_acts: u8,
    pub first_key_behavior: u8,
    pub n_key_behaviors: u8,
    pub virtual_mods: u16,
    pub first_key_explicit: u8,
    pub n_key_explicit: u8,
    pub first_mod_map_key: u8,
    pub n_mod_map_keys: u8,
    pub first_vmod_map_key: u8,
    pub n_vmod_map_keys: u8,
}

/// The part of `SelectEvents` we act on: what the client wants to hear when
/// the keymap changes. For each of NewKeyboardNotify's and MapNotify's
/// details, `affect` 0 leaves the client's selection as it was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Select {
    pub nkn: Change,
    pub map: Change,
}

/// Which detail bits change, and to what.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub affect: u16,
    pub values: u16,
}

impl Change {
    /// The details after the change, given the current ones.
    pub fn apply(&self, current: u16) -> u16 {
        (current & !self.affect) | (self.values & self.affect)
    }
}

/// A parsed XKB request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    UseExtension {
        major: u16,
        minor: u16,
    },
    SelectEvents(Select),
    GetState,
    GetControls,
    GetMap(MapRequest),
    GetCompatMap {
        groups: u8,
        all: bool,
        first: u16,
        count: u16,
    },
    GetIndicatorState,
    GetIndicatorMap {
        which: u32,
    },
    GetNamedIndicator {
        indicator: u32,
    },
    GetNames {
        which: u32,
    },
    GetGeometry {
        name: u32,
    },
    PerClientFlags {
        change: u32,
        value: u32,
    },
    ListComponents,
    GetKbdByName {
        load: bool,
    },
    GetDeviceInfo {
        first_button: u8,
        n_buttons: u8,
    },
    SetDebuggingFlags,
    /// Changes state we don't keep: no reply, nothing done.
    Ignored(u8),
    /// Would change the keyboard: refused (BadImplementation).
    Refused(u8),
    /// Not an XKB request we know.
    Unknown(u8),
}

/// The request's name, for traces.
pub fn request_name(minor: u8) -> &'static str {
    match minor {
        op::USE_EXTENSION => "UseExtension",
        op::SELECT_EVENTS => "SelectEvents",
        op::BELL => "Bell",
        op::GET_STATE => "GetState",
        op::LATCH_LOCK_STATE => "LatchLockState",
        op::GET_CONTROLS => "GetControls",
        op::SET_CONTROLS => "SetControls",
        op::GET_MAP => "GetMap",
        op::SET_MAP => "SetMap",
        op::GET_COMPAT_MAP => "GetCompatMap",
        op::SET_COMPAT_MAP => "SetCompatMap",
        op::GET_INDICATOR_STATE => "GetIndicatorState",
        op::GET_INDICATOR_MAP => "GetIndicatorMap",
        op::SET_INDICATOR_MAP => "SetIndicatorMap",
        op::GET_NAMED_INDICATOR => "GetNamedIndicator",
        op::SET_NAMED_INDICATOR => "SetNamedIndicator",
        op::GET_NAMES => "GetNames",
        op::SET_NAMES => "SetNames",
        op::GET_GEOMETRY => "GetGeometry",
        op::SET_GEOMETRY => "SetGeometry",
        op::PER_CLIENT_FLAGS => "PerClientFlags",
        op::LIST_COMPONENTS => "ListComponents",
        op::GET_KBD_BY_NAME => "GetKbdByName",
        op::GET_DEVICE_INFO => "GetDeviceInfo",
        op::SET_DEVICE_INFO => "SetDeviceInfo",
        op::SET_DEBUGGING_FLAGS => "SetDebuggingFlags",
        _ => "?",
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u8_at(b: &[u8], at: usize) -> Option<u8> {
    b.get(at).copied()
}

/// Parses an XKB request from its body (what follows the 4- or 8-byte
/// header). `None` if the body is too short for the request (BadLength).
pub fn parse(minor: u8, body: &[u8]) -> Option<Request> {
    let b = body;
    Some(match minor {
        op::USE_EXTENSION => Request::UseExtension {
            major: u16_at(b, 0)?,
            minor: u16_at(b, 2)?,
        },
        op::SELECT_EVENTS => Request::SelectEvents(parse_select(b)?),
        op::GET_STATE => Request::GetState,
        op::GET_CONTROLS => Request::GetControls,
        op::GET_MAP => Request::GetMap(MapRequest {
            full: u16_at(b, 2)?,
            partial: u16_at(b, 4)?,
            first_type: u8_at(b, 6)?,
            n_types: u8_at(b, 7)?,
            first_key_sym: u8_at(b, 8)?,
            n_key_syms: u8_at(b, 9)?,
            first_key_act: u8_at(b, 10)?,
            n_key_acts: u8_at(b, 11)?,
            first_key_behavior: u8_at(b, 12)?,
            n_key_behaviors: u8_at(b, 13)?,
            virtual_mods: u16_at(b, 14)?,
            first_key_explicit: u8_at(b, 16)?,
            n_key_explicit: u8_at(b, 17)?,
            first_mod_map_key: u8_at(b, 18)?,
            n_mod_map_keys: u8_at(b, 19)?,
            first_vmod_map_key: u8_at(b, 20)?,
            n_vmod_map_keys: u8_at(b, 21)?,
        }),
        op::GET_COMPAT_MAP => Request::GetCompatMap {
            groups: u8_at(b, 2)?,
            all: u8_at(b, 3)? != 0,
            first: u16_at(b, 4)?,
            count: u16_at(b, 6)?,
        },
        op::GET_INDICATOR_STATE => Request::GetIndicatorState,
        op::GET_INDICATOR_MAP => Request::GetIndicatorMap {
            which: u32_at(b, 4)?,
        },
        op::GET_NAMED_INDICATOR => Request::GetNamedIndicator {
            indicator: u32_at(b, 8)?,
        },
        op::GET_NAMES => Request::GetNames {
            which: u32_at(b, 4)?,
        },
        op::GET_GEOMETRY => Request::GetGeometry {
            name: u32_at(b, 4)?,
        },
        op::PER_CLIENT_FLAGS => Request::PerClientFlags {
            change: u32_at(b, 4)?,
            value: u32_at(b, 8)?,
        },
        op::LIST_COMPONENTS => Request::ListComponents,
        op::GET_KBD_BY_NAME => Request::GetKbdByName {
            load: u8_at(b, 6)? != 0,
        },
        op::GET_DEVICE_INFO => Request::GetDeviceInfo {
            first_button: u8_at(b, 5)?,
            n_buttons: u8_at(b, 6)?,
        },
        op::SET_DEBUGGING_FLAGS => Request::SetDebuggingFlags,
        op::BELL
        | op::LATCH_LOCK_STATE
        | op::SET_CONTROLS
        | op::SET_INDICATOR_MAP
        | op::SET_NAMED_INDICATOR => Request::Ignored(minor),
        op::SET_MAP
        | op::SET_COMPAT_MAP
        | op::SET_NAMES
        | op::SET_GEOMETRY
        | op::SET_DEVICE_INFO => Request::Refused(minor),
        other => Request::Unknown(other),
    })
}

/// `SelectEvents`: deviceSpec, affectWhich, clear, selectAll, affectMap, map,
/// then one (affect, details) pair per other event type being changed, in
/// event-type order, NewKeyboardNotify's first. Only the two events a keymap
/// change sends are kept: Xlib selects NewKeyboardNotify (keycodes) when it
/// turns XKB on, which is what makes it reload its whole copy of the keymap,
/// and MapNotify (`XkbAllClientInfoMask`) once it has loaded it.
fn parse_select(b: &[u8]) -> Option<Select> {
    let affect_which = u16_at(b, 2)?;
    let clear = u16_at(b, 4)?;
    let select_all = u16_at(b, 6)?;
    let affect_map = u16_at(b, 8)?;
    let map = u16_at(b, 10)?;
    let change = |bit: u16, all: u16, detail: &dyn Fn() -> Option<Change>| {
        let bit = 1 << bit;
        Some(if affect_which & bit == 0 {
            Change::default()
        } else if clear & bit != 0 {
            Change {
                affect: all,
                values: 0,
            }
        } else if select_all & bit != 0 {
            Change {
                affect: all,
                values: all,
            }
        } else {
            detail()?
        })
    };
    Some(Select {
        nkn: change(event::NEW_KEYBOARD_NOTIFY, nkn::ALL, &|| {
            Some(Change {
                affect: u16_at(b, 12)? & nkn::ALL,
                values: u16_at(b, 14)? & nkn::ALL,
            })
        })?,
        map: change(event::MAP_NOTIFY, part::ALL, &|| {
            Some(Change {
                affect: affect_map & part::ALL,
                values: map & part::ALL,
            })
        })?,
    })
}

/// A reply under construction (little-endian, as are all our clients).
struct Wire(Vec<u8>);

impl Wire {
    /// A reply header: type 1, `data` in byte 1, sequence and length patched
    /// on sending (see [`finish`]).
    fn reply(data: u8) -> Wire {
        Wire(vec![1, data, 0, 0, 0, 0, 0, 0])
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn pad(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + n, 0);
        self
    }
    /// Pads to a 4-byte boundary (of the whole reply, whose header is).
    fn align(&mut self) -> &mut Self {
        let n = (4 - self.0.len() % 4) % 4;
        self.pad(n)
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
}

/// Pads a reply to at least 32 bytes and a multiple of 4, and sets its length
/// field; the sequence is stamped when it is queued.
pub fn finish(mut buf: Vec<u8>) -> Vec<u8> {
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    let pad = (4 - buf.len() % 4) % 4;
    buf.resize(buf.len() + pad, 0);
    let length = ((buf.len() - 32) / 4) as u32;
    buf[4..8].copy_from_slice(&length.to_le_bytes());
    buf
}

pub fn use_extension() -> Vec<u8> {
    let mut w = Wire::reply(1); // supported
    w.u16(1).u16(0); // server 1.0
    w.0
}

pub fn per_client_flags(value: u32) -> Vec<u8> {
    let mut w = Wire::reply(DEVICE_ID);
    w.u32(PER_CLIENT_SUPPORTED).u32(value).u32(0).u32(0);
    w.0
}

/// The client's per-client flags after a `PerClientFlags`.
pub fn per_client_value(current: u32, change: u32, value: u32) -> u32 {
    ((current & !change) | (value & change)) & PER_CLIENT_SUPPORTED
}

/// All zero: we don't follow the compositor's modifier or group state.
pub fn get_state() -> Vec<u8> {
    Wire::reply(DEVICE_ID).0
}

pub fn get_controls(d: &Desc) -> Vec<u8> {
    let mut w = Wire::reply(DEVICE_ID);
    w.u8(1) // mouseKeysDfltBtn
        .u8(1) // numGroups
        .u8(0) // groupsWrap
        .u8(0) // internalMods.mask
        .u8(0) // ignoreLockMods.mask
        .u8(0) // internalMods.realMods
        .u8(0) // ignoreLockMods.realMods
        .pad(1)
        .u16(0) // internalMods.vmods
        .u16(0) // ignoreLockMods.vmods
        .u16(660) // repeatDelay (Xorg's defaults)
        .u16(40) // repeatInterval
        .u16(300) // slowKeysDelay
        .u16(300) // debounceDelay
        .u16(160) // mouseKeysDelay
        .u16(40) // mouseKeysInterval
        .u16(30) // mouseKeysTimeToMax
        .u16(30) // mouseKeysMaxSpeed
        .u16(500) // mouseKeysCurve
        .u16(0) // accessXOption
        .u16(120) // accessXTimeout
        .u16(0) // accessXTimeoutOptionsMask
        .u16(0) // accessXTimeoutOptionsValues
        .pad(2)
        .u32(0) // accessXTimeoutMask
        .u32(0) // accessXTimeoutValues
        .u32(1); // enabledControls: RepeatKeys
    let mut per_key = [0u8; 32];
    for (i, key) in d.keys.iter().enumerate() {
        let kc = i + usize::from(MIN_KEYCODE);
        if key.repeats {
            per_key[kc / 8] |= 1 << (kc % 8);
        }
    }
    w.bytes(&per_key);
    w.0
}

/// Clamps a requested key range to the keycodes we have: (first, count).
fn key_range(first: u8, n: u8) -> (u8, u8) {
    let first = first.max(MIN_KEYCODE);
    let last = (u16::from(first) + u16::from(n)).min(u16::from(MAX_KEYCODE) + 1);
    (first, (last - u16::from(first)) as u8)
}

fn keys(
    d: &Desc,
    (first, n): (u8, u8),
) -> impl Iterator<Item = (u8, &crate::bridge::xkbdesc::Key)> {
    (u16::from(first)..u16::from(first) + u16::from(n)).map(move |kc| {
        let kc = kc as u8; // key_range keeps it within 8..=255
        (kc, d.key(kc))
    })
}

fn explicit(k: &crate::bridge::xkbdesc::Key) -> u8 {
    let mut e = 0;
    if !k.syms.is_empty() {
        e |= EXPLICIT_KEY_TYPE1;
        if !k.repeats {
            e |= EXPLICIT_AUTO_REPEAT;
        }
    }
    if k.action.is_some() {
        e |= EXPLICIT_INTERPRET;
    }
    if k.vmodmap != 0 {
        e |= EXPLICIT_VMOD_MAP;
    }
    e
}

/// The 8-byte wire form of the action on one level of a key. The plain kinds
/// take the key's modifier map (`UseModMapMods`, resolved by whoever applies
/// them; the map is filled in too, for readers that just look); the `Mask`
/// kinds name their modifiers.
fn action(k: &crate::bridge::xkbdesc::Key, level: usize) -> [u8; 8] {
    // type, flags, mask, realMods, vmods (high, low), pad
    match Desc::level_action(k, level) {
        // flags: ClearLocks | UseModMapMods
        Some(Action::SetMods) => [1, 1 | 4, k.modmap, k.modmap, 0, 0, 0, 0],
        // flags: UseModMapMods
        Some(Action::LockMods) => [3, 4, k.modmap, k.modmap, 0, 0, 0, 0],
        // flags: ClearLocks
        Some(Action::SetModsMask { mask, real, vmods }) => {
            let [hi, lo] = vmods.to_be_bytes();
            [1, 1, mask, real, hi, lo, 0, 0]
        }
        Some(Action::LockModsMask { mask, real, vmods }) => {
            let [hi, lo] = vmods.to_be_bytes();
            [3, 0, mask, real, hi, lo, 0, 0]
        }
        None => [0; 8],
    }
}

pub fn get_map(d: &Desc, r: &MapRequest) -> Vec<u8> {
    let wanted = (r.full | r.partial) & part::ALL;
    let full = |p: u16| r.full & p != 0;
    let has = |p: u16| wanted & p != 0;
    let all_keys = (MIN_KEYCODE, NUM_KEYS as u8);
    let range = |p: u16, first: u8, n: u8| {
        if full(p) {
            all_keys
        } else if has(p) {
            key_range(first, n)
        } else {
            (0, 0)
        }
    };
    let total_types = d.types.len() as u8;
    let (first_type, n_types) = if full(part::KEY_TYPES) {
        (0, total_types)
    } else if has(part::KEY_TYPES) {
        let first = r.first_type.min(total_types);
        (first, r.n_types.min(total_types - first))
    } else {
        (0, 0)
    };
    let syms = range(part::KEY_SYMS, r.first_key_sym, r.n_key_syms);
    let acts = range(part::KEY_ACTIONS, r.first_key_act, r.n_key_acts);
    let behaviors = range(part::KEY_BEHAVIORS, r.first_key_behavior, r.n_key_behaviors);
    let explicits = range(part::EXPLICIT, r.first_key_explicit, r.n_key_explicit);
    let modmaps = range(part::MODIFIER_MAP, r.first_mod_map_key, r.n_mod_map_keys);
    let vmodmaps = range(
        part::VIRTUAL_MOD_MAP,
        r.first_vmod_map_key,
        r.n_vmod_map_keys,
    );
    let virtual_mods = if full(part::VIRTUAL_MODS) {
        0xffff
    } else if has(part::VIRTUAL_MODS) {
        r.virtual_mods
    } else {
        0
    };

    let total_syms: usize = keys(d, syms).map(|(_, k)| k.syms.len()).sum();
    let act_count = |k: &crate::bridge::xkbdesc::Key| {
        if k.action.is_some() {
            Desc::width(k)
        } else {
            0
        }
    };
    let total_acts: usize = keys(d, acts).map(|(_, k)| usize::from(act_count(k))).sum();
    let total_explicit = keys(d, explicits).filter(|(_, k)| explicit(k) != 0).count();
    let total_modmap = keys(d, modmaps).filter(|(_, k)| k.modmap != 0).count();
    let total_vmodmap = keys(d, vmodmaps).filter(|(_, k)| k.vmodmap != 0).count();

    let mut w = Wire::reply(DEVICE_ID);
    w.pad(2)
        .u8(MIN_KEYCODE)
        .u8(MAX_KEYCODE)
        .u16(wanted)
        .u8(first_type)
        .u8(n_types)
        .u8(total_types)
        .u8(syms.0)
        .u16(total_syms as u16)
        .u8(syms.1)
        .u8(acts.0)
        .u16(total_acts as u16)
        .u8(acts.1)
        .u8(behaviors.0)
        .u8(behaviors.1)
        .u8(0) // totalKeyBehaviors: none
        .u8(explicits.0)
        .u8(explicits.1)
        .u8(total_explicit as u8)
        .u8(modmaps.0)
        .u8(modmaps.1)
        .u8(total_modmap as u8)
        .u8(vmodmaps.0)
        .u8(vmodmaps.1)
        .u8(total_vmodmap as u8)
        .pad(1)
        .u16(virtual_mods);
    debug_assert_eq!(w.0.len(), 40);

    if has(part::KEY_TYPES) {
        for t in &d.types[usize::from(first_type)..usize::from(first_type + n_types)] {
            let preserve = t.entries.iter().any(|e| e.preserve != (0, 0, 0));
            w.u8(t.mods)
                .u8(t.real)
                .u16(t.vmods)
                .u8(t.num_levels)
                .u8(t.entries.len() as u8)
                .u8(u8::from(preserve))
                .pad(1);
            for e in &t.entries {
                w.u8(1)
                    .u8(e.mods)
                    .u8(e.level)
                    .u8(e.real)
                    .u16(e.vmods)
                    .pad(2);
            }
            if preserve {
                for e in &t.entries {
                    let (mask, real, vmods) = e.preserve;
                    w.u8(mask).u8(real).u16(vmods);
                }
            }
        }
    }
    if has(part::KEY_SYMS) {
        for (_, k) in keys(d, syms) {
            if k.syms.is_empty() {
                w.bytes(&[0; 4]).u8(0).u8(0).u16(0);
            } else {
                // one group, out-of-range groups wrap
                w.bytes(&[k.kt, 0, 0, 0])
                    .u8(1)
                    .u8(k.syms.len() as u8)
                    .u16(k.syms.len() as u16);
                for &s in &k.syms {
                    w.u32(s);
                }
            }
        }
    }
    if has(part::KEY_ACTIONS) {
        for (_, k) in keys(d, acts) {
            w.u8(act_count(k));
        }
        w.align();
        for (_, k) in keys(d, acts) {
            for level in 0..usize::from(act_count(k)) {
                w.bytes(&action(k, level));
            }
        }
    }
    // KEY_BEHAVIORS: none to list
    if has(part::VIRTUAL_MODS) {
        for i in 0..16 {
            if virtual_mods & (1 << i) != 0 {
                w.u8(d.vmods.get(i).map_or(0, |v| v.1));
            }
        }
        w.align();
    }
    if has(part::EXPLICIT) {
        for (kc, k) in keys(d, explicits) {
            if explicit(k) != 0 {
                w.u8(kc).u8(explicit(k));
            }
        }
        w.align();
    }
    if has(part::MODIFIER_MAP) {
        for (kc, k) in keys(d, modmaps) {
            if k.modmap != 0 {
                w.u8(kc).u8(k.modmap);
            }
        }
        w.align();
    }
    if has(part::VIRTUAL_MOD_MAP) {
        for (kc, k) in keys(d, vmodmaps) {
            if k.vmodmap != 0 {
                w.u8(kc).pad(1).u16(k.vmodmap);
            }
        }
    }
    w.0
}

/// The symbol interpretations: just one catch-all that gives keys no action
/// and lets them repeat, because the actions are on the (modifier) keys
/// themselves, marked explicit along with whether they repeat. Xlib won't
/// hand `xkbcomp -xkb` a compat map with none at all ("compatibility map not
/// defined"). No group compatibility modifiers.
const INTERPRETS: usize = 1;

pub fn get_compat_map(groups: u8, all: bool, first: u16, count: u16) -> Vec<u8> {
    let groups = groups & 0x0f;
    let (first, n) = if all {
        (0, INTERPRETS)
    } else {
        let first = usize::from(first).min(INTERPRETS);
        (first, usize::from(count).min(INTERPRETS - first))
    };
    let mut w = Wire::reply(DEVICE_ID);
    w.u8(groups)
        .pad(1)
        .u16(first as u16)
        .u16(n as u16)
        .u16(INTERPRETS as u16)
        .pad(16);
    for _ in 0..n {
        // sym: Any, mods: all, match: AnyOfOrNone, virtualMod: none,
        // flags: AutoRepeat, action: NoAction
        w.u32(0).u8(0xff).u8(1).u8(0xff).u8(1).pad(8);
    }
    for _ in 0..groups.count_ones() {
        w.u32(0); // ModDef: mask, realMods, vmods
    }
    w.0
}

pub fn get_indicator_state() -> Vec<u8> {
    let mut w = Wire::reply(DEVICE_ID);
    w.u32(0);
    w.0
}

/// The usual indicator maps, by LED name (what xkeyboard-config's compat
/// gives them): flags, whichGroups, groups, whichMods, mods, realMods, vmods,
/// ctrls.
fn indicator_map(d: &Desc, name: &str) -> (u8, u8, u8, u8, u8, u8, u16, u32) {
    const LOCKED: u8 = 1 << 2;
    const EFFECTIVE: u8 = 1 << 3;
    const NO_EXPLICIT: u8 = 1 << 7;
    let vmod = |n: &str| {
        d.vmods
            .iter()
            .position(|(v, _)| v == n)
            .map_or((0, 0), |i| (d.vmods[i].1, 1u16 << i))
    };
    match name {
        "Caps Lock" => (0, 0, 0, LOCKED, 2, 2, 0, 0),
        "Shift Lock" => (0, 0, 0, LOCKED, 1, 1, 0, 0),
        "Num Lock" | "Scroll Lock" => {
            let (mask, v) = vmod(if name == "Num Lock" {
                "NumLock"
            } else {
                "ScrollLock"
            });
            (0, 0, 0, LOCKED, mask, 0, v, 0)
        }
        "Group 2" => (NO_EXPLICIT, EFFECTIVE, 0xfe, 0, 0, 0, 0, 0),
        "Mouse Keys" => (0, 0, 0, 0, 0, 0, 0, 1 << 4),
        _ => (0, 0, 0, 0, 0, 0, 0, 0),
    }
}

fn indicator_bits(d: &Desc) -> u32 {
    d.indicators.iter().fold(0, |m, (i, _)| m | 1 << i)
}

pub fn get_indicator_map(d: &Desc, which: u32) -> Vec<u8> {
    let mut w = Wire::reply(DEVICE_ID);
    w.u32(which).u32(indicator_bits(d)).u8(32).pad(15);
    for i in 0..32u8 {
        if which & (1 << i) == 0 {
            continue;
        }
        let name = d
            .indicators
            .iter()
            .find(|(n, _)| *n == i)
            .map(|(_, s)| s.as_str());
        let m = name.map_or((0, 0, 0, 0, 0, 0, 0, 0), |n| indicator_map(d, n));
        w.u8(m.0)
            .u8(m.1)
            .u8(m.2)
            .u8(m.3)
            .u8(m.4)
            .u8(m.5)
            .u16(m.6)
            .u32(m.7);
    }
    w.0
}

pub fn get_named_indicator(d: &Desc, indicator: u32, name: Option<&str>) -> Vec<u8> {
    let found = name.and_then(|n| d.indicators.iter().find(|(_, s)| s == n));
    let m = found.map_or((0, 0, 0, 0, 0, 0, 0, 0), |(_, n)| indicator_map(d, n));
    let mut w = Wire::reply(DEVICE_ID);
    w.u32(indicator)
        .u8(u8::from(found.is_some()))
        .u8(0) // on
        .u8(u8::from(found.is_some())) // realIndicator
        .u8(found.map_or(0, |(i, _)| *i))
        .u8(m.0)
        .u8(m.1)
        .u8(m.2)
        .u8(m.3)
        .u8(m.4)
        .u8(m.5)
        .u16(m.6)
        .u32(m.7)
        .u8(1); // supported
    w.0
}

/// `GetNames`. `intern` turns a name into an atom ("" into None).
pub fn get_names(d: &Desc, which: u32, intern: &mut dyn FnMut(&str) -> u32) -> Vec<u8> {
    const KEYCODES: u32 = 1 << 0;
    const GEOMETRY: u32 = 1 << 1;
    const SYMBOLS: u32 = 1 << 2;
    const PHYS_SYMBOLS: u32 = 1 << 3;
    const TYPES: u32 = 1 << 4;
    const COMPAT: u32 = 1 << 5;
    const KEY_TYPE_NAMES: u32 = 1 << 6;
    const KT_LEVEL_NAMES: u32 = 1 << 7;
    const INDICATOR_NAMES: u32 = 1 << 8;
    const KEY_NAMES: u32 = 1 << 9;
    const KEY_ALIASES: u32 = 1 << 10;
    const VIRTUAL_MOD_NAMES: u32 = 1 << 11;
    const GROUP_NAMES: u32 = 1 << 12;
    const RG_NAMES: u32 = 1 << 13;
    let which = which & 0x3fff;
    let mut atom = |s: &str| if s.is_empty() { 0 } else { intern(s) };

    let n_types = d.types.len() as u8;
    let groups: u8 = u8::from(!d.group_name.is_empty());
    let vmods = d
        .vmods
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| !n.is_empty())
        .fold(0u16, |m, (i, _)| m | 1 << i);
    let indicators = indicator_bits(d);
    let n_kt_levels: usize = d.types.iter().map(|t| usize::from(t.num_levels)).sum();

    let mut w = Wire::reply(DEVICE_ID);
    w.u32(which)
        .u8(MIN_KEYCODE)
        .u8(MAX_KEYCODE)
        .u8(n_types)
        .u8(groups)
        .u16(vmods)
        .u8(MIN_KEYCODE)
        .u8(NUM_KEYS as u8)
        .u32(indicators)
        .u8(0) // radio groups
        .u8(d.aliases.len() as u8)
        .u16(n_kt_levels as u16)
        .pad(4);
    for (bit, name) in [
        (KEYCODES, &d.keycodes_name),
        (GEOMETRY, &d.geometry_name),
        (SYMBOLS, &d.symbols_name),
        (PHYS_SYMBOLS, &d.symbols_name),
        (TYPES, &d.types_name),
        (COMPAT, &d.compat_name),
    ] {
        if which & bit != 0 {
            w.u32(atom(name));
        }
    }
    if which & KEY_TYPE_NAMES != 0 {
        for t in &d.types {
            w.u32(atom(&t.name));
        }
    }
    if which & KT_LEVEL_NAMES != 0 {
        for t in &d.types {
            w.u8(t.num_levels);
        }
        w.align();
        for t in &d.types {
            for level in 0..usize::from(t.num_levels) {
                w.u32(atom(t.level_names.get(level).map_or("", String::as_str)));
            }
        }
    }
    if which & INDICATOR_NAMES != 0 {
        for (_, name) in &d.indicators {
            w.u32(atom(name));
        }
    }
    if which & VIRTUAL_MOD_NAMES != 0 {
        for (name, _) in d.vmods.iter().filter(|(n, _)| !n.is_empty()) {
            w.u32(atom(name));
        }
    }
    if which & GROUP_NAMES != 0 && groups != 0 {
        w.u32(atom(&d.group_name));
    }
    if which & KEY_NAMES != 0 {
        for k in &d.keys {
            w.bytes(&k.name);
        }
    }
    if which & KEY_ALIASES != 0 {
        for (real, alias) in &d.aliases {
            w.bytes(real).bytes(alias);
        }
    }
    let _ = RG_NAMES; // none
    w.0
}

/// No geometry: `found` false, which Xlib reports as BadName, not an error.
pub fn get_geometry(name: u32) -> Vec<u8> {
    let mut w = Wire::reply(DEVICE_ID);
    w.u32(name).u8(0);
    w.0
}

/// Nothing listed: the server has no component database.
pub fn list_components() -> Vec<u8> {
    Wire::reply(DEVICE_ID).0
}

/// Nothing loaded (read-only) and nothing found: `setxkbmap us` then fails
/// with "Error loading new keyboard description" instead of an X error.
pub fn get_kbd_by_name() -> Vec<u8> {
    let mut w = Wire::reply(DEVICE_ID);
    w.u8(MIN_KEYCODE).u8(MAX_KEYCODE).u8(0).u8(0).u16(0).u16(0);
    w.0
}

pub fn get_device_info(first_button: u8, n_buttons: u8) -> Vec<u8> {
    const NAME: &[u8] = b"xwlrvnc keyboard";
    let mut w = Wire::reply(DEVICE_ID);
    w.u16(0) // present
        .u16(0) // supported
        .u16(0) // unsupported
        .u16(0) // nDeviceLedFBs
        .u8(first_button)
        .u8(n_buttons)
        .u8(0) // firstBtnRtrn
        .u8(0) // nBtnsRtrn
        .u8(0) // totalBtns
        .u8(0) // hasOwnState
        .u16(0) // dfltKbdFB
        .u16(0) // dfltLedFB
        .pad(2)
        .u32(0) // devType
        .u16(NAME.len() as u16)
        .bytes(NAME)
        .align();
    w.0
}

pub fn set_debugging_flags() -> Vec<u8> {
    let mut w = Wire::reply(0);
    w.u32(0).u32(0).u32(0).u32(0);
    w.0
}

/// `XkbNewKeyboardNotify` saying the keyboard changed, if the client asked to
/// hear of new keycodes (`details`), as Xorg sends when it loads a keymap.
/// Xlib then reloads its whole copy of the keymap, types included: a
/// MapNotify alone has it re-read only the keysyms, whose type indices then
/// point into the old keymap's types (`XkbKeycodeToKeysym` returns NoSymbol
/// for a key whose new type is past the old ones).
pub fn new_keyboard_notify(first_event: u8, details: u16, time: u32) -> Option<[u8; 32]> {
    if details & nkn::KEYCODES == 0 {
        return None;
    }
    let mut e = [0u8; 32];
    e[0] = first_event;
    e[1] = event::NEW_KEYBOARD_NOTIFY as u8;
    // [2..4] sequence, stamped when queued
    e[4..8].copy_from_slice(&time.to_le_bytes());
    e[8] = DEVICE_ID; // deviceID
    e[9] = DEVICE_ID; // oldDeviceID
    e[10] = MIN_KEYCODE;
    e[11] = MAX_KEYCODE;
    e[12] = MIN_KEYCODE; // old range
    e[13] = MAX_KEYCODE;
    // [14..16] requestMajor, requestMinor: no request caused it
    e[16..18].copy_from_slice(&nkn::KEYCODES.to_le_bytes());
    Some(e)
}

/// `XkbMapNotify` for a whole new keymap, restricted to the components the
/// client selected (`details`); `None` if it selected none. Xlib marks its
/// copy stale and re-reads what changed.
pub fn map_notify(first_event: u8, details: u16, n_types: u8, time: u32) -> Option<[u8; 32]> {
    let changed = details & part::ALL;
    if changed == 0 {
        return None;
    }
    let mut e = [0u8; 32];
    e[0] = first_event;
    e[1] = event::MAP_NOTIFY as u8;
    // [2..4] sequence, stamped when queued
    e[4..8].copy_from_slice(&time.to_le_bytes());
    e[8] = DEVICE_ID;
    e[9] = 0; // ptrBtnActions
    e[10..12].copy_from_slice(&changed.to_le_bytes());
    e[12] = MIN_KEYCODE;
    e[13] = MAX_KEYCODE;
    e[14] = 0; // firstType
    e[15] = n_types;
    // first/count for syms, acts, behaviors, explicit, modmap, vmodmap
    for i in 0..6 {
        e[16 + 2 * i] = MIN_KEYCODE;
        e[17 + 2 * i] = NUM_KEYS as u8;
    }
    e[28..30].copy_from_slice(&0xffffu16.to_le_bytes()); // virtualMods
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb_protocol::protocol::xkb;
    use x11rb_protocol::x11_utils::TryParse;

    fn us() -> Desc {
        crate::bridge::keymap::build(crate::bridge::xkbdesc::tests::MUTTER_US)
            .unwrap()
            .xkb
            .as_ref()
            .clone()
    }

    /// A request body (after the 4-byte header) as x11rb would send it.
    fn body<R: x11rb_protocol::x11_utils::Request>(r: R) -> (u8, Vec<u8>) {
        let (buf, _) = r.serialize(130);
        (buf[1], buf[4..].to_vec())
    }

    #[test]
    fn get_map_requests_parse() {
        let (minor, b) = body(xkb::GetMapRequest {
            device_spec: 0x100,
            full: xkb::MapPart::KEY_TYPES | xkb::MapPart::KEY_SYMS,
            partial: xkb::MapPart::MODIFIER_MAP,
            first_type: 1,
            n_types: 2,
            first_key_sym: 3,
            n_key_syms: 4,
            first_key_action: 5,
            n_key_actions: 6,
            first_key_behavior: 7,
            n_key_behaviors: 8,
            virtual_mods: 0x0102u16.into(),
            first_key_explicit: 9,
            n_key_explicit: 10,
            first_mod_map_key: 11,
            n_mod_map_keys: 12,
            first_v_mod_map_key: 13,
            n_v_mod_map_keys: 14,
        });
        assert_eq!(minor, op::GET_MAP);
        let Some(Request::GetMap(r)) = parse(minor, &b) else {
            panic!("not parsed");
        };
        assert_eq!(
            r,
            MapRequest {
                full: 3,
                partial: 4,
                first_type: 1,
                n_types: 2,
                first_key_sym: 3,
                n_key_syms: 4,
                first_key_act: 5,
                n_key_acts: 6,
                first_key_behavior: 7,
                n_key_behaviors: 8,
                virtual_mods: 0x0102,
                first_key_explicit: 9,
                n_key_explicit: 10,
                first_mod_map_key: 11,
                n_mod_map_keys: 12,
                first_vmod_map_key: 13,
                n_vmod_map_keys: 14,
            }
        );
        // a short body is a BadLength, not a guess
        assert_eq!(parse(op::GET_MAP, &b[..10]), None);
    }

    #[test]
    fn other_requests_parse() {
        let (m, b) = body(xkb::UseExtensionRequest {
            wanted_major: 1,
            wanted_minor: 0,
        });
        assert_eq!(
            parse(m, &b),
            Some(Request::UseExtension { major: 1, minor: 0 })
        );
        let (m, b) = body(xkb::GetNamesRequest {
            device_spec: 0x100,
            which: xkb::NameDetail::KEY_NAMES | xkb::NameDetail::SYMBOLS,
        });
        assert_eq!(
            parse(m, &b),
            Some(Request::GetNames {
                which: (1 << 9) | (1 << 2)
            })
        );
        let (m, b) = body(xkb::PerClientFlagsRequest {
            device_spec: 0x100,
            change: xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            value: xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            ctrls_to_change: 0u32.into(),
            auto_ctrls: 0u32.into(),
            auto_ctrls_values: 0u32.into(),
        });
        assert_eq!(
            parse(m, &b),
            Some(Request::PerClientFlags {
                change: 1,
                value: 1
            })
        );
        let (m, b) = body(xkb::GetCompatMapRequest {
            device_spec: 0x100,
            groups: xkb::SetOfGroup::GROUP1,
            get_all_si: true,
            first_si: 0,
            n_si: 0,
        });
        assert_eq!(
            parse(m, &b),
            Some(Request::GetCompatMap {
                groups: 1,
                all: true,
                first: 0,
                count: 0
            })
        );
        let (m, b) = body(xkb::GetIndicatorMapRequest {
            device_spec: 0x100,
            which: 0xffff_ffff,
        });
        assert_eq!(parse(m, &b), Some(Request::GetIndicatorMap { which: !0 }));
        let (m, b) = body(xkb::GetNamedIndicatorRequest {
            device_spec: 0x100,
            led_class: xkb::LedClass::DFLT_XI_CLASS,
            led_id: 0x500,
            indicator: 77,
        });
        assert_eq!(
            parse(m, &b),
            Some(Request::GetNamedIndicator { indicator: 77 })
        );
        let (m, b) = body(xkb::LatchLockStateRequest {
            device_spec: 0x100,
            affect_mod_locks: 0u8.into(),
            mod_locks: 0u8.into(),
            lock_group: true,
            group_lock: xkb::Group::M1,
            affect_mod_latches: 0u8.into(),
            latch_group: false,
            group_latch: 0,
        });
        assert_eq!(parse(m, &b), Some(Request::Ignored(op::LATCH_LOCK_STATE)));
        assert_eq!(
            parse(op::SET_MAP, &[0; 32]),
            Some(Request::Refused(op::SET_MAP))
        );
        assert_eq!(parse(77, &[]), Some(Request::Unknown(77)));
    }

    #[test]
    fn select_events_tracks_keymap_change_details() {
        let sel = |affect_which: u16,
                   clear: u16,
                   all: u16,
                   affect_map: u16,
                   map: u16,
                   nkn: Option<(u16, u16)>| {
            let details = xkb::SelectEventsAux {
                new_keyboard_notify: nkn.map(|(affect, details)| {
                    xkb::SelectEventsAuxNewKeyboardNotify {
                        affect_new_keyboard: affect.into(),
                        new_keyboard_details: details.into(),
                    }
                }),
                ..Default::default()
            };
            let (m, b) = body(xkb::SelectEventsRequest {
                device_spec: 0x100,
                clear: clear.into(),
                select_all: all.into(),
                affect_map: affect_map.into(),
                map: map.into(),
                details: std::borrow::Cow::Owned(details),
            });
            let mut b = b;
            b[2..4].copy_from_slice(&affect_which.to_le_bytes()); // x11rb derives it from details
            match parse(m, &b) {
                Some(Request::SelectEvents(s)) => s,
                other => panic!("{other:?}"),
            }
        };
        // Xlib's XkbUseExtension: NewKeyboardNotify, keycodes
        let s = sel(1, 0, 0, 0, 0, Some((1, 1)));
        assert_eq!((s.nkn.apply(0), s.map.apply(0x7)), (1, 0x7));
        // Xlib's _XkbLoadDpy: MapNotify, AllClientInfo
        let s = sel(2, 0, 0, 0x7, 0x7, None);
        assert_eq!((s.nkn.apply(1), s.map.apply(0)), (1, 0x7));
        assert_eq!(s.map.apply(0xf0), 0xf7);
        assert_eq!(sel(2, 0, 2, 0, 0, None).map.apply(0), part::ALL);
        assert_eq!(sel(2, 2, 0, 0, 0, None).map.apply(0x7), 0);
        assert_eq!(sel(1, 0, 1, 0, 0, None).nkn.apply(0), nkn::ALL);
        assert_eq!(sel(1, 1, 0, 0, 0, None).nkn.apply(1), 0);
        assert_eq!(sel(3, 0, 0, 0x1, 0x0, Some((1, 0))).map.apply(0x7), 0x6);
        assert_eq!(sel(3, 0, 0, 0x1, 0x0, Some((1, 0))).nkn.apply(1), 0);
    }

    fn parse_reply<T: TryParse>(buf: Vec<u8>) -> T {
        let buf = finish(buf);
        let (r, rest) = T::try_parse(&buf).expect("x11rb parses the reply");
        assert!(rest.is_empty(), "{} bytes left over", rest.len());
        r
    }

    #[test]
    fn get_map_reply_matches_x11rb() {
        let d = us();
        let req = MapRequest {
            full: part::ALL,
            ..Default::default()
        };
        let r: xkb::GetMapReply = parse_reply(get_map(&d, &req));
        assert_eq!((r.min_key_code, r.max_key_code), (8, 255));
        assert_eq!(usize::from(r.n_types), d.types.len());
        let types = r.map.types_rtrn.as_ref().unwrap();
        assert_eq!(types[1].num_levels, 2);
        assert_eq!(types[1].map[0].level, 1);
        let ctrl_alt = d.types.iter().position(|t| t.name == "CTRL+ALT").unwrap();
        assert!(types[ctrl_alt].has_preserve);
        assert_eq!(types[ctrl_alt].preserve.len(), types[ctrl_alt].map.len());
        let syms = r.map.syms_rtrn.as_ref().unwrap();
        assert_eq!(syms.len(), NUM_KEYS);
        let a = &syms[38 - 8]; // <AC01>
        assert_eq!((a.kt_index[0], a.group_info, a.width), (2, 1, 2));
        assert_eq!(a.syms, [0x61, 0x41]);
        assert!(syms[0].syms.is_empty()); // keycode 8: nothing
        let acts = r.map.key_actions.as_ref().unwrap();
        assert_eq!(acts.acts_rtrn_count[50 - 8], 1); // <LFSH>
        assert_eq!(acts.acts_rtrn_count[204 - 8], 2); // <ALT>: NoAction, SetMods
        let modmap = r.map.modmap_rtrn.as_ref().unwrap();
        assert!(
            modmap
                .iter()
                .any(|m| m.keycode == 50 && u16::from(m.mods) == 1)
        );
        let vmods = r.map.vmods_rtrn.as_ref().unwrap();
        assert_eq!(vmods.len(), 16);
        assert!(
            r.map
                .explicit_rtrn
                .as_ref()
                .unwrap()
                .iter()
                .any(|e| e.keycode == 38)
        );
        assert!(!r.map.vmodmap_rtrn.as_ref().unwrap().is_empty());
        assert!(r.map.behaviors_rtrn.as_ref().unwrap().is_empty());
    }

    #[test]
    fn partial_get_map_is_clamped() {
        let d = us();
        // types and syms of keys 250.. asked for past the end: served to the end
        let req = MapRequest {
            partial: part::KEY_TYPES | part::KEY_SYMS,
            first_type: 2,
            n_types: 200,
            first_key_sym: 250,
            n_key_syms: 20,
            ..Default::default()
        };
        let r: xkb::GetMapReply = parse_reply(get_map(&d, &req));
        assert_eq!(r.first_type, 2);
        assert_eq!(usize::from(r.n_types), d.types.len() - 2);
        assert_eq!((r.first_key_sym, r.n_key_syms), (250, 6));
        // and nothing at all asked for is a bare, valid reply
        let r: xkb::GetMapReply = parse_reply(get_map(&d, &MapRequest::default()));
        assert!(r.map.types_rtrn.is_none() && r.map.syms_rtrn.is_none());
    }

    #[test]
    fn get_names_reply_matches_x11rb() {
        let d = us();
        let mut atoms: Vec<String> = Vec::new();
        let mut intern = |s: &str| {
            let i = atoms.iter().position(|a| a == s).unwrap_or_else(|| {
                atoms.push(s.to_string());
                atoms.len() - 1
            });
            i as u32 + 100
        };
        let r: xkb::GetNamesReply = parse_reply(get_names(&d, 0x3fff, &mut intern));
        let v = &r.value_list;
        let name = |a: Option<u32>| atoms[(a.unwrap() - 100) as usize].clone();
        assert_eq!(name(v.symbols_name), "pc+us+inet(evdev)");
        assert_eq!(name(v.keycodes_name), "evdev+aliases(qwerty)");
        let types = v.type_names.as_ref().unwrap();
        assert_eq!(name(Some(types[2])), "ALPHABETIC");
        let levels = v.kt_level_names.as_ref().unwrap();
        assert_eq!(levels.n_levels_per_type[1], 2);
        assert_eq!(name(Some(levels.kt_level_names[1])), "Base");
        let keys = v.key_names.as_ref().unwrap();
        assert_eq!(&keys[38 - 8].name, b"AC01");
        assert_eq!(name(Some(v.groups.as_ref().unwrap()[0])), "English (US)");
        assert_eq!(
            name(Some(v.indicator_names.as_ref().unwrap()[0])),
            "Caps Lock"
        );
        assert!(!v.key_aliases.as_ref().unwrap().is_empty());
        let vm = v.virtual_mod_names.as_ref().unwrap();
        assert_eq!(name(Some(vm[0])), "NumLock");
    }

    #[test]
    fn small_replies_match_x11rb() {
        let d = us();
        let r: xkb::UseExtensionReply = parse_reply(use_extension());
        assert!(r.supported && r.server_major == 1);
        let r: xkb::GetStateReply = parse_reply(get_state());
        assert_eq!(r.device_id, DEVICE_ID);
        let r: xkb::GetControlsReply = parse_reply(get_controls(&d));
        assert_eq!(
            (r.repeat_delay, r.repeat_interval, r.num_groups),
            (660, 40, 1)
        );
        assert_ne!(r.per_key_repeat[38 / 8] & (1 << (38 % 8)), 0); // <AC01> repeats
        assert_eq!(r.per_key_repeat[50 / 8] & (1 << (50 % 8)), 0); // <LFSH> doesn't
        let r: xkb::GetCompatMapReply = parse_reply(get_compat_map(0xf, true, 0, 0));
        assert_eq!(
            (r.si_rtrn.len(), r.group_rtrn.len(), r.n_total_si),
            (1, 4, 1)
        );
        assert_eq!((r.si_rtrn[0].match_, r.si_rtrn[0].flags), (1, 1));
        let r: xkb::GetCompatMapReply = parse_reply(get_compat_map(0, false, 1, 5));
        assert_eq!((r.si_rtrn.len(), r.first_si_rtrn), (0, 1));
        let r: xkb::GetIndicatorMapReply = parse_reply(get_indicator_map(&d, !0));
        assert_eq!(r.maps.len(), 32);
        assert_eq!(u8::from(r.maps[0].which_mods), 1 << 2); // Caps Lock: locked Lock
        let r: xkb::GetIndicatorStateReply = parse_reply(get_indicator_state());
        assert_eq!(r.state, 0);
        let r: xkb::GetNamedIndicatorReply =
            parse_reply(get_named_indicator(&d, 5, Some("Num Lock")));
        assert!(r.found && r.supported);
        assert_eq!(r.ndx, 1);
        let r: xkb::PerClientFlagsReply = parse_reply(per_client_flags(1));
        assert_eq!(u32::from(r.value), 1);
        let r: xkb::ListComponentsReply = parse_reply(list_components());
        assert!(r.keymaps.is_empty());
        let r: xkb::GetDeviceInfoReply = parse_reply(get_device_info(0, 0));
        assert_eq!(r.name, b"xwlrvnc keyboard");
        let r: xkb::SetDebuggingFlagsReply = parse_reply(set_debugging_flags());
        assert_eq!(r.supported_flags, 0);
        let r: xkb::GetKbdByNameReply = parse_reply(get_kbd_by_name());
        assert!(!r.loaded && u16::from(r.reported) == 0);
        let buf = finish(get_geometry(9));
        assert_eq!(buf.len(), 32);
        assert_eq!(&buf[8..13], &[9, 0, 0, 0, 0]); // name, found=false
    }

    #[test]
    fn per_client_flags_accumulate() {
        let v = per_client_value(0, 1, 1);
        assert_eq!(v, 1);
        assert_eq!(per_client_value(v, 2, 2), 3);
        assert_eq!(per_client_value(3, 1, 0), 2);
        assert_eq!(per_client_value(0, !0, !0), PER_CLIENT_SUPPORTED);
    }

    #[test]
    fn new_keyboard_notify_matches_x11rb() {
        let e = new_keyboard_notify(95, 1, 99).unwrap();
        let (ev, _) = xkb::NewKeyboardNotifyEvent::try_parse(&e).unwrap();
        assert_eq!(u16::from(ev.changed), 1);
        assert_eq!(
            (ev.xkb_type, ev.device_id, ev.min_key_code, ev.max_key_code),
            (0, 3, 8, 255)
        );
        assert!(new_keyboard_notify(95, 0x6, 1).is_none());
    }

    #[test]
    fn map_notify_matches_x11rb() {
        let e = map_notify(95, 0x7, 12, 1234).unwrap();
        let (ev, _) = xkb::MapNotifyEvent::try_parse(&e).unwrap();
        assert_eq!(u16::from(ev.changed), 0x7);
        assert_eq!((ev.n_types, ev.first_key_sym, ev.n_key_syms), (12, 8, 248));
        assert_eq!(ev.time, 1234);
        assert!(map_notify(95, 0, 12, 1).is_none());
    }
}
