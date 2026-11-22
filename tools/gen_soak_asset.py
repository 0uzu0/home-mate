#!/usr/bin/env python3
"""Generate the KWS soak asset: 28 s of INSTRUMENTAL audio, no speech.

Why a generated asset instead of a song: the media soak asks "does the device
hear its own playback as the wake word?" — a track with vocals answers a
different question (its lyrics can legitimately sound like 你好高达). The R1g
run used a file that was not kept, so the recipe lives here now.

Usage:
  tools/gen_soak_asset.py data/assets/soak-music.wav
  # then copy it to <device>:/data/hm/data/assets/ and run
  # HM_ASSET=soak-music.wav python3 <gateway>/tools/kws_soak.py 1800 --channel media
"""
import math
import struct
import sys
import wave

RATE = 16000
SECONDS = 28.0
# Root-fifth-octave triads, two seconds each: reads as music, contains no
# formants a spotter could mistake for speech.
CHORDS = [(220.0, 330.0, 440.0), (246.9, 370.0, 493.9), (196.0, 293.7, 392.0), (261.6, 392.0, 523.3)]


def main() -> int:
    out = sys.argv[1] if len(sys.argv) > 1 else "data/assets/soak-music.wav"
    frames = bytearray()
    for i in range(int(RATE * SECONDS)):
        t = i / RATE
        bar = int(t / 2.0) % len(CHORDS)
        swell = 0.5 * (1 - math.cos(2 * math.pi * (t % 2.0) / 2.0))
        s = sum((0.32 / (k + 1)) * math.sin(2 * math.pi * f * t + k) for k, f in enumerate(CHORDS[bar]))
        beat = 0.18 * math.sin(2 * math.pi * 90 * t) if (t % 0.5) < 0.08 else 0.0
        s = (s * swell + beat) * 0.55
        frames += struct.pack("<h", int(max(-1.0, min(1.0, s)) * 20000))
    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(bytes(frames))
    print(f"{out}: 1 ch, {RATE} Hz, {SECONDS:.0f} s, no speech")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
