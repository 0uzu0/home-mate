#!/usr/bin/env python3
"""Generate the follow-window end tone (stage C UX).

16 kHz mono s16le WAV, two gentle descending notes (880→660 Hz) with short
fades — "the conversation window closed". Output: <repo>/data/assets/
follow-window-end.wav (same assets dir the play paths read).
"""
import math
import os
import struct
import sys

SR = 16000


def note(freq_hz: float, ms: int, peak: float, fade_ms: int = 15) -> list[int]:
    n = SR * ms // 1000
    fade = SR * fade_ms // 1000
    out = []
    for i in range(n):
        env = 1.0
        if i < fade:
            env = i / fade
        if i > n - fade:
            env = (n - i) / fade
        out.append(int(peak * env * math.sin(2 * math.pi * freq_hz * i / SR) * 32767))
    return out


def main() -> None:
    samples = note(880, 110, 0.25) + [0] * (SR * 35 // 1000) + note(660, 130, 0.25)
    pcm = struct.pack(f"<{len(samples)}h", *samples)
    hdr = b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt " + struct.pack(
        "<IHHIIHH", 16, 1, 1, SR, SR * 2, 2, 16) + b"data" + struct.pack("<I", len(pcm))
    out = os.path.join(os.path.dirname(__file__), "..", "..", "data", "assets", "follow-window-end.wav")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "wb") as f:
        f.write(hdr + pcm)
    print(f"wrote {os.path.normpath(out)} ({len(hdr + pcm)} bytes, {len(samples) * 1000 // SR} ms)")


if __name__ == "__main__":
    sys.exit(main())
