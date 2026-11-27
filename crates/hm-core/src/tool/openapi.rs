//! Generic OpenAPI query adapter (R2-BM6).
//!
//! Why a single `openapi.call` tool and not one tool per operation: the tool
//! registry is assembled ONCE (`tool::mocks::build_registry`) and the prompt's
//! capability list is rendered from it. Registering/unregistering tools at
//! runtime would bypass that single assembly point. Instead the adapter owns
//! one tool whose description lists the household's whitelisted operations;
//! the operation id is an enum and the argument subset is enforced HERE.
//!
//! Hard boundaries:
//! - **GET only** in v1. An arbitrary third-party write has no read-back
//!   contract, so it cannot produce the core-owned fact BM5 requires. A
//!   non-GET whitelist entry is refused at save/import time.
//! - The OpenAPI document is read at **save/probe** time and the resolved
//!   catalogue is stored. A tool call never refetches the document, so a
//!   document that changes underneath us cannot change what the model may do.
//! - Unconfigured/disabled = ZERO HTTP, for the tool and for the probe.
//! - The bearer token lives in the secrets table and is never echoed, logged
//!   or put in an error string.
//! - Every request is bounded (timeout, retries, query length, response body).

use crate::config::{
    ConfigService, EffectLevel, OpenApiConfig, OpenApiOperation, OpenApiParam,
    OpenApiParamLocation, VerificationStrategy, MAX_OPENAPI_OPERATIONS,
};
use crate::store::Store;
use crate::tool::openapi_verify::{normalize_body_param, parse_request_body_params};
use crate::tool::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A misbehaving document/response must not OOM the box.
pub const MAX_SPEC_BODY: usize = 512 * 1024;
pub const MAX_RESPONSE_BODY: usize = 64 * 1024;
pub const MAX_QUERY_CHARS: usize = 1024;
pub const MAX_ARGS_BYTES: usize = 8 * 1024;
/// How many operations `discover` will list (a 500-endpoint spec is not a
/// household capability list).
pub const MAX_DISCOVERED: usize = 100;

pub const OPENAPI_UNCONFIGURED: &str = "OPENAPI_UNCONFIGURED";
pub const OPENAPI_DISABLED: &str = "OPENAPI_DISABLED";
pub const OPENAPI_OPERATION_UNKNOWN: &str = "OPENAPI_OPERATION_UNKNOWN";
pub const OPENAPI_METHOD_UNSUPPORTED: &str = "OPENAPI_METHOD_UNSUPPORTED";
pub const OPENAPI_PARAM_NOT_ALLOWED: &str = "OPENAPI_PARAM_NOT_ALLOWED";
pub const OPENAPI_PARAM_REQUIRED: &str = "OPENAPI_PARAM_REQUIRED";
pub const OPENAPI_PARAM_TYPE: &str = "OPENAPI_PARAM_TYPE";
pub const OPENAPI_PARAM_ENUM: &str = "OPENAPI_PARAM_ENUM";
pub const OPENAPI_SPEC: &str = "OPENAPI_SPEC";
pub const OPENAPI_SPEC_UNREACHABLE: &str = "OPENAPI_SPEC_UNREACHABLE";
pub const OPENAPI_SPEC_CHANGED: &str = "OPENAPI_SPEC_CHANGED";
pub const OPENAPI_AUTH: &str = "OPENAPI_AUTH";
pub const OPENAPI_HTTP: &str = "OPENAPI_HTTP";
pub const OPENAPI_TIMEOUT: &str = "OPENAPI_TIMEOUT";
pub const OPENAPI_OFFLINE: &str = "OPENAPI_OFFLINE";
pub const OPENAPI_RESPONSE_TOO_LARGE: &str = "OPENAPI_RESPONSE_TOO_LARGE";
pub const OPENAPI_RESPONSE: &str = "OPENAPI_RESPONSE";
pub const OPENAPI_ARGUMENTS: &str = "OPENAPI_ARGUMENTS";
pub const OPENAPI_CANCELLED: &str = "OPENAPI_CANCELLED";
pub const OPENAPI_CONFIRMATION_REQUIRED: &str = "OPENAPI_CONFIRMATION_REQUIRED";
pub const OPENAPI_READBACK_FAILED: &str = "OPENAPI_READBACK_FAILED";
pub const OPENAPI_READBACK_MISMATCH: &str = "OPENAPI_READBACK_MISMATCH";
pub const OPENAPI_RESPONSE_ASSERT_FAILED: &str = "OPENAPI_RESPONSE_ASSERT_FAILED";

#[derive(Debug, Clone)]
pub struct OpenApiError {
    pub code: &'static str,
    pub detail: String,
    pub retryable: bool,
}

impl OpenApiError {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            detail: detail.into(),
            retryable,
        }
    }
}

/// One operation as the discovery step reports it. `callable=false` operations
/// are shown to the console with the reason so a person is not left guessing
/// why a documented endpoint is missing from the picker.
#[derive(Debug, Clone)]
pub struct DiscoveredOperation {
    pub id: String,
    pub method: String,
    pub path: String,
    pub summary: String,
    pub params: Vec<OpenApiParam>,
    pub effect_level: EffectLevel,
    pub callable: bool,
    pub reason: Option<String>,
}

impl DiscoveredOperation {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "method": self.method,
            "path": self.path,
            "summary": self.summary,
            "params": self.params,
            "effect_level": self.effect_level,
            "callable": self.callable,
            "reason": self.reason,
        })
    }
}

