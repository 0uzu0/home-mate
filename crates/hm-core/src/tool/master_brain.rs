//! Provider-agnostic Master Brain async delegation tools (#267 Slice 2).
//!
//! `hm-core` stays the fast, offline-capable edge brain on every speaker. When
//! a household configures a Master Brain endpoint, four tools become `Ready`:
//! - `brain.delegate` — hand off a deep/multi-step/remote-computer/fleet task
//!   asynchronously and return a fast spoken acknowledgement in ~1 s.
//! - `brain.status`   — check current progress (including `last_tool`).
//! - `brain.steer`    — inject guidance or approve/deny a pending confirmation.
//! - `brain.cancel`   — abort a running task.
//!
//! Two wire protocols are supported via `MasterBrainProtocol`:
//! - `hermes` (default, zero-bridge direct mode): talks directly to Hermes
//!   Agent's HTTP/SSE API (`/v1/runs`, `/v1/runs/{id}/events`, `/steer`,
//!   `/approval`, `/stop`) and delivers concise spoken summaries straight into
//!   in-process `VoicePipeline::enqueue_announce` (no bridge daemon, no
//!   `callback_url` required).
//! - `generic`: talks to any external Master Brain service implementing the
//!   Home Mate async contract (`/v1/tasks*` + downward `/api/v0/voice/announce`).

use crate::config::{ConfigService, MasterBrainConfig, MasterBrainProtocol};
use crate::id;
use crate::store::Store;
use crate::tool::http::{read_bounded, validate_news_url};
use crate::tool::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use crate::voice::{AnnounceRequest, VoicePipeline};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

const MAX_JSON_BODY: usize = 256 * 1024;
const MAX_TASK_CHARS: usize = 1_000;
const MAX_CONTEXT_CHARS: usize = 1_000;
const MAX_GUIDANCE_CHARS: usize = 500;
const MAX_ACK_CHARS: usize = 120;
const MAX_SPEECH_CHARS: usize = 280;
const MAX_RECENT_TASKS: usize = 20;
const MAX_HERMES_WATCH_SECS: u64 = 300;
const HERMES_POLL_INTERVAL_MS: u64 = 2_000;
const MAX_SSE_LINE_BYTES: usize = 128 * 1024;

pub const MASTER_BRAIN_UNCONFIGURED: &str = "MASTER_BRAIN_UNCONFIGURED";
pub const MASTER_BRAIN_DISABLED: &str = "MASTER_BRAIN_DISABLED";
pub const MASTER_BRAIN_BAD_ARGS: &str = "MASTER_BRAIN_BAD_ARGS";
pub const MASTER_BRAIN_NO_ACTIVE_TASK: &str = "MASTER_BRAIN_NO_ACTIVE_TASK";
pub const MASTER_BRAIN_TIMEOUT: &str = "MASTER_BRAIN_TIMEOUT";
pub const MASTER_BRAIN_OFFLINE: &str = "MASTER_BRAIN_OFFLINE";
pub const MASTER_BRAIN_HTTP: &str = "MASTER_BRAIN_HTTP";
pub const MASTER_BRAIN_PARSE: &str = "MASTER_BRAIN_PARSE";

const ZH_DENY_PHRASES: &[&str] = &[
    "不好",
    "不行",
    "不继续",
    "不批准",
    "不同意",
    "不确认",
    "不允许",
    "不可以",
    "不执行",
    "不用",
    "算了",
    "取消",
    "拒绝",
    "不要",
    "别",
    "停",
];
const EN_DENY_WORDS: &[&str] = &["deny", "no", "reject", "cancel", "stop", "abort"];

const ZH_APPROVE_PHRASES: &[&str] = &[
    "确认", "同意", "批准", "好的", "可以", "继续", "执行", "行", "好",
];
const EN_APPROVE_WORDS: &[&str] = &["yes", "approve", "ok", "okay", "once", "confirm"];
const EN_APPROVE_PHRASES: &[&str] = &["go ahead"];

#[derive(Debug, Clone, Serialize)]
pub struct DelegatedTaskRecord {
    pub task_id: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

fn valid_task_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && !raw.contains("..")
        && !raw
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#') || c.is_whitespace() || c.is_control())
}

fn clip_str(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    let mut out: String = trimmed.chars().take(max_chars).collect();
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Classify a spoken guidance reply to a pending approval prompt into Hermes's
/// `"once"` or `"deny"` choice, or `None` if it is general steering text.
///
/// Always evaluates denial phrases BEFORE approval phrases so Chinese negations
/// like `"不好"` or `"不批准"` never match `"好"` / `"批准"`.
pub fn classify_approval_choice(guidance: &str) -> Option<&'static str> {
    let lower = guidance.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    if ZH_DENY_PHRASES.iter().any(|p| lower.contains(p))
        || EN_DENY_WORDS.iter().any(|w| words.contains(w))
    {
        return Some("deny");
    }
    if ZH_APPROVE_PHRASES.iter().any(|p| lower.contains(p))
        || EN_APPROVE_WORDS.iter().any(|w| words.contains(w))
        || EN_APPROVE_PHRASES.iter().any(|p| lower.contains(p))
    {
        return Some("once");
    }
    None
}

/// Strip Markdown formatting, code blocks, links, and raw URLs so a Master
/// Brain reply reads cleanly over TTS on a speaker.
pub fn clean_for_speech(text: &str, max_chars: usize) -> String {
    // 1. Strip fenced code blocks (``` ... ```).
    let mut no_fences = String::with_capacity(text.len());
    let mut in_fence = false;
    let mut rest = text;
    while let Some(pos) = rest.find("```") {
        if !in_fence {
            no_fences.push_str(&rest[..pos]);
            no_fences.push(' ');
        }
        in_fence = !in_fence;
        rest = &rest[pos + 3..];
    }
    if !in_fence {
        no_fences.push_str(rest);
    }

    // 2. Strip markdown links/images `[label](url)` / `![alt](url)` -> `label`/`alt`.
    let no_links = strip_markdown_links(&no_fences);

    // 3. Process line-by-line: drop heading prefixes, quote markers, list bullets,
    //    bare http(s) URLs, and inline markdown punctuation.
    let mut tokens: Vec<String> = Vec::new();
    for raw_line in no_links.lines() {
        let mut line = raw_line.trim();
        // Skip markdown table divider rows like `| --- | --- |`.
        if !line.is_empty()
            && line
                .chars()
                .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
        {
            continue;
        }
        line = line.trim_start_matches(['#', '>', ' ', '\t']);
        if let Some(stripped) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            line = stripped.trim_start();
        }
        for word in line.split_whitespace() {
            let trimmed_word =
                word.trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | '[' | ']'));
            if trimmed_word.starts_with("http://") || trimmed_word.starts_with("https://") {
                continue;
            }
            let cleaned: String = word
                .chars()
                .filter(|c| !matches!(c, '`' | '*' | '_' | '~' | '|'))
                .collect();
            if !cleaned.is_empty() {
                tokens.push(cleaned);
            }
        }
    }

    let joined = tokens.join(" ");
    let trimmed = joined.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let prefix: String = trimmed.chars().take(max_chars).collect();
    let half_chars = max_chars / 2;
    let mut best_end_byte: Option<usize> = None;
    for (idx, (byte_pos, ch)) in prefix.char_indices().enumerate() {
        if idx >= half_chars && matches!(ch, '。' | '！' | '？' | '.' | '!' | '?') {
            best_end_byte = Some(byte_pos + ch.len_utf8());
        }
    }
    if let Some(end) = best_end_byte {
        prefix[..end].trim().to_string()
    } else {
        format!("{}…", prefix.trim_end())
    }
}

