#!/bin/sh
# hm-provision.sh — one command from an unlocked dot2 to a talking Home Mate
# node. Implements provisioning design §2.2 (2026-11-14); the parts that touch
# another project's code are DELEGATED, never reimplemented.
#
# The line this script draws (docs/d/provision-design-20261114.md §1):
#
#   amonet unlock          NOT ours. Pointed at, not automated (bootrom exploit,
#                          bricking risk). The operator does it; we check it.
#   emOS system layer      NOT ours to rebuild. Upstream emOS `build.sh` + its
#                          wizard write p10 from the DEVICE'S OWN kernel/DTB.
#                          We call them when --emos-dir is given.
#   our application layer  Ours, and the only part this script really owns:
#                          bootstrap seed → hm-release install --activate →
#                          pairing. That is steps 6-8.
#
# Usage:
#   tools/hm-provision.sh --device-id dot2-f \
#       --seed build/bootstrap/home-mate-bootstrap-dot2-f-2026.11.14-32.tar.gz \
#       --release build/release/home-mate-release-2026.11.15-39.tar \
#       --serial /dev/cu.usbmodem8401 [--ip 192.168.1.72] [--emos-dir ~/emOS]
#       [--p10-backup ~/Downloads/p10-stock.img] [--token-file ~/.home-mate/dot2-f.token]
#       [--reference-node dot2-d --reference-ip 192.168.1.73 --wake-word 你好小雨]
#       [--log provision.log] [--dry-run]
#
# --reference-node / --reference-ip
#               A node that is already configured for this household. After
#               pairing, this node is registered there as a fleet peer and the
#               household configuration is PUSHED to it (services/keys, music,
#               weather, news, search, OpenAPI, the tone, the command words,
#               the voice/interaction policy). Without this step a fresh node
#               keeps the code defaults: the wrong close-window tone, the
#               "answer anything in the room" flag, and the same wake word as
#               the reference speaker (measured dot2a 2026-11-15). Skipped with
#               a warning when not given. Requires release 39+ on the reference.
# --wake-word   This node's own wake word. A push never carries wake words (by
#               design), so a new node MUST be given one here or from the
#               console; the script refuses a word the reference already uses
#               in the same house (--allow-same-wake-word overrides).
#
# --dry-run     Print every mutating action as WOULD and touch nothing on the
#               device. Read-only probes still run, and step 10 compares the
#               before/after snapshot so "nothing changed" is evidence, not a
#               promise. This is the mode to run first against a live node.
# --token-file  The node's family token (v0.23: /api/v0/* answers 401 without
#               it). Defaults to ~/.home-mate/<device-id>.token. The token is
#               read here and NEVER logged or printed.
#
# Every step prints what it observed, and the script stops at the first
# failure naming the step. A half-done provisioning must never look done:
# "seed installed" is not "installed", "release active" is not "healthy", and
# a pairing code that was never printed is a node nobody can use. Re-running
# against a node that is already provisioned is supported and must be a
# no-op — it reports "already installed / already active / already paired".
set -u

DEVICE_ID=""
SEED=""
RELEASE=""
SERIAL=""
IP=""
EMOS_DIR=""
P10_BACKUP=""
REF_NODE=""
REF_IP=""
REF_TOKEN_FILE=""
WAKE_WORD=""
ALLOW_SAME_WAKE=0
LOG="provision.log"
SERVE_PORT=8123
DRY=0
TOKEN_FILE=""

usage() { sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --device-id) DEVICE_ID=$2; shift 2 ;;
        --seed) SEED=$2; shift 2 ;;
        --release) RELEASE=$2; shift 2 ;;
        --serial) SERIAL=$2; shift 2 ;;
        --ip) IP=$2; shift 2 ;;
        --emos-dir) EMOS_DIR=$2; shift 2 ;;
        --p10-backup) P10_BACKUP=$2; shift 2 ;;
        --token-file) TOKEN_FILE=$2; shift 2 ;;
        --reference-node) REF_NODE=$2; shift 2 ;;
        --reference-ip) REF_IP=$2; shift 2 ;;
        --reference-token-file) REF_TOKEN_FILE=$2; shift 2 ;;
        --wake-word) WAKE_WORD=$2; shift 2 ;;
        --allow-same-wake-word) ALLOW_SAME_WAKE=1; shift ;;
        --log) LOG=$2; shift 2 ;;
        --port) SERVE_PORT=$2; shift 2 ;;
        --dry-run) DRY=1; shift ;;
        -h|--help|help) usage; exit 0 ;;
        *) echo "hm-provision: unknown argument $1" >&2; exit 2 ;;
    esac
done

[ -n "$DEVICE_ID" ] || { echo "hm-provision: --device-id is required" >&2; exit 2; }
[ -n "$SERIAL" ] || { echo "hm-provision: --serial is required (emOS has no adbd; the USB console is the only channel)" >&2; exit 2; }

HERE=$(cd "$(dirname "$0")/.." && pwd)
SERIAL_TOOL="$HERE/../home-mate-device-gateway-b/tools/serial_cmd.py"
if [ -z "$TOKEN_FILE" ] && [ -f "$HOME/.home-mate/$DEVICE_ID.token" ]; then
    TOKEN_FILE="$HOME/.home-mate/$DEVICE_ID.token"
fi
if [ -n "$REF_NODE" ] && [ -z "$REF_TOKEN_FILE" ] && [ -f "$HOME/.home-mate/$REF_NODE.token" ]; then
    REF_TOKEN_FILE="$HOME/.home-mate/$REF_NODE.token"
fi
STEP=0

say() { printf '%s\n' "$*"; }
log() {
    # Timestamped, append-only. Never pass a token through here.
    printf '[%s] %s\n' "$(date '+%Y-%m-%dT%H:%M:%S')" "$*" >> "$LOG"
    printf '%s\n' "$*"
}
step() { STEP=$(( STEP + 1 )); log ""; log "== step $STEP: $*"; }
fail() { log "STOPPED at step $STEP: $*"; log "provisioning is INCOMPLETE — nothing was reported as done"; exit 1; }
skip() { log "  skipped: $*"; }

# A mutating action. In --dry-run it is named and NOT executed (returns 1 so
# the caller's `if` body is skipped). Everything the device would receive is
# printed before it is ever sent.
mutate() {
    if [ "$DRY" = "1" ]; then log "  WOULD: $*"; return 1; fi
    return 0
}

