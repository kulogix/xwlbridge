#!/bin/bash
# Installs xwlbridge for the current user (no root needed): the bridge, xwlbridge-ctl,
# the systemd user services (started with the GNOME session) and a settings file.
# Running it again upgrades in place; settings are kept. An xwlrvnc install (this
# project's name before 0.1.0) is taken over once: xwlbridge-ctl migrate.
#
#   ./install.sh [--bin PATH] [--prefix DIR] [--service S] [--port N] [--engine E] [--no-enable] [--no-start]
#
#   --bin PATH     the xwlbridge binary (default: ./xwlbridge in a release bundle, else
#                  the source tree's target/dist/xwlbridge or target/release/xwlbridge)
#   --prefix DIR   where programs go (default ~/.local: ~/.local/bin, ~/.local/share/xwlbridge)
#   --service S    the service to enable: realvnc (xwlbridge-realvnc.service: RealVNC Server
#                  on the bridge) or display (xwlbridge.service: the bridge's X display
#                  alone, for X11 tools). Default: realvnc when RealVNC Server's
#                  vncserver-x11 is installed, else display. Both units are installed.
#   --port N       TCP port for a new settings file (default 5900, or 5901 while
#                  RealVNC's system service holds 5900). --port realvnc: leave the
#                  port to RealVNC's own settings (RfbPort); the default when
#                  RealVNC's settings already set RfbPort
#   --engine E     engine for a new settings file (auto|builtin|xvnc|xvfb|xwayland; default auto)
#   --no-enable    do not enable the service for future sessions
#   --no-start     do not start (or restart) it now
#
# Remove with: ~/.local/share/xwlbridge/uninstall.sh
# SPDX-License-Identifier: MIT
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BIN="" PREFIX="$HOME/.local" PORT="" ENGINE="" ENABLE=1 START=1 SERVICE=auto
while [ $# -gt 0 ]; do
  case $1 in
    --bin) BIN=$2; shift 2;;
    --prefix) PREFIX=$2; shift 2;;
    --port) PORT=$2; shift 2;;
    --engine) ENGINE=$2; shift 2;;
    --no-enable) ENABLE=0; shift;;
    --no-start) START=0; shift;;
    --service) SERVICE=$2; shift 2;;
    -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0;;
    *) echo "install.sh: unknown option $1 (try --help)" >&2; exit 2;;
  esac
done

say() { printf '==> %s\n' "$*"; }
warn() { printf 'install.sh: warning: %s\n' "$*" >&2; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -ne 0 ] || die "run this as the desktop user, without sudo (it installs into your home directory)"
if [ -n "$PORT" ] && [ "$PORT" != realvnc ]; then
  [[ $PORT =~ ^[0-9]{1,5}$ ]] && (( 10#$PORT >= 1 && 10#$PORT <= 65535 )) || die "--port: '$PORT' is not a TCP port (1-65535) or 'realvnc'"
fi
case $ENGINE in ""|auto|builtin|xvnc|xvfb|xwayland) ;; *) die "--engine: '$ENGINE' is not auto, builtin, xvnc, xvfb or xwayland";; esac
case $SERVICE in auto|realvnc|display) ;; *) die "--service: '$SERVICE' is not realvnc or display";; esac
[ -n "${HOME:-}" ] && [ -d "$HOME" ] || die "HOME is not set"

# ------------------------------------------------------------- what to install
if [ -f "$HERE/xwlbridge-ctl" ] && [ -f "$HERE/xwlbridge-realvnc.service.in" ] && [ -f "$HERE/xwlbridge.service.in" ]; then
  SRC=$HERE                                   # bundle, or the source tree's packaging/
else
  die "xwlbridge-ctl / xwlbridge-realvnc.service.in / xwlbridge.service.in not found next to install.sh"
fi
if [ -z "$BIN" ]; then
  for b in "$SRC/xwlbridge" "$SRC/../target/dist/xwlbridge" "$SRC/../target/release/xwlbridge"; do
    [ -x "$b" ] && { BIN=$b; break; }
  done
fi
[ -n "$BIN" ] && [ -x "$BIN" ] || die "no xwlbridge binary (build it: cargo build --profile dist --features gnome,xvfb, or pass --bin PATH)"
BIN=$(readlink -f -- "$BIN")

if [ -f "$SRC/VERSION" ]; then VERSION=$(cat "$SRC/VERSION")
elif [ -f "$SRC/../Cargo.toml" ]; then VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$SRC/../Cargo.toml" | head -1)
else VERSION=unknown; fi

