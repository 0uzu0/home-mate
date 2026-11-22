#!/bin/sh
# Build the on-device core (aarch64 musl) — the ONE supported way.
#
# Why this script exists (2026-11-13, M4 顺带项 / R0 follow-up):
#
#   `cargo zigbuild` hands the FINAL link to Zig's bundled LLD, which links
#   Zig's own musl. Its malloc is `heap.SmpAllocator`: freed small blocks go
#   onto per-size free lists and never return to the OS. That is the exact
#   allocator that grew the armv7 firmware's RSS ~0.33 MB/s until the kernel
#   killed it (R0, gateway docs/d/r0-20261112.md §1.6).
#
#   The core was still built with plain `cargo zigbuild`: `strings` showed
#   `Linker: Homebrew LLD 22.1.8`, and on-device the core went from
#   9,180 kB RSS to 23,128 kB in 15 minutes while idle (fd count flat) —
#   the same signature. This script moves ONLY the final link to rust-lld
#   (Rust's musl libc.a / mallocng) and keeps `zig cc` for C dependencies,
#   exactly as tools/build_firmware.sh does for armv7.
#
# Acceptance is behavioural, not just symbol-level: deploy and take two RSS
# readings >= 15 min apart (mem_probe.sh); the criterion is +-1-2 MB with a
# flat fd count.
set -eu

TARGET=aarch64-unknown-linux-musl
HERE=$(cd "$(dirname "$0")/.." && pwd)
cd "$HERE"

SLIM=0
for arg in "$@"; do
    case "$arg" in
        --slim) SLIM=1 ;;
        *) echo "build_core: unknown option '$arg' (supported: --slim)" >&2; exit 2 ;;
    esac
done

SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | sed -n 's/^host: //p')
LINKER="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld"
if [ ! -x "$LINKER" ]; then
    echo "build_core: rust-lld not found at $LINKER (rustup component missing?)" >&2
    exit 1
fi

CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$LINKER"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER

# Rust's musl libc.a carries symbols Zig's link happened to drop; stripping
# keeps the binary compact and well below the core update endpoint's 32 MiB
# body limit. Stripping is safe here because the assertions below read the
# .comment strings, not the symbol table.
RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols"
export RUSTFLAGS

if [ "$SLIM" -eq 1 ]; then
    cargo zigbuild --target "$TARGET" --release -p hm-core --no-default-features
else
    cargo zigbuild --target "$TARGET" --release -p hm-core
fi

BIN="target/$TARGET/release/hm-core"
python3 -I - "$BIN" <<'PY'
import subprocess, sys
bin_path = sys.argv[1]
head = subprocess.run(["head", "-c", "20", bin_path], capture_output=True).stdout
if head[4] != 2 or head[18] != 183:  # ELFCLASS64, EM_AARCH64
    sys.exit(f"{bin_path}: expected ELF64 aarch64")
strings = subprocess.run(["strings", "-a", bin_path], capture_output=True, text=True).stdout
if "Homebrew LLD" in strings or "GNU ld" in strings:
    sys.exit(f"{bin_path}: final link is NOT rust-lld (Zig/Homebrew LLD ran last)")
if "rust-lang/llvm-project" not in strings:
    sys.exit(f"{bin_path}: rust-lld marker missing")
print(f"PASS  elf64-aarch64-static + rust-lld-final-link")
PY

# The device update endpoint takes at most 33,554,432 bytes (32 MiB) per
# body, so an artifact above that cannot be delivered no matter how clean
# the assertions above are. The gate lives here, not in CI: .github/
# workflows/ci.yml builds and tests the HOST target only (no aarch64 musl
# cross build, no strip), so there is no release artifact there to measure
# and a green CI run is not a size measurement. This script is the one place
# the shippable bytes exist.
#
# Exit 0 = fits (a WARNING holds the artifact at 24,000,000+ so growth shows
# up in build logs before it becomes a hard failure); exit 1 = over 32 MiB,
# do not ship.
python3 -I "$HERE/tools/size_gate.py" "$BIN"

echo "build_core: $BIN"
