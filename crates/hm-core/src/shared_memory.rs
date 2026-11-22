//! Offline-resilient shared memory outbox & summary cache (`#80`).
//!
//! Live voice turns ONLY read `shared_profile_cache` from local SQLite (0 ms
//! network blocking). Background sync drains `shared_memory_outbox` and pulls
//! profile summaries from NAS `memory-svc` with a strict `<= 250 ms` HTTP
//! connect/request budget and an exponential-backoff circuit breaker.

use crate::config::{ConfigService, SharedMemoryConfig, SHARED_MEMORY_TOKEN_SECRET};
use crate::store::Store;
use crate::working_memory::ScopeContext;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const SYNC_HTTP_BUDGET: Duration = Duration::from_millis(250);
pub const OUTBOX_MAX_ENTRIES: usize = 500;
pub const OUTBOX_MAX_BYTES: usize = 512 * 1024;
pub const MAX_OBSERVATION_BYTES: usize = 4_096;
const CIRCUIT_BASE_COOLDOWN_MS: u64 = 5_000;
const CIRCUIT_MAX_COOLDOWN_MS: u64 = 60_000;
const OUTBOX_BATCH_LIMIT: usize = 50;
const PROMPT_CACHE_MAX_ITEMS: usize = 10;
const PROMPT_CACHE_MAX_CHARS: usize = 1_500;

