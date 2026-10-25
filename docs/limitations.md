# Limitations

These are the known limits of xwlbridge 0.3.0.

| Limitation | Affects | Workaround / status |
|---|---|---|
| A fast one-shot screen read right after connecting, while the session is stopped, can be black (xwd, import, scrot, maim, PyAutoGUI read within about 30 ms of connecting; in the release test 14 to 17 of 32 one-shot captures were correct on xvnc/xvfb, the others black). Java Robot, Wine and ffmpeg got the current desktop. | Stock engines | The builtin engine waits for the session (32 of 32 correct). Use `-alwayson` or `-warm always` (GNOME's sharing indicator then stays up), or take a throw-away first shot. |
| X clients never see the local user's input: no RECORD, XInput2 raw events, passive key grabs or idle time from GNOME. X idle time grows while the user works. | Global hotkeys (xbindkeys, sxhkd, push-to-talk), key display tools, idle/time trackers, an Input Leap/Barrier server | None. |
| Stock engines do not follow GNOME's pointer position. | Colour pickers, magnifiers, getmouselocation | None. |
| Characters outside the current keyboard layout are not typed (`xdotool type 'é€'`). AltGr (level-3) characters that a client types with an XKB latch (TeamViewer does) arrive as the base key. | Typing tools, remote-access apps | None. |
| An X client can resize the xvnc engine away from the desktop size (`xrandr -s`). | `xvnc` engine | None. |
| Bells are lost. | Stock engines | The builtin engine rings the desktop bell. |
| The built-in engine (and `-clipboard session`) bridges text only, CLIPBOARD only, and only while a remote-desktop session runs. | Built-in engine | Stock engines relay every type, CLIPBOARD and PRIMARY, through the desktop's Xwayland (`-clipboard auto`, when xwlbridge runs in the session). |
| GNOME's windows are not X windows. | wmctrl, xdotool search/getactivewindow, window pickers | None. |
| Xvnc follows the desktop monitor layout with one RandR output per monitor; Xvfb keeps a fixed screen with one output for the whole desktop. | Stock engines | Use `xvnc` for per-monitor RandR; `xvfb` is fixed-size by design. |
| mutter 50.1 keeps a stale primary flag after a primary change (both monitors report primary). | X primary on a multi-monitor desktop | Worked around: the bridge follows the flag history and asks the window extension (`GetMonitors`) when that cannot tell. A future mutter should fix the flags. |
| A Wayland window cannot be fullscreen across several monitors: a fullscreen window covers one monitor, and `_NET_WM_FULLSCREEN_MONITORS` is not supported. | Games and `-showwindows` across monitors | Use one monitor for fullscreen, a borderless window, or `-showscreen`. |
| HDR reaches a VNC viewer tone-mapped to SDR; Night Light and ICC colour profiles are not visible remotely. | HDR displays, VNC viewers | The local desktop keeps its HDR/colour; the remote picture is SDR. |
| Wine chooses its own DPI at 2x; it reads neither `Xft.dpi` nor XSETTINGS. | Wine under `-showwindows` at 2x | Set `HKCU\Control Panel\Desktop\LogPixels` in the Wine prefix to pin its DPI. |
| The GPU presentation path for games is opt-in. | Games on `-showwindows` | Set `XWLBRIDGE_GAME_PRESENT=gpu` (or `gpu-sync`/`gpu-async`); the default CPU composition is the measured path. |
| Runs only inside a logged-in GNOME session, not at the login screen (GDM). While the screen is locked GNOME closes remote access. | All | Viewers see a padlock unless the lock-screen extension in `contrib/gnome-shell` is enabled. |
| A restart of the bridge or its engine ends every X client on its display. | All | Restart the clients. |
| `-showwindows` and `-showscreen` are experimental. AT-SPI coordinates are engine coordinates. Per-window decorations are on by default (`-nodecorations`/`decorations=off` opts out); a header is not drawn for windows that declare their own (`_GTK_FRAME_EXTENTS`, `_MOTIF_WM_HINTS` decorations 0). XEmbed tray icons need GNOME's AppIndicator extension ([tray.md](tray.md)). HiDPI is on by default (`-nohidpi`/`hidpi=off` opts out). See [no-split.md](no-split.md), [hidpi.md](hidpi.md) and [tray.md](tray.md). | Stock engines | Settings reach engine apps through the XSETTINGS mirror in `-showwindows`; outside it, only the proxy's `--settings-only` mirror. |
| `-showwindows`: without the window extension (not installed, `extension=off`, while the screen is locked, GNOME older than 49), a window an app moves itself with a drag handle (a Qt dock, a palette) moves inside X but does not follow on the desktop; with it, it follows. Smooth scrolling arrives as wheel clicks; pinch and rotate are not forwarded. See [no-split.md](no-split.md) and [extension.md](extension.md). | Stock engines | Keep the extension enabled (the default), or use `_NET_WM_MOVERESIZE`-based moves (most toolkits' title bars) or `-showscreen`. |
| Accessibility proxy gaps: key repeat at about 2/3 of the native rate, GTK 4 apps must be started through the proxy, and others. See [accessibility.md](accessibility.md). | Screen reader use | Details in that file. |
| Old X11-only remote-access apps need helpers. See [legacy-remote-access-apps.md](legacy-remote-access-apps.md). | TeamViewer before 15.29 | From 0.1.1 TeamViewer's capture process is a default desktop driver, so no `-xtestdrivers` is needed. With 0.1.0, pass `-xtestdrivers default,TeamViewer_Desktop`. |
| Doctor gaps: it does not check the firewall, its install hints are apt-centric, and it reports the configured engine. | `xwlbridge-ctl doctor` | Check the firewall by hand. |
| GNOME 45 and older: no input. | Old GNOME | Use GNOME on Xorg there. |
| KDE Plasma has no backend. | KDE | None. |
| The default (no-feature) build has only the wlroots backend. | Source builds | Build with `--features gnome`. |
| `rfb_port` and `server_args` go on RealVNC's command line, which RealVNC forgets on reload (measured: `rfb_port=5901` went back to 5900). | RealVNC service | Set parameters in `~/.vnc/config.d/vncserver-x11` and use `rfb_port=realvnc`. Not changed in this release. |
| `-engine xwayland`: a few percent of frames can be torn in fast motion. | `xwayland` engine | Known; still present in 0.3.0. |

## Not tested

- aarch64 GNOME.
- Input Leap/Barrier client.
- Conferencing apps in X11 mode.
- sway and other wlroots compositors beyond labwc, in this fork.
- RealVNC installed from its .deb/.rpm, its system service on port 5900, and install.sh's automatic choice of port 5901.
- The RealVNC tray and UI on EL10.
- The renamed legacy remote-access helper files with TeamViewer.
- TeamViewer 15.29 to 15.7x (portal-based).
