#!/bin/bash
# Unit tests for xwlbridge-ctl: settings parsing/editing, the command it builds,
# and the engine binary guard (RealVNC's set-uid Xvnc must never be chosen, nor
# executed). Needs only bash + coreutils; runs as an ordinary user.
#   bash packaging/tests/test_ctl.sh
set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
CTL=$HERE/../xwlbridge-ctl
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
PASS=0; FAILN=0
ok() { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAILN=$((FAILN + 1)); printf 'FAIL %s\n' "$1"; [ $# -gt 1 ] && printf '     %s\n' "${@:2}"; }
check() { local name=$1; shift; if "$@"; then ok "$name"; else bad "$name"; fi; }

export HOME=$T/home XDG_CONFIG_HOME=$T/home/.config XDG_RUNTIME_DIR=$T/run
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
unset XWLBRIDGE_CONFIG DISPLAY
MARK=$T/EXECUTED

# fake bridge: prints its argv and what "Xvnc"/"Xvfb" resolve to on its PATH
mkdir -p "$T/pkg/bin"
cat >"$T/pkg/bin/xwlbridge" <<'EOF'
#!/bin/bash
[ "${1:-}" = -version ] && { echo "xwlbridge 0.0.8"; exit 0; }
[ "${1:-}" = -help ] && { echo " -backend auto|wlr|gnome"; echo " -engine builtin|xvfb|xvnc"; echo " -uidisplay display|auto"; exit 0; }
printf 'ARG=%s\n' "$@"
printf 'XVNC=%s\n' "$(readlink -f "$(command -v Xvnc)" 2>/dev/null)"
printf 'XVFB=%s\n' "$(readlink -f "$(command -v Xvfb)" 2>/dev/null)"
EOF
chmod 755 "$T/pkg/bin/xwlbridge"
cp "$CTL" "$T/pkg/bin/xwlbridge-ctl"; chmod 755 "$T/pkg/bin/xwlbridge-ctl"
C=$T/pkg/bin/xwlbridge-ctl

# fake RealVNC server programs
mkdir -p "$T/rvnc"
for p in vncserver-x11 vncserver-x11-core; do printf '#!/bin/sh\necho %s "$@"\n' "$p" >"$T/rvnc/$p"; chmod 755 "$T/rvnc/$p"; done

# decoys: RealVNC's Xvnc look-alikes that record any execution
mkdir -p "$T/decoy_suid" "$T/decoy_rvnc" "$T/tiger" "$T/tiger2" "$T/xvfb" "$T/empty"
printf '#!/bin/sh\n# https://www.realvnc.com\ntouch %s\n' "$MARK" >"$T/decoy_suid/Xvnc"; chmod 4755 "$T/decoy_suid/Xvnc"
printf '#!/bin/sh\n# Copyright RealVNC Ltd https://www.realvnc.com\ntouch %s\n' "$MARK" >"$T/decoy_rvnc/Xvnc"; chmod 755 "$T/decoy_rvnc/Xvnc"
printf '#!/bin/sh\n# TigerVNC\ntouch %s\n' "$MARK" >"$T/tiger/Xtigervnc"; chmod 755 "$T/tiger/Xtigervnc"
ln -s Xtigervnc "$T/tiger/Xvnc"
printf '#!/bin/sh\n# Xvnc TigerVNC 1.15 https://www.tigervnc.org\ntouch %s\n' "$MARK" >"$T/tiger2/Xvnc"; chmod 755 "$T/tiger2/Xvnc"
printf '#!/bin/sh\ntouch %s\n' "$MARK" >"$T/xvfb/Xvfb"; chmod 755 "$T/xvfb/Xvfb"
# the system's tools without any X server (the test machine may have TigerVNC/Xvfb installed)
mkdir -p "$T/sys"
for f in /usr/bin/* /bin/*; do
  case ${f##*/} in X|Xorg|Xvnc|Xtigervnc|Xvfb|Xwayland|Xnest|Xephyr|xwlbridge|xwlbridge-ctl|vncserver-x11*) continue;; esac
  [ -e "$T/sys/${f##*/}" ] || ln -s "$f" "$T/sys/${f##*/}"
