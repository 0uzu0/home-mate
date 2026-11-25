//! Unified tool contract: descriptors, execution, evidence.
//!
//! The brain proposes tool calls; this layer owns validation, scoping and
//! the truth about what actually happened. Business results and transport
//! success are separate; unknown stays unknown (never "success").

mod argument_schema;
pub mod catalog;
pub mod home;
pub mod http;
pub mod learning;
pub mod management;
pub mod master_brain;
pub mod memory;
pub mod mocks;
pub mod music;
pub mod news;
pub mod open_socket;
pub mod openapi;
pub(crate) mod openapi_verify;
pub mod personal_media;
pub mod reminder;
pub mod search;
pub mod time;
pub mod weather;

use crate::llm::ToolSpec;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideEffect {
    Query,
    Mutate,
}

/// Whether a household can use a registered service RIGHT NOW (R1h).
///
/// The prompt's capability list is rendered from this: a service that is not
/// registered is simply absent (「没注册＝没有」), and a registered service the
/// household has not configured is named as unconfigured instead of being
/// offered as if it worked. `Hidden` is a tool that exists for dispatch and
/// tests but is not a household capability at all (a fixed mock dataset is
/// not a service the model may advertise as real).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Ready,
    Unconfigured,
    Disabled,
    Hidden,
}

/// One tool as the model-facing capability list renders it. Deliberately not
/// a JSON schema: the parameters already travel in `Registry::specs()`, and
/// the prompt only needs the sentence that says what the tool is for.
#[derive(Debug, Clone)]
pub struct CapabilityTool {
    pub name: String,
    pub side_effect: SideEffect,
    pub description: String,
}

/// A service and the tools it contributes. Services are the unit of
/// registration: adding weather = registering its tools + configuring it,
/// never editing the prompt (R1h/RH-3).
#[derive(Debug, Clone)]
pub struct ServiceCapability {
    pub service: Cow<'static, str>,
    pub availability: Availability,
    pub tools: Vec<CapabilityTool>,
}

impl ServiceCapability {
    pub fn label(&self) -> &'static str {
        match self.availability {
            Availability::Ready => "已配置",
            Availability::Unconfigured => "未配置",
            Availability::Disabled => "已关闭",
            Availability::Hidden => "内部",
        }
    }

    /// The state a person would see on the console's service card.
    pub fn state_word(&self) -> &'static str {
        match self.availability {
            Availability::Ready => "ready",
            Availability::Unconfigured => "unconfigured",
            Availability::Disabled => "disabled",
            Availability::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Query tools: no side effect to attest.
    Observed,
    /// Mutating tools: request accepted by this layer.
    Accepted,
    /// Handed to the executing sink/service.
    Submitted,
    /// Transport-level send succeeded.
    Sent,
    /// The device/sink confirmed the action.
    DeviceConfirmed,
    /// Outcome could not be observed (timeout/disconnect). NOT success.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    #[serde(rename = "awaiting_confirmation")]
    AwaitingConfirmation,
    Completed,
    Failed,
    Partial,
    Cancelled,
    Unknown,
}

impl ToolStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Partial => "partial",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolError {
    pub code: String,
    pub retryable: bool,
    pub detail: String,
}

