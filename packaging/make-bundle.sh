#!/bin/bash
# Makes the release bundle users install from: xwlbridge-<version>-<arch>.tar.gz
# holding the binary, xwlbridge-ctl, the services, the default settings and
# install.sh/uninstall.sh.
#
#   packaging/make-bundle.sh [--bin PATH] [--out DIR] [--suffix TEXT]
#
# Default binary: target/dist/xwlbridge (cargo build --profile dist --features gnome,xvfb).
# The binary must be built on (or for) the oldest distro it should run on: glibc
# symbol versions are forward compatible only.
# SPDX-License-Identifier: MIT
set -euo pipefail
umask 022
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
BIN=$ROOT/target/dist/xwlbridge OUT=$ROOT/target/packaging SUFFIX=""
while [ $# -gt 0 ]; do
  case $1 in
    --bin) BIN=$2; shift 2;;
    --out) OUT=$2; shift 2;;
    --suffix) SUFFIX=$2; shift 2;;
    *) echo "make-bundle.sh: unknown option $1" >&2; exit 2;;
  esac
done
[ -x "$BIN" ] || { echo "make-bundle.sh: no binary at $BIN" >&2; exit 1; }
"$BIN" -help 2>&1 | grep -q -- '-backend.*gnome' || { echo "make-bundle.sh: $BIN lacks the gnome feature" >&2; exit 1; }
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
if git -C "$ROOT" rev-parse --short HEAD >/dev/null 2>&1; then
  VERSION="$VERSION+g$(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet HEAD -- . 2>/dev/null || echo .dirty)"
fi
ARCH=$(uname -m)
NAME=xwlbridge-${VERSION}-${ARCH}${SUFFIX:+-$SUFFIX}
STAGE=$(mktemp -d); trap 'rm -rf "$STAGE"' EXIT
D=$STAGE/$NAME
mkdir -p "$D"
install -m 0755 "$BIN" "$D/xwlbridge"
install -m 0755 "$HERE/install.sh" "$HERE/uninstall.sh" "$D/"
# the bundle's own xwlbridge-ctl knows its version too (its doctor runs before installing)
sed "s|@VERSION@|$VERSION|g" "$HERE/xwlbridge-ctl" >"$D/xwlbridge-ctl"; chmod 0755 "$D/xwlbridge-ctl"
install -m 0644 "$HERE/xwlbridge-realvnc.service.in" "$HERE/xwlbridge.service.in" "$HERE/xwlbridge-status.desktop.in" "$HERE/xwlbridge.conf" "$HERE/README.md" "$ROOT/LICENSE" "$D/"
printf '%s\n' "$VERSION" >"$D/VERSION"
( cd "$D" && sha256sum xwlbridge xwlbridge-ctl install.sh uninstall.sh xwlbridge-realvnc.service.in xwlbridge.service.in xwlbridge-status.desktop.in xwlbridge.conf README.md LICENSE VERSION >SHA256SUMS )
mkdir -p "$OUT"
tar -C "$STAGE" --owner=0 --group=0 --numeric-owner --sort=name -czf "$OUT/$NAME.tar.gz" "$NAME"
( cd "$OUT" && sha256sum "$NAME.tar.gz" >"$NAME.tar.gz.sha256" )
echo "$OUT/$NAME.tar.gz"