done
SYS=$T/sys

conf() { mkdir -p "$XDG_CONFIG_HOME/xwlbridge"; printf '%s\n' "$@" >"$XDG_CONFIG_HOME/xwlbridge/xwlbridge.conf"; }

# a private X socket directory with a live display :5 (the desktop's Xwayland stand-in)
export XWLBRIDGE_X11_UNIX_DIR=$T/x11
mkdir -p "$XWLBRIDGE_X11_UNIX_DIR"
python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' "$XWLBRIDGE_X11_UNIX_DIR/X5" 2>/dev/null || \
  { echo "test_ctl.sh needs python3 to make a unix socket" >&2; exit 2; }
cmdline() { PATH=$1 "$C" command 2>"$T/err"; }
has_arg() { grep -qxF -- "$1" <<<"$2"; }

# ---- settings
conf "engine=builtin" "rfb_port=5901" "server=$T/rvnc/vncserver-x11" "uidisplay=:5" "server_log=" "log=verbose"
out=$(cmdline "$T/empty:$SYS")
check "builtin command" grep -q -- "-verbose -backend gnome -engine builtin -uidisplay :5 $T/rvnc/vncserver-x11 -RfbPort=5901\$" <<<"$out"

conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=:5" 'server_args=-DesktopName="my desk" -Encryption=AlwaysOn' 'bridge_args=-fps 30'
out=$(PATH="$T/empty:$SYS" "$C" run 2>/dev/null)
check "quoted server_args keep one word" has_arg "ARG=-DesktopName=my desk" "$out"
check "bridge_args split" grep -qx -- "ARG=30" <<<"$out"
check "default port + journal log" bash -c 'grep -qxF -- "ARG=-RfbPort=5900" <<<"$1" && grep -qxF -- "ARG=-Log=*:stderr:30" <<<"$1"' _ "$out"

# rfb_port=realvnc / empty: the port is RealVNC's own setting (no -RfbPort)
for v in realvnc ""; do
  conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=:5" "rfb_port=$v"
  out=$(cmdline "$T/empty:$SYS"); rc=$?
  check "rfb_port='$v' -> no -RfbPort" bash -c '[ $2 -eq 0 ] && ! grep -q -- -RfbPort <<<"$1"' _ "$out" $rc
done

conf "engine=nope" "capture=x" "rfb_port=70000" "uidisplay=foo" "bogus=1" "no equals sign" "server_args=-a \"unbalanced"
PATH="$T/empty:$SYS" "$C" command >/dev/null 2>"$T/err"; rc=$?
check "bad settings -> exit 78" test $rc -eq 78
for want in "engine: 'nope'" "capture: 'x'" "rfb_port: '70000' is not a TCP port (1-65535), realvnc or empty" "uidisplay: 'foo'" "unknown setting 'bogus'" "expected KEY=value" "server_args: unbalanced"; do
  check "reports: $want" grep -qF -- "$want" "$T/err"
done