# Bearer header for the family plane (v0.23). Empty when there is no token —
# an unpaired node answers /status and /mode openly, so that is not an error.
auth_hdr() {
    if [ -n "$TOKEN_FILE" ] && [ -f "$TOKEN_FILE" ]; then
        printf 'Authorization: Bearer %s' "$(cat "$TOKEN_FILE")"
    fi
}

# The reference node speaks for itself: its own token, never printed and never
# written to the log. Empty = the reference was never paired, which the fleet
# step then reports instead of guessing.
ref_hdr() {
    if [ -n "$REF_TOKEN_FILE" ] && [ -f "$REF_TOKEN_FILE" ]; then
        printf 'Authorization: Bearer %s' "$(cat "$REF_TOKEN_FILE")"
    fi
}

# Is this node already registered on the reference? Read-only.
fleet_peer_registered() {  # $1 ref ip  $2 device id
    curl -sf -m 5 -H "$(ref_hdr)" "http://$1:8930/api/v0/fleet" 2>/dev/null \
        | python3 -c 'import json,sys
want=sys.argv[1]
print("1" if any(n.get("id")==want for n in (json.load(sys.stdin).get("nodes") or [])) else "0")' "$2" 2>/dev/null \
        || echo 0
}

# What a push WOULD change. Read-only on both sides (dry_run never contacts the
# peer), so it is printed in --dry-run too when the peer already exists.
fleet_plan_preview() {  # $1 ref ip  $2 device id
    _plan=$(curl -s -m 30 -X POST -H "$(ref_hdr)" -H 'content-type: application/json' \
        --data-binary "{\"targets\":[\"$2\"],\"include_secrets\":true,\"dry_run\":true}" \
        "http://$1:8930/api/v0/fleet/push" 2>/dev/null || true)
    printf '%s' "$_plan" | python3 -c 'import json,sys
d=json.load(sys.stdin); r=(d.get("results") or [{}])[0]
if r.get("error"): print("  plan: ERROR:", r["error"])
else: print("  plan: would write", len(r.get("applied") or []), "block(s):", ", ".join(r.get("applied") or []))
for w in (r.get("warnings") or []): print("   -", w)' 2>/dev/null || log "  plan: no readable answer ($_plan)"
}

# Run one command on the device console and print its output between markers.
# serial_cmd.py echoes the console, so the markers make the answer greppable.
dev() {
    _cmd=$1
    _wait=${2:-6}
    python3 "$SERIAL_TOOL" "echo __B__; $_cmd; echo __E__" "$_wait" "$SERIAL" 2>/dev/null \
        | sed -n '/__B__/,/__E__/p' | grep -v '__B__' | grep -v '__E__'
}

wait_http() {
    _url=$1
    _tries=${2:-40}
    _i=0
    while [ "$_i" -lt "$_tries" ]; do
        if curl -sf -m 3 -H "$(auth_hdr)" "$_url" >/dev/null 2>&1; then return 0; fi
        _i=$(( _i + 1 ))
        sleep 3
    done
    return 1
}

# Serve one directory and PROVE our file is on the wire before the device is
# told to download it. A stale `python3 -m http.server` from an earlier session
# may already own the port and answer 404 for everything (measured 2026-11-15:
# the device got "HTTP/1.0 404 File not found" for a seed that existed), so the
# port is chosen by probing and the URL is fetched back before use.
# Sets SERVE_PORT and writes the server pid to $1.
# The USB console re-enumerates with the machine: HTTP can answer seconds
# before the serial gadget is readable again, and the port NAME can change
# (codex review 2026-11-15, Q1b). Never assume the original `$SERIAL` is still
# the console — probe it, then fall back to the other usbmodem ports.
pick_serial_port() {
    if dev "echo __CONSOLE_OK__" 4 2>/dev/null | grep -q '__CONSOLE_OK__'; then return 0; fi
    for _p in $(ls /dev/cu.usbmodem* 2>/dev/null); do
        [ "$_p" = "$SERIAL" ] && continue
        if python3 "$SERIAL_TOOL" "echo __CONSOLE_OK__" 4 "$_p" 2>/dev/null | grep -q '__CONSOLE_OK__'; then
            log "  console moved: $SERIAL -> $_p"
            SERIAL=$_p
            return 0
        fi
    done
    return 1
}

wait_console() {
    _i=0
    while [ "$_i" -lt "${1:-20}" ]; do
        if pick_serial_port; then return 0; fi
        _i=$(( _i + 1 ))
        sleep 3
    done
    return 1
}

# The node's own uptime in seconds. Used to prove that "the core answers
# again" is a NEW boot and not the old process still holding the port.
node_uptime() {
    dev "busybox cat /proc/uptime" 6 2>/dev/null | tr -d '\r' | grep -E '^[0-9]' | head -1 | awk '{print int($1)}'
}

# The release the node itself reports as active, straight from its API. The
# console is a channel; this is the device's fact (v0.15: /release is what the
# installer wrote, including a rollback).
active_version() {
    curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/release" \
        | python3 -c 'import json,sys; s=json.load(sys.stdin).get("state") or {}; print(((s.get("active") or {}).get("version") or ""))' 2>/dev/null
}

# `last` is where a rollback is recorded (active can still name the release
# that was active BEFORE the attempt, and `rollback_failed` keeps it there —
# codex review 2026-11-15 Q1a). Printed as evidence when a check fails.
last_release_line() {
    curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/release/log?lines=40" \
        | python3 -c 'import json,sys; ls=json.load(sys.stdin).get("lines") or []; print("\n".join(ls[-8:]))' 2>/dev/null
}

