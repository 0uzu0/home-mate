#!/usr/bin/env python3
"""G4 acoustic calibration toolkit (unattended, 2026-11-07 night session).

Three independent measurements, one tool:

  silence  [secs]    Sample the dot2-d uplink level telemetry (the core's
                     mic level ring, /api/v0/mode) in a quiet room and
                     recommend VAD thresholds (start/end dBFS).
  wake-test [rounds] Play wake / non-wake phrases through the "2K USB
                     Camera" speaker → dot2-d hears → core wakes (or must
                     not). Reports wake rate, false wakes, and per-round
                     evidence from the voice log.
  roundtrip          Prove dot2-d actually EMITS audio without ears: run the
                     core's voice_play probe and reverse-record it on the
                     camera microphone; report the recorded RMS.

The tool never prints secrets; the volcano key is read from data/env.local
(VOLC_API_KEY) or the environment.

Usage (from the repo root, venv in data/venv-acoustic):
  data/venv-acoustic/bin/python scripts/acoustic/calibrate.py silence 60
  data/venv-acoustic/bin/python scripts/acoustic/calibrate.py gen-phrases
  data/venv-acoustic/bin/python scripts/acoustic/calibrate.py wake-test 3
  data/venv-acoustic/bin/python scripts/acoustic/calibrate.py roundtrip
"""

import argparse
import base64
import json
import os
import sys
import time
import wave
from pathlib import Path

import numpy as np
import requests

REPO = Path(__file__).resolve().parent.parent.parent
PHRASE_DIR = REPO / "data" / "acoustic" / "phrases"
CORE = os.environ.get("HM_CORE", "http://127.0.0.1:8930")
CAMERA = "2K USB Camera"

WAKE_PHRASES = [
    ("wake_greeting", "你好，高达。"),
    ("wake_bare", "高达。"),
    ("wake_command", "你好高达，今天周几？"),
]
NON_WAKE_PHRASES = [
    ("nonwx_chat1", "今天天气不错。"),
    ("nonwx_chat2", "帮我看看明天的日程安排。"),
    ("nonwx_chat3", "随便聊几句吧。"),
]


# --- core API helpers -----------------------------------------------------------


def mode():
    return requests.get(f"{CORE}/api/v0/mode", timeout=5).json()


def voice_log(limit=100):
    return requests.get(f"{CORE}/api/v0/voice/log?limit={limit}", timeout=5).json()["events"]


def put_mode(body):
    r = requests.put(f"{CORE}/api/v0/mode/dot2-d", json=body, timeout=5)
    r.raise_for_status()
    return r.json()


def set_volume(value):
    r = requests.post(
        f"{CORE}/api/v0/devices/dot2-d/volume", json={"value": value}, timeout=5
    )
    r.raise_for_status()


# --- audio devices ---------------------------------------------------------------


def find_camera_devices():
    import sounddevice as sd

    out_idx = in_idx = None
    for i, d in enumerate(sd.query_devices()):
        if CAMERA in d["name"]:
            if d["max_output_channels"] > 0 and out_idx is None:
                out_idx = i
            if d["max_input_channels"] > 0 and in_idx is None:
                in_idx = i
    return out_idx, in_idx


def play_wav(path: Path, gain: float = 1.5, speaker: str = "mac"):
    """Play a WAV audibly. Two routes (measured 2026-11-07 night):

    - "mac" (default): the Mac's own speaker via afplay — the ONLY route
      that produces sustained clean audio near the desk. The "2K USB
      Camera" output emits one ~0.4 s pop then goes silent (broken for
      sustained playback); portaudio additionally SIGSEGVs on it.
    - "camera": switch the system default to the camera output around
      afplay, always restored after (kept for future hardware retest).
    """
    import shutil
    import subprocess

    if shutil.which("afplay") is None:
        raise SystemExit("need afplay on PATH")
    if speaker == "camera":
        if shutil.which("SwitchAudioSource") is None:
            raise SystemExit("need SwitchAudioSource on PATH for --speaker camera")
        prev = subprocess.run(
            ["SwitchAudioSource", "-c"], capture_output=True, text=True
        ).stdout.strip()
        subprocess.run(
            ["SwitchAudioSource", "-t", "output", "-s", CAMERA + "-Audio"],
            capture_output=True,
        )
        try:
            time.sleep(0.4)
            subprocess.run(
                ["afplay", "-v", str(gain), str(path)], check=True, capture_output=True
            )
        finally:
            subprocess.run(
                ["SwitchAudioSource", "-t", "output", "-s", prev], capture_output=True
            )
    else:
        subprocess.run(
            ["afplay", "-v", str(gain), str(path)], check=True, capture_output=True
        )


# --- phrase generation (volcano TTS, key never printed) -------------------------


