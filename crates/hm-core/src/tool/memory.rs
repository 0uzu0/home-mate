//! `memory.remember` — the only voice-side writer to the household memory.
//!
//! Policy (user, 2026-11-13): memory holds important events and the user's
//! profile — names, aliases, preferences, household facts. It does NOT hold
//! operational state (which service is configured, tool receipts, volume,
//! playback). That split is stated in the description the model reads, and the
//! store itself is bounded and console-editable; the model is told to call
//! this only when the user explicitly asks to remember something.
use crate::memory::{MemoryService, KINDS};
use crate::shared_memory::{ObservationInput, SharedMemoryService};
use crate::tool::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use crate::user_md::{
    UserMdService, MAX_USER_MD_CONTENT_CHARS, MAX_USER_MD_KEY_CHARS, MAX_USER_MD_RATIONALE_CHARS,
};
use crate::working_memory::{WorkingMemoryStore, MAX_CONTENT_LEN, MAX_KEY_LEN};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct MemoryTool {
    svc: Arc<MemoryService>,
    shared_memory: Option<Arc<SharedMemoryService>>,
}

impl MemoryTool {
    pub fn new(svc: Arc<MemoryService>) -> Self {
        Self {
            svc,
            shared_memory: None,
        }
    }

    pub fn with_shared_memory(mut self, shared_memory: Arc<SharedMemoryService>) -> Self {
        self.shared_memory = Some(shared_memory);
        self
    }
}

#[async_trait::async_trait]
impl Tool for MemoryTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "memory.remember".into(),
            service: "memory".into(),
            side_effect: SideEffect::Mutate,
            description: String::from("记住用户明确交代的长期信息：称呼/名字的正确说法、偏好、习惯、家里重要的事。\
                          只在用户说「记住…」「以后都…」「我喜欢/不喜欢…」或纠正一个听错的名字时调用；\
                          一次一句话。不要记服务是否配置、工具调用、播放/音量这类会自己变化的状态。"),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "一句话，最多 200 字，例如「奶奶周日来吃饭」" },
                    "kind": {
                        "type": "string",
                        "enum": KINDS,
                        "description": "fact=事实，preference=偏好，event=重要事件，alias=纠正名字（写成「听错的=正确的」）"
                    }
                },
                "required": ["text"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(authority) = ctx.authority() else {
            return ToolResult::failed("MEMORY_REJECTED", "memory destination unavailable", false);
        };
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("fact");
        match self.svc.add_authorized(
            authority,
            text,
            kind,
            if authority.physical_device_id().is_some() {
                "voice"
            } else {
                "console"
            },
        ) {
            Ok(item) => {
                if let Some(ref shared) = self.shared_memory {
                    let dimension = match item.kind.as_str() {
                        "preference" => "preferences",
                        "fact" => "facts",
                        "event" => "schedule",
                        _ => "general",
                    };
                    let _ = shared.enqueue_observation_scoped(ObservationInput {
                        source_device: authority.physical_device_id().unwrap_or("console"),
                        scope_device: &item.scope_device,
                        scope_person: &item.scope_person,
                        room: authority.scope().room.as_deref().unwrap_or(""),
                        trace_id: ctx.trace_id.as_deref().unwrap_or(""),
                        content: &item.text,
                        domain: "notes",
                        record_id: &item.id,
                        event_kind: "explicit_save",
                        dimension,
                        expected_epoch: Some(authority.epoch() as i64),
                    });
                }
                ToolResult::observed_ok(
                    json!({"remembered":item.text,"kind":item.kind,"scope_person":item.scope_person,"scope_device":item.scope_device}),
                )
            }
            Err(e) => ToolResult::failed("MEMORY_REJECTED", e, false),
        }
    }
}

/// Write or update a long-term working memory entry (project path, progress,
/// active work context). Strictly separate from the 200-item household notes.
pub struct WorkingMemorySetTool {
    store: Arc<WorkingMemoryStore>,
    shared_memory: Option<Arc<SharedMemoryService>>,
}

impl WorkingMemorySetTool {
    pub fn new(store: Arc<WorkingMemoryStore>) -> Self {
        Self {
            store,
            shared_memory: None,
        }
    }

    pub fn with_shared_memory(mut self, shared_memory: Arc<SharedMemoryService>) -> Self {
        self.shared_memory = Some(shared_memory);
        self
    }
}

