//! Device-gateway side of protocol v0 (core ↔ external gateway).
//!
//! The core LISTENS (default 127.0.0.1:8932, `HM_GATEWAY_BIND`); gateways
//! dial out. Frames are newline-delimited JSON; schema authority is
//! docs/contracts-v0.md. This module mirrors the frames deliberately: the
//! two repos share the documented contract and contract tests, never code
//! (see docs/device-gateway-project.md).
//!
//! Ownership (architecture principles §1): the hub owns connections,
//! generations, per-device seq freshness and pending-receipt matching. The
//! voice pipeline owns interaction semantics; nothing here interprets user
//! intent. Late/stale frames are recorded, never acted on.

use crate::id;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};

pub(crate) fn resolve_remote_vad(vad_speech: Option<bool>, vad_prob: Option<f32>) -> Option<bool> {
    vad_speech.or_else(|| {
        vad_prob
            .filter(|p| p.is_finite())
            .map(|p| p >= crate::silero::DEFAULT_THRESHOLD)
    })
}

mod config;
#[cfg(test)]
mod config_tests;
mod frames;
mod negotiation;
mod northbound;
#[cfg(test)]
mod northbound_tests;
mod outbound;
mod stop;
#[cfg(test)]
mod tests;
mod transport_metrics;

pub use config::{ConfigApplyRequest, ConfigValidateRequest};
pub use frames::*;
pub use negotiation::*;
pub use northbound::{NorthboundFault, NorthboundFaultCode, NorthboundStatus};
pub use stop::{StopOutcome, StopSubmission};

/// A play submission the caller can await.
pub struct PlayTicket {
    pub play_id: String,
    pub receipt: oneshot::Receiver<GatewayFrame>,
}

#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub device: String,
    pub content_ref: String,
    /// WAV bytes (already containerized) sent inline base64.
    pub audio: Option<Vec<u8>>,
    pub audio_meta: AudioMeta,
    pub channel: String,
    pub text: Option<String>,
    pub simulated_ms: Option<u64>,
}

struct HubState {
    generation: u64,
    /// Dropping the prior sender retires both old transport directions.
    connection_retirement: Option<oneshot::Sender<()>>,
    connected_gateway: Option<String>,
    /// v1 mechanism: the machine-readable gateway/firmware package version
    /// from the live hello (empty for an older gateway), the negotiated
    /// operating protocol version, and the last link-level error the peer
    /// REPORTED (a `ProtocolError` frame). `last_link_error` is cleared by a
    /// successful handshake — it describes the previous rejection, never the
    /// live link.
    connected_gateway_version: Option<String>,
    protocol_operating: Option<u32>,
    last_link_error: Option<serde_json::Value>,
    /// Devices as last announced; seq freshness per device.
    devices: HashMap<String, DeviceSlot>,
    /// play_id → oneshot waiting for a terminal receipt, tagged with the
    /// connection generation that submitted it (a superseded connection's
    /// teardown must release ONLY its own waiters).
    pending: HashMap<String, PendingWaiter>,
    /// Per-device waiters for WifiState replies (v0.2.3 wifi change).
    pending_wifi: HashMap<String, oneshot::Sender<DeviceEvent>>,
    config_state_waiters: HashMap<String, config::ConfigOneshotWaiter>,
    config_validate_waiters: HashMap<String, config::ConfigOneshotWaiter>,
    config_apply_waiters: HashMap<String, config::ConfigApplyWaiter>,
    /// volume task_id → device, so an asynchronous `volume_receipt` can be
    /// attributed even though the v0.2 frame carries no device field.
    volume_pending: HashMap<String, String>,
    /// Every play submitted on a live connection, any channel — the hub is
    /// the single registry of in-flight plays so stops can reach media
    /// (tool-submitted) plays as well as voice plays. Removed on terminal
    /// receipt / stop receipt / play_finished / connection teardown.
    active_plays: HashMap<String, ActivePlay>,
    /// R1j/P0-a: callers that must not put new audio on a channel until the
    /// device has actually FLUSHED a stopped play (the waiting cue's tail
    /// overlapped the answer's first syllable). Resolved by the same frames
    /// that end the play: stop_receipt, play_finished, failed/unknown.
    stop_waiters: HashMap<String, Vec<oneshot::Sender<StopOutcome>>>,
    /// Plays a Stop already left for (M3). `stop_all_plays` skips them: the
    /// wake path pauses media first and then stops every channel, and the
    /// device must not receive two Stops for one play.
    stop_sent: std::collections::HashSet<String>,
    /// M6a: play ids retired by a connection teardown. Bounded (cleared when
    /// it grows past 64 — precision for very old ids is worth less than an
    /// unbounded set). A receipt naming one of these crossed a generation
    /// and must never settle anything.
    retired_plays: std::collections::HashSet<String>,
    /// Monotonic arrival sequence for every frame handled on this hub. Used
    /// by management receipts: "a report that arrived after my command".
    frame_seq: u64,
    /// Diagnostic feed for the console (bounded).
    ring: VecDeque<serde_json::Value>,
}

struct PendingWaiter {
    sender: oneshot::Sender<GatewayFrame>,
    generation: u64,
}

#[derive(Debug, Clone)]
pub struct ActivePlay {
    pub device: String,
    pub channel: String,
    pub generation: u64,
}

struct DeviceSlot {
    descriptor: HelloDevice,
    generation: u64,
    last_seq: u64,
    online: bool,
    /// Last volume the DEVICE reported (linear 0..1), not what we asked for.
    /// Updated by `volume_receipt` — including the boot report and the
    /// firmware's own volume keys (`vol:dev:*`), which is what makes the
    /// console follow physical changes.
    volume: Option<f64>,
    volume_at_ms: Option<u64>,
    /// Hub frame sequence of the last CONFIRMED volume receipt.
    volume_seq: u64,
    /// W1 (#72) & #114 (A5): AEC state and tail length reported in Hello.
    aec_enabled: Option<bool>,
    aec_tail_ms: Option<u32>,
    aec_tail_samples: Option<u32>,
    aec_sample_rate: Option<u32>,
    aec_provenance: Option<String>,
    /// W1 (#71/#74): Acoustic calibration profile version reported by gateway.
    profile_version: Option<String>,
    /// W1 (#72/#73): Latest echo-health state ("healthy" | "degraded" | ...).
    echo_health: Option<String>,
    echo_degraded_ms: Option<u64>,
    echo_health_reason: Option<String>,
    northbound: Option<northbound::NorthboundRecord>,
    gateway_boot_id: Option<String>,
    state_revision: Option<u64>,
    capability_fingerprint: Option<String>,
    config_blocks: HashMap<ConfigBlock, ConfigBlockState>,
}

impl HubState {
    fn record(&mut self, kind: &str, mut payload: serde_json::Value) {
        payload["kind"] = json!(kind);
        payload["ts_ms"] = json!(id::now_ms());
        self.ring.push_back(payload);
        while self.ring.len() > 100 {
            self.ring.pop_front();
        }
    }
}

/// Record a device-reported volume. The receipt is the truth (a requested
/// value that failed must not move the console). Only a `device_confirmed`
/// receipt may move the snapshot: `accepted`/`sent` are not the device's
/// resulting state, and a `failed`/`unknown` receipt must never be read as
/// success (R2-BM6 audit — a failed receipt used to update `volume` anyway,
/// which the management tool could then render as 「已调到」).
/// Old v0.2 frames carry no device field: a `vol:boot:`/`vol:dev:` receipt is
/// attributed only when the hub knows exactly one device; an API task is
/// attributed through `volume_pending`. Returns the device when attribution
/// succeeded AND the stage confirmed it.
fn record_volume(
    st: &mut HubState,
    task_id: &str,
    effective: f64,
    stage: &str,
    frame_seq: u64,
    hint: Option<&str>,
) -> Option<String> {
    let terminal = matches!(stage, "failed" | "unknown");
    if !effective.is_finite() || !(0.0..=1.0).contains(&effective) {
        // A terminal OR malformed receipt ends the attempt; keeping the
        // pending entry would leave a stale attribution for the next frame.
        st.volume_pending.remove(task_id);
        return None;
    }
    if stage != "device_confirmed" {
        if terminal {
            st.volume_pending.remove(task_id);
        }
        return None;
    }
    // R2 maintenance (2026-11-13): a receipt that NAMES its device is
    // self-attributing — that is what makes two devices' physical volume keys
    // each update their own snapshot. The hint is only honoured for a device
    // this link knows; a bogus name must not create a phantom entry.
    let hinted = hint
        .filter(|d| st.devices.contains_key(*d))
        .map(str::to_string);
    let device = hinted
        .or_else(|| st.volume_pending.remove(task_id))
        .or_else(|| {
            let device_fact = task_id.starts_with("vol:boot:") || task_id.starts_with("vol:dev:");
            if device_fact && st.devices.len() == 1 {
                st.devices.keys().next().cloned()
            } else {
                None
            }
        })?;
    if let Some(slot) = st.devices.get_mut(&device) {
        slot.volume = Some(effective);
        slot.volume_at_ms = Some(id::now_ms());
        slot.volume_seq = frame_seq;
        Some(device)
    } else {
        None
    }
}

