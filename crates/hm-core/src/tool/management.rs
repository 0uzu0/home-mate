//! Device management as TOOLS (R2-BM6): volume and duplex mode.
//!
//! The point is not "a second way to set the volume". It is that a voice
//! request and the console go through the SAME code: `VoicePipeline::set_volume`
//! and `VoicePipeline::set_mode` are exactly what `POST /devices/{id}/volume`
//! and `PUT /mode/{device}` call — same validation, same config history, same
//! device dispatch. The tool adds only the ACTION FACT: the spoken sentence is
//! rendered by the core from the DEVICE's receipt (`volume_receipt`,
//! `mode_report`), never from the request we sent.
//!
//! Why the `Weak<VoicePipeline>` instead of an `Arc`: the voice pipeline owns
//! the env factory, which owns the registry, which owns these tools. A strong
//! reference would be a cycle. The pipeline is attached once at startup; a
//! test registry without a pipeline answers `MANAGE_UNAVAILABLE` honestly.

use crate::gateway::DeviceHub;
use crate::store::Store;
use crate::tool::{
    Availability, Evidence, FactBasis, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolError,
    ToolFact, ToolResult, ToolStatus,
};
use crate::voice::VoicePipeline;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub const MANAGE_UNAVAILABLE: &str = "MANAGE_UNAVAILABLE";
pub const DEVICE_NOT_FOUND: &str = "DEVICE_NOT_FOUND";
pub const MANAGE_REJECTED: &str = "MANAGE_REJECTED";
pub const VOLUME_UNCONFIRMED: &str = "VOLUME_UNCONFIRMED";
pub const MODE_UNCONFIRMED: &str = "MODE_UNCONFIRMED";
pub const BAD_ARGS: &str = "BAD_ARGS";

/// How long a tool waits for the device's own receipt. The device answers in
/// tens of milliseconds on a healthy link; 1.5 s is a bound, not a delay
/// (a receipt that already arrived returns immediately).
const RECEIPT_WAIT: Duration = Duration::from_millis(1_500);
const POLL: Duration = Duration::from_millis(50);
/// The firmware reports a quantized 0..1 value (n/127); allow one step plus
/// rounding noise.
const VOLUME_TOLERANCE: f64 = 0.02;

pub enum ModeWait {
    Matched,
    /// The device kept reporting a different effective mode.
    Other(String),
    /// No usable mode report within the bound.
    NoReport,
}

pub struct ManagementRuntime {
    store: Arc<Store>,
    hub: Arc<DeviceHub>,
    voice: Mutex<Option<Weak<VoicePipeline>>>,
}

