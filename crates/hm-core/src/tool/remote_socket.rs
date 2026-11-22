//! Remote Computer Socket Client (#21).
//!
//! Calls an external process / HTTP service on another machine that implements
//! the Home Mate tool contract (query / mutating action + receipt).
//!
//! Hard boundaries:
//! - `hm-core` and `voice` NEVER contain `shell_command` or local shell execution.
//! - Unconfigured or disabled = ZERO HTTP requests.
//! - When the remote service is stopped or unreachable, return an honest
//!   unconfigured/unreachable failure (never fabricate success).
//! - Mutating actions require a confirmed receipt (`ToolFact`) from the remote
//!   service; an unconfirmed 2xx response is treated as `Unknown`.

use super::argument_schema as schema;

use crate::config_delivery::{ConfigDelivery, DeliveryError, RemoteSocketSnapshot};
use crate::store::Store;
use crate::tool::{
    Availability, DynamicToolProvider, Evidence, FactBasis, SideEffect, Tool, ToolCtx,
    ToolDescriptor, ToolError, ToolFact, ToolResult, ToolStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const REMOTE_SOCKET_CONFIG_KEY: &str = "remote_socket";
pub const REMOTE_SOCKET_SECRET_KEY: &str = "remote_socket.token";
pub const MAX_RESPONSE_BODY: usize = 64 * 1024;

pub const REMOTE_SOCKET_UNCONFIGURED: &str = "REMOTE_SOCKET_UNCONFIGURED";
pub const REMOTE_SOCKET_DISABLED: &str = "REMOTE_SOCKET_DISABLED";
pub const REMOTE_SOCKET_ARGUMENTS: &str = "REMOTE_SOCKET_ARGUMENTS";
pub const REMOTE_SOCKET_UNREACHABLE: &str = "REMOTE_SOCKET_UNREACHABLE";
pub const REMOTE_SOCKET_TIMEOUT: &str = "REMOTE_SOCKET_TIMEOUT";
pub const REMOTE_SOCKET_HTTP: &str = "REMOTE_SOCKET_HTTP";

fn default_service_name() -> String {
    "computer".into()
}

fn default_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteCapabilitySpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_query_effect")]
    pub side_effect: SideEffect,
    #[serde(default = "default_schema")]
    pub parameters: Value,
}

fn default_query_effect() -> SideEffect {
    SideEffect::Query
}

fn default_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteSocketConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint_url: String,
    #[serde(default = "default_service_name")]
    pub service_name: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub capabilities: Vec<RemoteCapabilitySpec>,
}

impl Default for RemoteSocketConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint_url: String::new(),
            service_name: default_service_name(),
            timeout_ms: default_timeout_ms(),
            capabilities: Vec::new(),
        }
    }
}

