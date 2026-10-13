# X11-only remote-access apps on GNOME (Wayland)

Some remote-access apps can capture the screen and inject input only through X11:
- the screen through XShm/XDamage grabs of the root window;
- input through XTest.

Examples are TeamViewer builds before 15.29 and AnyDesk's incoming sessions. A GNOME session on Wayland has no X display that shows the desktop. These programs therefore refuse the connection, show a black screen, or crash.

xwlbridge gives them one. Its display `:20` mirrors the GNOME desktop through mutter's own remote-desktop APIs. Input and the clipboard go back to GNOME. Most vendor daemons also need to be told that the session is an X11 session on `:20`. This page shows how to do that, and how to check each step.

```
remote viewer <-> vendor daemon (root) --starts--> capture process --X11--> display :20 <-> xwlbridge <-> mutter
                      |                                  |
                      |                                  `-- refuses to capture unless its own windows look
                      |                                      managed by a window manager  -> WM_STATE helper
                      `-- asks logind "what kind of session is this?" -> login1 proxy ("x11 on :20")
```

## Do you need this?

| program | on GNOME 50 (Wayland) without this page |
|---|---|
| TeamViewer 15.82 or newer | works on its own (tested on GNOME 50): it uses mutter's remote-desktop API, unattended. Use a current version if you can. |
| TeamViewer 15.29-15.7x | portal-based, per TeamViewer (not tested): someone must accept each connection |
| TeamViewer 13.x, 14.x, 15.0-15.28 | no Wayland code. On GNOME 50 Wayland, 15.20.6 never connects (stuck at "Connecting..."). On AlmaLinux 10 the client sees 100 % black. Needs this page. |
| x11vnc | run it on `:20`; nothing else is needed |
| RealVNC Server | see [realvnc-server-6.md](realvnc-server-6.md) |

AnyDesk incoming sessions needed the logind proxy plus an `LD_PRELOAD` environment shim. That shim is experimental and is not shipped.

## Before you start

- **Vendor terms.** Check the vendor's licence for your use. TeamViewer's free licence is for personal use. This page does not get around any vendor check.
- **Old builds have known security holes.** Keep an old build reachable on your LAN only. Use the newest build of its line.
- **What the proxy does to your system.** The login1 proxy tells one vendor daemon that your Wayland session is an X11 session on `:20`. It is bound over `/run/dbus` in that daemon's mount namespace only. Every other program keeps talking to the real system bus.
- **It runs as root.** The proxy is a root service. It serves each connection with the connecting client's own uid.

## Requirements