serve_dir() {  # $1 pidfile  $2 dir  $3 start-port  $4 filename
    _pidfile=$1
    _dir=$2
    _p=$3
    _name=$4
    _limit=$(( $3 + 20 ))
    while [ "$_p" -lt "$_limit" ]; do
        _code=$(curl -s -m 1 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$_p/" 2>/dev/null) || _code=000
        if [ "$_code" = "000" ]; then
            ( cd "$_dir" && python3 -m http.server "$_p" >/dev/null 2>&1 & echo $! > "$_pidfile" )
            sleep 1
            if curl -sf -m 3 -o /dev/null "http://127.0.0.1:$_p/$_name"; then
                SERVE_PORT=$_p
                return 0
            fi
            _pid=$(cat "$_pidfile" 2>/dev/null)
            [ -n "$_pid" ] && kill "$_pid" 2>/dev/null
        else
            log "  port $_p is already in use by something else — trying $(( _p + 1 ))"
        fi
        _p=$(( _p + 1 ))
    done
    return 1
}

stop_serve() {  # $1 pidfile
    _pid=$(cat "$1" 2>/dev/null)
    [ -n "$_pid" ] && kill "$_pid" 2>/dev/null
    rm -f "$1"
}

# The three facts the brief compares around a dry run. Time-varying database
# counters are deliberately excluded: this is about "did provisioning change
# the machine", not "did the device keep living".
snapshot() {
    _label=$1
    log "  -- snapshot: $_label --"
    if ! wait_http "http://$IP:8930/api/v0/status" 2; then
        log "    core: not answering"
        log "    /data/hm mtime: $(dev "busybox stat -c %Y /data/hm 2>/dev/null || echo absent" 6 | tr -d '\r' | tail -1)"
        return 0
    fi
    _status=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/status")
    log "    core: $(printf '%s' "$_status" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["build"]["id"], "data_dir", d["data_dir"])')"
    _rel=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/release")
    if [ -n "$_rel" ]; then
        log "    release: $(printf '%s' "$_rel" | python3 -c 'import json,sys; s=json.load(sys.stdin).get("state") or {}; print("active", (s.get("active") or {}).get("id"), "previous", (s.get("previous") or {}).get("id"))')"
    fi
    _mode=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/mode")
    if [ -n "$_mode" ]; then
        log "    mode: $(printf '%s' "$_mode" | python3 -c 'import json,sys; d=json.load(sys.stdin); dev=(d.get("devices") or [{}])[0]; print("wake_words", d.get("wake_words"), "| follow_window_tone", dev.get("follow_window_tone"), "| muted", dev.get("muted"), "| kws", (dev.get("kws") or {}).get("state"))')"
    fi
    log "    /data/hm mtime: $(dev "busybox stat -c %Y /data/hm 2>/dev/null || echo absent" 6 | tr -d '\r' | tail -1)"
}

: > "$LOG" 2>/dev/null || { echo "hm-provision: cannot write $LOG" >&2; exit 2; }
log "hm-provision: device-id=$DEVICE_ID serial=$SERIAL dry-run=$DRY"
[ "$DRY" = "1" ] && log "  DRY RUN: every mutating action is printed as WOULD and skipped"
[ -n "$SEED" ] && [ -f "$SEED" ] || fail "seed not found: ${SEED:-<empty>}"
[ -n "$RELEASE" ] && [ -f "$RELEASE" ] || fail "release package not found: ${RELEASE:-<empty>}"
[ -f "$SERIAL_TOOL" ] || fail "serial helper not found: $SERIAL_TOOL"
[ -n "$TOKEN_FILE" ] || log "  no token file — this node must be unpaired (v0.23 answers 401 otherwise)"

# ── 1. Preconditions: the console answers, and we can prove which machine it is.
step "console reachable + device identity (design §2.2 step 1)"
# An EMPTY console read is not "not emOS" — it is "the console did not answer"
# (it happens right after a reboot, or when a previous read swallowed the
# reply). Retry, and only conclude from a real read.
IDS=""
_try=0
while [ "$_try" -lt 4 ]; do
    IDS=$(dev "busybox cat /etc/os-release 2>/dev/null; echo OS_END; busybox cat /sys/class/net/wlan0/address 2>/dev/null" 10)
    case "$IDS" in
        *"ID=emos"*|*"android"*|*"Fire OS"*|*"OS_END"*) break ;;
    esac
    _try=$(( _try + 1 ))
    log "  console returned nothing usable (${_try}/4) — retrying in 3 s"
    sleep 3
done
case "$IDS" in
    *"ID=emos"*) log "  os: emOS (application layer can be provisioned)" ;;
    *)
        log "  os: not emOS (stock FireOS or TWRP)"
        log "  This path is NOT automated here. Do, in this order:"
        log "    1. amonet unlock + root  — XDA tutorial (we point at it, we do not ship it)"
        log "    2. emOS: cd <emOS checkout> && ./build.sh  then its wizard (reads the device's own p10, writes init)"
        log "    3. boot emOS, then on this console: em-wifi  (scan, pick, PSK — no AP, no Bluetooth)"
        log "    4. re-run this script with the same arguments"
        [ -n "$IDS" ] || fail "the console never answered on $SERIAL (no reply to /etc/os-release). This is a channel failure, not a verdict about the node: check the cable/port with pick_serial_port, then re-run"
        fail "device is not running emOS yet"
        ;;
esac
log "  $(printf '%s' "$IDS" | grep -v OS_END | tr '\n' ' ')"

# ── 2. Original p10 backup. We cannot take it from emOS (p10 is already the
#      emOS image at that point); the operator took it before flashing. What
#      this step does is refuse to pretend otherwise.
step "stock p10 backup on record (design §2.2 step 2)"
# A backup that exists on disk but was not named on the command line is still
# a documented way back. Searching the operator's backup folder first is what
# stops this step from printing "NO documented way back" at a machine that has
# two of them (measured 2026-11-15 on dot2a, which had the stock FireOS image
# and the pre-emOS image already filed).
FOUND_P10=""
if [ -z "$P10_BACKUP" ]; then
    for _d in "$HOME"/.home-mate/device-backups/"$DEVICE_ID"*/; do
        [ -d "$_d" ] || continue
        for _f in "$_d"*; do
            [ -f "$_f" ] || continue
            case "$_f" in *.img|*.img.gz|*.bin|*.gz|*.tar.gz|*.zip|*.sha256|*.txt|*.json) FOUND_P10="$FOUND_P10 $_f" ;; esac
        done
    done
fi
if [ -n "$P10_BACKUP" ]; then
    [ -f "$P10_BACKUP" ] || fail "p10 backup named but missing: $P10_BACKUP"
    log "  $(shasum -a 256 "$P10_BACKUP" | awk '{print $1}')  $P10_BACKUP  ($(wc -c < "$P10_BACKUP") bytes)"
    log "  keep this file: without it the machine cannot go back to FireOS"
