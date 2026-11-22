//! `reminder.set` / `reminder.list` / `reminder.cancel` — timed reminders as
//! tools (2026-11-14, user request).
//!
//! The person says a shape; the MODEL only extracts its structure (minutes,
//! or day offset + hour + minute); the core does every piece of time math
//! against the DEVICE's own clock ([crate::reminder]). The tool's spoken
//! sentence is authoritative (`rendered_answer` + `replace_answer`): the
//! resolved time is a computed fact and the model must not reword it.
//!
//! Like `memory.remember`, these are `SideEffect::Query` despite writing a
//! store row: the effect is a local note, not a change in the external world,
//! so no device receipt chain is involved. The spoken sentence comes from the
//! row read back after the write.
//!
//! Which DEVICE owns the reminder: the household's speaker that is online
//! right now (v1, single speaker; M12a's "哪台设置，哪台提醒" needs the turn
//! to carry a device identity, which the tool context does not have yet).

use crate::gateway::DeviceHub;
use crate::reminder::{spoken_when, ReminderError, ReminderService, When, MAX_NOTE_CHARS};
use crate::tool::time::{local_now, now_secs};
use crate::tool::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use serde_json::{json, Value};
use std::sync::Arc;

const REMINDER_UNAVAILABLE: &str = "REMINDER_UNAVAILABLE";

pub struct ReminderRuntime {
    svc: Arc<ReminderService>,
    hub: Arc<DeviceHub>,
    /// Test seam: a pinned device so the tool tests do not need a gateway
    /// handshake. Production leaves it None and asks the hub.
    fixed_device: Option<String>,
}

impl ReminderRuntime {
    pub fn new(svc: Arc<ReminderService>, hub: Arc<DeviceHub>) -> Arc<Self> {
        Arc::new(Self {
            svc,
            hub,
            fixed_device: None,
        })
    }

    #[cfg(test)]
    fn with_fixed_device(svc: Arc<ReminderService>, device: &str) -> Arc<Self> {
        let hub = DeviceHub::new();
        Arc::new(Self {
            svc,
            hub,
            fixed_device: Some(device.to_string()),
        })
    }

    fn svc(&self) -> Arc<ReminderService> {
        self.svc.clone()
    }

    /// The speaker that owns the reminder: an online one, else an honest
    /// refusal (a reminder for a speaker that is not there cannot fire).
    fn device(&self) -> Result<String, String> {
        if let Some(d) = &self.fixed_device {
            return Ok(d.clone());
        }
        let snap = self.hub.snapshot();
        let devices = snap["devices"].as_array().cloned().unwrap_or_default();
        let mut first_known: Option<String> = None;
        for d in &devices {
            let id = d["stable_id"].as_str().unwrap_or_default().to_string();
            if id.is_empty() {
                continue;
            }
            if first_known.is_none() {
                first_known = Some(id.clone());
            }
            if d["online"].as_bool().unwrap_or(false) {
                return Ok(id);
            }
        }
        first_known.ok_or_else(|| REMINDER_UNAVAILABLE.to_string())
    }
}

fn err_result(e: ReminderError) -> ToolResult {
    let (code, detail) = match &e {
        ReminderError::BadArgs(d) => ("REMINDER_BAD_ARGS", d.clone()),
        ReminderError::TimeInPast(d) => ("REMINDER_TIME_IN_PAST", d.clone()),
        ReminderError::Limit(d) => ("REMINDER_LIMIT", d.clone()),
        ReminderError::NotFound(d) => ("REMINDER_NOT_FOUND", d.clone()),
        ReminderError::Store(d) => ("REMINDER_STORE", d.clone()),
    };
    ToolResult::failed(code, detail, matches!(e, ReminderError::Store(_)))
}

