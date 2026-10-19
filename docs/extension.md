# The window extension (xwlbridge-windows@xwlbridge)

A Wayland app cannot place its own windows or learn where they are. With `-showwindows` each X window is a
Wayland window on the GNOME desktop, so without help:

- an X window that moves or resizes itself (a dock or palette dragged by its own handle, a slide-in panel, an
  animation) moves only inside X, not on the desktop;
- X windows open where GNOME puts them, not where the app asks (session restore, saved layouts);
- the X app does not learn where GNOME put its window, so popups and pointer positions can be off.

`xwlbridge-windows@xwlbridge` is a GNOME Shell extension (GNOME 49 and 50) that gives the bridge this help
through a small D-Bus service. It changes nothing for other windows and does nothing until a bridge asks.

## Install and opt out

`install.sh` installs it into `~/.local/share/gnome-shell/extensions/` and enables it. **GNOME on Wayland loads
a newly installed extension at login only: log out and in once.** Upgrades replace the files in place, and
GNOME runs the new version after the next login. If you turned the extension off in the Extensions app, an
upgrade keeps it off.

To go without it:

- at install: `./install.sh --no-extension` (removes a copy an earlier install put there), or
- later: `xwlbridge-ctl config set extension off`, then run `install.sh` again.

`extension=off` also tells the bridge not to use the extension where it is running anyway
(`xwlbridge-ctl run` gives the bridge `XWLBRIDGE_WINDOWS_EXTENSION=off`). Without the extension, `-showwindows`
works as in 0.1.2. `uninstall.sh` disables and removes it.

`xwlbridge-ctl doctor` reports whether it is installed, enabled and running, and its version. On a GNOME older
than 49, `install.sh` leaves it out. The Debian package installs it system-wide
(`/usr/share/gnome-shell/extensions/`); each user enables it with `gnome-extensions enable
xwlbridge-windows@xwlbridge` and logs in again.

If remote unlock or this extension stopped working after a gnome-shell crash, GNOME may have turned off all
user extensions: `gsettings get org.gnome.shell disable-user-extensions` shows `true`; set it back to `false`.

## Security model

- **Only the bridge's own windows.** A window is reachable only when its Wayland client tagged it with
  `xdg_toplevel_tag_v1` and the tag starts with `xwlbridge:` (the bridge uses `xwlbridge:<display>:<xid>`).
  Windows are found by that tag, never by title, app id or position.
- **Only by the process that owns the window.** Every method that names or lists windows asks the bus for
  the caller's process id (`org.freedesktop.DBus.GetConnectionUnixProcessID`) and serves only windows whose
  Wayland client has the same process id (`Meta.Window.get_pid`). Another process's window, tagged or not,
  answers "no such window", exactly like a tag that does not exist, so a caller cannot even probe for
  other windows. Two bridges on one desktop each see only their own windows.
- **Signals are private.** `Changed`, `Restacked` and `WindowGone` go only to the bus names of the window's
  own process (unicast), never to the whole bus.
