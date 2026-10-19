#!/bin/bash
# install.sh -> login -> doctor -> uninstall.sh with the window extension, against a real headless gnome-shell
# (GNOME 49/50) in a scratch session (own D-Bus session bus, HOME and XDG_RUNTIME_DIR). Run it in a container
# or VM with gnome-shell, never inside your own desktop session (it refuses where org.gnome.Shell is owned):
#
#   packaging/make-bundle.sh --out /tmp/b && tar xzf /tmp/b/xwlbridge-*.tar.gz -C /tmp/b
#   docker run --rm --network none -v /tmp/b:/b:ro -v "$PWD":/src:ro IMAGE \
#     dbus-run-session -- bash /src/contrib/gnome-shell/tests/run_install_flow.sh /b/xwlbridge-<version>-<arch>
# SPDX-License-Identifier: MIT
set -u
BUNDLE=${1:?usage: run_install_flow.sh BUNDLE_DIR}
UUID=xwlbridge-windows@xwlbridge
PASS=0 FAILN=0
check() { local name=$1; shift; if "$@"; then PASS=$((PASS + 1)); echo "ok   $name"; else FAILN=$((FAILN + 1)); echo "FAIL $name"; fi; }
command -v gnome-shell >/dev/null || { echo "run_install_flow.sh: gnome-shell is not installed here" >&2; exit 2; }
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || { echo "run_install_flow.sh: run it under dbus-run-session" >&2; exit 2; }
if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
     --method org.freedesktop.DBus.NameHasOwner org.gnome.Shell 2>/dev/null | grep -q true; then
  echo "run_install_flow.sh: a gnome-shell already runs on this session bus: refusing (use a scratch session)" >&2; exit 2
fi
export HOME=$(mktemp -d) XDG_RUNTIME_DIR=$(mktemp -d) XDG_CONFIG_HOME= XDG_DATA_HOME=
chmod 700 "$XDG_RUNTIME_DIR"; unset WAYLAND_DISPLAY DISPLAY
GS="" SYSBUS=""
trap 'kill $GS $SYSBUS 2>/dev/null; wait $GS 2>/dev/null; rm -rf "$HOME" "$XDG_RUNTIME_DIR"' EXIT
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
SYSBUS=$(dbus-daemon --config-file="$XDG_RUNTIME_DIR/sysbus.conf" --fork --nopidfile --print-pid) || exit 2
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/sysbus
dbus-update-activation-environment HOME XDG_RUNTIME_DIR DBUS_SYSTEM_BUS_ADDRESS 2>/dev/null
version_call() { gdbus call --session --dest org.xwlbridge.Windows --object-path /org/xwlbridge/Windows --method org.xwlbridge.Windows1.Version 2>/dev/null; }
E=$HOME/.local/share/gnome-shell/extensions/$UUID

# 1. install (no systemd user manager here: the service part is skipped, the extension is not)
out=$(bash "$BUNDLE/install.sh" --no-start 2>&1); rc=$?
check "install.sh rc 0" test $rc -eq 0
check "  ...extension files in ~/.local/share/gnome-shell/extensions" test -f "$E/extension.js" -a -f "$E/metadata.json"
check "  ...enabled in org.gnome.shell enabled-extensions" bash -c 'gsettings get org.gnome.shell enabled-extensions | grep -qF "'"'$UUID'"'"'
check "  ...tells the user to log out and in once" grep -q "It starts after you log out and in once" <<<"$out"

# 2. "log in": GNOME Shell starts and loads it
gnome-shell --headless --wayland --no-x11 --virtual-monitor 1280x800 >"$HOME/gs.log" 2>&1 &
GS=$!
for _ in $(seq 100); do version_call >/dev/null && break; sleep 0.2; done
check "after login: the extension's service answers Version() = 1" bash -c '[ "$(gdbus call --session --dest org.xwlbridge.Windows --object-path /org/xwlbridge/Windows --method org.xwlbridge.Windows1.Version 2>/dev/null)" = "(uint32 1,)" ]'
doc=$("$HOME/.local/bin/xwlbridge-ctl" --display doctor --json 2>/dev/null)
st=$(python3 -c 'import json,sys; c=[c for c in json.loads(sys.argv[1])["checks"] if c["id"]=="gnome.windows_extension"]; print(c[0]["status"], c[0]["summary"])' "$doc")
echo "     doctor: $st"
check "doctor: ok, active, API v1" grep -q "^ok window extension $UUID 1.0 is active (API v1)" <<<"$st"

# 3. uninstall: disabled at once in the running shell, files and settings entries gone
out=$(bash "$HOME/.local/share/xwlbridge/uninstall.sh" 2>&1); rc=$?
check "uninstall.sh rc 0" test $rc -eq 0
check "  ...said it disabled the extension" grep -q "disabled the GNOME Shell extension $UUID" <<<"$out"
sleep 1
check "  ...the running shell stopped it (name gone)" bash -c '! gdbus call --session --dest org.xwlbridge.Windows --object-path /org/xwlbridge/Windows --method org.xwlbridge.Windows1.Version >/dev/null 2>&1'
ls -A "$HOME/.local/share/gnome-shell" 2>/dev/null | sed 's/^/     left by GNOME in ~\/.local\/share\/gnome-shell: /'
check "  ...extension files removed (and the extensions dir the installer created)" test ! -e "$E" -a ! -e "$HOME/.local/share/gnome-shell/extensions"
check "  ...not left in enabled-extensions or disabled-extensions" bash -c '! gsettings get org.gnome.shell enabled-extensions | grep -qF "$1" && ! gsettings get org.gnome.shell disabled-extensions | grep -qF "$1"' _ "$UUID"
if grep -q "JS ERROR" "$HOME/gs.log" && grep -B2 -A8 "JS ERROR" "$HOME/gs.log" | grep -q "$UUID"; then
  echo "FAIL JS errors from $UUID:"; grep -A8 "JS ERROR" "$HOME/gs.log"; FAILN=$((FAILN + 1))
fi
echo; echo "$PASS passed, $FAILN failed"
[ $FAILN -eq 0 ]