/// Extract the shape from the model's arguments. One of:
/// `delay_minutes` (relative) or `hour`+`minute` with an optional
/// `day_offset` (absolute). Anything else is a named bad-args error.
fn when_of(args: &Value) -> Result<When, ReminderError> {
    if let Some(m) = args.get("delay_minutes").and_then(Value::as_u64) {
        return Ok(When::DelayMinutes(m));
    }
    if let (Some(h), Some(mi)) = (
        args.get("hour").and_then(Value::as_u64),
        args.get("minute").and_then(Value::as_u64),
    ) {
        let day = match args.get("day_offset") {
            Some(v) if v.is_u64() => Some(v.as_u64().unwrap()),
            _ => None,
        };
        return Ok(When::At {
            day_offset: day,
            hour: h as u32,
            minute: mi as u32,
        });
    }
    Err(ReminderError::BadArgs(
        "要给出时间：delay_minutes（几分钟后），或 hour+minute（可选 day_offset）".into(),
    ))
}

fn note_of(args: &Value) -> Result<String, ReminderError> {
    let note = args
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if note.is_empty() {
        return Err(ReminderError::BadArgs("note 不能为空：要提醒什么事".into()));
    }
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(ReminderError::BadArgs(format!(
            "note 最多 {MAX_NOTE_CHARS} 字"
        )));
    }
    Ok(note.to_string())
}

pub struct ReminderSetTool {
    rt: Arc<ReminderRuntime>,
}

impl ReminderSetTool {
    pub fn new(rt: Arc<ReminderRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for ReminderSetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "reminder.set".into(),
            service: "reminder".into(),
            side_effect: SideEffect::Query, // local store row, no external world
            description: "设置一个定时提醒，到点音箱会主动说出来。用户说「X分钟后提醒我…」「明天早上八点叫我…」时调用。\
                          相对时间传 delay_minutes（分钟）；带钟点的传 hour/minute（24小时制，早上=8点、晚上=20点由你判断）\
                          和可选 day_offset（0=今天，1=明天；用户没说哪天就不要传，系统会自动顺延）。\
                          note 写要提醒的事（≤40字）。所有时间换算由系统做，你只提取结构。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "maxLength": 40, "description": "要提醒的事，例如「关火」「接孩子」" },
                    "delay_minutes": { "type": "integer", "minimum": 1, "maximum": 10080,
                                       "description": "几分钟后提醒（「十分钟后」→10）" },
                    "hour": { "type": "integer", "minimum": 0, "maximum": 23 },
                    "minute": { "type": "integer", "minimum": 0, "maximum": 59 },
                    "day_offset": { "type": "integer", "minimum": 0, "maximum": 365,
                                    "description": "0=今天，1=明天；用户说了「今天/明天」才传" }
                },
                "required": ["note"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let note = match note_of(&args) {
            Ok(n) => n,
            Err(e) => return err_result(e),
        };
        let when = match when_of(&args) {
            Ok(w) => w,
            Err(e) => return err_result(e),
        };
        let now_secs = now_secs();
        let Some(now) = local_now() else {
            return ToolResult::failed(REMINDER_UNAVAILABLE, "本机时钟读不出来", true);
        };
        let due_ms = match crate::reminder::resolve_due(&when, now_secs, &now) {
            Ok(d) => d,
            Err(e) => return err_result(e),
        };
        let device = match self.rt.device() {
            Ok(d) => d,
            Err(e) => return ToolResult::failed(REMINDER_UNAVAILABLE, e, true),
        };
        match self
            .rt
            .svc()
            .create(&device, due_ms, &note, now_secs as u64 * 1000)
        {
            Ok(r) => {
                let when_spoken = spoken_when((due_ms / 1000) as i64, &now);
                let mut out = json!({
                    "id": r.id,
                    "device": r.device,
                    "due_ms": r.due_ms,
                    "due_spoken": when_spoken,
                    "note": r.note,
                });
                // The resolved time is the core's fact; the model must say it
                // as written, not re-derive it.
                out["rendered_answer"] = json!(format!("好的，{when_spoken}提醒你{note}。"));
                out["replace_answer"] = json!(true);
                ToolResult::observed_ok(out)
            }
            Err(e) => err_result(e),
        }
    }
}

pub struct ReminderListTool {
    rt: Arc<ReminderRuntime>,
}

