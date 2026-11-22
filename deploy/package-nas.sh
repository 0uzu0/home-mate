#!/usr/bin/env bash
# ARCHIVE builder for the old NAS tarball. Do not install its systemd unit.
# The current setup is one core per speaker (deploy/README.md, deploy/speaker/).
# This script still packages the same files as before, including
# deploy/home-mate.service. That unit is not installable: HM_GATEWAY_BIND is
# a LAN address and the current core refuses it. deploy/README.md ships in
# the tarball with the NAS section marked archive and with no install commands.
# Usage: ./deploy/package-nas.sh
# Produces target/dist/home-mate-nas-<version>-x86_64.tar.gz containing:
#   home-mate/bin/hm-core        — cross-compiled via cargo-zigbuild (glibc 2.17+)
#   home-mate/web/dist/...       — built Vue console (served by the core :8930)
#   home-mate/deploy/...         — archived unit + env template + deploy/README.md
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(git describe --tags --always --dirty)"
OUT="target/dist/home-mate-nas-${VERSION}-x86_64"
STAGE="target/dist/stage-${VERSION}"

echo "==> cross-compile hm-core (x86_64-unknown-linux-gnu, glibc 2.17)"
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.17 -p hm-core

echo "==> build console"
(cd web && npm run build)

echo "==> stage ${OUT}"
rm -rf "${STAGE}"
mkdir -p "${STAGE}/home-mate/bin" "${STAGE}/home-mate/data" "${STAGE}/home-mate/web" "${STAGE}/home-mate/deploy"
cp "target/x86_64-unknown-linux-gnu/release/hm-core" "${STAGE}/home-mate/bin/"
cp -R web/dist "${STAGE}/home-mate/web/"
cp deploy/home-mate.service deploy/env.local.example deploy/README.md "${STAGE}/home-mate/deploy/"

tar -C "${STAGE}" -czf "${OUT}.tar.gz" home-mate
rm -rf "${STAGE}"
echo "==> done: ${OUT}.tar.gz ($(du -h "${OUT}.tar.gz" | cut -f1))"
