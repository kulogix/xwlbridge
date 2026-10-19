#!/bin/bash
# The bridge's window sync end to end: a headless gnome-shell (GNOME 49/50) with xwlbridge-windows@xwlbridge and a
# probe extension (reports the desktop windows, moves/raises/maximises one on request), the bridge given as $1
# with -showwindows on an Xvnc engine, and X clients (xlogo, xeyes) driven from the X side (bridge_sync_x.py).
# MODE (second argument): sync (default) checks the sync; off runs with XWLBRIDGE_WINDOWS_EXTENSION=off and checks
# the 0.1.2 behaviour. Run it in a container or VM with gnome-shell, Xvnc, xlogo, xeyes, xdotool and python3-xlib,
# never inside your own desktop session (it refuses to run where a gnome-shell already owns org.gnome.Shell):
#
#   docker run --rm --network none -v "$PWD":/src:ro -v "$PWD/target/release/xwlbridge":/xwlbridge:ro IMAGE \
#     dbus-run-session -- bash /src/contrib/gnome-shell/tests/run_bridge_sync.sh /xwlbridge sync
#
# Exit status: 0 when every check passed. SPDX-License-Identifier: MIT
set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BRIDGE=${1:?usage: run_bridge_sync.sh /path/to/xwlbridge [sync|off]}; MODE=${2:-sync}
for t in gnome-shell Xvnc xlogo xeyes xdotool python3; do
  command -v $t >/dev/null || { echo "run_bridge_sync.sh: $t is not installed here" >&2; exit 2; }
done
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || { echo "run_bridge_sync.sh: no session bus: run it under dbus-run-session" >&2; exit 2; }
if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
     --method org.freedesktop.DBus.NameHasOwner org.gnome.Shell 2>/dev/null | grep -q true; then
  echo "run_bridge_sync.sh: a gnome-shell already runs on this session bus: refusing (use a scratch session)" >&2; exit 2
fi
export HOME=$(mktemp -d) XDG_RUNTIME_DIR=$(mktemp -d); chmod 700 "$XDG_RUNTIME_DIR"
OUT=$(mktemp -d); unset WAYLAND_DISPLAY DISPLAY
PIDS=""; trap 'kill $PIDS 2>/dev/null; rm -rf "$HOME" "$XDG_RUNTIME_DIR"' EXIT
EX=$HOME/.local/share/gnome-shell/extensions; mkdir -p "$EX/probe@test"
cp -r "$HERE/../xwlbridge-windows@xwlbridge" "$EX/"
echo '{"uuid": "probe@test", "name": "probe", "description": "test probe", "shell-version": ["49", "50"]}' >"$EX/probe@test/metadata.json"
cat >"$EX/probe@test/extension.js" <<J
import GLib from 'gi://GLib';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
export default class Probe extends Extension {
    enable() {
        this._id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
            const wins = global.display.sort_windows_by_stacking(global.get_window_actors().map(a => a.meta_window));
            GLib.file_set_contents('$OUT/desk.jsonl', wins.map(w => { const r = w.get_frame_rect();
                return JSON.stringify({title: w.get_title(), x: r.x, y: r.y, w: r.width, h: r.height,
                    above: w.is_above(), attention: w.demands_attention}); }).join('\n') + '\n');
            try {
                const [ok, c] = GLib.file_get_contents('$OUT/cmd');
                if (ok) {
                    GLib.unlink('$OUT/cmd');
                    const [verb, title, a, b] = new TextDecoder().decode(c).trim().split(' ');
                    const w = wins.find(w => w.get_title() === title);
                    if (w && verb === 'move') w.move_frame(true, +a, +b);
                    if (w && verb === 'raise') w.activate(global.get_current_time());
                    if (w && verb === 'max') w.maximize();
                }
            } catch (e) {}
            return GLib.SOURCE_CONTINUE;
        });
    }
    disable() { GLib.source_remove(this._id); }
}
J
# gnome-shell needs a system bus; an empty private one will do
DBUS_SYSTEM_BUS_ADDRESS=$(dbus-daemon --session --fork --print-address | head -1); export DBUS_SYSTEM_BUS_ADDRESS
dbus-update-activation-environment HOME XDG_RUNTIME_DIR
gsettings set org.gnome.shell disable-user-extensions false
gsettings set org.gnome.shell welcome-dialog-last-shown-version '999'
# both at startup, the probe first: two quick EnableExtension calls race on this key, and disabling an
# extension disables the ones enabled after it too
gsettings set org.gnome.shell enabled-extensions "['probe@test', 'xwlbridge-windows@xwlbridge']"
gnome-shell --headless --wayland --virtual-monitor 1280x720 >"$OUT/shell.log" 2>&1 & PIDS="$PIDS $!"
for i in $(seq 100); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 0.1; done
export WAYLAND_DISPLAY=wayland-0 XDG_SESSION_TYPE=wayland
for i in $(seq 100); do gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
  -m org.freedesktop.DBus.GetNameOwner org.xwlbridge.Windows >/dev/null 2>&1 && break; sleep 0.1; done
