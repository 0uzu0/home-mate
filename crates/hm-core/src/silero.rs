//! Silero VAD endpoint detector (M1b, session S2).
//!
//! Why this exists: the energy VAD in `mic.rs` decides speech against an
//! ABSOLUTE dBFS calibration (with a one-way floor adaptation that may only
//! raise it). A speaker 3 m away, or a soft voice, therefore sits under the
//! ceiling and never opens a segment — measured 2026-11-13 on device
//! recordings: speech at −35/−40 dBFS p95 over a −64 dBFS room was detected by
//! Silero and missed by the energy VAD (see docs/d/m1b-m5-20261113.md).
//!
//! The model is Silero VAD v4's 16 kHz export (MIT). The 643,854 B artifact is
//! **bit-identical in output** to the 1,806,511 B `silero_vad.onnx` found on the
//! device (which carries an 8 kHz branch as an `If` node tract cannot lower):
//! 130 frames, max |Δprob| = 0.0000, measured 2026-11-13 with onnxruntime.
//!
//! Budget on the device (dot2-d, single core, model resident):
//!   * 2.42 ms CPU per 512-sample (32 ms) frame → 7.6 % of one core,
//!   * +6.2 MB resident for the graph, +2.4 MB scratch (flat over 6000 frames),
//!   * 1.66 s one-off model optimise at startup (mostly `into_optimized`).
//!
//! Failure policy: a missing/unloadable model is not an error the caller has to
//! handle — `SileroDetector` reports it once and the mic processor keeps using
//! the energy VAD for the rest of the process (the rollback the S2 brief asks
//! for: "模型缺失即回退").

use std::sync::Arc;

#[cfg(feature = "silero-vad")]
use tract_onnx::prelude::*;

/// Silero v4 consumes exactly 512 samples (32 ms) at 16 kHz.
pub const WINDOW: usize = 512;

/// RUI's production parameters (documented in docs/d/next-session-s2-20261113.md):
/// threshold 0.5, fixed gain x3, min_speech 200 ms, end_silence 600 ms,
/// preroll 350 ms. The first two live here; the last three are the block
/// counts in `VadConfig` (3 blocks = 240 ms onset, 8 blocks = 640 ms hangover).
pub const DEFAULT_THRESHOLD: f32 = 0.5;
pub const DEFAULT_GAIN: f32 = 3.0;

#[cfg(feature = "silero-vad")]
type Plan = SimplePlan<TypedFact, Box<dyn TypedOp>, TypedModel>;

#[cfg(feature = "silero-vad")]
enum Slot {
    Audio,
    State(usize),
    Const(Tensor),
}

/// The parsed+optimised model, shareable across devices: the graph is read-only,
/// only the LSTM state is per stream.
#[cfg(feature = "silero-vad")]
pub struct SileroModel {
    plan: Plan,
    slots: Vec<Slot>,
    state_shapes: Vec<Vec<usize>>,
    pub window: usize,
    pub model_bytes: usize,
}

#[cfg(not(feature = "silero-vad"))]
pub struct SileroModel {
    pub window: usize,
    pub model_bytes: usize,
}

#[cfg(not(feature = "silero-vad"))]
impl SileroModel {
    pub fn from_path(_path: &std::path::Path) -> Result<Self, String> {
        Err(
            "silero-vad feature disabled in slim build (use gateway vad_speech or energy VAD)"
                .into(),
        )
    }

    pub fn from_bytes(_bytes: &[u8]) -> Result<Self, String> {
        Err(
            "silero-vad feature disabled in slim build (use gateway vad_speech or energy VAD)"
                .into(),
        )
    }

    fn new_stream(&self) -> SileroStream {
        SileroStream {}
    }

    fn prob(&self, _stream: &mut SileroStream, _frame: &[f32]) -> Result<f32, String> {
        Err("silero-vad feature disabled in slim build".into())
    }
}

