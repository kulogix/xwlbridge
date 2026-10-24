//! Which monitor is GNOME's primary one, from what mutter says.
//!
//! mutter 50.1's `GetCurrentState` keeps a stale `primary` flag after any
//! primary change: both logical monitors read `primary = true` from then on
//! (research/release/p2/dynamic/README.md, "GetCurrentState keeps a stale
//! primary"), so neither "the one with the flag" nor "the first one with the
//! flag" is the desktop's primary. This decides it from what is known:
//!
//! 1. one flag: that monitor (no ambiguity to resolve);
//! 2. several flags: the ones that were not flagged in the previous read
//!    are the ones that became primary since (that is what the user did).
//    Exactly one: it;
//! 3. still ambiguous (the first read, or several changed at once): the
//!    primary GNOME Shell reports itself through the `xwlbridge-windows`
//!    extension (`GetMonitors`, which answers anyone), if it is among the
//!    flagged ones;
//! 4. nothing of the sort: the primary in use stays, which is said once.

/// What the decision can ask besides the flags.
pub(super) trait PrimarySource {
    /// The primary monitor according to GNOME Shell's own view
    /// (`GetMonitors`), if it can be asked.
    fn shell_primary(&mut self) -> Option<usize>;
    /// Says something to the operator (once per [`PrimaryModel::update`]).
    fn log(&mut self, msg: &str);
}

/// What the model knows between two reads.
pub(super) struct PrimaryModel {
    /// The primary monitor in use, by connector.
    primary: Option<String>,
    /// Every monitor's `primary` flag at the previous read, by connector.
    flags: Vec<(String, bool)>,
    /// The last "can't tell" was said.
    complained: bool,
}

impl PrimaryModel {
    pub(super) fn new() -> Self {
        PrimaryModel {
            primary: None,
            flags: Vec::new(),
            complained: false,
        }
    }

    fn flagged_before(&self, connector: &str) -> bool {
        self.flags.iter().any(|(c, f)| c == connector && *f)
    }

