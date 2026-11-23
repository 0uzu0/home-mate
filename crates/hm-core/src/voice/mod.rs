//! Voice pipeline: per-device interaction state machine.
//!
//! Three dimensions are modeled SEPARATELY (voice-interaction-and-control
//! §2, architecture principles §4):
//! - duplex (audio capability / requested / negotiated / effective) — what
//!   the link can do and actually does;
//! - participation — whether a conversation window is open on the device;
//! - proactive speech — whether the assistant may speak unprompted (a
//!   policy flag; nothing proactive exists in M2, and the default is off).
//!
//! Half-duplex is the default behavior: a wake or admitted utterance while outputs
//! are active stops them (barge-in) instead of mixing. Full duplex only
//! differs when a device with that capability negotiates it — nothing fakes
//! it (the virtual dot2-d honestly reports `half`).
//!
//! Ownership: this module owns interaction semantics only. Connections,
//! receipts and generation/seq freshness live in the gateway hub; task
//! validity stays in the TaskRegistry; the model never sees mode writes.

mod barge;
#[cfg(test)]
mod d1_tests;
#[cfg(test)]
mod d2_tests;
#[cfg(test)]
mod d3_tests;
mod input;
mod input_turn;
mod mode;
mod owner;
mod participation;
mod playback;
mod pump;
mod reflex;
mod settings;
#[cfg(test)]
mod takeover_tests;
#[cfg(test)]
mod tests;
mod turn;
#[cfg(test)]
mod v2_tests;
pub use barge::*;
use mode::parse_first_json;
pub use mode::VOLUME_CAP;
use participation::{CloseReason, Identity, Participation, RequestTicket, TurnTicket};
pub use settings::*;

use crate::agent::{AgentEnv, AgentEvent};
use crate::gateway::{DeviceEvent, DeviceHub, DuplexState, GatewayFrame, PlayRequest};
use crate::id;
use crate::interaction::{self, Content, Duplex, Trigger};
use crate::llm::{LlmEvent, WireMessage};
use crate::mic::{self, FeedOutcome, MicProcessor, VadConfig};
use crate::store::Store;
use crate::task::TaskRegistry;
use futures::StreamExt;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{broadcast, mpsc};

/// XiaoAi-style L1+L2 acoustic telemetry & pre-arbitration timeline cursor (#261).
#[derive(Debug, Clone, Default)]
pub(super) struct WakeAcoustics {
    pub(super) tier: Option<String>,
    pub(super) wake_dur_ms: Option<u64>,
    pub(super) max_gap_ms: Option<u64>,
    pub(super) l2_passed: Option<bool>,
    pub(super) wake_end_cursor: Option<u64>,
}

/// Runtime rollback switch for Tier-2 physical wake-word timeline slicing
/// (`HM_WAKE_TIMELINE_SLICE=0` or `false` restores legacy full-segment ASR).
pub(super) fn wake_timeline_slice_enabled() -> bool {
    !matches!(
        std::env::var("HM_WAKE_TIMELINE_SLICE").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE") | Ok("off") | Ok("OFF")
    )
}

/// A finished ASR transcription of one mic segment, re-entering the pump.
struct MicTranscript {
    /// Frozen from retained segment PCM, before asynchronous ASR starts.
    ticket: crate::mic::CaptureProvenance,
    /// Production results consume the exact pending reservation. False is
    /// reserved for the explicit legacy transcript test seam.
    leased: bool,
    device: String,
    result: Result<String, String>,
    duration_ms: u64,
    /// Monotonic per device, reserved at the captured speech onset.
    /// Two windows' transcripts can arrive out of order; the id says which
    /// capture produced this text.
    input_id: u64,
    /// The mic processor's own block cursor at close (`total_blocks_seen`) —
    /// the capture's position in the device's audio stream.
    capture_seq: u64,
    /// Cancellation epoch frozen by input admission. This is distinct from
    /// the source-issued capture epoch in ticket.
    window_epoch: u64,
    control_only: bool,
    /// True when Tier-2 timeline slicing physically removed the wake word prefix
    /// before ASR so the post-wake command does not need to repeat the wake word (#261).
    wake_sliced: bool,
    enqueued_at: std::time::Instant,
}

/// M5: what the DEVICE confirmed for one submitted play. The three positions a
/// delivery must keep apart (2026-11-13):
///   * generated — the text exists (`chars`/`sentences` counted by the caller),
///   * submitted — `play_id` went on the wire,
///   * consumed  — `consumed_ms` from the terminal, trustworthy ONLY when
///     `progress_basis == "hw_delay"`. A "written" basis says how much we
///     handed the DAC, not how much left it.
#[derive(Debug, Clone, Default)]
pub struct SpokenTerminal {
    pub play_id: String,
    /// finished | stopped:<stage> | failed | unknown | timeout | link_lost
    pub stage: String,
    pub played_ms: Option<u64>,
    pub consumed_ms: Option<u64>,
    pub progress_basis: Option<String>,
    pub detail: Option<String>,
}

impl SpokenTerminal {
    /// The DAC-consumed milliseconds, available only when the device measured
    /// the hardware backlog. `None` means "not observable" — never 0.
    pub fn consumed(&self) -> Option<u64> {
        match self.progress_basis.as_deref() {
            Some("hw_delay") => self.consumed_ms,
            _ => None,
        }
    }

    /// Played through AND the device measured what it consumed.
    pub fn heard(&self) -> bool {
        self.stage == "finished" && self.consumed().map(|ms| ms > 0).unwrap_or(false)
    }

    /// Played through, but the device could not measure the DAC: the audio
    /// was WRITTEN in full. Deliberately not `heard()`.
    pub fn written_fully(&self) -> bool {
        self.stage == "finished" && self.consumed().is_none()
    }

    /// Any terminal that means the play did not run to its end.
    pub fn interrupted(&self) -> bool {
        self.stage != "finished"
    }
}

/// Slack on top of a sentence's own duration when waiting for its terminal
/// (a device that never answers must not stall a program forever).
const SPOKEN_TERMINAL_SLACK_MS: u64 = 12_000;

/// One live v0.3 streamed play owned by a device turn (T3): the wire
/// task id, the chunk seq the core owns, the stop epoch it opened under,
/// and the ticket whose receipt resolves on the device's terminal stage.
struct TurnStream {
    play_id: String,
    seq: u64,
    first_forward_ms: Option<u64>,
    /// Stream-level send pacing (2026-11-12): seconds of audio forwarded so
    /// far, and the wall clock the first piece left at. The budget is spent
    /// across the WHOLE play — reset per sentence it meant that N sentences
    /// each burst their own allowance, and the device measured 81 s ahead of
    /// playback (its 60 s backstop then cut a real answer at 20.8 s).
    sent_secs: f64,
    paced_from_ms: Option<u64>,
    _ticket: crate::gateway::PlayTicket,
}

/// Captured when the operation is accepted, never after synthesis/queueing.
#[derive(Clone, Copy)]
struct OutputPermit {
    generation: u64,
    stop_epoch: u64,
    request: Option<RequestTicket>,
}
impl OutputPermit {
    fn valid(self, st: &DeviceVoiceState) -> bool {
        self.generation == st.participation.generation()
            && self.stop_epoch == st.stop_epoch
            && self
                .request
                .is_none_or(|r| st.participation.is_active_request(r))
    }
}

#[derive(Clone)]
struct OutputRecord {
    /// Text actually submitted for this answer, used only with measured overlap.
    answer_text: String,
    request: Option<RequestTicket>,
    task_id: Option<String>,
    channel: String,
    /// R2-BM5: the output CLASS a receipt belongs to. B-M0 separated the
    /// events (`filler_played`/`cancelled`) but the receipt itself had no
    /// class, so the console could not tell an answer from a cue.
    /// answer | prompt | filler | ack (media plays are added by the hub view).
    kind: String,
    content_ref: String,
    stage: String, // submitted | device_confirmed | failed | unknown | stopping
    started_ms: u64,
    /// When `stage` last changed by the core or by a device receipt. The two
    /// residual sweeps age records by this, not by `started_ms` (a long
    /// answer's start says nothing about how long it has been stopping).
    stage_ms: u64,
    /// When the core closed the voice stream (`play_end`). `None` until then;
    /// the play-stall bound counts from here. A whole-WAV play never sends
    /// `play_end`, so it is exempt (its end is one `play_finished` away).
    end_sent_ms: Option<u64>,
    /// R2 (2026-11-13): how much audio this play carried in total (ms). The
    /// stall bound must not cut a healthy play that is still DRAINING: the
    /// device cannot play faster than realtime, so the earliest possible end
    /// is `started_ms + expected_ms`. 0 = unknown (whole-WAV / older paths),
    /// which falls back to the plain post-`play_end` bound.
    expected_ms: u64,
    /// TTS synthesis duration for voice plays (latency accounting, G4).
    synth_ms: u64,
}