#[cfg(feature = "silero-vad")]
impl SileroModel {
    pub fn from_path(path: &std::path::Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let parsed = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(bytes))
            .map_err(|e| format!("parse: {e}"))?;
        let optimized = parsed
            .into_optimized()
            .map_err(|e| format!("optimize: {e}"))?;
        let mut facts = Vec::new();
        for i in 0..optimized.inputs.len() {
            let f = optimized
                .input_fact(i)
                .map_err(|e| format!("input {i}: {e}"))?;
            let dims: Vec<usize> = f
                .shape
                .iter()
                .map(|d| d.as_i64().unwrap_or(1).max(0) as usize)
                .collect();
            facts.push((f.datum_type, dims));
        }
        let plan = optimized
            .into_runnable()
            .map_err(|e| format!("lower: {e}"))?;

        // Audio input: the rank-2 f32 tensor with the smallest product; the
        // states are rank-3 f32 and the sample-rate scalar is rank 0.
        let mut audio_idx = None;
        let mut window = 0usize;
        let mut best = usize::MAX;
        for (i, (dt, sh)) in facts.iter().enumerate() {
            if *dt != DatumType::F32 || sh.len() != 2 {
                continue;
            }
            let prod: usize = sh.iter().product();
            if prod < best {
                best = prod;
                window = *sh.last().unwrap_or(&WINDOW);
                audio_idx = Some(i);
            }
        }
        let audio_idx = audio_idx.ok_or_else(|| format!("no rank-2 f32 audio input: {facts:?}"))?;

        let mut slots = Vec::new();
        let mut state_shapes = Vec::new();
        for (i, (dt, sh)) in facts.iter().enumerate() {
            if i == audio_idx {
                slots.push(Slot::Audio);
            } else if *dt == DatumType::F32 {
                state_shapes.push(sh.clone());
                slots.push(Slot::State(state_shapes.len() - 1));
            } else if *dt == DatumType::I64 {
                let t: Tensor = if sh.is_empty() || sh.iter().all(|&d| d == 1) {
                    Tensor::from(16000i64)
                } else {
                    tract_ndarray::ArrayD::<i64>::zeros(tract_ndarray::IxDyn(sh)).into()
                };
                slots.push(Slot::Const(t));
            } else {
                return Err(format!("unsupported input datum {dt:?}"));
            }
        }
        Ok(SileroModel {
            plan,
            slots,
            state_shapes,
            window,
            model_bytes: bytes.len(),
        })
    }

    fn new_stream(&self) -> SileroStream {
        SileroStream {
            states: self
                .state_shapes
                .iter()
                .map(|sh| {
                    let a = tract_ndarray::ArrayD::<f32>::zeros(tract_ndarray::IxDyn(sh));
                    Tensor::from(a)
                })
                .collect(),
        }
    }

    /// One frame in, one speech probability out (state is per stream).
    fn prob(&self, stream: &mut SileroStream, frame: &[f32]) -> Result<f32, String> {
        let mut args: TVec<TValue> = tvec!();
        for slot in &self.slots {
            match slot {
                Slot::Audio => {
                    let mut v = frame.to_vec();
                    v.resize(self.window, 0.0);
                    let t: Tensor =
                        tract_ndarray::Array2::<f32>::from_shape_vec((1, self.window), v)
                            .map_err(|e| e.to_string())?
                            .into();
                    args.push(t.into());
                }
                Slot::State(i) => args.push(stream.states[*i].clone().into()),
                Slot::Const(t) => args.push(t.clone().into()),
            }
        }
        let out = self.plan.run(args).map_err(|e| format!("run: {e}"))?;
        let prob = *out[0]
            .to_array_view::<f32>()
            .map_err(|e| e.to_string())?
            .iter()
            .next()
            .unwrap_or(&0.0);
        for i in 0..stream.states.len() {
            if let Some(o) = out.get(i + 1) {
                let t: Tensor = o.clone().into_tensor();
                stream.states[i].clone_from(&t);
            }
        }
        Ok(prob)
    }
}