rm -f "$XDG_CONFIG_HOME/xwlbridge/xwlbridge.conf"
cp "$HERE/../xwlbridge.conf" "$T/pkg/bin/xwlbridge.conf"   # template beside the script (source tree layout)
PATH="$T/empty:$SYS" "$C" config set rfb_port 5902 2>/dev/null
check "config set creates the file from the template" grep -q '^# xwlbridge: RealVNC Server' "$XDG_CONFIG_HOME/xwlbridge/xwlbridge.conf"
check "config get" test "$(PATH="$T/empty:$SYS" "$C" config get rfb_port)" = 5902
check "config set replaces in place" test "$(grep -c '^rfb_port=' "$XDG_CONFIG_HOME/xwlbridge/xwlbridge.conf")" = 1
PATH="$T/empty:$SYS" "$C" config set engine bogus 2>/dev/null; rc=$?
check "config set rejects a bad value" test $rc -ne 0 -a "$(PATH="$T/empty:$SYS" "$C" config get engine)" = auto
PATH="$T/empty:$SYS" "$C" config set server_args '-Authentication=VncAuth -Encryption=AlwaysOn' 2>/dev/null
check "config set multi-word value" test "$(PATH="$T/empty:$SYS" "$C" config get server_args)" = "-Authentication=VncAuth -Encryption=AlwaysOn"
PATH="$T/empty:$SYS" "$C" config unset rfb_port 2>/dev/null
check "config unset -> default" test "$(PATH="$T/empty:$SYS" "$C" config get rfb_port)" = 5900
check "config file mode 0600" test "$(stat -c %a "$XDG_CONFIG_HOME/xwlbridge/xwlbridge.conf")" = 600

# ---- engine guard
base=("server=$T/rvnc/vncserver-x11" "uidisplay=:5")
conf "engine=auto" "${base[@]}"
out=$(cmdline "$T/decoy_suid:$T/empty:$SYS")
check "auto + set-uid decoy: engine builtin" grep -q -- '-engine builtin' <<<"$out"
out=$(cmdline "$T/decoy_rvnc:$T/empty:$SYS")
check "auto + non-suid RealVNC Xvnc -> builtin" grep -q -- '-engine builtin' <<<"$out"
conf "engine=xvnc" "${base[@]}"
PATH="$T/decoy_suid:$T/decoy_rvnc:$T/empty:$SYS" "$C" command >/dev/null 2>"$T/err"; rc=$?
check "xvnc + only RealVNC Xvnc -> exit 78" test $rc -eq 78
check "  ...says set-uid never used" grep -q 'set-uid/set-gid' "$T/err"
check "  ...suggests the TigerVNC package" grep -q 'tigervnc-standalone-server' "$T/err"
out=$(PATH="$T/decoy_suid:$T/tiger:$T/empty:$SYS" "$C" run 2>/dev/null)
check "xvnc: TigerVNC found behind a RealVNC Xvnc on PATH" grep -qx -- "XVNC=$T/tiger/Xtigervnc" <<<"$out"
check "xvnc: -engine xvnc passed" grep -qx -- "ARG=xvnc" <<<"$out"
out=$(PATH="$T/decoy_suid:$T/tiger2:$T/empty:$SYS" "$C" run 2>/dev/null)
check "xvnc: a TigerVNC binary named Xvnc (Fedora) is accepted" grep -qx -- "XVNC=$T/tiger2/Xvnc" <<<"$out"
conf "engine=xvnc" "xvnc=$T/decoy_suid/Xvnc" "${base[@]}"
PATH="$T/tiger:$T/empty:$SYS" "$C" command >/dev/null 2>"$T/err"; rc=$?
check "xvnc=<RealVNC path> explicitly -> refused (78)" test $rc -eq 78
conf "engine=auto" "${base[@]}"
out=$(PATH="$T/decoy_suid:$T/tiger:$T/empty:$SYS" "$C" run 2>/dev/null)
check "auto + TigerVNC -> xvnc" grep -qx -- "ARG=xvnc" <<<"$out"
conf "engine=xvfb" "${base[@]}"
out=$(PATH="$T/xvfb:$T/empty:$SYS" "$C" run 2>/dev/null)
check "xvfb engine via private PATH" grep -qx -- "XVFB=$T/xvfb/Xvfb" <<<"$out"
check "private engine dir is 0700" test "$(stat -c %a "$XDG_RUNTIME_DIR/xwlbridge/bin")" = 700
check "no decoy or engine was ever executed by xwlbridge-ctl" test ! -e "$MARK"