[ "$MODE" = off ] && export XWLBRIDGE_WINDOWS_EXTENSION=off
"$BRIDGE" -verbose -engine xvnc -display 20 -backend gnome -showwindows -nowrap >"$OUT/bridge.log" 2>&1 & PIDS="$PIDS $!"
for i in $(seq 150); do [ -S /tmp/.X11-unix/X20 ] && break; sleep 0.1; done; sleep 2
export DISPLAY=:20
X() { env -u WAYLAND_DISPLAY python3 "$HERE/bridge_sync_x.py" "$@"; }
desk() { X desk "$OUT/desk.jsonl" "$@"; }
order() { X order "$OUT/desk.jsonl" xlogo xeyes; }
cmd() { echo "$*" >"$OUT/cmd"; sleep 1; }
PASS=0 FAIL=0
check() { if [ "$2" = "$3" ]; then PASS=$((PASS+1)); echo "ok   $1"; else FAIL=$((FAIL+1)); echo "FAIL $1: got [$2], want [$3]"; fi; }
xpos() { X geom $1 | cut -d, -f1,2; }
dpos() { desk $1 x y; }
env -u WAYLAND_DISPLAY xlogo -geometry 210x190+50+60 >/dev/null 2>&1 & PIDS="$PIDS $!"
env -u WAYLAND_DISPLAY xeyes -geometry 150x100 >/dev/null 2>&1 & PIDS="$PIDS $!"
sleep 3
L=$(printf '%#x' "$(env -u WAYLAND_DISPLAY xdotool search --name '^xlogo$' | head -1)")
E=$(printf '%#x' "$(env -u WAYLAND_DISPLAY xdotool search --name '^xeyes$' | head -1)")
xorder() { X stack $L $E | sed "s/$L/xlogo/; s/$E/xeyes/"; }
sends() { grep -c "moved itself: the desktop follows" "$OUT/bridge.log"; }
if [ "$MODE" = sync ]; then
  check "the bridge found the extension" "$(grep -c 'window sync with the desktop: extension version' "$OUT/bridge.log")" 1
  check "USPosition: xlogo opens at 50,60" "$(dpos xlogo)" "50,60"
  check "xeyes: X follows GNOME's placement" "$(xpos $E)" "$(dpos xeyes)"
  X animate $L 300 200 0 0 10; sleep 1
  check "xlogo moves itself: the desktop follows" "$(dpos xlogo)" "300,200"
  X animate $L -100 200 0 0 10; sleep 1
  check "past the edge: the desktop clamps" "$(dpos xlogo)" "0,200"
  check "past the edge: X is put back" "$(xpos $L)" "0,200"
  cmd move xeyes 600 400
  check "the desktop moves xeyes: X follows" "$(xpos $E)" "600,400"
  N0=$(sends); X animate $L 100 300 10 40 5; sleep 1; N=$(( $(sends) - N0 ))
  check "an animation: the desktop ends where X does" "$(dpos xlogo)" "500,300"
  check "an animation: coalesced (fewer MoveResize than moves)" "$([ $N -ge 2 ] && [ $N -lt 41 ] && echo yes)" yes
  X resize $L 300 250; sleep 1
  check "xlogo resizes itself: the desktop follows" "$(desk xlogo w h)" "$(X geom $L | cut -d, -f3 | tr x ,)"
  cmd raise xeyes
  check "the desktop raises xeyes: X's stacking follows" "$(xorder)" "$(order)"
  cmd raise xlogo
  check "the desktop raises xlogo: X's stacking follows" "$(xorder)" "xeyes xlogo"
  X raise $E; sleep 1
  check "xeyes raises itself without input: X goes back" "$(xorder)" "xeyes xlogo"
  check "... and it demands attention instead" "$(desk xeyes attention)" True
  X raise $E; sleep 0.3; X raise $E; sleep 1
  check "repeats within 3 s are ignored" "$(grep -c 'raises itself again: ignored' "$OUT/bridge.log")" 2
  X above $E; sleep 1
  check "_NET_WM_STATE_ABOVE: above on the desktop" "$(desk xeyes above)" True
  check "_NET_WM_STATE_ABOVE: X's stacking follows" "$(xorder)" "xlogo xeyes"
  gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell -m org.gnome.Shell.Extensions.DisableExtension xwlbridge-windows@xwlbridge >/dev/null; sleep 1
  X animate $L 200 150 0 0 10; cmd move xeyes 900 450
  check "extension away: a self-move stays in X" "$(dpos xlogo)" "500,300"
  check "extension away: a desktop move does not reach X" "$(xpos $E)" "600,400"
  gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell -m org.gnome.Shell.Extensions.EnableExtension xwlbridge-windows@xwlbridge >/dev/null; sleep 2
  check "extension back: X resyncs to the desktop" "$(xpos $L) $(xpos $E)" "500,300 900,450"
  check "extension back: above kept" "$(desk xeyes above)" True
  cmd max xlogo; sleep 0.5
  check "the desktop maximises xlogo: X is the desktop's rectangle" "$(X geom $L | tr x ,)" "$(desk xlogo x y w h)"
else
  check "extension=off: no sync" "$(grep -c 'window sync with the desktop is off' "$OUT/bridge.log")" 1
  check "extension=off: USPosition is not applied (0.1.2)" "$([ "$(dpos xlogo)" != 50,60 ] && echo yes)" yes
  D0=$(dpos xlogo); X animate $L 300 200 0 0 10; sleep 1
  check "extension=off: a self-move stays in X (0.1.2)" "$(dpos xlogo) $(xpos $L)" "$D0 300,200"
  X0=$(xpos $E); cmd move xeyes 600 400
  check "extension=off: a desktop move does not reach X (0.1.2)" "$(xpos $E)" "$X0"
  check "extension=off: no MoveResize" "$(sends)" 0
fi
echo "run_bridge_sync.sh ($MODE): $PASS passed, $FAIL failed"
[ $FAIL -eq 0 ] || { echo "--- bridge log (tail)"; tail -40 "$OUT/bridge.log"; }
[ $FAIL -eq 0 ]
