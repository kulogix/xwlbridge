//! The compositor keymap as an XKB keyboard description, for the built-in X
//! server's (read-only) XKEYBOARD extension.
//!
//! It describes the same keyboard the core `GetKeyboardMapping` serves: one
//! layout of the keymap, the one the compositor has locked (its current input
//! source), as the only XKB group. XTest keycodes go to the compositor, which
//! looks them up in whatever group *it* is in, and the bridge cannot change
//! that group, so the X keyboard follows it instead. Keycodes are the keymap's
//! own (evdev + 8), cut to X's 8..=255.
//!
//! libxkbcommon keeps key types, actions and the modifier map private, so the
//! description is assembled from what it does expose (each key's levels and the
//! modifiers that select them, its keysyms, the virtual modifier mapping) plus
//! the keymap text itself for the parts that only have names there (type and
//! level names, declared virtual modifiers, key aliases, component names).

use xkbcommon_rs::xkb_state::{KeyDirection, StateComponent};
use xkbcommon_rs::{Keymap, State};

use crate::bridge::keymap::{MAX_KEYCODE, MIN_KEYCODE};

/// Keys in the X keycode range.
pub const NUM_KEYS: usize = (MAX_KEYCODE - MIN_KEYCODE) as usize + 1;
/// XKB has 16 virtual modifiers.
pub const MAX_VMODS: usize = 16;
/// Key types a description can carry: `GetMap` counts them in a CARD8, and a
/// key's type index has to be below the count.
pub const MAX_TYPES: usize = 255;

