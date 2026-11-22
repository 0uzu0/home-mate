//! Optional last-reported transport facts. These grant no execution authority.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NorthboundStatus {
    pub schema: u32,
    pub boot_id: String,
    pub link_generation: u64,
    pub sampled_uptime_ms: u64,
    pub critical_current_count: u64,
    pub critical_high_water_count: u64,
    pub critical_current_bytes: u64,
    pub critical_high_water_bytes: u64,
    pub health_coalesced: u64,
    pub presence_coalesced: u64,
    pub critical_full: u64,
    pub invalid_frames: u64,
    pub audio_dropped: u64,
    pub audio_current_count: u64,
    pub audio_high_water_count: u64,
    pub writer_timeouts: u64,
    pub writer_errors: u64,
    pub transport_disconnects: u64,
    pub oldest_pending_ms: u64,
    pub oldest_pending_high_water_ms: u64,
    pub successful_write_bytes: u64,
    pub last_fault: Option<NorthboundFault>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NorthboundFault {
    pub code: NorthboundFaultCode,
    pub uptime_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NorthboundFaultCode {
    CriticalCountFull,
    CriticalBytesFull,
    InvalidSource,
    OversizedFrame,
    EncodeFailed,
    InvalidFrame,
    WriterTimeout,
    WriterError,
    WriterPanic,
    ReaderError,
    PeerClosed,
    DecodeError,
    ProtocolMismatch,
}

#[derive(Debug, Clone)]
pub(super) struct NorthboundRecord {
    state: &'static str,
    schema: Option<u64>,
    received_ms: u64,
    connection_generation: u64,
    sample: Option<NorthboundStatus>,
}

impl NorthboundRecord {
    pub(super) fn new(wire: Option<Value>, generation: u64) -> Self {
        let schema = wire
            .as_ref()
            .and_then(|v| v.get("schema"))
            .and_then(Value::as_u64);
        let (state, sample) = match wire {
            None => ("absent", None),
            Some(_) if schema.is_some_and(|s| s != 1) => ("unsupported", None),
            Some(value) => {
                // Option fields normally accept omission; schema1 requires an
                // explicit last_fault:null, so missing facts never mean zero.
                let has_fault = value.get("last_fault").is_some();
                let parsed = serde_json::from_value::<NorthboundStatus>(value)
                    .ok()
                    .filter(|s| {
                        has_fault
                            && s.schema == 1
                            && !s.boot_id.is_empty()
                            && s.boot_id.len() <= 64
                            && s.boot_id
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                            && s.last_fault
                                .as_ref()
                                .is_none_or(|f| f.uptime_ms <= s.sampled_uptime_ms)
                    });
                if parsed.is_some() {
                    ("available", parsed)
                } else {
                    ("invalid", None)
                }
            }
        };
        Self {
            state,
            schema,
            sample,
            received_ms: crate::id::now_ms(),
            connection_generation: generation,
        }
    }

    pub(super) fn snapshot(record: Option<&Self>, generation: u64, connected: bool) -> Value {
        match record {
            Some(r) => json!({"state":r.state,"schema":r.schema,"sample":r.sample,
                "received_ms":r.received_ms,"connection_generation":r.connection_generation,
                "stale":!connected || r.connection_generation != generation}),
            None => json!({"state":"absent","schema":null,"sample":null,"received_ms":null,
                "connection_generation":null,"stale":!connected}),
        }
    }
}