pub(crate) fn truncate_observation_content(content: &str) -> String {
    v2::truncate_observation_content(content)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboxObservation {
    pub id: String,
    pub source_device: String,
    pub scope_device: String,
    pub scope_person: String,
    pub room: String,
    pub trace_id: String,
    pub content: String,
    pub created_ms: u64,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub record_id: String,
    #[serde(default)]
    pub event_kind: String,
    #[serde(default)]
    pub dimension: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ObservationInput<'a> {
    pub source_device: &'a str,
    pub scope_device: &'a str,
    pub scope_person: &'a str,
    pub room: &'a str,
    pub trace_id: &'a str,
    pub content: &'a str,
    pub domain: &'a str,
    pub record_id: &'a str,
    pub event_kind: &'a str,
    pub dimension: &'a str,
    pub expected_epoch: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedProfileEntry {
    pub id: String,
    pub scope_person: String,
    pub scope_device: String,
    #[serde(default)]
    pub source_device: String,
    #[serde(default)]
    pub dimension: String,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub row_revision: u64,
    pub summary: String,
    pub updated_ms: u64,
    pub fetched_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedMemorySyncStatus {
    pub enabled: bool,
    pub nas_base_url: String,
    pub upload_enabled: bool,
    pub token_set: bool,
    pub backlog_count: usize,
    pub cache_count: usize,
    pub circuit_open: bool,
    pub open_until_ms: u64,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub last_sync_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    SkippedUnconfigured,
    CircuitOpen,
    DiscardedReset,
    Synced { uploaded: usize, cached: usize },
    Failed(String),
}

#[derive(Default)]
struct CircuitState {
    epoch: i64,
    consecutive_failures: u32,
    open_until_ms: u64,
    last_error: Option<String>,
    last_sync_ms: Option<u64>,
}

pub struct SharedMemoryService {
    store: Arc<Store>,
    config: ConfigService,
    client: reqwest::Client,
    circuit: Mutex<CircuitState>,
    #[cfg(test)]
    test_gate: Mutex<
        Option<(
            TestStage,
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    #[cfg(test)]
    test_completion: Mutex<Option<tokio::sync::oneshot::Sender<SyncOutcome>>>,
}

impl SharedMemoryService {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(SYNC_HTTP_BUDGET)
            .timeout(SYNC_HTTP_BUDGET)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            config: ConfigService::new(store.clone()),
            store,
            client,
            circuit: Mutex::new(CircuitState::default()),
            #[cfg(test)]
            test_gate: Mutex::new(None),
            #[cfg(test)]
            test_completion: Mutex::new(None),
        })
    }

    pub fn enqueue_confirmed_memory(
        &self,
        source_device: &str,
        room: &str,
        session_id: &str,
        expected_epoch: i64,
        saved: &crate::memory::MemoryItem,
    ) {
        let dimension = match saved.kind.as_str() {
            "preference" => "preferences",
            "fact" => "facts",
            "event" => "schedule",
            _ => "general",
        };
        let _ = self.enqueue_observation_scoped(ObservationInput {
            source_device,
            scope_device: &saved.scope_device,
            scope_person: &saved.scope_person,
            room,
            trace_id: session_id,
            content: &saved.text,
            domain: "notes",
            record_id: &saved.id,
            event_kind: "accepted_candidate",
            dimension,
            expected_epoch: Some(expected_epoch),
        });
    }

    pub fn enqueue_observation(
        &self,
        source_device: &str,
        room: &str,
        scope_person: &str,
        trace_id: &str,
        content: &str,
    ) -> Result<Option<String>, String> {
        self.enqueue_observation_scoped(ObservationInput {
            source_device,
            scope_device: "",
            scope_person,
            room,
            trace_id,
            content,
            domain: "notes",
            record_id: "",
            event_kind: "explicit_save",
            dimension: "general",
            expected_epoch: None,
        })
    }

    /// Enqueue a scoped observation into `shared_memory_outbox`, enforcing
    /// `cfg.enabled && cfg.upload_enabled`, `data_epoch`, and outbox capacity
    /// bounds (`OUTBOX_MAX_ENTRIES`, `OUTBOX_MAX_BYTES`) atomically under Store.
    pub fn enqueue_observation_scoped(
        &self,
        input: ObservationInput<'_>,
    ) -> Result<Option<String>, String> {
        let content = v2::truncate_observation_content(input.content);
        if content.is_empty() {
            return Err("observation content cannot be empty".into());
        }
        let id = crate::id::next("SMO");
        let now = crate::id::now_ms();
        let source_device = input.source_device.trim();
        let scope_device = input.scope_device.trim();
        let scope_person = input.scope_person.trim();
        let room = input.room.trim();
        let trace_id = input.trace_id.trim();
        let domain = v2::normalize_domain(input.domain);
        let dimension = v2::normalize_dimension(input.dimension);
        let event_kind = v2::normalize_event_kind(input.event_kind, domain);
        let record_id = if input.record_id.trim().is_empty() {
            id.as_str()
        } else {
            input.record_id.trim()
        };

        self.store
            .with(|c| {
                use rusqlite::OptionalExtension;
                let tx = c.unchecked_transaction()?;
                let epoch = Self::epoch_from(&tx)?;
                if let Some(expected) = input.expected_epoch {
                    if epoch != expected {
                        return Err(crate::store::StoreError::Authority(
                            "shared_memory data_epoch changed".into(),
                        ));
                    }
                }
                let cfg: SharedMemoryConfig = tx
                    .query_row(
                        "SELECT value FROM config_kv WHERE key=?1",
                        [crate::config::SHARED_MEMORY_KEY],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or_default();
                if !(cfg.enabled && cfg.upload_enabled) {
                    return Ok(None);
                }
                let (count, total_bytes): (i64, i64) = tx.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(content AS BLOB))), 0) FROM shared_memory_outbox",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if (count as usize) >= OUTBOX_MAX_ENTRIES
                    || (total_bytes as usize).saturating_add(content.len()) > OUTBOX_MAX_BYTES
                {
                    return Err(crate::store::StoreError::Authority(
                        "shared_memory_outbox capacity reached".into(),
                    ));
                }
                tx.execute(
                    "INSERT INTO shared_memory_outbox
                     (id, device_id, room, person_id, trace_id, content, created_ms,
                      source_device, scope_device, scope_person, domain, record_id, event_kind, dimension)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?2,?8,?4,?9,?10,?11,?12)",
                    rusqlite::params![
                        id,
                        source_device,
                        room,
                        scope_person,
                        trace_id,
                        content,
                        now as i64,
                        scope_device,
                        domain,
                        record_id,
                        event_kind,
                        dimension,
                    ],
                )?;
                tx.commit()?;
                Ok(Some(id.clone()))
            })
            .map_err(|e| e.to_string())
    }

    pub fn backlog_count(&self) -> Result<usize, String> {
        self.store
            .with(|c| {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM shared_memory_outbox", [], |r| {
                    r.get(0)
                })?;
                Ok(n.max(0) as usize)
            })
            .map_err(|e| e.to_string())
    }

    pub fn cache_count(&self) -> Result<usize, String> {
        self.store
            .with(|c| {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM shared_profile_cache", [], |r| {
                    r.get(0)
                })?;
                Ok(n.max(0) as usize)
            })
            .map_err(|e| e.to_string())
    }

    pub fn clear_outbox(&self) -> Result<usize, String> {
        self.store
            .with(|c| Ok(c.execute("DELETE FROM shared_memory_outbox", [])?))
            .map_err(|e| e.to_string())
    }

    pub fn list_outbox(&self, limit: usize) -> Result<Vec<OutboxObservation>, String> {
        self.store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT id,
                            COALESCE(NULLIF(source_device, ''), device_id),
                            scope_device,
                            COALESCE(NULLIF(scope_person, ''), person_id),
                            room, trace_id, content, created_ms,
                            domain, record_id, event_kind, dimension
                     FROM shared_memory_outbox ORDER BY created_ms ASC, id ASC LIMIT ?1",
                )?;
                let rows = stmt.query_map([limit as i64], |r| {
                    Ok(OutboxObservation {
                        id: r.get(0)?,
                        source_device: r.get(1)?,
                        scope_device: r.get(2)?,
                        scope_person: r.get(3)?,
                        room: r.get(4)?,
                        trace_id: r.get(5)?,
                        content: r.get(6)?,
                        created_ms: r.get::<_, i64>(7)? as u64,
                        domain: r.get(8)?,
                        record_id: r.get(9)?,
                        event_kind: r.get(10)?,
                        dimension: r.get(11)?,
                    })
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .map_err(|e| e.to_string())
    }

    pub fn upsert_profile_cache(
        &self,
        id: &str,
        scope_person: &str,
        scope_device: &str,
        summary: &str,
        updated_ms: u64,
    ) -> Result<(), String> {
        self.upsert_profile_cache_with_source(
            id,
            scope_person,
            scope_device,
            "",
            summary,
            updated_ms,
        )
    }

    pub fn upsert_profile_cache_with_source(
        &self,
        id: &str,
        scope_person: &str,
        scope_device: &str,
        source_device: &str,
        summary: &str,
        updated_ms: u64,
    ) -> Result<(), String> {
        let fetched_ms = crate::id::now_ms();
        self.store
            .with(|c| {
                c.execute(
                    "INSERT INTO shared_profile_cache
                     (id, scope_person, scope_device, source_device, summary, updated_ms, fetched_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(id) DO UPDATE SET
                        scope_person=excluded.scope_person,
                        scope_device=excluded.scope_device,
                        source_device=excluded.source_device,
                        summary=excluded.summary,
                        updated_ms=excluded.updated_ms,
                        fetched_ms=excluded.fetched_ms",
                    rusqlite::params![
                        id.trim(),
                        scope_person.trim(),
                        scope_device.trim(),
                        source_device.trim(),
                        summary.trim(),
                        updated_ms as i64,
                        fetched_ms as i64,
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())
    }

    /// Render cached shared profile summaries from local SQLite only (0 ms
    /// network blocking), enforcing the same privacy boundary as `ScopeContext`.
    pub fn render_for_prompt_scoped(&self, scope: &ScopeContext) -> Option<String> {
        let entries = self
            .store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT id, scope_person, scope_device, source_device, dimension, version, row_revision, summary, updated_ms, fetched_ms
                     FROM shared_profile_cache ORDER BY updated_ms DESC, id ASC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(SharedProfileEntry {
                        id: r.get(0)?,
                        scope_person: r.get(1)?,
                        scope_device: r.get(2)?,
                        source_device: r.get(3)?,
                        dimension: r.get(4)?,
                        version: r.get::<_, i64>(5)?.max(0) as u64,
                        row_revision: r.get::<_, i64>(6)?.max(0) as u64,
                        summary: r.get(7)?,
                        updated_ms: r.get::<_, i64>(8)? as u64,
                        fetched_ms: r.get::<_, i64>(9)? as u64,
                    })
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .ok()?;

        let norm = scope.normalized();
        let matched: Vec<SharedProfileEntry> = entries
            .into_iter()
            .filter(|e| {
                if e.summary.is_empty() {
                    return false;
                }
                norm.allows(&e.scope_person, &e.scope_device)
            })
            .collect();

        if matched.is_empty() {
            return None;
        }
        let mut out = String::new();
        let mut used = 0usize;
        let mut count = 0usize;
        for entry in matched {
            let line = format!("- {}\n", entry.summary);
            let line_chars = line.chars().count();
            if count >= PROMPT_CACHE_MAX_ITEMS
                || (count > 0 && used + line_chars > PROMPT_CACHE_MAX_CHARS)
            {
                break;
            }
            used += line_chars;
            count += 1;
            out.push_str(&line);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub fn is_circuit_open(&self) -> bool {
        self.current_circuit(|st| crate::id::now_ms() < st.open_until_ms)
            .unwrap_or(false)
    }

    fn internal_token(&self) -> Option<String> {
        self.store
            .secret_get(SHARED_MEMORY_TOKEN_SECRET)
            .ok()
            .flatten()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    pub fn status(&self) -> SharedMemorySyncStatus {
        let cfg = self.config.load_shared_memory().unwrap_or_default();
        let token_set = self.internal_token().is_some();
        let backlog_count = self.backlog_count().unwrap_or(0);
        let cache_count = self.cache_count().unwrap_or(0);
        let now = crate::id::now_ms();
        let (open_until_ms, consecutive_failures, last_error, last_sync_ms) = self
            .current_circuit(|st| {
                (
                    st.open_until_ms,
                    st.consecutive_failures,
                    st.last_error.clone(),
                    st.last_sync_ms,
                )
            })
            .unwrap_or_else(|error| (0, 0, Some(error), None));
        SharedMemorySyncStatus {
            enabled: cfg.enabled,
            nas_base_url: cfg.nas_base_url,
            upload_enabled: cfg.upload_enabled,
            token_set,
            backlog_count,
            cache_count,
            circuit_open: now < open_until_ms,
            open_until_ms,
            consecutive_failures,
            last_error,
            last_sync_ms,
        }
    }

    pub fn save_config(
        &self,
        cfg: SharedMemoryConfig,
        actor: &str,
    ) -> Result<SharedMemoryConfig, String> {
        self.save_config_with_token(cfg, None, actor)
    }

    pub fn save_config_with_token(
        &self,
        cfg: SharedMemoryConfig,
        token: Option<&str>,
        actor: &str,
    ) -> Result<SharedMemoryConfig, String> {
        let saved = self
            .config
            .save_shared_memory_with_token(cfg, token, actor)
            .map_err(|e| e.to_string())?;
        // Reset circuit breaker when config is updated so user edits retry immediately.
        self.current_circuit(|st| {
            st.consecutive_failures = 0;
            st.open_until_ms = 0;
            st.last_error = None;
        })?;
        Ok(saved)
    }

    /// Run one bounded background sync cycle against NAS `memory-svc`.
    /// Never called synchronously on a live voice turn.
    pub async fn sync_once(&self) -> SyncOutcome {
        #[cfg(test)]
        let completed = self.test_completion.lock().unwrap().take();
        let outcome = self.sync_once_inner().await;
        #[cfg(test)]
        if let Some(completed) = completed {
            let _ = completed.send(outcome.clone());
        }
        outcome
    }

    async fn sync_once_inner(&self) -> SyncOutcome {
        // Capture before configuration/credentials/outbox preparation, never
        // attach a new epoch to configuration read before a reset.
        let epoch = match self.epoch() {
            Ok(epoch) => epoch,
            Err(error) => return SyncOutcome::Failed(error),
        };
        let cfg = self.config.load_shared_memory().unwrap_or_default();
        let base = cfg.nas_base_url.trim().trim_end_matches('/').to_string();
        let token = self.internal_token();
        let batch = if cfg.upload_enabled {
            match self.list_outbox(OUTBOX_BATCH_LIMIT) {
                Ok(batch) => batch,
                Err(error) => return self.failure(epoch, &error),
            }
        } else {
            Vec::new()
        };
        #[cfg(test)]
        self.test_boundary(TestStage::AttemptPrepared).await;
        if let Err(outcome) = self.admit(epoch) {
            return outcome;
        }
        if !cfg.enabled || base.is_empty() {
            return SyncOutcome::SkippedUnconfigured;
        }
        match self.at_epoch(epoch, |_| {
            let mut st = crate::lock_or_recover(&self.circuit, "shared_memory.circuit");
            Self::bind_circuit(&mut st, epoch);
            Ok(crate::id::now_ms() < st.open_until_ms)
        }) {
            Ok(Some(true)) => return SyncOutcome::CircuitOpen,
            Ok(Some(false)) => {}
            Ok(None) => return SyncOutcome::DiscardedReset,
            Err(error) => return SyncOutcome::Failed(error),
        }

        let mut uploaded = 0;
        let mut nas_source_epoch: Option<String> = None;
        if !batch.is_empty() {
            match self
                .upload_observations(
                    epoch,
                    &base,
                    token.as_deref(),
                    &batch,
                    &mut nas_source_epoch,
                )
                .await
            {
                Ok(n) => uploaded = n,
                Err(outcome) => return outcome,
            }
        }
        let profiles = match self
            .fetch_profiles(epoch, &base, token.as_deref(), nas_source_epoch.as_deref())
            .await
        {
            Ok(profiles) => profiles,
            Err(outcome) => return outcome,
        };
        let cached = match self.publish(epoch, &profiles) {
            Ok(Some(count)) => count,
            Ok(None) => return SyncOutcome::DiscardedReset,
            Err(error) => return self.failure(epoch, &error),
        };
        #[cfg(test)]
        self.test_boundary(TestStage::ProfilesPublished).await;
        match self.success(epoch) {
            Ok(Some(())) => SyncOutcome::Synced { uploaded, cached },
            Ok(None) => SyncOutcome::DiscardedReset,
            Err(error) => SyncOutcome::Failed(error),
        }
    }

    fn authed_get(&self, url: String, token: Option<&str>) -> reqwest::RequestBuilder {
        let req = self.client.get(url);
        match token {
            Some(t) => req.header("X-Internal-Token", t),
            None => req,
        }
    }

    fn authed_post(&self, url: String, token: Option<&str>) -> reqwest::RequestBuilder {
        let req = self.client.post(url);
        match token {
            Some(t) => req.header("X-Internal-Token", t),
            None => req,
        }
    }

    async fn upload_observations(
        &self,
        epoch: i64,
        base: &str,
        token: Option<&str>,
        batch: &[OutboxObservation],
        nas_source_epoch: &mut Option<String>,
    ) -> Result<usize, SyncOutcome> {
        let mut use_v2 = false;
        let status_req = self.authed_get(format!("{base}/api/v2/status"), token);
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }
        if let Ok(resp) = status_req.send().await {
            if resp.status().is_success() {
                if let Ok(st) = resp.json::<v2::V2StatusResponse>().await {
                    if let Some(se) = st.source_epoch.filter(|s| !s.trim().is_empty()) {
                        *nas_source_epoch = Some(se);
                        use_v2 = true;
                    }
                }
            }
        }
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }

        if use_v2 {
            let source_epoch = nas_source_epoch.as_deref().unwrap_or("");
            let core_id = self.node_id().unwrap_or_default();
            let mut uploaded = 0usize;
            for (idx, chunk) in batch.chunks(v2::V2_INGEST_BATCH_LIMIT).enumerate() {
                let body = v2::build_v2_ingest_payload(&core_id, epoch as u64, source_epoch, chunk);
                let req = self
                    .authed_post(format!("{base}/api/v2/observations"), token)
                    .json(&body);
                if let Err(outcome) = self.admit(epoch) {
                    return Err(outcome);
                }
                let response = req.send().await;
                #[cfg(test)]
                if idx == 0 {
                    self.test_boundary(TestStage::UploadResponse).await;
                }
                if let Err(outcome) = self.admit(epoch) {
                    return Err(outcome);
                }
                match response {
                    Ok(resp) if resp.status().is_success() => {
                        let ids: Vec<String> = chunk.iter().map(|item| item.id.clone()).collect();
                        match self.acknowledge(epoch, &ids) {
                            Ok(Some(_)) => uploaded += ids.len(),
                            Ok(None) => return Err(SyncOutcome::DiscardedReset),
                            Err(error) => return Err(self.failure(epoch, &error)),
                        }
                    }
                    Ok(resp)
                        if idx == 0
                            && (resp.status() == reqwest::StatusCode::NOT_FOUND
                                || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED) =>
                    {
                        return self
                            .upload_observations_v1(epoch, base, token, batch, false)
                            .await;
                    }
                    Ok(resp) => {
                        return Err(self.failure(
                            epoch,
                            &format!("NAS upload returned HTTP {}", resp.status()),
                        ));
                    }
                    Err(error) => {
                        return Err(self.failure(epoch, &format!("NAS upload error: {error}")));
                    }
                }
            }
            return Ok(uploaded);
        }

        self.upload_observations_v1(epoch, base, token, batch, true)
            .await
    }

    async fn upload_observations_v1(
        &self,
        epoch: i64,
        base: &str,
        token: Option<&str>,
        batch: &[OutboxObservation],
        fire_test_gate: bool,
    ) -> Result<usize, SyncOutcome> {
        let v1_rows: Vec<serde_json::Value> = batch
            .iter()
            .map(|o| {
                serde_json::json!({
                    "id": o.id,
                    "device_id": o.source_device,
                    "room": o.room,
                    "person_id": o.scope_person,
                    "trace_id": o.trace_id,
                    "content": o.content,
                    "created_ms": o.created_ms,
                })
            })
            .collect();
        let req = self
            .authed_post(format!("{base}/api/v1/observations"), token)
            .json(&serde_json::json!({ "observations": v1_rows }));
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }
        let response = req.send().await;
        #[cfg(test)]
        if fire_test_gate {
            self.test_boundary(TestStage::UploadResponse).await;
        }
        #[cfg(not(test))]
        let _ = fire_test_gate;
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }
        match response {
            Ok(response) if response.status().is_success() => {
                let ids: Vec<String> = batch.iter().map(|item| item.id.clone()).collect();
                match self.acknowledge(epoch, &ids) {
                    Ok(Some(_)) => Ok(ids.len()),
                    Ok(None) => Err(SyncOutcome::DiscardedReset),
                    Err(error) => Err(self.failure(epoch, &error)),
                }
            }
            Ok(response) => Err(self.failure(
                epoch,
                &format!("NAS upload returned HTTP {}", response.status()),
            )),
            Err(error) => Err(self.failure(epoch, &format!("NAS upload error: {error}"))),
        }
    }

    async fn fetch_profiles(
        &self,
        epoch: i64,
        base: &str,
        token: Option<&str>,
        initial_source_epoch: Option<&str>,
    ) -> Result<Vec<ProfileWire>, SyncOutcome> {
        let mut profiles = Vec::new();
        let mut cursor: Option<String> = None;
        let mut expected_source_epoch = initial_source_epoch.map(str::to_string);

        for page in 0..v2::V2_MAX_PROFILE_PAGES {
            let mut req = self
                .authed_get(format!("{base}/api/v2/profiles"), token)
                .query(&[("limit", v2::V2_PROFILE_PAGE_LIMIT.to_string())]);
            if let Some(ref c) = cursor {
                req = req.query(&[("cursor", c.as_str())]);
            }
            if let Some(ref se) = expected_source_epoch {
                req = req.query(&[("expected_source_epoch", se.as_str())]);
            }
            if let Err(outcome) = self.admit(epoch) {
                return Err(outcome);
            }
            let response = req.send().await;
            if let Ok(ref r) = response {
                if page == 0
                    && (r.status() == reqwest::StatusCode::NOT_FOUND
                        || r.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED)
                {
                    return self.fetch_profiles_v1(epoch, base, token, true).await;
                }
            }
            #[cfg(test)]
            if page == 0 {
                self.test_boundary(TestStage::ProfilesResponse).await;
            }
            if let Err(outcome) = self.admit(epoch) {
                return Err(outcome);
            }
            let resp = match response {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    return Err(
                        self.failure(epoch, &format!("NAS profiles returned HTTP {}", r.status()))
                    );
                }
                Err(error) => {
                    return Err(self.failure(epoch, &format!("NAS profiles error: {error}")));
                }
            };
            let decoded = resp.json::<v2::V2ProfilesEnvelope>().await;
            #[cfg(test)]
            if page == 0 && decoded.is_ok() {
                self.test_boundary(TestStage::ProfilesDecoded).await;
            }
            if let Err(outcome) = self.admit(epoch) {
                return Err(outcome);
            }
            let envelope = match decoded {
                Ok(env) => env,
                Err(error) => {
                    return Err(self.failure(epoch, &format!("NAS profiles decode error: {error}")));
                }
            };
            if expected_source_epoch.is_none() {
                expected_source_epoch = envelope.source_epoch.filter(|s| !s.trim().is_empty());
            }
            profiles.extend(envelope.items.into_iter().map(ProfileWire::from));
            match envelope.next_cursor.filter(|c| !c.trim().is_empty()) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(profiles)
    }

    async fn fetch_profiles_v1(
        &self,
        epoch: i64,
        base: &str,
        token: Option<&str>,
        fire_response_gate: bool,
    ) -> Result<Vec<ProfileWire>, SyncOutcome> {
        let req = self.authed_get(format!("{base}/api/v1/profiles"), token);
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }
        let response = req.send().await;
        #[cfg(test)]
        if fire_response_gate {
            self.test_boundary(TestStage::ProfilesResponse).await;
        }
        #[cfg(not(test))]
        let _ = fire_response_gate;
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }
        let response = match response {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                return Err(self.failure(
                    epoch,
                    &format!("NAS profiles returned HTTP {}", response.status()),
                ));
            }
            Err(error) => return Err(self.failure(epoch, &format!("NAS profiles error: {error}"))),
        };
        let decoded = response.json::<ProfilesEnvelope>().await;
        #[cfg(test)]
        if decoded.is_ok() {
            self.test_boundary(TestStage::ProfilesDecoded).await;
        }
        if let Err(outcome) = self.admit(epoch) {
            return Err(outcome);
        }
        match decoded {
            Ok(envelope) => Ok(envelope.profiles),
            Err(error) => Err(self.failure(epoch, &format!("NAS profiles decode error: {error}"))),
        }
    }

    pub fn spawn_background_sync(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let _ = svc.sync_once().await;
            }
        })
    }
}

