#!/bin/bash
# Runs windows_ext_test.py against a headless gnome-shell (GNOME 49/50) in a scratch session: its own D-Bus
# session bus, HOME and XDG_RUNTIME_DIR. Run it in a container or VM that has gnome-shell, never inside your own
# desktop session (it refuses to run where a gnome-shell already owns org.gnome.Shell):
#
#   docker run --rm --network none -v "$PWD":/src:ro IMAGE \
#     dbus-run-session -- bash /src/contrib/gnome-shell/tests/run_windows_ext.sh
#
# Exit status: 0 when every check passed. The shell's log is printed when something failed (or with -v).
# SPDX-License-Identifier: MIT
set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
EXT=$HERE/../xwlbridge-windows@xwlbridge
UUID=xwlbridge-windows@xwlbridge
VERBOSE=0; [ "${1:-}" = -v ] && VERBOSE=1
command -v gnome-shell >/dev/null || { echo "run_windows_ext.sh: gnome-shell is not installed here" >&2; exit 2; }
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || { echo "run_windows_ext.sh: no session bus: run it under dbus-run-session" >&2; exit 2; }
if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
     --method org.freedesktop.DBus.NameHasOwner org.gnome.Shell 2>/dev/null | grep -q true; then
  echo "run_windows_ext.sh: a gnome-shell already runs on this session bus: refusing (use a scratch session)" >&2; exit 2
fi
export HOME=$(mktemp -d) XDG_RUNTIME_DIR=$(mktemp -d) XDG_CONFIG_HOME= XDG_DATA_HOME=
chmod 700 "$XDG_RUNTIME_DIR"
unset WAYLAND_DISPLAY DISPLAY
GS="" SYSBUS=""
trap 'kill $GS $SYSBUS 2>/dev/null; wait $GS 2>/dev/null; rm -rf "$HOME" "$XDG_RUNTIME_DIR"' EXIT
# gnome-shell needs a system bus to start (logind, time limits); an empty private one will do
cat >"$XDG_RUNTIME_DIR/sysbus.conf" <<EOC
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <listen>unix:path=$XDG_RUNTIME_DIR/sysbus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default"><allow user="*"/><allow own="*"/><allow send_destination="*"/><allow receive_sender="*"/></policy>
</busconfig>
EOC
SYSBUS=$(dbus-daemon --config-file="$XDG_RUNTIME_DIR/sysbus.conf" --fork --nopidfile --print-pid) || { echo "run_windows_ext.sh: cannot start a private system bus" >&2; exit 2; }
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/sysbus
# services the bus starts (dconf above all) must see the scratch HOME too
dbus-update-activation-environment HOME XDG_RUNTIME_DIR DBUS_SYSTEM_BUS_ADDRESS 2>/dev/null
D=$HOME/.local/share/gnome-shell/extensions/$UUID
mkdir -p "$D" && cp "$EXT/metadata.json" "$EXT/extension.js" "$D/"
gsettings set org.gnome.shell enabled-extensions "['$UUID']"
gsettings set org.gnome.shell disable-user-extensions false
LOG=$HOME/gnome-shell.log
gnome-shell --headless --wayland --no-x11 --virtual-monitor 1280x800 --virtual-monitor 1024x768 >"$LOG" 2>&1 &
GS=$!
for _ in $(seq 150); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 0.1; done
up=0
for _ in $(seq 100); do
  gdbus call --session --dest org.xwlbridge.Windows --object-path /org/xwlbridge/Windows \
    --method org.xwlbridge.Windows1.Version >/dev/null 2>&1 && { up=1; break; }
  sleep 0.2
done
if [ $up = 0 ]; then
  echo "FAIL the extension's D-Bus service did not come up"; cat "$LOG"; exit 1
fi
echo "gnome-shell $(gnome-shell --version | grep -oE '[0-9.]+$'): $(gdbus call --session --dest org.gnome.Shell \
  --object-path /org/gnome/Shell --method org.gnome.Shell.Extensions.GetExtensionInfo $UUID 2>/dev/null | grep -oE "'state': <[0-9.]+>")"
WAYLAND_DISPLAY=wayland-0 python3 "$HERE/windows_ext_test.py"; rc=$?
errs=$(grep -E "JS ERROR|JS WARNING|$UUID|xwlbridge" "$LOG" | grep -v 'serving' || true)
if [ -n "$errs" ]; then
  echo "--- extension lines in the gnome-shell log:"; printf '%s\n' "$errs"
  grep -qE "JS ERROR" <<<"$errs" && { echo "FAIL JS errors in the gnome-shell log"; rc=1; }
fi
if [ $rc -ne 0 ] || [ $VERBOSE = 1 ]; then echo "--- gnome-shell log (tail)"; tail -60 "$LOG"; fi
exit $rc
