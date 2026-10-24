# Architecture

xwlbridge runs an X server (the engine) whose root window shows the desktop. A backend talks to the compositor: the picture comes in, input and clipboard go out.

## Components

| Component | Role |
|---|---|
| Engine | The X server clients connect to. One of `builtin`, `xvfb`, `xvnc`, `xwayland` (`-engine`). |
| Backend | Talks to the compositor. `gnome`, `wlr` or `auto` (`-backend`). |
| Agent | An ordinary X client of a stock engine. Used with `xvfb`, `xvnc` and `xwayland`. |

### Backends

- `gnome`: mutter's D-Bus APIs on the session bus: ScreenCast (PipeWire frames), RemoteDesktop (input through libei / EIS), the RemoteDesktop clipboard, and DisplayConfig (monitor layout, dynamic resolution, DPMS wake). It uses no xdg-desktop-portal and shows no permission dialog. It must run as the user inside the GNOME session. GNOME shows its screen-sharing indicator while the desktop is mirrored. It needs the `gnome` build feature.
- `wlr`: upstream's Wayland-protocol backend (wlr-screencopy / ext-image-copy-capture, virtual pointer and keyboard, data-control) for sway, labwc and other wlroots compositors. The default build has only this backend.
- `auto` picks `gnome` for mutter and `wlr` otherwise.

### Engines

