# HiDPI and mixed DPI (`-showwindows`)

xwlbridge 0.3.0 follows Xwayland's native-scaling model for the windows
`-showwindows` puts on the desktop. It is on by default when the desktop has a
scaled monitor; `-nohidpi` (or `hidpi=off` / `XWLBRIDGE_HIDPI=off`) restores
the 0.1.2/0.2.0 behaviour, in which one X pixel is one GNOME logical pixel.

## The model

- **S** is the largest monitor scale on the desktop, rounded up: 1 for an
  unscaled desktop, 2 for 2x, 2 for 1.25x or 1.5x, 3 for 3x.
- The X screen, the RandR monitors and the X windows are **logical x S**:
  a 640x400 logical desktop at 2x is a 1280x800 X screen; a window GNOME puts
  at 100,50 with a logical size of 300x200 is an X window at 200,100 with a
  size of 600x400.
- Each shown surface commits a **buffer at S times its logical size** and sets
  `wp_viewport.set_destination` to the logical size, so the compositor scales
  it to the monitor (`wp_fractional_scale_v1` is created where the compositor
  offers it). On a monitor whose scale is S the result is pixel-exact; on a
  lower-scale monitor of a mixed desktop it is downscaled and slightly soft,
  exactly as Xwayland shows an X client there.
- Input is converted the other way in one place: a compositor surface-local
  position is logical, so the reverse pointer moves to `origin + local x S` in
  X pixels; touch and tablet events take the same path.
- When S changes, the screen is reconfigured: the engine is resized, RandR
  monitors and their millimetre sizes are re-expressed, the extension resyncs
  every window, and the window sync comes back on once the X screen holds the
  new layout.

## What apps learn

- RandR monitor geometries are in X pixels; their millimetre sizes are the
  logical size at 96 dpi, so `xrandr`/`Xft` compute a DPI of `96*S`.
- The bridge owns `_XSETTINGS_S0` on the engine display **only when no other
  XSETTINGS manager runs there**, announces itself with the spec's `MANAGER`
  ClientMessage (and again on every scale change), and publishes
  `Gdk/WindowScalingFactor = S`, `Gdk/UnscaledDPI = 96*1024`,
  `Xft/DPI = 96*S*1024`.
- `Xft.dpi` in the root's `RESOURCE_MANAGER` is kept at `96*S` (other resource
  lines are preserved). Qt and Xft toolkits read it; Java reads
  `gnome.Xft/DPI`/`gnome.Gdk/WindowScalingFactor` from XSETTINGS.

Toolkits that read these settings scale their UI. Measured at 2x (the full
matrix is in `research/release/p5_hidpi.md`):

- GTK 3/4 and Qt 6 apply S: a 300x200 logical window becomes a 600x400 X
  window, and with the viewporter destination at 300x200 it is pixel-exact on
  the 2x monitor.
- Qt 5 does not scale window geometry by default (it reports
  `devicePixelRatio=1` and a 300x200 X window on a 2x monitor); that is exactly
  what it does on GNOME's own Xwayland at the same scale. Qt 5 needs
  `QT_AUTO_SCREEN_SCALE_FACTOR=1`/`QT_SCALE_FACTOR` if the app wants its
  geometry scaled.
- Java's AWT/Swing also stays at its own 96-dpi default (`screen_resolution=96`,
  `sun.java2d.uiScale=null`, a 300x200 X window), again the same as on GNOME's
  Xwayland; `-Dsun.java2d.uiScale` is the app-level switch.
- xterm reads `Xft.dpi`: its 11-point font is 14.67 px at 1x and 29.33 px at
  2x.
- Wine keeps its own DPI (its registry, or what it derives from the X server's
  reported DPI). Our RandR millimetres give it 192 dpi at S=2, so its Notepad
  is 1786x1136 X pixels at 2x versus 1132x821 at 1x; it does not read
  `Xft.dpi`. GNOME's Xwayland reports its monitors' physical millimetres, so
  Wine there stays at 1132x821 at 2x. This is Wine's own DPI model, not the
  bridge's.