# ---- uidisplay: RealVNC never runs on the built-in X server without its windows on the desktop
# (its UI segfaults there every ~2 s); the stock engines may hide them
conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=auto"
out=$(DISPLAY= PATH="$T/empty:$SYS" "$C" run 2>"$T/err"); rc=$?
check "builtin + uidisplay auto without DISPLAY -> exit 75 (retried), bridge not started" bash -c '[ $1 -eq 75 ] && [ -z "$2" ] && grep -q "waiting for the desktop" "$3"' _ $rc "$out" "$T/err"
out=$(DISPLAY=:987 PATH="$T/empty:$SYS" "$C" command 2>"$T/err"); rc=$?
check "builtin + uidisplay auto, display without socket -> 75" bash -c '[ $1 -eq 75 ] && grep -q "has no socket" "$2"' _ $rc "$T/err"
out=$(DISPLAY=:5 PATH="$T/empty:$SYS" "$C" command 2>/dev/null)
check "builtin + uidisplay auto with a live display" grep -q -- '-engine builtin -uidisplay auto' <<<"$out"
conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=:6"
PATH="$T/empty:$SYS" "$C" command >/dev/null 2>&1; rc=$?
check "builtin + uidisplay=:6 (no socket) -> 75" test $rc -eq 75
conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=otherhost:1"
out=$(PATH="$T/empty:$SYS" "$C" command 2>/dev/null)
check "uidisplay on another host is not checked locally" grep -q -- '-uidisplay otherhost:1' <<<"$out"
for e in builtin auto; do
  conf "engine=$e" "server=$T/rvnc/vncserver-x11" "uidisplay=none"
  PATH="$T/decoy_suid:$T/empty:$SYS" "$C" run >"$T/out" 2>"$T/err"; rc=$?
  check "engine=$e (-> builtin) + uidisplay=none -> 78, bridge not started" bash -c '[ $1 -eq 78 ] && [ ! -s "$2" ] && grep -q "uidisplay=none needs engine xvnc or xvfb" "$3"' _ $rc "$T/out" "$T/err"
done
conf "engine=xvnc" "server=$T/rvnc/vncserver-x11" "uidisplay=auto"
out=$(DISPLAY= PATH="$T/tiger:$T/empty:$SYS" "$C" command 2>"$T/err")
check "xvnc + uidisplay auto without DISPLAY -> windows hidden + note" bash -c 'grep -q -- "-uidisplay none" <<<"$1" && grep -q "no DISPLAY" "$2"' _ "$out" "$T/err"
conf "engine=xvnc" "server=$T/rvnc/vncserver-x11" "uidisplay=none"
out=$(PATH="$T/tiger:$T/empty:$SYS" "$C" command 2>/dev/null)
check "xvnc + uidisplay=none allowed, and said (the bridge's own default would be auto)" bash -c 'grep -q -- "-engine xvnc -uidisplay none" <<<"$1"' _ "$out"
out=$(PATH="$T/tiger:$T/empty:$SYS" setpriv --no-new-privs "$C" command 2>/dev/null)
check "no-new-privs + uidisplay=none -> vncserver-x11-core directly" grep -q -- "$T/rvnc/vncserver-x11-core" <<<"$out"

