use serde::{Deserialize, Serialize};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONFIG_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigBlock {
    Kws,
    AudioEq,
    Led,
    Duplex,
    Acoustics,
}

impl ConfigBlock {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kws => "kws",
            Self::AudioEq => "audio_eq",
            Self::Led => "led",
            Self::Duplex => "duplex",
            Self::Acoustics => "acoustics",
        }
    }

    pub const fn capability_name(self) -> &'static str {
        match self {
            Self::Kws => "config_block_kws",
            Self::AudioEq => "config_block_audio_eq",
            Self::Led => "config_block_led",
            Self::Duplex => "config_block_duplex",
            Self::Acoustics => "config_block_acoustics",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReceiptStatus {
    Validated,
    Persisted,
    Active,
    Failed,
    Unsupported,
    Superseded,
    /// The gateway reports a never-applied block as `"idle"` in `ConfigState`
    /// (its block state is a free string). One unknown block state must not
    /// make Core drop the whole frame, so any value Core does not model lands
    /// here. It is never terminal and never counts as applied.
    #[serde(other)]
    Unknown,
}

fn validated_status() -> ConfigReceiptStatus {
    ConfigReceiptStatus::Validated
}

impl ConfigReceiptStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Failed | Self::Unsupported | Self::Superseded
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigBlockState {
    pub block: ConfigBlock,
    #[serde(default)]
    pub block_generation: u64,
    #[serde(default)]
    pub operation_id: String,
    #[serde(default)]
    pub data_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_values: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    pub state: ConfigReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// --- protocol mirror (authoritative doc: docs/contracts-v0.md) ---------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloDevice {
    pub stable_id: String,
    pub model: String,
    #[serde(default)]
    pub firmware: Option<String>,
    pub capabilities: Vec<String>,
    pub audio_profile: String,
    /// v0.2.3: associated SSID when the device reports wifi capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi_ssid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioMeta {
    pub format: String,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DuplexState {
    pub capability: String,
    pub requested: String,
    pub negotiated: String,
    pub effective: String,
}

/// A dBFS value worth recording. The device's "no onset slice" sentinel is
/// `f32::MIN`; an older gateway sends it verbatim (M6b), and `-3.4e38` in a
/// diagnostic feed reads like a measurement. Below -200 dBFS nothing is
/// measured — report it as missing (`null`).
pub fn valid_db(v: f32) -> Option<f32> {
    (v.is_finite() && v >= -200.0).then_some(v)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AecState {
    pub owner: String,
    pub reference: String,
}

impl Default for AecState {
    fn default() -> Self {
        // "not reported" — the core maps an empty owner to unknown instead of
        // guessing (M6b: the gateway used to hardcode "core" here).
        Self {
            owner: String::new(),
            reference: String::new(),
        }
    }
}

/// Source-issued identity. Never inferred from send time, seq, or hub state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputOrigin {
    /// Older sender: capture identity is unproven, even on a fresh connection.
    #[default]
    Legacy,
    Audio {
        capture_epoch: String,
        ticket: String,
        input_id: String,
        /// Half-open mono sample-frame coordinates, before asynchronous KWS.
        start_sample: u64,
        end_sample: u64,
    },
    Text {
        input_id: String,
    },
    /// Frozen at capture/text creation, before asynchronous recognition.
    Leased {
        lease: String,
        origin: Box<InputOrigin>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlap: Option<PlaybackOverlap>,
    },
}

/// A gateway-measured overlap on the SAME capture sample clock. This is
/// not send time, accumulated fed_ms, or the core's submit timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackOverlap {
    pub play_id: String,
    pub capture_epoch: String,
    pub ticket: String,
    pub start_sample: u64,
    pub end_sample: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransportIdentity {
    pub generation: u64,
    pub inbound_seq: u64,
}

/// Hub-stamped envelope. `None` transport is reserved for local legacy/test
/// inputs; it must never be filled by consulting the current connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputProvenance {
    pub origin: InputOrigin,
    pub transport: Option<TransportIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayFrame {
    Hello {
        protocol: u32,
        /// v1 mechanism (#8/#11, 2026-11-16): the LOWEST peer protocol
        /// version this gateway accepts. Compatibility is INTERVAL
        /// intersection (see `protocol_operating_version`), so `[0,1]` vs
        /// `[0,0]` inter-operates at version 0. Absent (an older gateway) =
        /// exactly 0, which is what the strict-equality era meant.
        #[serde(default)]
        min_protocol: u32,
        #[serde(default)]
        supported_versions: Vec<u32>,
        gateway: String,
        /// v1 mechanism: the gateway/firmware PACKAGE version ("0.0.53"),
        /// machine-readable. The human-readable name stays in `gateway`.
        /// Empty = an older gateway that does not say.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        gateway_version: String,
        devices: Vec<HelloDevice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aec_tail_ms: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_version: Option<String>,
        /// #271: Optional household Bearer token presented by non-loopback LAN
        /// gateways (e.g. Android phone / smart display) when connecting to a
        /// paired Core. Loopback gateways remain zero-config trusted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_token: Option<String>,
    },
    Wake {
        device: String,
        #[serde(default)]
        origin: InputOrigin,
        ts: u64,
        seq: u64,
    },
    Utterance {
        device: String,
        #[serde(default)]
        origin: InputOrigin,
        ts: u64,
        seq: u64,
        source: String,
        text: String,
    },
    Button {
        device: String,
        ts: u64,
        seq: u64,
        action: String,
    },
    MuteState {
        device: String,
        ts: u64,
        seq: u64,
        muted: bool,
        source: String,
        /// v1 mechanism (#8/#13): echo of the `MuteSet.task_id` that caused
        /// this state, so a management command can resolve against THE
        /// receipt that answered it (a physical-key press carries no
        /// task_id and must never settle a remote command's waiter).
        /// None = a device fact (key press, boot report) or an older firmware.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    PlayReceipt {
        task_id: String,
        stage: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        ts: u64,
    },
    StopReceipt {
        task_id: String,
        stage: String,
        /// R0 (2026-11-13): the firmware now confirms a stop AFTER the
        /// hardware flush and states how much audio the DAC actually took
        /// (48 kHz mono samples, ms). Additive and optional — an older
        /// device omits it. Recorded in the event ring/log as evidence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        played_ms: Option<u64>,
        /// M6a: how much of `played_ms` the hardware is estimated to have
        /// CONSUMED (played_ms minus the DAC backlog at the drop). Absent =
        /// not observable; `progress_basis` then says "written".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumed_ms: Option<u64>,
        /// "hw_delay" | "written" — how the number above was obtained.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_basis: Option<String>,
        /// M6a: why the stop could not be confirmed (`flush_failed: …`).
        /// With a detail, `stage` is "unknown", never "device_confirmed".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    VolumeReceipt {
        task_id: String,
        effective: f64,
        unit: String,
        stage: String,
        /// R2 maintenance (2026-11-13): the device that produced this fact.
        /// Optional so an older firmware (which sends no device) still works:
        /// the receipt is then attributed through the pending map exactly as
        /// before. With it, two devices' physical volume keys each update
        /// their own snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
    },
    ModeReport {
        device: String,
        duplex: DuplexState,
        /// M6b: optional on the wire. An older gateway that does not report
        /// AEC ownership yields the default (empty) value, which the core
        /// shows as unknown — it must never silently mean "core".
        #[serde(default)]
        aec: AecState,
    },
    PlayFinished {
        device: String,
        task_id: String,
        played_ms: u64,
        /// M6a: how much of `played_ms` the DAC is estimated to have
        /// CONSUMED when the stream drained (played_ms minus the hardware
        /// backlog). Absent = the device cannot report the backlog; the
        /// caller must then treat `played_ms` as "written", not "heard".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumed_ms: Option<u64>,
        /// "hw_delay" | "written".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_basis: Option<String>,
        ts: u64,
    },
    /// v0.2: one block of uplink PCM (s16le). `layout` is "mono" or
    /// "mic_ref" (2ch interleaved [mic|ref], AEC reference in ch2 — the
    /// core owns AEC, per device-bridge-boundary §4). Small frames at a
    /// steady cadence (dot2-d: 80 ms blocks); no per-chunk ack.
    AudioChunk {
        device: String,
        #[serde(default)]
        origin: InputOrigin,
        ts: u64,
        seq: u64,
        pcm_b64: String,
        format: String, // "s16le"
        sample_rate: u32,
        channels: u16,
        layout: String,
        /// Optional gateway/sidecar neural VAD decision for this PCM block
        /// (e.g. sherpa-onnx Silero VAD on `hm-kws`). Backward-compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vad_speech: Option<bool>,
        /// Optional continuous speech probability `[0.0, 1.0]`; used as
        /// `>= 0.5` when `vad_speech` is omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vad_prob: Option<f32>,
    },
    /// v0.2: the gateway reports the mic session state it achieved (answer
    /// to mic_session, and honest transitions when the device drops).
    MicReport {
        device: String,
        active: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample_rate: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channels: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// v0.2 (mapping gap G6): per-device online/offline WITHIN a live
    /// gateway connection — a southbound device dropping must not tear the
    /// gateway↔core link down. `descriptor` is present when the frame
    /// introduces a device the core has not seen in this connection.
    DeviceStatus {
        device: String,
        online: bool,
        reason: String, // registered|control_lost|data_lost|mic_failed|…
        #[serde(default, skip_serializing_if = "Option::is_none")]
        descriptor: Option<HelloDevice>,
        /// Additive diagnostic schema; retain unknown versions for unavailable
        /// readback without turning a compatible wire0 frame into an error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        northbound: Option<serde_json::Value>,
    },
    /// v0.2.3 (stage C, replaced-capability #1-2): watchdog outcome of a
    /// WifiConfig change — "applied" (new ssid joined) or "reverted"
    /// (backup restored after the join watchdog fired).
    WifiState {
        device: String,
        ts: u64,
        ssid: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// v0.2.4 (D/T4): the device's local keyword spotter recognized a fixed
    /// phrase. A fact from the reflex layer; the pipeline decides whether a
    /// live voice playback stops (that rule is tested here, not on-device).
    Keyword {
        device: String,
        #[serde(default)]
        origin: InputOrigin,
        ts: u64,
        seq: u64,
        keyword: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fed_ms: Option<u64>,
        /// v0.2.7 (R1g): the device's echo guard suspects the microphone
        /// heard our own speakers. A HINT — the core decides, because only it
        /// knows whether our live answer text contains the phrase.
        #[serde(default)]
        self_heard: bool,
        /// v0.2.9 (wake arbitration): the peak post-AEC level over the
        /// phrase, and how stale the audio was when the model fired. The
        /// spatial evidence a claim is made of; both optional so an older
        /// firmware speaks the same protocol.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rms_dbfs: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lag_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bundle_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kws_generation: Option<u64>,
        /// XiaoAi-style L1+L2 acoustic tier ("real" | "suspect"). Optional so
        /// older gateways (0.0.53 / LX06) preserve legacy ASR confirmation (#261).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kws_tier: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake_dur_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_gap_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        l2_passed: Option<bool>,
    },
    /// v0.2.5 (D/T5): the device heard genuine near-end speech while its
    /// voice channel was playing (onset + echo cross-correlation guard),
    /// with the pre-roll from before the detection so the first syllable
    /// survives. A fact; the stop decision and the semantic handling of
    /// the words live in the pipeline (voice.rs::on_barge_in).
    BargeIn {
        device: String,
        ts: u64,
        seq: u64,
        /// Onset pre-roll: last ≤400 ms of the post-AEC mono stream, S16LE
        /// @16k mono.
        pcm_b64: String,
        sample_rate: u32,
        channels: u16,
        /// The device guard's own value (below its threshold — that is
        /// why this fired). Recorded for calibration evidence.
        corr: f32,
        /// Onset slice level (dBFS RMS).
        energy_db: f32,
        /// R1 self-hearing: the DAC reference level in the same block.
        /// `residual_db = energy_db - ref_db` (near-end EXCESS): a certified
        /// fact sits within the device's 15 dB margin (`residual_db > -15`),
        /// echo-only blocks sit far below it, and a person louder than the
        /// playback is positive. (Firmware reported suppression here until
        /// 2026-11-12, which made a loud person look like echo; the sign now
        /// matches this gate.)
        /// Old firmware omits these; the defaults (0/0/false) make the fact
        /// uncertified, so a new core refuses it instead of self-answering.
        #[serde(default)]
        ref_db: f32,
        #[serde(default)]
        residual_db: f32,
        /// Every 20 ms reference frame in the detection block was verified
        /// against the output ring. False = the fact is not trustworthy.
        #[serde(default)]
        ref_locked: bool,
    },
    /// R1d (2026-11-12 night): the device already stopped the voice channel
    /// on its own near-end reflex. This is a hardware FACT, not a request —
    /// the core still owns "does this count as a turn" through the following
    /// BargeIn fact, but the audio is gone before this frame arrives (the
    /// 3.078 s fact latency measured that night). `played_ms` is what the DAC
    /// actually took before the flush (the same truth StopReceipt carries);
    /// `latency_ms` is the device's own fire→flush time, so a slow north link
    /// cannot hide a fast reflex.
    StoppedLocally {
        device: String,
        task_id: String,
        reason: String,
        played_ms: u64,
        latency_ms: u64,
        /// Read-ahead left in the channel when the reflex cut it (proof that
        /// synthesis pacing holds on this answer; diagnostic only).
        #[serde(default)]
        ahead_ms: u64,
        ts: u64,
    },
    /// v0.2.4 (D/T4): spotter lifecycle ("starting"|"running"|"down"), so
    /// the console can state honestly whether keyword interruption exists.
    KwsReport {
        device: String,
        state: String,
        detail: String,
        #[serde(default)]
        restarts: u64,
        #[serde(default)]
        detections: u64,
        /// v0.2.5 (D/T5): hits the device's echo guard suppressed as
        /// self-heard (playback live + mic-vs-reference correlation high).
        #[serde(default)]
        guarded: u64,
        /// v0.2.6 (R1g/P7): the gateway process epoch. Its lifetime counters
        /// survive core restarts, so a number is only comparable inside one
        /// epoch; 0 = an older gateway that does not report one.
        #[serde(default)]
        epoch: u64,
        /// M6b (v0.13): the FNV-1a of the keyword file the SPOTTER loaded
        /// (empty until its first `ready`, and cleared when the process
        /// exits). The deploy receipt only proves what was INSTALLED.
        #[serde(default)]
        loaded_hash: String,
        #[serde(default)]
        loaded_ms: u64,
        /// Contract v2 §2.5 (#5, 2026-11-16): retreat words armed for a LOCAL
        /// exit right now. A DEVICE fact that survives core restarts, unlike
        /// this core's own deploy record — 0 = nothing can be cut locally.
        #[serde(default)]
        local_exit: u32,
        /// Contract v2 #4: the sensitivity the loaded model REPORTED
        /// (0.0 = an older gateway/driver that does not say).
        #[serde(default)]
        threshold: f32,
        #[serde(default)]
        score: f32,
    },
    /// R1j/P1: outcome of [`CoreFrame::KwsDeploy`]. `hash` is the FNV-1a of
    /// the file the device actually has (empty when rejected before writing).
    KwsDeployReceipt {
        device: String,
        accepted: bool,
        hash: String,
        keywords: u64,
        detail: String,
        /// Contract v2 §2.5 (#5, 2026-11-16): how many retreat words the
        /// device armed for LOCAL exit. 0 = nothing can be cut locally (an
        /// older gateway, or a table without a retreat word); the field is
        /// optional so an older device still parses.
        #[serde(default)]
        local_exit: u32,
        /// Echo of the word→intent map this device stored (empty = older
        /// gateway that only knows the vocabulary).
        #[serde(default)]
        intents_hash: String,
        /// Contract v2 #4: the sensitivity the spotter will run with (echo of
        /// what was installed; 0 = an older gateway that does not tune).
        #[serde(default)]
        threshold: f32,
        #[serde(default)]
        score: f32,
    },
    /// Contract v2 §2.5 (#5): the gateway recognised a retreat word and cut
    /// its own audio BEFORE the core could answer. A FACT, not a decision:
    /// the core's `close_session` stays the only authority over the session,
    /// and the `Keyword` frame that rides with this one is what closes it.
    LocalExitFlushed {
        device: String,
        keyword: String,
        ts: u64,
        /// Device-side hit → silence, measured at the hardware flush (a slow
        /// north link can never inflate it).
        latency_ms: u64,
        /// Channels the device cleared, in its own words ("voice", "music").
        #[serde(default)]
        channels: Vec<String>,
        /// Set when a channel's flush failed — the audio may still be live.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// v1 mechanism (#8/#11, 2026-11-16): the gateway/firmware refused the
    /// link — a version interval with no intersection, or a frame whose
    /// capability the device does not declare (privileged-frame gate).
    /// Sent BEFORE the gateway closes the connection so the core records the
    /// reason instead of guessing. Decode reality (keep honest): a runtime
    /// frame an OLD core cannot decode is logged and skipped (never fatal
    /// there); a HELLO-stage frame from an old core IS fatal. Old peers are
    /// never promised to understand this frame — EOF degradation accepted.
    ProtocolError {
        protocol: u32,
        #[serde(default)]
        min_protocol: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        gateway_version: String,
        reason: String,
    },
    /// W1 (#72/#73): gateway AEC echo-health telemetry.
    EchoHealth {
        device: String,
        #[serde(default)]
        health: String,
        #[serde(default)]
        degraded_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// W1 (#71/#73/#74): rolling near-end acoustic evidence emitted by gateway.
    NearEndPresence {
        device: String,
        #[serde(default)]
        ts_ms: u64,
        #[serde(default)]
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rms_dbfs: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        noise_floor_dbfs: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snr_db: Option<f32>,
        #[serde(default)]
        clipped: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        echo_health: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_version: Option<String>,
    },
    PlayRejected {
        task_id: String,
        device: String,
        reason: String,
        ts: u64,
    },
    ConfigState {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        device: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability_fingerprint: Option<String>,
        #[serde(default)]
        blocks: Vec<ConfigBlockState>,
        #[serde(default)]
        ts: u64,
    },
    ConfigValidated {
        request_id: String,
        device: String,
        block: ConfigBlock,
        #[serde(default)]
        schema_version: u32,
        #[serde(default)]
        block_hash: String,
        #[serde(default)]
        ok: bool,
        /// Gateway wire: `supported` + `blockers` (it never sends `ok`/`state`).
        #[serde(default)]
        supported: bool,
        #[serde(default)]
        blockers: Vec<String>,
        #[serde(default = "validated_status")]
        state: ConfigReceiptStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default)]
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability_fingerprint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<serde_json::Value>,
        #[serde(default)]
        ts: u64,
    },
    ConfigReceipt {
        device: String,
        #[serde(default)]
        core_node_id: String,
        #[serde(default)]
        data_epoch: u64,
        #[serde(default)]
        application_id: String,
        operation_id: String,
        #[serde(default)]
        desired_revision: u64,
        #[serde(default)]
        block_generation: u64,
        block: ConfigBlock,
        #[serde(default)]
        schema_version: u32,
        #[serde(default)]
        block_hash: String,
        /// Gateway wire names this `status` and the failure code `error_code`.
        #[serde(alias = "status")]
        state: ConfigReceiptStatus,
        #[serde(default, skip_serializing_if = "Option::is_none", alias = "error_code")]
        reason: Option<String>,
        #[serde(default)]
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persisted_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_values: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<serde_json::Value>,
        #[serde(default)]
        ts: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreFrame {
    HelloAck {
        protocol: u32,
        /// v1 mechanism: lowest peer version this core accepts (see Hello).
        #[serde(default)]
        min_protocol: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        supported_versions: Vec<u32>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
        /// v1 mechanism: machine-readable core version (CARGO_PKG_VERSION).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        core_version: String,
        generation: u64,
    },
    /// v1 mechanism (#8/#11): the core refused the gateway's hello — no
    /// version intersection, a capability gap, or duplicate device ids. Sent
    /// over the handshake socket BEFORE the connection closes so the gateway
    /// can paint its protocol-error lamp with a reason; best-effort (an old
    /// gateway that cannot decode it just sees the disconnect).
    HelloReject {
        protocol: u32,
        #[serde(default)]
        min_protocol: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        core_version: String,
        reason: String,
    },
    Play {
        task_id: String,
        device: String,
        content_ref: String,
        audio: AudioMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio_b64: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        simulated_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        barge_in: Option<String>,
    },
    Stop {
        task_id: String,
        flush: bool,
    },
    VolumeSet {
        task_id: String,
        device: String,
        value: f64,
        unit: String,
    },
    ModeSet {
        device: String,
        duplex_requested: String,
    },
    /// v0.2: start/stop a device's mic session ("continuous" streams
    /// audio_chunk blocks; gated/locked rounds are a later revision). The
    /// gateway answers with mic_report carrying the achieved truth.
    MicSession {
        device: String,
        action: String, // "start" | "stop"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    /// v0.2 (G4, mapping gap G8): the assistant's interaction state per
    /// device, core-owned semantics (the core alone knows wake/participation).
    /// The gateway maps this to device presentation (dot2-d: LED ring) —
    /// palette and wire format are gateway-owned, this frame is semantic
    /// only: idle|listening|wake|thinking|speaking|music|muted|error.
    AssistantState {
        device: String,
        state: String,
    },
    /// v0.2 (G6, replaced-capability #2): project the host clock to the
    /// device right after hello (emOS has no time service; its clock sits
    /// in 2010 and C-stage TLS depends on sane time). Fire-and-forget
    /// projection — the device sets what it can and logs the outcome.
    TimeSet {
        ts_ms: u64,
    },
    /// v0.2.3 (stage C, replaced-capability #1-2): online WiFi change. The
    /// device firmware backs up the active wpa config, applies the new
    /// network and runs a join watchdog (auto-restore on timeout), then
    /// reports WifiState.
    WifiConfig {
        ssid: String,
        psk: String,
    },
    /// v0.3 (D/T3): streamed playback — begin, incremental raw-PCM chunks,
    /// end. Only for devices whose descriptor advertises `play_stream`;
    /// whole-WAV `play` remains the fallback. Chunks carry bare S16LE PCM
    /// at the rate `play_begin` declares; seq starts at 0 and must
    /// increase by exactly one (the device treats violations as a
    /// poisoned stream and drops the connection).
    PlayBegin {
        task_id: String,
        device: String,
        content_ref: String,
        audio: AudioMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        barge_in: Option<String>,
    },
    PlayChunk {
        task_id: String,
        seq: u64,
        audio_b64: String,
    },
    PlayEnd {
        task_id: String,
    },
    /// R1j/P1 (v0.4): replace the device-side keyword vocabulary. The core
    /// COMPILES the file from the reflex configuration; the gateway owns the
    /// device assets: it validates every token against `tokens.txt`, backs up
    /// the previous file, writes the new one and restarts the spotter. The
    /// receipt says what actually happened (a bad token silently kills the
    /// WHOLE vocabulary on this stack, so "accepted" must be a device fact,
    /// not an optimistic assumption).
    KwsDeploy {
        device: String,
        keywords: String,
        /// Contract v2 §2.5 (#5, 2026-11-16): the words in this file whose
        /// intent is `retreat`. The gateway keeps them as its LOCAL EXIT set:
        /// a hit the echo guard does not call self-heard cuts both audio
        /// channels before the core is asked. Empty (an older core, or a
        /// table with no retreat word) means no local exit at all.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retreat_words: Vec<String>,
        /// FNV-1a of the word→intent mapping this deploy asks the device to
        /// act on (`text\tintent\n` per enabled word). The keyword file hash
        /// proves the VOCABULARY; this proves which MEANING came with it, and
        /// the receipt echoes it so "installed" can name both.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        intents_hash: String,
        /// Contract v2 #4 (2026-11-16): the spotter's sensitivity. The
        /// gateway passes them to the sidecar as `KWS_THRESHOLD`/`KWS_SCORE`
        /// and reports back the values the MODEL actually loaded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        score: Option<f32>,
    },
    /// M6c-2 (2026-11-14): the output tone. `curve` picks the fixed voicing
    /// ("echomuse" | "official" | "rui" | "custom"); `bands` is the user's
    /// peaking EQ on top of it. The gateway applies it to the playback thread
    /// between periods and logs the result; nothing is played by this frame.
    AudioEq {
        device: String,
        curve: String,
        #[serde(default)]
        bands: Vec<EqBand>,
    },
    /// v1 mechanism (#8/#13, 2026-11-16): set the device's mute LATCH to the
    /// given value — the same latch the physical key toggles, no timeout, no
    /// auto-unmute. This is the ONE frame that may write the latch; nothing
    /// implicit (an AssistantState, a mode change) ever may. Sent ONLY to
    /// devices whose descriptor declares `remote_mute` (an older firmware
    /// would skip an unknown frame at runtime and silently ignore the
    /// command — the capability gate keeps the core from calling that
    /// success). `task_id` correlates the answering `MuteState` so a stale
    /// or cross-device fact cannot settle the wrong command.
    MuteSet {
        task_id: String,
        device: String,
        muted: bool,
    },
    /// (#35): configure LED ring night dimming schedule & physical orientation.
    LedConfig {
        device: String,
        #[serde(default)]
        night_enabled: bool,
        #[serde(default)]
        night_start_minute: u16,
        #[serde(default)]
        night_end_minute: u16,
        #[serde(default)]
        night_brightness_pct: u8,
        #[serde(default)]
        orientation_rotation: u8,
        #[serde(default)]
        orientation_reverse: bool,
    },
    ConfigStateGet {
        request_id: String,
        device: String,
    },
    ConfigValidate {
        request_id: String,
        device: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_state_revision: Option<u64>,
        block: ConfigBlock,
        schema_version: u32,
        block_hash: String,
        values: serde_json::Value,
    },
    ConfigApply {
        device: String,
        core_node_id: String,
        data_epoch: u64,
        application_id: String,
        operation_id: String,
        desired_revision: u64,
        block_generation: u64,
        block: ConfigBlock,
        schema_version: u32,
        block_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_gateway_boot_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_state_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability_fingerprint: Option<String>,
        values: serde_json::Value,
    },
}

/// One peaking band of the output tone (mirrors the gateway's `EqBand`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EqBand {
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

// --- events the hub publishes to the voice pipeline ---------------------------

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    GatewayConnected {
        gateway: String,
        generation: u64,
    },
    GatewayDisconnected {
        generation: u64,
    },
    DeviceJoined {
        device: HelloDevice,
        generation: u64,
        aec_tail_ms: Option<u32>,
        profile_version: Option<String>,
    },
    DeviceLeft {
        device: String,
    },
    Wake {
        device: String,
        ts: u64,
        seq: u64,
        provenance: InputProvenance,
    },
    Utterance {
        device: String,
        ts: u64,
        seq: u64,
        source: String,
        text: String,
        provenance: InputProvenance,
    },
    Button {
        device: String,
        ts: u64,
        seq: u64,
        action: String,
    },
    MuteState {
        device: String,
        ts: u64,
        seq: u64,
        muted: bool,
        source: String,
        /// v1 mechanism: `MuteSet.task_id` echo (None = a device fact).
        task_id: Option<String>,
    },
    /// M6a: `played_ms` is what the device WROTE; `consumed_ms` (with the
    /// basis that produced it) is how much the DAC consumed. M5's resume
    /// cursor must read `consumed_ms` and must not treat `played_ms` as
    /// "heard" when the basis is "written".
    PlayFinished {
        device: String,
        play_id: String,
        played_ms: u64,
        consumed_ms: Option<u64>,
        progress_basis: Option<String>,
        ts: u64,
    },
    /// v0.2.3: watchdog outcome of a WifiConfig change ("applied" with the
    /// new ssid joined, or "reverted" with the backup restored).
    WifiState {
        device: String,
        ts: u64,
        ssid: String,
        state: String,
        detail: Option<String>,
    },
    ModeReport {
        device: String,
        duplex: DuplexState,
        aec: AecState,
        /// Hub frame sequence at ARRIVAL (not when the voice loop consumes
        /// it). A management command uses this to require a report that
        /// arrived after it was sent.
        seq: u64,
    },
    /// v0.2: raw uplink PCM block (already seq-checked; the voice pipeline's
    /// mic processor consumes these — no interpretation happens in the hub).
    AudioChunk {
        device: String,
        provenance: InputProvenance,
        ts: u64,
        seq: u64,
        pcm: Vec<i16>,
        sample_rate: u32,
        channels: u16,
        layout: String,
        /// Resolved gateway/sidecar neural VAD decision, if provided on the wire.
        remote_vad: Option<bool>,
    },
    /// v0.2: the gateway's achieved mic state (answer to mic_session).
    MicReport {
        device: String,
        active: bool,
        detail: Option<String>,
    },
    /// v0.2: per-device online/offline inside a live gateway connection.
    /// The pipeline treats it like DeviceJoined/DeviceLeft for state setup
    /// and teardown; the gateway connection itself stays up.
    DeviceStatus {
        device: String,
        online: bool,
        reason: String,
        descriptor: Option<HelloDevice>,
    },
    /// v0.2.4 (D/T4): local keyword-spotter hit on the device.
    Keyword {
        device: String,
        provenance: InputProvenance,
        ts: u64,
        keyword: String,
        fed_ms: Option<u64>,
        /// v0.2.7 (R1g): echo-guard hint (see GatewayFrame::Keyword).
        self_heard: bool,
        /// v0.2.9: acoustic evidence for the wake claim (see the frame).
        rms_dbfs: Option<f32>,
        lag_ms: Option<u64>,
        kws_tier: Option<String>,
        wake_dur_ms: Option<u64>,
        max_gap_ms: Option<u64>,
        l2_passed: Option<bool>,
    },
    /// v0.2.5 (D/T5): genuine near-end speech detected during voice
    /// playback, with the onset pre-roll PCM (decoded S16LE @16k mono).
    BargeIn {
        device: String,
        /// The device's send time (gateway clock). `rx_ms` is when THIS
        /// process decoded the frame; the two together split the 2026-11-12
        /// 3.078 s fact latency into wire (device out + north link + this
        /// reader) and pump (broadcast queue + serialized pipeline) instead
        /// of guessing (R1d instrumentation).
        ts: u64,
        rx_ms: u64,
        pcm: Vec<i16>,
        sample_rate: u32,
        corr: f32,
        energy_db: f32,
        /// R1: reference level / residual margin / block lock integrity.
        ref_db: f32,
        residual_db: f32,
        ref_locked: bool,
    },
    /// R1d: the device stopped the voice channel locally (hardware fact).
    StoppedLocally {
        device: String,
        task_id: String,
        reason: String,
        played_ms: u64,
        latency_ms: u64,
        ahead_ms: u64,
        ts: u64,
    },
    /// v0.2.4 (D/T4): spotter state from the device gateway.
    KwsReport {
        device: String,
        state: String,
        detail: String,
        restarts: u64,
        detections: u64,
        /// M6b: the spotter's own load proof (see `GatewayFrame::KwsReport`).
        loaded_hash: String,
        loaded_ms: u64,
        /// v0.2.5 (D/T5): self-heard hits suppressed by the device guard.
        guarded: u64,
        /// v0.2.6 (R1g/P7): the gateway process epoch (see GatewayFrame).
        epoch: u64,
        /// #5: the device's armed local-exit count (see GatewayFrame).
        local_exit: u32,
        /// #4: the sensitivity the loaded model reported.
        threshold: f32,
        score: f32,
    },
    /// R1j/P1: the device's answer to a keyword deployment — accepted or
    /// rejected, with the hash of the file it now holds.
    KwsDeployReceipt {
        device: String,
        accepted: bool,
        hash: String,
        keywords: u64,
        detail: String,
        /// #5: retreat words armed for local exit (0 = none), and the
        /// word→intent map version the device stored.
        local_exit: u32,
        intents_hash: String,
        /// #4: the sensitivity the spotter runs with after this deploy.
        threshold: f32,
        score: f32,
    },
    /// Contract v2 §2.5 (#5, 2026-11-16): the device cut its own audio on a
    /// retreat word and reported the fact. The pipeline records it; the
    /// session itself is closed by the `Keyword` frame, never by this one.
    LocalExitFlushed {
        device: String,
        keyword: String,
        ts: u64,
        latency_ms: u64,
        channels: Vec<String>,
        detail: Option<String>,
    },
    /// Receipt progress: stage transitions worth surfacing (device_confirmed,
    /// failed, unknown) plus stop/volume confirmations.
    PlayStage {
        play_id: String,
        stage: String,
        detail: Option<String>,
    },
    StopStage {
        play_id: String,
        stage: String,
        outcome: super::StopOutcome,
        played_ms: Option<u64>,
        /// M6a: consumed vs written (see `DeviceEvent::PlayFinished`).
        consumed_ms: Option<u64>,
        progress_basis: Option<String>,
    },
    VolumeStage {
        play_id: String,
        effective: f64,
        stage: String,
    },
    EchoHealth {
        device: String,
        health: String,
        degraded_ms: u64,
        reason: Option<String>,
    },
    NearEndPresence {
        device: String,
        ts_ms: u64,
        duration_ms: u64,
        rms_dbfs: Option<f32>,
        noise_floor_dbfs: Option<f32>,
        snr_db: Option<f32>,
        clipped: bool,
        echo_health: Option<String>,
        profile_version: Option<String>,
    },
    PlayRejected {
        play_id: String,
        device: String,
        reason: String,
        ts: u64,
    },
    ConfigState {
        device: String,
        gateway_boot_id: Option<String>,
        state_revision: Option<u64>,
        blocks: Vec<ConfigBlockState>,
        ts: u64,
    },
    ConfigReceipt {
        device: String,
        application_id: String,
        operation_id: String,
        block: ConfigBlock,
        block_generation: u64,
        state: ConfigReceiptStatus,
        reason: Option<String>,
        retryable: bool,
        persisted_hash: Option<String>,
        active_hash: Option<String>,
        active_values: Option<serde_json::Value>,
        evidence: Option<serde_json::Value>,
        ts: u64,
    },
}
