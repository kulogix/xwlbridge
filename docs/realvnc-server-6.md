# RealVNC Server 6 on GNOME (Wayland): the picture from the bridge, the UI on GNOME

RealVNC Server's user mode (`vncserver-x11`) shares an X display. A GNOME session on Wayland has no X display that
shows the desktop, so RealVNC has nothing to share. xwlbridge gives it one: an X display that mirrors the GNOME desktop
through mutter's own remote-desktop APIs (ScreenCast over PipeWire for the picture, RemoteDesktop with libei for
input, the remote-desktop clipboard).

With `-uidisplay auto` RealVNC's own user interface runs on GNOME's Xwayland instead of on the bridge's display: the
tray icon, the status window, Options, the Information Center, connection notices and accept/reject prompts, chat
and file transfer are ordinary windows on your desktop.

    VNC viewers <-> RealVNC Server (vncserver-x11-core, vncagent) <-> bridge X display <-> xwlbridge <-> mutter
                        `-- vncserverui (status, tray icon, Options, notices, chat, files, clipboard) <-> GNOME's Xwayland

You need your own RealVNC Server licence. This project contains no RealVNC software and changes nothing about how
RealVNC is licensed.

## What was tested

- **RealVNC Server 6.11 (licensed), user mode**: picture, input, clipboard both ways, audio, printing, file transfer
  both ways and 30 fps video with the real RealVNC Viewer 6.22, in containers running headless mutter 50.1;
  multi-monitor and scaling on mutter 46, 48 and 50; daily use on an Ubuntu 26.04 desktop.
- **Fresh Ubuntu 26.04.1 VM** (GNOME Shell 50.1, mutter 50.1, RealVNC Server 6.11.0), following this page and
  [install.md](install.md) with the release bundle: a RealVNC Viewer with a VNC password and `Encryption=AlwaysOn` saw
  the VM's screen (similarity 0.9993) and its typing reached the desktop; the tray icon, the status window, its menu
  and Options worked with the machine's own mouse and keyboard; the service came back by itself after a logout/login
  (within 1 s, and the viewer reconnected) and after a reboot with automatic login.
- **Fresh AlmaLinux 10.2 VM** (GNOME 49.4): the service with `engine=xwayland` (through the unit's command line at the
  time), logout/login and reboot; a viewer with password and encryption. The tray icon and RealVNC's UI were not
  tested there.
- After the rename to xwlbridge: RealVNC Server 6.11 next to x11vnc on one display (four viewers, concurrent input),
  21/21 checks on the builtin, xvnc and xvfb engines; the picture/typing/scrolling/cursor/monitor-change checks
  11 or 12 of 12 per engine and run (the miss: a click-latency bound of 60 ms at the 95th percentile, about 70 ms
  measured on a heavily loaded host; the final binary made 11/12 on all three engines).
- **Not tested**: RealVNC installed from its own .deb or .rpm (the tests used a copy of a licensed install), and with
  it RealVNC's system service on port 5900 and install.sh's automatic choice of port 5901.
- **RealVNC Server 7.17 (unlicensed)**: starts and shows its UI on GNOME the same way, but it accepts no viewers
  without a licence. See [RealVNC Server 7](#realvnc-server-7).

## Requirements

- GNOME on Wayland with mutter 46 or newer and an xwlbridge built with the GNOME backend (the release bundle is).
  See [install.md](install.md).
- RealVNC Server for Linux, user mode (`vncserver-x11`), licensed (`sudo vnclicense -add <key>` for 6.x).
- For the tray icon: GTK 2 at run time and a GNOME Shell extension that hosts legacy tray icons. See
  [Tray icon](#tray-icon). Without them RealVNC still works; its status window opens as a normal window.

## Quick start

1. **RealVNC's settings go in its per-user file**, not on its command line (RealVNC forgets command-line parameters
   when it reloads its settings, see [below](#configure-realvnc-in-its-file-not-on-the-command-line)):

   ```sh
   mkdir -p ~/.vnc/config.d
   vncpasswd -user                         # asks for a VNC password; writes it to ~/.vnc/config.d/vncserver-x11
   chmod 600 ~/.vnc/config.d/vncserver-x11 # vncpasswd creates it readable by others
   cat >> ~/.vnc/config.d/vncserver-x11 <<'EOF'
   Authentication=VncAuth
   Encryption=AlwaysOn
   RfbPort=5900
   EOF
   ```

2. **Install xwlbridge with the release bundle** (or `packaging/install.sh` from a source build). With RealVNC Server
   installed, `install.sh` enables and starts `xwlbridge-realvnc.service`, which runs RealVNC on the bridge every
   time you log in to GNOME:

   ```sh
   ./install.sh
   ~/.local/bin/xwlbridge-ctl config set rfb_port realvnc   # let RealVNC's file decide the port
   ~/.local/bin/xwlbridge-ctl restart
   ~/.local/bin/xwlbridge-ctl doctor
   ```

   To try it by hand instead (Ctrl+C stops it): `xwlbridge -backend gnome -uidisplay auto vncserver-x11`.

**Never run two RealVNC user-mode servers.** RealVNC runs one per user: the second exits with
`Existing server found; exiting.` (status 5). Do not start `vncserver-x11` by hand, or add a unit of your own, while
`xwlbridge-realvnc.service` runs; apply RealVNC settings with `vncserver-x11 -reload` or `xwlbridge-ctl restart`.

The first time it runs, RealVNC shows an **Initial settings** prompt on the desktop (update checks, usage data).
Until you answer it, it blocks the rest of RealVNC's UI. You can record the answers in advance:

```sh
printf 'EnableAnalytics=0\nEnableAutoUpdateChecks=0\n' >> ~/.vnc/config.d/vncserver-x11
```

### The service

`xwlbridge-realvnc.service` is a systemd user unit, part of `graphical-session.target`: it starts after you log in to
GNOME and stops when you log out. It runs `xwlbridge-ctl run`, which reads `~/.config/xwlbridge/xwlbridge.conf`
(`xwlbridge-ctl config show`; the settings are explained in [packaging/README.md](../packaging/README.md)).

- Nobody can connect after a reboot until you have logged in at the machine, unless GNOME logs you in automatically
  (Settings > System > Users > Automatic Login).
- If you stop RealVNC from its own menu (Stop VNC Server), it stays stopped until `xwlbridge-ctl start` or your next
  login.
- `xwlbridge-ctl logs` shows the bridge's and RealVNC's log (RealVNC's level: setting `server_log`).

### Engines

The engine is the X server RealVNC talks to. Any of them works for RealVNC's picture and input; with
`-uidisplay auto` everything RealVNC needs works on each. Choose with `xwlbridge-ctl config set engine ENGINE`.

- `auto` (the default): `xvnc` when TigerVNC's Xvnc is installed, else `builtin`.
- `builtin`: xwlbridge's own X server; no extra packages.
- `xvnc`: TigerVNC's Xvnc, a complete X server that follows monitor changes. Choose it when other X11 tools should
  share the display.
- `xvfb`: Xorg's Xvfb; complete, fixed screen size.
- `xwayland`: a rootful Xwayland on a private compositor; complete, follows monitor changes. On RHEL 10 and its
  rebuilds, which package no Xvnc or Xvfb, this is the complete engine.

With RealVNC installed, `/usr/bin/Xvnc` is **RealVNC's** set-uid launcher, not TigerVNC's (on Debian/Ubuntu whichever
package came first; TigerVNC's server is then `/usr/bin/Xtigervnc`). xwlbridge picks `Xtigervnc` itself and refuses
set-uid X servers and RealVNC's Xvnc; `-xserver PATH` names the binary explicitly. On Fedora, RHEL and openSUSE,
RealVNC's rpm conflicts with TigerVNC's server: use `builtin` or `xwayland` there.

## Configure RealVNC in its file, not on the command line

RealVNC treats parameters on its command line (`vncserver-x11 -RfbPort=5900 ...`) as preferences of the running
server only. The first time the Options dialog applies a change (or `vncserver-x11 -reload` runs), the server reloads
its settings from its files and **the command-line values are gone**: measured, a port given on the command line
(5901) went back to the file's value (5900), and `-Authentication=VncAuth` fell back to system authentication.

`xwlbridge-ctl` passes its `rfb_port` and `server_args` settings on RealVNC's command line, so they have the same
problem. Keep RealVNC's parameters in `~/.vnc/config.d/vncserver-x11` (the file the Options dialog edits) and set
`rfb_port=realvnc` and an empty `server_args`:

```
# ~/.vnc/config.d/vncserver-x11 (mode 0600)
RfbPort=5900
Authentication=VncAuth
Encryption=AlwaysOn
```

Files RealVNC reads for the user-mode server, later ones winning: `/etc/vnc/config.d/common`,
`/etc/vnc/config.d/vncserver-x11`, `~/.vnc/config.d/common`, `~/.vnc/config.d/vncserver-x11`, then the command line,
then the policy files in `/etc/vnc/policy.d/`, which override everything. Useful commands, run as you:

```sh
vncserver-x11 -getconfig RfbPort          # the value the running server uses
vncserver-x11 -reload                     # apply edits to the files without a restart
xwlbridge-ctl realvnc-get RfbPort         # what the files say, without asking the server
```

## Authentication

- **VNC password (recommended)**: `vncpasswd -user` and `Authentication=VncAuth`. No root is involved and RealVNC
  Viewer logs in with the password. The stored value is an obfuscation, not a hash: keep the file mode 0600.
- **System authentication** (`Authentication=SystemAuth`): with `-uidisplay` only your own account can log in (next
  section).
- `Encryption=AlwaysOn` refuses viewers that cannot encrypt.
- `QueryConnect=1` asks you on the desktop to accept or reject each connection; with `-uidisplay` you can see and
  answer the prompt.

## What `-uidisplay` does, and what it costs

RealVNC has no setting for the display of its UI: its core starts `vncserverui` itself, on the display it shares.
xwlbridge therefore preloads a small hook (built from `uihook/uihook.rs`, embedded in the binary and written to
`$XDG_RUNTIME_DIR`) that, in `vncserverui` only, points `DISPLAY` and `XAUTHORITY` at the desktop's Xwayland. The
set-uid launcher `vncserver-x11` would drop the preload, so xwlbridge starts the program it would start,
`vncserver-x11-core`, directly. RealVNC then runs entirely as you, without its root helper:

| | with `-uidisplay auto` | without (`-uidisplay none`) |
|---|---|---|
| RealVNC's windows, tray icon, Options, file transfer, chat, prompts | on your desktop | on the bridge's display, out of sight |
| VNC password (`Authentication=VncAuth`) | works | works |
| System authentication | only for the account that runs the server | any permitted account |
| Remote printing (the viewer's printer added as a CUPS queue) | needs you in a CUPS admin group (`lpadmin` on Debian/Ubuntu, `wheel` on Fedora/RHEL/Arch) | works (root helper) |
| PRIMARY selection | RealVNC's UI reads and sets the desktop's PRIMARY itself | through the bridge |

Before starting RealVNC, xwlbridge checks that it can use the desktop's display (waiting up to 30 s for the session's
Xwayland) and whether a system tray is there. If the display cannot be used yet, RealVNC starts anyway with its UI on
the bridge's display, and xwlbridge moves the UI to the desktop once the display answers. The hook is written only
into a directory owned by you and writable by no one else.

With the built-in engine xwlbridge always runs RealVNC's core through the hook (RealVNC's UI is known to crash in a
loop otherwise on that X server). Keep `vncserver-x11` itself as the wrapped command: through `env`, a shell or another
wrapper the set-uid launcher runs in between and drops the hook (xwlbridge warns).

## The system service on port 5900

RealVNC's package enables its **service mode**, `vncserver-x11-serviced.service`, which runs as root, listens on port
5900 and shares the console's X server. A GNOME Wayland session has none, so it cannot share your desktop, but it
still holds port 5900. RealVNC's user mode does not move to another port when 5900 is taken: it keeps retrying and
accepts nobody. Choose one:

1. Disable the system service: `sudo systemctl disable --now vncserver-x11-serviced.service`.
2. Keep it and serve your desktop on another port: `RfbPort=5901` in `~/.vnc/config.d/vncserver-x11`, then
   `vncserver-x11 -reload`. Viewers connect to `host::5901` (RealVNC Viewer syntax).

`install.sh` chooses port 5901 by itself when 5900 is taken at install time (this was not exercised in the tests).
GNOME's app grid shows RealVNC's **VNC Server** launcher, which starts the system service, not your user-mode server;
use the tray icon, `xwlbridge-ctl show` or the **VNC Server (Wayland desktop)** entry that install.sh adds.

## Firewall

- RHEL, AlmaLinux and Rocky 10: firewalld's default zone drops the VNC port (measured). Open it with firewalld's own
  service (ports 5900-5903/tcp), or another port:

  ```sh
  sudo firewall-cmd --permanent --add-service=vnc-server
  sudo firewall-cmd --reload
  # another port: sudo firewall-cmd --permanent --add-port=5910/tcp && sudo firewall-cmd --reload
  ```

- Ubuntu has no firewall on by default (ufw users: `sudo ufw allow 5900/tcp`). Fedora Workstation's default zone
  already allows ports 1025-65535.
- Keep VNC on your LAN or behind an SSH tunnel; with RealVNC use a password **and** `Encryption=AlwaysOn`.

## Tray icon

RealVNC's tray icon is a legacy (XEmbed) icon drawn with GTK 2, which RealVNC loads at run time. GNOME Shell shows such
icons only through an extension:

| distribution | extension (UUID) | package | GTK 2 package |
|---|---|---|---|
| Ubuntu 24.04 / 26.04 | Ubuntu AppIndicators (`ubuntu-appindicators@ubuntu.com`), enabled by default | `gnome-shell-extension-appindicator` (24.04), `gnome-shell-ubuntu-extensions` (26.04) | `libgtk2.0-0t64` |
| Debian 13 | the same UUID | `gnome-shell-extension-appindicator` | `libgtk2.0-0t64` |
| Fedora 43 | AppIndicator and KStatusNotifierItem Support (`appindicatorsupport@rgcjonas.gmail.com`) | `gnome-shell-extension-appindicator` | `gtk2` |
| RHEL / AlmaLinux / Rocky 10 | the same, from EPEL | `gnome-shell-extension-appindicator` (EPEL) | `gtk2` (EPEL) |
| Arch Linux | the same | `gnome-shell-extension-appindicator` | `gtk2` from the AUR |
| openSUSE Tumbleweed | from extensions.gnome.org (extension 615) | — | `libgtk-2_0-0` |

Enable the extension with `gnome-extensions enable <UUID>`, then log out and back in. Left-click on the icon opens the
status window; right-click opens RealVNC's menu (Information Center, Options, File Transfer, Chat, Disconnect, Stop VNC
Server, ...). Without the extension or GTK 2 there is no icon: open the status window with `xwlbridge-ctl show` (or
`vncserver-x11 -showstatus`).

## RealVNC Server 7

RealVNC Server 7.17.0 (its .deb) was tested the same way, **unlicensed**: the same command works, its set-uid launcher
and `vncserver-x11-core` are still there, and its UI windows all appeared on GNOME's Xwayland. Unlicensed, it starts
and shows its UI but nothing listens on TCP (`Licensing: No license` in its log), so no viewer can connect. Its
configuration files and their order are the same as 6.x. Whether 6.x licence keys license 7.x was not tried.

## Troubleshooting

| symptom | cause / fix |
|---|---|
| `Existing server found; exiting.` (status 5) | another RealVNC user-mode server runs for you: stop it; `xwlbridge-ctl doctor` names it |
| nothing listens on the port | `ss -ltnp \| grep vnc`; `vncserver-x11 -getconfig RfbPort`; RealVNC's system service may hold 5900; on RHEL 10 check firewalld; an unlicensed server listens on nothing |
| the port changed back after Options > Apply | it came from the command line (`rfb_port`): put `RfbPort=` in `~/.vnc/config.d/vncserver-x11` and set `rfb_port=realvnc` |
| the viewer asks for a user name instead of a password | `Authentication=VncAuth` is not in effect: put it in `~/.vnc/config.d/vncserver-x11` |
| `-uidisplay auto: ... no DISPLAY` | xwlbridge was started outside the GNOME session: use the service, a terminal in the session, or `-uidisplay :0` |
| `Unrecognized option: -displayfd` | the engine found RealVNC's `/usr/bin/Xvnc`: install TigerVNC (`Xtigervnc`) or pass `-xserver /usr/bin/Xtigervnc` |
| no tray icon | check GTK 2 and the extension (`gnome-extensions list --enabled`); `xwlbridge-ctl show` |
| printing from the viewer fails with `client-error-forbidden` | join a CUPS admin group (`sudo usermod -aG lpadmin $USER` on Debian/Ubuntu), log out and in |

More: [troubleshooting.md](troubleshooting.md) and `xwlbridge-ctl doctor`.
