//! The generic OpenAPI query adapter (R2-BM6).
//!
//! The spec is read ONCE at save time; the resolved operation catalogue lives in
//! this block, so a tool call never refetches a document that could have changed
//! underneath it. `validate_openapi_config` is structural only — the save path
//! resolves the whitelist against the real document before calling it.

use serde::{Deserialize, Serialize};

use super::{valid_public_endpoint, ConfigError};

/// R2-BM6: the generic OpenAPI query adapter. The spec is read ONCE at save
/// time; the resolved operation catalogue lives in this block, so a tool call
/// never refetches a document that could have changed underneath it.
pub const OPENAPI_KEY: &str = "openapi";
/// R2-BM6: the OpenAPI bearer token (secrets table, write-only).
pub const OPENAPI_TOKEN_SECRET: &str = "openapi.token";

// --- OpenAPI query adapter (R2-BM6) ---------------------------------------

pub const MAX_OPENAPI_OPERATIONS: usize = 8;
pub const MAX_OPENAPI_PARAMS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenApiParamLocation {
    #[default]
    Path,
    Query,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EffectLevel {
    #[default]
    Query,
    Mutate,
    Destructive,
}

impl EffectLevel {
    pub fn to_side_effect(self) -> crate::tool::SideEffect {
        match self {
            Self::Query => crate::tool::SideEffect::Query,
            Self::Mutate | Self::Destructive => crate::tool::SideEffect::Mutate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerificationStrategy {
    ReadbackOp {
        op_id: String,
        json_path: String,
        expected: serde_json::Value,
    },
    ResponseAssert {
        json_path: String,
        expected: serde_json::Value,
    },
    #[default]
    TransportOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenApiParam {
    pub name: String,
    /// Retained OpenAPI password/writeOnly/x-sensitive declaration; never authority.
    #[serde(default, skip_serializing_if = "openapi_not_sensitive")]
    pub sensitive: bool,
    #[serde(default)]
    pub location: OpenApiParamLocation,
    #[serde(default)]
    pub required: bool,
    /// `string|integer|number|boolean` (and JSON body property types).
    #[serde(default = "default_openapi_param_kind")]
    pub kind: String,
    #[serde(default)]
    pub enum_values: Vec<String>,
}

fn openapi_not_sensitive(value: &bool) -> bool {
    !*value
}

fn default_openapi_param_kind() -> String {
    "string".into()
}

/// One whitelisted operation. The catalogue is RESOLVED at save time and
/// stored, so the executor never re-reads the document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenApiOperation {
    pub id: String,
    /// Allowed: `get`, `post`, `put`, `patch`, `delete` (case-insensitive).
    #[serde(default = "default_openapi_method")]
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub params: Vec<OpenApiParam>,
    #[serde(default)]
    pub effect_level: EffectLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationStrategy>,
}

fn default_openapi_method() -> String {
    "get".into()
}

pub const DEFAULT_OPENAPI_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_OPENAPI_RETRIES: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenApiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The JSON OpenAPI 3 document used for discovery. Read at save/probe
    /// time only.
    #[serde(default)]
    pub spec_url: String,
    /// "" = use the scheme+authority of `spec_url`.
    #[serde(default)]
    pub base_url: String,
    /// Optional extra auth header NAME (e.g. `X-Internal-Token`). The VALUE is
    /// the stored token, so one secret serves both Bearer and this header —
    /// no second secret, and the existing "clear on source change" rule
    /// applies unchanged. Empty = Bearer only (audit 2026-11-14: many
    /// self-hosted services only accept a custom header).
    #[serde(default)]
    pub auth_header: String,
    /// The whitelist: the ONLY operations the model may call.
    #[serde(default)]
    pub operations: Vec<OpenApiOperation>,
    #[serde(default = "default_openapi_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_openapi_retries")]
    pub retries: u32,
}

fn default_openapi_timeout_ms() -> u64 {
    DEFAULT_OPENAPI_TIMEOUT_MS
}

fn default_openapi_retries() -> u32 {
    DEFAULT_OPENAPI_RETRIES
}

impl Default for OpenApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            spec_url: String::new(),
            base_url: String::new(),
            auth_header: String::new(),
            operations: Vec::new(),
            timeout_ms: DEFAULT_OPENAPI_TIMEOUT_MS,
            retries: DEFAULT_OPENAPI_RETRIES,
        }
    }
}