# ---- RealVNC's own settings: realvnc-get, and the doctor's port / IP filter
mkdir -p "$HOME/.vnc/config.d"
printf '%s\n' '_AnlLastConnTime=1' 'RfbPort=5911' 'IpClientAddresses=+192.168.1.0/255.255.255.0,+127.0.0.1,-' 'Authentication=VncAuth' 'Password=0123456789abcdef' >"$HOME/.vnc/config.d/vncserver-x11"
conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=:5" "rfb_port=realvnc"
check "realvnc-get RfbPort" test "$(PATH="$T/empty:$SYS" "$C" realvnc-get RfbPort)" = 5911
check "realvnc-get unset parameter -> exit 1" bash -c '! PATH="$1" "$2" realvnc-get DesktopName >/dev/null' _ "$T/empty:$SYS" "$C"
if command -v python3 >/dev/null; then
  PATH="$T/empty:$SYS" "$C" doctor --json >"$T/doctor_rv.json" 2>/dev/null
  check "doctor: port 5911 from RealVNC's own settings" python3 -c 'import json,sys; d={c["id"]:c for c in json.load(open(sys.argv[1]))["checks"]}; c=d["realvnc.rfb_port"]; assert "port 5911" in c["summary"] and "vncserver-x11" in c["summary"], c' "$T/doctor_rv.json"
  check "doctor: IP filter reported" python3 -c 'import json,sys; d={c["id"]:c for c in json.load(open(sys.argv[1]))["checks"]}; c=d["realvnc.ip_filter"]; assert c["status"]=="ok" and "+192.168.1.0/255.255.255.0" in c["summary"], c' "$T/doctor_rv.json"
  check "doctor: VncAuth with a password" python3 -c 'import json,sys; d={c["id"]:c for c in json.load(open(sys.argv[1]))["checks"]}; c=d["realvnc.auth"]; assert c["status"]=="ok" and "VncAuth" in c["summary"], c' "$T/doctor_rv.json"
  check "doctor never prints the password hash" bash -c '! grep -q 0123456789abcdef "$1"' _ "$T/doctor_rv.json"
  conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=:5" "rfb_port=5902"
  PATH="$T/empty:$SYS" "$C" doctor --json >"$T/doctor_rv2.json" 2>/dev/null
  check "doctor: rfb_port number beats RealVNC's RfbPort" python3 -c 'import json,sys; d={c["id"]:c for c in json.load(open(sys.argv[1]))["checks"]}; c=d["realvnc.rfb_port"]; assert "port 5902 (rfb_port" in c["summary"], c' "$T/doctor_rv2.json"
  conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=auto"
  DISPLAY= PATH="$T/empty:$SYS" "$C" doctor --json >"$T/doctor_rv3.json" 2>/dev/null
  check "doctor: builtin without a desktop X display -> engine.selected fail (service waits)" python3 -c 'import json,sys; d={c["id"]:c for c in json.load(open(sys.argv[1]))["checks"]}; c=d["engine.selected"]; assert c["status"]=="fail" and "waits" in c["summary"], c' "$T/doctor_rv3.json"
fi
rm -rf "$HOME/.vnc"

# ---- doctor runs anywhere and emits valid JSON
conf "engine=auto" "server=$T/rvnc/vncserver-x11"
PATH="$T/decoy_suid:$T/empty:$SYS" "$C" doctor --json >"$T/doctor.json" 2>"$T/doctor.err"
if command -v python3 >/dev/null; then
  check "doctor --json parses" python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["checks"]' "$T/doctor.json"
  check "doctor reports the RealVNC Xvnc as never used" python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert any(c["id"]=="engine.realvnc_xvnc" and "never used" in c["summary"] for c in d["checks"])' "$T/doctor.json"
fi
check "doctor never executed a decoy" test ! -e "$MARK"

# ---- one RealVNC user-mode server per user: another one running -> run exits 75 (retried), doctor fails
mkdir -p "$T/other"
printf '#!/bin/sh\nsleep 30\n' >"$T/other/vncserver-x11-core"; chmod 755 "$T/other/vncserver-x11-core"
"$T/other/vncserver-x11-core" -RfbPort=5911 & OTHER=$!
sleep 0.3
conf "engine=builtin" "server=$T/rvnc/vncserver-x11" "uidisplay=:5"
out=$(PATH="$T/empty:$SYS" "$C" run 2>"$T/err"); rc=$?
check "another user-mode server -> run exits 75" test $rc -eq 75
check "  ...names it" grep -q "pid $OTHER" "$T/err"
check "  ...and did not start the bridge" test -z "$out"
if command -v python3 >/dev/null; then
  PATH="$T/empty:$SYS" "$C" doctor --json >"$T/doctor2.json" 2>/dev/null
  check "doctor fails realvnc.other_server" python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert any(c["id"]=="realvnc.other_server" and c["status"]=="fail" for c in d["checks"])' "$T/doctor2.json"