fn ev_device(ev: &DeviceEvent) -> &str {
    match ev {
        DeviceEvent::WifiState { device, .. } => device,
        _ => "",
    }
}

#[derive(Debug, Clone)]
pub struct QueuedDeviceEvent {
    pub seq: u64,
    pub enqueued_at: std::time::Instant,
    pub event: DeviceEvent,
}

fn is_critical_control_event(ev: &DeviceEvent) -> bool {
    matches!(
        ev,
        DeviceEvent::Wake { .. }
            | DeviceEvent::Keyword { .. }
            | DeviceEvent::LocalExitFlushed { .. }
            | DeviceEvent::Button { .. }
            | DeviceEvent::PlayFinished { .. }
    )
}

pub type GatewayHub = DeviceHub;

pub struct DeviceHub {
    transport_tasks: transport_metrics::TransportTasks,
    state: Mutex<HubState>,
    /// Dedicated high-capacity channel for control/lifecycle events so an
    /// `AudioChunk` flood can never lag/drop `Wake`, `Keyword`, `PlayFinished`,
    /// `StopStage`, `LocalExitFlushed`, `MuteState`, etc.
    control_events: broadcast::Sender<DeviceEvent>,
    /// Bounded lossy channel for raw `AudioChunk` frames (may lag/drop under load).
    audio_events: broadcast::Sender<DeviceEvent>,
    /// Timed control/audio channels for the VoicePipeline event pump.
    pump_control_events: broadcast::Sender<QueuedDeviceEvent>,
    pump_audio_events: broadcast::Sender<QueuedDeviceEvent>,
    pump_seq: std::sync::atomic::AtomicU64,
    /// Lossless fallback buffer for critical control events (`Wake`, `Keyword`,
    /// `LocalExitFlushed`, `Button`, `PlayFinished`) when `pump_control_events` lags.
    critical_events: Mutex<VecDeque<QueuedDeviceEvent>>,
    /// Bounded two-tier outbound frame channel of the live connection (latest wins).
    outbound: Mutex<Option<outbound::OutboundSender>>,
    pub bind_addr: Mutex<String>,
    /// Media-channel output trim in dB (0 = off). Applied to the WAV payload
    /// of every media play at submit time; voice plays are never trimmed.
    /// Live value comes from VoiceSettings via VoicePipeline::set_media_trim.
    media_trim_db: Mutex<f64>,
}