# the binary must run here and have the GNOME backend
missing=$(ldd "$BIN" 2>/dev/null | awk '/not found/ {print $1}' | tr '\n' ' ' || true)   # ldd fails on a static binary
[ -z "$missing" ] || die "$BIN needs libraries this system lacks: $missing(Ubuntu: sudo apt install libpipewire-0.3-0t64)"
help=$("$BIN" -help 2>&1) || true
grep -q -- '-backend.*gnome' <<<"$help" || die "$BIN was built without the GNOME backend (cargo build --features gnome,xvfb)"
grep -q -- 'xvnc' <<<"$help" || warn "$BIN was built without the stock engines (feature xvfb): only engine=builtin works"
bver=$("$BIN" -version 2>&1 | head -1)

CONF_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
BINDIR=$PREFIX/bin
SHARE=$PREFIX/share/xwlbridge
UNITDIR=$CONF_HOME/systemd/user
UNIT=$UNITDIR/xwlbridge-realvnc.service
UNIT_DISPLAY=$UNITDIR/xwlbridge.service
CFGDIR=$CONF_HOME/xwlbridge
CFG=$CFGDIR/xwlbridge.conf
MANIFEST=$SHARE/manifest
APPDIR=${XDG_DATA_HOME:-$HOME/.local/share}/applications

UPGRADE=0; OLD_VERSION=""
if [ -f "$MANIFEST" ]; then UPGRADE=1; OLD_VERSION=$(sed -n 's/^version //p' "$MANIFEST" | head -1); fi
say "$([ $UPGRADE = 1 ] && echo "Upgrading xwlbridge ${OLD_VERSION:-?} -> $VERSION" || echo "Installing xwlbridge $VERSION") for $(id -un) ($bver)"

# directories this install creates (uninstall.sh removes them again if empty);
# on an upgrade the first install's list carries over
CREATED_DIRS=()
[ $UPGRADE = 1 ] && mapfile -t CREATED_DIRS < <(sed -n 's/^dir //p' "$MANIFEST")
for d in "$PREFIX" "$BINDIR" "$PREFIX/share" "$SHARE" "$CONF_HOME" "$CONF_HOME/systemd" "$UNITDIR" \
         "$UNITDIR/graphical-session.target.wants" "$CFGDIR" "${APPDIR%/applications}" "$APPDIR"; do
  [ -e "$d" ] || CREATED_DIRS+=("$d")
done
[ $UPGRADE = 1 ] || CREATED_DIRS+=("$SHARE")

NEW_FILES=()
put() {  # put MODE SRC DEST [sed-expr]: atomic replace
  local mode=$1 src=$2 dest=$3 expr=${4:-} tmp
  mkdir -p -- "${dest%/*}"
  tmp=$(mktemp "${dest%/*}/.${dest##*/}.XXXXXX")
  if [ -n "$expr" ]; then sed -e "$expr" -- "$src" >"$tmp"; else cat -- "$src" >"$tmp"; fi
  chmod "$mode" "$tmp"
  mv -f -- "$tmp" "$dest"
  NEW_FILES+=("$dest")
}

# remember RealVNC's per-user files from before the first install, so that
# uninstall.sh --purge removes only what appeared since
if [ $UPGRADE = 0 ]; then
  mkdir -p -- "$SHARE"
  { [ -d "$HOME/.vnc" ] && find "$HOME/.vnc" -mindepth 1 -printf '%p\n' 2>/dev/null; true; } | LC_ALL=C sort >"$SHARE/vnc-before.list"
fi

esc() { printf '%s' "$1" | sed 's/[\/&|]/\\&/g'; }
put 0755 "$BIN" "$BINDIR/xwlbridge"
put 0755 "$SRC/xwlbridge-ctl" "$BINDIR/xwlbridge-ctl" "s|@VERSION@|$(esc "$VERSION")|g"
put 0644 "$SRC/xwlbridge.conf" "$SHARE/xwlbridge.conf"
put 0755 "$SRC/uninstall.sh" "$SHARE/uninstall.sh"
for f in README.md LICENSE; do
  for d in "$SRC" "$SRC/.."; do [ -f "$d/$f" ] && { put 0644 "$d/$f" "$SHARE/$f"; break; }; done