/// R1j/P1: the last keyword deployment attempt for a device, as the DEVICE
/// answered it (state flows sent → accepted|rejected).
#[derive(Clone, Default)]
pub(crate) struct ReflexDeploy {
    pub(crate) state: String,
    /// What the DEVICE says it installed (receipt).
    pub(crate) hash: String,
    /// What this core COMPILED and sent. Kept apart from `hash`: "sent" and
    /// "installed" are different facts, and neither proves a load.
    pub(crate) sent_hash: String,
    /// M6b: what the spotter reported it LOADED (its `ready` line), and when.
    pub(crate) loaded_hash: Option<String>,
    pub(crate) loaded_ms: u64,
    pub(crate) keywords: u64,
    pub(crate) detail: String,
    pub(crate) sent_ms: u64,
    pub(crate) acked_ms: u64,
    /// Contract v2 §2.5 (#5): how many retreat words this deploy asked the
    /// device to arm, and how many the DEVICE says it armed. Equal and > 0 =
    /// local exit is live; `local_exits` counts the facts it has reported.
    pub(crate) retreat_words_sent: u32,
    pub(crate) local_exit: u32,
    /// The word→intent map SENT and the one the device stored (receipt echo).
    pub(crate) intents_hash: String,
    pub(crate) intents_hash_installed: String,
    pub(crate) local_exits: u64,
    /// The last `local_exit_flushed` fact (ms / latency / channels / word).
    pub(crate) last_local_exit: Option<serde_json::Value>,
    /// Contract v2 #4: the sensitivity this deploy SENT, and the numbers the
    /// loaded model reported back (0.0 = not reported).
    pub(crate) kws_threshold: f64,
    pub(crate) kws_score: f64,
    pub(crate) applied_threshold: f32,
    pub(crate) applied_score: f32,
}

/// The live waiting cue for a device (B-M0). Tracked apart from `outputs`
/// because the answer's first audio must be able to REPLACE it: the cue is
/// not an answer, and its end must not be mistaken for one.
#[derive(Clone)]
struct FillerPlay {
    play_id: String,
    scene: String,
    phrase: String,
}

/// Process-lifetime mic counters per device (G4 calibration evidence:
/// wake rate / false-wake context / ASR health at a glance).
#[derive(Default, Clone, Copy)]
struct MicCounters {
    speech_started: u64,
    segments: u64,
    segments_discarded: u64,
    wakes: u64,
    followup_turns: u64,
    ignored_unwoken: u64,
    asr_failed: u64,
    self_echo_suppressed: u64,
    /// Segments swallowed by the post-play tail blind window (echo fix v2).
    tail_suppressed: u64,
    /// Chunks dropped because OUR OWN voice was live (half duplex). Counted
    /// since 2026-11-14: a person talking over an answer and being ignored
    /// used to leave no trace at all (codex audit, finding 5).
    suppressed_during_voice: u64,
    /// Transcripts submitted to ASR and not yet back (2026-11-14, codex
    /// finding 9). A segment closing clears `in_speech`, but the person's
    /// words only become a turn when the transcript returns — a proactive
    /// line (a reminder) that starts in that gap talks over someone who has
    /// just finished a sentence.
    asr_inflight: u64,
    /// Blocks discarded in the 250 ms after an accepted barge-in (R1:
    /// the stopped answer's DAC/room tail must not become the user's words).
    barge_tail_dropped: u64,
    /// R1g: segments whose ASR result was empty. They used to be only a log
    /// line, which is why "the 6.48 s segment transcribed to nothing" could
    /// not be counted against anything (P7).
    segment_empty: u64,
    /// Segments that ended on the hangover (the person finished).
    close_quiet: u64,
    /// Segments that ran into `max_blocks` (the VAD never saw the room go
    /// quiet — a drifting floor, or someone talking for 12 s). Counted apart
    /// from `close_quiet` because only one of them is a sentence end.
    close_cap: u64,
    /// Segments NOT sent to ASR because `asr_wake_fallback` is off and the
    /// session was closed (idle room speech). Counted so the privacy trade-off
    /// is visible as a number, not a claim — the console shows it.
    asr_skipped: u64,
    /// KWS wake hits dropped before arbitration by Tier-1 L2/duration rejection
    /// or Tier-3 idle mid-segment gating (#261).
    kws_suspect_dropped: u64,
    /// L1+L2 verified bare wakes (`wakeup-normal`) confirmed locally without
    /// invoking ASR (#261).
    bare_wake_asr_skipped: u64,
}

/// One completed voice-play latency sample (G4 accounting): TTS synthesis
/// time and submit→device_confirmed delivery time.
#[derive(Clone, Copy, serde::Serialize)]
struct LatencySample {
    synth_ms: u64,
    confirm_ms: u64,
}

/// A spoken program (R1e, RUI-local `SpokenProgram` semantics): the text the
/// device is reading plus the byte offset of the sentence currently in the
/// speaker. A pause freezes this at a sentence boundary; 继续 resumes there.
/// Ordinary chat answers are programs too — they are simply discarded instead
/// of bookmarked, because only an explicit pause asks for them back.
#[derive(Clone, Default)]
struct Program {
    text: String,
    /// Byte offset of the sentence being spoken. On resume, speaking starts
    /// HERE, so the sentence the user cut is repeated rather than skipped.
    cursor: usize,
    /// False while the model is still generating: a paused program may then
    /// hold only the part that existed when the user asked to stop.
    complete: bool,
    origin_task: Option<String>,
    created_ms: u64,
    /// True when this turn's delivery shape was long content or a detail
    /// answer — the only programs worth resuming with 「继续」 and the only
    /// ones the truncation notice may describe. A one-line answer or a music
    /// confirmation is not a program (device 2026-11-13).
    resumable: bool,
    /// True when this bookmark exists because the SYSTEM stopped the delivery
    /// (the spoken cap ran out) rather than the person. A system cut is an
    /// offer, not a pause: it does not light the amber ring and it dies with
    /// the follow window (user decision 2026-11-14; device report: a cut
    /// answer kept the ring amber through a session exit, and 「继续」 two
    /// minutes later replayed its tail as if it were a fresh answer).
    system_cut: bool,
    /// The short-answer size when this program was created. What remains
    /// unsaid must exceed it for the program to be worth a bookmark: a plain
    /// answer is discarded, a real piece of content is kept. Replaces the
    /// keyword-derived `resumable` for model-shaped turns (2026-11-14);
    /// 0 = not tracked (then only an explicit `resumable` counts).
    short_chars: usize,
    /// The runaway backstop snapshotted at creation. A continuation reads up
    /// to this much per 「继续」 — a story resumes in paragraphs, not in
    /// two-sentence crumbs (device 2026-11-14: every 「继续」 gave 2 sentences
    /// and asked for 「继续」 again).
    cont: Option<crate::agent::prompt::SpokenBudget>,
    /// The resume lead labels the FIRST continuation only; repeating it on
    /// every 「继续」 is noise.
    lead_said: bool,
}

/// The conversation session (R1f object model): `device` owns mute/volume,
/// `session` owns the follow window, `program` owns the bookmark. Presentation
/// reads those objects; it never keeps a second copy of their meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Idle,
    Listening,
    /// A retreat/close is in progress: a local stop arriving now must not
    /// turn into a bookmark (the amber the user saw after 退下).
    Closing,
}

/// See [`VoicePipeline::in_window_command`].
#[derive(Debug, Clone, PartialEq)]
enum InWindow {
    /// Nothing but the wake phrase (or a greeting in its wake): do nothing —
    /// the window is already open and the KWS hit itself was silent.
    Bare,
    Command(String),
}

#[derive(Debug, Clone, Copy)]
struct Session {
    state: SessionState,
    since_ms: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
            since_ms: 0,
        }
    }
}

impl Session {
    fn open(&mut self, now: u64) {
        if self.state != SessionState::Listening {
            self.state = SessionState::Listening;
            self.since_ms = now;
        }
    }
    fn close(&mut self, now: u64) {
        self.state = SessionState::Idle;
        self.since_ms = now;
    }
    fn label(&self) -> &'static str {
        match self.state {
            SessionState::Idle => "idle",
            SessionState::Listening => "listening",
            SessionState::Closing => "closing",
        }
    }
}

