//! Task lifecycle: identity, states, cancellation, late results.
//!
//! One owner for "can this task still act": the TaskRegistry. The agent
//! loop and tool layer ask it; nobody else flips status flags.

use crate::id;
use crate::store::{Result, Store};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    AwaitingConfirmation,
    Completed,
    Cancelled,
    Failed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::AwaitingConfirmation => "awaiting_confirmation",
            TaskStatus::Completed => "completed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Failed => "failed",
        }
    }
}

/// A parent request can revoke work before a task id has been announced.
/// The commit gate also orders session writes before close clears history.
#[derive(Default)]
pub struct CancellationScope {
    flag: Arc<AtomicBool>,
    commit: Mutex<()>,
    notify: tokio::sync::Notify,
}
impl CancellationScope {
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
    pub fn cancel(&self) {
        let _commit = crate::lock_or_recover(&self.commit, "task.cancel_commit");
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub async fn cancelled(&self) {
        if self.flag.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.notify.notified();
        if self.flag.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
    pub(crate) fn inspect<T>(&self, f: impl FnOnce(bool) -> T) -> T {
        let _commit = crate::lock_or_recover(&self.commit, "task.cancel_commit");
        f(!self.flag.load(Ordering::SeqCst))
    }
    pub fn commit<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let _commit = crate::lock_or_recover(&self.commit, "task.cancel_commit");
        (!self.flag.load(Ordering::SeqCst)).then(f)
    }
}

pub struct TaskState {
    pub id: String,
    pub session_id: String,
    pub created_ms: u64,
    status: Mutex<TaskStatus>,
    cancel: Arc<CancellationScope>,
    /// Free-form notes: late results, skips, cancellation stage, etc.
    notes: Mutex<Vec<String>>,
}

impl TaskState {
    pub fn status(&self) -> TaskStatus {
        *crate::lock_or_recover(&self.status, "task.status")
    }
    pub(crate) fn cancellation_scope(&self) -> Arc<CancellationScope> {
        self.cancel.clone()
    }
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.flag()
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.flag.load(Ordering::SeqCst)
    }
    pub fn commit<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        self.cancel.commit(f)
    }
    fn set_status(&self, s: TaskStatus) {
        *crate::lock_or_recover(&self.status, "task.status") = s;
    }
    pub fn note(&self, s: impl Into<String>) {
        crate::lock_or_recover(&self.notes, "task.notes").push(s.into());
    }
    pub fn notes(&self) -> Vec<String> {
        crate::lock_or_recover(&self.notes, "task.notes").clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossTurnState {
    Active,
    AwaitingContinue,
    Completed,
    Cancelled,
}

impl CrossTurnState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CrossTurnState::Active => "active",
            CrossTurnState::AwaitingContinue => "awaiting_continue",
            CrossTurnState::Completed => "completed",
            CrossTurnState::Cancelled => "cancelled",
        }
    }
}

/// Per-session/per-device cross-turn task state (M6).
/// Lives beside the single-turn agent loop so multi-step requests can pause
/// after step 1 (`AwaitingContinue`) and resume on 「继续」 or abort on 「取消」.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrossTurnTask {
    pub session_id: String,
    pub goal: String,
    pub state: CrossTurnState,
    pub next_socket: Option<String>,
    pub last_summary: Option<String>,
    pub cancelled: bool,
    pub updated_ms: u64,
}

pub struct TaskRegistry {
    store: Arc<Store>,
    tasks: Mutex<HashMap<String, Arc<TaskState>>>,
    cross_turn: Mutex<HashMap<String, CrossTurnTask>>,
}