/// RFC 7230 token characters only: no spaces, colons, CR/LF — a header-name
/// injection would otherwise let a config write arbitrary headers.
pub fn validate_auth_header_name(name: &str) -> std::result::Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Ok(());
    }
    if n.len() > 64 {
        return Err("认证头名称最长 64 字符".into());
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("认证头名称含非法字符（只允许字母数字 - _ .）".into());
    }
    Ok(())
}

impl OpenApiConfig {
    pub(crate) fn normalized(mut self, allow_loopback: bool) -> Result<Self, ConfigError> {
        self.spec_url = self.spec_url.trim().to_owned();
        self.base_url = self.base_url.trim().to_owned();
        self.auth_header = self.auth_header.trim().to_owned();
        self.timeout_ms = self.timeout_ms.clamp(500, 15_000);
        self.retries = self.retries.min(3);
        for op in &mut self.operations {
            op.id = op.id.trim().to_owned();
            op.path = op.path.trim().to_owned();
            op.summary = op.summary.trim().chars().take(120).collect();
            if let Some(VerificationStrategy::ReadbackOp { op_id, .. }) = &mut op.verification {
                *op_id = op_id.trim().to_owned();
            }
            for param in &mut op.params {
                param.name = param.name.trim().to_owned();
                param.kind = param.kind.trim().to_ascii_lowercase();
            }
        }
        validate_auth_header_name(&self.auth_header).map_err(ConfigError::BadOpenApi)?;
        validate_openapi_config_with_policy(&self, allow_loopback)?;
        self.base_url = self.base_url.trim_end_matches('/').to_owned();
        for op in &mut self.operations {
            op.method = op.method.trim().to_ascii_lowercase();
        }
        Ok(self)
    }

    pub(crate) fn has_operation_base(&self) -> bool {
        self.base_url.is_empty()
            || reqwest::Url::parse(&self.base_url)
                .is_ok_and(|url| url.query().is_none() && url.fragment().is_none())
    }

    pub(crate) fn same_credential_source(&self, other: &Self) -> bool {
        let origin = |cfg: &Self| {
            cfg.effective_base()
                .and_then(|url| reqwest::Url::parse(&url).ok())
                .map(|url| url.origin())
        };
        self.spec_url.trim() == other.spec_url.trim() && origin(self) == origin(other)
    }

    /// Static, public catalogue fields must never contain the node credential.
    pub(crate) fn contains_token(&self, token: &str) -> bool {
        let token = token.trim();
        if token.is_empty() {
            return false;
        }
        let contains = |text: &str| text == token || (token.len() >= 8 && text.contains(token));
        fn contains_json(value: &serde_json::Value, contains: &impl Fn(&str) -> bool) -> bool {
            match value {
                serde_json::Value::String(text) => contains(text),
                serde_json::Value::Array(items) => {
                    items.iter().any(|item| contains_json(item, contains))
                }
                serde_json::Value::Object(fields) => fields
                    .iter()
                    .any(|(name, value)| contains(name) || contains_json(value, contains)),
                _ => false,
            }
        }

        [&self.spec_url, &self.base_url, &self.auth_header]
            .into_iter()
            .any(|text| contains(text))
            || self.operations.iter().any(|op| {
                [&op.id, &op.path, &op.summary]
                    .into_iter()
                    .any(|text| contains(text))
                    || op.params.iter().any(|param| {
                        contains(&param.name)
                            || param.enum_values.iter().any(|value| contains(value))
                    })
                    || match &op.verification {
                        Some(VerificationStrategy::ReadbackOp {
                            op_id,
                            json_path,
                            expected,
                        }) => {
                            contains(op_id)
                                || contains(json_path)
                                || contains_json(expected, &contains)
                        }
                        Some(VerificationStrategy::ResponseAssert {
                            json_path,
                            expected,
                        }) => contains(json_path) || contains_json(expected, &contains),
                        _ => false,
                    }
            })
    }