fn strip_markdown_links(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let is_img = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let is_link = bytes[i] == b'[';
        if is_img || is_link {
            let label_start = if is_img { i + 2 } else { i + 1 };
            if let Some(rel_close) = input[label_start..].find("](") {
                let label_end = label_start + rel_close;
                let url_start = label_end + 2;
                if let Some(rel_paren) = input[url_start..].find(')') {
                    let label = &input[label_start..label_end];
                    if !label.contains('\n') {
                        out.push_str(label);
                        i = url_start + rel_paren + 1;
                        continue;
                    }
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn build_hermes_prompt(
    task: &str,
    context: Option<&str>,
    device: Option<&str>,
    room: Option<&str>,
) -> String {
    let dev_label = match (room, device) {
        (Some(r), Some(d)) => format!("{r}（{d}）"),
        (Some(r), None) => r.to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) => "家庭音箱".to_string(),
    };
    let mut lines = vec![
        "[Home Mate 语音任务委派]".to_string(),
        format!("来自设备：{dev_label}"),
        format!("用户请求：{task}"),
    ];
    if let Some(ctx) = context.filter(|s| !s.trim().is_empty()) {
        lines.push(format!("对话上下文：{ctx}"));
    }
    lines.push(
        "回复要求：最终答复将直接由音箱朗读，请用自然简洁的中文口语回答（建议不超过150字），不要输出 Markdown 表格、代码块或原始 JSON。".to_string(),
    );
    lines.join("\n")
}

fn extract_str<'a>(val: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(s) = val.get(*k).and_then(Value::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

enum SseOutcome {
    Continue,
    Finished,
}

#[derive(Clone)]
pub struct MasterBrainRuntime {
    store: Arc<Store>,
    client: reqwest::Client,
    stream_client: reqwest::Client,
    voice: Arc<Mutex<Option<Weak<VoicePipeline>>>>,
    recent_tasks: Arc<Mutex<VecDeque<DelegatedTaskRecord>>>,
    cancel_notifiers: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>>,
}

impl MasterBrainRuntime {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        let client = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
        )
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
        let stream_client = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
        )
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            store,
            client,
            stream_client,
            voice: Arc::new(Mutex::new(None)),
            recent_tasks: Arc::new(Mutex::new(VecDeque::new())),
            cancel_notifiers: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub fn attach_voice(&self, voice: &Arc<VoicePipeline>) {
        let mut guard = crate::lock_or_recover(&self.voice, "master_brain.voice");
        *guard = Some(Arc::downgrade(voice));
    }

    fn voice_pipeline(&self) -> Option<Arc<VoicePipeline>> {
        crate::lock_or_recover(&self.voice, "master_brain.voice")
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub fn config(&self) -> (MasterBrainConfig, Option<String>) {
        let svc = ConfigService::new(self.store.clone());
        let cfg = svc.master_brain().unwrap_or_default();
        let tok = svc.master_brain_token().ok().flatten();
        (cfg, tok)
    }

    pub fn availability(&self) -> Availability {
        let (cfg, _) = self.config();
        if cfg.is_ready() {
            Availability::Ready
        } else if !cfg.url.is_empty() {
            Availability::Disabled
        } else {
            Availability::Unconfigured
        }
    }

    pub fn recent_tasks(&self) -> Vec<DelegatedTaskRecord> {
        crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks")
            .iter()
            .cloned()
            .collect()
    }

    fn record_task(&self, rec: DelegatedTaskRecord) {
        let mut guard = crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks");
        guard.retain(|r| r.task_id != rec.task_id);
        guard.push_front(rec);
        while guard.len() > MAX_RECENT_TASKS {
            guard.pop_back();
        }
    }

    fn cancel_notify_for(&self, task_id: &str) -> Arc<tokio::sync::Notify> {
        let mut map = crate::lock_or_recover(&self.cancel_notifiers, "master_brain.cancel");
        if map.len() > MAX_RECENT_TASKS * 2 {
            let live: std::collections::HashSet<String> =
                crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks")
                    .iter()
                    .map(|r| r.task_id.clone())
                    .collect();
            map.retain(|k, _| live.contains(k));
        }
        map.entry(task_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    fn clear_cancel_notify(&self, task_id: &str) {
        let mut map = crate::lock_or_recover(&self.cancel_notifiers, "master_brain.cancel");
        map.remove(task_id);
    }

    fn update_task_status(&self, task_id: &str, status: &str) {
        let now = id::now_ms();
        {
            let mut guard = crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks");
            if let Some(rec) = guard.iter_mut().find(|r| r.task_id == task_id) {
                rec.status = status.to_string();
                rec.updated_ms = now;
            }
        }
        if matches!(status, "cancelling" | "cancelled" | "stopped") {
            let notify = crate::lock_or_recover(&self.cancel_notifiers, "master_brain.cancel")
                .get(task_id)
                .cloned();
            if let Some(n) = notify {
                n.notify_waiters();
            }
        }
    }

    fn update_task_tool(&self, task_id: &str, tool_name: &str) {
        let now = id::now_ms();
        let mut guard = crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks");
        if let Some(rec) = guard.iter_mut().find(|r| r.task_id == task_id) {
            rec.last_tool = Some(clip_str(tool_name, 64));
            rec.updated_ms = now;
        }
    }

    fn set_pending_approval(&self, task_id: &str, prompt: Option<String>) {
        let now = id::now_ms();
        let mut guard = crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks");
        if let Some(rec) = guard.iter_mut().find(|r| r.task_id == task_id) {
            rec.pending_approval = prompt;
            rec.updated_ms = now;
        }
    }

    fn task_snapshot(&self, task_id: &str) -> Option<DelegatedTaskRecord> {
        crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks")
            .iter()
            .find(|r| r.task_id == task_id)
            .cloned()
    }

    fn is_cancelled_locally(&self, task_id: &str) -> bool {
        self.task_snapshot(task_id)
            .is_some_and(|r| matches!(r.status.as_str(), "cancelling" | "cancelled" | "stopped"))
    }

    fn latest_task_id(&self, device: Option<&str>) -> Option<String> {
        let guard = crate::lock_or_recover(&self.recent_tasks, "master_brain.recent_tasks");
        if let Some(dev) = device {
            if let Some(rec) = guard.iter().find(|r| r.device.as_deref() == Some(dev)) {
                return Some(rec.task_id.clone());
            }
        }
        guard.front().map(|r| r.task_id.clone())
    }

    fn room_for_device(&self, device: Option<&str>) -> Option<String> {
        let dev = device?;
        let svc = ConfigService::new(self.store.clone());
        svc.device_meta()
            .ok()
            .and_then(|m| m.get(dev).map(|meta| meta.room.clone()))
            .filter(|r| !r.trim().is_empty())
    }

    pub fn status_json(&self) -> Value {
        let (cfg, tok) = self.config();
        json!({
            "enabled": cfg.enabled,
            "protocol": cfg.protocol,
            "url": cfg.url,
            "callback_url": cfg.callback_url,
            "timeout_ms": cfg.timeout_ms,
            "bearer_token_set": tok.is_some(),
            "state": cfg.state_word(),
            "recent_tasks": self.recent_tasks(),
        })
    }

    fn require_ready(&self) -> Result<(MasterBrainConfig, Option<String>), Box<ToolResult>> {
        let (cfg, tok) = self.config();
        if cfg.url.is_empty() {
            return Err(Box::new(ToolResult::failed(
                MASTER_BRAIN_UNCONFIGURED,
                "主脑服务尚未配置，请在控制台「工具」页设置主脑地址并启用。",
                false,
            )));
        }
        if !cfg.enabled {
            return Err(Box::new(ToolResult::failed(
                MASTER_BRAIN_DISABLED,
                "主脑服务当前已停用，请在控制台启用后再试。",
                false,
            )));
        }
        Ok((cfg, tok))
    }

    async fn get_json(
        &self,
        cfg: &MasterBrainConfig,
        token: Option<&str>,
        path: &str,
    ) -> Result<Value, (&'static str, String, bool)> {
        let url = format!("{}{}", cfg.url.trim_end_matches('/'), path);
        validate_news_url(&url, self.store.allows_loopback_public())
            .map_err(|e| (MASTER_BRAIN_BAD_ARGS, format!("invalid url: {e}"), false))?;
        let mut req = self
            .client
            .get(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms));
        if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
            req = req.bearer_auth(tok.trim());
        }
        let resp = req.send().await.map_err(classify_reqwest_err)?;
        decode_response(resp).await
    }

    async fn post_json(
        &self,
        cfg: &MasterBrainConfig,
        token: Option<&str>,
        path: &str,
        body: Value,
    ) -> Result<Value, (&'static str, String, bool)> {
        let url = format!("{}{}", cfg.url.trim_end_matches('/'), path);
        validate_news_url(&url, self.store.allows_loopback_public())
            .map_err(|e| (MASTER_BRAIN_BAD_ARGS, format!("invalid url: {e}"), false))?;
        let mut req = self
            .client
            .post(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .json(&body);
        if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
            req = req.bearer_auth(tok.trim());
        }
        let resp = req.send().await.map_err(classify_reqwest_err)?;
        decode_response(resp).await
    }

    pub async fn test_connection(&self) -> Value {
        let (cfg, tok) = self.config();
        if cfg.url.is_empty() {
            return json!({
                "ok": false,
                "protocol": cfg.protocol,
                "state": "unconfigured",
                "detail": "master_brain.url is not configured",
            });
        }
        let t0 = Instant::now();
        let health = match self.get_json(&cfg, tok.as_deref(), "/health").await {
            Ok(h) => h,
            Err((code, detail, _)) => {
                return json!({
                    "ok": false,
                    "protocol": cfg.protocol,
                    "state": "error",
                    "code": code,
                    "latency_ms": t0.elapsed().as_millis() as u64,
                    "detail": detail,
                });
            }
        };
        if cfg.protocol == MasterBrainProtocol::Hermes {
            // Hermes `/health` is unauthenticated; probe `/v1/models` to verify
            // the configured Bearer API key honestly.
            if let Err((code, detail, _)) = self.get_json(&cfg, tok.as_deref(), "/v1/models").await
            {
                return json!({
                    "ok": false,
                    "protocol": cfg.protocol,
                    "state": "error",
                    "code": code,
                    "latency_ms": t0.elapsed().as_millis() as u64,
                    "detail": detail,
                    "health": health,
                });
            }
        }
        json!({
            "ok": true,
            "protocol": cfg.protocol,
            "state": cfg.state_word(),
            "latency_ms": t0.elapsed().as_millis() as u64,
            "health": health,
        })
    }

    fn announce_to_voice(
        &self,
        device: Option<String>,
        text: String,
        task_id: &str,
        priority: &str,
        open_followup: bool,
    ) {
        let Some(voice) = self.voice_pipeline() else {
            tracing::warn!(
                task_id = %task_id,
                "master_brain: voice pipeline not attached; dropping spoken announcement"
            );
            return;
        };
        let req = AnnounceRequest {
            text,
            title: None,
            device,
            priority: Some(priority.to_string()),
            open_followup,
            task_id: Some(task_id.to_string()),
            source: Some("master_brain".to_string()),
            ttl_ms: None,
        };
        if let Err(err) = voice.enqueue_announce(req) {
            tracing::warn!(
                task_id = %task_id,
                code = %err.code,
                detail = %err.detail,
                "master_brain: failed to enqueue voice announcement"
            );
        }
    }

    /// Background watcher for direct Hermes mode (`protocol == Hermes`):
    /// consumes SSE `/v1/runs/{run_id}/events` with automatic fallback polling
    /// on `/v1/runs/{run_id}`, and pushes approval prompts and final summaries
    /// straight into `VoicePipeline::enqueue_announce` (zero external bridge).
    async fn watch_hermes_run(
        self: Arc<Self>,
        cfg: MasterBrainConfig,
        token: Option<String>,
        run_id: String,
        device: Option<String>,
        want_followup: bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(MAX_HERMES_WATCH_SECS);
        let sse_done = self
            .consume_hermes_sse(
                &cfg,
                token.as_deref(),
                &run_id,
                device.clone(),
                want_followup,
                deadline,
            )
            .await;
        if sse_done {
            self.clear_cancel_notify(&run_id);
            return;
        }
        self.poll_hermes_until_terminal(
            &cfg,
            token.as_deref(),
            &run_id,
            device,
            want_followup,
            deadline,
        )
        .await;
        self.clear_cancel_notify(&run_id);
    }

    async fn consume_hermes_sse(
        &self,
        cfg: &MasterBrainConfig,
        token: Option<&str>,
        run_id: &str,
        device: Option<String>,
        want_followup: bool,
        deadline: Instant,
    ) -> bool {
        let url = format!("{}/v1/runs/{run_id}/events", cfg.url.trim_end_matches('/'));
        if validate_news_url(&url, self.store.allows_loopback_public()).is_err() {
            return false;
        }
        let mut req = self
            .stream_client
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
            req = req.bearer_auth(tok.trim());
        }
        let cancel_notify = self.cancel_notify_for(run_id);
        let header_timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(15));
        let send_notified = cancel_notify.notified();
        tokio::pin!(send_notified);
        send_notified.as_mut().enable();
        if header_timeout.is_zero() || self.is_cancelled_locally(run_id) {
            return self.is_cancelled_locally(run_id);
        }
        let send_res = tokio::select! {
            _ = &mut send_notified => return true,
            res = tokio::time::timeout(header_timeout, req.send()) => res,
        };
        let resp = match send_res {
            Ok(Ok(r)) if r.status().is_success() => r,
            Ok(Ok(r)) => {
                tracing::debug!(
                    run_id = %run_id,
                    status = %r.status(),
                    "master_brain: Hermes SSE unavailable, falling back to poll"
                );
                return false;
            }
            Ok(Err(e)) => {
                tracing::debug!(
                    run_id = %run_id,
                    error = %e,
                    "master_brain: Hermes SSE connect failed, falling back to poll"
                );
                return false;
            }
            Err(_) => {
                tracing::debug!(
                    run_id = %run_id,
                    "master_brain: Hermes SSE response header timeout, falling back to poll"
                );
                return false;
            }
        };

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut cur_event = String::new();
        let mut cur_data: Vec<String> = Vec::new();

        loop {
            let chunk_notified = cancel_notify.notified();
            tokio::pin!(chunk_notified);
            chunk_notified.as_mut().enable();
            if self.is_cancelled_locally(run_id) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let next_item = tokio::select! {
                _ = &mut chunk_notified => return true,
                item = tokio::time::timeout(remaining, stream.next()) => item,
            };
            let chunk = match next_item {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    tracing::debug!(run_id = %run_id, error = %e, "master_brain: SSE stream read error");
                    break;
                }
                Ok(None) | Err(_) => break,
            };
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_SSE_LINE_BYTES * 4 {
                buf.clear();
                break;
            }
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let mut line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                while matches!(line_bytes.last(), Some(b'\n' | b'\r')) {
                    line_bytes.pop();
                }
                let line = String::from_utf8_lossy(&line_bytes);
                if line.is_empty() {
                    if !cur_data.is_empty() {
                        let raw_data = cur_data.join("\n");
                        let ev_name = std::mem::take(&mut cur_event);
                        cur_data.clear();
                        if matches!(
                            self.handle_hermes_event(
                                run_id,
                                &ev_name,
                                &raw_data,
                                device.clone(),
                                want_followup,
                            ),
                            SseOutcome::Finished
                        ) {
                            return true;
                        }
                    } else {
                        cur_event.clear();
                    }
                    continue;
                }
                if line.starts_with(':') {
                    continue;
                }
                if let Some(ev) = line.strip_prefix("event:") {
                    cur_event = ev.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    cur_data.push(d.trim_start().to_string());
                }
            }
        }
        // Flush trailing event if the server closed without a final blank line.
        if !cur_data.is_empty() {
            let raw_data = cur_data.join("\n");
            if matches!(
                self.handle_hermes_event(run_id, &cur_event, &raw_data, device, want_followup),
                SseOutcome::Finished
            ) {
                return true;
            }
        }
        false
    }

    fn handle_hermes_event(
        &self,
        run_id: &str,
        sse_event: &str,
        raw_data: &str,
        device: Option<String>,
        want_followup: bool,
    ) -> SseOutcome {
        if self.is_cancelled_locally(run_id) {
            self.set_pending_approval(run_id, None);
            return SseOutcome::Finished;
        }
        let data: Value =
            serde_json::from_str(raw_data).unwrap_or_else(|_| json!({ "text": raw_data }));
        let ev_kind = if !sse_event.is_empty() {
            sse_event
        } else {
            extract_str(&data, &["event", "type"]).unwrap_or("")
        }
        .to_ascii_lowercase();
        let status = extract_str(&data, &["status"])
            .unwrap_or("")
            .to_ascii_lowercase();

        if ev_kind.contains("tool") {
            if let Some(tool_name) = extract_str(&data, &["tool", "tool_name", "name"]) {
                self.update_task_tool(run_id, tool_name);
            }
            return SseOutcome::Continue;
        }

        let is_approval_prompt = (ev_kind.contains("approval")
            && !ev_kind.contains("resolved")
            && !ev_kind.contains("granted")
            && !ev_kind.contains("denied")
            && !ev_kind.contains("approved"))
            || status == "awaiting_approval";
        if is_approval_prompt {
            let prompt = extract_str(&data, &["prompt", "command", "description", "message"])
                .unwrap_or("执行一项高权限操作");
            let cleaned = clean_for_speech(prompt, 120);
            self.set_pending_approval(run_id, Some(cleaned.clone()));
            self.update_task_status(run_id, "awaiting_approval");
            self.announce_to_voice(
                device,
                format!("主脑需要你确认操作：{cleaned}。请说确认执行或取消。"),
                run_id,
                "high",
                true,
            );
            return SseOutcome::Continue;
        }

        if matches!(
            ev_kind.as_str(),
            "run.completed" | "completed" | "done" | "finished"
        ) || status == "completed"
        {
            self.set_pending_approval(run_id, None);
            if self.is_cancelled_locally(run_id) {
                return SseOutcome::Finished;
            }
            self.update_task_status(run_id, "completed");
            let output =
                extract_str(&data, &["output", "response", "text", "result"]).unwrap_or("");
            let cleaned = clean_for_speech(output, MAX_SPEECH_CHARS);
            let spoken = if cleaned.is_empty() {
                "主脑已经完成刚才的任务。".to_string()
            } else {
                cleaned
            };
            self.announce_to_voice(device, spoken, run_id, "normal", want_followup);
            return SseOutcome::Finished;
        }

        if matches!(ev_kind.as_str(), "run.failed" | "failed" | "error") || status == "failed" {
            self.set_pending_approval(run_id, None);
            if self.is_cancelled_locally(run_id) {
                return SseOutcome::Finished;
            }
            self.update_task_status(run_id, "failed");
            let err = extract_str(&data, &["error", "message", "detail"]).unwrap_or("未知错误");
            let cleaned = clean_for_speech(err, 140);
            self.announce_to_voice(
                device,
                format!("主脑处理刚才的任务时遇到问题：{cleaned}"),
                run_id,
                "normal",
                false,
            );
            return SseOutcome::Finished;
        }

        if matches!(
            ev_kind.as_str(),
            "run.cancelled" | "run.stopped" | "cancelled" | "stopped"
        ) || matches!(status.as_str(), "cancelled" | "stopped")
        {
            self.set_pending_approval(run_id, None);
            self.update_task_status(run_id, "cancelled");
            return SseOutcome::Finished;
        }

        SseOutcome::Continue
    }

    async fn poll_hermes_until_terminal(
        &self,
        cfg: &MasterBrainConfig,
        token: Option<&str>,
        run_id: &str,
        device: Option<String>,
        want_followup: bool,
        deadline: Instant,
    ) {
        let cancel_notify = self.cancel_notify_for(run_id);
        let path = format!("/v1/runs/{run_id}");
        while Instant::now() < deadline {
            if self.is_cancelled_locally(run_id) {
                self.clear_cancel_notify(run_id);
                return;
            }
            match self.get_json(cfg, token, &path).await {
                Ok(body) => {
                    if self.is_cancelled_locally(run_id) {
                        self.clear_cancel_notify(run_id);
                        return;
                    }
                    let status = extract_str(&body, &["status"])
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if status == "awaiting_approval" {
                        let prompt = extract_str(
                            &body,
                            &["description", "prompt", "command", "message", "detail"],
                        )
                        .unwrap_or("执行一项需要确认的操作");
                        let cleaned = clean_for_speech(prompt, 120);
                        let already_prompted = self
                            .task_snapshot(run_id)
                            .and_then(|r| r.pending_approval)
                            .as_deref()
                            == Some(cleaned.as_str());
                        if !already_prompted {
                            self.set_pending_approval(run_id, Some(cleaned.clone()));
                            self.update_task_status(run_id, "awaiting_approval");
                            self.announce_to_voice(
                                device.clone(),
                                format!(
                                    "主脑需要你的确认：{cleaned}。同意请说批准，拒绝请说不要。"
                                ),
                                run_id,
                                "urgent",
                                true,
                            );
                        }
                    } else if status == "completed" {
                        self.set_pending_approval(run_id, None);
                        self.update_task_status(run_id, "completed");
                        self.clear_cancel_notify(run_id);
                        let output = extract_str(&body, &["output", "response", "text", "result"])
                            .unwrap_or("");
                        let cleaned = clean_for_speech(output, MAX_SPEECH_CHARS);
                        let spoken = if cleaned.is_empty() {
                            "主脑已经完成刚才的任务。".to_string()
                        } else {
                            cleaned
                        };
                        self.announce_to_voice(device, spoken, run_id, "normal", want_followup);
                        return;
                    } else if status == "failed" {
                        self.set_pending_approval(run_id, None);
                        self.clear_cancel_notify(run_id);
                        if self.is_cancelled_locally(run_id) {
                            return;
                        }
                        self.update_task_status(run_id, "failed");
                        let err = extract_str(&body, &["error", "message", "detail"])
                            .unwrap_or("未知错误");
                        let cleaned = clean_for_speech(err, 140);
                        self.announce_to_voice(
                            device,
                            format!("主脑处理刚才的任务时遇到问题：{cleaned}"),
                            run_id,
                            "normal",
                            false,
                        );
                        return;
                    } else if matches!(status.as_str(), "cancelled" | "stopped") {
                        self.set_pending_approval(run_id, None);
                        self.update_task_status(run_id, "cancelled");
                        self.clear_cancel_notify(run_id);
                        return;
                    }
                }
                Err((code, detail, retryable)) => {
                    let is_permanent_4xx = code == MASTER_BRAIN_HTTP
                        && (detail.contains("401")
                            || detail.contains("403")
                            || detail.contains("404"));
                    if !retryable || is_permanent_4xx {
                        self.set_pending_approval(run_id, None);
                        self.clear_cancel_notify(run_id);
                        if self.is_cancelled_locally(run_id) {
                            return;
                        }
                        self.update_task_status(run_id, "failed");
                        let cleaned = clean_for_speech(&detail, 140);
                        self.announce_to_voice(
                            device,
                            format!("主脑任务查询失败：{cleaned}"),
                            run_id,
                            "normal",
                            false,
                        );
                        return;
                    }
                }
            }
            let sleep_notified = cancel_notify.notified();
            tokio::pin!(sleep_notified);
            sleep_notified.as_mut().enable();
            if self.is_cancelled_locally(run_id) {
                self.clear_cancel_notify(run_id);
                return;
            }
            tokio::select! {
                _ = &mut sleep_notified => {
                    self.clear_cancel_notify(run_id);
                    return;
                }
                _ = tokio::time::sleep(Duration::from_millis(HERMES_POLL_INTERVAL_MS)) => {}
            }
        }
        self.clear_cancel_notify(run_id);
        if !self.is_cancelled_locally(run_id) {
            self.set_pending_approval(run_id, None);
            self.update_task_status(run_id, "timeout");
            self.announce_to_voice(
                device,
                "主脑后台任务运行超时，你可以稍后让我查一下进度。".to_string(),
                run_id,
                "normal",
                false,
            );
        }
    }
}

