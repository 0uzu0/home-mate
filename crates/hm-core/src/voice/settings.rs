use crate::interaction::Content;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

pub(crate) const VOICE_CFG_KEY: &str = "voice_mode";
/// Core-side send pacing for streamed TTS (v0.3): keep at most this much
/// synthesized audio ahead of (estimated) playback.
pub(super) const TTS_AHEAD_LIMIT_S: f64 = 30.0;
/// A conversation mic follow-up window (`VoiceSession`) closes after this much
/// inactivity (8–10s range, view-level derivation; it never cancels anything by itself).
pub(super) const DEFAULT_FOLLOW_WINDOW_MS: u64 = crate::sovereignty::DEFAULT_VOICE_WINDOW_MS;
/// Semantic context retention (`ContextSession`) in ms (180–300s range).
pub(super) const DEFAULT_CONTEXT_WINDOW_MS: u64 = crate::sovereignty::DEFAULT_CONTEXT_WINDOW_MS;

/// What an event that opens the conversation window does to the follow-up
/// budget. The configured allowance is held in VoiceSettings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CreditGrant {
    /// Explicit activation (wake word, protocol wake/utterance): refill.
    Full,
    /// A stop: guarantee the next command, do not refill.
    One,
    /// An ordinary follow-up: neither.
    None,
}

/// A window-close cue waits at most this long for playback to drain before it
/// is dropped.
pub(super) const FOLLOW_CLOSE_WAIT_MS: u64 = 30_000;
/// How long the wake confirmation stays on the light (presentation only).
pub(super) const WAKE_FLASH_MS: u64 = 1_500;
/// How long an error indication stays on the light before falling back to
/// the underlying state (presentation only).
pub(super) const ERROR_FLASH_MS: u64 = 3_000;
/// Accepted-barge-in window (D/T5).
pub(super) const BARGE_IN_WINDOW_MS: u64 = 2_000;
/// R1 structural split: after an accepted barge-in, mic audio is discarded
/// until this long after the fact.
pub(super) const BARGE_UTTERANCE_GUARD_MS: u64 = 250;
/// R1e: how much of that guarded audio may be retained as the head of the
/// user's segment.
pub(super) const BARGE_HEAD_MAX_SAMPLES: usize = 16_000;
/// How long a retained head may wait for the segment it belongs to.
pub(super) const BARGE_HEAD_TTL_MS: u64 = 6_000;
/// How long a paused program waits for 继续 before it is forgotten.
pub(super) const PROGRAM_TTL_MS: u64 = 10 * 60 * 1000;

/// How long a proactive line (a reminder) waits after the person's last
/// finished sentence.
pub(super) const REMINDER_QUIET_AFTER_VOICE_MS: u64 = 2_500;
/// R1d: how long a device's own local stop stays authoritative for a barge-in fact.
pub(super) const LOCAL_STOP_TRUST_MS: u64 = 10_000;
/// How long after the device spoke a local cue its words may still appear at
/// the head of an ASR transcript (B-M1).
pub(super) const CUE_ECHO_WINDOW_MS: u64 = 6_000;
/// Contract v2 #2: how long another device's cue counts as "a neighbour just spoke".
pub(super) const FOREIGN_CUE_WINDOW_MS: u64 = 5_000;
/// Contract v2 #4: the spotter's tunable range.
pub(super) const KWS_THRESHOLD_RANGE: (f64, f64) = (0.05, 0.90);
pub(super) const KWS_SCORE_RANGE: (f64, f64) = (0.5, 6.0);
pub fn default_kws_threshold() -> f64 {
    0.25
}
pub fn default_kws_score() -> f64 {
    3.0
}
/// Hardware-family KWS sensitivity class so devices with distinct microphone
/// arrays never inherit another family's node-global tuning (`MG-P1-01`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKwsClass {
    /// Dot2 / standard 7-mic array (inherits node-global `kws_threshold` / `kws_score`).
    Standard,
    /// Android phone / tablet / 86-panel gateway (`0.22 / 3.00`).
    Android,
    /// Xiaomi AI Speaker Pro (`LX06`, lower PDM floor -> `0.20 / 2.50`).
    Lx06,
}

/// Mobile-calibrated KWS sensitivity defaults for Android mobile/panel gateways
/// (never inheriting Dot2 7-mic array global defaults).
pub fn default_android_kws_threshold() -> f64 {
    0.22
}
pub fn default_android_kws_score() -> f64 {
    3.0
}
/// Hardware-calibrated KWS sensitivity defaults for Xiaomi LX06 speakers
/// (measured on live LX06 PDM mic array: 5/5 detection at -38 dBFS, 0 FP on
/// 22.5 s Chinese negative speech at -30 dBFS; prevents Dot2's 0.32/2.50 from
/// blacking out far-field `"你好高达"` wake-up on LX06).
pub fn default_lx06_kws_threshold() -> f64 {
    0.20
}
pub fn default_lx06_kws_score() -> f64 {
    2.5
}
/// Bound on the house-cue pane.
pub(super) const HOUSE_CUE_PANE: usize = 16;

/// Normalise a cue/transcript for an exact comparison: punctuation and spaces
/// are what ASR adds and what the phrase string does not have.
pub(super) fn cue_key(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && !"。！？，、,.!?;；：:\"'「」".contains(*c))
        .collect()
}