pub fn validate_remote_config(cfg: &mut RemoteSocketConfig) -> Result<(), String> {
    let endpoint = cfg.endpoint_url.trim();
    if endpoint.chars().count() > 4096
        || endpoint
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\')
        || (!endpoint.is_empty()
            && !reqwest::Url::parse(endpoint).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            }))
    {
        return Err(
            "endpoint_url must be an HTTP(S) base without inline credentials, query or fragment"
                .into(),
        );
    }
    cfg.endpoint_url = endpoint.trim_end_matches('/').to_owned();
    cfg.service_name = cfg.service_name.trim().to_owned();
    if cfg.service_name.is_empty() {
        cfg.service_name = default_service_name();
    }
    if cfg.service_name.chars().count() > 64 || cfg.service_name.chars().any(char::is_control) {
        return Err("invalid service_name".into());
    }
    cfg.timeout_ms = cfg.timeout_ms.clamp(500, 30_000);
    if cfg.enabled && cfg.endpoint_url.is_empty() {
        return Err("endpoint_url is required when remote_socket is enabled".into());
    }
    if cfg.capabilities.len() > MAX_REMOTE_CAPABILITIES {
        return Err("too many remote capabilities".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for capability in &mut cfg.capabilities {
        capability.name = capability.name.trim().to_owned();
        capability.description = capability.description.trim().to_owned();
        if capability.name.is_empty()
            || capability.name.chars().count() > 64
            || capability.name.chars().any(char::is_control)
            || capability.description.chars().count() > 4096
            || capability.description.contains('\0')
        {
            return Err("invalid capability name or description".into());
        }
        if !seen.insert(capability.name.clone()) {
            return Err("duplicate remote capability name".into());
        }
        if !schema::valid_parameters(&capability.parameters) {
            return Err("capability parameters contain an invalid or unsupported schema".into());
        }
    }
    Ok(())
}

pub(crate) const MAX_REMOTE_CAPABILITIES: usize = 32;

impl RemoteSocketConfig {
    pub(crate) fn availability(&self) -> Availability {
        if self.endpoint_url.is_empty() {
            Availability::Unconfigured
        } else if !self.enabled {
            Availability::Disabled
        } else {
            Availability::Ready
        }
    }
}

pub struct RemoteSocketRuntime {
    store: Arc<Store>,
    client: reqwest::Client,
}

impl RemoteSocketRuntime {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        let client = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
        )
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("build remote socket HTTP client")
        });
        Self::with_client(store, client)
    }

    pub fn with_client(store: Arc<Store>, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self { store, client })
    }

    pub(crate) fn snapshot(&self) -> Result<RemoteSocketSnapshot, DeliveryError> {
        ConfigDelivery::remote_socket_snapshot(&self.store)
    }

    pub fn config(&self) -> RemoteSocketConfig {
        self.snapshot().unwrap_or_default().config
    }

    pub fn availability(&self) -> Availability {
        self.config().availability()
    }

    pub fn token_set(&self) -> bool {
        self.snapshot()
            .is_ok_and(|snapshot| snapshot.token.is_some())
    }

    pub fn revision(&self) -> String {
        self.snapshot().unwrap_or_default().revision
    }

    pub fn status_json(&self) -> Value {
        Self::status(self.snapshot().unwrap_or_default())
    }

    fn status(snapshot: RemoteSocketSnapshot) -> Value {
        let cfg = snapshot.config;
        let avail = cfg.availability();
        json!({
            "enabled": cfg.enabled,
            "endpoint_url": cfg.endpoint_url,
            "service_name": cfg.service_name,
            "timeout_ms": cfg.timeout_ms,
            "capabilities": cfg.capabilities,
            "token_set": snapshot.token.is_some(),
            "availability": avail,
            "state": match avail {
                Availability::Ready => "ready",
                Availability::Unconfigured => "unconfigured",
                Availability::Disabled => "disabled",
                Availability::Hidden => "hidden",
            },
            "revision": snapshot.revision,
        })
    }

    pub fn save(
        &self,
        cfg: RemoteSocketConfig,
        token: Option<&str>,
        actor: &str,
    ) -> Result<Value, String> {
        self.save_expected(cfg, token, actor, None)
            .map_err(|error| error.detail)
    }

    pub(crate) fn save_expected(
        &self,
        cfg: RemoteSocketConfig,
        token: Option<&str>,
        actor: &str,
        expected: Option<&str>,
    ) -> Result<Value, DeliveryError> {
        ConfigDelivery::save_remote_socket(&self.store, cfg, token, actor, expected, None)
            .map(Self::status)
    }

    pub(crate) fn restore(
        &self,
        cfg: RemoteSocketConfig,
        actor: &str,
        revision: &str,
        source_node: &str,
    ) -> Result<(), DeliveryError> {
        ConfigDelivery::save_remote_socket(
            &self.store,
            cfg,
            None,
            actor,
            Some(revision),
            Some(source_node),
        )
        .map(|_| ())
    }

    pub(crate) fn export(&self) -> Result<Option<Value>, DeliveryError> {
        let snapshot = self.snapshot()?;
        if !snapshot.verified {
            return Ok(None);
        }
        let mut public = serde_json::to_value(snapshot.config)?;
        public["token_set"] = json!(snapshot.token.is_some());
        Ok(Some(public))
    }

    /// Real probe: verifies that the remote service responds on `/tools`.
    /// Unconfigured or disabled = ZERO HTTP requests.
    pub async fn probe(&self) -> Value {
        let snapshot = self.snapshot().unwrap_or_default();
        let avail = snapshot.config.availability();
        if avail != Availability::Ready {
            return json!({
                "ok": false,
                "state": match avail {
                    Availability::Unconfigured => "unconfigured",
                    Availability::Disabled => "disabled",
                    _ => "unavailable",
                },
                "http_requests": 0,
                "detail": "remote_socket is not configured or disabled",
            });
        }
        let cfg = snapshot.config;
        let url = format!("{}/tools", cfg.endpoint_url);
        let start = Instant::now();
        let mut req = self
            .client
            .get(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms));
        if let Some(token) = snapshot.token.as_deref() {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let ok = resp.status().is_success();
                json!({
                    "ok": ok,
                    "state": if ok { "ready" } else { "error" },
                    "http_status": status,
                    "latency_ms": start.elapsed().as_millis() as u64,
                    "http_requests": 1,
                })
            }
            Err(e) => json!({
                "ok": false,
                "state": "unreachable",
                "latency_ms": start.elapsed().as_millis() as u64,
                "http_requests": 1,
                "detail": e.to_string(),
            }),
        }
    }

    /// Discover remote capabilities from `GET {endpoint_url}/tools`.
    /// Explicit setup discovery can inspect a saved endpoint before enabling calls.
    pub async fn discover(&self) -> Value {
        let snapshot = self.snapshot().unwrap_or_default();
        let cfg = snapshot.config;
        if cfg.endpoint_url.is_empty() {
            return json!({
                "ok": false,
                "error": REMOTE_SOCKET_UNCONFIGURED,
                "capabilities": [],
                "http_requests": 0,
            });
        }
        let url = format!("{}/tools", cfg.endpoint_url);
        let mut req = self
            .client
            .get(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms));
        if let Some(token) = snapshot.token.as_deref() {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
                let mut caps = body
                    .get("tools")
                    .or_else(|| body.get("capabilities"))
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                if let Some(token) = snapshot.token.as_deref() {
                    crate::tool::openapi::redact_secret(&mut caps, token);
                }
                json!({
                    "ok": true,
                    "capabilities": caps,
                    "http_requests": 1,
                })
            }
            Ok(resp) => json!({
                "ok": false,
                "error": REMOTE_SOCKET_HTTP,
                "http_status": resp.status().as_u16(),
                "capabilities": [],
                "http_requests": 1,
            }),
            Err(e) => json!({
                "ok": false,
                "error": REMOTE_SOCKET_UNREACHABLE,
                "detail": e.to_string(),
                "capabilities": [],
                "http_requests": 1,
            }),
        }
    }

    async fn call_remote(
        &self,
        snapshot: &RemoteSocketSnapshot,
        expected_effect: SideEffect,
        capability: &str,
        arguments: Value,
        subject: Option<&str>,
        ctx: &ToolCtx,
    ) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let avail = snapshot.config.availability();
        let make_unconfirmed_fact = |detail: String| -> Option<ToolFact> {
            if expected_effect == SideEffect::Mutate {
                Some(ToolFact {
                    subject: subject
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(capability)
                        .to_string(),
                    action: "other".into(),
                    basis: FactBasis::DeviceReceipt,
                    confirmed: false,
                    value: None,
                    detail: Some(detail),
                })
            } else {
                None
            }
        };

        match avail {
            Availability::Unconfigured => {
                let mut r = ToolResult::failed(
                    REMOTE_SOCKET_UNCONFIGURED,
                    "远端电脑插座尚未配置（没有发出任何请求）",
                    false,
                );
                r.fact = make_unconfirmed_fact("远端电脑插座未配置".into());
                return r;
            }
            Availability::Disabled | Availability::Hidden => {
                let mut r = ToolResult::failed(
                    REMOTE_SOCKET_DISABLED,
                    "远端电脑插座已关闭（没有发出任何请求）",
                    false,
                );
                r.fact = make_unconfirmed_fact("远端电脑插座已关闭".into());
                return r;
            }
            Availability::Ready => {}
        }

        let cfg = &snapshot.config;
        if !cfg.capabilities.is_empty() {
            let Some(spec) = cfg
                .capabilities
                .iter()
                .find(|spec| spec.name == capability && spec.side_effect == expected_effect)
            else {
                return ToolResult::failed(
                    REMOTE_SOCKET_ARGUMENTS,
                    "当前能力列表未声明该名称和操作类型",
                    false,
                );
            };
            if let Err(detail) = schema::validate_arguments(&arguments, &spec.parameters) {
                return ToolResult::failed(REMOTE_SOCKET_ARGUMENTS, detail, false);
            }
        }
        let url = format!("{}/call", cfg.endpoint_url);
        let payload = json!({
            "tool": capability,
            "side_effect": match expected_effect {
                SideEffect::Query => "query",
                SideEffect::Mutate => "mutate",
            },
            "arguments": arguments,
            "task_id": ctx.task_id,
            "call_id": ctx.call_id,
        });

        let mut req = self
            .client
            .post(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .json(&payload);
        if let Some(token) = snapshot.token.as_deref() {
            req = req.bearer_auth(token);
        }

        let resp = tokio::select! {
            r = req.send() => r,
            _ = async {
                while !ctx.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => {
                return ToolResult::cancelled();
            }
        };

        let resp = match resp {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                let mut r = ToolResult {
                    status: ToolStatus::Unknown,
                    evidence: Evidence::Unknown,
                    result: None,
                    error: Some(ToolError {
                        code: REMOTE_SOCKET_TIMEOUT.into(),
                        retryable: true,
                        detail: format!("远端电脑服务请求超时: {e}"),
                    }),
                    fact: None,
                };
                r.fact = make_unconfirmed_fact("请求超时".into());
                return r;
            }
            Err(e) => {
                let mut r = ToolResult::failed(
                    REMOTE_SOCKET_UNREACHABLE,
                    format!("远端电脑服务未启动或不可达: {e}"),
                    true,
                );
                r.fact = make_unconfirmed_fact("远端服务不可达".into());
                return r;
            }
        };

        let status = resp.status();
        let bytes = match resp.bytes().await {
            Ok(b) if b.len() <= MAX_RESPONSE_BODY => b,
            Ok(_) => {
                let mut r = ToolResult::failed(
                    REMOTE_SOCKET_HTTP,
                    "remote socket response exceeded 64 KiB",
                    false,
                );
                r.fact = make_unconfirmed_fact("响应过大".into());
                return r;
            }
            Err(e) => {
                let mut r = ToolResult::failed(
                    REMOTE_SOCKET_HTTP,
                    format!("failed reading remote response: {e}"),
                    true,
                );
                r.fact = make_unconfirmed_fact("读取响应失败".into());
                return r;
            }
        };

        if !status.is_success() {
            let mut r = ToolResult::failed(
                REMOTE_SOCKET_HTTP,
                format!("远端电脑服务返回 HTTP {}", status.as_u16()),
                status.is_server_error(),
            );
            r.fact = make_unconfirmed_fact(format!("HTTP {}", status.as_u16()));
            return r;
        }

        // Remove credential echoes before either ToolResult or fallback output is built.
        let mut body: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&bytes).to_string() }));
        if let Some(token) = snapshot.token.as_deref() {
            crate::tool::openapi::redact_secret(&mut body, token);
        }
        match serde_json::from_value::<ToolResult>(body.clone()) {
            Ok(mut remote_res) => {
                if expected_effect == SideEffect::Mutate {
                    let confirmed = remote_res.status == ToolStatus::Completed
                        && remote_res.evidence == Evidence::DeviceConfirmed
                        && remote_res
                            .fact
                            .as_ref()
                            .map(|f| f.confirmed)
                            .unwrap_or(false);
                    if !confirmed {
                        if remote_res.status == ToolStatus::Completed {
                            remote_res.status = ToolStatus::Unknown;
                            remote_res.evidence = Evidence::Accepted;
                        }
                        if remote_res.fact.is_none() {
                            remote_res.fact =
                                make_unconfirmed_fact("远端电脑服务未返回确认回执".into());
                        } else if let Some(f) = remote_res.fact.as_mut() {
                            f.confirmed = false;
                        }
                    }
                }
                remote_res
            }
            Err(_) => match expected_effect {
                SideEffect::Query => ToolResult::observed_ok(body),
                SideEffect::Mutate => ToolResult {
                    status: ToolStatus::Unknown,
                    evidence: Evidence::Accepted,
                    result: Some(body),
                    error: Some(ToolError {
                        code: "REMOTE_SOCKET_UNCONFIRMED".into(),
                        retryable: false,
                        detail: "远端电脑服务未按工具合同返回确认回执".into(),
                    }),
                    fact: make_unconfirmed_fact("未按工具合同返回回执".into()),
                },
            },
        }
    }
}

