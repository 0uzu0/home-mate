//! NAS `learning-svc` (#119–#123) Core client, speaker-first voice tools, and
//! `LEARN-10` explicit bounded learning wait state.
//!
//! Optional service: registered by default, unconfigured until a household
//! sets the NAS `learning-svc` URL + device token and enables it. Per the
//! 2026-11-19 product decision, the speaker (`hm-core`, `role="core"`) is the
//! primary voice study terminal (no 36-char `binding_id` UUID required), while
//! phone/web/admin is maintenance-only. Legacy `binding_id` lookup remains
//! supported for backwards compatibility.

use crate::config::{ConfigService, PersonalEndpointConfig, LEARNING_KEY, LEARNING_TOKEN_SECRET};
use crate::id;
use crate::store::Store;
use crate::tool::http::{read_bounded, validate_news_url};
use crate::tool::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn next_idempotency_uuid(prefix: &str) -> String {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    h.update(b":");
    h.update(id::next(prefix).as_bytes());
    h.update(b":");
    h.update(id::now_ms().to_le_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x40; // UUID v4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

const MAX_JSON_BODY: usize = 512 * 1024;
const API_PREFIX: &str = "/api/v1/learning";

/// `LEARN-10`: bounded extended follow-up window while an active vocabulary
/// card is waiting for the learner's spoken answer (never infinite open mic).
pub const LEARNING_WAIT_WINDOW_MS: u64 = 25_000;
/// Hard ceiling on any learning wait window override.
pub const MAX_LEARNING_WAIT_MS: u64 = 30_000;
/// Hard TTL for a learning wait lease without fresh learning activity.
pub const LEARNING_WAIT_TTL_MS: u64 = 120_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LearningWaitLease {
    pub window_ms: u64,
    pub until_ms: u64,
    pub session_id: Option<String>,
    pub attempt_id: Option<String>,
}

fn valid_id(raw: &str) -> bool {
    !raw.is_empty()
        && !raw.contains("..")
        && !raw
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#') || c.is_whitespace() || c.is_control())
}

#[derive(Clone)]
pub struct LearningRuntime {
    store: Arc<Store>,
    client: reqwest::Client,
    wait: Arc<Mutex<Option<LearningWaitLease>>>,
}

impl LearningRuntime {
    pub fn new(store: Arc<Store>) -> Self {
        let allow_loopback = store.allows_loopback_public();
        let client = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
        )
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            store,
            client,
            wait: Arc::new(Mutex::new(None)),
        }
    }

    pub fn config(&self) -> (PersonalEndpointConfig, Option<String>) {
        let svc = ConfigService::new(self.store.clone());
        let cfg = svc.personal_endpoint(LEARNING_KEY).unwrap_or_default();
        let tok = svc.personal_token(LEARNING_TOKEN_SECRET).ok().flatten();
        (cfg, tok)
    }

    /// Returns the active bounded `LEARN-10` wait lease if still valid at `now_ms`.
    pub fn active_wait(&self, now_ms: u64) -> Option<LearningWaitLease> {
        let mut guard = crate::lock_or_recover(&self.wait, "learning.wait");
        if let Some(lease) = guard.as_ref() {
            if now_ms < lease.until_ms {
                return Some(lease.clone());
            }
            *guard = None;
        }
        None
    }

    /// Explicitly clears the `LEARN-10` wait lease (pause, end, retreat, close).
    pub fn clear_wait(&self) {
        *crate::lock_or_recover(&self.wait, "learning.wait") = None;
    }

    fn sync_wait_from_response(&self, body: &Value, now_ms: u64) {
        let status = body["status"].as_str();
        let active_flag = body["active"].as_bool();
        let has_attempt = body["attempt"].is_object();
        let is_active =
            matches!(status, Some("active")) || (active_flag == Some(true) && has_attempt);
        let is_inactive =
            matches!(status, Some("paused" | "ended" | "completed")) || active_flag == Some(false);
        if is_active {
            let lease = LearningWaitLease {
                window_ms: LEARNING_WAIT_WINDOW_MS.min(MAX_LEARNING_WAIT_MS),
                until_ms: now_ms.saturating_add(LEARNING_WAIT_TTL_MS),
                session_id: body["session_id"]
                    .as_str()
                    .or_else(|| body["session"]["id"].as_str())
                    .map(String::from),
                attempt_id: body["attempt"]["attempt_id"]
                    .as_str()
                    .or_else(|| body["attempt"]["id"].as_str())
                    .or_else(|| body["attempt_id"].as_str())
                    .map(String::from),
            };
            *crate::lock_or_recover(&self.wait, "learning.wait") = Some(lease);
        } else if is_inactive {
            self.clear_wait();
        }
    }

    async fn get_json(
        &self,
        cfg: &PersonalEndpointConfig,
        token: Option<&str>,
        path: &str,
    ) -> Result<Value, String> {
        let url = format!("{}{}", cfg.url.trim_end_matches('/'), path);
        validate_news_url(&url, self.store.allows_loopback_public())
            .map_err(|e| format!("invalid url: {e}"))?;
        let mut req = self
            .client
            .get(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms));
        if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
            req = req.bearer_auth(tok.trim());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        self.decode_response(resp).await
    }

    async fn post_json(
        &self,
        cfg: &PersonalEndpointConfig,
        token: Option<&str>,
        path: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, String> {
        let url = format!("{}{}", cfg.url.trim_end_matches('/'), path);
        validate_news_url(&url, self.store.allows_loopback_public())
            .map_err(|e| format!("invalid url: {e}"))?;
        let mut req = self
            .client
            .post(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .json(&body);
        if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
            req = req.bearer_auth(tok.trim());
        }
        if let Some(key) = idempotency_key.filter(|k| !k.trim().is_empty()) {
            req = req.header("Idempotency-Key", key.trim());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        self.decode_response(resp).await
    }

    async fn decode_response(&self, resp: reqwest::Response) -> Result<Value, String> {
        let status = resp.status();
        let bytes = read_bounded(resp, MAX_JSON_BODY)
            .await
            .map_err(|e| format!("body read failed: {e}"))?;
        if !status.is_success() {
            let preview = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                preview.chars().take(200).collect::<String>()
            ));
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid json: {e}"))
    }

    fn clear_if_session_gone(&self, err: &str) {
        if err.starts_with("HTTP 404:") || err.starts_with("HTTP 410:") {
            self.clear_wait();
        }
    }

    pub async fn test_connection(&self) -> Value {
        let (cfg, tok) = self.config();
        if !cfg.is_ready() {
            return json!({ "ok": false, "state": "unconfigured", "detail": "learning-svc not enabled or url missing" });
        }
        match self.get_json(&cfg, tok.as_deref(), "/health").await {
            Ok(health) => match self.get_json(&cfg, tok.as_deref(), "/ready").await {
                Ok(ready) => {
                    json!({ "ok": true, "state": "ready", "health": health, "ready": ready })
                }
                Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
            },
            Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
        }
    }
}

