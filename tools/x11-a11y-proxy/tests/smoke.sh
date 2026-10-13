#!/bin/bash
# Smoke test: toolkits through the proxy with Orca, one after another in one session. OUT, APPS="gtk3 qt6 ..."
OUT=${OUT:-/w/out/a11y-proxy/smoke}; rm -rf "$OUT"; source /w/tools/x11-a11y-proxy/tests/lib.sh
source $T/tests/apps.sh
export ELECTRON_ENABLE_LOGGING=1
for APP in ${APPS:-gtk3}; do
  app_cmd $APP S
  mark "start $APP"; proxy_start $APP $CMD; sleep ${WAIT:-14}
  python3 $T/tests/treedump.py x11-a11y-proxy >"$OUT/dump_proxy_$APP.json" 2>/dev/null
  [ -n "$PRIV" ] && AT_SPI_BUS_ADDRESS=$PRIV python3 $T/tests/treedump.py >"$OUT/dump_app_$APP.json" 2>/dev/null
  mark "$APP tab x4"; GI key 15 sleep 1.5 key 15 sleep 1.5 key 15 sleep 1.5 key 15 sleep 1.5
  mark "$APP alt+c"; GI down 56 key 46 up 56 sleep 2
  mark "$APP tab, type 12"; GI key 15 sleep 1.5 key 2 sleep 0.8 key 3 sleep 1.5
  mark "$APP where-am-i"; GI key 96 sleep 2.5
  mark "$APP end"; kill $PROXY; sleep 3; pkill -f "probe.*S\b|xcalc|motif_probe" ; sleep 1
done
finish
echo SMOKE_DONE