fi
kill $OTHER 2>/dev/null; wait $OTHER 2>/dev/null
out=$(PATH="$T/empty:$SYS" "$C" run 2>/dev/null)
check "once it is gone, run starts the bridge" grep -q -- '-engine' <<<"$out"

# ---- install.sh / uninstall.sh in a scratch HOME with a fake systemd user manager
H2=$T/home2; mkdir -p "$H2" "$T/fakebin"
cat >"$T/fakebin/systemctl" <<'EOS'
#!/bin/bash
# fake `systemctl --user`: graphical-session.target is "activating" for the first 3 queries, then "active"
log=$FAKE_DIR/systemctl.log; echo "$*" >>"$log"
case "$*" in
  "--user show-environment") echo "WAYLAND_DISPLAY=wayland-0"; echo "DISPLAY=:0";;
  "--user is-active graphical-session.target")
    n=$(cat "$FAKE_DIR/gs" 2>/dev/null || echo 0); echo $((n + 1)) >"$FAKE_DIR/gs"
    if [ "$n" -lt 3 ]; then echo activating; exit 3; else echo active; fi;;
  "--user is-active xwlbridge-realvnc.service") echo active;;
  "--user enable xwlbridge-realvnc.service") mkdir -p "$HOME/.config/systemd/user/graphical-session.target.wants"
    ln -sf ../xwlbridge-realvnc.service "$HOME/.config/systemd/user/graphical-session.target.wants/xwlbridge-realvnc.service";;
  "--user disable --now xwlbridge-realvnc.service") rm -f "$HOME/.config/systemd/user/graphical-session.target.wants/xwlbridge-realvnc.service";;
  *) ;;
