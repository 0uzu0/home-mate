#!/usr/bin/env python3
"""Synthesize a test phrase with the SAME TTS the product uses (Volcano
seed-tts-2.0), so the device's microphone hears a standard voice instead of
macOS `say`.

Why: on 2026-11-12 the user pointed out that a `say`-synthesized "停一下。"
is not standard Mandarin — the ASR heard "听一下。" (first syllable lost),
which would look like a product defect when it is a stimulus defect. Every
acoustic acceptance run must use the production voice.

Usage:
  tools/tts_phrase.py "你好，高达。" data/acoustic/phrases/wake_ni_gaoda_volc.wav
  tools/tts_phrase.py "停一下。"   data/acoustic/phrases/cmd_stop_volc.wav

Key: VOLC_API_KEY from the environment, else data/env.local (same file the
device uses). Voice: --voice, else $HM_TTS_VOICE, else the production default.
"""
import argparse
import base64
import json
import os
import struct
import sys
import urllib.request

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
URL = "https://openspeech.bytedance.com/api/v3/tts/unidirectional"
RESOURCE = "seed-tts-2.0"
DEFAULT_VOICE = "zh_female_xiaohe_uranus_bigtts"
OK = 20000000


def api_key() -> str:
    key = os.environ.get("VOLC_API_KEY", "").strip()
    if key:
        return key
    path = os.path.join(HERE, "data/env.local")
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line.startswith("VOLC_API_KEY="):
                    return line.split("=", 1)[1].strip()
    raise SystemExit("VOLC_API_KEY not found (env or data/env.local)")


def synth(text: str, voice: str, rate: int = 16000) -> bytes:
    payload = json.dumps({
        "user": {"uid": "home-mate-b"},
        "req_params": {
            "text": text,
            "speaker": voice,
            "audio_params": {"format": "pcm", "sample_rate": rate},
        },
    }).encode()
    req = urllib.request.Request(
        URL, data=payload,
        headers={
            "Content-Type": "application/json",
            "X-Api-Key": api_key(),
            "X-Api-Resource-Id": RESOURCE,
        },
    )
    pcm = bytearray()
    with urllib.request.urlopen(req, timeout=30) as r:
        for line in r:
            line = line.strip()
            if not line:
                continue
            frame = json.loads(line)
            code = frame.get("code", 0)
            if code not in (0, OK):
                raise SystemExit(f"volc code={code} message={frame.get('message')}")
            data = frame.get("data")
            if data:
                pcm += base64.b64decode(data)
    return bytes(pcm)


def write_wav(path: str, pcm: bytes, rate: int) -> None:
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "wb") as f:
        data = len(pcm)
        f.write(b"RIFF" + struct.pack("<I", 36 + data) + b"WAVEfmt ")
        f.write(struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16))
        f.write(b"data" + struct.pack("<I", data) + pcm)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("text")
    ap.add_argument("out")
    ap.add_argument("--voice", default=os.environ.get("HM_TTS_VOICE", DEFAULT_VOICE))
    ap.add_argument("--rate", type=int, default=16000)
    a = ap.parse_args()
    pcm = synth(a.text, a.voice, a.rate)
    if not pcm:
        raise SystemExit("empty synthesis")
    write_wav(a.out, pcm, a.rate)
    ms = len(pcm) // 2 * 1000 // a.rate
    print(f"{a.out}: {len(pcm)} bytes, {ms} ms, voice={a.voice}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
