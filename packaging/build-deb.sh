#!/bin/bash
# Builds the optional Debian package with cargo-deb (cargo install cargo-deb):
#   packaging/build-deb.sh [cargo-deb options, e.g. --no-build]
# -> target/debian/xwlrvnc_<version>-1_<arch>.deb. Build it on the oldest release it should install on
# (glibc and libpipewire versions come from the build system via dpkg-shlibdeps).
# SPDX-License-Identifier: MIT
set -euo pipefail
umask 022
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
if git rev-parse --short HEAD >/dev/null 2>&1; then
  VERSION="$VERSION+g$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- . 2>/dev/null || echo .dirty)"
fi

# cargo-deb's asset paths are relative to the crate: the rendered files go in target/packaging even when
# CARGO_TARGET_DIR points elsewhere. Extra arguments go to cargo-deb (cargo arguments after --, e.g. -- --offline).
mkdir -p target/packaging
sed "s|@BINDIR@|/usr/bin|g" packaging/xwlrvnc.service.in >target/packaging/xwlrvnc.service
sed "s|@VERSION@|$VERSION|g" packaging/xwlrvnc-ctl >target/packaging/xwlrvnc-ctl
chmod 0755 target/packaging/xwlrvnc-ctl
sed "s|@BINDIR@|/usr/bin|g; s|@ICON@|preferences-desktop-remote-desktop|g" packaging/xwlrvnc-status.desktop.in >target/packaging/xwlrvnc-status.desktop
cargo deb --profile dist --features gnome,xvfb "$@"   # honours CARGO_TARGET_DIR
