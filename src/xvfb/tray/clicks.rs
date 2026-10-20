//! A click on the desktop's icon, made on the X icon window.
//!
//! GNOME reaches an icon through its StatusNotifierItem: left click, double click, middle click, wheel.
//! The app behind an XEmbed icon knows none of that: it waits for button events on its icon window, and its
//! own menu is an override-redirect X window the reverse path shows on the desktop like any other popup
//! (next to the icon, in the top bar). So a click becomes what an X panel would have sent the icon, through
//! the same XTest the desktop's input is injected with on the engine display's reverse master device (so
//! input attribution sees the bridge's own input, not a foreign client's):
//!
//! | on the desktop | on the icon |
//! |---|---|
//! | left click (single) | a left click |
//! | double click | a left double click: press, release, press, release |
//! | middle click | a middle click (the app's own menu, for a `Gtk.StatusIcon`) |
//! | wheel up/down/left/right | buttons 4/5/6/7, once per notch |
//!
//! A click is made in the middle of the icon, well inside it, from a pointer that was taken there without
//! the pointer really moving on the desktop (the tray raises the icon over the agent's shield for it, see
//! [`over`]).

/// The X buttons a wheel notch maps to.
pub const WHEEL_UP: u8 = 4;
pub const WHEEL_DOWN: u8 = 5;
pub const WHEEL_LEFT: u8 = 6;
pub const WHEEL_RIGHT: u8 = 7;

/// What the tray thread is asked to do (see [`super::Tray`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Click {
        container: u32,
        button: u8,
        double: bool,
    },
    Scroll {
        container: u32,
        steps: i32,
        horizontal: bool,
    },
}

/// One X event of a click sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// XTest motion to the icon's middle (root coordinates).
    Move(i16, i16),
    Press(u8),
    Release(u8),
}

/// The clicks a StatusNotifierItem asks for.
///
/// `double` is the app indicator protocol's double click (`Activate` and `SecondaryActivate` both carry the
/// coordinates of a double click; a single click arrives with a pointer position and no double click). A
/// right click is the menu, which the app shows itself when it gets the middle button (the convention an X
/// panel follows for a `Gtk.StatusIcon`).
pub fn click(button: u8, double: bool) -> Vec<Step> {
    let clicks = if double { 2 } else { 1 };
    let mut steps = Vec::with_capacity(clicks * 2);
    for _ in 0..clicks {
        steps.push(Step::Press(button));
        steps.push(Step::Release(button));
    }
    steps
}

/// The wheel: one press and release per notch, in the direction of `org.kde.StatusNotifierItem`'s `Scroll`
/// (a positive `delta` is up or left, which is X's button 4 or 6).
pub fn scroll(delta: i32, horizontal: bool) -> Vec<Step> {
    let notches = (delta.abs() / 120).clamp(1, 10) as usize;
    let button = match (horizontal, delta > 0) {
        (false, true) => WHEEL_UP,
        (false, false) => WHEEL_DOWN,
        (true, true) => WHEEL_LEFT,
        (true, false) => WHEEL_RIGHT,
    };
    std::iter::repeat_n([Step::Press(button), Step::Release(button)], notches)
        .flatten()
        .collect()
}

/// The middle of an icon `size` at `(x, y)`, where a click goes.
pub fn middle(x: i16, y: i16, width: u16, height: u16) -> (i16, i16) {
    (
        x.saturating_add((width / 2) as i16),
        y.saturating_add((height / 2) as i16),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use Step::*;

    #[test]
    fn a_click_is_a_press_and_a_release() {
        assert_eq!(click(1, false), vec![Press(1), Release(1)]);
        assert_eq!(click(2, false), vec![Press(2), Release(2)]);
    }

    #[test]
    fn a_double_click_is_two() {
        assert_eq!(
            click(1, true),
            vec![Press(1), Release(1), Press(1), Release(1)]
        );
    }

    #[test]
    fn the_wheel_turns_both_ways() {
        // org.kde.StatusNotifierItem: up (which is a positive delta) is X's button 4
        assert_eq!(scroll(120, false), vec![Press(4), Release(4)]);
        assert_eq!(scroll(-120, false), vec![Press(5), Release(5)]);
        assert_eq!(scroll(120, true), vec![Press(6), Release(6)]);
        assert_eq!(scroll(-120, true), vec![Press(7), Release(7)]);
        // a small delta still turns once, a huge one is capped
        assert_eq!(scroll(1, false).len(), 2);
        assert_eq!(scroll(120 * 60, false).len(), 20);
    }

    #[test]
    fn the_middle_of_an_icon() {
        assert_eq!(middle(100, 50, 22, 22), (111, 61));
        assert_eq!(middle(-200, -200, 16, 16), (-192, -192));
    }
}
