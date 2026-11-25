//! Core-side mic processing for v0.2 uplink audio (G3, mapping gap G1/G3).
//!
//! Continuous 16 kHz PCM arrives as 80 ms `DeviceEvent::AudioChunk` blocks.
//! Device-side KWS owns the local exit reflex; this module owns the shared
//! block state machine (onset run, pre-roll, hangover, hard cap, discards)
//! over either energy VAD (adaptive floor) or Silero VAD v4 (`crate::silero`),
//! plus tone-insensitive pinyin wake-word matching.

/// Tunables (env-overridable; calibrated against the real dot2-d microphone).
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// Absolute onset threshold (dBFS RMS). Adaptation may only RAISE the
    /// effective value above this, never lower it below the calibration.
    pub start_db: f32,
    /// Absolute offset threshold (dBFS RMS), same ceiling semantics.
    pub end_db: f32,
    /// Quiet blocks needed to close a segment (hangover, ~640 ms).
    pub end_blocks: usize,
    /// Loud blocks needed to open a segment (~240 ms).
    pub start_blocks: usize,
    /// Blocks kept before the trigger (pre-roll).
    pub preroll_blocks: usize,
    /// Hard segment cap (blocks); longer runs close immediately (~12 s).
    pub max_blocks: usize,
    /// Minimum segment length (blocks); shorter closes are discarded.
    pub min_blocks: usize,
    /// Offset threshold sits this far above the tracked room floor. The room
    /// jitters ±2 dB block to block, so a smaller margin is a coin flip —
    /// which is exactly how an absolute `end_db=-49` over a −48.8 dB floor
    /// hung until the cap (device 2026-11-13).
    pub noise_margin_db: f32,
    /// Onset threshold sits this far above the tracked floor, so a speaker
    /// quieter than the last calibration still opens a segment.
    pub speak_margin_db: f32,
    /// Minimum gap between effective onset and offset (hysteresis).
    pub hysteresis_db: f32,
    /// Blocks of NON-speech levels kept for the floor estimate (~8 s). The
    /// ring is frozen while a segment is open, so the estimate can never
    /// learn the user's voice or our own answer.
    pub floor_blocks: usize,
    /// Low percentile of that ring used as the floor: one bump (a cup on the
    /// table, a door) must not move it.
    pub floor_percentile: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            start_db: -37.0,
            end_db: -42.0,
            end_blocks: 8,
            start_blocks: 3,
            preroll_blocks: 3,
            max_blocks: 150,
            min_blocks: 3,
            noise_margin_db: 6.0,
            speak_margin_db: 10.0,
            hysteresis_db: 5.0,
            floor_blocks: 100,
            floor_percentile: 0.20,
        }
    }
}

impl VadConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("HM_MIC_VAD_START_DB") {
            if let Ok(v) = v.parse() {
                c.start_db = v;
            }
        }
        if let Ok(v) = std::env::var("HM_MIC_VAD_END_DB") {
            if let Ok(v) = v.parse() {
                c.end_db = v;
            }
        }
        c
    }
}

/// One retained slice of an original input frame. Offsets describe the PCM
/// actually kept; the source envelope itself is never rewritten by trimming.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SourceSpan {
    pub provenance: crate::gateway::InputProvenance,
    pub offset_samples: usize,
    pub samples: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub control_only: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wake_ack_overlap: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tail_blind_only: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub speech: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct CaptureProvenance {
    pub spans: Vec<SourceSpan>,
}

impl CaptureProvenance {
    pub fn chunk(provenance: crate::gateway::InputProvenance, samples: usize) -> Self {
        Self {
            spans: vec![SourceSpan {
                provenance,
                offset_samples: 0,
                samples,
                control_only: false,
                wake_ack_overlap: false,
                tail_blind_only: false,
                speech: false,
            }],
        }
    }

    /// A wake cue (and any immediately following post-ack `tail_blind_only`
    /// window) may prefix a real question, but a protected answer, a pure
    /// post-answer `tail_blind_only` run without `wake_ack_overlap`, or a cue
    /// followed only by silence cannot authorize a follow-up.
    pub fn allows_wake_ack_followup(&self) -> bool {
        let mut has_wake_ack = false;
        for s in &self.spans {
            if s.control_only {
                if s.wake_ack_overlap {
                    has_wake_ack = true;
                } else if !(has_wake_ack && s.tail_blind_only) {
                    return false;
                }
            }
        }
        has_wake_ack
            && self
                .spans
                .rsplit(|s| s.control_only)
                .next()
                .is_some_and(|spans| spans.iter().any(|s| s.speech))
    }

    /// Whether this captured segment must be restricted to control-only commands.
    ///
    /// Any span captured during live voice or media playback (`control_only && !tail_blind_only`)
    /// keeps the whole segment control-only. If ONLY up to 3 leading preroll/onset spans grazed
    /// the post-playback `tail_blind_ms` guard while nothing was playing, and the remainder is
    /// strictly clean speech (`>= 3` clean speech spans and strictly more clean speech than
    /// leading grazed spans), treat the segment as a normal user utterance rather than echo.
    pub fn effective_control_only(&self) -> bool {
        if !self.spans.iter().any(|s| s.control_only) {
            return false;
        }
        if self
            .spans
            .iter()
            .any(|s| s.control_only && !s.tail_blind_only)
        {
            return true;
        }
        let leading = self.spans.iter().take_while(|s| s.control_only).count();
        if leading <= 3 && self.spans[leading..].iter().all(|s| !s.control_only) {
            let clean_speech = self.spans[leading..].iter().filter(|s| s.speech).count();
            if clean_speech >= 3 && clean_speech > leading {
                return false;
            }
        }
        true
    }

    pub fn legacy(samples: usize) -> Self {
        Self::chunk(crate::gateway::InputProvenance::default(), samples)
    }

    pub fn samples(&self) -> usize {
        self.spans.iter().map(|s| s.samples).sum()
    }

    pub fn append(&mut self, other: Self) {
        self.spans.extend(other.spans);
    }

    /// Slice in retained mono sample coordinates, including across frames.
    pub fn slice(&self, start: usize, len: usize) -> Self {
        let mut skip = start;
        let mut remaining = len;
        let mut spans = Vec::new();
        for span in &self.spans {
            if skip >= span.samples {
                skip -= span.samples;
                continue;
            }
            let samples = (span.samples - skip).min(remaining);
            if samples > 0 {
                spans.push(SourceSpan {
                    provenance: span.provenance.clone(),
                    offset_samples: span.offset_samples + skip,
                    samples,
                    control_only: span.control_only,
                    wake_ack_overlap: span.wake_ack_overlap,
                    tail_blind_only: span.tail_blind_only,
                    speech: span.speech,
                });
            }
            remaining -= samples;
            skip = 0;
            if remaining == 0 {
                break;
            }
        }
        Self { spans }
    }
}

#[derive(Debug, Clone)]
struct CapturedBlock {
    pcm: Vec<i16>,
    provenance: CaptureProvenance,
}

/// Outcome of XiaoAi-style physical audio timeline slicing at `wakeup_end` (#261).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WakeSliceOutcome {
    /// No timeline slicing applied (normal follow-up or legacy gateway).
    #[default]
    None,
    /// `wakeup-normal`: <= 3 post-wake speech blocks from `cut_idx` (covering 1 wake-tail
    /// block from `lag_blocks` plus up to 2 blocks of near-field room reverb); skip ASR and
    /// confirm wake locally.
    BareWake,
    /// `wakeup-oneshot`: wake-word prefix physically sliced off; `pcm` holds command audio only.
    CommandSliced,
}

/// One closed speech segment (mono s16 PCM at the stream's sample rate).
#[derive(Debug, Clone)]
pub struct MicSegment {
    pub provenance: CaptureProvenance,
    pub pcm: Vec<i16>,
    /// Up to 2 clean (`!control_only || wake_ack_overlap`) 80 ms pre-onset
    /// blocks (`160 ms`) retained immediately before the `start_blocks` trigger
    /// for ASR WAV encoding (`#216`), kept separate from `pcm` and `provenance`
    /// so `provenance.samples() == pcm.len()` remains invariant.
    pub lead_pcm: Vec<i16>,
    pub sample_rate: u32,
    pub blocks: usize,
    /// Why the segment closed. A cap close is not a sentence end (see the
    /// enum's docs); the caller logs it as such.
    pub close_reason: CloseReason,
    /// Counters snapshot for diagnostics.
    pub total_blocks_seen: u64,
    /// Physical wake-word timeline slice classification (#261).
    pub wake_slice: WakeSliceOutcome,
}

/// Why a segment closed. Only `Quiet` means the person finished; `Cap` means
/// the segment ran out of room while sound continued, so the tail of the
/// utterance lives in the NEXT segment (and the transcript may be cut).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The hangover of quiet blocks elapsed.
    Quiet,
    /// `max_blocks` was reached while the signal continued.
    Cap,
}

/// Which detector decides "speech" for this processor.
///
/// `Silero` latches a `failure` if inference breaks; `feed` then keeps using
/// the energy path for the rest of the process (the model-missing rollback is
/// the same path: the caller simply never installs the detector).
pub enum Endpoint {
    Energy,
    Silero(Box<crate::silero::SileroDetector>),
}

impl Endpoint {
    pub fn kind(&self) -> &'static str {
        match self {
            Endpoint::Energy => "energy",
            Endpoint::Silero(_) => "silero",
        }
    }
}

pub struct MicProcessor {
    cfg: VadConfig,
    endpoint: Endpoint,
    remote_vad_active: bool,
    in_speech: bool,
    wake_capture_promoted: bool,
    above_run: usize,
    onset: Option<(u64, tokio::time::Instant)>,
    below_run: usize,
    preroll: std::collections::VecDeque<CapturedBlock>,
    lead_pcm: std::collections::VecDeque<Vec<i16>>,
    speech: Vec<CapturedBlock>,
    speech_blocks: usize,
    pub blocks_seen: u64,
    /// Rolling block-energy telemetry (G4 acoustic calibration): the last
    /// `LEVEL_RING` fed blocks' dBFS values, oldest first. Diagnostics read
    /// this to measure room noise floors and speech levels.
    levels: std::collections::VecDeque<f32>,
    /// Levels of blocks that were NOT speech, kept for the room estimate.
    /// Fed only between segments — never while one is open.
    floor_ring: std::collections::VecDeque<f32>,
    /// Tracked room floor (low percentile of `floor_ring`), dBFS.
    noise_db: Option<f32>,
    /// What the last close was and how long the segment ran, for diagnostics.
    last_close: Option<(CloseReason, usize)>,
    /// Wall-clock timestamp (`id::now_ms()`) recorded inside `MicProcessor` at the
    /// exact instant `in_speech` flips to `false` on segment close, closing the
    /// cross-lock race window with `on_keyword_at` (`bare_wake_closed`).
    last_close_ms: Option<u64>,
    /// Armed `wakeup_end` block cursor (`blocks_seen` units), whether a speech
    /// segment or onset was already active when armed, when it was armed
    /// (`Instant`), and optional acoustic `wake_dur_ms`, for XiaoAi-style
    /// physical timeline slicing on the currently active wake capture (#261).
    wake_cut_cursor: Option<(u64, bool, std::time::Instant, Option<u64>)>,
    /// Arrival timestamp of the previous audio block, used to flush stale idle
    /// `preroll` / `lead_pcm` / Silero LSTM state across idle-gate discontinuities.
    last_feed_at: Option<std::time::Instant>,
}

/// How many recent block levels the telemetry ring keeps (~8 s at 12.5/s).
const LEVEL_RING: usize = 100;
/// Clean pre-onset blocks (`160 ms` at `80 ms/block`) retained before the
/// `start_blocks` trigger for ASR WAV encoding (`#216`).
const PRE_ONSET_LEAD_BLOCKS: usize = 2;
/// Idle discontinuity threshold (ms): when no segment is open and the gap since
/// the last chunk reaches this bound, drop stale preroll/lead PCM and reset Silero.
pub const IDLE_GAP_RESET_MS: u64 = 500;
/// Maximum block-cursor gap (`25 * 80 ms = 2.0 s`) tolerated when a gateway delivers
/// `keyword` just before flushing preroll PCM during wake arbitration.
const MAX_WAKE_CURSOR_GAP_BLOCKS: u64 = 25;

