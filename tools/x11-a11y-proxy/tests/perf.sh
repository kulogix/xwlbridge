#!/bin/bash
# Cost of the proxy: RSS and CPU of the proxy and its reader with an idle GTK 3 probe, and with a 640x480 window
# repainted in full 30 times a second (frames the proxy shows are counted from its log).
OUT=${OUT:-/w/out/a11y-proxy/perf}; rm -rf "$OUT"; ORCA=0; source /w/tools/x11-a11y-proxy/tests/lib.sh
measure() {  # $1 label, $2 seconds
  local pids; pids=$(pgrep -f "x11_a11y_proxy" | tr '\n' ',' | sed 's/,$//')
  local t0; t0=$(for p in ${pids//,/ }; do awk '{print $14+$15}' /proc/$p/stat; done | paste -sd+ | bc)
  sleep $2
  local t1; t1=$(for p in ${pids//,/ }; do awk '{print $14+$15}' /proc/$p/stat; done | paste -sd+ | bc)
  local hz; hz=$(getconf CLK_TCK)
  echo "$1: cpu $(echo "scale=1; ($t1-$t0)*100/$hz/$2" | bc)% of one core over $2 s; rss $(ps -o rss= -p $pids | paste -sd+ | bc) KiB ($(ps -o pid=,rss=,args= -p $pids | cut -c1-90 | tr '\n' ';'))" >>$OUT/perf.txt
}
proxy_start idle python3 $TA/gtk3_probe.py P; sleep 12
measure "idle GTK3 probe" 20
kill $PROXY; sleep 2
proxy_start anim python3 $TA/anim_probe.py P 640 480 30 60; sleep 8
grep "frames shown" $OUT/proxy_anim.log | tail -1 >>$OUT/perf.txt; measure "640x480 at 30 fps" 20; grep "frames shown" $OUT/proxy_anim.log | tail -1 >>$OUT/perf.txt
grep FRAMES $OUT/app_anim.log | tail -1 >>$OUT/perf.txt
kill $PROXY; sleep 2
finish
cat $OUT/perf.txt
