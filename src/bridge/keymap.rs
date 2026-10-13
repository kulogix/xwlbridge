//! Builds an X keyboard mapping from the compositor's xkb keymap.
//!
//! X keycode = evdev code + 8, which is what xkb uses too. So a keysym the
//! client looks up resolves to a keycode whose `keycode - 8` is the evdev code
//! XTest injects, and the compositor — running the same keymap, which we forward
//! verbatim — turns it back into the keysym the client meant.

use xkbcommon_rs::xkb_state::{KeyDirection, StateComponent};
use xkbcommon_rs::{Context, Keymap, KeymapFormat, State};

/// X keycode range we expose (evdev 0..=247 shifted by 8).
pub const MIN_KEYCODE: u8 = 8;
pub const MAX_KEYCODE: u8 = 255;

/// The X keyboard mapping derived from one compositor keymap.
pub struct KeyTable {
    /// Keysyms per keycode in [`syms`](Self::syms).
    pub per: u8,
    /// The core keyboard mapping (`GetKeyboardMapping`) for keycodes
    /// `MIN_KEYCODE..=MAX_KEYCODE`, `per` each, laid out as Xorg derives it
    /// from the XKB description (see [`core_mapping`]).
    pub syms: Vec<u32>,
    /// The same keys in [`LEGACY_PER`] columns (base, shift, AltGr,
    /// shift+AltGr), the layout served without XKB (`-noxkb`): core-only
    /// clients find AltGr keysyms in columns 2 and 3 there (RealVNC's
    /// vncagent does without XKB; Xorg's columns 4 and 5 it doesn't use).
    pub legacy: Vec<u32>,
    /// The `GetModifierMapping` table: 8 modifiers (Shift, Lock, Control,
    /// Mod1..Mod5) of `modmap.len() / 8` X keycodes each, 0 padding.
    pub modmap: Vec<u8>,
    /// The same keyboard as an XKB description (the XKEYBOARD extension).
    pub xkb: std::sync::Arc<crate::bridge::xkbdesc::Desc>,
    /// Keysyms X clients put on spare keycodes (ones the compositor keyboard
    /// leaves empty) with `ChangeKeyboardMapping`, as xdotool, x11vnc and
    /// pynput do for a keysym they find nowhere else; levels 1 and 2 only.
    /// See [`change_mapping`](Self::change_mapping) and [`reach`](Self::reach).
    pub overrides: std::collections::BTreeMap<u8, Vec<u32>>,
    /// Keycodes with no keysyms in the compositor keyboard (bit per keycode).
    spare: [u64; 4],
    /// Keysyms only the keymap's *other* layouts have (levels 1 and 2), and
    /// the key that has each (the first layout and key with it). With ru
    /// active, `c` is here on keycode 54, where "us" has it. See
    /// [`plan`](Self::plan).
    other_layouts: std::collections::HashMap<u32, u8>,
}

/// Why a `ChangeKeyboardMapping` was not applied.
#[derive(Debug, PartialEq, Eq)]
pub enum MappingRefused {
    /// It would change a keycode the compositor keyboard uses: X can't
    /// change that keyboard.
    InUse(u8),
}