/// A structured action fact from a MUTATING tool (R2-BM5).
///
/// The tool reports what it did and whether the executing backend READ THE
/// TARGET STATE BACK and found the requested value (`confirmed`). The CORE
/// renders the sentence a person hears from this struct — a model's prose can
/// therefore never be the only source of 「已关/已开/已播放」. A tool that
/// cannot observe the target state leaves `confirmed=false` even when the
/// transport accepted the request (`service_accepted ≠ confirmed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FactBasis {
    /// The backend READ the target state back and it matched (Home
    /// Assistant: `GET /api/states/<entity>` after the service call). Only
    /// `Evidence::DeviceConfirmed` may combine with this.
    #[default]
    ReadBack,
    /// The DEVICE reported its own play stage for this play (media stream
    /// `accepted`). This is a device event, not a transport write; the
    /// receipt is `Sent` or `DeviceConfirmed`.
    DeviceStage,
    /// The DEVICE reported the resulting state/value in its own receipt
    /// (R2-BM6: `volume_receipt` for volume, `mode_report` for duplex). Only
    /// `Evidence::DeviceConfirmed` may combine with this — the request we
    /// sent is never the observation.
    DeviceReceipt,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolFact {
    /// The thing acted on, in the user's vocabulary (「儿童房的灯」/《夜曲》).
    pub subject: String,
    /// Normalized action: `on`|`off`|`play`|`pause`|`resume`|`set`|`switch`|`other`.
    pub action: String,
    /// What kind of observation backs `confirmed` (default: read-back).
    #[serde(default)]
    pub basis: FactBasis,
    /// True ONLY when the observation in `basis` matched the requested state.
    pub confirmed: bool,
    /// The value the action targeted, already in the user's vocabulary
    /// ("80%", "全双工"). Rendered with `set`/`switch` actions; omitted for
    /// actions where the subject already says it (灯 on/off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Short, human, tool-controlled reason for a failure/unknown. Never a
    /// raw provider payload; the console may show it, the fact renderer may
    /// append it in a bounded clause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub status: ToolStatus,
    pub evidence: Evidence,
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    /// R2-BM5: the core-owned fact for a mutating tool. `None` for queries
    /// and for refusals that have no target to talk about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<ToolFact>,
}

impl ToolResult {
    pub fn observed_ok(result: serde_json::Value) -> Self {
        Self {
            status: ToolStatus::Completed,
            evidence: Evidence::Observed,
            result: Some(result),
            error: None,
            fact: None,
        }
    }
    pub fn failed(code: &str, detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            status: ToolStatus::Failed,
            evidence: Evidence::Observed,
            result: None,
            error: Some(ToolError {
                code: code.into(),
                retryable,
                detail: detail.into(),
            }),
            fact: None,
        }
    }
    pub fn cancelled() -> Self {
        Self {
            status: ToolStatus::Cancelled,
            evidence: Evidence::Accepted,
            result: None,
            error: None,
            fact: None,
        }
    }
    /// Attach the core-owned action fact (builder style, so existing
    /// constructors stay untouched).
    pub fn with_fact(mut self, fact: ToolFact) -> Self {
        self.fact = Some(fact);
        self
    }

    /// Short human-readable summary for the model, trace and console.
    pub fn summary(&self) -> String {
        match (&self.status, &self.error) {
            (ToolStatus::AwaitingConfirmation, _) => {
                "awaiting paired approval and separate execution".into()
            }
            (ToolStatus::Completed, _) => format!("completed ({:?})", self.evidence),
            (ToolStatus::Cancelled, _) => "cancelled before side effect".into(),
            // R2-BM5: an unknown outcome must NAME what could not be
            // observed (timeout / read-back mismatch / unrecognised stage),
            // not just say "unknown" — the code is what an audit can follow.
            (ToolStatus::Unknown, Some(e)) => {
                format!(
                    "outcome unknown [{}]: {} — treat as unconfirmed",
                    e.code, e.detail
                )
            }
            (ToolStatus::Unknown, None) => "outcome unknown — treat as unconfirmed".into(),
            (ToolStatus::Failed, Some(e)) => format!("failed [{}]: {}", e.code, e.detail),
            (ToolStatus::Failed, None) => "failed".into(),
            (ToolStatus::Partial, _) => "partially completed".into(),
        }
    }
}

/// Execution context handed to every call.
pub struct ToolCtx {
    pub task_id: String,
    pub call_id: String,
    pub device_id: Option<String>,
    pub trace_id: Option<String>,
    cancel: Arc<AtomicBool>,
    authority: Option<crate::execution::Authority>,
    model_exchange: bool,
    confirmation_completion: Mutex<
        Option<(
            std::sync::Weak<crate::confirmation::ConfirmationService>,
            String,
        )>,
    >,
}

