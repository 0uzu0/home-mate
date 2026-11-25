#!/system/bin/sh
# Home Mate bootstrap seed — on-device installer (M9.5, 2026-11-14).
#
# What this is: the minimal runtime chain that turns a freshly emOS-flashed,
# otherwise empty dot2 into a machine whose core answers http://0.0.0.0:8930.
# It is NOT an installation: no release is active afterwards, `hm-release
# status` still says "active: none", and every later version arrives through
# `hm-release install --activate` like on any other node.
#
# The seed lands exactly where the emOS init hook and the two supervisors
# already look (determined on dot2-d, 2026-11-14):
#
#   /init  ──exec──>  /data/local/bin/start_server.sh
#                       ├─ /data/hm/hm-core-supervisor.sh  → /data/hm/bin/current (core)
#                       └─ loop: /data/local/tmp/hm-direct/current firmware --core 127.0.0.1:8932
#
# Hard rules (provisioning design §2.1, M9-2 lessons):
#   * refuses to touch a machine that already runs an install — a seed is for
#     empty machines, and overwriting a working node is never a side effect;
#   * never writes the database, state.json, session/bookmark data, led-map,
#     acoustic calibration, auth tokens or env.local. A seed is not a copy of
#     another device; copying those is how two nodes end up sharing an identity;
#   * never writes kw.txt: the word list belongs to the node's own config and
#     is pushed by the core (`POST /api/v0/reflex/deploy`), so until that call
#     the keyword sidecar reports `down` — that is the honest state, not a
#     failure;
#   * "seed installed" is not "device works". This script proves the files
#     landed and prints the check that proves the service, nothing more.
#
# Usage: sh install.sh [--start] [--force-empty]
#   --start        also start the stack now (default: let emOS init do it on
#                  the next boot — init execs start_server.sh after /run/net-up)
#   --force-empty  proceed even though an install is present (operator override;
#                  it OVERWRITES the runtime chain — the DB/config stay, the
#                  release pointers do not)
set -u