/// A play stage that still holds the channel. `stopping` is on its way out;
/// failed/unknown are not output any more.
fn stage_is_live(stage: &str) -> bool {
    !matches!(stage, "failed" | "unknown" | "stopping")
}

#[derive(Default)]
struct DeviceVoiceState {
    current_capture_epoch: Option<String>,
    input: Option<input::InputLease>,
    input_status: Option<&'static str>,
    participation: Participation,
    /// Last ModeReport facts; None until the device reports.
    duplex: Option<DuplexState>,
    /// Hub arrival sequence of the LAST ModeReport (0 = never). Not a wall
    /// clock: a report that arrived before a management command must never
    /// confirm it, even in the same millisecond.
    duplex_report_seq: u64,
    aec: Option<crate::gateway::AecState>,
    muted: Option<bool>,
    /// v1 mechanism (#8/#13): WHO last wrote the latch — the device's own
    /// `MuteState.source` ("button"/"hardware"/"device_boot"/"core_set"). Shown by
    /// the console badge; a bare `muted:true` cannot say whether the person
    /// in the room pressed the key or the household plane answered.
    mute_source: Option<String>,
    /// R2 (2026-11-13): who opened the FOLLOW WINDOW. `Some("keyword")` means
    /// the device spotter opened it and no turn has consumed it yet — so an
    /// ASR transcript that is only the wake phrase is the person's whole
    /// input and deserves the ack. Any other opener (a turn, an ASR wake, a
    /// barge) clears it: a later wake phrase in the SAME window is our own
    /// echo bleeding into the next segment (codex interaction review B-2).
    window_opener: Option<&'static str>,
    /// Input lease ID transferred across the most recent KWS wake boundary
    /// (`restore_wake_capture_locked`), used to recognize when a short (`<=1.6s`,
    /// `<=5` chars) wake-transferred segment was mis-transcribed by ASR into a
    /// near-homophone (e.g. `"据我所知。"` / `"可知。"` for `"你好小智"`) with no
    /// trailing command.
    last_wake_transferred_input_id: Option<u64>,
    /// Follow-window state as seen by the last ticker sweep (tone edge
    /// detection): fires the end tone on open→closed by timeout.
    follow_open_last_tick: bool,
    /// Post-play tail blind window (echo fix v2): room reverb beyond the
    /// gateway AEC's 160 ms reference window (TAIL = 2560 samples at 16 kHz) is
    /// uncancelable by design — segments opening inside this window after OUR
    /// voice play are self-echo tails.
    self_play_blind_until_ms: u64,
    wake_ack_play: Option<String>,
    wake_ack_tail_until_ms: u64,
    other_voice_tail_until_ms: u64,
    current_task: Option<String>,
    /// Delivery invalidation counter: bumped by every stop/barge-in on this
    /// device. A speak() that returns from synthesis under a different
    /// epoch than it started under is a late answer — suppressed even if
    /// its task already reached Completed (request_cancel cannot flag a
    /// finished task, so the epoch is the authoritative "user said stop").
    stop_epoch: u64,
    /// Next input id (reserved at onset) and the highest id
    /// already ACTED on — a later-arriving older capture is out of order.
    mic_input_seq: u64,
    last_transcript_input: u64,
    outputs: HashMap<String, OutputRecord>,
    // --- G4: assistant presentation state (LED, mapping gap G8) ---
    /// Last assistant_state actually SENT to the gateway ("" = never; the
    /// wire value is the honest "what the gateway was told", not intent).
    assistant_last: String,
    fleet_playback_channel: &'static str,
    fleet_playback_preview: String,
    fleet_playback_sent_ms: u64,
    wake_flash_until_ms: u64,
    error_flash_until_ms: u64,
    // --- G4: mic session/stream telemetry (console debt) ---
    /// Achieved mic session state from the gateway's mic_report (None =
    /// never reported).
    mic_active: Option<bool>,
    mic_detail: Option<String>,
    last_chunk_ms: Option<u64>,
    /// When the last mic segment closed (2026-11-14). A proactive line (a
    /// reminder) waits a beat after this: the person has just finished a
    /// sentence and is still in the room.
    mic_last_close_ms: Option<u64>,
    chunk_window_start_ms: u64,
    chunk_window_count: u64,
    chunks_per_sec: Option<f64>,
    mic_counters: MicCounters,
    /// Rolling voice latency samples (last 20, G4 accounting).
    latency: std::collections::VecDeque<LatencySample>,
    // --- D/T4: keyword interruption (device-side reflex + core decision) ---
    /// Last spotter state reported by the gateway ("starting"|"running"|"down").
    kws_state: Option<String>,
    kws_detail: Option<String>,
    kws_restarts: u64,
    kws_detections: u64,
    /// Self-heard hits the device echo guard suppressed (v0.2.5).
    kws_guarded: u64,
    /// Contract v2 §2.5 (#5): retreat words the DEVICE has armed for a local
    /// exit. A device fact that survives core restarts — the deploy record in
    /// this struct does not, so this is what `/mode` and `hm-release` read.
    kws_local_exit: u32,
    /// Contract v2 #4: the sensitivity the DEVICE reports its spotter runs
    /// with (0.0 = not reported, an older gateway).
    kws_threshold: f32,
    kws_score: f32,
    /// Keyword events this pipeline ACTED on (stop submitted).
    keyword_stops: u64,
    /// R1g: keyword hits that were a WAKE (nothing was playing) — window +
    /// ack instead of a stop. Same entry point, different matrix cell.
    keyword_wakes: u64,
    /// Keyword events recorded but not acted on (with reason in the log).
    keyword_ignored: u64,
    last_keyword_ms: u64,
    /// R1j/P1: per-word counts from the reflex table — each word is judged
    /// on its own (a false-trigger budget is per phrase). Keyed by the word
    /// as configured, values are lifetime counts for this core process.
    reflex_hits: std::collections::BTreeMap<String, u64>,
    reflex_ignored: std::collections::BTreeMap<String, u64>,
    /// R1j/P1: the same word executed from its TRANSCRIPT (the spotter missed
    /// it, or the text fallback ran first). Kept apart from `reflex_hits` so
    /// the console can say which path a word actually takes.
    reflex_text_hits: std::collections::BTreeMap<String, u64>,
    /// The last intent the SPOTTER executed and when (dedupe window for the
    /// same speech arriving later as an ASR transcript).
    last_reflex: Option<(interaction::KeywordIntent, u64)>,
    /// Last keyword deployment and the device's answer.
    reflex_deploy: Option<ReflexDeploy>,
    /// R1g/P7: the GATEWAY process epoch this core's keyword counters belong
    /// to (0 = never reported). A gateway restart keeps its lifetime counters
    /// while the core's are zeroed, so the two numbers were never comparable
    /// (device log 2026-11-12: gateway `detections=14`, core `stops=0`).
    kws_epoch: u64,
    /// The gateway's `detections` value when THIS core first saw that epoch;
    /// `detections - base` is the window the core's own stops share.
    kws_detections_base: u64,
    kws_epoch_seen_ms: u64,
    // --- D/T5: semantic barge-in (near-end speech during voice playback) ---
    /// Barge-in facts this pipeline ACTED on (stop + follow window + preroll).
    barge_in_stops: u64,
    /// Barge-in facts recorded but not acted on (reason in the log).
    barge_in_ignored: u64,
    /// R1g: near-end facts the HALF-DUPLEX rule answered with "no stop, no
    /// window". A normal fact on a half-duplex link, not an error — counted
    /// separately from `barge_in_ignored` (which means "untrusted fact").
    barge_in_suppressed: u64,
    last_barge_in_ms: u64,
    barge: Barge,
    input_connection_generation: Option<u64>,
    /// Answer protection period (D/T5 R1): a barge-in fact inside it is
    /// ignored — the fresh answer's own TTS onset decorrelates the echo
    /// residual, which is what cut every answer off on 2026-11-09.
    barge_protect_until_ms: u64,
    /// #10: single live turn per device invariant. Monotonically increasing
    /// turn id; `turn.rs` checks `st.live_turn == Some(my_turn_id)` before
    /// submitting `play_begin` / `speak_stream_segment`.
    next_turn_seq: u64,
    live_turn: Option<u64>,
    /// When the last ANSWER play on this device ended (PlayFinished). Used to
    /// tell streamed continuation pieces (no new protection period) from a
    /// genuinely new answer (protection re-arms). Participation owns its
    /// own tail reference so an old request cannot move a newer window.
    /// Contract v2 #4 (2026-11-16): only `kind == "answer"` writes this —
    /// an ack/filler/prompt's end is not a continuation of anything.
    last_voice_end_ms: u64,
    /// When the last MEDIA play on this device ended (used for the 2000 ms
    /// post-media mic follow-window guard, #77).
    last_media_end_ms: u64,
    /// The window closed but its spoken cue has not been played yet (the
    /// device was playing something). Retried on each sweep until it lands,
    /// or dropped after FOLLOW_CLOSE_WAIT_MS.
    follow_close_pending_at: Option<u64>,
    /// B-M0: the live waiting cue, if any. At most one per turn; replaced
    /// (stopped) the moment the answer's first audio is submitted.
    filler_play: Option<FillerPlay>,
    /// B-M0: monotonic turn counter for cue rotation (deterministic in
    /// tests, unlike the wall clock).
    filler_turn_seq: u64,
    /// B-M1: the last phrase this device SPOKE from the local cache (wake
    /// ack, retreat ack, waiting cue) and when. A transcript that begins
    /// with it is our own echo, not the user's words.
    last_cue: Option<(String, u64)>,
    /// B-M1: the last "keep going" intent that arrived too late to save the
    /// playback (semantic barge-in had already stopped it) — measured, not
    /// guessed. Counted per device for the acceptance record.
    continue_intents: u64,
    /// R1d: the last local stop the DEVICE performed on its own reflex
    /// (task_id, device ts). It is the hardware proof that the barge-in
    /// fact's audio is already gone; `on_barge_in` accepts such a fact even
    /// though no play is live any more, and consumes the marker once.
    local_stop: Option<(String, u64)>,
    /// R2 (2026-11-13): the next turn skips its waiting cue because it was
    /// already introduced by a local wake ack. Consumed once; time-bounded.
    suppress_cue_until_ms: Option<u64>,
    /// R1e: the answer this turn is speaking. A pause freezes it into
    /// `paused_program` at the sentence boundary the user cut.
    live_program: Option<Program>,
    /// R1e: a program the user paused. ONLY an explicit 继续 plays it again —
    /// a new question, a wake or an unrelated transcript must never resume it
    /// (RUI-local: 手动暂停不能变成自动续播).
    paused_program: Option<Program>,
    /// M6b (S3): what the device actually did with THIS turn's answer audio.
    /// Updated by the event pump (submit / play_finished / stop_stage), read
    /// once when `turn_end` is written. Kept per device, not per task: the
    /// same turn may stream several sentences and each is its own play.
    turn_audio: TurnAudio,
    /// M7-1: the last time a session-end memory extraction ran for this
    /// device. One conversation = one extraction; the bound also keeps a
    /// flapping session from spending two model calls per minute.
    last_memory_extract_ms: u64,
    /// Wake-word attribution for the next turn: `(channel, word, at_ms)` where
    /// `channel` is `"kws"` or `"asr"`. Consumed once when the turn starts.
    last_wake: Option<(String, String, u64)>,
    /// Whether this device's explicitly saved `UnitAcousticProfile` has already
    /// been reconciled during the current online session (reset on join/offline).
    acoustics_reconciled: bool,
}