    pub fn state_word(&self) -> &'static str {
        if self.spec_url.trim().is_empty() || self.operations.is_empty() {
            "unconfigured"
        } else if !self.enabled {
            "disabled"
        } else {
            "ready"
        }
    }

    /// The origin the operations are called against: an explicit base, else
    /// the spec document's scheme+authority.
    pub fn effective_base(&self) -> Option<String> {
        let explicit = self.base_url.trim();
        if !explicit.is_empty() {
            return Some(explicit.trim_end_matches('/').to_string());
        }
        let s = self.spec_url.trim();
        let rest = s
            .strip_prefix("http://")
            .map(|r| ("http://", r))
            .or_else(|| s.strip_prefix("https://").map(|r| ("https://", r)))?;
        let authority = rest.1.split(['/', '?', '#']).next().unwrap_or("");
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        Some(format!("{}{}", rest.0, authority))
    }

    pub fn find_operation(&self, id: &str) -> Option<&OpenApiOperation> {
        self.operations.iter().find(|o| o.id == id)
    }
}

/// Read-back shape: the token is not here (only whether one is set).
#[derive(Debug, Clone, Serialize)]
pub struct OpenApiStatus {
    pub enabled: bool,
    pub spec_url: String,
    pub base_url: String,
    pub auth_header: String,
    pub token_set: bool,
    pub operations: Vec<OpenApiOperation>,
    pub timeout_ms: u64,
    pub retries: u32,
    pub state: String,
    pub revision: String,
}

/// Structural validation shared by the save path and config import. It never
/// performs an HTTP request; the save path additionally resolves the
/// whitelist against the real document before calling this.
pub fn validate_openapi_config(cfg: &OpenApiConfig) -> std::result::Result<(), ConfigError> {
    validate_openapi_config_with_policy(cfg, false)
}