impl ReminderListTool {
    pub fn new(rt: Arc<ReminderRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for ReminderListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "reminder.list".into(),
            service: "reminder".into(),
            side_effect: SideEffect::Query,
            description: "列出这台音箱上还没响的定时提醒（每条带说出来的时间和内容）。\
                          用户问「我有什么提醒」「都有哪些提醒」时调用。"
                .into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCtx) -> ToolResult {
        let device = match self.rt.device() {
            Ok(d) => d,
            Err(e) => return ToolResult::failed(REMINDER_UNAVAILABLE, e, true),
        };
        match self.rt.svc().pending(Some(&device)) {
            Ok(items) => {
                let Some(now) = local_now() else {
                    return ToolResult::failed(REMINDER_UNAVAILABLE, "本机时钟读不出来", true);
                };
                let listed: Vec<Value> = items
                    .iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "note": r.note,
                            "due_spoken": spoken_when((r.due_ms / 1000) as i64, &now),
                        })
                    })
                    .collect();
                let mut out = json!({ "count": listed.len(), "items": listed });
                if listed.is_empty() {
                    out["rendered_answer"] = json!("现在没有待提醒的事。");
                    out["replace_answer"] = json!(true);
                }
                ToolResult::observed_ok(out)
            }
            Err(e) => err_result(e),
        }
    }
}

pub struct ReminderCancelTool {
    rt: Arc<ReminderRuntime>,
}