done
put 0644 "$SRC/xwlbridge-realvnc.service.in" "$UNIT" "s|@BINDIR@|$(esc "$BINDIR")|g"
put 0644 "$SRC/xwlbridge.service.in" "$UNIT_DISPLAY" "s|@BINDIR@|$(esc "$BINDIR")|g"
# an app-grid entry for RealVNC's status window on this desktop (RealVNC's own visible "VNC Server" entry
# starts its service-mode server, which cannot share a Wayland desktop); RealVNC's icon when installed
ICON=preferences-desktop-remote-desktop
[ -f /usr/share/icons/hicolor/48x48/apps/vncserver48x48.png ] && ICON=/usr/share/icons/hicolor/48x48/apps/vncserver48x48.png
put 0644 "$SRC/xwlbridge-status.desktop.in" "$APPDIR/xwlbridge-status.desktop" "s|@BINDIR@|$(esc "$BINDIR")|g; s|@ICON@|$(esc "$ICON")|g"
say "installed $BINDIR/xwlbridge, $BINDIR/xwlbridge-ctl, $UNIT, $UNIT_DISPLAY, $APPDIR/xwlbridge-status.desktop"

# ------------------------------------------------------------- an xwlrvnc install (the name before 0.1.0)
MIGRATED=0
if [ ! -f "$CFG" ] && { [ -f "$CONF_HOME/xwlrvnc/xwlrvnc.conf" ] || [ -f "$UNITDIR/xwlrvnc.service" ]; }; then
  say "found an xwlrvnc install: taking it over (xwlbridge-ctl migrate)"
  systemctl --user daemon-reload 2>/dev/null || true
  if "$BINDIR/xwlbridge-ctl" migrate --no-start; then MIGRATED=1; else warn "xwlbridge-ctl migrate failed; continuing with a fresh install"; fi
  [ "$SERVICE" = auto ] && SERVICE=realvnc   # xwlrvnc.service ran RealVNC
fi
WANTS=$UNITDIR/graphical-session.target.wants
if [ "$SERVICE" = auto ] && [ $UPGRADE = 1 ]; then   # an upgrade keeps the service the user had enabled
  if [ -e "$WANTS/xwlbridge-realvnc.service" ]; then SERVICE=realvnc
  elif [ -e "$WANTS/xwlbridge.service" ]; then SERVICE=display; fi
fi
if [ "$SERVICE" = auto ]; then
  if command -v vncserver-x11 >/dev/null 2>&1; then SERVICE=realvnc; else SERVICE=display; fi
fi
if [ "$SERVICE" = realvnc ]; then SVC=xwlbridge-realvnc.service DFLAG=""; else SVC=xwlbridge.service DFLAG=--display; fi
say "service: $SVC$([ "$SERVICE" = display ] && echo ' (the bridge display alone; RealVNC Server not found or --service display)')"

# ------------------------------------------------------------- settings
port_taken() {  # port_taken N: RealVNC's system service (5900) or any listener
  { [ "$1" = 5900 ] && [ "$(systemctl is-active vncserver-x11-serviced.service 2>/dev/null)" = active ]; } || \
    ss -Hltn "sport = :$1" 2>/dev/null | grep -q .
}
port_user() {  # who listens on port N, if visible: "name pid N"
  ss -Hltnp "sport = :$1" 2>/dev/null | grep -oE 'users:\(\("[^"]+",pid=[0-9]+' | head -1 | sed -E 's/users:\(\("([^"]+)",pid=/\1 pid /'
}
CREATED_CFG=0
if [ ! -f "$CFG" ]; then
  RVPORT=$("$BINDIR/xwlbridge-ctl" realvnc-get RfbPort 2>/dev/null || true)
  if [ -z "$PORT" ] && [ -n "$RVPORT" ]; then
    PORT=realvnc
    say "RealVNC's own settings set RfbPort=$RVPORT: the port is left to them (rfb_port=realvnc)"
  fi
  if [ "$PORT" = realvnc ]; then
    if port_taken "${RVPORT:-5900}"; then
      who=$(port_user "${RVPORT:-5900}")
      warn "port ${RVPORT:-5900} (RealVNC's RfbPort${RVPORT:+ setting}) is in use now${who:+ ($who)}: the service can serve it only once that has stopped; see xwlbridge-ctl doctor"
    fi
  elif [ -z "$PORT" ]; then
    PORT=5900
    if port_taken 5900; then
      PORT=5901
      warn "port 5900 is taken (RealVNC's system service vncserver-x11-serviced?): this desktop will be served on port 5901"
      warn "  to use 5900 instead: sudo systemctl disable --now vncserver-x11-serviced && xwlbridge-ctl config set rfb_port 5900 && xwlbridge-ctl restart"
    fi
  fi
  ( umask 077; mkdir -p -- "$CFGDIR"; cp -- "$SRC/xwlbridge.conf" "$CFG" )
  "$BINDIR/xwlbridge-ctl" config set rfb_port "$PORT" 2>/dev/null
  [ -n "$ENGINE" ] && "$BINDIR/xwlbridge-ctl" config set engine "$ENGINE" 2>/dev/null
  CREATED_CFG=1
  say "settings: $CFG (rfb_port=$PORT${ENGINE:+, engine=$ENGINE})"