#[async_trait::async_trait]
impl Tool for WorkingMemorySetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: std::borrow::Cow::Borrowed("workmem.set"),
            service: std::borrow::Cow::Borrowed("memory"),
            side_effect: SideEffect::Mutate,
            description: String::from(
                "保存或更新长期工作记忆（如项目路径、上次做到哪、工作上下文或跨会话任务进度）。\
                 与家庭短注分开，可按人（scope_person）和设备（scope_device）记录更长内容（最多 4000 字）。\
                 不要把音量、当前播放歌曲或服务配置写进家庭短注。",
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key_or_topic": {
                        "type": "string",
                        "description": format!("工作主题或键名，最多 {MAX_KEY_LEN} 字，如 project_path、last_progress")
                    },
                    "content": {
                        "type": "string",
                        "description": format!("工作记忆内容，最多 {MAX_CONTENT_LEN} 字")
                    },
                    "scope_person": {
                        "type": "string",
                        "description": "可选：关联的人名或用户标识，省略使用本轮授权范围；空字符串不能扩大权限"
                    },
                    "scope_device": {
                        "type": "string",
                        "description": "可选：关联的设备 ID，省略使用本轮授权范围；空字符串不能扩大权限"
                    }
                },
                "required": ["key_or_topic", "content"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(authority) = ctx.authority() else {
            return ToolResult::failed(
                "WORKING_MEMORY_REJECTED",
                "memory destination unavailable",
                false,
            );
        };
        let Some((person, device)) = authority.write_target(
            args.get("scope_person").and_then(Value::as_str),
            args.get("scope_device").and_then(Value::as_str),
        ) else {
            return ToolResult::failed(
                "WORKING_MEMORY_REJECTED",
                "memory destination unavailable",
                false,
            );
        };
        match self.store.set_authorized(
            authority,
            &person,
            &device,
            args.get("key_or_topic")
                .and_then(Value::as_str)
                .unwrap_or(""),
            args.get("content").and_then(Value::as_str).unwrap_or(""),
            ctx.trace_id.as_deref().unwrap_or(""),
        ) {
            Ok(item) => {
                if let Some(ref shared) = self.shared_memory {
                    let source_dev = if item.written_by_device.trim().is_empty() {
                        authority.physical_device_id().unwrap_or("console")
                    } else {
                        item.written_by_device.as_str()
                    };
                    let content =
                        format!("[working_memory: {}] {}", item.key_or_topic, item.content);
                    let _ = shared.enqueue_observation_scoped(ObservationInput {
                        source_device: source_dev,
                        scope_device: &item.scope_device,
                        scope_person: &item.scope_person,
                        room: authority.scope().room.as_deref().unwrap_or(""),
                        trace_id: &item.source_trace_id,
                        content: &content,
                        domain: "workmem",
                        record_id: &item.id,
                        event_kind: "explicit_save",
                        dimension: "general",
                        expected_epoch: Some(authority.epoch() as i64),
                    });
                }
                ToolResult::observed_ok(json!({"saved":item}))
            }
            Err(e) => ToolResult::failed("WORKING_MEMORY_REJECTED", e, false),
        }
    }
}

/// Read one working memory item by key or list items filtered by scope.
pub struct WorkingMemoryGetTool {
    store: Arc<WorkingMemoryStore>,
}

