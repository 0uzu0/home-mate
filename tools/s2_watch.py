#!/usr/bin/env python3
"""Live acceptance watcher for the S2 human window (dot2-d).

Polls the core and prints ONE line per new voice-log event that matters, so a
spoken attempt can be attributed to a distance/phase by timestamp. Also prints
a periodic counter line so a MISSED attempt shows up as a silent slot.

Usage: s2_watch.py [--base http://192.168.1.73:8930] [--cadence 6] [--out FILE]
"""
import argparse
import json
import time
import urllib.request

KEEP = (
    "mic_segment_closed", "mic_ignored_unwoken", "mic_filler_ignored",
    # The PRODUCT-level wake facts: a KWS hit and the ASR-path wake are
    # different entries, and both must be counted (the first version of this
    # script filtered `keyword_wake` out and lost nine of ten attempts).
    "keyword_wake", "keyword_wake_ignored", "wake_word_open", "wake_ack",
    "mic_utterance", "mic_wake_echo_in_window", "mic_speech_started", "mic_asr_failed",
    "program_paused", "program_delivery", "spoken_terminal", "turn_end",
    "program_resumed", "pause_intent", "turn_budget_exceeded",
    "transcript_stale_after_epoch", "transcript_out_of_order",
    "keyword_ignored", "program_sentence_unheard", "keyword_wake_ignored",
    "follow_window", "follow_window_closed",
)


def get(base, path):
    with urllib.request.urlopen(base + path, timeout=8) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--cadence", type=float, default=6.0)
    ap.add_argument("--out", default="/tmp/s2-window.log")
    args = ap.parse_args()
    base = args.base
    seen = set()
    t0 = time.time()
    slot = 0
    last_slot_t = t0
    f = open(args.out, "a", buffering=1)

    def emit(line):
        print(line, flush=True)
        f.write(line + "\n")

    emit(f"# watch start {time.strftime('%H:%M:%S')} cadence {args.cadence}s")
    while True:
        try:
            events = get(base, "/api/v0/voice/log?limit=200")["events"]
            mode = get(base, "/api/v0/mode")
        except Exception as e:  # noqa: BLE001 - the window must not die on a blip
            time.sleep(1.0)
            continue
        for e in reversed(events):
            key = (e.get("ts_ms"), e.get("kind"), e.get("play_id"), e.get("text"))
            if key in seen:
                continue
            seen.add(key)
            k = e.get("kind")
            if k not in KEEP:
                continue
            el = (e.get("ts_ms", 0) / 1000.0) - t0
            extra = {
                kk: e[kk]
                for kk in ("text", "keyword", "blocks", "duration_ms", "close_reason",
                           "sentences", "consumed_ms", "basis", "phase", "wait_ms",
                           "advanced", "bookmark_kept", "stage", "reason", "input_id")
                if kk in e
            }
            emit(f"[{el:7.1f}s] {k:26s} {json.dumps(extra, ensure_ascii=False)}")
        dev = mode["devices"][0]
        st = dev["mic"]["vad"]["endpoint"].get("stats") or {}
        counters = dev["mic"]["counters"]
        # One counter line every 2 s: a MISSED attempt looks like a slot with
        # no segment, so the raw deltas must be visible.
        if int(time.time() * 2) % 4 == 0:
            emit(
                f"    .. frames={st.get('frames')} speech={st.get('speech_frames')} "
                f"seg={counters.get('segments')} woke={counters.get('wakes')} "
                f"state={dev['assistant_state']}"
            )
        time.sleep(0.3)


if __name__ == "__main__":
    main()
