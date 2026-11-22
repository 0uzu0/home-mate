# R1e pause acceptance: a long answer plays, real speech lands next to the
# device, and the words have to survive the barge-in path. The 250 ms
# post-barge guard exists to drop our OWN stopped answer (stop flush + DAC
# ring + room reverb), but it also ate the first syllable of whatever the user
# said at that moment — 「暂停」 became 「停」, which is the DISCARD intent.
#
# This probe measures the transcript, not an opinion: the device's own ASR
# text, the intent that fired, and the bookmark (or its absence) afterwards.
# The room preflight is 10 s of CONTINUOUS quiet (the user's rule): one quiet
# instant next to a conversation is not a quiet room.
#
# Usage: pause_probe.py [--delay-ms N] [--phase pause|resume|both]
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
DEFAULT_STIM = "/tmp/hm-pause"
CAMERA_OUT = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
MAC_OUT = "Mac Studio扬声器"

INTENTS = (
    "barge_in_stop", "barge_in_ignored", "stopped_locally", "barge_in_tail_dropped",
    "barge_in_head_kept", "barge_in_preroll_held",
    "mic_utterance", "mic_wake", "mic_ignored_unwoken",
    "pause_intent", "program_paused", "program_pause_noop", "program_resumed",
    "program_finished", "program_interrupted", "stop_intent", "continue_intent",
    "play_finished", "mic_suppressed_self_echo", "tts_streamed", "turn_end",
)


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def mode():
    return get("/api/v0/mode")


def dev():
    return mode()["devices"][0]


def select_camera():
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False, stdout=subprocess.DEVNULL)


def select_mac():
    subprocess.run([SWITCH, "-s", MAC_OUT], check=False, stdout=subprocess.DEVNULL)


def play(wav):
    """Play one stimulus through the camera speaker next to the device. The
    output device is selected ONCE up front: switching it inside a timed step
    used to add ~14 s of unexplained delay to the timeline."""
    subprocess.run(["afplay", wav], check=False)


def voice_play_live():
    for o in dev().get("outputs") or []:
        if o.get("channel") == "voice" and o.get("stage") in (
            "accepted", "streaming", "device_confirmed", "playing"
        ):
            return o
    return None


def quiet(seconds=10.0):
    """The user's preflight: in_speech false AND mean_db < -50 dBFS for a
    CONTINUOUS stretch, with the segment counter not moving (a quiet gap in a
    conversation passes an instant check and fails this one)."""
    deadline = time.time() + seconds + 20.0
    streak_start = None
    base = None
    while time.time() < deadline:
        m = dev()["mic"]
        mean = (m.get("levels") or {}).get("mean_db")
        segs = m.get("counters", {}).get("segments", 0)
        ok = (not m.get("in_speech")) and mean is not None and mean < -50
        if ok:
            if streak_start is None:
                streak_start = time.time()
                base = segs
            elif segs != base:
                # Somebody talked inside the window: start over.
                streak_start = time.time()
                base = segs
            elif time.time() - streak_start >= seconds:
                return True
        else:
            streak_start = None
        time.sleep(0.5)
    return False


def events_since(ts_ms):
    return [e for e in get("/api/v0/voice/log")["events"] if e.get("ts_ms", 0) > ts_ms]


def interesting(evs):
    return [e for e in evs if e.get("kind") in INTENTS]


def program_state():
    d = dev()
    return {"live": d.get("program", {}).get("live"), "paused": d.get("program", {}).get("paused")}


def now_ms():
    """Pacing clock. The Mac and the core share the same wall clock (checked:
    the device `date` and the core's event ts agree to the second), and the
    event ring has no "now" — the newest ts stops advancing when nothing
    happens, which is exactly when a pacing loop must not trust it."""
    return int(time.time() * 1000)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--stim", default=DEFAULT_STIM,
                    help="directory with story.wav / pause.wav / resume.wav")
    ap.add_argument("--story", default=None)
    ap.add_argument("--phase", default="pause", choices=["pause", "resume", "both"])
    ap.add_argument("--delay-ms", type=int, default=6000, help="answer age before the pause")
    ap.add_argument("--gap-ms", type=int, default=3000, help="silence between pause and resume")
    args = ap.parse_args()
    args.story = args.story or os.path.join(args.stim, "story.wav")
    pause_wav = os.path.join(args.stim, "pause.wav")
    resume_wav = os.path.join(args.stim, "resume.wav")

    if not quiet(10.0):
        print("room not quiet for 10 s — refusing to play")
        return 2

    select_camera()
    t0 = now_ms()
    print(f"stimulus: story ({args.story})")
    play(args.story)

    deadline = time.time() + 25.0
    started = None
    while time.time() < deadline:
        o = voice_play_live()
        if o:
            started = o
            break
        time.sleep(0.2)
    if not started:
        print("no voice playback started — record the failure and stop")
        print(json.dumps(interesting(events_since(t0)), ensure_ascii=False)[:2000])
        select_mac()
        return 3
    print(f"answer live: {json.dumps(started, ensure_ascii=False)[:200]}")

    # Pace from the moment the answer's audio was live (this clock and the
    # device's agree; the event ring's newest ts does not advance while idle).
    live_ms = now_ms()
    target = live_ms + args.delay_ms
    while now_ms() < target:
        time.sleep(0.2)

    t_pause = now_ms()
    age = t_pause - live_ms
    print(f"pause stimulus at answer age {age} ms: {pause_wav}")
    play(pause_wav)
    time.sleep(7.0)
    after_pause = interesting(events_since(t_pause))
    print("--- after 暂停 ---")
    print(json.dumps(after_pause, ensure_ascii=False, indent=1))

    result = {
        "answer_live_ms": live_ms,
        "after_pause": after_pause,
        "program_after_pause": program_state(),
    }

    if args.phase in ("resume", "both"):
        if not quiet(8.0):
            print("room not quiet for the resume leg")
            result["resume_skipped"] = "room not quiet"
        else:
            time.sleep(args.gap_ms / 1000)
            t_res = now_ms()
            print(f"resume stimulus: {resume_wav}")
            play(resume_wav)
            time.sleep(9.0)
            result["after_resume"] = interesting(events_since(t_res))
            result["program_after_resume"] = program_state()
            print("--- after 继续 ---")
            print(json.dumps(result["after_resume"], ensure_ascii=False, indent=1))

    select_mac()
    print(json.dumps({"summary": result}, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