elif [ -n "$FOUND_P10" ]; then
    log "  on record in ~/.home-mate/device-backups/ (not passed with --p10-backup):"
    for _f in $FOUND_P10; do
        log "    $(wc -c < "$_f" | awk '{printf "%12d", $1}') bytes  $_f"
    done
    log "  verify before relying on these: a backup file's SIZE is not its integrity"
else
    log "  none supplied (--p10-backup) and none found under ~/.home-mate/device-backups/$DEVICE_ID*/"
    log "  Recording that this node has NO documented way back."
    log "  This is the restore image; deleting it is irreversible."
fi

# ── 3. emOS system layer: delegated to upstream, by design.
step "emOS system layer (design §2.2 step 3)"
if [ -n "$EMOS_DIR" ]; then
    if [ -x "$EMOS_DIR/build.sh" ]; then
        log "  would run: (cd $EMOS_DIR && ./build.sh) then its wizard"
        log "  NOT executed by this script: the build/wizard drive TWRP and p10 writes,"
        log "  and its interactive flash is the step the operator must watch."
        skip "upstream emOS build+wizard (run by hand; this script does not wrap a p10 write)"
    else
        skip "no $EMOS_DIR/build.sh — operator flashes emOS by hand"
    fi
else
    skip "emOS already running; no --emos-dir given"
fi

# ── 3b. emOS WiFi control directory (dot2-b first install, 2026-11-15).
#      /data/emos/wpa.conf declares ctrl_interface=/data/emos/sockets and the
#      emOS init does not create it, so wpa_supplicant exits on start, init
#      respawns it, and the node sits at stage 11 with no wlan0 address. The
#      seed creates this on FireOS before the first emOS boot (the durable
#      fix); this step is what repairs an already-booting node and what makes
#      a re-run recover it. Idempotent: a correct directory is left alone.
step "emOS runtime directories (not in §2.2: /data/emos/sockets, §2.1)"
SOCK_MODE=$(dev "busybox stat -c '%a:%u:%g' /data/emos/sockets 2>/dev/null || echo ABSENT" 8 | tr -d '\r' | tail -1)
case "$SOCK_MODE" in
    771:1010:1010|771:1013:1013)
        log "  /data/emos/sockets $SOCK_MODE (ok)"
        ;;
    ABSENT|"")
        if mutate "mkdir /data/emos/sockets on the node (1010:1010 771)"; then
            dev "busybox mkdir -p /data/emos/sockets && busybox chown 1010:1010 /data/emos/sockets && busybox chmod 771 /data/emos/sockets && busybox stat -c '%a:%u:%g' /data/emos/sockets" 10 >/dev/null \
                || fail "could not create /data/emos/sockets on the node"
            log "  /data/emos/sockets created (1010:1010 771) — the init retries the supplicant within 5 s"
        fi
        ;;
    *)
        if mutate "chmod 771 + chown 1010:1010 /data/emos/sockets (was $SOCK_MODE)"; then
            dev "busybox chown 1010:1010 /data/emos/sockets && busybox chmod 771 /data/emos/sockets" 10 >/dev/null \
                || fail "could not fix the mode of /data/emos/sockets"
            log "  /data/emos/sockets repaired (was $SOCK_MODE)"
        fi
        ;;
esac

# ── 4. Network: emOS waits at stage 11 without /data/emos/wpa.conf. That is a
#      wait, not a failure (provisioning design §1).
step "network (design §2.2 steps 4-5)"
if [ -z "$IP" ]; then
    IP=$(dev "busybox ip -4 addr show wlan0 2>/dev/null | busybox grep -o 'inet [0-9.]*' | busybox head -1 | busybox cut -d' ' -f2" 6 | tr -d '\r' | grep -E '^[0-9]+\.' | head -1)
fi
[ -n "$IP" ] || fail "no wlan0 address. On the console run: em-wifi  (no AP mode needed; pick the SSID and enter the PSK)"
log "  wlan0 $IP"
snapshot "before"

# ── 5. What is already here? This is the whole difference between "first
#      install" and "re-run on a working node": the answer decides steps 6-8.
step "what is already installed (design §2.2 step 6 precondition)"
CORE_UP=0
if wait_http "http://$IP:8930/api/v0/status" 3; then
    CORE_UP=1
    _id=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/status" | python3 -c 'import json,sys; print(json.load(sys.stdin)["build"]["id"])' 2>/dev/null)
    log "  core already answers (build ${_id:-unknown}) — the seed step will refuse to overwrite it"
else
    log "  core not running (expected on an empty machine)"
fi
# The seed carries this node's id (`hm-direct/device-id`, handed to the gateway
# as --device-id). Installing dot2-c's seed while declaring --device-id dot2-a
# produces a node whose identity is not what was announced — and nothing used
# to complain (measured 2026-11-15). Checked on EVERY run, not only when a seed
# install is about to happen: wrong input must fail before anything is touched.
_entry=$(tar -tzf "$SEED" 2>/dev/null | grep 'hm-direct/device-id$' | head -1)
[ -n "$_entry" ] || fail "seed has no hm-direct/device-id — it is not a bootstrap seed this script can install"
SEED_ID=$(tar -xzOf "$SEED" "$_entry" 2>/dev/null | tr -d '\r\n')
[ -n "$SEED_ID" ] || fail "seed declares an empty device id ($_entry)"
if [ "$SEED_ID" != "$DEVICE_ID" ]; then
    fail "seed belongs to '$SEED_ID' but --device-id is '$DEVICE_ID'. The node would come up announcing '$SEED_ID'. Rebuild the seed with --device-id $DEVICE_ID (tools/build_bootstrap.py) or fix the argument."
fi
log "  seed declares device-id $SEED_ID (matches --device-id)"

REL_NAME=$(basename "$RELEASE")
REL_VER=$(printf '%s' "$REL_NAME" | sed -n 's/^home-mate-release-\(.*\)\.tar$/\1/p')
[ -n "$REL_VER" ] || fail "release file name must be home-mate-release-<version>.tar (got $REL_NAME)"
ACTIVE_VER=""
if [ "$CORE_UP" = "1" ]; then
    ACTIVE_VER=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/release" \
        | python3 -c 'import json,sys; s=json.load(sys.stdin).get("state") or {}; print(((s.get("active") or {}).get("version") or ""))' 2>/dev/null)
