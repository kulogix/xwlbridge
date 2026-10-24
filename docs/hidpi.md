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

GTK 3/4, Qt 5/6 and Java therefore render their UI at the right physical size.
Wine keeps its own DPI (its registry's `logpixels`, or `WINEDEBUG`/`winecfg`);
Electron/Chromium follow GTK's XSETTINGS. Toolkits that ignore both XSETTINGS
and Xft.dpi keep rendering at 96 dpi.

## Mixed DPI

One X screen has one S. On a 1x + 2x desktop S is 2, so the 1x monitor's X
windows are rendered at 2x and downscaled to 1x (slightly soft), while the 2x
monitor is native. This is the same trade-off Xwayland makes; a window moved
between the monitors keeps its X pixels and is resampled by the compositor.

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