/// Parse an OpenAPI 3 JSON document into GET operations. Only JSON is
/// supported (YAML would need a new dependency and a licensing review); the
/// error names that explicitly.
pub fn parse_spec(doc: &Value) -> Result<Vec<DiscoveredOperation>, String> {
    let version = doc.get("openapi").and_then(Value::as_str).unwrap_or("");
    if !version.starts_with("3.") {
        return Err(format!(
            "只支持 OpenAPI 3.x 的 JSON 文档（openapi={version:?}）；YAML/2.0 暂不支持"
        ));
    }
    let paths = doc
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "文档缺少 paths 对象".to_string())?;
    let mut out = Vec::new();
    for (path, item) in paths {
        if !path.starts_with('/') {
            continue;
        }
        let Some(item) = resolve_ref(doc, item) else {
            continue;
        };
        let Some(item) = item.as_object() else {
            continue;
        };
        let path_level = item
            .get("parameters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for method in ["get", "post", "put", "patch", "delete"] {
            let Some(op) = item.get(method) else { continue };
            let Some(op) = op.as_object() else { continue };
            let id = op
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| default_operation_id(method, path));
            let summary = op
                .get("summary")
                .or_else(|| op.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .chars()
                .take(120)
                .collect::<String>();
            let mut params = Vec::new();
            let mut reason: Option<String> = None;
            let mut seen: Vec<(String, OpenApiParamLocation)> = Vec::new();
            let mut all = path_level.clone();
            if let Some(list) = op.get("parameters").and_then(Value::as_array) {
                all.extend(list.clone());
            }
            for raw in all {
                let Some(raw) = resolve_ref(doc, &raw) else {
                    reason.get_or_insert_with(|| "有参数使用无法解析的 $ref".into());
                    continue;
                };
                let Some(p) = raw.as_object() else { continue };
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let location = match p.get("in").and_then(Value::as_str) {
                    Some("path") => OpenApiParamLocation::Path,
                    Some("query") => OpenApiParamLocation::Query,
                    // Headers/cookies are not forwarded: a required one would
                    // make the operation impossible to call honestly.
                    Some(other) => {
                        if p.get("required").and_then(Value::as_bool).unwrap_or(false) {
                            reason.get_or_insert_with(|| {
                                format!("参数 '{name}' 在 {other} 里；v1 只支持 path/query")
                            });
                        }
                        continue;
                    }
                    None => continue,
                };
                let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
                // `schema` may be an inline object or a local `$ref`
                // (`#/components/schemas/...`). Audit P2: the old code only
                // looked at the inline object and silently defaulted a
                // referenced integer/enum schema to "string", which widened
                // the accepted arguments.
                let schema = p.get("schema").and_then(|s| resolve_schema(doc, s));
                let enum_values = schema
                    .and_then(|s| s.get("enum"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| match v {
                                Value::String(s) => Some(s.clone()),
                                Value::Number(n) => Some(n.to_string()),
                                Value::Bool(b) => Some(b.to_string()),
                                _ => None,
                            })
                            .take(32)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let declared = schema.and_then(|s| s.get("type")).and_then(Value::as_str);
                let kind = match declared {
                    Some(k @ ("string" | "integer" | "number" | "boolean")) => k.to_string(),
                    // An enum without an explicit type is a string enum in
                    // practice (the only shape a query can carry).
                    None if !enum_values.is_empty() => "string".to_string(),
                    // Arrays/objects cannot ride a GET query safely here, and
                    // an untyped schema must not be guessed.
                    other => {
                        if required {
                            let shown = other.unwrap_or("（没有 type）");
                            reason.get_or_insert_with(|| {
                                format!("参数 '{name}' 的类型 '{shown}' 不支持")
                            });
                        }
                        continue;
                    }
                };
                let key = (name.clone(), location);
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                params.push(OpenApiParam {
                    sensitive: schema.is_some_and(|schema| {
                        schema.get("format").and_then(Value::as_str) == Some("password")
                            || schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
                            || schema.get("x-sensitive").and_then(Value::as_bool) == Some(true)
                    }) || p.get("x-sensitive").and_then(Value::as_bool) == Some(true),
                    name,
                    location,
                    required,
                    kind,
                    enum_values,
                });
            }
            if method != "get" {
                parse_request_body_params(doc, op, &mut params, &mut seen, &mut reason);
            }
            if params.len() > crate::config::MAX_OPENAPI_PARAMS {
                reason.get_or_insert_with(|| {
                    format!("参数超过 {} 个", crate::config::MAX_OPENAPI_PARAMS)
                });
            }
            let effect_level = if method == "get" {
                EffectLevel::Query
            } else {
                EffectLevel::Mutate
            };
            let callable = reason.is_none();
            out.push(DiscoveredOperation {
                id,
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                summary,
                params,
                effect_level,
                callable,
                reason,
            });
            if out.len() >= MAX_DISCOVERED {
                break;
            }
        }
        if out.len() >= MAX_DISCOVERED {
            break;
        }
    }
    if out.is_empty() {
        return Err("文档里没有可用的操作".into());
    }
    Ok(out)
}

fn default_operation_id(method: &str, path: &str) -> String {
    let tail: String = path
        .trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let tail = tail.trim_matches('_');
    if tail.is_empty() {
        method.to_string()
    } else {
        format!("{method}_{tail}")
    }
}

/// Only local `#/components/...` references are resolved. External refs are
/// left unresolved (the caller reports them) — fetching an arbitrary URL from
/// a document would be an SSRF path.
fn resolve_ref<'a>(doc: &'a Value, value: &'a Value) -> Option<&'a Value> {
    let Some(r) = value.get("$ref").and_then(Value::as_str) else {
        return Some(value);
    };
    let path = r.strip_prefix("#/")?;
    let mut cur = doc;
    for part in path.split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        cur = cur.get(&part)?;
    }
    Some(cur)
}

/// Resolve a schema through local `$ref`s (audit P2: a referenced schema used
/// to be ignored and defaulted to "string"). A cycle/too-deep chain is
/// treated as unresolved — the caller then refuses to guess the type.
fn resolve_schema<'a>(doc: &'a Value, schema: &'a Value) -> Option<&'a Value> {
    let mut cur = schema;
    for _ in 0..4 {
        if cur.get("$ref").is_none() {
            return Some(cur);
        }
        cur = resolve_ref(doc, cur)?;
    }
    None
}

/// The token to use when reading a document. An explicit non-empty token
/// wins; an explicit empty string means "clear" and never falls back; the
/// stored token is reused ONLY for the stored URL, so pointing discovery at a
/// new source cannot forward the old credential (audit P1).
pub fn token_for_source(
    stored_spec: &str,
    stored: Option<&str>,
    requested: &str,
    explicit: Option<&str>,
) -> Option<String> {
    if let Some(t) = explicit.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(t.to_string());
    }
    if explicit.is_some() {
        return None;
    }
    if stored_spec.trim() == requested.trim() {
        stored.map(str::to_string)
    } else {
        None
    }
}

