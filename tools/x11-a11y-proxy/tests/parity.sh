#!/bin/bash
# Accessibility parity: the same probe app, the same keys (the person's, through mutter RemoteDesktop), the same
# Orca 50 -- arm X: the app on GNOME's Xwayland (what users get today; the parity reference available without a
# VM), arm P: the app fully on the bridge's engine display (:20) behind x11-a11y-proxy. Records per step: Orca's
# speech (Speech Dispatcher logger), the app's effects (stdout), focus events on both buses, tree dumps.
# Env: OUT, KINDS="gtk3 gtk4 qt5 qt6 java electron tk motif xcalc", ARMS="X P".
OUT=${OUT:-/w/out/a11y-proxy/parity}; rm -rf "$OUT"; source /w/tools/x11-a11y-proxy/tests/lib.sh
source $T/tests/apps.sh
export ELECTRON_ENABLE_LOGGING=1
# GNOME's Xwayland (arm X), with gsd-xsettings as a GNOME session runs it
env -u WAYLAND_DISPLAY DISPLAY=:0 xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1; sleep 2
XW=$(grep -oE "public X11 display :[0-9]+" /tmp/shell.log | grep -oE ":[0-9]+" | head -1); XW=${XW:-:0}
XA=$(ls $XDG_RUNTIME_DIR/.mutter-Xwaylandauth.* 2>/dev/null | head -1)
G() { env -u WAYLAND_DISPLAY DISPLAY=$XW XAUTHORITY=$XA XDG_SESSION_TYPE=x11 QT_QPA_PLATFORM=xcb GDK_BACKEND=x11 \
      QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 \
      JAVA_TOOL_OPTIONS="-Djavax.accessibility.assistive_technologies=org.GNOME.Accessibility.AtkWrapper -Xbootclasspath/a:/usr/share/java/java-atk-wrapper.jar" "$@"; }
env DISPLAY=$XW XAUTHORITY=$XA GDK_BACKEND=x11 /usr/libexec/gsd-xsettings >"$OUT/gsd-xsettings.log" 2>&1 &
sleep 2
step() { mark "$ARM $K $1"; }
TSL() { python3 $T/tests/tsline.py "$OUT/app_$1.log" "$OUT/app_$1_ts.log"; }   # effects bound to steps (report.py)
api_steps() {  # $1 = window title, $2 = app selector for at_do (arm P: x11-a11y-proxy; arm X: title=...)
  local TITLE=$1 SEL=$2
  step "S1 tab x4"; GI key 15 sleep 1.5 key 15 sleep 1.5 key 15 sleep 1.5 key 15 sleep 1.8
  step "S2 alt+c"; GI down 56 key 46 up 56 sleep 2
  step "S3 tab, type 12"; GI key 15 sleep 1.5 key 2 sleep 0.8 key 3 sleep 1.8
  step "S4 where-am-i"; GI key 96 sleep 2.5
  step "S5 tab, right x2"; GI key 15 sleep 1.5 key 106 sleep 1.2 key 106 sleep 1.8
  step "S6 shift+tab x3"; GI down 42 key 15 up 42 sleep 1.5 down 42 key 15 up 42 sleep 1.5 down 42 key 15 up 42 sleep 1.8
  step "S7 space"; GI key 57 sleep 2
  step "S8 ctrl+k"; GI down 29 key 37 up 29 sleep 2
  step "S9 flat review KP_8"; GI key 72 sleep 2.5
  step "S10 AT action on Seven"; python3 $T/tests/at_do.py "$SEL" "push button" Seven action >>"$OUT/at_do.log" 2>&1
  python3 $T/tests/at_do.py "$SEL" button Seven action >>"$OUT/at_do.log" 2>&1; sleep 2
  step "S11 AT set slider value 75"; python3 $T/tests/at_do.py "$SEL" slider "$SLIDER" setvalue 75 >>"$OUT/at_do.log" 2>&1; sleep 2
  local XY; XY=$(python3 $T/tests/click_target.py "$TITLE" "check box" "Check me" 2>>"$OUT/click.log")
  echo "$ARM $K click target: $XY" >>"$OUT/click.log"
  step "S12 pointer click on Check me"
  [ "${XY%% *}" != NONE ] && [ -n "$XY" ] && GI move $(echo $XY | cut -d' ' -f1-2) sleep 0.5 click sleep 2
}
inferred_steps() {
  local TITLE=$1 SEL=$2
  step "S1 tab x3"; GI key 15 sleep 1.5 key 15 sleep 1.5 key 15 sleep 1.8
  step "S2 space"; GI key 57 sleep 2
  step "S3 where-am-i"; GI key 96 sleep 2.5
  step "S4 alt+s"; GI down 56 key 31 up 56 sleep 2
  step "S5 type 7"; GI key 8 sleep 2.5
  step "S6 AT action on Seven"; python3 $T/tests/at_do.py "$SEL" "push button" Seven action >>"$OUT/at_do.log" 2>&1
  python3 $T/tests/at_do.py "$SEL" button Seven action >>"$OUT/at_do.log" 2>&1; sleep 2
  step "S7 flat review KP_8"; GI key 72 sleep 2.5
}
menu_steps() {
  step "S1 alt+f"; GI down 56 key 33 up 56 sleep 2
  step "S2 down"; GI key 108 sleep 1.8
  step "S3 escape x2"; GI key 1 sleep 1.2 key 1 sleep 1.8
  step "S4 ctrl+s"; GI down 29 key 31 up 29 sleep 2
  step "S5 alt+f, down, return"; GI down 56 key 33 up 56 sleep 1.5 key 108 sleep 1.5 key 28 sleep 2
}
for K in ${KINDS:-gtk3 gtk4 qt5 qt6 java electron tk motif xcalc gtk3menu qt6menu}; do
  for ARM in ${ARMS:-X P}; do
    TAG=${K}_$ARM; app_cmd $K $ARM
    SLIDER=""; [ $K = electron ] && SLIDER=level
    step "S0 start"
    if [ $ARM = X ]; then
      G $CMD > >(TSL $TAG) 2>&1 & APID=$!
      SEL="title=$TITLE"
    else
      TS=1 proxy_start $TAG $CMD; APID=$PROXY
      SEL=x11-a11y-proxy
    fi
    sleep ${WAIT:-12}; [ $K = electron ] && sleep 4
    if [ $ARM = X ]; then python3 $T/tests/treedump.py >"$OUT/dump_$TAG.json" 2>/dev/null
    else
      python3 $T/tests/treedump.py x11-a11y-proxy >"$OUT/dump_${TAG}_proxy.json" 2>/dev/null
      AT_SPI_BUS_ADDRESS=$PRIV python3 $T/tests/treedump.py >"$OUT/dump_${TAG}_app.json" 2>/dev/null
    fi
    case $K in tk|motif|xcalc) inferred_steps "$TITLE" "$SEL";; *menu) menu_steps;; *) api_steps "$TITLE" "$SEL";; esac
    step "end"
    kill $APID 2>/dev/null; sleep 1
    # (arm X's app runs under a shell function: killing $APID leaves it running -- Electron's command line has no
    # "probe" in it, so it stayed on GNOME through arm P and every later kind until proxy_fix)
    pkill -f "probe.* $ARM\$|probe.*--tag=$ARM\$|^xcalc|motif_probe Motif_probe_$ARM|menu_probe.py" 2>/dev/null
    pkill -f -- "--user-data-dir=/tmp/el_$ARM " 2>/dev/null; sleep 2
  done
done
finish
echo PARITY_DONE
