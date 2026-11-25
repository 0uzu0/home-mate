//! Home automation: a real Home Assistant adapter (R2-BM5).
//!
//! The 2026-11-12 21:53 turn proved why this module exists. `home.light` was
//! an in-memory mock that answered `Completed / DeviceConfirmed` out of a
//! HashMap, and accepted any entity name the model invented
//! (`children_room.light`). The model then truthfully relayed what it was
//! told: 「儿童房的灯已经关好了。」 Nothing lied except the tool layer.
//!
//! R1j (P0-b) removed the mock: with `backend = none` the tool refuses with
//! `HOME_UNCONFIGURED` before touching anything. R2-BM5 adds the other half:
//! with a real backend, **only a read-back may confirm**. A `POST
//! /api/services/light/turn_off` answer of 200 means the service accepted the
//! request — `service_accepted ≠ confirmed`. The tool then reads
//! `GET /api/states/<entity>` and compares; mismatch or timeout is `Unknown`,
//! never success. The model never chooses an entity id: it may only use a
//! name from the household's whitelist (`HomeEntity`), and the core renders
//! the spoken fact from the structured receipt (see `agent::facts`).

use crate::config::{ConfigService, HomeBackend, HomeConfig, HomeEntity};
use crate::config_delivery::HomeSnapshot;
use crate::store::Store;
use crate::tool::{
    Availability, Evidence, FactBasis, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolFact,
    ToolResult, ToolStatus,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const HOME_UNCONFIGURED: &str = "HOME_UNCONFIGURED";
pub const HOME_ENTITY_NOT_ALLOWED: &str = "HOME_ENTITY_NOT_ALLOWED";
pub const HOME_AUTH: &str = "HOME_AUTH";
pub const HOME_TIMEOUT: &str = "HOME_TIMEOUT";
pub const HOME_UNREACHABLE: &str = "HOME_UNREACHABLE";
pub const HOME_HTTP: &str = "HOME_HTTP";
pub const HOME_RESPONSE: &str = "HOME_RESPONSE";
pub const HOME_READBACK_MISMATCH: &str = "HOME_READBACK_MISMATCH";
/// The service accepted the write but the state could not be read back.
pub const HOME_READBACK_UNOBSERVED: &str = "HOME_READBACK_UNOBSERVED";
/// A configured entity whose domain is outside `HOME_SET_DOMAINS`.
pub const HOME_UNSUPPORTED_DOMAIN: &str = "HOME_UNSUPPORTED_DOMAIN";
/// The backend response exceeded the bounded read (a full /api/states list is
/// the big one; never buffer it without a ceiling).
pub const HOME_RESPONSE_TOO_LARGE: &str = "HOME_RESPONSE_TOO_LARGE";

/// One `/api/states` response is a whole house; 1 MiB is far above a normal
/// install and still bounds a hostile/broken backend.
const MAX_HA_BODY: usize = 1024 * 1024;
/// Discovery is a picker, not an export. The console cannot render more.
const MAX_DISCOVERED: usize = 500;
/// The turn was cancelled; no further attempt was started.
pub const HOME_CANCELLED: &str = "HOME_CANCELLED";
/// R2-BM6.3: the exposed-directory read failed. Named so the console can say
/// "HA's exposure list could not be read" instead of showing an empty house
/// or silently switching to `/api/states`.
pub const HOME_EXPOSURE_UNAVAILABLE: &str = "HOME_EXPOSURE_UNAVAILABLE";

/// How many state reads after the service call. Home Assistant applies most
/// light changes immediately; three reads 150 ms apart cover the common
/// event-loop lag without holding a voice turn open for seconds.
const READBACK_READS: usize = 3;
const READBACK_GAP: Duration = Duration::from_millis(150);
/// A read-back is a short, local GET; never let one attempt consume the
/// action's whole budget.
const READBACK_ATTEMPT_TIMEOUT_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct HaError {
    pub code: &'static str,
    pub detail: String,
    pub retryable: bool,
}

impl HaError {
    fn new(code: &'static str, detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            detail: detail.into(),
            retryable,
        }
    }
}

pub struct HomeRuntime {
    store: Arc<Store>,
    client: reqwest::Client,
}