/// Apply the stored credential to a request: always `Authorization: Bearer`,
/// plus the configured custom header when one is set (some self-hosted
/// services only accept their own header). The header NAME is public config;
/// the VALUE is the same secret, so redaction and the clear-on-source-change
/// rule cover both.
fn apply_auth(
    req: reqwest::RequestBuilder,
    token: Option<&str>,
    auth_header: &str,
) -> reqwest::RequestBuilder {
    let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return req;
    };
    let mut req = req.header("Authorization", format!("Bearer {t}"));
    let name = auth_header.trim();
    if !name.is_empty() {
        req = req.header(name, t);
    }
    req
}

fn redact_str(s: &str, secret: &str) -> String {
    let secret = secret.trim();
    if secret.is_empty() {
        s.to_string()
    } else {
        s.replace(secret, "[redacted]")
    }
}

/// Remove a secret from a JSON value before it can reach a tool result, the
/// trace or the persisted conversation (audit P1: an upstream that echoes the
/// Authorization header would otherwise leak the token into all three).
pub fn redact_secret(value: &mut Value, secret: &str) {
    let secret = secret.trim();
    if secret.is_empty() {
        return;
    }
    match value {
        Value::String(s) => {
            if s.contains(secret) {
                *s = s.replace(secret, "[redacted]");
            }
        }
        Value::Array(a) => {
            for v in a {
                redact_secret(v, secret);
            }
        }
        Value::Object(o) => {
            // Keys too: an upstream can echo the token as a JSON KEY, which
            // would still reach the trace/history (audit round 2).
            let mut rebuilt = serde_json::Map::new();
            for (k, mut v) in std::mem::take(o) {
                redact_secret(&mut v, secret);
                rebuilt.insert(redact_str(&k, secret), v);
            }
            *o = rebuilt;
        }
        _ => {}
    }
}

/// Remove the current token from stored operation metadata. Applied on the
/// SAVE path (the document is external and could echo it in a summary), not
/// only on discovery.
pub fn redact_operation(op: &mut OpenApiOperation, secret: &str) {
    op.id = redact_str(&op.id, secret);
    op.path = redact_str(&op.path, secret);
    op.summary = redact_str(&op.summary, secret);
    for p in op.params.iter_mut() {
        p.name = redact_str(&p.name, secret);
        for v in p.enum_values.iter_mut() {
            *v = redact_str(v, secret);
        }
    }
}

/// Resolve the household's whitelist against a parsed document. The result is
/// the catalogue that gets STORED, so the executor never needs the document.
pub fn resolve_operations(
    doc: &Value,
    ids: &[String],
) -> Result<Vec<OpenApiOperation>, OpenApiError> {
    if ids.is_empty() {
        return Err(OpenApiError::new(
            OPENAPI_OPERATION_UNKNOWN,
            "白名单至少选一个操作",
            false,
        ));
    }
    if ids.len() > MAX_OPENAPI_OPERATIONS {
        return Err(OpenApiError::new(
            OPENAPI_OPERATION_UNKNOWN,
            format!("白名单最多 {MAX_OPENAPI_OPERATIONS} 个操作"),
            false,
        ));
    }
    let discovered = parse_spec(doc).map_err(|e| OpenApiError::new(OPENAPI_SPEC, e, false))?;
    let mut out = Vec::new();
    for id in ids {
        let Some(op) = discovered.iter().find(|o| &o.id == id) else {
            return Err(OpenApiError::new(
                OPENAPI_OPERATION_UNKNOWN,
                format!("文档里没有操作 '{id}'（或没有 operationId）"),
                false,
            ));
        };
        if !op.callable {
            return Err(OpenApiError::new(
                OPENAPI_METHOD_UNSUPPORTED,
                format!(
                    "操作 '{id}' 不能接入：{}",
                    op.reason.as_deref().unwrap_or("参数不支持")
                ),
                false,
            ));
        }
        let method_lower = op.method.to_ascii_lowercase();
        let verification = if method_lower == "get" {
            None
        } else {
            Some(VerificationStrategy::TransportOnly)
        };
        out.push(OpenApiOperation {
            id: op.id.clone(),
            method: method_lower,
            path: op.path.clone(),
            summary: op.summary.clone(),
            params: op.params.clone(),
            effect_level: op.effect_level,
            verification,
        });
    }
    Ok(out)
}

mod prepared;
pub(crate) use prepared::{scrub_result, scrub_value, PreparedOperation};

// --- runtime ---------------------------------------------------------------

pub struct OpenApiRuntime {
    store: Arc<Store>,
    client: reqwest::Client,
    confirmations: std::sync::Mutex<std::sync::Weak<crate::confirmation::ConfirmationService>>,
}

