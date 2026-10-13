#!/bin/bash
# app_cmd KIND TAG -> sets CMD and TITLE for a probe (sourced after lib.sh)
mkdir -p /tmp/probes
[ -x /tmp/probes/motif_probe ] || cc -O2 -o /tmp/probes/motif_probe $TA/motif_probe.c -lXm -lXt -lX11 2>/tmp/probes/cc.log
app_cmd() {
  local k=$1 tag=$2
  case $k in
    gtk3) CMD="python3 $TA/gtk3_probe.py $tag"; TITLE="GTK3 probe $tag";;
    gtk4) CMD="python3 $TA/gtk4_probe.py $tag"; TITLE="GTK4 probe $tag";;
    qt5) CMD="python3 $TA/qt_probe.py 5 $tag"; TITLE="Qt5 probe $tag";;
    qt6) CMD="python3 $TA/qt_probe.py 6 $tag"; TITLE="Qt6 probe $tag";;
    java) CMD="java $TA/JavaProbe.java $tag"; TITLE="Java probe $tag";;
    electron) CMD="electron --no-sandbox --ozone-platform=x11 --user-data-dir=/tmp/el_$tag $TA/electron --tag=$tag"; TITLE="Electron probe $tag";;
    tk) CMD="python3 $TA/tk_probe.py $tag"; TITLE="Tk probe $tag";;
    motif) CMD="/tmp/probes/motif_probe Motif_probe_$tag"; TITLE="Motif_probe_$tag";;
    xcalc) CMD="xcalc"; TITLE="Calculator";;
    gtk3menu) CMD="python3 $TA/menu_probe.py gtk3 $tag"; TITLE="GTK3 menu probe $tag";;
    qt6menu) CMD="python3 $TA/menu_probe.py qt6 $tag"; TITLE="Qt6 menu probe $tag";;
    canvas) CMD="python3 $TA/canvas_probe.py $tag"; TITLE="Canvas probe $tag";;
  esac
}
