# T4 self-hear battery (story/answer channel): continuous voice playback for
# N minutes while counting the two things that would betray self-hearing.
#
#  1. keyword detections DURING playback — the spotter eats the post-AEC
#     residual; a hit while our own voice is playing is the failure mode T4's
#     zero-self-trigger criterion is about (and unlike the mic pipeline, the
#     spotter is NOT behind the half-duplex gate, so it can see it).
#  2. mic segments right after a playback ends — the tail-blind window's job.
#     Segments during voice playback are structurally impossible (the gate
#     drops the blocks), so the honest measure of "the room heard us" is the
#     tail: a segment opening inside the blind window is counted as
#     tail_suppressed by the pipeline, not as a heard segment.
#
# Usage: t4_selfhear_battery.py [minutes=20] [--channel voice] [--asset NAME]
import json
import os
import sys
import threading
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
OUT = os.environ.get("HM_OUT", "/tmp/hm-t4/selfhear-battery.log")


def post(path, body=None, timeout=120):
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def dev():
    return get("/api/v0/mode")["devices"][0]


def main() -> int:
    minutes = float(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else 20.0
    channel = sys.argv[sys.argv.index("--channel") + 1] if "--channel" in sys.argv else "voice"
    asset = os.environ.get("HM_ASSET", "erle-stimulus.wav")
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    log = open(OUT, "a", buffering=1)
    t0 = time.time()
    start_ms = int(time.time() * 1000)
    log.write(f"# selfhear battery {time.strftime('%Y-%m-%d %H:%M:%S')} minutes={minutes} "
              f"channel={channel} asset={asset}\n")

    d0 = dev()
    c0 = d0["mic"]["counters"]
    k0 = d0.get("kws") or {}
    plays = 0
    play_s = 0.0
    prev_activity = None
    prev_t = t0
    last_note = t0
    tail_segments = 0
    keyword_while_playing = 0
    keyword_idle = 0
    seen_events = set()

    while time.time() - t0 < minutes * 60:
        d = dev()
        activity = d.get("activity")
        if activity in ("speaking", "playing"):
            play_s += time.time() - prev_t
        elif prev_activity in ("speaking", "playing"):
            # Playback just ended: any segment closing within the next 3 s is
            # the tail window's business.
            deadline = time.time() + 3.0
            while time.time() < deadline:
                dd = dev()
                if dd["mic"]["counters"]["segments"] != d["mic"]["counters"]["segments"]:
                    tail_segments += 1
                    break
                time.sleep(0.2)
        prev_activity = activity
        prev_t = time.time()

        # Keep a playback alive (whole-WAV asset; restart when it ends).
        if activity not in ("speaking", "playing"):
            threading.Thread(
                target=lambda: post("/api/v0/diagnostics/tests/play_asset",
                                    {"asset": asset, "channel": channel}, timeout=180),
                daemon=True,
            ).start()
            plays += 1
            time.sleep(1.0)

        # Keyword events since the run started (kind + ts are enough).
        for e in get("/api/v0/voice/log").get("events", []):
            if e.get("ts_ms", 0) < start_ms:
                continue
            key = (e.get("ts_ms"), e.get("kind"))
            if key in seen_events or e.get("kind") not in ("keyword", "keyword_stop", "keyword_ignored"):
                continue
            seen_events.add(key)
            if e.get("kind") == "keyword_ignored" and e.get("reason") == "no_voice_playback":
                keyword_idle += 1
            else:
                keyword_while_playing += 1

        if time.time() - last_note >= 60:
            last_note = time.time()
            c = d["mic"]["counters"]
            log.write(f"t={int(time.time()-t0)}s plays={plays} play_s={play_s:.0f} "
                      f"kw_during_play={keyword_while_playing} kw_idle={keyword_idle} "
                      f"segments=+{c['segments']-c0['segments']} "
                      f"self_echo_suppressed=+{c['self_echo_suppressed']-c0['self_echo_suppressed']} "
                      f"tail_suppressed=+{c['tail_suppressed']-c0['tail_suppressed']}\n")
        time.sleep(2)

    d1 = dev()
    c1 = d1["mic"]["counters"]
    k1 = d1.get("kws") or {}
    log.write(
        f"# done plays={plays} playback_s={play_s:.0f} "
        f"kw_during_play={keyword_while_playing} kw_idle={keyword_idle} "
        f"tail_segments={tail_segments} "
        f"segments=+{c1['segments']-c0['segments']} "
        f"self_echo_suppressed=+{c1['self_echo_suppressed']-c0['self_echo_suppressed']} "
        f"tail_suppressed=+{c1['tail_suppressed']-c0['tail_suppressed']} "
        f"kws_stops={k1.get('stops')}(+{k1.get('stops',0)-k0.get('stops',0)}) "
        f"kws_ignored={k1.get('ignored')}(+{k1.get('ignored',0)-k0.get('ignored',0)})\n"
    )
    print(f"battery done: playback {play_s:.0f}s, keyword_during_play={keyword_while_playing}, "
          f"keyword_idle={keyword_idle}, tail_segments={tail_segments} -> {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