- `builtin`: xwlbridge's own minimal X server, from upstream. It needs no extra packages. It implements a subset of X11: read-only XKEYBOARD from the compositor's keymap (`-noxkb` hides it), RandR monitors, CRTCs and modes, MIT-SHM including ShmCreateSegment, MIT-SCREEN-SAVER requests, and the bell (rung on the compositor's xdg_system_bell_v1 where present). It has no XInput2, no RECORD and no Composite.
- `xvnc`: TigerVNC's Xvnc, with its VNC listener confined to a private Unix socket. Complete X11. It resizes with the desktop. RealVNC's set-uid `/usr/bin/Xvnc` and any set-uid server are refused. `-xserver PATH` picks the binary.
- `xvfb`: Xorg's Xvfb. Complete X11. Fixed size.
- `xwayland`: a rootful Xwayland on a private headless Wayland compositor inside xwlbridge. It never touches the user's session. Complete X11. It resizes. A few percent of frames can be torn during fast motion.

The stock engines need the `xvfb` build feature and `xkbcomp`.

## Data flow: builtin engine

```
 X client ----X11----> [ builtin X server ] <---> [ backend ] <---> compositor
 (xdotool, ffmpeg,       root window =            gnome: D-Bus,       (mutter or
  VNC server ...)        desktop picture          PipeWire, EIS       wlroots)
                         input, clipboard ------> input/clipboard out
                         <------ picture in
```

## Data flow: stock engines with the agent

```
 X client --X11--> [ Xvnc | Xvfb | Xwayland ] <--X11-- [ agent ]
                          root window                     |
                                                          v
                                                    [ backend ] <--> compositor
 agent: puts the captured desktop on the root window (MIT-SHM),
        mirrors cursor, keymap and screen layout,
        forwards the input its clients inject,
        bridges the clipboard.
```

## Session on demand

With the gnome backend the remote-desktop session runs on demand. It starts while an X client watches (DAMAGE), reads the screen or injects input. It stops after a while.

| Flag | Meaning |
|---|---|
| `-warm secs\|always` | Keep the session after the last activity. Default 10 s. |
| `-linger secs` | Keep it after an X client that read the screen (a screenshot tool) went away. Default 30 s. |
| `-alwayson` | Same as `-warm always`. |
| `-coldconnect none\|start` | Stock engines. Default `start`: a client that connects while the session is stopped starts it at once. |
| `-coldwait ms` | Builtin. A screen read may wait this long while the session starts. Default 2000. |

`-reclaim` recreates the session after the user stopped it from GNOME's indicator.

## Input path and attribution

Only XTest meant for the desktop is forwarded.

- `-xtesthelpers names`: programs whose XTest only drives windows on the bridge display. Default `xembedsniproxy,xpra`.
- `-xtestdrivers names`: programs whose XTest always reaches the desktop. Default: the VNC servers, the KVM clients, and TeamViewer's capture process. Stock engines.
- `-xtestgrace ms`: keys a one-shot client leaves held stay down this long for another client to release. Default 1000.
- `-xtestpersist`: keep held keys like Xorg.

This is an accident filter, not a security boundary.

## Clipboard

`-clipboard auto|none|wlr|ext|session|xwayland` selects the path. `xwayland` relays the engine clients' selections X to X with the desktop's own Xwayland. It works while no remote-desktop session runs. `-noprimary` stops bridging PRIMARY.

## -uidisplay

`-uidisplay display|auto|none` puts RealVNC Server's own UI process (vncserverui) on another X display, normally the desktop's Xwayland. A small preload library (`uihook/`), embedded in the binary, does this. The hook is written only into a directory owned by the user and writable by no one else.

## xwlbridge run and the display registry

`xwlbridge run [-display N] [--] command` runs an X tool on a running bridge's display. It finds the display through a registry in `$XDG_RUNTIME_DIR/xwlbridge-displays`. The command gets the engine's cookie, Wayland hidden and toolkits pinned to X11 (GTK, Qt, SDL, Electron, Firefox, Clutter, winit). `xwlbridge run -print` prints that environment. A command wrapped by xwlbridge gets the same environment.

## -showwindows, -reverseinput, -a11ybus

- `-showwindows` (stock engines, experimental): X windows mapped on the engine display are shown natively on the desktop. The desktop's input on them goes to them only. `-showscreen` shows the whole X screen as one window. Per-window decorations (`src/xvfb/decor.rs`, `-nodecorations` to opt out): a header `wl_subsurface` painted with cairo/pango `dlopen`, a content-vs-surface `FrameGeom` conversion so winsync keeps working in client coordinates (the window geometry is the surface box, because mutter clips pointer input to it), `_NET_FRAME_EXTENTS`, and presses on the header handed to mutter's `xdg_toplevel.move`/`resize`. `decor_settings.rs` follows GNOME's live settings (settings portal, dconf fallback).
- `-a11ybus` (stock engines): a private AT-SPI bus for the display.
- `-reverseinput`: a second XInput2 master, `xwlbridge-reverse`. A desktop-side proxy types and clicks through it into windows on the display without echoing to the desktop.

`-a11ybus` and `-reverseinput` are used by `tools/x11-a11y-proxy`.

## Packaging units

Two systemd user units, both PartOf, After and Requisite of `graphical-session.target`, and WantedBy it. Both are hardened (NoNewPrivileges, MemoryMax=4G, ...).

| Unit | Runs |
|---|---|
| `xwlbridge-realvnc.service` | RealVNC Server (user mode, vncserver-x11-core) on the bridge, via `xwlbridge-ctl run`. |
| `xwlbridge.service` | The bridge display alone (`-nowrap`), via `xwlbridge-ctl --display run`, for X11 tools. |

## Code layout

| Path | Content |
|---|---|
| `src/bridge` | X protocol server, input, keymap/XKB, capture, clipboard, backends (`wayland/`, `gnome/`) |
| `src/xvfb` | Stock engines and agent: `record.rs` input attribution, `reverse.rs` -showwindows/-reverseinput, `decor.rs`/`decor_settings.rs` window decorations, `wlhost.rs` private compositor for `-engine xwayland`, `xrelay.rs` Xwayland clipboard relay, `a11ybus.rs` |
| `src/uidisplay.rs`, `uihook/` | `-uidisplay` |
| `src/childenv.rs` | `run` and the environment |
| `dynres/` | Upstream's RealVNC dynamic-resolution hook |
| `packaging/`, `contrib/` | Packaging and extras |
| `tools/x11-a11y-proxy` | Accessibility proxy (Python + GTK 4) |

## Exit statuses

| Status | Meaning |
|---|---|
| 0 | Stopped (SIGTERM). |
| 69 | The X server engine or the desktop was lost. |
| 70 | Backend failure. |

## Relation to upstream

xwlbridge is based on xwlrvnc by Patrick Gaskin (MIT, Copyright (c) 2026 Patrick Gaskin). The git history keeps all upstream commits. The tag `upstream-base` marks upstream v0.0.8.

From upstream:

- The fake X server backed by a Wayland display (the builtin engine).
- The wlroots backend.
- The dynamic-resolution hook (`dynres/`).
- The test approach.

Added here:

- A GNOME (mutter) backend.
- Stock X servers as engines, with a desktop agent.
- Packaging.
- An accessibility proxy.
- Work to make it a general-purpose bridge.
