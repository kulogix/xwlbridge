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

## What the bridge does with it

With `-showwindows` (per-window mode; not `-showscreen`) the bridge tags each shown toplevel
`xwlbridge:<display>:<window id>`, finds the extension on the session bus and checks its version and features.
While the extension is there:

- **The X window moves the desktop window.** When an app moves or resizes its own top-level window (its own
  drag loop, an animation, a slide-in panel), the bridge sends `MoveResize` with the clamp to the work area of
  the monitor it moves to (a held-button drag instead gets mutter's user-move constraints, so it can cross
  monitors), coalesced to one per frame (the window's frame callback; at most one per 16 ms for a window that does
  not repaint) and one at a time. If the desktop keeps the window elsewhere (an edge, a size limit, a maximised
  window), the X window is put back there and gets the synthetic ConfigureNotify an X window manager sends
  (ICCCM 4.1.5). During a drag with a button held the X app is in charge; the window moves under the pointer and
  the X pointer is kept where the desktop's pointer is (the bridge works out which of the window's positions each
  pointer event is relative to: from the relative motion for a mouse or touchpad, from the pointer's track for a
  tablet, a VNC or RDP viewer's pointer and touch; a move not answered yet counts where the desktop's last
  replies held the window, so a drag past a screen edge follows too), so the app's drag loop sees the pointer
  where it is.