impl ToolCtx {
    pub fn new(
        task_id: impl Into<String>,
        call_id: impl Into<String>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            call_id: call_id.into(),
            device_id: None,
            trace_id: None,
            cancel,
            authority: None,
            model_exchange: false,
            confirmation_completion: Mutex::new(None),
        }
    }
    pub(crate) fn with_model_exchange(mut self) -> Self {
        self.model_exchange = true;
        self
    }
    pub(crate) fn requires_model_exchange(&self) -> bool {
        self.model_exchange
    }
    pub(crate) fn register_confirmation(
        &self,
        service: std::sync::Weak<crate::confirmation::ConfirmationService>,
        id: String,
    ) {
        *crate::lock_or_recover(
            &self.confirmation_completion,
            "tool.confirmation.completion",
        ) = Some((service, id));
    }
    pub(crate) fn complete_confirmation_exchange(&self) {
        let completion = crate::lock_or_recover(
            &self.confirmation_completion,
            "tool.confirmation.completion",
        )
        .clone();
        if let Some((service, id)) = completion {
            if let Some(service) = service.upgrade() {
                let _ = service.exchange_complete(&id);
            }
        }
    }
    #[cfg(test)]
    pub(crate) async fn before_confirmation_receipt(&self) {
        let completion = crate::lock_or_recover(
            &self.confirmation_completion,
            "tool.confirmation.completion",
        )
        .clone();
        if let Some((service, _)) = completion {
            if let Some(service) = service.upgrade() {
                service.pause_before_receipt().await;
            }
        }
    }
    pub fn with_authority(mut self, authority: crate::execution::Authority) -> Self {
        self.authority = Some(authority.with_cancel(self.cancel.clone()));
        self
    }
    pub(crate) fn with_task_authority(
        mut self,
        authority: crate::execution::Authority,
        scope: Arc<crate::task::CancellationScope>,
    ) -> Self {
        self.authority = Some(authority.with_cancel_scope(scope));
        self
    }
    pub fn authority(&self) -> Option<&crate::execution::Authority> {
        self.authority.as_ref()
    }
    pub fn is_lan_exempt(&self) -> bool {
        self.authority.as_ref().is_some_and(|a| a.is_lan_exempt())
    }
    pub fn with_provenance(mut self, device_id: Option<String>, trace_id: Option<String>) -> Self {
        self.device_id = device_id;
        self.trace_id = trace_id;
        self
    }
    /// The task's cancel flag, for work that outlives the call (a media
    /// stream keeps pushing audio after the tool returned).
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
    /// Pre-execution hook: reject work for cancelled tasks.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDescriptor {
    pub name: Cow<'static, str>,
    /// The service this tool belongs to (`time`, `music`, `weather`, `home`).
    /// Capabilities are grouped BY SERVICE: that is what the prompt renders
    /// and what a console card configures.
    pub service: Cow<'static, str>,
    pub side_effect: SideEffect,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    /// Safe projection for model-call history, traces and UI; execution keeps
    /// the original arguments. A tool owns its declared sensitive fields.
    fn redact_arguments(&self, args: &serde_json::Value) -> serde_json::Value {
        args.clone()
    }
    /// Whether this service is usable right now. The default is Ready (a
    /// local, always-available tool); configured services report their own
    /// config state so the prompt never advertises something that cannot
    /// answer.
    fn availability(&self) -> Availability {
        Availability::Ready
    }
    /// Providers that recreate wrappers must identify the captured execution
    /// contract, including targets/configuration absent from the descriptor.
    /// Otherwise the catalog conservatively uses this instance's identity.
    fn catalog_revision(&self) -> Option<String> {
        None
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult;
}

