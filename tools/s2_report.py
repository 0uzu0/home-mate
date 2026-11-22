#!/usr/bin/env python3
"""Acceptance-window reducer: turn the voice log into a per-attempt table.

One spoken wake attempt can become: a Silero segment (core path) and/or a
KWS `keyword_wake` (spotter path); the product-level fact is whether the
device ACKNOWLEDGED (`wake_ack` / `wake_word_open`). Printing all three
separately is the point — "detector opened" and "wake confirmed" are
different facts and must not be merged.

Usage: s2_report.py [--base URL] [--seconds 300]
"""
import argparse
import json
import sys
import time
import urllib.request


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--seconds", type=float, default=300)
    args = ap.parse_args()
    with urllib.request.urlopen(args.base + "/api/v0/voice/log?limit=200", timeout=8) as r:
        events = json.loads(r.read())["events"]
    now = time.time() * 1000
    rows = []
    for e in reversed(events):
        age = (now - e["ts_ms"]) / 1000.0
        if age > args.seconds:
            continue
        k = e["kind"]
        if k not in ("mic_segment_closed", "keyword_wake", "wake_word_open", "wake_ack",
                     "mic_utterance", "mic_wake_echo_in_window", "keyword_ignored",
                     "mic_ignored_unwoken", "turn_end"):
            continue
        rows.append((age, k, e))
    segs = [e for a, k, e in rows if k == "mic_segment_closed"]
    kw = [e for a, k, e in rows if k == "keyword_wake"]
    asr_wake = [e for a, k, e in rows if k == "wake_word_open"]
    acks = [e for a, k, e in rows if k == "wake_ack"]
    utt = [e for a, k, e in rows if k == "mic_utterance"]
    print(f"window {args.seconds:.0f}s: silero_segments={len(segs)} keyword_wake={len(kw)} "
          f"asr_wake={len(asr_wake)} wake_ack={len(acks)} asr_transcripts={len(utt)}")
    texts = {}
    for e in utt:
        t = e.get("text", "")
        texts[t] = texts.get(t, 0) + 1
    for t, n in sorted(texts.items(), key=lambda x: -x[1])[:6]:
        print(f"   transcript x{n}: {t}")
    for e in utt[:3]:
        pass
    # Segment duration spread: a ladder that works shows ~1.1-1.3 s blocks.
    durs = sorted(e.get("duration_ms", 0) for e in segs)
    if durs:
        print(f"   segment durations ms: min {durs[0]} median {durs[len(durs)//2]} max {durs[-1]}")


if __name__ == "__main__":
    sys.exit(main())