impl DeviceVoiceState {
    /// Claim a monotonically increasing live turn id for this device.
    /// Any prior turn holding an older id will fail `st.live_turn == Some(id)`.
    fn claim_turn(&mut self) -> u64 {
        self.next_turn_seq = self.next_turn_seq.saturating_add(1);
        let id = self.next_turn_seq;
        self.live_turn = Some(id);
        id
    }

    fn invalidate_live_turn(&mut self) {
        self.next_turn_seq = self.next_turn_seq.saturating_add(1);
        self.live_turn = None;
    }

    fn release_turn(&mut self, turn_id: u64) {
        if self.live_turn == Some(turn_id) {
            self.live_turn = None;
        }
    }
}

/// M6b: the audible timeline of one turn. Times are absolute ms; the turn
/// loop subtracts its own start. `gen` distinguishes "this turn's audio" from
/// a previous turn's late receipt.
#[derive(Clone, Default)]
struct TurnAudio {
    gen: u64,
    /// The task this timeline belongs to. Set by the turn loop when the
    /// Task event names it (or by the first submit). A late receipt from the
    /// PREVIOUS turn is refused by task, not merely by arrival order.
    task: Option<String>,
    first_submit_ms: Option<u64>,
    last_submit_ms: Option<u64>,
    last_end_ms: Option<u64>,
    played_ms: u64,
    plays: u64,
}

impl TurnAudio {
    fn note_submit(&mut self, now: u64, task: Option<&str>) {
        if self.task.is_none() {
            self.task = task.map(str::to_string);
        }
        if self.first_submit_ms.is_none() {
            self.first_submit_ms = Some(now);
        }
        self.last_submit_ms = Some(now);
    }

    fn note_end(&mut self, now: u64, played_ms: u64, task: Option<&str>) {
        if let (Some(mine), Some(theirs)) = (self.task.as_deref(), task) {
            if mine != theirs {
                return; // a previous turn's play ending late
            }
        }
        self.last_end_ms = Some(now);
        self.played_ms = self.played_ms.saturating_add(played_ms);
        self.plays += 1;
    }
}

/// What the hub's play registry says is live on a device (plays the
/// pipeline did not submit itself: tool media, diagnostics probes, future
/// proactive speech — they all register in the hub only).
#[derive(Clone, Copy, Default)]
pub struct HubLive {
    pub voice: bool,
    pub media: bool,
}

impl DeviceVoiceState {
    /// A bookmark the PERSON left (暂停, a wake take-over) that has not
    /// expired. The LED state `paused` and that bookmark's lifetime are the
    /// same fact, so they are computed the same way ([docs/led-states.md] §2
    /// rule 1). A system cut is excluded: amber means "you put something on
    /// hold", and nobody did.
    fn program_paused(&self, now: u64) -> bool {
        self.paused_program
            .as_ref()
            .map(|p| !p.system_cut && now.saturating_sub(p.created_ms) < PROGRAM_TTL_MS)
            .unwrap_or(false)
    }

    /// The follow window just closed by timeout: a system-cut bookmark goes
    /// with it. Returns the unread byte count when one was dropped. A
    /// person's pause keeps its TTL — walking away from a story on hold and
    /// coming back to it is the case the bookmark exists for.
    fn drop_system_cut(&mut self) -> Option<usize> {
        let p = self.paused_program.as_ref()?;
        if !p.system_cut {
            return None;
        }
        let left = p.text.len().saturating_sub(p.cursor.min(p.text.len()));
        self.paused_program = None;
        Some(left)
    }

    /// Legacy budget helper used by the behavior tests. Production ordinary
    /// admission commits its debit with the request ticket in Participation.
    #[cfg(test)]
    fn spend_followup_credit(&mut self) -> Option<u8> {
        self.participation.spend_followup_credit()
    }

    fn voice_live(&self, hub_live: HubLive) -> bool {
        self.outputs
            .values()
            .any(|o| o.channel == "voice" && stage_is_live(&o.stage))
            || hub_live.voice
    }

    fn media_live(&self, hub_live: HubLive) -> bool {
        self.outputs
            .values()
            .any(|o| o.channel == "media" && stage_is_live(&o.stage))
            || hub_live.media
    }

    fn in_post_media_guard(&self, now: u64) -> bool {
        self.last_media_end_ms > 0
            && now.saturating_sub(self.last_media_end_ms)
                < crate::sovereignty::POST_MEDIA_FOLLOW_GUARD_MS
    }

    fn session_open(&self, now: u64) -> bool {
        self.session_open_with_live(now, HubLive::default())
    }

    fn session_open_with_live(&self, now: u64, hub_live: HubLive) -> bool {
        self.participation
            .session_open(now, self.voice_live(hub_live))
    }

    #[cfg(test)]
    fn session_covers(&self, start_ms: u64, end_ms: u64, now: u64) -> bool {
        self.session_covers_with_live(start_ms, end_ms, now, HubLive::default())
    }

