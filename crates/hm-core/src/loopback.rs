//! ch8 playback-loopback self-check (D/hardware task B, 2026-11-10).
//!
//! The device's 9-channel capture carries the DAC loopback on one channel
//! (ch8). The AEC reference is built from what we WROTE (post-gain,
//! post-loudness); ch8 only aligns the de-alias ring. That premise was
//! established once by a standalone probe; this module turns it into a
//! running diagnostic: the firmware (with `--loopback-probe`) writes the
//! DAC-side samples and the ch8 samples to two tmpfs tracks, and this
//! analysis answers:
//!
//! - does ch8 carry the same signal (normalized correlation peak),
//! - at what lag (in 16 kHz samples),
//! - is it sample-exact within ±2 LSB (the de-alias premise),
//! - are there full-scale runs or silent holes (dropped samples / amp off).
//!
//! Everything is pure functions over PCM slices, so the contract is tested
//! without hardware. A silent capture is a *reported* state (the G6
//! cross-boot ch8 question), never a fabricated pass.

/// One loopback comparison result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoopbackReport {
    pub write_samples: usize,
    pub capture_samples: usize,
    /// Lag of the capture behind the DAC-side track, in samples (16 kHz).
    pub lag_samples: Option<i64>,
    /// Normalized correlation at the best lag (0..1).
    pub correlation: Option<f64>,
    /// Longest run of |capture - write| <= 2 at the best lag.
    pub exact_run: usize,
    /// Samples pinned at full scale (±32767) in each track.
    pub write_clipped: usize,
    pub capture_clipped: usize,
    /// Longest digital-silence run inside the active region (ms) — a dropped
    /// stretch shows up here.
    pub longest_zero_ms: f64,
    /// Human-readable verdict; also the machine state for the API.
    pub verdict: String,
}