/// Answer protection period (D/T5 R1 fix).
pub(super) const ANSWER_PROTECT_MS: u64 = 800;
/// A voice play submitted this soon after the previous voice play ENDED is
/// a continuation of the same answer (streamed pieces), not a new answer.
pub(super) const VOICE_CONTINUATION_GAP_MS: u64 = 400;
/// R1j/P0-a: how long the answer's first audio waits for the cancelled
/// waiting cue's flush receipt.
pub(super) const FILLER_STOP_WAIT_MS: u64 = 200;
/// How long the answer's first audio waits for a nearly-done waiting cue to
/// finish on its own.
pub(super) const FILLER_COMPLETE_WAIT_MS: u64 = 1_500;
/// Bound on waiting for the wake ack's terminal receipt before the command runs.
/// Sized for LAN/Tailscale gateways so a ~780 ms ack does not time out over RTT.
pub(super) const ACK_SETTLE_MS: u64 = 2_500;
/// R1j/P1: how long after a spotted intent the same intent arriving as an
/// ASR transcript is treated as the same speech.
pub(super) const REFLEX_DEDUPE_MS: u64 = 3_000;
/// Stall bound after `play_end` with no terminal receipt.
pub(super) const PLAY_STALL_MS: u64 = 10_000;

/// M5 turn budget, counted in TIME SPENT WAITING on the model/tools only.
pub(super) const TURN_WAIT_FIRST_MS: u64 = 15_000;
pub(super) const TURN_WAIT_TOTAL_MS: u64 = 45_000;
/// How often the budget is recomputed.
pub(super) const TURN_BUDGET_TICK_MS: u64 = 250;
/// How long a terminal-but-stale output record may stay in `/api/v0/mode`.
pub(super) const OUTPUT_STALE_MS: u64 = 5_000;

pub fn default_wake_words() -> Vec<String> {
    vec!["你好高达".into()]
}

/// Live VAD thresholds (G4 acoustic calibration): measured against the real
/// room, editable through the management API without a restart.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct VadSettings {
    pub start_db: f32,
    pub end_db: f32,
}

/// Per-device overrides for unit acoustic and room interaction policies (`MG-P1-01`).
/// Absent (`None`) fields inherit from the node-level `VoiceSettings` defaults.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeviceVoiceOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kws_threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kws_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_blind_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_protect_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword_interrupt: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive_speech: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_window_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_ms: Option<u64>,
    /// Outer `Option`: whether this device overrides `followup_credits`;
    /// inner `Option<u8>`: `None` (unlimited) or `Some(n)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup_credits: Option<Option<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_window_tone: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_words: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_ack: Option<WakeAckSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retreat_intent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retreat_words: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_wake_fallback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive_asr_wake: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_arbitration: Option<WakeArbitration>,
}

impl DeviceVoiceOverrides {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VoiceSettings {
    #[serde(default)]
    pub assistant_name: String,
    #[serde(default)]
    pub persona_prompt: String,
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    #[serde(default)]
    pub follow_window_enabled: bool,
    #[serde(default = "default_follow_window_ms")]
    pub follow_window_ms: u64,
    #[serde(default = "default_context_window_ms")]
    pub context_window_ms: u64,
    /// Optional explicit follow-up cap (`None` = unlimited within the window;
    /// `Some(0)` = single-request mode).
    #[serde(default)]
    pub followup_credits: Option<u8>,
    #[serde(default)]
    pub proactive_speech: bool,
    /// device → requested duplex ("half"|"full"). Absent = half.
    #[serde(default)]
    pub duplex_requested: HashMap<String, String>,
    /// Wake words for core-side KWS (v0.2).
    #[serde(default = "default_wake_words")]
    pub wake_words: Vec<String>,
    /// Contract v2 #4 (2026-11-16): the device spotter's sensitivity.
    #[serde(default = "default_kws_threshold")]
    pub kws_threshold: f64,
    #[serde(default = "default_kws_score")]
    pub kws_score: f64,
    /// Measured VAD thresholds (G4); absent = env/first-guess defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vad: Option<VadSettings>,
    /// Per-device measured VAD thresholds (multi-gateway isolation). Absent = fall back to `vad`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub vad_by_device: HashMap<String, VadSettings>,
    #[serde(default)]
    pub follow_window_tone: bool,
    #[serde(default)]
    pub wake_ack: WakeAckSettings,
    #[serde(default = "default_true")]
    pub retreat_intent: bool,
    #[serde(default = "default_retreat_words")]
    pub retreat_words: Vec<String>,
    #[serde(default = "default_retreat_ack")]
    pub retreat_ack: String,
    #[serde(default = "default_window_close_ack")]
    pub window_close_ack: String,
    #[serde(default = "default_truncation_notice")]
    pub truncation_notice: String,
    #[serde(default)]
    pub stop_intents: StopIntentSettings,
    #[serde(default = "default_media_trim_db")]
    pub media_trim_db: f64,
    /// Per-device media-channel trim overrides in dB (multi-gateway isolation).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub media_trim_db_by_device: HashMap<String, f64>,
    #[serde(default = "default_true")]
    pub keyword_interrupt: bool,
    #[serde(default = "default_true")]
    pub barge_in: bool,
    #[serde(default = "default_barge_protect_ms")]
    pub barge_protect_ms: u64,
    #[serde(default = "default_tail_blind_ms")]
    pub tail_blind_ms: u64,
    #[serde(default)]
    pub filler: FillerSettings,
    #[serde(default)]
    pub spoken: SpokenDeliverySettings,
    #[serde(default = "default_true")]
    pub asr_wake_fallback: bool,
    #[serde(default = "default_true")]
    pub proactive_asr_wake: bool,
    #[serde(default)]
    pub wake_arbitration: WakeArbitration,
    /// Per-device acoustic & room policy overrides (`MG-P1-01`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub device_overrides: BTreeMap<String, DeviceVoiceOverrides>,
}

/// See [`VoiceSettings::wake_arbitration`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeArbitration {
    /// Single-node behaviour: no claims leave this device.
    #[default]
    Off,
    /// Claims are exchanged and the decision recorded; behaviour unchanged.
    Observe,
    /// The loser of a claim stays silent.
    Enforce,
}