impl TaskRegistry {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            tasks: Mutex::new(HashMap::new()),
            cross_turn: Mutex::new(HashMap::new()),
        }
    }

    pub fn create(&self, session_id: &str) -> Result<Arc<TaskState>> {
        self.create_with_scope(session_id, Arc::new(CancellationScope::default()))
    }

    pub fn create_with_scope(
        &self,
        session_id: &str,
        cancel: Arc<CancellationScope>,
    ) -> Result<Arc<TaskState>> {
        let t = Arc::new(TaskState {
            id: id::next("T"),
            session_id: session_id.to_string(),
            created_ms: id::now_ms(),
            status: Mutex::new(TaskStatus::Running),
            cancel,
            notes: Mutex::new(Vec::new()),
        });
        self.persist(&t)?;
        crate::lock_or_recover(&self.tasks, "task.registry").insert(t.id.clone(), t.clone());
        Ok(t)
    }

    pub fn get(&self, task_id: &str) -> Option<Arc<TaskState>> {
        crate::lock_or_recover(&self.tasks, "task.registry")
            .get(task_id)
            .cloned()
    }

    /// Resolve the `session_id` of `task_id` from in-memory active tasks or
    /// persisted SQLite `tasks` (used for fail-closed loopback isolation on
    /// `POST /api/v0/session/cancel`).
    pub fn session_id_for(&self, task_id: &str) -> Option<String> {
        let tid = task_id.trim();
        if tid.is_empty() {
            return None;
        }
        if let Some(t) = crate::lock_or_recover(&self.tasks, "task.registry").get(tid) {
            return Some(t.session_id.clone());
        }
        use rusqlite::OptionalExtension;
        self.store
            .with(|c| {
                c.query_row(
                    "SELECT session_id FROM tasks WHERE id = ?1 LIMIT 1",
                    [tid],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .ok()
            .flatten()
    }

    /// True if `task_id` matches an in-memory or persisted task in SQLite `tasks`.
    pub fn exists(&self, task_id: &str) -> bool {
        let tid = task_id.trim();
        if tid.is_empty() {
            return false;
        }
        if crate::lock_or_recover(&self.tasks, "task.registry").contains_key(tid) {
            return true;
        }
        self.store
            .with(|c| {
                let exists: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                    [tid],
                    |r| r.get(0),
                )?;
                Ok(exists)
            })
            .unwrap_or(false)
    }

    /// Request cancellation: the honest boundary — running tools check the
    /// flag before side effects; already-submitted actions are NOT undone.
    pub fn request_cancel(&self, task_id: &str) -> Option<bool> {
        let t = self.get(task_id)?;
        if !matches!(
            t.status(),
            TaskStatus::Running | TaskStatus::AwaitingConfirmation
        ) {
            return Some(false);
        }
        t.cancel.cancel();
        t.note("cancel requested");
        Some(true)
    }

    pub fn finish(&self, task: &TaskState, status: TaskStatus) -> Result<()> {
        task.set_status(status);
        self.persist(task)?;
        self.trim_finished();
        Ok(())
    }
    fn trim_finished(&self) {
        // Keep finished tasks queryable for a while; trim old ones.
        let mut g = crate::lock_or_recover(&self.tasks, "task.registry");
        if g.len() > 256 {
            let finished: Vec<String> = g
                .iter()
                .filter(|(_, t)| {
                    !matches!(
                        t.status(),
                        TaskStatus::Running | TaskStatus::AwaitingConfirmation
                    )
                })
                .map(|(k, _)| k.clone())
                .take(g.len() - 200)
                .collect();
            for k in finished {
                g.remove(&k);
            }
        }
    }

    /// Confirmation creation and its pending task state share one transaction.
    /// The caller already holds this task's CancellationScope admission gate.
    pub(crate) fn persist_confirmation_pending_in(
        &self,
        c: &rusqlite::Connection,
        task: &TaskState,
    ) -> Result<()> {
        let changed=c.execute("UPDATE tasks SET status='awaiting_confirmation',updated_ms=?1,detail=?2 WHERE id=?3 AND status='running'",rusqlite::params![id::now_ms() as i64,json!({"notes":task.notes()}).to_string(),task.id])?;
        if changed != 1 {
            return Err(crate::store::StoreError::Authority(
                "CONFIRMATION_TASK_CHANGED".into(),
            ));
        }
        Ok(())
    }
    /// Infallible publication only after the journal/task transaction commits.
    pub(crate) fn publish_confirmation_pending(&self, task: &TaskState) {
        task.set_status(TaskStatus::AwaitingConfirmation);
    }

    pub(crate) fn persist_confirmation_status_in(
        &self,
        c: &rusqlite::Connection,
        task: &TaskState,
        status: TaskStatus,
    ) -> Result<()> {
        let changed = c.execute(
            "UPDATE tasks SET status=?1,updated_ms=?2,detail=?3 WHERE id=?4",
            rusqlite::params![
                status.as_str(),
                id::now_ms() as i64,
                json!({"notes":task.notes()}).to_string(),
                task.id
            ],
        )?;
        if changed != 1 {
            return Err(crate::store::StoreError::Authority(
                "CONFIRMATION_TASK_CHANGED".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn publish_confirmation_status(&self, task: &TaskState, status: TaskStatus) {
        task.set_status(status);
        self.trim_finished();
    }
    pub(crate) fn finish_at_epoch(
        &self,
        task: &TaskState,
        status: TaskStatus,
        epoch: u64,
    ) -> Result<()> {
        self.store.with(|c| {
            let current: i64 =
                c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
                    r.get(0)
                })?;
            if current as u64 != epoch {
                task.set_status(TaskStatus::Cancelled);
                return Ok(());
            }
            c.execute(
                "UPDATE tasks SET status=?1,updated_ms=?2,detail=?4 WHERE id=?3",
                rusqlite::params![
                    status.as_str(),
                    id::now_ms() as i64,
                    task.id,
                    json!({"notes":task.notes()}).to_string()
                ],
            )?;
            task.set_status(status);
            Ok(())
        })?;
        self.trim_finished();
        Ok(())
    }
    fn persist(&self, t: &TaskState) -> Result<()> {
        self.store.with(|c| {
            c.execute(
                "INSERT INTO tasks (id, session_id, status, created_ms, updated_ms, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET status=?3, updated_ms=?5, detail=?6",
                rusqlite::params![
                    t.id,
                    t.session_id,
                    t.status().as_str(),
                    t.created_ms as i64,
                    id::now_ms() as i64,
                    serde_json::to_string(&json!({ "notes": t.notes() })).unwrap_or_default()
                ],
            )?;
            Ok(())
        })
    }

    /// All tasks (recent first), including persisted ones from restarts.
    pub fn list(&self, limit: u32) -> Result<Vec<serde_json::Value>> {
        self.store.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_id, status, created_ms, updated_ms, detail
                 FROM tasks ORDER BY updated_ms DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map([limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "session_id": r.get::<_, String>(1)?,
                        "status": r.get::<_, String>(2)?,
                        "created_ms": r.get::<_, i64>(3)?,
                        "updated_ms": r.get::<_, i64>(4)?,
                        "detail": serde_json::from_str::<serde_json::Value>(
                            &r.get::<_, String>(5)?).unwrap_or(json!({})),
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn set_cross_turn(&self, task: CrossTurnTask) {
        crate::lock_or_recover(&self.cross_turn, "task.cross_turn")
            .insert(task.session_id.clone(), task);
    }

    pub fn begin_cross_turn(
        &self,
        session_id: &str,
        goal: impl Into<String>,
        next_socket: Option<String>,
        last_summary: Option<String>,
        state: CrossTurnState,
    ) -> CrossTurnTask {
        let task = CrossTurnTask {
            session_id: session_id.to_string(),
            goal: goal.into(),
            state,
            next_socket,
            last_summary,
            cancelled: state == CrossTurnState::Cancelled,
            updated_ms: id::now_ms(),
        };
        self.set_cross_turn(task.clone());
        task
    }

    pub fn get_cross_turn(&self, session_id: &str) -> Option<CrossTurnTask> {
        crate::lock_or_recover(&self.cross_turn, "task.cross_turn")
            .get(session_id)
            .cloned()
    }

    pub fn has_pending_cross_turn(&self, session_id: &str) -> bool {
        crate::lock_or_recover(&self.cross_turn, "task.cross_turn")
            .get(session_id)
            .is_some_and(|t| {
                !t.cancelled
                    && matches!(
                        t.state,
                        CrossTurnState::AwaitingContinue | CrossTurnState::Active
                    )
            })
    }

    pub fn complete_cross_turn(
        &self,
        session_id: &str,
        summary: Option<String>,
    ) -> Option<CrossTurnTask> {
        let mut g = crate::lock_or_recover(&self.cross_turn, "task.cross_turn");
        let t = g.get_mut(session_id)?;
        t.state = CrossTurnState::Completed;
        t.next_socket = None;
        if summary.is_some() {
            t.last_summary = summary;
        }
        t.updated_ms = id::now_ms();
        Some(t.clone())
    }

    pub fn cancel_cross_turn(&self, session_id: &str) -> Option<CrossTurnTask> {
        let pending: Vec<String> = crate::lock_or_recover(&self.tasks, "task.registry")
            .values()
            .filter(|task| {
                task.session_id == session_id && task.status() == TaskStatus::AwaitingConfirmation
            })
            .map(|task| task.id.clone())
            .collect();
        for task in pending {
            self.request_cancel(&task);
        }
        let mut g = crate::lock_or_recover(&self.cross_turn, "task.cross_turn");
        let t = g.get_mut(session_id)?;
        if t.cancelled || t.state == CrossTurnState::Completed {
            return None;
        }
        t.state = CrossTurnState::Cancelled;
        t.cancelled = true;
        t.next_socket = None;
        t.updated_ms = id::now_ms();
        Some(t.clone())
    }

    /// Cancel all in-flight tasks and clear completed task & cross-turn state
    /// (used on factory reset).
    pub fn reset(&self) {
        {
            let mut g = crate::lock_or_recover(&self.tasks, "task.registry");
            g.retain(|_, task| {
                if matches!(
                    task.status(),
                    TaskStatus::Running | TaskStatus::AwaitingConfirmation
                ) {
                    task.cancel.cancel();
                    task.set_status(TaskStatus::Cancelled);
                    true
                } else {
                    false
                }
            });
        }
        crate::lock_or_recover(&self.cross_turn, "task.cross_turn").clear();
    }

    /// Render the active/awaiting cross-turn task for system prompt injection.
    /// Returns `None` when no pending cross-turn task exists, keeping short
    /// conversations byte-for-byte identical.
    pub fn render_cross_turn_for_prompt(&self, session_id: &str) -> Option<String> {
        let g = crate::lock_or_recover(&self.cross_turn, "task.cross_turn");
        let t = g.get(session_id)?;
        if t.cancelled
            || !matches!(
                t.state,
                CrossTurnState::AwaitingContinue | CrossTurnState::Active
            )
        {
            return None;
        }
        let next = t.next_socket.as_deref().unwrap_or("完成剩余步骤");
        let summary = t.last_summary.as_deref().unwrap_or("（无）");
        Some(format!(
            "当前跨回合任务（未完成，等待继续或取消）：\n\
             - 原始目标：{goal}\n\
             - 状态：{state}\n\
             - 下一步插座：{next}\n\
             - 上一步摘要：{summary}\n\
             说明：若用户说「继续」/「接着」/「continue」，请直接执行上述「下一步插座」，不要重复已完成的上一步，也不要要求用户重述原话。",
            goal = t.goal,
            state = t.state.as_str(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> TaskRegistry {
        TaskRegistry::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn cancel_is_visible_and_idempotent_per_status() {
        let r = reg();
        let t = r.create("S1").unwrap();
        assert!(r.request_cancel(&t.id).unwrap());
        assert!(t.is_cancelled());
        r.finish(&t, TaskStatus::Cancelled).unwrap();
        assert_eq!(
            r.request_cancel(&t.id),
            Some(false),
            "finished task cannot re-cancel"
        );
        assert_eq!(t.status(), TaskStatus::Cancelled);
    }

    #[test]
    fn unknown_task_cancel_is_none() {
        assert!(reg().request_cancel("T-nope").is_none());
    }

    #[test]
    fn tasks_listed_across_restart_sim() {
        let r = reg();
        let t = r.create("S1").unwrap();
        r.finish(&t, TaskStatus::Completed).unwrap();
        assert!(r.list(10).unwrap()[0]["status"].as_str() == Some("completed"));
    }

    #[test]
    fn cross_turn_task_lifecycle_and_prompt_gating() {
        let r = reg();
        assert!(!r.has_pending_cross_turn("S1"));
        assert!(r.render_cross_turn_for_prompt("S1").is_none());

        let ct = r.begin_cross_turn(
            "S1",
            "先查天气再开灯",
            Some("打开客厅灯".into()),
            Some("已查完天气".into()),
            CrossTurnState::AwaitingContinue,
        );
        assert_eq!(ct.state, CrossTurnState::AwaitingContinue);
        assert!(!ct.cancelled);
        assert!(r.has_pending_cross_turn("S1"));
        let block = r.render_cross_turn_for_prompt("S1").expect("prompt block");
        assert!(block.contains("先查天气再开灯"));
        assert!(block.contains("打开客厅灯"));
        assert!(block.contains("已查完天气"));

        let done = r.complete_cross_turn("S1", Some("已开灯".into())).unwrap();
        assert_eq!(done.state, CrossTurnState::Completed);
        assert!(!r.has_pending_cross_turn("S1"));
        assert!(r.render_cross_turn_for_prompt("S1").is_none());
    }

    #[test]
    fn cross_turn_cancel_prevents_continue() {
        let r = reg();
        r.begin_cross_turn(
            "dev:dot2-1",
            "第一步查新闻，第二步播放音乐",
            Some("播放音乐".into()),
            Some("新闻已播报".into()),
            CrossTurnState::AwaitingContinue,
        );
        let cancelled = r.cancel_cross_turn("dev:dot2-1").unwrap();
        assert_eq!(cancelled.state, CrossTurnState::Cancelled);
        assert!(cancelled.cancelled);
        assert!(cancelled.next_socket.is_none());
        assert!(!r.has_pending_cross_turn("dev:dot2-1"));
        assert!(r.render_cross_turn_for_prompt("dev:dot2-1").is_none());
        // Idempotent: cancelling again returns None
        assert!(r.cancel_cross_turn("dev:dot2-1").is_none());
    }
}
