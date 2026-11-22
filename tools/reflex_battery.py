#!/usr/bin/env python3
"""R1j/P1 false-trigger battery: non-command speech must not fire a word.

Plays ordinary speech (the `nonwx_*` stimuli: everyday sentences that contain
none of the reflex words) through the room speaker and counts per-word hits on
the device. Two phases:

  idle   nothing playing — the room, the TV or a person talking
  media  our own media playback is live — the echo path is in the loop

Criteria (RJ-5): 0 hits of any word across >=20 non-command utterances; the
wake word is reported separately (a wake hit on non-command speech is also a
false trigger, and it is counted, not excused).

Usage:
  tools/reflex_battery.py [--base URL] [--reps 8] [--media-asset erle-stimulus.wav]
"""
import argparse
import json
import subprocess
import threading
import time
import urllib.request

PHRASES = "data/acoustic/phrases"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
CAMERA_OUT = "2K USB Camera-Audio"
MAC_OUT = "Mac Studio扬声器"
STIMULI = ["nonwx_chat1.wav", "nonwx_chat2.wav", "nonwx_chat3.wav"]


def get(base, path):
    with urllib.request.urlopen(base + path, timeout=8) as r:
        return json.load(r)


def post(base, path, body, timeout=120):
    req = urllib.request.Request(
        base + path,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def counters(base):
    mode = get(base, "/api/v0/mode")
    dev = mode["devices"][0]
    words = {w["text"]: (w["hits"], w["ignored"]) for w in dev["reflex"]["words"]}
    mic = dev.get("mic", {})
    return {
        "words": words,
        "segments": (mic.get("counters") or {}).get("segments", 0),
        "kws": {k: (dev.get("kws") or {}).get(k, 0) for k in ("detections", "guarded", "restarts")},
    }


def say(path):
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False, capture_output=True)
    try:
        subprocess.run(["afplay", path], check=False)
    finally:
        subprocess.run([SWITCH, "-s", MAC_OUT], check=False, capture_output=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--reps", type=int, default=8, help="reps per stimulus (3 stimuli)")
    ap.add_argument("--media-asset", default="erle-stimulus.wav")
    ap.add_argument("--skip-media", action="store_true")
    args = ap.parse_args()
    base = args.base.rstrip("/")

    def report(label, before, after):
        print(f"\n[{label}] segments +{after['segments'] - before['segments']}")
        worst = 0
        for text, (hits, _) in after["words"].items():
            b = before["words"].get(text, (0, 0))
            d = hits - b[0]
            print(f"  {text:8s} hits +{d}")
            worst = max(worst, d)
        d_guard = after["kws"]["guarded"] - before["kws"]["guarded"]
        print(f"  guarded +{d_guard}")
        return worst

    # Phase 1: idle room, ordinary speech.
    before = counters(base)
    # Let the previous turn's tail blind window expire before measuring.
    time.sleep(3)
    for i in range(args.reps):
        for s in STIMULI:
            say(f"{PHRASES}/{s}")
            time.sleep(2.0)
    time.sleep(2)
    after = counters(base)
    worst_idle = report("idle", before, after)

    worst_media = 0
    if not args.skip_media:
        stop = threading.Event()

        def play_loop():
            while not stop.is_set():
                try:
                    post(base, "/api/v0/diagnostics/tests/play_asset",
                         {"asset": args.media_asset, "channel": "media"}, timeout=120)
                except Exception:
                    pass
                time.sleep(1)

        before = counters(base)
        t = threading.Thread(target=play_loop, daemon=True)
        t.start()
        time.sleep(4)
        for i in range(2):
            for s in STIMULI:
                say(f"{PHRASES}/{s}")
                time.sleep(2.0)
        stop.set()
        time.sleep(3)
        after = counters(base)
        worst_media = report("media", before, after)

    total = worst_idle + worst_media
    print(
        f"\nRESULT {'PASS' if total == 0 else 'FAIL'}: non-command hits idle={worst_idle} media={worst_media}"
    )
    return 0 if total == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
