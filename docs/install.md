# Installing xwlbridge

xwlbridge is an X display that mirrors your Wayland desktop, so X11 screen-capture, remote-desktop and automation tools work on Wayland-only systems (GNOME first). Examples are RealVNC Server, x11vnc, xdotool and screen recorders.

The GNOME backend uses mutter's own D-Bus APIs on your session bus:

- ScreenCast with PipeWire for the picture;
- RemoteDesktop with libei (`ConnectToEIS`) for input;
- the RemoteDesktop clipboard;
- DisplayConfig for the monitor layout.

No xdg-desktop-portal and no permission dialog is involved. That is why xwlbridge runs as your user, inside your GNOME session.

For RealVNC Server (its UI on your desktop, the systemd user unit, ports, authentication), read [realvnc-server-6.md](realvnc-server-6.md) after this page.

The build and the runtime packages below were checked in a clean container of each distribution listed (with an
earlier version of the code). This release was checked on fresh Ubuntu 26.04.1 (the bundle) and AlmaLinux 10.2 (a
source build) virtual machines.

## Install the release bundle (recommended)

Download `xwlbridge-<version>-x86_64-<distro>.tar.gz` and its `.sha256` file. Then, as the desktop user (no sudo):

```sh
sha256sum -c xwlbridge-<version>-x86_64-<distro>.tar.gz.sha256
tar xzf xwlbridge-<version>-x86_64-<distro>.tar.gz
cd xwlbridge-<version>-x86_64-<distro>
./install.sh
```

The bundle's x86_64 binary needs glibc 2.39 or newer. It ran on Ubuntu 26.04 and AlmaLinux 10.2.

install.sh installs:

- `~/.local/bin/xwlbridge` and `~/.local/bin/xwlbridge-ctl`;
- two systemd user units in `~/.config/systemd/user`: `xwlbridge-realvnc.service` (RealVNC Server on the bridge) and `xwlbridge.service` (the bridge display alone, for X11 tools);
- a settings file, `~/.config/xwlbridge/xwlbridge.conf`;
- an app-grid entry.

It then runs the doctor. It enables one unit: `xwlbridge-realvnc.service` when `vncserver-x11` is installed, otherwise `xwlbridge.service`. Use `--service` to choose.

On Ubuntu, `~/.local/bin` is on your PATH only after the next login. Until then, run:

```sh
~/.local/bin/xwlbridge-ctl doctor
```

install.sh options:

| option | meaning |
|---|---|
| `--bin PATH` | binary to install |
| `--prefix DIR` | install prefix |
| `--service realvnc\|display` | which unit to enable |
| `--port N\|realvnc` | VNC port, or RealVNC's own setting |
| `--engine auto\|builtin\|xvnc\|xvfb\|xwayland` | engine to use |
| `--no-enable` | do not enable the unit |
| `--no-start` | do not start the unit |

To remove the bundle install:

```sh
~/.local/share/xwlbridge/uninstall.sh
```

Options: `--keep-config` keeps your settings. `--purge` also removes RealVNC files created since the install. Your `~/.vnc` files from before the install are never touched.

### Coming from xwlrvnc

install.sh runs `xwlbridge-ctl migrate` once when it finds an xwlrvnc install. The old files stay until you run `~/.local/share/xwlrvnc/uninstall.sh --keep-config`.

## Requirements

- GNOME on Wayland with mutter 46 or newer. See the table below.
- PipeWire. Every GNOME desktop has it.
- Xwayland, so RealVNC's own windows can appear on GNOME (`-uidisplay`). GNOME installs it.
- `xkbcomp` for the stock engines (xvfb, xvnc, xwayland).
- The builtin engine needs no extra packages.
- To build: Rust (the locked dependencies need 1.89 or newer), a C toolchain, pkg-config, libclang and the PipeWire development headers.

## GNOME versions