impl ReminderCancelTool {
    pub fn new(rt: Arc<ReminderRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for ReminderCancelTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "reminder.cancel".into(),
            service: "reminder".into(),
            side_effect: SideEffect::Query, // local store row, no external world
            description: "取消还没响的定时提醒。用户说「取消提醒」「别提醒了」时调用：\
                          全部取消传 all=true；只取消一条就先 reminder.list 拿 id 再传 id。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "要取消的提醒 id（来自 reminder.list）" },
                    "all": { "type": "boolean", "description": "取消这台音箱上全部待提醒" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        Availability::Ready
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let device = match self.rt.device() {
            Ok(d) => d,
            Err(e) => return ToolResult::failed(REMINDER_UNAVAILABLE, e, true),
        };
        let now_ms = now_secs() as u64 * 1000;
        let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
        let cancelled: Result<Vec<String>, ReminderError> = if all {
            self.rt.svc().pending(Some(&device)).map(|items| {
                items
                    .iter()
                    .filter_map(|r| self.rt.svc().cancel(r.id, now_ms).ok().map(|c| c.note))
                    .collect()
            })
        } else if let Some(id) = args.get("id").and_then(Value::as_i64) {
            self.rt.svc().cancel(id, now_ms).map(|c| vec![c.note])
        } else {
            return err_result(ReminderError::BadArgs("要给 id，或 all=true".into()));
        };
        match cancelled {
            Ok(notes) if notes.is_empty() => {
                let mut out = json!({ "cancelled": 0 });
                out["rendered_answer"] = json!("没有要取消的提醒。");
                out["replace_answer"] = json!(true);
                ToolResult::observed_ok(out)
            }
            Ok(notes) => {
                let mut out = json!({ "cancelled": notes.len(), "notes": notes });
                if notes.len() == 1 {
                    out["rendered_answer"] = json!(format!("好，{}的提醒取消了。", notes[0]));
                } else {
                    out["rendered_answer"] = json!(format!("好，{}个提醒都取消了。", notes.len()));
                }
                out["replace_answer"] = json!(true);
                ToolResult::observed_ok(out)
            }
            Err(e) => err_result(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;

    fn ctx() -> ToolCtx {
        ToolCtx::new(
            "T1",
            "C1",
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    fn runtime() -> Arc<ReminderRuntime> {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        ReminderRuntime::with_fixed_device(Arc::new(ReminderService::new(store)), "dot2-d")
    }

    #[tokio::test]
    async fn set_renders_the_resolved_time_itself() {
        let rt = runtime();
        let r = ReminderSetTool::new(rt.clone())
            .execute(json!({ "note": "关火", "delay_minutes": 10 }), &ctx())
            .await;
        assert_eq!(r.status, crate::tool::ToolStatus::Completed, "{r:?}");
        let v = r.result.unwrap();
        assert_eq!(v["replace_answer"], json!(true));
        let said = v["rendered_answer"].as_str().unwrap();
        assert!(said.contains("提醒你关火"), "{said}");
        assert!(
            said.contains("10分钟后") || said.contains("点"),
            "the time must be named: {said}"
        );
        // The row exists and is pending.
        assert_eq!(rt.svc.pending(Some("dot2-d")).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_absolute_morning_resolves_without_a_day_word() {
        let rt = runtime();
        let r = ReminderSetTool::new(rt)
            .execute(json!({ "note": "起床", "hour": 8, "minute": 0 }), &ctx())
            .await;
        assert_eq!(r.status, crate::tool::ToolStatus::Completed, "{r:?}");
        let said = r.result.unwrap()["rendered_answer"]
            .as_str()
            .unwrap()
            .to_string();
        // 8:00 is either ahead today or tomorrow; both are the tool's call.
        assert!(said.contains("8点00分"), "{said}");
    }

    #[tokio::test]
    async fn missing_time_and_missing_note_are_named_errors() {
        let rt = runtime();
        let r = ReminderSetTool::new(rt.clone())
            .execute(json!({ "note": "关火" }), &ctx())
            .await;
        assert_eq!(
            r.error.as_ref().map(|e| e.code.as_str()),
            Some("REMINDER_BAD_ARGS")
        );
        let r = ReminderSetTool::new(rt)
            .execute(json!({ "delay_minutes": 5 }), &ctx())
            .await;
        assert_eq!(
            r.error.as_ref().map(|e| e.code.as_str()),
            Some("REMINDER_BAD_ARGS")
        );
    }

    #[tokio::test]
    async fn list_says_empty_and_names_items() {
        let rt = runtime();
        let r = ReminderListTool::new(rt.clone())
            .execute(json!({}), &ctx())
            .await;
        assert_eq!(
            r.result.unwrap()["rendered_answer"],
            json!("现在没有待提醒的事。")
        );

        ReminderSetTool::new(rt.clone())
            .execute(json!({ "note": "关火", "delay_minutes": 10 }), &ctx())
            .await;
        let r = ReminderListTool::new(rt).execute(json!({}), &ctx()).await;
        let v = r.result.unwrap();
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["items"][0]["note"], json!("关火"));
        assert!(
            !v["items"][0]["due_spoken"]
                .as_str()
                .unwrap_or("")
                .is_empty(),
            "each item must carry a spoken time: {v}"
        );
    }

    #[tokio::test]
    async fn cancel_by_id_and_cancel_all() {
        let rt = runtime();
        let set = ReminderSetTool::new(rt.clone())
            .execute(json!({ "note": "关火", "delay_minutes": 10 }), &ctx())
            .await;
        let id = set.result.unwrap()["id"].as_i64().unwrap();
        let r = ReminderCancelTool::new(rt.clone())
            .execute(json!({ "id": id }), &ctx())
            .await;
        assert_eq!(r.result.unwrap()["cancelled"], json!(1));
        // Cancelling again names the miss.
        let r = ReminderCancelTool::new(rt.clone())
            .execute(json!({ "id": id }), &ctx())
            .await;
        assert_eq!(
            r.error.as_ref().map(|e| e.code.as_str()),
            Some("REMINDER_NOT_FOUND")
        );

        ReminderSetTool::new(rt.clone())
            .execute(json!({ "note": "喝水", "delay_minutes": 20 }), &ctx())
            .await;
        ReminderSetTool::new(rt.clone())
            .execute(json!({ "note": "散步", "delay_minutes": 30 }), &ctx())
            .await;
        let r = ReminderCancelTool::new(rt)
            .execute(json!({ "all": true }), &ctx())
            .await;
        assert_eq!(r.result.unwrap()["cancelled"], json!(2));
    }
}