fi
log "  package $REL_VER; active ${ACTIVE_VER:-none}"
if [ "$ACTIVE_VER" = "$REL_VER" ]; then
    ALREADY_ACTIVE=1
    log "  this node already runs $REL_VER — steps 6-7 must be no-ops"
else
    ALREADY_ACTIVE=0
fi

# ── 6. Bootstrap seed: the only part this script owns end to end. Push over
#      LAN with the device's own wget (emOS has no adbd), verify the hash,
#      unpack, install. The seed carries no config, DB or credentials.
step "bootstrap seed (design §2.2 step 6)"
if [ "$CORE_UP" = "1" ]; then
    skip "core already answers — a seed is for empty machines and install.sh refuses to overwrite a working node"
else
    SEED_NAME=$(basename "$SEED")
    SEED_SHA=$(shasum -a 256 "$SEED" | awk '{print $1}')
    log "  seed $SEED_NAME sha256 $SEED_SHA"
    MAC_IP=$(ipconfig getifaddr en0 2>/dev/null || echo "")
    [ -n "$MAC_IP" ] || fail "cannot determine the Mac's LAN address (en0)"
    if mutate "serve $SEED_NAME on a verified port and wget it to /data/bootstrap.tgz (sha256 $SEED_SHA)"; then
        serve_dir /tmp/hm-provision-http.pid "$(dirname "$SEED")" "$SERVE_PORT" "$SEED_NAME" \
            || fail "could not serve $SEED_NAME on any port from $(( SERVE_PORT )) to $(( SERVE_PORT + 20 ))"
        log "  serving $SEED_NAME on :$SERVE_PORT"
        GOT=$(dev "cd /data && busybox rm -rf bootstrap.tgz && busybox wget -q -O bootstrap.tgz http://$MAC_IP:$SERVE_PORT/$SEED_NAME && busybox sha256sum bootstrap.tgz | busybox cut -c1-64" 90)
        stop_serve /tmp/hm-provision-http.pid
        case "$GOT" in
            *"$SEED_SHA"*) log "  transferred, sha256 matches" ;;
            *) fail "seed transfer/hash mismatch (device said: $(printf '%s' "$GOT" | tr '\n' ' '))" ;;
        esac
    fi
    # Install WITHOUT --start, then REBOOT. Why this is not a preference
    # (measured dot2-c 2026-11-15): the firmware supervisor exits with code 0
    # when the firmware stops cleanly, and the A/B swap that `install
    # --activate` performs relies on emOS init RESPAWNING that supervisor from
    # the flipped slot. A supervisor started by `install.sh --start` (setsid
    # from this console) has no init parent, so after activation killed the
    # firmware nobody restarted it and activation rolled back with
    # "firmware process did not restart". Booting the node puts init in
    # charge, which is the shape the release path was designed and tested for.
    if mutate "unpack the seed, run install.sh (no --start), then reboot so emOS init owns the supervisor"; then
        UPTIME_BEFORE=$(node_uptime)
        OUT=$(dev "cd /data && busybox mkdir -p seed && cd seed && busybox rm -rf home-mate-bootstrap-* && busybox tar xzf ../bootstrap.tgz && cd home-mate-bootstrap-$DEVICE_ID && busybox sh install.sh 2>&1 | busybox tail -20" 60)
        log "$OUT"
        case "$OUT" in
            *"FAILED"*) fail "seed install refused or failed (see above)" ;;
            *"landed:"*) log "  seed reported its landing list (above)" ;;
            *) fail "the seed installer produced no landing list — an empty console read is not a successful install (check the console)" ;;
        esac
        # The tarball and the unpacked seed are ~76 MB and no longer needed:
        # the release install needs ~32 MB of staging and a 24 MB package, and
        # on this node (1 GB /data) leaving both behind is what turned the
        # release step into "not enough free space for staging" (2026-11-15).
        dev "busybox rm -rf /data/bootstrap.tgz /data/seed" 20 >/dev/null 2>&1
        log "  seed staging removed; df: $(dev "busybox df -h /data | busybox tail -1" 10 2>/dev/null | tr -d '\r' | tail -1)"
        log "  rebooting so init execs start_server.sh after /run/net-up"
        dev "busybox reboot" 3 >/dev/null 2>&1 || true
        # The console disappears with the machine and comes back with it; the
        # HTTP probe is the reliable signal (WiFi takes ~40-60 s on this node).
        _up=0
        _i=0
        while [ "$_i" -lt 40 ]; do
            if curl -sf -m 3 -H "$(auth_hdr)" "http://$IP:8930/api/v0/status" >/dev/null 2>&1; then _up=1; break; fi
            _i=$(( _i + 1 ))
            sleep 5
        done
        if [ "$_up" != "1" ]; then
            fail "the node did not come back at ${IP}:8930 after the seed reboot. On the console check: wlan0 has an IPv4 address, and /run/net-up exists (emOS init only execs start_server.sh after the network is up). If the WiFi interface is stuck with NO-CARRIER, power-cycle the device once (a warm reboot may not reset the WiFi chip)"
        fi
        wait_console 20 || fail "the node answers HTTP but its USB console never came back — reconnect the cable, then re-run (the release step needs the console to run hm-release)"
        # Proving a REBOOT, not a stale process: the uptime must have reset.
        UPTIME_AFTER=$(node_uptime)
        if [ -n "$UPTIME_BEFORE" ] && [ -n "$UPTIME_AFTER" ] && [ "$UPTIME_AFTER" -ge "$UPTIME_BEFORE" ]; then
            fail "the node never rebooted (uptime $UPTIME_BEFORE -> $UPTIME_AFTER); the supervisor would still not be init-owned, and activation would roll back"
        fi
        log "  core + console answered after the reboot (uptime ${UPTIME_BEFORE:-?} -> ${UPTIME_AFTER:-?})"
    fi
fi
if wait_http "http://$IP:8930/api/v0/status" 2; then
    STATUS=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/status")
    log "  core: $(printf '%s' "$STATUS" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["build"]["id"], d["data_dir"])')"
fi

