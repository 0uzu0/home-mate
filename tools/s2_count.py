#!/usr/bin/env python3
"""Snapshot the acceptance counters (monotonic per core epoch).

The voice log ring keeps only ~100 events, so a 10-attempt ladder can age out
of it. These counters cannot: sample before and after a phase and diff.

Usage: s2_count.py [--base URL] [--label TEXT]
"""
import argparse
import json
import urllib.request


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--label", default="")
    args = ap.parse_args()
    with urllib.request.urlopen(args.base + "/api/v0/mode", timeout=8) as r:
        mode = json.loads(r.read())
    d = mode["devices"][0]
    c = d["mic"]["counters"]
    kws = d["kws"]
    e = d["mic"]["vad"]["endpoint"].get("stats") or {}
    out = {
        "label": args.label,
        "core_epoch": d["kws"]["core_epoch"],
        "segments": c["segments"],
        "speech_started": c["speech_started"],
        "wakes": c["wakes"],
        "ignored_unwoken": c["ignored_unwoken"],
        "followup_turns": c["followup_turns"],
        "asr_failed": c["asr_failed"],
        "close_cap": c["close_cap"],
        "kws_detections": kws["detections"],
        "kws_ignored": kws["ignored"],
        "silero_frames": e.get("frames"),
        "silero_speech_frames": e.get("speech_frames"),
        "floor_db": d["mic"]["vad"]["noise_db"],
        "state": d["assistant_state"],
    }
    print(json.dumps(out, ensure_ascii=False))


if __name__ == "__main__":
    main()