impl WakeArbitration {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "observe" => Some(Self::Observe),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

/// The spoken-delivery policy (R2 interaction layer, 2026-11-13).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpokenDeliverySettings {
    /// Sentences per spoken answer (1–8).
    #[serde(default = "default_spoken_max_sentences")]
    pub max_sentences: usize,
    /// Characters per spoken answer (40–600).
    #[serde(default = "default_spoken_max_chars")]
    pub max_chars: usize,
    #[serde(default = "default_detail_max_sentences")]
    pub detail_max_sentences: usize,
    #[serde(default = "default_detail_max_chars")]
    pub detail_max_chars: usize,
    #[serde(default = "default_detail_words")]
    pub detail_words: Vec<String>,
    #[serde(default = "default_long_form_words")]
    pub long_form_words: Vec<String>,
    #[serde(default = "default_continue_hint")]
    pub continue_hint: String,
    #[serde(default = "default_resume_lead")]
    pub resume_lead: String,
    #[serde(default = "default_backstop_max_sentences")]
    pub backstop_max_sentences: usize,
    #[serde(default = "default_backstop_max_chars")]
    pub backstop_max_chars: usize,
}

/// Partial update for `SpokenDeliverySettings` used by `PUT /api/v0/mode`:
/// omitting `detail_words`, `long_form_words`, or any other field preserves
/// the currently stored value instead of overwriting it with serde defaults.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SpokenDeliveryPatch {
    #[serde(default)]
    pub max_sentences: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub detail_max_sentences: Option<usize>,
    #[serde(default)]
    pub detail_max_chars: Option<usize>,
    #[serde(default)]
    pub detail_words: Option<Vec<String>>,
    #[serde(default)]
    pub long_form_words: Option<Vec<String>>,
    #[serde(default)]
    pub continue_hint: Option<String>,
    #[serde(default)]
    pub resume_lead: Option<String>,
    #[serde(default)]
    pub backstop_max_sentences: Option<usize>,
    #[serde(default)]
    pub backstop_max_chars: Option<usize>,
}

impl SpokenDeliveryPatch {
    pub fn apply_to(&self, base: &SpokenDeliverySettings) -> SpokenDeliverySettings {
        let mut next = base.clone();
        if let Some(v) = self.max_sentences {
            next.max_sentences = v;
        }
        if let Some(v) = self.max_chars {
            next.max_chars = v;
        }
        if let Some(v) = self.detail_max_sentences {
            next.detail_max_sentences = v;
        }
        if let Some(v) = self.detail_max_chars {
            next.detail_max_chars = v;
        }
        if let Some(v) = &self.detail_words {
            next.detail_words = v.clone();
        }
        if let Some(v) = &self.long_form_words {
            next.long_form_words = v.clone();
        }
        if let Some(v) = &self.continue_hint {
            next.continue_hint = v.clone();
        }
        if let Some(v) = &self.resume_lead {
            next.resume_lead = v.clone();
        }
        if let Some(v) = self.backstop_max_sentences {
            next.backstop_max_sentences = v;
        }
        if let Some(v) = self.backstop_max_chars {
            next.backstop_max_chars = v;
        }
        next
    }
}

pub fn default_spoken_max_sentences() -> usize {
    2
}
pub fn default_spoken_max_chars() -> usize {
    120
}
pub fn default_detail_max_sentences() -> usize {
    6
}
pub fn default_detail_max_chars() -> usize {
    300
}
pub fn default_backstop_max_sentences() -> usize {
    400
}
pub fn default_backstop_max_chars() -> usize {
    2000
}