impl ManagementRuntime {
    pub fn new(store: Arc<Store>, hub: Arc<DeviceHub>) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            voice: Mutex::new(None),
        })
    }

    /// Called once after `VoicePipeline::start()`. Not a configuration step:
    /// without it the tools refuse instead of pretending.
    pub fn attach_voice(&self, voice: &Arc<VoicePipeline>) {
        *crate::lock_or_recover(&self.voice, "management.voice") = Some(Arc::downgrade(voice));
    }

    fn voice(&self) -> Option<Arc<VoicePipeline>> {
        crate::lock_or_recover(&self.voice, "management.voice")
            .as_ref()
            .and_then(Weak::upgrade)
    }

    fn label(&self, device: &str) -> String {
        let meta = crate::config::ConfigService::new(self.store.clone())
            .device_meta()
            .unwrap_or_default();
        match meta.get(device) {
            Some(m) if !m.room.trim().is_empty() => m.room.trim().to_string(),
            Some(m) if !m.name.trim().is_empty() => m.name.trim().to_string(),
            _ => device.to_string(),
        }
    }

    /// `None` = the only/first audio-out device. A name/room from
    /// `device_meta` is accepted because that is what a person says.
    fn resolve_device(&self, requested: Option<&str>) -> Result<String, String> {
        let req = requested.map(str::trim).filter(|s| !s.is_empty());
        let snap = self.hub.snapshot();
        let devices = snap["devices"].as_array().cloned().unwrap_or_default();
        let online = |d: &Value| d["online"].as_bool().unwrap_or(false);
        if let Some(req) = req {
            let meta = crate::config::ConfigService::new(self.store.clone())
                .device_meta()
                .unwrap_or_default();
            let by_meta: Option<String> = meta.iter().find_map(|(id, m)| {
                let hit = m.name.trim().eq_ignore_ascii_case(req)
                    || m.room.trim().eq_ignore_ascii_case(req);
                hit.then(|| id.clone())
            });
            let hit = devices.iter().find(|d| {
                d["stable_id"].as_str() == Some(req)
                    || by_meta.as_deref() == d["stable_id"].as_str()
            });
            return match hit {
                Some(d) if online(d) => Ok(d["stable_id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| req.to_string())),
                Some(_) => Err(format!("{DEVICE_NOT_FOUND}: 设备「{req}」当前不在线")),
                None => Err(format!(
                    "{DEVICE_NOT_FOUND}: 找不到设备「{req}」（可用：{}）",
                    devices
                        .iter()
                        .filter_map(|d| d["stable_id"].as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                )),
            };
        }
        devices
            .iter()
            .find(|d| {
                online(d)
                    && d["capabilities"]
                        .as_array()
                        .map(|c| c.iter().any(|x| x == "audio_out"))
                        .unwrap_or(false)
            })
            .and_then(|d| d["stable_id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("{DEVICE_NOT_FOUND}: 没有已连接的输出设备（音箱）"))
    }

    /// Wait for a NEW device receipt (arrival after `baseline_at_ms`) whose
    /// value equals the target. The audit caught the old behaviour: a cached
    /// value that happened to match was treated as this command's receipt, so
    /// a silent device could still produce 「已调到」. A matching old state is
    /// not evidence that THIS request was applied.
    async fn wait_for_volume(&self, device: &str, target: f64, baseline_seq: u64) -> Option<f64> {
        let deadline = Instant::now() + RECEIPT_WAIT;
        loop {
            if let Some((v, seq)) = self.device_volume(device) {
                if seq > baseline_seq && (v - target).abs() <= VOLUME_TOLERANCE {
                    return Some(v);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// `(reported volume, hub arrival sequence of the confirming receipt)`.
    fn device_volume(&self, device: &str) -> Option<(f64, u64)> {
        let snap = self.hub.snapshot();
        let d = snap["devices"]
            .as_array()?
            .iter()
            .find(|d| d["stable_id"] == json!(device))?;
        Some((d["volume"].as_f64()?, d["volume_seq"].as_u64().unwrap_or(0)))
    }

    /// `(effective mode, hub arrival sequence of the ModeReport that said it)`.
    fn current_mode(&self, device: &str) -> Option<(String, u64)> {
        let snap = self.voice()?.mode_snapshot();
        let d = snap["devices"]
            .as_array()?
            .iter()
            .find(|d| d["device"] == json!(device))?;
        Some((
            d["duplex"]["effective"].as_str()?.to_string(),
            d["duplex_report_seq"].as_u64().unwrap_or(0),
        ))
    }

    /// Wait for a ModeReport that ARRIVED after `baseline_seq`.
    /// `Matched` = a NEW report says the target; `Other` = a NEW report says
    /// something else (an evidenced capability downgrade); `NoReport` = no
    /// new report at all — the outcome is unknown, never "the device cannot".
    async fn wait_for_mode(&self, device: &str, target: &str, baseline_seq: u64) -> ModeWait {
        let deadline = Instant::now() + RECEIPT_WAIT;
        loop {
            if let Some((effective, seq)) = self.current_mode(device) {
                if seq > baseline_seq {
                    return if effective == target {
                        ModeWait::Matched
                    } else {
                        ModeWait::Other(effective)
                    };
                }
            }
            if Instant::now() >= deadline {
                return ModeWait::NoReport;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn mode_label(mode: &str) -> &'static str {
        if mode == "full" {
            "全双工"
        } else {
            "半双工"
        }
    }
}

fn fact(
    subject: String,
    action: &str,
    value: Option<String>,
    confirmed: bool,
    detail: Option<String>,
) -> ToolFact {
    ToolFact {
        subject,
        action: action.into(),
        basis: FactBasis::DeviceReceipt,
        confirmed,
        detail,
        value,
    }
}

// --- volume ----------------------------------------------------------------

pub struct VolumeTool {
    rt: Arc<ManagementRuntime>,
}

impl VolumeTool {
    pub fn new(rt: Arc<ManagementRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for VolumeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "system.volume".into(),
            service: "system".into(),
            side_effect: SideEffect::Mutate,
            description:
                "调节已连接音箱的音量（百分比 0–100）。结果只认设备回执；没有回执就如实说没确认"
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "percent": { "type": "number", "minimum": 0, "maximum": 100 },
                    "device": { "type": "string", "description": "设备名或房间；不填用默认音箱" }
                },
                "required": ["percent"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let Some(percent) = args.get("percent").and_then(Value::as_f64) else {
            return ToolResult::failed(BAD_ARGS, "缺少 percent（0–100）", false);
        };
        if !(0.0..=100.0).contains(&percent) || !percent.is_finite() {
            return ToolResult::failed(BAD_ARGS, "percent 必须在 0–100 之间", false);
        }
        let device = match self
            .rt
            .resolve_device(args.get("device").and_then(Value::as_str))
        {
            Ok(d) => d,
            Err(e) => return ToolResult::failed(DEVICE_NOT_FOUND, e, false),
        };
        let Some(voice) = self.rt.voice() else {
            return ToolResult::failed(
                MANAGE_UNAVAILABLE,
                "语音管线还没就绪，没有执行任何动作",
                true,
            );
        };
        let value = (percent / 100.0).clamp(0.0, 1.0);
        let subject = format!("{}的音量", self.rt.label(&device));
        // Baseline arrival sequence: only a receipt that arrived AFTER this
        // request may confirm it (an old queued receipt has a lower seq).
        let baseline_seq = self.rt.device_volume(&device).map(|(_, s)| s).unwrap_or(0);
        if let Err(e) = voice.set_volume(&device, value) {
            return ToolResult::failed(MANAGE_REJECTED, e.clone(), false).with_fact(fact(
                subject,
                "set",
                Some(format!("{percent:.0}%")),
                false,
                Some(e),
            ));
        }
        match self.rt.wait_for_volume(&device, value, baseline_seq).await {
            Some(reported) => {
                let shown = format!("{:.0}%", (reported * 100.0).round());
                ToolResult {
                    status: ToolStatus::Completed,
                    evidence: Evidence::DeviceConfirmed,
                    result: Some(json!({
                        "device": device, "requested": value, "reported": reported,
                    })),
                    error: None,
                    fact: Some(fact(subject, "set", Some(shown), true, None)),
                }
            }
            None => ToolResult {
                status: ToolStatus::Unknown,
                evidence: Evidence::Unknown,
                result: Some(json!({ "device": device, "requested": value })),
                error: Some(ToolError {
                    code: VOLUME_UNCONFIRMED.into(),
                    retryable: true,
                    detail: "设备没有回执这次音量设置".into(),
                }),
                fact: Some(fact(
                    subject,
                    "set",
                    Some(format!("{percent:.0}%")),
                    false,
                    Some("设备没有回执".into()),
                )),
            },
        }
    }
}

/// Reading the volume (2026-11-15, user report).
///
/// The mutating tool needs a number, so "现在音量多少" had no answer at all —
/// the model had to admit 「我没有读音量工具」 and a relative request
/// (「小一点」/「大声点」) could only guess a percentage. This is the read
/// half: the DEVICE's last reported level, with the same honesty rule as
/// everywhere else (a level that was never reported is unknown, not 0).
pub struct VolumeReadTool {
    rt: Arc<ManagementRuntime>,
}

impl VolumeReadTool {
    pub fn new(rt: Arc<ManagementRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for VolumeReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "system.volume_reading".into(),
            service: "system".into(),
            side_effect: SideEffect::Query,
            description: "查询音箱当前的音量（百分比 0–100，来自设备自己的回执）。用户问「现在音量多少」、或要相对调整（「小一点」「大声点」）时先读它，再决定要不要调用 system.volume".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "device": { "type": "string", "description": "设备名或房间；不填用默认音箱" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        // Read-only: usable without a speaker too (it then reports why).
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let device = match self
            .rt
            .resolve_device(args.get("device").and_then(Value::as_str))
        {
            Ok(d) => d,
            Err(e) => return ToolResult::failed(DEVICE_NOT_FOUND, e, false),
        };
        let subject = format!("{}的音量", self.rt.label(&device));
        match self.rt.device_volume(&device) {
            Some((level, _seq)) => {
                let shown = format!("{:.0}%", (level * 100.0).round());
                ToolResult {
                    status: ToolStatus::Completed,
                    evidence: Evidence::DeviceConfirmed,
                    result: Some(json!({
                        "device": device,
                        // Both units: the device speaks 0–127, people speak %.
                        "percent": (level * 100.0).round(),
                        "level": (level * 127.0).round(),
                    })),
                    error: None,
                    // `get` renders as 「儿童房的音量是 30%」.
                    fact: Some(fact(subject, "get", Some(shown), true, None)),
                }
            }
            None => ToolResult {
                status: ToolStatus::Unknown,
                evidence: Evidence::Unknown,
                result: Some(json!({ "device": device })),
                error: Some(ToolError {
                    code: "VOLUME_NOT_REPORTED".into(),
                    retryable: true,
                    detail: "设备还没有报过音量".into(),
                }),
                fact: Some(fact(
                    subject,
                    "get",
                    None,
                    false,
                    Some("设备还没有报过音量".into()),
                )),
            },
        }
    }
}

// --- duplex mode -----------------------------------------------------------

pub struct ModeTool {
    rt: Arc<ManagementRuntime>,
}

impl ModeTool {
    pub fn new(rt: Arc<ManagementRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for ModeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "system.duplex".into(),
            service: "system".into(),
            side_effect: SideEffect::Mutate,
            description: "切换与音箱的对话模式（half=半双工，full=全双工）。与页面同一个校验与变更记录；结果只认设备的模式回报".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["half", "full"] },
                    "device": { "type": "string", "description": "设备名或房间；不填用默认音箱" }
                },
                "required": ["mode"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let Some(raw) = args.get("mode").and_then(Value::as_str) else {
            return ToolResult::failed(BAD_ARGS, "缺少 mode（half|full）", false);
        };
        let mode = match raw.trim().to_ascii_lowercase().as_str() {
            "half" | "半双工" => "half",
            "full" | "全双工" => "full",
            _ => return ToolResult::failed(BAD_ARGS, "mode 只能是 half 或 full", false),
        };
        let device = match self
            .rt
            .resolve_device(args.get("device").and_then(Value::as_str))
        {
            Ok(d) => d,
            Err(e) => return ToolResult::failed(DEVICE_NOT_FOUND, e, false),
        };
        let Some(voice) = self.rt.voice() else {
            return ToolResult::failed(
                MANAGE_UNAVAILABLE,
                "语音管线还没就绪，没有执行任何动作",
                true,
            );
        };
        let subject = format!("{}的对话模式", self.rt.label(&device));
        let value = ManagementRuntime::mode_label(mode).to_string();
        let baseline_seq = self.rt.current_mode(&device).map(|(_, s)| s).unwrap_or(0);
        // The SAME call the console's PUT /mode uses: same validation, same
        // history row (actor=voice), same mode_set dispatch.
        let applied = voice.set_mode(
            &device,
            Some(mode.to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "voice",
        );
        let applied = match applied {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::failed(MANAGE_REJECTED, e.clone(), false).with_fact(fact(
                    subject,
                    "switch",
                    Some(value),
                    false,
                    Some(e),
                ))
            }
        };
        let reason = applied["reason"].as_str().map(str::to_string);
        match self.rt.wait_for_mode(&device, mode, baseline_seq).await {
            ModeWait::Matched => ToolResult {
                status: ToolStatus::Completed,
                evidence: Evidence::DeviceConfirmed,
                result: Some(json!({ "device": device, "mode": mode })),
                error: None,
                fact: Some(fact(subject, "switch", Some(value), true, None)),
            },
            // The device reported a different effective mode (a capability
            // downgrade): this is a FAILED switch, never "已切换".
            ModeWait::Other(effective) => {
                // Name the real constraint, in words a person hears: a
                // device that reports "half" cannot be "switched to full".
                let detail = if mode == "full" && effective == "half" {
                    "设备只支持半双工".to_string()
                } else {
                    reason.unwrap_or_else(|| {
                        format!("设备当前只能{}", ManagementRuntime::mode_label(&effective))
                    })
                };
                ToolResult {
                    status: ToolStatus::Failed,
                    evidence: Evidence::Accepted,
                    result: Some(
                        json!({ "device": device, "requested": mode, "effective": effective }),
                    ),
                    error: Some(ToolError {
                        code: MODE_UNCONFIRMED.into(),
                        retryable: false,
                        detail: detail.clone(),
                    }),
                    fact: Some(fact(subject, "switch", Some(value), false, Some(detail))),
                }
            }
            // No report: the request is recorded, the outcome is unknown.
            ModeWait::NoReport => ToolResult {
                status: ToolStatus::Unknown,
                evidence: Evidence::Unknown,
                result: Some(json!({ "device": device, "requested": mode })),
                error: Some(ToolError {
                    code: MODE_UNCONFIRMED.into(),
                    retryable: true,
                    detail: "设备没有回报对话模式".into(),
                }),
                fact: Some(fact(
                    subject,
                    "switch",
                    Some(value),
                    false,
                    Some("设备没有回报".into()),
                )),
            },
        }
    }
}

/// Scope is the immutable originating device request, never a default speaker.
pub struct SessionCloseTool {
    rt: Arc<ManagementRuntime>,
}
impl SessionCloseTool {
    pub fn new(rt: Arc<ManagementRuntime>) -> Self {
        Self { rt }
    }
}
#[async_trait::async_trait]
impl Tool for SessionCloseTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "session.close".into(), service: "session".into(), side_effect: SideEffect::Mutate,
            description: "结束当前设备会话并停止输出（scope=device）。明确要退下/结束/别再说了时调用，不得用文字声称已退出；普通词义问题不调用。不确定时 confirm:true 先询问，用户下一轮确认后再次调用。".into(),
            parameters: json!({"type":"object","properties":{"confirm":{"type":"boolean"}},"additionalProperties":false}),
        }
    }
    fn availability(&self) -> Availability {
        Availability::Ready
    }
    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        if !args.is_object()
            || args.as_object().unwrap().keys().any(|k| k != "confirm")
            || args.get("confirm").is_some_and(|v| !v.is_boolean())
        {
            return ToolResult::failed(BAD_ARGS, "参数仅允许 confirm:boolean", false);
        }
        let Some(voice) = self.rt.voice() else {
            return ToolResult::failed(MANAGE_UNAVAILABLE, "会话管理未就绪", true);
        };
        match voice.model_close_session(ctx, args["confirm"].as_bool().unwrap_or(false)) {
            Ok(value) => ToolResult {
                status: ToolStatus::Completed,
                evidence: Evidence::Observed,
                result: Some(value),
                error: None,
                fact: None,
            },
            Err(e) => ToolResult::failed("SESSION_CLOSE_REJECTED", e, false),
        }
    }
}

#[cfg(test)]
mod volume_reading_tests {
    use super::*;

    /// 2026-11-15 (user report, measured on dot2-d through the text path):
    /// 「现在音量是多少」 answered 「我没有"读音量"的工具」. The read half exists
    /// now, and its sentence is the device's number — never 「已完成」.
    #[test]
    fn the_reading_renders_the_devices_number_not_an_action() {
        let subject = "儿童房的音量".to_string();
        let mut r = ToolResult {
            status: ToolStatus::Completed,
            evidence: Evidence::DeviceConfirmed,
            result: Some(json!({ "device": "dot2-d", "percent": 30.0, "level": 38.0 })),
            error: None,
            fact: Some(fact(subject, "get", Some("30%".into()), true, None)),
        };
        let f = r.fact.clone().unwrap();
        assert_eq!(
            crate::agent::facts::render_fact(&r, &f),
            "儿童房的音量是30%。"
        );

        // A device that never reported a level is unknown, not 0 %.
        r.status = ToolStatus::Unknown;
        r.evidence = Evidence::Unknown;
        let f = fact(
            "儿童房的音量".into(),
            "get",
            None,
            false,
            Some("设备还没有报过音量".into()),
        );
        let r2 = ToolResult {
            fact: Some(f.clone()),
            ..r
        };
        assert_eq!(
            crate::agent::facts::render_fact(&r2, &f),
            "没能读到儿童房的音量：设备还没有报过音量。"
        );
    }
}

// --- playback reading ------------------------------------------------------

pub struct PlaybackReadTool {
    rt: Arc<ManagementRuntime>,
}

impl PlaybackReadTool {
    pub fn new(rt: Arc<ManagementRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for PlaybackReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "system.playback_reading".into(),
            service: "system".into(),
            side_effect: SideEffect::Query,
            description: "查询音箱当前或刚才（含唤醒暂停前）播放的音乐、电台、故事或播报内容，以及是否处于暂停或空闲状态。用户询问「现在在放什么」「刚才在放什么」「正在播放什么」「有没有在放歌」时调用；一句直接回答，不修改任何播放状态".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "device": { "type": "string", "description": "设备名或房间；不填优先使用当前发问音箱" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let explicit_dev = args
            .get("device")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let device = if explicit_dev.is_some() {
            match self.rt.resolve_device(explicit_dev) {
                Ok(d) => d,
                Err(e) => return ToolResult::failed(DEVICE_NOT_FOUND, e, false),
            }
        } else if let Some(ctx_dev) = ctx
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.rt
                .resolve_device(Some(ctx_dev))
                .unwrap_or_else(|_| ctx_dev.to_string())
        } else {
            match self.rt.resolve_device(None) {
                Ok(d) => d,
                Err(e) => return ToolResult::failed(DEVICE_NOT_FOUND, e, false),
            }
        };
        let Some(voice) = self.rt.voice() else {
            return ToolResult::failed(
                MANAGE_UNAVAILABLE,
                "语音管线还没就绪，暂时无法读取播放状态",
                true,
            );
        };
        let mut reading = voice.playback_reading(&device, Some(&ctx.task_id));
        reading["device_label"] = json!(self.rt.label(&device));
        ToolResult::observed_ok(reading)
    }
}

#[cfg(test)]
mod playback_reading_tests {
    use super::*;

    #[tokio::test]
    async fn playback_reading_prefers_context_device_and_reports_idle() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let hub = DeviceHub::new();
        let tasks = Arc::new(crate::task::TaskRegistry::new(store.clone()));
        let rt = ManagementRuntime::new(store.clone(), hub.clone());
        let env_factory: crate::voice::EnvFactory = Arc::new(|| Err("no env".into()));
        let tts_factory: crate::voice::TtsFactory = Arc::new(|| Err("no tts".into()));
        let asr_factory: crate::voice::AsrFactory = Arc::new(|| Err("no asr".into()));
        let voice = VoicePipeline::new(
            hub,
            store,
            tasks,
            env_factory,
            tts_factory,
            asr_factory,
            std::env::temp_dir(),
        );
        rt.attach_voice(&voice);
        let tool = PlaybackReadTool::new(rt);
        let ctx = ToolCtx::new(
            "task-q1",
            "call-q1",
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .with_provenance(Some("dot2-d".into()), None);
        let res = tool.execute(json!({}), &ctx).await;
        assert_eq!(res.status, ToolStatus::Completed);
        let v = res.result.unwrap();
        assert_eq!(v["device"], json!("dot2-d"));
        assert_eq!(v["state"], json!("idle"));
        assert_eq!(v["replace_answer"], json!(true));
        assert_eq!(v["rendered_answer"], json!("当前没有在播放任何内容。"));
    }
}
