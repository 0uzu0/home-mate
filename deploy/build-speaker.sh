#!/usr/bin/env bash
# Stage-C speaker build: static aarch64 musl hm-core + console dist.
# Usage: deploy/build-speaker.sh [staging-dir]   (default /tmp/hm-speaker)
#
# Layout produced (staging-dir/speaker/):
#   bin/core_a            — the binary (core_b/current are set at install)
#   web/                  — console dist (HM_WEB_DIR)
#   hm-core-supervisor.sh — service supervisor (same primitives as audio fw)
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=aarch64-unknown-linux-musl
OUT="${1:-/tmp/hm-speaker}/speaker"

echo "==> ./tools/build_core.sh"
./tools/build_core.sh

echo "==> console dist (web/)"
(cd web && npm run build >/dev/null)

rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/web"
cp "target/$TARGET/release/hm-core" "$OUT/bin/core_a"
cp -r web/dist/* "$OUT/web/"
cp deploy/speaker/hm-core-supervisor.sh "$OUT/"

BIN_SHA=$(shasum -a 256 "$OUT/bin/core_a" | cut -c1-8)
echo "==> staged $OUT (binary sha256 prefix: $BIN_SHA)"
file "$OUT/bin/core_a"