pub fn default_detail_words() -> Vec<String> {
    [
        "讲讲",
        "说说",
        "说一下",
        "聊聊",
        "聊",
        "介绍",
        "解释",
        "为什么",
        "怎么回事",
        "分析",
        "对比",
        "最近",
        "详细",
        "讲解",
        "展开",
        "播",
        "查一下",
        "查查",
        "搜一下",
        "搜搜",
        "搜索",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn legacy_default_detail_words() -> Vec<String> {
    [
        "讲讲",
        "说说",
        "说一下",
        "聊聊",
        "聊",
        "介绍",
        "解释",
        "为什么",
        "怎么回事",
        "分析",
        "对比",
        "最近",
        "详细",
        "讲解",
        "展开",
        "播",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn default_long_form_words() -> Vec<String> {
    [
        "讲个",
        "讲一个",
        "讲一遍",
        "念",
        "念完",
        "读一遍",
        "读一下",
        "播报",
        "播一下",
        "全部",
        "完整",
        "从头",
        "都说",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub(crate) fn preferred_continue_word(words: &[String]) -> Option<&str> {
    words
        .iter()
        .find(|word| word.as_str() == "继续")
        .or_else(|| words.first())
        .map(String::as_str)
}

impl VoiceSettings {
    /// Resolve the effective settings for `device` by overlaying any per-device
    /// overrides (`device_overrides`, `vad_by_device`, `media_trim_db_by_device`)
    /// onto node-level defaults (`MG-P1-01`).
    pub fn effective_for(&self, device: &str) -> Self {
        let mut eff = self.clone();
        if let Some(vad) = self.vad_by_device.get(device).copied() {
            eff.vad = Some(vad);
        }
        if let Some(db) = self.media_trim_db_by_device.get(device).copied() {
            eff.media_trim_db = db;
        }
        if let Some(ov) = self.device_overrides.get(device) {
            if let Some(v) = ov.kws_threshold {
                eff.kws_threshold = v;
            }
            if let Some(v) = ov.kws_score {
                eff.kws_score = v;
            }
            if let Some(v) = ov.tail_blind_ms {
                eff.tail_blind_ms = v;
            }
            if let Some(v) = ov.barge_in {
                eff.barge_in = v;
            }
            if let Some(v) = ov.barge_protect_ms {
                eff.barge_protect_ms = v;
            }
            if let Some(v) = ov.keyword_interrupt {
                eff.keyword_interrupt = v;
            }
            if let Some(v) = ov.proactive_speech {
                eff.proactive_speech = v;
            }
            if let Some(v) = ov.follow_window_enabled {
                eff.follow_window_enabled = v;
            }
            if let Some(v) = ov.follow_window_ms {
                eff.follow_window_ms = v;
            }
            if let Some(v) = ov.context_window_ms {
                eff.context_window_ms = v;
            }
            if let Some(v) = ov.followup_credits {
                eff.followup_credits = v;
            }
            if let Some(v) = ov.follow_window_tone {
                eff.follow_window_tone = v;
            }
            if let Some(words) = &ov.wake_words {
                eff.wake_words = words.clone();
            }
            if let Some(ack) = &ov.wake_ack {
                eff.wake_ack = ack.clone();
            }
            if let Some(v) = ov.retreat_intent {
                eff.retreat_intent = v;
            }
            if let Some(words) = &ov.retreat_words {
                eff.retreat_words = words.clone();
            }
            if let Some(v) = ov.asr_wake_fallback {
                eff.asr_wake_fallback = v;
            }
            if let Some(v) = ov.proactive_asr_wake {
                eff.proactive_asr_wake = v;
            }
            if let Some(v) = ov.wake_arbitration {
                eff.wake_arbitration = v;
            }
        }
        eff
    }

    /// Resolve effective `(kws_threshold, kws_score)` for `device`.
    ///
    /// Explicit per-device overrides always win. When no per-device override is
    /// set, Android gateways use mobile-calibrated defaults (`0.22 / 3.00`) and
    /// LX06 speakers use LX06-calibrated defaults (`0.20 / 2.50`) instead of
    /// copying Dot2's node-global values.
    pub fn effective_kws_for(&self, device: &str, class: DeviceKwsClass) -> (f64, f64) {
        let ov = self.device_overrides.get(device);
        let (fallback_threshold, fallback_score) = match class {
            DeviceKwsClass::Android => {
                (default_android_kws_threshold(), default_android_kws_score())
            }
            DeviceKwsClass::Lx06 => (default_lx06_kws_threshold(), default_lx06_kws_score()),
            DeviceKwsClass::Standard => (self.kws_threshold, self.kws_score),
        };
        let threshold = ov
            .and_then(|o| o.kws_threshold)
            .unwrap_or(fallback_threshold);
        let score = ov.and_then(|o| o.kws_score).unwrap_or(fallback_score);
        (threshold, score)
    }

    pub(crate) fn continue_command(&self) -> Option<&str> {
        self.stop_intents
            .enabled
            .then(|| preferred_continue_word(&self.stop_intents.continue_words))
            .flatten()
    }

    pub(crate) fn effective_continue_hint(&self) -> String {
        let Some(word) = self.continue_command() else {
            return String::new();
        };
        if self.spoken.continue_hint == default_continue_hint() {
            format!("剩下的还留着，说{word}我接着讲。")
        } else {
            self.spoken.continue_hint.clone()
        }
    }
}

pub fn default_continue_hint() -> String {
    "剩下的还留着，说继续我接着讲。".into()
}

pub fn default_resume_lead() -> String {
    "接着刚才的话题。".into()
}

impl Default for SpokenDeliverySettings {
    fn default() -> Self {
        Self {
            max_sentences: default_spoken_max_sentences(),
            max_chars: default_spoken_max_chars(),
            detail_max_sentences: default_detail_max_sentences(),
            detail_max_chars: default_detail_max_chars(),
            detail_words: default_detail_words(),
            long_form_words: default_long_form_words(),
            continue_hint: default_continue_hint(),
            resume_lead: default_resume_lead(),
            backstop_max_sentences: default_backstop_max_sentences(),
            backstop_max_chars: default_backstop_max_chars(),
        }
    }
}

impl SpokenDeliverySettings {
    pub(super) fn normalized_for_console(mut self) -> Result<Self, String> {
        for words in [&mut self.detail_words, &mut self.long_form_words] {
            for word in words.iter_mut() {
                *word = word.trim().to_owned();
            }
            words.retain(|word| !word.is_empty());
        }
        self.continue_hint = self.continue_hint.trim().to_owned();
        self.resume_lead = self.resume_lead.trim().to_owned();
        self.validate()?;
        Ok(self)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        self.validate_delivery()?;
        self.validate_legacy_words()
    }

    fn validate_legacy_words(&self) -> Result<(), String> {
        if self.detail_words.len() > 24 {
            return Err("detail_words 最多 24 个".into());
        }
        for w in &self.detail_words {
            let n = w.trim().chars().count();
            if n == 0 || n > 8 || w.contains('"') || w.contains('/') {
                return Err(format!("解释类词 '{w}'：1–8 字，不含引号或斜杠"));
            }
        }
        if self.long_form_words.len() > 16 {
            return Err("long_form_words 最多 16 个".into());
        }
        for w in &self.long_form_words {
            let n = w.trim().chars().count();
            if n == 0 || n > 8 || w.contains('"') || w.contains('/') {
                return Err(format!("长内容词 '{w}'：1–8 字，不含引号或斜杠"));
            }
        }
        Ok(())
    }

    pub(super) fn validate_delivery_values(&self) -> Result<(), String> {
        if !(1..=8).contains(&self.max_sentences) {
            return Err("max_sentences 必须在 1–8".into());
        }
        if !(40..=600).contains(&self.max_chars) {
            return Err("max_chars 必须在 40–600".into());
        }
        if !(1..=8).contains(&self.detail_max_sentences) {
            return Err("detail_max_sentences 必须在 1–8".into());
        }
        if !(40..=600).contains(&self.detail_max_chars) {
            return Err("detail_max_chars 必须在 40–600".into());
        }
        let hint = self.continue_hint.trim();
        if hint.chars().count() > 40 || hint.contains('"') || hint.contains('/') {
            return Err("continue_hint 最多 40 字，不含引号或斜杠".into());
        }
        let lead = self.resume_lead.trim();
        if lead.chars().count() > 40 || lead.contains('"') || lead.contains('/') {
            return Err("resume_lead 最多 40 字，不含引号或斜杠".into());
        }
        if !(2..=1000).contains(&self.backstop_max_sentences) {
            return Err("backstop_max_sentences 必须在 2–1000".into());
        }
        if !(200..=6000).contains(&self.backstop_max_chars) {
            return Err("backstop_max_chars 必须在 200–6000".into());
        }
        Ok(())
    }

    pub(super) fn validate_delivery(&self) -> Result<(), String> {
        self.validate_delivery_values()?;
        if self.backstop_max_sentences <= self.detail_max_sentences.max(self.max_sentences)
            || self.backstop_max_chars <= self.detail_max_chars.max(self.max_chars)
        {
            return Err("兜底预算必须宽于普通档和解释档".into());
        }
        Ok(())
    }

    pub(super) fn budget(&self) -> crate::agent::prompt::SpokenBudget {
        crate::agent::prompt::SpokenBudget {
            max_sentences: self.max_sentences,
            max_chars: self.max_chars,
        }
    }

    pub(super) fn detail_budget(&self) -> crate::agent::prompt::SpokenBudget {
        crate::agent::prompt::SpokenBudget {
            max_sentences: self.detail_max_sentences,
            max_chars: self.detail_max_chars,
        }
    }

    /// The runaway guard: the only budget the runtime still enforces.
    pub(super) fn backstop_budget(&self) -> crate::agent::prompt::SpokenBudget {
        crate::agent::prompt::SpokenBudget {
            max_sentences: self.backstop_max_sentences,
            max_chars: self.backstop_max_chars,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillerMode {
    /// 电台风：音乐起播不取消垫话；回答语音永远独占 voice 通道。
    Radio,
    /// 垫话只覆盖静音：任何其他播放（含音乐）起播即取消。
    Interrupt,
}

impl Default for FillerMode {
    fn default() -> Self {
        Self::Radio
    }
}

/// The cue's START policy (R1g/P4 + R1j/P0-a), one place.
pub(super) fn cue_may_start(mode: FillerMode, content: Content) -> bool {
    match mode {
        FillerMode::Radio => !content.voice,
        FillerMode::Interrupt => !content.any(),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FillerSettings {
    #[serde(default = "default_filler_delay_ms")]
    pub delay_ms: u64,
    #[serde(default = "default_filler_tool_delay_ms")]
    pub tool_delay_ms: u64,
    #[serde(default)]
    pub mode: FillerMode,
    #[serde(default = "default_filler_thinking")]
    pub thinking: Vec<String>,
    #[serde(default = "default_filler_search")]
    pub search: Vec<String>,
    #[serde(default = "default_filler_device")]
    pub device: Vec<String>,
    #[serde(default = "default_filler_story")]
    pub story: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resynth: Option<bool>,
}

pub fn default_filler_delay_ms() -> u64 {
    300
}

pub fn default_filler_tool_delay_ms() -> u64 {
    600
}

pub fn default_filler_thinking() -> Vec<String> {
    vec!["我想想。".into()]
}

pub fn default_filler_search() -> Vec<String> {
    vec!["我查一下。".into()]
}

pub fn default_filler_device() -> Vec<String> {
    vec!["我看一下。".into()]
}

pub fn default_filler_story() -> Vec<String> {
    vec!["我想想怎么讲。".into()]
}

/// The scene of a waiting cue, from the transcript alone.
pub fn filler_scene(text: &str) -> &'static str {
    const STORY: &[&str] = &["故事", "童话", "story"];
    const DEVICE: &[&str] = &[
        "灯", "窗帘", "设备", "音量", "空调", "light", "device", "volume",
    ];
    const SEARCH: &[&str] = &[
        "查", "搜", "天气", "新闻", "搜索", "播放", "weather", "search", "news",
    ];
    if STORY.iter().any(|x| text.contains(x)) {
        "story"
    } else if DEVICE.iter().any(|x| text.contains(x)) {
        "device"
    } else if SEARCH.iter().any(|x| text.contains(x)) {
        "search"
    } else {
        "thinking"
    }
}

impl Default for FillerSettings {
    fn default() -> Self {
        Self {
            delay_ms: default_filler_delay_ms(),
            tool_delay_ms: default_filler_tool_delay_ms(),
            mode: FillerMode::default(),
            thinking: default_filler_thinking(),
            search: default_filler_search(),
            device: default_filler_device(),
            story: default_filler_story(),
            resynth: None,
        }
    }
}

impl FillerSettings {
    pub fn scene_phrases(&self, scene: &str) -> &[String] {
        match scene {
            "story" => &self.story,
            "device" => &self.device,
            "search" => &self.search,
            _ => &self.thinking,
        }
    }

    pub fn pick(&self, text: &str, seq: u64) -> Option<(&'static str, String)> {
        let scene = filler_scene(text);
        let list = self.scene_phrases(scene);
        if list.is_empty() {
            return None;
        }
        Some((scene, list[(seq as usize) % list.len()].clone()))
    }

    pub fn all_phrases(&self) -> Vec<String> {
        let mut all = Vec::new();
        for list in [&self.thinking, &self.search, &self.device, &self.story] {
            all.extend(list.iter().cloned());
        }
        all
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if !(200..=10_000).contains(&self.delay_ms) {
            return Err("filler delay_ms must be 200–10000".into());
        }
        if self.tool_delay_ms > 5_000 {
            return Err("filler tool_delay_ms must be 0–5000".into());
        }
        for (scene, list) in [
            ("thinking", &self.thinking),
            ("search", &self.search),
            ("device", &self.device),
            ("story", &self.story),
        ] {
            if list.len() > 4 {
                return Err(format!("filler {scene}: at most 4 phrases"));
            }
            for (i, ph) in list.iter().enumerate() {
                let n = ph.trim().chars().count();
                if n == 0 || n > 40 || ph.contains('"') || ph.contains('/') {
                    return Err(format!(
                        "filler {scene} phrase {i}: 1–40 chars, no quotes or slashes"
                    ));
                }
                if list[..i].contains(ph) {
                    return Err(format!("filler {scene}: duplicate phrase '{ph}'"));
                }
            }
        }
        Ok(())
    }
}

pub fn default_barge_protect_ms() -> u64 {
    ANSWER_PROTECT_MS
}

pub fn default_tail_blind_ms() -> u64 {
    400
}

pub fn default_media_trim_db() -> f64 {
    -6.0
}

pub fn default_retreat_words() -> Vec<String> {
    [
        "退下",
        "退一下",
        "退吧",
        "休息吧",
        "休息了",
        "没事了",
        "不用了",
        "结束吧",
        "到这吧",
        "停止播放",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Short local ack phrases; WAVs are pre-synthesized into `<data_dir>/assets/wake-acks/`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WakeAckSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_ack_phrases")]
    pub phrases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resynth: Option<bool>,
}

/// Partial update for `wake_ack` on `PUT /api/v0/mode/{dev}`. Omitting
/// `phrases` preserves existing custom phrases atomically under
/// `settings_edit_gate`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct WakeAckPatch {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub phrases: Option<Vec<String>>,
    #[serde(default)]
    pub resynth: Option<bool>,
}

impl From<WakeAckSettings> for WakeAckPatch {
    fn from(s: WakeAckSettings) -> Self {
        Self {
            enabled: Some(s.enabled),
            phrases: Some(s.phrases),
            resynth: s.resynth,
        }
    }
}

impl Default for WakeAckSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            phrases: default_ack_phrases(),
            resynth: None,
        }
    }
}

pub fn default_ack_phrases() -> Vec<String> {
    ["在呢", "我在", "听着呢", "来了", "你说"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Stop / keep-going intents (B-M1).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StopIntentSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_stop_words")]
    pub stop_words: Vec<String>,
    #[serde(default = "default_continue_words")]
    pub continue_words: Vec<String>,
}

impl Default for StopIntentSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            stop_words: default_stop_words(),
            continue_words: default_continue_words(),
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            assistant_name: String::new(),
            persona_prompt: String::new(),
            system_prompt_override: None,
            follow_window_enabled: false,
            follow_window_ms: default_follow_window_ms(),
            context_window_ms: default_context_window_ms(),
            followup_credits: None,
            proactive_speech: false,
            duplex_requested: HashMap::new(),
            wake_words: default_wake_words(),
            vad: None,
            vad_by_device: HashMap::new(),
            follow_window_tone: false,
            wake_ack: WakeAckSettings::default(),
            retreat_intent: true,
            retreat_words: default_retreat_words(),
            retreat_ack: default_retreat_ack(),
            window_close_ack: default_window_close_ack(),
            truncation_notice: default_truncation_notice(),
            stop_intents: StopIntentSettings::default(),
            tail_blind_ms: default_tail_blind_ms(),
            media_trim_db: default_media_trim_db(),
            media_trim_db_by_device: HashMap::new(),
            keyword_interrupt: true,
            barge_in: true,
            barge_protect_ms: default_barge_protect_ms(),
            kws_threshold: default_kws_threshold(),
            kws_score: default_kws_score(),
            filler: FillerSettings::default(),
            spoken: SpokenDeliverySettings::default(),
            asr_wake_fallback: true,
            proactive_asr_wake: true,
            wake_arbitration: WakeArbitration::Off,
            device_overrides: BTreeMap::new(),
        }
    }
}

fn default_follow_window_ms() -> u64 {
    DEFAULT_FOLLOW_WINDOW_MS
}
fn default_context_window_ms() -> u64 {
    DEFAULT_CONTEXT_WINDOW_MS
}
pub(super) fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Validated partial update shared by the console and the mode writer.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct FollowWindowPatch {
    pub follow_window_enabled: Option<bool>,
    pub follow_window_ms: Option<u64>,
    pub context_window_ms: Option<u64>,
    pub followup_credits: Option<u8>,
    pub assistant_name: Option<String>,
    pub persona_prompt: Option<String>,
    #[serde(default, deserialize_with = "deserialize_override_patch")]
    pub system_prompt_override: Option<Option<String>>,
    pub kws_threshold: Option<f64>,
    pub kws_score: Option<f64>,
    #[serde(skip)]
    pub spoken: Option<SpokenDeliverySettings>,
    #[serde(skip)]
    pub spoken_patch: Option<SpokenDeliveryPatch>,
    #[serde(skip)]
    pub idle_asr_enabled: Option<bool>,
    #[serde(skip)]
    pub wake_arbitration: Option<WakeArbitration>,
}

pub(crate) fn deserialize_override_patch<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(<Option<String> as serde::Deserialize>::deserialize(
        d,
    )?))
}

fn validate_prompt(field: &str, text: &str) -> Result<(), String> {
    if text.contains('\0') {
        return Err(format!("{field} cannot contain NUL"));
    }
    if text.chars().count() > 4000 {
        return Err(format!("{field} must be at most 4000 characters"));
    }
    Ok(())
}

impl FollowWindowPatch {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(v) = self.kws_threshold {
            if !v.is_finite() || v < KWS_THRESHOLD_RANGE.0 || v > KWS_THRESHOLD_RANGE.1 {
                return Err(format!(
                    "kws_threshold must be within {}..={} (lower = more sensitive)",
                    KWS_THRESHOLD_RANGE.0, KWS_THRESHOLD_RANGE.1
                ));
            }
        }
        if let Some(v) = self.kws_score {
            if !v.is_finite() || v < KWS_SCORE_RANGE.0 || v > KWS_SCORE_RANGE.1 {
                return Err(format!(
                    "kws_score must be within {}..={} (higher = more sensitive)",
                    KWS_SCORE_RANGE.0, KWS_SCORE_RANGE.1
                ));
            }
        }
        if let Some(name) = &self.assistant_name {
            if name.contains('\0') {
                return Err("assistant_name cannot contain NUL".into());
            }
            if name.trim().chars().count() > 64 {
                return Err("assistant_name must be at most 64 characters".into());
            }
        }
        if let Some(text) = &self.persona_prompt {
            validate_prompt("persona_prompt", text)?;
        }
        if let Some(Some(text)) = &self.system_prompt_override {
            validate_prompt("system_prompt_override", text)?;
        }
        if self
            .follow_window_ms
            .is_some_and(|ms| !(5_000..=60_000).contains(&ms))
        {
            return Err("follow_window_ms must be 5000–60000".into());
        }
        if self
            .context_window_ms
            .is_some_and(|ms| !(60_000..=600_000).contains(&ms))
        {
            return Err("context_window_ms must be 60000–600000".into());
        }
        if self.followup_credits.is_some_and(|n| !(0..=5).contains(&n)) {
            return Err("followup_credits must be 0–5".into());
        }
        Ok(())
    }
}

/// Read-only projection of the global policy into each active device.
#[derive(Debug, Clone, Copy)]
pub(super) struct FollowWindowPolicy {
    pub enabled: bool,
    pub ms: u64,
    pub credits: Option<u8>,
}

impl Default for FollowWindowPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            ms: DEFAULT_FOLLOW_WINDOW_MS,
            credits: None,
        }
    }
}