mod lifecycle;
mod v2;

#[cfg(test)]
#[path = "shared_memory/reset_tests.rs"]
mod reset_tests;

fn default_profile_version() -> u64 {
    1
}

#[derive(Deserialize)]
struct ProfileWire {
    id: String,
    #[serde(default)]
    scope_person: String,
    #[serde(default)]
    scope_device: String,
    #[serde(default)]
    source_device: String,
    #[serde(default)]
    dimension: String,
    #[serde(default = "default_profile_version")]
    version: u64,
    #[serde(default)]
    row_revision: u64,
    summary: String,
    #[serde(default)]
    updated_ms: u64,
}
#[derive(Deserialize)]
struct ProfilesEnvelope {
    #[serde(default)]
    profiles: Vec<ProfileWire>,
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
enum TestStage {
    AttemptPrepared,
    ProfilesPublished,
    UploadResponse,
    ProfilesResponse,
    ProfilesDecoded,
}
#[cfg(test)]
impl SharedMemoryService {
    async fn test_boundary(&self, stage: TestStage) {
        let gate = {
            let mut slot = self.test_gate.lock().unwrap();
            if slot.as_ref().is_some_and(|gate| gate.0 == stage) {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, reached, release)) = gate {
            let _ = reached.send(());
            let _ = release.await;
        }
    }
}