impl HomeRuntime {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        Self::with_client(
            store,
            crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                crate::tool::http::OutboundPolicy::Lan {
                    reject_public_ip_literals: false,
                    allow_loopback,
                },
            )
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default(),
        )
    }

    /// Test seam: inject a client (short timeouts, its own pool).
    pub fn with_client(store: Arc<Store>, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self { store, client })
    }

    fn snapshot(&self) -> HomeSnapshot {
        ConfigService::new(self.store.clone())
            .home_snapshot()
            .unwrap_or_default()
    }

    pub fn config(&self) -> HomeConfig {
        self.snapshot().config
    }

    /// The CONNECTION is usable (backend + address + token). Discovery needs
    /// only this: the whitelist is what discovery is for, so requiring an
    /// entity first would be a chicken-and-egg gate.
    fn missing_connection(snapshot: &HomeSnapshot) -> Option<&'static str> {
        Self::missing_connection_with(snapshot, true)
    }

    fn missing_connection_with(
        snapshot: &HomeSnapshot,
        allow_loopback: bool,
    ) -> Option<&'static str> {
        let cfg = &snapshot.config;
        if cfg.backend != HomeBackend::HomeAssistant {
            return Some("没有选择家庭自动化后端");
        }
        if cfg.base_url.trim().is_empty() {
            return Some("还没有填 Home Assistant 地址");
        }
        if !HomeConfig::endpoint_is_portable(&cfg.base_url)
            || !crate::config::valid_home_endpoint_with(&cfg.base_url, allow_loopback)
        {
            return Some("Home Assistant 地址无效，请在控制台修复");
        }
        if snapshot.token.is_none() {
            return Some("还没有填 Home Assistant 长期令牌");
        }
        None
    }

    /// What is missing before the adapter can RUN A TOOL. `None` = ready.
    fn missing(snapshot: &HomeSnapshot) -> Option<&'static str> {
        Self::missing_with(snapshot, true)
    }

    fn missing_with(snapshot: &HomeSnapshot, allow_loopback: bool) -> Option<&'static str> {
        if let Some(what) = Self::missing_connection_with(snapshot, allow_loopback) {
            return Some(what);
        }
        let cfg = &snapshot.config;
        if cfg.entities.is_empty() {
            return Some(match cfg.entity_source {
                crate::config::EntitySource::Exposed => {
                    "暴露列表是空的：先在控制台刷新 Home Assistant 的暴露实体"
                }
                crate::config::EntitySource::Api => "实体白名单是空的：先在控制台发现/勾选一个实体",
            });
        }
        None
    }

    /// `none` and an incomplete Home Assistant block are both Unconfigured —
    /// registered, honestly not usable. It is never `Ready` by accident.
    pub fn availability(&self) -> Availability {
        if Self::missing_with(&self.snapshot(), self.store.allows_loopback_public()).is_some() {
            Availability::Unconfigured
        } else {
            Availability::Ready
        }
    }

    fn detail(snapshot: &HomeSnapshot) -> String {
        match Self::missing(snapshot) {
            Some(what) => {
                format!("未接入家庭自动化（{what}）：助手会如实说控制不了设备，不会声称已完成")
            }
            None => "已接入 Home Assistant：动作以设备回读为准；没读回就不会说已完成".into(),
        }
    }

    fn find_entity<'a>(cfg: &'a HomeConfig, requested: &str) -> Option<&'a HomeEntity> {
        cfg.entities
            .iter()
            .find(|e| e.name == requested || e.entity == requested)
    }

    /// Every whitelisted name, not the first ten. The model can only call an
    /// entity it can NAME, and this string is the only place the names reach
    /// it (tool description + prompt capability block). The old `.take(10)`
    /// hid the rest of a big house: the real whitelist had 39 entities and
    /// 「儿童房主灯」 was #17, so the assistant answered "没有儿童房的灯"
    /// (2026-11-13). The save path already caps the list at 64.
    fn allowed_names(cfg: &HomeConfig) -> String {
        let names: Vec<&str> = cfg.entities.iter().map(|e| e.name.as_str()).collect();
        if names.is_empty() {
            "（白名单为空）".into()
        } else {
            names.join("、")
        }
    }

    /// One home-assistant REST request with bounded retries. Only
    /// connect/timeout/5xx are retried; auth and 4xx are final.
    async fn request(
        &self,
        snapshot: &HomeSnapshot,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        expect_json: bool,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<(Value, usize), (HaError, usize)> {
        let cfg = &snapshot.config;
        let token = snapshot
            .token
            .as_deref()
            .ok_or_else(|| (HaError::new(HOME_UNCONFIGURED, "缺少长期令牌", false), 0))?;
        let url = format!("{}{}", cfg.base_url.trim_end_matches('/'), path);
        let attempts = 1 + cfg.retries.min(3) as usize;
        let mut last: Option<HaError> = None;
        let mut seen = 0usize;
        for attempt in 0..attempts {
            // Audit Q5: a cancelled turn must not start another retry. An
            // already-submitted write cannot be undone, but a NEW attempt can
            // be prevented.
            if cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false) {
                return Err((
                    HaError::new(HOME_CANCELLED, "cancelled before the next attempt", false),
                    seen,
                ));
            }
            seen += 1;
            let mut req = self
                .client
                .request(method.clone(), &url)
                .timeout(Duration::from_millis(cfg.timeout_ms.clamp(500, 15_000)))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json");
            if let Some(b) = &body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        // R2-BM6.1: /api/states is a whole-house response;
                        // read it with a ceiling instead of buffering it
                        // unbounded.
                        let bytes = crate::tool::http::read_bounded(resp, MAX_HA_BODY)
                            .await
                            .map_err(|e| {
                                let (code, detail, retryable) = match e {
                                    crate::tool::http::BodyReadError::TooLarge { .. } => (
                                        HOME_RESPONSE_TOO_LARGE,
                                        format!(
                                            "Home Assistant 响应超过 {} KiB",
                                            MAX_HA_BODY / 1024
                                        ),
                                        false,
                                    ),
                                    crate::tool::http::BodyReadError::Timeout => {
                                        (HOME_TIMEOUT, "读取 Home Assistant 响应超时".into(), true)
                                    }
                                    crate::tool::http::BodyReadError::PrematureClose(_) => (
                                        HOME_RESPONSE,
                                        "读取 Home Assistant 响应时连接提前关闭".into(),
                                        false,
                                    ),
                                    crate::tool::http::BodyReadError::Protocol(_) => (
                                        HOME_RESPONSE,
                                        "读取 Home Assistant 响应时协议异常".into(),
                                        false,
                                    ),
                                };
                                (HaError::new(code, detail, retryable), seen)
                            })?;
                        let text = String::from_utf8_lossy(&bytes);
                        let v = if text.trim().is_empty() || !expect_json {
                            json!({})
                        } else {
                            serde_json::from_str(&text).map_err(|e| {
                                (
                                    HaError::new(
                                        HOME_RESPONSE,
                                        format!("返回了无法解析的内容：{e}"),
                                        false,
                                    ),
                                    seen,
                                )
                            })?
                        };
                        return Ok((v, seen));
                    }
                    let detail = match status.as_u16() {
                        401 | 403 => format!("鉴权失败（HTTP {status}）：检查长期令牌"),
                        _ => format!("Home Assistant 返回 HTTP {status}"),
                    };
                    let code = if status.as_u16() == 401 || status.as_u16() == 403 {
                        HOME_AUTH
                    } else {
                        HOME_HTTP
                    };
                    let retryable = status.is_server_error() && attempt + 1 < attempts;
                    last = Some(HaError::new(code, detail, retryable));
                    if !retryable {
                        break;
                    }
                }
                Err(e) => {
                    let (code, detail) = if e.is_timeout() {
                        (HOME_TIMEOUT, "Home Assistant 超时")
                    } else if e.is_connect() {
                        (
                            HOME_UNREACHABLE,
                            "连不上 Home Assistant（网络或地址不可达）",
                        )
                    } else {
                        (HOME_UNREACHABLE, "Home Assistant 请求失败")
                    };
                    let retryable = attempt + 1 < attempts;
                    last = Some(HaError::new(code, detail, retryable));
                    if !retryable {
                        break;
                    }
                }
            }
        }
        Err((
            last.unwrap_or_else(|| HaError::new(HOME_HTTP, "请求失败", false)),
            seen,
        ))
    }

    async fn read_state(
        &self,
        snapshot: &HomeSnapshot,
        entity: &str,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<(Value, usize), (HaError, usize)> {
        self.request(
            snapshot,
            reqwest::Method::GET,
            &format!("/api/states/{}", urlencode(entity)),
            None,
            true,
            cancel,
        )
        .await
    }

    /// The write-then-read-back contract. `Ok(state)` is the ONLY path to
    /// `DeviceConfirmed`; the service call's 200 is not enough.
    ///
    /// `domain` is the first half of the entity id (light/switch/fan/
    /// input_boolean/cover). `service` is the HA service name; `readback` is
    /// the state HA must report (`closed` for a closed cover).
    async fn set_entity(
        &self,
        snapshot: &HomeSnapshot,
        entity: &str,
        domain: &str,
        service: &str,
        readback: &str,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<Value, (HaError, usize, Option<String>, bool)> {
        let (_, calls) = self
            .request(
                snapshot,
                reqwest::Method::POST,
                &format!("/api/services/{domain}/{service}"),
                Some(json!({ "entity_id": entity })),
                true,
                cancel,
            )
            .await
            .map_err(|(e, n)| (e, n, None, false))?;
        // From here on the service ACCEPTED the write. A failed observation
        // must therefore stay Unknown — the action may already have happened
        // even if we could not read it back (audit Q2).
        // Read back; `observed` stays available to the honest failure.
        let mut observed: Option<String> = None;
        let scoped = HomeSnapshot {
            config: HomeConfig {
                timeout_ms: snapshot.config.timeout_ms.min(READBACK_ATTEMPT_TIMEOUT_MS),
                retries: 0,
                ..snapshot.config.clone()
            },
            token: snapshot.token.clone(),
        };
        for i in 0..READBACK_READS {
            if i > 0 {
                tokio::time::sleep(READBACK_GAP).await;
            }
            match self.read_state(&scoped, entity, cancel).await {
                Ok((v, n)) => {
                    let got = v
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    observed = Some(got.clone());
                    if got == readback {
                        return Ok(json!({
                            "entity": entity, "state": got, "service": service,
                            "last_changed": v.get("last_changed"),
                            "attempts": i + 1, "http_requests": calls + n,
                        }));
                    }
                }
                Err((e, _)) if i + 1 == READBACK_READS => {
                    return Err((e, calls, observed, true));
                }
                Err(_) => continue,
            }
        }
        Err((
            HaError::new(
                HOME_READBACK_MISMATCH,
                format!(
                    "读回状态是 {}，目标是 {readback}",
                    observed.as_deref().unwrap_or("（没有读到）")
                ),
                false,
            ),
            calls,
            observed,
            true,
        ))
    }

    /// A REAL probe (R2-BM5): verifies address, token and one whitelisted
    /// entity, without changing anything. Returns the state word the console
    /// renders, the observed entity state, latency and the number of HTTP
    /// requests actually made. Unconfigured = 0 requests.
    pub async fn probe(&self) -> Value {
        let snapshot = self.snapshot();
        let cfg = &snapshot.config;
        let started = Instant::now();
        if let Some(what) = Self::missing(&snapshot) {
            return json!({
                "ok": false, "state": "unconfigured", "detail": what,
                "backend": cfg.backend, "token_set": snapshot.token.is_some(),
                "http_requests": 0, "latency_ms": 0,
            });
        }
        let mut requests = 0usize;
        match self
            .request(&snapshot, reqwest::Method::GET, "/api/", None, false, None)
            .await
        {
            Ok((_, n)) => requests += n,
            Err((e, n)) => {
                return json!({
                    "ok": false, "state": "failed", "code": e.code, "detail": e.detail,
                    "backend": cfg.backend, "token_set": true,
                    "http_requests": requests + n, "latency_ms": started.elapsed().as_millis(),
                })
            }
        }
        let entity = &cfg.entities[0];
        match self.read_state(&snapshot, &entity.entity, None).await {
            Ok((v, n)) => json!({
                "ok": true, "state": "ready",
                "detail": "鉴权与实体读取通过；动作仍以写后回读为准",
                "backend": cfg.backend, "token_set": true,
                "entity": { "name": entity.name, "entity": entity.entity,
                            "state": v.get("state").cloned().unwrap_or(Value::Null) },
                "http_requests": requests + n,
                "latency_ms": started.elapsed().as_millis(),
            }),
            Err((e, n)) => json!({
                "ok": false, "state": "failed", "code": e.code, "detail": e.detail,
                "backend": cfg.backend, "token_set": true,
                "entity": { "name": entity.name, "entity": entity.entity },
                "http_requests": requests + n,
                "latency_ms": started.elapsed().as_millis(),
            }),
        }
    }

    /// R2-BM6.1: discover the household's entities from `GET /api/states`.
    ///
    /// The list is a PICKER, not the whitelist: nothing here becomes callable
    /// until the household saves the selected entries. Names are HA's
    /// `friendly_name`, cleaned and bounded; states are shown in the console
    /// but never injected into the prompt. `supported` marks the domains
    /// `home.set` can actually control; unsupported entities are still listed
    /// so the card can show the house instead of pretending it is empty.
    pub async fn discover(&self) -> Value {
        let started = Instant::now();
        let snapshot = self.snapshot();
        let cfg = &snapshot.config;
        // Discovery only needs the CONNECTION: an empty whitelist is the
        // reason to discover, not a reason to refuse.
        if let Some(what) = Self::missing_connection(&snapshot) {
            return json!({
                "ok": false, "state": "unconfigured", "detail": what,
                "backend": cfg.backend, "token_set": snapshot.token.is_some(),
                "entity_source": cfg.entity_source,
                "entities": [], "domains": {},
                "http_requests": 0, "latency_ms": 0,
            });
        }
        if let (crate::config::EntitySource::Exposed, Some(token)) =
            (cfg.entity_source, snapshot.token.as_deref())
        {
            return self.discover_exposed(&snapshot, token, started).await;
        }
        match self
            .request(
                &snapshot,
                reqwest::Method::GET,
                "/api/states",
                None,
                true,
                None,
            )
            .await
        {
            Ok((value, requests)) => {
                let rows = value.as_array().cloned().unwrap_or_default();
                let mut domains: std::collections::BTreeMap<String, u32> = Default::default();
                let mut entities: Vec<Value> = Vec::new();
                for item in rows.iter().take(MAX_DISCOVERED * 4) {
                    let Some(eid) = item.get("entity_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let eid = eid.trim();
                    // domain.object_id, no whitespace: HA ids are slugs. An
                    // odd id must not reach the console as a fake entity.
                    let Some((domain, object)) = eid.split_once('.') else {
                        continue;
                    };
                    if domain.is_empty()
                        || object.is_empty()
                        || eid.chars().any(|c| c.is_whitespace() || c.is_control())
                    {
                        continue;
                    }
                    let supported = crate::config::home_domain_supported(eid);
                    *domains.entry(domain.to_string()).or_default() += 1;
                    let friendly = item
                        .pointer("/attributes/friendly_name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| object.replace('_', " "));
                    entities.push(json!({
                        "entity": eid,
                        "name": clean_entity_name(&friendly),
                        "state": item.get("state").and_then(Value::as_str).unwrap_or(""),
                        "domain": domain,
                        "supported": supported,
                    }));
                    if entities.len() >= MAX_DISCOVERED {
                        break;
                    }
                }
                // Supported first, then a stable name order: the picker's
                // useful rows are at the top.
                entities.sort_by(|a, b| {
                    let sa = !a["supported"].as_bool().unwrap_or(false);
                    let sb = !b["supported"].as_bool().unwrap_or(false);
                    sa.cmp(&sb)
                        .then_with(|| a["domain"].as_str().cmp(&b["domain"].as_str()))
                        .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
                });
                let supported = entities
                    .iter()
                    .filter(|e| e["supported"].as_bool().unwrap_or(false))
                    .count();
                json!({
                    "ok": true, "state": "ready",
                    "detail": format!("发现 {} 个实体，其中 {} 个可控制", entities.len(), supported),
                    "backend": cfg.backend, "token_set": true,
                    "entities": entities,
                    "domains": domains,
                    "supported": supported,
                    "http_requests": requests,
                    "latency_ms": started.elapsed().as_millis(),
                })
            }
            Err((e, n)) => json!({
                "ok": false, "state": "failed", "code": e.code, "detail": e.detail,
                "backend": cfg.backend, "token_set": true,
                "entity_source": cfg.entity_source,
                "entities": [], "domains": {},
                "http_requests": n, "latency_ms": started.elapsed().as_millis(),
            }),
        }
    }

    /// R2-BM6.3: the EXPOSED directory. Home Assistant decides what Assist may
    /// talk to; this mode reads that decision (`conversation: true`) over its
    /// WebSocket API and never falls back to `/api/states` — a named failure
    /// is more useful than a silently different catalogue.
    ///
    /// The set comes from the WS command; names/states are enriched with one
    /// bounded `GET /api/states` when that read works. If the enrichment
    /// fails, the entities are still the exposed ones, named by their object
    /// id and with an empty state — never invented.
    async fn discover_exposed(
        &self,
        snapshot: &HomeSnapshot,
        token: &str,
        started: Instant,
    ) -> Value {
        let cfg = &snapshot.config;
        let timeout = Duration::from_millis(cfg.timeout_ms.clamp(500, 15_000));
        let ids = match exposure_list(&cfg.base_url, token, timeout).await {
            Ok(ids) => ids,
            Err((code, detail)) => {
                return json!({
                    "ok": false, "state": "unavailable", "code": code, "detail": detail,
                    "backend": cfg.backend, "token_set": true,
                    "entity_source": cfg.entity_source,
                    "entities": [], "domains": {},
                    "http_requests": 0, "ws_commands": 1,
                    "latency_ms": started.elapsed().as_millis(),
                    "note": "暴露列表读取失败：不回退到 /api/states（那会是另一套目录）",
                })
            }
        };
        // Enrichment is one bounded read, best effort by design.
        let mut names: std::collections::HashMap<String, (String, String)> = Default::default();
        let mut requests = 0usize;
        if let Ok((value, n)) = self
            .request(
                snapshot,
                reqwest::Method::GET,
                "/api/states",
                None,
                true,
                None,
            )
            .await
        {
            requests = n;
            if let Some(rows) = value.as_array() {
                for item in rows.iter().take(MAX_DISCOVERED * 4) {
                    let Some(eid) = item.get("entity_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let friendly = item
                        .pointer("/attributes/friendly_name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| {
                            eid.split_once('.')
                                .map(|(_, o)| o.replace('_', " "))
                                .unwrap_or_default()
                        });
                    let state = item
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    names.insert(eid.to_string(), (clean_entity_name(&friendly), state));
                }
            }
        }
        let mut domains: std::collections::BTreeMap<String, u32> = Default::default();
        let mut entities: Vec<Value> = Vec::new();
        for eid in &ids {
            let (domain, object) = match eid.split_once('.') {
                Some((d, o)) if !d.is_empty() && !o.is_empty() => (d, o),
                _ => continue,
            };
            let supported = crate::config::home_domain_supported(eid);
            *domains.entry(domain.to_string()).or_default() += 1;
            let (name, state) = names
                .get(eid)
                .cloned()
                .unwrap_or_else(|| (clean_entity_name(&object.replace('_', " ")), String::new()));
            entities.push(json!({
                "entity": eid,
                "name": name,
                "state": state,
                "domain": domain,
                "supported": supported,
            }));
        }
        entities.sort_by(|a, b| {
            let sa = !a["supported"].as_bool().unwrap_or(false);
            let sb = !b["supported"].as_bool().unwrap_or(false);
            sa.cmp(&sb)
                .then_with(|| a["domain"].as_str().cmp(&b["domain"].as_str()))
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        let supported = entities
            .iter()
            .filter(|e| e["supported"].as_bool().unwrap_or(false))
            .count();
        json!({
            "ok": true, "state": "ready", "entity_source": cfg.entity_source,
            "detail": format!(
                "Home Assistant 暴露给 Assist 的会话实体 {} 个，其中 {} 个可控制",
                entities.len(), supported
            ),
            "backend": cfg.backend, "token_set": true,
            "entities": entities,
            "domains": domains,
            "supported": supported,
            "http_requests": requests,
            "ws_commands": 1,
            "latency_ms": started.elapsed().as_millis(),
        })
    }
}

/// HA names are user-editable text (an entity can be called anything). The
/// console is the only consumer, but a control character or a 4,000-character
/// name must not reach it; `<`/`>` are stripped because the card renders text.
fn clean_entity_name(raw: &str) -> String {
    // Strip tags, not just the angle brackets: `<b>观影</b>` must not become
    // `b观影/b` in the picker.
    let mut cleaned = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag && !c.is_control() => cleaned.push(c),
            _ => {}
        }
    }
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(60).collect();
    if collapsed.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// One WebSocket frame from Home Assistant is bounded like every other
/// backend response (a 1 MiB exposed list is already a very large house).
const MAX_WS_MESSAGE: usize = 1 << 20;

/// R2-BM6.3: the exposed-for-conversation entity ids, over HA's WebSocket API
/// (`auth_required` → `auth` → `auth_ok` → `homeassistant/expose_entity/list`
/// → `result`). Every step is bounded by the same timeout, every frame by
/// [`MAX_WS_MESSAGE`]. A failure is NAMED (`HOME_EXPOSURE_UNAVAILABLE`) and
/// carries the step that failed — the caller never falls back to `/api/states`,
/// because that would silently show a different catalogue.
async fn exposure_list(
    base_url: &str,
    token: &str,
    timeout: Duration,
) -> Result<Vec<String>, (&'static str, String)> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    const CODE: &str = HOME_EXPOSURE_UNAVAILABLE;
    let base = base_url.trim_end_matches('/');
    let url = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/api/websocket")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/api/websocket")
    } else {
        return Err((CODE, format!("地址不是 http(s)：{base_url}")));
    };
    let (mut ws, _resp) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(&url))
        .await
        .map_err(|_| (CODE, format!("连接 {url} 超时")))?
        .map_err(|e| (CODE, format!("连接 {url} 失败: {e}")))?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut sent_auth = false;
    let mut command_sent = false;
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| (CODE, "等待 Home Assistant 回复超时".to_string()))?;
        let Some(msg) = next else {
            return Err((CODE, "连接在命令完成前关闭".into()));
        };
        let msg = msg.map_err(|e| (CODE, format!("WebSocket 读失败: {e}")))?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Close(_) => return Err((CODE, "连接在命令完成前关闭".into())),
            _ => continue, // ping/pong/frame: not protocol traffic
        };
        if text.len() > MAX_WS_MESSAGE {
            return Err((CODE, "暴露列表响应超过 1 MiB".into()));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| (CODE, format!("响应不是 JSON: {e}")))?;
        match v["type"].as_str().unwrap_or("") {
            "auth_required" if !sent_auth => {
                sent_auth = true;
                let auth = json!({ "type": "auth", "access_token": token });
                ws.send(Message::Text(auth.to_string().into()))
                    .await
                    .map_err(|e| (CODE, format!("发送 auth 失败: {e}")))?;
            }
            "auth_invalid" => {
                return Err((CODE, "Home Assistant 拒绝了令牌（auth_invalid）".into()))
            }
            "auth_ok" if !command_sent => {
                let cmd = json!({ "id": 1, "type": "homeassistant/expose_entity/list" });
                ws.send(Message::Text(cmd.to_string().into()))
                    .await
                    .map_err(|e| (CODE, format!("发送 expose_entity/list 失败: {e}")))?;
                command_sent = true;
            }
            "result" if command_sent && v["id"].as_u64() == Some(1) => {
                if v["success"].as_bool() != Some(true) {
                    let code = v
                        .pointer("/error/code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let msg = v
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    return Err((
                        CODE,
                        format!("expose_entity/list 失败: {code} {msg}")
                            .trim_end()
                            .to_string(),
                    ));
                }
                let _ = ws.close(None).await;
                return Ok(parse_exposed(&v["result"]));
            }
            _ => continue,
        }
    }
}

