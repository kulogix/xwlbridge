# Changelog

All notable changes to xwlbridge are listed here. The format follows Keep a Changelog.

## 0.3.0 (2026-10-25)

### Added

- Live display changes. The bridge's RandR monitors follow GNOME's: hotplugging a monitor, changing its mode, resolution or scale, rotating it, mirroring or joining, and changing which monitor is primary. The X screen is the monitors' bounding box; a window that a change left outside it is moved back in, and the window sync recovers after the change instead of staying off. The X primary output follows GNOME's primary, including after a primary-only change. `_NET_WORKAREA` and `_GTK_WORKAREAS_D0` are published while the window sync is on (one work area per monitor) and removed when it goes off, so GTK 3/4, Qt and Java know where the desktop's panels and dock are.
- `-showwindows`: window decorations. Each shown X window gets the frame a window manager would give it on X: a GNOME-style header with the title and GNOME's button layout, double-click to maximise, a right-click window menu, and invisible resize edges. A press on the header starts GNOME's own move or resize, so there is no bridge-side drag loop. The header follows GNOME's live button layout, titlebar actions and font, colour scheme and accent through the settings portal, with dconf as the fallback; `-windowbuttons LAYOUT` overrides the button layout. Windows that draw their own title bar (`_GTK_FRAME_EXTENTS`, `_MOTIF_WM_HINTS` decorations 0) keep it, and a maximised or fullscreen window's header is removed and restored with the window. `-nodecorations` or `decorations=off` restores the 0.2.0 look.
- `-showwindows`: an XEmbed system tray. The bridge owns `_NET_SYSTEM_TRAY_S0` on its display and shows each X tray icon (Java, Wine, GTK, Qt, ...) on GNOME's top bar as a StatusNotifierItem, with the application's own icon, tooltip, menu (*Open*, *Menu*, *Middle click*) and its balloon messages as notifications; clicking the icon in the top bar clicks the icon on the X display. It needs GNOME's AppIndicator extension ([docs/tray.md](docs/tray.md)); `-notray` (setting `tray`) leaves the tray alone as 0.2.0 did, and `xwlbridge-ctl doctor` reports whether a status icon can be shown.
- `-showwindows`: HiDPI and mixed DPI. The bridge follows Xwayland's native-scaling model: the X screen and X windows are the desktop's logical layout times S (the largest monitor scale, rounded up), shown surfaces commit S-times buffers and the compositor scales them to the monitor, and input is converted back through the same S. The engine display's XSETTINGS manager mirrors the same 53-key table `gsd-xsettings` publishes on GNOME's Xwayland (theme, fonts, cursor, DPI, window scale, decoration layout, titlebar actions, thresholds, ...), live and key-for-key, with the same `RESOURCE_MANAGER`; the bridge also owns `_NET_WM_CM_S0` and advertises `_GTK_FRAME_EXTENTS`, so GTK keeps its client-side shadow margins. GTK 3/4, Qt 6 and Xft apps get the right physical size at 1x and 2x, and a window moved between monitors of different scales is resampled by the compositor as on Xwayland. `-nohidpi` (setting `hidpi`, or `XWLBRIDGE_HIDPI=off`) restores the 0.2.0 behaviour.
- `-showwindows`: window types and transients. A client's panels, toolbars, menus and dialogs (UTILITY, TOOLBAR, DIALOG or MENU windows, and modal windows) belong to its main window, so they stay above it and are not treated as separate applications. A focused transient that goes away gives the keyboard back to its parent, as an X window manager does. Client-side-decorated windows use their `_GTK_FRAME_EXTENTS` shadow margins as the resize border, and a window that a client moves or grows off the X screen is put back on it.
- `-showwindows`: games. On the Xvnc engine the bridge asks Xvnc for one RandR output per desktop monitor, so a game sees the desktop's monitors instead of one bounding box. A game's mode change on its own output is emulated: the mode is kept, the game is shown letterboxed on its monitor with the aspect kept, desktop clicks are mapped through the letterbox to the game's pixels, and the saved mode is restored when the game unfullscreens, is minimised, loses the keyboard, exits or its monitor changes. Other outputs and the X screen stay as they are; a mode change with no fullscreen game on that monitor is refused. The CPU composition is the default; the GPU presentation path is available with `XWLBRIDGE_GAME_PRESENT=gpu` (or `gpu-sync`/`gpu-async`) for compositors where it works.
- `xwlbridge-ctl doctor` reports the window decorations (cairo/pango, live settings, button layout, fonts and accent), the tray's StatusNotifierWatcher and the window extension's features and version.