def volc_key():
    env = REPO / "data" / "env.local"
    if env.exists():
        for line in env.read_text().splitlines():
            if line.startswith("VOLC_API_KEY="):
                return line.split("=", 1)[1].strip()
    return os.environ.get("VOLC_API_KEY", "")


def synth(text: str) -> bytes:
    key = volc_key()
    if not key:
        raise SystemExit("VOLC_API_KEY not found in data/env.local or environment")
    r = requests.post(
        "https://openspeech.bytedance.com/api/v3/tts/unidirectional",
        headers={"X-Api-Key": key, "X-Api-Resource-Id": "seed-tts-2.0"},
        json={
            "user": {"uid": "home-mate-calibration"},
            "req_params": {
                "text": text,
                "speaker": "zh_female_xiaohe_uranus_bigtts",
                "audio_params": {"format": "pcm", "sample_rate": 16000},
            },
        },
        timeout=20,
    )
    r.raise_for_status()
    pcm = b""
    for line in r.text.splitlines():
        line = line.strip()
        if not line:
            continue
        frame = json.loads(line)
        if frame.get("code") not in (0, 20000000):
            raise SystemExit(f"volc error: {frame.get('code')} {frame.get('message')}")
        if frame.get("data"):
            pcm += base64.b64decode(frame["data"])
    if not pcm:
        raise SystemExit("volc returned no audio")
    return pcm


def pcm_to_wav(pcm: bytes, path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(pcm)


def boost_wav(src: Path, dst: Path, target_rms_db: float = -12.0):
    """Amplify with a soft limiter (tanh knee) — afplay -v is a raw linear
    gain that hard-clips and turns speech into unrecognizable garbage
    (measured 2026-11-07: gain 6 transcribed as "啊。")."""
    with wave.open(str(src), "rb") as w:
        sr = w.getframerate()
        data = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16).astype(np.float64) / 32768.0
    rms = np.sqrt(np.mean(data**2)) + 1e-12
    gain = 10 ** (target_rms_db / 20.0) / rms
    boosted = np.tanh(data * gain * 1.2) * 0.95  # soft knee, ceiling below FS
    pcm = (np.clip(boosted, -1.0, 1.0) * 32767).astype(np.int16)
    pcm_to_wav(pcm.tobytes(), dst)


def cmd_gen_phrases(_args):
    for name, text in WAKE_PHRASES + NON_WAKE_PHRASES:
        out = PHRASE_DIR / f"{name}.wav"
        if out.exists():
            print(f"skip (exists): {out.name}")
        else:
            pcm = synth(text)
            pcm_to_wav(pcm, out)
            print(f"synthesized {out.name} ({len(pcm)/32000:.1f}s): {text}")
        boosted = PHRASE_DIR / f"{name}.boost.wav"
        boost_wav(out, boosted)
        print(f"boosted   {boosted.name}")


# --- silence / threshold recommendation ------------------------------------------


def cmd_silence(args):
    secs = args.secs
    print(f"sampling uplink levels for {secs}s (quiet room)…")
    samples = []
    deadline = time.time() + secs
    while time.time() < deadline:
        try:
            levels = mode()["devices"][0]["mic"]["levels"]
            if levels:
                samples.append(levels)
        except (KeyError, IndexError, requests.RequestException):
            pass
        time.sleep(5)
    if not samples:
        raise SystemExit("no level telemetry — is the device online?")
    p95 = max(s["p95_db"] for s in samples)
    mx = max(s["max_db"] for s in samples)
    mean = min(s["mean_db"] for s in samples)  # conservative: the quietest window
    print(f"windows={len(samples)}")
    print(f"quietest-window mean {mean:.1f} | worst p95 {p95:.1f} | worst max {mx:.1f} dBFS")
    # Speech near the device lands well above the floor; keep ≥8 dB trigger
    # margin above the WORST p95 so spikes don't open segments, and 5 dB
    # hysteresis below it (blocks > -80 by construction).
    start = max(p95 + 8.0, -45.0)
    end = start - 5.0
    rec = {"start_db": round(start, 1), "end_db": round(end, 1)}
    print(f"recommended vad: {rec}")
    if args.apply:
        put_mode({"vad": rec})
        print("applied via PUT /mode (live + persisted + in history)")
    else:
        print("re-run with --apply to set it")


# --- wake / false-wake test --------------------------------------------------------


def events_since(ts):
    return [e for e in voice_log() if e.get("ts_ms", 0) > ts]


