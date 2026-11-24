#!/system/bin/sh
# hm-core speaker service supervisor — stage C1 (2026-11-08).
# Same supervision primitives as the audio firmware supervisor
# (deploy/dot2d/emos/start_server.sh in the gateway repo): respawn loop,
# fast-exit swap to the other slot, capped logs (eMMC-light), TERM handling.
# Spawned by start_server.sh after /run/net-up; the audio chain never
# depends on this process (independent failure domains, by design).
#
# Layout: /data/hm/{bin/{core_a,core_b,current},web/,data/,supervisor.log}

# /data/hm/tools (last) holds the seed's busybox applets on an older FireOS
# base that has no toybox (no tail, wc, awk — measured on dot2-c 2026-11-14).
# Inert on emOS: the directory does not exist there.
export PATH=/sbin:/system/bin:/system/xbin:/data/hm/tools

# emOS ships no netd: loopback stays DOWN at boot (verified 2026-11-08) and
# the core's gateway listener binds 127.0.0.1:8932. Idempotent.
ip link set lo up 2>/dev/null || ifconfig lo 127.0.0.1 up 2>/dev/null

DIR=/data/hm
BINDIR=$DIR/bin
# RAM log: /run is the 4 MB tmpfs on emOS (wiped at boot). FireOS has no /run
# and its / is read-only, so the FireOS form of the same node logs to /tmp
# (found on dot2-c 2026-11-14: the hardcoded path made the core unable to
# start at all, because the redirect fails before the binary is exec'd).
if [ -d /run ] && [ -w /run ]; then LOGDIR=/run; else LOGDIR=/tmp; fi
LOG=$LOGDIR/hm-core.log
MAX_LOG=1048576               # 1MB cap: firmware log takes the other half
KEEP_LOG=262144
SUP_LOG=$DIR/supervisor.log
SUP_MAX=65536
SUP_KEEP=32768
MIN_RUNTIME=15
MAX_ATTEMPTS=3

# LAN console, multi-gateway (:8932, Bearer-authenticated for non-loopback)
# and UDP discovery (:8933): emOS INPUT policy is DROP; allow private RFC1918
# and Tailscale subnets idempotently on every supervisor start.
for CIDR in 192.168.0.0/16 10.0.0.0/8 172.16.0.0/12 100.64.0.0/10; do
    iptables -C INPUT -p tcp --dport 8930 -s "$CIDR" -j ACCEPT 2>/dev/null || \
        iptables -I INPUT -p tcp --dport 8930 -s "$CIDR" -j ACCEPT 2>/dev/null
    iptables -C INPUT -p tcp --dport 8932 -s "$CIDR" -j ACCEPT 2>/dev/null || \
        iptables -I INPUT -p tcp --dport 8932 -s "$CIDR" -j ACCEPT 2>/dev/null
    iptables -C INPUT -p udp --dport 8933 -s "$CIDR" -j ACCEPT 2>/dev/null || \
        iptables -I INPUT -p udp --dport 8933 -s "$CIDR" -j ACCEPT 2>/dev/null
done

sup_log() {
    if [ -f "$SUP_LOG" ]; then
        SIZE=$(wc -c < "$SUP_LOG" 2>/dev/null)
    else
        SIZE=0
    fi
    if [ "$SIZE" -gt "$SUP_MAX" ]; then
        tail -c $SUP_KEEP "$SUP_LOG" > "${SUP_LOG}.tmp" 2>/dev/null
        mv "${SUP_LOG}.tmp" "$SUP_LOG" 2>/dev/null
    fi
    UP=""
    if [ -r /proc/uptime ]; then
        read UP _REST < /proc/uptime 2>/dev/null
        UP=${UP%.*}
    fi
    echo "up=${UP}s wall=$(date '+%Y-%m-%d %H:%M:%S') $*" >> "$SUP_LOG"
}