pub(super) fn validate_openapi_config_with_policy(
    cfg: &OpenApiConfig,
    allow_loopback: bool,
) -> std::result::Result<(), ConfigError> {
    let bad = |m: String| ConfigError::BadOpenApi(m);
    if !cfg.spec_url.trim().is_empty() && !valid_public_endpoint(&cfg.spec_url, allow_loopback) {
        return Err(bad("spec_url 不是合法的公网 http(s) 地址".into()));
    }
    if !cfg.base_url.trim().is_empty() && !valid_public_endpoint(&cfg.base_url, allow_loopback) {
        return Err(bad("base_url 不是合法的公网 http(s) 地址".into()));
    }
    if !cfg.has_operation_base() {
        return Err(bad("base_url 不能包含查询参数或片段".into()));
    }
    if cfg.operations.len() > MAX_OPENAPI_OPERATIONS {
        return Err(bad(format!("白名单最多 {MAX_OPENAPI_OPERATIONS} 个操作")));
    }
    let mut seen: Vec<&str> = Vec::new();
    for op in &cfg.operations {
        let id = op.id.trim();
        if id.is_empty() || id.chars().count() > 64 {
            return Err(bad("operation id：1–64 字".into()));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err(bad(format!("operation id '{id}'：只允许字母/数字/_ . -")));
        }
        if seen.contains(&id) {
            return Err(bad(format!("operation id '{id}' 重复")));
        }
        seen.push(id);
        let method_lower = op.method.to_ascii_lowercase();
        if !matches!(
            method_lower.as_str(),
            "get" | "post" | "put" | "patch" | "delete"
        ) {
            return Err(bad(format!(
                "operation '{id}' 的方法 {} 不支持（仅支持 GET/POST/PUT/PATCH/DELETE）",
                op.method.to_ascii_uppercase()
            )));
        }
        if method_lower != "get" && op.effect_level == EffectLevel::Query {
            return Err(bad("非 GET 操作必须显式声明写入或破坏性效果".into()));
        }
        if let Some(v) = &op.verification {
            match v {
                VerificationStrategy::ReadbackOp {
                    op_id, json_path, ..
                } => {
                    let rb_id = op_id.trim();
                    if rb_id.is_empty() {
                        return Err(bad(format!("operation '{id}' 的 ReadbackOp 缺少 op_id")));
                    }
                    if rb_id == id {
                        return Err(bad(format!("operation '{id}' 的 ReadbackOp 不能回读自身")));
                    }
                    let Some(target) = cfg.operations.iter().find(|o| o.id.trim() == rb_id) else {
                        return Err(bad(format!(
                            "operation '{id}' 的 ReadbackOp '{rb_id}' 不在白名单里"
                        )));
                    };
                    if !target.method.eq_ignore_ascii_case("get")
                        || target.effect_level != EffectLevel::Query
                    {
                        return Err(bad(format!(
                            "operation '{id}' 的 ReadbackOp '{rb_id}' 必须是 Query GET 操作"
                        )));
                    }
                    if json_path.trim().is_empty() {
                        return Err(bad(format!(
                            "operation '{id}' 的 ReadbackOp 缺少 json_path"
                        )));
                    }
                }
                VerificationStrategy::ResponseAssert { json_path, .. } => {
                    if json_path.trim().is_empty() {
                        return Err(bad(format!(
                            "operation '{id}' 的 ResponseAssert 缺少 json_path"
                        )));
                    }
                }
                VerificationStrategy::TransportOnly => {}
            }
        }
        let path = op.path.trim();
        if !path.starts_with('/')
            || path.len() > 200
            || path.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(bad(format!(
                "operation '{id}' 的 path 必须是以 / 开头的无空白路径"
            )));
        }
        // A template containing "."/".." (or an encoded dot/slash/backslash)
        // could be normalised by the URL parser into a different path than the
        // one the household whitelisted.
        let lower = path.to_ascii_lowercase();
        if path.split('/').any(|seg| seg == "." || seg == "..")
            || path.contains('\\')
            || ["%2e", "%2f", "%5c", "%00"]
                .iter()
                .any(|bad| lower.contains(bad))
        {
            return Err(bad(format!(
                "operation '{id}' 的 path 不能包含 . / .. / 反斜杠或编码的路径分隔符"
            )));
        }
        // A path parameter must occupy a WHOLE segment. Otherwise a template
        // like `/rooms/..{suffix}/admin` can be turned into a `..` segment by
        // substituting an empty suffix.
        for seg in path.split('/') {
            if seg.contains('{') || seg.contains('}') {
                let whole = seg.starts_with('{')
                    && seg.ends_with('}')
                    && seg.matches('{').count() == 1
                    && seg.matches('}').count() == 1;
                if !whole {
                    return Err(bad(format!(
                        "operation '{id}' 的 path 参数必须独占一个路径段（不允许 ..{{x}} 这类拼接）"
                    )));
                }
            }
        }
        if op.params.len() > MAX_OPENAPI_PARAMS {
            return Err(bad(format!(
                "operation '{id}' 参数超过 {MAX_OPENAPI_PARAMS} 个"
            )));
        }
        let mut names: Vec<(String, OpenApiParamLocation)> = Vec::new();
        for p in &op.params {
            let name = p.name.trim();
            if name.is_empty()
                || name.chars().count() > 64
                || name.chars().any(|c| c.is_whitespace())
            {
                return Err(bad(format!("operation '{id}' 参数名不合法")));
            }
            if !matches!(p.kind.as_str(), "string" | "integer" | "number" | "boolean") {
                return Err(bad(format!(
                    "operation '{id}' 参数 '{name}' 的类型 '{}' 不支持",
                    p.kind
                )));
            }
            if p.enum_values.len() > 32 || p.enum_values.iter().any(|v| v.chars().count() > 64) {
                return Err(bad(format!("operation '{id}' 参数 '{name}' 的枚举过大")));
            }
            let key = (name.to_string(), p.location);
            if names.contains(&key) {
                return Err(bad(format!("operation '{id}' 参数 '{name}' 重复")));
            }
            names.push(key);
            if p.location == OpenApiParamLocation::Path && !path.contains(&format!("{{{name}}}")) {
                return Err(bad(format!("operation '{id}' 的 path 里没有 {{{name}}}")));
            }
        }
        // Every `{x}` in the path must be declared, or the URL would keep a
        // literal brace and hit a different resource.
        let mut rest = path;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            let name = &rest[open + 1..open + close];
            if !op
                .params
                .iter()
                .any(|p| p.name == name && p.location == OpenApiParamLocation::Path)
            {
                return Err(bad(format!(
                    "operation '{id}' 的 path 占位符 {{{name}}} 未声明"
                )));
            }
            rest = &rest[open + close + 1..];
        }
    }
    Ok(())
}