/// One `map[...] = Level` entry of a key type. `mods` is the effective real
/// mask (`real` plus whatever the virtual modifiers are bound to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub mods: u8,
    pub real: u8,
    pub vmods: u16,
    /// 0-based.
    pub level: u8,
    /// `preserve[...]`: modifiers the entry leaves unconsumed (effective
    /// mask, real, virtual).
    pub preserve: (u8, u8, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyType {
    pub name: String,
    /// Effective real mask of `modifiers=`, and its declared real and virtual parts.
    pub mods: u8,
    pub real: u8,
    pub vmods: u16,
    pub num_levels: u8,
    pub entries: Vec<Entry>,
    /// One per level ("" for none).
    pub level_names: Vec<String>,
}

impl KeyType {
    /// What the type does, as a key sees it: the level count and the
    /// (effective mask, level) pairs that can select a level, without the
    /// implicit "no modifiers is level 1". Two types with the same signature
    /// pick the same keysyms in every modifier state.
    fn signature(&self) -> (u8, Vec<(u8, u8)>) {
        signature(
            self.num_levels,
            self.entries.iter().map(|e| (e.mods, e.level)),
        )
    }
}

fn signature(levels: u8, entries: impl Iterator<Item = (u8, u8)>) -> (u8, Vec<(u8, u8)>) {
    let mut v: Vec<(u8, u8)> = entries.filter(|&(m, _)| m != 0).collect();
    v.sort_unstable();
    v.dedup_by_key(|e| e.0);
    (levels, v)
}

/// The XKB action a key carries. Only modifier keys get one (what the
/// keymap's compat interprets give them), so that a description written out
/// with `xkbcomp -xkb` and loaded elsewhere still has working modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// SetMods(modifiers=modMapMods, clearLocks)
    SetMods,
    /// LockMods(modifiers=modMapMods)
    LockMods,
    /// SetMods(modifiers=..., clearLocks) for a key outside the modifier map
    /// that sets modifiers all the same, as `<RALT>` (ISO_Level3_Shift) sets
    /// LevelThree on layouts with AltGr: the effective real modifiers
    /// (`mask`), and the real and virtual ones the action names.
    SetModsMask { mask: u8, real: u8, vmods: u16 },
    /// LockMods(modifiers=...) likewise.
    LockModsMask { mask: u8, real: u8, vmods: u16 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Key {
    /// The XKB key name (`<AE01>`), NUL-padded.
    pub name: [u8; 4],
    /// Index into [`Desc::types`] of group 1's type; meaningless without syms.
    pub kt: u8,
    /// Group 1's keysyms, one per level of its type; empty: no symbols at all.
    pub syms: Vec<u32>,
    /// The key's real modifiers (the keymap's `modifier_map`).
    pub modmap: u8,
    /// Virtual modifiers bound through this key.
    pub vmodmap: u16,
    pub action: Option<Action>,
    pub repeats: bool,
}

/// The `_XKB_RULES_NAMES` an X server publishes on its root window, for
/// `setxkbmap -query`/`-print`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rmlvo {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

impl Rmlvo {
    /// The property value: the five names, each NUL-terminated.
    pub fn property(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for s in [
            &self.rules,
            &self.model,
            &self.layout,
            &self.variant,
            &self.options,
        ] {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desc {
    pub types: Vec<KeyType>,
    /// Indexed by keycode - [`MIN_KEYCODE`].
    pub keys: Vec<Key>,
    /// Virtual modifier names and the real modifiers each is bound to, in XKB
    /// order (the keymap's declaration order).
    pub vmods: Vec<(String, u8)>,
    pub group_name: String,
    /// LED index (0-based) and name.
    pub indicators: Vec<(u8, String)>,
    /// (real, alias) key names.
    pub aliases: Vec<([u8; 4], [u8; 4])>,
    pub keycodes_name: String,
    pub types_name: String,
    pub compat_name: String,
    pub symbols_name: String,
    pub geometry_name: String,
    pub rules: Option<Rmlvo>,
}

/// Real modifier bits.
pub const SHIFT: u8 = 1;
pub const LOCK: u8 = 2;
const REAL_MODS: [&str; 8] = [
    "shift", "lock", "control", "mod1", "mod2", "mod3", "mod4", "mod5",
];

impl Desc {
    /// The description served before the compositor keymap is known: no
    /// keysyms (as `GetKeyboardMapping` then reports), the required types, and
    /// the given core modifier map (8 rows of `modmap.len() / 8` keycodes).
    pub fn without_keymap(modmap: &[u8]) -> Desc {
        let mut keys = vec![Key::default(); NUM_KEYS];
        apply_core_modmap(&mut keys, modmap);
        Desc {
            types: required_types(&[], &[]),
            keys,
            vmods: Vec::new(),
            group_name: String::new(),
            indicators: Vec::new(),
            aliases: Vec::new(),
            keycodes_name: String::new(),
            types_name: String::new(),
            compat_name: String::new(),
            symbols_name: String::new(),
            geometry_name: String::new(),
            rules: None,
        }
    }

    pub fn key(&self, keycode: u8) -> &Key {
        &self.keys[usize::from(keycode.max(MIN_KEYCODE) - MIN_KEYCODE)]
    }

    /// The action on each level of a key that has one: on the levels that
    /// have a keysym (`<ALT>` is `[NoSymbol, Alt_L]`, and only Alt_L acts).
    pub fn level_action(key: &Key, level: usize) -> Option<Action> {
        key.action
            .filter(|_| key.syms.get(level).is_some_and(|&s| s != 0))
    }

    /// Number of keysyms (= actions, when it has any) a key has: its type's
    /// width (the levels its keysyms were read for) times its one group.
    pub fn width(key: &Key) -> u8 {
        key.syms.len().min(255) as u8
    }
}

/// Sets each key's real modifier map from a core `GetModifierMapping` table.
fn apply_core_modmap(keys: &mut [Key], modmap: &[u8]) {
    let per = modmap.len() / 8;
    if per == 0 {
        return;
    }
    for (bit, row) in modmap.chunks(per).enumerate().take(8) {
        for &kc in row {
            if kc >= MIN_KEYCODE {
                keys[usize::from(kc - MIN_KEYCODE)].modmap |= 1 << bit;
            }
        }
    }
}

/// Builds the description. `modmap` is the core modifier map (as
/// `GetModifierMapping` serves it), used when the keymap text has no
/// `modifier_map` of its own. The text's is the exact one: the core map is
/// worked out by pressing keys, which misses keys whose first level has no
/// symbol (`<ALT>`, `<META>`, `<SUPR>`), as Xorg would list them. `group` is
/// the keymap layout (0-based) to describe, as the description's one group.
pub fn build(keymap: &Keymap, text: &str, modmap: &[u8], group: usize) -> Desc {
    // virtual modifiers: libxkbcommon numbers them after the 8 real ones, in
    // declaration order, as X does; what each is bound to is what setting it
    // alone makes effective
    let mut state = State::new(keymap.clone());
    let mut vmods = Vec::new();
    for idx in 8..keymap.num_mods().min(8 + MAX_VMODS) {
        let name = keymap.mod_get_name(idx).unwrap_or("").to_string();
        state.update_mask(1 << idx, 0, 0, 0, 0, 0);
        let real = (state.serialize_mods(StateComponent::MODS_EFFECTIVE) & 0xff) as u8;
        vmods.push((name, real));
    }
    state.update_mask(0, 0, 0, 0, 0, 0);

    let parsed = parse_types(section(text, "xkb_types").unwrap_or(""), &vmods);
    let mut types = required_types(&parsed, &vmods);
    for t in parsed {
        if types.len() < MAX_TYPES && !types.iter().any(|have| have.name == t.name) {
            types.push(t);
        }
    }

    // which of a key's layouts `group` selects (keys with fewer layouts wrap
    // or clamp into range, as the compositor resolves them)
    let mut in_group = State::new(keymap.clone());
    in_group.update_mask(0, 0, 0, 0, 0, group);

    let mut keys = vec![Key::default(); NUM_KEYS];
    let symbols = section(text, "xkb_symbols").unwrap_or("");
    if !apply_text_modmap(&mut keys, keymap, symbols) {
        apply_core_modmap(&mut keys, modmap);
    }
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        let key = &mut keys[usize::from(kc - MIN_KEYCODE)];
        let code = u32::from(kc);
        if let Some(name) = keymap.key_get_name(code) {
            key.name = key_name(name);
        }
        key.repeats = keymap.key_repeats(code);
        if keymap.num_layouts_for_key(code).unwrap_or(0) == 0 {
            continue;
        }
        let Some(layout) = in_group.key_get_layout(code) else {
            continue;
        };
        let levels = keymap.num_levels_for_key(code, layout).clamp(1, 255) as u8;
        let sig = signature(
            levels,
            (0..levels).flat_map(|level| {
                keymap
                    .key_get_mods_for_level(code, layout, usize::from(level), 64)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |m| ((m & 0xff) as u8, level))
            }),
        );
        // a shape no type has and no room for another (MAX_TYPES of them,
        // which no real keymap comes near): the key keeps its first level
        // only, as ONE_LEVEL (index 0), so no key points past the types
        let (kt, levels) = match type_for(&mut types, &sig) {
            Some(i) => (i, levels),
            None => (0, 1),
        };
        key.kt = kt as u8;
        key.syms = (0..levels)
            .map(|level| {
                keymap
                    .key_get_syms_by_level(code, layout, usize::from(level))
                    .ok()
                    .and_then(|s| s.first().map(|s| s.raw()))
                    .unwrap_or(0)
            })
            .collect();
    }

    // actions and the virtual modifier map, as the usual compat interprets
    // assign them: modifier keys set (or lock) the modifiers they map, and a
    // virtual modifier is bound through the keys whose keysym names it. A key
    // that sets or locks modifiers but maps none gets those, as pressing it
    // in the keymap does: `<RALT>` as ISO_Level3_Shift sets LevelThree (Mod5)
    // on GNOME's "de". One that maps other modifiers than it sets is a key
    // of a later group whose modifier map was made for group 1 (`<RALT>` in
    // group 2 of "us,de" maps group 1's Alt_R Mod1): the one group served
    // maps what it does here. Readers that work out how to reach a level
    // from the modifier map and actions (x11vnc's XKB mode) need both to
    // find AltGr.
    // a libxkbcommon modifier mask (real ones, then the virtual ones by
    // index) as (effective real mask, real ones, virtual ones)
    let resolve = |mods: u32| -> (u8, u8, u16) {
        let real = (mods & 0xff) as u8;
        let virt = ((mods >> 8) & 0xffff) as u16;
        let mask = (0..vmods.len())
            .filter(|i| virt & (1 << i) != 0)
            .fold(real, |m, i| m | vmods[i].1);
        (mask, real, virt)
    };
    let mut press = State::new(keymap.clone());
    press.update_mask(0, 0, 0, 0, 0, group);
    for (i, key) in keys.iter_mut().enumerate() {
        let Some(&sym) = key.syms.iter().find(|&&s| s != 0) else {
            continue;
        };
        let code = i as u32 + u32::from(MIN_KEYCODE);
        press.update_key(code, KeyDirection::Down);
        let depressed = resolve(press.serialize_mods(StateComponent::MODS_DEPRESSED));
        let locked = resolve(press.serialize_mods(StateComponent::MODS_LOCKED));
        press.update_key(code, KeyDirection::Up);
        press.update_mask(0, 0, 0, 0, 0, group);
        let mapped = if LOCKING_KEYSYMS.contains(&sym) {
            Action::LockMods
        } else {
            Action::SetMods
        };
        let as_mapped = match mapped {
            Action::LockMods => locked.0 == key.modmap,
            _ => depressed.0 == key.modmap,
        };
        key.action = if key.modmap != 0 && (as_mapped || (depressed.0 | locked.0) == 0) {
            Some(mapped)
        } else if key.modmap != 0 {
            key.modmap = depressed.0 | locked.0;
            Some(if depressed.0 != 0 {
                Action::SetMods
            } else {
                Action::LockMods
            })
        } else if depressed.0 != 0 {
            let (mask, real, vmods) = depressed;
            Some(Action::SetModsMask { mask, real, vmods })
        } else if locked.0 != 0 {
            let (mask, real, vmods) = locked;
            Some(Action::LockModsMask { mask, real, vmods })
        } else {
            None
        };
    }
    for (i, (name, real)) in vmods.iter().enumerate() {
        if *real == 0 {
            continue;
        }
        let syms = vmod_keysyms(name);
        let binds = |k: &Key| {
            k.modmap != 0
                && k.modmap & !real == 0
                && match syms {
                    Some(syms) => k.syms.iter().any(|s| syms.contains(s)),
                    // a name we don't know: the keys mapping exactly its modifiers
                    None => k.modmap == *real,
                }
        };
        for key in keys.iter_mut().filter(|k| binds(k)) {
            key.vmodmap |= 1 << i;
        }
    }

    let indicators = (0..32u8)
        .filter_map(|i| {
            keymap
                .led_get_name(usize::from(i))
                .map(|n| (i, n.to_string()))
        })
        .collect();
    // the served keyboard is one group, so its name is too: GNOME's
    // "pc+de+us:2+inet(evdev)" would otherwise name a second group that a
    // dump (`xkbcomp -xkb`) carries no symbols for
    let symbols_name = component_name(text, "xkb_symbols")
        .map(|n| one_layout(&n, group as u32))
        .unwrap_or_default();
    let group_name = keymap.layout_get_name(group).unwrap_or("").to_string();
    // a keymap whose sections carry no names (libxkbcommon 1.7's, as labwc
    // serves it on Raspberry Pi OS) still names its layout ("English (UK)"):
    // xkeyboard-config's list says which layout that is
    let rules = rmlvo(&symbols_name).or_else(|| rmlvo_from_description(&group_name));
    Desc {
        types,
        keys,
        group_name,
        indicators,
        aliases: parse_aliases(section(text, "xkb_keycodes").unwrap_or("")),
        keycodes_name: component_name(text, "xkb_keycodes").unwrap_or_default(),
        types_name: component_name(text, "xkb_types").unwrap_or_default(),
        compat_name: component_name(text, "xkb_compatibility").unwrap_or_default(),
        geometry_name: "pc(pc105)".to_string(),
        rules,
        symbols_name,
        vmods,
    }
}

/// Sets each key's real modifiers from the keymap's `modifier_map NAME {
/// <KEY>, ... };` lines. False if there are none (or a key it names is
/// unknown), leaving the keys as they were.
fn apply_text_modmap(keys: &mut [Key], keymap: &Keymap, symbols: &str) -> bool {
    let mut found = false;
    let mut mods = vec![0u8; keys.len()];
    for line in symbols.lines() {
        let Some(rest) = line.trim().strip_prefix("modifier_map") else {
            continue;
        };
        let Some((name, list)) = rest.split_once('{') else {
            return false;
        };
        let lower = name.trim().to_ascii_lowercase();
        let Some(bit) = REAL_MODS.iter().position(|m| *m == lower) else {
            return false;
        };
        for entry in list
            .split(['}', ','])
            .map(str::trim)
            .filter(|e| !e.is_empty() && *e != ";")
        {
            // a keysym rather than a key: libxkbcommon writes keys, so a keymap
            // with these is one we'd better not second-guess
            let Some(name) = entry.strip_prefix('<').and_then(|e| e.strip_suffix('>')) else {
                return false;
            };
            let Some(kc) = keymap.key_by_name(name) else {
                return false;
            };
            if let Some(m) = usize::try_from(kc.0)
                .ok()
                .and_then(|kc| kc.checked_sub(usize::from(MIN_KEYCODE)))
                .and_then(|i| mods.get_mut(i))
            {
                *m |= 1 << bit;
            }
        }
        found = true;
    }
    if found {
        for (key, m) in keys.iter_mut().zip(mods) {
            key.modmap = m;
        }
    }
    found
}

/// Lock-style modifier keys (LockMods rather than SetMods).
const LOCKING_KEYSYMS: [u32; 3] = [
    0xffe5, // Caps_Lock
    0xffe6, // Shift_Lock
    0xff7f, // Num_Lock
];

/// The keysyms the standard compat interprets bind each well-known virtual
/// modifier through.
fn vmod_keysyms(name: &str) -> Option<&'static [u32]> {
    Some(match name {
        "NumLock" => &[0xff7f],
        "Alt" => &[0xffe9, 0xffea],
        "Meta" => &[0xffe7, 0xffe8],
        "Super" => &[0xffeb, 0xffec],
        "Hyper" => &[0xffed, 0xffee],
        "LevelThree" => &[0xfe03, 0xfe04, 0xfe05],
        "LevelFive" => &[0xfe11, 0xfe12, 0xfe13],
        "ScrollLock" => &[0xff14],
        "AltGr" => &[0xff7e],
        _ => return None,
    })
}

fn key_name(name: &str) -> [u8; 4] {
    let mut out = [0u8; 4];
    for (o, b) in out.iter_mut().zip(name.bytes()) {
        *o = b;
    }
    out
}

/// The index of the type with this signature, adding a synthesized one if no
/// type has it; `None` when there is none and [`MAX_TYPES`] are already there.
fn type_for(types: &mut Vec<KeyType>, sig: &(u8, Vec<(u8, u8)>)) -> Option<usize> {
    if let Some(i) = types.iter().position(|t| t.signature() == *sig) {
        return Some(i);
    }
    if types.len() >= MAX_TYPES {
        return None;
    }
    types.push(synthesized_type(types.len(), sig));
    Some(types.len() - 1)
}

/// A type for a key shape no named type has.
fn synthesized_type(index: usize, sig: &(u8, Vec<(u8, u8)>)) -> KeyType {
    let (levels, entries) = sig;
    KeyType {
        name: format!("XWLBRIDGE_TYPE{index}"),
        mods: entries.iter().fold(0, |m, e| m | e.0),
        real: entries.iter().fold(0, |m, e| m | e.0),
        vmods: 0,
        num_levels: *levels,
        entries: entries
            .iter()
            .map(|&(mods, level)| Entry {
                mods,
                real: mods,
                vmods: 0,
                level,
                preserve: (0, 0, 0),
            })
            .collect(),
        level_names: vec![String::new(); usize::from(*levels)],
    }
}

/// The four types XKB requires at indices 0..4, from the keymap's own when it
/// has them, else their standard definitions.
fn required_types(parsed: &[KeyType], vmods: &[(String, u8)]) -> Vec<KeyType> {
    let numlock = vmods.iter().position(|(n, _)| n == "NumLock");
    let (num_real, num_vmod) = match numlock {
        Some(i) => (vmods[i].1, 1u16 << i),
        None => (0, 0),
    };
    let entry = |real: u8, vmods: u16, mods: u8, level: u8| Entry {
        mods,
        real,
        vmods,
        level,
        preserve: (0, 0, 0),
    };
    let names = |n: &[&str]| n.iter().map(|s| (*s).to_string()).collect();
    let standard = [
        KeyType {
            name: "ONE_LEVEL".into(),
            mods: 0,
            real: 0,
            vmods: 0,
            num_levels: 1,
            entries: vec![],
            level_names: names(&["Any"]),
        },
        KeyType {
            name: "TWO_LEVEL".into(),
            mods: SHIFT,
            real: SHIFT,
            vmods: 0,
            num_levels: 2,
            entries: vec![entry(SHIFT, 0, SHIFT, 1)],
            level_names: names(&["Base", "Shift"]),
        },
        KeyType {
            name: "ALPHABETIC".into(),
            mods: SHIFT | LOCK,
            real: SHIFT | LOCK,
            vmods: 0,
            num_levels: 2,
            entries: vec![entry(SHIFT, 0, SHIFT, 1), entry(LOCK, 0, LOCK, 1)],
            level_names: names(&["Base", "Caps"]),
        },
        KeyType {
            name: "KEYPAD".into(),
            mods: SHIFT | num_real,
            real: SHIFT,
            vmods: num_vmod,
            num_levels: 2,
            entries: [entry(SHIFT, 0, SHIFT, 1), entry(0, num_vmod, num_real, 1)]
                .into_iter()
                .filter(|e| e.mods != 0)
                .collect(),
            level_names: names(&["Base", "Number"]),
        },
    ];
    standard
        .into_iter()
        .map(|std| {
            parsed
                .iter()
                .find(|p| p.name == std.name)
                .cloned()
                .unwrap_or(std)
        })
        .collect()
}

/// The body of a top-level keymap section (`xkb_types "..." { body };`).
fn section<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let start = text.find(keyword)?;
    let open = start + text[start..].find('{')?;
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Resolves a modifier expression (`Shift+NumLock`, `none`) to its declared
/// real and virtual parts. `None` if it names a modifier we don't know.
fn parse_mods(expr: &str, vmods: &[(String, u8)]) -> Option<(u8, u16)> {
    let (mut real, mut virt) = (0u8, 0u16);
    for tok in expr.split('+').map(str::trim) {
        let lower = tok.to_ascii_lowercase();
        if lower == "none" || lower.is_empty() {
            continue;
        }
        if lower == "all" {
            real = 0xff;
            continue;
        }
        if let Some(bit) = REAL_MODS.iter().position(|m| *m == lower) {
            real |= 1 << bit;
        } else if lower == "ctrl" {
            real |= 1 << 2;
        } else {
            let i = vmods.iter().position(|(n, _)| n == tok)?;
            virt |= 1 << i;
        }
    }
    Some((real, virt))
}

fn effective(real: u8, virt: u16, vmods: &[(String, u8)]) -> u8 {
    vmods
        .iter()
        .enumerate()
        .filter(|(i, _)| virt & (1 << i) != 0)
        .fold(real, |m, (_, (_, r))| m | r)
}

/// `Level3` or `3` → 2 (0-based).
fn parse_level(s: &str) -> Option<u8> {
    let s = s.trim();
    let digits = s
        .strip_prefix("Level")
        .or_else(|| s.strip_prefix("level"))
        .unwrap_or(s);
    let n: u8 = digits.trim().parse().ok()?;
    n.checked_sub(1)
}

/// The `type "NAME" { ... };` blocks of an `xkb_types` body. Types using a
/// modifier the keymap doesn't declare are skipped (a key using one gets a
/// synthesized type instead).
fn parse_types(body: &str, vmods: &[(String, u8)]) -> Vec<KeyType> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("type \"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let name = rest[..end].to_string();
        let Some(open) = rest.find('{') else { break };
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let block = &rest[open + 1..open + close];
        rest = &rest[open + close..];
        if let Some(t) = parse_type(name, block, vmods) {
            out.push(t);
        }
    }
    out
}

fn parse_type(name: String, block: &str, vmods: &[(String, u8)]) -> Option<KeyType> {
    let (mut real, mut virt) = (0u8, 0u16);
    let mut entries = Vec::new();
    let mut level_names: Vec<(u8, String)> = Vec::new();
    let mut max_level = 0u8;
    for stmt in block.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (lhs, rhs) = stmt.split_once('=')?;
        let lhs = lhs.trim();
        let rhs = rhs.trim();
        let index = || {
            let i = lhs.find('[')?;
            let j = lhs.rfind(']')?;
            Some(&lhs[i + 1..j])
        };
        if lhs.eq_ignore_ascii_case("modifiers") {
            (real, virt) = parse_mods(rhs, vmods)?;
        } else if lhs.starts_with("map[") {
            let (r, v) = parse_mods(index()?, vmods)?;
            let level = parse_level(rhs)?;
            max_level = max_level.max(level);
            entries.push(Entry {
                mods: effective(r, v, vmods),
                real: r,
                vmods: v,
                level,
                preserve: (0, 0, 0),
            });
        } else if lhs.starts_with("level_name[") {
            let level = parse_level(index()?)?;
            max_level = max_level.max(level);
            level_names.push((level, rhs.trim_matches('"').to_string()));
        } else if lhs.starts_with("preserve[") {
            // for the map entry of the same modifiers, written before it
            let (r, v) = parse_mods(index()?, vmods)?;
            let (pr, pv) = parse_mods(rhs, vmods)?;
            if let Some(e) = entries.iter_mut().find(|e| e.real == r && e.vmods == v) {
                e.preserve = (effective(pr, pv, vmods), pr, pv);
            }
        }
    }
    // an entry whose virtual modifiers are all unbound can never match (X
    // marks it inactive); one selecting level 1 says nothing
    entries.retain(|e| e.mods != 0);
    let num_levels = max_level.checked_add(1)?;
    let mut names = vec![String::new(); usize::from(num_levels)];
    for (level, n) in level_names {
        names[usize::from(level)] = n;
    }
    Some(KeyType {
        name,
        mods: effective(real, virt, vmods),
        real,
        vmods: virt,
        num_levels,
        entries,
        level_names: names,
    })
}

