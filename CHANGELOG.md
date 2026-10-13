# Changelog

All notable changes to xwlbridge are listed here. The format follows Keep a Changelog.

## Unreleased

### Fixed

- TeamViewer's capture process (`TeamViewer_Desktop`) is a default desktop driver: its keys reach the desktop without `-xtestdrivers` naming it (the process is matched by its executable and `argv[0]`; its kernel `comm` is truncated to `TeamViewer_Desk`). Before, its keys stayed on the bridge display unless `-xtestdrivers` named it.

## 0.1.0 (2026-10-13)

First release under the name xwlbridge. It is based on xwlrvnc 0.0.8 by Patrick Gaskin.

### Added

- GNOME backend (`-backend gnome`). It uses mutter's D-Bus APIs: ScreenCast, RemoteDesktop (input through libei / EIS), the RemoteDesktop clipboard and DisplayConfig. It needs the `gnome` build feature.
- Stock X servers as engines (`-engine xvnc`, `xvfb`, `xwayland`) with a desktop agent. They need the `xvfb` build feature and `xkbcomp`.
- On-demand remote-desktop session. It runs while an X client watches, reads the screen or injects input, and stops after a while. Flags: `-warm`, `-linger`, `-alwayson`, `-coldconnect`, `-coldwait`.
- Input attribution for XTest: `-xtesthelpers`, `-xtestdrivers`, `-xtestgrace`, `-xtestpersist`.
- `-uidisplay display|auto|none`: runs RealVNC Server's own UI on another X display.
- `xwlbridge run [-display N] [--] command`, and `xwlbridge run -print`.
- `-showwindows` and `-showscreen` (experimental, stock engines).
- `-a11ybus` and `-reverseinput` (stock engines), and `tools/x11-a11y-proxy`.
- Builtin engine: read-only XKEYBOARD (`-noxkb` hides it), RandR monitors and modes, MIT-SHM ShmCreateSegment, MIT-SCREEN-SAVER requests, and the bell.
- Packaging:
  - two systemd user services, `xwlbridge-realvnc.service` and `xwlbridge.service`;
  - `xwlbridge-ctl`, including `doctor` and `migrate`; without `--display` or `--realvnc`, its `doctor`, `status`, `logs`, `start`, `stop`, `restart` and `show` follow the enabled service;
  - `install.sh` and `uninstall.sh`;
  - a release bundle and an optional .deb (`packaging/build-deb.sh`).
- contrib: a GNOME lock-screen extension and legacy remote-access helpers.
- CI on GitHub Actions: format check, clippy, tests, release build, `test_ctl.sh` and the proxy unit tests.

### Changed

- Upstream xwlrvnc's binary `xwlrvnc` is now `xwlbridge`, with upstream's flags; its X vendor string `xwlrvnc` is now `xwlbridge`, and its selection atoms `XWLRVNC_FETCH_*` are now `XWLBRIDGE_FETCH_*`.
- This fork's unpublished pre-release packages also used the name xwlrvnc. Their names changed:
  - `xwlrvnc-ctl` to `xwlbridge-ctl`;
  - `xwlrvnc.service` to `xwlbridge-realvnc.service`;
  - `~/.config/xwlrvnc` to `~/.config/xwlbridge`;
  - `XWLRVNC_*` environment variables to `XWLBRIDGE_*`;
  - lock-screen extension UUID `xwlrvnc-lockscreen@xwlrvnc` to `xwlbridge-lockscreen@xwlbridge`. Re-enable the extension under the new UUID.
- `xwlbridge-ctl migrate` moves such a pre-release install to the new names. `install.sh` runs it once when it finds one.

### Known issues

- See [docs/limitations.md](docs/limitations.md).
- `rfb_port` and `server_args` go on RealVNC's command line. RealVNC forgets them when it reloads its settings. Keep RealVNC parameters in `~/.vnc/config.d/vncserver-x11` and use `rfb_port=realvnc`.
- Without `-xtestdrivers default,TeamViewer_Desktop`, keys typed by TeamViewer 15.20.6 stay on the bridge's display while TeamViewer's own panel there has the X focus. The fix is on a development branch and is not in 0.1.0.

## 0.0.8 and earlier

Upstream xwlrvnc releases by Patrick Gaskin, see https://github.com/pgaskin/xwlrvnc and the git history before the tag `upstream-base`.