impl KeyTable {
    /// Applies a core `ChangeKeyboardMapping`: `rows` holds `per` keysyms
    /// for each keycode from `first` on. A keycode whose row doesn't change
    /// (as served: the legacy layout with `legacy`) is left alone; one the
    /// compositor keyboard has no keysyms on takes the row's first two (none:
    /// back to empty); any other change refuses the whole request, nothing
    /// applied, as the compositor's keyboard can't be changed from X.
    /// Returns whether anything changed.
    pub fn change_mapping(
        &mut self,
        first: u8,
        per: u8,
        rows: &[u32],
        legacy: bool,
    ) -> Result<bool, MappingRefused> {
        let per = usize::from(per.max(1));
        let trimmed = |row: &[u32]| -> Vec<u32> {
            let n = row.iter().rposition(|&s| s != 0).map_or(0, |i| i + 1);
            row[..n].to_vec()
        };
        let mut changes = Vec::new();
        for (i, row) in rows.chunks(per).enumerate() {
            let Some(kc) = u8::try_from(usize::from(first) + i)
                .ok()
                .filter(|&k| k >= MIN_KEYCODE)
            else {
                continue;
            };
            let (table, width) = if legacy {
                (&self.legacy, usize::from(LEGACY_PER))
            } else {
                (&self.syms, usize::from(self.per))
            };
            let at = usize::from(kc - MIN_KEYCODE) * width;
            if trimmed(&table[at..at + width]) == trimmed(row) {
                continue;
            }
            if !self.is_spare(kc) {
                return Err(MappingRefused::InUse(kc));
            }
            let mut levels = trimmed(&row[..row.len().min(2)]);
            if levels.len() == 2 && levels[1] == 0 {
                levels.truncate(1);
            }
            changes.push((kc, levels));
        }
        let changed = !changes.is_empty();
        for (kc, levels) in changes {
            self.set_override(kc, levels);
        }
        Ok(changed)
    }

    fn is_spare(&self, kc: u8) -> bool {
        self.spare[usize::from(kc / 64)] & (1 << (kc % 64)) != 0
    }

    /// Puts `levels` (none: nothing) on spare keycode `kc` in every form the
    /// keyboard is served in.
    fn set_override(&mut self, kc: u8, levels: Vec<u32>) {
        let i = usize::from(kc - MIN_KEYCODE);
        let (per, legacy_per) = (usize::from(self.per), usize::from(LEGACY_PER));
        let mut row = vec![0u32; per.max(legacy_per)];
        row[..levels.len()].copy_from_slice(&levels);
        self.syms[i * per..(i + 1) * per].copy_from_slice(&row[..per]);
        self.legacy[i * legacy_per..(i + 1) * legacy_per].copy_from_slice(&row[..legacy_per]);
        let key = &mut std::sync::Arc::make_mut(&mut self.xkb).keys[i];
        key.syms = levels.clone();
        // ONE_LEVEL or TWO_LEVEL (the required types' places)
        key.kt = u8::from(levels.len() > 1);
        key.action = None;
        if levels.is_empty() {
            self.overrides.remove(&kc);
        } else {
            self.overrides.insert(kc, levels);
        }
    }

    /// Where the compositor keyboard has `sym`: a keycode and the modifier
    /// keycodes to hold for its level (Shift for level 2, ISO_Level3_Shift for
    /// AltGr levels, ...), the fewest modifiers first, keys that only set
    /// (not lock) them. `None` if the keyboard doesn't have it, or not on a
    /// level its modifier keys can reach. Spare keycodes clients have put
    /// keysyms on don't count.
    pub fn reach(&self, sym: u32) -> Option<(u8, Vec<u8>)> {
        use crate::bridge::xkbdesc::Action;
        if sym == 0 {
            return None;
        }
        let per = self.modmap.len() / 8;
        // a keycode that sets real modifier `bit` when pressed
        let setter = |bit: usize| -> Option<u8> {
            self.modmap
                .get(bit * per..(bit + 1) * per)?
                .iter()
                .copied()
                .find(|&kc| {
                    kc >= MIN_KEYCODE
                        && !self.overrides.contains_key(&kc)
                        && matches!(
                            self.xkb.key(kc).action,
                            Some(Action::SetMods | Action::SetModsMask { .. })
                        )
                })
        };
        let mut best: Option<(u32, u8, Vec<u8>)> = None;
        for kc in MIN_KEYCODE..=MAX_KEYCODE {
            if self.overrides.contains_key(&kc) {
                continue;
            }
            let key = self.xkb.key(kc);
            for (level, _) in key.syms.iter().enumerate().filter(|&(_, &s)| s == sym) {
                let mut options: Vec<u8> = if level == 0 {
                    vec![0]
                } else {
                    let t = &self.xkb.types[usize::from(key.kt)];
                    t.entries
                        .iter()
                        .filter(|e| usize::from(e.level) == level)
                        .map(|e| e.mods)
                        .collect()
                };
                options.sort_by_key(|m| m.count_ones());
                for mods in options {
                    let keys: Option<Vec<u8>> = (0..8)
                        .filter(|b| mods & (1 << b) != 0)
                        .map(setter)
                        .collect();
                    let Some(keys) = keys else { continue };
                    let cost = mods.count_ones();
                    if best.as_ref().is_none_or(|b| cost < b.0) {
                        best = Some((cost, kc, keys));
                    }
                    break;
                }
            }
        }
        best.map(|(_, kc, keys)| (kc, keys))
    }