fn classify_reqwest_err(e: reqwest::Error) -> (&'static str, String, bool) {
    if e.is_timeout() {
        (MASTER_BRAIN_TIMEOUT, format!("主脑连接超时：{e}"), true)
    } else {
        (MASTER_BRAIN_OFFLINE, format!("主脑暂不可达：{e}"), true)
    }
}

async fn decode_response(resp: reqwest::Response) -> Result<Value, (&'static str, String, bool)> {
    let status = resp.status();
    let bytes = read_bounded(resp, MAX_JSON_BODY)
        .await
        .map_err(|e| (MASTER_BRAIN_PARSE, format!("响应读取失败：{e}"), true))?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&bytes);
        let retryable = status.is_server_error() || status.as_u16() == 429;
        return Err((
            MASTER_BRAIN_HTTP,
            format!(
                "主脑返回 HTTP {}：{}",
                status.as_u16(),
                clip_str(&preview, 160)
            ),
            retryable,
        ));
    }
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        (
            MASTER_BRAIN_PARSE,
            format!("主脑响应不是合法 JSON：{e}"),
            false,
        )
    })
}

// --- Tools -------------------------------------------------------------------

pub struct BrainDelegateTool {
    rt: Arc<MasterBrainRuntime>,
}

impl BrainDelegateTool {
    pub fn new(rt: Arc<MasterBrainRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for BrainDelegateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("brain.delegate"),
            service: Cow::Borrowed("master_brain"),
            side_effect: SideEffect::Mutate,
            description: "Delegate complex, multi-step, cross-device, remote-computer/NAS/file/code, or long-running tasks asynchronously to the household Master Brain (and ALWAYS delegate when the user explicitly asks the Master Brain to check or do something; never refuse on the Master Brain's behalf or guess its skills/permissions—let the Master Brain execute or answer honestly). Returns immediately while the Master Brain works in the background and calls back when done.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Clear natural-language description of the task for the Master Brain to execute."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional brief background context from the current conversation."
                    },
                    "want_followup": {
                        "type": "boolean",
                        "description": "Whether the speaker should open a short listening window after announcing the Master Brain's final result."
                    }
                },
                "required": ["task"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let (cfg, tok) = match self.rt.require_ready() {
            Ok(v) => v,
            Err(r) => return *r,
        };
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if task.is_empty() {
            return ToolResult::failed(MASTER_BRAIN_BAD_ARGS, "task 不能为空", false);
        }
        let task = clip_str(task, MAX_TASK_CHARS);
        let context = args
            .get("context")
            .and_then(Value::as_str)
            .map(|s| clip_str(s, MAX_CONTEXT_CHARS))
            .filter(|s| !s.is_empty());
        let want_followup = args
            .get("want_followup")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let device = ctx.device_id.clone();
        let room = self.rt.room_for_device(device.as_deref());

        let (path, payload) = match cfg.protocol {
            MasterBrainProtocol::Hermes => {
                let prompt = build_hermes_prompt(
                    &task,
                    context.as_deref(),
                    device.as_deref(),
                    room.as_deref(),
                );
                ("/v1/runs", json!({ "input": prompt }))
            }
            MasterBrainProtocol::Generic => {
                let callback_url =
                    (!cfg.callback_url.is_empty()).then_some(cfg.callback_url.clone());
                (
                    "/v1/tasks",
                    json!({
                        "task": task,
                        "context": context,
                        "device": device,
                        "room": room,
                        "callback_url": callback_url,
                        "want_followup": want_followup,
                        "trace_id": ctx.trace_id,
                    }),
                )
            }
        };

        match self.rt.post_json(&cfg, tok.as_deref(), path, payload).await {
            Ok(body) => {
                let task_id = extract_str(&body, &["task_id", "run_id", "id"]).map(str::to_string);
                let Some(task_id) = task_id else {
                    return ToolResult::failed(
                        MASTER_BRAIN_PARSE,
                        "主脑未返回 task_id，无法追踪后台任务",
                        false,
                    );
                };
                if !valid_task_id(&task_id) {
                    return ToolResult::failed(
                        MASTER_BRAIN_PARSE,
                        "主脑返回的 task_id 格式非法，已拒绝追踪",
                        false,
                    );
                }
                let status = extract_str(&body, &["status"])
                    .unwrap_or(match cfg.protocol {
                        MasterBrainProtocol::Hermes => "running",
                        MasterBrainProtocol::Generic => "queued",
                    })
                    .to_string();
                let now = id::now_ms();
                self.rt.record_task(DelegatedTaskRecord {
                    task_id: task_id.clone(),
                    summary: clip_str(&task, 80),
                    device: device.clone(),
                    room: room.clone(),
                    status: status.clone(),
                    last_tool: None,
                    pending_approval: None,
                    created_ms: now,
                    updated_ms: now,
                });

                if cfg.protocol == MasterBrainProtocol::Hermes {
                    let rt_watch = self.rt.clone();
                    let cfg_watch = cfg.clone();
                    let tok_watch = tok.clone();
                    let run_id_watch = task_id.clone();
                    let dev_watch = device.clone();
                    tokio::spawn(async move {
                        rt_watch
                            .watch_hermes_run(
                                cfg_watch,
                                tok_watch,
                                run_id_watch,
                                dev_watch,
                                want_followup,
                            )
                            .await;
                    });
                }

                let ack = extract_str(&body, &["ack"])
                    .map(|s| clip_str(s, MAX_ACK_CHARS))
                    .unwrap_or_else(|| "好的，这件事我交给主脑在后台处理，好了我叫你。".into());
                ToolResult::observed_ok(json!({
                    "task_id": task_id,
                    "status": status,
                    "protocol": cfg.protocol,
                    "device": device,
                    "room": room,
                    "rendered_answer": ack,
                    "replace_answer": true,
                }))
            }
            Err((code, detail, retryable)) => ToolResult::failed(code, detail, retryable),
        }
    }
}

