//! Household-plane API authentication (M9, 2026-11-14).
//!
//! Real bearer tokens, not a placeholder: 32 bytes from the kernel's CSPRNG,
//! handed to the console exactly once (`hm1_…`), stored only as SHA-256.
//!
//! Coverage rules (one middleware, `api::auth::require_bearer`):
//!
//! - every `/api/v0/*` path needs `Authorization: Bearer <token>` **except**
//!   - the two bootstrap entries `GET /api/v0/auth` and
//!     `POST /api/v0/auth/provision`, because a fresh device has no token and
//!     the console must be able to pair it (the bundle's static files also
//!     stay open: a browser navigation cannot carry the header);
//!   - loopback peers (`hm-release` talks to the core over 127.0.0.1 for
//!     snapshot/restart/health — the user's 2026-11-14 decision keeps that
//!     trust instead of making the installer upgrade order a hard release
//!     dependency);
//! - while no token has ever been issued the API stays open: that is the
//!   visible "unpaired" state (console setup card, `status.auth.configured`),
//!   and it ends at the first provisioning.
//!
//! One token = the household plane (user decision presented 2026-11-14,
//! default taken while unanswered). The store is a LIST so per-client pairing
//! (individual revocation) is additive: provisioning/rotation simply keep one
//! entry today.
//!
//! Recovery when the token is lost: create `<data_dir>/auth-reset` as root on
//! the device (serial shell) and restart the core; it clears the list and the
//! device returns to the unpaired state. No API path can un-pair a device.

use crate::store::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

pub const TOKEN_PREFIX: &str = "hm1_";
const CONFIG_KEY: &str = "api_tokens";
/// Sliding window and threshold for per-source failed bearer auth throttling.
pub const AUTH_FAIL_WINDOW_MS: u64 = 60_000;
pub const AUTH_FAIL_THRESHOLD: usize = 20;
/// A token's `last_used_ms` is refreshed at most this often (one SQLite write
/// per minute instead of one per console poll).
const TOUCH_INTERVAL_MS: u64 = 60_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenEntry {
    pub id: String,
    pub label: String,
    /// Lowercase hex SHA-256 of the full presented token.
    pub sha256: String,
    pub created_ms: u64,
    #[serde(default)]
    pub last_used_ms: u64,
}

/// What the console may see. Never the hash, never the token.
#[derive(Clone, Debug, Serialize)]
pub struct TokenSummary {
    pub id: String,
    pub label: String,
    pub created_ms: u64,
    pub last_used_ms: u64,
}

