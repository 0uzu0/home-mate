//! Controlled downward proactive voice announcements (#267 Slice 1).
//!
//! External callers (such as the Master Brain async bridge) use
//! `POST /api/v0/voice/announce` to ask a speaker to read out a finished
//! result or prompt. Announcements share the exact same safety invariants as
//! reminders (`reminder_sweep`):
//! - bounded per-device queue (`MAX_ANNOUNCE_QUEUE_PER_DEVICE = 8`),
//! - never interrupt active user speech, ASR-in-flight, active turns, or live
//!   voice playback (defer until the speaker settles),
//! - respect hardware mute and offline state honestly (never fake delivery),
//! - gate unsolicited announcements (`task_id.is_none()`) on `proactive_speech`,
//!   while allowing user-requested task callbacks (`task_id.is_some()`),
//! - optionally open a single bounded follow-up listening window when
//!   `open_followup: true` so the user can reply ("确认" / "取消") without
//!   re-waking,
//! - record every stage (`voice_announce_queued`, `voice_announce_spoken`,
//!   `voice_announce_followup_opened`, `voice_announce_expired`,
//!   `voice_announce_failed`, `voice_announce_evicted`,
//!   `voice_announce_rejected`) in the voice event journal.

use super::*;
use serde::{Deserialize, Serialize};

pub const MAX_ANNOUNCE_QUEUE_PER_DEVICE: usize = 8;
pub const MAX_ANNOUNCE_RECENT: usize = 20;
pub const MAX_ANNOUNCE_CHARS: usize = 500;
pub const MAX_ANNOUNCE_TITLE_CHARS: usize = 80;
pub const MAX_ANNOUNCE_SOURCE_CHARS: usize = 48;
pub const MAX_ANNOUNCE_TASK_ID_CHARS: usize = 128;
pub const DEFAULT_ANNOUNCE_TTL_MS: u64 = 300_000;
pub const MIN_ANNOUNCE_TTL_MS: u64 = 10_000;
pub const MAX_ANNOUNCE_TTL_MS: u64 = 300_000;
pub const ANNOUNCE_RETRY_BACKOFF_MS: u64 = 15_000;
pub const MAX_ANNOUNCE_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnnouncePriority {
    #[default]
    Normal,
    High,
}

