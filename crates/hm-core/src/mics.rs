//! 7-mic array measurement (D/hardware task C, 2026-11-10).
//!
//! Today only the FUSED single uplink is available; per-mic level and
//! arrival-time information is discarded. This module consumes a short 9ch
//! dump (taken by the firmware probe alongside the loopback tracks) and
//! reports what the array actually carries:
//!
//! - per-channel RMS during the probe (level differences between mics),
//! - per-channel floor (tail of the window) and the resulting SNR,
//! - arrival-time difference vs ch0 in 16 kHz samples (62.5 µs/sample,
//!   ≈2.1 cm of extra path), by normalized cross-correlation.
//!
//! This is measurement support only — no beamforming/DOA is implemented,
//! and the report says so. ch8 is the DAC loopback reference on this unit;
//! it is reported separately and must not be read as a "mic".

#[derive(Debug, Clone, serde::Serialize)]
pub struct MicChannelReport {
    pub channel: usize,
    pub rms_db: f64,
    pub floor_db: f64,
    pub snr_db: f64,
    /// Arrival-time difference vs ch0, in 16 kHz samples.
    pub lag_vs_ch0: Option<i64>,
    /// Normalized correlation at that lag (0..1).
    pub corr_vs_ch0: Option<f64>,
}

/// Two channel tracks compared against each other (ch7 vs ch8 answers a
/// concrete question: are they two taps of the same digital playback path,
/// or two different loopbacks?).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelPair {
    pub a: usize,
    pub b: usize,
    pub lag: Option<i64>,
    pub corr: Option<f64>,
    /// Longest run where |b - a| <= 2 at the best lag.
    pub exact_run: usize,
    /// Level difference b vs a in dB.
    pub gain_db: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MicArrayReport {
    pub frames: usize,
    pub channels: usize,
    pub active_ms: f64,
    pub channels_report: Vec<MicChannelReport>,
    /// ch7 vs ch8 (the two non-mic channels) when both are present.
    pub loopback_pair: Option<ChannelPair>,
    pub note: String,
}

/// RMS in dBFS (full scale = 32768).
fn dbfs(frames: &[i16]) -> f64 {
    if frames.is_empty() {
        return -120.0;
    }
    let mean_sq: f64 =
        frames.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / frames.len() as f64;
    if mean_sq <= 0.5 {
        return -120.0;
    }
    10.0 * mean_sq.sqrt().log10() * 2.0 - 20.0 * 32768f64.log10()
}

/// Analyze one interleaved s16le 9ch dump.
pub fn analyze(pcm: &[i16], channels: usize) -> MicArrayReport {
    let channels = channels.max(1);
    let frames = pcm.len() / channels;
    let mut de: Vec<Vec<i16>> = vec![Vec::with_capacity(frames); channels];
    for f in 0..frames {
        for (c, ch) in de.iter_mut().enumerate() {
            ch.push(pcm[f * channels + c]);
        }
    }
    // Active region: from the first frame where ANY channel is loud.
    let onset = (0..frames)
        .find(|&f| (0..channels).any(|c| de[c][f].abs() > 200))
        .unwrap_or(0);
    let active = &de[0][onset..];
    let n_active = active.len();
    // Floor: the last 300 ms (assumed post-play tail; a run with no tail
    // simply reports the programme material's own quiet part — the note
    // says so).
    let tail = (n_active.saturating_sub(4800)).min(n_active);
    let floor_start = tail.max(0);

    let mut out = Vec::with_capacity(channels);
    for c in 0..channels {
        let act = &de[c][onset..];
        let fl = if n_active > 4800 {
            &de[c][onset + floor_start..]
        } else {
            &de[c][onset..onset]
        };
        let r = dbfs(act);
        let fl_db = dbfs(fl);
        let (lag, corr) = if c == 0 {
            (Some(0), Some(1.0))
        } else {
            correlate_lag(&de[0][onset..], &de[c][onset..], 64)
        };
        out.push(MicChannelReport {
            channel: c,
            rms_db: (r * 10.0).round() / 10.0,
            floor_db: (fl_db * 10.0).round() / 10.0,
            snr_db: ((r - fl_db) * 10.0).round() / 10.0,
            lag_vs_ch0: lag,
            corr_vs_ch0: corr.map(|v| (v * 1000.0).round() / 1000.0),
        });
    }
    let loopback_pair = if channels > 8 {
        // Start at the LATER of the two loopback onsets: the loopback path
        // is ~220 ms behind the acoustic path, so anchoring on the mic onset
        // compares two stretches of digital silence (caught live: exact_run
        // 2048 over all-zero data, corr 0.0).
        let on7 = de[7].iter().position(|s| s.abs() > 200).unwrap_or(onset);
        let on8 = de[8].iter().position(|s| s.abs() > 200).unwrap_or(onset);
        let pstart = on7.max(on8).max(onset);
        let (lag, corr) = correlate_lag(&de[7][pstart..], &de[8][pstart..], 64);
        let (mut run, mut max_run) = (0usize, 0usize);
        if let Some(l) = lag {
            let off = 64i64 + l;
            if off >= 0 {
                let n2 = 2048.min(de[7].len().saturating_sub(pstart + off as usize));
                for i in 0..n2 {
                    if (de[7][pstart + i] as i32 - de[8][pstart + off as usize + i] as i32).abs()
                        <= 2
                    {
                        run += 1;
                        max_run = max_run.max(run);
                    } else {
                        run = 0;
                    }
                }
            }
        }
        let ra = dbfs(&de[7][pstart..]);
        let rb = dbfs(&de[8][pstart..]);
        Some(ChannelPair {
            a: 7,
            b: 8,
            lag,
            corr: corr.map(|c| (c * 1000.0).round() / 1000.0),
            exact_run: max_run,
            gain_db: ((rb - ra) * 10.0).round() / 10.0,
        })
    } else {
        None
    };
    MicArrayReport {
        frames,
        channels,
        active_ms: (n_active as f64) * 1000.0 / 16_000.0,
        channels_report: out,
        loopback_pair,
        note: "ch0-ch6 are microphones, ch8 is the DAC loopback reference (not a mic); \
               lags are vs ch0 in 16 kHz samples (62.5 us each). Measurement only — \
               no beamforming/DOA is implemented."
            .into(),
    }
}