pub struct BrainStatusTool {
    rt: Arc<MasterBrainRuntime>,
}

impl BrainStatusTool {
    pub fn new(rt: Arc<MasterBrainRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for BrainStatusTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("brain.status"),
            service: Cow::Borrowed("master_brain"),
            side_effect: SideEffect::Query,
            description: "Check the progress or final result of an asynchronous Master Brain task. If task_id is omitted, checks the latest task delegated from this speaker.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Optional task_id returned by brain.delegate. Omit to inspect the latest delegated task."
                    }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let (cfg, tok) = match self.rt.require_ready() {
            Ok(v) => v,
            Err(r) => return *r,
        };
        let requested = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let task_id = match requested.or_else(|| self.rt.latest_task_id(ctx.device_id.as_deref())) {
            Some(id) => id,
            None => {
                return ToolResult::failed(
                    MASTER_BRAIN_NO_ACTIVE_TASK,
                    "目前没有正在追踪的主脑后台任务。",
                    false,
                );
            }
        };
        if !valid_task_id(&task_id) {
            return ToolResult::failed(MASTER_BRAIN_BAD_ARGS, "task_id 格式不合法", false);
        }
        let path = match cfg.protocol {
            MasterBrainProtocol::Hermes => format!("/v1/runs/{task_id}"),
            MasterBrainProtocol::Generic => format!("/v1/tasks/{task_id}"),
        };
        match self.rt.get_json(&cfg, tok.as_deref(), &path).await {
            Ok(mut body) => {
                if let Some(st) = body["status"].as_str() {
                    self.rt.update_task_status(&task_id, st);
                }
                if cfg.protocol == MasterBrainProtocol::Hermes {
                    if let Some(obj) = body.as_object_mut() {
                        obj.entry("task_id".to_string())
                            .or_insert_with(|| json!(task_id));
                        if let Some(rec) = self.rt.task_snapshot(&task_id) {
                            if let Some(lt) = rec.last_tool {
                                obj.entry("last_tool".to_string())
                                    .or_insert_with(|| json!(lt));
                            }
                            if let Some(pa) = rec.pending_approval {
                                obj.entry("pending_approval".to_string())
                                    .or_insert_with(|| json!(pa));
                            }
                        }
                    }
                }
                ToolResult::observed_ok(body)
            }
            Err((code, detail, retryable)) => ToolResult::failed(code, detail, retryable),
        }
    }
}

