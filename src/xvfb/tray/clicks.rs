//! A click on the desktop's icon, made on the X icon window.
//!
//! GNOME reaches an icon through its StatusNotifierItem: left click, double click, middle click, wheel.
//! The app behind an XEmbed icon knows none of that: it waits for button events on its icon window, and its
//! own menu is an override-redirect X window the reverse path shows on the desktop like any other popup.
//!
//! # Where the click lands
//!
//! GNOME's coordinates are the engine display's: with `-showwindows` every X window is placed at the
//! desktop position of the same rectangle (design §2, `MoveResize`), so a click the user makes at desktop
//! position `(gx, gy)` is the X root position `(gx, gy)` — the mapping the reverse path uses both ways
//! (`reverse::State::show` configures each shown window to the rectangle the desktop gives it, and
//! `State::place_popup` puts a popup back at the position the X client chose). So a click at `(gx, gy)`
//! whose pointer is `(px, py)` inside the icon puts the icon's container at `(gx - px, gy - py)`: the
//! icon's menu then opens where the app puts it on X (right under the icon), and the reverse path shows
//! it there.
//!
//! A click under GNOME's top bar has a negative `y`, where no X window goes: its button is pressed with
//! the container at the screen's top left corner (so that the app opens its menu on the screen, where the
//! reverse path can show it), and the tray moves the window that appeared to the click's position.
//!
//! # Where the input comes from
//!
//! The click is the tray's own X connection sending XTest requests, never the core pointer: the desktop's
//! pointer does not move and gets no button, so nothing of this can reach the desktop as input of its own.
//! The connection gives the reverse direction's master device as its ClientPointer (as the reverse path
//! does for every X client), where there is one. The container is raised over the agent's full-screen
//! window for the click (the only way the pointer can enter the icon at all) and lowered again after it.

/// The X buttons a wheel notch maps to.
pub const WHEEL_UP: u8 = 4;
pub const WHEEL_DOWN: u8 = 5;
pub const WHEEL_LEFT: u8 = 6;
pub const WHEEL_RIGHT: u8 = 7;

/// What the bus side asks the tray thread for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// A click with `button` (1 left, 2 middle, 3 right) at desktop position `(x, y)`, `None` where the
    /// call has no position (a menu of ours, the session's idea of the pointer).
    Click {
        container: u32,
        button: u8,
        x: Option<i32>,
        y: Option<i32>,
    },
    /// `steps` notches of the wheel (positive: up or left) at `(x, y)`.
    Scroll {
        container: u32,
        steps: i32,
        horizontal: bool,
        x: Option<i32>,
        y: Option<i32>,
    },
}

impl Command {
    /// The icon (its container) the command is for.
    pub fn container(&self) -> u32 {
        match self {
            Command::Click { container, .. } | Command::Scroll { container, .. } => *container,
        }
    }
}

/// One X event of a click sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// XTest motion to the icon (root coordinates).
    Move(i16, i16),
    Press(u8),
    Release(u8),
}

/// The X buttons a status icon's click becomes. XEmbed icons see plain button presses: a left click is
/// button 1, a right click button 3 (the context menu, which every toolkit binds), a middle click button 2.
pub fn buttons(button: u8) -> &'static [u8] {
    match button {
        1 => &[1],
        2 => &[2],
        3 => &[3],
        _ => &[],
    }
}

/// The steps of one click of `button` with the pointer at `(x, y)`.
pub fn click(x: i16, y: i16, button: u8) -> Vec<Step> {
    let mut steps = vec![Step::Move(x, y)];
    for b in buttons(button) {
        steps.push(Step::Press(*b));
        steps.push(Step::Release(*b));
    }
    steps
}

/// The wheel: one press and release per notch, in the direction of `org.kde.StatusNotifierItem`'s `Scroll`
/// (a positive `delta` is up or left, which is X's button 4 or 6).
pub fn scroll(x: i16, y: i16, delta: i32, horizontal: bool) -> Vec<Step> {
    let notches = (delta.abs() / 120).clamp(1, 10);
    let button = match (horizontal, delta > 0) {
        (false, true) => WHEEL_UP,
        (false, false) => WHEEL_DOWN,
        (true, true) => WHEEL_LEFT,
        (true, false) => WHEEL_RIGHT,
    };
    let mut steps = vec![Step::Move(x, y)];
    for _ in 0..notches {
        steps.push(Step::Press(button));
        steps.push(Step::Release(button));
    }
    steps
}

/// Where the container goes for a click at `(gx, gy)` of the desktop with the pointer at `(px, py)` inside
/// the icon: the icon's top left corner.
pub fn origin(gx: i32, gy: i32, px: i32, py: i32) -> (i32, i32) {
    (gx - px, gy - py)
}

/// Where a click with no position (a menu entry of ours) goes: the top right of the primary monitor's work
/// area, where the top bar's status icons are (or the monitor's, without one).
pub fn default_point(
    monitors: &[(i32, i32, i32, i32)],
    work_area: Option<(i32, i32, i32, i32)>,
) -> (i32, i32) {
    let primary = monitors.first().copied().unwrap_or((0, 0, 1024, 768));
    let (x, y, w, _) = work_area.unwrap_or(primary);
    (x + w - 64, y + 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Step::*;

    #[test]
    fn a_click_is_a_move_and_a_press() {
        assert_eq!(click(100, 50, 1), vec![Move(100, 50), Press(1), Release(1)]);
        assert_eq!(click(0, 0, 3), vec![Move(0, 0), Press(3), Release(3)]);
        assert_eq!(click(0, 0, 2), vec![Move(0, 0), Press(2), Release(2)]);
        // nothing known to do for another button
        assert_eq!(click(5, 5, 9), vec![Move(5, 5)]);
    }

    #[test]
    fn the_wheel_turns_both_ways() {
        // org.kde.StatusNotifierItem: up (which is a positive delta) is X's button 4
        assert_eq!(
            scroll(1, 1, 120, false),
            vec![Move(1, 1), Press(4), Release(4)]
        );
        assert_eq!(
            scroll(1, 1, -120, false),
            vec![Move(1, 1), Press(5), Release(5)]
        );
        assert_eq!(
            scroll(1, 1, 120, true),
            vec![Move(1, 1), Press(6), Release(6)]
        );
        assert_eq!(
            scroll(1, 1, -120, true),
            vec![Move(1, 1), Press(7), Release(7)]
        );
        // a small delta still turns once, a huge one is capped
        assert_eq!(scroll(1, 1, 1, false).len(), 3);
        assert_eq!(scroll(1, 1, 120 * 60, false).len(), 1 + 20);
    }

    #[test]
    fn the_click_point_is_where_the_pointer_was() {
        // the icon at 1200,5 on the desktop, the pointer 10,10 inside it
        assert_eq!(origin(1200, 5, 10, 10), (1190, -5));
        assert_eq!(origin(0, 0, 0, 0), (0, 0));
    }

    #[test]
    fn a_click_without_a_position_lands_in_the_top_right_corner() {
        let m = [(0, 0, 1280, 800)];
        assert_eq!(default_point(&m, Some((0, 0, 1280, 768))), (1216, 16));
        assert_eq!(default_point(&m, None), (1216, 16));
    }
}