#[cfg(feature = "silero-vad")]
struct SileroStream {
    states: Vec<Tensor>,
}

#[cfg(not(feature = "silero-vad"))]
struct SileroStream {}

/// Who computes a frame's speech probability. Production is the ONNX model;
/// `Test` lets the block/segment logic be tested without a 644 KB asset.
enum Scorer {
    Model(Arc<SileroModel>, SileroStream),
    #[cfg(test)]
    Test(Box<dyn FnMut(&[f32]) -> Result<f32, String> + Send>),
}

impl Scorer {
    fn score(&mut self, frame: &[f32]) -> Result<f32, String> {
        match self {
            Scorer::Model(m, stream) => m.prob(stream, frame),
            #[cfg(test)]
            Scorer::Test(f) => f(frame),
        }
    }
}

/// Turns 80 ms mic blocks into Silero frames and answers "is this block speech?".
///
/// The frame hop is the model's own window (512): a 1280-sample block completes
/// two frames and carries 256 samples into the next block, so the decision
/// boundary is smeared by at most one frame (32 ms) — the state machine in
/// `mic.rs` counts blocks (80 ms), which is coarser than that.
pub struct SileroDetector {
    scorer: Scorer,
    pending: Vec<i16>,
    pub threshold: f32,
    pub gain: f32,
    pub frames: u64,
    pub speech_frames: u64,
    pub last_prob: f32,
    /// Last block's maximum frame probability (diagnostics).
    pub last_block_max: f32,
    /// Set once when inference fails; the caller then falls back to energy.
    failure: Option<String>,
}

impl SileroDetector {
    pub fn new(model: Arc<SileroModel>) -> Self {
        let stream = model.new_stream();
        Self {
            scorer: Scorer::Model(model, stream),
            pending: Vec::with_capacity(WINDOW * 2),
            threshold: DEFAULT_THRESHOLD,
            gain: DEFAULT_GAIN,
            frames: 0,
            speech_frames: 0,
            last_prob: 0.0,
            last_block_max: 0.0,
            failure: None,
        }
    }