- **Nothing else.** `Version`, `Features` and `GetMonitors` answer anyone: they carry nothing private (the
  monitor layout is public through GNOME's own D-Bus APIs). Arguments are range-checked; at most 32 callers
  are remembered and 256 waiting initial positions (each expires after 60 s).
- **Lock screen.** GNOME disables extensions while the screen is locked; the service then goes away and comes
  back at unlock, and the bridge resyncs.

## D-Bus API

Session bus, name `org.xwlbridge.Windows`, object `/org/xwlbridge/Windows`, interface `org.xwlbridge.Windows1`.
Rectangles are `(x, y, width, height)` in logical desktop pixels (GNOME's layout coordinates) of the window
geometry: the client area the app draws, without shadows. For Wayland windows that is mutter's frame
rectangle.

| method | |
|---|---|
| `Version() -> u` | API version: 1 |
| `Features() -> as` | `move-resize`, `clamp-work-area`, `initial-position`, `activate`, `raise`, `above`, `demands-attention`, `stick`, `workspace`, `minimize`, `state`, `monitors`, `restacked`; empty on a mutter without window tags |
| `MoveResize(s tag, i x, i y, i w, i h, u flags) -> (iiii)` | Moves and resizes. `w` or `h` 0 keeps the size. Flags: 1 = clamp into the work area of the window's monitor (shrinking it to fit), 2 = user-initiated (mutter's user-action constraints). Returns the rectangle applied: a move is applied at once and the reply is mutter's result; a new size waits for the app to draw it, so the reply is the rectangle asked for after the clamp, and `Changed` carries the final one. A maximized or fullscreen window is not changed; the reply is its rectangle. |
| `SetInitialPosition(s tag, i x, i y)` | Where the window opens. Before the window exists it waits (up to 60 s) and is applied when GNOME first shows it, after GNOME's own placement and before the first frame is drawn, so the window appears there without a jump. For a window already shown, it moves it now. |
| `Activate(s tag, u timestamp)` | Raises, focuses and switches to the window's workspace. `timestamp` is compositor time in ms (the time of the input event that led to it, e.g. a `wl_pointer`/`wl_keyboard` event), 0 = now. GNOME's focus-stealing prevention applies to old timestamps. |
| `Raise(s tag)` | Raises within its layer. |
| `SetAbove(s tag, b)` | Always on top on / off. |
| `SetDemandsAttention(s tag, b)` | GNOME's "is ready" attention on / off. |
| `Stick(s tag, b)` | On all workspaces on / off. |
| `SetWorkspace(s tag, i index)` | Moves it to that workspace (0-based; an index that does not exist is an error). |
| `Minimize(s tag, b)` | Minimize / restore. |
| `GetState() -> a(sa{sv})` | The caller's tagged windows, bottom to top: tag and `rect (iiii)`, `monitor i`, `workspace i` (-1 = all), `minimized b`, `maximized b`, `fullscreen b`, `above b`, `focused b`, `user b` (a user grab is active), `shown b`, `stack u` (0 = bottom), `flags u` and `serial u` (as in `Changed`). |
| `GetMonitors() -> a(i(iiii)(iiii)db)` | Per monitor: index, geometry, work area (without panels), scale, primary. |

| signal | |
|---|---|
| `Changed(s tag, (iiii) rect, i monitor, i workspace, u flags, u serial)` | The window moved, was resized or changed state. Flags: 1 = a user grab (interactive move or resize) is active on it, 2 minimized, 4 maximized, 8 fullscreen, 16 above, 32 focused. Changes are coalesced (one per window per main-loop turn, only when something differs); `serial` grows with every `Changed`. Sent from the first show on. |
| `Restacked(as tags)` | The stacking order of the caller's windows changed (bottom to top). |
| `MonitorsChanged()` | Monitors or work areas changed: call `GetMonitors`. |
| `WindowGone(s tag)` | The window was closed or lost its tag. |

Errors: `org.freedesktop.DBus.Error.InvalidArgs` (a tag without the `xwlbridge:` prefix, an out-of-range
position, size or workspace), `org.xwlbridge.Windows1.Error.NoSuchWindow` (no such window of the caller),
`org.freedesktop.DBus.Error.AccessDenied` (the bus could not name the caller's process).

**Receiving signals.** The extension sends signals to the bus names that have called it. A client subscribes
to the signals (a match rule on the sender `org.xwlbridge.Windows`) and then calls any method, for example
`Version()` at start-up and `GetState()` for a full picture. When the name's owner changes (GNOME restarted,
the screen was locked and unlocked, the extension was turned off and on), the client does the same again:
the new instance knows nothing of earlier callers.

Unknown flag bits are ignored, so later versions can add flags without breaking callers.

## Tested

`contrib/gnome-shell/tests/run_windows_ext.sh` runs the extension in a real, headless gnome-shell 50.1 with two
virtual monitors, in a scratch session (container or VM; it refuses to run inside a desktop session), with a
Wayland client that tags its toplevels and calls the API from the same process: every method, the clamp, the
initial position (the first painted frame is at the asked position: checked with a paint probe), the process
and tag checks from other processes, unicast signals, stacking, window removal, and disable/enable as on the
lock screen, including a window minimized across it (50 checks). `contrib/gnome-shell/tests/run_install_flow.sh`
runs a release bundle's `install.sh`, then GNOME Shell ("the next login": the extension loads by itself),
`xwlbridge-ctl doctor` and `uninstall.sh` (the running shell stops the extension; files and settings entries
are gone). `packaging/tests/test_ctl.sh` covers the installer's choices with fakes (opt-out, upgrades, a user
who turned the extension off, GNOME older than 49, settings that cannot be written).
Not covered: a real user drag (the `user` flag), several workspaces, the bridge itself (it does not use the
extension yet in this version).
