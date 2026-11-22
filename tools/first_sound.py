#!/usr/bin/env python3
"""First-sound baseline (B-M0): spoken question -> first audio out.

Measures, per arm, the gap between the END of the user's speech (the
device's own segment close, logged by the core) and the first voice audio
the core submitted. The two arms are the two `thinking` settings of the
same LLM, same question, so the number is comparable across arms.

Why this clock: the core logs `mic_segment_closed` when the VAD closes the
user's segment and `play_stage accepted` when the device took the play.
Both come from the same process clock, so no Mac/device clock skew enters
the number (D-56 lesson). It is NOT an acoustic measurement (that needs an
external mic); it is the pipeline's own admission-to-first-audio latency.

The stimulus is played through the USB camera speaker (the established
SOP in lessons D-13): switch the Mac output, afplay, switch back.

Usage:
  tools/first_sound.py --stim data/acoustic/phrases/wake_command.wav --repeat 3
  tools/first_sound.py --arms disabled --repeat 5 --no-play   # analyse only

Output: one TSV row per run plus a per-arm median line.
"""

import argparse
import json
import subprocess
import sys
import time
import urllib.request

CAMERA = "2K USB Camera-Audio"
MAC_SPEAKER = "Mac Studio扬声器"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"


def api(base, path, method="GET", body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        base.rstrip("/") + path,
        data=data,
        method=method,
        headers={"Content-Type": "application/json"} if data else {},
    )
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read().decode() or "{}")


def play(path):
    subprocess.run([SWITCH, "-s", CAMERA], check=False,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        subprocess.run(["afplay", path], check=True)
    finally:
        subprocess.run([SWITCH, "-s", MAC_SPEAKER], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def set_thinking(base, value):
    """Set the LLM thinking arm, keeping every other field as it is."""
    services = api(base, "/api/v0/config/services")
    llm = services.get("llm") or {}
    body = {
        "provider": llm.get("provider", ""),
        "base_url": llm.get("base_url", ""),
        "model": llm.get("model", ""),
        "api_key_env": llm.get("api_key_env", ""),
        "voice": llm.get("voice", ""),
        "thinking": value,
    }
    api(base, "/api/v0/config/services/llm", "PUT", body)


def events(base, limit=200):
    return (api(base, f"/api/v0/voice/log?limit={limit}") or {}).get("events", [])


def run_one(base, stim, timeout_s=40.0):
    """Play one stimulus and return the measured latencies."""
    t_play = int(time.time() * 1000)
    play(stim)
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        time.sleep(0.5)
        evs = sorted(events(base, 300), key=lambda e: e.get("ts_ms", 0))
        # This run starts at t_play: an older turn's events must never be
        # used (they would produce 30 s or negative latencies).
        mine = [e for e in evs if e.get("ts_ms", 0) >= t_play]
        turns = [e for e in mine if e.get("kind") == "turn_end"]
        if not turns:
            continue
        turn = turns[0]
        starts = [e for e in mine if e.get("kind") in ("mic_wake", "mic_utterance")
                  and e["ts_ms"] < turn["ts_ms"]]
        if not starts:
            continue
        start = starts[0]
        seg = [e for e in evs
               if e.get("kind") == "mic_segment_closed"
               and t_play - 8000 <= e["ts_ms"] <= start["ts_ms"]]
        if not seg:
            continue
        t_speech_end = seg[-1]["ts_ms"]
        # First voice audio after the utterance, excluding waiting cues.
        filler_ids = {e.get("play_id") for e in mine if e.get("kind") == "filler_played"}
        answer_audio = None
        for e in mine:
            if e.get("kind") != "play_stage" or e.get("stage") != "accepted":
                continue
            if e["ts_ms"] < start["ts_ms"] or e["ts_ms"] > turn["ts_ms"]:
                continue
            if e.get("play_id") in filler_ids:
                continue
            answer_audio = e["ts_ms"]
            break
        cue = [e for e in mine if e.get("kind") == "filler_played"]
        cue_audio = cue[0]["ts_ms"] if cue else None
        first_sound = min(x for x in (answer_audio, cue_audio) if x) if (answer_audio or cue_audio) else None
        return {
            "text": start.get("text") or start.get("rest") or "",
            "speech_end_ms": t_speech_end,
            "first_sound_ms": None if first_sound is None else first_sound - t_speech_end,
            "answer_audio_ms": None if answer_audio is None else answer_audio - t_speech_end,
            "cue": bool(cue),
            "cue_phrase": cue[0].get("phrase") if cue else "",
            "turn_ms": turn.get("duration_ms"),
        }
    return None


def median(xs):
    xs = sorted(x for x in xs if x is not None)
    if not xs:
        return None
    n = len(xs)
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--stim", default="data/acoustic/phrases/wake_command.wav")
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--arms", default="disabled,default",
                    help='"disabled" = thinking off, "default" = "" (provider default)')
    ap.add_argument("--gap", type=float, default=4.0,
                    help="seconds between runs (follow window closes at 30 s; keep the wake fresh)")
    ap.add_argument("--no-play", action="store_true", help="analyse the current log instead of playing")
    args = ap.parse_args()

    arms = [a.strip() for a in args.arms.split(",") if a.strip()]
    saved = (api(args.base, "/api/v0/config/services").get("llm") or {}).get("thinking", "disabled")

    print("arm\trun\tfirst_sound_ms\tanswer_audio_ms\tcue\tturn_ms")
    summary = {}
    try:
        for arm in arms:
            value = "" if arm == "default" else arm
            if not args.no_play:
                set_thinking(args.base, value)
                time.sleep(1.0)
            rows = []
            for i in range(args.repeat):
                if args.no_play:
                    r = None
                else:
                    r = run_one(args.base, args.stim)
                    time.sleep(args.gap)
                if r is None:
                    print(f"{arm}\t{i + 1}\tMISS\t\t\t")
                    continue
                rows.append(r)
                print(
                    f"{arm}\t{i + 1}\t{r['first_sound_ms']}\t{r['answer_audio_ms']}\t"
                    f"{'yes' if r['cue'] else 'no'}\t{r['turn_ms']}\t{r['text'][:24]}"
                )
            summary[arm] = rows
    finally:
        if not args.no_play:
            try:
                set_thinking(args.base, saved)
            except Exception as e:  # noqa: BLE001 - restoring the arm is best effort
                print(f"# WARNING: could not restore thinking={saved!r}: {e}", file=sys.stderr)

    print("\narm\tn\tfirst_sound_med\tanswer_audio_med\tcues")
    for arm, rows in summary.items():
        print(
            f"{arm}\t{len(rows)}\t{median([r['first_sound_ms'] for r in rows])}\t"
            f"{median([r['answer_audio_ms'] for r in rows])}\t"
            f"{sum(1 for r in rows if r['cue'])}/{len(rows)}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
