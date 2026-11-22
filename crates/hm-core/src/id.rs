//! Compact time-ordered ids for sessions, tasks, traces and tool calls.
//!
//! Home-grown on purpose (no uuid/ulid dependency): monotonic within the
//! process, sortable as strings, safe for SQLite keys.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn nonce() -> u64 {
    // Captured once per process from the clock; distinguishes restarts.
    static CELL: AtomicU64 = AtomicU64::new(0);
    let v = CELL.load(Ordering::Relaxed);
    if v != 0 {
        return v;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5eed);
    CELL.store(now, Ordering::Relaxed);
    now
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate an id like `T000001a2b3c4-00007-d41d8cd9` — the leading
/// milliseconds keep string ordering aligned with generation order.
pub fn next(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{:011x}-{:05x}-{:08x}", now_ms(), n, nonce() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_increasing() {
        let a = next("T");
        let b = next("T");
        assert!(a.starts_with('T'));
        assert!(b > a, "string ordering should follow generation order");
        assert_ne!(next("S"), next("T"));
    }
}