- xwlbridge 0.1.0, built with `--features gnome,xvfb` (see [install.md](install.md)), with `xwlbridge` and `xwlbridge-ctl` in `~/.local/bin`.
- An engine that is a complete X server:

  | distribution | engine |
  |---|---|
  | Ubuntu, Debian, Fedora, openSUSE, Arch | `xvnc` (TigerVNC's Xvnc) |
  | RHEL 10, AlmaLinux 10, Rocky 10 | `xwayland` |

  RHEL 10 packages no Xvnc or Xvfb. Every GNOME install has Xwayland.
- systemd and Python 3; PyGObject (`python3-gi` / `python3-gobject`) for the logind proxy.
- `python3-xlib` for `wm_state.py`. RHEL 10 has no `python3-xlib`; use `wm_state_ctypes.py` there. It calls libX11 through ctypes.
- The helper files, in `contrib/legacy-remote-access/` of a clone of this repository (the release bundle does not
  contain them). They assume the display `:20` that step 1 sets:

  | file | purpose |
  |---|---|
  | `login1_x11_proxy.py`, `login1_x11_proxy_mu.py` | the logind proxy |
  | `wm_state.py` | WM_STATE helper (python-xlib) |
  | `wm_state_ctypes.py` | WM_STATE helper (libX11 through ctypes, for RHEL 10) |
  | `xwlbridge-login1-proxy.service` | unit for the proxy |
  | `teamviewerd-xwlbridge.conf` | drop-in for `teamviewerd.service` |
  | `xwlbridge-wm-state.service` | user unit for the WM_STATE helper |
  | `xwlbridge-restart-teamviewerd.path`, `.service` | daemon restart (Ubuntu/Debian) |
  | `teamviewerd-restart.polkit.rules.in`, `xwlbridge-service-restart-teamviewerd.conf` | daemon restart (RHEL 10) |
  | `xwlbridge-xdg-runtime.sh` | `XDG_RUNTIME_DIR` for `su -` (RHEL 10) |

## 1. The bridge display `:20`

Use the packaged display service. Do not write a unit by hand.

```sh
xwlbridge-ctl config set display 20             # used by xwlbridge.service only
xwlbridge-ctl config set engine xvnc            # RHEL 10: xwayland
xwlbridge-ctl config set bridge_args '-xtestdrivers default,TeamViewer_Desktop'
xwlbridge-ctl --display enable
xwlbridge-ctl --display restart
xwlbridge run -display 20 -- xdpyinfo | head -5   # a display the size of your desktop
```

`-xtestdrivers` names the programs whose XTest input is meant for the desktop. Without it, keys typed by TeamViewer stay on `:20` while TeamViewer's own panel there has the X focus. Adding `TeamViewer_Desktop` to the default drivers is fixed only on a development branch. It is not in 0.1.0, so pass `-xtestdrivers default,TeamViewer_Desktop` as shown.

## 2. The login1 proxy

These programs choose between their X11 and Wayland code from logind's `Session.Type`. The proxy sits between the vendor daemon and the system bus. It reports the console session as X11 on `:20` to the vendor daemon only. Every other message passes through. Each connection is served with the connecting process's own uid.

Install the scripts and the unit:

```sh
cd contrib/legacy-remote-access
sudo install -D -m 0644 login1_x11_proxy.py login1_x11_proxy_mu.py -t /usr/local/lib/xwlbridge-contrib/
sudo install -m 0644 xwlbridge-login1-proxy.service /etc/systemd/system/
sudo install -D -m 0644 teamviewerd-xwlbridge.conf /etc/systemd/system/teamviewerd.service.d/xwlbridge.conf
sudo systemctl daemon-reload
sudo systemctl enable --now xwlbridge-login1-proxy.service
sudo systemctl restart teamviewerd
```

The proxy's runtime directory is `/run/xwlbridge-login1`. The drop-in binds that directory over `/run/dbus` in the mount namespace of `teamviewerd`, so the daemon and the processes it starts see the proxy as their system bus. If the proxy stops, the daemon has no system bus, so the drop-in makes the daemon depend on the proxy.

Check the result:

```sh
S=$(busctl call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager GetSession s "$XDG_SESSION_ID" | awk '{print $2}' | tr -d '"')
busctl get-property org.freedesktop.login1 "$S" org.freedesktop.login1.Session Type Display
busctl --address=unix:path=/run/xwlbridge-login1/system_bus_socket get-property org.freedesktop.login1 "$S" org.freedesktop.login1.Session Type Display
```

On the real bus the session reads `wayland` and an empty display. Through the proxy it reads `x11` and `:20`.

## 3. The WM_STATE helper

TeamViewer refuses to capture until its own windows on `:20` look managed by a window manager. The helper sets `WM_STATE` on the top-level windows of `:20`.

```sh
mkdir -p ~/.local/lib/xwlbridge-contrib ~/.config/systemd/user
install -m 0644 wm_state.py ~/.local/lib/xwlbridge-contrib/wm_state.py
install -m 0644 xwlbridge-wm-state.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now xwlbridge-wm-state.service
```

On RHEL 10 install the ctypes variant under the same name, so the unit runs it unchanged:
`install -m 0644 wm_state_ctypes.py ~/.local/lib/xwlbridge-contrib/wm_state.py`.

The unit has `BindsTo` and `WantedBy` set to `xwlbridge.service`. Keep it that way.

## 4. Restart the daemon when `:20` is recreated

After the bridge restarts, TeamViewer keeps the old display's X authority and refuses new connections until `teamviewerd` restarts. There are two variants.

**Ubuntu and Debian: a path unit.**

```sh
sudo install -m 0644 xwlbridge-restart-teamviewerd.path xwlbridge-restart-teamviewerd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now xwlbridge-restart-teamviewerd.path
```

**RHEL 10: a polkit rule and a user drop-in.** SELinux denies the path watch there. Replace `@USER@` with your user name:

```sh
sed "s/@USER@/$USER/" teamviewerd-restart.polkit.rules.in | sudo tee /etc/polkit-1/rules.d/50-xwlbridge-teamviewerd.rules
mkdir -p ~/.config/systemd/user/xwlbridge.service.d
install -m 0644 xwlbridge-service-restart-teamviewerd.conf ~/.config/systemd/user/xwlbridge.service.d/restart-teamviewerd.conf
systemctl --user daemon-reload
systemctl --user restart xwlbridge.service
```

**RHEL 10 only: `XDG_RUNTIME_DIR` and the firewall.** A login shell started with `su -` has no `XDG_RUNTIME_DIR`. Install the profile snippet:

```sh
sudo install -m 0644 xwlbridge-xdg-runtime.sh /etc/profile.d/xwlbridge-xdg-runtime.sh
```

For direct LAN connections, open TeamViewer's port 5938/tcp:

```sh
sudo firewall-cmd --permanent --add-port=5938/tcp
sudo firewall-cmd --reload
```

## 5. What to expect

TeamViewer 15.20.6 (2021) worked through the bridge, unattended, including reboot and logout/login:
- on Ubuntu 26.04 / GNOME 50 with `-engine xvnc`;
- on AlmaLinux 10.2 / GNOME 49 with `-engine xwayland`.

Ubuntu result, compared with the same TeamViewer on "Ubuntu on Xorg" (GNOME 46):

| | through xwlbridge | native Xorg |
|---|---|---|
| still picture (PSNR against the screen) | 99 dB (identical) | 57.5 dB (identical outside TeamViewer's own panel) |
| resolution changes, 200 % scale, cursor | correct | correct |
| typing, shortcuts, key repeat, buttons 1-3, wheel, drag | correct | correct |
| clipboard text, UTF-8, images, both ways; file transfer | works | works |
| two monitors | one combined picture | the primary monitor, with a switch |
| screen locked | the session stays; viewers see a padlock and cannot unlock; correct again after a local unlock | the real lock screen |
| login screen | not reachable | not reachable |
| reboot or new login, nobody at the computer | connects | connects |
| 60 fps animation delivered | 16 fps | 17.4 fps |
| click to picture, p50 | 158 ms | 145 ms |

The frame rates and delays were measured on busy VMs without a GPU. The bridge uses more CPU in fast motion.

AlmaLinux 10.2: it worked through the bridge with `-engine xwayland`, unattended, including reboot and logout/login.

The helper files were renamed for this release and were not re-run with TeamViewer after the renaming.

Known gaps on the bridge side:
- AltGr (level-3) characters that TeamViewer types with an XKB latch arrive as the base key.
- Characters outside the layout are not typed.
- Each extra monitor is not its own output.
- A bridge restart ends the vendor's session. Reconnecting works once its processes restart.
- With `-engine xwayland`, a few percent of frames can be torn during fast motion.
- The login screen is not reachable. While the screen is locked, viewers see a padlock unless the lock-screen extension in `contrib/gnome-shell` is enabled.

## Troubleshooting

| symptom | cause | fix |
|---|---|---|
| the viewer stays at "Connecting..." | the vendor daemon still sees a Wayland session | step 2: check the proxy with the `busctl` commands, check the drop-in with `systemctl cat teamviewerd`, then restart the daemon |
| black picture or "The screen cannot be captured at the moment" | the vendor's windows on `:20` have no `WM_STATE` | step 3: `systemctl --user status xwlbridge-wm-state` |
| mouse works, keys do nothing | the capture process is not a desktop driver | check `bridge_args` in step 1 |
| it worked until the bridge restarted; now every connection fails | the daemon kept the old display's state | step 4, or restart `teamviewerd` |
| the session ends about 20 s after connecting (RHEL 10) | no `XDG_RUNTIME_DIR` in the `su -` shell | step 4: the profile snippet |
| the helper units are inactive after a reboot | a unit ordering problem | keep `WantedBy=xwlbridge.service` in the WM_STATE unit |
| a padlock instead of the desktop | the screen is locked | unlock at the computer |

## Removing it

```sh
systemctl --user disable --now xwlbridge-wm-state.service
rm -f ~/.config/systemd/user/xwlbridge-wm-state.service
rm -f ~/.config/systemd/user/xwlbridge.service.d/restart-teamviewerd.conf
rm -rf ~/.local/lib/xwlbridge-contrib
xwlbridge-ctl --display disable
sudo systemctl disable --now xwlbridge-restart-teamviewerd.path xwlbridge-login1-proxy.service
sudo rm -f /etc/systemd/system/xwlbridge-restart-teamviewerd.{path,service} /etc/systemd/system/xwlbridge-login1-proxy.service \
  /etc/systemd/system/teamviewerd.service.d/xwlbridge.conf /etc/profile.d/xwlbridge-xdg-runtime.sh \
  /etc/polkit-1/rules.d/50-xwlbridge-teamviewerd.rules
sudo rm -rf /usr/local/lib/xwlbridge-contrib
sudo systemctl daemon-reload && sudo systemctl restart teamviewerd
systemctl --user daemon-reload
```

On Ubuntu and Debian the `.path` unit is the restart variant. On RHEL 10 the polkit rule and the user drop-in are. Commands for the variant you did not install report that a file or unit is missing. This is harmless.