pub struct BrainSteerTool {
    rt: Arc<MasterBrainRuntime>,
}

impl BrainSteerTool {
    pub fn new(rt: Arc<MasterBrainRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for BrainSteerTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("brain.steer"),
            service: Cow::Borrowed("master_brain"),
            side_effect: SideEffect::Mutate,
            description: "Send follow-up guidance, clarification, or approval to a running Master Brain task.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "guidance": {
                        "type": "string",
                        "description": "Additional instructions, answer, or approval decision to forward to the Master Brain."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Optional task_id; defaults to the latest task delegated from this speaker."
                    }
                },
                "required": ["guidance"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let (cfg, tok) = match self.rt.require_ready() {
            Ok(v) => v,
            Err(r) => return *r,
        };
        let guidance = args
            .get("guidance")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if guidance.is_empty() {
            return ToolResult::failed(MASTER_BRAIN_BAD_ARGS, "guidance 不能为空", false);
        }
        let guidance = clip_str(guidance, MAX_GUIDANCE_CHARS);
        let requested = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let task_id = match requested.or_else(|| self.rt.latest_task_id(ctx.device_id.as_deref())) {
            Some(id) => id,
            None => {
                return ToolResult::failed(
                    MASTER_BRAIN_NO_ACTIVE_TASK,
                    "目前没有正在运行的主脑任务可补充指示。",
                    false,
                );
            }
        };
        if !valid_task_id(&task_id) {
            return ToolResult::failed(MASTER_BRAIN_BAD_ARGS, "task_id 格式不合法", false);
        }

        match cfg.protocol {
            MasterBrainProtocol::Hermes => {
                let pending = self
                    .rt
                    .task_snapshot(&task_id)
                    .and_then(|r| r.pending_approval);
                if pending.is_some() {
                    if let Some(choice) = classify_approval_choice(&guidance) {
                        let path = format!("/v1/runs/{task_id}/approval");
                        return match self
                            .rt
                            .post_json(
                                &cfg,
                                tok.as_deref(),
                                &path,
                                json!({ "choice": choice, "reason": guidance }),
                            )
                            .await
                        {
                            Ok(body) => {
                                self.rt.set_pending_approval(&task_id, None);
                                self.rt.update_task_status(
                                    &task_id,
                                    if choice == "once" {
                                        "running"
                                    } else {
                                        "denied"
                                    },
                                );
                                let ack = if choice == "once" {
                                    "好的，已经批准主脑继续执行。"
                                } else {
                                    "好的，已经通知主脑拒绝这次操作。"
                                };
                                ToolResult::observed_ok(json!({
                                    "task_id": task_id,
                                    "choice": choice,
                                    "response": body,
                                    "rendered_answer": ack,
                                    "replace_answer": true,
                                }))
                            }
                            Err((code, detail, retryable)) => {
                                ToolResult::failed(code, detail, retryable)
                            }
                        };
                    }
                }
                let path = format!("/v1/runs/{task_id}/steer");
                match self
                    .rt
                    .post_json(&cfg, tok.as_deref(), &path, json!({ "message": guidance }))
                    .await
                {
                    Ok(body) => ToolResult::observed_ok(json!({
                        "task_id": task_id,
                        "response": body,
                        "rendered_answer": "好的，已经把你的补充要求转达给主脑了。",
                        "replace_answer": true,
                    })),
                    Err((code, detail, retryable)) => ToolResult::failed(code, detail, retryable),
                }
            }
            MasterBrainProtocol::Generic => {
                let path = format!("/v1/tasks/{task_id}/steer");
                match self
                    .rt
                    .post_json(
                        &cfg,
                        tok.as_deref(),
                        &path,
                        json!({
                            "guidance": guidance,
                            "device": ctx.device_id,
                        }),
                    )
                    .await
                {
                    Ok(body) => {
                        let ack = extract_str(&body, &["ack"])
                            .map(|s| clip_str(s, MAX_ACK_CHARS))
                            .unwrap_or_else(|| "好的，已经把你的补充要求转达给主脑了。".into());
                        ToolResult::observed_ok(json!({
                            "task_id": task_id,
                            "response": body,
                            "rendered_answer": ack,
                            "replace_answer": true,
                        }))
                    }
                    Err((code, detail, retryable)) => ToolResult::failed(code, detail, retryable),
                }
            }
        }
    }
}

