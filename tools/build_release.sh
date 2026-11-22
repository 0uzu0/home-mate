#!/bin/sh
# Build the on-device release tool (aarch64 musl) — the ONE supported way.
#
# `hm-release` is bootstrap infrastructure, not a release component: it is
# installed once by hand (deploy/release/README.md) and carries the Ed25519
# public key. It has no 16 MiB body gate, but it must be a static aarch64
# binary built with rust-lld, same as hm-core: Zig's musl allocator is the
# known RSS leaker on this kernel and there is no reason to reintroduce it.
set -eu

TARGET=aarch64-unknown-linux-musl
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"

SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | sed -n 's/^host: //p')
LINKER="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
if [ ! -x "$LINKER" ]; then
    echo "build_release: rust-lld not found at $LINKER (rustup component missing?)" >&2
    exit 1
fi

CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$LINKER"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER

RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols"
export RUSTFLAGS

cargo zigbuild --target "$TARGET" --release -p hm-release

BIN="target/$TARGET/release/hm-release"
python3 -I - "$BIN" <<'PY'
import os, subprocess, sys
bin_path = sys.argv[1]
head = subprocess.run(["head", "-c", "20", bin_path], capture_output=True).stdout
if head[4] != 2 or head[18] != 183:  # ELFCLASS64, EM_AARCH64
    sys.exit(f"{bin_path}: expected ELF64 aarch64")
strings = subprocess.run(["strings", "-a", bin_path], capture_output=True, text=True).stdout
if "Homebrew LLD" in strings or "GNU ld" in strings:
    sys.exit(f"{bin_path}: final link is NOT rust-lld (Zig/Homebrew LLD ran last)")
if "rust-lang/llvm-project" not in strings:
    sys.exit(f"{bin_path}: rust-lld marker missing")
size = os.path.getsize(bin_path)
if size > 12 * 1024 * 1024:
    sys.exit(f"{bin_path}: {size} bytes is larger than the 12 MB bootstrap cap")
print(f"PASS  elf64-aarch64-static + rust-lld-final-link + {size} bytes")
PY
echo "build_release: $BIN"