    fn session_covers_with_live(
        &self,
        start_ms: u64,
        end_ms: u64,
        now: u64,
        hub_live: HubLive,
    ) -> bool {
        // #77: mic follow-up window is disabled during media playback + 2000 ms
        // post-media guard.
        if self.media_live(hub_live) || self.in_post_media_guard(now) {
            return false;
        }
        self.participation
            .session_covers(start_ms, end_ms, now, self.voice_live(hub_live))
    }

    /// A voice play ended. Contract v2 §2.3 (#4, 2026-11-16): the output's
    /// KIND decides, not a play-id exception. Only a real answer — the
    /// answer text produced for a request (`kind == "answer"`) — may move
    /// the follow window's reference; the wake ack, the close announcement,
    /// the retreat ack, a filler and the truncation prompt end as facts with
    /// no authority over the window.
    ///
    /// Why the kind and not "does a request exist": a play id can only ever
    /// name the one cue we remembered to exempt (`boundary_play_id`), so
    /// every cue added later silently regained the right to reopen the
    /// window. The class of the output is the thing that decides.
    fn note_voice_play_end(&mut self, play_id: &str, rec: &OutputRecord, now: u64) {
        if rec.kind != "answer" {
            return;
        }
        self.last_voice_end_ms = now;
        if let Some(request) = rec.request {
            self.participation.answer_ended(request, play_id, now);
        }
    }

    fn begin_window_request(
        &mut self,
        epoch: u64,
        expected: Identity,
        debit: bool,
    ) -> Option<RequestTicket> {
        if self.stop_epoch != epoch {
            return None;
        }
        self.participation
            .begin_request(expected, id::now_ms(), debit)
    }

    fn finish_window_request(&mut self, request: RequestTicket, now: u64) -> bool {
        self.participation.finish_request(request, now)
    }

    fn note_output_terminal(&mut self, play_id: &str, rec: &OutputRecord, now: u64) {
        if let Some(request) = rec.request {
            self.participation.answer_aborted(request, play_id, now);
        }
    }

    /// Both timeout callers perform the same conditional transition and
    /// bookmark cleanup while holding states. No caller closes after unlock.
    #[cfg(test)]
    fn timeout_session(&mut self, now: u64) -> bool {
        self.timeout_session_with_live(now, HubLive::default())
    }

    fn timeout_session_with_live(&mut self, now: u64, hub_live: HubLive) -> bool {
        // #88.2: active music/media playback NEVER triggers a Timeout session
        // close or farewell ("拜拜").
        if self
            .input
            .as_ref()
            .is_some_and(|i| i.admitted() && i.valid(self))
            || self.media_live(hub_live)
            || self.session_open_with_live(now, hub_live)
        {
            return false;
        }
        let expected = self.participation.identity();
        if !self
            .participation
            .close(expected, CloseReason::Timeout, now)
        {
            return false;
        }
        self.drop_system_cut();
        true
    }

    /// `hub_live`: channels with active plays in the hub registry (tool
    /// plays and diagnostics probes live there, not in this map).
    fn activity(&self, now: u64, hub_live: HubLive) -> &'static str {
        let in_conversation = self.session_open(now);
        let live =
            |o: &OutputRecord| !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping");
        if self
            .outputs
            .values()
            .any(|o| o.channel == "voice" && live(o))
            || hub_live.voice
        {
            "speaking"
        } else if self
            .outputs
            .values()
            .any(|o| o.channel == "media" && live(o))
            || hub_live.media
        {
            "playing"
        } else if self.current_task.is_some() {
            "thinking"
        } else if in_conversation {
            "listening"
        } else {
            "idle"
        }
    }

    /// The assistant presentation state (v0.2 assistant_state frame, G4):
    /// core-owned semantics, gateway maps to LEDs. One table for all three
    /// layers: [docs/led-states.md]. Precedence: hardware mute > error flash
    /// > speaking > music > thinking > wake flash > paused > listening > idle.
    /// A real turn always wins over the wake flash; the flash only decorates
    /// the wake→(listening|thinking) gap, and `paused` (a bookmark waiting for
    /// 继续) outranks `listening` because green would hide it.
    fn assistant_state(&self, now: u64, hub_live: HubLive, capturing: bool) -> &'static str {
        if self.muted == Some(true) {
            return "muted";
        }
        if self.error_flash_until_ms > now {
            return "error";
        }
        match self.activity(now, hub_live) {
            "speaking" => "speaking",
            "playing" => "music",
            "thinking" => "thinking",
            // The wake flash confirms the wake itself, then hands over.
            _ if self.wake_flash_until_ms > now => "wake",
            // A bookmark outranks the follow window: right after 「暂停」 the
            // window is open (green), and green would HIDE the one fact worth
            // showing — the story is on hold ([docs/led-states.md] §1). Only
            // the person's own pause counts; a system cut leaves the ring on
            // the follow window's green.
            _ if self.program_paused(now) => "paused",
            // LED B (user decision 2026-11-15): green carries two facts a
            // person can tell apart — a window that is merely OPEN (breathing
            // green: speak any time) and a MIC SEGMENT BEING CAPTURED right
            // now (solid green). One colour, two brightness behaviours.
            "listening" if capturing => "hearing",
            "listening" => "listening",
            _ => "idle",
        }
    }
}

/// Factory closures keep the pipeline decoupled from AppState: tests inject
/// FakeLlm/MockTts; production rebuilds from live config on every turn.
pub type EnvFactory = Arc<dyn Fn() -> Result<Arc<AgentEnv>, String> + Send + Sync>;
pub type TtsFactory =
    Arc<dyn Fn() -> Result<Arc<dyn crate::speech::TtsClient>, String> + Send + Sync>;
pub type AsrFactory =
    Arc<dyn Fn() -> Result<Arc<dyn crate::speech::AsrClient>, String> + Send + Sync>;

/// Wake-ack cache manifest: which phrases have a synthesized WAV, under
/// which TTS voice/model. Voice mismatch = stale (re-synthesizes on save).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct AckManifest {
    voice: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    files: HashMap<String, String>,
}

/// R2 (2026-11-13): what became of the wake ack. The wake word heard as
/// WORDS (the ASR transcript) is the confirmation the R1g matrix reserved the
/// ack for — and, until this round, the only path that played it was the
/// greeting-only branch, so 「你好高达，帮我播新闻」 woke silently.
///
/// The outcome is CLASSIFIED, never a bare `settled` bool: the old shape
/// reported success for a device that refused the play and for a wait that
/// timed out, which made "the ack was heard" unfalsifiable (Codex
/// interaction review, 2026-11-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// The device played it to the end (`play_finished`, or the terminal
    /// `device_confirmed` receipt on a device that does not send finish).
    Finished,
    /// The device refused the play (`failed`) or could not confirm it.
    Failed,
    /// The waiter was dropped: a stop, a disconnect, or a newer play.
    Dropped,
    /// No terminal fact within the bound — the audio may still be playing.
    Timeout,
    /// Nothing was played at all (disabled, no cached phrase, submit refused).
    NotPlayed,
}

impl AckOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
            Self::Timeout => "timeout",
            Self::NotPlayed => "not_played",
        }
    }

    /// Did the ack take the channel (so a cue on top of it would be a second
    /// bridge phrase before the same answer)? A timeout still counts: the
    /// play was submitted and may be sounding right now.
    pub fn covers_the_wait(self) -> bool {
        matches!(self, Self::Finished | Self::Timeout)
    }
}

/// The spoken budget in force for ONE delivery. `admit` decides how much of a
/// segment may still be spoken; the first refusal marks the cap exhausted, so
/// the hint is said once and no later token re-opens the voice.
///
/// It is enforced in the DELIVERY layer (streaming tokens, the whole-answer
/// fallback and program continuation all go through it) because a check at
/// the final Answer is too late: the earlier sentences are already sounding
/// (codex interaction review C, 2026-11-13).
#[derive(Debug, Clone, Copy)]
struct SpokenCap {
    max_sentences: usize,
    max_chars: usize,
    sentences: usize,
    chars: usize,
    exhausted: bool,
}

impl SpokenCap {
    fn new(budget: crate::agent::prompt::SpokenBudget) -> Self {
        Self {
            max_sentences: budget.max_sentences.max(1),
            max_chars: budget.max_chars.max(1),
            sentences: 0,
            chars: 0,
            exhausted: false,
        }
    }

