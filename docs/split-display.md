# Programs with their own windows: the split display

xwlbridge's X display is for the parts of a program that need the whole desktop: reading the screen, injecting
input, the global clipboard. Nobody sees the windows that are opened *on* it (on the stock engines the bridge's
agent holds Composite redirection, so client windows never reach the screen it mirrors; the built-in engine draws
none). So there are two kinds of programs:

| kind | examples | how to start it |
|---|---|---|
| **tools without a window of their own** | xdotool, xte, scrot, xwd, maim, import, ffmpeg x11grab, GStreamer ximagesrc, x11vnc, x0vncserver, xclip, PyAutoGUI and Java Robot scripts | on the bridge: `xwlbridge run -- TOOL ...` (or as the bridge's own command) |
| **programs whose windows you use** | OBS, SimpleScreenRecorder, a VNC *viewer*, RealVNC Server's status window | on the desktop as usual; only their capture is pointed at the bridge's display (the recipes below) |

`xwlbridge run` and the bridge's own command put **every** window of the program on the bridge's display: they set
`DISPLAY` to it, remove `WAYLAND_DISPLAY` and pin GTK, Qt, SDL, Electron, Firefox, Clutter and winit to X11
(`xwlbridge run -print` shows the exact environment). That is what a tool needs: a GTK or Qt program that still sees
the desktop's Wayland socket would otherwise open there (GTK even with `WAYLAND_DISPLAY` unset, since libwayland
then uses `wayland-0`) and capture through Wayland instead of the bridge. It is wrong for a program whose windows
you want to see. Never export the bridge's `DISPLAY` for your whole session either: every X app you start would
disappear onto it.

In the recipes, `:20` stands for the bridge's display (`xwlbridge run -print` prints it).

## Recipes

**SimpleScreenRecorder** (measured: GNOME 50, xvnc engine; 248 frames of the composited desktop, with GNOME's
top bar):

```sh
DISPLAY=:20 XDG_SESSION_TYPE=x11 simplescreenrecorder -display :0
```

Its recorder opens `$DISPLAY` (the bridge); Qt's `-display :0` puts its window on GNOME's Xwayland. With
`XDG_SESSION_TYPE=wayland` it waits on a warning dialog instead of recording.

**OBS Studio** (measured: OBS 32.1, xvnc engine; prefer OBS's own *Screen Capture (PipeWire)* source when it is
enough):

```sh
QT_QPA_PLATFORM=xcb obs &
```

OBS runs on GNOME's Xwayland (its XSHM source only exists when OBS runs on X11). Then Sources → **+** →
*Screen Capture (XSHM)* → **Advanced Settings** → **X Server** = `:20`. Minimise or crop OBS's own window, which
is part of the desktop it records. Needs the xvnc or xvfb engine (the built-in one reports no RandR monitors).

**x11vnc** has no window; run it on the bridge with `xwlbridge run -- x11vnc ...`. Started by hand from a desktop
terminal it needs `env -u WAYLAND_DISPLAY x11vnc -display :20 ...`: x11vnc 0.9.17 exits ("Wayland display server
detected") whenever `WAYLAND_DISPLAY` is set, even with `-display`. Its
optional Tk control panel can go to the desktop with `-gui :0` (from `x11vnc -help`; not measured).

**ffmpeg / GStreamer** take the display as an argument and have no window:
`ffmpeg -f x11grab -video_size WxH -i :20 out.mkv`, `gst-launch-1.0 ximagesrc display-name=:20 ! ...`.

**RealVNC Server** under the bridge: `-uidisplay auto` puts its status window, tray icon and dialogs on GNOME's
Xwayland while it remotes the bridge's display.

**VNC viewers, remote-desktop clients, anything you look at**: start them normally. They do not need the bridge.

## Programs that cannot be split

A program with a single X connection for both its windows and its capture cannot be split: flameshot's capture
overlay, gpick, xzoom, gromit-mpx, screenkey, and Wine programs that both capture the desktop and show a window
(a Wine process has one display). On the bridge they capture correctly but their windows are invisible; on the
desktop they see only Xwayland. Prefer their GNOME/Wayland equivalents (the Screenshot portal, GNOME's colour
picker and magnifier).