# No dirname on an older FireOS base (measured dot2-c 2026-11-14) — parse it.
case "$0" in
    */*) SEED=$(cd "${0%/*}" && pwd) ;;
    *)   SEED=$PWD ;;
esac
HM=/data/hm
FW=/data/local/tmp/hm-direct
KWS=/data/local/tmp/hm-kws
BIN=/data/local/bin

START=0
FORCE=0
for arg in "$@"; do
    case "$arg" in
        --start) START=1 ;;
        --force-empty) FORCE=1 ;;
        *) echo "install.sh: unknown argument $arg" >&2; exit 2 ;;
    esac
done

say() { echo "[seed] $*"; }
die() { echo "[seed] FAILED: $*" >&2; exit 1; }

# Portability: emOS ships busybox applets on /system; an older FireOS base does
# not. Use the node's busybox when we found one, plain PATH tools otherwise.
bb() {
    if [ -n "${BB:-}" ]; then "$BB" "$@"; else "$@"; fi
}
cp_a() {  # copy preserving mode; toybox cp may lack -a
    if [ -n "${BB:-}" ]; then "$BB" cp -a "$1" "$2"; else cp -a "$1" "$2" 2>/dev/null || cp -r "$1" "$2"; fi
}

# Android's `id` ignores -u and prints the long form; busybox prints uid=0.
case "$(id 2>/dev/null)" in
    uid=0\(*|uid=0) ;;
    *) die "must run as root (this writes /data and /data/local)" ;;
esac
[ -r "$SEED/SEED.json" ] || die "SEED.json missing next to install.sh — is this a complete seed?"
[ -r "$SEED/hm/bin/core_a" ] || die "hm/bin/core_a missing"
[ -r "$SEED/hm-direct/audio_a" ] || die "hm-direct/audio_a missing"
[ -r "$SEED/local/bin/start_server.sh" ] || die "local/bin/start_server.sh missing"
DEVICE_ID=$(cat "$SEED/hm-direct/device-id" 2>/dev/null)
[ -n "$DEVICE_ID" ] || die "hm-direct/device-id missing — the gateway would come up as dot2-d"
case "$DEVICE_ID" in
    *[!a-z0-9-]*) die "invalid device id '$DEVICE_ID' (lowercase letters, digits, dash)" ;;
esac

# ── Empty-machine guard. Two signals, both are "someone already lives here":
#    a release state file (installed by hm-release) and a live slot pointer.
if [ "$FORCE" = "0" ]; then
    if [ -e "$HM/releases/state.json" ]; then
        die "this machine already has an install ($HM/releases/state.json exists); a seed is for empty machines. Use 'hm-release install --activate' to upgrade it (or --force-empty to overwrite the runtime chain)"
    fi
    if [ -e "$HM/bin/current" ]; then
        die "$HM/bin/current exists; refusing to overwrite a running node (use --force-empty if that is really intended)"
    fi
fi

say "device id: $DEVICE_ID"
say "seed: $SEED"

# ── Userland. The supervisors assume a shell with tail/wc/grep/df — true on
#    emOS (its /system ships them) but NOT on an older FireOS base (dot2-c,
#    LVY48F: no tail, wc, awk, sed, tar — measured 2026-11-14). We do not ship
#    a busybox of our own (a bundled GPLv2 binary would drag its source offer
#    into the product); we link the one the node already has. Both supervisors
#    put /data/hm/tools last on PATH, which is inert when it is empty.
BB=""
for cand in /data/adb/magisk/busybox /system/xbin/busybox /system/bin/busybox; do
    [ -x "$cand" ] && { BB=$cand; break; }
done
if [ -z "$BB" ] && command -v busybox >/dev/null 2>&1; then
    BB=$(command -v busybox)
fi
if [ -n "$BB" ]; then
    mkdir -p /data/hm/tools 2>/dev/null
    "$BB" --install -s /data/hm/tools >/dev/null 2>&1
    say "userland: applets from $BB into /data/hm/tools"
else
    say "userland: no busybox on this node — the supervisors need tail/wc/grep/df from /system"
fi

# ── emOS WiFi control directory (dot2-b first install, 2026-11-15).
#
# /data/emos/wpa.conf declares `ctrl_interface=/data/emos/sockets`, and the
# emOS init spawns wpa_supplicant without creating that directory. The
# supplicant then exits immediately, init respawns it, and the machine sits at
# stage 11 with a zombie pair and no wlan0 address — a freshly flashed device
# that never reaches the network. Creating it HERE (the seed runs on FireOS,
# before emOS ever boots) makes the first boot work with no console surgery.
#
# Ownership is the wifi uid/gid 1010 (same as the emOS/RUI control socket),
# mode 771 so the supplicant can create its socket without being readable by
# everyone. Idempotent: an existing directory is left exactly as it is.
if [ ! -d /data/emos/sockets ]; then
    bb mkdir -p /data/emos/sockets || die "mkdir /data/emos/sockets"
    bb chown 1010:1010 /data/emos/sockets 2>/dev/null || say "warning: could not chown /data/emos/sockets to 1010:1010"
    bb chmod 771 /data/emos/sockets || die "chmod /data/emos/sockets"
    say "wifi control dir: /data/emos/sockets created (1010:1010 771)"
else
    say "wifi control dir: /data/emos/sockets already present (left as is)"
fi

# ── Create the layout, then copy. cp_a everywhere: emOS ships a
#    /system/bin/cp that ignores -P and the console deployment already learned
#    that lesson (2026-11-12).
bb mkdir -p "$HM/bin" "$HM/data/models" "$FW" "$KWS" "$BIN" || die "mkdir"
cp_a "$SEED/hm/bin/core_a" "$HM/bin/core_a" || die "copy core"
cp_a "$SEED/hm/hm-core-supervisor.sh" "$HM/hm-core-supervisor.sh" || die "copy core supervisor"
bb rm -rf "$HM/web" || die "clean console"
cp_a "$SEED/hm/web" "$HM/web" || die "copy console"
cp_a "$SEED/hm/data/models/silero_vad.onnx" "$HM/data/models/silero_vad.onnx" || die "copy model"
# Pre-synthesized wake acks (data_dir/assets/wake-acks). Without them a fresh
# node answers a wake with nothing until someone re-saves the ack settings in
# the console (measured dot2-c 2026-11-14). Not device state: every node may
# legitimately have the same acks, and re-copying them is harmless.
if [ -d "$SEED/hm/data/assets" ]; then
    bb mkdir -p "$HM/data/assets" || die "mkdir assets"
    cp_a "$SEED/hm/data/assets/." "$HM/data/assets/" || die "copy assets"
fi
if [ -e "$SEED/hm/bin/hm-release" ]; then
    cp_a "$SEED/hm/bin/hm-release" "$HM/bin/hm-release" || die "copy installer"
fi
cp_a "$SEED/hm-direct/audio_a" "$FW/audio_a" || die "copy firmware"
cp_a "$SEED/hm-direct/device-id" "$FW/device-id" || die "copy device id"
cp_a "$SEED/hm-kws/." "$KWS/" || die "copy kws bundle"
cp_a "$SEED/local/bin/start_server.sh" "$BIN/start_server.sh" || die "copy supervisor"

# ── Modes: the two supervisors are exec'd, the slot binaries are exec'd.
bb chmod 755 "$HM/bin/core_a" "$HM/hm-core-supervisor.sh" "$BIN/start_server.sh" \
    "$FW/audio_a" "$KWS/hm-kws" "$KWS/ld-linux-aarch64.so.1" 2>/dev/null
[ -e "$HM/bin/hm-release" ] && bb chmod 755 "$HM/bin/hm-release"
bb chmod 644 "$FW/device-id"

# ── Slot pointers. hm-release flips these itself once a release is active;
#    the seed only needs the bootstrap slot to be the current one.
[ -e "$HM/bin/current" ] || ln -sf core_a "$HM/bin/current" || die "link core slot"
[ -e "$FW/current" ] || ln -sf audio_a "$FW/current" || die "link firmware slot"

# ── Facts only: what is on disk now, and what is deliberately absent.
say "landed:"
for f in "$HM/bin/current" "$HM/bin/core_a" "$HM/hm-core-supervisor.sh" \
         "$FW/current" "$FW/device-id" "$BIN/start_server.sh" "$KWS/run.sh" \
         "$HM/web/index.html" "$HM/data/models/silero_vad.onnx" "/data/emos/sockets"; do
    if [ -e "$f" ]; then
        say "  ok      $f"
    else
        say "  MISSING $f"
    fi
done
for f in "$HM/releases/state.json" "$HM/data/home-mate.db" "$FW/state.json" "$FW/led-map.json" "$KWS/kw.txt"; do
    [ -e "$f" ] && say "  present $f (not created by the seed)"
done
say "no database, state.json, led-map, tokens or env.local were written — this is a fresh node"

if [ "$START" = "1" ]; then
    # Detach so the installer's shell (serial console) can exit. init will
    # re-exec it after the next boot; the core's port check makes that
    # idempotent.
    say "starting the stack now"
    if command -v setsid >/dev/null 2>&1; then
        setsid /system/bin/sh "$BIN/start_server.sh" >/dev/null 2>&1 &
    else
        nohup /system/bin/sh "$BIN/start_server.sh" >/dev/null 2>&1 &
    fi
    say "started — wait for the core, then: /data/hm/bin/hm-release status"
else
    say "not started (default). Either reboot, or run:"
    say "  setsid /system/bin/sh $BIN/start_server.sh >/dev/null 2>&1 &"
fi
say "next (provisioning §2.2 steps 6-7):"
say "  1. wait for GET http://<ip>:8930/api/v0/status to answer"
say "  2. POST /api/v0/reflex/deploy  (installs the node's own word list; the keyword sidecar is 'down' until then)"
say "  3. hm-release install --package <release.tar> --activate   (this is what makes it a managed node)"
exit 0
