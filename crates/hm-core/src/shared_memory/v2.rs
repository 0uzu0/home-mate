//! NAS `memory-svc` `/api/v2/*` wire types and payload builders.

use super::{OutboxObservation, ProfileWire, MAX_OBSERVATION_BYTES};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const V2_INGEST_BATCH_LIMIT: usize = 16;
pub(super) const V2_PROFILE_PAGE_LIMIT: usize = 100;
pub(super) const V2_MAX_PROFILE_PAGES: usize = 50;

static BATCH_SEQ: AtomicU64 = AtomicU64::new(1);

pub(super) fn truncate_observation_content(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.len() <= MAX_OBSERVATION_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_OBSERVATION_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].trim_end().to_string()
}

pub(super) fn deterministic_uuid(seed: &str) -> String {
    let hash = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

pub(super) fn next_batch_uuid() -> String {
    let seq = BATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let now = crate::id::now_ms();
    deterministic_uuid(&format!("hm-batch:{now}:{seq}"))
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

pub(super) fn normalize_domain(domain: &str) -> &'static str {
    match domain.trim() {
        "workmem" => "workmem",
        "user_md" => "user_md",
        _ => "notes",
    }
}

pub(super) fn normalize_dimension(dimension: &str) -> &'static str {
    match dimension.trim() {
        "preferences" => "preferences",
        "habits" => "habits",
        "relationships" => "relationships",
        "schedule" => "schedule",
        "facts" => "facts",
        _ => "general",
    }
}

pub(super) fn normalize_event_kind(event_kind: &str, domain: &str) -> &'static str {
    match (event_kind.trim(), domain) {
        ("accepted_candidate", "notes") => "accepted_candidate",
        ("capacity_eviction", "workmem") => "capacity_eviction",
        _ => "explicit_save",
    }
}

#[derive(Deserialize)]
pub(super) struct V2StatusResponse {
    #[serde(default)]
    pub source_epoch: Option<String>,
}

#[derive(Serialize)]
struct V2Identity<'a> {
    source_core_id: &'a str,
    data_epoch: u64,
    domain: &'static str,
    record_id: &'a str,
    record_revision: u64,
}

#[derive(Serialize)]
struct V2Target<'a> {
    source_epoch: &'a str,
    group_generation: u64,
}

#[derive(Serialize)]
struct V2Visibility<'a> {
    person_id: Option<&'a str>,
    device_id: Option<&'a str>,
}

#[derive(Serialize)]
struct V2Source<'a> {
    device_id: Option<&'a str>,
    room: Option<&'a str>,
    trace_id: Option<&'a str>,
    event_id: String,
    event_kind: &'static str,
}

#[derive(Serialize)]
struct V2ObservationWire<'a> {
    identity: V2Identity<'a>,
    target: V2Target<'a>,
    visibility: V2Visibility<'a>,
    dimension: &'static str,
    source: V2Source<'a>,
    content: String,
    event_ms: u64,
}

#[derive(Serialize)]
struct V2IngestRequest<'a> {
    protocol_version: u8,
    batch_id: String,
    observations: Vec<V2ObservationWire<'a>>,
}

pub(super) fn build_v2_ingest_payload(
    source_core_id: &str,
    data_epoch: u64,
    source_epoch: &str,
    chunk: &[OutboxObservation],
) -> serde_json::Value {
    let core_id = non_empty(source_core_id).unwrap_or("hm-core");
    let observations = chunk
        .iter()
        .map(|obs| {
            let domain = normalize_domain(&obs.domain);
            let dimension = normalize_dimension(&obs.dimension);
            let event_kind = normalize_event_kind(&obs.event_kind, domain);
            let record_id = non_empty(&obs.record_id).unwrap_or(&obs.id);
            V2ObservationWire {
                identity: V2Identity {
                    source_core_id: core_id,
                    data_epoch,
                    domain,
                    record_id,
                    record_revision: obs.created_ms.max(1),
                },
                target: V2Target {
                    source_epoch,
                    group_generation: 0,
                },
                visibility: V2Visibility {
                    person_id: non_empty(&obs.scope_person),
                    device_id: non_empty(&obs.scope_device),
                },
                dimension,
                source: V2Source {
                    device_id: non_empty(&obs.source_device),
                    room: non_empty(&obs.room),
                    trace_id: non_empty(&obs.trace_id),
                    event_id: deterministic_uuid(&format!("hm-obs:{}", obs.id)),
                    event_kind,
                },
                content: truncate_observation_content(&obs.content),
                event_ms: obs.created_ms,
            }
        })
        .collect();
    serde_json::to_value(V2IngestRequest {
        protocol_version: 2,
        batch_id: next_batch_uuid(),
        observations,
    })
    .unwrap_or_default()
}

#[derive(Deserialize)]
pub(super) struct V2VisibilityWire {
    #[serde(default)]
    pub person_id: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct V2ProfileWire {
    pub id: String,
    #[serde(default)]
    pub summary: String,
    pub visibility: V2VisibilityWire,
    #[serde(default)]
    pub source_device: String,
    #[serde(default)]
    pub dimension: String,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub row_revision: u64,
    #[serde(default)]
    pub updated_ms: u64,
}

#[derive(Deserialize)]
pub(super) struct V2ProfilesEnvelope {
    #[serde(default)]
    pub source_epoch: Option<String>,
    #[serde(default)]
    pub items: Vec<V2ProfileWire>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl From<V2ProfileWire> for ProfileWire {
    fn from(v: V2ProfileWire) -> Self {
        Self {
            id: v.id,
            scope_person: v.visibility.person_id.unwrap_or_default(),
            scope_device: v.visibility.device_id.unwrap_or_default(),
            source_device: v.source_device,
            dimension: v.dimension,
            version: v.version.max(1),
            row_revision: v.row_revision,
            summary: v.summary,
            updated_ms: v.updated_ms,
        }
    }
}