impl AnnouncePriority {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "normal" => Some(Self::Normal),
            "high" | "urgent" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnnounceRequest {
    pub text: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub open_followup: bool,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingAnnounce {
    pub id: String,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub text: String,
    pub priority: AnnouncePriority,
    pub open_followup: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub source: String,
    pub created_ms: u64,
    pub expires_at_ms: u64,
    pub attempts: u32,
    pub next_attempt_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnnounceHistoryEntry {
    pub id: String,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub text: String,
    pub priority: AnnouncePriority,
    pub open_followup: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub source: String,
    pub status: String,
    pub created_ms: u64,
    pub settled_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
}

#[derive(Default)]
pub(super) struct AnnounceStore {
    pub(super) queues: HashMap<String, VecDeque<PendingAnnounce>>,
    /// Per-device in-flight announcement id so two lines never overlap.
    pub(super) inflight: HashMap<String, String>,
    pub(super) recent: VecDeque<AnnounceHistoryEntry>,
}

impl AnnounceStore {
    pub(super) fn push_recent(&mut self, entry: AnnounceHistoryEntry) {
        self.recent.push_front(entry);
        while self.recent.len() > MAX_ANNOUNCE_RECENT {
            self.recent.pop_back();
        }
    }

    pub(super) fn clear(&mut self) {
        self.queues.clear();
        self.inflight.clear();
        self.recent.clear();
    }
}

#[derive(Debug, Clone)]
pub struct AnnounceError {
    pub status: u16,
    pub code: &'static str,
    pub detail: String,
}

impl AnnounceError {
    fn new(status: u16, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
        }
    }
}

impl VoicePipeline {
    /// Validate and enqueue a downward proactive announcement, then trigger an
    /// immediate sweep so an idle speaker speaks without waiting for the 1 Hz
    /// ticker.
    pub fn enqueue_announce(
        self: &Arc<Self>,
        req: AnnounceRequest,
    ) -> Result<serde_json::Value, AnnounceError> {
        let task_known = req
            .task_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some_and(|tid| self.tasks.exists(tid));
        self.enqueue_announce_checked(req, task_known)
    }

    /// Variant accepting a caller-verified `task_known` flag (e.g. HTTP API
    /// verifying `task_id` against `TaskRegistry` OR `MasterBrainRuntime`, or
    /// internal `MasterBrainRuntime` callbacks for tracked delegated tasks).
    pub fn enqueue_announce_checked(
        self: &Arc<Self>,
        req: AnnounceRequest,
        task_known: bool,
    ) -> Result<serde_json::Value, AnnounceError> {
        let text = req.text.trim();
        if text.is_empty() {
            return Err(AnnounceError::new(
                400,
                "ANNOUNCE_EMPTY_TEXT",
                "text is required",
            ));
        }
        if text.chars().count() > MAX_ANNOUNCE_CHARS {
            return Err(AnnounceError::new(
                400,
                "ANNOUNCE_TEXT_TOO_LONG",
                format!("text must be at most {MAX_ANNOUNCE_CHARS} characters"),
            ));
        }
        let title = req
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if title
            .as_ref()
            .is_some_and(|t| t.chars().count() > MAX_ANNOUNCE_TITLE_CHARS)
        {
            return Err(AnnounceError::new(
                400,
                "ANNOUNCE_BAD_TITLE",
                format!("title must be at most {MAX_ANNOUNCE_TITLE_CHARS} characters"),
            ));
        }
        let priority = match req.priority.as_deref() {
            None => AnnouncePriority::Normal,
            Some(raw) => AnnouncePriority::parse(raw).ok_or_else(|| {
                AnnounceError::new(
                    400,
                    "ANNOUNCE_BAD_PRIORITY",
                    r#"priority must be "normal" or "high""#,
                )
            })?,
        };
        let task_id = req
            .task_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if task_id
            .as_ref()
            .is_some_and(|t| t.chars().count() > MAX_ANNOUNCE_TASK_ID_CHARS)
        {
            return Err(AnnounceError::new(
                400,
                "ANNOUNCE_BAD_TASK_ID",
                format!("task_id must be at most {MAX_ANNOUNCE_TASK_ID_CHARS} characters"),
            ));
        }
        let source = req
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("external");
        if source.chars().count() > MAX_ANNOUNCE_SOURCE_CHARS {
            return Err(AnnounceError::new(
                400,
                "ANNOUNCE_BAD_SOURCE",
                format!("source must be at most {MAX_ANNOUNCE_SOURCE_CHARS} characters"),
            ));
        }

        // Unsolicited announcements (no valid user-originated task_id) obey the
        // household `proactive_speech` toggle. Task callbacks are the user's
        // own delegated work, matching how reminders are not blocked by it —
        // but arbitrary unknown task_ids cannot bypass `proactive_speech=false`.
        let proactive_enabled = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            let target_dev = req
                .device
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    self.hub
                        .resolve_default_audio_out()
                        .ok()
                        .flatten()
                        .map(|d| d.stable_id)
                });
            match target_dev.as_deref() {
                Some(dev) => s.effective_for(dev).proactive_speech,
                None => s.proactive_speech,
            }
        };
        let verified_task_callback = task_id.is_some() && task_known;
        if !verified_task_callback && !proactive_enabled {
            let reason = if task_id.is_some() {
                "unknown_task_id"
            } else {
                "proactive_speech_disabled"
            };
            self.note(
                "voice_announce_rejected",
                json!({
                    "reason": reason,
                    "source": source,
                    "priority": priority.as_str(),
                    "task_id": task_id,
                }),
            );
            return Err(AnnounceError::new(
                409,
                "PROACTIVE_SPEECH_DISABLED",
                if task_id.is_some() {
                    "proactive_speech is disabled and task_id does not match a known Core or Master Brain task"
                } else {
                    "proactive_speech is disabled for unsolicited announcements (set a known task_id for user-requested task callbacks or enable proactive_speech)"
                },
            ));
        }

        let device = self.resolve_announce_device(req.device.as_deref())?;
        let now = id::now_ms();
        let ttl_ms = req
            .ttl_ms
            .unwrap_or(DEFAULT_ANNOUNCE_TTL_MS)
            .clamp(MIN_ANNOUNCE_TTL_MS, MAX_ANNOUNCE_TTL_MS);
        let entry = PendingAnnounce {
            id: id::next("A"),
            device: device.clone(),
            title: title.clone(),
            text: text.to_string(),
            priority,
            open_followup: req.open_followup,
            task_id: task_id.clone(),
            source: source.to_string(),
            created_ms: now,
            expires_at_ms: now.saturating_add(ttl_ms),
            attempts: 0,
            next_attempt_ms: now,
        };