impl DeviceHub {
    pub fn new() -> Arc<Self> {
        let (control_tx, _) = broadcast::channel(4096);
        let (audio_tx, _) = broadcast::channel(256);
        let (pump_control_tx, _) = broadcast::channel(4096);
        let (pump_audio_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            transport_tasks: Default::default(),
            state: Mutex::new(HubState {
                generation: 0,
                connection_retirement: None,
                connected_gateway: None,
                connected_gateway_version: None,
                protocol_operating: None,
                last_link_error: None,
                devices: HashMap::new(),
                pending: HashMap::new(),
                pending_wifi: HashMap::new(),
                config_state_waiters: HashMap::new(),
                config_validate_waiters: HashMap::new(),
                config_apply_waiters: HashMap::new(),
                volume_pending: HashMap::new(),
                frame_seq: 0,
                active_plays: HashMap::new(),
                stop_waiters: HashMap::new(),
                stop_sent: std::collections::HashSet::new(),
                retired_plays: std::collections::HashSet::new(),
                ring: VecDeque::new(),
            }),
            control_events: control_tx,
            audio_events: audio_tx,
            pump_control_events: pump_control_tx,
            pump_audio_events: pump_audio_tx,
            pump_seq: std::sync::atomic::AtomicU64::new(0),
            critical_events: Mutex::new(VecDeque::new()),
            outbound: Mutex::new(None),
            bind_addr: Mutex::new(String::new()),
            media_trim_db: Mutex::new(crate::voice::default_media_trim_db()),
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HubState> {
        crate::lock_or_recover(&self.state, "hub.state")
    }

    /// Accept loop. One gateway connection at a time is the v0 shape (a new
    /// connection supersedes the previous one and bumps the generation).
    pub async fn listen(self: &Arc<Self>, addr: std::net::SocketAddr) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        *crate::lock_or_recover(&self.bind_addr, "hub.bind_addr") = addr.to_string();
        tracing::info!(%addr, "gateway listener up");
        loop {
            let (stream, peer) = listener.accept().await?;
            let hub = self.clone();
            tokio::spawn(async move {
                if let Err(e) = hub.serve_connection(stream).await {
                    tracing::warn!(%peer, error = %e, "gateway connection ended with error");
                }
            });
        }
    }

    /// Subscribe to control/lifecycle events (never carries `AudioChunk`).
    pub fn subscribe_control(&self) -> broadcast::Receiver<DeviceEvent> {
        self.control_events.subscribe()
    }

    /// Subscribe to timed control/lifecycle events for the VoicePipeline pump.
    pub fn subscribe_pump_control(&self) -> broadcast::Receiver<QueuedDeviceEvent> {
        self.pump_control_events.subscribe()
    }

    /// Subscribe to raw `AudioChunk` frames (lossy under backpressure).
    pub fn subscribe_audio(&self) -> broadcast::Receiver<DeviceEvent> {
        self.audio_events.subscribe()
    }

    /// Subscribe to timed `AudioChunk` frames for the VoicePipeline pump.
    pub fn subscribe_pump_audio(&self) -> broadcast::Receiver<QueuedDeviceEvent> {
        self.pump_audio_events.subscribe()
    }

    /// Recover any critical control events with `after_seq < seq < before_seq`.
    pub fn take_critical_between(&self, after_seq: u64, before_seq: u64) -> Vec<QueuedDeviceEvent> {
        let mut crit = crate::lock_or_recover(&self.critical_events, "hub.critical_events");
        let mut out = Vec::new();
        while let Some(front) = crit.front() {
            if front.seq >= before_seq {
                break;
            }
            let Some(item) = crit.pop_front() else {
                break;
            };
            if item.seq > after_seq {
                out.push(item);
            }
        }
        out
    }

    /// Recover any critical control events with `seq > after_seq`.
    pub fn take_critical_after(&self, after_seq: u64) -> Vec<QueuedDeviceEvent> {
        let mut crit = crate::lock_or_recover(&self.critical_events, "hub.critical_events");
        let mut out = Vec::new();
        while let Some(item) = crit.pop_front() {
            if item.seq > after_seq {
                out.push(item);
            }
        }
        out
    }

    /// Acknowledge critical control events up to `seq` so they can be evicted.
    pub fn ack_critical_up_to(&self, seq: u64) {
        let mut crit = crate::lock_or_recover(&self.critical_events, "hub.critical_events");
        while crit.front().is_some_and(|e| e.seq <= seq) {
            crit.pop_front();
        }
    }

    fn emit_control(&self, ev: DeviceEvent) {
        let _ = self.control_events.send(ev.clone());
        let seq = self
            .pump_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let queued = QueuedDeviceEvent {
            seq,
            enqueued_at: std::time::Instant::now(),
            event: ev,
        };
        if is_critical_control_event(&queued.event) && self.pump_control_events.receiver_count() > 0
        {
            let mut crit = crate::lock_or_recover(&self.critical_events, "hub.critical_events");
            if crit.len() >= 65_536 {
                crit.pop_front();
            }
            crit.push_back(queued.clone());
        }
        let _ = self.pump_control_events.send(queued);
    }

    fn emit_audio(&self, ev: DeviceEvent) {
        let _ = self.audio_events.send(ev.clone());
        let seq = self
            .pump_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let _ = self.pump_audio_events.send(QueuedDeviceEvent {
            seq,
            enqueued_at: std::time::Instant::now(),
            event: ev,
        });
    }

    /// Backwards-compatible alias for `subscribe_control()`.
    pub fn subscribe(&self) -> broadcast::Receiver<DeviceEvent> {
        self.control_events.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn handle_frame_for_test(&self, generation: u64, frame: GatewayFrame) {
        self.handle_frame(generation, frame);
    }

    #[cfg(test)]
    pub(crate) fn set_generation_for_test(&self, generation: u64) {
        self.lock_state().generation = generation;
    }

    #[cfg(test)]
    pub(crate) fn poison_state_for_test(self: &Arc<Self>) {
        let hub = self.clone();
        let _ = std::thread::spawn(move || {
            let _g = hub.state.lock().unwrap();
            panic!("simulated panic while holding hub.state");
        })
        .join();
    }

    pub(crate) async fn serve_connection(
        self: Arc<Self>,
        stream: TcpStream,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _connection_task =
            transport_metrics::LiveTask::enter(&self.transport_tasks.connections);
        let (rd, mut wr) = stream.into_split();
        let mut lines = CappedLineReader::new(BufReader::new(rd));
        let (tx, mut rx) =
            outbound::outbound_channel_with_metrics(self.transport_tasks.outbound.clone());

        // Handshake: bounded wait (a connected-but-silent peer must not park
        // this task forever; the line cap applies from the first byte).
        let hello_line =
            match tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await
            {
                Err(_) => return Err("hello not received within 10s".into()),
                Ok(Err(e)) => return Err(e),
                Ok(Ok(None)) => return Err("connection closed before hello".into()),
                Ok(Ok(Some(l))) => l,
            };
        let gateway_name;
        let gateway_version: Option<String>;
        let operating: u32;
        let announced: Vec<HelloDevice>;
        let hello_aec_tail_ms: Option<u32>;
        let hello_aec_enabled: Option<bool>;
        let hello_aec_tail_samples: Option<u32>;
        let hello_aec_sample_rate: Option<u32>;
        let hello_aec_provenance: Option<String>;
        let hello_profile_version: Option<String>;
        let hello_extra: serde_json::Value =
            serde_json::from_str(&hello_line).unwrap_or(serde_json::Value::Null);
        match serde_json::from_str::<GatewayFrame>(&hello_line)? {
            GatewayFrame::Hello {
                protocol,
                min_protocol,
                supported_versions,
                gateway,
                gateway_version: version,
                devices,
                aec_tail_ms,
                profile_version,
            } => {
                let mine = (
                    crate::DEVICE_PROTOCOL_MIN_VERSION,
                    crate::DEVICE_PROTOCOL_VERSION,
                );
                let peer = (min_protocol, protocol);
                let Some(v) = protocol_operating_version_with_supported(
                    mine,
                    crate::DEVICE_PROTOCOL_SUPPORTED_VERSIONS,
                    peer,
                    &supported_versions,
                ) else {
                    // v1 mechanism: say WHY before closing, over the handshake
                    // socket itself (the writer task does not exist yet). A
                    // bounded write — a stuck peer must not park this task.
                    let reason = protocol_no_intersection_reason(mine, peer);
                    {
                        let mut st = self.lock_state();
                        st.last_link_error = Some(json!({
                            "kind": "protocol_error", "reason": reason,
                            "peer_protocol": protocol, "peer_min_protocol": min_protocol,
                        }));
                        st.record("handshake_rejected", json!({ "reason": reason }));
                    }
                    tracing::warn!(%gateway, reason, "refused gateway hello");
                    let reject = CoreFrame::HelloReject {
                        protocol: crate::DEVICE_PROTOCOL_VERSION,
                        min_protocol: crate::DEVICE_PROTOCOL_MIN_VERSION,
                        core_version: crate::CORE_VERSION.to_string(),
                        reason: reason.clone(),
                    };
                    let line = serde_json::to_string(&reject)?;
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(REJECT_WRITE_TIMEOUT_MS),
                        async {
                            let _ = wr.write_all(line.as_bytes()).await;
                            let _ = wr.write_all(b"\n").await;
                        },
                    )
                    .await;
                    return Err(format!("protocol rejected: {reason}").into());
                };
                // A gateway must announce DISTINCT ids ... (moved into the
                // same reject path: the duplicate now also gets a reason).
                if let Err(reason) = ensure_unique_device_ids(&devices) {
                    {
                        let mut st = self.lock_state();
                        st.last_link_error =
                            Some(json!({ "kind": "protocol_error", "reason": reason }));
                        st.record("handshake_rejected", json!({ "reason": reason }));
                    }
                    tracing::warn!(%gateway, reason, "refused gateway hello");
                    let reject = CoreFrame::HelloReject {
                        protocol: crate::DEVICE_PROTOCOL_VERSION,
                        min_protocol: crate::DEVICE_PROTOCOL_MIN_VERSION,
                        core_version: crate::CORE_VERSION.to_string(),
                        reason: reason.clone(),
                    };
                    let line = serde_json::to_string(&reject)?;
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(REJECT_WRITE_TIMEOUT_MS),
                        async {
                            let _ = wr.write_all(line.as_bytes()).await;
                            let _ = wr.write_all(b"\n").await;
                        },
                    )
                    .await;
                    return Err(format!("protocol rejected: {reason}").into());
                }
                gateway_name = gateway;
                gateway_version = (!version.is_empty()).then_some(version);
                operating = v;
                announced = devices;
                hello_aec_tail_ms = aec_tail_ms.or_else(|| {
                    hello_extra
                        .pointer("/aec/tail_ms")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32)
                });
                hello_aec_enabled = hello_extra
                    .get("aec_enabled")
                    .or_else(|| hello_extra.pointer("/aec/enabled"))
                    .and_then(|v| v.as_bool())
                    .or_else(|| hello_aec_tail_ms.map(|ms| ms > 0));
                hello_aec_sample_rate = hello_extra
                    .get("aec_sample_rate")
                    .or_else(|| hello_extra.pointer("/aec/sample_rate"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .or_else(|| (hello_aec_enabled == Some(true)).then_some(16_000));
                hello_aec_tail_samples = hello_extra
                    .get("effective_tail_samples")
                    .or_else(|| hello_extra.pointer("/aec/effective_tail_samples"))
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .or_else(|| {
                        hello_aec_tail_ms
                            .zip(hello_aec_sample_rate)
                            .map(|(ms, sr)| ms.saturating_mul(sr) / 1000)
                    });
                hello_aec_provenance = hello_extra
                    .get("aec_provenance")
                    .or_else(|| hello_extra.pointer("/aec/provenance"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                hello_profile_version = profile_version.filter(|s| !s.trim().is_empty());
            }
            f => return Err(format!("expected hello, got {f:?}").into()),
        }

        // v1 mechanism: remember the negotiated link facts and clear the
        // previous rejection — the link is healthy NOW; last_link_error only
        // ever describes the handshake before this one.
        {
            let mut st = self.lock_state();
            st.connected_gateway_version = gateway_version.clone();
            st.protocol_operating = Some(operating);
            st.last_link_error = None;
        }

        let (retirement_tx, retirement_rx) = oneshot::channel::<()>();
        let (generation, superseded) = {
            let mut st = self.lock_state();
            st.generation += 1;
            let gen = st.generation;
            // Supersession actively wakes the old connection even while its
            // writer is blocked in write_all and its reader remains open.
            st.connection_retirement.replace(retirement_tx);
            // These command maps belong to the replaced current connection;
            // clear before any new-generation command can be submitted.
            st.volume_pending.clear();
            st.pending_wifi.clear();
            st.config_state_waiters.clear();
            st.config_validate_waiters.clear();
            st.config_apply_waiters.clear();
            let superseded = crate::lock_or_recover(&self.outbound, "hub.outbound")
                .replace(tx.clone())
                .is_some();
            // Re-register announced devices on this generation.
            for d in &announced {
                let previous_northbound = st
                    .devices
                    .get(&d.stable_id)
                    .and_then(|s| s.northbound.clone());
                st.devices.insert(
                    d.stable_id.clone(),
                    DeviceSlot {
                        descriptor: d.clone(),
                        generation: gen,
                        last_seq: 0,
                        online: true,
                        volume: None,
                        volume_at_ms: None,
                        volume_seq: 0,
                        aec_enabled: hello_aec_enabled,
                        aec_tail_ms: hello_aec_tail_ms,
                        aec_tail_samples: hello_aec_tail_samples,
                        aec_sample_rate: hello_aec_sample_rate,
                        aec_provenance: hello_aec_provenance.clone(),
                        profile_version: hello_profile_version.clone(),
                        echo_health: None,
                        echo_degraded_ms: None,
                        echo_health_reason: None,
                        northbound: previous_northbound,
                        gateway_boot_id: None,
                        state_revision: None,
                        capability_fingerprint: None,
                        config_blocks: HashMap::new(),
                    },
                );
                st.record(
                    "device_joined",
                    json!({
                        "device": d.stable_id,
                        "generation": gen,
                        "aec_tail_ms": hello_aec_tail_ms,
                        "profile_version": hello_profile_version,
                    }),
                );
            }
            st.connected_gateway = Some(gateway_name.clone());
            st.record(
                "gateway_connected",
                json!({
                    "gateway": gateway_name,
                    "gateway_version": gateway_version,
                    "protocol_operating": operating,
                    "generation": gen,
                }),
            );
            (gen, superseded)
        };
        if superseded {
            tracing::warn!("new gateway connection supersedes a previous one");
        }
        self.emit_control(DeviceEvent::GatewayConnected {
            gateway: gateway_name.clone(),
            generation,
        });
        for d in &announced {
            self.emit_control(DeviceEvent::DeviceJoined {
                device: d.clone(),
                generation,
                aec_tail_ms: hello_aec_tail_ms,
                profile_version: hello_profile_version.clone(),
            });
        }

        // Say hello_ack (and project the host clock — emOS has no time
        // service; G6 replaced-capability #2), then pump both directions.
        // The hub owns the only outbound sender. The independent retirement
        // signal also interrupts a superseded writer blocked on the socket.
        tx.send(CoreFrame::HelloAck {
            protocol: operating,
            min_protocol: crate::DEVICE_PROTOCOL_MIN_VERSION,
            supported_versions: if operating < 2 {
                vec![operating]
            } else {
                crate::DEVICE_PROTOCOL_SUPPORTED_VERSIONS.to_vec()
            },
            capabilities: if operating < 2 {
                Vec::new()
            } else {
                vec!["config_apply_v2".to_string()]
            },
            core_version: crate::CORE_VERSION.to_string(),
            generation,
        })
        .map_err(|_| "outbound channel closed before hello_ack")?;
        let _ = tx.send(CoreFrame::TimeSet {
            ts_ms: crate::id::now_ms(),
        });
        drop(tx);

        let writer_lifetime = transport_metrics::LiveTask::enter(&self.transport_tasks.writers);
        let mut writer = tokio::spawn(async move {
            let _writer_task = writer_lifetime;
            while let Some(f) = rx.recv().await {
                let line = match serde_json::to_string(&f) {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if wr.write_all(line.as_bytes()).await.is_err()
                    || wr.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
        });

        let hub = self.clone();
        let reader_lifetime = transport_metrics::LiveTask::enter(&self.transport_tasks.readers);
        let reader = async move {
            let _reader_future = reader_lifetime;
            while let Some(line) = lines.next_line().await? {
                if line.is_empty() {
                    continue;
                }
                let frame: GatewayFrame = match serde_json::from_str(&line) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = %e, "undecodable gateway frame");
                        continue;
                    }
                };
                hub.handle_frame(generation, frame);
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        };
        // Exit when EITHER side ends: reader EOF (gateway gone) or writer
        // task done (superseded by a newer connection closed our channel).
        let (res, writer_joined): (Result<(), Box<dyn std::error::Error + Send + Sync>>, bool) = tokio::select! {
            r = reader => (r, false),
            _ = &mut writer => (Err("outbound channel closed".into()), true),
            _ = retirement_rx => (Err("connection superseded".into()), false),
        };
        // Dropping a JoinHandle detaches it: a half-closed peer that does not
        // read could keep the old writer and queued payloads forever. Retire
        // that actual task and TCP half before reporting connection teardown.
        if !writer_joined {
            writer.abort();
            let _ = writer.await;
        }
        // Connection over. A CURRENT generation tears down its devices,
        // outbound slot and events; a SUPERSEDED generation may only release
        // its own pending waiters and active plays — never the live
        // connection's (they cannot receive receipts anyway, so waiters see
        // a closed channel = unknown outcome).
        self.retire_connection(generation);
        res
    }

    fn retire_connection(&self, generation: u64) {
        {
            let mut st = self.lock_state();
            let current = st.generation == generation;
            if current {
                st.connection_retirement = None;
                st.connected_gateway = None;
                st.connected_gateway_version = None;
                st.protocol_operating = None;
                let left: Vec<String> = st
                    .devices
                    .iter()
                    .filter(|(_, d)| d.generation == generation)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in &left {
                    if let Some(dev) = st.devices.get_mut(id) {
                        dev.online = false;
                    }
                    st.record("device_left", json!({ "device": id }));
                    self.emit_control(DeviceEvent::DeviceLeft { device: id.clone() });
                }
                *crate::lock_or_recover(&self.outbound, "hub.outbound") = None;
                st.record("gateway_disconnected", json!({ "generation": generation }));
            } else {
                st.record(
                    "gateway_connection_superseded_teardown",
                    json!({ "generation": generation }),
                );
            }
            // M6a: remember which plays this generation owned BEFORE the
            // retain drops them. A receipt for one of these that arrives on a
            // later connection is stale evidence (the gateway clears its own
            // audio state on link-down), and it must not settle a waiter or
            // drive an event.
            let retiring: Vec<String> = st
                .active_plays
                .iter()
                .filter(|(_, p)| p.generation == generation)
                .map(|(id, _)| id.clone())
                .chain(
                    st.pending
                        .iter()
                        .filter(|(_, w)| w.generation == generation)
                        .map(|(id, _)| id.clone()),
                )
                .collect();
            for id in retiring {
                Self::resolve_stop_waiters(
                    &mut st,
                    &id,
                    StopOutcome::unknown("connection_retired"),
                );
                st.stop_sent.remove(&id);
                st.retired_plays.insert(id);
            }
            if st.retired_plays.len() > 64 {
                st.retired_plays.clear();
            }
            st.pending.retain(|_, w| w.generation != generation);
            st.config_state_waiters
                .retain(|_, w| w.generation != generation);
            st.config_validate_waiters
                .retain(|_, w| w.generation != generation);
            st.config_apply_waiters
                .retain(|_, w| w.generation != generation);
            if current {
                st.volume_pending.clear();
                st.pending_wifi.clear();
            }
            st.active_plays.retain(|_, p| p.generation != generation);
            // R1j/P0-a: a dead link can never confirm a flush. Release the
            // bounded waiters now instead of making every answer wait out
            // its timeout.
            if current {
                st.stop_waiters.clear();
                st.stop_sent.clear();
            }
            if current {
                self.emit_control(DeviceEvent::GatewayDisconnected { generation });
            }
        }
    }

    /// Route one gateway frame: freshness check on events, receipt matching
    /// for play/stop/volume, state updates for mode reports.
    fn handle_frame(&self, generation: u64, frame: GatewayFrame) {
        let mut st = self.lock_state();
        // Monotonic ARRIVAL sequence, not a wall-clock millisecond: a report
        // that arrived before a management command must never be able to
        // confirm it, even when both happen in the same millisecond or the
        // voice loop consumes the queued event later (audit round 2).
        st.frame_seq = st.frame_seq.wrapping_add(1);
        let frame_seq = st.frame_seq;
        let transport = TransportIdentity {
            generation,
            inbound_seq: frame_seq,
        };
        // Events from a superseded connection generation: record only.
        let stale_generation = st.generation != generation;
        match frame {
            GatewayFrame::Hello { .. } => {
                st.record("late_hello", json!({ "generation": generation }));
            }
            GatewayFrame::ProtocolError {
                protocol,
                min_protocol,
                gateway_version,
                reason,
            } => {
                // v1 mechanism: the gateway refused the link and said why
                // before disconnecting. A fact about the PREVIOUS handshake
                // attempt — recorded here and surfaced by /status; cleared by
                // the next successful handshake, never by this frame.
                st.last_link_error = Some(json!({
                    "kind": "protocol_error",
                    "reason": reason,
                    "peer_protocol": protocol,
                    "peer_min_protocol": min_protocol,
                    "gateway_version": gateway_version,
                    "ts_ms": id::now_ms(),
                }));
                st.record(
                    "link_protocol_error",
                    json!({ "reason": reason, "peer_protocol": protocol }),
                );
                tracing::warn!(%reason, peer_protocol = protocol, "gateway reported a protocol error");
            }
            GatewayFrame::Wake {
                device,
                ts,
                seq,
                origin,
            } => {
                let provenance = InputProvenance {
                    origin,
                    transport: Some(transport),
                };
                if stale_generation || !seq_fresh(&mut st, &device, seq) {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "seq": seq, "kind": "wake" }),
                    );
                    return;
                }
                st.record(
                    "wake",
                    json!({ "device": device, "seq": seq, "provenance": provenance }),
                );
                self.emit_control(DeviceEvent::Wake {
                    device,
                    ts,
                    seq,
                    provenance,
                });
            }
            GatewayFrame::Utterance {
                device,
                ts,
                seq,
                source,
                text,
                origin,
            } => {
                let provenance = InputProvenance {
                    origin,
                    transport: Some(transport),
                };
                if stale_generation || !seq_fresh(&mut st, &device, seq) {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "seq": seq, "kind": "utterance" }),
                    );
                    return;
                }
                st.record("utterance", json!({ "device": device, "seq": seq, "source": source, "text": text, "provenance": provenance }));
                self.emit_control(DeviceEvent::Utterance {
                    device,
                    ts,
                    seq,
                    source,
                    text,
                    provenance,
                });
            }
            GatewayFrame::Button {
                device,
                ts,
                seq,
                action,
            } => {
                if stale_generation || !seq_fresh(&mut st, &device, seq) {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "seq": seq, "kind": "button" }),
                    );
                    return;
                }
                st.record(
                    "button",
                    json!({ "device": device, "seq": seq, "action": action }),
                );
                self.emit_control(DeviceEvent::Button {
                    device,
                    ts,
                    seq,
                    action,
                });
            }
            GatewayFrame::MuteState {
                device,
                ts,
                seq,
                muted,
                source,
                task_id,
            } => {
                if stale_generation || !seq_fresh(&mut st, &device, seq) {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "seq": seq, "kind": "mute_state" }),
                    );
                    return;
                }
                st.record(
                    "mute_state",
                    json!({ "device": device, "muted": muted, "source": source, "task_id": task_id }),
                );
                // v1 mechanism: settle THE command this fact answers. A
                // physical-key press (or a boot report) carries no task_id and
                // must never resolve a remote command's waiter; a fact that
                // crossed a connection generation settles nothing either.
                if let Some(tid) = task_id.clone() {
                    if let Some(waiter) = st.pending.remove(&tid) {
                        if waiter.generation == generation {
                            let _ = waiter.sender.send(GatewayFrame::MuteState {
                                device: device.clone(),
                                ts,
                                seq,
                                muted,
                                source: source.clone(),
                                task_id: Some(tid),
                            });
                        }
                    }
                }
                self.emit_control(DeviceEvent::MuteState {
                    device,
                    ts,
                    seq,
                    muted,
                    source,
                    task_id,
                });
            }
            GatewayFrame::PlayReceipt {
                task_id: play_id,
                stage,
                detail,
                ts,
            } => {
                if Self::receipt_is_stale(
                    &mut st,
                    &play_id,
                    generation,
                    json!({ "frame": "play_receipt", "stage": stage, "detail": detail }),
                ) {
                    return;
                }
                st.record(
                    "play_receipt",
                    json!({ "play_id": play_id, "stage": stage, "detail": detail }),
                );
                let is_terminal =
                    matches!(stage.as_str(), "device_confirmed" | "failed" | "unknown");
                if is_terminal {
                    if let Some(waiter) = st.pending.remove(&play_id) {
                        let _ = waiter.sender.send(GatewayFrame::PlayReceipt {
                            task_id: play_id.clone(),
                            stage: stage.clone(),
                            detail: detail.clone(),
                            ts,
                        });
                    } else {
                        // Late or unknown-play receipt: record only (the ring
                        // entry above is the audit trail).
                        tracing::debug!(%play_id, %stage, "receipt with no waiter");
                    }
                }
                // failed/unknown end the play; device_confirmed means it is
                // RUNNING now (removed later by stop_receipt/play_finished).
                if matches!(stage.as_str(), "failed" | "unknown") {
                    st.active_plays.remove(&play_id);
                    st.stop_sent.remove(&play_id);
                    Self::resolve_stop_waiters(
                        &mut st,
                        &play_id,
                        if stage == "failed" {
                            StopOutcome::Failed {
                                reason: detail.clone().unwrap_or(stage.clone()),
                            }
                        } else {
                            StopOutcome::unknown(detail.clone().unwrap_or(stage.clone()))
                        },
                    );
                }
                self.emit_control(DeviceEvent::PlayStage {
                    play_id,
                    stage,
                    detail,
                });
            }
            GatewayFrame::StopReceipt {
                task_id: play_id,
                stage,
                played_ms,
                consumed_ms,
                progress_basis,
                detail,
            } => {
                if Self::receipt_is_stale(
                    &mut st,
                    &play_id,
                    generation,
                    json!({ "frame": "stop_receipt", "stage": stage, "played_ms": played_ms, "detail": detail }),
                ) {
                    return;
                }
                st.record(
                    "stop_receipt",
                    json!({
                        "play_id": play_id, "stage": stage, "played_ms": played_ms,
                        "consumed_ms": consumed_ms, "progress_basis": progress_basis,
                        "detail": detail,
                    }),
                );
                st.active_plays.remove(&play_id);
                st.stop_sent.remove(&play_id);
                // M5: a stop receipt IS the play's terminal. The waiter used
                // to be released only by `play_finished`, so a caller that
                // waited for ONE play's end (the per-sentence resume cursor)
                // hung until its timeout on every interruption and then had
                // no idea what the DAC had consumed.
                if let Some(waiter) = st.pending.remove(&play_id) {
                    let _ = waiter.sender.send(GatewayFrame::StopReceipt {
                        task_id: play_id.clone(),
                        stage: stage.clone(),
                        played_ms,
                        consumed_ms,
                        progress_basis: progress_basis.clone(),
                        detail: detail.clone(),
                    });
                }
                let outcome = StopOutcome::receipt(
                    &stage,
                    detail.as_deref(),
                    played_ms,
                    consumed_ms,
                    progress_basis.clone(),
                );
                Self::resolve_stop_waiters(&mut st, &play_id, outcome.clone());
                st.record("stop_outcome", json!({"play_id":play_id,"outcome":outcome}));
                self.emit_control(DeviceEvent::StopStage {
                    outcome,
                    play_id,
                    stage,
                    played_ms,
                    consumed_ms,
                    progress_basis,
                });
            }
            GatewayFrame::VolumeReceipt {
                task_id: play_id,
                effective,
                unit,
                stage,
                device,
            } => {
                let device = record_volume(
                    &mut st,
                    &play_id,
                    effective,
                    &stage,
                    frame_seq,
                    device.as_deref(),
                );
                st.record(
                    "volume_receipt",
                    json!({
                        "play_id": play_id, "effective": effective, "unit": unit,
                        "stage": stage, "device": device,
                    }),
                );
                self.emit_control(DeviceEvent::VolumeStage {
                    play_id,
                    effective,
                    stage,
                });
            }
            GatewayFrame::PlayRejected {
                task_id,
                device,
                reason,
                ts,
            } => {
                if Self::receipt_is_stale(
                    &mut st,
                    &task_id,
                    generation,
                    json!({ "frame": "play_rejected", "reason": reason }),
                ) {
                    return;
                }
                st.active_plays.remove(&task_id);
                st.stop_sent.remove(&task_id);
                Self::resolve_stop_waiters(
                    &mut st,
                    &task_id,
                    StopOutcome::unknown("play_rejected"),
                );
                if let Some(waiter) = st.pending.remove(&task_id) {
                    let _ = waiter.sender.send(GatewayFrame::PlayRejected {
                        task_id: task_id.clone(),
                        device: device.clone(),
                        reason: reason.clone(),
                        ts,
                    });
                }
                st.record(
                    "play_rejected",
                    json!({ "play_id": task_id, "device": device, "reason": reason }),
                );
                self.emit_control(DeviceEvent::PlayRejected {
                    play_id: task_id,
                    device,
                    reason,
                    ts,
                });
            }
            f @ GatewayFrame::ConfigState { .. } => {
                self.handle_config_state(&mut st, stale_generation, f);
            }
            f @ GatewayFrame::ConfigValidated { .. } => {
                self.handle_config_validated(&mut st, stale_generation, f);
            }
            f @ GatewayFrame::ConfigReceipt { .. } => {
                self.handle_config_receipt(&mut st, stale_generation, f);
            }
            GatewayFrame::ModeReport {
                device,
                duplex,
                aec,
            } => {
                st.record(
                    "mode_report",
                    json!({ "device": device, "duplex": duplex, "aec": aec }),
                );
                self.emit_control(DeviceEvent::ModeReport {
                    device,
                    duplex,
                    aec,
                    seq: frame_seq,
                });
            }
            GatewayFrame::WifiState {
                device,
                ts,
                ssid,
                state,
                detail,
            } => {
                // handle_frame runs under the hub state lock — reuse the
                // `st` guard; re-locking here deadlocks the same thread
                // (std Mutex is not reentrant; caught by wifi_wedge_repro).
                let ev = DeviceEvent::WifiState {
                    device,
                    ts,
                    ssid,
                    state,
                    detail,
                };
                let waiter = st.pending_wifi.remove(ev_device(&ev));
                if let Some(tx) = waiter {
                    let _ = tx.send(ev.clone());
                }
                self.emit_control(ev);
            }
            GatewayFrame::PlayFinished {
                device,
                task_id: play_id,
                played_ms,
                consumed_ms,
                progress_basis,
                ts,
            } => {
                if Self::receipt_is_stale(
                    &mut st,
                    &play_id,
                    generation,
                    json!({ "frame": "play_finished", "played_ms": played_ms, "consumed_ms": consumed_ms }),
                ) {
                    return;
                }
                st.record(
                    "play_finished",
                    json!({
                        "device": device, "play_id": play_id, "played_ms": played_ms,
                        "consumed_ms": consumed_ms, "progress_basis": progress_basis,
                    }),
                );
                st.active_plays.remove(&play_id);
                st.stop_sent.remove(&play_id);
                // A finish IS a terminal fact for a parked play waiter: a
                // device that skips the `device_confirmed` receipt must not
                // hold a caller (the wake ack waits on it) for its full bound.
                if let Some(waiter) = st.pending.remove(&play_id) {
                    let _ = waiter.sender.send(GatewayFrame::PlayFinished {
                        device: device.clone(),
                        task_id: play_id.clone(),
                        played_ms,
                        consumed_ms,
                        progress_basis: progress_basis.clone(),
                        ts,
                    });
                }
                Self::resolve_stop_waiters(
                    &mut st,
                    &play_id,
                    StopOutcome::unknown("play_finished_without_flush"),
                );
                self.emit_control(DeviceEvent::PlayFinished {
                    device,
                    play_id,
                    played_ms,
                    consumed_ms,
                    progress_basis,
                    ts,
                });
            }
            GatewayFrame::AudioChunk {
                device,
                ts,
                seq,
                pcm_b64,
                format,
                sample_rate,
                channels,
                layout,
                origin,
                vad_speech,
                vad_prob,
            } => {
                let provenance = InputProvenance {
                    origin,
                    transport: Some(transport),
                };
                let remote_vad = resolve_remote_vad(vad_speech, vad_prob);
                // Steady-cadence frames (dot2-d ≈ 12.5/s): seq-check yes, ring
                // NO — recording every chunk would flush the diagnostic ring
                // in seconds. Loss shows up as a seq gap recorded here.
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "seq": seq, "kind": "audio_chunk" }),
                    );
                    return;
                }
                let seq_check = match st.devices.get_mut(&device) {
                    Some(slot) if seq > slot.last_seq => {
                        let expected = slot.last_seq + 1;
                        slot.last_seq = seq;
                        if seq != expected {
                            Some(format!("gap: seq {seq}, expected {expected}"))
                        } else {
                            None
                        }
                    }
                    Some(_) => {
                        st.record("audio_chunk_stale", json!({ "device": device, "seq": seq }));
                        return;
                    }
                    None => {
                        st.record(
                            "audio_chunk_unknown_device",
                            json!({ "device": device, "seq": seq }),
                        );
                        None
                    }
                };
                if let Some(anomaly) = seq_check {
                    st.record(
                        "audio_chunk_gap",
                        json!({ "device": device, "anomaly": anomaly }),
                    );
                }
                if format != "s16le" {
                    st.record(
                        "audio_chunk_bad_format",
                        json!({ "device": device, "format": format }),
                    );
                    return;
                }
                // channels feeds a modulo below: 0 would panic and poison
                // the hub lock (review P2-1). v0.2 layouts are 1|2 only.
                if !matches!(channels, 1 | 2) {
                    st.record(
                        "audio_chunk_bad_channels",
                        json!({ "device": device, "channels": channels }),
                    );
                    return;
                }
                use base64::Engine;
                let raw = match base64::engine::general_purpose::STANDARD.decode(&pcm_b64) {
                    Ok(r) => r,
                    Err(e) => {
                        st.record(
                            "audio_chunk_bad_b64",
                            json!({ "device": device, "error": e.to_string() }),
                        );
                        return;
                    }
                };
                // Whole frames only: 2ch mic_ref must carry whole stereo
                // pairs (odd sample counts would silently lose the last
                // sample in the pipeline's downmix).
                if raw.len() % 2 != 0 || raw.len() % (channels as usize * 2) != 0 {
                    st.record(
                        "audio_chunk_bad_alignment",
                        json!({ "device": device, "len": raw.len(), "channels": channels }),
                    );
                    return;
                }
                let pcm = raw
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                self.emit_audio(DeviceEvent::AudioChunk {
                    device,
                    ts,
                    seq,
                    pcm,
                    sample_rate,
                    channels,
                    layout,
                    provenance,
                    remote_vad,
                });
            }
            GatewayFrame::MicReport {
                device,
                active,
                layout,
                sample_rate,
                channels,
                detail,
            } => {
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "mic_report" }),
                    );
                    return;
                }
                st.record("mic_report", json!({ "device": device, "active": active, "layout": layout, "sample_rate": sample_rate, "channels": channels, "detail": detail }));
                self.emit_control(DeviceEvent::MicReport {
                    device,
                    active,
                    detail,
                });
            }
            GatewayFrame::DeviceStatus {
                device,
                online,
                reason,
                descriptor,
                northbound,
            } => {
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "device_status" }),
                    );
                    return;
                }
                // v0.2 online governance: upsert the slot (introducing via
                // descriptor when present), flip `online`, and surface a
                // DeviceStatus event. The gateway connection itself is NOT
                // touched — a southbound device drop is device state.
                let generation = st.generation;
                match descriptor {
                    Some(desc) if desc.stable_id == device => {
                        let slot = st.devices.entry(device.clone()).or_insert(DeviceSlot {
                            descriptor: desc.clone(),
                            generation,
                            last_seq: 0,
                            online: false,
                            volume: None,
                            volume_at_ms: None,
                            volume_seq: 0,
                            aec_enabled: None,
                            aec_tail_ms: None,
                            aec_tail_samples: None,
                            aec_sample_rate: None,
                            aec_provenance: None,
                            profile_version: None,
                            echo_health: None,
                            echo_degraded_ms: None,
                            echo_health_reason: None,
                            northbound: None,
                            gateway_boot_id: None,
                            state_revision: None,
                            capability_fingerprint: None,
                            config_blocks: HashMap::new(),
                        });
                        // A descriptor arriving on a NEWER connection than
                        // the slot's starts a fresh seq epoch: gateways that
                        // re-introduce via device_status (adapter restarts,
                        // the on-device form announces an empty hello) begin
                        // their seq space at 0 — keeping the old watermark
                        // would drop every frame as stale (caught live on
                        // the speaker-resident adapter, 2026-11-08).
                        if slot.generation != generation {
                            slot.generation = generation;
                            slot.last_seq = 0;
                        }
                        slot.descriptor = desc;
                        slot.online = online;
                    }
                    None => {
                        if let Some(slot) = st.devices.get_mut(&device) {
                            if slot.generation != generation {
                                st.record(
                                    "device_status_unregistered_generation",
                                    json!({"device":device}),
                                );
                                return;
                            }
                            slot.online = online;
                        } else {
                            st.record(
                                "device_status_unknown_device",
                                json!({ "device": device, "reason": reason }),
                            );
                            return;
                        }
                    }
                    Some(desc) => {
                        st.record(
                            "device_status_mismatched_descriptor",
                            json!({ "device": device, "descriptor": desc.stable_id }),
                        );
                        return;
                    }
                }
                // Existing descriptor-backed hot join is registration on this
                // connection. Diagnostics attach only after it was accepted.
                if let Some(slot) = st.devices.get_mut(&device) {
                    slot.northbound =
                        Some(northbound::NorthboundRecord::new(northbound, generation));
                }
                st.record(
                    "device_status",
                    json!({ "device": device, "online": online, "reason": reason }),
                );
                self.emit_control(DeviceEvent::DeviceStatus {
                    device: device.clone(),
                    online,
                    reason,
                    descriptor: st.devices.get(&device).map(|d| d.descriptor.clone()),
                });
            }
            // D/T4: device-side keyword spotter. Detections are facts —
            // whether a playback stops is the pipeline's call (voice.rs),
            // so the gateway can never act on its own.
            GatewayFrame::Keyword {
                device,
                ts,
                seq,
                keyword,
                fed_ms,
                self_heard,
                rms_dbfs,
                lag_ms,
                origin,
                bundle_id,
                kws_generation,
            } => {
                let provenance = InputProvenance {
                    origin,
                    transport: Some(transport),
                };
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "keyword" }),
                    );
                    return;
                }
                if let Some(kw_gen) = kws_generation {
                    let active_kws_gen = st
                        .devices
                        .get(&device)
                        .and_then(|d| d.config_blocks.get(&ConfigBlock::Kws))
                        .filter(|b| {
                            b.state == ConfigReceiptStatus::Active && b.block_generation > 0
                        })
                        .map(|b| b.block_generation);
                    if let Some(active_gen) = active_kws_gen {
                        if kw_gen < active_gen {
                            st.record(
                                "stale_kws_keyword",
                                json!({
                                    "device": device,
                                    "keyword": keyword,
                                    "kws_generation": kw_gen,
                                    "active_kws_generation": active_gen,
                                    "bundle_id": bundle_id,
                                }),
                            );
                            return;
                        }
                    }
                }
                // seq is informational here (no ordering guarantees are
                // needed for a reflex event), but it IS recorded so the
                // console can line a detection up with the uplink stream.
                st.record(
                    "keyword",
                    json!({ "device": device, "keyword": keyword, "seq": seq, "fed_ms": fed_ms,
                            "self_heard": self_heard, "rms_dbfs": rms_dbfs, "lag_ms": lag_ms,
                            "bundle_id": bundle_id, "kws_generation": kws_generation,
                            "provenance": provenance }),
                );
                self.emit_control(DeviceEvent::Keyword {
                    device,
                    ts,
                    keyword,
                    fed_ms,
                    self_heard,
                    rms_dbfs,
                    lag_ms,
                    provenance,
                });
            }
            GatewayFrame::KwsReport {
                device,
                state,
                detail,
                restarts,
                detections,
                guarded,
                epoch,
                loaded_hash,
                loaded_ms,
                local_exit,
                threshold,
                score,
            } => {
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "kws_report" }),
                    );
                    return;
                }
                st.record(
                    "kws_report",
                    json!({ "device": device, "state": state, "detail": detail,
                            "restarts": restarts, "detections": detections, "guarded": guarded,
                            "epoch": epoch,
                            // M6b: the spotter's own load proof (empty = not
                            // reported yet), never the installed file hash.
                            "loaded_hash": loaded_hash, "loaded_ms": loaded_ms,
                            // #5: armed local-exit words (a device fact).
                            "local_exit": local_exit,
                            // #4: the applied sensitivity (model's own line).
                            "threshold": threshold, "score": score }),
                );
                self.emit_control(DeviceEvent::KwsReport {
                    device,
                    state,
                    detail,
                    restarts,
                    detections,
                    guarded,
                    epoch,
                    loaded_hash,
                    loaded_ms,
                    local_exit,
                    threshold,
                    score,
                });
            }
            GatewayFrame::KwsDeployReceipt {
                device,
                accepted,
                hash,
                keywords,
                detail,
                local_exit,
                intents_hash,
                threshold,
                score,
            } => {
                st.record(
                    "kws_deploy_receipt",
                    json!({ "device": device, "accepted": accepted, "hash": hash,
                            "keywords": keywords, "detail": detail,
                            "local_exit": local_exit, "intents_hash": intents_hash,
                            "threshold": threshold, "score": score }),
                );
                self.emit_control(DeviceEvent::KwsDeployReceipt {
                    device,
                    accepted,
                    hash,
                    keywords,
                    detail,
                    local_exit,
                    intents_hash,
                    threshold,
                    score,
                });
            }
            // Contract v2 §2.5 (#5): the device already cut its own audio on
            // a retreat word. Recorded with the device's own numbers; the
            // session is closed by the Keyword frame, never by this fact.
            GatewayFrame::LocalExitFlushed {
                device,
                keyword,
                ts,
                latency_ms,
                channels,
                detail,
            } => {
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "local_exit_flushed" }),
                    );
                    return;
                }
                st.record(
                    "local_exit_flushed",
                    json!({ "device": device, "keyword": keyword, "ts": ts,
                            "latency_ms": latency_ms, "channels": channels,
                            "detail": detail }),
                );
                self.emit_control(DeviceEvent::LocalExitFlushed {
                    device,
                    keyword,
                    ts,
                    latency_ms,
                    channels,
                    detail,
                });
            }
            // R1d: the device already stopped its own voice channel. Record
            // it with the device's own numbers and let the pipeline close its
            // bookkeeping; the semantic decision still rides the BargeIn fact.
            GatewayFrame::StoppedLocally {
                device,
                task_id,
                reason,
                played_ms,
                latency_ms,
                ahead_ms,
                ts,
            } => {
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "stopped_locally" }),
                    );
                    return;
                }
                st.record(
                    "stopped_locally",
                    json!({ "device": device, "task_id": task_id, "reason": reason,
                            "played_ms": played_ms, "latency_ms": latency_ms,
                            "ahead_ms": ahead_ms, "ts": ts }),
                );
                self.emit_control(DeviceEvent::StoppedLocally {
                    device,
                    task_id,
                    reason,
                    played_ms,
                    latency_ms,
                    ahead_ms,
                    ts,
                });
            }
            // D/T5: near-end speech during voice playback. The frame decodes
            // the same way AudioChunk does (bad payloads are recorded, never
            // crash the hub); the fact then flows to the pipeline — this
            // branch takes no other hub state beyond the shared record.
            GatewayFrame::BargeIn {
                device,
                ts,
                seq,
                pcm_b64,
                sample_rate,
                channels,
                corr,
                energy_db,
                ref_db,
                residual_db,
                ref_locked,
            } => {
                if stale_generation {
                    st.record(
                        "stale_event",
                        json!({ "device": device, "kind": "barge_in" }),
                    );
                    return;
                }
                st.record(
                    "barge_in",
                    json!({ "device": device, "ts": ts, "seq": seq, "corr": corr,
                            // M6b: the device's `f32::MIN` "no measurement"
                            // sentinel is recorded as MISSING, not as a number.
                            "energy_db": valid_db(energy_db), "ref_db": valid_db(ref_db),
                            "residual_db": valid_db(residual_db), "ref_locked": ref_locked,
                            "samples": pcm_b64.len() / 4 * 3 / 2,
                            "sample_rate": sample_rate, "channels": channels }),
                );
                use base64::Engine;
                let raw = match base64::engine::general_purpose::STANDARD.decode(&pcm_b64) {
                    Ok(r) => r,
                    Err(e) => {
                        st.record(
                            "barge_in_bad_b64",
                            json!({ "device": device, "error": e.to_string() }),
                        );
                        return;
                    }
                };
                if raw.len() % 2 != 0 || channels != 1 {
                    st.record(
                        "barge_in_bad_payload",
                        json!({ "device": device, "len": raw.len(), "channels": channels }),
                    );
                    return;
                }
                let pcm = raw
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                // R1d attribution: stamped HERE, at decode, so the pipeline
                // can separate "the wire was slow" from "this pump was busy".
                let rx_ms = crate::id::now_ms();
                self.emit_control(DeviceEvent::BargeIn {
                    device,
                    ts,
                    rx_ms,
                    pcm,
                    sample_rate,
                    corr,
                    energy_db,
                    ref_db,
                    residual_db,
                    ref_locked,
                });
            }
            GatewayFrame::EchoHealth {
                device,
                health,
                degraded_ms,
                reason,
            } => {
                if stale_generation {
                    return;
                }
                let health_norm = if health.trim().is_empty() {
                    "unknown".to_string()
                } else {
                    health.trim().to_string()
                };
                let changed = if let Some(slot) = st.devices.get_mut(&device) {
                    let prev = slot.echo_health.as_deref();
                    let c = prev != Some(health_norm.as_str());
                    slot.echo_health = Some(health_norm.clone());
                    slot.echo_degraded_ms = Some(degraded_ms);
                    slot.echo_health_reason = reason.clone();
                    c
                } else {
                    true
                };
                if changed || health_norm != "healthy" {
                    st.record(
                        "echo_health",
                        json!({
                            "device": device,
                            "health": health_norm,
                            "degraded_ms": degraded_ms,
                            "reason": reason,
                        }),
                    );
                }
                self.emit_control(DeviceEvent::EchoHealth {
                    device,
                    health: health_norm,
                    degraded_ms,
                    reason,
                });
            }
            GatewayFrame::NearEndPresence {
                device,
                ts_ms,
                duration_ms,
                rms_dbfs,
                noise_floor_dbfs,
                snr_db,
                clipped,
                echo_health,
                profile_version,
            } => {
                if stale_generation {
                    return;
                }
                let echo_health = echo_health.filter(|s| !s.trim().is_empty());
                let profile_version = profile_version.filter(|s| !s.trim().is_empty());
                if let Some(slot) = st.devices.get_mut(&device) {
                    if let Some(ref h) = echo_health {
                        slot.echo_health = Some(h.clone());
                    }
                    if let Some(ref pv) = profile_version {
                        slot.profile_version = Some(pv.clone());
                    }
                }
                self.emit_control(DeviceEvent::NearEndPresence {
                    device,
                    ts_ms,
                    duration_ms,
                    rms_dbfs: rms_dbfs.and_then(valid_db),
                    noise_floor_dbfs: noise_floor_dbfs.and_then(valid_db),
                    snr_db: snr_db.filter(|v| v.is_finite()),
                    clipped,
                    echo_health,
                    profile_version,
                });
            }
        }
    }
}