fn ready_availability(rt: &LearningRuntime) -> Availability {
    if rt.config().0.is_ready() {
        Availability::Ready
    } else {
        Availability::Unconfigured
    }
}

pub struct LearningDecksTool {
    rt: LearningRuntime,
}

impl LearningDecksTool {
    pub fn new(rt: LearningRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for LearningDecksTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("learning.decks"),
            service: Cow::Borrowed("learning"),
            side_effect: SideEffect::Query,
            description: "List available learners and English vocabulary decks on learning-svc for speaker voice practice.".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn availability(&self) -> Availability {
        ready_availability(&self.rt)
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.config();
        if !cfg.is_ready() {
            return ToolResult::failed(
                "LEARNING_UNCONFIGURED",
                "learning-svc is not configured",
                false,
            );
        }
        match self
            .rt
            .get_json(&cfg, tok.as_deref(), &format!("{API_PREFIX}/device/decks"))
            .await
        {
            Ok(v) => ToolResult::observed_ok(v),
            Err(e) => ToolResult::failed("LEARNING_ERROR", e, true),
        }
    }
}

pub struct LearningStartTool {
    rt: LearningRuntime,
}

impl LearningStartTool {
    pub fn new(rt: LearningRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for LearningStartTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("learning.start"),
            service: Cow::Borrowed("learning"),
            side_effect: SideEffect::Mutate,
            description: "Start or resume a speaker-led English vocabulary study session (no binding_id needed). Returns the first prompt card for the child to answer by voice.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "learner_id": { "type": "string", "description": "Optional learner UUID; defaults to household learner." },
                    "deck_id": { "type": "string", "description": "Optional deck UUID; defaults to first non-empty deck." },
                    "deck_query": { "type": "string", "description": "Optional deck title keyword filter (e.g. 五年级)." },
                    "dimension": { "type": "string", "enum": ["meaning_to_spelling", "en_to_meaning"], "description": "Study direction; defaults to meaning_to_spelling (speak Chinese meaning, learner answers English)." },
                    "mode": { "type": "string", "enum": ["auto", "due", "new"], "description": "Queue mode; defaults to auto." },
                    "limit": { "type": "integer", "enum": [5, 10, 20], "description": "Max cards for this session (5, 10, or 20; default 10)." }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        ready_availability(&self.rt)
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.config();
        if !cfg.is_ready() {
            return ToolResult::failed(
                "LEARNING_UNCONFIGURED",
                "learning-svc is not configured",
                false,
            );
        }
        let mut obj = serde_json::Map::new();
        for key in ["learner_id", "deck_id"] {
            if let Some(s) = args
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                obj.insert(key.into(), Value::String(s.into()));
            }
        }
        if let Some(q) = args
            .get("deck_query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let trimmed: String = q.chars().take(100).collect();
            obj.insert("deck_query".into(), Value::String(trimmed));
        }
        if let Some(dim) = args
            .get("dimension")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !matches!(dim, "meaning_to_spelling" | "en_to_meaning") {
                return ToolResult::failed(
                    "INVALID_ARGS",
                    "dimension must be meaning_to_spelling or en_to_meaning",
                    false,
                );
            }
            obj.insert("dimension".into(), Value::String(dim.into()));
        }
        if let Some(mode) = args
            .get("mode")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !matches!(mode, "auto" | "due" | "new") {
                return ToolResult::failed("INVALID_ARGS", "mode must be auto, due, or new", false);
            }
            obj.insert("mode".into(), Value::String(mode.into()));
        }
        if let Some(n) = args.get("limit").and_then(Value::as_u64) {
            let bucket = if n <= 5 {
                5
            } else if n <= 10 {
                10
            } else {
                20
            };
            obj.insert("limit".into(), json!(bucket));
        }
        let idem = next_idempotency_uuid("learn-start");
        match self
            .rt
            .post_json(
                &cfg,
                tok.as_deref(),
                &format!("{API_PREFIX}/device/sessions/start"),
                Value::Object(obj),
                Some(&idem),
            )
            .await
        {
            Ok(v) => {
                self.rt.sync_wait_from_response(&v, id::now_ms());
                ToolResult::observed_ok(v)
            }
            Err(e) => {
                self.rt.clear_if_session_gone(&e);
                ToolResult::failed("LEARNING_ERROR", e, true)
            }
        }
    }
}