    /// How to type `sym` on the compositor keyboard: the key and the modifier
    /// keys to hold around it. This is the one place that decides which of
    /// the keymap's layouts (xkb groups) a keysym is typed through, for both
    /// engines and every path (a spare keycode an X client put the keysym on,
    /// queued input re-resolved after a layout switch):
    ///
    /// - the active layout's key for it ([`reach`](Self::reach)), as the X
    ///   keyboard shows it;
    /// - otherwise, only as part of a `shortcut` (Control, Alt or Super held:
    ///   [`Input::shortcut_held`](crate::bridge::input::Input::shortcut_held)),
    ///   the key where another layout of the keymap
    ///   has it, pressed as it is. The compositor applies its active layout
    ///   there, and apps match shortcuts on that key across layouts, exactly
    ///   as for a local user: Ctrl + the C key types Ctrl + Cyrillic_es with
    ///   ru active, which GTK, Qt and GNOME take for Ctrl+C. Without such a
    ///   modifier it would type that other character, so it types nothing
    ///   (the active layout lacks the keysym);
    /// - otherwise `None`: no layout has it.
    pub fn plan(&self, sym: u32, shortcut: bool) -> Option<(u8, Vec<u8>)> {
        self.reach(sym).or_else(|| {
            shortcut.then(|| self.other_layouts.get(&sym).map(|&kc| (kc, Vec::new())))?
        })
    }

    /// Whether `sym` is on one of the keymap's other layouts only (so
    /// [`plan`](Self::plan) types it as part of a shortcut and not alone).
    pub fn on_other_layout(&self, sym: u32) -> bool {
        self.reach(sym).is_none() && self.other_layouts.contains_key(&sym)
    }

    /// The keysym X keycode `kc` types without modifiers (its level 1) on
    /// the served layout, 0 for none; a spare keycode's client keysym counts.
    #[cfg_attr(not(feature = "gnome"), allow(dead_code))]
    pub fn level1(&self, kc: u8) -> u32 {
        if kc < MIN_KEYCODE {
            return 0;
        }
        self.xkb.key(kc).syms.first().copied().unwrap_or(0)
    }

    /// Whether the compositor keyboard has no keysyms on `kc` (so an X
    /// client may put its own there).
    #[cfg_attr(not(feature = "xvfb"), allow(dead_code))]
    pub fn spare(&self, kc: u8) -> bool {
        self.is_spare(kc)
    }
}

/// What an X client's key event becomes on the compositor keyboard, for keys
/// on spare keycodes the client put a keysym on (`ChangeKeyboardMapping`, as
/// xdotool, x11vnc, pynput and RealVNC's server do for a keysym they find on
/// no key): the keysym's key, pressed through [`KeyTable::plan`] with the
/// modifiers its level needs held around it. Kept per X client (on a stock
/// engine, per X server). Shared by the built-in engine's XTest and the stock
/// engines' agent, so both type such keysyms the same way.
///
/// The modifiers are only for that key: they go up before any other key of
/// the client's goes down, and the whole thing is released by a release of
/// the spare keycode or of the key pressed for it (x11vnc releases what its
/// re-read mapping says, which after the MappingNotify the change caused can
/// be the real key).
#[derive(Default)]
pub struct Synth {
    /// Spare keycode held -> (the key pressed for it, the modifier keys
    /// pressed for its level that the client wasn't holding itself).
    held: std::collections::HashMap<u8, (u8, Vec<u8>)>,
    /// Keysyms logged as untypeable, so each is logged once (bounded).
    warned: std::collections::HashSet<u32>,
}