    /// The index (into `monitors`) of the monitor to show as primary, each
    /// monitor's connector and GNOME `primary` flag. `None` only without
    /// monitors.
    pub(super) fn update(
        &mut self,
        monitors: &[(&str, bool)],
        src: &mut dyn PrimarySource,
    ) -> Option<usize> {
        if monitors.is_empty() {
            return None;
        }
        let flagged: Vec<usize> = (0..monitors.len()).filter(|&i| monitors[i].1).collect();
        let in_use = self
            .primary
            .as_deref()
            .and_then(|p| monitors.iter().position(|m| m.0 == p));
        // the flagged monitors that were not flagged at the previous read
        let turned_on: Vec<usize> = flagged
            .iter()
            .copied()
            .filter(|&i| !self.flagged_before(monitors[i].0))
            .collect();
        let mut choice = match flagged.as_slice() {
            // one flag: that is the primary
            [only] => Some(*only),
            // nothing flagged: nothing new
            [] => None,
            // several (mutter 50.1): the one whose flag turned on since
            // the previous read is the user's choice
            _ => match turned_on.as_slice() {
                [one] => Some(*one),
                _ => None,
            },
        };
        if choice.is_none() && flagged.len() > 1 {
            // ask GNOME Shell; else keep the primary in use
            choice = src.shell_primary().filter(|i| flagged.contains(i));
            if choice.is_none() && !self.complained {
                src.log(
                    "gnome: mutter can't say which monitor is primary (stale flags); keeping \
                     the one in use",
                );
            }
            self.complained = choice.is_none();
        } else {
            self.complained = false;
        }
        // a first read with nothing to tell is no answer at all
        let choice = choice.or(in_use).or(match flagged.as_slice() {
            [only] => Some(*only),
            _ => None,
        });
        if let Some(i) = choice {
            self.primary = Some(monitors[i].0.to_string());
        }
        self.flags = monitors.iter().map(|&(c, f)| (c.to_string(), f)).collect();
        choice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GNOME Shell's answer (as `GetMonitors` would give it), and what was
    /// logged.
    #[derive(Default)]
    struct Src {
        shell: Option<usize>,
        log: Vec<String>,
    }

    impl PrimarySource for Src {
        fn shell_primary(&mut self) -> Option<usize> {
            self.shell
        }

        fn log(&mut self, msg: &str) {
            self.log.push(msg.to_string());
        }
    }

    const A: &str = "DP-1";
    const B: &str = "HDMI-1";
    const C: &str = "DP-2";

    #[test]
    fn one_flag_is_the_primary() {
        let mut m = PrimaryModel::new();
        let mut s = Src::default();
        assert_eq!(m.update(&[(A, true), (B, false)], &mut s), Some(0));
        assert_eq!(m.update(&[(A, false), (B, true)], &mut s), Some(1));
        assert!(s.log.is_empty());
    }

    /// mutter 50.1 after a primary change: both flags set. The monitor
    /// whose flag went on since the last read is the user's choice.
    #[test]
    fn several_flags_take_the_one_that_turned_on() {
        let mut m = PrimaryModel::new();
        let mut s = Src::default();
        let step =
            |m: &mut PrimaryModel, f: (bool, bool), s: &mut Src| m.update(&[(A, f.0), (B, f.1)], s);
        assert_eq!(step(&mut m, (true, false), &mut s), Some(0));
        // the user makes the second monitor primary: both flags now
        assert_eq!(step(&mut m, (true, true), &mut s), Some(1));
        // the stale pair on its own says nothing new: the primary in use
        // stays
        assert_eq!(step(&mut m, (true, true), &mut s), Some(1));
        // both flags already on: nothing turned on, so nothing is newer
        // than the primary in use
        assert_eq!(step(&mut m, (true, true), &mut s), Some(1));
        // a flag going away is an answer again; then the user makes the
        // first primary, which turns its flag on
        assert_eq!(step(&mut m, (false, true), &mut s), Some(1));
        // (both flags already on: nothing says which is newer, so the
        // primary in use stays, and that was said once)
        assert_eq!(step(&mut m, (true, true), &mut s), Some(0));
        assert_eq!(s.log.len(), 1, "{:?}", s.log);
    }

    /// No read before to compare with (the first one): GNOME Shell's own
    /// view, if the extension is there.
    #[test]
    fn several_flags_ask_gnome_shell() {
        let mut m = PrimaryModel::new();
        let mut s = Src {
            shell: Some(2),
            ..Default::default()
        };
        let flags = [(A, true), (B, true), (C, true)];
        assert_eq!(m.update(&flags, &mut s), Some(2), "the extension's answer");
        // an answer that is not a flagged monitor is no answer
        let mut m = PrimaryModel::new();
        let mut s = Src {
            shell: Some(2),
            ..Default::default()
        };
        assert_eq!(m.update(&[(A, true), (B, true), (C, false)], &mut s), None);
        assert_eq!(s.log.len(), 1);
    }

    /// Nothing to tell: the primary in use stays, and that is said once.
    #[test]
    fn without_history_or_shell_the_primary_in_use_stays() {
        let mut m = PrimaryModel::new();
        let mut s = Src::default();
        assert_eq!(m.update(&[(A, true), (B, true)], &mut s), None);
        assert_eq!(s.log.len(), 1);
        assert_eq!(m.update(&[(A, true), (B, true)], &mut s), None);
        assert_eq!(s.log.len(), 1, "said once, not on every read");
        // one flag is an answer again
        assert_eq!(m.update(&[(A, false), (B, true)], &mut s), Some(1));
        // no monitor, no primary
        assert_eq!(m.update(&[], &mut s), None);
    }

    /// A monitor plugged in before the primary does not move it, and one
    /// whose flag turns on while others are flagged is the new primary.
    #[test]
    fn the_primary_follows_its_connector() {
        let mut m = PrimaryModel::new();
        let mut s = Src::default();
        assert_eq!(m.update(&[(A, false), (B, true)], &mut s), Some(1));
        // C plugged in before: B is still the primary
        assert_eq!(
            m.update(&[(C, false), (A, false), (B, true)], &mut s),
            Some(2)
        );
        // A's flag turns on next to B's: A
        assert_eq!(
            m.update(&[(C, false), (A, true), (B, true)], &mut s),
            Some(1)
        );
    }
}