# RAM log cap, same shape as the audio supervisor.
(
    while true; do
        sleep 300
        SIZE=$(wc -c < "$LOG" 2>/dev/null)
        if [ -n "$SIZE" ] && [ "$SIZE" -gt "$MAX_LOG" ]; then
            tail -c $KEEP_LOG "$LOG" > "${LOG}.1" 2>/dev/null
            : > "$LOG"
            echo "[supervisor] log trimmed: ${SIZE} bytes" >> "$LOG"
        fi
    done
) &
TRIM_PID=$!

CHILD=0
trap 'sup_log "term signalled — core supervisor exiting"; kill $CHILD $TRIM_PID 2>/dev/null; wait $CHILD 2>/dev/null; exit 0' TERM INT

# Speaker runtime env: loopback gateway link (firmware dials 127.0.0.1:8932;
# paired LAN Cores auto-promote :8932 to 0.0.0.0 for Bearer-authenticated
# multi-gateway clients unless HM_GATEWAY_LOCK_LOOPBACK=1), LAN-facing console,
# DoH resolver (this network's router does not answer the speaker's DNS),
# HTTP-Date clock bootstrap for cold boots.
export HM_DATA_DIR=$DIR/data
export HM_BIND=0.0.0.0:8930
export HM_GATEWAY_BIND=127.0.0.1:8932
export HM_WEB_DIR=$DIR/web
export HM_RESOLVER=doh
export HM_TIME_BOOTSTRAP=1
# D/CPU task 2026-11-10: cpu0 kernel/network, cpu1 audio firmware, cpu2 core.
# Opt-in inside hm-core: unset/failed = scheduler's choice (logged, not fatal).
export HM_CORE_CPU=2
export RUST_LOG=${RUST_LOG:-hm_core=info,tower_http=info}

[ -e $BINDIR/current ] || ln -sf $BINDIR/core_a $BINDIR/current
attempt=0
round=0
sup_log "boot slot=$(readlink $BINDIR/current 2>/dev/null)"

while true; do
    START_TIME=$(date +%s)
    $BINDIR/current >> $LOG 2>&1 &
    CHILD=$!
    wait $CHILD
    EXIT_CODE=$?
    END_TIME=$(date +%s)
    RUNTIME=$(( END_TIME - START_TIME ))
    LOG_TAIL=$(tail -6 $LOG 2>/dev/null)
    sup_log "exit code=$EXIT_CODE ran=${RUNTIME}s; log tail: $LOG_TAIL"

    if [ $RUNTIME -ge $MIN_RUNTIME ]; then
        attempt=0
        sup_log "exit code=$EXIT_CODE ran=${RUNTIME}s — restarting"
        sleep 2
        continue
    fi

    attempt=$(( attempt + 1 ))
    sup_log "fast-exit ${attempt}/${MAX_ATTEMPTS} code=$EXIT_CODE ran=${RUNTIME}s"
    if [ $attempt -lt $MAX_ATTEMPTS ]; then
        sleep 3
        continue
    fi

    attempt=0
    round=$(( round + 1 ))
    if [ $round -ge 2 ]; then
        # This core supervisor does not launch EchoMuse. The device firmware
        # supervisor still does: start_server.sh sets
        # ECHOMUSE_FALLBACK=/data/local/bin/start_server.echomuse.sh, and
        # server_a/server_b are still installed, so two failed boots still
        # fall back to EchoMuse. That script is removed with the adapter
        # (gateway PR #3 plus the manual on-device install, gateway issue #8).
        # Here the audio chain keeps working without core; retry slowly.
        sup_log "both rounds failed — retrying with backoff"
        sleep 30
        continue
    fi

    CURRENT=$(readlink $BINDIR/current 2>/dev/null)
    case "$CURRENT" in
        core_a) FALLBACK_SLOT=core_b ;;
        core_b) FALLBACK_SLOT=core_a ;;
        *) FALLBACK_SLOT=core_a ;;
    esac
    if [ ! -x "$BINDIR/$FALLBACK_SLOT" ]; then
        FALLBACK_SLOT=core_a
    fi
    ln -sf "$FALLBACK_SLOT" $BINDIR/current
    sup_log "slot swap $CURRENT -> $FALLBACK_SLOT after $MAX_ATTEMPTS fast exits"
done