impl OpenApiRuntime {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        Self::with_client(
            store,
            crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                crate::tool::http::OutboundPolicy::Public { allow_loopback },
            )
            .timeout(Duration::from_secs(8))
            // A document/feed we fetch must not redirect the request to a
            // different host (audit P1: the default follows up to 10
            // hops, so an external source could steer a bearer-token
            // request at a loopback/LAN target).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default(),
        )
    }

    pub fn with_client(store: Arc<Store>, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            store,
            client,
            confirmations: Default::default(),
        })
    }

    fn snapshot(&self) -> crate::config_delivery::OpenApiSnapshot {
        ConfigService::new(self.store.clone())
            .openapi_snapshot()
            .unwrap_or_default()
    }

    pub fn config(&self) -> OpenApiConfig {
        self.snapshot().config
    }

    pub fn token(&self) -> Option<String> {
        self.snapshot().token
    }

    pub fn availability(&self) -> Availability {
        let cfg = self.config();
        match cfg.state_word() {
            "ready" => Availability::Ready,
            "disabled" => Availability::Disabled,
            _ => Availability::Unconfigured,
        }
    }

    pub fn state_detail(&self) -> String {
        Self::detail(&self.config())
    }

    fn detail(cfg: &OpenApiConfig) -> String {
        match cfg.state_word() {
            "ready" => {
                let writes = cfg
                    .operations
                    .iter()
                    .filter(|o| !o.method.eq_ignore_ascii_case("get"))
                    .count();
                if writes == 0 {
                    format!("已接入 OpenAPI：{} 个白名单查询操作", cfg.operations.len())
                } else {
                    format!(
                        "已接入 OpenAPI：{} 个白名单操作（含 {} 个写操作）",
                        cfg.operations.len(),
                        writes
                    )
                }
            }
            "disabled" => "OpenAPI 已配置但当前关闭：助手不会调用它".into(),
            _ => "未接入 OpenAPI：请先填文档地址并选择白名单操作".into(),
        }
    }

    /// Fetch and parse a document. One bounded request; a management action,
    /// not a tool call.
    async fn fetch_spec(
        &self,
        spec_url: &str,
        token: Option<&str>,
        timeout_ms: u64,
        auth_header: &str,
    ) -> Result<(Value, usize), OpenApiError> {
        let req = self
            .client
            .get(spec_url)
            .timeout(Duration::from_millis(timeout_ms.clamp(500, 15_000)))
            .header("Accept", "application/json");
        let req = apply_auth(req, token, auth_header);
        let resp = req.send().await.map_err(|e| transport_error(&e))?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(OpenApiError::new(
                OPENAPI_AUTH,
                format!("读取 OpenAPI 文档被拒绝（HTTP {status}）：检查令牌"),
                false,
            ));
        }
        if !status.is_success() {
            return Err(OpenApiError::new(
                OPENAPI_SPEC_UNREACHABLE,
                format!("读取 OpenAPI 文档失败（HTTP {status}）"),
                status.is_server_error(),
            ));
        }
        let bytes = crate::tool::http::read_bounded(resp, MAX_SPEC_BODY)
            .await
            .map_err(|e| match e {
                crate::tool::http::BodyReadError::TooLarge { .. } => OpenApiError::new(
                    OPENAPI_RESPONSE_TOO_LARGE,
                    format!("OpenAPI 文档超过 {} KiB", MAX_SPEC_BODY / 1024),
                    false,
                ),
                // Audit P2: a body that stalls after the headers is a TIMEOUT,
                // not a broken connection or an oversized body.
                crate::tool::http::BodyReadError::Timeout => {
                    OpenApiError::new(OPENAPI_TIMEOUT, "读取 OpenAPI 文档超时", true)
                }
                crate::tool::http::BodyReadError::PrematureClose(_) => OpenApiError::new(
                    OPENAPI_SPEC_UNREACHABLE,
                    "读取 OpenAPI 文档时连接提前关闭",
                    true,
                ),
                crate::tool::http::BodyReadError::Protocol(_) => OpenApiError::new(
                    OPENAPI_SPEC_UNREACHABLE,
                    "读取 OpenAPI 文档时协议异常",
                    true,
                ),
            })?;
        let doc: Value = serde_json::from_slice(&bytes)
            .map_err(|e| OpenApiError::new(OPENAPI_SPEC, format!("文档不是 JSON：{e}"), false))?;
        Ok((doc, 1))
    }

    /// The save path's document read (the API resolves the whitelist and
    /// stores the catalogue). Management action — not a tool call.
    pub async fn fetch_spec_for_save(
        &self,
        spec_url: &str,
        token: Option<&str>,
        timeout_ms: u64,
        auth_header: &str,
    ) -> Result<Value, OpenApiError> {
        self.fetch_spec(spec_url, token, timeout_ms, auth_header)
            .await
            .map(|(doc, _)| doc)
    }

    /// Discovery for the console: fetch a document and list its GET
    /// operations. `token` overrides the stored one (the console may be
    /// entering a new one); it is never echoed.
    pub async fn discover(&self, spec_url: &str, token: Option<&str>) -> Value {
        let started = Instant::now();
        let url = spec_url.trim();
        if url.is_empty()
            || !crate::config::valid_public_endpoint(url, self.store.allows_loopback_public())
        {
            return json!({
                "ok": false, "code": "OPENAPI_SPEC",
                "detail": "文档地址必须是公网 http(s)://", "http_requests": 0,
                "latency_ms": 0,
            });
        }
        let snapshot = self.snapshot();
        let cfg = snapshot.config;
        // Source-bound credential: a new URL never receives the stored token.
        let token = token_for_source(&cfg.spec_url, snapshot.token.as_deref(), url, token);
        match self
            .fetch_spec(url, token.as_deref(), cfg.timeout_ms, &cfg.auth_header)
            .await
        {
            Ok((doc, requests)) => match parse_spec(&doc) {
                Ok(mut ops) => {
                    // The document is external; if it echoes the credential
                    // in metadata, do not put it on the console.
                    if let Some(t) = token.as_deref() {
                        for op in ops.iter_mut() {
                            op.id = redact_str(&op.id, t);
                            op.path = redact_str(&op.path, t);
                            op.summary = redact_str(&op.summary, t);
                            op.reason = op.reason.as_deref().map(|r| redact_str(r, t));
                            for p in op.params.iter_mut() {
                                p.name = redact_str(&p.name, t);
                                for v in p.enum_values.iter_mut() {
                                    *v = redact_str(v, t);
                                }
                            }
                        }
                    }
                    let title = redact_str(
                        doc.pointer("/info/title")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        token.as_deref().unwrap_or(""),
                    );
                    let version = redact_str(
                        doc.pointer("/info/version")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        token.as_deref().unwrap_or(""),
                    );
                    json!({
                        "ok": true, "state": "ready",
                        "spec": { "title": title, "version": version },
                        "operations": ops.iter().map(DiscoveredOperation::to_json).collect::<Vec<_>>(),
                        "http_requests": requests,
                        "latency_ms": started.elapsed().as_millis(),
                    })
                }
                Err(e) => json!({
                    "ok": false, "code": OPENAPI_SPEC, "detail": e,
                    "http_requests": requests,
                    "latency_ms": started.elapsed().as_millis(),
                }),
            },
            Err(e) => json!({
                "ok": false, "code": e.code, "detail": e.detail,
                "http_requests": 0,
                "latency_ms": started.elapsed().as_millis(),
            }),
        }
    }

    /// A REAL probe: refetch the document (auth), verify the stored catalogue
    /// still matches it, and call the first whitelisted operation that needs
    /// no arguments. Unconfigured/disabled = 0 requests.
    pub async fn probe(&self) -> Value {
        let started = Instant::now();
        let snapshot = self.snapshot();
        let cfg = snapshot.config;
        let token = snapshot.token;
        let state = cfg.state_word();
        if state != "ready" {
            return json!({
                "ok": false, "state": state,
                "detail": Self::detail(&cfg),
                "token_set": token.is_some(),
                "operations": cfg.operations.len(),
                "http_requests": 0, "latency_ms": 0,
            });
        }
        let mut requests = 0usize;
        let (doc, n) = match self
            .fetch_spec(
                &cfg.spec_url,
                token.as_deref(),
                cfg.timeout_ms,
                &cfg.auth_header,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return json!({
                    "ok": false, "state": "failed", "code": e.code, "detail": e.detail,
                    "token_set": token.is_some(), "operations": cfg.operations.len(),
                    "http_requests": 1, "latency_ms": started.elapsed().as_millis(),
                })
            }
        };
        requests += n;
        // Drift check: the stored catalogue is what the model may call; if
        // the document no longer has it, say so instead of pretending ready.
        let drifted = match parse_spec(&doc) {
            Ok(ops) => cfg.operations.iter().find_map(|stored| {
                match ops.iter().find(|o| o.id == stored.id) {
                    None => Some(format!("文档里已经没有操作 '{}'", stored.id)),
                    Some(found) if found.path != stored.path => Some(format!(
                        "操作 '{}' 的路径从 {} 变成了 {}",
                        stored.id, stored.path, found.path
                    )),
                    // Audit P2: the document may add/remove/retype a
                    // parameter without changing id or path. The stored
                    // argument subset would then be validated against stale
                    // metadata, so a drift in the parameter catalogue is a
                    // failed probe too.
                    Some(found) if found.params != stored.params => Some(format!(
                        "操作 '{}' 的参数目录变了（需要重新保存白名单）",
                        stored.id
                    )),
                    Some(found) if !found.callable => Some(format!(
                        "操作 '{}' 现在不可调用：{}",
                        stored.id,
                        found.reason.as_deref().unwrap_or("参数不支持")
                    )),
                    Some(_) => None,
                }
            }),
            Err(e) => Some(format!("文档解析失败：{e}")),
        };
        if let Some(detail) = drifted {
            return json!({
                "ok": false, "state": "failed", "code": OPENAPI_SPEC_CHANGED,
                "detail": format!("{detail}；请重新保存白名单"),
                "token_set": token.is_some(), "operations": cfg.operations.len(),
                "http_requests": requests, "latency_ms": started.elapsed().as_millis(),
            });
        }
        // One real GET call, only when it needs no arguments (never fire a
        // mutating write or invent values for a required parameter during probe).
        let sample = cfg.operations.iter().find(|o| {
            o.method.eq_ignore_ascii_case("get")
                && o.effect_level == EffectLevel::Query
                && !o.params.iter().any(|p| p.required)
        });
        let Some(op) = sample else {
            return json!({
                "ok": true, "state": "ready",
                "detail": "文档与白名单一致；白名单操作都需要参数，探针只验证了文档与鉴权",
                "token_set": token.is_some(), "operations": cfg.operations.len(),
                "http_requests": requests, "latency_ms": started.elapsed().as_millis(),
            });
        };
        let request = match self.prepare_http(&cfg, op, &json!({})) {
            Ok(request) => request,
            Err((error, _)) => {
                return json!({"ok":false,"code":error.code,"detail":error.detail,"http_requests":requests})
            }
        };
        match self
            .send_http(&cfg, op, request, token.as_deref(), None, None)
            .await
        {
            Ok((body, n)) => {
                requests += n;
                json!({
                    "ok": true, "state": "ready",
                    "detail": "文档、鉴权与一次真实查询都通过",
                    "sample": { "operation": op.id, "status": body.get("status") },
                    "token_set": token.is_some(), "operations": cfg.operations.len(),
                    "http_requests": requests, "latency_ms": started.elapsed().as_millis(),
                })
            }
            Err((e, n)) => {
                requests += n;
                json!({
                    "ok": false, "state": "failed", "code": e.code, "detail": e.detail,
                    "token_set": token.is_some(), "operations": cfg.operations.len(),
                    "http_requests": requests, "latency_ms": started.elapsed().as_millis(),
                })
            }
        }
    }

    /// Execute one whitelisted operation. Validation happens BEFORE any
    /// request: an operation/argument outside the resolved catalogue costs
    /// zero HTTP.
    pub async fn call(
        &self,
        operation: &str,
        args: &Value,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<(Value, usize), (OpenApiError, usize)> {
        let prepared = self.prepare_operation(operation, args)?;
        if prepared.op.effect_level == EffectLevel::Destructive {
            return Err((
                OpenApiError::new(
                    OPENAPI_CONFIRMATION_REQUIRED,
                    "paired approval and separate execution required",
                    false,
                ),
                0,
            ));
        }
        let secrets = prepared.secrets.clone();
        let response = self
            .send_http(
                &prepared.cfg,
                &prepared.op,
                prepared.request,
                prepared.token.as_deref(),
                cancel,
                None,
            )
            .await;
        match response {
            Ok((mut value, count)) => {
                scrub_value(&mut value, &secrets);
                Ok((value, count))
            }
            Err((mut error, count)) => {
                let mut result = ToolResult::failed(error.code, error.detail, error.retryable);
                scrub_result(&mut result, &secrets);
                error.detail = result.error.map(|e| e.detail).unwrap_or_default();
                Err((error, count))
            }
        }
    }

    pub(super) async fn send_http(
        &self,
        cfg: &OpenApiConfig,
        op: &OpenApiOperation,
        request: prepared::PreparedHttp,
        token: Option<&str>,
        cancel: Option<&Arc<AtomicBool>>,
        permit: Option<crate::confirmation::DispatchPermit>,
    ) -> Result<(Value, usize), (OpenApiError, usize)> {
        if op.effect_level == EffectLevel::Destructive && permit.is_none() {
            return Err((
                OpenApiError::new(
                    OPENAPI_CONFIRMATION_REQUIRED,
                    "dispatch permit required",
                    false,
                ),
                0,
            ));
        }
        let single_attempt = permit.is_some();
        let prepared::PreparedHttp {
            method,
            url,
            body_obj,
            retry_post,
            ..
        } = request;

        // Disable automatic HTTP transport retries on non-idempotent writes
        // (POST without idempotency key).
        let attempts = if single_attempt || (method == reqwest::Method::POST && !retry_post) {
            1
        } else {
            1 + cfg.retries.min(3) as usize
        };
        let mut last: Option<OpenApiError> = None;
        let mut seen = 0usize;
        for attempt in 0..attempts {
            if cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false) {
                return Err((
                    OpenApiError::new(OPENAPI_CANCELLED, "任务已取消，没有发出下一次请求", false),
                    seen,
                ));
            }
            seen += 1;
            let mut req = self
                .client
                .request(method.clone(), &url)
                .timeout(Duration::from_millis(cfg.timeout_ms.clamp(500, 15_000)))
                .header("Accept", "application/json");
            if !body_obj.is_empty() {
                req = req.json(&Value::Object(body_obj.clone()));
            }
            let req = apply_auth(req, token, &cfg.auth_header);
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 401 || status.as_u16() == 403 {
                        return Err((
                            OpenApiError::new(
                                OPENAPI_AUTH,
                                format!("操作 '{}' 鉴权失败（HTTP {status}）：检查令牌", op.id),
                                false,
                            ),
                            seen,
                        ));
                    }
                    if !status.is_success() {
                        let retryable = status.is_server_error() && attempt + 1 < attempts;
                        last = Some(OpenApiError::new(
                            OPENAPI_HTTP,
                            format!("操作 '{}' 返回 HTTP {status}", op.id),
                            retryable,
                        ));
                        if !retryable {
                            break;
                        }
                        continue;
                    }
                    let bytes = match crate::tool::http::read_bounded(resp, MAX_RESPONSE_BODY).await
                    {
                        Ok(b) => b,
                        Err(crate::tool::http::BodyReadError::TooLarge { .. }) => {
                            return Err((
                                OpenApiError::new(
                                    OPENAPI_RESPONSE_TOO_LARGE,
                                    format!("响应超过 {} KiB", MAX_RESPONSE_BODY / 1024),
                                    false,
                                ),
                                seen,
                            ))
                        }
                        Err(crate::tool::http::BodyReadError::Timeout) => {
                            return Err((
                                OpenApiError::new(OPENAPI_TIMEOUT, "读取 OpenAPI 响应超时", true),
                                seen,
                            ))
                        }
                        Err(crate::tool::http::BodyReadError::PrematureClose(_)) => {
                            return Err((
                                OpenApiError::new(
                                    OPENAPI_RESPONSE,
                                    "读取响应时连接提前关闭",
                                    false,
                                ),
                                seen,
                            ))
                        }
                        Err(crate::tool::http::BodyReadError::Protocol(_)) => {
                            return Err((
                                OpenApiError::new(OPENAPI_RESPONSE, "读取响应时协议异常", false),
                                seen,
                            ))
                        }
                    };
                    let text = String::from_utf8_lossy(&bytes);
                    let mut body = match serde_json::from_str::<Value>(&text) {
                        Ok(v) => json!({ "json": v }),
                        Err(_) => json!({ "text": clip(&text, 2000) }),
                    };
                    // Audit P1: an upstream that echoes the request headers
                    // must not put the token into the tool result, trace or
                    // persisted history.
                    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
                        redact_secret(&mut body, t);
                        let encoded = pct(t);
                        if encoded != t {
                            redact_secret(&mut body, &encoded);
                        }
                    }
                    let mut out = json!({
                        "operation": op.id,
                        "status": status.as_u16(),
                        "body": body,
                    });
                    if let Some(rem) = out.as_object_mut() {
                        rem.insert("url_path".into(), json!(op.path));
                    }
                    return Ok((out, seen));
                }
                Err(e) => {
                    let (code, detail, retryable) = if e.is_timeout() {
                        (OPENAPI_TIMEOUT, "OpenAPI 请求超时", attempt + 1 < attempts)
                    } else if e.is_connect() {
                        (
                            OPENAPI_OFFLINE,
                            "连不上 OpenAPI 服务（网络或地址不可达）",
                            attempt + 1 < attempts,
                        )
                    } else {
                        (OPENAPI_OFFLINE, "OpenAPI 请求失败", attempt + 1 < attempts)
                    };
                    last = Some(OpenApiError::new(code, detail, retryable));
                    if !retryable {
                        break;
                    }
                }
            }
        }
        Err((
            last.unwrap_or_else(|| OpenApiError::new(OPENAPI_HTTP, "请求失败", false)),
            seen,
        ))
    }
}

