use super::{store as journal, DispatchPermit};
use crate::{
    execution::Authority,
    store::{Store, StoreError},
    task::{TaskRegistry, TaskState, TaskStatus},
    tool::openapi::{OpenApiRuntime, PreparedOperation},
    tool::{Evidence, Registry, ToolCtx, ToolResult, ToolStatus},
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;
const TTL_MS: u64 = 300_000;
const TERMINAL_KEEP: usize = 128;
struct Live {
    authority: Authority,
    task: Arc<TaskState>,
    requester: Option<String>,
    actor: String,
    session_generation: i64,
    catalog_revision: String,
    deadline: Instant,
    prepared: Option<PreparedOperation>,
    completed: Option<ToolResult>,
    exchange_ready: bool,
}
pub struct ConfirmationService {
    store: Arc<Store>,
    tasks: Arc<TaskRegistry>,
    registry: Arc<Registry>,
    runtime: Arc<OpenApiRuntime>,
    live: Mutex<HashMap<String, Live>>,
    ready: bool,
    boot_id: String,
    #[cfg(test)]
    before_admission: Mutex<Option<(Arc<tokio::sync::Notify>, tokio::sync::watch::Receiver<bool>)>>,
    #[cfg(test)]
    before_receipt: Mutex<Option<(Arc<tokio::sync::Notify>, tokio::sync::watch::Receiver<bool>)>>,
}
fn failure(code: &str) -> StoreError {
    StoreError::Authority(code.into())
}
fn state(action: &Value) -> &str {
    action["state"].as_str().unwrap_or("stale")
}
fn transition(action: &mut Value, to: &str) {
    action["state"] = json!(to);
    action["revision"] = json!(action["revision"].as_u64().unwrap_or(0) + 1);
}
fn principal_live(c: &rusqlite::Connection, principal: &str) -> crate::store::Result<()> {
    let raw: Option<String> = c
        .query_row(
            "SELECT value FROM config_kv WHERE key='api_tokens'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let live = raw
        .and_then(|s| serde_json::from_str::<Vec<crate::auth::TokenEntry>>(&s).ok())
        .is_some_and(|entries| entries.iter().any(|entry| entry.id == principal));
    if live {
        Ok(())
    } else {
        Err(failure("CONFIRMATION_AUTHORITY_INVALID"))
    }
}
fn actor_key(authority: &Authority, requester: Option<&str>) -> String {
    requester
        .map(|p| format!("management:{p}"))
        .unwrap_or_else(|| {
            format!(
                "physical:{}",
                authority.physical_device_id().unwrap_or("unavailable")
            )
        })
}
/// A distinct server-owned assistant continuation, never a duplicate tool
/// response or an invented user utterance. Clear/revoke/reset wins over history.
fn append_continuation(
    c: &rusqlite::Connection,
    action: &Value,
    live: &Live,
) -> crate::store::Result<()> {
    if live.authority.check_in(c).is_err()
        || journal::generation(c, &live.task.session_id).ok() != Some(live.session_generation)
    {
        return Ok(());
    }
    let content = format!(
        "[Core confirmation outcome] {}",
        json!({"action_id":action["id"],"state":action["state"],"outcome":action["outcome"]})
    );
    let wire = crate::llm::WireMessage::assistant_text(content.clone());
    c.execute(
        "INSERT INTO messages(session_id,role,content,ts_ms,wire) VALUES(?1,'assistant',?2,?3,?4)",
        params![
            live.task.session_id,
            content,
            crate::id::now_ms() as i64,
            serde_json::to_string(&wire)?
        ],
    )?;
    Ok(())
}
fn terminal_history_in(
    c: &rusqlite::Connection,
    action: &Value,
    live: &Live,
    scope_active: bool,
) -> crate::store::Result<bool> {
    let valid = scope_active
        && live.authority.check_in(c).is_ok()
        && journal::generation(c, &live.task.session_id).ok() == Some(live.session_generation);
    let id = action["id"].as_str().unwrap_or("");
    if !valid {
        c.execute(
            "UPDATE confirmations SET continuation_written=1 WHERE id=?1",
            [id],
        )?;
        return Ok(false);
    }
    if !live.exchange_ready {
        return Ok(true);
    }
    let written: bool = c.query_row(
        "SELECT continuation_written FROM confirmations WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if !written {
        append_continuation(c, action, live)?;
        c.execute(
            "UPDATE confirmations SET continuation_written=1 WHERE id=?1",
            [id],
        )?;
    }
    Ok(false)
}
impl ConfirmationService {
    pub fn new(
        store: Arc<Store>,
        tasks: Arc<TaskRegistry>,
        registry: Arc<Registry>,
        runtime: Arc<OpenApiRuntime>,
    ) -> Arc<Self> {
        let ready=store.with(|c| {
            let tx=c.unchecked_transaction()?;
            tx.execute("UPDATE tasks SET status='cancelled' WHERE status='awaiting_confirmation' AND id IN (SELECT task_id FROM confirmations WHERE state IN ('pending','approved'))",[])?;
            tx.execute("UPDATE confirmations SET state='cancelled',revision=revision+1,private_payload=NULL WHERE state IN ('pending','approved')",[])?;
            let mut query=tx.prepare("SELECT id FROM confirmations WHERE state='consumed'")?;
            let ids=query.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;drop(query);
            for id in ids {if let Some(mut a)=journal::load(&tx,&id)? {if a["outcome"]["status"]=="running" {
                a["outcome"]=json!({"status":"unknown","evidence":"sent","result":null,"error":{"code":"CONFIRMATION_RESTART_UNKNOWN","retryable":false,"detail":"Core restarted after dispatch admission; reconcile externally before a new request"}});
                journal::update(&tx,&a)?;
                tx.execute("UPDATE tasks SET status='failed',detail=?1 WHERE id=?2",params![json!({"notes":["confirmation outcome unknown after restart"]}).to_string(),a["task_id"].as_str().unwrap_or("")])?;
            }}}
            tx.execute("UPDATE confirmations SET private_payload=NULL,continuation_written=1",[])?;
            tx.commit()?;Ok(())
        }).is_ok();
        let service = Arc::new(Self {
            store,
            tasks,
            registry,
            runtime,
            live: Mutex::new(HashMap::new()),
            ready,
            boot_id: crate::config_delivery::uuid().unwrap_or_else(|_| "unavailable".into()),
            #[cfg(test)]
            before_admission: Mutex::new(None),
            #[cfg(test)]
            before_receipt: Mutex::new(None),
        });
        if tokio::runtime::Handle::try_current().is_ok() {
            let weak = Arc::downgrade(&service);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let Some(service) = weak.upgrade() else { break };
                    let _ = service.sweep();
                }
            });
        }
        service
    }
    pub(crate) fn exchange_complete(&self, id: &str) -> crate::store::Result<()> {
        if let Some(live) = crate::lock_or_recover(&self.live, "confirmations.live").get_mut(id) {
            live.exchange_ready = true;
        }
        self.sweep()
    }
    #[cfg(test)]
    pub(crate) async fn pause_before_receipt(&self) {
        let gate =
            crate::lock_or_recover(&self.before_receipt, "confirmation.test.receipt").clone();
        if let Some((arrived, mut release)) = gate {
            arrived.notify_one();
            while !*release.borrow() {
                if release.changed().await.is_err() {
                    break;
                }
            }
        }
    }
    fn visible(
        &self,
        c: &rusqlite::Connection,
        id: &str,
        principal: &str,
    ) -> crate::store::Result<Value> {
        principal_live(c, principal)?;
        let requester: Option<String> = c
            .query_row(
                "SELECT requester FROM confirmations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if !requester.is_some_and(|p| p.is_empty() || p == principal) {
            return Err(failure("CONFIRMATION_NOT_FOUND"));
        }
        journal::load(c, id)?.ok_or_else(|| failure("CONFIRMATION_NOT_FOUND"))
    }
    pub(crate) fn create(
        self: &Arc<Self>,
        ctx: &ToolCtx,
        mut prepared: PreparedOperation,
    ) -> ToolResult {
        let mut live = crate::lock_or_recover(&self.live, "confirmations.live");
        match self.create_inner(ctx, &mut prepared, &live) {
            Ok((id, action, catalog_revision, generation, requester, authority, task)) => {
                let actor = actor_key(&authority, requester.as_deref());
                ctx.register_confirmation(Arc::downgrade(self), id.clone());
                live.insert(
                    id,
                    Live {
                        authority,
                        task,
                        requester,
                        actor,
                        session_generation: generation,
                        catalog_revision,
                        deadline: Instant::now() + Duration::from_millis(TTL_MS),
                        prepared: Some(prepared),
                        completed: None,
                        exchange_ready: !ctx.requires_model_exchange(),
                    },
                );
                ToolResult {
                    status: ToolStatus::AwaitingConfirmation,
                    evidence: Evidence::Accepted,
                    result: Some(json!({"action":action})),
                    error: None,
                    fact: None,
                }
            }
            Err(e) => {
                let detail = e.to_string();
                let code = if detail.contains("PAIRING_REQUIRED") {
                    "PAIRING_REQUIRED"
                } else if detail.contains("CAPACITY") {
                    "CONFIRMATION_CAPACITY"
                } else {
                    "CONFIRMATION_REQUIRED"
                };
                ToolResult::failed(code, detail, false)
            }
        }
    }
    #[allow(clippy::type_complexity)]
    fn create_inner(
        &self,
        ctx: &ToolCtx,
        prepared: &mut PreparedOperation,
        live: &HashMap<String, Live>,
    ) -> crate::store::Result<(
        String,
        Value,
        String,
        i64,
        Option<String>,
        Authority,
        Arc<TaskState>,
    )> {
        if !self.ready {
            return Err(failure("CONFIRMATION_UNAVAILABLE"));
        }
        // Reserve under one live-map lock until durable creation. No await.
        let authority = ctx
            .authority()
            .cloned()
            .ok_or_else(|| failure("PAIRING_REQUIRED"))?;
        let requester = authority
            .confirmation_requester()
            .ok_or_else(|| failure("PAIRING_REQUIRED"))?;
        let task = self
            .tasks
            .get(&ctx.task_id)
            .ok_or_else(|| failure("CONFIRMATION_TASK_REQUIRED"))?;
        let authority = authority.with_cancel_scope(task.cancellation_scope());
        if task.status() != TaskStatus::Running || task.is_cancelled() {
            return Err(failure("CONFIRMATION_CANCELLED"));
        }
        let actor = actor_key(&authority, requester.as_deref());
        if live.len() >= 64
            || live.values().filter(|l| l.actor == actor).count() >= 16
            || live.values().any(|l| l.task.id == ctx.task_id)
        {
            return Err(failure("CONFIRMATION_CAPACITY"));
        }
        let id = format!(
            "A{}",
            crate::config_delivery::uuid().map_err(|_| failure("CONFIRMATION_ID_UNAVAILABLE"))?
        );
        let now = crate::id::now_ms();
        let mut scope = serde_json::to_value(authority.scope())?;
        if let Some(scope) = scope.as_object_mut() {
            scope.retain(|_, value| !value.is_null());
        }
        prepared.preview["scope"] = scope;
        let action = json!({"id":id,"revision":1,"state":"pending","task_id":ctx.task_id,"call_id":ctx.call_id,"session_id":task.session_id,"created_ms":now,"expires_ms":now+TTL_MS,"ttl_ms":TTL_MS,"preview":prepared.preview,"requester":if requester.is_some(){json!({"kind":"management"})}else{json!({"kind":"physical","device_id":authority.physical_device_id()})},"approver_policy":if requester.is_some(){"requester"}else{"paired_management"},"approved_ms":null,"consumed_ms":null,"outcome":null});
        let (catalog_revision,generation)=authority.admit(||self.registry.with_catalog(|catalog| {
            let _config=crate::lock_or_recover(&self.store.openapi_gate,"openapi.confirmation.create");
            if self.store.openapi_revision()?!=prepared.revision {return Err(failure("CONFIRMATION_STALE"));}
            self.store.with(|c| {
                let tx=c.unchecked_transaction()?;
                authority.check_in(&tx)?;
                if let Some(principal)=&requester {principal_live(&tx,principal)?;}
                if !authority.session_is_bound_in(&tx,&task.session_id)? {return Err(failure("CONFIRMATION_AUTHORITY_INVALID"));}
                let generation=journal::generation(&tx,&task.session_id)?;
                let active:i64=tx.query_row("SELECT count(*) FROM confirmations WHERE state IN ('pending','approved') OR (state='consumed' AND json_extract(action_json,'$.outcome.status')='running')",[],|r|r.get(0))?;
                let same_task:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM confirmations WHERE task_id=?1 AND state IN ('pending','approved'))",[&ctx.task_id],|r|r.get(0))?;
                let actor_active:i64=tx.query_row("SELECT count(*) FROM confirmations WHERE actor_key=?1 AND (state IN ('pending','approved') OR (state='consumed' AND json_extract(action_json,'$.outcome.status')='running'))",[&actor],|r|r.get(0))?;
                if active>=64||actor_active>=16||same_task {return Err(failure("CONFIRMATION_CAPACITY"));}
                tx.execute("INSERT INTO confirmations(id,task_id,session_id,requester,state,revision,data_epoch,session_generation,created_ms,expires_ms,action_json,private_payload,boot_id,catalog_revision,operation_revision,request_digest,actor_key) VALUES(?1,?2,?3,?4,'pending',1,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![id,ctx.task_id,task.session_id,requester.as_deref().unwrap_or(""),authority.epoch() as i64,generation,now as i64,(now+TTL_MS) as i64,action.to_string(),json!({"args":prepared.args,"digest":prepared.digest}).to_string(),self.boot_id,catalog.revision,prepared.revision,prepared.digest,actor])?;
                self.tasks.persist_confirmation_pending_in(&tx,&task)?;
                tx.commit()?;Ok((catalog.revision,generation))
            })
        }))?;
        // Journal and durable task status already committed together. No
        // fallible persistence may strand an executable action before Live publication.
        self.tasks.publish_confirmation_pending(&task);
        Ok((
            id,
            action,
            catalog_revision,
            generation,
            requester,
            authority,
            task,
        ))
    }
    fn check_live(&self, c: &rusqlite::Connection, live: &Live) -> crate::store::Result<()> {
        live.authority.check_in(c)?;
        if live.task.is_cancelled() {
            return Err(failure("CONFIRMATION_CANCELLED"));
        }
        if Instant::now() >= live.deadline {
            return Err(failure("CONFIRMATION_EXPIRED"));
        }
        if journal::generation(c, &live.task.session_id)? != live.session_generation
            || !live
                .authority
                .session_is_bound_in(c, &live.task.session_id)?
        {
            return Err(failure("CONFIRMATION_AUTHORITY_INVALID"));
        }
        if let Some(principal) = &live.requester {
            principal_live(c, principal)?;
        }
        Ok(())
    }
    pub fn read(&self, id: &str, principal: &str) -> crate::store::Result<Value> {
        self.sweep()?;
        self.store.with(|c| self.visible(c, id, principal))
    }
    pub fn list(
        &self,
        principal: &str,
        states: &[String],
        limit: usize,
        cursor: Option<&str>,
    ) -> crate::store::Result<Value> {
        self.sweep()?;
        self.store.with(|c| {
            principal_live(c,principal)?;
            let limit=limit.clamp(1,64);
            let mut query=c.prepare("SELECT id FROM confirmations WHERE (requester='' OR requester=?1) AND (?2='[]' OR state IN (SELECT value FROM json_each(?2))) AND (?3='' OR created_ms<(SELECT created_ms FROM confirmations WHERE id=?3 AND (requester='' OR requester=?1)) OR (created_ms=(SELECT created_ms FROM confirmations WHERE id=?3 AND (requester='' OR requester=?1)) AND id<?3)) ORDER BY created_ms DESC,id DESC LIMIT ?4")?;
            let ids=query.query_map(params![principal,serde_json::to_string(states)?,cursor.unwrap_or(""),(limit+1) as i64],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let more=ids.len()>limit;
            let mut actions=Vec::new();
            for id in ids.into_iter().take(limit) {actions.push(self.visible(c,&id,principal)?);}
            let next=if more {actions.last().and_then(|a:&Value|a["id"].as_str()).map(str::to_string)}else{None};
            Ok(json!({"actions":actions,"next_cursor":next}))
        })
    }
    pub fn change(
        &self,
        id: &str,
        principal: &str,
        expected_state: &str,
        expected_revision: u64,
        operation: &str,
    ) -> crate::store::Result<Value> {
        self.sweep()?;
        self.store.with(|c| self.visible(c, id, principal))?;
        let mut lives = crate::lock_or_recover(&self.live, "confirmations.live");
        let live = lives
            .get(id)
            .ok_or_else(|| failure("CONFIRMATION_CONFLICT"))?;
        let authority = live.authority.clone();
        let (action, waiting) = authority.admit(|| {
            self.registry.with_catalog(|catalog| {
                let _config = crate::lock_or_recover(
                    &self.store.openapi_gate,
                    "openapi.confirmation.approve",
                );
                let prepared = live
                    .prepared
                    .as_ref()
                    .ok_or_else(|| failure("CONFIRMATION_CONFLICT"))?;
                if catalog.revision != live.catalog_revision
                    || self.store.openapi_revision()? != prepared.revision
                {
                    return Err(failure("CONFIRMATION_STALE"));
                }
                self.store.with(|c| {
                    let tx = c.unchecked_transaction()?;
                    let mut action = self.visible(&tx, id, principal)?;
                    self.check_live(&tx, live)?;
                    if state(&action) != expected_state || action["revision"] != expected_revision {
                        return Err(failure("CONFIRMATION_CONFLICT"));
                    }
                    match operation {
                        "approve" if state(&action) == "pending" => {
                            transition(&mut action, "approved");
                            action["approved_ms"] = json!(crate::id::now_ms());
                            tx.execute(
                                "UPDATE confirmations SET approver=?1 WHERE id=?2",
                                params![principal, id],
                            )?;
                        }
                        "reject" | "cancel" if matches!(state(&action), "pending" | "approved") => {
                            transition(
                                &mut action,
                                if operation == "reject" {
                                    "rejected"
                                } else {
                                    "cancelled"
                                },
                            )
                        }
                        _ => return Err(failure("CONFIRMATION_CONFLICT")),
                    }
                    journal::update(&tx, &action)?;
                    let waiting = if matches!(state(&action), "rejected" | "cancelled") {
                        self.tasks.persist_confirmation_status_in(
                            &tx,
                            &live.task,
                            TaskStatus::Cancelled,
                        )?;
                        terminal_history_in(&tx, &action, live, true)?
                    } else {
                        false
                    };
                    tx.commit()?;
                    Ok((action, waiting))
                })
            })
        })?;
        if matches!(state(&action), "rejected" | "cancelled") {
            self.tasks
                .publish_confirmation_status(&live.task, TaskStatus::Cancelled);
            if let Some(live) = lives.get_mut(id) {
                live.prepared = None;
            }
            if !waiting {
                lives.remove(id);
            }
        }
        Ok(action)
    }
    pub async fn execute(
        self: &Arc<Self>,
        id: &str,
        principal: &str,
        expected_state: &str,
        expected_revision: u64,
    ) -> crate::store::Result<Value> {
        // Per-instance test barrier pauses the real HTTP admission path before
        // every cancellation/catalog/config/Store gate. Absent in production.
        #[cfg(test)]
        {
            let gate = {
                crate::lock_or_recover(&self.before_admission, "confirmation.test.admission")
                    .clone()
            };
            if let Some((arrived, mut release)) = gate {
                arrived.notify_one();
                while !*release.borrow() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
            }
        }
        self.sweep()?;
        // Visible consumed tombstones are idempotent even after worker removal.
        let previous = self.store.with(|c| self.visible(c, id, principal))?;
        if state(&previous) == "consumed" {
            return Ok(previous);
        }
        let mut lives = crate::lock_or_recover(&self.live, "confirmations.live");
        let repeated = self.store.with(|c| self.visible(c, id, principal))?;
        if state(&repeated) == "consumed" {
            return Ok(repeated);
        }
        let live = lives
            .get_mut(id)
            .ok_or_else(|| failure("CONFIRMATION_CONFLICT"))?;
        let prepared = live
            .prepared
            .as_ref()
            .ok_or_else(|| failure("CONFIRMATION_CONFLICT"))?;
        let action = live.authority.admit(|| {
            self.registry.with_catalog(|catalog| {
                let _config = crate::lock_or_recover(
                    &self.store.openapi_gate,
                    "openapi.confirmation.execute",
                );
                if catalog.revision != live.catalog_revision
                    || self.store.openapi_revision()? != prepared.revision
                {
                    return Err(failure("CONFIRMATION_STALE"));
                }
                self.store.with(|c| {
                    let tx = c.unchecked_transaction()?;
                    let mut action = self.visible(&tx, id, principal)?;
                    self.check_live(&tx, live)?;
                    if state(&action) != "approved"
                        || expected_state != "approved"
                        || action["revision"] != expected_revision
                    {
                        return Err(failure("CONFIRMATION_CONFLICT"));
                    }
                    let approver: Option<String> = tx.query_row(
                        "SELECT approver FROM confirmations WHERE id=?1",
                        [id],
                        |r| r.get(0),
                    )?;
                    if approver.as_deref() != Some(principal) {
                        return Err(failure("CONFIRMATION_AUTHORITY_INVALID"));
                    }
                    principal_live(&tx, principal)?;
                    let raw: String = tx.query_row(
                        "SELECT private_payload FROM confirmations WHERE id=?1",
                        [id],
                        |r| r.get(0),
                    )?;
                    let payload: Value = serde_json::from_str(&raw)?;
                    if payload["digest"] != prepared.digest || payload["args"] != prepared.args {
                        return Err(failure("CONFIRMATION_STALE"));
                    }
                    transition(&mut action, "consumed");
                    action["consumed_ms"] = json!(crate::id::now_ms());
                    action["outcome"] =
                        json!({"status":"running","evidence":"accepted","result":null});
                    journal::update(&tx, &action)?;
                    tx.commit()?;
                    Ok(action)
                })
            })
        })?;
        let prepared = live
            .prepared
            .take()
            .expect("validated unique dispatch ownership");
        let permit = DispatchPermit { _sealed: () };
        let service = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let outcome = service
                .runtime
                .dispatch_confirmation(prepared, permit)
                .await;
            let _ = service.complete(&id, outcome);
        });
        Ok(action)
    }
    fn complete(&self, id: &str, mut outcome: ToolResult) -> crate::store::Result<()> {
        if serde_json::to_vec(&outcome.result)?.len() > 4096 {
            outcome.result = Some(
                json!({"detail":"response omitted from retained confirmation journal","bounded":true}),
            );
        }
        if let Some(live) = crate::lock_or_recover(&self.live, "confirmations.live").get_mut(id) {
            live.completed = Some(outcome);
        }
        self.sweep()
    }
    pub fn sweep(&self) -> crate::store::Result<()> {
        if !self.ready {
            return Err(failure("CONFIRMATION_UNAVAILABLE"));
        }
        let catalog_revision = self.registry.catalog().ok().map(|c| c.revision);
        let execution_revision = self.store.openapi_revision()?;
        let ids: Vec<String> = crate::lock_or_recover(&self.live, "confirmations.live")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.refresh_one(&id, catalog_revision.as_deref(), &execution_revision)?;
        }
        self.prune()
    }
    fn refresh_one(
        &self,
        id: &str,
        catalog_revision: Option<&str>,
        execution_revision: &str,
    ) -> crate::store::Result<()> {
        let mut lives = crate::lock_or_recover(&self.live, "confirmations.live");
        let Some(live) = lives.get_mut(id) else {
            return Ok(());
        };
        let (remove, status, terminal) = live.authority.bookkeep(|scope_active| {
            self.store.with(|c| {
                let tx = c.unchecked_transaction()?;
                let epoch: i64 =
                    tx.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| {
                        r.get(0)
                    })?;
                if epoch as u64 != live.authority.epoch() {
                    return Ok((true, Some(TaskStatus::Cancelled), true));
                }
                let Some(mut action) = journal::load(&tx, id)? else {
                    return Ok((true, Some(TaskStatus::Cancelled), true));
                };
                let mut changed = false;
                if matches!(state(&action), "pending" | "approved") {
                    let revoked_approver = if state(&action) == "approved" {
                        let approver: Option<String> = tx.query_row(
                            "SELECT approver FROM confirmations WHERE id=?1",
                            [id],
                            |r| r.get(0),
                        )?;
                        !approver
                            .as_deref()
                            .is_some_and(|p| principal_live(&tx, p).is_ok())
                    } else {
                        false
                    };
                    let next = if !scope_active || revoked_approver {
                        Some("cancelled")
                    } else if catalog_revision != Some(live.catalog_revision.as_str())
                        || live
                            .prepared
                            .as_ref()
                            .is_some_and(|p| p.revision != execution_revision)
                    {
                        Some("stale")
                    } else {
                        self.check_live(&tx, live).err().map(|e| {
                            if e.to_string().contains("EXPIRED") {
                                "expired"
                            } else {
                                "cancelled"
                            }
                        })
                    };
                    if let Some(next) = next {
                        transition(&mut action, next);
                        changed = true;
                    }
                }
                if state(&action) == "consumed" && action["outcome"]["status"] == "running" {
                    if let Some(outcome) = &live.completed {
                        action["outcome"] = serde_json::to_value(outcome)?;
                        action["revision"] = json!(action["revision"].as_u64().unwrap_or(0) + 1);
                        changed = true;
                    }
                }
                let terminal = !matches!(state(&action), "pending" | "approved")
                    && !(state(&action) == "consumed" && action["outcome"]["status"] == "running");
                if !terminal {
                    return Ok((false, None, false));
                }
                let status = if !scope_active || state(&action) != "consumed" {
                    TaskStatus::Cancelled
                } else if action["outcome"]["status"] == "completed" {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                };
                self.tasks
                    .persist_confirmation_status_in(&tx, &live.task, status)?;
                if changed {
                    journal::update(&tx, &action)?;
                }
                let waiting = terminal_history_in(&tx, &action, live, scope_active)?;
                tx.commit()?;
                Ok((!waiting, Some(status), true))
            })
        })?;
        if let Some(status) = status {
            self.tasks.publish_confirmation_status(&live.task, status);
        }
        if terminal {
            live.prepared = None;
        }
        if remove {
            lives.remove(id);
        }
        Ok(())
    }
    fn prune(&self) -> crate::store::Result<()> {
        self.store.with(|c| {
            c.execute("DELETE FROM confirmations WHERE id IN (SELECT id FROM confirmations WHERE continuation_written=1 AND state NOT IN ('pending','approved') AND NOT (state='consumed' AND json_extract(action_json,'$.outcome.status')='running') ORDER BY created_ms DESC,id DESC LIMIT -1 OFFSET ?1)",[TERMINAL_KEEP as i64])?;
            // Public redacted previews/results are display payload too: retain
            // them only for the original five-minute display window, then
            // keep bounded minimal tombstones. Never restore authority.
            let cutoff=crate::id::now_ms().saturating_sub(TTL_MS) as i64;
            let mut query=c.prepare("SELECT id FROM confirmations WHERE created_ms<=?1 AND state NOT IN ('pending','approved') AND NOT (state='consumed' AND json_extract(action_json,'$.outcome.status')='running')")?;
            let ids=query.query_map([cutoff],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;drop(query);
            for id in ids {
                if let Some(mut action)=journal::load(c,&id)? {
                    let previous=action.clone();
                    action["preview"]["parameters"]=json!({});action["preview"]["scope"]=json!({});
                    action["preview"]["target"]=action["preview"]["service"].clone();
                    if action["outcome"].is_object(){action["outcome"]["result"]=Value::Null;}
                    if action!=previous {action["revision"]=json!(action["revision"].as_u64().unwrap_or(0)+1);journal::update(c,&action)?;}
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