Electron/Chromium follow GTK's XSETTINGS. Toolkits that ignore both XSETTINGS
and Xft.dpi keep rendering at 96 dpi.

## Mixed DPI

One X screen has one S. On a 1x + 2x desktop S is 2, so the 1x monitor's X
windows are rendered at 2x and downscaled to 1x (slightly soft), while the 2x
monitor is native. This is the same trade-off Xwayland makes; a window moved
between the monitors keeps its X pixels and is resampled by the compositor.

## Measured in the 2x VM (2026-10-24)

`dsh-p2mon`, Virtual-1 1920x1200 at 2x + Virtual-2 1280x800 at 1x, GNOME 50.1;
a 300x200 window requested by the toolkit (its own `get_scale_factor`/`dpr`
and `xwininfo`; a parallel run on GNOME's Xwayland `:0` at the same scale is
the control):

| toolkit | bridge `:20`, 2x | Xwayland `:0`, 2x |
|---|---|---|
| GTK 3 | scale 2, allocation 300x200, X 600x400 | scale 2, allocation 300x200, X 600x400 |
| GTK 4 | scale 2, allocation 300x200, X 600x400 | scale 2, allocation 300x200, X 600x400 |
| Qt 6 | dpr 2, X 600x400 | dpr 2, X 600x400 |
| Qt 5 | dpr 1, logical DPI 192, X 300x200 | dpr 1, logical DPI 192, X 300x200 |
| Java | `screen_resolution=96`, `uiScale=null`, X 300x200 | `screen_resolution=96`, `uiScale=null`, X 300x200 |
| xterm | Xft font 29.33 px at 2x (14.67 at 1x) | — |
| Wine | own DPI: X 1786x1136 at 2x (1132x821 at 1x) | X 1132x821 (its own DPI there too) |

A real GTK app (`gnome-calculator`) is full-density again: 740x1232 X on
`:20` at 2x (820x1332 on `:0`); the remaining difference is the theme/font
set, which the bridge does not mirror (it publishes the scale/DPI part of
GNOME's XSETTINGS, not its 53 keys). The first frame log of a 30x8 xterm at 2x is
`buffer 546x286 X pixels, viewport destination 273x143 logical, S = 2`: the
buffer is exactly S times the logical size, so the 2x monitor shows it 1:1.

Earlier rounds saw GTK 3/4 and Qt 6 create half-size windows and X
self-moves that the desktop did not follow. Both had one cause: an
`xdg_toplevel.configure` size is in **logical** pixels, and the bridge applied
it to the X window as X pixels. The compositor repeats the current logical
size on every configure, so a 600x400 X window was resized to 300x200 (then
150x100 ...) and the resize/`xSendEvent` echo swallowed a pending X move.
`Space::configure_size` converts the configure size once; the window sync then
keeps X at S times the logical size and a move-only configure reaches the
desktop again.

## Opting out

```sh
xwlbridge -showwindows -nohidpi ...          # this run
xwlbridge-ctl config set hidpi off            # settings file (then restart)
xwlbridge-ctl config set hidpi on             # back on
XWLBRIDGE_HIDPI=off xwlbridge ...             # environment
```

With HiDPI off, X pixels are GNOME logical pixels again and a scaled desktop
keeps the window sync off (as in 0.2.0); the log says which monitor is scaled.

## Troubleshooting

- `xwlbridge-ctl doctor` reports the extension's state; the sync needs the
  `xwlbridge-windows` extension.
- The log line `HiDPI: X space = logical x N; X screen WxH` shows S and the X
  screen; `first frame ... (buffer WxH X pixels, viewport destination LxH
  logical, S = N)` shows a shown window's two sizes.
- If no `_XSETTINGS_S0` appears in `xprop -root`, another XSETTINGS manager
  owns it; the bridge leaves it alone. `Xft.dpi` still comes from
  `RESOURCE_MANAGER`.