impl FollowWindowPolicy {
    pub(super) fn for_device(s: &VoiceSettings, device: &str) -> Self {
        let eff = s.effective_for(device);
        Self::from(&eff)
    }
}

impl From<&VoiceSettings> for FollowWindowPolicy {
    fn from(s: &VoiceSettings) -> Self {
        Self {
            enabled: s.follow_window_enabled,
            ms: s.follow_window_ms,
            credits: s.followup_credits,
        }
    }
}

pub const KWS_PHRASE: &str = "你好高达";

/// Strip punctuation/whitespace — intent matching works on bare text.
pub fn normalize_utterance(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Greeting-only residue after the wake word.
pub fn is_greeting_only(rest: &str) -> bool {
    const GREETINGS: &[&str] = &["你好", "您好", "嗨", "哈喽", "喂", "hi", "hello", "hey"];
    let n = normalize_utterance(rest).to_lowercase();
    n.is_empty() || GREETINGS.contains(&n.as_str())
}

/// Session-end intent ("退下" family).
pub fn is_retreat(text: &str) -> bool {
    retreat_match(text, &default_retreat_words())
}

/// The configurable core of [`is_retreat`].
pub fn retreat_match(text: &str, words: &[String]) -> bool {
    let n = normalize_utterance(text);
    if words.iter().any(|word| {
        let word = normalize_utterance(word);
        !word.is_empty() && n.trim_end_matches("谢谢") == word
    }) {
        return true;
    }
    if n.chars().count() > 8
        || [
            "不要", "别", "不许", "不能", "不用", "不是", "什么", "意思", "叫", "说", "词",
        ]
        .iter()
        .any(|w| n.contains(w))
    {
        return false;
    }
    words
        .iter()
        .any(|word| !normalize_utterance(word).is_empty() && n.contains(word.as_str()))
}

pub(super) fn dominant_match(text: &str, words: &[String], max_len: usize) -> bool {
    const TAIL: usize = 4;
    let n = normalize_utterance(text);
    let len = n.chars().count();
    if len == 0 || len > max_len {
        return false;
    }
    words.iter().any(|k| {
        let k = normalize_utterance(k);
        if k.is_empty() || !n.contains(&k) {
            return false;
        }
        let rest = n.replacen(&k, "", 1).chars().count();
        rest <= TAIL
    })
}

pub(super) fn last_sentence(text: &str) -> &str {
    let trimmed = text.trim();
    let mut start = 0usize;
    for (i, ch) in trimmed.char_indices() {
        if matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n') {
            let after = i + ch.len_utf8();
            if !trimmed[after..].trim().is_empty() {
                start = after;
            }
        }
    }
    trimmed[start..].trim()
}

