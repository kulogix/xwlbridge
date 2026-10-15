# xwlbridge — X11 ⇄ Wayland desktop bridge

**xwlbridge is an X display that mirrors your Wayland desktop, so X11 screen-capture, remote-desktop and automation tools work on Wayland-only systems (GNOME first).**
Screen recorders, VNC servers, remote-support tools and automation scripts written for X11 capture and control the real GNOME desktop through it. It needs no portal prompt and no X11 (Xorg) login session.

```
X11 tool (x11vnc, RealVNC, OBS XSHM, ffmpeg x11grab, xdotool, PyAutoGUI, Java Robot, ...)
        │  X11: GetImage/XShm, XTest, XFixes cursor, RandR, clipboard
        ▼
  bridge X display  :20     ← engine: builtin | xvnc | xvfb | xwayland
        │
     xwlbridge  ── picture:   mutter ScreenCast  → PipeWire frames → root window
                ── input:     XTest              → libei / EIS (RemoteDesktop) → GNOME
                ── monitors:  DisplayConfig      → RandR outputs/monitors named like GNOME's
                ── clipboard: CLIPBOARD/PRIMARY ⇄ the desktop clipboard (Xwayland relay, or RemoteDesktop)
        ▼
  GNOME Shell / mutter (Wayland session)
```

