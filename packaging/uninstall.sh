#!/bin/bash
# Removes what install.sh installed for the current user: stops and disables the
# services and the GNOME Shell extension, deletes the programs, the units, the
# extension, the settings and runtime files.
#
#   ~/.local/share/xwlbridge/uninstall.sh [--keep-config] [--purge] [--prefix DIR]
#
#   --keep-config  keep ~/.config/xwlbridge (settings)
#   --purge        also delete RealVNC's per-user files (~/.vnc) that appeared after
#                  the first install (keys, logs, the first-run answers); files that
#                  were there before are never touched
# SPDX-License-Identifier: MIT
set -uo pipefail

PREFIX="$HOME/.local" KEEP=0 PURGE=0
while [ $# -gt 0 ]; do
  case $1 in
    --keep-config) KEEP=1; shift;;
    --purge) PURGE=1; shift;;
    --prefix) PREFIX=$2; shift 2;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0;;
    *) echo "uninstall.sh: unknown option $1" >&2; exit 2;;
  esac
done
say() { printf '==> %s\n' "$*"; }
[ "$(id -u)" -ne 0 ] || { echo "uninstall.sh: run as the desktop user, without sudo" >&2; exit 1; }

CONF_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
SHARE=$PREFIX/share/xwlbridge
MANIFEST=$SHARE/manifest
UNITS="xwlbridge-realvnc.service xwlbridge.service"
[ -f "$MANIFEST" ] || { echo "uninstall.sh: no install found ($MANIFEST missing)" >&2; exit 1; }

# 1. the service
if systemctl --user show-environment >/dev/null 2>&1; then
  for UNIT in $UNITS; do
    systemctl --user disable --now "$UNIT" >/dev/null 2>&1 && say "stopped and disabled $UNIT"
    systemctl --user reset-failed "$UNIT" >/dev/null 2>&1 || true
  done
fi
# a wants-link left by a manual enable with another unit path
for UNIT in $UNITS; do
  rm -f -- "$CONF_HOME/systemd/user/graphical-session.target.wants/$UNIT" "$CONF_HOME/systemd/user/default.target.wants/$UNIT"
  rm -rf -- "$CONF_HOME/systemd/user/$UNIT.d"
done

# 1b. the GNOME Shell extension xwlbridge-windows@xwlbridge: disabled again if install.sh enabled it (its files
# go with the manifest below). A running GNOME stops it now; its entries leave enabled/disabled-extensions.
EXT_UUID=xwlbridge-windows@xwlbridge
gs_list() {  # gs_list KEY: org.gnome.shell's string list KEY, one item per line
  local v it
  v=$(timeout 5 gsettings get org.gnome.shell "$1" 2>/dev/null) || return 1
  case $v in *'"'*|*'\'*) return 1;; esac
  v=${v#@as }; v=${v#[}; v=${v%]}
  local IFS=,
  for it in $v; do
    it=${it#"${it%%[![:space:]]*}"}; it=${it%"${it##*[![:space:]]}"}; it=${it#\'}; it=${it%\'}
    [ -n "$it" ] && printf '%s\n' "$it"
  done
  return 0
}
gs_drop() {  # gs_drop KEY ITEM: removes ITEM from the list KEY
  local items out="" it
  items=$(gs_list "$1") || return 1
  grep -qxF -- "$2" <<<"$items" || return 0
  while IFS= read -r it; do [ -n "$it" ] && [ "$it" != "$2" ] && out+="${out:+, }'$it'"; done <<<"$items"
  timeout 5 gsettings set org.gnome.shell "$1" "[$out]" 2>/dev/null
}
if grep -qxF "extension-enabled $EXT_UUID" "$MANIFEST"; then
  timeout 5 gnome-extensions disable "$EXT_UUID" >/dev/null 2>&1
  if gs_drop enabled-extensions "$EXT_UUID" && gs_drop disabled-extensions "$EXT_UUID"; then
    say "disabled the GNOME Shell extension $EXT_UUID"
  else
    say "could not change GNOME's extension settings here; after this, run in your session: gnome-extensions disable $EXT_UUID"
  fi
fi

# 2. RealVNC's per-user files that appeared since the first install (--purge)
if [ $PURGE = 1 ] && [ -d "$HOME/.vnc" ] && [ ! -L "$HOME/.vnc" ] && [ ! -f "$SHARE/vnc-before.list" ]; then
  # without the list of what was there before the install, nothing in ~/.vnc can be told apart from the user's
  # own RealVNC settings, keys and password: keep everything
  say "kept ~/.vnc: $SHARE/vnc-before.list is missing, so files created since the install cannot be told apart"
elif [ $PURGE = 1 ] && [ -d "$HOME/.vnc" ] && [ ! -L "$HOME/.vnc" ]; then
  before=$SHARE/vnc-before.list
  vnc_real=$(realpath -- "$HOME/.vnc")
  n=0
  # NUL-separated, so a name with a newline is one name; find never follows a
  # link, and each candidate's directory must really be inside ~/.vnc (no link
  # in the path), so nothing outside it can be reached
  while IFS= read -r -d '' p; do
    case $p in "$HOME/.vnc/"*) ;; *) continue;; esac
    case $(realpath -- "$(dirname -- "$p")" 2>/dev/null) in "$vnc_real"|"$vnc_real/"*) ;; *) continue;; esac
    # (a name with a newline is never in the newline-separated list: kept as the user's if any part matches)
    grep -qxF -- "$p" "$before" && continue
    if [ -d "$p" ] && [ ! -L "$p" ]; then rmdir -- "$p" 2>/dev/null && n=$((n + 1))
    else rm -f -- "$p" && n=$((n + 1)); fi
  done < <(find "$HOME/.vnc" -mindepth 1 -depth -print0 2>/dev/null)
  if [ ! -s "$before" ] && [ -d "$HOME/.vnc" ]; then rmdir -- "$HOME/.vnc" 2>/dev/null; fi
  # RealVNC's empty runtime directories in /tmp, unless a RealVNC user-mode server of ours still runs
  if [ -d "/tmp/.vnc-$(id -u)" ] && ! pgrep -u "$(id -u)" -x vncserver-x11-c >/dev/null 2>&1; then
    find "/tmp/.vnc-$(id -u)" -depth -type d -empty -delete 2>/dev/null
  fi
  say "removed $n RealVNC per-user file(s) created since the install"