fn transport_error(e: &reqwest::Error) -> OpenApiError {
    if e.is_timeout() {
        OpenApiError::new(OPENAPI_TIMEOUT, "读取 OpenAPI 文档超时", true)
    } else if e.is_connect() {
        OpenApiError::new(OPENAPI_SPEC_UNREACHABLE, "连不上 OpenAPI 文档地址", true)
    } else {
        OpenApiError::new(OPENAPI_SPEC_UNREACHABLE, "读取 OpenAPI 文档失败", false)
    }
}

fn normalize_param(p: &OpenApiParam, value: &Value) -> Result<String, String> {
    match p.kind.as_str() {
        "string" => value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("参数 '{}' 需要字符串", p.name)),
        "integer" => {
            if let Some(i) = value.as_i64() {
                Ok(i.to_string())
            } else if let Some(f) = value.as_f64() {
                if f.fract() == 0.0
                    && f.is_finite()
                    && f >= i64::MIN as f64
                    && f < (i64::MAX as f64)
                {
                    Ok((f as i64).to_string())
                } else {
                    Err(format!("参数 '{}' 需要整数", p.name))
                }
            } else {
                Err(format!("参数 '{}' 需要整数", p.name))
            }
        }
        "number" => value
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| {
                if f.fract() == 0.0 {
                    format!("{}", f as i64)
                } else {
                    f.to_string()
                }
            })
            .ok_or_else(|| format!("参数 '{}' 需要数字", p.name)),
        "boolean" => value
            .as_bool()
            .map(|b| b.to_string())
            .ok_or_else(|| format!("参数 '{}' 需要 true/false", p.name)),
        other => Err(format!("参数 '{}' 的类型 '{other}' 不支持", p.name)),
    }
}