xwlbridge is based on [xwlrvnc](https://github.com/pgaskin/xwlrvnc) by Patrick Gaskin (MIT). Upstream built the first X server of this kind, a fake X server backed by a Wayland display, so that RealVNC Server works on wlroots compositors. xwlbridge adds a GNOME (mutter) backend, stock X servers as engines with a desktop agent, packaging, an accessibility proxy, and the work to make it a general-purpose bridge.

Version 0.1.0 is the first release under this name. Licence: MIT.

## Contents
- [The problem](#the-problem) · [How it works](#how-it-works) · [Supported desktops](#supported-desktops) · [Install](#install) · [Quick start](#quick-start) · [On-demand session](#on-demand-session)
- [Use cases](#use-cases) · [Coming from xwlrvnc](#coming-from-xwlrvnc) · [Security](#security) · [Limitations](#limitations) · [Troubleshooting](#troubleshooting)
- [Compared with](#compared-with-portals-xwaylandvideobridge-gnome-remote-desktop) · [Documentation](#documentation) · [Credits and licence](#credits-and-licence)

## The problem

GNOME has no X11 session any more on Ubuntu 25.10 and 26.04, Fedora 43, RHEL 10 and its rebuilds (no Xorg server at all, only Xwayland), openSUSE Tumbleweed and Arch (GNOME 50/51). X11 applications still work there through Xwayland. X11 desktop tools do not: Xwayland's root window holds only X windows, and its input reaches only X windows.

Measured on GNOME 50's own Xwayland and through the bridge:

| what an X11 tool does | on Xwayland under GNOME | through xwlbridge |
|---|---|---|
| capture the root (xwd, scrot, ffmpeg x11grab, OBS XSHM, Java Robot) | BadMatch or all black | the GNOME desktop, pixel-exact |
| inject input (XTest: xdotool, PyAutoGUI, VNC servers) | stays inside Xwayland (0 of 2 clicks reached a Wayland window) | reaches every window |
| cursor shape (XFixes) | Xwayland's default cursor | GNOME's cursor sprite |
| monitors (RandR) | one emulated output | GNOME's monitors, with GNOME's names |

## How it works

xwlbridge runs an X server (the engine) whose root window shows the desktop. A backend talks to the compositor: picture in, input and clipboard out.

| backend (`-backend auto\|wlr\|gnome`) | what it uses |
|---|---|
| `gnome` | mutter's D-Bus APIs on the session bus: ScreenCast (PipeWire), RemoteDesktop (input through libei / EIS, clipboard), DisplayConfig (monitor layout, dynamic resolution, DPMS wake). No xdg-desktop-portal and no permission dialog. It must run as the user inside the GNOME session. GNOME shows its screen-sharing indicator while the desktop is mirrored. Needs the `gnome` build feature. |
| `wlr` | upstream's Wayland-protocol backend (wlr-screencopy / ext-image-copy-capture, virtual pointer and keyboard, data-control) for sway, labwc and other wlroots compositors. The default build has only this backend. |

`auto` picks gnome for mutter and wlr otherwise.

| engine (`-engine builtin\|xvfb\|xvnc\|xwayland`) | what it is |
|---|---|
| `builtin` | xwlbridge's own minimal X server (from upstream). No extra packages. A subset of X11: read-only XKEYBOARD from the compositor's keymap (`-noxkb` hides it), RandR monitors/CRTCs/modes, MIT-SHM, MIT-SCREEN-SAVER requests, the bell. No XInput2, no RECORD, no Composite. |
| `xvnc` | TigerVNC's Xvnc, its VNC listener confined to a private Unix socket. Complete X11. Resizes with the desktop. RealVNC's `/usr/bin/Xvnc` (set-uid launcher) and any set-uid server are refused; `-xserver PATH` picks the binary. |
| `xvfb` | Xorg's Xvfb. Complete X11. Fixed size. |
| `xwayland` | A rootful Xwayland on a private headless Wayland compositor inside xwlbridge. It never touches your session. Complete X11. Resizes. The complete engine on RHEL/AlmaLinux 10. A few percent of frames can be torn during fast motion. |

The stock engines (xvfb, xvnc, xwayland) need the `xvfb` build feature and `xkbcomp`. An agent, an ordinary X client of the engine, puts the captured desktop on the root window, mirrors cursor, keymap and screen layout, forwards the input its clients inject, and bridges the clipboard.

Only XTest meant for the desktop is forwarded. `-xtesthelpers` and `-xtestdrivers` tell xwlbridge which programs' XTest drives only bridge windows, or always reaches the desktop. This is an accident filter, not a security boundary.

## Supported desktops

| mutter / GNOME | where checked | status |
|---|---|---|
| 51 (Arch 51.0) | container smoke 9/9 on builtin, xvnc, xvfb | works |
| 50 (Ubuntu 26.04: 50.1; openSUSE Tumbleweed: 50.5) | real desktops, VMs, containers | primary platform |
| 49 (Fedora 43: 49.8; AlmaLinux 10.2: 49.4) | real sessions in VMs, container smoke | works |
| 48 (Debian 13: 48.7) | real session in a VM, multi-monitor and scaling, container smoke 9/9 | works |
| 47 (Fedora 41) | real session in a VM | works |
| 46 (Ubuntu 24.04: 46.2) | real session in a VM, container smoke 9/9 | works; the cursor is drawn into the picture (cursor metadata can crash mutter 46) |
| 45 and older (RHEL 9: mutter 40) | package inspection | not supported: no ConnectToEIS, picture without input; use GNOME on Xorg there |
| labwc 0.9 (Raspberry Pi OS, Debian 13, aarch64, Pi 5) | native build, headless labwc tests | wlr backend + builtin works; `-engine xwayland` 17-18 of 18 checks per run in a labwc container |
| sway and other wlroots compositors | upstream backend | not re-tested in this fork beyond labwc |
| KDE Plasma | – | not supported (no backend) |

Where GNOME on Xorg still exists (Ubuntu 24.04, Debian 13, RHEL 9), X11 tools work natively and do not need this.

## Install

Recommended: the release bundle `xwlbridge-<version>-x86_64-<distro>.tar.gz` and its `.sha256` from the [Releases page](https://github.com/kulogix/xwlbridge/releases). Run as the desktop user, no sudo.

```sh
sha256sum -c xwlbridge-*.tar.gz.sha256
tar xzf xwlbridge-*.tar.gz && cd xwlbridge-*/
./install.sh
~/.local/bin/xwlbridge-ctl doctor
```

`install.sh` installs `~/.local/bin/xwlbridge` and `xwlbridge-ctl`, both systemd user units, the settings file `~/.config/xwlbridge/xwlbridge.conf` and an app-grid entry, then runs the doctor. The doctor checks the service that `install.sh` enabled (`--display` or `--realvnc` names one). On Ubuntu, `~/.local/bin` is on PATH only after the next login. The bundle's x86_64 binary needs glibc >= 2.39. Uninstall: `~/.local/share/xwlbridge/uninstall.sh`.

Build from source (Ubuntu 24.04/26.04, Debian 13 shown; Rust is pinned by `rust-toolchain.toml`):

```sh
sudo apt install build-essential pkg-config libclang-dev libpipewire-0.3-dev libspa-0.2-dev curl ca-certificates git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
git clone https://github.com/kulogix/xwlbridge && cd xwlbridge
rustup toolchain install
cargo build --profile dist --features gnome,xvfb   # stripped; --release keeps debug info (about 190 MB)
sudo apt install xwayland x11-xkb-utils           # runtime: Xwayland, xkbcomp for the stock engines
packaging/install.sh                              # the same per-user install as the bundle's
```

Optional engines on Ubuntu/Debian: `tigervnc-standalone-server xvfb`. Other distributions, runtime packages, firewall notes: [docs/install.md](docs/install.md).

First run, from a terminal in your GNOME session (`~/.local/bin/xwlbridge` until `~/.local/bin` is on your PATH):

```sh
xwlbridge -backend gnome -engine builtin xdpyinfo | grep -E 'name of display|dimensions'
```

## Quick start

`install.sh` has already started a bridge display: `xwlbridge.service`, or `xwlbridge-realvnc.service` when RealVNC Server is installed. Run X11 tools on it:

```sh
xwlbridge run -- xdotool getmouselocation
xwlbridge run -- sh -c 'echo "$DISPLAY"'     # the bridge's display, for tools that take it as an argument
```

`xwlbridge run` finds the running bridge, sets the engine's cookie, hides Wayland and pins toolkits (GTK, Qt, SDL, Electron, Firefox, Clutter, winit) to X11. `xwlbridge run -print` prints that environment.

A bridge of your own, without the services: when more than one bridge runs, `xwlbridge run` needs `-display N`.

```sh
xwlbridge -backend gnome -engine xvnc -display 20 -nowrap &
xwlbridge run -display 20 -- xdotool getmouselocation
```

`-engine xvnc` needs TigerVNC's Xvnc (`sudo apt install tigervnc-standalone-server`; Fedora: `tigervnc-server-minimal`). `-engine builtin` needs no extra package and `-engine xwayland` only Xwayland.

Packaged services (two systemd user units):

- `xwlbridge.service`: the bridge display alone, for X11 tools.
- `xwlbridge-realvnc.service`: RealVNC Server on the bridge.

```sh
xwlbridge-ctl --display show     # prints the display of xwlbridge.service
```

`install.sh` picks `xwlbridge-realvnc.service` when `vncserver-x11` is installed, else `xwlbridge.service`.

## On-demand session

The GNOME remote-desktop session runs on demand. It runs while an X client watches (DAMAGE), reads the screen or injects input, and stops after a while.

| flag | meaning |
|---|---|
| `-warm secs\|always` | keep the session this long after the last activity (default 10 s) |
| `-linger secs` | keep it this long after an X client that read the screen (a screenshot tool) went away (default 30 s) |
| `-alwayson` | same as `-warm always` |
| `-coldconnect none\|start` | stock engines: a client that connects while the session is stopped starts it at once (default `start`) |
| `-coldwait ms` | builtin: a screen read may wait this long while the session starts (default 2000) |

One-shot caveat on the stock engines: a fast one-shot screen read right after connecting, while the session is stopped, can be black. xwd, import, scrot, maim and PyAutoGUI read within about 30 ms of connecting. In the release test 14 to 17 of 32 one-shot captures were correct on xvnc and xvfb (the others black); Java Robot, Wine and ffmpeg got the current desktop. The builtin engine waits for the session (32 of 32 correct). Remedy: run with `-alwayson` on hosts that need instant one-shot captures (GNOME's sharing indicator then stays up), or take a throw-away first shot.

## Use cases

### RealVNC Server

Tested: RealVNC Server 6.11 (licensed) in user mode on the bridge with the real RealVNC Viewer 6.22: picture, input, clipboard both ways, audio, printing, file transfer both ways and 30 fps video (containers, mutter 50.1); multi-monitor and scaling on mutter 46/48/50; Ubuntu 26.04 VMs; daily use on an Ubuntu 26.04 desktop. RealVNC Server 7.17 unlicensed: UI and startup only. You need your own RealVNC licence; this project contains none.

`-uidisplay auto` puts RealVNC's tray icon, status window, Options and prompts on the GNOME desktop. Configure RealVNC in `~/.vnc/config.d/vncserver-x11` (mode 0600), not on the command line, which RealVNC forgets on reload.

```sh
systemctl --user enable --now xwlbridge-realvnc.service
```

Details: [docs/realvnc-server-6.md](docs/realvnc-server-6.md).

### x11vnc

```sh
x11vnc -storepasswd ~/.vnc/passwd                 # once
xwlbridge run -- x11vnc -nomodtweak -xrandr -localhost -rfbauth ~/.vnc/passwd -forever -shared
```

Tested on an Ubuntu 26.04 VM: x11vnc picture and keys on the service's display. Also tested next to RealVNC Server on one display (containers).

### TigerVNC x0vncserver

```sh
xwlbridge run -- x0vncserver -localhost -PasswordFile ~/.vnc/tigervnc.passwd
```

Tested with the xvnc engine (containers). Create the password file with TigerVNC's `vncpasswd` (`tigervncpasswd` on
Debian/Ubuntu; with RealVNC installed, plain `vncpasswd` is RealVNC's). TigerVNC's server is not packaged on
RHEL/AlmaLinux 10.

### OBS Studio

Prefer OBS's own Screen Capture (PipeWire) source when it is enough. For OBS's X11 capture, run OBS on the desktop and
point only its capture at the bridge display: `QT_QPA_PLATFORM=xcb obs`, a Screen Capture (XSHM) source, Advanced
Settings, X Server = the bridge's display (`xwlbridge run -- sh -c 'echo "$DISPLAY"'` prints it, for example `:2`). Tested with the xvnc engine (OBS 32.1, containers); on GNOME's Xwayland the same source
records black. Details: [docs/no-split.md](docs/no-split.md).

### ffmpeg and GStreamer

```sh
xwlbridge run -- sh -c 'ffmpeg -f x11grab -framerate 30 -i "$DISPLAY" desktop.mkv'   # ffmpeg takes the display as an argument
xwlbridge run -- gst-launch-1.0 ximagesrc ! videoconvert ! x264enc tune=zerolatency ! matroskamux ! filesink location=desktop.mkv
```

ffmpeg x11grab (30 frames) was tested on Ubuntu 26.04 and with EPEL's ffmpeg-free on AlmaLinux 10.2.

### Automation: xdotool, PyAutoGUI, Java Robot

```sh
xwlbridge run -- xdotool mousemove 640 360 click 1 type 'hello'
xwlbridge run -- python3 my_pyautogui_script.py
xwlbridge run -- java -Dawt.robot.screenshotMethod=x11 -jar my-robot-tests.jar
```

xdotool typing and a click were tested on Ubuntu 26.04. Java's `Robot` uses the RemoteDesktop/ScreenCast portal on
Wayland unless `-Dawt.robot.screenshotMethod=x11` keeps it on X11. Characters outside the current keyboard layout are
not typed.

### Screenshot tools

xwd, scrot, import and maim work on the bridge. On the stock engines see the one-shot caveat above.

### X11-only remote-access apps

TeamViewer 15.82 and newer works on GNOME 50 by itself. TeamViewer 15.20.6 (2021) worked through the bridge on Ubuntu 26.04 and AlmaLinux 10.2, with helpers from `contrib/legacy-remote-access/` (in a clone of this repository; the release bundle does not contain them) and, with 0.1.0, `-xtestdrivers default,TeamViewer_Desktop`. From 0.1.1 TeamViewer's capture process is a default desktop driver, so that flag is not needed. Old builds have known security holes. See [docs/legacy-remote-access-apps.md](docs/legacy-remote-access-apps.md).

### Keeping an X app entirely on the bridge

`-showwindows` (stock engines, experimental) shows X windows mapped on the engine display as native desktop windows. See [docs/no-split.md](docs/no-split.md).

### Accessibility

`tools/x11-a11y-proxy` shows windows of an app running on the bridge as GNOME windows with accessible objects, for Orca. It is in a clone of this repository, not in the release bundle, and needs `python3-gi gir1.2-gtk-4.0 gir1.2-atspi-2.0 python3-xlib dbus` (Ubuntu/Debian). See [docs/accessibility.md](docs/accessibility.md).

### Input Leap / Barrier, conferencing apps

Input Leap/Barrier server: does not work, because X clients never see the local user's input. Input Leap/Barrier client: not tested. Conferencing apps in X11 mode: not tested.

## Coming from xwlrvnc

From upstream xwlrvnc (`cargo install xwlrvnc`, run as `xwlrvnc vncserver-x11` on a wlroots compositor): replace `xwlrvnc` with `xwlbridge` in your command, for example `xwlbridge vncserver-x11`. Upstream's flags still work, and `-backend auto` picks the wlroots backend there. `cargo uninstall xwlrvnc` removes the old binary. On GNOME, use the packaged services instead (see [Install](#install)). The X vendor string is now `xwlbridge`. Upstream has no control script, unit or settings directory, so there is nothing to migrate.

From a pre-release package of this fork: before 0.1.0 those unpublished packages also used the name xwlrvnc. Their names changed: `xwlrvnc-ctl` → `xwlbridge-ctl`, `xwlrvnc.service` → `xwlbridge-realvnc.service`, `~/.config/xwlrvnc` → `~/.config/xwlbridge`, `XWLRVNC_*` → `XWLBRIDGE_*`. The lock-screen extension UUID is now `xwlbridge-lockscreen@xwlbridge`; re-enable it. Move such an install with:

```sh
xwlbridge-ctl migrate --dry-run
xwlbridge-ctl migrate
```

`install.sh` runs it once when it finds such an install.

## Security

- The bridge display is your desktop. Any client that can connect sees the whole screen, types and clicks anywhere and reads and writes the clipboard, with no portal prompt. Treat access like access to your logged-in session.
- The builtin engine admits only your user and root. Stock engines get a fresh MIT-MAGIC-COOKIE plus localuser entries for your uid and root. Never run `xhost +` on it.
- VNC servers on top are reachable from the network unless you restrict them: use `-localhost` and SSH tunnels, or a password and encryption.
- xwlbridge needs no root and installs nothing system-wide.

Report vulnerabilities privately through GitHub's private vulnerability reporting. See [SECURITY.md](SECURITY.md).

## Limitations

- X clients never see the local user's input (no RECORD, XInput2 raw events, passive key grabs, idle time): global hotkeys, key display tools and an Input Leap/Barrier server do not work.
- Stock engines do not follow GNOME's pointer position.
- Characters outside the current keyboard layout are not typed.
- Stock engines: X bells are lost (the builtin engine rings the desktop bell).
- GNOME's windows are not X windows: wmctrl and `xdotool search` find nothing.
- Stock engines have one RandR output for the whole desktop.
- Runs inside a logged-in GNOME session only, not at the login screen. While the screen is locked, viewers see a padlock.
- A restart of the bridge ends every X client on its display.
- `-showwindows` and `-showscreen` are experimental.
- GNOME 45 and older: no input. KDE: no backend.
- Not tested: aarch64 GNOME.

Full list: [docs/limitations.md](docs/limitations.md).

## Troubleshooting

Start with `xwlbridge-ctl doctor`, `xwlbridge-ctl logs -f`, or run with `-verbose`. Black one-shot screenshots on the stock engines: see [On-demand session](#on-demand-session). More: [docs/troubleshooting.md](docs/troubleshooting.md).

## Compared with portals, xwaylandvideobridge, gnome-remote-desktop

| | xwlbridge | xdg-desktop-portal ScreenCast / RemoteDesktop | Xwayland `-enable-ei-portal` | XWayland Video Bridge | gnome-remote-desktop |
|---|---|---|---|---|---|
| who uses it | unmodified X11 tools | apps written for the portal API | X11 apps injecting XTest | X11 apps that share a window or screen | RDP/VNC clients |
| screen capture | whole desktop as an X root | yes, per consent | no | one chosen window or screen | yes |
| input injection | yes, all windows | yes, per consent | yes, via the portal | no | yes |
| user prompt | none (GNOME's indicator shows) | every session | yes | every share | set in Settings |
| X11 protocol needed | yes | no | yes | yes | no |
| scope | GNOME (mutter >= 46) and wlroots | every desktop with a portal backend | compositors with a RemoteDesktop portal | KDE, other desktops with portals | GNOME |

## Documentation

- [docs/install.md](docs/install.md)
- [docs/realvnc-server-6.md](docs/realvnc-server-6.md)
- [docs/no-split.md](docs/no-split.md)
- [docs/accessibility.md](docs/accessibility.md)
- [docs/legacy-remote-access-apps.md](docs/legacy-remote-access-apps.md)
- [docs/architecture.md](docs/architecture.md)
- [docs/limitations.md](docs/limitations.md)
- [docs/troubleshooting.md](docs/troubleshooting.md)
- [packaging/README.md](packaging/README.md)
- [CHANGELOG.md](CHANGELOG.md)
- [CONTRIBUTING.md](CONTRIBUTING.md)

## Credits and licence

MIT. Based on [xwlrvnc](https://github.com/pgaskin/xwlrvnc), Copyright (c) 2026 Patrick Gaskin. His notice is kept in [LICENSE](LICENSE). The git history keeps all upstream commits with their authorship. The tag `upstream-base` marks upstream v0.0.8; our work is `git log upstream-base..`.

RealVNC® and VNC® are trademarks of RealVNC Ltd. GNOME is a trademark of the GNOME Foundation. X Window System is a trademark of The Open Group. xwlbridge is not affiliated with or endorsed by any of them.