else
  [ -z "$PORT$ENGINE" ] || warn "keeping your settings in $CFG (--port/--engine apply to a new file only; use xwlbridge-ctl config set)"
  say "settings kept: $CFG"
  if ! "$BINDIR/xwlbridge-ctl" config show >/dev/null 2>&1; then
    warn "your settings have errors: xwlbridge-ctl config show"
  fi
fi

# ------------------------------------------------------------- manifest
OLD_FILES=()
[ $UPGRADE = 1 ] && mapfile -t OLD_FILES < <(sed -n 's/^file //p' "$MANIFEST")
{
  echo "# xwlbridge install manifest: what uninstall.sh removes"
  echo "version $VERSION"
  echo "prefix $PREFIX"
  echo "installed $(date -u +%FT%TZ)"
  for f in "${NEW_FILES[@]}"; do echo "file $f"; done
  echo "file $SHARE/vnc-before.list"
  if [ $CREATED_CFG = 1 ] || grep -q '^config ' "$MANIFEST" 2>/dev/null; then echo "config $CFG"; fi
  printf '%s\n' "${CREATED_DIRS[@]}" | LC_ALL=C sort -u | sed 's/^/dir /'
} >"$MANIFEST.new"
echo "file $MANIFEST" >>"$MANIFEST.new"
mv -f -- "$MANIFEST.new" "$MANIFEST"
# files a previous version installed that this one no longer has
for f in "${OLD_FILES[@]}"; do
  grep -qxF "file $f" "$MANIFEST" || { rm -f -- "$f"; say "removed obsolete $f"; }
done

# ------------------------------------------------------------- service
if ! systemctl --user show-environment >/dev/null 2>&1; then
  warn "no systemd user manager reachable: the service is installed but not enabled; run 'xwlbridge-ctl ${DFLAG:+$DFLAG }enable' from your session"
  exit 0
fi
systemctl --user daemon-reload
if [ $ENABLE = 1 ]; then
  systemctl --user enable "$SVC" >/dev/null 2>&1 && say "enabled $SVC: starts with your GNOME session"
fi
if [ $START = 1 ]; then
  # right after login GNOME may still be bringing graphical-session.target up; the service (Requisite= on it)
  # can only start once it is active, and enabling it now does not add it to the transaction already running
  gs=$(systemctl --user is-active graphical-session.target 2>/dev/null || true)
  uenv=$(systemctl --user show-environment 2>/dev/null || true)
  if [ "$gs" != active ] && grep -q '^WAYLAND_DISPLAY=' <<<"$uenv"; then
    say "waiting for the GNOME session to finish starting ($gs)..."
    for _ in $(seq 90); do
      gs=$(systemctl --user is-active graphical-session.target 2>/dev/null || true)
      [ "$gs" = active ] && break
      sleep 1
    done
  fi
  if [ "$gs" = active ]; then
    systemctl --user reset-failed "$SVC" 2>/dev/null || true
    if systemctl --user restart "$SVC"; then
      sleep 3
      st=$(systemctl --user is-active "$SVC" || true)
      say "service $SVC: $st"
    else
      warn "the service did not start: xwlbridge-ctl $DFLAG logs"
    fi
  else
    say "no GNOME session running now: the service starts with your next login"
  fi
fi

echo
"$BINDIR/xwlbridge-ctl" $DFLAG doctor || true
echo
case ":$PATH:" in *":$BINDIR:"*) ;; *) echo "Note: $BINDIR is not on your PATH yet (it is after your next login on Ubuntu)";; esac
echo "Manage:    xwlbridge-ctl ${DFLAG:+$DFLAG }status | logs | restart | doctor | config set KEY VALUE"
[ "$SERVICE" = display ] && echo "Display:   xwlbridge-ctl --display show; run X11 tools with: xwlbridge run -- COMMAND"
echo "Uninstall: $SHARE/uninstall.sh"