/// Normalized cross-correlation over ±`max_lag` samples. Returns
/// (lag, correlation) at the peak; lag > 0 means `b` arrives LATER than `a`.
fn correlate_lag(a: &[i16], b: &[i16], max_lag: i64) -> (Option<i64>, Option<f64>) {
    let n = 2048.min(a.len().min(b.len()).saturating_sub(max_lag as usize * 2));
    if n < 256 {
        return (None, None);
    }
    let start = max_lag as usize;
    let a_seg = &a[start..start + n];
    let mut best = (0i64, f64::MIN);
    for lag in -(max_lag as i64)..=(max_lag as i64) {
        let off = start as i64 + lag;
        if off < 0 || off as usize + n > b.len() {
            continue;
        }
        let b_seg = &b[off as usize..off as usize + n];
        let dot: f64 = a_seg
            .iter()
            .zip(b_seg)
            .map(|(&x, &y)| (x as f64) * (y as f64))
            .sum();
        let ea: f64 = a_seg
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum::<f64>()
            .sqrt()
            .max(1.0);
        let eb: f64 = b_seg
            .iter()
            .map(|&y| (y as f64) * (y as f64))
            .sum::<f64>()
            .sqrt()
            .max(1.0);
        let corr = dot / (ea * eb);
        if corr > best.1 {
            best = (lag, corr);
        }
    }
    (Some(best.0), Some(best.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lfsr(n: usize) -> Vec<i16> {
        // Exponential chirp 200 Hz -> 3.2 kHz over the buffer: aperiodic, so
        // the correlation peak is unique (a short-period pseudo-random
        // sequence aliases at its period and would teach the test the wrong
        // lesson — the real diagnostic asset is a chirp for this reason).
        let rate = 16000.0f64;
        let k = (3200.0f64 / 200.0).powf(1.0 / (n as f64 / rate));
        (0..n)
            .map(|i| {
                let t = i as f64 / rate;
                let phase = 2.0 * std::f64::consts::PI * 200.0 * ((k.powf(t) - 1.0) / k.ln());
                (phase.sin() * 9000.0) as i16
            })
            .collect()
    }

    fn interleave(chans: &[Vec<i16>]) -> Vec<i16> {
        let n = chans[0].len();
        let mut out = Vec::with_capacity(n * chans.len());
        for f in 0..n {
            for c in chans {
                out.push(c[f]);
            }
        }
        out
    }

    #[test]
    fn reports_per_mic_levels_and_delays() {
        let base = lfsr(4096);
        let mut chans: Vec<Vec<i16>> = Vec::new();
        for c in 0..9 {
            // channel c is attenuated by 10% steps and delayed by c samples
            let mut v = vec![0i16; c];
            v.extend(
                base.iter()
                    .map(|&s| (s as f64 * (1.0 - 0.1 * c as f64)) as i16),
            );
            v.truncate(base.len());
            chans.push(v);
        }
        let report = analyze(&interleave(&chans), 9);
        assert_eq!(report.channels_report.len(), 9);
        assert_eq!(report.channels_report[0].lag_vs_ch0, Some(0));
        for c in 1..5 {
            let lag = report.channels_report[c].lag_vs_ch0.unwrap();
            assert!((lag - c as i64).abs() <= 1, "ch{c} lag {lag}");
            assert!(report.channels_report[c].corr_vs_ch0.unwrap() > 0.9);
        }
        // Attenuation shows up as a monotonically lower level.
        assert!(report.channels_report[0].rms_db > report.channels_report[4].rms_db);
    }

    #[test]
    fn empty_or_short_input_is_reported_not_guessed() {
        let r = analyze(&[], 9);
        assert_eq!(r.frames, 0);
        let r2 = analyze(&vec![0i16; 90], 9);
        assert_eq!(r2.frames, 10);
    }
}