# ── 6b. The node's own word list. The seed ships no kw.txt on purpose, so the
#      keyword sidecar reports "down" until the core is told which words this
#      household uses. A node whose spotter already loaded a list is LEFT
#      ALONE: a deploy restarts the sidecar (~7 s), and a re-run of a
#      provisioning script must not disturb a working node (measured
#      dot2-c 2026-11-15: the first re-run deployed anyway, then step 9's
#      health read kws.state="starting" and stopped — a false alarm caused by
#      this script, not by the node).
step "reflex word list (not in §2.2: the seed ships no kw.txt, §2.1)"
if wait_http "http://$IP:8930/api/v0/status" 2; then
    LOADED_HASH=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/mode" \
        | python3 -c 'import json,sys; dev=(json.load(sys.stdin).get("devices") or [{}])[0]; print(((dev.get("reflex") or {}).get("deploy") or {}).get("loaded_hash") or "")' 2>/dev/null)
    KWS_STATE=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/mode" \
        | python3 -c 'import json,sys; dev=(json.load(sys.stdin).get("devices") or [{}])[0]; print((dev.get("kws") or {}).get("state") or "")' 2>/dev/null)
    if [ -n "$LOADED_HASH" ] && [ "$KWS_STATE" = "running" ]; then
        skip "spotter already loaded $LOADED_HASH and is running — deploying would restart it for no reason"
    elif mutate "POST /api/v0/reflex/deploy (the node's own words; idempotent)"; then
        DEPLOY=$(curl -sf -m 30 -H "$(auth_hdr)" -X POST "http://$IP:8930/api/v0/reflex/deploy" || true)
        if [ -n "$DEPLOY" ]; then
            log "  $(printf '%s' "$DEPLOY" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("sent hash", d.get("hash"), "words", d.get("words"))' 2>/dev/null || echo "$DEPLOY")"
        else
            log "  no default list deployed (the console or the first release activation will push one)"
        fi
    fi
else
    skip "core not answering — nothing to deploy a word list to"
fi

# ── 7. The release: this is what makes the node managed (previous gets a
#      baseline from now on). Served from the same temporary httpd.
step "release install --activate (design §2.2 step 7)"
if [ "$ALREADY_ACTIVE" = "1" ]; then
    log "  already active: $ACTIVE_VER — no install, no activation (a re-run must not re-serialize the node)"
else
    MAC_IP=${MAC_IP:-$(ipconfig getifaddr en0 2>/dev/null || echo "")}
    [ -n "$MAC_IP" ] || fail "cannot determine the Mac's LAN address (en0)"
    if mutate "serve $REL_NAME and run hm-release install --package … --activate"; then
        serve_dir /tmp/hm-provision-http2.pid "$(dirname "$RELEASE")" "$(( SERVE_PORT + 1 ))" "$REL_NAME" \
            || fail "could not serve $REL_NAME on any port from $(( SERVE_PORT + 1 ))"
        log "  serving $REL_NAME on :$SERVE_PORT"
        OUT=$(dev "/data/hm/bin/hm-release install --package http://$MAC_IP:$SERVE_PORT/$REL_NAME --activate 2>&1 | busybox tail -12" 120)
        stop_serve /tmp/hm-provision-http2.pid
        [ -n "$OUT" ] && log "$OUT"
        # The verdict is the NODE's, not this console's: /release reports what
        # the installer committed and whether it rolled back. A silent console
        # read is "unknown", never "failed" (dot2-c 2026-11-15: the install
        # had already succeeded and reported healthy, while the console was
        # still re-enumerating after the seed reboot).
        _i=0
        _ver=""
        while [ "$_i" -lt 60 ]; do
            _ver=$(active_version)
            [ "$_ver" = "$REL_VER" ] && break
            _i=$(( _i + 1 ))
            sleep 5
        done
        if [ "$_ver" = "$REL_VER" ]; then
            log "  active on the node: $REL_VER (previous: $(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/release" | python3 -c 'import json,sys; s=json.load(sys.stdin).get("state") or {}; p=s.get("previous") or {}; print(p.get("id") or "none")' 2>/dev/null))"
        else
            log "  installer log (last lines):"
            last_release_line
            fail "activation did not land $REL_VER (node reports active '${_ver:-none}') — the installer log above says why; on an empty machine a rollback here means the new firmware never restarted"
        fi
    fi
fi

step "health (design §5 criterion 5)"
# Nothing installed means nothing to health-check. A dry run on a clean node
# must not fail here (its whole point is "show me what would happen").
if [ "$DRY" = "1" ]; then
    skip "dry run — nothing was installed, so there is nothing to health-check"
elif ! wait_http "http://$IP:8930/api/v0/status" 2; then
    fail "health has no node to ask: the core does not answer and an install was expected by now"
else
# A spotter that is still loading is not a broken node: it reports `starting`
# for ~7 s after any deploy or boot. Wait for that specific state instead of
# failing the provisioning on our own restart (dot2-c 2026-11-15).
# An EMPTY read is not health either (codex review 2026-11-15 Q1d: the old
# loop broke on empty output and the sole rejection check only looked for the
# word FAIL, so a dead console read as a healthy node).
TRIES=0
while :; do
    HEALTH=$(dev "/data/hm/bin/hm-release health 2>&1 | busybox tail -10" 20)
    case "$HEALTH" in
        *'kws.state="starting"'*) TRIES=$(( TRIES + 1 )) ;;
        *"PASS"*|*"FAIL"*) break ;;
        *) TRIES=$(( TRIES + 1 )) ;;   # no verdict at all: retry, never pass
    esac
    [ "$TRIES" -ge 8 ] && break
    log "  no complete verdict yet — waiting 5 s (${TRIES}/8)"
    sleep 5
done
log "$HEALTH"
case "$HEALTH" in
    *FAIL*) fail "health has a failing criterion (above)" ;;
    *healthy*) log "  health 7/7 (device's own report)" ;;
    *) fail "health unverified: the console returned no verdict. Activation did land (see /release), but this run does not claim a healthy node — reconcile the serial console, then re-run (it is a no-op)" ;;
esac
fi