/// Compare the DAC-side track with the ch8 track.
///
/// Alignment: take a window after the DAC-side onset, slide it over the
/// capture track (coarse stride then ±stride refine — a 30 s capture at
/// sample resolution would be needlessly slow). A capture that never rises
/// above the noise floor is reported as `capture_silent` (the honest state
/// behind the G6 "ch8 cross-boot silent" observation).
pub fn analyze(write: &[i16], capture: &[i16]) -> LoopbackReport {
    // Full scale is ±32767. Do not call `i16::abs`: `i16::MIN` overflows and
    // panics in debug, and `abs() >= 32767` is only true for `i16::MAX`.
    let clipped = |x: &[i16]| {
        x.iter()
            .filter(|s| **s == i16::MIN || **s == i16::MAX)
            .count()
    };
    let mut report = LoopbackReport {
        write_samples: write.len(),
        capture_samples: capture.len(),
        lag_samples: None,
        correlation: None,
        exact_run: 0,
        write_clipped: clipped(write),
        capture_clipped: clipped(capture),
        longest_zero_ms: 0.0,
        verdict: "insufficient_data".into(),
    };
    let Some(w_onset) = write.iter().position(|s| s.abs() > 200) else {
        report.verdict = "write_silent".into();
        return report;
    };
    let Some(c_onset) = capture.iter().position(|s| s.abs() > 200) else {
        report.verdict = "capture_silent".into();
        return report;
    };
    let n = 1024.min(write.len() - w_onset);
    if n < 256 || capture.len() < n {
        report.verdict = "insufficient_data".into();
        return report;
    }
    let w = &write[w_onset..w_onset + n];
    let w_energy: f64 = w
        .iter()
        .map(|&s| (s as f64) * (s as f64))
        .sum::<f64>()
        .sqrt()
        .max(1.0);

    // Coarse-to-fine search. Start no earlier than 100 ms before the
    // capture onset (the loopback cannot lead the DAC write by more).
    // A periodic test tone has many near-equal correlation peaks, so the
    // coarse pass keeps its top 3 candidates and refines each at sample
    // resolution — refining only the single coarse winner can land on a
    // side lobe one period away (caught by the unit tests).
    let start = c_onset.saturating_sub(1600);
    let stride = 8usize;
    let mut coarse: Vec<(usize, f64)> = Vec::new();
    let mut p = start;
    while p + n <= capture.len() {
        let c = &capture[p..p + n];
        let dot: f64 = w
            .iter()
            .zip(c)
            .map(|(&a, &b)| (a as f64) * (b as f64))
            .sum();
        let ce: f64 = c
            .iter()
            .map(|&s| (s as f64) * (s as f64))
            .sum::<f64>()
            .sqrt()
            .max(1.0);
        coarse.push((p, dot / (w_energy * ce)));
        p += stride;
    }
    coarse.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut best_pos = coarse.first().map(|c| c.0).unwrap_or(start);
    let mut best_corr = coarse.first().map(|c| c.1).unwrap_or(0.0);
    for &(cp, _) in coarse.iter().take(3) {
        let lo = cp.saturating_sub(stride);
        let hi = (cp + stride).min(capture.len().saturating_sub(n));
        for q in lo..=hi {
            let c = &capture[q..q + n];
            let dot: f64 = w
                .iter()
                .zip(c)
                .map(|(&a, &b)| (a as f64) * (b as f64))
                .sum();
            let ce: f64 = c
                .iter()
                .map(|&s| (s as f64) * (s as f64))
                .sum::<f64>()
                .sqrt()
                .max(1.0);
            let corr = dot / (w_energy * ce);
            if corr > best_corr {
                best_corr = corr;
                best_pos = q;
            }
        }
    }
    report.lag_samples = Some((best_pos as i64 - w_onset as i64) + 0);
    report.correlation = Some((best_corr * 1000.0).round() / 1000.0);

    // Sample-exact run at the best lag: the de-alias premise only holds if
    // there is a real run of agreement, not one lucky sample.
    let mut run = 0usize;
    let mut max_run = 0usize;
    for i in 0..n {
        if (capture[best_pos + i] as i32 - w[i] as i32).abs() <= 2 {
            run += 1;
            max_run = max_run.max(run);
        } else {
            run = 0;
        }
    }
    report.exact_run = max_run;

    // Longest digital-zero stretch inside the DAC track's active region
    // (the WHOLE track, not just the correlation window: a dropped stretch
    // later in a long play is exactly what this diagnostic is for).
    let mut zeros = 0usize;
    let mut longest = 0usize;
    for &s in &write[w_onset..] {
        if s == 0 {
            zeros += 1;
            longest = longest.max(zeros);
        } else {
            zeros = 0;
        }
    }
    report.longest_zero_ms = (longest as f64) * 1000.0 / 16_000.0;

    report.verdict = if best_corr < 0.5 {
        "no_relationship".into()
    } else if max_run >= 64 {
        "loopback_intact".into()
    } else if best_corr >= 0.8 {
        "loopback_transformed".into()
    } else {
        "weak_relationship".into()
    };
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize) -> Vec<i16> {
        // Broadband LFSR pseudo-noise, not a sine: a pure tone has
        // near-equal correlation peaks one period apart, which is a
        // property of the SIGNAL, not of the analyzer (the real diagnostic
        // asset is a chirp/noise probe for the same reason).
        let mut lfsr: u32 = 0xACE1_2345;
        (0..n)
            .map(|_| {
                let bit = ((lfsr ^ (lfsr >> 2) ^ (lfsr >> 3) ^ (lfsr >> 5)) & 1) as i16;
                lfsr >>= 1;
                bit * 12000 - 6000
            })
            .collect()
    }

    #[test]
    fn full_scale_samples_are_counted_without_overflowing() {
        let samples = [0, i16::MAX, i16::MIN, 1];
        let r = analyze(&samples, &samples);
        assert_eq!(r.write_clipped, 2);
        assert_eq!(r.capture_clipped, 2);
    }

    #[test]
    fn exact_loopback_with_known_lag_is_reported() {
        let w = tone(2048);
        // Capture = 300 samples of silence + the same signal (sample-exact).
        let mut c = vec![0i16; 300];
        c.extend_from_slice(&w);
        let r = analyze(&w, &c);
        assert_eq!(r.verdict, "loopback_intact", "{r:?}");
        let lag = r.lag_samples.unwrap();
        assert!((lag - 300).abs() <= 8, "lag {lag}");
        assert!(r.correlation.unwrap() > 0.99);
        assert!(r.exact_run >= 1024);
    }

    #[test]
    fn scaled_loopback_is_transformed_not_exact() {
        let w = tone(2048);
        let mut c = vec![0i16; 100];
        c.extend(w.iter().map(|&s| (s as f32 * 0.5) as i16));
        let r = analyze(&w, &c);
        assert_eq!(r.verdict, "loopback_transformed", "{r:?}");
        assert!(r.exact_run < 64);
    }

    #[test]
    fn silent_capture_is_reported_not_passed() {
        let w = tone(2048);
        let c = vec![0i16; 4096];
        let r = analyze(&w, &c);
        assert_eq!(r.verdict, "capture_silent");
        assert!(r.correlation.is_none());
    }

    #[test]
    fn silence_holes_in_the_dac_track_are_measured() {
        let mut w = tone(4096);
        for s in w[1000..3000].iter_mut() {
            *s = 0;
        }
        let mut c = vec![0i16; 200];
        c.extend_from_slice(&w);
        let r = analyze(&w, &c);
        assert!(r.longest_zero_ms >= 100.0, "{r:?}");
    }
}
