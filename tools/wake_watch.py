# R1e user-voice wake verification (the person is the stimulus, not a WAV).
#
# Plays nothing. Watches the device while YOU say 「你好，高达」, and records what
# the ASR actually heard plus the three counters that decide pass/fail:
#   wakes            +1 per accepted wake
#   ignored_unwoken  +1 per transcript with no wake word (a FAILED wake)
#   asr_failed       +1 per ASR error
# A cycle passes when wakes +1 and the other two stay 0.
#
# Spacing is enforced (default 35 s) because a wake inside the follow window
# legitimately becomes a plain follow-up turn instead of a wake.
#
# Usage: wake_watch.py [cycles=5] [gap_s=35] [out=/tmp/hm-r1e/wake-user.log]
import json
import os
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
KEYS = ("wakes", "ignored_unwoken", "asr_failed", "followup_turns", "speech_started", "segments")
WATCH = ("mic_wake", "mic_ignored_unwoken", "mic_asr_failed", "mic_utterance")


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def dev():
    return get("/api/v0/mode")["devices"][0]


def counters():
    c = dev()["mic"]["counters"]
    return {k: c.get(k, 0) for k in KEYS}


def events_since(ts_ms):
    return [e for e in get("/api/v0/voice/log")["events"] if e.get("ts_ms", 0) > ts_ms]


def main() -> int:
    cycles = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    gap = float(sys.argv[2]) if len(sys.argv) > 2 else 35.0
    out = sys.argv[3] if len(sys.argv) > 3 else "/tmp/hm-r1e/wake-user.log"
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)

    rows = []
    last = 0.0
    for c in range(cycles):
        if last:
            wait = gap - (time.time() - last)
            if wait > 0:
                print(f"… {wait:.0f} s until the next allowed attempt (the follow window)")
                time.sleep(wait)
        print(f"[{c + 1}/{cycles}] 请现在说「你好，高达」（说完等 8 秒）")
        before, t0 = counters(), int(time.time() * 1000)
        deadline = time.time() + 20.0
        heard = []
        while time.time() < deadline:
            time.sleep(0.5)
            for e in events_since(t0):
                if e.get("kind") in WATCH and e not in heard:
                    heard.append(e)
            if any(e.get("kind") in ("mic_wake", "mic_ignored_unwoken") for e in heard):
                # Give the wake ack / turn a moment to land, then stop listening.
                time.sleep(1.5)
                heard += [e for e in events_since(t0) if e.get("kind") in WATCH and e not in heard]
                break
        last = time.time()
        after = counters()
        delta = {k: after[k] - before[k] for k in KEYS}
        text = next((e.get("text") for e in heard if e.get("kind") in ("mic_wake", "mic_ignored_unwoken")), None)
        accepted = any(e.get("kind") == "mic_wake" for e in heard)
        ok = delta["wakes"] >= 1 and delta["ignored_unwoken"] == 0 and delta["asr_failed"] == 0
        if not heard:
            verdict = "NO-RESPONSE（20 秒内没有转写：说不出声/太小声/设备不在听）"
        elif ok:
            verdict = "PASS"
        else:
            verdict = "FAIL"
        row = {
            "cycle": c + 1,
            "t_ms": t0,
            "verdict": verdict,
            "accepted": accepted,
            "raw_transcript": text,
            "delta": delta,
            "events": [e.get("kind") for e in heard],
        }
        rows.append(row)
        print(json.dumps(row, ensure_ascii=False))
        if heard:
            print(f"    → {verdict}  原始转写: {text!r}  wakes+{delta['wakes']} "
                  f"ignored_unwoken+{delta['ignored_unwoken']} asr_failed+{delta['asr_failed']}")

    with open(out, "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    passed = sum(1 for r in rows if r["verdict"] == "PASS")
    print(f"\n{passed}/{len(rows)} PASS — wrote {out}")
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
