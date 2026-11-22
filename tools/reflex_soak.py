#!/usr/bin/env python3
"""R1j/P1 soak: count reflex detections while the room is quiet or playing.

Pass criteria (roadmap §0.8):
  RJ-3  empty room 30 min: ZERO detections of every non-wake word.
  RJ-4  media 30 min:      ZERO detections, sidecar restarts 0, firmware RSS flat.

Counters come from the DEVICE's own report (`/api/v0/mode` → kws.detections /
guarded / restarts) plus the core's PER-WORD counters (`reflex.words[*].hits`).
The event ring is bounded, so deltas of monotonic counters are the evidence —
never a tail of log lines.

Usage:
  tools/reflex_soak.py --minutes 30 [--media] [--asset erle-stimulus.wav]
                       [--base http://192.168.1.73:8930] [--out /tmp/reflex-soak.log]
"""
import argparse
import json
import threading
import time
import urllib.request


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


def snapshot(base):
    mode = get(base, "/api/v0/mode")
    dev = mode["devices"][0]
    kws = dev.get("kws") or {}
    reflex = dev.get("reflex") or {}
    words = {w["text"]: {"hits": w["hits"], "ignored": w["ignored"], "intent": w["intent"]}
             for w in reflex.get("words", [])}
    status = get(base, "/api/v0/status")
    fw = (status["gateway"]["devices"] or [{}])[0].get("firmware")
    return {
        "ts": int(time.time() * 1000),
        "detections": kws.get("detections", 0),
        "guarded": kws.get("guarded", 0),
        "restarts": kws.get("restarts", 0),
        "state": kws.get("state"),
        "detail": kws.get("detail"),
        "words": words,
        "firmware": fw,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--minutes", type=float, default=30)
    ap.add_argument("--media", action="store_true", help="keep a media asset playing")
    ap.add_argument("--asset", default="erle-stimulus.wav")
    ap.add_argument("--out", default="/tmp/reflex-soak.log")
    args = ap.parse_args()
    base = args.base.rstrip("/")

    before = snapshot(base)
    log = open(args.out, "a", buffering=1)
    log.write(
        f"# reflex soak {time.strftime('%Y-%m-%d %H:%M:%S')} minutes={args.minutes} "
        f"media={args.media} asset={args.asset} firmware={before['firmware']}\n"
    )
    log.write(f"# before {json.dumps(before, ensure_ascii=False)}\n")
    print(f"soak start: {args.minutes} min, media={args.media}, kws={before['state']}")

    stopped = threading.Event()
    plays = [0]

    def media_loop():
        while not stopped.is_set():
            try:
                post(base, "/api/v0/diagnostics/tests/play_asset",
                     {"asset": args.asset, "channel": "media"}, timeout=120)
            except Exception as e:  # a failed play is a fact, keep the window honest
                log.write(f"# play_asset error: {e}\n")
            plays[0] += 1
            time.sleep(1)

    if args.media:
        threading.Thread(target=media_loop, daemon=True).start()

    start = time.time()
    last = 0
    while time.time() - start < args.minutes * 60:
        time.sleep(5)
        if time.time() - last >= 60:
            last = time.time()
            try:
                s = snapshot(base)
                log.write(f"# t={int(time.time()-start)}s {json.dumps(s, ensure_ascii=False)}\n")
            except Exception as e:
                log.write(f"# snapshot error at t={int(time.time()-start)}s: {e}\n")
    stopped.set()
    time.sleep(2)
    after = snapshot(base)
    log.write(f"# after {json.dumps(after, ensure_ascii=False)}\n")

    d_det = after["detections"] - before["detections"]
    d_guard = after["guarded"] - before["guarded"]
    d_restart = after["restarts"] - before["restarts"]
    print(f"\nwindow {args.minutes} min: detections +{d_det}, guarded +{d_guard}, restarts +{d_restart}")
    worst = 0
    rows = []
    for text, w in after["words"].items():
        b = before["words"].get(text, {"hits": 0, "ignored": 0})
        d = w["hits"] - b["hits"]
        di = w["ignored"] - b["ignored"]
        rows.append((text, w["intent"], d, di))
        if w["intent"] != "wake":
            worst = max(worst, d)
        print(f"  {text:8s} [{w['intent']:8s}] hits +{d} ignored +{di}")
        log.write(f"# word {text} [{w['intent']}] hits +{d} ignored +{di}\n")
    verdict = "PASS" if worst == 0 and d_restart == 0 else "FAIL"
    print(f"\nRESULT {verdict}: non-wake false triggers={worst}, sidecar restarts +{d_restart}")
    log.write(f"# RESULT {verdict} non_wake_false={worst} restarts +{d_restart} "
              f"detections +{d_det} guarded +{d_guard} plays={plays[0]}\n")
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