/// `alias <AB01> = <LatZ>;` lines of the keycodes section, as (real, alias).
fn parse_aliases(body: &str) -> Vec<([u8; 4], [u8; 4])> {
    body.lines()
        .filter_map(|l| {
            let l = l.trim().strip_prefix("alias")?;
            let (alias, real) = l.split_once('=')?;
            let strip = |s: &str| {
                let s = s.trim().trim_end_matches(';').trim();
                s.strip_prefix('<')?.strip_suffix('>').map(key_name)
            };
            Some((strip(real)?, strip(alias)?))
        })
        .take(255)
        .collect()
}

/// The component a section was compiled from, as setxkbmap would name it
/// ("evdev+aliases(qwerty)"), or `None` if the keymap doesn't say.
/// libxkbcommon writes "pc+us+us:2+inet(evdev)" as "pc_us_us_2_inet(evdev)":
/// only top-level separators are rewritten (variant names keep their
/// underscores), and a part that is just a number was a group index. Older
/// versions (mutter 46's) write "(unnamed)", which names nothing.
pub fn component_name(text: &str, section: &str) -> Option<String> {
    let line = text.lines().find(|l| l.trim_start().starts_with(section))?;
    let quoted = line.split('"').nth(1)?;
    if quoted.is_empty() || quoted.starts_with('(') {
        return None;
    }
    let mut parts: Vec<String> = vec![String::new()];
    let mut depth = 0i32;
    for ch in quoted.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            '_' | '+' if depth == 0 => {
                parts.push(String::new());
                continue;
            }
            _ => {}
        }
        parts.last_mut().expect("never empty").push(ch);
    }
    let mut out: Vec<String> = Vec::with_capacity(parts.len());
    for part in parts {
        match out.last_mut() {
            Some(prev) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
                prev.push(':');
                prev.push_str(&part);
            }
            _ => out.push(part),
        }
    }
    Some(out.join("+"))
}