/// At most this many distinct untypeable keysyms are logged per [`Synth`]: a
/// client cycling through keysyms can't grow the log (or the set) at
/// request rate.
const SYNTH_WARN_MAX: usize = 32;

/// One event for [`Input::fake_input`](crate::bridge::input::Input::fake_input):
/// `(press, keycode)`.
pub type KeyEvent = (bool, u8);

impl Synth {
    /// Handles an X key press or release of keycode `kc`. `spare_sym` tells
    /// the keysym the client put on `kc` if `kc` is a spare keycode it put
    /// one on (asked only for presses); `holds` whether the client holds a
    /// key already; `table` the compositor keyboard; `shortcut` whether a
    /// shortcut modifier is held now. Returns the events to inject instead,
    /// or `None` to pass the event through as it is. `log` names the client
    /// in warnings.
    #[allow(clippy::too_many_arguments)]
    pub fn key(
        &mut self,
        press: bool,
        kc: u8,
        spare_sym: impl FnOnce() -> Option<u32>,
        holds: impl Fn(u8) -> bool,
        table: Option<&KeyTable>,
        shortcut: bool,
        log: &dyn std::fmt::Display,
    ) -> Option<Vec<KeyEvent>> {
        let mut out = Vec::new();
        if !press {
            let spare = self
                .held
                .iter()
                .find(|&(&spare, &(target, _))| spare == kc || target == kc)
                .map(|(&spare, _)| spare)?;
            let (target, mods) = self.held.remove(&spare)?;
            crate::vlog!("{log} keycode {kc} up: keycode {target}, then {mods:?}");
            out.push((false, target));
            out.extend(mods.iter().rev().map(|&m| (false, m)));
            return Some(out);
        }
        if self.held.contains_key(&kc) {
            return Some(out); // a repeat: still held
        }
        // modifiers held for an earlier spare key must not change this one,
        // and a key pressed for one is let go before the client presses it
        for (_, mods) in self.held.values_mut() {
            out.extend(mods.drain(..).rev().map(|m| (false, m)));
        }
        self.held.retain(|_, &mut (target, _)| {
            if target == kc {
                out.push((false, target));
            }
            target != kc
        });
        let Some(sym) = spare_sym() else {
            // an ordinary key: through as it is, after the releases above
            if out.is_empty() {
                return None;
            }
            out.push((true, kc));
            return Some(out);
        };
        match table.and_then(|t| t.plan(sym, shortcut)) {
            Some((target, mods)) => {
                let mods: Vec<u8> = mods.into_iter().filter(|&m| !holds(m)).collect();
                crate::vlog!(
                    "{log} spare keycode {kc} (keysym {sym:#x}) down: {mods:?}, then keycode {target}"
                );
                out.extend(mods.iter().map(|&m| (true, m)));
                out.push((true, target));
                self.held.insert(kc, (target, mods));
            }
            None => {
                if self.warned.len() < SYNTH_WARN_MAX && self.warned.insert(sym) {
                    let why = if table.is_some_and(|t| t.on_other_layout(sym)) {
                        "is only on a layout that isn't active, so it is typed there only as part \
                         of a shortcut (with Ctrl, Alt or Super)"
                    } else {
                        "is not on the compositor's keyboard, so it can't be typed"
                    };
                    crate::warning!(
                        "{log} keysym {sym:#x} (on spare keycode {kc}) {why}{}",
                        if self.warned.len() == SYNTH_WARN_MAX {
                            " (no more such lines for this client)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        Some(out)
    }

    /// Whether a spare keycode's key is held (for tests and diagnostics).
    #[cfg(test)]
    pub fn holding(&self) -> usize {
        self.held.len()
    }
}

/// Keysyms per keycode in [`KeyTable::legacy`].
pub const LEGACY_PER: u8 = 4;

/// The X modifiers, in `GetModifierMapping` order, by their xkb real-modifier
/// names.
const X_MODIFIERS: [&str; 8] = [
    "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
];

/// Compiles the keymap text into the keysym and modifier tables, for its
/// first layout.
pub fn build(text: &str) -> Option<KeyTable> {
    build_layout(text, 0)
}

/// Compiles the keymap text into the keysym and modifier tables for one of its
/// layouts (xkb groups, from 0; past the last wraps, as xkb's does): the one
/// the compositor has active. A keymap can carry several (GNOME puts up to
/// three input sources in one and switches between them by the group), and
/// the compositor turns a keycode into the active one's keysym, so an X client
/// looking a keysym up has to find it there. X's own groups are no use: the
/// compositor never learns which one an X client picked. The core map and the
/// XKB description both describe that layout alone, as XKB's group 1.
pub fn build_layout(text: &str, group: u32) -> Option<KeyTable> {
    let ctx = Context::new(0).ok()?;
    let keymap = Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok()?;
    let group = match keymap.num_layouts() {
        0 => 0,
        n => group as usize % n,
    };
    let mut state = State::new(keymap.clone());
    let mask = |name| keymap.mod_get_index(name).map_or(0u32, |i| 1u32 << i);

    let shift = mask("Shift");
    let level3 = mask("Mod5"); // AltGr
    let levels = [0, shift, level3, shift | level3];
    let mut legacy = Vec::with_capacity(usize::from(MAX_KEYCODE - MIN_KEYCODE + 1) * 4);
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        let mut key = [0u32; LEGACY_PER as usize];
        for (sym, &m) in key.iter_mut().zip(&levels) {
            state.update_mask(m, 0, 0, 0, 0, group);
            *sym = state.key_get_one_sym(u32::from(kc)).map_or(0, |s| s.raw());
        }
        // Keys without a level3 symbol resolve to the same keysyms with Mod5
        // set, so blank those out. Core clients treat the second pair as a
        // separate group, and x11vnc's modtweak uses the last column a keysym
        // appears in, so with duplicates it thinks shifted keysyms need
        // Shift+AltGr and releases Shift around them.
        if key[2] == key[0] {
            key[2] = 0;
        }
        if key[3] == key[1] {
            key[3] = 0;
        }
        legacy.extend_from_slice(&key);
    }
    state.update_mask(0, 0, 0, 0, 0, group);

    // The modifier map comes from the keymap too, so xkb options that move
    // modifiers (swapping Caps and Ctrl, say) show up in X: press each key and
    // see which real modifiers it sets, resetting any lock or latch after.
    let x_mods = X_MODIFIERS.map(mask);
    let mut per_mod: [Vec<u8>; 8] = Default::default();
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        // xkbcommon-rs panics in update_key for a keycode with no groups
        if state.key_get_layout(u32::from(kc)).is_none() {
            continue;
        }
        state.update_key(u32::from(kc), KeyDirection::Down);
        let set = state.serialize_mods(StateComponent::MODS_EFFECTIVE);
        state.update_key(u32::from(kc), KeyDirection::Up);
        state.update_mask(0, 0, 0, 0, 0, group);
        for (keys, &m) in per_mod.iter_mut().zip(&x_mods) {
            if m != 0 && set & m != 0 {
                keys.push(kc);
            }
        }
    }
    let per = per_mod.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut modmap = Vec::with_capacity(8 * per);
    for keys in &per_mod {
        modmap.extend_from_slice(keys);
        modmap.resize(modmap.len() + per - keys.len(), 0);
    }
    let xkb = std::sync::Arc::new(crate::bridge::xkbdesc::build(&keymap, text, &modmap, group));
    let (per, syms) = core_mapping(&xkb);
    let mut spare = [0u64; 4];
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        if xkb.key(kc).syms.iter().all(|&s| s == 0) {
            spare[usize::from(kc / 64)] |= 1 << (kc % 64);
        }
    }
    let other_layouts = other_layouts(&keymap, group, &xkb);
    Some(KeyTable {
        per,
        syms,
        legacy,
        modmap,
        xkb,
        overrides: std::collections::BTreeMap::new(),
        spare,
        other_layouts,
    })
}

/// The keysyms on levels 1 and 2 of the keymap's layouts other than
/// `active` that the active one (`served`) has nowhere, each with the first
/// key (by layout, then keycode) that has it. GNOME keeps a Latin layout in
/// every keymap (it appends the locale's layout to the user's input sources
/// for shortcuts), so with a non-Latin source active the Latin letters are
/// here.
fn other_layouts(
    keymap: &Keymap,
    active: usize,
    served: &crate::bridge::xkbdesc::Desc,
) -> std::collections::HashMap<u32, u8> {
    let have: std::collections::HashSet<u32> = served
        .keys
        .iter()
        .flat_map(|k| k.syms.iter().copied())
        .collect();
    let mut out = std::collections::HashMap::new();
    for layout in (0..keymap.num_layouts()).filter(|&l| l != active) {
        for kc in MIN_KEYCODE..=MAX_KEYCODE {
            // a key with fewer layouts wraps to one of them, which is how
            // the compositor types it too
            if keymap.num_layouts_for_key(u32::from(kc)).unwrap_or(0) == 0 {
                continue;
            }
            for level in 0..2 {
                let Ok(syms) = keymap.key_get_syms_by_level(u32::from(kc), layout, level) else {
                    continue;
                };
                if let [sym] = syms[..] {
                    let sym = sym.raw();
                    if sym != 0 && !have.contains(&sym) {
                        out.entry(sym).or_insert(kc);
                    }
                }
            }
        }
    }
    out
}

/// The core keyboard mapping of a (one-group) XKB description, laid out as
/// Xorg lays it out (XKB protocol spec section 12.4; xserver's
/// `XkbGetCoreMap`), so that core clients find every keysym in the column
/// they would on Xorg: per key, group 1's levels 1 and 2 in columns 0 and 1,
/// group 2's in 2 and 3, then group 1's further levels (AltGr ones in columns
/// 4 and 5). The width is the widest key's level count + 2 (7 for the usual
/// pc105 layouts); unused columns are NoSymbol. Xlib with XKB maps columns 4
/// and up to group 1's levels 3 and up, so `XKeysymToKeycode` finds AltGr
/// keysyms only with this width, and tools that choose modifiers by column
/// (xte, x11vnc's modtweak) press ISO_Level3_Shift for them.
///
/// One difference from Xorg: Xorg fills group 2's columns of a one-group key
/// with copies of group 1 (`a A a A`); they stay NoSymbol here, since the one
/// group is the whole keyboard. Tools that take the last column a keysym
/// appears in would otherwise think group 2 (Mode_switch) is needed: x11vnc's
/// core modtweak, which it uses when XKB isn't usable at its start, types
/// `a z 2 backslash 1` for `aZ@|!` on Xorg with `-noxkb` because of them.
pub fn core_mapping(desc: &crate::bridge::xkbdesc::Desc) -> (u8, Vec<u32>) {
    let per = desc
        .keys
        .iter()
        .filter(|k| !k.syms.is_empty())
        .map(|k| k.syms.len().max(2) + if k.syms.len() > 2 { 2 } else { 0 })
        .max()
        .unwrap_or(1)
        .min(255);
    let mut out = vec![0u32; desc.keys.len() * per];
    for (key, core) in desc.keys.iter().zip(out.chunks_mut(per)) {
        for (level, &sym) in key.syms.iter().enumerate() {
            let column = if level < 2 { level } else { level + 2 };
            if let Some(c) = core.get_mut(column) {
                *c = sym;
            }
        }
    }
    (per as u8, out)
}

/// Slices the table for a `GetKeyboardMapping(first_keycode, count)` request.
pub fn mapping_slice(table: &[u32], per: u8, first_keycode: u8, count: u8) -> Vec<u32> {
    let per = usize::from(per);
    let start = first_keycode.saturating_sub(MIN_KEYCODE) as usize * per;
    (0..count as usize * per)
        .map(|i| table.get(start + i).copied().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two layouts in one keymap, as GNOME builds it for two input sources:
    /// a "us"-like first one and a "de"-like second one with Y and Z swapped.
    const TWO_LAYOUTS: &str = r#"xkb_keymap {
  xkb_keycodes "test" { minimum = 8; maximum = 255; <AD06> = 29; <LFSH> = 50; <AB01> = 52; };
  xkb_types "test" {
    virtual_modifiers NumLock;
    type "ONE_LEVEL" { modifiers = none; level_name[Level1] = "Any"; };
    type "TWO_LEVEL" { modifiers = Shift; map[Shift] = Level2; level_name[Level1] = "Base"; level_name[Level2] = "Shift"; };
    type "ALPHABETIC" { modifiers = Shift+Lock; map[Shift] = Level2; map[Lock] = Level2; level_name[Level1] = "Base"; level_name[Level2] = "Caps"; };
  };
  xkb_compat "test" {
    interpret Shift_L { action = SetMods(modifiers = Shift); };
  };
  xkb_symbols "pc_us_de_2" {
    name[Group1] = "English (US)";
    name[Group2] = "German";
    key <AD06> { symbols[Group1] = [ y, Y ], symbols[Group2] = [ z, Z ] };
    key <AB01> { symbols[Group1] = [ z, Z ], symbols[Group2] = [ y, Y ] };
    key <LFSH> { [ Shift_L ] };
    modifier_map Shift { <LFSH> };
  };
};"#;

    fn syms(t: &KeyTable, kc: u8) -> [u32; 2] {
        let i = (kc - MIN_KEYCODE) as usize * t.per as usize;
        [t.syms[i], t.syms[i + 1]]
    }

    #[test]
    fn each_layout_gets_its_own_table() {
        let us = build_layout(TWO_LAYOUTS, 0).expect("compiles");
        assert_eq!(syms(&us, 52), [u32::from(b'z'), u32::from(b'Z')]);
        assert_eq!(syms(&us, 29), [u32::from(b'y'), u32::from(b'Y')]);
        // the second layout active (GNOME's Super+Space): an X client looking up
        // 'z' must find the key that types 'z' there
        let de = build_layout(TWO_LAYOUTS, 1).expect("compiles");
        assert_eq!(syms(&de, 52), [u32::from(b'y'), u32::from(b'Y')]);
        assert_eq!(syms(&de, 29), [u32::from(b'z'), u32::from(b'Z')]);
        // single-layout keys and the modifier map are the same in both
        assert_eq!(syms(&de, 50), syms(&us, 50));
        assert_eq!(de.modmap, us.modmap);
        assert!(us.modmap.contains(&50), "Shift_L is a Shift key");
        // and plain build() is the first layout
        assert_eq!(build(TWO_LAYOUTS).unwrap().syms, us.syms);
    }

    /// GNOME's keymap for the input sources us and ru: the user's two plus
    /// the locale's layout for shortcuts (`pc+us+ru:2+us:3`), from xkbcomp.
    const US_RU_US: &str = include_str!("testdata/xkbcomp_us_ru_us.xkb");
    const C: u32 = 0x63;
    const CYRILLIC_ES: u32 = 0x6d3;

    #[test]
    fn latin_keysyms_go_through_another_layout_only_as_shortcuts() {
        let ru = build_layout(US_RU_US, 1).expect("compiles");
        assert_eq!(ru.level1(54), CYRILLIC_ES);
        // the active layout's own keysym, as text or shortcut
        assert_eq!(ru.plan(CYRILLIC_ES, false), Some((54, vec![])));
        assert_eq!(ru.plan(0x6f3, false), Some((54, vec![50]))); // Cyrillic_ES: Shift
        // 'c' isn't on ru: as text it would type 'с', so nothing; as a
        // shortcut (Ctrl+C) the C key, which mutter and GTK take for Ctrl+C
        assert_eq!(ru.reach(C), None);
        assert_eq!(ru.plan(C, false), None);
        assert_eq!(ru.plan(C, true), Some((54, vec![])));
        assert!(ru.on_other_layout(C));
        // Ctrl+Shift+C: the client holds Shift itself
        assert_eq!(ru.plan(0x43, true), Some((54, vec![])));
        assert_eq!(ru.plan(0x7a, true), Some((52, vec![]))); // Ctrl+Z
        // on no layout at all
        assert_eq!(ru.plan(0xe9, true), None);
        assert!(!ru.on_other_layout(0xe9));
        // us active: 'c' is just the active layout's, Cyrillic isn't typed
        let us = build_layout(US_RU_US, 0).unwrap();
        assert_eq!(us.plan(C, false), Some((54, vec![])));
        assert!(!us.on_other_layout(C));
        assert_eq!(us.plan(CYRILLIC_ES, false), None);
        assert_eq!(us.plan(CYRILLIC_ES, true), Some((54, vec![])));
        // the third layout (us again) adds nothing the first doesn't have
        let third = build_layout(US_RU_US, 2).unwrap();
        assert_eq!(third.plan(C, false), Some((54, vec![])));
    }

    #[test]
    fn spare_keycode_keysyms_are_typed_through_the_layouts() {
        let ru = build_layout(US_RU_US, 1).unwrap();
        let mut s = Synth::default();
        let free = |_: u8| false;
        let k = |s: &mut Synth, press, kc, sym: Option<u32>, shortcut| {
            s.key(press, kc, || sym, free, Some(&ru), shortcut, &"[test]")
        };
        assert!(ru.spare(248) && !ru.spare(54));
        // RealVNC put the viewer's 'c' on spare keycode 248, Control held
        assert_eq!(k(&mut s, true, 248, Some(C), true), Some(vec![(true, 54)]));
        assert_eq!(
            k(&mut s, true, 248, Some(C), true),
            Some(vec![]),
            "a repeat"
        );
        assert_eq!(k(&mut s, false, 248, None, false), Some(vec![(false, 54)]));
        assert_eq!(s.holding(), 0);
        // without Control: not typed, and its release passes through (to be
        // dropped, as nothing was pressed)
        assert_eq!(k(&mut s, true, 248, Some(C), false), Some(vec![]));
        assert_eq!(k(&mut s, false, 248, None, false), None);
        // a keysym of the active layout's level 2: Shift around it, let go
        // before the client's next key
        assert_eq!(
            k(&mut s, true, 247, Some(0x6f3), false),
            Some(vec![(true, 50), (true, 54)])
        );
        assert_eq!(
            k(&mut s, true, 38, None, false),
            Some(vec![(false, 50), (true, 38)])
        );
        assert_eq!(k(&mut s, false, 247, None, false), Some(vec![(false, 54)]));
        // ordinary keys and their releases pass through untouched
        assert_eq!(k(&mut s, false, 38, None, false), None);
        assert_eq!(k(&mut s, true, 37, None, false), None);
        // the client holds Shift itself: not pressed again
        let shifted = s.key(
            true,
            246,
            || Some(0x6f3),
            |m| m == 50,
            Some(&ru),
            false,
            &"[test]",
        );
        assert_eq!(shifted, Some(vec![(true, 54)]));
        // no keyboard yet: nothing typed for a spare keycode's keysym
        assert_eq!(
            s.key(true, 245, || Some(C), free, None, true, &"[test]"),
            Some(vec![])
        );
    }
}