/// Dynamic tool source (e.g. declarative open sockets loaded from store).
/// Queried by `Registry` so new contracts become visible to both the prompt
/// and the execution loop without modifying `agent/mod.rs`.
pub trait DynamicToolProvider: Send + Sync {
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// A registration has its own lifetime even when callers reuse a tool Arc.
struct DynamicRegistration {
    tool: Arc<dyn Tool>,
    generation: u64,
}

#[async_trait::async_trait]
impl Tool for DynamicRegistration {
    fn descriptor(&self) -> ToolDescriptor {
        self.tool.descriptor()
    }
    fn redact_arguments(&self, args: &serde_json::Value) -> serde_json::Value {
        self.tool.redact_arguments(args)
    }
    fn availability(&self) -> Availability {
        self.tool.availability()
    }
    fn catalog_revision(&self) -> Option<String> {
        Some(format!(
            "dynamic:{}:{:p}:{}",
            self.generation,
            Arc::as_ptr(&self.tool),
            self.tool.catalog_revision().unwrap_or_default()
        ))
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult {
        self.tool.execute(args, ctx).await
    }
}

pub struct Registry {
    // Order: catalog gate -> provider state -> Store. Never await under it.
    catalog_gate: Mutex<()>,
    tools: HashMap<String, Arc<dyn Tool>>,
    dynamic_tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    providers: RwLock<Vec<Arc<dyn DynamicToolProvider>>>,
    dynamic_generation: AtomicU64,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            catalog_gate: Mutex::new(()),
            tools: HashMap::new(),
            dynamic_tools: RwLock::new(HashMap::new()),
            providers: RwLock::new(Vec::new()),
            dynamic_generation: AtomicU64::new(0),
        }
    }

