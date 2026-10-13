#!/bin/bash
# Shared set-up for the proxy's container tests (a GNOME test-bed image, see bed/README.md; under dbus-run-session):
#   headless gnome-shell 50 session, the bridge with -engine xvnc -a11ybus -reverseinput on :20, the Speech
#   Dispatcher logger module, Orca, focus/event loggers on both buses, and helpers to drive the person's keys.
# Source with OUT=<dir> BIN=<bridge binary> [BRIDGE_FLAGS=...] [ORCA=1].
set -u
W=${W:-/w}
T=$W/tools/x11-a11y-proxy
TA=$T/tests/apps
BIN=${BIN:-$W/target/release/xwlrvnc}
BRIDGE_FLAGS=${BRIDGE_FLAGS:--a11ybus -reverseinput}
mkdir -p "$OUT"
SIZE=${SIZE:-1280x720} W=$W source $T/tests/bed/shell_session.sh >"$OUT/session.txt" 2>&1
gsettings set org.gnome.desktop.interface toolkit-accessibility true
$BIN -verbose -engine xvnc -display 20 -backend gnome -nowrap $BRIDGE_FLAGS >"$OUT/bridge.log" 2>&1 &
BRIDGE=$!
for i in $(seq 150); do [ -S /tmp/.X11-unix/X20 ] && break; sleep 0.1; done
sleep 1
E() { env -u WAYLAND_DISPLAY DISPLAY=:20 XDG_SESSION_TYPE=x11 QT_QPA_PLATFORM=xcb GDK_BACKEND=x11 "$@"; }
PRIV=$(E xprop -root AT_SPI_BUS 2>/dev/null | sed -n 's/.*= "\(.*\)"/\1/p')
echo "private bus: $PRIV" >>"$OUT/session.txt"
E xinput list --short >"$OUT/engine_devices.txt" 2>&1
if [ "${ORCA:-1}" = 1 ]; then
  SPEECH_LOG=$OUT/speech.log source $T/tests/bed/speechd_logger.sh
  orca --replace --debug-file=$OUT/orca_debug.txt >"$OUT/orca.log" 2>&1 &
  ORCA_PID=$!
  sleep 8
fi
mark() { echo "$(date +%T.%N) === MARK $1" >>"$OUT/speech.log"; echo "$(date +%T.%N) === $1" >>"$OUT/marks.txt"; }
# the person's keys/pointer through mutter RemoteDesktop; each call's first key press is lost -> lead with F24
GI() { python3 $T/tests/bed/gnome_input.py key 194 sleep 0.3 "$@" >>"$OUT/gnome_input.log" 2>&1; }
# event loggers: focus/state/value/text events of the proxy (session bus) and of the app (private bus)
python3 $T/tests/evlog.py session "$OUT/events_proxy.jsonl" &
EVP=$!
[ -n "$PRIV" ] && { AT_SPI_BUS_ADDRESS=$PRIV python3 $T/tests/evlog.py private "$OUT/events_app.jsonl" "" & EVA=$!; }
proxy_start() {  # $1 = tag, rest = app command (started by the proxy on :20); TS=1: also app_TAG_ts.log
  local tag=$1; shift
  if [ "${TS:-0}" = 1 ]; then
    GDK_BACKEND=wayland $T/x11-a11y-proxy --display :20 --log "$OUT/proxy_$tag.log" -v ${PROXY_FLAGS:-} -- "$@" \
      > >(python3 $T/tests/tsline.py "$OUT/app_$tag.log" "$OUT/app_${tag}_ts.log") 2>&1 &
  else
    GDK_BACKEND=wayland $T/x11-a11y-proxy --display :20 --log "$OUT/proxy_$tag.log" -v ${PROXY_FLAGS:-} -- "$@" \
      >"$OUT/app_$tag.log" 2>&1 &
  fi
  PROXY=$!
}
finish() {
  [ -n "${ORCA_PID:-}" ] && { kill -TERM $ORCA_PID; sleep 3; kill -9 $ORCA_PID 2>/dev/null; }
  grep -E "SPEECH OUTPUT" "$OUT/orca_debug.txt" 2>/dev/null | cut -c1-240 >"$OUT/orca_speech.txt"
  kill $EVP ${EVA:-} 2>/dev/null
  kill $BRIDGE 2>/dev/null; sleep 1
  cp /tmp/shell.log "$OUT/shell.log" 2>/dev/null
}