        let mut evicted: Option<PendingAnnounce> = None;
        let mut expired_during_enqueue: Vec<PendingAnnounce> = Vec::new();
        let queue_depth = {
            let mut store = crate::lock_or_recover(&self.announces, "voice.announces");
            let q = store.queues.entry(device.clone()).or_default();
            // Prune any already-expired items before enforcing the cap.
            let mut expired = Vec::new();
            q.retain(|item| {
                if now >= item.expires_at_ms {
                    expired.push(item.clone());
                    false
                } else {
                    true
                }
            });
            for exp in expired {
                store.push_recent(AnnounceHistoryEntry {
                    id: exp.id.clone(),
                    device: exp.device.clone(),
                    title: exp.title.clone(),
                    text: exp.text.clone(),
                    priority: exp.priority,
                    open_followup: exp.open_followup,
                    task_id: exp.task_id.clone(),
                    source: exp.source.clone(),
                    status: "expired".into(),
                    created_ms: exp.created_ms,
                    settled_ms: now,
                    stage: None,
                });
                expired_during_enqueue.push(exp);
            }
            let q = store.queues.entry(device.clone()).or_default();
            if q.len() >= MAX_ANNOUNCE_QUEUE_PER_DEVICE {
                if priority == AnnouncePriority::High {
                    if let Some(pos) = q
                        .iter()
                        .position(|item| item.priority == AnnouncePriority::Normal)
                    {
                        evicted = q.remove(pos);
                    } else {
                        return Err(AnnounceError::new(
                            429,
                            "ANNOUNCE_QUEUE_FULL",
                            format!(
                                "announce queue for device '{device}' is full ({MAX_ANNOUNCE_QUEUE_PER_DEVICE} high-priority items)"
                            ),
                        ));
                    }
                } else {
                    return Err(AnnounceError::new(
                        429,
                        "ANNOUNCE_QUEUE_FULL",
                        format!(
                            "announce queue for device '{device}' is full (max {MAX_ANNOUNCE_QUEUE_PER_DEVICE})"
                        ),
                    ));
                }
            }
            q.push_back(entry.clone());
            let depth = q.len();
            if let Some(ref ev) = evicted {
                store.push_recent(AnnounceHistoryEntry {
                    id: ev.id.clone(),
                    device: ev.device.clone(),
                    title: ev.title.clone(),
                    text: ev.text.clone(),
                    priority: ev.priority,
                    open_followup: ev.open_followup,
                    task_id: ev.task_id.clone(),
                    source: ev.source.clone(),
                    status: "evicted".into(),
                    created_ms: ev.created_ms,
                    settled_ms: now,
                    stage: None,
                });
            }
            depth
        };

        for exp in expired_during_enqueue {
            self.note(
                "voice_announce_expired",
                json!({
                    "device": exp.device,
                    "id": exp.id,
                    "task_id": exp.task_id,
                    "source": exp.source,
                    "attempts": exp.attempts,
                }),
            );
        }

        if let Some(ev) = evicted {
            self.note(
                "voice_announce_evicted",
                json!({
                    "device": ev.device,
                    "id": ev.id,
                    "evicted_by": entry.id,
                    "task_id": ev.task_id,
                    "source": ev.source,
                }),
            );
        }

        self.note(
            "voice_announce_queued",
            json!({
                "device": device,
                "id": entry.id,
                "title": title,
                "priority": priority.as_str(),
                "open_followup": entry.open_followup,
                "task_id": task_id,
                "source": source,
                "chars": text.chars().count(),
                "queue_depth": queue_depth,
                "expires_at_ms": entry.expires_at_ms,
            }),
        );

        let deferred_reason = self.announce_defer_reason(&device, now);
        if tokio::runtime::Handle::try_current().is_ok() {
            self.announce_sweep();
        }