pub enum FeedOutcome {
    /// Nothing to report (still inside silence or speech).
    None,
    /// A segment opened this block.
    SpeechStarted,
    /// A segment closed; payload as above. `None` pcm = discarded (too short).
    Segment(Option<MicSegment>),
}

impl MicProcessor {
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            endpoint: Endpoint::Energy,
            remote_vad_active: false,
            in_speech: false,
            wake_capture_promoted: false,
            above_run: 0,
            onset: None,
            below_run: 0,
            preroll: std::collections::VecDeque::new(),
            lead_pcm: std::collections::VecDeque::new(),
            speech: Vec::new(),
            speech_blocks: 0,
            blocks_seen: 0,
            levels: std::collections::VecDeque::new(),
            floor_ring: std::collections::VecDeque::new(),
            noise_db: None,
            last_close: None,
            last_close_ms: None,
            wake_cut_cursor: None,
            last_feed_at: None,
        }
    }

    /// Install the Silero detector (M1b). The block state machine is
    /// unchanged: only the per-block speech decision moves to the model.
    pub fn with_silero(mut self, det: crate::silero::SileroDetector) -> Self {
        self.endpoint = Endpoint::Silero(Box::new(det));
        self
    }

    /// True when healthy in-process Silero VAD is installed and authoritative.
    pub fn uses_silero(&self) -> bool {
        matches!(&self.endpoint, Endpoint::Silero(d) if d.failure().is_none())
    }

    pub fn endpoint_kind(&self) -> &'static str {
        if self.uses_silero() {
            "silero"
        } else if self.remote_vad_active {
            "gateway_vad"
        } else {
            self.endpoint.kind()
        }
    }

    /// `Some(reason)` once the model detector has failed and this processor is
    /// running on the energy fallback.
    pub fn endpoint_failure(&self) -> Option<&str> {
        match &self.endpoint {
            Endpoint::Energy => None,
            Endpoint::Silero(d) => d.failure(),
        }
    }

    /// Detector telemetry for the console: (frames, speech_frames, threshold,
    /// gain, last_prob) — `None` for the energy detector, which has no frames.
    pub fn endpoint_stats(&self) -> Option<(u64, u64, f32, f32, f32)> {
        match &self.endpoint {
            Endpoint::Energy => None,
            Endpoint::Silero(d) => {
                Some((d.frames, d.speech_frames, d.threshold, d.gain, d.last_prob))
            }
        }
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// Wall-clock timestamp of the most recent completed VAD segment close,
    /// stamped atomically with `self.in_speech = false`.
    pub fn last_close_ms(&self) -> Option<u64> {
        self.last_close_ms
    }

    /// Current monotonic block cursor (`blocks_seen`) for `wakeup_end` slicing (#261).
    pub fn current_block_cursor(&self) -> u64 {
        self.blocks_seen
    }

    /// Elapsed speech duration (ms, 80 ms/block) in the currently open or pending
    /// capture before the current block. Used to detect mid-sentence KWS triggers
    /// on long idle audio (e.g. TV/video commentary).
    pub fn open_speech_elapsed_ms(&self) -> u64 {
        if self.in_speech {
            let speech_blocks = self
                .speech
                .iter()
                .filter(|b| b.provenance.spans.iter().any(|s| s.speech))
                .count();
            speech_blocks as u64 * 80
        } else if self.above_run > 0 {
            self.above_run as u64 * 80
        } else {
            0
        }
    }

    /// Whether the currently open/pending capture already carries playback-overlap
    /// (`control_only`) spans, in which case residual echo may hold VAD open and
    /// idle mid-segment wake gating must not apply.
    pub fn open_is_control_only(&self) -> bool {
        if self.in_speech {
            self.speech
                .iter()
                .any(|b| b.provenance.spans.iter().any(|s| s.control_only))
        } else if self.above_run > 0 {
            self.preroll
                .back()
                .is_some_and(|b| b.provenance.spans.iter().any(|s| s.control_only))
        } else {
            false
        }
    }

    /// Arm physical wake-word timeline slicing at `end_block_cursor` (`blocks_seen`
    /// coordinates captured at KWS arrival, before any arbitration delay), optionally
    /// bounded by acoustic `wake_dur_ms` when Zipformer right-context lookahead
    /// overshoots the wake-word end into a tight one-shot command (#261).
    pub fn arm_wake_slice_with_dur(&mut self, end_block_cursor: u64, wake_dur_ms: Option<u64>) {
        let active = self.in_speech || self.onset_pending();
        self.wake_cut_cursor = Some((
            end_block_cursor,
            active,
            std::time::Instant::now(),
            wake_dur_ms,
        ));
    }

    /// Arm physical wake-word timeline slicing at `end_block_cursor` (`blocks_seen`
    /// coordinates captured at KWS arrival, before any arbitration delay) (#261).
    pub fn arm_wake_slice(&mut self, end_block_cursor: u64) {
        self.arm_wake_slice_with_dur(end_block_cursor, None);
    }

    /// Clear stale idle `preroll`, `lead_pcm`, pending onset, and Silero LSTM state
    /// if the elapsed wall gap since the previous chunk reached `IDLE_GAP_RESET_MS`
    /// while no speech segment is open (e.g. across gateway idle-gate pauses).
    /// Freshly armed `wake_cut_cursor`s (< 5 s old) are preserved so post-idle PCM
    /// arriving after an idle-gated KWS wake does not wipe the armed slice cursor.
    pub fn reset_stale_preroll_if_gap(&mut self, now: std::time::Instant) -> bool {
        let prev = self.last_feed_at.replace(now);
        let Some(prev) = prev else {
            return false;
        };
        if self.in_speech {
            return false;
        }
        if now.saturating_duration_since(prev) < std::time::Duration::from_millis(IDLE_GAP_RESET_MS)
        {
            return false;
        }
        let preserve_wake_cut = self.wake_cut_cursor.is_some_and(|(_, _, armed_at, _)| {
            now.saturating_duration_since(armed_at) < std::time::Duration::from_secs(5)
        });
        let had_state = !self.preroll.is_empty()
            || !self.lead_pcm.is_empty()
            || self.above_run > 0
            || self.onset.is_some()
            || (!preserve_wake_cut && self.wake_cut_cursor.is_some());
        self.preroll.clear();
        self.lead_pcm.clear();
        self.above_run = 0;
        self.onset = None;
        if !preserve_wake_cut {
            self.wake_cut_cursor = None;
        }
        if let Endpoint::Silero(d) = &mut self.endpoint {
            d.reset();
        }
        had_state
    }

    /// The tracked room floor (dBFS), from NON-speech blocks only. `None`
    /// until enough room has been observed to trust an estimate.
    pub fn noise_floor_db(&self) -> Option<f32> {
        self.noise_db
    }

    /// The thresholds actually in force right now: the configured values,
    /// raised when the room is louder than the calibration assumed. The
    /// configured numbers stay the sensitivity CEILING — a quiet room is
    /// never made less sensitive than the person calibrated.
    pub fn effective_thresholds(&self) -> (f32, f32) {
        let Some(floor) = self.noise_db else {
            return (self.cfg.start_db, self.cfg.end_db);
        };
        let end = (floor + self.cfg.noise_margin_db).max(self.cfg.end_db);
        let start = (floor + self.cfg.speak_margin_db)
            .max(end + self.cfg.hysteresis_db)
            .max(self.cfg.start_db);
        (start, end)
    }

    /// `(reason, blocks)` of the most recent close.
    pub fn last_close(&self) -> Option<(CloseReason, usize)> {
        self.last_close
    }

    /// Recompute the room floor from the non-speech ring. Only called while
    /// NO segment is open, so the estimate is frozen for the whole utterance.
    fn update_floor(&mut self, db: f32) {
        if !db.is_finite() {
            // Digital silence is information (the room is very quiet), but it
            // cannot be "20 dB above a −inf floor": drop it from the average
            // and let the configured threshold stand.
            return;
        }
        self.floor_ring.push_back(db);
        while self.floor_ring.len() > self.cfg.floor_blocks.max(8) {
            self.floor_ring.pop_front();
        }
        if self.floor_ring.len() < 8 {
            return; // too few blocks to tell a room from a coincidence
        }
        let mut sorted: Vec<f32> = self.floor_ring.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        let idx = ((sorted.len() - 1) as f32 * self.cfg.floor_percentile.clamp(0.0, 1.0)) as usize;
        self.noise_db = Some(sorted[idx.min(sorted.len() - 1)]);
    }

    /// Snapshot of the level telemetry ring: (count, min, mean, p95, max)
    /// over the kept blocks, all dBFS. None when nothing was fed yet. The
    /// p95 (not max) is the calibration anchor — a single outlier block
    /// (button click, bump) must not set the speech-level estimate.
    pub fn level_stats(&self) -> Option<(usize, f32, f32, f32, f32)> {
        if self.levels.is_empty() {
            return None;
        }
        let mut sorted: Vec<f32> = self
            .levels
            .iter()
            .copied()
            .filter(|v| v.is_finite())
            .collect();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_by(f32::total_cmp);
        let n = sorted.len();
        let min = sorted[0];
        let max = sorted[n - 1];
        let mean = sorted.iter().sum::<f32>() / n as f32;
        let p95 = sorted[(n as f32 * 0.95) as usize % n];
        Some((self.levels.len(), min, mean, p95, max))
    }

    /// Live threshold adjustment (G4): calibration writes new values through
    /// the management API; running processors adopt them without a restart.
    /// Hysteresis (start > end) is validated by the caller.
    pub fn set_thresholds(&mut self, start_db: f32, end_db: f32) {
        self.cfg.start_db = start_db;
        self.cfg.end_db = end_db;
        // A mid-speech threshold change can invalidate the close decision:
        // re-arm from the current run state instead of trusting stale runs.
        self.above_run = 0;
        self.below_run = 0;
    }

    /// First loud block in the confirmed onset, before the VAD confirmation lag.
    pub fn onset(&self) -> Option<(u64, tokio::time::Instant)> {
        self.onset
    }
    pub fn onset_pending(&self) -> bool {
        !self.in_speech && self.above_run > 0
    }
    pub fn onset_provenance(&self) -> CaptureProvenance {
        if self.in_speech {
            self.open_provenance()
        } else {
            // Earlier quiet pre-roll is retained for the segment, but only
            // this first loud block determines provisional input permission.
            self.preroll
                .back()
                .map(|b| b.provenance.clone())
                .unwrap_or_default()
        }
    }

    pub fn config(&self) -> &VadConfig {
        &self.cfg
    }

    /// Feed one block of mono s16 samples (interleaved 2ch input must be
    /// downmixed by the caller — ch2 is the AEC reference, not a mic copy).
    pub fn feed(&mut self, block: Vec<i16>, sample_rate: u32) -> FeedOutcome {
        let provenance = CaptureProvenance::legacy(block.len());
        self.feed_with_provenance(block, sample_rate, provenance)
    }

    /// Snapshot of the retained preroll/onset while the segment is open.
    pub fn open_provenance(&self) -> CaptureProvenance {
        CaptureProvenance {
            spans: self
                .speech
                .iter()
                .flat_map(|b| b.provenance.spans.clone())
                .collect(),
        }
    }

    pub fn feed_with_provenance(
        &mut self,
        block: Vec<i16>,
        sample_rate: u32,
        provenance: CaptureProvenance,
    ) -> FeedOutcome {
        self.feed_with_provenance_and_vad(block, sample_rate, provenance, None)
    }

    /// Feed one PCM block with an optional gateway/sidecar neural VAD decision
    /// (`AudioChunk.vad_speech` / `vad_prob`). In-process Silero VAD (Dot2 fat
    /// build) is authoritative whenever healthy; `remote_vad` is only consumed
    /// when no healthy local Silero model is active (`--slim` / `Endpoint::Energy`).
    pub fn feed_with_provenance_and_vad(
        &mut self,
        block: Vec<i16>,
        sample_rate: u32,
        mut provenance: CaptureProvenance,
        remote_vad: Option<bool>,
    ) -> FeedOutcome {
        debug_assert_eq!(block.len(), provenance.samples());
        self.reset_stale_preroll_if_gap(std::time::Instant::now());
        self.blocks_seen += 1;
        let db = block_db(&block);
        self.levels.push_back(db);
        while self.levels.len() > LEVEL_RING {
            self.levels.pop_front();
        }
        // Thresholds FIRST, from the floor as it stood before this block, so
        // a block can never reclassify itself.
        let (start_db, end_db) = self.effective_thresholds();
        // Local Silero is authoritative on Dot2 fat builds; remote_vad only
        // drives slim/energy processors (or after local Silero latches failure).
        let (loud, quiet) = match &mut self.endpoint {
            Endpoint::Silero(d) if d.failure().is_none() => {
                self.remote_vad_active = false;
                let speech = d.block_speech(&block);
                if d.failure().is_some() {
                    if let Some(remote) = remote_vad {
                        self.remote_vad_active = true;
                        (remote, !remote)
                    } else {
                        (db >= start_db, db < end_db)
                    }
                } else {
                    (speech, !speech)
                }
            }
            Endpoint::Energy | Endpoint::Silero(_) => {
                if let Some(speech) = remote_vad {
                    self.remote_vad_active = true;
                    (speech, !speech)
                } else {
                    self.remote_vad_active = false;
                    (db >= start_db, db < end_db)
                }
            }
        };

        for span in &mut provenance.spans {
            span.speech = loud;
        }

        if !self.in_speech {
            // The floor learns only from blocks that were not speech, and
            // only between segments: a room estimate that chases the user's
            // voice would raise the thresholds until the mic went deaf.
            if !loud {
                self.update_floor(db);
            }
            let block_tainted = provenance
                .spans
                .iter()
                .any(|s| s.control_only && !s.wake_ack_overlap);
            if block_tainted {
                self.lead_pcm.clear();
            }
            self.preroll.push_back(CapturedBlock {
                pcm: block,
                provenance,
            });
            while self.preroll.len() > self.cfg.preroll_blocks {
                if let Some(evicted) = self.preroll.pop_front() {
                    let evicted_tainted = evicted
                        .provenance
                        .spans
                        .iter()
                        .any(|s| s.control_only && !s.wake_ack_overlap);
                    if evicted_tainted {
                        self.lead_pcm.clear();
                    } else {
                        self.lead_pcm.push_back(evicted.pcm);
                        while self.lead_pcm.len() > PRE_ONSET_LEAD_BLOCKS {
                            self.lead_pcm.pop_front();
                        }
                    }
                }
            }
            if loud {
                if self.above_run == 0 {
                    self.onset = Some((crate::id::now_ms(), tokio::time::Instant::now()));
                }
                self.above_run += 1;
            } else {
                if self.above_run > 0 {
                    self.wake_cut_cursor = None;
                }
                self.above_run = 0;
            }
            if self.above_run >= self.cfg.start_blocks {
                self.in_speech = true;
                self.speech = self.preroll.drain(..).collect();
                self.speech_blocks = self.speech.len();
                self.below_run = 0;
                return FeedOutcome::SpeechStarted;
            }
            return FeedOutcome::None;
        }

        // Inside speech.
        if self.wake_capture_promoted {
            for s in &mut provenance.spans {
                if s.control_only {
                    s.wake_ack_overlap = true;
                }
            }
        }
        self.speech.push(CapturedBlock {
            pcm: block,
            provenance,
        });
        self.speech_blocks += 1;
        if quiet {
            self.below_run += 1;
        } else {
            self.below_run = 0;
        }
        let quiet_close = self.below_run >= self.cfg.end_blocks;
        let cap_close = self.speech_blocks >= self.cfg.max_blocks;
        if !quiet_close && !cap_close {
            return FeedOutcome::None;
        }
        // A cap close is NOT a sentence end: the signal was still going, so
        // there is no hangover to trim and the last block is the user's.
        let close_reason = if quiet_close {
            CloseReason::Quiet
        } else {
            CloseReason::Cap
        };
        // The trailing quiet blocks belong to the hangover, not the user —
        // keep them (they carry breath/tail consonants ASR wants) but a
        // full hangover tail of near-silence is trimmed to half.
        let trim = if close_reason == CloseReason::Quiet {
            self.cfg.end_blocks / 2
        } else {
            0
        };
        if trim > 0 && self.speech.len() > trim {
            let new_len = self.speech.len() - trim;
            self.speech.truncate(new_len);
        }
        let blocks = self.speech_blocks;
        self.last_close = Some((close_reason, blocks));
        let mut wake_slice = WakeSliceOutcome::None;
        if let Some((cut_cursor, armed_active, _armed_at, wake_dur_ms)) =
            self.wake_cut_cursor.take()
        {
            // `blocks` is the pre-trim block count of `self.speech`, so `self.speech[0]`
            // corresponds to 1-indexed `blocks_seen` value `blocks_seen - blocks + 1`.
            let speech_start_cursor = self.blocks_seen.saturating_sub(blocks as u64) + 1;
            if cut_cursor.saturating_add(1) >= speech_start_cursor && !self.speech.is_empty() {
                let cut_idx = cut_cursor
                    .saturating_add(1)
                    .saturating_sub(speech_start_cursor)
                    .min(self.speech.len() as u64) as usize;
                let valid_wake_dur = wake_dur_ms.filter(|dur| (240..=1800).contains(dur));
                let effective_cut = if let Some(dur) = valid_wake_dur {
                    let first_speech_idx = self
                        .speech
                        .iter()
                        .position(|b| b.provenance.spans.iter().any(|s| s.speech))
                        .unwrap_or(0);
                    let wake_blocks = dur.div_ceil(80) as usize;
                    let acoustic_end_idx = (first_speech_idx + wake_blocks).min(self.speech.len());
                    cut_idx.min(acoustic_end_idx)
                } else {
                    cut_idx
                };
                let post_wake_speech_blocks = self.speech[effective_cut..]
                    .iter()
                    .filter(|b| b.provenance.spans.iter().any(|s| s.speech))
                    .count();
                // `cut_cursor` already steps back `lag_blocks` (~1 block at 94 ms) before
                // KWS arrival, and `effective_cut` caps it by `first_speech_idx + ceil(wake_dur_ms/80)`
                // when Zipformer chunk-16 right-context lookahead overshoots into a tight
                // one-shot command. Tolerating <= 3 speech blocks from `effective_cut`
                // keeps bare wakes at 0 ASR in both mid-field and near-field rooms while
                // commands (>= 4 blocks) stay `CommandSliced`.
                if close_reason == CloseReason::Quiet && post_wake_speech_blocks <= 3 {
                    wake_slice = WakeSliceOutcome::BareWake;
                } else {
                    // Pause-aware physical slicing: if a clear post-wake quiet gap
                    // (>= 2 consecutive non-speech blocks = 160 ms) begins at/near
                    // `effective_cut` and is followed by command speech, start right after
                    // the pause (keeping 1 quiet lead-in block) instead of backing off across
                    // the silence into the wake word's final syllable. Otherwise (0 ms tight
                    // one-shot), keep a bounded safety overlap when `wake_dur_ms` anchored
                    // the cut, or back off 5 blocks (400 ms) when `wake_dur_ms` is absent.
                    let is_speech = |b: &CapturedBlock| b.provenance.spans.iter().any(|s| s.speech);
                    let pause_cmd_start = {
                        let scan_end = (effective_cut + 3).min(self.speech.len());
                        let mut found = None;
                        let mut i = effective_cut.saturating_sub(1);
                        while i <= scan_end && i < self.speech.len() {
                            if !is_speech(&self.speech[i]) {
                                let mut j = i;
                                while j < self.speech.len() && !is_speech(&self.speech[j]) {
                                    j += 1;
                                }
                                if j - i >= 2 && j < self.speech.len() {
                                    found = Some(j.saturating_sub(1));
                                    break;
                                }
                                i = j;
                            } else {
                                i += 1;
                            }
                        }
                        found
                    };
                    let slice_idx = pause_cmd_start.unwrap_or_else(|| {
                        if valid_wake_dur.is_some() {
                            if effective_cut < cut_idx {
                                effective_cut.saturating_sub(1)
                            } else {
                                effective_cut.saturating_sub(2)
                            }
                        } else {
                            cut_idx.saturating_sub(5)
                        }
                    });
                    if slice_idx > 0 && slice_idx < self.speech.len() {
                        self.speech = self.speech.split_off(slice_idx);
                        self.lead_pcm.clear();
                        wake_slice = WakeSliceOutcome::CommandSliced;
                    } else if slice_idx == 0 && !self.speech.is_empty() {
                        self.lead_pcm.clear();
                        wake_slice = WakeSliceOutcome::CommandSliced;
                    }
                }
            } else if armed_active
                && !self.speech.is_empty()
                && speech_start_cursor.saturating_sub(cut_cursor) <= MAX_WAKE_CURSOR_GAP_BLOCKS
            {
                // Gateway sent `keyword` slightly before flushed preroll PCM so the
                // captured segment began just after `wake_end_cursor` within the same
                // wake transfer (e.g. during 200 ms wake arbitration). Classify it
                // cleanly without truncating post-wake command PCM.
                let total_speech_blocks = self
                    .speech
                    .iter()
                    .filter(|b| b.provenance.spans.iter().any(|s| s.speech))
                    .count();
                if close_reason == CloseReason::Quiet && total_speech_blocks <= 3 {
                    wake_slice = WakeSliceOutcome::BareWake;
                } else {
                    self.lead_pcm.clear();
                    wake_slice = WakeSliceOutcome::CommandSliced;
                }
            }
        }
        let provenance = self.open_provenance();
        let pcm: Vec<i16> = std::mem::take(&mut self.speech)
            .into_iter()
            .flat_map(|b| b.pcm)
            .collect();
        let lead_pcm: Vec<i16> = std::mem::take(&mut self.lead_pcm)
            .into_iter()
            .flatten()
            .collect();
        self.in_speech = false;
        self.last_close_ms = Some(crate::id::now_ms());
        self.wake_capture_promoted = false;
        self.below_run = 0;
        self.above_run = 0;
        self.preroll.clear();
        self.speech_blocks = 0;
        let keep = blocks >= self.cfg.min_blocks && !pcm.is_empty();
        FeedOutcome::Segment(Some(MicSegment {
            provenance: if keep {
                provenance
            } else {
                CaptureProvenance::default()
            },
            pcm: if keep { pcm } else { Vec::new() },
            lead_pcm: if keep { lead_pcm } else { Vec::new() },
            sample_rate,
            blocks,
            close_reason,
            total_blocks_seen: self.blocks_seen,
            wake_slice: if keep {
                wake_slice
            } else {
                WakeSliceOutcome::None
            },
        }))
    }

    /// Clear an uncommitted onset or pre-roll tainted by non-wake-ack playback
    /// (`control_only && !wake_ack_overlap`) when voice playback finishes, so
    /// residual echo blocks and LSTM state cannot combine with post-play room
    /// tail to open a bogus segment (`#217` Problem A).
    pub fn clear_tainted_onset(&mut self) -> bool {
        if self.in_speech {
            return false;
        }
        let tainted = self.preroll.iter().any(|b| {
            b.provenance
                .spans
                .iter()
                .any(|s| s.control_only && !s.wake_ack_overlap)
        });
        if !tainted {
            return false;
        }
        self.preroll.clear();
        self.lead_pcm.clear();
        self.above_run = 0;
        self.onset = None;
        self.wake_cut_cursor = None;
        if let Endpoint::Silero(d) = &mut self.endpoint {
            d.reset();
        }
        true
    }

    /// Promote already-buffered and subsequent speech spans of a wake-transferred
    /// open capture to `wake_ack_overlap` without exempting uncommitted echo
    /// `preroll` from `clear_tainted_onset`.
    pub fn promote_wake_capture(&mut self) {
        if !self.in_speech {
            return;
        }
        self.wake_capture_promoted = true;
        for b in &mut self.speech {
            for s in &mut b.provenance.spans {
                if s.control_only {
                    s.wake_ack_overlap = true;
                }
            }
        }
    }

    /// Drop everything (called when the pipeline gates the mic, e.g. the
    /// assistant starts speaking — half-duplex core-side gating). A partial
    /// segment is discarded, not half-delivered.
    pub fn reset(&mut self) {
        self.remote_vad_active = false;
        self.in_speech = false;
        self.wake_capture_promoted = false;
        self.speech.clear();
        self.preroll.clear();
        self.lead_pcm.clear();
        self.above_run = 0;
        self.onset = None;
        self.below_run = 0;
        self.speech_blocks = 0;
        self.last_close_ms = None;
        self.wake_cut_cursor = None;
        self.last_feed_at = None;
        // The model's LSTM state and the half-frame carry must not survive a
        // gated stretch: the next frame would splice audio from both sides.
        if let Endpoint::Silero(d) = &mut self.endpoint {
            d.reset();
        }
    }
}