pub(super) fn merge_detail(base: Value, detail: Value) -> Value {
    let mut out = base;
    if let Some(obj) = out.as_object_mut() {
        if let Some(d) = detail.as_object() {
            for (k, v) in d {
                obj.insert(k.clone(), v.clone());
            }
        }
        obj.insert("detail".into(), detail);
    }
    out
}

pub(super) fn pause_match(text: &str) -> bool {
    let t = text.trim().trim_end_matches(['。', '.', '！', '!']).trim();
    matches!(
        t,
        "暂停" | "暂停播放" | "暂停一下" | "先暂停" | "pause" | "pause playback"
    )
}

pub(super) fn continue_match(text: &str, words: &[String]) -> bool {
    let n = normalize_utterance(text);
    // A negated continuation ("不要你继续放", "别继续", "不用接着讲") must go
    // to the model instead of being hijacked as a resume command, while
    // positive keep-going phrases ("不要停", "别停", "不用停") stay valid.
    for root in ["继续", "接着"] {
        if let Some((prefix, _)) = n.split_once(root) {
            if ["不要", "别", "不用", "不许", "不能", "不想"]
                .iter()
                .any(|neg| prefix.contains(neg))
            {
                return false;
            }
        }
    }
    dominant_match(text, words, 10)
}

pub fn stop_remainder(text: &str, words: &[String]) -> Option<String> {
    const TAIL: usize = 4;
    let n = normalize_utterance(text);
    let len = n.chars().count();
    if len == 0 || len > 8 {
        return None;
    }
    let mut best: Option<String> = None;
    for k in words {
        let kk = normalize_utterance(k);
        if kk.is_empty() || !n.contains(&kk) {
            continue;
        }
        if best
            .as_ref()
            .map(|b| b.chars().count() < kk.chars().count())
            .unwrap_or(true)
        {
            best = Some(kk);
        }
    }
    let k = best?;
    if len - k.chars().count() > TAIL {
        return None;
    }
    Some(n.replacen(&k, "", 1))
}

