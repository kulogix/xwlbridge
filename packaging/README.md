# xwlbridge for GNOME: RealVNC Server on a Wayland desktop

RealVNC Server 6.x/7.x in user mode only knows X11. On a GNOME Wayland desktop
(Ubuntu 24.04 and later, Debian 13, Fedora 41 and later) there is no X server
for it to share. xwlbridge gives it one: an X display backed by GNOME's own
remote-desktop APIs (mutter's ScreenCast via PipeWire, RemoteDesktop/libei for
input, the remote-desktop clipboard). RealVNC's own windows (status dialog,
tray icon, connection notices, chat, file transfer) show up on the GNOME desktop.

    VNC viewer  ->  RealVNC Server (user mode)  ->  X display (engine)  ->  xwlbridge  ->  GNOME (mutter)

This directory installs it as a per-user systemd service that starts with your
GNOME session. Nothing here needs root, except the optional `apt install` and
the choice about RealVNC's system service described below.

## Install

From a release bundle:

    tar xzf xwlbridge-<version>-x86_64-<distro>.tar.gz
    cd xwlbridge-<version>-x86_64-<distro>
    ./install.sh

From the source tree:

    cargo build --profile dist --features gnome,xvfb
    packaging/install.sh

`install.sh` puts `xwlbridge` and `xwlbridge-ctl` in `~/.local/bin`, two user
units in `~/.config/systemd/user/` (`xwlbridge-realvnc.service`: RealVNC Server
on the bridge; `xwlbridge.service`: the bridge's X display alone, see
[below](#the-bridge-display-alone-xwlbridgeservice)), the settings in
`~/.config/xwlbridge/xwlbridge.conf`, an app-grid entry in
`~/.local/share/applications/`, enables one of the units for your GNOME
sessions (`xwlbridge-realvnc.service` when RealVNC Server's `vncserver-x11` is
installed, else `xwlbridge.service`; `--service realvnc|display` chooses; an
upgrade keeps the one you had), starts it, and runs `xwlbridge-ctl doctor`
(`~/.local/bin/xwlbridge-ctl` until `~/.local/bin` is on your PATH, on Ubuntu
after your next login). It also takes over an xwlrvnc install, once
([below](#coming-from-xwlrvnc)).

Other options: `--bin PATH`, `--prefix DIR`, `--port N|realvnc`, `--engine
auto|builtin|xvnc|xvfb|xwayland` (for a new settings file), `--no-enable`,
`--no-start`. Running a newer `install.sh` upgrades
in place (settings are kept; the service restarts, so connected viewers
reconnect).

Requirements: GNOME on Wayland, PipeWire (standard), RealVNC Server installed
and licensed (`sudo vnclicense -add <key>`). Built binaries need glibc as new
as the distro they were built on: use the bundle built for your distro.

## Check the setup

    xwlbridge-ctl doctor          # or: xwlbridge-ctl doctor --json

It reports, without printing any key or password: the GNOME/mutter version and
its remote-desktop APIs, PipeWire, Xwayland, the bridge binary and settings, the
X server engines available (and refuses RealVNC's own `Xvnc`), RealVNC Server's
version, licence, authentication and encryption, RealVNC's system service and
the port, the AppIndicator extension (tray icon), GNOME's own Remote Desktop
settings, automatic login, the screen lock, and the service's state.

## RealVNC's system service and port 5900

The RealVNC package enables `vncserver-x11-serviced` (service mode). On a GNOME
Wayland desktop it finds no X server on the console, so it accepts viewers on
port 5900 and drops them after authentication. It cannot be made to work there;
this user-mode setup is what shares the desktop. Choose one:

1. **Disable the system service** (viewers keep connecting to port 5900):

        sudo systemctl disable --now vncserver-x11-serviced
        xwlbridge-ctl config set rfb_port realvnc && xwlbridge-ctl restart   # RealVNC's RfbPort: 5900 unless set

2. **Keep it**, and serve the desktop on another port (no root needed): viewers
   connect to `host:5901` (RealVNC Viewer: `host::5901`).

        xwlbridge-ctl config set rfb_port 5901 && xwlbridge-ctl restart

`install.sh` picks 5901 automatically while the system service holds 5900.

## Default configuration (Ubuntu 26.04, GNOME 50, RealVNC Server 6.11)

`engine=auto`, `uidisplay=auto`, `capture=area`, RealVNC's own authentication
(by default your Linux login; or a VNC password), port 5900 (5901 while
RealVNC's system service holds 5900).

- **engine=auto**: with no extra packages that is `builtin`; after
  `sudo apt install tigervnc-standalone-server` it becomes `xvnc` on the next
  start. Measured on mutter 50.1 with RealVNC 6.11 (9 interleaved runs: 4
  builtin, 5 xvnc, one of them without its clipboard and CPU phases; host load
  average 100-140): the same click-to-picture
  latency (median p50 45.6 ms builtin vs 44.0 ms xvnc, p95 69 vs 70 ms), the
  same functional and clipboard results (the release tests' functional checks
  and the four clipboard paths through RealVNC: all pass on both), about the same CPU
  while an animation is watched (median 16 vs 17 % of a core for the stack),
  and about 70 MiB less memory without Xvnc (84 vs 157 MiB idle). So nothing needs
  installing for RealVNC; TigerVNC is worth installing when other X11 tools
  (xdotool, x11vnc, recorders) should see the whole desktop with every X
  extension.
- **uidisplay=auto**: RealVNC's windows (tray icon, status and Options,
  connection notices, chat, file transfer) and its clipboard work on the GNOME
  desktop. Without it, RealVNC's clipboard does not work on the built-in engine.
- **capture=area**: mutter older than 51 crashes when a monitor recorded with
  RecordMonitor is disabled.

## One RealVNC user-mode server per user

RealVNC runs at most one user-mode server per user: a second `vncserver-x11`
hands over to the first ("Existing server found; exiting.") and quits. If you
started one by hand (for example `xwlbridge ... vncserver-x11 -RfbPort=...` in a
terminal), stop it: until then the service waits (it retries with a back-off of
up to 2 minutes and logs who is in the way) and then starts by itself.
`xwlbridge-ctl doctor` names the process.

To replace such a server for good, keep its RealVNC settings: copy `RfbPort`,
`IpClientAddresses`, `Authentication`, `Encryption` and `Password` from the
`.vnc/config.d/vncserver-x11` it used into your own
`~/.vnc/config.d/vncserver-x11` (mode 0600), and its `.vnc/private.key` if
viewers should keep seeing the same server identity; then
`./install.sh --no-start --port realvnc`, stop the old server, and
`xwlbridge-ctl start`.

## Settings

`~/.config/xwlbridge/xwlbridge.conf` (commented; `xwlbridge-ctl config show` prints
the effective values). Change with `xwlbridge-ctl config set KEY VALUE`, then
`xwlbridge-ctl restart`.

| key | default | meaning |
|---|---|---|
| `engine` | `auto` | X server: `builtin` (no extra packages), `xvnc` (TigerVNC's Xvnc: every X extension, follows monitor changes), `xvfb`, `xwayland` (a rootful Xwayland on a private compositor: every X extension; the complete engine on RHEL 10); `auto` = xvnc if TigerVNC is installed, else builtin |
| `capture` | `area` | `area` (RecordArea, safe everywhere) or `monitor` (RecordMonitor; mutter >= 51 only, older mutter crashes when a recorded monitor is disabled: [mutter#2718](https://gitlab.gnome.org/GNOME/mutter/-/issues/2718)) |
| `uidisplay` | `auto` | where RealVNC's windows appear: `auto` = the GNOME desktop (Xwayland), `none` = hidden (engine `xvnc`/`xvfb` only), or an X display |
| `rfb_port` | `5900` | TCP port (`-RfbPort`); `realvnc` or empty = RealVNC's own `RfbPort` setting |
| `server` | `vncserver-x11` | the RealVNC Server program |
| `server_log` | `30` | RealVNC log level sent to the journal; empty = RealVNC's own logging |
| `server_args` | | more RealVNC parameters, e.g. `-Authentication=VncAuth -Encryption=AlwaysOn` |
| `log` | `normal` | bridge logging: `quiet`, `normal`, `verbose` |
| `bridge_args` | | more `xwlbridge` options (`xwlbridge -help`) |
| `xvnc`, `xvfb` | `auto` | engine binaries; auto = found on PATH (`Xtigervnc` first) |
| `backend` | `gnome` | `gnome`, `wlr` or `auto` |
| `bridge` | `auto` | the `xwlbridge` binary; auto = next to `xwlbridge-ctl`, else on PATH |
| `display` | `auto` | `xwlbridge.service` only: the display number (e.g. `20`); auto = the bridge picks one |

RealVNC's own settings (authentication, encryption, permissions, ...) stay in
RealVNC's files and its Options dialog (status window > menu > Options), e.g. a
VNC password: `vncpasswd -user` and `Authentication=VncAuth` in
`~/.vnc/config.d/vncserver-x11`.

**`rfb_port` and `server_args` go on RealVNC's command line, and RealVNC forgets
its command line when it reloads its settings** (Options > Apply,
`vncserver-x11 -reload`): measured, `rfb_port=5901` went back to the file's 5900
after a reload. Prefer `rfb_port=realvnc`, an empty `server_args`, and RealVNC's
parameters in its file (next section). This is not changed in this release.

### Leaving port, IP filter and authentication to RealVNC

With `rfb_port=realvnc` and an empty `server_args`, xwlbridge passes RealVNC
none of its connection settings: the port (`RfbPort`), the IP address filter
(`IpClientAddresses`), authentication and encryption all come from RealVNC's
own configuration, `~/.vnc/config.d/vncserver-x11` (what its Options dialog
writes in user mode; later lines win) and `/etc/vnc/config.d/common.custom`
(policy files in `/etc/vnc/policy.d/` override both). For example:

    xwlbridge-ctl config set rfb_port realvnc
    vncpasswd -user                                   # sets Password=
    cat >>~/.vnc/config.d/vncserver-x11 <<'EOF'
    RfbPort=5910
    IpClientAddresses=+192.168.1.0/255.255.255.0,+127.0.0.1,-
    Authentication=VncAuth
    Encryption=AlwaysOn
    EOF
    chmod 600 ~/.vnc/config.d/vncserver-x11
    xwlbridge-ctl restart && xwlbridge-ctl doctor

`install.sh` chooses `rfb_port=realvnc` by itself when RealVNC's settings
already set `RfbPort`, and takes `--port realvnc`. `xwlbridge-ctl doctor` shows
the effective port, the IP filter and authentication with where each comes
from; `xwlbridge-ctl realvnc-get NAME` prints one of RealVNC's values.

The first time RealVNC's windows appear on the desktop it asks for its "Initial
settings": automatic update checks and anonymous usage data, both ticked.
Untick them before OK if you do not want them.

### Engines

- `builtin`: xwlbridge's own X server, implementing what VNC servers need. No
  extra packages. With `uidisplay=auto` RealVNC's clipboard, file transfer and
  connection prompts work through its UI on the GNOME desktop. RealVNC's own
  windows crash on the built-in X server itself, so with `builtin` the service
  never starts RealVNC without them on the desktop: `uidisplay=none` is refused
  (settings error), and while the desktop's X display is missing the service
  waits and retries instead.
- `xvnc`: TigerVNC's Xvnc (`sudo apt install tigervnc-standalone-server`), a
  complete Xorg-based X server that the bridge keeps in step with the desktop;
  for any X11 tool besides RealVNC (xdotool, x11vnc, screen recorders, ...).
  On a RealVNC machine that package also repoints `/usr/bin/vncserver`
  (RealVNC's virtual mode) to TigerVNC's; see docs/troubleshooting.md for the
  one-line restore.
- `xvfb`: Xorg's Xvfb, complete but a fixed screen size.

**Never RealVNC's Xvnc.** The RealVNC package installs its own `/usr/bin/Xvnc`
(a set-uid root launcher for its virtual-mode server). `xwlbridge-ctl` only
accepts a TigerVNC binary (`Xtigervnc`, or an `Xvnc` that is not set-uid/set-gid,
not owned by a RealVNC package and carries TigerVNC's identification), never
executes a candidate while checking it, and hands the bridge a private PATH
directory (`$XDG_RUNTIME_DIR/xwlbridge/bin`) whose `Xvnc` is exactly that binary.
Installing TigerVNC next to RealVNC is safe: dpkg keeps RealVNC's real
`/usr/bin/Xvnc` file and TigerVNC's server stays at `/usr/bin/Xtigervnc`.

## The bridge display alone (`xwlbridge.service`)

For X11 tools (screen recorders, automation, x11vnc, a remote-access server)
that need a stable X display mirroring the desktop, without RealVNC:

    xwlbridge-ctl config set display 20          # optional: a fixed number
    xwlbridge-ctl --display enable               # start with every GNOME login
    xwlbridge-ctl --display restart              # start (or restart) it now
    xwlbridge-ctl --display show                 # DISPLAY=:20   (xwlbridge.service)
    xwlbridge run -- xdotool getmouselocation    # an X11 tool on it

`--display` before a command acts on `xwlbridge.service` (`run`, `command`,
`doctor` without the RealVNC checks, `status`, `logs`, `start`, `stop`,
`restart`, `enable`, `disable`, `show`). It runs `xwlbridge -nowrap` with the
same `engine`, `capture`, `backend`, `log` and `bridge_args` settings. Both units
can run at the same time (each starts its own bridge).

Without `--display` or `--realvnc`, `doctor`, `status`, `logs`, `start`, `stop`,
`restart` and `show` act on `xwlbridge.service` when it is enabled and
`xwlbridge-realvnc.service` is not (an install without RealVNC Server), and on
`xwlbridge-realvnc.service` otherwise. `enable`, `disable`, `run` and `command`
act on `xwlbridge-realvnc.service` unless `--display` is given.

## Coming from xwlrvnc

Upstream xwlrvnc (`cargo install xwlrvnc`) has no service or settings file to
take over: run `xwlbridge` where you ran `xwlrvnc`. `xwlbridge-ctl migrate`
takes over a per-user install of a pre-release package of this fork (those
unpublished packages used the name xwlrvnc before 0.1.0): it stops and disables `xwlrvnc.service`, copies
`~/.config/xwlrvnc/xwlrvnc.conf` to `~/.config/xwlbridge/xwlbridge.conf` (a
`bridge=` path to the old binary becomes `auto`; an existing new file is kept),
installs and enables `xwlbridge-realvnc.service`, and starts it when the old
service was running inside a graphical session. `--dry-run` shows what it
would do, `--no-start` does not start anything. It can be run again.
`install.sh` runs it once when it finds an xwlrvnc install. xwlrvnc's own
files stay until `~/.local/share/xwlrvnc/uninstall.sh --keep-config`. Tested
with a real systemd user manager (systemd 259) in a container: the old service
running and enabled before, stopped and disabled after, the settings carried,
the new service enabled; again idempotent; then both uninstallers.

## Manage

From GNOME: RealVNC's tray icon in the top bar (needs the AppIndicator
extension, enabled by default on Ubuntu) and the app-grid entry **VNC Server
(Wayland desktop)** open RealVNC's status window and its Options. (RealVNC's
own visible "VNC Server" entry starts its service-mode server, which cannot
share a Wayland desktop.)

    xwlbridge-ctl status | logs [-f] | start | stop | restart | enable | disable
    xwlbridge-ctl show            # RealVNC's status window on the desktop (starts the service if needed)
    xwlbridge-ctl command         # the exact command the service runs

Logs go to the journal (`xwlbridge-ctl logs`, or `journalctl --user -u xwlbridge-realvnc`), RealVNC's included.

## The service

`xwlbridge-realvnc.service` is a systemd *user* unit, wanted by and part of
`graphical-session.target`: it starts after you log in to GNOME and stops when
you log out. It restarts on failure with a back-off (2 s growing to 2 min) and
does not restart on a settings error (exit 78: fix the settings, then
`xwlbridge-ctl restart`). If you stop RealVNC from its own menu, it stays stopped
until `xwlbridge-ctl start` or your next login.

A stop is not a failure: on `xwlbridge-ctl stop` and on logging out xwlbridge
passes the SIGTERM on to RealVNC, stops its X server and exits 0, also while a
viewer is connected. Where your user manager outlives the session (an ssh
login, lingering), `systemctl --user status xwlbridge-realvnc` then says `inactive
(dead)`, and `systemctl --user --failed` does not list it. A unit that is
`failed` after a logout failed for a reason of its own: `xwlbridge-ctl logs`.
So the unit has no `SuccessExitStatus=`: any other non-zero status is a
failure, restarted as above (128 + n: RealVNC was killed by signal n). RealVNC
itself exits 0 on a SIGTERM from anywhere, so a SIGTERM that is not a stop of
the service stops it for good, as its own menu does.

After a reboot nobody can connect until you have logged in at the machine,
unless GNOME logs you in automatically (Settings > System > Users > Automatic
Login).

Hardening: `NoNewPrivileges`, `RestrictSUIDSGID`, `LockPersonality`,
`RestrictNamespaces`, `RestrictRealtime`, `MemoryDenyWriteExecute`,
`SystemCallArchitectures=native`,
`RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK`, `UMask=0077`,
memory/task limits. With these the session bus, PipeWire, Xwayland,
`/tmp/.X11-unix` and RealVNC's licence check (which enumerates network
interfaces over netlink) keep working on both engines. Not used: `PrivateTmp`
(hides `/tmp/.X11-unix` and RealVNC's control socket from the desktop),
`ProtectSystem=strict`/`ProtectHome` (RealVNC writes `~/.vnc` and received
files), `PrivateDevices`/`ProtectClock`/`CapabilityBoundingSet` (a user
manager cannot drop capabilities: the unit fails with status 218),
`SystemCallFilter=@system-service` (TigerVNC's Xvnc calls `mincore` and is
killed with SIGSYS). Under `NoNewPrivileges`
RealVNC's set-uid launcher cannot gain root, so `vncserver-x11-core` is started
directly, as `-uidisplay` does anyway. Consequences of running without
RealVNC's root helper: system authentication works for your own account only,
and adding the viewer's printer needs membership in `lpadmin`. To restore the
root helper (other local accounts, no `lpadmin`), install TigerVNC, set
`engine=xvnc` and `uidisplay=none`, and remove the hardening with a drop-in
(`systemctl --user edit xwlbridge-realvnc`):

    [Service]
    NoNewPrivileges=no
    RestrictSUIDSGID=no
    LockPersonality=no
    RestrictNamespaces=no
    RestrictRealtime=no
    MemoryDenyWriteExecute=no
    SystemCallArchitectures=
    RestrictAddressFamilies=

(Each seccomp-based directive implies `NoNewPrivileges`, so all of them have to
go for the set-uid launcher to work.)

## Uninstall

    ~/.local/share/xwlbridge/uninstall.sh            # everything it installed, settings included
    ~/.local/share/xwlbridge/uninstall.sh --keep-config
    ~/.local/share/xwlbridge/uninstall.sh --purge    # also RealVNC's per-user files created since the install

RealVNC Server itself and the settings it had before are never touched. Past
log lines stay in the journal until it rotates them. `--purge` tells RealVNC's
new files from yours by the listing of `~/.vnc` the first install made
(`~/.local/share/xwlbridge/vnc-before.list`); if that listing is gone it keeps
`~/.vnc` entirely. Settings you add to `~/.vnc` after installing (for example
with `vncpasswd -user`) count as new: leave out `--purge` to keep them.

## Debian package (optional)

    cargo install cargo-deb
    packaging/build-deb.sh            # target/debian/xwlbridge_<version>_amd64.deb
    sudo apt install ./target/debian/xwlbridge_*.deb
    systemctl --user enable --now xwlbridge-realvnc   # per user

The package installs `/usr/bin/xwlbridge`, `/usr/bin/xwlbridge-ctl`, the two user units
in `/usr/lib/systemd/user/`, the app-grid entry and the default settings in
`/usr/share/xwlbridge/` (copied to `~/.config/xwlbridge/` by `xwlbridge-ctl config
set`). It enables nothing by itself. Before removing it, each user who enabled
it runs `systemctl --user disable --now xwlbridge-realvnc xwlbridge` (and may delete
`~/.config/xwlbridge`); dpkg cannot reach per-user files.