| mutter / GNOME | where checked | status |
|---|---|---|
| 51 (Arch 51.0) | container smoke 9/9 on builtin, xvnc, xvfb | works |
| 50 (Ubuntu 26.04: 50.1; openSUSE Tumbleweed: 50.5) | real desktops, VMs, containers | primary platform |
| 49 (Fedora 43: 49.8; AlmaLinux 10.2: 49.4) | real sessions in VMs (Fedora 43; AlmaLinux 10.2 acceptance of this release), container smoke | works |
| 48 (Debian 13: 48.7) | real session in a VM, multi-monitor and scaling, container smoke 9/9 | works |
| 47 (Fedora 41) | real session in a VM | works |
| 46 (Ubuntu 24.04: 46.2) | real session in a VM, container smoke 9/9 | works; the cursor is drawn into the picture (cursor metadata can crash mutter 46) |
| 45 and older (RHEL 9: mutter 40) | package inspection | not supported: no ConnectToEIS, picture without input; use GNOME on Xorg there |
| labwc 0.9 (Raspberry Pi OS, Debian 13, aarch64, Raspberry Pi 5) | native build and headless labwc tests | wlr backend + builtin works (input, clipboard, picture exact); `-engine xwayland` 18/18 in a labwc container |
| sway and other wlroots compositors | upstream backend | not re-tested in this fork beyond labwc |
| KDE Plasma | -- | not supported (no backend) |

Where GNOME on Xorg still exists (Ubuntu 24.04, Debian 13, RHEL 9), X11 tools work natively and do not need this.

