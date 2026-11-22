#!/usr/bin/env python3
"""R1j/P0-a on-device check: the waiting cue must never run under the answer.

The 2026-11-12 21:53 defect: the cue ("我看一下。") was playing, the answer's
first audio was submitted, `cancel_filler` only SENT a stop (no receipt wait),
and each play flushes only itself on the device — so the cue's tail overlapped
the answer's first syllable (stop latency 34-158 ms).

This tool drives the real device the way the user did (production TTS voice
through the room speaker: wake phrase, then a command) and reads the core's
event ring. A turn PASSES when, for the cue of that turn:

  * `filler_cancelled.stop_sent` is true,
  * `filler_stop_settled.settled` is true (the device confirmed the flush),
  * the device's `stop_stage` for the cue is NOT after the answer's first
    `play_stage` (the audio-submission fact), i.e. no overlap window.

Usage:
  tools/p0a_cue_overlap.py [--base http://192.168.1.73:8930] [--turns 4]

Stimuli (production voice, synthesized with tools/tts_phrase.py):
  data/acoustic/phrases/wake_ni_gaoda_volc.wav
  data/acoustic/phrases/cmd_children_light_off_volc.wav
"""
import argparse
import json
import subprocess
import sys
import time
import urllib.request

HERE = __file__.rsplit("/tools/", 1)[0]
PHRASES = HERE + "/data/acoustic/phrases"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
CAMERA_OUT = "2K USB Camera-Audio"
MAC_OUT = "Mac Studio扬声器"
WAKE = "wake_ni_gaoda_volc.wav"
COMMAND = "cmd_children_light_off_volc.wav"

# Events that matter for the overlap proof, in arrival order.
KINDS = {
    "filler_armed",
    "filler_played",
    "filler_skipped",
    "filler_cancelled",
    "filler_stop_settled",
    "stop_stage",
    "play_stage",
    "play_finished",
    "tts_played",
    "tts_stream_begin_failed",
    "turn_start",
    "turn_end",
}


def say_it(name):
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False, capture_output=True)
    try:
        subprocess.run(["afplay", f"{PHRASES}/{name}"], check=False)
    finally:
        subprocess.run([SWITCH, "-s", MAC_OUT], check=False, capture_output=True)


def ts_of(e):
    """Voice-log entries carry `ts_ms`; device-origin events also carry their
    own `ts` (the DEVICE clock). Always prefer `ts_ms` — one clock per tool."""
    v = e.get("ts_ms")
    return v if isinstance(v, (int, float)) else e.get("ts", 0)


def voice_log(base):
    with urllib.request.urlopen(f"{base}/api/v0/voice/log", timeout=5) as r:
        return json.load(r).get("events", [])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://192.168.1.73:8930")
    ap.add_argument("--turns", type=int, default=4)
    ap.add_argument("--settle", type=float, default=6.0, help="idle time before each turn")
    args = ap.parse_args()
    base = args.base.rstrip("/")

    seen = set()
    scenes = []
    for turn in range(1, args.turns + 1):
        time.sleep(args.settle)
        before = voice_log(base)
        mark = max((ts_of(e) for e in before), default=0)
        print(f"\n=== turn {turn}: wake → 关闭儿童房的灯 ===")
        say_it(WAKE)
        time.sleep(1.0)
        say_it(COMMAND)
        # A tool round-trip + streamed TTS answer; give the turn room.
        time.sleep(18)
        events = [e for e in voice_log(base) if e.get("kind") in KINDS]
        fresh = [e for e in events if ts_of(e) > mark]
        # dedupe by (kind, ts, play_id) — the ring is bounded and overlapping.
        keyed = []
        for e in fresh:
            key = (e.get("kind"), ts_of(e), e.get("play_id"), e.get("stage"))
            if key not in seen:
                seen.add(key)
                keyed.append(e)
        scenes.append(keyed)

    ok = 0
    checked = 0
    for i, evs in enumerate(scenes, 1):
        print(f"\n--- turn {i} ---")
        cue = None
        for e in evs:
            if e["kind"] == "filler_played":
                cue = e.get("play_id")
        if not cue:
            print("  no cue played in this turn (answer beat the deadline or a cue was skipped)")
            continue
        checked += 1
        t = {e["kind"]: e for e in evs}
        cancel = t.get("filler_cancelled")
        settled = t.get("filler_stop_settled")
        stop = next(
            (e for e in evs if e["kind"] == "stop_stage" and e.get("play_id") == cue), None
        )
        cue_start = t.get("filler_played")
        # The ANSWER is the first voice play submitted after the cue started
        # (the wake ack, if any, came before it).
        answer = next(
            (
                e
                for e in evs
                if e["kind"] == "play_stage"
                and e.get("play_id") != cue
                and e.get("stage") in ("submitted", "accepted", "sent")
                and cue_start
                and ts_of(e) > ts_of(cue_start)
            ),
            None,
        )
        for e in evs:
            if e["kind"] in ("filler_armed", "filler_played", "filler_cancelled", "filler_stop_settled", "filler_skipped"):
                print(f"  {e['kind']:22s} ts={ts_of(e)} {json.dumps({k: v for k, v in e.items() if k not in ('kind', 'ts')}, ensure_ascii=False)}")
            if e["kind"] == "stop_stage" and e.get("play_id") == cue:
                print(f"  stop_stage(cue)        ts={ts_of(e)} stage={e.get('stage')} played_ms={e.get('played_ms')}")
        if answer:
            print(f"  answer play_stage      ts={ts_of(answer)} stage={answer.get('stage')} play={answer.get('play_id')}")
        # The cue is silent at its stop receipt, or after it played through.
        # Either way the answer must not have been submitted before that.
        played_out = next(
            (e for e in evs if e["kind"] == "play_finished" and e.get("play_id") == cue), None
        )
        cue_end = stop or played_out
        if cancel:
            # The cancellation path is the one the defect lived in: the stop
            # must have been SENT and the device must have confirmed the
            # flush before the answer could start.
            good = bool(cancel.get("stop_sent")) and bool(settled and settled.get("settled"))
        else:
            # No cancel: the cue ended on its own. Legal only if it ended
            # before the answer's first audio — otherwise it overlapped.
            good = True
        good = good and cue_end is not None and answer is not None and ts_of(cue_end) <= ts_of(answer)
        if cancel and settled:
            print(
                f"  cancel→settled: {ts_of(settled) - ts_of(cancel)} ms "
                f"(waited_ms={settled.get('waited_ms')}, detail={settled.get('detail')})"
            )
        if stop and answer:
            print(f"  stop_stage→answer: {ts_of(answer) - ts_of(stop)} ms")
        print("  PASS" if good else "  FAIL")
        ok += 1 if good else 0
    print(f"\nRESULT: {ok}/{checked} cue turns prove no overlap (0 cue turns = inconclusive)")
    return 0 if checked and ok == checked else 1


if __name__ == "__main__":
    sys.exit(main())