impl WorkingMemoryGetTool {
    pub fn new(store: Arc<WorkingMemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for WorkingMemoryGetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: std::borrow::Cow::Borrowed("workmem.get"),
            service: std::borrow::Cow::Borrowed("memory"),
            side_effect: SideEffect::Query,
            description: String::from(
                "读取或列出长期工作记忆（项目路径、上次进度、工作上下文）。\
                 传 key_or_topic 读取指定条目；省略则列出当前工作记忆。",
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key_or_topic": { "type": "string", "description": "可选：要读取的工作主题或键名" },
                    "scope_person": { "type": "string", "description": "可选：按人过滤" },
                    "scope_device": { "type": "string", "description": "可选：按设备过滤" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(authority) = ctx.authority() else {
            return ToolResult::observed_ok(json!({"items":[],"count":0,"item":null}));
        };
        let key = args
            .get("key_or_topic")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match self.store.list_authorized(
            authority,
            args.get("scope_person").and_then(Value::as_str),
            args.get("scope_device").and_then(Value::as_str),
            key,
        ) {
            Ok(items) => {
                let item = if key.is_some() && items.len() == 1 {
                    Some(items[0].clone())
                } else {
                    None
                };
                ToolResult::observed_ok(json!({"items":items,"count":items.len(),"item":item}))
            }
            Err(e) => ToolResult::failed("WORKING_MEMORY_ERROR", e, false),
        }
    }
}

/// Delete a working memory entry by `(scope_person, scope_device, key_or_topic)`.
pub struct WorkingMemoryDeleteTool {
    store: Arc<WorkingMemoryStore>,
}

impl WorkingMemoryDeleteTool {
    pub fn new(store: Arc<WorkingMemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for WorkingMemoryDeleteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: std::borrow::Cow::Borrowed("workmem.delete"),
            service: std::borrow::Cow::Borrowed("memory"),
            side_effect: SideEffect::Mutate,
            description: String::from("删除不再需要的长期工作记忆条目。"),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key_or_topic": { "type": "string", "description": "要删除的工作主题或键名" },
                    "scope_person": { "type": "string", "description": "可选：关联的人名，默认为空" },
                    "scope_device": { "type": "string", "description": "可选：关联的设备 ID，默认为空" }
                },
                "required": ["key_or_topic"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let key = args
            .get("key_or_topic")
            .and_then(Value::as_str)
            .unwrap_or("");
        let denied = || ToolResult::observed_ok(json!({"deleted":false,"key_or_topic":key}));
        let Some(authority) = ctx.authority() else {
            return denied();
        };
        let Some((person, device)) = authority.write_target(
            args.get("scope_person").and_then(Value::as_str),
            args.get("scope_device").and_then(Value::as_str),
        ) else {
            return denied();
        };
        match self
            .store
            .delete_authorized(authority, &person, &device, key)
        {
            Ok(deleted) => ToolResult::observed_ok(json!({"deleted":deleted,"key_or_topic":key})),
            Err(e) => ToolResult::failed("WORKING_MEMORY_ERROR", e, false),
        }
    }
}

/// Propose a provenance-tagged `USER.md` profile entry (`#83`, W4b-2).
/// Never touches `Agent.md`; every entry records `source_device`, `trace_id`,
/// `turn_ts`, and `rationale` so Console users can audit or revert it.
pub struct UserProfileProposeTool {
    user_md: Arc<UserMdService>,
    shared_memory: Option<Arc<SharedMemoryService>>,
}

impl UserProfileProposeTool {
    pub fn new(user_md: Arc<UserMdService>) -> Self {
        Self {
            user_md,
            shared_memory: None,
        }
    }

    pub fn with_shared_memory(mut self, shared_memory: Arc<SharedMemoryService>) -> Self {
        self.shared_memory = Some(shared_memory);
        self
    }
}

#[async_trait::async_trait]
impl Tool for UserProfileProposeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: std::borrow::Cow::Borrowed("user_profile.propose"),
            service: std::borrow::Cow::Borrowed("memory"),
            side_effect: SideEffect::Mutate,
            description: String::from(
                "记录带来源审计的用户画像或长期偏好条目到 USER.md（如称呼、作息、口味偏好）。\
                 必须提供 rationale 说明依据；禁止用于修改 Agent.md 或系统规则。",
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": format!("偏好或画像键名，最多 {MAX_USER_MD_KEY_CHARS} 字，如 coffee_preference")
                    },
                    "content": {
                        "type": "string",
                        "description": format!("条目内容，最多 {MAX_USER_MD_CONTENT_CHARS} 字")
                    },
                    "rationale": {
                        "type": "string",
                        "description": format!("记录依据/原因，最多 {MAX_USER_MD_RATIONALE_CHARS} 字")
                    },
                    "person_id": {
                        "type": "string",
                        "description": "可选：关联的家庭成员标识，省略使用本轮授权范围；全屋共享需要显式授权"
                    }
                },
                "required": ["key", "content", "rationale"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(authority) = ctx.authority() else {
            return ToolResult::failed(
                "USER_MD_REJECTED",
                "profile destination unavailable",
                false,
            );
        };
        match self.user_md.propose_authorized(
            authority,
            args.get("person_id").and_then(Value::as_str),
            args.get("key").and_then(Value::as_str).unwrap_or(""),
            args.get("content").and_then(Value::as_str).unwrap_or(""),
            ctx.trace_id.as_deref().unwrap_or(""),
            args.get("rationale").and_then(Value::as_str).unwrap_or(""),
        ) {
            Ok(entry) => {
                if let Some(ref shared) = self.shared_memory {
                    let content = format!(
                        "[user_profile: {}] {} ({})",
                        entry.key, entry.content, entry.rationale
                    );
                    let _ = shared.enqueue_observation_scoped(ObservationInput {
                        source_device: &entry.source_device,
                        scope_device: &entry.scope_device,
                        scope_person: &entry.person_id,
                        room: authority.scope().room.as_deref().unwrap_or(""),
                        trace_id: &entry.trace_id,
                        content: &content,
                        domain: "user_md",
                        record_id: &entry.id,
                        event_kind: "explicit_save",
                        dimension: "preferences",
                        expected_epoch: Some(authority.epoch() as i64),
                    });
                }
                ToolResult::observed_ok(json!({"entry":entry}))
            }
            Err(e) => ToolResult::failed("USER_MD_REJECTED", e, false),
        }
    }
}