### Changed

- Monitor changes are coalesced: a burst of changes no longer restarts the remote-desktop session for each step. A colour-only change (HDR/RGB range) keeps the session, its devices and its streams; the session is restarted only when the X layout the backend must serve actually changes. A scale change no longer turns the window sync off, and a scaled desktop no longer makes the bridge fall back to the 0.2.0 behaviour unless HiDPI is turned off.
- The bridge uses one RandR output and CRTC per monitor on Xvnc, with GNOME's connector names and primary. The forward display and input visible to a VNC viewer are unchanged (one whole-screen framebuffer), but X clients, games and screen tools now see the desktop's monitor layout.
- The X screen's work-area properties are the desktop's: `_NET_WORKAREA` is one rectangle and `_GTK_WORKAREAS_D0` holds one rectangle per monitor; they are announced in `_NET_SUPPORTED` only while they are true.

### Fixed

- `-showwindows`: monitor changes that arrived while the window sync was off were dropped, so a scale round trip, an unplug or a shrink could leave the sync off; they are handled now, and the sync comes back on.
- `-showwindows`: an X window left outside a shrunken X screen, or moved there by its client, is fitted back into the X screen.
- `-showwindows`: the X primary stays GNOME's primary after a primary change on mutter 50.1, whose primary flags go stale (both monitors report primary after the first change); the bridge follows the flag history and, when that cannot tell, asks the window extension, which reads GNOME Shell's own answer.
- `-showwindows`: a decorated window now restores to exactly its pre-maximise rectangle on its own monitor after unmaximise, at 1x and 2x, with decorations on and off. The header no longer commits an empty surface, which consumed the pending configure acknowledgement and made mutter save the wrong restore position, and a stale desktop report from the maximised surface is no longer followed.
- `-showwindows`: GIMP's decorated main window and its dialogs (Open Location, export, tooltips) work again: the restore position above was what moved the File menu off the window, and a focused transient now gives the keyboard back so Escape and typing reach the main window.
- `-showwindows`: a GTK 4 client-side-decorated window resizes from its shadow margin, as it does on GNOME's Xwayland, instead of ignoring the drag inside the content.
- `-showwindows`: at 2x, header hit-testing, moves, resizes, maximises, the game letterbox and game input use the logical/X conversion once, so the 0.3.0 features work together on a scaled desktop.
- A slow X client (x11vnc asking for a large image during a layout storm) is no longer dropped at the queue cap and no longer takes the bridge down: its replies get backpressure, server events that do not fit are dropped, and the write timeout is gone.
- HDR/RGB-range changes keep the captured stream pinned to 8-bit sRGB and do not renegotiate it; the final picture is still bit-exact against mutter, and the bridge's memory stays flat across repeated toggles.

### Known limitations

- mutter 50.1 keeps a stale primary flag after a primary change; it is worked around from the flag history and GNOME Shell's own answer through the window extension.
- A Wayland window cannot be fullscreen across several monitors: a fullscreen window covers one monitor, and `_NET_WM_FULLSCREEN_MONITORS` is not supported.
- HDR reaches a VNC viewer tone-mapped to SDR; Night Light and ICC colour profiles are not visible remotely.
- Wine chooses its own DPI at 2x: it reads neither `Xft.dpi` nor XSETTINGS, so its windows can differ from native apps; set `LogPixels` in the Wine prefix to pin it.
- The GPU presentation path for games is opt-in (`XWLBRIDGE_GAME_PRESENT=gpu`); the default CPU composition is the measured, reliable path.