# ── 8. Pairing. The token exists exactly once, in this response: it is printed
#      to the terminal and deliberately NOT written to the log (the log is the
#      artifact that gets pasted into reports). It IS written to the operator's
#      token file (0600) and used for the rest of this run: the node requires a
#      bearer the moment a token exists (v0.23), so continuing without it made
#      the final snapshot read "core: not answering" (codex review Q1c/Q5).
step "pairing code (design §2.2 step 8)"
if mutate "POST /api/v0/auth/provision (409 = already paired, anything else is a failure)"; then
    _body=$(mktemp)
    _code=$(curl -s -m 10 -o "$_body" -w '%{http_code}' -X POST "http://$IP:8930/api/v0/auth/provision" 2>/dev/null || echo 000)
    case "$_code" in
        409)
            log "  this node is already paired — tokens live on the operator's machine, not in this log"
            ;;
        200)
            TOKEN=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("token",""))' "$_body" 2>/dev/null)
            if [ -z "$TOKEN" ]; then
                fail "the node answered 200 but no token was in the body — the node may now be paired with a token nobody has; rotate it from the console (POST /api/v0/auth/rotate) before continuing"
            fi
            log "  paired: token issued once, printed below, NOT written to $LOG"
            say ""
            say "  ┌─ Home Mate pairing token for $DEVICE_ID (shown once) ────────────"
            say "  │ $TOKEN"
            say "  └─ also saved to ~/.home-mate/$DEVICE_ID.token (0600)"
            say ""
            log "  (token redacted from this log by design)"
            mkdir -p "$HOME/.home-mate"
            ( umask 077; printf '%s' "$TOKEN" > "$HOME/.home-mate/$DEVICE_ID.token" )
            TOKEN_FILE="$HOME/.home-mate/$DEVICE_ID.token"
            chmod 600 "$TOKEN_FILE" 2>/dev/null
            log "  token file written; the remaining steps authenticate with it"
            ;;
        *)
            rm -f "$_body"
            fail "pairing did not answer (HTTP ${_code}) — 409 means already paired, 000 means the request never reached the node; do not report a node as paired on a lost response"
            ;;
    esac
    rm -f "$_body"
fi

# ── 8b. Household configuration, and this node's own wake word. A fresh node
#      that stops here runs the CODE defaults: `asr_wake_fallback=true` (it
#      answers anything said in the room), the close-window tone off, and the
#      same wake word as the reference speaker — so one sentence wakes two
#      nodes (measured dot2a, 2026-11-15). The household's own values live on
#      a working node; M12a-1 exists to move them. The wake word is the one
#      thing that must NOT move (per-node by design), so it is asked for.
step "household configuration + this node's wake word (2026-11-15)"
NODE_DEV=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/mode" \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(((d.get("devices") or [{}])[0].get("device")) or "")' 2>/dev/null || true)
if [ -z "$NODE_DEV" ]; then
    NODE_DEV=$(curl -sf -m 5 -H "$(auth_hdr)" "http://$IP:8930/api/v0/status" \
        | python3 -c 'import json,sys; d=json.load(sys.stdin); print(((d.get("gateway") or {}).get("devices") or [{}])[0].get("stable_id") or "")' 2>/dev/null || true)
fi
log "  node device: ${NODE_DEV:-unknown}"

if [ -z "$REF_NODE" ]; then
    skip "no --reference-node: this node keeps the code defaults (asr_wake_fallback, follow_window_tone, command words) until someone pushes the household configuration"
elif [ -z "$REF_IP" ]; then
    fail "--reference-node $REF_NODE needs --reference-ip (the Mac must reach it over the LAN)"
elif [ -z "$REF_TOKEN_FILE" ] || [ ! -f "$REF_TOKEN_FILE" ]; then
    fail "no token for reference node $REF_NODE (expected ~/.home-mate/$REF_NODE.token or --reference-token-file). The fleet API requires it"
else
    # The reference must be new enough to have the fleet API at all.
    REF_STATUS=$(curl -sf -m 5 -H "$(ref_hdr)" "http://$REF_IP:8930/api/v0/status" 2>/dev/null || true)
    [ -n "$REF_STATUS" ] || fail "reference node $REF_IP:8930 does not answer with $REF_NODE's token — wrong IP, wrong token file, or the node is down"
    log "  reference: $REF_NODE at $REF_IP ($(printf '%s' "$REF_STATUS" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["build"]["id"])' 2>/dev/null))"
    REF_HAS_FLEET=$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "$(ref_hdr)" "http://$REF_IP:8930/api/v0/fleet" 2>/dev/null || echo 000)
    [ "$REF_HAS_FLEET" = "200" ] || fail "reference node does not expose /api/v0/fleet (HTTP $REF_HAS_FLEET) — it must run release 39 or newer to push configuration"

    NODE_TOKEN=$(cat "$TOKEN_FILE" 2>/dev/null || true)
    [ -n "$NODE_TOKEN" ] || fail "this node's token is not in $TOKEN_FILE — a re-run on a node that was paired elsewhere must be given --token-file"

    PEER_EXISTS=$(fleet_peer_registered "$REF_IP" "$DEVICE_ID")
    if [ "$DRY" = "1" ]; then
        log "  WOULD: register $DEVICE_ID on $REF_NODE (http://$IP:8930) and push the household configuration (include_secrets=true)"
        if [ "$PEER_EXISTS" = "1" ]; then
            fleet_plan_preview "$REF_IP" "$DEVICE_ID"
        else
            log "  (no plan preview: $DEVICE_ID is not registered there yet, and a dry run must not register it)"
        fi
    else
        _body=$(mktemp)
        _code=$(curl -s -m 15 -o "$_body" -w '%{http_code}' -X PUT -H "$(ref_hdr)" -H 'content-type: application/json' \
            --data-binary "$(python3 -c 'import json,sys; print(json.dumps({"base_url": sys.argv[1], "token": sys.argv[2]}))' "http://$IP:8930" "$NODE_TOKEN")" \
            "http://$REF_IP:8930/api/v0/fleet/$DEVICE_ID" 2>/dev/null || echo 000)
        [ "$_code" = "200" ] || { cat "$_body"; rm -f "$_body"; fail "registering $DEVICE_ID on $REF_NODE answered HTTP $_code"; }
        rm -f "$_body"
        log "  registered: $REF_NODE → $DEVICE_ID (http://$IP:8930); its token is stored sealed there"

        PING=$(curl -s -m 15 -X POST -H "$(ref_hdr)" -H 'content-type: application/json' \
            --data-binary "{\"ids\":[\"$DEVICE_ID\"]}" "http://$REF_IP:8930/api/v0/fleet/ping" 2>/dev/null || true)
        printf '%s' "$PING" | python3 -c 'import json,sys; d=json.load(sys.stdin); r=(d.get("results") or [{}])[0]; print("  ping:", r.get("id"), "ok" if r.get("ok") else "FAILED", r.get("build") or r.get("error") or "")' 2>/dev/null \
            || log "  ping: no readable answer ($PING)"

        # A dry run of the push is read-only on both sides; printing the plan
        # before the real send is what makes "what will change" observable.
        fleet_plan_preview "$REF_IP" "$DEVICE_ID"

        PUSH=$(curl -s -m 60 -X POST -H "$(ref_hdr)" -H 'content-type: application/json' \
            --data-binary "{\"targets\":[\"$DEVICE_ID\"],\"include_secrets\":true,\"dry_run\":false}" \
            "http://$REF_IP:8930/api/v0/fleet/push" 2>/dev/null || true)
        PUSH_OK=$(printf '%s' "$PUSH" | python3 -c 'import json,sys; d=json.load(sys.stdin); r=(d.get("results") or [{}])[0]; print("1" if r.get("ok") else "0")' 2>/dev/null || echo 0)
        printf '%s' "$PUSH" | python3 -c 'import json,sys