esac
exit 0
EOS
chmod 755 "$T/fakebin/systemctl"
cp "$HERE/../install.sh" "$HERE/../uninstall.sh" "$HERE/../xwlbridge-realvnc.service.in" "$HERE/../xwlbridge-status.desktop.in" "$HERE/../xwlbridge.conf" "$T/pkg/bin/"
before=$(cd "$H2" && find . | sort)
out=$(HOME=$H2 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$T/pkg/bin/install.sh" --bin "$T/pkg/bin/xwlbridge" 2>&1); rc=$?
check "install.sh (no root, fake user manager) rc 0" test $rc -eq 0
check "  ...waited for graphical-session.target" grep -q 'waiting for the GNOME session to finish starting (activating)' <<<"$out"
check "  ...then (re)started the service" grep -qx -- '--user restart xwlbridge-realvnc.service' "$T/systemctl.log"
check "  ...unit rendered with the install path" grep -qx "ExecStart=$H2/.local/bin/xwlbridge-ctl run" "$H2/.config/systemd/user/xwlbridge-realvnc.service"
# a stop/logout exits 0 (the bridge's own doing); every failure status still restarts
check "  ...unit: a crash restarts, a stop is a success by its status 0 alone" bash -c 'grep -qx "Restart=on-failure" "$1" &&
  ! grep -qE "^[[:space:]]*SuccessExitStatus[[:space:]]*=" "$1" &&
  ! grep -qE "^[[:space:]]*RestartPreventExitStatus[[:space:]]*=.*\<(143|69|70|137|139)\>" "$1"' _ "$H2/.config/systemd/user/xwlbridge-realvnc.service"
check "  ...settings 0600" test "$(stat -c %a "$H2/.config/xwlbridge/xwlbridge.conf")" = 600
check "  ...app-grid entry" grep -qx "Exec=$H2/.local/bin/xwlbridge-ctl show" "$H2/.local/share/applications/xwlbridge-status.desktop"
out=$(HOME=$H2 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$H2/.local/share/xwlbridge/uninstall.sh" --purge 2>&1); rc=$?
check "uninstall.sh --purge rc 0" test $rc -eq 0
after=$(cd "$H2" && find . | sort)
check "  ...HOME exactly as before the install" test "$before" = "$after"

# a user whose RealVNC settings already choose the port (as the host's :5911 setup): left to RealVNC
H3=$T/home3; mkdir -p "$H3/.vnc/config.d" "$H3/.cache"
printf '%s\n' 'RfbPort=5911' 'IpClientAddresses=+127.0.0.1,-' >"$H3/.vnc/config.d/vncserver-x11"
before=$(cd "$H3" && find . | sort)
out=$(HOME=$H3 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$T/pkg/bin/install.sh" --bin "$T/pkg/bin/xwlbridge" --no-start 2>&1); rc=$?
check "install with RealVNC's own RfbPort -> rfb_port=realvnc" bash -c '[ $1 -eq 0 ] && grep -qx "rfb_port=realvnc" "$2" && grep -q "RfbPort=5911" <<<"$3"' _ $rc "$H3/.config/xwlbridge/xwlbridge.conf" "$out"
check "  ...RealVNC's file untouched" test "$(cat "$H3/.vnc/config.d/vncserver-x11")" = "$(printf '%s\n' 'RfbPort=5911' 'IpClientAddresses=+127.0.0.1,-')"
# the bridge's -uidisplay hook copy (newer bridges: ~/.cache/xwlbridge) is runtime litter of ours
mkdir -p "$H3/.cache/xwlbridge"; : >"$H3/.cache/xwlbridge/xwlbridge-uihook-0123456789abcdef.so"
HOME=$H3 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$H3/.local/share/xwlbridge/uninstall.sh" --purge >/dev/null 2>&1
after=$(cd "$H3" && find . | sort)
check "  ...uninstall --purge: HOME as before (hook cache dir removed, RealVNC settings kept)" test "$before" = "$after"
# --purge without the before-list keeps ~/.vnc entirely (it cannot tell the user's files from RealVNC's new ones)
H6=$T/home6; mkdir -p "$H6/.vnc/config.d"; printf 'Password=x\n' >"$H6/.vnc/config.d/vncserver-x11"
HOME=$H6 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$T/pkg/bin/install.sh" --bin "$T/pkg/bin/xwlbridge" --no-start >/dev/null 2>&1
: >"$H6/.vnc/created-after-install"
rm -f "$H6/.local/share/xwlbridge/vnc-before.list"
out=$(HOME=$H6 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$H6/.local/share/xwlbridge/uninstall.sh" --purge 2>&1)
check "uninstall --purge without vnc-before.list keeps ~/.vnc" bash -c '[ -f "$1/.vnc/config.d/vncserver-x11" ] && [ -f "$1/.vnc/created-after-install" ] && grep -q "kept ~/.vnc" <<<"$2"' _ "$H6" "$out"
# with the list: only what appeared since goes; a name with a newline cannot reach outside ~/.vnc
H7=$T/home7; mkdir -p "$H7/.vnc/config.d"; printf 'Password=x\n' >"$H7/.vnc/config.d/vncserver-x11"; : >"$H7/victim"
HOME=$H7 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$T/pkg/bin/install.sh" --bin "$T/pkg/bin/xwlbridge" --no-start >/dev/null 2>&1
: >"$H7/.vnc/new-after-install"
mkdir -p "$H7/.vnc/evil"$'\n'"$H7" && : >"$H7/.vnc/evil"$'\n'"$H7/victim"   # find prints "...evil" + newline + "$H7/victim"
HOME=$H7 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$H7/.local/share/xwlbridge/uninstall.sh" --purge >/dev/null 2>&1
check "uninstall --purge: new files gone, old settings and files outside ~/.vnc kept" bash -c '[ ! -e "$1/.vnc/new-after-install" ] && [ -f "$1/.vnc/config.d/vncserver-x11" ] && [ -f "$1/victim" ]' _ "$H7"
# a link inside ~/.vnc plus a crafted name: nothing outside ~/.vnc is reached
H8=$T/home8; mkdir -p "$H8/.vnc/config.d"; printf 'Password=x\n' >"$H8/.vnc/config.d/vncserver-x11"; : >"$H8/victim"
HOME=$H8 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$T/pkg/bin/install.sh" --bin "$T/pkg/bin/xwlbridge" --no-start >/dev/null 2>&1
ln -s "$H8" "$H8/.vnc/aaa"
mkdir -p "$H8/.vnc/evil"$'\n'"$H8/.vnc/aaa" && : >"$H8/.vnc/evil"$'\n'"$H8/.vnc/aaa/victim"   # a line "$H8/.vnc/aaa/victim"
HOME=$H8 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$H8/.local/share/xwlbridge/uninstall.sh" --purge >/dev/null 2>&1
check "uninstall --purge: a link in ~/.vnc plus a crafted name keeps the file outside" bash -c '[ -f "$1/victim" ] && [ -f "$1/.vnc/config.d/vncserver-x11" ] && [ ! -e "$1/.vnc/aaa" ]' _ "$H8"
# the -uidisplay hook stays while a bridge started by hand runs; stale engine dirs go
H9=$T/home9; RT9=$T/rt9; mkdir -p "$H9" "$RT9" "$T/procs"; cp "$(command -v bash)" "$T/procs/xwlbridge"
HOME=$H9 XDG_CONFIG_HOME= XDG_DATA_HOME= XDG_RUNTIME_DIR=$RT9 FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$T/pkg/bin/install.sh" --bin "$T/pkg/bin/xwlbridge" --no-start >/dev/null 2>&1
: >"$RT9/xwlbridge-uihook-0123456789abcdef.so"
"$T/procs/xwlbridge" -c 'sleep 30; :' & HANDMADE=$!
sleep 30 & LIVE=$!; ( exit 0 ) & DEADP=$!; wait $DEADP
mkdir -p "$RT9/xwlbridge-$DEADP-abcdef" "$RT9/xwlbridge-$LIVE-ghijkl"; : >"$RT9/xwlbridge-$DEADP-abcdef/Xauthority"
out=$(HOME=$H9 XDG_CONFIG_HOME= XDG_DATA_HOME= XDG_RUNTIME_DIR=$RT9 FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash "$H9/.local/share/xwlbridge/uninstall.sh" 2>&1)
check "uninstall keeps the hook while a hand-started bridge runs" bash -c '[ -f "$1/xwlbridge-uihook-0123456789abcdef.so" ] && grep -q "kept the -uidisplay hook" <<<"$2"' _ "$RT9" "$out"
check "  ...and removes a dead bridge's engine directory, not a live one's" bash -c '[ ! -e "$1/xwlbridge-$2-abcdef" ] && [ -d "$1/xwlbridge-$3-ghijkl" ]' _ "$RT9" "$DEADP" "$LIVE"
kill $HANDMADE $LIVE 2>/dev/null; wait $HANDMADE $LIVE 2>/dev/null
out=$(HOME=$T/home4 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash -c 'mkdir -p "$HOME"; bash "$1" --bin "$2" --port realvnc --no-start' _ "$T/pkg/bin/install.sh" "$T/pkg/bin/xwlbridge" 2>&1); rc=$?
check "install --port realvnc" bash -c '[ $1 -eq 0 ] && grep -qx "rfb_port=realvnc" "$2"' _ $rc "$T/home4/.config/xwlbridge/xwlbridge.conf"
out=$(HOME=$T/home5 XDG_CONFIG_HOME= XDG_DATA_HOME= FAKE_DIR=$T PATH="$T/fakebin:$T/empty:$SYS" bash -c 'mkdir -p "$HOME"; bash "$1" --bin "$2" --port bogus --no-start' _ "$T/pkg/bin/install.sh" "$T/pkg/bin/xwlbridge" 2>&1); rc=$?
check "install --port bogus -> refused" test $rc -ne 0

printf '\n%d passed, %d failed\n' "$PASS" "$FAILN"
[ $FAILN -eq 0 ]
