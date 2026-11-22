//! Declarative Open Socket Registry (#22).
//!
//! Allows registering new external HTTP tool contracts declaratively (name,
//! description, JSON parameter schema, read/write effect kind, endpoint URL,
//! HTTP method) without adding Rust types to `api/mod.rs` or touching
//! `agent/mod.rs`'s execution loop.
//!
//! Hard boundaries:
//! - Kept strictly separate from `openapi.rs` (OpenAPI stays GET-only).
//! - Write/mutating sockets are explicitly opted-in via `effect: Mutate` and
//!   MUST produce a `ToolFact` receipt: an HTTP 2xx without an explicit
//!   `confirmed: true` receipt stays `Unknown` / unconfirmed.
//! - Built-in tools and `shell_command` can never be shadowed or registered.

use crate::store::Store;
use crate::tool::{
    Availability, DynamicToolProvider, Evidence, FactBasis, SideEffect, Tool, ToolCtx,
    ToolDescriptor, ToolError, ToolFact, ToolResult, ToolStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const OPEN_SOCKETS_CONFIG_KEY: &str = "open_sockets";
pub const MAX_OPEN_SOCKETS: usize = 32;
pub const MAX_RESPONSE_BODY: usize = 64 * 1024;

pub const SOCKET_UNREGISTERED: &str = "SOCKET_UNREGISTERED";
pub const SOCKET_DISABLED: &str = "SOCKET_DISABLED";
pub const SOCKET_ARGUMENTS: &str = "SOCKET_ARGUMENTS";
pub const SOCKET_UNREACHABLE: &str = "SOCKET_UNREACHABLE";
pub const SOCKET_TIMEOUT: &str = "SOCKET_TIMEOUT";
pub const SOCKET_HTTP: &str = "SOCKET_HTTP";
pub const SOCKET_RESPONSE_TOO_LARGE: &str = "SOCKET_RESPONSE_TOO_LARGE";
pub const SOCKET_CANCELLED: &str = "SOCKET_CANCELLED";

const RESERVED_NAMES: &[&str] = &[
    "shell_command",
    "shell",
    "bash",
    "sh",
    "exec",
    "hotlist.query",
    "music.search",
    "music.play",
    "music.control",
    "time.now",
    "weather.now",
    "weather.forecast",
    "home.set",
    "news.headlines",
    "openapi.call",
    "system.volume",
    "system.volume_reading",
    "system.duplex",
    "session.close",
    "web.search",
    "memory.remember",
    "reminder.set",
    "reminder.list",
    "reminder.cancel",
    "computer.query",
    "computer.action",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    GET,
    POST,
    PUT,
    PATCH,
    DELETE,
}

impl Default for HttpMethod {
    fn default() -> Self {
        Self::GET
    }
}

fn default_service() -> String {
    "open_socket".into()
}

fn default_parameters() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn default_true() -> bool {
    true
}

fn default_timeout_ms() -> u64 {
    10_000
}

fn default_action() -> String {
    "other".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenSocketContract {
    pub name: String,
    #[serde(default = "default_service")]
    pub service: String,
    pub description: String,
    #[serde(default = "default_parameters")]
    pub parameters: Value,
    pub effect: SideEffect,
    pub endpoint_url: String,
    #[serde(default)]
    pub method: HttpMethod,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub auth_header: String,
    #[serde(default)]
    pub default_subject: String,
    #[serde(default = "default_action")]
    pub default_action: String,
}

impl OpenSocketContract {
    pub(crate) fn same_credential_source(&self, other: &Self) -> bool {
        self.auth_header.eq_ignore_ascii_case(&other.auth_header)
            && endpoint(&self.endpoint_url)
                .zip(endpoint(&other.endpoint_url))
                .is_some_and(|(current, desired)| current.origin() == desired.origin())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenSocketStatus {
    #[serde(flatten)]
    pub contract: OpenSocketContract,
    pub token_set: bool,
    pub availability: Availability,
}

pub fn validate_contract(c: &mut OpenSocketContract) -> Result<(), String> {
    c.name = c.name.trim().to_string();
    c.service = c.service.trim().to_string();
    if c.service.is_empty() {
        c.service = default_service();
    }
    c.description = c.description.trim().to_string();
    c.endpoint_url = c.endpoint_url.trim().to_string();
    c.auth_header = c.auth_header.trim().to_string();
    c.default_subject = c.default_subject.trim().to_string();
    c.default_action = c.default_action.trim().to_string();
    if c.default_action.is_empty() {
        c.default_action = default_action();
    }
    c.timeout_ms = c.timeout_ms.clamp(500, 30_000);

    if c.name.is_empty() || c.name.len() > 64 {
        return Err("socket name must be 1..=64 characters".into());
    }
    if !c
        .name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err("socket name may only contain ASCII letters, digits, '.', '_' or '-'".into());
    }
    let lower = c.name.to_ascii_lowercase();
    if RESERVED_NAMES.iter().any(|&r| r == lower) {
        return Err(format!("socket name '{}' is reserved", c.name));
    }
    if c.description.is_empty() || c.description.chars().count() > 512 {
        return Err("socket description must be 1..=512 characters".into());
    }
    if endpoint(&c.endpoint_url).is_none() {
        return Err(
            "endpoint_url must be HTTP(S) with a fixed host and no inline credentials or fragment"
                .into(),
        );
    }
    if !super::argument_schema::valid_parameters(&c.parameters) {
        return Err("parameters contain an invalid or unsupported object schema".into());
    }
    if c.effect == SideEffect::Query && !matches!(c.method, HttpMethod::GET | HttpMethod::POST) {
        return Err("Query sockets only allow GET or POST; use Mutate for write methods".into());
    }
    if !c.auth_header.is_empty()
        && reqwest::header::HeaderName::from_bytes(c.auth_header.as_bytes()).is_err()
    {
        return Err("auth_header must be a valid HTTP header name".into());
    }
    Ok(())
}

fn endpoint(value: &str) -> Option<reqwest::Url> {
    let (scheme, rest) = value.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if !matches!(scheme, "http" | "https")
        || host.is_empty()
        || host.contains(['{', '}', '@'])
        || value.chars().count() > 4096
        || value
            .chars()
            .any(|ch| ch == '\\' || ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    let url = reqwest::Url::parse(value).ok()?;
    (url.host_str().is_some() && url.fragment().is_none()).then_some(url)
}

pub(crate) fn secret_key(name: &str) -> String {
    format!("open_socket.token.{name}")
}

pub struct OpenSocketRuntime {
    store: Arc<Store>,
    client: reqwest::Client,
    contracts: RwLock<Vec<OpenSocketContract>>,
    generation: AtomicU64,
}

impl OpenSocketRuntime {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        let client = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::Lan {
                reject_public_ip_literals: false,
                allow_loopback,
            },
        )
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("build open socket HTTP client")
        });
        Self::with_client(store, client)
    }

    pub fn with_client(store: Arc<Store>, client: reqwest::Client) -> Arc<Self> {
        let loaded = load_contracts(&store);
        Arc::new(Self {
            store,
            client,
            contracts: RwLock::new(loaded),
            generation: AtomicU64::new(0),
        })
    }

    pub fn list(&self) -> Vec<OpenSocketStatus> {
        self.contracts
            .read()
            .unwrap()
            .iter()
            .map(|c| self.status_for(c))
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<OpenSocketContract> {
        self.contracts
            .read()
            .unwrap()
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    pub fn register(
        &self,
        registry: &crate::tool::Registry,
        contract: OpenSocketContract,
        token: Option<&str>,
        actor: &str,
    ) -> Result<OpenSocketStatus, String> {
        let name = contract.name.trim().to_owned();
        self.edit(registry, &name, token, actor, |_| contract)
    }

    pub(crate) fn edit(
        &self,
        registry: &crate::tool::Registry,
        name: &str,
        token: Option<&str>,
        actor: &str,
        patch: impl FnOnce(Option<&OpenSocketContract>) -> OpenSocketContract,
    ) -> Result<OpenSocketStatus, String> {
        registry.update_provider(self, |validate| {
            let mut current = self.contracts.write().unwrap();
            let mut contract = patch(current.iter().find(|entry| entry.name == name.trim()));
            validate_contract(&mut contract)?;
            let mut next = current.clone();
            if let Some(existing) = next.iter_mut().find(|c| c.name == contract.name) {
                *existing = contract.clone();
            } else {
                if next.len() >= MAX_OPEN_SOCKETS {
                    return Err(format!("maximum {MAX_OPEN_SOCKETS} open sockets allowed"));
                }
                next.push(contract.clone());
            }
            let key = secret_key(&contract.name);
            let token = token.map(str::trim);
            let token_changed = match token {
                None => false,
                Some("") => self
                    .store
                    .with(|c| {
                        Ok(c.query_row(
                            "SELECT EXISTS(SELECT 1 FROM secrets WHERE name=?1)",
                            [&key],
                            |r| r.get::<_, bool>(0),
                        )?)
                    })
                    .map_err(|e| e.to_string())?,
                Some(value) => {
                    self.store
                        .secret_get(&key)
                        .map_err(|e| e.to_string())?
                        .as_deref()
                        .unwrap_or("")
                        != value
                }
            };
            let changed = *current != next || token_changed;
            let generation = self.generation.load(Ordering::SeqCst) + u64::from(changed);
            validate(self.tools_for(&next, generation, &[]))?;
            if changed {
                self.store
                    .save_open_socket_config(
                        &serde_json::to_string(&next).map_err(|e| e.to_string())?,
                        token.map(|value| (key.as_str(), value)),
                        actor,
                        next.len(),
                    )
                    .map_err(|e| e.to_string())?;
                *current = next;
                self.generation.store(generation, Ordering::SeqCst);
            }
            Ok(self.status_for(&contract))
        })
    }

    pub fn remove(
        &self,
        registry: &crate::tool::Registry,
        name: &str,
        actor: &str,
    ) -> Result<bool, String> {
        registry.update_provider(self, |validate| {
            let mut current = self.contracts.write().unwrap();
            let next: Vec<_> = current.iter().filter(|c| c.name != name).cloned().collect();
            if next.len() == current.len() {
                return Ok(false);
            }
            let generation = self.generation.load(Ordering::SeqCst) + 1;
            if let Err(error) = validate(self.tools_for(&next, generation, &[])) {
                // A strict deletion may monotonically repair an already-bad
                // persisted source. It cannot introduce a tool or widen the
                // catalog: dispatch stays fail-closed until all faults go.
                if validate(self.tools_for(&current, generation - 1, &[])).is_ok() {
                    return Err(error);
                }
            }
            let key = secret_key(name);
            self.store
                .save_open_socket_config(
                    &serde_json::to_string(&next).map_err(|e| e.to_string())?,
                    Some((&key, "")),
                    actor,
                    next.len(),
                )
                .map_err(|e| e.to_string())?;
            *current = next;
            self.generation.store(generation, Ordering::SeqCst);
            Ok(true)
        })
    }

    /// Reset removes all contracts while the normal publication gates are
    /// held. Retained credentials are dormant until explicitly rebound.
    pub(crate) fn reset_configuration<T>(
        &self,
        registry: &crate::tool::Registry,
        reset: impl FnOnce() -> crate::store::Result<T>,
    ) -> crate::store::Result<T> {
        registry
            .update_provider(self, |_| {
                let mut current = self.contracts.write().unwrap();
                let result = reset();
                if result.is_ok() {
                    current.clear();
                    self.generation.fetch_add(1, Ordering::SeqCst);
                }
                Ok(result)
            })
            .map_err(crate::store::StoreError::Authority)?
    }

    /// The journal callback commits the SQLite projection and receipt while
    /// this provider and the competing Registry namespace are both fenced.
    pub(crate) fn publish_reviewed(
        &self,
        registry: &crate::tool::Registry,
        next: Vec<OpenSocketContract>,
        commit: impl FnOnce() -> crate::config_delivery::Result<bool>,
    ) -> crate::config_delivery::Result<()> {
        registry
            .update_provider(self, |validate| {
                let mut current = self.contracts.write().unwrap();
                let changed = *current != next;
                let generation = self.generation.load(Ordering::SeqCst) + u64::from(changed);
                validate(self.tools_for(&next, generation, &[]))?;
                let result = commit();
                if matches!(result, Ok(true)) && changed {
                    *current = next;
                    self.generation.store(generation, Ordering::SeqCst);
                }
                Ok(result.map(|_| ()))
            })
            .map_err(|error| {
                crate::config_delivery::DeliveryError::conflict(
                    "OPEN_SOCKETS_CATALOG_CONFLICT",
                    error,
                )
                .at(OPEN_SOCKETS_CONFIG_KEY)
            })?
    }

    pub(crate) fn contracts(&self) -> Vec<OpenSocketContract> {
        self.contracts.read().unwrap().clone()
    }

    #[cfg(test)]
    pub async fn call_by_name(&self, name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        let snapshot = {
            let guard = self.contracts.read().unwrap();
            guard
                .iter()
                .find(|contract| contract.name == name)
                .map(|contract| {
                    let token = self.store.secret_get(&secret_key(name)).ok().flatten();
                    (contract.clone(), token)
                })
        };
        let Some((contract, token)) = snapshot else {
            return ToolResult::failed(
                SOCKET_UNREGISTERED,
                format!("open socket '{name}' is not registered"),
                false,
            );
        };
        execute_contract(token.as_deref(), &self.client, &contract, args, ctx).await
    }

    fn status_for(&self, c: &OpenSocketContract) -> OpenSocketStatus {
        let token_set = self
            .store
            .secret_get(&secret_key(&c.name))
            .ok()
            .flatten()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        OpenSocketStatus {
            contract: c.clone(),
            token_set,
            availability: if c.enabled {
                Availability::Ready
            } else {
                Availability::Disabled
            },
        }
    }

    fn tools_for(
        &self,
        contracts: &[OpenSocketContract],
        generation: u64,
        tokens: &[Option<String>],
    ) -> Vec<Arc<dyn Tool>> {
        contracts
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, contract)| {
                Arc::new(OpenSocketTool {
                    client: self.client.clone(),
                    contract,
                    generation,
                    token: tokens.get(index).cloned().flatten(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

impl DynamicToolProvider for OpenSocketRuntime {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let contracts = self.contracts.read().unwrap();
        let tokens = self
            .store
            .with(|c| {
                let tx = c.unchecked_transaction()?;
                contracts
                    .iter()
                    .map(|contract| self.store.secret_get_on(&tx, &secret_key(&contract.name)))
                    .collect::<crate::store::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        self.tools_for(&contracts, self.generation.load(Ordering::SeqCst), &tokens)
    }
}

struct OpenSocketTool {
    client: reqwest::Client,
    contract: OpenSocketContract,
    generation: u64,
    token: Option<String>,
}

#[async_trait::async_trait]
impl Tool for OpenSocketTool {
    fn catalog_revision(&self) -> Option<String> {
        Some(format!(
            "{}:{}",
            self.generation,
            serde_json::to_string(&self.contract).expect("serializable socket contract")
        ))
    }
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.contract.name.clone().into(),
            service: self.contract.service.clone().into(),
            side_effect: self.contract.effect,
            description: self.contract.description.clone(),
            parameters: self.contract.parameters.clone(),
        }
    }

    fn availability(&self) -> Availability {
        if self.contract.enabled {
            Availability::Ready
        } else {
            Availability::Disabled
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        execute_contract(
            self.token.as_deref(),
            &self.client,
            &self.contract,
            args,
            ctx,
        )
        .await
    }
}

fn load_contracts(store: &Store) -> Vec<OpenSocketContract> {
    store
        .config_get(OPEN_SOCKETS_CONFIG_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Vec<OpenSocketContract>>(&raw).ok())
        .unwrap_or_default()
}

fn build_request_url_and_remaining(
    template: &str,
    args: &Value,
) -> (String, serde_json::Map<String, Value>) {
    let mut url = template.to_string();
    let mut remaining = args.as_object().cloned().unwrap_or_default();
    let keys: Vec<String> = remaining.keys().cloned().collect();
    for k in keys {
        let placeholder = format!("{{{k}}}");
        if url.contains(&placeholder) {
            if let Some(val) = remaining.remove(&k) {
                let s = match &val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                url = url.replace(&placeholder, &url_component(&s));
            }
        }
    }
    (url, remaining)
}

fn url_component(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string");
        }
    }
    encoded
}

fn unconfirmed_fact(contract: &OpenSocketContract, args: &Value, detail: String) -> ToolFact {
    let subject = args
        .get("subject")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .filter(|_| true)
        .unwrap_or_else(|| {
            if contract.default_subject.is_empty() {
                contract.name.clone()
            } else {
                contract.default_subject.clone()
            }
        });
    ToolFact {
        subject,
        action: contract.default_action.clone(),
        basis: FactBasis::DeviceReceipt,
        confirmed: false,
        value: None,
        detail: Some(detail),
    }
}

fn parse_mutate_receipt(
    contract: &OpenSocketContract,
    args: &Value,
    body: &Value,
) -> (bool, ToolFact) {
    let receipt = body
        .get("fact")
        .or_else(|| body.get("receipt"))
        .unwrap_or(body);
    let confirmed = receipt
        .get("confirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let subject = receipt
        .get("subject")
        .and_then(Value::as_str)
        .or_else(|| args.get("subject").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if contract.default_subject.is_empty() {
                contract.name.clone()
            } else {
                contract.default_subject.clone()
            }
        });
    let action = receipt
        .get("action")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| contract.default_action.clone());
    let value = receipt
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_string);
    let detail = receipt
        .get("detail")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            if confirmed {
                None
            } else {
                Some("远端插座未返回确认回执".into())
            }
        });
    (
        confirmed,
        ToolFact {
            subject,
            action,
            basis: FactBasis::DeviceReceipt,
            confirmed,
            value,
            detail,
        },
    )
}

async fn execute_contract(
    token: Option<&str>,
    client: &reqwest::Client,
    contract: &OpenSocketContract,
    args: Value,
    ctx: &ToolCtx,
) -> ToolResult {
    if ctx.is_cancelled() {
        return ToolResult::cancelled();
    }
    if !contract.enabled {
        return ToolResult::failed(
            SOCKET_DISABLED,
            format!("open socket '{}' is disabled", contract.name),
            false,
        );
    }
    let args_obj = if args.is_null() { json!({}) } else { args };
    if !super::argument_schema::valid_parameters(&contract.parameters) {
        return ToolResult::failed(
            SOCKET_ARGUMENTS,
            "repair the saved socket argument schema",
            false,
        );
    }
    if let Err(error) = super::argument_schema::validate_arguments(&args_obj, &contract.parameters)
    {
        return ToolResult::failed(SOCKET_ARGUMENTS, error, false);
    }

    let (url, remaining) = build_request_url_and_remaining(&contract.endpoint_url, &args_obj);
    let Some(url) = endpoint(&url).filter(|request| {
        endpoint(&contract.endpoint_url).is_some_and(|source| request.origin() == source.origin())
    }) else {
        return ToolResult::failed(
            SOCKET_ARGUMENTS,
            "socket endpoint is invalid or its host changed",
            false,
        );
    };
    let method = match contract.method {
        HttpMethod::GET => reqwest::Method::GET,
        HttpMethod::POST => reqwest::Method::POST,
        HttpMethod::PUT => reqwest::Method::PUT,
        HttpMethod::PATCH => reqwest::Method::PATCH,
        HttpMethod::DELETE => reqwest::Method::DELETE,
    };

    let mut req = client.request(method, url).timeout(Duration::from_millis(
        contract.timeout_ms.clamp(500, 30_000),
    ));

    if contract.method == HttpMethod::GET {
        if !remaining.is_empty() {
            let pairs: Vec<(String, String)> = remaining
                .into_iter()
                .map(|(k, v)| {
                    let s = match v {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (k, s)
                })
                .collect();
            req = req.query(&pairs);
        }
    } else {
        req = req.json(&Value::Object(remaining));
    }

    if let Some(token) = token.filter(|token| !token.is_empty()) {
        if contract.auth_header.is_empty() {
            req = req.bearer_auth(token);
        } else {
            req = req.header(&contract.auth_header, token);
        }
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
                    code: SOCKET_TIMEOUT.into(),
                    retryable: true,
                    detail: format!("open socket '{}' timed out: {e}", contract.name),
                }),
                fact: None,
            };
            if contract.effect == SideEffect::Mutate {
                r.fact = Some(unconfirmed_fact(contract, &args_obj, "请求超时".into()));
            }
            return r;
        }
        Err(e) => {
            let mut r = ToolResult::failed(
                SOCKET_UNREACHABLE,
                format!("open socket '{}' unreachable: {e}", contract.name),
                true,
            );
            if contract.effect == SideEffect::Mutate {
                r.fact = Some(unconfirmed_fact(
                    contract,
                    &args_obj,
                    "远端服务不可达".into(),
                ));
            }
            return r;
        }
    };

    let status = resp.status();
    let bytes = match resp.bytes().await {
        Ok(b) if b.len() <= MAX_RESPONSE_BODY => b,
        Ok(_) => {
            return ToolResult::failed(
                SOCKET_RESPONSE_TOO_LARGE,
                "open socket response exceeded 64 KiB",
                false,
            );
        }
        Err(e) => {
            return ToolResult::failed(SOCKET_HTTP, format!("failed reading response: {e}"), true);
        }
    };

    if !status.is_success() {
        let mut r = ToolResult::failed(
            SOCKET_HTTP,
            format!(
                "open socket '{}' returned HTTP {}",
                contract.name,
                status.as_u16()
            ),
            status.is_server_error(),
        );
        if contract.effect == SideEffect::Mutate {
            r.fact = Some(unconfirmed_fact(
                contract,
                &args_obj,
                format!("HTTP {}", status.as_u16()),
            ));
        }
        return r;
    }

    let mut body: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&bytes).to_string() }));
    if let Some(token) = token {
        super::openapi::redact_secret(&mut body, token);
    }

    match contract.effect {
        SideEffect::Query => ToolResult::observed_ok(body),
        SideEffect::Mutate => {
            let (confirmed, fact) = parse_mutate_receipt(contract, &args_obj, &body);
            if confirmed {
                ToolResult {
                    status: ToolStatus::Completed,
                    evidence: Evidence::DeviceConfirmed,
                    result: Some(body),
                    error: None,
                    fact: Some(fact),
                }
            } else {
                ToolResult {
                    status: ToolStatus::Unknown,
                    evidence: Evidence::Accepted,
                    result: Some(body),
                    error: Some(ToolError {
                        code: "SOCKET_UNCONFIRMED".into(),
                        retryable: false,
                        detail: "mutating open socket did not return confirmed=true receipt".into(),
                    }),
                    fact: Some(fact),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Registry;
    use std::sync::atomic::AtomicBool;

    fn ctx() -> ToolCtx {
        ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn validation_rejects_reserved_and_unsafe_contracts() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = OpenSocketRuntime::new(store);

        let shell = OpenSocketContract {
            name: "shell_command".into(),
            service: "open_socket".into(),
            description: "bad".into(),
            parameters: json!({ "type": "object" }),
            effect: SideEffect::Mutate,
            endpoint_url: "http://127.0.0.1:9999/exec".into(),
            method: HttpMethod::POST,
            enabled: true,
            timeout_ms: 5000,
            auth_header: String::new(),
            default_subject: String::new(),
            default_action: "other".into(),
        };
        assert!(rt.register(&Registry::new(), shell, None, "test").is_err());

        let query_delete = OpenSocketContract {
            name: "desk.reset".into(),
            service: "open_socket".into(),
            description: "bad query method".into(),
            parameters: json!({ "type": "object" }),
            effect: SideEffect::Query,
            endpoint_url: "http://127.0.0.1:9999/reset".into(),
            method: HttpMethod::DELETE,
            enabled: true,
            timeout_ms: 5000,
            auth_header: String::new(),
            default_subject: String::new(),
            default_action: "other".into(),
        };
        assert!(rt
            .register(&Registry::new(), query_delete, None, "test")
            .is_err());
    }

    #[tokio::test]
    async fn dynamic_registration_updates_registry_and_unregistered_fails() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rt = OpenSocketRuntime::new(store);
        let reg = Registry::new();
        reg.add_provider(rt.clone())
            .expect("valid unique tool registration");

        // Before registration, calling an unregistered socket returns an explicit error.
        let missing = reg.call("desk.status", json!({}), &ctx()).await;
        assert_eq!(missing.status, ToolStatus::Failed);
        assert_eq!(missing.error.as_ref().unwrap().code, "UNKNOWN_TOOL");

        let direct_missing = rt.call_by_name("desk.status", json!({}), &ctx()).await;
        assert_eq!(direct_missing.status, ToolStatus::Failed);
        assert_eq!(
            direct_missing.error.as_ref().unwrap().code,
            SOCKET_UNREGISTERED
        );

        // Register a new declarative socket contract.
        rt.register(
            &reg,
            OpenSocketContract {
                name: "desk.status".into(),
                service: "desk".into(),
                description: "查询书桌传感器状态".into(),
                parameters: json!({
                    "type": "object",
                    "properties": { "sensor": { "type": "string" } },
                    "required": ["sensor"]
                }),
                effect: SideEffect::Query,
                endpoint_url: "http://127.0.0.1:9/status".into(),
                method: HttpMethod::GET,
                enabled: true,
                timeout_ms: 2000,
                auth_header: String::new(),
                default_subject: String::new(),
                default_action: "get".into(),
            },
            None,
            "test",
        )
        .unwrap();

        assert!(reg.names().contains(&"desk.status".to_string()));
        assert!(reg
            .ready_capabilities()
            .iter()
            .any(|s| s.service == "desk" && s.tools.iter().any(|t| t.name == "desk.status")));

        // Missing required arg fails before any HTTP request.
        let bad_args = reg.call("desk.status", json!({}), &ctx()).await;
        assert_eq!(bad_args.status, ToolStatus::Failed);
        assert_eq!(bad_args.error.as_ref().unwrap().code, SOCKET_ARGUMENTS);
    }
}