pub(crate) struct RemoteSocketProvider(pub Arc<RemoteSocketRuntime>);

impl DynamicToolProvider for RemoteSocketProvider {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let snapshot = Arc::new(self.0.snapshot().unwrap_or_default());
        vec![
            Arc::new(ComputerQueryTool {
                rt: self.0.clone(),
                snapshot: snapshot.clone(),
            }),
            Arc::new(ComputerActionTool {
                rt: self.0.clone(),
                snapshot,
            }),
        ]
    }
}

pub struct ComputerQueryTool {
    rt: Arc<RemoteSocketRuntime>,
    snapshot: Arc<RemoteSocketSnapshot>,
}

impl ComputerQueryTool {
    pub fn new(rt: Arc<RemoteSocketRuntime>) -> Self {
        let snapshot = Arc::new(rt.snapshot().unwrap_or_default());
        Self { rt, snapshot }
    }
}

#[async_trait::async_trait]
impl Tool for ComputerQueryTool {
    fn descriptor(&self) -> ToolDescriptor {
        let cfg = &self.snapshot.config;
        let cap_names: Vec<String> = cfg
            .capabilities
            .iter()
            .filter(|c| c.side_effect == SideEffect::Query)
            .map(|c| {
                if c.description.is_empty() {
                    c.name.clone()
                } else {
                    format!("{}（{}）", c.name, c.description)
                }
            })
            .collect();
        let description = if cfg.availability() == Availability::Ready {
            if cap_names.is_empty() {
                format!("向远端服务 {} 发起只读查询。", cfg.service_name)
            } else {
                format!(
                    "向远端服务 {} 发起只读查询。可用能力：{}。",
                    cfg.service_name,
                    cap_names.join("、")
                )
            }
        } else {
            "远端电脑只读查询（尚未配置或已关闭：不会发出任何请求）。".into()
        };
        ToolDescriptor {
            name: "computer.query".into(),
            service: "computer".into(),
            side_effect: SideEffect::Query,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "capability": {
                        "type": "string",
                        "description": "远端电脑服务提供的只读能力名称"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "传递给远端能力的 JSON 参数"
                    }
                },
                "required": ["capability"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.snapshot.config.availability()
    }

    fn catalog_revision(&self) -> Option<String> {
        Some(self.snapshot.revision.clone())
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(cap) = args
            .get("capability")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::failed(REMOTE_SOCKET_ARGUMENTS, "缺少 capability 参数", false);
        };
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        self.rt
            .call_remote(&self.snapshot, SideEffect::Query, cap, arguments, None, ctx)
            .await
    }
}