    /// Deterministic scorer for tests (no model, no tract artefact).
    #[cfg(test)]
    pub fn for_test(score: impl FnMut(&[f32]) -> Result<f32, String> + Send + 'static) -> Self {
        Self {
            scorer: Scorer::Test(Box::new(score)),
            pending: Vec::with_capacity(WINDOW * 2),
            threshold: DEFAULT_THRESHOLD,
            gain: DEFAULT_GAIN,
            frames: 0,
            speech_frames: 0,
            last_prob: 0.0,
            last_block_max: 0.0,
            failure: None,
        }
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Feed one block; true when ANY frame completed inside it crossed the
    /// threshold. An inference failure reports `false` once and latches
    /// `failure()`.
    pub fn block_speech(&mut self, block: &[i16]) -> bool {
        if self.failure.is_some() {
            return false;
        }
        self.pending.extend_from_slice(block);
        let mut speech = false;
        let mut block_max = 0.0f32;
        while self.pending.len() >= WINDOW {
            let frame: Vec<f32> = self.pending[..WINDOW]
                .iter()
                .map(|&s| s as f32 / 32768.0 * self.gain)
                .collect();
            self.pending.drain(..WINDOW);
            match self.scorer.score(&frame) {
                Ok(p) => {
                    self.frames += 1;
                    self.last_prob = p;
                    if p >= self.threshold {
                        self.speech_frames += 1;
                        speech = true;
                    }
                    block_max = block_max.max(p);
                }
                Err(e) => {
                    self.failure = Some(e);
                    self.last_block_max = block_max;
                    return false;
                }
            }
        }
        self.last_block_max = block_max;
        speech
    }

    /// Drop buffered samples and the LSTM state (gated mic, device reset).
    /// The model expects continuity; carrying a half-frame across a gated
    /// stretch would splice unrelated audio into one frame.
    pub fn reset(&mut self) {
        self.pending.clear();
        // A fresh stream zeroes the recurrent state; the plan is shared.
        let fresh = match &self.scorer {
            Scorer::Model(m, _) => Some(Scorer::Model(m.clone(), m.new_stream())),
            #[cfg(test)]
            Scorer::Test(_) => None,
        };
        if let Some(s) = fresh {
            self.scorer = s;
        }
    }

    /// Peak frame probability of the last block (diagnostics).
    pub fn last_block_max(&self) -> f32 {
        self.last_block_max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(level: f32, n: usize) -> Vec<i16> {
        vec![(level * 32767.0) as i16; n]
    }

    /// 1280-sample blocks complete 2 frames and carry 256 samples over: the
    /// 512-sample hop is what makes the NEXT block start at sample 1024, not
    /// 1280 — so block two completes 3 frames, not 2.
    #[test]
    fn a_block_completes_two_frames_and_carries_the_rest() {
        let mut d = SileroDetector::for_test(|f| Ok(if f[0].abs() > 0.01 { 0.9 } else { 0.01 }));
        assert!(!d.block_speech(&block(0.0, 1280)), "silence is not speech");
        assert_eq!(d.frames, 2, "1280 samples = two 512 frames, 256 carried");
        assert!(d.block_speech(&block(0.5, 1280)), "loud is speech");
        assert_eq!(d.frames, 5, "256 carried + 1280 = exactly three frames");
    }

    /// The threshold is the ONLY decision input — a probability just under it
    /// is not speech (RUI's 0.5 production value, not a tuned constant).
    #[test]
    fn the_threshold_is_the_decision() {
        let mut d = SileroDetector::for_test(|_| Ok(0.49));
        assert!(!d.block_speech(&block(0.5, 1280)));
        let mut d = SileroDetector::for_test(|_| Ok(0.50));
        assert!(d.block_speech(&block(0.5, 1280)));
    }

    /// Gain is applied BEFORE the model (RUI's x3): a frame that would be
    /// 0.3 at unity becomes 0.9 at x3. This is the setting that recovered the
    /// quiet wake phrase in the device recordings.
    #[test]
    fn input_gain_is_applied_before_scoring() {
        let mut d = SileroDetector::for_test(|f| Ok(if f[0] > 0.25 { 0.9 } else { 0.1 }));
        d.gain = 1.0;
        assert!(
            !d.block_speech(&block(0.1, 1280)),
            "x1: 0.1 is below the fake knee"
        );
        d.gain = 3.0;
        assert!(d.block_speech(&block(0.1, 1280)), "x3: 0.3 clears it");
    }

    /// An inference error is reported through `failure()` and must not panic
    /// inside the audio path; the mic processor falls back to energy.
    #[test]
    fn an_inference_error_latches_and_stops_deciding() {
        let mut calls = 0;
        let mut d = SileroDetector::for_test(move |_| {
            calls += 1;
            if calls > 1 {
                Err("run: synthetic".to_string())
            } else {
                Ok(0.9)
            }
        });
        // One 512-sample block = exactly one frame: Ok(0.9) decides it.
        assert!(d.block_speech(&block(0.5, 512)), "the first frame decides");
        assert!(
            !d.block_speech(&block(0.5, 1280)),
            "the failure reports no speech"
        );
        assert_eq!(d.failure(), Some("run: synthetic"));
    }

    /// Reset must clear the carry so a gated stretch cannot splice into the
    /// next frame.
    #[test]
    fn reset_drops_the_carry() {
        let mut d = SileroDetector::for_test(|_| Ok(0.9));
        let _ = d.block_speech(&block(0.5, 300)); // less than one frame: carried
        assert_eq!(d.frames, 0);
        d.reset();
        let _ = d.block_speech(&block(0.5, 300));
        assert_eq!(
            d.frames, 0,
            "reset dropped the 300 carried samples; without it this block would complete a frame"
        );
    }
}