## 0.2.0 (2026-10-19)

### Added

- GNOME Shell extension `xwlbridge-windows@xwlbridge` (GNOME 49 and 50): a D-Bus service through which the bridge can move, resize, place, activate and restack the windows `-showwindows` shows, and learn about desktop-side moves, maximising, stacking and focus (see [docs/extension.md](docs/extension.md)). It serves only Wayland windows tagged `xwlbridge:` and only to the process that created them, and it never lets a caller take the keyboard from another app. `install.sh` installs and enables it (one log-out/log-in on Wayland), `--no-extension` or the setting `extension=off` opt out, `uninstall.sh` removes it and `xwlbridge-ctl doctor` reports it.
- `-showwindows` with the extension running (window sync, see [docs/extension.md](docs/extension.md)): an X window and its desktop window keep one position.
  - A window that moves or resizes itself (its own drag handle - a Qt dock, a palette -, a slide-in panel, an animation) moves on the desktop too, at most once per frame and kept inside the work area of the monitor it moves to; at an edge the X window is put back where the desktop keeps it, with the ConfigureNotify an X window manager sends. A held-button self-drag needs no desktop grab: the window moves under the pointer and the drag stays with the app, and the X pointer stays where the desktop's is, with a mouse, a tablet or a remote (VNC, RDP) pointer alike, also while the desktop holds the window at a screen edge (a remote pointer that repeats positions or steps unevenly stays within a few steps of it).
  - The desktop's moves (dragging, tiling, maximising, another monitor) reach the X window, so apps know where they are and popups open in the right place.
  - A window that asks for a position (USPosition, or a PPosition other than 0,0: session restore, saved layouts) opens there instead of where GNOME would put it.
  - The X stacking order follows the desktop's. An app's raise or `_NET_ACTIVE_WINDOW` request activates its window only after recent input in that app; otherwise the window demands attention (at most once per 3 s), so an X app cannot steal the focus or start a raise loop. Minimised windows and windows on other workspaces are kept at the bottom of the X stack, so they never take the pointer.
  - `_NET_WM_STATE_ABOVE` (and `_NET_WM_STATE_STAYS_ON_TOP`) keeps a window above others, together with its transient dialogs; WM_HINTS urgency and `_NET_WM_STATE_DEMANDS_ATTENTION` raise GNOME's "is ready" notice.
  - The desktop's "Always on Top" reaches the X window as `_NET_WM_STATE_ABOVE`; `_NET_WM_STATE_DEMANDS_ATTENTION` is removed when the window gets the keyboard.
  - Without the extension (not installed, turned off with `extension=off`, the lock screen, an older version, a build without `gnome`), on a scaled desktop, or while the X screen does not hold the desktop's monitors (a fixed-size Xvfb after a monitor was added), the bridge behaves as 0.1.2; it picks the sync up again when that changes. Only gnome-shell's own connection is accepted as the extension.

### Fixed

- `-showwindows`: the compositor's liveness pings are answered on a thread of their own, so an X client holding a server grab (a screenshot tool selecting a region) can no longer make GNOME offer to force-quit the bridge - which would have closed every X app on the display at once.
- X bells on the stock engines (Xvfb, Xvnc: -showwindows, -showscreen and the RealVNC service) ring the desktop's system bell, as the built-in engine already did; before, they made no sound at all. A bell rung for its event only (XkbBell event_only) stays silent, and GNOME's audible/visual bell settings apply.

## 0.1.2 (2026-10-16)

### Fixed

