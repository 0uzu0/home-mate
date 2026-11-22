#!/usr/bin/env python3
"""Generate the loopback/mic-array probe asset (2s 200 Hz→3.2 kHz chirp).

Broadband and aperiodic so a cross-correlation peak is unambiguous (a pure
tone has near-equal peaks one period apart). Written to
`data/assets/loopback-probe.wav` (gitignored data dir); copy it to the
device's `<data_dir>/assets/` when the console/API needs it.

Usage: scripts/asr/make_probe_asset.py [out_path]
"""
import math
import os
import struct
import sys
import wave

out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "..", "..", "data", "assets", "loopback-probe.wav")
rate, secs = 16000, 2.0
n = int(rate * secs)
f0, f1 = 200.0, 3200.0
k = (f1 / f0) ** (1.0 / secs)
samples = []
for i in range(n):
    t = i / rate
    phase = 2 * math.pi * f0 * ((k ** t - 1) / math.log(k))
    v = 0.25 * math.sin(phase)
    edge = int(0.02 * rate)
    if i < edge:
        v *= i / edge
    if i > n - edge:
        v *= (n - i) / edge
    samples.append(int(max(-1.0, min(1.0, v)) * 32767))
os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
with wave.open(out, "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(b"".join(struct.pack("<h", s) for s in samples))
print(f"wrote {out} ({n} samples, {secs}s)")