pub struct LearningCurrentTool {
    rt: LearningRuntime,
}

impl LearningCurrentTool {
    pub fn new(rt: LearningRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for LearningCurrentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("learning.current"),
            service: Cow::Borrowed("learning"),
            side_effect: SideEffect::Query,
            description: "Fetch the speaker's active English vocabulary card/prompt (or legacy binding_id if provided) from learning-svc. Only for vocabulary study sessions, never for media or playback status questions.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "binding_id": { "type": "string", "description": "Optional legacy phone binding UUID; omit for speaker-first active session." }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        ready_availability(&self.rt)
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.config();
        if !cfg.is_ready() {
            return ToolResult::failed(
                "LEARNING_UNCONFIGURED",
                "learning-svc is not configured",
                false,
            );
        }
        let (path, is_speaker_route) = match args
            .get("binding_id")
            .and_then(Value::as_str)
            .map(str::trim)
        {
            Some(bid) if !bid.is_empty() => {
                if !valid_id(bid) {
                    return ToolResult::failed(
                        "INVALID_ARGS",
                        "valid binding_id is required",
                        false,
                    );
                }
                (format!("{API_PREFIX}/device/bindings/{bid}/current"), false)
            }
            _ => (format!("{API_PREFIX}/device/current"), true),
        };
        match self.rt.get_json(&cfg, tok.as_deref(), &path).await {
            Ok(v) => {
                self.rt.sync_wait_from_response(&v, id::now_ms());
                ToolResult::observed_ok(v)
            }
            Err(e) => {
                if is_speaker_route {
                    self.rt.clear_if_session_gone(&e);
                }
                ToolResult::failed("LEARNING_ERROR", e, true)
            }
        }
    }
}