/// The symbols of keymap components `symbols` ("pc+us+de:2+us:3+inet(evdev)")
/// with layout `group` (from 0) as the only one: "pc+de+inet(evdev)". GNOME
/// puts up to three input sources in one keymap and switches between them by
/// the group, which X clients don't follow (and an X client picking a key from
/// another group would get the active one's symbol on the desktop), so the X
/// keyboard is the active layout alone. A part numbered for a layout (":N")
/// goes with it; the first layout's has no number (it follows the model's
/// "pc"); the rest (options, "inet(evdev)") apply to every layout. A group
/// past the last layout wraps, as xkb's does.
pub fn one_layout(symbols: &str, group: u32) -> String {
    let parts: Vec<&str> = symbols.split('+').collect();
    let first = usize::from(parts.first() == Some(&"pc"));
    let layout_of = |i: usize, p: &str| -> Option<(u32, String)> {
        match p.rsplit_once(':') {
            Some((name, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                Some((n.parse().ok()?, name.to_string()))
            }
            _ if i == first => Some((1, p.to_string())),
            _ => None,
        }
    };
    let count = parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| layout_of(i, p).map(|(n, _)| n))
        .max()
        .unwrap_or(1);
    let wanted = group % count + 1;
    parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match layout_of(i, p) {
            Some((n, name)) if n == wanted => Some(name),
            Some(_) => None,
            None => Some((*p).to_string()),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// The rules names behind a symbols component ("pc+de(nodeadkeys)+inet(evdev)"
/// → evdev / pc105 / de / nodeadkeys). Only the first layout, as the name
/// served is one group's; options can't be told apart from layouts reliably, so they
/// are left out, and the model isn't recorded at all, so it is the usual one.
fn rmlvo(symbols: &str) -> Option<Rmlvo> {
    let mut parts = symbols.split('+').filter(|p| !p.is_empty());
    let mut first = parts.next()?;
    if first == "pc" || first.starts_with("pc(") {
        first = parts.next()?;
    }
    let first = first.split(':').next().unwrap_or(first);
    let (layout, variant) = match first.split_once('(') {
        Some((l, v)) => (l, v.trim_end_matches(')')),
        None => (first, ""),
    };
    if layout.is_empty() || layout.starts_with("inet") {
        return None;
    }
    Some(Rmlvo {
        rules: "evdev".into(),
        model: "pc105".into(),
        layout: layout.into(),
        variant: variant.into(),
        options: String::new(),
    })
}

/// The rules names of the layout (or layout variant) that xkeyboard-config
/// describes as `description` ("English (UK)" → gb; "English (UK, extended,
/// with Win keys)" → gb / extd), from its list of the evdev rules (base's if
/// that is missing), read once.
fn rmlvo_from_description(description: &str) -> Option<Rmlvo> {
    static LIST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| {
        let root = std::env::var_os("XKB_CONFIG_ROOT")
            .filter(|r| !r.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "/usr/share/X11/xkb".into());
        ["evdev.lst", "base.lst"]
            .iter()
            .find_map(|f| std::fs::read_to_string(root.join("rules").join(f)).ok())
    });
    let (layout, variant) = layout_described(list.as_deref()?, description)?;
    Some(Rmlvo {
        rules: "evdev".into(),
        model: "pc105".into(),
        layout,
        variant,
        options: String::new(),
    })
}

/// In an xkeyboard-config rules list (`evdev.lst`: "! layout" lines
/// "  gb  English (UK)", "! variant" lines "  extd  gb: English (UK, ...)"),
/// the layout and variant described as `description`.
fn layout_described(list: &str, description: &str) -> Option<(String, String)> {
    let description = description.trim();
    if description.is_empty() {
        return None;
    }
    let mut section = "";
    let mut variant = None;
    for line in list.lines() {
        if let Some(name) = line.strip_prefix('!') {
            section = name.trim();
            continue;
        }
        let Some((name, rest)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim();
        match section {
            "layout" if rest == description => return Some((name.to_string(), String::new())),
            "variant" if variant.is_none() => {
                if let Some((layout, desc)) = rest.split_once(": ")
                    && desc.trim() == description
                {
                    variant = Some((layout.trim().to_string(), name.to_string()));
                }
            }
            _ => {}
        }
    }
    variant
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// mutter 50.1's keymap (Ubuntu 26.04, libxkbcommon 1.13, "us").
    pub(crate) const MUTTER_US: &str = include_str!("testdata/mutter50_us.xkb");
    /// Two layouts, as GNOME's keymap for input sources "us" and "de":
    /// libxkbcommon 1.13's `xkb_keymap_new_from_names(evdev, pc105, "us,de")`
    /// written out with `xkb_keymap_get_as_string`, which is how mutter makes
    /// the text it sends.
    const US_DE: &str = include_str!("testdata/xkbcommon113_us_de.xkb");

    pub(crate) fn us() -> Desc {
        crate::bridge::keymap::build(MUTTER_US)
            .unwrap()
            .xkb
            .as_ref()
            .clone()
    }

    fn kc(d: &Desc, name: &str) -> u8 {
        let n = key_name(name);
        let i = d.keys.iter().position(|k| k.name == n).unwrap();
        i as u8 + MIN_KEYCODE
    }

    fn type_of<'a>(d: &'a Desc, name: &str) -> &'a KeyType {
        &d.types[usize::from(d.key(kc(d, name)).kt)]
    }

    #[test]
    fn required_types_come_first() {
        let d = us();
        let names: Vec<_> = d.types.iter().take(4).map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC", "KEYPAD"]);
        let empty =
            Desc::without_keymap(&[50, 62, 66, 0, 37, 105, 64, 108, 77, 0, 0, 0, 133, 134, 0, 0]);
        assert_eq!(empty.types.len(), 4);
        assert!(empty.keys.iter().all(|k| k.syms.is_empty()));
        assert_eq!(empty.key(50).modmap, SHIFT);
        assert_eq!(empty.key(66).modmap, LOCK);
    }

    #[test]
    fn keys_get_their_named_types_and_symbols() {
        let d = us();
        assert_eq!(type_of(&d, "AE01").name, "TWO_LEVEL");
        assert_eq!(d.key(10).syms, [0x31, 0x21]); // 1 !
        assert_eq!(type_of(&d, "AC01").name, "ALPHABETIC");
        assert_eq!(d.key(38).syms, [0x61, 0x41]); // a A
        assert_eq!(type_of(&d, "ESC").name, "ONE_LEVEL");
        assert_eq!(type_of(&d, "FK01").name, "CTRL+ALT");
        assert_eq!(type_of(&d, "FK01").num_levels, 5);
        assert_eq!(d.key(kc(&d, "FK01")).syms[4], 0x1008fe01); // XF86Switch_VT_1
        assert_eq!(type_of(&d, "KP7").name, "KEYPAD");
        // nothing had to be made up for this keymap
        assert!(!d.types.iter().any(|t| t.name.starts_with("XWLBRIDGE")));
    }

    #[test]
    fn types_resolve_virtual_modifiers() {
        let d = us();
        let numlock = d.vmods.iter().position(|(n, _)| n == "NumLock").unwrap();
        assert_eq!(d.vmods[numlock].1, 1 << 4); // Mod2
        let keypad = &d.types[3];
        assert_eq!(keypad.vmods, 1 << numlock);
        assert_eq!(keypad.real, SHIFT);
        assert_eq!(keypad.mods, SHIFT | 1 << 4);
        let ctrl_alt = d.types.iter().find(|t| t.name == "CTRL+ALT").unwrap();
        let control = ctrl_alt.entries.iter().find(|e| e.real == 1 << 2).unwrap();
        assert_eq!((control.level, control.preserve), (0, (1 << 2, 1 << 2, 0)));
        let four = d.types.iter().find(|t| t.name == "FOUR_LEVEL").unwrap();
        assert_eq!(four.level_names, ["Base", "Shift", "AltGr", "Shift AltGr"]);
        // LevelThree is Mod5 here
        assert!(four.entries.iter().any(|e| e.level == 2 && e.mods == 0x80));
    }

    #[test]
    fn modifier_keys_have_actions_and_bind_virtual_modifiers() {
        let d = us();
        let lfsh = d.key(50);
        assert_eq!((lfsh.modmap, lfsh.action), (SHIFT, Some(Action::SetMods)));
        let caps = d.key(66);
        assert_eq!((caps.modmap, caps.action), (LOCK, Some(Action::LockMods)));
        assert_eq!(d.key(38).action, None);
        let vmod = |n: &str| 1u16 << d.vmods.iter().position(|(v, _)| v == n).unwrap();
        assert_eq!(d.key(kc(&d, "NMLK")).vmodmap, vmod("NumLock"));
        assert_eq!(d.key(kc(&d, "LALT")).vmodmap, vmod("Alt"));
        assert_eq!(d.key(kc(&d, "LVL3")).vmodmap, vmod("LevelThree"));
        assert_eq!(d.key(kc(&d, "LWIN")).vmodmap, vmod("Super"));
        assert!(!d.key(kc(&d, "LALT")).repeats);
        assert!(d.key(38).repeats);
        // the keymap's own modifier map: <META> is [NoSymbol, Meta_L] on Mod1,
        // which pressing it at level 1 would never show
        let meta = d.key(kc(&d, "META"));
        assert_eq!(meta.modmap, 1 << 3);
        assert_eq!(meta.vmodmap, vmod("Meta"));
        assert_eq!(Desc::level_action(meta, 0), None);
        assert_eq!(Desc::level_action(meta, 1), Some(Action::SetMods));
        assert_eq!(d.key(kc(&d, "SUPR")).modmap, 1 << 6);
    }

    #[test]
    fn names_come_from_the_keymap() {
        let d = us();
        assert_eq!(d.keycodes_name, "evdev+aliases(qwerty)");
        assert_eq!(d.types_name, "complete");
        assert_eq!(d.compat_name, "complete");
        assert_eq!(d.symbols_name, "pc+us+inet(evdev)");
        assert_eq!(d.group_name, "English (US)");
        assert_eq!(d.indicators[0], (0, "Caps Lock".to_string()));
        assert!(d.aliases.contains(&(*b"AB01", *b"LatZ")));
        let r = d.rules.unwrap();
        assert_eq!(r.property(), b"evdev\0pc105\0us\0\0\0");
    }

    #[test]
    fn rules_names_from_the_layout_description() {
        // excerpt of xkeyboard-config 2.42's evdev.lst (Raspberry Pi OS 13)
        let list = "! model\n  pc105           Generic 105-key PC\n\
                    ! layout\n  us              English (US)\n  gb              English (UK)\n\
                    ! variant\n  chr             us: Cherokee\n  extd            gb: English (UK, extended, with Win keys)\n\
                    ! option\n  grp:switch      Right Alt (while pressed)\n";
        assert_eq!(layout_described(list, "English (UK)"), Some(("gb".into(), String::new())));
        assert_eq!(
            layout_described(list, "English (UK, extended, with Win keys)"),
            Some(("gb".into(), "extd".into()))
        );
        assert_eq!(layout_described(list, "Generic 105-key PC"), None);
        assert_eq!(layout_described(list, "Klingon"), None);
        assert_eq!(layout_described(list, ""), None);
    }

    #[test]
    fn rules_names_follow_the_first_layout() {
        let r = rmlvo("pc+de(nodeadkeys)+ru:2+inet(evdev)+ctrl(nocaps)").unwrap();
        assert_eq!(
            (r.layout.as_str(), r.variant.as_str()),
            ("de", "nodeadkeys")
        );
        assert!(rmlvo("").is_none());
        assert!(rmlvo("pc+inet(evdev)").is_none());
    }

    #[test]
    fn component_names_are_unmangled() {
        let t = "xkb_keymap {\nxkb_keycodes \"evdev_aliases(qwerty)\" {\n};\n\
                 xkb_symbols \"pc_us_fr(oss_latin9):2_inet(evdev)\" {\n};\n};";
        assert_eq!(
            component_name(t, "xkb_keycodes").unwrap(),
            "evdev+aliases(qwerty)"
        );
        assert_eq!(
            component_name(t, "xkb_symbols").unwrap(),
            "pc+us+fr(oss_latin9):2+inet(evdev)"
        );
        let t = "xkb_symbols \"pc_us_us_2_inet(evdev)\" {";
        assert_eq!(
            component_name(t, "xkb_symbols").unwrap(),
            "pc+us+us:2+inet(evdev)"
        );
        assert!(component_name("xkb_types \"(unnamed)\" {", "xkb_types").is_none());
    }

    #[test]
    fn served_symbols_name_is_one_group() {
        let g = |s: &str, group: u32| one_layout(s, group);
        assert_eq!(g("pc+de+us:2+inet(evdev)", 0), "pc+de+inet(evdev)");
        assert_eq!(g("pc+de+us:2+inet(evdev)", 1), "pc+us+inet(evdev)");
        let many = "pc+us+fr(oss_latin9):2+ru:3+inet(evdev)+group(alt_shift_toggle)";
        assert_eq!(g(many, 0), "pc+us+inet(evdev)+group(alt_shift_toggle)");
        assert_eq!(
            g(many, 1),
            "pc+fr(oss_latin9)+inet(evdev)+group(alt_shift_toggle)"
        );
        assert_eq!(g(many, 2), "pc+ru+inet(evdev)+group(alt_shift_toggle)");
        // a group the name has nothing for is named as the first
        assert_eq!(g(many, 3), "pc+us+inet(evdev)+group(alt_shift_toggle)");
        assert_eq!(g("pc+de:1+inet(evdev)", 0), "pc+de+inet(evdev)");
        assert_eq!(g("pc+us+inet(evdev)", 0), "pc+us+inet(evdev)");
        assert_eq!(g("", 0), "");
        // GNOME's keymap for a "de" input source carries "us" as group 2
        let text = MUTTER_US.replacen("pc_us_inet(evdev)", "pc_de_us_2_inet(evdev)", 1);
        assert_ne!(text, MUTTER_US);
        let d = crate::bridge::keymap::build(&text)
            .unwrap()
            .xkb
            .as_ref()
            .clone();
        assert_eq!(d.symbols_name, "pc+de+inet(evdev)");
        assert_eq!(d.rules.as_ref().unwrap().layout, "de");
    }

    #[test]
    fn the_locked_group_is_served_as_the_keyboard() {
        use crate::bridge::keymap::build_layout;
        let core = |t: &crate::bridge::keymap::KeyTable, kc: u8| {
            let i = usize::from(kc - MIN_KEYCODE) * usize::from(t.per);
            t.syms[i..i + 2].to_vec()
        };
        // <AD06> is y on us, z on de; <ESC> has one layout, which every group wraps to
        let (ad06, esc) = (29, 9);
        let us = build_layout(US_DE, 0).unwrap();
        assert_eq!(core(&us, ad06), [0x79, 0x59]);
        assert_eq!(us.xkb.key(ad06).syms[..2], [0x79, 0x59]);
        assert_eq!(us.xkb.group_name, "English (US)");
        assert_eq!(us.xkb.symbols_name, "pc+us+inet(evdev)");
        assert_eq!(us.xkb.rules.as_ref().unwrap().layout, "us");

        let de = build_layout(US_DE, 1).unwrap();
        assert_eq!(core(&de, ad06), [0x7a, 0x5a]);
        assert_eq!(de.xkb.key(ad06).syms, [0x7a, 0x5a, 0x8fb, 0xa5]);
        assert_eq!(core(&de, esc), core(&us, esc));
        assert_eq!(core(&de, esc)[0], 0xff1b);
        assert_eq!(de.xkb.key(esc).syms, [0xff1b]);
        assert_eq!(de.xkb.group_name, "German");
        assert_eq!(de.xkb.symbols_name, "pc+de+inet(evdev)");
        assert_eq!(de.xkb.rules.as_ref().unwrap().layout, "de");
        // de's AltGr level needs the four-level type the us layout never uses
        let kt = &de.xkb.types[usize::from(de.xkb.key(ad06).kt)];
        assert_eq!(kt.num_levels, 4, "{}", kt.name);

        // a group past the last wraps, as the compositor's own does
        let wrapped = build_layout(US_DE, 2).unwrap();
        assert_eq!(core(&wrapped, ad06), [0x79, 0x59]);
        assert_eq!(*wrapped.xkb, *us.xkb);
    }

    #[test]
    fn core_mapping_is_laid_out_as_xorgs() {
        use crate::bridge::keymap::{KeyTable, build, build_layout};
        // what Xorg (Xvnc 1.15, xserver 21.1) serves for the same layouts
        // (GetKeyboardMapping, 7 keysyms per keycode), but for its copies of
        // group 1 as group 2 (columns 2, 3 and 6), which stay NoSymbol here
        let row = |t: &KeyTable, kc: u8| {
            let i = usize::from(kc - MIN_KEYCODE) * usize::from(t.per);
            t.syms[i..i + usize::from(t.per)].to_vec()
        };
        let us = build(MUTTER_US).unwrap();
        assert_eq!(us.per, 7);
        assert_eq!(row(&us, 24), [0x71, 0x51, 0, 0, 0, 0, 0]); // q Q
        assert_eq!(row(&us, 9), [0xff1b, 0, 0, 0, 0, 0, 0]); // Escape
        // <LSGT>: less greater . . bar brokenbar
        assert_eq!(row(&us, 94), [0x3c, 0x3e, 0, 0, 0x7c, 0xa6, 0]);
        // <META>: NoSymbol Meta_L
        assert_eq!(row(&us, 205), [0, 0xffe7, 0, 0, 0, 0, 0]);
        // <FK01> (CTRL+ALT, five levels): F1 F1 . . F1 F1 XF86Switch_VT_1
        assert_eq!(
            row(&us, 67),
            [0xffbe, 0xffbe, 0, 0, 0xffbe, 0xffbe, 0x1008fe01]
        );

        let de = build_layout(US_DE, 1).unwrap();
        assert_eq!(de.per, 7);
        // q Q . . at Greek_OMEGA
        assert_eq!(row(&de, 24), [0x71, 0x51, 0, 0, 0x40, 0x7d9, 0]);
        // z Z . . leftarrow yen
        assert_eq!(row(&de, 29), [0x7a, 0x5a, 0, 0, 0x8fb, 0xa5, 0]);
        // 1 exclam . . onesuperior exclamdown
        assert_eq!(row(&de, 10), [0x31, 0x21, 0, 0, 0xb9, 0xa1, 0]);
        // <RALT> is ISO_Level3_Shift on de
        assert_eq!(row(&de, 108), [0xfe03, 0, 0, 0, 0, 0, 0]);
        // every level of every key is in its row
        for kc in MIN_KEYCODE..=MAX_KEYCODE {
            for &sym in de.xkb.key(kc).syms.iter().filter(|&&s| s != 0) {
                assert!(row(&de, kc).contains(&sym), "{kc} {sym:#x}");
            }
        }
        // and without XKB, the four columns served before (AltGr in 2 and 3)
        let legacy = |t: &KeyTable, kc: u8| {
            let i = usize::from(kc - MIN_KEYCODE) * 4;
            t.legacy[i..i + 4].to_vec()
        };
        assert_eq!(legacy(&de, 24), [0x71, 0x51, 0x40, 0x7d9]); // q Q at Greek_OMEGA
        assert_eq!(legacy(&us, 24), [0x71, 0x51, 0, 0]);
        assert_eq!(legacy(&us, 94), [0x3c, 0x3e, 0x7c, 0xa6]);
    }

    #[test]
    fn altgr_keys_outside_the_modifier_map_get_their_action() {
        use crate::bridge::keymap::build_layout;
        let (ralt, lvl3, a) = (108, 92, 38);
        // de (group 2 of "us,de"): <RALT> is ISO_Level3_Shift and sets
        // LevelThree = Mod5, though the keymap's modifier map, which is
        // group 1's, has it as Alt_R's Mod1: served as mapping Mod5
        let de = build_layout(US_DE, 1).unwrap();
        let k = de.xkb.key(ralt);
        assert_eq!((k.syms.as_slice(), k.modmap), ([0xfe03].as_slice(), 0x80));
        assert_eq!(k.action, Some(Action::SetMods));
        assert_eq!(de.xkb.key(lvl3).action, Some(Action::SetMods));
        assert_eq!(de.xkb.key(a).action, None);
        // us: <RALT> is Alt_R, in the modifier map as Mod1
        let us = build_layout(US_DE, 0).unwrap();
        assert_eq!(us.xkb.key(ralt).action, Some(Action::SetMods));
        assert_eq!(us.xkb.key(ralt).modmap, 1 << 3);
    }

    #[test]
    fn spare_keycodes_take_keysyms_and_keysyms_are_reached() {
        use crate::bridge::keymap::{MappingRefused, build, build_layout};
        let mut us = build(MUTTER_US).unwrap();
        let row = |t: &crate::bridge::keymap::KeyTable, kc: u8| {
            let i = usize::from(kc - MIN_KEYCODE) * usize::from(t.per);
            t.syms[i..i + usize::from(t.per)].to_vec()
        };
        // where keysyms are: a plain, A and @ with Shift; no eacute on us
        assert_eq!(us.reach(0x61), Some((38, vec![])));
        assert_eq!(us.reach(0x41), Some((38, vec![50])));
        assert_eq!(us.reach(0x40), Some((11, vec![50])));
        assert_eq!(us.reach(0xe9), None);
        // KP_7: level 2 of a KEYPAD key, only through NumLock, which locks:
        // not reached (that would toggle the user's NumLock)
        assert_eq!(us.reach(0xffb7), None);
        assert_eq!(us.reach(0xff95), Some((79, vec![]))); // KP_Home

        // xdotool's scratch keycode: the last spare one, one keysym
        let spare = (MIN_KEYCODE..=MAX_KEYCODE)
            .rev()
            .find(|&kc| us.xkb.key(kc).syms.is_empty())
            .unwrap();
        assert_eq!(us.change_mapping(spare, 1, &[0xe9], false), Ok(true));
        assert_eq!(us.overrides.get(&spare), Some(&vec![0xe9]));
        assert_eq!(row(&us, spare)[..2], [0xe9, 0]);
        let i = usize::from(spare - MIN_KEYCODE) * 4;
        assert_eq!(us.legacy[i..i + 4], [0xe9, 0, 0, 0]);
        assert_eq!(
            (us.xkb.key(spare).syms.as_slice(), us.xkb.key(spare).kt),
            ([0xe9].as_slice(), 0)
        );
        // the same again changes nothing; reach doesn't count the override
        assert_eq!(us.change_mapping(spare, 1, &[0xe9], false), Ok(false));
        assert_eq!(us.reach(0xe9), None);
        // two levels (x11vnc, pynput), then back to nothing
        assert_eq!(us.change_mapping(spare, 2, &[0x40, 0x40], false), Ok(true));
        assert_eq!(us.xkb.key(spare).kt, 1);
        assert_eq!(us.reach(0x40), Some((11, vec![50])));
        assert_eq!(us.change_mapping(spare, 2, &[0, 0], false), Ok(true));
        assert!(us.overrides.is_empty());
        assert!(us.xkb.key(spare).syms.is_empty());

        // a keycode the keyboard uses can't change, and nothing of the
        // request is applied (keycode 8 is spare, 9 is Escape)
        assert!(us.xkb.key(8).syms.is_empty());
        assert_eq!(
            us.change_mapping(8, 1, &[0xe9, 0x62], false),
            Err(MappingRefused::InUse(9))
        );
        assert!(us.overrides.is_empty());
        // rewriting a used keycode with what it already has is fine
        let a = row(&us, 38);
        assert_eq!(us.change_mapping(38, us.per, &a, false), Ok(false));
        assert_eq!(us.change_mapping(38, 2, &[0x61, 0x41], true), Ok(false));
        assert_eq!(
            us.change_mapping(38, 1, &[0x62], false),
            Err(MappingRefused::InUse(38))
        );

        // de: @ is AltGr+q, held with <LVL3> (ISO_Level3_Shift, sets Mod5)
        let de = build_layout(US_DE, 1).unwrap();
        assert_eq!(de.reach(0x40), Some((24, vec![92])));
        assert_eq!(de.reach(0x7a), Some((29, vec![])));
    }

    #[test]
    fn key_types_stay_within_the_count() {
        let mut types = required_types(&[], &[]);
        let sig = |n: u8| (2u8, vec![(n, 1u8)]);
        // fill up with distinct shapes; a known one is found, not added
        for n in 0..=255u8 {
            if types.len() == MAX_TYPES {
                break;
            }
            let _ = type_for(&mut types, &(3, vec![(n, 2)]));
        }
        assert_eq!(types.len(), MAX_TYPES);
        let known = types[MAX_TYPES - 1].signature();
        assert_eq!(type_for(&mut types, &known), Some(MAX_TYPES - 1));
        // no room: the caller falls back to ONE_LEVEL instead of index 255
        assert_eq!(type_for(&mut types, &sig(0xff)), None);
        assert_eq!(types.len(), MAX_TYPES);
        // and below the limit a new shape is added at the end
        types.truncate(10);
        assert_eq!(type_for(&mut types, &sig(0xfe)), Some(10));
    }

    #[test]
    fn type_blocks_parse() {
        let vmods = vec![
            ("NumLock".to_string(), 0x10),
            ("LevelThree".to_string(), 0x80),
        ];
        let body = "type \"X\" {\n modifiers= Shift+LevelThree;\n map[Shift]= Level2;\n \
                    map[LevelThree]= 3;\n preserve[Shift]= Shift;\n level_name[Level1]= \"Base\";\n};\n\
                    type \"BAD\" { modifiers= Bogus; };";
        let t = parse_types(body, &vmods);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].name, "X");
        assert_eq!((t[0].real, t[0].vmods, t[0].mods), (SHIFT, 2, SHIFT | 0x80));
        assert_eq!(t[0].num_levels, 3);
        assert_eq!(t[0].entries[1].mods, 0x80);
        assert_eq!(t[0].entries[0].preserve, (SHIFT, SHIFT, 0));
        assert_eq!(t[0].entries[1].preserve, (0, 0, 0));
        assert_eq!(t[0].level_names, ["Base", "", ""]);
    }
}
