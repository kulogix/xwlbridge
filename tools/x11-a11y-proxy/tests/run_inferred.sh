#!/bin/bash
# Host-side runner for tests/inferred.sh: two containers (the a11y session and the UI-detector sidecar) joined by
# FIFOs. The sidecar (OCR + an icon detector speaking JSON-RPC on stdin/stdout) is not part of this repository:
#   UIDET_IMAGE=<image> UIDET_MODELS=<dir with icon_detect_dyn.onnx> [BED_IMAGE=<test-bed image>] \
#     tests/run_inferred.sh OUTDIR_UNDER_REPO (e.g. out/a11y-proxy/inferred)
# Without UIDET_IMAGE / UIDET_MODELS it prints SKIP and exits 0.
set -u
P=$(cd "$(dirname "$0")/../../.." && pwd)
if [ -z "${UIDET_IMAGE:-}" ] || [ -z "${UIDET_MODELS:-}" ]; then
  echo "SKIP: set UIDET_IMAGE and UIDET_MODELS to run the inferred-mode test"; exit 0
fi
OUTREL=$1; OUT=$P/$OUTREL; rm -rf "$OUT"; mkdir -p "$OUT/fifo"
mkfifo "$OUT/fifo/req" "$OUT/fifo/resp"
docker run --rm -i --name pb_uidet --network none --memory=2g --memory-swap=2g --cpus=4 -u "$(id -u)" \
  -v "$UIDET_MODELS":/models:ro "$UIDET_IMAGE" --model /models/icon_detect_dyn.onnx --threads 4 \
  <"$OUT/fifo/req" >"$OUT/fifo/resp" 2>"$OUT/uidet.stderr" &
UID_PID=$!
timeout 900 docker run --rm --name pb_inferred --network none --memory=8g -u "$(id -u)" -v "$P":/w \
  -e OUT=/w/$OUTREL -e FIFO=/w/$OUTREL/fifo -e ONLY="${ONLY:-}" "${BED_IMAGE:-xwlbridge-a11y-bed}" \
  dbus-run-session bash /w/tools/x11-a11y-proxy/tests/inferred.sh >"$OUT/console.txt" 2>&1
docker stop pb_uidet >/dev/null 2>&1; wait $UID_PID 2>/dev/null
rm -f "$OUT/fifo/req" "$OUT/fifo/resp"; rmdir "$OUT/fifo" 2>/dev/null
echo "done: $OUT"