fn safe_final_path(path: &str) -> Result<(), String> {
    let lower = path.to_ascii_lowercase();
    for bad in ["%2e", "%2f", "%5c", "%00"] {
        if lower.contains(bad) {
            return Err(format!("路径包含不允许的编码段 {bad}"));
        }
    }
    for seg in path.split('/') {
        if seg == "." || seg == ".." {
            return Err("路径不能包含 . 或 .. 段".into());
        }
        if seg.contains('\\') {
            return Err("路径不能包含反斜杠".into());
        }
    }
    Ok(())
}

fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Read-back shape shared by `/mode` and the config GET. No token value.
pub fn status_json(rt: &OpenApiRuntime) -> Value {
    let snapshot = rt.snapshot();
    let detail = OpenApiRuntime::detail(&snapshot.config);
    let base = snapshot
        .config
        .effective_base()
        .filter(|url| crate::config_delivery::portable_service_url(url));
    let mut status = json!(snapshot.status());
    status["detail"] = json!(detail);
    status["effective_base"] = json!(base);
    status
}

// --- the tool --------------------------------------------------------------

pub struct OpenApiCallTool {
    rt: Arc<OpenApiRuntime>,
}

impl OpenApiCallTool {
    pub fn new(rt: Arc<OpenApiRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for OpenApiCallTool {
    fn redact_arguments(&self, args: &Value) -> Value {
        self.rt.public_arguments(args)
    }
    fn catalog_revision(&self) -> Option<String> {
        self.rt.store.openapi_revision().ok()
    }
    fn descriptor(&self) -> ToolDescriptor {
        let cfg = self.rt.config();
        let ops: Vec<String> = cfg
            .operations
            .iter()
            .map(|o| {
                if o.summary.is_empty() {
                    o.id.clone()
                } else {
                    format!("{}（{}）", o.id, clip(&o.summary, 40))
                }
            })
            .collect();
        let has_writes = cfg
            .operations
            .iter()
            .any(|o| o.effect_level.to_side_effect() == SideEffect::Mutate);
        let side_effect = if has_writes {
            SideEffect::Mutate
        } else {
            SideEffect::Query
        };
        let description = if self.rt.availability() == Availability::Ready {
            format!(
                "调用家庭配置的 OpenAPI 操作（支持 GET/POST/PUT/PATCH/DELETE）。operation 必须是白名单之一：{}。参数只能使用每个操作声明的字段；破坏性操作只生成待确认预览，必须由已配对用户在管理页面批准后另行执行",
                ops.join("、")
            )
        } else {
            "调用家庭配置的 OpenAPI 操作（尚未配置：不会发出任何请求）".into()
        };
        let enum_values: Vec<Value> = cfg
            .operations
            .iter()
            .map(|o| Value::String(o.id.clone()))
            .collect();
        let operation_schema = if enum_values.is_empty() {
            json!({
                "type": "string",
                "description": "白名单里的操作 id"
            })
        } else {
            json!({
                "type": "string",
                "enum": enum_values,
                "description": "白名单里的操作 id"
            })
        };
        ToolDescriptor {
            name: "openapi.call".into(),
            service: "openapi".into(),
            side_effect,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": operation_schema,
                    "arguments": {
                        "type": "object",
                        "description": "该操作声明的参数子集（未声明的参数会被拒绝）"
                    },

                },
                "required": ["operation"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(operation) = args.get("operation").and_then(Value::as_str) else {
            return ToolResult::failed(OPENAPI_ARGUMENTS, "缺少 operation", false);
        };
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        match self.rt.prepare_operation(operation, &arguments) {
            Ok(prepared)
                if ctx.is_lan_exempt()
                    && (prepared.op.effect_level != EffectLevel::Query
                        || prepared.token.is_some()) =>
            {
                ToolResult::failed(
                    "PAIRING_REQUIRED",
                    "未认证的局域网访问不允许执行变更、破坏性或带密钥凭据的 OpenAPI 操作，请使用配对令牌认证",
                    false,
                )
            }
            Ok(prepared) if prepared.op.effect_level == EffectLevel::Destructive => {
                let service =
                    crate::lock_or_recover(&self.rt.confirmations, "openapi.confirmations")
                        .upgrade();
                match service {
                    Some(service) => service.create(ctx, prepared),
                    None => ToolResult::failed(
                        "CONFIRMATION_UNAVAILABLE",
                        "paired confirmation service unavailable",
                        false,
                    ),
                }
            }
            Ok(prepared) => {
                self.rt
                    .execute_snapshot(prepared, Some(&ctx.cancel_flag()))
                    .await
            }
            Err((e, _)) => ToolResult::failed(e.code, e.detail, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Value {
        json!({
            "openapi": "3.0.3",
            "info": {"title": "Kids API", "version": "1.0"},
            "paths": {
                "/status": {
                    "get": {
                        "operationId": "getStatus",
                        "summary": "Service status",
                        "parameters": []
                    }
                },
                "/rooms/{room}/temperature": {
                    "get": {
                        "operationId": "getTemperature",
                        "summary": "Room temperature",
                        "parameters": [
                            {"name": "room", "in": "path", "required": true,
                             "schema": {"type": "string"}},
                            {"name": "unit", "in": "query",
                             "schema": {"type": "string", "enum": ["c", "f"]}}
                        ]
                    }
                },
                "/rooms": {
                    "post": {
                        "operationId": "createRoom",
                        "requestBody": {"required": true}
                    }
                }
            }
        })
    }

    #[test]
    fn supported_methods_are_discovered() {
        let ops = parse_spec(&spec()).unwrap();
        let mut ids: Vec<&str> = ops.iter().map(|o| o.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["createRoom", "getStatus", "getTemperature"]);
        assert!(ops
            .iter()
            .filter(|o| o.id != "createRoom")
            .all(|o| o.callable));
    }

    #[test]
    fn an_unknown_operation_is_never_in_the_catalogue() {
        let ids = vec!["missingOp".to_string()];
        let err = resolve_operations(&spec(), &ids).unwrap_err();
        assert_eq!(err.code, OPENAPI_OPERATION_UNKNOWN);
    }

    #[test]
    fn a_required_header_parameter_makes_an_operation_uncallable() {
        let doc = json!({
            "openapi": "3.0.3",
            "paths": {"/x": {"get": {"operationId": "x", "parameters": [
                {"name": "X-Tenant", "in": "header", "required": true, "schema": {"type": "string"}}
            ]}}}
        });
        let ops = parse_spec(&doc).unwrap();
        assert!(!ops[0].callable);
        let err = resolve_operations(&doc, &["x".to_string()]).unwrap_err();
        assert_eq!(err.code, OPENAPI_METHOD_UNSUPPORTED);
    }

    #[test]
    fn a_local_component_ref_is_resolved() {
        let doc = json!({
            "openapi": "3.0.3",
            "components": {"parameters": {"Room": {"name": "room", "in": "query",
                "schema": {"type": "string"}}}},
            "paths": {"/x": {"get": {"operationId": "x", "parameters": [
                {"$ref": "#/components/parameters/Room"}
            ]}}}
        });
        let ops = parse_spec(&doc).unwrap();
        assert_eq!(ops[0].params[0].name, "room");
        assert_eq!(ops[0].params[0].location, OpenApiParamLocation::Query);
    }

    #[test]
    fn a_yaml_style_document_is_refused_by_name() {
        let doc = json!({"swagger": "2.0"});
        let err = parse_spec(&doc).unwrap_err();
        assert!(err.contains("OpenAPI 3.x"));
    }

    #[test]
    fn parameter_types_are_normalized_and_checked() {
        let p = OpenApiParam {
            sensitive: false,
            name: "n".into(),
            location: OpenApiParamLocation::Query,
            required: true,
            kind: "integer".into(),
            enum_values: vec![],
        };
        assert_eq!(normalize_param(&p, &json!(5)).unwrap(), "5");
        assert!(normalize_param(&p, &json!(5.5)).is_err());
        assert!(normalize_param(&p, &json!("5")).is_err());
    }

    #[test]
    fn a_referenced_schema_keeps_its_real_type() {
        let doc = json!({
            "openapi": "3.0.3",
            "components": {"schemas": {"Limit": {"type": "integer", "minimum": 1}}},
            "paths": {"/x": {"get": {"operationId": "x", "parameters": [
                {"name": "limit", "in": "query", "required": true,
                 "schema": {"$ref": "#/components/schemas/Limit"}}
            ]}}}
        });
        let ops = parse_spec(&doc).unwrap();
        assert_eq!(ops[0].params[0].kind, "integer");
        assert!(normalize_param(&ops[0].params[0], &json!("ten")).is_err());
        assert_eq!(
            normalize_param(&ops[0].params[0], &json!(10)).unwrap(),
            "10"
        );
    }

    #[test]
    fn a_stored_token_is_bound_to_its_source() {
        let stored = Some("tok");
        // Same document URL: reusable.
        assert_eq!(
            token_for_source("http://a/spec.json", stored, "http://a/spec.json", None).as_deref(),
            Some("tok")
        );
        // New URL: never forwarded.
        assert_eq!(
            token_for_source("http://a/spec.json", stored, "http://b/spec.json", None),
            None
        );
        // Explicit clear: no fallback.
        assert_eq!(
            token_for_source("http://a/spec.json", stored, "http://a/spec.json", Some("")),
            None
        );
        // Explicit new token wins.
        assert_eq!(
            token_for_source(
                "http://a/spec.json",
                stored,
                "http://b/spec.json",
                Some("new")
            )
            .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn an_echoed_token_is_redacted_from_a_result() {
        let mut v = json!({
            "json": {"echo": "Bearer sekret-123", "nested": ["sekret-123", 1],
                     "sekret-123": "value-under-a-leaking-key"},
            "text": "token=sekret-123",
        });
        redact_secret(&mut v, "sekret-123");
        let s = v.to_string();
        assert!(!s.contains("sekret-123"), "{s}");
        assert!(s.contains("[redacted]"), "{s}");
    }

    #[test]
    fn a_path_template_with_a_dot_segment_is_refused_by_config_validation() {
        let cfg = |path: &str| crate::config::OpenApiConfig {
            enabled: true,
            spec_url: "https://example.com/spec.json".into(),
            operations: vec![OpenApiOperation {
                id: "x".into(),
                method: "get".into(),
                path: path.into(),
                summary: String::new(),
                // Declared so the last case is a VALID whole-segment template.
                params: vec![OpenApiParam {
                    sensitive: false,
                    name: "x".into(),
                    location: OpenApiParamLocation::Path,
                    required: true,
                    kind: "string".into(),
                    enum_values: vec![],
                }],
                effect_level: EffectLevel::Query,
                verification: None,
            }],
            ..Default::default()
        };
        assert!(crate::config::validate_openapi_config(&cfg("/a/../b")).is_err());
        assert!(crate::config::validate_openapi_config(&cfg("/a/%2e%2e/b")).is_err());
        assert!(crate::config::validate_openapi_config(&cfg("/a/..{x}/b")).is_err());
        assert!(crate::config::validate_openapi_config(&cfg("/a/{x}/b")).is_ok());
    }

    /// The optional custom header carries the SAME secret as Bearer: both go
    /// out when configured, only Bearer when not, and nothing without a token
    /// (audit 2026-11-14). The header NAME is public config.
    #[test]
    fn the_custom_auth_header_travels_with_the_bearer_token() {
        let client = reqwest::Client::new();
        let build = |token: Option<&str>, header: &str| {
            let req = apply_auth(client.get("http://127.0.0.1:1/x"), token, header);
            req.build().expect("request builds")
        };

        let both = build(Some("tok-123"), "X-Internal-Token");
        assert_eq!(
            both.headers().get("authorization").unwrap(),
            "Bearer tok-123"
        );
        assert_eq!(both.headers().get("x-internal-token").unwrap(), "tok-123");

        let bearer_only = build(Some("tok-123"), "");
        assert!(bearer_only.headers().get("x-internal-token").is_none());
        assert_eq!(
            bearer_only.headers().get("authorization").unwrap(),
            "Bearer tok-123"
        );

        let none = build(None, "X-Internal-Token");
        assert!(none.headers().get("authorization").is_none());
        assert!(none.headers().get("x-internal-token").is_none());
    }

    /// A header NAME is config a person types: token characters only, so it
    /// cannot inject CR/LF or a second header.
    #[test]
    fn auth_header_names_are_restricted_to_http_token_chars() {
        use crate::config::validate_auth_header_name as v;
        assert!(v("").is_ok());
        assert!(v("X-Internal-Token").is_ok());
        assert!(v("x_api.token").is_ok());
        assert!(v("X-Internal-Token\r\nX-Evil: 1").is_err());
        assert!(v("X Internal").is_err());
        assert!(v(&"a".repeat(65)).is_err());
    }
}
