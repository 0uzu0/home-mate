//! Stop submission and hardware evidence are separate facts (wire v0 compatible).
use super::*;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StopOutcome {
    Confirmed {
        played_ms: u64,
        consumed_ms: Option<u64>,
        progress_basis: Option<String>,
    },
    Failed {
        reason: String,
    },
    Unknown {
        reason: String,
    },
}

impl StopOutcome {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }
    pub fn confirmed(&self) -> bool {
        matches!(self, Self::Confirmed { .. })
    }
    pub(super) fn receipt(
        stage: &str,
        detail: Option<&str>,
        played_ms: Option<u64>,
        consumed_ms: Option<u64>,
        progress_basis: Option<String>,
    ) -> Self {
        // Current firmware FlushDone emits exactly this combination. Its
        // not-found immediate receipt has no played_ms and proves no flush.
        match (stage, detail, played_ms) {
            ("device_confirmed", None, Some(played_ms)) => Self::Confirmed {
                played_ms,
                consumed_ms,
                progress_basis,
            },
            ("failed" | "flush_failed", _, _) => Self::Failed {
                reason: detail.unwrap_or(stage).into(),
            },
            (_, Some(d), _) if d.starts_with("flush_failed:") => Self::Failed { reason: d.into() },
            _ => Self::unknown(format!(
                "stop_receipt:{stage}:{}",
                detail.unwrap_or("unverified_flush")
            )),
        }
    }
}

pub enum StopSubmission {
    NoActivePlay,
    Accepted {
        stop_id: String,
        play_id: String,
        connection_generation: u64,
        receiver: oneshot::Receiver<StopOutcome>,
    },
    SendFailed {
        reason: String,
    },
}

impl StopSubmission {
    pub fn accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }
    /// Logical cancellation never upgrades even a simultaneous flush to success.
    pub async fn settle(self, bound: Duration, cancel: Option<&AtomicBool>) -> StopOutcome {
        let outcome = match self {
            Self::NoActivePlay => StopOutcome::unknown("no_active_play"),
            Self::SendFailed { reason } => StopOutcome::unknown(format!("send_failed:{reason}")),
            Self::Accepted { receiver, .. } => {
                let cancelled = async {
                    loop {
                        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancelled => StopOutcome::unknown("cancelled"),
                    r = tokio::time::timeout(bound, receiver) => match r {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(_)) => StopOutcome::unknown("receiver_dropped"),
                        Err(_) => StopOutcome::unknown("timeout"),
                    }
                }
            }
        };
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            StopOutcome::unknown("cancelled")
        } else {
            outcome
        }
    }
}