pub struct BrainCancelTool {
    rt: Arc<MasterBrainRuntime>,
}

impl BrainCancelTool {
    pub fn new(rt: Arc<MasterBrainRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for BrainCancelTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("brain.cancel"),
            service: Cow::Borrowed("master_brain"),
            side_effect: SideEffect::Mutate,
            description: "Cancel a running asynchronous Master Brain task.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Optional task_id; defaults to the latest task delegated from this speaker."
                    }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let (cfg, tok) = match self.rt.require_ready() {
            Ok(v) => v,
            Err(r) => return *r,
        };
        let requested = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let task_id = match requested.or_else(|| self.rt.latest_task_id(ctx.device_id.as_deref())) {
            Some(id) => id,
            None => {
                return ToolResult::failed(
                    MASTER_BRAIN_NO_ACTIVE_TASK,
                    "目前没有正在运行的主脑任务可取消。",
                    false,
                );
            }
        };
        if !valid_task_id(&task_id) {
            return ToolResult::failed(MASTER_BRAIN_BAD_ARGS, "task_id 格式不合法", false);
        }

        let (path, payload) = match cfg.protocol {
            MasterBrainProtocol::Hermes => (format!("/v1/runs/{task_id}/stop"), json!({})),
            MasterBrainProtocol::Generic => (
                format!("/v1/tasks/{task_id}/cancel"),
                json!({ "device": ctx.device_id }),
            ),
        };
        // Mark cancelling BEFORE the network call so an SSE `run.completed` /
        // `run.failed` emitted synchronously during `/stop` cannot slip through
        // and trigger a spurious voice announcement. Restore if `/stop` fails.
        let prev_status = self
            .rt
            .task_snapshot(&task_id)
            .map(|r| r.status)
            .unwrap_or_else(|| "running".to_string());
        if cfg.protocol == MasterBrainProtocol::Hermes {
            self.rt.update_task_status(&task_id, "cancelling");
        }
        match self
            .rt
            .post_json(&cfg, tok.as_deref(), &path, payload)
            .await
        {
            Ok(body) => {
                self.rt.set_pending_approval(&task_id, None);
                self.rt.update_task_status(&task_id, "cancelled");
                let ack = extract_str(&body, &["ack"])
                    .map(|s| clip_str(s, MAX_ACK_CHARS))
                    .unwrap_or_else(|| "好的，已经通知主脑取消这个任务。".into());
                ToolResult::observed_ok(json!({
                    "task_id": task_id,
                    "response": body,
                    "rendered_answer": ack,
                    "replace_answer": true,
                }))
            }
            Err((code, detail, retryable)) => {
                if cfg.protocol == MasterBrainProtocol::Hermes {
                    self.rt.update_task_status(&task_id, &prev_status);
                }
                ToolResult::failed(code, detail, retryable)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn clean_for_speech_strips_markdown_code_and_urls() {
        let raw = "# 巡检结果\n- **客厅音箱**：`online` [详情](https://example.local/d)\n```json\n{\"ok\":true}\n```\n参考 https://example.local/log ，全部正常。";
        let out = clean_for_speech(raw, 280);
        assert!(!out.contains("```"));
        assert!(!out.contains("https://"));
        assert!(!out.contains("**"));
        assert!(!out.contains('`'));
        assert!(out.contains("巡检结果"));
        assert!(out.contains("客厅音箱"));
        assert!(out.contains("详情"));
        assert!(out.contains("全部正常。"));
    }

    #[test]
    fn approval_classifier_checks_negation_before_approval() {
        assert_eq!(classify_approval_choice("不好"), Some("deny"));
        assert_eq!(classify_approval_choice("不行"), Some("deny"));
        assert_eq!(classify_approval_choice("不继续"), Some("deny"));
        assert_eq!(classify_approval_choice("不用了"), Some("deny"));
        assert_eq!(classify_approval_choice("算了"), Some("deny"));
        assert_eq!(classify_approval_choice("不批准"), Some("deny"));
        assert_eq!(classify_approval_choice("不同意执行"), Some("deny"));
        assert_eq!(classify_approval_choice("取消吧"), Some("deny"));
        assert_eq!(classify_approval_choice("no thanks"), Some("deny"));
        assert_eq!(classify_approval_choice("确认执行"), Some("once"));
        assert_eq!(classify_approval_choice("好的继续"), Some("once"));
        assert_eq!(classify_approval_choice("行"), Some("once"));
        assert_eq!(classify_approval_choice("approve once"), Some("once"));
        assert_eq!(classify_approval_choice("go ahead"), Some("once"));
        // Normal guidance containing "no" inside "note" or "ok" inside "token" must not misclassify.
        assert_eq!(classify_approval_choice("check the token note"), None);
        assert_eq!(classify_approval_choice("顺便看看桌面的 PDF"), None);
    }

    #[derive(Default)]
    struct MockBridgeState {
        last_auth: Mutex<Option<String>>,
        last_delegate: Mutex<Option<Value>>,
        last_steer: Mutex<Option<Value>>,
        cancelled: Mutex<Vec<String>>,
        status_code: Mutex<u16>,
    }

    async fn spawn_mock_bridge(state: Arc<MockBridgeState>) -> String {
        let app = Router::new()
            .route(
                "/health",
                get(|| async { Json(json!({ "status": "ok", "platform": "mock-bridge" })) }),
            )
            .route(
                "/v1/tasks",
                post(
                    |State(st): State<Arc<MockBridgeState>>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        *st.last_auth.lock().unwrap() = auth;
                        *st.last_delegate.lock().unwrap() = Some(body);
                        let code = *st.status_code.lock().unwrap();
                        if code >= 400 {
                            return (
                                StatusCode::from_u16(code).unwrap(),
                                Json(json!({ "error": "injected failure" })),
                            );
                        }
                        (
                            StatusCode::ACCEPTED,
                            Json(json!({
                                "task_id": "run_test_01",
                                "status": "queued",
                                "ack": "收到，主脑已在后台排查。"
                            })),
                        )
                    },
                ),
            )
            .route(
                "/v1/tasks/{id}",
                get(|Path(id): Path<String>| async move {
                    Json(json!({
                        "task_id": id,
                        "status": "running",
                        "progress": "checking dot2-d voice events"
                    }))
                }),
            )
            .route(
                "/v1/tasks/{id}/steer",
                post(
                    |State(st): State<Arc<MockBridgeState>>,
                     Path(id): Path<String>,
                     Json(body): Json<Value>| async move {
                        *st.last_steer.lock().unwrap() = Some(body);
                        Json(json!({ "task_id": id, "steered": true }))
                    },
                ),
            )
            .route(
                "/v1/tasks/{id}/cancel",
                post(
                    |State(st): State<Arc<MockBridgeState>>, Path(id): Path<String>| async move {
                        st.cancelled.lock().unwrap().push(id.clone());
                        Json(json!({ "task_id": id, "status": "cancelled" }))
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn generic_protocol_lifecycle_and_honest_failure() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let rt = MasterBrainRuntime::new(store.clone());
        let ctx = ToolCtx::new("T1", "C1", Arc::new(AtomicBool::new(false)))
            .with_provenance(Some("dot2-d".into()), Some("TR1".into()));

        // Unconfigured refuses honestly without HTTP.
        let delegate = BrainDelegateTool::new(rt.clone());
        assert_eq!(delegate.availability(), Availability::Unconfigured);
        let res = delegate
            .execute(json!({ "task": "查全屋音箱状态" }), &ctx)
            .await;
        assert_eq!(res.status, crate::tool::ToolStatus::Failed);
        assert_eq!(res.error.as_ref().unwrap().code, MASTER_BRAIN_UNCONFIGURED);

        let bridge_state = Arc::new(MockBridgeState::default());
        let url = spawn_mock_bridge(bridge_state.clone()).await;
        let cfg_svc = ConfigService::new(store.clone());
        cfg_svc
            .save_device_meta("dot2-d", "书房音箱", "书房", "test")
            .unwrap();
        cfg_svc
            .save_master_brain(
                MasterBrainConfig {
                    enabled: true,
                    protocol: MasterBrainProtocol::Generic,
                    url: url.clone(),
                    callback_url: "http://127.0.0.1:8930".into(),
                    timeout_ms: 4_000,
                },
                Some("bridge-secret-token"),
                "test",
            )
            .unwrap();

        assert_eq!(delegate.availability(), Availability::Ready);
        let probe = rt.test_connection().await;
        assert_eq!(probe["ok"], true);
        assert_eq!(probe["protocol"], "generic");

        let res = delegate
            .execute(
                json!({
                    "task": "排查为什么客厅音箱刚才没响应",
                    "want_followup": true
                }),
                &ctx,
            )
            .await;
        assert_eq!(res.status, crate::tool::ToolStatus::Completed);
        let body = res.result.unwrap();
        assert_eq!(body["task_id"], "run_test_01");
        assert_eq!(body["replace_answer"], true);
        assert_eq!(body["rendered_answer"], "收到，主脑已在后台排查。");
        assert_eq!(
            bridge_state.last_auth.lock().unwrap().as_deref(),
            Some("Bearer bridge-secret-token")
        );
        let sent = bridge_state.last_delegate.lock().unwrap().clone().unwrap();
        assert_eq!(sent["device"], "dot2-d");
        assert_eq!(sent["room"], "书房");
        assert_eq!(sent["callback_url"], "http://127.0.0.1:8930");
        assert_eq!(sent["want_followup"], true);

        let status_tool = BrainStatusTool::new(rt.clone());
        let st_res = status_tool.execute(json!({}), &ctx).await;
        assert_eq!(st_res.status, crate::tool::ToolStatus::Completed);
        assert_eq!(st_res.result.unwrap()["status"], "running");

        let steer_tool = BrainSteerTool::new(rt.clone());
        let steer_res = steer_tool
            .execute(json!({ "guidance": "先只看最近十分钟日志" }), &ctx)
            .await;
        assert_eq!(steer_res.status, crate::tool::ToolStatus::Completed);
        assert_eq!(
            bridge_state.last_steer.lock().unwrap().clone().unwrap()["guidance"],
            "先只看最近十分钟日志"
        );

        let cancel_tool = BrainCancelTool::new(rt.clone());
        let cancel_res = cancel_tool.execute(json!({}), &ctx).await;
        assert_eq!(cancel_res.status, crate::tool::ToolStatus::Completed);
        assert_eq!(
            bridge_state.cancelled.lock().unwrap().as_slice(),
            &["run_test_01"]
        );
        assert_eq!(rt.recent_tasks()[0].status, "cancelled");

        // HTTP 500 fails honestly (never claims queued).
        *bridge_state.status_code.lock().unwrap() = 500;
        let fail_res = delegate
            .execute(json!({ "task": "再次测试失败" }), &ctx)
            .await;
        assert_eq!(fail_res.status, crate::tool::ToolStatus::Failed);
        assert_eq!(fail_res.error.unwrap().code, MASTER_BRAIN_HTTP);
    }

    #[tokio::test]
    async fn hermes_protocol_probe_validates_auth_and_routes_runs_api() {
        #[derive(Default)]
        struct MockHermes {
            last_input: Mutex<Option<String>>,
            last_steer: Mutex<Option<Value>>,
            last_approval: Mutex<Option<Value>>,
            stopped: Mutex<Vec<String>>,
        }
        let st = Arc::new(MockHermes::default());
        let app = Router::new()
            .route(
                "/health",
                get(|| async {
                    Json(json!({ "status": "ok", "platform": "hermes-agent", "version": "0.23.3" }))
                }),
            )
            .route(
                "/v1/models",
                get(|headers: HeaderMap| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        != Some("Bearer hermes-key-123")
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "error": "unauthorized" })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({ "data": [{ "id": "hermes-agent" }] })),
                    )
                }),
            )
            .route(
                "/v1/runs",
                post(
                    |State(s): State<Arc<MockHermes>>, Json(body): Json<Value>| async move {
                        *s.last_input.lock().unwrap() = body["input"].as_str().map(str::to_string);
                        (
                            StatusCode::ACCEPTED,
                            Json(json!({ "run_id": "hermes_run_77", "status": "started" })),
                        )
                    },
                ),
            )
            .route(
                "/v1/runs/{id}",
                get(|Path(id): Path<String>| async move {
                    Json(json!({ "run_id": id, "status": "running" }))
                }),
            )
            .route(
                "/v1/runs/{id}/events",
                get(|| async { (StatusCode::OK, "") }),
            )
            .route(
                "/v1/runs/{id}/steer",
                post(
                    |State(s): State<Arc<MockHermes>>, Json(body): Json<Value>| async move {
                        *s.last_steer.lock().unwrap() = Some(body);
                        Json(json!({ "ok": true }))
                    },
                ),
            )
            .route(
                "/v1/runs/{id}/approval",
                post(
                    |State(s): State<Arc<MockHermes>>, Json(body): Json<Value>| async move {
                        *s.last_approval.lock().unwrap() = Some(body);
                        Json(json!({ "ok": true }))
                    },
                ),
            )
            .route(
                "/v1/runs/{id}/stop",
                post(
                    |State(s): State<Arc<MockHermes>>, Path(id): Path<String>| async move {
                        s.stopped.lock().unwrap().push(id);
                        Json(json!({ "status": "stopped" }))
                    },
                ),
            )
            .with_state(st.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}");

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let rt = MasterBrainRuntime::new(store.clone());
        let cfg_svc = ConfigService::new(store.clone());