    fn read_lock<'a, T>(
        lock: &'a RwLock<T>,
        name: &'static str,
    ) -> std::sync::RwLockReadGuard<'a, T> {
        match lock.read() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::error!(lock = name, "rwlock poisoned — recovering read guard");
                poisoned.into_inner()
            }
        }
    }

    fn write_lock<'a, T>(
        lock: &'a RwLock<T>,
        name: &'static str,
    ) -> std::sync::RwLockWriteGuard<'a, T> {
        match lock.write() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::error!(lock = name, "rwlock poisoned — recovering write guard");
                poisoned.into_inner()
            }
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), catalog::CatalogError> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let d = tool.descriptor();
        catalog::validate_name(&d.name)?;
        if self.catalog_unlocked()?.contains(&d.name) {
            return Err(catalog::CatalogError::Duplicate(d.name.to_string()));
        }
        self.tools.insert(d.name.to_string(), tool);
        Ok(())
    }

    /// Register a dynamic tool at runtime (thread-safe). Static tools cannot
    /// be shadowed by dynamic tools.
    pub fn register_dynamic(&self, tool: Arc<dyn Tool>) -> bool {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let name = tool.descriptor().name.to_string();
        if catalog::validate_name(&name).is_err() || self.tools.contains_key(&name) {
            return false;
        }
        let providers = Self::read_lock(&self.providers, "tool.providers");
        let mut dynamic = Self::write_lock(&self.dynamic_tools, "tool.dynamic");
        if dynamic.contains_key(&name)
            || providers.iter().any(|p| {
                p.tools()
                    .iter()
                    .any(|t| t.descriptor().name.as_ref() == name)
            })
        {
            return false;
        }
        let generation = self.dynamic_generation.fetch_add(1, Ordering::SeqCst) + 1;
        dynamic.insert(name, Arc::new(DynamicRegistration { tool, generation }));
        true
    }

    /// Remove a previously registered dynamic tool by name.
    pub fn unregister_dynamic(&self, name: &str) -> bool {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let mut dynamic = Self::write_lock(&self.dynamic_tools, "tool.dynamic");
        if dynamic.remove(name).is_none() {
            return false;
        }
        self.dynamic_generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Attach a dynamic tool provider (e.g. `OpenSocketRuntime`).
    pub fn add_provider(
        &self,
        provider: Arc<dyn DynamicToolProvider>,
    ) -> Result<(), catalog::CatalogError> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let mut providers = Self::write_lock(&self.providers, "tool.providers");
        if providers.iter().any(|p| Arc::ptr_eq(p, &provider)) {
            return Err(catalog::CatalogError::Duplicate(
                "provider registration".into(),
            ));
        }
        let mut tools: Vec<_> = self.tools.values().cloned().collect();
        tools.extend(
            Self::read_lock(&self.dynamic_tools, "tool.dynamic")
                .values()
                .cloned(),
        );
        for p in providers.iter() {
            tools.extend(p.tools());
        }
        tools.extend(provider.tools());
        catalog::CatalogSnapshot::build(tools)?;
        providers.push(provider);
        Ok(())
    }

    /// Startup must keep persisted sources attached for management repair.
    /// An invalid source blocks the whole catalog, never the management API.
    /// New live registrations should use add_provider, which rejects errors.
    pub fn attach_provider_for_repair(
        &self,
        provider: Arc<dyn DynamicToolProvider>,
    ) -> Result<(), catalog::CatalogError> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        if Self::read_lock(&self.providers, "tool.providers")
            .iter()
            .any(|p| Arc::ptr_eq(p, &provider))
        {
            return Err(catalog::CatalogError::Duplicate(
                "provider registration".into(),
            ));
        }
        Self::write_lock(&self.providers, "tool.providers").push(provider);
        self.catalog_unlocked().map(|_| ())
    }

    pub fn catalog(&self) -> Result<catalog::CatalogSnapshot, catalog::CatalogError> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        self.catalog_unlocked()
    }

    /// Confirmation lock order: CancellationScope → catalog → OpenAPI gate → Store.
    pub(crate) fn with_catalog<T>(
        &self,
        f: impl FnOnce(catalog::CatalogSnapshot) -> crate::store::Result<T>,
    ) -> crate::store::Result<T> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let catalog = self
            .catalog_unlocked()
            .map_err(|_| crate::store::StoreError::Authority("CONFIRMATION_STALE".into()))?;
        f(catalog)
    }
    fn catalog_unlocked(&self) -> Result<catalog::CatalogSnapshot, catalog::CatalogError> {
        let mut out: Vec<_> = self.tools.values().cloned().collect();
        out.extend(
            Self::read_lock(&self.dynamic_tools, "tool.dynamic")
                .values()
                .cloned(),
        );
        for provider in Self::read_lock(&self.providers, "tool.providers").iter() {
            out.extend(provider.tools());
        }
        catalog::CatalogSnapshot::build(out)
    }

    /// Validate a complete replacement without reading the target provider
    /// under its write lock. The closure commits durable state and publishes
    /// before releasing the same gate used by all registration domains.
    pub(crate) fn update_provider<T>(
        &self,
        target: &dyn DynamicToolProvider,
        update: impl FnOnce(&dyn Fn(Vec<Arc<dyn Tool>>) -> Result<(), String>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let others = self.other_provider_tools(target)?;
        update(&|candidate| {
            let mut all = others.clone();
            all.extend(candidate);
            catalog::CatalogSnapshot::build(all)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    /// Snapshot the actual competing names before configuration preflight.
    /// Publication revalidates the complete candidate under the same gate.
    pub(crate) fn competing_tool_names(
        &self,
        target: &dyn DynamicToolProvider,
    ) -> Result<Vec<String>, String> {
        let _gate = crate::lock_or_recover(&self.catalog_gate, "tool.catalog");
        let baseline = catalog::CatalogSnapshot::build(self.other_provider_tools(target)?)
            .map_err(|error| error.to_string())?;
        Ok(baseline
            .specs()
            .into_iter()
            .map(|spec| spec.canonical_name)
            .collect())
    }

    fn other_provider_tools(
        &self,
        target: &dyn DynamicToolProvider,
    ) -> Result<Vec<Arc<dyn Tool>>, String> {
        let mut others: Vec<_> = self.tools.values().cloned().collect();
        others.extend(
            Self::read_lock(&self.dynamic_tools, "tool.dynamic")
                .values()
                .cloned(),
        );
        let target_id = target as *const dyn DynamicToolProvider as *const ();
        let mut attached = false;
        for provider in Self::read_lock(&self.providers, "tool.providers").iter() {
            if Arc::as_ptr(provider) as *const () == target_id {
                attached = true;
            } else {
                others.extend(provider.tools());
            }
        }
        if !attached {
            return Err("provider is not attached to this registry".into());
        }
        Ok(others)
    }

    fn all_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.catalog().map(|c| c.tools()).unwrap_or_else(|error| {
            tracing::error!(%error, "invalid tool catalog");
            Vec::new()
        })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.catalog().map(|c| c.specs()).unwrap_or_default()
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .all_tools()
            .into_iter()
            .map(|t| t.descriptor().name.to_string())
            .collect();
        v.sort();
        v
    }

    /// The capability list the model is grounded on (R1h). Grouped by
    /// service and ordered deterministically; `Hidden` tools are dropped
    /// entirely. This is the ONLY source of "what this household can do" —
    /// the prompt carries the rules, the registry carries the facts.
    pub fn capabilities(&self) -> Vec<ServiceCapability> {
        let mut by_service: std::collections::BTreeMap<Cow<'static, str>, ServiceCapability> =
            std::collections::BTreeMap::new();
        for tool in self.all_tools() {
            let d = tool.descriptor();
            let availability = tool.availability();
            if availability == Availability::Hidden {
                continue;
            }
            let service_key = d.service.clone();
            let entry = by_service
                .entry(service_key)
                .or_insert_with(|| ServiceCapability {
                    service: d.service,
                    availability,
                    tools: Vec::new(),
                });
            // A service is only as ready as its least-ready tool: a music
            // search that works with a play that has no device configured is
            // not a music service.
            entry.availability = match (entry.availability, availability) {
                (Availability::Disabled, _) | (_, Availability::Disabled) => Availability::Disabled,
                (Availability::Unconfigured, _) | (_, Availability::Unconfigured) => {
                    Availability::Unconfigured
                }
                _ => Availability::Ready,
            };
            entry.tools.push(CapabilityTool {
                name: d.name.to_string(),
                side_effect: d.side_effect,
                description: d.description,
            });
        }
        by_service
            .into_values()
            .map(|mut s| {
                s.tools.sort_by(|a, b| a.name.cmp(&b.name));
                s
            })
            .collect()
    }

    /// Ready services only — what the model may claim to have.
    pub fn ready_capabilities(&self) -> Vec<ServiceCapability> {
        self.capabilities()
            .into_iter()
            .filter(|s| s.availability == Availability::Ready)
            .collect()
    }

    /// Services that are registered but not usable, with their state word.
    /// They are NOT capabilities («没注册≠没配置»: the model may say "还没配置"
    /// but must not offer to do the work).
    pub fn unavailable_services(&self) -> Vec<ServiceCapability> {
        self.capabilities()
            .into_iter()
            .filter(|s| {
                matches!(
                    s.availability,
                    Availability::Unconfigured | Availability::Disabled
                )
            })
            .collect()
    }

    /// Dispatch through the common execution boundary: existence check,
    /// cancellation check, then the tool itself. Unknown tools return a
    /// structured failure the model can see and react to.
    pub async fn call(&self, name: &str, args: serde_json::Value, ctx: &ToolCtx) -> ToolResult {
        let catalog = match self.catalog() {
            Ok(c) => c,
            Err(e) => return ToolResult::failed("TOOL_CATALOG_INVALID", e.to_string(), false),
        };
        let Some(tool) = catalog.tool(name) else {
            return ToolResult::failed("UNKNOWN_TOOL", format!("no tool named '{name}'"), false);
        };
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        if ctx.authority().is_some_and(|a| a.check().is_err()) {
            return ToolResult::failed(
                "EXECUTION_AUTHORITY_EXPIRED",
                "execution scope is no longer valid",
                false,
            );
        }
        tool.execute(args, ctx).await
    }

    /// Model dispatch is restricted to its advertised immutable catalog.
    pub async fn call_advertised(
        &self,
        advertised: &catalog::CatalogSnapshot,
        wire: &str,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let Some(name) = advertised.canonical_name(wire) else {
            return ToolResult::failed(
                "UNKNOWN_TOOL",
                "name was not advertised to this model call",
                false,
            );
        };
        let current = match self.catalog() {
            Ok(c) => c,
            Err(e) => return ToolResult::failed("TOOL_CATALOG_INVALID", e.to_string(), false),
        };
        if !advertised.same_entry(&current, name) {
            return ToolResult::failed(
                "TOOL_STALE",
                format!("tool '{name}' changed or was removed"),
                false,
            );
        }
        // Validation above is the dispatch admission point. Already admitted
        // external effects cannot be recalled by a later catalog mutation.
        let tool = advertised.tool(name).expect("advertised tool exists");
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        if ctx.authority().is_some_and(|a| a.check().is_err()) {
            return ToolResult::failed(
                "EXECUTION_AUTHORITY_EXPIRED",
                "execution scope is no longer valid",
                false,
            );
        }
        tool.execute(args, ctx).await
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