        Ok(json!({
            "id": entry.id,
            "device": device,
            "status": if deferred_reason.is_none() { "accepted" } else { "queued" },
            "deferred_reason": deferred_reason,
            "priority": priority.as_str(),
            "open_followup": entry.open_followup,
            "task_id": entry.task_id,
            "source": entry.source,
            "queue_depth": queue_depth,
            "expires_at_ms": entry.expires_at_ms,
        }))
    }

    fn resolve_announce_device(&self, requested: Option<&str>) -> Result<String, AnnounceError> {
        let online_ids: std::collections::HashSet<String> = self.hub.snapshot()["devices"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|d| d["online"].as_bool().unwrap_or(false))
                    .filter_map(|d| d["stable_id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(raw) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            if !self.hub.device_exists(raw) {
                return Err(AnnounceError::new(
                    404,
                    "ANNOUNCE_UNKNOWN_DEVICE",
                    format!("unknown device '{raw}'"),
                ));
            }
            if !online_ids.contains(raw) {
                return Err(AnnounceError::new(
                    503,
                    "ANNOUNCE_DEVICE_OFFLINE",
                    format!("device '{raw}' is currently offline"),
                ));
            }
            return Ok(raw.to_string());
        }

        if online_ids.len() > 1 {
            return Err(AnnounceError::new(
                400,
                "ANNOUNCE_DEVICE_REQUIRED",
                "multiple devices are online; specify target device explicitly",
            ));
        }

        self.hub
            .primary_audio_out()
            .map(|d| d.stable_id)
            .filter(|id| online_ids.contains(id))
            .ok_or_else(|| {
                AnnounceError::new(
                    503,
                    "ANNOUNCE_NO_ONLINE_DEVICE",
                    "no online audio_out device available",
                )
            })
    }

    /// Returns `None` when `device` can accept a non-interrupting proactive
    /// announcement right now, or a short machine-readable reason why it must
    /// defer.
    fn announce_defer_reason(&self, device: &str, now: u64) -> Option<&'static str> {
        let hub_live = self.hub_live(device);
        let (muted, has_task, has_voice_out, session_open, asr_inflight, last_close_age_ms) = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            match states.get(device) {
                Some(st) => {
                    let live = |o: &OutputRecord| {
                        !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                    };
                    (
                        st.muted == Some(true),
                        st.current_task.is_some(),
                        st.outputs.values().any(|o| o.channel == "voice" && live(o)),
                        st.session_open_with_live(now, hub_live),
                        st.mic_counters.asr_inflight,
                        st.mic_last_close_ms
                            .map(|t| now.saturating_sub(t))
                            .unwrap_or(u64::MAX),
                    )
                }
                None => (false, false, false, false, 0, u64::MAX),
            }
        };
        if muted {
            return Some("muted");
        }
        if has_task {
            return Some("turn_active");
        }
        if has_voice_out || hub_live.voice {
            return Some("voice_playing");
        }
        if session_open {
            return Some("listening_session");
        }
        let speaking_now = self
            .mic_arc(device)
            .map(|arc| crate::lock_or_recover(&arc, "voice.mic.dev").in_speech())
            .unwrap_or(false);
        if speaking_now {
            return Some("user_speaking");
        }
        if asr_inflight > 0 {
            return Some("asr_inflight");
        }
        if last_close_age_ms < REMINDER_QUIET_AFTER_VOICE_MS {
            return Some("post_speech_quiet");
        }
        None
    }

    /// 1 Hz sweep (and immediate trigger on enqueue): expires stale entries
    /// and speaks the highest-priority ready announcement per idle online
    /// device without interrupting live turns or speech.
    pub(super) fn announce_sweep(self: &Arc<Self>) {
        let now = id::now_ms();
        let online: std::collections::HashSet<String> = self.hub.snapshot()["devices"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|d| d["online"].as_bool().unwrap_or(false))
                    .filter_map(|d| d["stable_id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let mut expired_events: Vec<PendingAnnounce> = Vec::new();
        let mut candidates: Vec<PendingAnnounce> = Vec::new();
        {
            let mut store = crate::lock_or_recover(&self.announces, "voice.announces");
            let devices: Vec<String> = store.queues.keys().cloned().collect();
            for dev in devices {
                let is_inflight = store.inflight.contains_key(&dev);
                let mut dev_expired = Vec::new();
                if let Some(q) = store.queues.get_mut(&dev) {
                    q.retain(|item| {
                        if now >= item.expires_at_ms && !is_inflight {
                            dev_expired.push(item.clone());
                            false
                        } else {
                            true
                        }
                    });
                    if !is_inflight && online.contains(&dev) {
                        // Pick first ready High item, else first ready Normal item.
                        let picked = q
                            .iter()
                            .find(|i| {
                                i.priority == AnnouncePriority::High && now >= i.next_attempt_ms
                            })
                            .or_else(|| q.iter().find(|i| now >= i.next_attempt_ms))
                            .cloned();
                        if let Some(item) = picked {
                            candidates.push(item);
                        }
                    }
                }
                for exp in dev_expired {
                    store.push_recent(AnnounceHistoryEntry {
                        id: exp.id.clone(),
                        device: exp.device.clone(),
                        title: exp.title.clone(),
                        text: exp.text.clone(),
                        priority: exp.priority,
                        open_followup: exp.open_followup,
                        task_id: exp.task_id.clone(),
                        source: exp.source.clone(),
                        status: "expired".into(),
                        created_ms: exp.created_ms,
                        settled_ms: now,
                        stage: None,
                    });
                    expired_events.push(exp);
                }
                if store.queues.get(&dev).is_some_and(|q| q.is_empty()) {
                    store.queues.remove(&dev);
                }
            }
        }

        for exp in expired_events {
            self.note(
                "voice_announce_expired",
                json!({
                    "device": exp.device,
                    "id": exp.id,
                    "task_id": exp.task_id,
                    "source": exp.source,
                    "attempts": exp.attempts,
                }),
            );
        }

        for item in candidates {
            if self.announce_defer_reason(&item.device, now).is_some() {
                continue;
            }
            let permit = {
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                let st = states.entry(item.device.clone()).or_default();
                let hub_voice = self.hub_live(&item.device).voice;
                let live = |o: &OutputRecord| {
                    !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                };
                if st.muted == Some(true)
                    || st.current_task.is_some()
                    || st.outputs.values().any(|o| o.channel == "voice" && live(o))
                    || hub_voice
                {
                    continue;
                }
                OutputPermit {
                    generation: st.participation.generation(),
                    stop_epoch: st.stop_epoch,
                    request: None,
                }
            };
            {
                let mut store = crate::lock_or_recover(&self.announces, "voice.announces");
                if store.inflight.contains_key(&item.device) {
                    continue;
                }
                store.inflight.insert(item.device.clone(), item.id.clone());
            }

            let pipe = self.clone();
            tokio::spawn(async move {
                let terminal = pipe
                    .speak_inner_at(&item.device, &item.text, None, "prompt", true, permit)
                    .await;
                let ok = terminal
                    .as_ref()
                    .map(|t| t.heard() || t.written_fully())
                    .unwrap_or(false);
                let stage = terminal.as_ref().map(|t| t.stage.clone());
                let settled_ms = id::now_ms();
                if ok {
                    {
                        let mut store = crate::lock_or_recover(&pipe.announces, "voice.announces");
                        store.inflight.remove(&item.device);
                        if let Some(q) = store.queues.get_mut(&item.device) {
                            q.retain(|e| e.id != item.id);
                            if q.is_empty() {
                                store.queues.remove(&item.device);
                            }
                        }
                        store.push_recent(AnnounceHistoryEntry {
                            id: item.id.clone(),
                            device: item.device.clone(),
                            title: item.title.clone(),
                            text: item.text.clone(),
                            priority: item.priority,
                            open_followup: item.open_followup,
                            task_id: item.task_id.clone(),
                            source: item.source.clone(),
                            status: "spoken".into(),
                            created_ms: item.created_ms,
                            settled_ms,
                            stage: stage.clone(),
                        });
                    }
                    pipe.note(
                        "voice_announce_spoken",
                        json!({
                            "device": item.device,
                            "id": item.id,
                            "task_id": item.task_id,
                            "source": item.source,
                            "priority": item.priority.as_str(),
                            "open_followup": item.open_followup,
                            "stage": stage,
                            "wait_ms": settled_ms.saturating_sub(item.created_ms),
                        }),
                    );
                    if item.open_followup {
                        pipe.open_announce_followup(
                            &item.device,
                            &item.id,
                            item.task_id.as_deref(),
                        );
                    }
                } else {
                    let mut dropped = false;
                    let mut attempts_now = item.attempts + 1;
                    {
                        let mut store = crate::lock_or_recover(&pipe.announces, "voice.announces");
                        store.inflight.remove(&item.device);
                        let mut failed_entry: Option<PendingAnnounce> = None;
                        if let Some(q) = store.queues.get_mut(&item.device) {
                            if let Some(pos) = q.iter().position(|e| e.id == item.id) {
                                q[pos].attempts += 1;
                                attempts_now = q[pos].attempts;
                                if attempts_now >= MAX_ANNOUNCE_ATTEMPTS {
                                    failed_entry = q.remove(pos);
                                    dropped = true;
                                } else {
                                    q[pos].next_attempt_ms =
                                        settled_ms.saturating_add(ANNOUNCE_RETRY_BACKOFF_MS);
                                }
                            }
                            if q.is_empty() {
                                store.queues.remove(&item.device);
                            }
                        }
                        if let Some(f) = failed_entry {
                            store.push_recent(AnnounceHistoryEntry {
                                id: f.id,
                                device: f.device,
                                title: f.title,
                                text: f.text,
                                priority: f.priority,
                                open_followup: f.open_followup,
                                task_id: f.task_id,
                                source: f.source,
                                status: "failed".into(),
                                created_ms: f.created_ms,
                                settled_ms,
                                stage: stage.clone(),
                            });
                        }
                    }
                    pipe.note(
                        "voice_announce_failed",
                        json!({
                            "device": item.device,
                            "id": item.id,
                            "task_id": item.task_id,
                            "source": item.source,
                            "attempts": attempts_now,
                            "dropped": dropped,
                            "retry_in_ms": (!dropped).then_some(ANNOUNCE_RETRY_BACKOFF_MS),
                            "stage": stage,
                        }),
                    );
                }
            });
        }
    }

    /// Open a single bounded follow-up listening window right after an
    /// announcement with `open_followup: true` finishes speaking.
    fn open_announce_followup(&self, device: &str, announce_id: &str, task_id: Option<&str>) {
        let now = id::now_ms();
        let base_window_ms = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            s.effective_for(device)
                .follow_window_ms
                .clamp(5_000, 30_000)
        };
        let policy = FollowWindowPolicy {
            enabled: true,
            ms: base_window_ms,
            credits: Some(1),
        };
        let opened = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            if st.muted == Some(true) || st.current_task.is_some() {
                false
            } else {
                let expected = st.participation.identity();
                if st
                    .participation
                    .activate(expected, policy, CreditGrant::One, now)
                    .is_some()
                {
                    st.follow_open_last_tick = true;
                    st.follow_close_pending_at = None;
                    st.window_opener = Some("announce_followup");
                    true
                } else {
                    false
                }
            }
        };
        if opened {
            self.claim_sovereignty(device, now);
            self.push_assistant_state(device, false);
            self.note(
                "voice_announce_followup_opened",
                json!({
                    "device": device,
                    "id": announce_id,
                    "task_id": task_id,
                    "window_ms": base_window_ms,
                    "credits": 1,
                }),
            );
        }
    }

    /// Full announcement queue & recent delivery snapshot for
    /// `GET /api/v0/voice/announce`.
    pub fn announce_snapshot(&self) -> serde_json::Value {
        let proactive_speech =
            crate::lock_or_recover(&self.settings, "voice.settings").proactive_speech;
        let store = crate::lock_or_recover(&self.announces, "voice.announces");
        let mut pending: Vec<PendingAnnounce> = Vec::new();
        for q in store.queues.values() {
            pending.extend(q.iter().cloned());
        }
        pending.sort_by_key(|p| p.created_ms);
        let recent: Vec<AnnounceHistoryEntry> = store.recent.iter().cloned().collect();
        let inflight: serde_json::Map<String, serde_json::Value> = store
            .inflight
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        json!({
            "proactive_speech": proactive_speech,
            "max_queue_per_device": MAX_ANNOUNCE_QUEUE_PER_DEVICE,
            "pending": pending,
            "inflight": inflight,
            "recent": recent,
        })
    }

    /// Compact summary included in `GET /api/v0/mode`.
    pub fn announce_queue_summary(&self) -> serde_json::Value {
        let store = crate::lock_or_recover(&self.announces, "voice.announces");
        let pending_total: usize = store.queues.values().map(VecDeque::len).sum();
        json!({
            "pending": pending_total,
            "inflight": store.inflight.len(),
            "recent": store.recent.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_parsing_accepts_normal_high_and_urgent_alias_only() {
        assert_eq!(AnnouncePriority::parse(""), Some(AnnouncePriority::Normal));
        assert_eq!(
            AnnouncePriority::parse("normal"),
            Some(AnnouncePriority::Normal)
        );
        assert_eq!(
            AnnouncePriority::parse("HIGH"),
            Some(AnnouncePriority::High)
        );
        assert_eq!(
            AnnouncePriority::parse("urgent"),
            Some(AnnouncePriority::High)
        );
        assert_eq!(AnnouncePriority::parse("critical"), None);
    }
}