        // Wrong Bearer token must fail test_connection even when /health is 200!
        cfg_svc
            .save_master_brain(
                MasterBrainConfig {
                    enabled: true,
                    protocol: MasterBrainProtocol::Hermes,
                    url: url.clone(),
                    callback_url: String::new(),
                    timeout_ms: 4_000,
                },
                Some("wrong-key"),
                "test",
            )
            .unwrap();
        let bad_probe = rt.test_connection().await;
        assert_eq!(bad_probe["ok"], false);
        assert_eq!(bad_probe["code"], MASTER_BRAIN_HTTP);

        // Right Bearer token succeeds.
        cfg_svc
            .save_master_brain(
                MasterBrainConfig {
                    enabled: true,
                    protocol: MasterBrainProtocol::Hermes,
                    url,
                    callback_url: String::new(),
                    timeout_ms: 4_000,
                },
                Some("hermes-key-123"),
                "test",
            )
            .unwrap();
        let ok_probe = rt.test_connection().await;
        assert_eq!(ok_probe["ok"], true);
        assert_eq!(ok_probe["protocol"], "hermes");

        let ctx = ToolCtx::new("T2", "C2", Arc::new(AtomicBool::new(false)))
            .with_provenance(Some("dot2-d".into()), Some("TR2".into()));
        let res = BrainDelegateTool::new(rt.clone())
            .execute(json!({ "task": "看看桌面有多少文件" }), &ctx)
            .await;
        assert_eq!(res.status, crate::tool::ToolStatus::Completed);
        assert_eq!(res.result.unwrap()["task_id"], "hermes_run_77");
        assert!(st
            .last_input
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .contains("看看桌面有多少文件"));