- **The desktop window moves the X window.** A user's move, tiling, maximising, a monitor change: the X window
  is configured to the same position (the size follows the desktop's configure, as before), with the synthetic
  ConfigureNotify.
- **Placement at map.** A window with USPosition, or PPosition other than 0,0, in WM_NORMAL_HINTS opens at that
  position (`SetInitialPosition`); other windows are placed by GNOME and the X window follows.
- **Stacking.** GNOME is the only stacking authority: X's stacking order of the shown windows follows
  `Restacked`, which lists the windows that are not showing (minimised, on another workspace) at the bottom:
  they stay mapped in X, and must not take the pointer of a window above them on the desktop. The window the
  desktop's pointer enters is also raised in X right away (as in 0.1.2), since the desktop's order arrives a
  little later over D-Bus. An app's raise (XRaiseWindow, seen as its window going over windows the desktop has above it) or
  `_NET_ACTIVE_WINDOW` request activates the window when the app had input (a key or button press on one of its
  windows) in the last 2 s; otherwise the window demands attention instead, at most once per 3 s per window, and
  X goes back to the desktop's order. `_NET_WM_STATE_ABOVE` and `_NET_WM_STATE_STAYS_ON_TOP` keep a window above
  (`SetAbove`), and its transient children with it, so a dialog never opens behind its own above parent.
- **Attention.** WM_HINTS UrgencyHint or `_NET_WM_STATE_DEMANDS_ATTENTION` -> `SetDemandsAttention`. When
  the window gets the keyboard, the bridge removes `_NET_WM_STATE_DEMANDS_ATTENTION` (EWMH makes that the
  window manager's job).
- **Always on Top.** The desktop's "Always on Top" on a shown window becomes its `_NET_WM_STATE_ABOVE`, so the
  user's choice also survives the lock screen.
- **Resync.** At start, on `MonitorsChanged` and whenever the extension comes back (the lock screen turns it
  off and on again, as does a shell restart), the bridge reads `GetMonitors` and `GetState` and puts every X
  window where the desktop has it. Replies and signals are put in the desktop's order by their D-Bus message
  serials (they all leave gnome-shell on one connection), so a late reply never undoes a newer report.
- **Layout.** The sync is on only while the desktop's monitors are unscaled and lie inside the X screen (X
  positions are the desktop's, and the pointer reaches only the X screen): on a scaled desktop, or while a
  fixed-size X screen (Xvfb) is smaller than the monitors, windows keep the 0.1.2 behaviour, and the log says
  why.

While it is not there, `-showwindows` behaves exactly as 0.1.2 did (and `_NET_SUPPORTED` announces only what
0.1.2 carries out). The log says which: `window sync with the desktop: extension version 1, ...` or
`no window sync with the desktop (...): windows keep the 0.1.2 behaviour`.

Not yet supported: HiDPI scaling (a scaled desktop keeps the 0.1.2 behaviour), workspaces, sticky, minimise
through the extension, keep-below (mutter has no call for it).

The bridge accepts the name `org.xwlbridge.Windows` only from gnome-shell's own connection (the owner of
`org.gnome.Shell`): the name is free while the screen is locked, and another process could take it.

## Security model

- **Only windows the caller created.** A window is reachable only when it is a Wayland toplevel (not an X11
  window of GNOME's own Xwayland, not override-redirect), its Wayland client tagged it with
  `xdg_toplevel_tag_v1`, and the tag starts with `xwlbridge:` (the bridge uses `xwlbridge:<display>:<xid>`).
  Windows are found by that tag, never by title, app id or position. Tagging is unprivileged, so any process
  can tag its own windows and then move them, keep them above or raise them, as it could with any X window
  manager hint; what it cannot do is reach another process's windows (below) or take the keyboard from
  another app (`Activate`).
- **Only by the process that owns the window.** Every method that names or lists windows asks the bus for
  the caller's process id (`org.freedesktop.DBus.GetConnectionUnixProcessID`) and serves only windows whose
  Wayland client has the same process id (`Meta.Window.get_pid`). Another process's window, tagged or not,
  answers "no such window", exactly like a tag that does not exist, so a caller cannot even probe for
  other windows. Two bridges on one desktop each see only their own windows.
- **Signals are private.** `Changed`, `Restacked` and `WindowGone` go only to the bus names of the window's
  own process (unicast), never to the whole bus.
- **No focus stealing.** `Activate` needs the timestamp of the input it follows (0 is refused) and focuses the
  window only while the keyboard is on no window or on one of the caller's own; otherwise the window demands
  attention. A timestamp alone would not do: compositor time is the monotonic clock in ms, easy to make up.
- **Nothing else.** `Version`, `Features`, `GetMonitors` and `GetPointer` answer anyone: they carry nothing private (the
  monitor layout is public through GNOME's own D-Bus APIs). Arguments are range-checked; at most 64 callers
  are remembered (4 per process; a process crowds out only its own) and 256 waiting initial positions (64 per
  process; each expires after 60 s).
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
| `Features() -> as` | `move-resize`, `clamp-work-area`, `initial-position`, `activate`, `raise`, `above`, `demands-attention`, `stick`, `workspace`, `minimize`, `state`, `monitors`, `restacked`, `pointer`; empty on a mutter without window tags |
| `MoveResize(s tag, i x, i y, i w, i h, u flags) -> (iiii)` | Moves and resizes. `w` or `h` 0 keeps the size. Flags: 1 = clamp into the work area of the monitor the rectangle is (mostly) on (shrinking it to fit; a rectangle that is exactly a monitor is left alone), 2 = user-initiated (mutter's user-action constraints). Returns the rectangle applied: a move is applied at once and the reply is mutter's result; a new size waits for the app to draw it, so the reply is the rectangle asked for after the clamp, and `Changed` carries the final one. A maximized or fullscreen window is not changed; the reply is its rectangle. |
| `SetInitialPosition(s tag, i x, i y)` | Where the window opens. Before the window exists it waits (up to 60 s) and is applied when GNOME first shows it, after GNOME's own placement and before the first frame is drawn, so the window appears there without a jump. For a window already shown, it moves it now. |
| `Activate(s tag, u timestamp)` | Raises, focuses and switches to the window's workspace, if the keyboard is on no window or on one of the caller's; otherwise the window demands attention. `timestamp` is compositor time in ms (the time of the input event that led to it, e.g. a `wl_pointer`/`wl_keyboard` event); 0 is refused (`InvalidArgs`). GNOME's focus-stealing prevention applies to old timestamps. |
| `Raise(s tag)` | Raises within its layer. |
| `SetAbove(s tag, b)` | Always on top on / off. |
| `SetDemandsAttention(s tag, b)` | GNOME's "is ready" attention on / off. |
| `Stick(s tag, b)` | On all workspaces on / off. |
| `SetWorkspace(s tag, i index)` | Moves it to that workspace (0-based; an index that does not exist is an error). |
| `Minimize(s tag, b)` | Minimize / restore. |
| `GetState() -> a(sa{sv})` | The caller's tagged windows, bottom to top: tag and `rect (iiii)`, `monitor i`, `workspace i` (-1 = all), `minimized b`, `maximized b`, `fullscreen b`, `above b`, `focused b`, `user b` (a user grab is active), `shown b`, `stack u` (0 = bottom), `flags u` and `serial u` (as in `Changed`). |
| `GetMonitors() -> a(i(iiii)(iiii)db)` | Per monitor: index, geometry, work area (without panels), scale, primary. |
| `GetPointer() -> (iib)` | The pointer's position and whether a button is held (`Features` lists `pointer`). The session's status icons use it to open an X app's own menu where the menu was clicked. |

| signal | |
|---|---|
| `Changed(s tag, (iiii) rect, i monitor, i workspace, u flags, u serial)` | The window moved, was resized or changed state. Flags: 1 = a user grab (interactive move or resize) is active on it, 2 minimized, 4 maximized, 8 fullscreen, 16 above, 32 focused. Changes are coalesced (one per window per main-loop turn, only when something differs); `serial` grows with every `Changed`. Sent from the first show on. |
| `Restacked(as tags)` | The stacking order of the caller's windows changed (bottom to top; windows that are not showing - minimised, on another workspace - first). |
| `MonitorsChanged()` | Monitors or work areas changed: call `GetMonitors`. |
| `WindowGone(s tag)` | The window was closed or lost its tag. |

Errors: `org.freedesktop.DBus.Error.InvalidArgs` (a tag without the `xwlbridge:` prefix, an out-of-range
position, size or workspace), `org.xwlbridge.Windows1.Error.NoSuchWindow` (no such window of the caller),
`org.freedesktop.DBus.Error.AccessDenied` (the bus could not name the caller's process).

**Receiving signals.** The extension sends signals to the bus names that have called it (any method but
`Version` and `Features`). A client subscribes to the signals (a match rule on the sender
`org.xwlbridge.Windows`) and then calls `GetState()` for a full picture. When the name's owner changes (GNOME restarted,
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
Not covered here: a real user drag (the `user` flag), several workspaces.

The bridge's side: `src/xvfb/winsync.rs` unit-tests the sync's decisions, and
`contrib/gnome-shell/tests/run_bridge_sync.sh BRIDGE [sync|off]` runs a bridge binary with `-showwindows` against
this extension in a scratch headless gnome-shell (xlogo and xeyes moved, resized, raised and flagged from the X
side, and from the desktop by a probe extension): placement at map, self-moves and the clamp, desktop moves,
an animation's coalescing, a self-resize, stacking both ways, raise gating, keep-above, the extension going and
coming back (as at the lock screen) and maximise (22 checks); `off` checks the 0.1.2 behaviour with
`XWLBRIDGE_WINDOWS_EXTENSION=off` (5 checks). A held-button drag is not covered: the headless shell has no
pointer; `src/xvfb/selfdrag.rs` checks the bridge's rule for it against a simulation of the compositor.
