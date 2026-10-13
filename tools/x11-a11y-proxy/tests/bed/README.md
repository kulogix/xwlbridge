# Test bed for the proxy's container tests

The shell tests in `tests/` (`smoke.sh`, `parity.sh`, `settings.sh`, `keyboard.sh`, `perf.sh`, `inferred.sh`) run
inside a container with a headless GNOME Shell session. This repository does not ship that image. The published
results were measured in an Ubuntu 26.04 image with GNOME Shell 50 and the packages below. Any image with the same
packages should work.

- Session: `gnome-shell mutter xwayland pipewire pipewire-bin pipewire-pulse wireplumber dbus dbus-x11
  dconf-gsettings-backend gsettings-desktop-schemas gnome-settings-daemon-common`
- X engine and tools: `tigervnc-standalone-server xvfb x11-utils x11-xserver-utils x11-apps xdotool xinput
  python3-xlib`
- Accessibility: `at-spi2-core python3-pyatspi gir1.2-atspi-2.0 orca speech-dispatcher speech-dispatcher-espeak-ng
  espeak-ng python3-speechd libatk-wrapper-java libatk-wrapper-java-jni xdg-desktop-portal xdg-desktop-portal-gnome
  gnome-themes-extra`
- The proxy: `python3 python3-gi python3-dbus gir1.2-gtk-4.0 tesseract-ocr`
- Probe apps (`tests/apps/`): `python3-tk python3-pyqt5 python3-pyqt6 gir1.2-gtk-3.0 openjdk-25-jdk-headless
  libmotif-dev` (build `apps/motif_probe.c` to `/opt/a11y/motif_probe` with `cc -O2 -o /opt/a11y/motif_probe
  motif_probe.c -lXm -lXt -lX11`). Electron is optional and goes in `/opt/electron`.
- The bridge binary: build it with `cargo build --release --features gnome,xvfb` and pass `BIN=/w/target/release/xwlrvnc`
  (the default), or mount a binary somewhere else and set `BIN`.

The helpers in this directory:

| file | what |
|---|---|
| `shell_session.sh` | starts headless `gnome-shell --wayland --virtual-monitor` with PipeWire and leaves the overview |
| `gnome_input.py` | the person at the GNOME machine: keys and pointer through mutter's RemoteDesktop API |
| `speechd_logger.sh` | sends every Speech Dispatcher utterance to a log file instead of a sound card |
| `xsettings_dump.py` | prints the XSETTINGS and RESOURCE_MANAGER an X client on `$DISPLAY` sees |

Example (the repository mounted at `/w`, results in `out/a11y-proxy/smoke`):

```sh
docker run --rm --network none --memory=8g -u "$(id -u)" -v "$PWD":/w -e APPS="gtk3 qt6" \
  <bed-image> dbus-run-session bash /w/tools/x11-a11y-proxy/tests/smoke.sh
```
