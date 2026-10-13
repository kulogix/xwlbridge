#!/bin/bash
# Settings carry-over: which of GNOME's high contrast, text scaling, cursor size and reduced motion reach
# each toolkit (a) on GNOME's Xwayland with gsd-xsettings (reference, G), (b) on the engine before the mirror (E0),
# (c) on the engine with x11-a11y-proxy's settings mirror (XSETTINGS + RESOURCE_MANAGER; E). Phase 0 = defaults,
# phase 1 = high-contrast, text-scaling-factor 1.5, cursor-size 48, reduced-motion 'reduce' (GNOME 50's switch).
# Phase-0 apps keep running across the change ("live"); phase-1 apps start after it ("fresh").
OUT=${OUT:-/w/out/a11y-proxy/settings}; rm -rf "$OUT"; ORCA=0; source /w/tools/x11-a11y-proxy/tests/lib.sh
source $T/tests/apps.sh
export ELECTRON_ENABLE_LOGGING=1
gsettings set org.gnome.desktop.interface enable-animations true   # the session script turned it off
env -u WAYLAND_DISPLAY DISPLAY=:0 xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1; sleep 2
XW=$(grep -oE "public X11 display :[0-9]+" /tmp/shell.log | grep -oE ":[0-9]+" | head -1); XW=${XW:-:0}
XA=$(ls $XDG_RUNTIME_DIR/.mutter-Xwaylandauth.* 2>/dev/null | head -1)
G() { env -u WAYLAND_DISPLAY DISPLAY=$XW XAUTHORITY=$XA XDG_SESSION_TYPE=x11 QT_QPA_PLATFORM=xcb GDK_BACKEND=x11 "$@"; }
env DISPLAY=$XW XAUTHORITY=$XA GDK_BACKEND=x11 /usr/libexec/gsd-xsettings >"$OUT/gsd-xsettings.log" 2>&1 &
sleep 3
KS=${KINDS:-gtk3 gtk4 qt5 qt6 java electron tk motif xcalc}
start_apps() {  # $1 = display wrapper (G|E), $2 = tag
  local D=$1 tag=$2
  for K in $KS; do
    app_cmd $K $tag
    [ $K = motif ] && CMD="/tmp/probes/motif_probe Motif_probe_$tag"
    $D $CMD >"$OUT/app_${K}_$tag.log" 2>&1 &
  done
}
snap() {  # $1 = display wrapper, $2 = tag (apps), $3 = label
  local D=$1 tag=$2 lab=$3
  for K in $KS; do
    app_cmd $K $tag
    [ $K = xcalc ] && TITLE=Calculator
    local id; id=$($D xdotool search --name "^$TITLE\$" 2>/dev/null | tail -1)
    [ -n "$id" ] && $D import -window "$id" "$OUT/shot_${K}_${lab}.png" 2>/dev/null
  done
  $D python3 /w/tools/x11-a11y-proxy/tests/bed/xsettings_dump.py >"$OUT/xs_${lab}.json" 2>&1
  $D xprop -root RESOURCE_MANAGER >"$OUT/rm_${lab}.txt" 2>&1
}
# E0: the engine before the mirror (xcalc twice would collide on the title: E0 is run, snapped and closed first)
start_apps E E0; sleep 16; snap E E0 E0
for K in $KS; do pkill -f "probe.*E0\b|--tag=E0|Motif_probe_E0"; done; pkill -x xcalc; sleep 2
GDK_BACKEND=wayland $T/x11-a11y-proxy --display :20 --settings-only --log "$OUT/mirror.log" >/dev/null 2>&1 &
MIRROR=$!; sleep 3
start_apps G G0; start_apps E E0m; sleep 16
snap G G0 G_p0; snap E E0m E_p0
gsettings set org.gnome.desktop.a11y.interface high-contrast true
gsettings set org.gnome.desktop.interface text-scaling-factor 1.5
gsettings set org.gnome.desktop.interface cursor-size 48
gsettings set org.gnome.desktop.a11y.interface reduced-motion reduce
sleep 10
snap G G0 G_p0live; snap E E0m E_p0live
pkill -x xcalc; sleep 1
start_apps G G1; start_apps E E1; sleep 16
snap G G1 G_p1; snap E E1 E_p1
gsettings list-recursively org.gnome.desktop.a11y.interface >"$OUT/gsettings_p1.txt"
gsettings list-recursively org.gnome.desktop.interface | grep -E "scaling|cursor|animations|gtk-theme" >>"$OUT/gsettings_p1.txt"
gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop \
  -m org.freedesktop.portal.Settings.ReadAll "['org.freedesktop.appearance','org.gnome.desktop.a11y.interface']" >"$OUT/portal_p1.txt" 2>&1
kill $MIRROR; sleep 2
E python3 /w/tools/x11-a11y-proxy/tests/bed/xsettings_dump.py >"$OUT/xs_E_after_mirror_exit.json" 2>&1
E xprop -root RESOURCE_MANAGER >"$OUT/rm_E_after_mirror_exit.txt" 2>&1
finish
echo SETTINGS_DONE