    /// Everything of `seg` this delivery may still say. `None` = stop
    /// speaking; the text itself is already in the program, so nothing is
    /// lost — 「继续」 reads it.
    fn admit(&mut self, seg: &str) -> Option<String> {
        if self.exhausted {
            return None;
        }
        let n = seg.chars().count();
        if self.sentences < self.max_sentences && self.chars + n <= self.max_chars {
            self.sentences += 1;
            self.chars += n;
            return Some(seg.to_string());
        }
        if self.chars == 0 {
            // The FIRST thing to say is already over the ceiling (one huge
            // comma-spliced sentence). Say a safe prefix; saying nothing
            // would be a silent answer.
            let cut = safe_speech_prefix(seg, self.max_chars);
            self.sentences += 1;
            self.chars += cut.chars().count();
            self.exhausted = true;
            return Some(cut);
        }
        self.exhausted = true;
        None
    }
}

/// The longest prefix of `text` at or before `max` chars that ends on a
/// clause boundary when one is reasonably close — never inside a character,
/// and never at the cost of the whole sentence.
fn safe_speech_prefix(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let floor = max * 6 / 10;
    let mut cut = max;
    for (i, c) in chars.iter().enumerate().take(max) {
        if i >= floor
            && matches!(
                c,
                '。' | '！' | '？' | '；' | '，' | '、' | '.' | '!' | '?' | ';' | ',' | ' '
            )
        {
            cut = i + 1;
        }
    }
    chars[..cut].iter().collect::<String>().trim().to_string()
}

/// Apply the cap to a complete answer (the whole-answer TTS path): take whole
/// sentences while they fit, and stop at the first one that does not.
/// Returns `(what may be spoken, was_anything_left_out)`.
fn cap_text(cap: &mut SpokenCap, text: &str) -> (String, bool) {
    let mut buf = text.to_string();
    let mut out = String::new();
    while let Some(seg) = VoicePipeline::take_speak_segment(&mut buf) {
        match cap.admit(&seg) {
            Some(t) => out.push_str(&t),
            None => return (out, true),
        }
    }
    let rest = buf.trim().to_string();
    if !rest.is_empty() {
        match cap.admit(&rest) {
            Some(t) => out.push_str(&t),
            None => return (out, true),
        }
    }
    (out, false)
}

enum SessionCloseGuard {
    Identity(Identity),
    Management(Option<u64>),
    Model {
        flag: Arc<std::sync::atomic::AtomicBool>,
        confirm: bool,
        call_id: String,
    },
}

/// Rolling per-device acoustic level snapshot updated by `NearEndPresence`
/// and `Keyword` `rms_dbfs` (#71 / #74 item 2).
#[derive(Debug, Clone, Default)]
pub(crate) struct MicLevelSnapshot {
    pub rms_dbfs: Option<f32>,
    pub noise_floor_dbfs: Option<f32>,
    pub snr_db: Option<f32>,
    pub clipped: bool,
    pub echo_health: Option<String>,
    pub profile_version: Option<String>,
    pub at_ms: u64,
}

pub struct VoicePipeline {
    hub: Arc<DeviceHub>,
    store: Arc<Store>,
    tasks: Arc<TaskRegistry>,
    env_factory: EnvFactory,
    tts_factory: TtsFactory,
    asr_factory: AsrFactory,
    states: Mutex<HashMap<String, DeviceVoiceState>>,
    /// Per-device VAD/wake processors (v0.2 core-side mic path).
    mic: Mutex<HashMap<String, MicProcessor>>,
    /// ASR results re-enter the event pump through this channel (ASR runs
    /// as a spawned task so a slow service never lags device events).
    mic_tx: mpsc::Sender<MicTranscript>,
    mic_rx: Mutex<Option<mpsc::Receiver<MicTranscript>>>,
    settings: Mutex<VoiceSettings>,
    settings_edit_gate: Mutex<()>,
    last_cue_synth: Mutex<Option<serde_json::Value>>,
    /// Bounded diagnostic feed for the mode/devices pages.
    log: Mutex<Vec<serde_json::Value>>,
    /// One-shot test seam for the §0.3 item-2 race: fires between the
    /// stop-epoch check and the play submission (taken by the next speak).
    /// Production never sets it; see `set_before_play_submit_hook`.
    before_play_submit_hook: Mutex<Option<BeforePlaySubmitHook>>,
    /// Data dir (tone asset lives in `<data_dir>/assets`).
    data_dir: std::path::PathBuf,
    /// R1g/P7: this process's epoch. Every event carries it, and the mode
    /// snapshot exposes it next to the gateway's own epoch — two counters
    /// from two processes are only comparable inside one window.
    started_ms: u64,
    /// M3: the media runtime, for the "wake = take over" pause and the media
    /// half of the reflex words. Weak: the API owns both; neither owns the
    /// other's lifetime.
    music: Mutex<Option<Weak<crate::tool::music::MusicRuntime>>>,
    /// `LEARN-10`: learning runtime for bounded vocabulary wait-window sync.
    learning: Mutex<Option<crate::tool::learning::LearningRuntime>>,
    /// M5: per-play terminal waiters. `hub.play()`'s own waiter resolves on
    /// `device_confirmed` too (a DIAGNOSTIC correctly treats that as "started"),
    /// so a per-sentence caller that needs the play's END listens here: the
    /// event pump resolves these on `play_finished` / `stop_receipt`.
    terminal_waiters: Mutex<HashMap<String, tokio::sync::oneshot::Sender<SpokenTerminal>>>,
    /// M5: the turn's WAITING budget (first answer, whole turn) in ms.
    /// Playback time is never counted against it. Settable so a test — or a
    /// future console field — can shrink it without a rebuild.
    turn_wait_budget: Mutex<Option<(u64, u64)>>,
    /// Wake arbitration (2026-11-15): the one in-flight wake claim and the
    /// peer transport. Built from the same store the fleet uses, so the peer
    /// list and the sealed tokens are the household's own — never a second
    /// copy that could drift.
    #[cfg(test)]
    input_handoff_finished: tokio::sync::Notify,
    #[cfg(test)]
    input_handoff_hook: Mutex<
        Option<(
            &'static str,
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    pub arbiter: Arc<crate::arbitration::Arbiter>,
    /// Wave 2: per-room session sovereignty, owner-scoped retreat, and
    /// `FleetPlaybackNotice` cache.
    pub sovereignty: Arc<crate::sovereignty::RoomSessionDomain>,
    /// The last arbitration verdict of this node: (won, at_ms, winner).
    ///
    /// It exists for the OTHER wake path. The keyword spotter is not the only
    /// way the same phrase can open a session: with `asr_wake_fallback` on,
    /// the transcript of that utterance is matched again a second or so
    /// later — on every speaker that heard it. Without this memory the loser
    /// would stay silent on the spotter path and then wake up anyway from the
    /// transcript, which is the duplicate answer arbitration exists to stop.
    /// `(won, at_ms, winner, consumed)` — `consumed` makes the veto ONE-SHOT.
    ///
    /// A time window was the first shape and it was wrong on hardware: after a
    /// single loss every transcript for the next 5 s was refused, so the only
    /// remaining wake path was dead for a fixed stretch — the user's report
    /// was exactly that ("隔一段才能唤醒一次，固定的一段"), and it disappeared
    /// the moment arbitration was switched off. The transcript of the SAME
    /// utterance is the one thing to refuse; a later sentence must pass.
    last_arbitration: Mutex<Option<(bool, u64, String, bool)>>,
    /// Rolling per-device acoustic evidence (#71 / #74 item 2).
    mic_levels: Mutex<HashMap<String, MicLevelSnapshot>>,
    /// Contract v2 #2 (2026-11-16): the cue phrases THIS core played recently,
    /// with the device that said them — `(phrase, device, at_ms)`.
    ///
    /// One core serves the whole house, so "a NEIGHBOUR just said 来了" is a
    /// local fact; no fleet protocol is needed for it. Without this, devices
    /// standing 30 cm apart on a desk feed each other's cues back as user
    /// requests: dot2-d opened a turn and answered dot2-b's wake ack
    /// (live 2026-11-16 08:31:17, `mic_utterance "来了。"` → 1.58 s answer).
    foreign_cues: Mutex<std::collections::VecDeque<(String, String, u64)>>,
    /// The fleet plane, for telling peers about a cue we just spoke (#2).
    /// Weak: the API owns it, this pipeline does not.
    fleet: Mutex<Option<Weak<crate::fleet::FleetService>>>,
    /// M1b: the Silero endpoint model, loaded once and shared by every
    /// device's mic processor (the graph is read-only; only the LSTM state is
    /// per stream). `None` = the documented fallback to the energy VAD.
    silero: SileroSetup,
    /// Reminders in flight (2026-11-14): id → earliest next attempt. One
    /// entry per reminder being spoken, so the 1 Hz sweep neither double-fires
    /// while the line is sounding nor hammers a TTS that is down.
    reminder_inflight: Mutex<HashMap<i64, u64>>,
    /// Diagnostic mic capture (2026-11-13): the post-AEC mono the CORE
    /// receives is byte-for-byte what the device-side spotter consumes (the
    /// gateway's own comment says so), so recording it here gives the honest
    /// input for a KWS hit-rate measurement — no firmware flag, no extra mic.
    mic_capture: Mutex<Option<MicCapture>>,
    /// M6b (S3): the bounded on-disk journal. The in-memory ring above stays
    /// the console's live view (100 events); acceptance needs the whole
    /// window, so every note ALSO goes here. `None` = the directory could
    /// not be created (the voice path keeps running without it).
    events_file: Option<Arc<crate::voice_events::VoiceEventJournal>>,
    /// M7-1: session-end memory extraction spends two REAL model calls. It is
    /// on by default; an injected test environment turns it off so a test
    /// environment's scripted responses are not consumed by background work.
    memory_extract_enabled: std::sync::atomic::AtomicBool,
    /// Devices currently in Wi-Fi provisioning mode triggered by `mute_hold_wifi_setup`.
    wifi_provisioning: Mutex<std::collections::BTreeMap<String, u64>>,
    /// Pre-subscribed timed control receiver for the pump (captures queue age from VoicePipeline::new).
    pump_control_rx: Mutex<Option<broadcast::Receiver<crate::gateway::QueuedDeviceEvent>>>,
    /// Pre-subscribed timed audio receiver for the pump.
    pump_audio_rx: Mutex<Option<broadcast::Receiver<crate::gateway::QueuedDeviceEvent>>>,
    /// Phase 3 (#32): count of lagged `AudioChunk` frames dropped on the audio
    /// subscription (control events are on a separate channel and never drop
    /// due to audio backpressure).
    audio_dropped_total: std::sync::atomic::AtomicU64,
    /// Phase 3 (#32): count of lagged control events (should remain 0).
    control_lagged_total: std::sync::atomic::AtomicU64,
    /// Phase 5 (#44): high-water mark of control queue wait age in ms.
    max_control_queue_age_ms: std::sync::atomic::AtomicU64,
    /// Phase 5 (#44): high-water mark of audio queue wait age in ms.
    max_audio_queue_age_ms: std::sync::atomic::AtomicU64,
    /// Phase 5 (#44): high-water mark of event pump queue age in ms.
    max_pump_queue_age_ms: std::sync::atomic::AtomicU64,
}

/// M1b: how the Silero endpoint detector ended up (loaded / missing /
/// broken). The console shows it verbatim — "which detector is deciding" is
/// not something an operator should have to infer from audio behaviour.
pub struct SileroSetup {
    pub model: Option<Arc<crate::silero::SileroModel>>,
    pub path: String,
    pub detail: String,
}

impl SileroSetup {
    /// `<data_dir>/models/silero_vad.onnx`, or `HM_SILERO_VAD=<path>` /
    /// `off` to force the energy VAD. A missing file is NOT an error: it is
    /// the rollback path (model absent → energy detector, no behaviour
    /// change beyond M1b's).
    fn load(data_dir: &std::path::Path) -> Self {
        let override_ = std::env::var("HM_SILERO_VAD").ok();
        if override_.as_deref() == Some("off") {
            return Self {
                model: None,
                path: String::new(),
                detail: "disabled by HM_SILERO_VAD=off".into(),
            };
        }
        let path = override_.unwrap_or_else(|| {
            data_dir
                .join("models")
                .join("silero_vad.onnx")
                .to_string_lossy()
                .into_owned()
        });
        let p = std::path::Path::new(&path);
        if !p.exists() {
            return Self {
                model: None,
                path,
                detail: "model file absent: energy VAD in use".into(),
            };
        }
        let t0 = std::time::Instant::now();
        match crate::silero::SileroModel::from_path(p) {
            Ok(m) => Self {
                model: Some(Arc::new(m)),
                detail: format!("loaded in {} ms", t0.elapsed().as_millis()),
                path,
            },
            Err(e) => Self {
                model: None,
                path,
                detail: format!("load failed ({e}): energy VAD in use"),
            },
        }
    }
}

/// One in-flight diagnostic capture.
struct MicCapture {
    device: String,
    samples: Vec<i16>,
    rate: u32,
    target: usize,
    path: std::path::PathBuf,
    started_ms: u64,
    done: bool,
}

impl MicCapture {
    /// 16-bit mono WAV, written once at the end (≤30 s ≈ 960 KB).
    fn to_wav(samples: &[i16], rate: u32) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + samples.len() * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }
}

