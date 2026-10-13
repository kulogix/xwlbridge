# Helpers for X11-only remote-access servers

Files for running remote-access servers that only know X11 (TeamViewer before 15.29, for one) on a GNOME Wayland
desktop through the bridge's display `:20`. The how-to, what was tested and the known limits are in
[docs/legacy-remote-access-apps.md](../../docs/legacy-remote-access-apps.md).

| file | install as | what |
|---|---|---|
| `login1_x11_proxy.py`, `login1_x11_proxy_mu.py` | `/usr/local/lib/xwlbridge-contrib/` | a system-bus proxy that reports the console session to one vendor daemon as X11 on `:20` (logind `Session.Type`, `Display`); every other message unchanged; each connection served with the client's own uid |
| `xwlbridge-login1-proxy.service` | `/etc/systemd/system/` | runs the proxy (root) |
| `teamviewerd-xwlbridge.conf` | `/etc/systemd/system/teamviewerd.service.d/xwlbridge.conf` | binds the proxy over `/run/dbus` in the vendor daemon's mount namespace only |
| `wm_state.py` (python-xlib) or `wm_state_ctypes.py` (libX11 through ctypes; RHEL 10) | `~/.local/lib/xwlbridge-contrib/wm_state.py` | ICCCM `WM_STATE` on the top-level windows of `:20`, as a window manager sets it |
| `xwlbridge-wm-state.service` | `~/.config/systemd/user/` | runs the WM_STATE helper with `xwlbridge.service` |
| `xwlbridge-restart-teamviewerd.path`, `.service` | `/etc/systemd/system/` | Ubuntu/Debian: restart teamviewerd when `:20` is recreated |
| `teamviewerd-restart.polkit.rules.in`, `xwlbridge-service-restart-teamviewerd.conf` | `/etc/polkit-1/rules.d/50-xwlbridge-teamviewerd.rules` (`@USER@` replaced), `~/.config/systemd/user/xwlbridge.service.d/restart-teamviewerd.conf` | RHEL 10: the same restart, from the bridge's own unit |
| `xwlbridge-xdg-runtime.sh` | `/etc/profile.d/` | RHEL 10: `XDG_RUNTIME_DIR` for processes a daemon starts with `su -` |

Status: experimental. These are the files of the recipe tested with TeamViewer 15.20.6 on Ubuntu 26.04 (GNOME 50)
and AlmaLinux 10.2 (GNOME 49); for this release they were renamed and were not re-run with TeamViewer afterwards.
They do not get around any vendor check: check the vendor's licence for your use.