#[cfg(test)]
pub fn stop_match(text: &str, words: &[String]) -> bool {
    stop_remainder(text, words).is_some()
}

pub fn remainder_is_content(rest: &str) -> bool {
    const PARTICLES: &[&str] = &["一下", "吧", "啊", "呀", "嘛", "哦", "呗", "了", "的", "下"];
    let mut s = rest.to_string();
    for p in PARTICLES {
        s = s.replace(p, "");
    }
    s.chars().count() >= 2
}

pub fn default_stop_words() -> Vec<String> {
    [
        "停",
        "停下",
        "停止",
        "别说了",
        "不要说了",
        "先别说了",
        "别播了",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn default_continue_words() -> Vec<String> {
    [
        "不要停",
        "别停",
        "不用停",
        "继续",
        "接着说",
        "继续说",
        "接着讲",
        "继续讲",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn strip_leading_phrase(text: &str, phrase: &str) -> Option<String> {
    let want: Vec<char> = normalize_utterance(phrase).to_lowercase().chars().collect();
    if want.is_empty() {
        return None;
    }
    let mut seen = 0usize;
    let mut cut = None;
    for (i, ch) in text.char_indices() {
        if !ch.is_alphanumeric() {
            continue;
        }
        let lc = ch.to_lowercase().next().unwrap_or(ch);
        if lc != want[seen] {
            return None;
        }
        seen += 1;
        if seen == want.len() {
            cut = Some(i + ch.len_utf8());
            break;
        }
    }
    let cut = cut?;
    let rest: String = text[cut..]
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_string();
    Some(rest)
}

pub const RETREAT_ACK_PHRASE: &str = "好嘞";

pub fn default_retreat_ack() -> String {
    RETREAT_ACK_PHRASE.into()
}

pub(super) fn default_window_close_ack() -> String {
    "拜拜啦".into()
}

pub(crate) fn validate_short_ack(field: &str, ack: &str) -> Result<(), String> {
    let n = ack.trim().chars().count();
    if n == 0 || n > 8 || ack.contains('"') || ack.contains('/') {
        return Err(format!("{field}：1–8 字，不含引号或斜杠"));
    }
    Ok(())
}

pub const TRUNCATION_NOTICE_PHRASE: &str = "这段内容当时没有生成完，后面就没有了。";

pub fn default_truncation_notice() -> String {
    TRUNCATION_NOTICE_PHRASE.into()
}