pub struct ComputerActionTool {
    rt: Arc<RemoteSocketRuntime>,
    snapshot: Arc<RemoteSocketSnapshot>,
}

impl ComputerActionTool {
    pub fn new(rt: Arc<RemoteSocketRuntime>) -> Self {
        let snapshot = Arc::new(rt.snapshot().unwrap_or_default());
        Self { rt, snapshot }
    }
}

#[async_trait::async_trait]
impl Tool for ComputerActionTool {
    fn descriptor(&self) -> ToolDescriptor {
        let cfg = &self.snapshot.config;
        let cap_names: Vec<String> = cfg
            .capabilities
            .iter()
            .filter(|c| c.side_effect == SideEffect::Mutate)
            .map(|c| {
                if c.description.is_empty() {
                    c.name.clone()
                } else {
                    format!("{}（{}）", c.name, c.description)
                }
            })
            .collect();
        let description = if cfg.availability() == Availability::Ready {
            if cap_names.is_empty() {
                format!("向远端服务 {} 发起带回执的动作请求。", cfg.service_name)
            } else {
                format!(
                    "向远端服务 {} 发起带回执的动作请求。可用动作：{}。",
                    cfg.service_name,
                    cap_names.join("、")
                )
            }
        } else {
            "远端电脑动作插座（尚未配置或已关闭：不会发出任何请求）。".into()
        };
        ToolDescriptor {
            name: "computer.action".into(),
            service: "computer".into(),
            side_effect: SideEffect::Mutate,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "capability": {
                        "type": "string",
                        "description": "远端电脑服务提供的动作能力名称"
                    },
                    "subject": {
                        "type": "string",
                        "description": "动作对象的名称（用于回执播报）"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "传递给远端能力的 JSON 参数"
                    }
                },
                "required": ["capability"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.snapshot.config.availability()
    }

    fn catalog_revision(&self) -> Option<String> {
        Some(self.snapshot.revision.clone())
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(cap) = args
            .get("capability")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::failed(REMOTE_SOCKET_ARGUMENTS, "缺少 capability 参数", false);
        };
        let subject = args.get("subject").and_then(Value::as_str);
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        self.rt
            .call_remote(
                &self.snapshot,
                SideEffect::Mutate,
                cap,
                arguments,
                subject,
                ctx,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn ctx() -> ToolCtx {
        ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)))
    }

    #[tokio::test]
    async fn unconfigured_remote_socket_makes_zero_http_and_never_fabricates() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = RemoteSocketRuntime::new(store);

        assert_eq!(rt.availability(), Availability::Unconfigured);
        let probe = rt.probe().await;
        assert_eq!(probe["ok"], json!(false));
        assert_eq!(probe["state"], json!("unconfigured"));
        assert_eq!(probe["http_requests"], json!(0));

        let q = ComputerQueryTool::new(rt.clone());
        let q_res = q
            .execute(json!({ "capability": "clipboard.read" }), &ctx())
            .await;
        assert_eq!(q_res.status, ToolStatus::Failed);
        assert_eq!(
            q_res.error.as_ref().unwrap().code,
            REMOTE_SOCKET_UNCONFIGURED
        );

        let a = ComputerActionTool::new(rt);
        let a_res = a
            .execute(
                json!({ "capability": "slides.next", "subject": "演示文稿" }),
                &ctx(),
            )
            .await;
        assert_eq!(a_res.status, ToolStatus::Failed);
        assert_eq!(
            a_res.error.as_ref().unwrap().code,
            REMOTE_SOCKET_UNCONFIGURED
        );
        assert!(!a_res.fact.as_ref().unwrap().confirmed);
    }

    #[tokio::test]
    async fn stopped_remote_service_fails_honestly() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = RemoteSocketRuntime::new(store);
        // Point to an unused closed port (127.0.0.1:9 discard/closed).
        rt.save(
            RemoteSocketConfig {
                enabled: true,
                endpoint_url: "http://127.0.0.1:19999".into(),
                service_name: "computer".into(),
                timeout_ms: 1000,
                capabilities: vec![],
            },
            None,
            "test",
        )
        .unwrap();

        let q = ComputerQueryTool::new(rt.clone());
        let q_res = q
            .execute(json!({ "capability": "active_window" }), &ctx())
            .await;
        assert_eq!(q_res.status, ToolStatus::Failed);
        assert_eq!(
            q_res.error.as_ref().unwrap().code,
            REMOTE_SOCKET_UNREACHABLE
        );

        let a = ComputerActionTool::new(rt);
        let a_res = a
            .execute(
                json!({ "capability": "app.open", "subject": "备忘录" }),
                &ctx(),
            )
            .await;
        assert_eq!(a_res.status, ToolStatus::Failed);
        assert_eq!(
            a_res.error.as_ref().unwrap().code,
            REMOTE_SOCKET_UNREACHABLE
        );
        assert!(!a_res.fact.as_ref().unwrap().confirmed);
    }
}
