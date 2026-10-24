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

/// The steps of one click of `button` with the pointer at `(x, y)` of the container (which is where the
/// click's X position is: see [`Origin`]).
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

/// The click's X point, inside the container: the middle of the icon.
pub fn target(size: (u16, u16)) -> (i16, i16) {
    ((size.0 / 2) as i16, (size.1 / 2) as i16)
}

/// Where the icon goes for a click at desktop `(gx, gy)` with the click at `target` inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The container at this root position (the desktop shows it there): the click's X point is the
    /// desktop's point.
    Container(i32, i32),
    /// No window goes to the click's point (under the top bar: the desktop's top left corner is below it):
    /// the container stays at [`crate::xvfb::tray::dock::PARK`] and the reverse path opens the menu the app
    /// pops up at the point it was asked to (`winsync::initial_position`).
    Asked(i16, i16),
}

/// How a click at desktop `(gx, gy)` is made on an icon with the click at `target` inside it: the
/// container is moved so that the click's X point is the desktop's point (design §2: the desktop shows
/// every X window at its own position), or, where no window goes, the click goes to the desktop's point.
///
/// `(gx, gy)` is the desktop's (logical) position --- a StatusNotifierItem's `Activate(x, y)`, or the
/// extension's pointer --- and the container, the XTest click and `target` are X pixels, so the point is
/// converted with S here (the one conversion between the two spaces).
pub fn origin(gx: i32, gy: i32, target: (i16, i16), scale: u32) -> Origin {
    let s = scale.max(1) as i32;
    let (gx, gy) = (gx.saturating_mul(s), gy.saturating_mul(s));
    let (tx, ty) = (i32::from(target.0), i32::from(target.1));
    if gx >= 0 && gy >= 0 {
        Origin::Container(gx - tx, gy - ty)
    } else {
        let clamp = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        Origin::Asked(clamp(gx), clamp(gy))
    }
}

/// A move of an X client that puts the icon under a desktop position, and its undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// The core pointer (with XTest): to this root position, with `window` (a parked icon) as the pointer's
    /// window for the length of the motion.
    Pointer(i16, i16, u32),
    /// A window: back to this root position.
    Window(u32, i16, i16),
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
    fn the_click_point_is_where_the_desktop_was_clicked() {
        // the click 9,4 inside the icon, at 1600,12 on the desktop: the icon's X point 9,4 is the
        // desktop's 1600,12
        assert_eq!(origin(1600, 12, (9, 4), 1), Origin::Container(1591, 8));
        assert_eq!(origin(0, 0, (0, 0), 1), Origin::Container(0, 0));
        // under the top bar no window goes: the click is made where it is and the app's menu is asked to
        // open there
        assert_eq!(origin(1200, -11, (9, 4), 1), Origin::Asked(1200, -11));
        assert_eq!(origin(-3, 5, (1, 1), 1), Origin::Asked(-3, 5));
        // at 2x the desktop's logical point is the X point: the icon's
        // container and the XTest click move with S too
        assert_eq!(origin(800, 12, (9, 4), 2), Origin::Container(1591, 20));
        assert_eq!(origin(1200, -11, (9, 4), 2), Origin::Asked(2400, -22));
    }

    #[test]
    fn a_click_without_a_position_lands_in_the_top_right_corner() {
        let m = [(0, 0, 1280, 800)];
        assert_eq!(default_point(&m, Some((0, 0, 1280, 768))), (1216, 16));
        assert_eq!(default_point(&m, None), (1216, 16));
    }
}