#[derive(Debug)]
pub enum AuthError {
    AlreadyConfigured,
    Corrupt(String),
    Store(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::AlreadyConfigured => write!(f, "already paired"),
            AuthError::Corrupt(e) => write!(f, "token store unreadable: {e}"),
            AuthError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

pub struct AuthStore {
    store: Arc<Store>,
    /// Serializes read-modify-write of the list (rotation vs `last_used`).
    write: Mutex<()>,
    /// Per-source-IP sliding window of failed auth timestamps (ms).
    failures: Mutex<HashMap<IpAddr, VecDeque<u64>>>,
}

impl AuthStore {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            store,
            write: Mutex::new(()),
            failures: Mutex::new(HashMap::new()),
        })
    }

    /// Check whether `ip` has exceeded [`AUTH_FAIL_THRESHOLD`] failures within
    /// [`AUTH_FAIL_WINDOW_MS`]. Loopback is never rate-limited.
    pub fn is_rate_limited(&self, ip: IpAddr) -> Option<u64> {
        if ip.is_loopback() {
            return None;
        }
        let now = crate::id::now_ms();
        let cutoff = now.saturating_sub(AUTH_FAIL_WINDOW_MS);
        let mut map = crate::lock_or_recover(&self.failures, "auth.failures");
        let queue = map.get_mut(&ip)?;
        while queue.front().is_some_and(|&ts| ts < cutoff) {
            queue.pop_front();
        }
        if queue.len() >= AUTH_FAIL_THRESHOLD {
            let oldest = *queue.front().unwrap_or(&now);
            let remaining_ms = oldest
                .saturating_add(AUTH_FAIL_WINDOW_MS)
                .saturating_sub(now);
            Some((remaining_ms / 1000).max(1))
        } else {
            if queue.is_empty() {
                map.remove(&ip);
            }
            None
        }
    }

    /// Record a failed auth attempt for `ip` and return the current failure
    /// count in the sliding window. Loopback is never tracked.
    pub fn record_auth_failure(&self, ip: IpAddr) -> usize {
        if ip.is_loopback() {
            return 0;
        }
        let now = crate::id::now_ms();
        let cutoff = now.saturating_sub(AUTH_FAIL_WINDOW_MS);
        let mut map = crate::lock_or_recover(&self.failures, "auth.failures");
        if map.len() > 256 {
            map.retain(|_, q| {
                while q.front().is_some_and(|&ts| ts < cutoff) {
                    q.pop_front();
                }
                !q.is_empty()
            });
        }
        let queue = map.entry(ip).or_default();
        while queue.front().is_some_and(|&ts| ts < cutoff) {
            queue.pop_front();
        }
        queue.push_back(now);
        queue.len()
    }

    /// Clear tracked failures for `ip` after a successful authentication.
    pub fn clear_auth_failures(&self, ip: IpAddr) {
        if ip.is_loopback() {
            return;
        }
        let mut map = crate::lock_or_recover(&self.failures, "auth.failures");
        map.remove(&ip);
    }

    /// Startup recovery: `<data_dir>/auth-reset` clears every token.
    /// Returns true when it fired (the caller logs it).
    pub fn apply_reset_sentinel(&self, data_dir: &std::path::Path) -> bool {
        let sentinel = data_dir.join("auth-reset");
        if !sentinel.exists() {
            return false;
        }
        let _guard = crate::lock_or_recover(&self.write, "auth.write");
        let cleared = match self.store.config_get(CONFIG_KEY) {
            Ok(Some(raw)) => serde_json::from_str::<Vec<TokenEntry>>(&raw)
                .map(|v| v.len())
                .unwrap_or(0),
            _ => 0,
        };
        if let Err(e) = self.store.config_delete(CONFIG_KEY) {
            tracing::error!(error = %e, "auth: failed to clear api_tokens on auth-reset sentinel");
            return false;
        }
        let _ = std::fs::remove_file(&sentinel);
        tracing::warn!(
            cleared,
            "auth: auth-reset sentinel found — API returned to the unpaired state"
        );
        true
    }

    /// `None` = never paired (API open). `Some([])` = paired then cleared —
    /// fail closed rather than silently reopening.
    fn raw(&self) -> Result<Option<String>, AuthError> {
        self.store
            .config_get(CONFIG_KEY)
            .map_err(|e| AuthError::Store(e.to_string()))
    }

    pub fn entries(&self) -> Result<Option<Vec<TokenEntry>>, AuthError> {
        match self.raw()? {
            None => Ok(None),
            Some(raw) => serde_json::from_str::<Vec<TokenEntry>>(&raw)
                .map(Some)
                .map_err(|e| AuthError::Corrupt(e.to_string())),
        }
    }

    pub fn configured(&self) -> bool {
        matches!(self.raw(), Ok(Some(_)))
    }

    pub fn summaries(&self) -> Vec<TokenSummary> {
        self.entries()
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|e| TokenSummary {
                id: e.id,
                label: e.label,
                created_ms: e.created_ms,
                last_used_ms: e.last_used_ms,
            })
            .collect()
    }

    /// Issue the first token. Only legal while the device was never paired:
    /// afterwards the caller must prove possession (rotation).
    pub fn provision(&self, label: &str) -> Result<String, AuthError> {
        let _guard = crate::lock_or_recover(&self.write, "auth.write");
        if self.raw()?.is_some() {
            return Err(AuthError::AlreadyConfigured);
        }
        let token = fresh_token();
        self.save(&[new_entry(&token, label)])?;
        Ok(token)
    }

    /// Replace every token with one fresh token (revocation for everyone).
    pub fn rotate(&self, label: &str) -> Result<String, AuthError> {
        let _guard = crate::lock_or_recover(&self.write, "auth.write");
        let token = fresh_token();
        self.save(&[new_entry(&token, label)])?;
        Ok(token)
    }

    /// `Some(id)` when the presented token matches a live entry.
    pub fn verify(&self, presented: &str) -> Option<String> {
        let entries = self.entries().ok().flatten()?;
        verify_in(&entries, presented)
    }

    /// Record use, at most once per minute; a rotation in between wins (the
    /// entry is gone and nothing is written).
    pub fn touch(&self, id: &str) {
        let _guard = crate::lock_or_recover(&self.write, "auth.write");
        let Ok(Some(mut entries)) = self.entries() else {
            return;
        };
        let now = crate::id::now_ms();
        let Some(e) = entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if now.saturating_sub(e.last_used_ms) < TOUCH_INTERVAL_MS {
            return;
        }
        e.last_used_ms = now;
        let _ = self.save(&entries);
    }

    fn save(&self, entries: &[TokenEntry]) -> Result<(), AuthError> {
        let raw = serde_json::to_string(entries).map_err(|e| AuthError::Store(e.to_string()))?;
        self.store
            .config_set(CONFIG_KEY, &raw)
            .map_err(|e| AuthError::Store(e.to_string()))
    }
}

/// Verify against an already-loaded list (the middleware reads the list once
/// per request, then reuses it for `touch`).
pub fn verify_in(entries: &[TokenEntry], presented: &str) -> Option<String> {
    if !presented.starts_with(TOKEN_PREFIX) || presented.len() != TOKEN_PREFIX.len() + 43 {
        return None;
    }
    let digest = hex(&Sha256::digest(presented.as_bytes()));
    let mut found = None;
    for e in entries {
        if ct_eq(digest.as_bytes(), e.sha256.as_bytes()) {
            found = Some(e.id.clone());
        }
    }
    found
}