pub struct LearningAnswerTool {
    rt: LearningRuntime,
}

impl LearningAnswerTool {
    pub fn new(rt: LearningRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for LearningAnswerTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("learning.answer"),
            service: Cow::Borrowed("learning"),
            side_effect: SideEffect::Mutate,
            description: "Submit the learner's spoken answer (English word, letter-by-letter spelling like 'b u s y', or Chinese meaning) for the active card, grade it with FSRS, and advance to the next card. Also call this directly when the learner speaks a difficulty/self-rating or skip command (whether or not the card is revealed): '困难'/'太难了'/'有点难' -> answer='困难', rating='hard'; '简单'/'太简单了'/'会了'/'掌握了' -> answer='简单', rating='good'; '下一个'/'跳过'/'不会'/'重来' or '不知道'/'看答案' on an already-revealed card -> answer='跳过', rating='again'.".into(),
            parameters: json!({
                "type": "object",
                "required": ["answer"],
                "properties": {
                    "answer": { "type": "string", "description": "Learner's spoken answer, or self-rating/skip phrase ('困难', '简单', '跳过')." },
                    "auto_grade": { "type": "boolean", "description": "Automatically apply FSRS grade and advance (default true)." },
                    "rating": { "type": "string", "enum": ["again", "hard", "good"], "description": "Optional explicit FSRS rating override ('hard' for 困难, 'good' for 简单, 'again' when skipping/repeating)." }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        ready_availability(&self.rt)
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.config();
        if !cfg.is_ready() {
            return ToolResult::failed(
                "LEARNING_UNCONFIGURED",
                "learning-svc is not configured",
                false,
            );
        }
        let Some(ans) = args["answer"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::failed("INVALID_ARGS", "non-empty answer is required", false);
        };
        if ans.chars().count() > 200 {
            return ToolResult::failed(
                "INVALID_ARGS",
                "answer must be at most 200 characters",
                false,
            );
        }
        let mut obj = serde_json::Map::new();
        obj.insert("answer".into(), Value::String(ans.into()));
        obj.insert(
            "auto_grade".into(),
            json!(args["auto_grade"].as_bool().unwrap_or(true)),
        );
        if let Some(r) = args["rating"]
            .as_str()
            .filter(|r| matches!(*r, "again" | "hard" | "good"))
        {
            obj.insert("rating".into(), Value::String(r.into()));
        }
        let idem = next_idempotency_uuid("learn-ans");
        match self
            .rt
            .post_json(
                &cfg,
                tok.as_deref(),
                &format!("{API_PREFIX}/device/current/answer"),
                Value::Object(obj),
                Some(&idem),
            )
            .await
        {
            Ok(v) => {
                self.rt.sync_wait_from_response(&v, id::now_ms());
                ToolResult::observed_ok(v)
            }
            Err(e) => {
                self.rt.clear_if_session_gone(&e);
                ToolResult::failed("LEARNING_ERROR", e, true)
            }
        }
    }
}

pub struct LearningRevealTool {
    rt: LearningRuntime,
}

impl LearningRevealTool {
    pub fn new(rt: LearningRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for LearningRevealTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("learning.reveal"),
            service: Cow::Borrowed("learning"),
            side_effect: SideEffect::Mutate,
            description: "Reveal the answer, spelling, and meaning of the active vocabulary card when the learner says they don't know or asks for a hint. NOTE: learning.reveal does NOT advance to the next card; never invent a next word after reveal—wait for the learner to repeat/spell the revealed word or say '下一个', then call learning.answer to advance.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "assisted": { "type": "boolean", "description": "Mark reveal as assisted (default false)." }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        ready_availability(&self.rt)
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.config();
        if !cfg.is_ready() {
            return ToolResult::failed(
                "LEARNING_UNCONFIGURED",
                "learning-svc is not configured",
                false,
            );
        }
        let body = json!({ "assisted": args["assisted"].as_bool().unwrap_or(false) });
        let idem = next_idempotency_uuid("learn-rev");
        match self
            .rt
            .post_json(
                &cfg,
                tok.as_deref(),
                &format!("{API_PREFIX}/device/current/reveal"),
                body,
                Some(&idem),
            )
            .await
        {
            Ok(mut v) => {
                self.rt.sync_wait_from_response(&v, id::now_ms());
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "_instruction".into(),
                        Value::String(
                            "当前卡片答案已揭晓，但系统仍停留在这一题（并未切换到下一题）。请简洁播报该词的英文发音、字母拼写和中文释义，引导用户跟读拼写或说“下一个”；严禁在未调用 learning.answer 的情况下自行编造或提问下一个单词。若用户随后说“下一个/跳过/还是不知道”，请调用 learning.answer(answer=\"跳过\", rating=\"again\")；若说“困难/太难了”，调用 learning.answer(answer=\"困难\", rating=\"hard\")；若说“简单/会了”，调用 learning.answer(answer=\"简单\", rating=\"good\") 推进到下一题。"
                                .into(),
                        ),
                    );
                }
                ToolResult::observed_ok(v)
            }
            Err(e) => {
                self.rt.clear_if_session_gone(&e);
                ToolResult::failed("LEARNING_ERROR", e, true)
            }
        }
    }
}

pub struct LearningControlTool {
    rt: LearningRuntime,
}

impl LearningControlTool {
    pub fn new(rt: LearningRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for LearningControlTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("learning.control"),
            service: Cow::Borrowed("learning"),
            side_effect: SideEffect::Mutate,
            description: "Pause, resume, or end the active speaker vocabulary study session."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["pause", "resume", "end"] }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        ready_availability(&self.rt)
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.config();
        if !cfg.is_ready() {
            return ToolResult::failed(
                "LEARNING_UNCONFIGURED",
                "learning-svc is not configured",
                false,
            );
        }
        let Some(action) = args["action"]
            .as_str()
            .map(str::trim)
            .filter(|a| matches!(*a, "pause" | "resume" | "end"))
        else {
            return ToolResult::failed(
                "INVALID_ARGS",
                "action must be pause, resume, or end",
                false,
            );
        };
        let idem = next_idempotency_uuid("learn-ctrl");
        match self
            .rt
            .post_json(
                &cfg,
                tok.as_deref(),
                &format!("{API_PREFIX}/device/current/control"),
                json!({ "action": action }),
                Some(&idem),
            )
            .await
        {
            Ok(v) => {
                self.rt.sync_wait_from_response(&v, id::now_ms());
                ToolResult::observed_ok(v)
            }
            Err(e) => {
                self.rt.clear_if_session_gone(&e);
                ToolResult::failed("LEARNING_ERROR", e, true)
            }
        }
    }
}

