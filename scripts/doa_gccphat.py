# 7-mic TDOA spike: GCC-PHAT over the device's own 9-channel dump (D/DOA).
#
# Input is the raw track the firmware writes during POST
# /api/v0/diagnostics/tests/mics: 9 interleaved s16 channels at 16 kHz, ch0–6
# are the mic array, ch7/ch8 the two loopback scalings (not mics).
#
# What this measures: the delay of arrival between every mic pair, via the
# PHAT-weighted cross spectrum (robust to level differences and to the room
# coloration a plain cross-correlation peak would sit on). Self-play is the
# CONTROL: the speaker is essentially equidistant from every mic, so a
# correct pipeline must read ≈0 for every pair. Off-axis human data is what
# turns this into a direction estimate — that needs the array geometry and a
# person to speak, so it is out of scope for the spike (deliberately).
#
# Usage: doa_gccphat.py <mics.pcm> [--channels 0-6] [--max-lag-samples 32]
import math
import sys

import numpy as np

RATE = 16000
SOUND = 343.0  # m/s
CH_NAMES = [f"ch{i}" for i in range(9)]


def load(path, channels=9):
    raw = np.fromfile(path, dtype="<i2")
    n = raw.size // channels
    return raw[: n * channels].reshape(n, channels).astype(np.float64) / 32768.0


def gcc_phat(a, b, max_lag):
    """Delay of b relative to a, in samples, via PHAT-weighted cross spectrum."""
    n = 1 << int(math.ceil(math.log2(a.size + b.size)))
    A = np.fft.rfft(a - a.mean(), n)
    B = np.fft.rfft(b - b.mean(), n)
    R = A * np.conj(B)
    mag = np.abs(R)
    mag[mag < 1e-12] = 1e-12
    cc = np.fft.irfft(R / mag, n)
    # Search both directions; keep the circular layout consistent.
    cc = np.concatenate([cc[-max_lag:], cc[: max_lag + 1]])
    lag = int(np.argmax(np.abs(cc))) - max_lag
    # Parabolic interpolation around the peak for sub-sample resolution.
    idx = int(np.argmax(np.abs(cc)))
    if 0 < idx < cc.size - 1:
        y0, y1, y2 = np.abs(cc[idx - 1]), np.abs(cc[idx]), np.abs(cc[idx + 1])
        denom = y0 - 2 * y1 + y2
        if abs(denom) > 1e-12:
            lag = lag + 0.5 * (y0 - y2) / denom
    peak = float(np.abs(cc).max() / (np.abs(cc).sum() + 1e-12))
    return lag, peak


def main() -> int:
    path = sys.argv[1]
    max_lag = 32
    if "--max-lag-samples" in sys.argv:
        max_lag = int(sys.argv[sys.argv.index("--max-lag-samples") + 1])
    x = load(path)
    print(f"{path}: {x.shape[0]} frames ({x.shape[0]/RATE:.2f}s), {x.shape[1]} channels")
    mics = list(range(7))
    print(f"\nper-channel RMS (dBFS): " +
          "  ".join(f"{CH_NAMES[c]}={20*math.log10(np.sqrt((x[:,c]**2).mean())+1e-12):.1f}" for c in range(9)))

    print("\nGCC-PHAT pair lags (samples / µs / cm), self-play control:")
    lags = {}
    for i in range(len(mics)):
        for j in range(i + 1, len(mics)):
            a, b = mics[i], mics[j]
            lag, peak = gcc_phat(x[:, a], x[:, b], max_lag)
            lags[(a, b)] = lag
            us = lag / RATE * 1e6
            cm = lag / RATE * SOUND * 100
            print(f"  {CH_NAMES[a]:>4}-{CH_NAMES[b]:<4} lag={lag:+7.2f} samp  {us:+8.1f} µs  {cm:+7.2f} cm  peak={peak:.4f}")

    vals = np.array(list(lags.values()))
    print(f"\nself-play summary: n={len(vals)} pairs  max|lag|={np.abs(vals).max():.2f} samp "
          f"({np.abs(vals).max()/RATE*1e6:.1f} µs, {np.abs(vals).max()/RATE*SOUND*100:.2f} cm)  "
          f"mean={vals.mean():+.2f}")
    # Geometry-free verdict: near-field self-play must not look like a direction.
    ok = np.abs(vals).max() <= 4.0
    print(f"verdict: {'near-field (all pairs ≈ 0) — pipeline sane' if ok else 'unexpected spread: check wiring/geometry'}")
    if x.shape[1] >= 9:
        lag, peak = gcc_phat(x[:, 7], x[:, 8], max_lag)
        print(f"ch7-ch8 (loopback, NOT mics): lag={lag:+.2f} samp peak={peak:.4f} — "
              f"same signal, two digital scalings (expected 0)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
