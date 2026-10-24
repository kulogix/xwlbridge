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
  XSETTINGS manager runs there**, and publishes
  `Gdk/WindowScalingFactor = S`, `Gdk/UnscaledDPI = 96*1024`,
  `Xft/DPI = 96*S*1024`.
- `Xft.dpi` in the root's `RESOURCE_MANAGER` is kept at `96*S` (other resource
  lines are preserved). Qt and Xft toolkits read it; Java reads
  `gnome.Xft/DPI`/`gnome.Gdk/WindowScalingFactor` from XSETTINGS.

Toolkits that read these settings scale their UI; two caveats measured in the
2x VM (`research/release/p5_hidpi.md`, round 2):

- Qt 5 does not scale window geometry from Xft.dpi by default (it reports
  `devicePixelRatio=1` and a 300x200 X window on a 2x monitor, exactly as it
  does on GNOME's own Xwayland); Qt 6 reports `dpr=2`. Qt 5 needs
  `QT_AUTO_SCREEN_SCALE_FACTOR=1`/`QT_SCALE_FACTOR` if the app wants its
  geometry scaled.
- Java's AWT/Swing on this VM stayed at `screen_resolution=96`,
  `sun.java2d.uiScale=null` and a 300x200 request became a 150x100 X window;
  `-Dsun.java2d.uiScale=2` makes it 300x200 X (still half of GTK's Xwayland
  control in the same acceptance). The bridge does not publish
  `gnome.Xft/DPI`, which some JDKs read.
- GTK 3/4 and Qt 6 reported scale 2 on the bridge, but their first 300x200
  window ended at half the Xwayland control's logical size; the open geometry
  bug is in `research/release/p5_hidpi.md` (round 2).

Wine keeps its own DPI (its registry's `logpixels`, or `WINEDEBUG`/`winecfg`);
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
| GTK 3 | scale 2, allocation 150x100, X 300x200 | scale 2, allocation 300x200, X 600x400 |
| GTK 4 | scale 2, allocation 150x100, X 300x200 | scale 2, allocation 300x200, X 600x400 |
| Qt 6 | dpr 2, X 300x200 | dpr 2, X 600x400 |
| Qt 5 | dpr 1, logical DPI 192, X 300x200 | dpr 1, logical DPI 192, X 300x200 |
| Java | `screen_resolution=96`, `uiScale=null`, X 150x100 | — |
| xterm | Xft font 29.33 px at 2x (14.67 at 1x) | — |

The geometries of real GTK apps were also affected: `gnome-calculator` was
820x1332 X on `:0` and 370x616 X on `:20`. The `desktop_position` fix in the
round-2 commit request removes the 2x-too-large oscillation (GIMP's toolbox
went from 2520x1376 X to a sane 432x1120 X in the 2x cell), but the remaining
half-size/window-sync findings above are open.

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
