#!/bin/bash
# Inferred mode with pixels: the uidet sidecar (OCR + icon_detect) serves the proxy over two FIFOs in $FIFO (the
# sidecar runs in its own container, started by the host with stdin/stdout on those FIFOs). Apps: xcalc (editres +
# resources, the LCD read live by OCR), a self-drawn Xlib window (detector + OCR only), Tk with its send
# introspection switched off (window tree + OCR). Orca on GNOME; the person's keys through mutter.
OUT=${OUT:-/w/out/a11y-proxy/inferred}; source /w/tools/x11-a11y-proxy/tests/lib.sh
source $T/tests/apps.sh
# hold both FIFOs open for the whole run, so the sidecar outlives each proxy (one proxy per app below)
exec 3>"$FIFO/req" 4<"$FIFO/resp"
# wait for the sidecar to have loaded its model (its first answer)
echo '{"jsonrpc":"2.0","id":0,"method":"ping","params":{}}' >&3; read -t 90 WARM <&4; echo "uidet warm: $WARM" >"$OUT/uidet_warm.txt"
PROXY_FLAGS="--uidet-fifo $FIFO"
run() {  # $1 tag, $2.. command
  local tag=$1; shift
  mark "$tag start"; proxy_start $tag "$@"; sleep 16
  python3 $T/tests/treedump.py x11-a11y-proxy >"$OUT/dump_$tag.json" 2>/dev/null
}
want() { [ -z "${ONLY:-}" ] || [[ " $ONLY " == *" $1 "* ]]; }
# xcalc
if want xcalc; then
run xcalc xcalc
mark "xcalc tab x3"; GI key 15 sleep 1.5 key 15 sleep 1.5 key 15 sleep 2
mark "xcalc type 7"; GI key 8 sleep 3
mark "xcalc type +2="; GI down 42 key 13 up 42 sleep 1.5 key 3 sleep 1.5 key 13 sleep 3
mark "xcalc where-am-i"; GI key 96 sleep 2.5
mark "xcalc end"; kill $PROXY; sleep 2; pkill -x xcalc; sleep 1
fi
# self-drawn
if want canvas; then
run canvas python3 $TA/canvas_probe.py C
mark "canvas tab x2"; GI key 15 sleep 1.5 key 15 sleep 2
mark "canvas space"; GI key 57 sleep 2
mark "canvas where-am-i"; GI key 96 sleep 2.5
mark "canvas end"; kill $PROXY; sleep 2; pkill -f canvas_probe; sleep 1
fi
# Tk without its send introspection
if want tk; then
export X11_A11Y_PROXY_NO_TKSEND=1
run tk python3 $TA/tk_probe.py N
mark "tk tab x2"; GI key 15 sleep 1.5 key 15 sleep 2
mark "tk space"; GI key 57 sleep 2
mark "tk end"; kill $PROXY; sleep 2
unset X11_A11Y_PROXY_NO_TKSEND
fi
finish
echo INFERRED_DONE
