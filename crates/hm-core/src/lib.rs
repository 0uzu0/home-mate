//! Home Mate core: brain loop, task runner, tool layer, management API.
//! See docs/plan.md (roadmap) and docs/contracts-v0.md (contracts).

pub mod agent;
pub mod agent_md;
pub mod api;
pub mod arbitration;
pub mod auth;
pub mod config;
pub mod config_delivery;
pub mod cpu;
pub mod diagnostics;
pub mod execution;
pub mod fleet;
pub mod fwupdate;
pub mod gateway;
pub mod id;
pub mod interaction;
pub mod llm;
pub mod loopback;
pub mod memory;
pub mod mic;
pub mod mics;
pub mod net;
pub mod release;
pub mod reminder;
pub mod session;
pub mod shared_memory;
pub mod silero;
pub mod sovereignty;
pub mod speech;
pub mod store;
pub mod task;
pub mod tool;
pub mod trace;
pub mod user_md;
pub mod vault;
pub mod voice;
pub mod voice_events;
pub mod working_memory;

/// Version of the device protocol this core speaks with the external
/// gateway. Must match the gateway's PROTOCOL_VERSION contract tests at G1.
pub const DEVICE_PROTOCOL_VERSION: u32 = 2;

/// Lowest peer protocol version this core accepts (protocol v1 mechanism,
/// 2026-11-16: home-mate-b#8 / gateway#11). Compatibility is INTERVAL
/// intersection — `max(both min) <= min(both max)` — not equality, so a
/// mixed window (new firmware + old core) still inter-operates.
pub const DEVICE_PROTOCOL_MIN_VERSION: u32 = 0;

/// Exact protocol versions this core supports (wire 1 was never shipped).
pub const DEVICE_PROTOCOL_SUPPORTED_VERSIONS: &[u32] = &[0, 2];

/// Build identity (git short hash + `-dirty`), embedded at compile time and
/// reported by `/status.build.id`; `hm-release` compares it before calling a
/// release healthy. A literal marker also stays in `.rodata` so the pack tool
/// can read the same identity out of the binary without executing it.
pub const BUILD_ID: &str = env!("HM_CORE_BUILD");
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// In-process VAD backend this binary was built with: `silero` (default
/// feature) or `none` (`--no-default-features` / `tools/build_core.sh
/// --slim`). A release requires `silero` plus the pinned model file, so this
/// is a packaged-binary property, not a runtime preference.
pub const VAD_BACKEND: &str = env!("HM_CORE_VAD");

#[no_mangle]
pub static HM_CORE_BUILD_MARKER: [u8; 64] = build_marker();

/// Separate `.rodata` marker (`hm-core-vad <backend>` + NUL) so `hm-release
/// pack` can reject a slim core without executing it. Kept independent of the
/// `hm-core <version> <build-id>` marker: that format and its parser are
/// unchanged, and older cores simply have no VAD marker (which the packer
/// treats as "unknown backend", i.e. not shippable).
///
/// A `&str` (not a padded `[u8; N]`) on purpose: the link-time string merge
/// can drop trailing NUL *padding* and leave the marker running straight into
/// the next .rodata string, which would make a "starts with" reader accept
/// `silero0.0.1...`. The NUL here is part of the literal, not padding.
pub static HM_CORE_VAD_MARKER: &str = concat!("hm-core-vad ", env!("HM_CORE_VAD"), "\0");

const fn build_marker() -> [u8; 64] {
    let src = concat!(
        "hm-core ",
        env!("CARGO_PKG_VERSION"),
        " ",
        env!("HM_CORE_BUILD"),
        "\0"
    );
    let bytes = src.as_bytes();
    let mut out = [0u8; 64];
    let mut i = 0;
    while i < bytes.len() && i < 64 {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

static LOCK_POISON_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Total number of poisoned `Mutex` recoveries across `hm-core`.
pub fn lock_poison_total() -> u64 {
    LOCK_POISON_TOTAL.load(Ordering::Relaxed)
}

/// Lock a `std::sync::Mutex`, recovering the inner value if a previous holder
/// panicked so a single task panic cannot poison-crash the event pump.
pub(crate) fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        let count = LOCK_POISON_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            mutex = name,
            poison_total = count,
            "recovered poisoned mutex"
        );
        poisoned.into_inner()
    })
}

/// The assistant's OWN name (user decision 2026-11-15: every speaker says it
/// is the household's Home Mate — the old constant was the wake word 高达, so a
/// node woken by 你好小智 still answered "我是高达").
pub const ASSISTANT_NAME: &str = "Home Mate";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_constants() {
        assert_eq!(DEVICE_PROTOCOL_VERSION, 2);
        assert_eq!(DEVICE_PROTOCOL_SUPPORTED_VERSIONS, &[0, 2]);
        assert_eq!(ASSISTANT_NAME, "Home Mate");
    }
}

pub mod confirmation;