        // Steer sends {"message": ...}.
        let steer = BrainSteerTool::new(rt.clone());
        let sr = steer
            .execute(json!({ "guidance": "只统计文件夹" }), &ctx)
            .await;
        assert_eq!(sr.status, crate::tool::ToolStatus::Completed);
        assert_eq!(
            st.last_steer.lock().unwrap().clone().unwrap()["message"],
            "只统计文件夹"
        );

        // Pending approval + "不好" sends {"choice": "deny"} to /approval.
        rt.set_pending_approval("hermes_run_77", Some("删除临时文件".into()));
        let deny = steer.execute(json!({ "guidance": "不好" }), &ctx).await;
        assert_eq!(deny.status, crate::tool::ToolStatus::Completed);
        assert_eq!(
            st.last_approval.lock().unwrap().clone().unwrap()["choice"],
            "deny"
        );

        // Cancel calls /v1/runs/{id}/stop.
        let cr = BrainCancelTool::new(rt.clone())
            .execute(json!({}), &ctx)
            .await;
        assert_eq!(cr.status, crate::tool::ToolStatus::Completed);
        assert_eq!(st.stopped.lock().unwrap().as_slice(), &["hermes_run_77"]);
    }

    #[tokio::test]
    async fn delegate_rejects_path_traversal_task_id_from_upstream() {
        let app = Router::new().route(
            "/v1/runs",
            post(|| async {
                (
                    StatusCode::OK,
                    Json(json!({ "run_id": "../../admin/secret", "status": "running" })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let rt = MasterBrainRuntime::new(store.clone());
        ConfigService::new(store)
            .save_master_brain(
                MasterBrainConfig {
                    enabled: true,
                    protocol: MasterBrainProtocol::Hermes,
                    url: format!("http://{addr}"),
                    callback_url: String::new(),
                    timeout_ms: 4_000,
                },
                Some("secret-token"),
                "test",
            )
            .unwrap();
        let ctx = ToolCtx::new("T3", "C3", Arc::new(AtomicBool::new(false)));
        let res = BrainDelegateTool::new(rt.clone())
            .execute(json!({ "task": "test" }), &ctx)
            .await;
        assert_eq!(res.status, crate::tool::ToolStatus::Failed);
        assert_eq!(
            res.error.as_ref().map(|e| e.code.as_str()),
            Some(MASTER_BRAIN_PARSE)
        );
        assert!(rt.recent_tasks().is_empty());
    }

    #[tokio::test]
    async fn hermes_cancel_wakes_sse_and_poll_404_preserves_cancelled_status() {
        // 1. SSE endpoint that hangs on response headers; local cancellation must wake promptly
        //    and leave `cancel_notifiers` empty.
        let app = Router::new()
            .route(
                "/v1/runs/run_sse_cancel/events",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    StatusCode::OK
                }),
            )
            .route(
                "/v1/runs/run_poll_404",
                get(|| async { (StatusCode::NOT_FOUND, Json(json!({ "detail": "gone" }))) }),
            )
            .route(
                "/v1/runs/run_poll_404/events",
                get(|| async { StatusCode::NOT_FOUND }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let rt = MasterBrainRuntime::new(store.clone());
        let cfg = MasterBrainConfig {
            enabled: true,
            protocol: MasterBrainProtocol::Hermes,
            url: format!("http://{addr}"),
            callback_url: String::new(),
            timeout_ms: 4_000,
        };

        rt.record_task(DelegatedTaskRecord {
            task_id: "run_sse_cancel".into(),
            summary: "hang sse".into(),
            device: None,
            room: None,
            status: "running".into(),
            last_tool: None,
            pending_approval: None,
            created_ms: 1,
            updated_ms: 1,
        });
        let rt_watch = rt.clone();
        let cfg_watch = cfg.clone();
        let watch_fut = tokio::spawn(async move {
            rt_watch
                .watch_hermes_run(cfg_watch, None, "run_sse_cancel".into(), None, false)
                .await;
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        rt.update_task_status("run_sse_cancel", "cancelled");
        tokio::time::timeout(Duration::from_secs(2), watch_fut)
            .await
            .expect("SSE watcher must wake promptly on cancel")
            .unwrap();
        assert!(
            !rt.cancel_notifiers
                .lock()
                .unwrap()
                .contains_key("run_sse_cancel"),
            "cancel_notifiers must be cleaned up after SSE exit"
        );

        // 2. Poll 404 when already locally cancelled must NOT clobber status to "failed".
        rt.record_task(DelegatedTaskRecord {
            task_id: "run_poll_404".into(),
            summary: "cancelled task".into(),
            device: None,
            room: None,
            status: "cancelled".into(),
            last_tool: None,
            pending_approval: None,
            created_ms: 2,
            updated_ms: 2,
        });
        rt.poll_hermes_until_terminal(
            &cfg,
            None,
            "run_poll_404",
            None,
            false,
            Instant::now() + Duration::from_secs(5),
        )
        .await;
        let status = rt
            .recent_tasks()
            .into_iter()
            .find(|t| t.task_id == "run_poll_404")
            .map(|t| t.status);
        assert_eq!(status.as_deref(), Some("cancelled"));
        assert!(
            !rt.cancel_notifiers
                .lock()
                .unwrap()
                .contains_key("run_poll_404"),
            "cancel_notifiers must be cleaned up after poll 404 exit"
        );
    }
}