/// RMS energy of a block in dBFS (s16 full scale).
pub fn block_db(block: &[i16]) -> f32 {
    if block.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f64 = block
        .iter()
        .map(|&s| {
            let f = s as f64 / 32768.0;
            f * f
        })
        .sum();
    let rms = (sum / block.len() as f64).sqrt();
    if rms <= 0.0 {
        f32::NEG_INFINITY
    } else {
        (20.0 * rms.log10()) as f32
    }
}

// --- wake-word matching (pinyin, tone-insensitive) -------------------------------

/// Syllable sequence of a Hanzi string, tone-insensitive ("高达" →
/// ["gao","da"]). Non-Hanzi chars map to their lowercase form (so Latin
/// wake words still work, matched verbatim).
pub fn pinyin_syllables(text: &str) -> Vec<String> {
    use pinyin::ToPinyin;
    let mut out = Vec::new();
    for c in text.chars() {
        match c.to_pinyin() {
            Some(py) => out.push(py.plain().to_string()),
            None => {
                // ToPinyin yields None for punctuation AND non-Hanzi; only
                // alphanumerics become matchable tokens.
                if c.is_ascii_alphanumeric() {
                    out.push(c.to_ascii_lowercase().to_string());
                }
            }
        }
    }
    out
}

/// Find the wake word in `text`: returns the matched CHAR range if the
/// wake word's syllables appear contiguously (tone-insensitive). For short
/// wake words (≤2 syllables) only an exact syllable run matches — edit
/// distance tolerance at this length false-positives badly ("告诉" is one
/// substitution away from "高达").
pub fn wake_span(text: &str, wake: &str) -> Option<(usize, usize)> {
    let wake_syl = pinyin_syllables(wake);
    if wake_syl.is_empty() {
        return None;
    }
    // Walk chars, collecting (byte start, syllable, char byte-len) for
    // mappable chars — the char's own byte length gives the exact end
    // boundary of the match (never a syllable-string length).
    use pinyin::ToPinyin;
    let mut items: Vec<(usize, String, usize)> = Vec::new();
    for (i, c) in text.char_indices() {
        match c.to_pinyin() {
            Some(py) => items.push((i, py.plain().to_string(), c.len_utf8())),
            None if c.is_ascii_alphanumeric() => {
                items.push((i, c.to_ascii_lowercase().to_string(), c.len_utf8()))
            }
            None => {}
        }
    }
    if items.len() < wake_syl.len() {
        return None;
    }
    for window in items.windows(wake_syl.len()) {
        if window.iter().zip(&wake_syl).all(|((_, s, _), w)| s == w) {
            let start = window[0].0;
            let last = window.last().unwrap();
            let end = last.0 + last.2;
            return Some((start, end));
        }
    }
    None
}