def cmd_wake_test(args):
    rounds = args.rounds
    missing = [n for n, _ in WAKE_PHRASES + NON_WAKE_PHRASES if not (PHRASE_DIR / f"{n}.wav").exists()]
    if missing:
        raise SystemExit(f"missing phrases {missing} — run gen-phrases first")
    # The assistant may ANSWER out loud when woken; keep it near-silent.
    set_volume(0.08)
    print("device volume set to 0.08 for the test")

    def wait_idle(timeout=45):
        """Wait out the 30 s participation window so the next phrase measures
        WAKE detection, not follow-up semantics (first run cascaded: one wake
        turned every later phrase into a follow-up turn)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                if mode()["devices"][0]["participation"] == "idle":
                    return True
            except (KeyError, IndexError, requests.RequestException):
                pass
            time.sleep(1)
        return False

    wake_hits, wake_total = 0, 0
    false_wakes = 0
    report = []
    for r in range(rounds):
        for name, text in WAKE_PHRASES:
            if not wait_idle():
                report.append((name, text, "skip (conversation still open)", "-", ""))
                continue
            t0 = time.time() * 1000
            play_wav(PHRASE_DIR / f"{name}.boost.wav", gain=args.gain, speaker=args.speaker)
            hit = None
            deadline = time.time() + 8
            while time.time() < deadline:
                evs = events_since(t0)
                hit = next((e for e in evs if e["kind"] == "mic_wake"), None)
                if hit:
                    break
                time.sleep(0.5)
            wake_total += 1
            if hit:
                wake_hits += 1
                report.append((name, text, "WAKE", hit.get("wake"), hit.get("rest")))
            else:
                others = [e["kind"] for e in events_since(t0)]
                report.append((name, text, "miss", "-", f"log:{others[:6]}"))
        for name, text in NON_WAKE_PHRASES:
            if not wait_idle():
                report.append((name, text, "skip (conversation still open)", "-", ""))
                continue
            t0 = time.time() * 1000
            play_wav(PHRASE_DIR / f"{name}.boost.wav", gain=args.gain, speaker=args.speaker)
            time.sleep(6)
            evs = events_since(t0)
            fw = next((e for e in evs if e["kind"] == "mic_wake"), None)
            if fw:
                false_wakes += 1
                report.append((name, text, "FALSE-WAKE", fw.get("wake"), ""))
            else:
                ignored = sum(1 for e in evs if e["kind"] == "mic_ignored_unwoken")
                report.append((name, text, "ignored" if ignored else "no-event", "-", ""))

    print(f"\nwake rate: {wake_hits}/{wake_total}")
    print(f"false wakes: {false_wakes}/{rounds * len(NON_WAKE_PHRASES)}")
    for row in report:
        print("  ", row)
    c = mode()["devices"][0]["mic"]["counters"]
    print("core counters now:", json.dumps(c, ensure_ascii=False))


# --- round-trip (dot2-d audible emission proof) ------------------------------------


def cmd_roundtrip(_args):
    import sounddevice as sd

    _, in_idx = find_camera_devices()
    if in_idx is None:
        raise SystemExit("camera microphone not found")
    sr = 16000
    secs = 6
    print(f"recording {secs}s from camera mic while dot2-d speaks…")
    rec = sd.rec(int(sr * secs), samplerate=sr, channels=1, dtype="float64", device=in_idx)
    time.sleep(0.3)
    r = requests.post(f"{CORE}/api/v0/diagnostics/tests/voice_play", timeout=15)
    probe = r.json()
    sd.wait()
    if not np.any(rec):
        print("BLOCKED: recording is all zeros — macOS microphone permission (TCC)")
        print("denied for this process. Grant mic access to the terminal/host app")
        print("in System Settings → Privacy, then re-run. The wake-test direction")
        print("(camera speaker → dot2-d) is NOT affected by this.")
        return
    rms = float(np.sqrt(np.mean(rec**2)))
    db = 20 * np.log10(max(rms, 1e-9))
    # Speech-free camera noise sits far below; anything > -50 dBFS here is a
    # clear emission signature at typical night distances.
    print(f"probe: {probe.get('stage')} synth={probe.get('synth_ms')}ms total={probe.get('total_ms')}ms")
    print(f"recorded RMS {db:.1f} dBFS ({'signal present' if db > -50 else 'below threshold — check distance'})")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("silence")
    s.add_argument("secs", nargs="?", type=int, default=60)
    s.add_argument("--apply", action="store_true")
    s.set_defaults(fn=cmd_silence)
    g = sub.add_parser("gen-phrases")
    g.set_defaults(fn=cmd_gen_phrases)
    w = sub.add_parser("wake-test")
    w.add_argument("rounds", nargs="?", type=int, default=1)
    w.add_argument("--gain", type=float, default=1.5)
    w.add_argument("--speaker", choices=["mac", "camera"], default="mac",
                   help="camera speaker measured broken for sustained audio 2026-11-07")
    w.set_defaults(fn=cmd_wake_test)
    rt = sub.add_parser("roundtrip")
    rt.set_defaults(fn=cmd_roundtrip)
    args = p.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
