# Keeping an X app entirely on the bridge (no split display)

## Background: the split display

xwlbridge's X display is for the parts of a program that need the whole desktop: reading the screen, injecting input, the global clipboard. Nobody sees the windows that are opened *on* it. On the stock engines the bridge's agent holds Composite redirection, so client windows never reach the screen it mirrors. The built-in engine draws none.

So there are two kinds of programs:

| kind | examples | how to start it |
|---|---|---|
| **tools without a window of their own** | xdotool, xte, scrot, xwd, maim, import, ffmpeg x11grab, GStreamer ximagesrc, x11vnc, x0vncserver, xclip, PyAutoGUI and Java Robot scripts | on the bridge: `xwlbridge run -- TOOL ...` (or as the bridge's own command) |
| **programs whose windows you use** | OBS, SimpleScreenRecorder, a VNC *viewer*, RealVNC Server's status window | on the desktop as usual; only their capture is pointed at the bridge's display (the recipes below) |

`xwlbridge run` and the bridge's own command put every window of the program on the bridge's display. They set `DISPLAY` to it, remove `WAYLAND_DISPLAY` and pin GTK, Qt, SDL, Electron, Firefox, Clutter and winit to X11 (`xwlbridge run -print` shows the exact environment). That is what a tool needs. It is wrong for a program whose windows you want to see. Never export the bridge's `DISPLAY` for your whole session: every X app you start would disappear onto it.

In the recipes, `:20` stands for the bridge's display.

## Recipes

**SimpleScreenRecorder** (measured: GNOME 50, xvnc engine; 248 frames of the composited desktop, with GNOME's top bar):

```sh
DISPLAY=:20 XDG_SESSION_TYPE=x11 simplescreenrecorder -display :0
```

Its recorder opens `$DISPLAY` (the bridge). Qt's `-display :0` puts its window on GNOME's Xwayland. With `XDG_SESSION_TYPE=wayland` it waits on a warning dialog instead of recording.

**OBS Studio** (measured: OBS 32.1, xvnc engine; prefer OBS's own *Screen Capture (PipeWire)* source when it is enough):

```sh
QT_QPA_PLATFORM=xcb obs &
```

Then Sources → **+** → *Screen Capture (XSHM)* → **Advanced Settings** → **X Server** = `:20`. Minimise or crop OBS's own window, which is part of the desktop it records.

**x11vnc** has no window. Run it on the bridge with `xwlbridge run -- x11vnc ...`. Started by hand from a desktop terminal it needs `env -u WAYLAND_DISPLAY x11vnc -display :20 ...`, because x11vnc 0.9.17 exits ("Wayland display server detected") whenever `WAYLAND_DISPLAY` is set.

**ffmpeg / GStreamer** take the display as an argument and have no window:

```sh
ffmpeg -f x11grab -video_size WxH -i :20 out.mkv     # WxH: the bridge display's size (xdpyinfo)
gst-launch-1.0 ximagesrc display-name=:20 ! ...
```

**RealVNC Server** under the bridge: `-uidisplay auto` puts its status window, tray icon and dialogs on GNOME's Xwayland while it remotes the bridge's display.

**VNC viewers, remote-desktop clients, anything you look at**: start them normally. They do not need the bridge.

## Programs that cannot be split

A program with a single X connection for both its windows and its capture cannot be split. Examples: flameshot's capture overlay, gpick, xzoom, gromit-mpx, screenkey, and Wine programs that both capture the desktop and show a window (a Wine process has one display).

On the bridge they capture correctly but their windows are invisible; on the desktop they see only Xwayland. `-showwindows` (experimental, below) shows the windows of a program that runs entirely on the bridge on your desktop; it was tested with the apps listed under "Measured comparison", not with each program named here. Their GNOME/Wayland equivalents (the Screenshot portal, GNOME's colour picker and magnifier) are the other choice.

## The no-split options

These options keep an X app entirely on the engine and still show it on GNOME. They need a stock engine (xvnc, xvfb or xwayland).

- `-showwindows` (experimental): X windows mapped on the engine display are shown natively on the desktop. The desktop's input on them goes to them only. Each shown window gets a bridge-drawn Adwaita-style header and invisible resize edges (see below).
- `-showscreen` (experimental, used with `-showwindows`): the whole X screen as one window (no per-window header; it is one window).
- The accessibility proxy (`tools/x11-a11y-proxy`, supported): shows each X top-level window as a GNOME window with live pixels and accessible objects.

### Start each option

```sh
xwlbridge -backend gnome -engine xvnc -showwindows APP
xwlbridge -backend gnome -engine xvnc -showwindows -showscreen APP
```

`-engine xvnc` needs TigerVNC's Xvnc (`sudo apt install tigervnc-standalone-server`); `-engine xvfb` and
`-engine xwayland` work too.

The proxy (from a clone of this repository; its packages are listed in [accessibility.md](accessibility.md#quick-start)):

```sh
xwlbridge -backend gnome -engine xvnc -display 20 -a11ybus -reverseinput -nowrap &
tools/x11-a11y-proxy/x11-a11y-proxy --display :20 -- APP
```

## Measured comparison

Measured with 7 apps in headless GNOME 50 containers. These were functional checks. The reference is the app on GNOME's Xwayland.

| setup | checks passed | notes |
|---|---|---|
| Reference: app on GNOME's Xwayland | 34/38 | XSHM and SimpleScreenRecorder record black, x11vnc dies, RealVNC viewer black |
| `-showwindows` | 37/38 | the miss: GNOME places every new window itself, so onboard opened on top of its target |
| `-showwindows -showscreen` | 38/38 | |
| Accessibility proxy as a way of showing windows | 29/38 | popups clipped, among others |

Latency and accessibility:

| measure | Xwayland | `-showwindows` | `-showwindows -showscreen` |
|---|---|---|---|
| key to frame, mean | 11.6 ms | 11.4 ms | 13.6 ms |
| click to frame, mean | 9.9 ms | 17.4 ms | 27.7 ms |
| zoom focus tracking | 39 px off | 0-1 px off | 49-51 px off |

With `-showwindows`, Orca words are identical to Xwayland for Java and Qt 6. Idle cost is about 0.

## Release acceptance

Ubuntu 26.04 VM: `-showwindows` with xcalc and a GTK 3 app passed 6/6. Both GNOME windows appeared within 0.9 s. Typing and clicks from the machine's own keyboard and mouse reached the app. The windows went away when the bridge stopped.

## Limits of `-showwindows`

- No title bars. mutter draws no server-side decorations. Super+drag moves a window.
- Window positions. With the window extension running ([extension.md](extension.md), installed and enabled by default) an X window and its GNOME window keep one position: a window that moves or resizes itself (a drag handle, a Qt dock, an animation) moves on the desktop too, kept inside the monitor's work area, and the X window follows the desktop's moves, tiling and maximising; a window that asks for a position (USPosition, PPosition other than 0,0) opens there; the X stacking order is the desktop's, and an app's raise or activation request is honoured only after recent input in that app (else the window demands attention). Without the extension (as in 0.1.2) X window positions are not kept when a window opens: GNOME places toplevels, and a client that moves its own window with a button held moves it inside X only: the window does not follow on the desktop, and the pointer mapping is held steady until release. Either way a client's move-resize (`_NET_WM_MOVERESIZE`), maximise, fullscreen, minimise or iconify requests are carried out. Popups are shown where the X client placed them, anchored to the parent's current position; one that does not fit the monitor is clipped, not slid. Keyboard enter sets X focus for input-taking windows (including clients advertising WM_TAKE_FOCUS, which also receive that notification); leaving all shown windows still clears X focus.
- AT-SPI screen coordinates are engine coordinates. Accerciser and Orca flat-review click are off by the window's offset.
- Settings (high contrast and so on) reach engine apps only with the proxy's `--settings-only` mirror (3/3 with it, 0/3 without).
- Input devices: the mouse (buttons 1-10, so back and forward work; buttons 11 and up are not delivered), the keyboard, a touch screen (the first point drives the pointer, Xwayland-style, until the lift; no multi-touch, so pinch and rotate gestures are not forwarded) and a pen or eraser (the tip is the left button, its side buttons are middle, right and 8; pressure and tilt are not delivered) reach shown windows. Smooth (touchpad) scrolling is delivered as wheel clicks, one per 10 surface pixels of scroll; there are no smooth scroll axes.
- XEmbed tray icons are not shown.
- It needs a stock engine (xvnc, xvfb or xwayland).
- It is experimental, as is `-showscreen`.

## Games on `-showwindows`

- Mouse-look works for both SDL generations: a game that hides the cursor and warps the pointer (SDL1-style) locks the desktop's pointer, and so does one whose pointer grab confines the pointer to a window (SDL2's relative mode: a core GrabPointer with confine-to; RECORD sees the grabs, so without it only the warp locks). While locked, the game receives the desktop's unaccelerated motion as relative motion and the desktop pointer stays put; the lock ends when the cursor is visible again, or when the confining grab ends. A grab that does not confine (GTK 4's XI2 grabs during drag-and-drop and popups) does not lock.
- The X focus follows the desktop keyboard: when it leaves the shown windows, the focus is cleared, so a game sees FocusOut and stops receiving keys and focus-dependent joystick input until the desktop keyboard comes back to one of its windows.

## When to use which

| you want | use |
|---|---|
| a tool that reads the screen or injects input, with no window | `xwlbridge run -- TOOL` |
| OBS, SimpleScreenRecorder, a viewer | run on the desktop, point only the capture at the bridge |
| an X app whose windows you see and use, entirely on the engine | `-showwindows` |
| the same, with the whole X screen as one window | `-showwindows -showscreen` |
| accessibility for apps without a usable accessibility tree | the accessibility proxy, not as the window mode |

Without the proxy, an app on the engine registers on the desktop's accessibility bus and Orca may follow an invisible window. With `-showwindows` the windows are visible and their accessibility is Xwayland's.

## Window decorations

A shown X window gets the frame a window manager would give it on X, drawn by
the bridge on GNOME:

- a header in its own `wl_subsurface` above the client (never a copy of the
  client's pixels), with the title and the GNOME button layout, painted with
  cairo and pango through `dlopen` (no new build dependency);
- a press on the header is `xdg_toplevel.move`/`resize` at that press: mutter
  owns the drag, so there is no bridge-side move loop and no extra latency;
  double click toggles maximise, right click opens GNOME's window menu, and the
  close/minimise/maximise buttons follow `_MOTIF_WM_HINTS` functions;
- a 6 px invisible resize margin around the content. Mutter clips pointer input
  to the xdg window geometry, so (measured on GNOME 50.1) the window geometry
  is the whole surface box while framed; winsync converts every rectangle at
  its boundary, so the client area still lands exactly where it would without
  a frame plus the header, and the header/margins take input. Maximised or
  fullscreen the geometry becomes the content (the header is detached).
  `_NET_FRAME_EXTENTS` = `0,0,header,0` and `_NET_REQUEST_FRAME_EXTENTS` are
  published as EWMH asks;
- GNOME's live settings are followed through `org.freedesktop.portal.Settings`
  (`Read` and `SettingChanged`) with dconf as the fallback: `button-layout`,
  `action-double/middle/right-click-titlebar`, `titlebar-font`,
  `color-scheme` and `accent-color`; `-windowbuttons=LAYOUT` overrides the
  button layout;
- no heuristics for client-drawn title bars: `_GTK_FRAME_EXTENTS` (GTK CSD)
  and `_MOTIF_WM_HINTS decorations 0` mean the client keeps its own bar, and
  the bridge draws none;
- `-nodecorations` (or `decorations=off` with the service) turns all of it
  off, restoring 0.2.0 bit for bit (no `_NET_FRAME_EXTENTS`, cairo/pango never
  loaded).

The header is removed while a window is maximised or fullscreen, as GNOME's
own windows are, and comes back on restore. The doctor prints the setting and
the values in force; `-verbose` logs the reason for every window's frame
decision.