d=json.load(sys.stdin); r=(d.get("results") or [{}])[0]
print("  pushed:", len(r.get("applied") or []), "block(s); actor on the node:", r.get("actor"))
for w in (r.get("warnings") or []): print("   note:", w)
if r.get("error"): print("   ERROR:", r["error"])' 2>/dev/null || log "  push: no readable answer ($PUSH)"
        [ "$PUSH_OK" = "1" ] || fail "the household push to $DEVICE_ID failed (see above) — this node is running but NOT configured like the house"
        log "  household configuration applied (this node's config history records actor=fleet:$REF_NODE)"
    fi
fi

# The wake word is per node: a push deliberately never carries one, so a new
# node keeps the code default (你好高达) — the same sentence the reference
# speaker answers to.
if [ -z "$WAKE_WORD" ]; then
    skip "no --wake-word: this node keeps the default wake word — if it is the same as $REF_NODE's, one sentence will wake both"
elif [ -z "$NODE_DEV" ]; then
    fail "cannot set a wake word: the node did not report a device id"
else
    if [ -n "$REF_NODE" ] && [ "$ALLOW_SAME_WAKE" != "1" ]; then
        REF_WAKE=$(curl -sf -m 5 -H "$(ref_hdr)" "http://$REF_IP:8930/api/v0/mode" \
            | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin).get("wake_words") or []))' 2>/dev/null || true)
        case " $REF_WAKE " in
            *" $WAKE_WORD "*) fail "wake word '$WAKE_WORD' is already used by $REF_NODE in this house — two nodes would answer the same sentence. Pick another (or --allow-same-wake-word if that is really what you want)" ;;
        esac
        log "  wake word '$WAKE_WORD' does not collide with $REF_NODE's (${REF_WAKE:-none})"
    fi
    if mutate "set this node's wake word to '$WAKE_WORD' (mode.wake_words + the wake entry in the reflex table) and deploy it"; then
        _enc=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$WAKE_WORD")
        TOK=$(curl -sf -m 8 -H "$(auth_hdr)" "http://$IP:8930/api/v0/reflex/tokens?text=$_enc" \
            | python3 -c 'import json,sys; print(json.load(sys.stdin).get("tokens") or "")' 2>/dev/null || true)
        [ -n "$TOK" ] || fail "the node could not spell '$WAKE_WORD' into device tokens — pick a word it can pronounce (see /api/v0/reflex/tokens)"
        # Keep every non-wake word, replace the wake entry (or add one).
        WORDS=$(curl -sf -m 8 -H "$(auth_hdr)" "http://$IP:8930/api/v0/reflex" \
            | python3 -c 'import json,sys
new_text, new_tok = sys.argv[1], sys.argv[2]
d = json.load(sys.stdin)
words = [w for w in (d.get("words") or []) if w.get("intent") != "wake"]
words.insert(0, {"text": new_text, "tokens": new_tok, "intent": "wake", "enabled": True})
print(json.dumps({"words": words, "deploy": True, "device": sys.argv[3]}))' "$WAKE_WORD" "$TOK" "$NODE_DEV" 2>/dev/null || true)
        [ -n "$WORDS" ] || fail "could not read the node's reflex table"
        DEPLOYED=$(curl -s -m 30 -X PUT -H "$(auth_hdr)" -H 'content-type: application/json' --data-binary "$WORDS" \
            "http://$IP:8930/api/v0/reflex" 2>/dev/null || true)
        printf '%s' "$DEPLOYED" | python3 -c 'import json,sys
d=json.load(sys.stdin)
print("  reflex:", [w.get("text") for w in (d.get("words") or [])])
dep=d.get("deploy") or {}
print("  deploy:", dep.get("state") or dep.get("detail") or "no deploy receipt")' 2>/dev/null || log "  reflex: no readable answer ($DEPLOYED)"
        MODE_PUT=$(curl -s -m 10 -X PUT -H "$(auth_hdr)" -H 'content-type: application/json' \
            --data-binary "$(python3 -c 'import json,sys; print(json.dumps({"wake_words": [sys.argv[1]]}))' "$WAKE_WORD")" \
            "http://$IP:8930/api/v0/mode/$NODE_DEV" 2>/dev/null || true)
        printf '%s' "$MODE_PUT" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("  core wake words:", d.get("wake_words"))' 2>/dev/null || log "  mode: no readable answer ($MODE_PUT)"
        log "  this node answers to '$WAKE_WORD' (device-side reflexes + the core list)"
    fi
fi

# ── 9. What remains needs a person. Say so instead of claiming success.
step "human checks that remain (design §5 criteria 7-8)"
log "  - say the wake word; the ring should answer (needs someone in the room)"
log "  - console: http://$IP:8930 — eight pages, pair with the token above"
log "  - physical mute button, volume keys"

# ── 10. The dry-run proof: the same three facts as the start, after everything
#       that would have changed them. Identical = the node was not touched.
snapshot "after"
if [ "$DRY" = "1" ]; then
    log ""
    log "DRY RUN complete. Compare 'snapshot: before' with 'snapshot: after' above:"
    log "  they must name the same core build, the same active/previous release and"
    log "  the same /data/hm mtime. Nothing was written to the device."
    log ""
    log "DONE (dry run). Log: $LOG"
else
    log ""
    log "DONE (automated part). Log: $LOG"
fi