/// Live capture state for the console/diagnostics callers.
fn mic_capture_status_json(cap: Option<&MicCapture>, device: &str) -> serde_json::Value {
    match cap.filter(|c| c.device == device) {
        Some(c) => json!({
            "state": if c.done { "done" } else { "recording" },
            "device": c.device,
            "seconds": (c.samples.len() as f64 / c.rate as f64 * 10.0).round() / 10.0,
            "requested_seconds": (c.target as f64 / c.rate as f64 * 10.0).round() / 10.0,
            "rate": c.rate,
            "path": c.path.to_string_lossy(),
            "started_ms": c.started_ms,
        }),
        None => json!({ "state": "idle", "device": device }),
    }
}

/// Async hook (the race window must be awaitable so a stop can COMPLETE
/// inside it deterministically — a sync hook would deadlock single-threaded
/// test runtimes).
pub type BeforePlaySubmitHook =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

impl VoicePipeline {
    pub fn new(
        hub: Arc<DeviceHub>,
        store: Arc<Store>,
        tasks: Arc<TaskRegistry>,
        env_factory: EnvFactory,
        tts_factory: TtsFactory,
        asr_factory: AsrFactory,
        data_dir: std::path::PathBuf,
    ) -> Arc<Self> {
        let settings: VoiceSettings = store
            .config_get(VOICE_CFG_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        // M2.5: the default detail-word list gained the search phrases
        // (查一下/搜一下…). A stored list that still equals the OLD default
        // was never edited, so it is upgraded in place; a list with even one
        // household change is kept as-is (the console owns it).
        let mut settings = settings;
        if settings.spoken.detail_words == legacy_default_detail_words() {
            settings.spoken.detail_words = default_detail_words();
        }
        // Media trim lives on the hub (applied at play submit); seed it from
        // the persisted settings so a reboot keeps the saved value.
        hub.set_media_trim_db(settings.media_trim_db);
        let silero = SileroSetup::load(&data_dir);
        if silero.model.is_none() {
            tracing::info!(path = %silero.path, detail = %silero.detail,
                "silero endpoint detector unavailable — energy VAD applies");
        }
        let (mic_tx, mic_rx) = mpsc::channel(16);
        let pump_control_rx = hub.subscribe_pump_control();
        let pump_audio_rx = hub.subscribe_pump_audio();
        let started_ms = id::now_ms();
        let arbiter = crate::arbitration::Arbiter::new(store.clone());
        let sovereignty = Arc::new(crate::sovereignty::RoomSessionDomain::new());
        // M6b: the journal is started BEFORE the struct so the very first
        // events of this process are on disk with the same epoch.
        let events_file = crate::voice_events::VoiceEventJournal::start(&data_dir, started_ms);
        Arc::new(Self {
            hub,
            store,
            tasks,
            env_factory,
            tts_factory,
            asr_factory,
            states: Mutex::new(HashMap::new()),
            mic: Mutex::new(HashMap::new()),
            mic_tx,
            mic_rx: Mutex::new(Some(mic_rx)),
            pump_control_rx: Mutex::new(Some(pump_control_rx)),
            pump_audio_rx: Mutex::new(Some(pump_audio_rx)),
            settings: Mutex::new(settings),
            settings_edit_gate: Mutex::new(()),
            last_cue_synth: Mutex::new(None),
            log: Mutex::new(Vec::new()),
            before_play_submit_hook: Mutex::new(None),
            data_dir,
            started_ms,
            music: Mutex::new(None),
            learning: Mutex::new(None),
            terminal_waiters: Mutex::new(HashMap::new()),
            turn_wait_budget: Mutex::new(None),
            arbiter,
            #[cfg(test)]
            input_handoff_hook: Mutex::new(None),
            #[cfg(test)]
            input_handoff_finished: tokio::sync::Notify::new(),
            sovereignty,
            last_arbitration: Mutex::new(None),
            mic_levels: Mutex::new(HashMap::new()),
            foreign_cues: Mutex::new(std::collections::VecDeque::new()),
            fleet: Mutex::new(None),
            silero,
            reminder_inflight: Mutex::new(HashMap::new()),
            mic_capture: Mutex::new(None),
            wifi_provisioning: Mutex::new(std::collections::BTreeMap::new()),
            events_file,
            memory_extract_enabled: std::sync::atomic::AtomicBool::new(true),
            audio_dropped_total: std::sync::atomic::AtomicU64::new(0),
            control_lagged_total: std::sync::atomic::AtomicU64::new(0),
            max_control_queue_age_ms: std::sync::atomic::AtomicU64::new(0),
            max_audio_queue_age_ms: std::sync::atomic::AtomicU64::new(0),
            max_pump_queue_age_ms: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// M1b: one mic processor per device, with the Silero detector when the
    /// model is available. Every construction site goes through here so a
    /// device never accidentally runs a different detector than its peers.
    fn new_mic_processor(&self) -> MicProcessor {
        let p = MicProcessor::new(self.live_vad_config());
        match &self.silero.model {
            Some(m) => p.with_silero(crate::silero::SileroDetector::new(m.clone())),
            None => p,
        }
    }

    /// M7-1: turn the session-end memory extractor on/off. Production keeps
    /// the default (on); an injected test environment switches it off so a
    /// test's scripted model responses are not consumed by background work.
    pub fn set_memory_extraction(&self, enabled: bool) {
        self.memory_extract_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// M5: override the turn waiting budget (first answer, whole turn).
    pub fn set_turn_wait_budget(&self, first_ms: u64, total_ms: u64) {
        *crate::lock_or_recover(&self.turn_wait_budget, "voice.turn_wait_budget") =
            Some((first_ms, total_ms));
    }

    /// Install the one-shot race-window seam (tests only): the next speak()
    /// awaits this hook AFTER passing the stop-epoch check and BEFORE
    /// submitting the play — exactly the window a user stop can slip into.
    pub fn set_before_play_submit_hook(&self, hook: BeforePlaySubmitHook) {
        *crate::lock_or_recover(
            &self.before_play_submit_hook,
            "voice.before_play_submit_hook",
        ) = Some(hook);
    }

    fn note(&self, kind: &str, mut payload: serde_json::Value) {
        payload["kind"] = json!(kind);
        payload["ts_ms"] = json!(id::now_ms());
        // R1g/P7: every event names the process that wrote it. Two counters
        // from two processes are only comparable inside one epoch.
        payload["core_epoch"] = json!(self.started_ms);
        // M6b: the export cursor. `ts_ms` collides inside a burst; the
        // sequence never does (and restarts with the process, which the
        // epoch disambiguates). Assignment and enqueue happen together so
        // the file's order always matches the sequence.
        let payload =
            crate::voice_events::assign_seq_and_append(self.events_file.as_deref(), payload);
        let mut log = crate::lock_or_recover(&self.log, "voice.log");
        log.push(payload);
        let len = log.len();
        if len > 100 {
            log.drain(0..len - 100);
        }
    }

    /// M6b: the persistent journal's health (bounded by construction). The
    /// console shows the numbers, so "the disk is full" is never a guess.
    pub fn events_file_stats(&self) -> serde_json::Value {
        match &self.events_file {
            Some(j) => j.stats(),
            None => json!({ "available": false, "why": "journal directory could not be created" }),
        }
    }

    /// M6b: export the journal. See `VoiceEventJournal::export` for the
    /// cursor semantics (`since` exclusive; `(epoch, seq)` exact).
    pub fn export_events(
        &self,
        since_ts: u64,
        since_seq: Option<u64>,
        since_epoch: Option<u64>,
        limit: usize,
    ) -> serde_json::Value {
        self.export_events_with_tail(since_ts, since_seq, since_epoch, limit, false)
    }

    pub fn export_events_with_tail(
        &self,
        since_ts: u64,
        since_seq: Option<u64>,
        since_epoch: Option<u64>,
        limit: usize,
        tail: bool,
    ) -> serde_json::Value {
        match &self.events_file {
            Some(j) => j.export_with_tail(since_ts, since_seq, since_epoch, limit, tail),
            None => json!({
                "events": [], "count": 0, "truncated": false, "tail": tail,
                "available": false,
                "why": "journal directory could not be created",
            }),
        }
    }

    /// Test seam (M5): deliver a mic transcript with an explicit capture
    /// identity, so the ordering/epoch gate can be exercised without racing
    /// a real ASR service. Production builds it from the segment that closed.
    pub async fn test_deliver_transcript(
        self: &Arc<Self>,
        device: &str,
        text: &str,
        input_id: u64,
        capture_seq: u64,
        window_epoch: u64,
    ) {
        self.handle_mic_transcript(MicTranscript {
            leased: false,
            ticket: crate::mic::CaptureProvenance::legacy(0),
            device: device.to_string(),
            result: Ok(text.to_string()),
            duration_ms: 1_000,
            input_id,
            capture_seq,
            window_epoch,
            control_only: false,
            wake_sliced: false,
            enqueued_at: std::time::Instant::now(),
        })
        .await;
    }

    /// Immediately invalidate any in-flight `live_turn` token on `device` so
    /// an active turn fails its `live_turn` check even if no new turn starts
    /// (for example on an exit/retreat word or wake interruption).
    pub(super) fn clear_live_turn(&self, device: &str) {
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.invalidate_live_turn();
        }
    }

    /// List of devices currently in long-press mute Wi-Fi provisioning mode.
    pub fn wifi_provisioning_devices(&self) -> Vec<String> {
        crate::lock_or_recover(&self.wifi_provisioning, "voice.wifi_provisioning")
            .keys()
            .cloned()
            .collect()
    }

    /// Clear Wi-Fi provisioning status for `device`.
    pub fn clear_wifi_provisioning(&self, device: &str) {
        crate::lock_or_recover(&self.wifi_provisioning, "voice.wifi_provisioning").remove(device);
    }

    /// Public event note: other owners (music) write their facts into the
    /// same ring, so the console and the acceptance tools read one timeline.
    pub fn log_event(&self, kind: &str, payload: serde_json::Value) {
        self.note(kind, payload);
    }

    pub fn recent(&self, limit: usize) -> Vec<serde_json::Value> {
        crate::lock_or_recover(&self.log, "voice.log")
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Total number of lagged `AudioChunk` frames dropped on the audio channel.
    pub fn audio_dropped_total(&self) -> u64 {
        self.audio_dropped_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total number of lagged control events on the control channel.
    pub fn control_lagged_total(&self) -> u64 {
        self.control_lagged_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// High-water mark of control queue wait age in milliseconds.
    pub fn max_control_queue_age_ms(&self) -> u64 {
        self.max_control_queue_age_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// High-water mark of audio queue wait age in milliseconds.
    pub fn max_audio_queue_age_ms(&self) -> u64 {
        self.max_audio_queue_age_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// High-water mark of event pump queue age in milliseconds.
    pub fn max_pump_queue_age_ms(&self) -> u64 {
        self.max_pump_queue_age_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(super) fn record_control_queue_age(&self, age_ms: u64) {
        self.max_control_queue_age_ms
            .fetch_max(age_ms, std::sync::atomic::Ordering::Relaxed);
        self.max_pump_queue_age_ms
            .fetch_max(age_ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn record_audio_queue_age(&self, age_ms: u64) {
        self.max_audio_queue_age_ms
            .fetch_max(age_ms, std::sync::atomic::Ordering::Relaxed);
        self.max_pump_queue_age_ms
            .fetch_max(age_ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Snapshot of event-pump health and queue metrics for `/api/v0/status`.
    pub fn pump_metrics_json(&self) -> serde_json::Value {
        json!({
            "audio_dropped_total": self.audio_dropped_total(),
            "control_lagged_total": self.control_lagged_total(),
            "lock_poison_total": crate::lock_poison_total(),
            "max_control_queue_age_ms": self.max_control_queue_age_ms(),
            "max_audio_queue_age_ms": self.max_audio_queue_age_ms(),
            "max_pump_queue_age_ms": self.max_pump_queue_age_ms(),
        })
    }

    /// Alias for `pump_metrics_json()`.
    pub fn pump_metrics(&self) -> serde_json::Value {
        self.pump_metrics_json()
    }
}