/// Event freshness: seq must be strictly greater than the last accepted.
/// Updates the watermark only for accepted events. Unknown devices (never
/// announced in a hello) are accepted — the ring records the anomaly.
fn seq_fresh(st: &mut HubState, device: &str, seq: u64) -> bool {
    match st.devices.get_mut(device) {
        None => true,
        Some(slot) => {
            if seq > slot.last_seq {
                slot.last_seq = seq;
                true
            } else {
                false
            }
        }
    }
}

/// Newline-delimited JSON reader with a hard byte cap BEFORE buffering a
/// full frame — an oversized or newline-less peer cannot grow memory.
struct CappedLineReader<R> {
    inner: R,
    buf: Vec<u8>,
    eof: bool,
}

impl<R: tokio::io::AsyncRead + Unpin> CappedLineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(512),
            eof: false,
        }
    }

    /// Next line without the trailing newline; None on clean EOF. Errors
    /// when a single frame exceeds MAX_FRAME_BYTES.
    async fn next_line(
        &mut self,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        use tokio::io::AsyncReadExt;
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.eof {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                return Ok(Some(line));
            }
            if self.buf.len() > MAX_FRAME_BYTES {
                return Err(format!("frame exceeds {MAX_FRAME_BYTES} bytes").into());
            }
            let mut chunk = [0u8; 8192];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

#[cfg(test)]
mod phase5_tests {
    use super::*;

    #[test]
    fn real_wav_playback_streams_play_begin_chunk_end_and_never_whole_wav_play() {
        let hub = DeviceHub::new();
        let (tx, mut rx) = outbound::outbound_channel();
        *crate::lock_or_recover(&hub.outbound, "hub.outbound") = Some(tx);

        // Build a valid 16kHz mono WAV with 20,000 bytes of PCM (>1 chunk).
        let pcm = vec![0x2Au8; 20_000];
        let mut wav = Vec::with_capacity(44 + pcm.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((36 + pcm.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);

        let _ticket = hub
            .play(PlayRequest {
                device: "dot2-d".into(),
                content_ref: "tts:test".into(),
                audio_meta: AudioMeta {
                    format: "wav".into(),
                    sample_rate: 16000,
                    channels: 1,
                },
                audio: Some(wav),
                channel: "voice".into(),
                text: Some("测试".into()),
                simulated_ms: None,
            })
            .unwrap();

        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        assert!(
            !frames.iter().any(|f| matches!(
                f,
                CoreFrame::Play {
                    audio_b64: Some(_),
                    ..
                }
            )),
            "whole-WAV CoreFrame::Play must never be emitted: {frames:?}"
        );
        assert!(matches!(frames.first(), Some(CoreFrame::PlayBegin { .. })));
        assert!(matches!(frames.last(), Some(CoreFrame::PlayEnd { .. })));
        let chunk_count = frames
            .iter()
            .filter(|f| matches!(f, CoreFrame::PlayChunk { .. }))
            .count();
        assert!(
            chunk_count >= 2,
            "expected multiple PlayChunk frames, got {chunk_count}"
        );
    }

    #[test]
    fn led_config_frame_serializes_and_sends_via_hub() {
        let hub = DeviceHub::new();
        let (tx, mut rx) = outbound::outbound_channel();
        *crate::lock_or_recover(&hub.outbound, "hub.outbound") = Some(tx);

        hub.send_led_config("dot2-d", true, 1320, 420, 25, 3, true)
            .unwrap();
        let frame = rx.try_recv().unwrap();
        match &frame {
            CoreFrame::LedConfig {
                device,
                night_enabled,
                night_start_minute,
                night_end_minute,
                night_brightness_pct,
                orientation_rotation,
                orientation_reverse,
            } => {
                assert_eq!(device, "dot2-d");
                assert!(*night_enabled);
                assert_eq!(*night_start_minute, 1320);
                assert_eq!(*night_end_minute, 420);
                assert_eq!(*night_brightness_pct, 25);
                assert_eq!(*orientation_rotation, 3);
                assert!(*orientation_reverse);
            }
            other => panic!("expected LedConfig, got {other:?}"),
        }
        let wire = serde_json::to_value(&frame).unwrap();
        assert_eq!(wire["type"], "led_config");
        assert_eq!(wire["device"], "dot2-d");
        assert_eq!(wire["night_brightness_pct"], 25);
        assert_eq!(wire["orientation_rotation"], 3);
    }
}