/// Near-homophone tolerance for a WAKE WORD only (user decision 2026-11-13:
/// "口齿不清的写法也加进去" — as a rule, not an unbounded word list).
///
/// Measured on the device at 1–4 m, the ASR renders 「高达」 as 郭达/郭大/高单/
/// 高搭…: the first syllable loses its vowel (gao→guo) or the second gains a
/// coda (da→dan). A hand-maintained list cannot keep up, so the match
/// compares syllables as NEIGHBOURS:
///
///   equal, OR same leading consonant AND
///     (a) the finals are equal once a nasal coda is removed (da↔dan,
///         ni↔nin, ang↔an), OR
///     (b) both finals are short and end in the same vowel, i.e. only the
///         medial glide differs (ao↔uo: gao↔guo).
///
/// 2026-11-14, with two real false wakes as the evidence, the old
/// "one final is a prefix of the other" and "same last LETTER" rules were
/// removed:
///   - 郭总 (guo zong) matched 国政 (guo zheng) because zong/zheng both end
///     in the letter g — a coda is not a nucleus;
///   - 高达 (gao da) matched 嘎达 (ga da) because "ga" is a prefix of "gao" —
///     a short syllable is not a slip of a long one.
/// Rule (a) keeps the measured coda confusions and rule (b) keeps
/// gao↔guo. What is lost: a *medial glide* substitution (da↔dao, so
/// 郭导 no longer rides on 高达) needs its own wake-word entry — the
/// console's list already carries one for the measured renderings.
/// Deliberately NOT applied to command words: 「暂停播放」 must be heard,
/// not guessed.
pub fn syllable_near(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let ia = a.chars().next();
    let ib = b.chars().next();
    if ia.is_none() || ia != ib {
        return false;
    }
    let fa: String = a.chars().skip(1).collect();
    let fb: String = b.chars().skip(1).collect();
    if fa.is_empty() || fb.is_empty() {
        return false;
    }
    let na = strip_nasal(&fa);
    let nb = strip_nasal(&fb);
    // (a) the same nucleus once the coda is removed.
    if !na.is_empty() && na == nb {
        return true;
    }
    // (b) same final vowel, different (single-letter) medial glide.
    let (ca, cb) = (na.chars().last(), nb.chars().last());
    if let (Some(ca), Some(cb)) = (ca, cb) {
        if ca == cb && is_vowel_char(ca) {
            let pa: String = na.chars().take(na.chars().count() - 1).collect();
            let pb: String = nb.chars().take(nb.chars().count() - 1).collect();
            if pa.chars().count() <= 1 && pb.chars().count() <= 1 {
                return true;
            }
        }
    }
    false
}

/// Pinyin nasal codas: `n`/`ng`. Removing them is what makes da/dan,
/// ni/nin and ang/an comparable by nucleus.
fn strip_nasal(final_part: &str) -> String {
    if let Some(x) = final_part.strip_suffix("ng") {
        return x.to_string();
    }
    if let Some(x) = final_part.strip_suffix('n') {
        return x.to_string();
    }
    final_part.to_string()
}

fn is_vowel_char(c: char) -> bool {
    matches!(c, 'a' | 'o' | 'e' | 'i' | 'u' | 'v')
}

/// `wake_span` with the near-homophone tolerance above. Used ONLY where a
/// spoken transcript is checked for the wake word.
pub fn wake_span_near(text: &str, wake: &str) -> Option<(usize, usize)> {
    let wake_syl = pinyin_syllables(wake);
    if wake_syl.is_empty() {
        return None;
    }
    use pinyin::ToPinyin;
    let mut items: Vec<(usize, String, usize)> = Vec::new();
    for (i, c) in text.char_indices() {
        match c.to_pinyin() {
            Some(py) => items.push((i, py.plain().to_string(), c.len_utf8())),
            None if c.is_ascii_alphanumeric() => {
                items.push((i, c.to_ascii_lowercase().to_string(), c.len_utf8()))
            }
            None => {}
        }
    }
    if items.len() < wake_syl.len() {
        return None;
    }
    for window in items.windows(wake_syl.len()) {
        let mut near = 0usize;
        let mut ok = true;
        for ((_, s, _), w) in window.iter().zip(&wake_syl) {
            if s == w {
                continue;
            }
            if !syllable_near(s, w) {
                ok = false;
                break;
            }
            near += 1;
        }
        // A LONG phrase may tolerate at most ONE fuzzy syllable. Every extra
        // fuzz had multiplied the chance of matching ordinary speech: with
        // two, 「迷你火锅打折了」 (ni huo guo da) matched 「你好高达」 because
        // huo≈hao and guo≈gao (audit 2026-11-14). Short words keep one fuzz.
        if ok && !(wake_syl.len() >= 3 && near > 1) {
            let start = window[0].0;
            let last = window.last().unwrap();
            return Some((start, last.0 + last.2));
        }
    }
    None
}

/// The command that FOLLOWS the wake word (prologue before the wake word is dropped).
pub fn wake_tail(text: &str, span: (usize, usize)) -> String {
    let (_, end) = span;
    let sep = |c: char| matches!(c, '，' | '、' | '。' | '！' | '!' | ',' | ' ' | '：' | ':');
    text[end..]
        .trim_start_matches(sep)
        .trim_end_matches(sep)
        .trim()
        .to_string()
}