#[cfg(test)]
mod rc6_rc7_tests {
    use super::*;

    #[test]
    fn rc6_story_recent_search_matches_corrected_query_and_tracks_album_kind() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let hub = crate::gateway::DeviceHub::new();
        let rt = crate::tool::personal_media::PersonalMediaRuntime::new(store, hub);
        let mut search_payload = json!({
            "results": [
                {
                    "id": "c84da55b316a0:1",
                    "kind": "album",
                    "title": "儿童睡前故事：一千零一夜合集"
                },
                {
                    "id": "web:2044572111",
                    "kind": "web-episode",
                    "title": "【奶龙睡前】第356集"
                }
            ]
        });
        rt.remember_story_items(&mut search_payload);
        assert_eq!(rt.lookup_kind("c84da55b316a0:1").as_deref(), Some("album"));

        // Even if LLM calls story.play with ASR homophone "一千零一页", it matches the recent search album
        let matched = rt.match_recent_search("一千零一页", "一千零一页").unwrap();
        assert_eq!(matched.id, "c84da55b316a0:1");
        assert_eq!(matched.kind, "album");
    }

    #[test]
    fn rc7_learning_reveal_and_answer_descriptors_prevent_post_reveal_hallucination() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = LearningRuntime::new(store);
        let reveal_desc = LearningRevealTool::new(rt.clone()).descriptor();
        let answer_desc = LearningAnswerTool::new(rt).descriptor();
        assert!(
            reveal_desc
                .description
                .contains("does NOT advance to the next card"),
            "learning.reveal descriptor must state it does not advance: {}",
            reveal_desc.description
        );
        assert!(
            answer_desc.description.contains("跳过")
                && answer_desc.description.contains("困难")
                && answer_desc.description.contains("简单"),
            "learning.answer descriptor must document skip and self-rating advancement: {}",
            answer_desc.description
        );
    }
}