`-recordmonitor` is only safe on mutter 51 or newer (mutter#2718). The default, RecordArea, is safe everywhere.

## Distributions at a glance

| distribution | GNOME (mutter) | engines | Rust in the distro |
|---|---|---|---|
| Ubuntu 24.04 | 46.2 | builtin, xvnc (TigerVNC 1.13.1), xvfb, xwayland | 1.75 (too old) |
| Ubuntu 26.04 | 50.1 | builtin, xvnc, xvfb, xwayland | 1.93 |
| Debian 13 | 48.7 | builtin, xvnc (1.15.0), xvfb, xwayland | 1.85 (too old) |
| Fedora 43 | 49.8 | builtin, xvnc (1.16.2), xvfb, xwayland | 1.98 |
| RHEL 10 and rebuilds (tested: AlmaLinux 10.2) | 49.4 | builtin, xwayland | 1.92 |
| openSUSE Tumbleweed | 50.5 | builtin, xvnc (1.16.1), xvfb, xwayland | 1.98 |
| Arch | 51.0 | builtin, xvnc (1.16.2), xvfb, xwayland | 1.99 |

RHEL 10 packages no Xvnc and no Xvfb. Its complete engine is `xwayland`.

## Build from source

### Build dependencies

Ubuntu 24.04, Ubuntu 26.04, Debian 13:

```sh
sudo apt install build-essential pkg-config libclang-dev libpipewire-0.3-dev libspa-0.2-dev curl ca-certificates git
```

Fedora 43:

```sh
sudo dnf install gcc clang-devel pkgconf-pkg-config pipewire-devel curl git
```

RHEL, AlmaLinux, Rocky 10:

```sh
sudo dnf install dnf-plugins-core epel-release
sudo dnf config-manager --set-enabled crb
sudo dnf install gcc clang-devel pkgconf-pkg-config pipewire-devel curl git
```

On RHEL itself, run `subscription-manager repos --enable codeready-builder-for-rhel-10-$(arch)-rpms` and install EPEL from its release RPM. Only AlmaLinux 10.2 was tested.

openSUSE Tumbleweed:

```sh
sudo zypper install gcc clang-devel pkgconf-pkg-config pipewire-devel curl git
```

Arch:

```sh
sudo pacman -S --needed base-devel clang pipewire curl git
```

Raspberry Pi OS (Debian 13, aarch64, labwc): use Debian's list above and a user-local rustup.

### Rust

Install rustup as your user, with no toolchain yet. `rust-toolchain.toml` (1.96.0) decides which one you get.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
```

### Build, test, install

```sh
git clone https://github.com/kulogix/xwlbridge && cd xwlbridge
rustup toolchain install
cargo build --release --features gnome,xvfb    # about a minute
cargo test --release --features gnome,xvfb     # 351 tests
packaging/install.sh
```

`packaging/install.sh` uses `target/dist` or `target/release`. To copy only the binary:

```sh
install -Dm755 target/release/xwlbridge ~/.local/bin/xwlbridge
```

`cargo build --profile dist --features gnome,xvfb` makes the stripped binary the bundle uses.

- `gnome` is the GNOME backend. `xvfb` adds the stock engines (`xvnc`, `xvfb`, `xwayland`).
- The default build, without features, is pure Rust and has only the wlr backend.

On Raspberry Pi OS with labwc, GNOME is not used. Build with the wlr backend:

```sh
cargo build --release --features xvfb
```

The engines there are `builtin` and `xwayland`. The build took about 140 s on a Pi 5, and 230 of 230 tests passed there.
`packaging/install.sh` and the services need a binary with the GNOME backend: on labwc, run `xwlbridge` directly
(for example `xwlbridge -backend wlr -engine xwayland -display 20 -nowrap`).

## Runtime packages

Ubuntu, Debian:

```sh
sudo apt install xwayland x11-xkb-utils
sudo apt install tigervnc-standalone-server xvfb   # optional engines
```

Fedora 43:

```sh
sudo dnf install pipewire-libs xorg-x11-server-Xwayland xkbcomp
sudo dnf install tigervnc-server-minimal xorg-x11-server-Xvfb   # optional
```

RHEL, AlmaLinux, Rocky 10 (engines: builtin, xwayland):

```sh
sudo dnf install pipewire-libs xorg-x11-server-Xwayland xkbcomp
```

openSUSE Tumbleweed:

```sh
sudo zypper install libpipewire-0_3-0 xwayland xkbcomp
sudo zypper install xorg-x11-Xvnc xorg-x11-server-Xvfb   # optional
```

On minimal openSUSE images, install `gawk` first.

Arch:

```sh
sudo pacman -S --needed libpipewire xorg-xwayland xorg-xkbcomp
sudo pacman -S --needed tigervnc xorg-server-xvfb   # optional
```

RealVNC's tray icon needs GTK 2 at run time: `libgtk2.0-0t64` (Ubuntu/Debian), `gtk2` (Fedora; EPEL on EL10), `libgtk-2_0-0` (openSUSE). On Arch it is only in the AUR. Without GTK 2 there is no icon, but the status dialog still opens.

## X11 tools on the bridge

| distribution | tools |
|---|---|
| Ubuntu 26.04 | x11vnc, xdotool, ffmpeg, xclip, wl-clipboard, x11-apps (all worked) |
| AlmaLinux 10 (+ EPEL + CRB) | xclip, wl-clipboard, xdpyinfo (EPEL), xprop (AppStream), ffmpeg-free (EPEL, with x11grab, without x264) |

On AlmaLinux 10 there is no x11vnc, no TigerVNC server (Xvnc, x0vncserver), no xdotool, no Xvfb, and no xwininfo or xcalc. On EL10:

- record with ffmpeg-free (`-f x11grab`);
- serve VNC with RealVNC Server;
- build xdotool from its upstream release tarball, or x11vnc from Fedora's source RPM, yourself.

With the service running, run a tool against its display:

```sh
xwlbridge run -- x11vnc ...
xwlbridge run -- xdotool ...
```

`xwlbridge run` finds the running bridge's display and sets the environment for it.

## Firewall

RHEL and AlmaLinux 10: firewalld (zone public) drops the VNC port.

```sh
sudo firewall-cmd --permanent --add-service=vnc-server
sudo firewall-cmd --reload
```

The `vnc-server` service is 5900-5903/tcp. For another port, use `--add-port=5911/tcp`.

Ubuntu has no firewall on by default. With ufw: `sudo ufw allow 5900/tcp`.

Fedora Workstation's zone already allows 1025-65535 tcp/udp.

## Where the engine binaries live

TigerVNC is `/usr/bin/Xtigervnc` on Ubuntu and Debian (`/usr/bin/Xvnc` through alternatives). It is `/usr/bin/Xvnc` on Fedora, openSUSE and Arch.

With RealVNC Server installed, `/usr/bin/Xvnc` is RealVNC's set-uid launcher on Debian and Ubuntu, whichever package is installed first. On RPM distributions, RealVNC's package conflicts with TigerVNC's server. xwlbridge refuses set-uid servers. Use `-xserver PATH` to pick a binary explicitly.

## First run

From a terminal in your GNOME session (`~/.local/bin/xwlbridge` until `~/.local/bin` is on your PATH):

```sh
xwlbridge -backend gnome -engine builtin xdpyinfo | grep -E 'name of display|dimensions'
```

GNOME shows its screen-sharing indicator in the top bar while the desktop is mirrored. It goes away about ten seconds
after the last activity (`-warm`), or 30 s after a client that read the screen went away (`-linger`).

Next: [realvnc-server-6.md](realvnc-server-6.md) for RealVNC Server.

## Uninstall

Bundle or `install.sh` install:

```sh
~/.local/share/xwlbridge/uninstall.sh
```

Add `--keep-config` or `--purge` as described above.

Manual install:

```sh
systemctl --user disable --now xwlbridge-realvnc.service xwlbridge.service
rm -f ~/.local/bin/xwlbridge ~/.config/systemd/user/xwlbridge-realvnc.service ~/.config/systemd/user/xwlbridge.service
systemctl --user daemon-reload
```