fn new_entry(token: &str, label: &str) -> TokenEntry {
    let digest = hex(&Sha256::digest(token.as_bytes()));
    TokenEntry {
        id: digest[..16].to_string(),
        label: label.to_string(),
        sha256: digest,
        created_ms: crate::id::now_ms(),
        last_used_ms: 0,
    }
}

/// 32 bytes from the kernel CSPRNG, base64url (no padding) after `hm1_`.
fn fresh_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    // Linux/macOS: /dev/urandom always exists; a failure is fatal for the
    // caller's intent (never fall back to a predictable token).
    let mut f = std::fs::File::open("/dev/urandom").expect("open /dev/urandom for API token");
    use std::io::Read;
    f.read_exact(&mut bytes)
        .expect("read 32 random bytes for API token");
    format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Compare digests without an early exit (the values are hashes, but a
/// constant-time compare costs nothing and keeps the habit).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().unwrap())
    }

    #[test]
    fn unpaired_is_distinct_from_cleared() {
        let auth = AuthStore::new(store());
        assert!(!auth.configured());
        let token = auth.provision("family").unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert!(auth.configured());
        auth.store.config_set(CONFIG_KEY, "[]").unwrap();
        assert!(auth.configured(), "an empty list must stay fail-closed");
        assert!(auth.verify(&token).is_none());
    }

    #[test]
    fn provision_does_not_overwrite_and_rotate_revokes() {
        let auth = AuthStore::new(store());
        let first = auth.provision("family").unwrap();
        assert!(matches!(
            auth.provision("again"),
            Err(AuthError::AlreadyConfigured)
        ));
        let id = auth.verify(&first).expect("first token verifies");
        let second = auth.rotate("family").unwrap();
        assert!(
            auth.verify(&first).is_none(),
            "rotation kills the old token"
        );
        assert!(auth.verify(&second).is_some());
        assert_ne!(id, auth.verify(&second).unwrap());
        assert_eq!(auth.summaries().len(), 1);
    }

    #[test]
    fn verify_rejects_wrong_shape_and_wrong_value() {
        let auth = AuthStore::new(store());
        let token = auth.provision("family").unwrap();
        assert!(auth.verify("").is_none());
        assert!(auth.verify("hm1_short").is_none());
        let mut wrong = token.clone();
        wrong.pop();
        wrong.push(if token.ends_with('A') { 'B' } else { 'A' });
        assert!(auth.verify(&wrong).is_none());
    }

    #[test]
    fn corrupt_store_fails_closed() {
        let auth = AuthStore::new(store());
        auth.store.config_set(CONFIG_KEY, "{not json").unwrap();
        assert!(auth.configured());
        assert!(auth.entries().is_err());
        assert!(auth.verify("hm1_whatever").is_none());
    }

    #[test]
    fn reset_sentinel_clears_tokens() {
        let dir = std::env::temp_dir().join(format!("hm-auth-reset-{}", crate::id::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = AuthStore::new(store());
        let token = auth.provision("family").unwrap();
        std::fs::write(dir.join("auth-reset"), b"1").unwrap();
        assert!(auth.apply_reset_sentinel(&dir));
        assert!(auth.verify(&token).is_none());
        assert!(
            !auth.configured(),
            "reset sentinel returns store to unpaired state"
        );
        let fresh = auth
            .provision("repaired")
            .expect("re-provision must succeed after reset sentinel");
        assert!(auth.verify(&fresh).is_some());
        assert!(!dir.join("auth-reset").exists(), "sentinel consumed");
        assert!(!auth.apply_reset_sentinel(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn touch_is_throttled() {
        let auth = AuthStore::new(store());
        let token = auth.provision("family").unwrap();
        let id = auth.verify(&token).unwrap();
        auth.touch(&id);
        let first = auth.summaries()[0].last_used_ms;
        assert!(first > 0);
        auth.touch(&id);
        assert_eq!(auth.summaries()[0].last_used_ms, first);
    }

    #[test]
    fn touch_racing_rotate_never_resurrects_revoked_token() {
        use std::sync::Barrier;
        let auth = AuthStore::new(store());
        let mut current = auth.provision("family").unwrap();
        for _ in 0..32 {
            let old_token = current.clone();
            let old_id = auth.verify(&old_token).expect("current token must verify");
            let barrier = Arc::new(Barrier::new(2));
            let a1 = Arc::clone(&auth);
            let b1 = Arc::clone(&barrier);
            let id_clone = old_id.clone();
            let t_touch = std::thread::spawn(move || {
                b1.wait();
                a1.touch(&id_clone);
            });
            let a2 = Arc::clone(&auth);
            let b2 = Arc::clone(&barrier);
            let t_rotate = std::thread::spawn(move || {
                b2.wait();
                a2.rotate("family").unwrap()
            });
            t_touch.join().unwrap();
            let rotated = t_rotate.join().unwrap();
            assert!(
                auth.verify(&old_token).is_none(),
                "touch() must never overwrite a rotated token"
            );
            assert!(
                auth.verify(&rotated).is_some(),
                "rotated token must remain valid"
            );
            current = rotated;
        }
    }
}