/// True if `text` contains at least one Unicode letter, CJK character, or digit (rejects pure punctuation like `"。"`).
pub fn has_speech_chars(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// A transcript that is only a filler/acknowledgement/laughter particle ("啊。"/"嗯嗯"/"笑。").
pub fn is_filler_utterance(text: &str) -> bool {
    const FILLERS: &[char] = &[
        '啊', '嗯', '哦', '噢', '唉', '呀', '诶', '欸', '哈', '呃', '额', '唔', '嗷', '喔', '恩',
        '好', '行', '对', '是', '的', '了', '吧', '呢', '笑', '呵', '嘿', '嘻', '咳',
    ];
    let n: Vec<char> = text.chars().filter(|c| c.is_alphanumeric()).collect();
    !n.is_empty() && n.len() <= 4 && n.iter().all(|c| FILLERS.contains(c))
}

/// Strip the wake word (and adjacent filler punctuation) from an utterance
/// transcript. Returns the remainder, trimmed of leading/trailing
/// punctuation and spaces; empty when the wake word was the whole input.
#[cfg(test)]
pub fn strip_wake(text: &str, span: (usize, usize)) -> String {
    let (start, end) = span;
    let before = &text[..start];
    let after = &text[end..];
    let sep = |c: char| matches!(c, '，' | '、' | '。' | '！' | '!' | ',' | ' ' | '：' | ':');
    // If the removal leaves TWO separators at the junction ("嗯，|，讲个"),
    // drop one; otherwise plain concatenation.
    let junction_doubled = before.chars().last().map(sep).unwrap_or(false)
        && after.chars().next().map(sep).unwrap_or(false);
    let rest = if junction_doubled {
        format!("{}{}", before, after.trim_start_matches(sep))
    } else {
        format!("{}{}", before, after)
    };
    rest.trim_start_matches(sep)
        .trim_end_matches(sep)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, ms: usize, rate: u32) -> Vec<i16> {
        let n = (rate as f32 * ms as f32 / 1000.0) as usize;
        (0..n)
            .map(|i| {
                ((freq * i as f32 * 2.0 * std::f32::consts::PI / rate as f32).sin() * 12000.0)
                    as i16
            })
            .collect()
    }

    /// Laughter and breath are not requests (device log 2026-11-12 19:09:
    /// 「笑。」 inside the follow window ran a turn, and the model answered the
    /// PREVIOUS turn's story request — the device told a story nobody asked
    /// for). Deterministic intents (停/继续/退下) are matched before this
    /// filter, so short COMMANDS are unaffected.
    #[test]
    fn laughter_and_breath_are_not_requests() {
        for t in ["笑。", "呵呵。", "哈哈", "嘿", "嘻，", "咳。"] {
            assert!(is_filler_utterance(t), "{t} must be treated as noise");
        }
        for t in ["开灯", "停", "继续", "退下"] {
            assert!(!is_filler_utterance(t), "{t} is a command, not noise");
        }
    }

    fn silence(ms: usize, rate: u32) -> Vec<i16> {
        vec![0; (rate as f32 * ms as f32 / 1000.0) as usize]
    }

    /// Deterministic ±2 dB block-to-block jitter: a real room floor is a
    /// band, not a constant. A 1.5 dB spread around the threshold is exactly
    /// what made an absolute `end_db=-49` never see `end_blocks` consecutive
    /// quiet blocks on the device (2026-11-13: floor mean −48.8, p95 −44.8).
    fn jitter(seed: &mut u64) -> f32 {
        let mut x = *seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *seed = x;
        ((x % 401) as f32 - 200.0) / 100.0
    }

    /// One 80 ms block at the requested dBFS RMS.
    fn block_at(db: f32, seed: &mut u64) -> Vec<i16> {
        let d = db + jitter(seed);
        let amp = 32768.0 * 10f32.powf(d / 20.0);
        vec![amp.clamp(-32768.0, 32767.0) as i16; 1280]
    }

    /// FIELD CASE (2026-11-13): the room sits at −48.8 dB mean with a p95 of
    /// −44.8, the configured offset threshold is −49. Every block is "not
    /// quiet", so `below_run` never reaches 8 and the VAD waits for the
    /// 150-block cap — 11.68 s per segment, two utterances merged, and every
    /// downstream stage (ASR, ACK, answer) 9–12 s late.
    #[test]
    fn a_noisy_floor_closes_the_segment_instead_of_waiting_for_the_cap() {
        let mut p = MicProcessor::new(VadConfig {
            start_db: -45.0,
            end_db: -49.0,
            ..VadConfig::default()
        });
        let mut seed = 0x5EED_1234_5678_9ABCu64;
        // ~8 s of room only.
        for _ in 0..100 {
            assert!(matches!(
                p.feed(block_at(-48.8, &mut seed), 16000),
                FeedOutcome::None
            ));
        }
        // 2 s of speech, ~20 dB above that floor.
        for _ in 0..25 {
            let _ = p.feed(block_at(-28.0, &mut seed), 16000);
        }
        // The room comes back. A correct endpoint closes within the 640 ms
        // hangover plus one block; allow 1 s of slack, not 12.
        let mut closed_at = None;
        for i in 0..40 {
            if let FeedOutcome::Segment(Some(seg)) = p.feed(block_at(-48.8, &mut seed), 16000) {
                if !seg.pcm.is_empty() {
                    closed_at = Some(i + 1);
                    break;
                }
            }
        }
        let at = closed_at
            .expect("the segment must close on the room's floor, not on the 150-block cap");
        assert!(
            at <= 12,
            "closed after {at} blocks; at 80 ms each this is {:.2} s",
            at as f32 * 0.08
        );
    }

    /// The same floor must not merge two separate utterances into one
    /// segment — that is what turned 「打开关闭儿童房的灯」 + 「你好，高达。关闭
    /// 儿童房的灯。」 into a single transcript.
    #[test]
    fn two_utterances_stay_two_segments_on_a_noisy_floor() {
        let mut p = MicProcessor::new(VadConfig {
            start_db: -45.0,
            end_db: -49.0,
            ..VadConfig::default()
        });
        let mut seed = 0x0BAD_F00D_DEAD_BEEFu64;
        for _ in 0..40 {
            let _ = p.feed(block_at(-48.8, &mut seed), 16000);
        }
        let mut segments = 0;
        // Two 2 s utterances separated by 1.2 s of room.
        for _ in 0..25 {
            let _ = p.feed(block_at(-28.0, &mut seed), 16000);
        }
        for _ in 0..60 {
            if let FeedOutcome::Segment(Some(seg)) = p.feed(block_at(-48.8, &mut seed), 16000) {
                if !seg.pcm.is_empty() {
                    segments += 1;
                }
            }
            if segments == 1 {
                break;
            }
        }
        for _ in 0..25 {
            let _ = p.feed(block_at(-28.0, &mut seed), 16000);
        }
        for _ in 0..60 {
            if let FeedOutcome::Segment(Some(seg)) = p.feed(block_at(-48.8, &mut seed), 16000) {
                if !seg.pcm.is_empty() {
                    segments += 1;
                }
            }
            if segments == 2 {
                break;
            }
        }
        assert_eq!(segments, 2, "two utterances must be two segments");
    }

    /// A segment that runs into the cap while sound continues is a DIAGNOSTIC
    /// event, not a sentence end — and the old code still trimmed half a
    /// hangover (320 ms) off it, cutting the user's last syllable.
    #[test]
    fn a_capped_segment_is_named_and_keeps_its_tail() {
        let mut p = MicProcessor::new(VadConfig::default());
        let mut seed = 7u64;
        let mut seg = None;
        for _ in 0..200 {
            if let FeedOutcome::Segment(Some(s)) = p.feed(block_at(-25.0, &mut seed), 16000) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("the cap must close the segment");
        assert_eq!(
            seg.close_reason,
            CloseReason::Cap,
            "a cap close must say so"
        );
        assert_eq!(
            seg.pcm.len() / 1280,
            VadConfig::default().max_blocks,
            "a capped segment must not lose its tail"
        );
    }

    /// A quiet room keeps the configured sensitivity: adaptation may only
    /// RAISE the thresholds, never lower them below what the user calibrated.
    #[test]
    fn a_quiet_room_keeps_the_configured_sensitivity() {
        let mut p = MicProcessor::new(VadConfig {
            start_db: -37.0,
            end_db: -42.0,
            ..VadConfig::default()
        });
        let mut seed = 11u64;
        for _ in 0..120 {
            let _ = p.feed(block_at(-65.0, &mut seed), 16000);
        }
        // −44 dB is above the offset threshold but below the onset one: the
        // configured calibration says "not speech", and adaptation must not
        // silently change that.
        for _ in 0..30 {
            assert!(
                !matches!(
                    p.feed(block_at(-44.0, &mut seed), 16000),
                    FeedOutcome::SpeechStarted
                ),
                "a quiet room must not get a more sensitive threshold than configured"
            );
        }
    }

    /// The floor estimate must never learn the user's voice, and a single
    /// bump must not move it: it is computed from NON-speech blocks only.
    #[test]
    fn the_floor_never_learns_speech_and_ignores_a_single_bump() {
        let mut p = MicProcessor::new(VadConfig {
            start_db: -45.0,
            end_db: -49.0,
            ..VadConfig::default()
        });
        let mut seed = 23u64;
        for _ in 0..100 {
            let _ = p.feed(block_at(-50.0, &mut seed), 16000);
        }
        let floor_before = p
            .noise_floor_db()
            .expect("a floor estimate after 8 s of room");
        assert!(
            (floor_before + 50.0).abs() < 2.0,
            "floor estimate {floor_before} is not the room"
        );

        // A single loud bump is not the room.
        let _ = p.feed(block_at(-8.0, &mut seed), 16000);
        let after_bump = p.noise_floor_db().unwrap();
        assert!(
            (after_bump - floor_before).abs() < 0.5,
            "one bump moved the floor to {after_bump}"
        );

        // 5 s of speech: the estimate is frozen while a segment is open.
        for _ in 0..60 {
            let _ = p.feed(block_at(-25.0, &mut seed), 16000);
        }
        let after_speech = p.noise_floor_db().unwrap();
        assert!(
            (after_speech - floor_before).abs() < 0.5,
            "speech moved the floor from {floor_before} to {after_speech}"
        );
    }

    /// Adaptation is ONE-WAY: it may raise the thresholds for a louder room,
    /// never lower them below the calibration (a quiet room must not become
    /// trigger-happy). The ordering invariant is what keeps the hysteresis
    /// real — if onset ever fell to the offset, every pause would re-open.
    #[test]
    fn adaptation_only_raises_and_keeps_the_hysteresis() {
        let mut p = MicProcessor::new(VadConfig {
            start_db: -37.0,
            end_db: -42.0,
            ..VadConfig::default()
        });
        assert_eq!(
            p.effective_thresholds(),
            (-37.0, -42.0),
            "before any room is known, the calibration governs"
        );
        let mut seed = 31u64;
        for _ in 0..100 {
            let _ = p.feed(block_at(-65.0, &mut seed), 16000);
        }
        assert_eq!(
            p.effective_thresholds(),
            (-37.0, -42.0),
            "a quieter room must never make the mic MORE sensitive than the calibration"
        );
        // The room rises to −40 dB, still below the onset: thresholds follow.
        for _ in 0..100 {
            let _ = p.feed(block_at(-40.0, &mut seed), 16000);
        }
        let (start, end) = p.effective_thresholds();
        let floor = p.noise_floor_db().unwrap();
        assert!(
            end - floor >= 5.5,
            "a −40 dB room must carry the offset threshold above itself (floor {floor}, end {end})"
        );
        assert!(
            start > end + 4.0,
            "onset must keep its hysteresis above offset (got {start} / {end})"
        );
    }

    /// M1b: with Silero installed, the per-block speech decision comes from
    /// the model — NOT from the dBFS thresholds. Blocks far below `start_db`
    /// open a segment when the detector says speech, and quiet blocks close it
    /// when it stops saying so.
    #[test]
    fn the_silero_detector_replaces_the_energy_threshold() {
        use crate::silero::SileroDetector;
        // `for_test` sees the GATED frame (sample/32768 x gain 3), so the fake
        // knee at 0.003 sits between a −70 dBFS room (0.0009) and −50 dBFS
        // speech (0.0095).
        let det = SileroDetector::for_test(|f| Ok(if f[0].abs() > 0.003 { 0.9 } else { 0.01 }));
        let mut p = MicProcessor::new(VadConfig {
            start_db: -37.0,
            end_db: -42.0,
            ..VadConfig::default()
        })
        .with_silero(det);
        let mut seed = 41u64;
        // ~9 s of room first so the energy path would have a floor estimate.
        for _ in 0..110 {
            let _ = p.feed(block_at(-70.0, &mut seed), 16000);
        }
        assert_eq!(p.endpoint_kind(), "silero");
        // −50 dBFS is 13 dB under the onset threshold: the energy detector
        // could never open here (see the quiet-room test above).
        let mut opened = false;
        for _ in 0..10 {
            if matches!(
                p.feed(block_at(-50.0, &mut seed), 16000),
                FeedOutcome::SpeechStarted
            ) {
                opened = true;
                break;
            }
        }
        assert!(
            opened,
            "the model's decision must open the segment, not the dBFS ceiling"
        );
        // Back to a level the detector calls silence (the block_at jitter is
        // ±2 dB, so aim well under the fake knee).
        let mut closed = None;
        for i in 0..20 {
            if let FeedOutcome::Segment(Some(seg)) = p.feed(block_at(-70.0, &mut seed), 16000) {
                closed = Some((i, seg));
                break;
            }
        }
        let (i, seg) = closed.expect("the model's silence must close the segment");
        assert!(
            i < 12,
            "closed after {} blocks ({:.2} s)",
            i + 1,
            (i + 1) as f32 * 0.08
        );
        assert!(!seg.pcm.is_empty());
    }

    /// M1b: the hard cap is in the shared state machine, so the Silero path
    /// must keep the tail exactly like the energy path (the S2 criterion
    /// "cap 不丢尾"): a capped close reports `Cap` and carries every block.
    #[test]
    fn a_silero_capped_segment_keeps_its_tail() {
        use crate::silero::SileroDetector;
        let det = SileroDetector::for_test(|_| Ok(0.9));
        let cfg = VadConfig::default();
        let max_blocks = cfg.max_blocks;
        let mut p = MicProcessor::new(cfg).with_silero(det);
        let mut seed = 47u64;
        let mut seg = None;
        for _ in 0..(max_blocks + 40) {
            if let FeedOutcome::Segment(Some(s)) = p.feed(block_at(-25.0, &mut seed), 16000) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("the cap must close a segment the model calls speech");
        assert_eq!(
            seg.close_reason,
            CloseReason::Cap,
            "a cap close must say so"
        );
        assert_eq!(
            seg.pcm.len() / 1280,
            max_blocks,
            "a capped Silero segment must not lose its tail"
        );
    }

    /// M1b rollback: a detector whose inference fails must not silence the
    /// mic. The block decision falls back to the energy thresholds for good,
    /// and the reason is exposed for the console.
    #[test]
    fn a_failed_model_falls_back_to_the_energy_vad() {
        use crate::silero::SileroDetector;
        let det = SileroDetector::for_test(move |_| Err("run: synthetic".to_string()));
        let mut p = MicProcessor::new(VadConfig {
            start_db: -37.0,
            end_db: -42.0,
            ..VadConfig::default()
        })
        .with_silero(det);
        let mut seed = 43u64;
        let mut opened = false;
        for _ in 0..30 {
            if matches!(
                p.feed(block_at(-25.0, &mut seed), 16000),
                FeedOutcome::SpeechStarted
            ) {
                opened = true;
                break;
            }
        }
        assert!(opened, "the energy fallback must still open on loud speech");
        assert!(
            p.endpoint_failure().is_some(),
            "the model failure must be reported, not hidden"
        );
    }

    #[test]
    fn vad_opens_on_speech_and_closes_after_hangover() {
        let mut p = MicProcessor::new(VadConfig {
            start_blocks: 3,
            end_blocks: 8,
            preroll_blocks: 3,
            min_blocks: 3,
            max_blocks: 150,
            ..VadConfig::default()
        });
        // Leading silence: nothing.
        for _ in 0..5 {
            assert!(matches!(
                p.feed(silence(80, 16000), 16000),
                FeedOutcome::None
            ));
        }
        // Speech: opens.
        let mut opened = false;
        for _ in 0..10 {
            if matches!(
                p.feed(tone(440.0, 80, 16000), 16000),
                FeedOutcome::SpeechStarted
            ) {
                opened = true;
            }
        }
        assert!(opened, "must open on loud blocks");
        // Hangover of quiet blocks closes it with a segment.
        let mut seg = None;
        for _ in 0..8 {
            if let FeedOutcome::Segment(s) = p.feed(silence(80, 16000), 16000) {
                seg = s;
            }
        }
        let seg = seg.expect("segment must close");
        // 3 preroll-ish + speech + partial hangover, minus the trimmed tail:
        // a plausible band, not an exact count.
        let blocks = seg.pcm.len() / 1280;
        assert!(
            (10..=18).contains(&blocks),
            "unexpected segment size: {blocks} blocks"
        );
    }

    #[test]
    fn vad_discards_too_short_blips() {
        let mut p = MicProcessor::new(VadConfig {
            start_blocks: 3,
            end_blocks: 8,
            min_blocks: 3,
            ..VadConfig::default()
        });
        for _ in 0..3 {
            let _ = p.feed(tone(440.0, 80, 16000), 16000); // opens at block 3
        }
        let mut got = None;
        for _ in 0..8 {
            if let FeedOutcome::Segment(s) = p.feed(silence(80, 16000), 16000) {
                got = s;
            }
        }
        // 3 loud blocks = exactly min_blocks → kept (non-empty pcm).
        assert!(!got.expect("segment").pcm.is_empty());
    }

    #[test]
    fn vad_resets_discard_partial() {
        let mut p = MicProcessor::new(VadConfig::default());
        for _ in 0..6 {
            let _ = p.feed(tone(440.0, 80, 16000), 16000);
        }
        assert!(p.in_speech());
        p.reset();
        assert!(!p.in_speech());
        // Next close yields nothing (state cleared).
        assert!(matches!(
            p.feed(silence(80, 16000), 16000),
            FeedOutcome::None
        ));
    }

    #[test]
    fn wake_matches_tone_insensitive_and_homophones() {
        // Exact + homophone + tone variants all match "高达" syllables.
        for text in ["高达", "你好高达", "糕达讲个故事", "高，达"] {
            let span = wake_span(text, "高达");
            assert!(span.is_some(), "must match: {text}");
        }
    }

    #[test]
    fn wake_rejects_near_misses() {
        // "告诉" is [gao,su] — one substitution from [gao,da]; a 2-syllable
        // wake word must NOT tolerate it.
        for text in ["告诉", "告诉我", "搞定了", "到点"] {
            assert!(wake_span(text, "高达").is_none(), "must not match: {text}");
        }
    }

    /// FIELD (2026-11-13, 1–4 m): the ASR hears the wake word as 郭达/郭大/
    /// 高单/高搭/您好. The near-homophone rule must accept those and still
    /// reject the near misses the exact rule was protecting.
    #[test]
    fn the_wake_word_tolerates_measured_mishearings_but_not_near_misses() {
        for text in [
            "你好，高达。",
            "你好，高大。",
            "您好，高达。",
            "你好，郭达。",
            "你好，郭大。",
            "你好，高单。",
            "你好，高搭。",
        ] {
            assert!(wake_span_near(text, "高达").is_some(), "must wake: {text}");
            assert!(
                wake_span_near(text, "你好高达").is_some(),
                "must wake (4 syllables): {text}"
            );
        }
        for text in [
            "你好，告诉。",
            "你好，高低。",
            "到点了。",
            "搞定了",
            "你好，隔壁。",
        ] {
            assert!(
                wake_span_near(text, "高达").is_none(),
                "must NOT wake: {text}"
            );
        }
        // 2026-11-14: a medial-glide difference (dao vs da) no longer rides
        // on a longer wake word — 郭导 has its own console entry, so the
        // measured 郭导 rendering still wakes through THAT word.
        assert!(wake_span_near("你好，郭导。", "高达").is_none());
        assert!(wake_span_near("你好，郭导。", "郭导").is_some());
        // The exact matcher keeps its old behaviour: the tolerance is opt-in.
        assert!(wake_span("你好，郭达。", "高达").is_none());
    }

    /// The two false wakes of 2026-11-14 (device journal 09:56/10:28), pinned
    /// as regressions: a coda letter and a short-syllable prefix must never
    /// turn room speech into a wake.
    #[test]
    fn room_speech_does_not_become_a_wake_word() {
        // 郭总 (guo zong) heard inside 国政/中国政府 (guo zheng).
        assert!(wake_span_near("这中国人和中国政府是两码事儿，是啥意思啊？", "郭总").is_none());
        assert!(wake_span_near("中国政，中国政府是两码事。", "郭总").is_none());
        // 高达 (gao da) heard inside 嘎达 (ga da).
        assert!(wake_span_near("我就想提你一下，这嘎达 C P 有这么难吗？", "高达").is_none());
        // The words themselves still match (the fix is not a blanket ban).
        assert!(wake_span_near("郭总，你在吗？", "郭总").is_some());
        assert!(wake_span_near("高达，讲个故事。", "高达").is_some());
        assert!(wake_span_near("你好，郭达。", "高达").is_some());
    }

    #[test]
    fn wake_span_char_indices_are_boundaries() {
        let text = "嗯，高达，讲个故事吧";
        let (s, e) = wake_span(text, "高达").expect("match");
        assert_eq!(&text[s..e], "高达");
        let rest = strip_wake(text, (s, e));
        assert_eq!(rest, "嗯，讲个故事吧");
    }

    #[test]
    fn strip_wake_handles_wake_only_input() {
        let text = "高达";
        let (s, e) = wake_span(text, "高达").unwrap();
        assert_eq!(strip_wake(text, (s, e)), "");
    }
}

#[cfg(test)]
mod d0_source_tests {
    use super::*;
    use crate::gateway::{InputOrigin, InputProvenance, TransportIdentity};

    fn source(epoch: &str, start: u64, count: usize) -> CaptureProvenance {
        CaptureProvenance::chunk(
            InputProvenance {
                origin: InputOrigin::Audio {
                    capture_epoch: epoch.into(),
                    ticket: format!("ticket-{epoch}"),
                    input_id: format!("input-{epoch}"),
                    start_sample: start,
                    end_sample: start + count as u64,
                },
                transport: Some(TransportIdentity {
                    generation: if epoch == "old" { 1 } else { 2 },
                    inbound_seq: start + 1,
                }),
            },
            count,
        )
    }

    #[test]
    fn d0_vad_keeps_preroll_open_source_and_trimmed_ranges_across_context_change() {
        let cfg = VadConfig {
            start_blocks: 2,
            end_blocks: 2,
            preroll_blocks: 3,
            min_blocks: 1,
            ..Default::default()
        };
        let mut p = MicProcessor::new(cfg);
        p.feed_with_provenance(vec![0; 1280], 16000, source("old", 0, 1280));
        p.feed_with_provenance(vec![10000; 1280], 16000, source("old", 1280, 1280));
        assert!(matches!(
            p.feed_with_provenance(vec![10000; 1280], 16000, source("old", 2560, 1280)),
            FeedOutcome::SpeechStarted
        ));
        let opened = p.open_provenance();
        p.feed_with_provenance(vec![0; 1280], 16000, source("new", 0, 1280));
        let FeedOutcome::Segment(Some(seg)) =
            p.feed_with_provenance(vec![0; 1280], 16000, source("new", 1280, 1280))
        else {
            panic!("closed segment")
        };
        assert_eq!(&seg.provenance.spans[..3], &opened.spans);
        assert_eq!(seg.provenance.spans.len(), 4); // final hangover block was trimmed
        assert_eq!(seg.provenance.samples(), seg.pcm.len());
        assert_eq!(
            seg.provenance.spans[0].provenance,
            source("old", 0, 1280).spans[0].provenance
        );
        assert_eq!(
            seg.provenance.spans[3].provenance,
            source("new", 0, 1280).spans[0].provenance
        );
    }

    #[test]
    fn d0_reset_clears_preroll_and_open_metadata_without_changing_audio() {
        let cfg = VadConfig {
            start_blocks: 1,
            end_blocks: 1,
            min_blocks: 1,
            ..Default::default()
        };
        let mut p = MicProcessor::new(cfg);
        p.feed_with_provenance(vec![10000; 1280], 16000, source("old", 0, 1280));
        p.reset();
        assert!(p.open_provenance().spans.is_empty());
        p.feed_with_provenance(vec![10000; 1280], 16000, source("new", 0, 1280));
        let FeedOutcome::Segment(Some(seg)) =
            p.feed_with_provenance(vec![0; 1280], 16000, source("new", 1280, 1280))
        else {
            panic!("closed segment")
        };
        assert!(seg.provenance.spans.iter().all(|s| matches!(&s.provenance.origin, InputOrigin::Audio {capture_epoch, ..} if capture_epoch == "new")));
    }

    #[test]
    fn d0_provenance_slice_tracks_partial_head_without_reissuing_source() {
        let source = source("old", 100, 1280);
        let slice = source.slice(200, 300);
        assert_eq!(slice.spans[0].provenance, source.spans[0].provenance);
        assert_eq!(slice.spans[0].offset_samples, 200);
        assert_eq!(slice.samples(), 300);
    }

    #[test]
    fn stage2_remote_vad_drives_slim_endpoint_while_local_silero_stays_authoritative() {
        let cfg = VadConfig {
            start_blocks: 2,
            end_blocks: 2,
            min_blocks: 1,
            ..Default::default()
        };
        let mut p = MicProcessor::new(cfg);
        assert_eq!(p.endpoint_kind(), "energy");

        // Slim/Energy processor: quiet PCM opens a segment when remote_vad == Some(true).
        assert!(matches!(
            p.feed_with_provenance_and_vad(
                vec![0; 1280],
                16000,
                CaptureProvenance::legacy(1280),
                Some(true)
            ),
            FeedOutcome::None
        ));
        assert_eq!(p.endpoint_kind(), "gateway_vad");
        assert!(matches!(
            p.feed_with_provenance_and_vad(
                vec![0; 1280],
                16000,
                CaptureProvenance::legacy(1280),
                Some(true)
            ),
            FeedOutcome::SpeechStarted
        ));

        // Loud PCM closes the segment when remote_vad == Some(false).
        assert!(matches!(
            p.feed_with_provenance_and_vad(
                vec![10000; 1280],
                16000,
                CaptureProvenance::legacy(1280),
                Some(false)
            ),
            FeedOutcome::None
        ));
        let FeedOutcome::Segment(Some(_seg)) = p.feed_with_provenance_and_vad(
            vec![10000; 1280],
            16000,
            CaptureProvenance::legacy(1280),
            Some(false),
        ) else {
            panic!("expected Segment close driven by remote_vad == Some(false)");
        };

        // When remote_vad is None (sidecar restarting / old gateway), falls back cleanly to local endpoint.
        let _ = p.feed_with_provenance_and_vad(
            vec![0; 1280],
            16000,
            CaptureProvenance::legacy(1280),
            None,
        );
        assert_eq!(p.endpoint_kind(), "energy");

        // Dot2 fat processor with local Silero attached: local Silero is authoritative
        // even if a stale/async gateway remote_vad tag arrives (`Some(false)`).
        use std::sync::{Arc, Mutex};
        let seen_frames: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let seen_c = Arc::clone(&seen_frames);
        let det = crate::silero::SileroDetector::for_test(move |_frame| {
            *seen_c.lock().unwrap() += 1;
            Ok(0.9)
        });
        let mut ps = MicProcessor::new(VadConfig {
            start_blocks: 2,
            end_blocks: 2,
            min_blocks: 1,
            ..Default::default()
        })
        .with_silero(det);
        assert!(ps.uses_silero());
        assert!(matches!(
            ps.feed_with_provenance_and_vad(
                vec![1000; 1280],
                16000,
                CaptureProvenance::legacy(1280),
                Some(false)
            ),
            FeedOutcome::None
        ));
        assert_eq!(ps.endpoint_kind(), "silero");
        assert!(matches!(
            ps.feed_with_provenance_and_vad(
                vec![1000; 1280],
                16000,
                CaptureProvenance::legacy(1280),
                Some(false)
            ),
            FeedOutcome::SpeechStarted
        ));
        assert_eq!(ps.endpoint_kind(), "silero");
        assert!(
            *seen_frames.lock().unwrap() >= 4,
            "local Silero must score every block without remote_vad override"
        );
    }

    #[test]
    fn clear_tainted_onset_drops_uncommitted_echo_run_while_preserving_wake_ack_overlap() {
        let cfg = VadConfig {
            start_blocks: 3,
            end_blocks: 2,
            preroll_blocks: 3,
            min_blocks: 1,
            ..Default::default()
        };
        let mut p = MicProcessor::new(cfg.clone());
        let mut echo = CaptureProvenance::legacy(1280);
        echo.spans[0].control_only = true;
        // Two loud echo blocks (< start_blocks=3) leave an uncommitted onset in preroll.
        assert!(matches!(
            p.feed_with_provenance(vec![10000; 1280], 16000, echo.clone()),
            FeedOutcome::None
        ));
        assert!(matches!(
            p.feed_with_provenance(vec![10000; 1280], 16000, echo),
            FeedOutcome::None
        ));
        assert!(p.onset_pending());
        assert!(p.clear_tainted_onset());
        assert!(!p.onset_pending());
        assert!(p.onset().is_none());
        // One post-play room reverb block cannot push above_run to 3 anymore.
        assert!(matches!(
            p.feed_with_provenance(vec![10000; 1280], 16000, CaptureProvenance::legacy(1280)),
            FeedOutcome::None
        ));
        assert!(!p.in_speech());

        // By contrast, a wake_ack_overlap onset (#215) is preserved across wake_ack PlayFinished.
        let mut pw = MicProcessor::new(cfg);
        let mut ack_ov = CaptureProvenance::legacy(1280);
        ack_ov.spans[0].control_only = true;
        ack_ov.spans[0].wake_ack_overlap = true;
        assert!(matches!(
            pw.feed_with_provenance(vec![10000; 1280], 16000, ack_ov.clone()),
            FeedOutcome::None
        ));
        assert!(matches!(
            pw.feed_with_provenance(vec![10000; 1280], 16000, ack_ov),
            FeedOutcome::None
        ));
        assert!(
            !pw.clear_tainted_onset(),
            "wake_ack_overlap onset must not be cleared"
        );
        assert!(pw.onset_pending());
        assert!(matches!(
            pw.feed_with_provenance(vec![10000; 1280], 16000, CaptureProvenance::legacy(1280)),
            FeedOutcome::SpeechStarted
        ));
    }
}

#[cfg(test)]
mod wake_ack_capture_tests {
    use super::*;

    #[test]
    fn wake_ack_handoff_requires_clean_speech_after_the_protected_prefix() {
        let mut protected = CaptureProvenance::legacy(1280).spans.remove(0);
        protected.control_only = true;
        protected.wake_ack_overlap = true;
        protected.speech = true;
        let quiet = CaptureProvenance::legacy(1280).spans.remove(0);
        let mut speech = quiet.clone();
        speech.speech = true;
        assert!(!CaptureProvenance {
            spans: vec![protected.clone()]
        }
        .allows_wake_ack_followup());
        assert!(!CaptureProvenance {
            spans: vec![protected.clone(), quiet]
        }
        .allows_wake_ack_followup());
        assert!(!CaptureProvenance {
            spans: vec![speech.clone(), protected.clone()]
        }
        .allows_wake_ack_followup());
        assert!(CaptureProvenance {
            spans: vec![protected.clone(), speech.clone()]
        }
        .allows_wake_ack_followup());
        protected.wake_ack_overlap = false;
        assert!(!CaptureProvenance {
            spans: vec![protected, speech]
        }
        .allows_wake_ack_followup());
    }

    #[test]
    fn xiaoai_wake_timeline_slicing_classifies_bare_wake_oneshot_and_stale_cursor() {
        let cfg = VadConfig::default();
        let loud = vec![10000i16; 1280];
        let quiet = vec![0i16; 1280];

        // 1) Bare wake: 9 loud blocks (720 ms wake word), arm 1 block before wake end
        //    (matching ~94 ms lag_blocks=1) plus 2 trailing reverb blocks = 3 blocks -> BareWake.
        let mut p = MicProcessor::new(cfg.clone());
        for _ in 0..9 {
            let _ = p.feed(loud.clone(), 16_000);
        }
        assert!(p.in_speech());
        let wake_end = p.current_block_cursor().saturating_sub(1);
        p.arm_wake_slice(wake_end);
        for _ in 0..2 {
            let _ = p.feed(loud.clone(), 16_000); // 1 pre-arrival tail + 2 reverb = 3 blocks (<= 3)
        }
        let mut seg = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p.feed(quiet.clone(), 16_000) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("bare wake segment must close");
        assert_eq!(seg.wake_slice, WakeSliceOutcome::BareWake);
        assert_eq!(seg.provenance.samples(), seg.pcm.len());

        // 2) Tight 0 ms one-shot command: 8 loud wake blocks (tag sample 11110), arm at wake_end,
        //    then 6 loud command blocks (tag sample 22220) + hangover -> CommandSliced (400 ms backoff).
        let mut p2 = MicProcessor::new(cfg.clone());
        let wake_blk = vec![11110i16; 1280];
        let cmd_blk = vec![22220i16; 1280];
        // Pre-onset clean blocks populate lead_pcm; CommandSliced must clear lead_pcm.
        for _ in 0..5 {
            let _ = p2.feed(quiet.clone(), 16_000);
        }
        for _ in 0..8 {
            let _ = p2.feed(wake_blk.clone(), 16_000);
        }
        let wake_end2 = p2.current_block_cursor();
        p2.arm_wake_slice(wake_end2);
        for _ in 0..6 {
            let _ = p2.feed(cmd_blk.clone(), 16_000);
        }
        let mut seg2 = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p2.feed(quiet.clone(), 16_000) {
                seg2 = Some(s);
                break;
            }
        }
        let seg2 = seg2.expect("one-shot segment must close");
        assert_eq!(seg2.wake_slice, WakeSliceOutcome::CommandSliced);
        assert!(
            seg2.lead_pcm.is_empty(),
            "sliced one-shot segment must clear pre-wake lead_pcm"
        );
        assert_eq!(
            seg2.provenance.samples(),
            seg2.pcm.len(),
            "provenance sample count must stay invariant after slicing"
        );
        // 8 wake blocks with 5-block Zipformer chunk-16 backoff -> opening wake blocks removed
        // while preserving command PCM cleanly.
        assert!(
            seg2.pcm.len() < (8 + 6 + 4) * 1280,
            "wake-word prefix must be physically sliced off"
        );
        assert!(
            seg2.pcm.contains(&22220),
            "post-wake command PCM must be preserved"
        );

        // 2b) Paused one-shot (e.g., 240 ms pause between wake word and command):
        //     must start right after the pause (1 quiet lead-in + command) without
        //     backing off across the silence into wake-word PCM (11110).
        let mut p2b = MicProcessor::new(cfg.clone());
        for _ in 0..8 {
            let _ = p2b.feed(wake_blk.clone(), 16_000);
        }
        let wake_end2b = p2b.current_block_cursor();
        p2b.arm_wake_slice(wake_end2b);
        for _ in 0..3 {
            let _ = p2b.feed(quiet.clone(), 16_000); // 240 ms pause (< end_blocks)
        }
        for _ in 0..6 {
            let _ = p2b.feed(cmd_blk.clone(), 16_000);
        }
        let mut seg2b = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p2b.feed(quiet.clone(), 16_000) {
                seg2b = Some(s);
                break;
            }
        }
        let seg2b = seg2b.expect("paused one-shot segment must close");
        assert_eq!(seg2b.wake_slice, WakeSliceOutcome::CommandSliced);
        assert_eq!(seg2b.provenance.samples(), seg2b.pcm.len());
        assert!(
            !seg2b.pcm.contains(&11110),
            "paused one-shot must not back off across silence into wake-word PCM"
        );
        assert!(
            seg2b.pcm.contains(&22220),
            "paused one-shot must preserve full command PCM"
        );

        // 3) Stale wake_cut_cursor predating a new segment must NOT slice the follow-up.
        let mut p3 = MicProcessor::new(cfg);
        for _ in 0..5 {
            let _ = p3.feed(quiet.clone(), 16_000);
        }
        p3.arm_wake_slice(2); // predates upcoming speech starting at block 6
        for _ in 0..6 {
            let _ = p3.feed(cmd_blk.clone(), 16_000);
        }
        let mut seg3 = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p3.feed(quiet.clone(), 16_000) {
                seg3 = Some(s);
                break;
            }
        }
        let seg3 = seg3.expect("follow-up segment must close");
        assert_eq!(
            seg3.wake_slice,
            WakeSliceOutcome::None,
            "stale cursor must not slice a subsequent follow-up segment"
        );
    }

    #[test]
    fn wake_slice_cursor_race_when_keyword_precedes_flushed_pcm() {
        let cfg = VadConfig {
            start_db: -37.0,
            end_db: -42.0,
            start_blocks: 3,
            end_blocks: 6,
            preroll_blocks: 2,
            min_blocks: 3,
            ..VadConfig::default()
        };
        let quiet = vec![0i16; 1280];
        let cmd_blk = vec![22220i16; 1280];

        // Case A: gateway sends Keyword before flushed PCM (`cut_cursor = 1`),
        // then during 200ms arbitration preroll + command PCM opens speech at
        // `speech_start_cursor = 4` (`> cut_cursor + 1`). `arm_wake_slice(1)`
        // fires while `in_speech` is true -> must classify as `CommandSliced`
        // without truncating post-wake command PCM.
        let mut p = MicProcessor::new(cfg.clone());
        let _ = p.feed(quiet.clone(), 16_000); // block 1
        let wake_end_before_flush = p.current_block_cursor(); // 1
        for _ in 0..2 {
            let _ = p.feed(quiet.clone(), 16_000); // blocks 2..3
        }
        for _ in 0..6 {
            let _ = p.feed(cmd_blk.clone(), 16_000); // blocks 4..9 -> opens speech
        }
        assert!(p.in_speech());
        p.arm_wake_slice(wake_end_before_flush);
        let mut seg = None;
        for _ in 0..10 {
            if let FeedOutcome::Segment(Some(s)) = p.feed(quiet.clone(), 16_000) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("post-wake command segment must close");
        assert_eq!(seg.wake_slice, WakeSliceOutcome::CommandSliced);
        assert!(seg.lead_pcm.is_empty());
        assert!(seg.pcm.contains(&22220));
        assert_eq!(seg.provenance.samples(), seg.pcm.len());

        // Case B: short bare wake (<= 3 speech blocks) after early cursor -> BareWake.
        let mut p_bare = MicProcessor::new(cfg.clone());
        let _ = p_bare.feed(quiet.clone(), 16_000);
        let cut = p_bare.current_block_cursor();
        for _ in 0..3 {
            let _ = p_bare.feed(quiet.clone(), 16_000);
        }
        for _ in 0..3 {
            let _ = p_bare.feed(cmd_blk.clone(), 16_000);
        }
        assert!(p_bare.in_speech());
        p_bare.arm_wake_slice(cut);
        let mut bare_seg = None;
        for _ in 0..10 {
            if let FeedOutcome::Segment(Some(s)) = p_bare.feed(quiet.clone(), 16_000) {
                bare_seg = Some(s);
                break;
            }
        }
        assert_eq!(bare_seg.unwrap().wake_slice, WakeSliceOutcome::BareWake);

        // Case C: cursor > 25 blocks behind even when active -> WakeSliceOutcome::None.
        let mut p_far = MicProcessor::new(cfg);
        for _ in 0..30 {
            let _ = p_far.feed(quiet.clone(), 16_000);
        }
        for _ in 0..6 {
            let _ = p_far.feed(cmd_blk.clone(), 16_000);
        }
        p_far.arm_wake_slice(1);
        let mut far_seg = None;
        for _ in 0..10 {
            if let FeedOutcome::Segment(Some(s)) = p_far.feed(quiet.clone(), 16_000) {
                far_seg = Some(s);
                break;
            }
        }
        assert_eq!(far_seg.unwrap().wake_slice, WakeSliceOutcome::None);
    }

    #[test]
    fn idle_gap_resets_stale_preroll_and_lead_pcm() {
        let cfg = VadConfig {
            start_db: -37.0,
            end_db: -42.0,
            start_blocks: 3,
            end_blocks: 6,
            preroll_blocks: 3,
            min_blocks: 3,
            ..VadConfig::default()
        };
        let mut p = MicProcessor::new(cfg);
        let old_marker = vec![333i16; 1280]; // quiet (~ -40 dBFS) distinct marker
        let new_speech = vec![20_000i16; 1280];
        let quiet = vec![0i16; 1280];
        let t0 = std::time::Instant::now();

        // Populate preroll + lead_pcm before idle-gate pause.
        for _ in 0..5 {
            let _ = p.feed(old_marker.clone(), 16_000);
        }
        // Short gap (< 500ms) must NOT flush preroll.
        p.last_feed_at = Some(t0);
        assert!(!p.reset_stale_preroll_if_gap(t0 + std::time::Duration::from_millis(200)));
        assert!(!p.preroll.is_empty());

        // Long idle gap (>= 500ms) MUST flush stale preroll and lead_pcm.
        assert!(p.reset_stale_preroll_if_gap(t0 + std::time::Duration::from_millis(800)));
        assert!(p.preroll.is_empty());
        assert!(p.lead_pcm.is_empty());

        // Now feed a fresh utterance; stale old_marker must NOT appear in pcm or lead_pcm.
        for _ in 0..4 {
            let _ = p.feed(new_speech.clone(), 16_000);
        }
        let mut seg = None;
        for _ in 0..10 {
            if let FeedOutcome::Segment(Some(s)) = p.feed(quiet.clone(), 16_000) {
                seg = Some(s);
                break;
            }
        }
        let seg = seg.expect("fresh segment must close");
        assert!(
            !seg.pcm.contains(&333) && !seg.lead_pcm.contains(&333),
            "stale pre-gap preroll must never leak into post-gap utterance"
        );
    }

    #[test]
    fn effective_control_only_allows_leading_tail_blind_grazing_only() {
        let mk = |control_only: bool, tail_blind_only: bool, speech: bool| SourceSpan {
            provenance: crate::gateway::InputProvenance::default(),
            offset_samples: 0,
            samples: 1280,
            control_only,
            wake_ack_overlap: false,
            tail_blind_only,
            speech,
        };

        // 2 leading tail_blind_only preroll blocks + 5 clean speech blocks -> NOT control_only.
        let clean_followup = CaptureProvenance {
            spans: vec![
                mk(true, true, false),
                mk(true, true, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, false),
            ],
        };
        assert!(!clean_followup.effective_control_only());

        // Pure echo inside tail_blind (no clean speech after) -> stays control_only.
        let pure_tail_echo = CaptureProvenance {
            spans: vec![
                mk(true, true, true),
                mk(true, true, true),
                mk(true, true, true),
                mk(false, false, false),
            ],
        };
        assert!(pure_tail_echo.effective_control_only());

        // Even 1 live voice/media span (`control_only && !tail_blind_only`) -> stays control_only.
        let live_overlap = CaptureProvenance {
            spans: vec![
                mk(true, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
            ],
        };
        assert!(live_overlap.effective_control_only());

        // Too many leading tail_blind blocks (> 3) -> stays control_only.
        let long_tail = CaptureProvenance {
            spans: vec![
                mk(true, true, true),
                mk(true, true, true),
                mk(true, true, true),
                mk(true, true, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
                mk(false, false, true),
            ],
        };
        assert!(long_tail.effective_control_only());
    }

    #[test]
    fn allows_wake_ack_followup_accepts_post_ack_tail_blind_bridge_and_rejects_answer_overlap() {
        let mk =
            |control_only: bool, wake_ack_overlap: bool, tail_blind_only: bool, speech: bool| {
                SourceSpan {
                    provenance: crate::gateway::InputProvenance::default(),
                    offset_samples: 0,
                    samples: 1280,
                    control_only,
                    wake_ack_overlap,
                    tail_blind_only,
                    speech,
                }
            };

        let ack = mk(true, true, false, true);
        let tail = mk(true, false, true, true);
        let answer = mk(true, false, false, true);
        let clean_speech = mk(false, false, false, true);
        let clean_quiet = mk(false, false, false, false);

        // [wake_ack_overlap, tail_blind_only, clean_speech] -> true
        assert!(CaptureProvenance {
            spans: vec![ack.clone(), tail.clone(), clean_speech.clone()],
        }
        .allows_wake_ack_followup());

        // [control_only (answer voice), clean_speech] -> false
        assert!(!CaptureProvenance {
            spans: vec![answer.clone(), clean_speech.clone()],
        }
        .allows_wake_ack_followup());

        // Mixed wake_ack_overlap + answer voice control_only -> false
        assert!(!CaptureProvenance {
            spans: vec![ack.clone(), answer, clean_speech.clone()],
        }
        .allows_wake_ack_followup());

        // Pure post-answer tail_blind_only (zero wake_ack_overlap) -> false
        assert!(!CaptureProvenance {
            spans: vec![
                tail.clone(),
                tail.clone(),
                tail.clone(),
                tail.clone(),
                clean_speech.clone(),
            ],
        }
        .allows_wake_ack_followup());

        // Pre-ack tail_blind_only before wake_ack_overlap -> false
        assert!(!CaptureProvenance {
            spans: vec![tail.clone(), ack.clone(), clean_speech],
        }
        .allows_wake_ack_followup());

        // [wake_ack_overlap, tail_blind_only] (no trailing clean speech) -> false
        assert!(!CaptureProvenance {
            spans: vec![ack.clone(), tail.clone()],
        }
        .allows_wake_ack_followup());

        // Trailing quiet only (no clean speech) -> false
        assert!(!CaptureProvenance {
            spans: vec![ack, tail, clean_quiet],
        }
        .allows_wake_ack_followup());
    }

    #[test]
    fn wake_slice_with_dur_handles_zipformer_lookahead_oneshot_bare_wake_and_paused_oneshot() {
        let cfg = VadConfig::default();
        let wake_blk = vec![11110i16; 1280];
        let cmd_blk = vec![22220i16; 1280];
        let quiet = vec![0i16; 1280];

        // 1) Tight oneshot ("你好小智，几点了？"): wake_dur_ms = Some(640) (8 blocks),
        //    15 total speech blocks (8 wake + 7 command), cut_cursor arrives late at
        //    block 14 due to Zipformer chunk-16 right-context lookahead -> MUST classify
        //    as CommandSliced and preserve the post-wake command blocks (blocks ~7..15).
        let mut p1 = MicProcessor::new(cfg.clone());
        for _ in 0..8 {
            let _ = p1.feed(wake_blk.clone(), 16_000);
        }
        for _ in 0..6 {
            let _ = p1.feed(cmd_blk.clone(), 16_000);
        }
        let late_cursor = p1.current_block_cursor(); // block 14
        assert_eq!(late_cursor, 14);
        p1.arm_wake_slice_with_dur(late_cursor, Some(640));
        let _ = p1.feed(cmd_blk.clone(), 16_000); // block 15
        let mut seg1 = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p1.feed(quiet.clone(), 16_000) {
                seg1 = Some(s);
                break;
            }
        }
        let seg1 = seg1.expect("tight oneshot segment must close");
        assert_eq!(
            seg1.wake_slice,
            WakeSliceOutcome::CommandSliced,
            "late Zipformer lookahead cursor must not misclassify tight oneshot as BareWake"
        );
        assert!(seg1.lead_pcm.is_empty());
        assert_eq!(seg1.provenance.samples(), seg1.pcm.len());
        let cmd_samples = seg1.pcm.iter().filter(|&&s| s == 22220).count();
        let wake_samples = seg1.pcm.iter().filter(|&&s| s == 11110).count();
        assert_eq!(
            cmd_samples,
            7 * 1280,
            "all 7 post-wake command blocks must be preserved"
        );
        assert_eq!(
            wake_samples, 1280,
            "effective_cut < cut_idx must retain exactly 1 block (80 ms) safety overlap"
        );

        // 2a) Genuine bare wake: wake_dur_ms = Some(680) with 8 speech blocks -> BareWake.
        let mut p2a = MicProcessor::new(cfg.clone());
        for _ in 0..8 {
            let _ = p2a.feed(wake_blk.clone(), 16_000);
        }
        p2a.arm_wake_slice_with_dur(p2a.current_block_cursor(), Some(680));
        let mut seg2a = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p2a.feed(quiet.clone(), 16_000) {
                seg2a = Some(s);
                break;
            }
        }
        assert_eq!(
            seg2a.expect("bare wake 680ms closes").wake_slice,
            WakeSliceOutcome::BareWake
        );

        // 2b) Genuine bare wake: wake_dur_ms = Some(840) with 10 speech blocks -> BareWake.
        let mut p2b = MicProcessor::new(cfg.clone());
        for _ in 0..10 {
            let _ = p2b.feed(wake_blk.clone(), 16_000);
        }
        p2b.arm_wake_slice_with_dur(p2b.current_block_cursor(), Some(840));
        let mut seg2b = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p2b.feed(quiet.clone(), 16_000) {
                seg2b = Some(s);
                break;
            }
        }
        assert_eq!(
            seg2b.expect("bare wake 840ms closes").wake_slice,
            WakeSliceOutcome::BareWake
        );

        // 3) Paused oneshot: wake_dur_ms = Some(640) (8 wake blocks), 2 quiet blocks,
        //    6 command blocks, late cut_cursor -> CommandSliced starting right before command.
        let mut p3 = MicProcessor::new(cfg);
        for _ in 0..8 {
            let _ = p3.feed(wake_blk.clone(), 16_000);
        }
        for _ in 0..2 {
            let _ = p3.feed(quiet.clone(), 16_000);
        }
        for _ in 0..5 {
            let _ = p3.feed(cmd_blk.clone(), 16_000);
        }
        let late_paused_cursor = p3.current_block_cursor(); // block 15
        p3.arm_wake_slice_with_dur(late_paused_cursor, Some(640));
        let _ = p3.feed(cmd_blk.clone(), 16_000); // block 16
        let mut seg3 = None;
        for _ in 0..12 {
            if let FeedOutcome::Segment(Some(s)) = p3.feed(quiet.clone(), 16_000) {
                seg3 = Some(s);
                break;
            }
        }
        let seg3 = seg3.expect("paused oneshot closes");
        assert_eq!(seg3.wake_slice, WakeSliceOutcome::CommandSliced);
        assert_eq!(seg3.provenance.samples(), seg3.pcm.len());
        assert!(
            !seg3.pcm.contains(&11110),
            "paused oneshot must slice cleanly past the pause with zero wake PCM"
        );
        assert_eq!(
            seg3.pcm.iter().filter(|&&s| s == 22220).count(),
            6 * 1280,
            "paused oneshot must preserve all 6 command blocks"
        );
    }
}
