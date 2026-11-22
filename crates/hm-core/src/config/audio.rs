//! The output tone (M6c-2): which fixed voicing plays, plus the household's own
//! peaking bands on top of it.
//!
//! The `echomuse` preset name is part of the wire format (the console sends it
//! and the gateway matches on it) — it is kept as-is.

use serde::{Deserialize, Serialize};

/// M6c-2 (2026-11-14): the output tone — which fixed voicing, plus the
/// user's own peaking bands on top of it.
pub const AUDIO_EQ_KEY: &str = "audio_eq";

/// The EchoMuse biscuit voicing's LINEAR part, for the console's curve
/// drawing only: the gateway owns the real chain (which also has the bass
/// guard and the three compressors, so this is not the whole response).
/// Keep in sync with `crates/gateway/src/firmware/loudness.rs` (EM_*).
pub const VOICING_REF: [(f32, f32, f32); 5] = [
    (65.0, -25.0, 4.0),
    (174.9, 26.13, 0.388),
    (576.3, -10.93, 0.735),
    (3277.5, -10.94, 0.753),
    (8707.8, 5.52, 2.985),
];

/// One peaking band of the user's EQ (the same numbers the gateway applies).
/// The graphic editor uses ten octave-spaced centres (31.5 Hz … 16 kHz, Q 1.4);
/// the API accepts up to ten so the console can send exactly those.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBandCfg {
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// The output tone: a fixed voicing plus the user's taste EQ on top.
///
/// `curve` picks the voicing: `echomuse` (the EchoMuse biscuit voicing we
/// ported), `official` (the stock EQ_50 shape fitted from the device),
/// `rui` (the legacy ladder), `custom` (the bands ARE the curve).
///
/// The default is the sound chosen by ear on the device on 2026-11-14:
/// EchoMuse's voicing with a -6 dB bass / +4 dB presence / +2 dB air taste
/// EQ. It exists so a fresh install does not start with the muffled stock
/// shape or the thin flat one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioEqConfig {
    pub curve: String,
    #[serde(default)]
    pub bands: Vec<EqBandCfg>,
}

impl Default for AudioEqConfig {
    fn default() -> Self {
        AudioEqConfig {
            curve: "echomuse".into(),
            bands: vec![
                EqBandCfg {
                    freq: 175.0,
                    gain_db: -6.0,
                    q: 0.6,
                },
                EqBandCfg {
                    freq: 3500.0,
                    gain_db: 4.0,
                    q: 1.2,
                },
                EqBandCfg {
                    freq: 8000.0,
                    gain_db: 2.0,
                    q: 1.0,
                },
            ],
        }
    }
}

pub const AUDIO_CURVES: [&str; 4] = ["echomuse", "official", "rui", "custom"];

impl AudioEqConfig {
    /// What the gateway would clamp anyway is refused here instead, so the
    /// console shows the error rather than a silently different curve.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !AUDIO_CURVES.contains(&self.curve.as_str()) {
            return Err(format!(
                "curve '{}' is not one of {:?}",
                self.curve, AUDIO_CURVES
            ));
        }
        if self.bands.len() > 10 {
            return Err(format!("at most 10 bands (got {})", self.bands.len()));
        }
        if self.curve == "custom" && self.bands.is_empty() {
            return Err("the custom curve needs at least one band".into());
        }
        for (i, b) in self.bands.iter().enumerate() {
            if !(20.0..=16000.0).contains(&b.freq) {
                return Err(format!("band {i}: freq {} outside 20..16000 Hz", b.freq));
            }
            if !(-18.0..=18.0).contains(&b.gain_db) {
                return Err(format!("band {i}: gain {} outside -18..18 dB", b.gain_db));
            }
            if !(0.3..=8.0).contains(&b.q) {
                return Err(format!("band {i}: Q {} outside 0.3..8", b.q));
            }
        }
        Ok(())
    }
}