- `-showwindows`: dragging a window that moves itself (a Qt dock such as OBS's, a toolkit's drag handle) no longer loses the drag: 0.1.1 handed it to the desktop's interactive move, whose grab took the pointer away mid-drag, so a Qt dock re-docked and the main window could be dragged off-screen. The client's own move now carries the drag inside X, with the pointer mapping held steady until release; the window does not follow on the desktop (as in 0.1.0). `_NET_WM_MOVERESIZE` requests still start the desktop's move.
- `-showwindows`: Wine (and other clients that advertise WM_TAKE_FOCUS with input false) get the X input focus again, and WM_TAKE_FOCUS is sent when advertised; leaving all shown windows still clears the focus.
- `-showwindows`: popups are shown where the X client placed them again (no slide onto the monitor): the 0.1.1 slide let a tall menu move 25 px while the client kept its own position, so a maximised LibreOffice opened Character for Format > Paragraph...; a popup that does not fit is clipped instead. Popups are anchored to the parent's current position.

## 0.1.1 (2026-10-15)

### Added

- `-showwindows`: extra mouse buttons are injected, numbered as Xwayland numbers them (back = 8, forward = 9, then 10); buttons 11 and up are not delivered (the reverse master's XTest device has 10).
- `-showwindows`: a touch screen drives the reverse pointer Xwayland-style: the first touch point moves it and holds button 1 until the lift; a second point down suppresses the emulation.
- `-showwindows`: a pen or eraser drives the reverse pointer while it is over a shown window: motion, the tip as button 1, the stylus buttons as middle, right and 8, and everything released when it leaves proximity (pressure and tilt are not delivered).

### Changed

- Repeating a single-value command-line option with different values is now an error naming the option and both values. Equal values warn once per option.
- `xwlbridge-ctl` lets `bridge_args` override generated single-value options in both display and RealVNC modes, without emitting duplicates. `doctor` and config validation warn which settings are overridden and which value is used.

### Fixed

- TeamViewer's capture process (`TeamViewer_Desktop`) is a default desktop driver: its keys reach the desktop without `-xtestdrivers` naming it (the process is matched by its executable and `argv[0]`; its kernel `comm` is truncated to `TeamViewer_Desk`). Before, its keys stayed on the bridge display unless `-xtestdrivers` named it.
- `-showwindows`: a GTK 3 drag no longer ends the session (mutter: Invalid popup parent window). Invisible toolkit helpers (GTK 3's 1x1 drag-and-drop IPC window, mapped off the screen) and drag-and-drop icons are no longer shown as popups: a desktop that dismissed the helper took the icon's parent away with it, and mutter's protocol error killed every shown window.
- `-showwindows`: a client's window-manager requests are carried out instead of emulated on the engine display — WM_NORMAL_HINTS' min/max size becomes the toplevel's, a modal dialog is attached to its parent (xdg-dialog-v1), _NET_WM_STATE maximise/fullscreen/hidden and WM_CHANGE_STATE iconify go to the toplevel, _NET_WM_MOVERESIZE becomes the desktop's interactive move or resize (announced via _NET_SUPPORTED unless a window manager runs on the engine), and the compositor's maximised/fullscreen state is reflected back into _NET_WM_STATE.
- `-showwindows`: a client that moves its own top-level with a button held starts the desktop's interactive move (no motion is forwarded meanwhile).
- `-showwindows`: smooth (touchpad) scrolling is wheel clicks, one per 10 surface pixels of scroll (before, it did nothing).
- `-showwindows`: a popup placed past the monitor's edge slides onto it (its X window follows); a tooltip or notification takes no pointer input.
- `-showwindows`: when the keyboard leaves for another window, the client's open menus are closed with Escape (one per menu-like popup, at most 4).
- `-showwindows`: when the desktop's keyboard leaves all shown windows, the reverse keyboard's X focus is cleared, so games see FocusOut and stop receiving keys and focus-dependent joystick input while the GNOME user is elsewhere.
- `-showwindows`: SDL2 relative-mode mouse-look works without warps: a client whose pointer grab confines the pointer to a window (a core `GrabPointer` with confine-to, seen on RECORD) and hides the cursor locks the desktop's pointer, and the desktop's unaccelerated motion is injected as relative XTest motion; the lock ends when the grab does (UngrabPointer, the client leaving, a new grab without confine-to) or the cursor shows. A grab without confine-to does not lock (GTK 4's XI2 grabs during drag-and-drop and popups) (before, any grab of a hidden cursor locked, and such a lock kept every later absolute-pointer step from working; only a warp of a hidden pointer locked before that, so SDL read absolute positions as deltas and the look ran to the screen corner).

## 0.1.0 (2026-10-13)

First release under the name xwlbridge. It is based on xwlrvnc 0.0.8 by Patrick Gaskin.

### Added

- GNOME backend (`-backend gnome`). It uses mutter's D-Bus APIs: ScreenCast, RemoteDesktop (input through libei / EIS), the RemoteDesktop clipboard and DisplayConfig. It needs the `gnome` build feature.
- Stock X servers as engines (`-engine xvnc`, `xvfb`, `xwayland`) with a desktop agent. They need the `xvfb` build feature and `xkbcomp`.
- On-demand remote-desktop session. It runs while an X client watches, reads the screen or injects input, and stops after a while. Flags: `-warm`, `-linger`, `-alwayson`, `-coldconnect`, `-coldwait`.
- Input attribution for XTest: `-xtesthelpers`, `-xtestdrivers`, `-xtestgrace`, `-xtestpersist`.
- `-uidisplay display|auto|none`: runs RealVNC Server's own UI on another X display.
- `xwlbridge run [-display N] [--] command`, and `xwlbridge run -print`.
- `-showwindows` and `-showscreen` (experimental, stock engines).
- `-a11ybus` and `-reverseinput` (stock engines), and `tools/x11-a11y-proxy`.
- Builtin engine: read-only XKEYBOARD (`-noxkb` hides it), RandR monitors and modes, MIT-SHM ShmCreateSegment, MIT-SCREEN-SAVER requests, and the bell.
- Packaging:
  - two systemd user services, `xwlbridge-realvnc.service` and `xwlbridge.service`;
  - `xwlbridge-ctl`, including `doctor` and `migrate`; without `--display` or `--realvnc`, its `doctor`, `status`, `logs`, `start`, `stop`, `restart` and `show` follow the enabled service;
  - `install.sh` and `uninstall.sh`;
  - a release bundle and an optional .deb (`packaging/build-deb.sh`).
- contrib: a GNOME lock-screen extension and legacy remote-access helpers.
- CI on GitHub Actions: format check, clippy, tests, release build, `test_ctl.sh` and the proxy unit tests.

### Changed

- Upstream xwlrvnc's binary `xwlrvnc` is now `xwlbridge`, with upstream's flags; its X vendor string `xwlrvnc` is now `xwlbridge`, and its selection atoms `XWLRVNC_FETCH_*` are now `XWLBRIDGE_FETCH_*`.
- This fork's unpublished pre-release packages also used the name xwlrvnc. Their names changed:
  - `xwlrvnc-ctl` to `xwlbridge-ctl`;
  - `xwlrvnc.service` to `xwlbridge-realvnc.service`;
  - `~/.config/xwlrvnc` to `~/.config/xwlbridge`;
  - `XWLRVNC_*` environment variables to `XWLBRIDGE_*`;
  - lock-screen extension UUID `xwlrvnc-lockscreen@xwlrvnc` to `xwlbridge-lockscreen@xwlbridge`. Re-enable the extension under the new UUID.
- `xwlbridge-ctl migrate` moves such a pre-release install to the new names. `install.sh` runs it once when it finds one.

### Known issues

- See [docs/limitations.md](docs/limitations.md).
- `rfb_port` and `server_args` go on RealVNC's command line. RealVNC forgets them when it reloads its settings. Keep RealVNC parameters in `~/.vnc/config.d/vncserver-x11` and use `rfb_port=realvnc`.
- Without `-xtestdrivers default,TeamViewer_Desktop`, keys typed by TeamViewer 15.20.6 stay on the bridge's display while TeamViewer's own panel there has the X focus. The fix is on a development branch and is not in 0.1.0.

## 0.0.8 and earlier

Upstream xwlrvnc releases by Patrick Gaskin, see https://github.com/pgaskin/xwlrvnc and the git history before the tag `upstream-base`.