/// `result.exposed_entities[entity_id].conversation` is the Assist exposure
/// flag. Tolerant of a bare boolean value too (older/newer shapes), strict
/// about ids: only `domain.object` slugs reach the console.
fn parse_exposed(result: &Value) -> Vec<String> {
    let Some(map) = result.get("exposed_entities").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = map
        .iter()
        .filter(|(_, v)| {
            v.get("conversation")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| v.as_bool().unwrap_or(false))
        })
        .filter_map(|(id, _)| {
            let id = id.trim();
            let (domain, object) = id.split_once('.')?;
            if domain.is_empty()
                || object.is_empty()
                || id.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                return None;
            }
            Some(id.to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids.truncate(MAX_DISCOVERED);
    ids
}

/// Minimal percent-encoding for an entity id path segment (`light.foo` needs
/// no escaping, but a malformed id must not become a different URL).
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

pub struct HomeSetTool {
    rt: Arc<HomeRuntime>,
}

impl HomeSetTool {
    pub fn new(rt: Arc<HomeRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for HomeSetTool {
    fn descriptor(&self) -> ToolDescriptor {
        let snapshot = self.rt.snapshot();
        let names = HomeRuntime::allowed_names(&snapshot.config);
        let description = if HomeRuntime::missing(&snapshot).is_none() {
            format!(
                "开关家庭设备（Home Assistant）：灯/开关/风扇/输入布尔用 on/off，窗帘用 open/close。entity 必须使用白名单里的名字：{names}。结果以写后回读为准"
            )
        } else {
            "开关家庭设备（需要已接入的 Home Assistant；未接入时不会执行任何动作）".into()
        };
        ToolDescriptor {
            name: "home.set".into(),
            service: "home".into(),
            side_effect: SideEffect::Mutate,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "entity": { "type": "string", "description": format!("白名单里的实体名字（{names}）") },
                    "state": { "type": "string", "enum": ["on", "off", "open", "close"] }
                },
                "required": ["entity", "state"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_lan_exempt() {
            return ToolResult::failed(
                "PAIRING_REQUIRED",
                "未认证的局域网访问不允许控制智能家居设备，请使用配对令牌认证",
                false,
            );
        }
        let snapshot = self.rt.snapshot();
        let cfg = &snapshot.config;
        // Checked BEFORE anything else: no backend = no request, no fact that
        // could be mistaken for an observation.
        if let Some(what) = HomeRuntime::missing(&snapshot) {
            return ToolResult::failed(
                HOME_UNCONFIGURED,
                format!("家庭自动化还没有接入（{what}）；没有执行任何动作，也不要声称设备的状态"),
                false,
            );
        }
        let Some(requested) = args.get("entity").and_then(Value::as_str) else {
            return ToolResult::failed("BAD_ARGS", "缺少 entity", false);
        };
        let requested_state = match args.get("state").and_then(Value::as_str) {
            Some(s @ ("on" | "off" | "open" | "close")) => s,
            _ => {
                return ToolResult::failed("BAD_ARGS", "state 必须是 on/off/open/close", false);
            }
        };
        let Some(entity) = HomeRuntime::find_entity(cfg, requested) else {
            return ToolResult::failed(
                HOME_ENTITY_NOT_ALLOWED,
                format!(
                    "「{requested}」不在实体白名单里（可用：{}）；没有发出任何请求",
                    HomeRuntime::allowed_names(cfg)
                ),
                false,
            )
            .with_fact(ToolFact {
                subject: requested.to_string(),
                action: requested_state.to_string(),
                basis: FactBasis::ReadBack,
                confirmed: false,
                detail: Some("这个实体不在白名单里".into()),
                value: None,
            });
        };
        let domain = entity.entity.split('.').next().unwrap_or("");
        if !crate::config::home_domain_supported(&entity.entity) {
            return ToolResult::failed(
                HOME_UNSUPPORTED_DOMAIN,
                format!(
                    "「{}」不在可控域（只支持 {}）；没有发出任何请求",
                    entity.entity,
                    crate::config::HOME_SET_DOMAINS.join("/")
                ),
                false,
            );
        }
        // The requested verb maps to an HA service and the state HA must
        // report back. Covers speak open/close but HA reports open/closed.
        let (service, action, readback) = match (domain, requested_state) {
            ("cover", "open") => ("open_cover", "open", "open"),
            ("cover", "close") => ("close_cover", "close", "closed"),
            ("cover", _) => {
                return ToolResult::failed("BAD_ARGS", "窗帘只能用 open 或 close", false)
            }
            (_, "on") => ("turn_on", "on", "on"),
            (_, "off") => ("turn_off", "off", "off"),
            _ => {
                return ToolResult::failed(
                    "BAD_ARGS",
                    "灯/开关/风扇/输入布尔只能用 on 或 off",
                    false,
                )
            }
        };
        let action = action.to_string();
        let cancel = ctx.cancel_flag();
        match self
            .rt
            .set_entity(
                &snapshot,
                &entity.entity,
                domain,
                service,
                readback,
                Some(&cancel),
            )
            .await
        {
            Ok(readback) => ToolResult {
                status: ToolStatus::Completed,
                evidence: Evidence::DeviceConfirmed,
                result: Some(json!({
                    "confirmed": true,
                    "entity": entity.entity, "name": entity.name, "state": requested_state,
                    "readback": readback,
                })),
                error: None,
                fact: Some(ToolFact {
                    subject: entity.name.clone(),
                    action,
                    basis: FactBasis::ReadBack,
                    confirmed: true,
                    detail: None,
                    value: None,
                }),
            },
            Err((e, _, observed, post_accepted)) => {
                if e.code == HOME_CANCELLED && !post_accepted {
                    // Nothing was submitted: the registry's cancellation
                    // result is the honest one.
                    return ToolResult::cancelled();
                }
                // A read-back failure AFTER the service accepted the write is
                // an unknown ACTION outcome, not a failed action: HA may have
                // applied it (audit Q2/Q2-bug). Only a failed POST is Failed.
                let (status, evidence, code, detail) = if post_accepted {
                    (
                        ToolStatus::Unknown,
                        Evidence::Unknown,
                        if e.code == HOME_READBACK_MISMATCH {
                            HOME_READBACK_MISMATCH
                        } else {
                            HOME_READBACK_UNOBSERVED
                        },
                        format!("服务已接受，但没能读回状态：{}", e.detail),
                    )
                } else if e.code == HOME_AUTH || e.code == HOME_HTTP || e.code == HOME_UNREACHABLE {
                    (
                        ToolStatus::Failed,
                        Evidence::Submitted,
                        e.code,
                        e.detail.clone(),
                    )
                } else {
                    (
                        ToolStatus::Unknown,
                        Evidence::Unknown,
                        e.code,
                        e.detail.clone(),
                    )
                };
                let unconfirmed = ToolResult {
                    status,
                    evidence,
                    result: Some(json!({
                        "confirmed": false, "entity": entity.entity, "name": entity.name,
                        "requested": requested_state, "observed": observed,
                    })),
                    error: Some(crate::tool::ToolError {
                        code: code.into(),
                        retryable: e.retryable,
                        detail: detail.clone(),
                    }),
                    fact: None,
                };
                unconfirmed.with_fact(ToolFact {
                    subject: entity.name.clone(),
                    action,
                    basis: FactBasis::ReadBack,
                    confirmed: false,
                    detail: Some(detail),
                    value: None,
                })
            }
        }
    }
}

/// The service's state, from the backend that would actually run it.
pub fn status_json(rt: &HomeRuntime) -> Value {
    status_from(&rt.snapshot())
}

pub(crate) fn status_from(snapshot: &HomeSnapshot) -> Value {
    let cfg = &snapshot.config;
    let availability = if HomeRuntime::missing(snapshot).is_some() {
        Availability::Unconfigured
    } else {
        Availability::Ready
    };
    let state = match availability {
        Availability::Ready => "ready",
        Availability::Unconfigured => "unconfigured",
        Availability::Disabled => "disabled",
        Availability::Hidden => "hidden",
    };
    json!({
        "backend": cfg.backend,
        // R2-BM6.3: which directory mechanism the card must render.
        "entity_source": cfg.entity_source,
        "availability": availability,
        "state": state,
        "detail": HomeRuntime::detail(snapshot),
        "tools": ["home.set"],
        // The user-facing promise the card carries: the assistant will say so.
        "claim": if availability == Availability::Ready {
            "动作结果以设备回读为准；没读回就不会说已完成"
        } else {
            "助手不会声称能控制设备或设备已改变"
        },
        "base_url": if HomeConfig::endpoint_is_portable(&cfg.base_url) { cfg.base_url.as_str() } else { "" },
        // Write-only: the value never leaves the secrets table.
        "token_set": snapshot.token.is_some(),
        "entities": cfg.entities,
        "timeout_ms": cfg.timeout_ms,
        "retries": cfg.retries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigService;
    use crate::tool::{Registry, ToolCtx, ToolStatus};
    use std::sync::atomic::AtomicBool;

    fn ctx() -> ToolCtx {
        ToolCtx::new("T1", "C1", Arc::new(AtomicBool::new(false)))
    }

    fn registry_with(cfg: HomeConfig) -> Registry {
        let store = Arc::new(Store::open_in_memory().unwrap());
        ConfigService::new(store.clone())
            .save_home(cfg, "test")
            .unwrap();
        let rt = HomeRuntime::new(store);
        let mut reg = Registry::new();
        reg.register(Arc::new(HomeSetTool::new(rt)))
            .expect("valid unique tool registration");
        reg
    }

    /// R1j/P0-b: the exact 2026-11-12 failure. With no backend, asking the
    /// assistant to turn off a light must produce a NAMED failure the model
    /// can relay — never `Completed`/`DeviceConfirmed` out of thin air.
    #[tokio::test]
    async fn an_unconfigured_home_never_confirms_anything() {
        let reg = registry_with(HomeConfig::default());
        let r = reg
            .call(
                "home.set",
                json!({ "entity": "children_room.light", "state": "off" }),
                &ctx(),
            )
            .await;
        assert_eq!(r.status, ToolStatus::Failed);
        assert_ne!(r.evidence, crate::tool::Evidence::DeviceConfirmed);
        assert_eq!(r.error.as_ref().unwrap().code, HOME_UNCONFIGURED);
        assert!(r.result.is_none(), "a refusal carries no state claim");
        assert!(
            r.fact.is_none(),
            "and no fact the core could speak as an observation"
        );

        // The prompt must see it as registered-but-unusable, not as absent
        // and not as ready.
        let caps = reg.capabilities();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].service, "home");
        assert_eq!(caps[0].availability, Availability::Unconfigured);
        assert!(reg.ready_capabilities().is_empty());
        assert_eq!(reg.unavailable_services().len(), 1);

        let prompt = crate::agent::prompt::system_prompt(
            "高达",
            &[][..],
            &caps,
            crate::agent::prompt::Delivery::Text,
        );
        assert!(prompt.contains("已注册但当前不能用"));
        assert!(
            prompt.contains("home"),
            "the model is told the service exists but cannot run: {prompt}"
        );
    }

    /// An incomplete Home Assistant block is still unconfigured, with a
    /// detail that names the missing piece (never "disabled").
    #[tokio::test]
    async fn an_incomplete_home_assistant_block_is_unconfigured_not_ready() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let cfg = ConfigService::new(store.clone())
            .save_home(
                HomeConfig {
                    backend: HomeBackend::HomeAssistant,
                    base_url: "http://ha.local:8123".into(),
                    entities: vec![HomeEntity {
                        name: "儿童房的灯".into(),
                        entity: "light.children".into(),
                    }],
                    ..Default::default()
                },
                "test",
            )
            .unwrap();
        assert_eq!(cfg.timeout_ms, 4000);
        let rt = HomeRuntime::new(store);
        assert_eq!(rt.availability(), Availability::Unconfigured);
        let status = status_json(&rt);
        assert_eq!(status["state"], "unconfigured");
        assert_eq!(status["token_set"], false);
        assert!(
            status["detail"].as_str().unwrap().contains("令牌"),
            "{status}"
        );
        // And no request path exists without the token.
        let mut reg = Registry::new();
        reg.register(Arc::new(HomeSetTool::new(rt)))
            .expect("valid unique tool registration");
        let r = reg
            .call(
                "home.set",
                json!({"entity":"儿童房的灯","state":"off"}),
                &ctx(),
            )
            .await;
        assert_eq!(r.status, ToolStatus::Failed);
        assert_eq!(r.error.as_ref().unwrap().code, HOME_UNCONFIGURED);
    }

    /// The whitelist is the model's ONLY vocabulary: an invented name is a
    /// named refusal before any HTTP, and it carries an unconfirmed fact so
    /// the core tells the user the truth.
    #[tokio::test]
    async fn an_invented_entity_is_refused_before_any_request() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let cfg = ConfigService::new(store.clone())
            .save_home(
                HomeConfig {
                    backend: HomeBackend::HomeAssistant,
                    base_url: "http://127.0.0.1:1".into(),
                    entities: vec![HomeEntity {
                        name: "儿童房的灯".into(),
                        entity: "light.children".into(),
                    }],
                    ..Default::default()
                },
                "test",
            )
            .unwrap();
        ConfigService::new(store.clone())
            .save_home_token(Some("t"))
            .unwrap();
        let rt = HomeRuntime::new(store);
        assert_eq!(rt.availability(), Availability::Ready, "{cfg:?}");
        let mut reg = Registry::new();
        reg.register(Arc::new(HomeSetTool::new(rt)))
            .expect("valid unique tool registration");
        let r = reg
            .call(
                "home.set",
                json!({ "entity": "children_room.light", "state": "off" }),
                &ctx(),
            )
            .await;
        assert_eq!(r.status, ToolStatus::Failed);
        assert_eq!(r.error.as_ref().unwrap().code, HOME_ENTITY_NOT_ALLOWED);
        assert_ne!(r.evidence, Evidence::DeviceConfirmed);
        let fact = r
            .fact
            .clone()
            .expect("a refused target still yields an unconfirmed fact");
        assert!(!fact.confirmed);
        assert_eq!(
            crate::agent::facts::render_fact(&r, &fact),
            "没有完成children_room.light的关闭：这个实体不在白名单里。"
        );
    }

    /// 2026-11-13: a 39-entity house hid 「儿童房主灯」 from the model because
    /// the name list stopped at ten. Every whitelisted name must be visible —
    /// the save path caps the list at 64, which is the only bound.
    #[test]
    fn the_model_sees_every_whitelisted_name_not_just_ten() {
        let entities: Vec<HomeEntity> = (1..=39)
            .map(|i| HomeEntity {
                name: format!("实体{i:02}"),
                entity: format!("light.e{i:02}"),
            })
            .collect();
        let cfg = HomeConfig {
            entities,
            ..Default::default()
        };
        let names = HomeRuntime::allowed_names(&cfg);
        for i in 1..=39 {
            assert!(
                names.contains(&format!("实体{i:02}")),
                "missing 实体{i:02}: {names}"
            );
        }
        assert_eq!(
            names.matches('、').count(),
            38,
            "one separator between 39 names"
        );
    }

    /// The card's state word comes from the backend, not from the UI.
    #[test]
    fn the_status_word_is_the_backends() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let status = status_json(&HomeRuntime::new(store));
        assert_eq!(status["state"], "unconfigured");
        assert_eq!(status["backend"], serde_json::json!(HomeBackend::None));
    }

    #[test]
    fn home_config_validation_rejects_bad_blocks() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let svc = ConfigService::new(store);
        let bad = HomeConfig {
            backend: HomeBackend::HomeAssistant,
            base_url: "ftp://ha".into(),
            ..Default::default()
        };
        assert!(svc.save_home(bad, "test").is_err());
        let loopback = HomeConfig {
            backend: HomeBackend::HomeAssistant,
            base_url: "http://127.0.0.1:8123".into(),
            ..Default::default()
        };
        assert!(
            svc.save_home(loopback, "test").is_err(),
            "production store must reject loopback base_url on home"
        );
        let dup = HomeConfig {
            base_url: "http://ha:8123".into(),
            entities: vec![
                HomeEntity {
                    name: "灯".into(),
                    entity: "light.a".into(),
                },
                HomeEntity {
                    name: "灯".into(),
                    entity: "light.b".into(),
                },
            ],
            ..Default::default()
        };
        assert!(svc.save_home(dup, "test").is_err());
    }
}