fi

# 3. files from the manifest (settings unless --keep-config)
mapfile -t FILES < <(sed -n 's/^file //p' "$MANIFEST")
mapfile -t CONFIGS < <(sed -n 's/^config //p' "$MANIFEST")
mapfile -t DIRS < <(sed -n 's/^dir //p' "$MANIFEST")
for f in "${FILES[@]}"; do [ -f "$f" ] || [ -L "$f" ] && rm -f -- "$f"; done
if [ $KEEP = 0 ]; then
  for f in "${CONFIGS[@]}"; do rm -f -- "$f" "$f".??????; done
  # settings files we did not create are left alone, but say so
  [ -f "$CONF_HOME/xwlbridge/xwlbridge.conf" ] && say "kept $CONF_HOME/xwlbridge/xwlbridge.conf (not created by the installer; delete it yourself)"
else
  say "kept settings in $CONF_HOME/xwlbridge"
fi
# directories the installer created, deepest first, only if empty now
while IFS= read -r d; do [ -d "$d" ] && rmdir -- "$d" 2>/dev/null; done < <(printf '%s\n' "${DIRS[@]}" | awk '{ print length($0) "\t" $0 }' | sort -rn | cut -f2-)
say "removed $(printf '%s\n' "${FILES[@]}" | grep -c .) installed file(s)"

# 4. runtime leftovers: the private engine PATH dir, the -uidisplay hook copies,
# stale stock-engine files (only ours: they carry our name and a dead pid)
rt=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
rm -rf -- "$rt/xwlbridge" "/tmp/xwlbridge-$(id -u)"
# The -uidisplay hook is one file per hook build, shared by every bridge of this
# user: a bridge still running (one started by hand, say) has RealVNC preload it
# into each UI process it starts, and without it the UI crash-loops on the
# built-in X server. So it goes only when no bridge runs (the next start writes
# it again anyway).
if pgrep -u "$(id -u)" -x xwlbridge >/dev/null 2>&1; then
  say "kept the -uidisplay hook in $rt: a bridge started by hand still runs and uses it"
else
  rm -f -- "$rt"/xwlbridge-uihook-*.so "$rt"/.xwlbridge-uihook-*.tmp "$HOME/.cache"/xwlbridge-uihook-*.so 2>/dev/null
  # newer bridges keep the hook in ~/.cache/xwlbridge when XDG_RUNTIME_DIR is not usable
  if [ -d "$HOME/.cache/xwlbridge" ] && [ ! -L "$HOME/.cache/xwlbridge" ]; then
    rm -f -- "$HOME/.cache/xwlbridge"/xwlbridge-uihook-*.so "$HOME/.cache/xwlbridge"/.xwlbridge-uihook-*.tmp 2>/dev/null
    rmdir -- "$HOME/.cache/xwlbridge" 2>/dev/null
  fi
fi
for s in "$rt"/xwlbridge-xvnc-*.sock; do
  [ -S "$s" ] || continue
  pid=${s##*xwlbridge-xvnc-}; pid=${pid%.sock}
  kill -0 "$pid" 2>/dev/null || rm -f -- "$s"
done
# a stock engine's private directory (authority file, Xvnc's socket) of a bridge
# that was killed outright: xwlbridge-<pid>-XXXXXX
for d in "$rt"/xwlbridge-[0-9]*-??????; do
  [ -d "$d" ] && [ ! -L "$d" ] || continue
  pid=${d##*/xwlbridge-}; pid=${pid%-*}
  case $pid in ''|*[!0-9]*) continue;; esac
  kill -0 "$pid" 2>/dev/null || rm -rf -- "$d"
done
# the display registry `xwlbridge run` reads: dead bridges' entries, and the directory once empty
if [ -d "$rt/xwlbridge-displays" ] && [ ! -L "$rt/xwlbridge-displays" ]; then
  for e in "$rt"/xwlbridge-displays/*; do
    [ -f "$e" ] || continue
    read -r pid _ <"$e" 2>/dev/null || pid=""
    case $pid in ''|*[!0-9]*) continue;; esac
    kill -0 "$pid" 2>/dev/null || rm -f -- "$e"
  done
  rmdir -- "$rt/xwlbridge-displays" 2>/dev/null || true
fi

if systemctl --user show-environment >/dev/null 2>&1; then systemctl --user daemon-reload; fi
say "xwlbridge is uninstalled. RealVNC Server itself and its settings (~/.vnc) are unchanged$([ $PURGE = 1 ] && echo ' apart from the purged files')."
