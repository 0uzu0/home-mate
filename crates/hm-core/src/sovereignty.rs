//! Room Sovereignty, Session Ownership & Fleet Playback Context (Wave 2).
//!
//! Separates per-room `VoiceSession` (short mic follow window, default 9s)
//! from `ContextSession` (semantic reference thread, default 240s), enforces
//! concurrent independent rooms, provides owner-scoped retreat authorization,
//! and caches lightweight event-driven `FleetPlaybackNotice` metadata to
//! suppress cross-room bleed self-wakes.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Default mic follow-up window (`VoiceSession`) in milliseconds (8–10s range).
pub const DEFAULT_VOICE_WINDOW_MS: u64 = 9_000;
/// Default semantic context retention (`ContextSession`) in milliseconds (180–300s range).
pub const DEFAULT_CONTEXT_WINDOW_MS: u64 = 240_000;
/// Post-media mic follow-up guard in milliseconds (2s after media stops).
pub const POST_MEDIA_FOLLOW_GUARD_MS: u64 = 2_000;
/// Continuity bonus (dB) for an active room's follow-up / retreat claims.
pub const SESSION_STICKY_BONUS_DB: f32 = 4.0;
/// Default turn lease duration in milliseconds.
pub const DEFAULT_LEASE_MS: u64 = 15_000;
/// Default TTL for cached `FleetPlaybackNotice` entries.
pub const DEFAULT_PLAYBACK_NOTICE_TTL_MS: u64 = 15_000;

/// Per-room session ownership record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOwner {
    pub session_id: String,
    pub room: String,
    pub owner_device: String,
    pub lease_until_ms: u64,
    pub active_voice_until_ms: u64,
    pub context_until_ms: u64,
    /// Monotonic per-owner sequence number used to order distributed peer
    /// commit/renew/release notices (`#116` / `ARB-04`).
    #[serde(default)]
    pub epoch: u64,
    /// Per-process boot identity of the Core that minted `epoch`, so peers
    /// accept fresh `epoch = 1` after an owner Core restarts while still
    /// rejecting delayed pre-restart packets.
    #[serde(default)]
    pub boot_id: String,
}

/// Wire notice exchanged over `/api/v0/fleet/owner` to replicate room session
/// ownership commits, renewals, and releases across independent Core nodes.
///
/// Uses relative TTL durations rather than raw wall-clock deadlines so
/// cross-unit wall-clock skew (#76) cannot prematurely expire or extend leases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetOwnerNotice {
    pub session_id: String,
    pub room: String,
    pub owner_device: String,
    pub epoch: u64,
    #[serde(default)]
    pub boot_id: String,
    pub active: bool,
    #[serde(default)]
    pub lease_ttl_ms: u64,
    #[serde(default)]
    pub voice_ttl_ms: u64,
    #[serde(default)]
    pub context_ttl_ms: u64,
    #[serde(default)]
    pub clear_context: bool,
}

impl SessionOwner {
    pub fn voice_active(&self, now_ms: u64) -> bool {
        self.active_voice_until_ms > now_ms || self.lease_until_ms > now_ms
    }

    pub fn context_active(&self, now_ms: u64) -> bool {
        self.context_until_ms > now_ms
    }
}

/// Result of checking whether a device may execute a spoken retreat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetreatAuth {
    Allowed,
    DeniedNotOwner {
        owner_device: String,
        owner_room: String,
    },
}

/// Lightweight active TTS/media metadata exchanged event-driven across fleet peers
/// to suppress pure cross-room bleed self-wakes without PCM fingerprints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPlaybackNotice {
    pub device: String,
    #[serde(default)]
    pub room: String,
    /// `"voice"` | `"media"` | `"idle"`
    pub channel: String,
    pub active: bool,
    #[serde(default)]
    pub text_preview: String,
    #[serde(default)]
    pub at_ms: u64,
    #[serde(default = "default_notice_ttl")]
    pub ttl_ms: u64,
}

fn default_notice_ttl() -> u64 {
    DEFAULT_PLAYBACK_NOTICE_TTL_MS
}

impl FleetPlaybackNotice {
    pub fn is_live(&self, now_ms: u64) -> bool {
        self.active && now_ms.saturating_sub(self.at_ms) <= self.ttl_ms
    }
}

const MAX_RETIRED_BOOTS: usize = 16;

#[derive(Default)]
struct OwnerEpochTrack {
    boot_id: String,
    last_epoch: u64,
    retired_boot_ids: VecDeque<String>,
}

#[derive(Default)]
struct DomainState {
    rooms: HashMap<String, SessionOwner>,
    /// Monotonic per-device `(boot_id, epoch)` tracker so duplicate/stale
    /// notices are rejected while an owner Core restart with a fresh `boot_id`
    /// cleanly re-syncs from `epoch = 1` (`#116` / `ARB-04`).
    owner_epochs: HashMap<String, OwnerEpochTrack>,
    playback_notices: HashMap<String, FleetPlaybackNotice>,
    local_music: HashMap<String, (crate::tool::music::MusicPlaybackFact, tokio::time::Instant)>,
}

pub const MAX_TRACKED_DEVICES: usize = 64;

impl DomainState {
    fn prune_local_music(&mut self) {
        self.local_music.retain(|_, (f, received)| {
            f.active && received.elapsed().as_millis() < f.ttl_ms as u128
        });
        while self.local_music.len() > MAX_TRACKED_DEVICES {
            let Some(oldest_key) = self
                .local_music
                .iter()
                .min_by_key(|(_, (_, received))| *received)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.local_music.remove(&oldest_key);
        }
    }

    fn cap_owner_epochs(&mut self, protect_device: &str) {
        while self.owner_epochs.len() > MAX_TRACKED_DEVICES {
            let candidate = self
                .owner_epochs
                .iter()
                .filter(|(dev, _)| {
                    *dev != protect_device && !self.rooms.values().any(|o| &o.owner_device == *dev)
                })
                .min_by_key(|(_, track)| track.last_epoch)
                .or_else(|| {
                    self.owner_epochs
                        .iter()
                        .filter(|(dev, _)| *dev != protect_device)
                        .min_by_key(|(_, track)| track.last_epoch)
                })
                .map(|(k, _)| k.clone());
            let Some(victim) = candidate else {
                break;
            };
            self.owner_epochs.remove(&victim);
        }
    }
}

/// Thread-safe registry of room session sovereignty and peer playback notices.
pub struct RoomSessionDomain {
    boot_id: String,
    state: Mutex<DomainState>,
}

impl Default for RoomSessionDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomSessionDomain {
    pub fn new() -> Self {
        Self {
            boot_id: crate::id::next("rboot"),
            state: Mutex::new(DomainState::default()),
        }
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Normalize a room key; unroomed devices get a device-isolated domain
    /// (`dev:<device>`) so two unroomed devices never swallow each other.
    pub fn normalize_room(room: &str, device: &str) -> String {
        let trimmed = room.trim();
        if trimmed.is_empty() {
            format!("dev:{device}")
        } else {
            trimmed.to_string()
        }
    }

    fn next_epoch_locked(
        epochs: &mut HashMap<String, OwnerEpochTrack>,
        device: &str,
        boot_id: &str,
    ) -> u64 {
        let track = epochs.entry(device.to_string()).or_default();
        if track.boot_id != boot_id {
            if !track.boot_id.is_empty() {
                track.retired_boot_ids.push_back(track.boot_id.clone());
                while track.retired_boot_ids.len() > MAX_RETIRED_BOOTS {
                    track.retired_boot_ids.pop_front();
                }
            }
            track.boot_id = boot_id.to_string();
            track.last_epoch = 0;
        }
        track.last_epoch = track.last_epoch.saturating_add(1);
        track.last_epoch
    }

    /// Build a relative-TTL `FleetOwnerNotice` from an active `SessionOwner`.
    pub fn notice_from_owner(owner: &SessionOwner, now_ms: u64) -> FleetOwnerNotice {
        FleetOwnerNotice {
            session_id: owner.session_id.clone(),
            room: owner.room.clone(),
            owner_device: owner.owner_device.clone(),
            epoch: owner.epoch,
            boot_id: owner.boot_id.clone(),
            active: true,
            lease_ttl_ms: owner.lease_until_ms.saturating_sub(now_ms),
            voice_ttl_ms: owner.active_voice_until_ms.saturating_sub(now_ms),
            context_ttl_ms: owner.context_until_ms.saturating_sub(now_ms),
            clear_context: false,
        }
    }

    /// Claim or renew session ownership for `(room, device)`.
    ///
    /// Concurrent rooms maintain completely independent `SessionOwner` entries.
    pub fn claim_or_renew(
        &self,
        room: &str,
        device: &str,
        now_ms: u64,
        voice_window_ms: u64,
        context_window_ms: u64,
    ) -> SessionOwner {
        let key = Self::normalize_room(room, device);
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        let epoch = Self::next_epoch_locked(&mut st.owner_epochs, device, &self.boot_id);
        st.cap_owner_epochs(device);
        let voice_until = now_ms.saturating_add(voice_window_ms);
        let context_until = now_ms.saturating_add(context_window_ms);
        let lease_until = now_ms.saturating_add(DEFAULT_LEASE_MS.max(voice_window_ms));

        let owner = match st.rooms.get_mut(&key) {
            Some(existing) if existing.context_active(now_ms) => {
                existing.owner_device = device.to_string();
                existing.lease_until_ms = existing.lease_until_ms.max(lease_until);
                existing.active_voice_until_ms = existing.active_voice_until_ms.max(voice_until);
                existing.context_until_ms = existing.context_until_ms.max(context_until);
                existing.epoch = epoch;
                existing.boot_id = self.boot_id.clone();
                existing.clone()
            }
            _ => {
                let created = SessionOwner {
                    session_id: crate::id::next("rsess"),
                    room: key.clone(),
                    owner_device: device.to_string(),
                    lease_until_ms: lease_until,
                    active_voice_until_ms: voice_until,
                    context_until_ms: context_until,
                    epoch,
                    boot_id: self.boot_id.clone(),
                };
                st.rooms.insert(key, created.clone());
                created
            }
        };
        owner
    }

    /// Extend the active voice window and context thread when a device answers or
    /// processes a turn in `room`, returning the updated `SessionOwner` if owned.
    pub fn renew_voice_window(
        &self,
        room: &str,
        device: &str,
        now_ms: u64,
        voice_window_ms: u64,
        context_window_ms: u64,
    ) -> Option<SessionOwner> {
        let key = Self::normalize_room(room, device);
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        let is_owner = st
            .rooms
            .get(&key)
            .is_some_and(|existing| existing.owner_device == device);
        if !is_owner {
            return None;
        }
        let epoch = Self::next_epoch_locked(&mut st.owner_epochs, device, &self.boot_id);
        let existing = st.rooms.get_mut(&key)?;
        existing.lease_until_ms = existing
            .lease_until_ms
            .max(now_ms.saturating_add(DEFAULT_LEASE_MS.max(voice_window_ms)));
        existing.active_voice_until_ms = existing
            .active_voice_until_ms
            .max(now_ms.saturating_add(voice_window_ms));
        existing.context_until_ms = existing
            .context_until_ms
            .max(now_ms.saturating_add(context_window_ms));
        existing.epoch = epoch;
        existing.boot_id = self.boot_id.clone();
        Some(existing.clone())
    }

    /// Close the short `VoiceSession` window for `(room, device)` while retaining
    /// `ContextSession` until `context_until_ms` (unless `clear_context` is true).
    /// Returns a `FleetOwnerNotice` for peer replication when an active entry changed.
    pub fn close_voice_session(
        &self,
        room: &str,
        device: &str,
        now_ms: u64,
        clear_context: bool,
    ) -> Option<FleetOwnerNotice> {
        let key = Self::normalize_room(room, device);
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        let is_owner = st
            .rooms
            .get(&key)
            .is_some_and(|existing| existing.owner_device == device);
        if !is_owner {
            return None;
        }
        let epoch = Self::next_epoch_locked(&mut st.owner_epochs, device, &self.boot_id);
        let existing = st.rooms.get_mut(&key)?;
        existing.active_voice_until_ms = now_ms;
        existing.lease_until_ms = now_ms;
        existing.epoch = epoch;
        existing.boot_id = self.boot_id.clone();
        if clear_context {
            existing.context_until_ms = now_ms;
        }
        Some(FleetOwnerNotice {
            session_id: existing.session_id.clone(),
            room: existing.room.clone(),
            owner_device: existing.owner_device.clone(),
            epoch,
            boot_id: self.boot_id.clone(),
            active: false,
            lease_ttl_ms: 0,
            voice_ttl_ms: 0,
            context_ttl_ms: existing.context_until_ms.saturating_sub(now_ms),
            clear_context,
        })
    }

    /// Apply a replicated peer `FleetOwnerNotice` using local wall-clock time
    /// (`local_now_ms`) and monotonic `(boot_id, epoch)` fencing (`#116` / `ARB-04`).
    ///
    /// - Same `boot_id`: requires `notice.epoch > last_epoch`.
    /// - Fresh unseen non-empty `boot_id` (owner Core restarted): retires the
    ///   previous `boot_id` and accepts `notice.epoch >= 1`.
    /// - Retired `boot_id`: rejected so delayed pre-restart packets cannot
    ///   overwrite post-restart state.
    pub fn apply_remote_owner(&self, notice: FleetOwnerNotice, local_now_ms: u64) -> bool {
        if notice.owner_device.trim().is_empty() || notice.epoch == 0 {
            return false;
        }
        let key = Self::normalize_room(&notice.room, &notice.owner_device);
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        let track = st
            .owner_epochs
            .entry(notice.owner_device.clone())
            .or_default();
        let incoming_boot = notice.boot_id.trim();
        if !incoming_boot.is_empty() {
            if track.retired_boot_ids.iter().any(|b| b == incoming_boot) {
                return false;
            }
            if track.boot_id != incoming_boot {
                if !track.boot_id.is_empty() {
                    track.retired_boot_ids.push_back(track.boot_id.clone());
                    while track.retired_boot_ids.len() > MAX_RETIRED_BOOTS {
                        track.retired_boot_ids.pop_front();
                    }
                }
                track.boot_id = incoming_boot.to_string();
                track.last_epoch = notice.epoch;
            } else {
                if notice.epoch <= track.last_epoch {
                    return false;
                }
                track.last_epoch = notice.epoch;
            }
        } else {
            if notice.epoch <= track.last_epoch {
                return false;
            }
            track.last_epoch = notice.epoch;
        }
        st.cap_owner_epochs(&notice.owner_device);

        if !notice.active {
            if let Some(existing) = st.rooms.get_mut(&key) {
                if existing.owner_device == notice.owner_device {
                    existing.active_voice_until_ms = local_now_ms;
                    existing.lease_until_ms = local_now_ms;
                    existing.epoch = notice.epoch;
                    existing.boot_id = notice.boot_id;
                    if notice.clear_context {
                        existing.context_until_ms = local_now_ms;
                    }
                }
            }
            return true;
        }

        let owner = SessionOwner {
            session_id: if notice.session_id.trim().is_empty() {
                crate::id::next("rsess")
            } else {
                notice.session_id
            },
            room: key.clone(),
            owner_device: notice.owner_device,
            lease_until_ms: local_now_ms.saturating_add(notice.lease_ttl_ms),
            active_voice_until_ms: local_now_ms.saturating_add(notice.voice_ttl_ms),
            context_until_ms: local_now_ms.saturating_add(notice.context_ttl_ms),
            epoch: notice.epoch,
            boot_id: notice.boot_id,
        };
        st.rooms.insert(key, owner);
        true
    }

    /// Whether `device` currently holds an active voice window in `room` (for
    /// sticky room preference and passive ASR resolution).
    pub fn has_active_voice_window(&self, room: &str, device: &str, now_ms: u64) -> bool {
        let key = Self::normalize_room(room, device);
        let st = crate::lock_or_recover(&self.state, "sovereignty.state");
        st.rooms
            .get(&key)
            .is_some_and(|o| o.owner_device == device && o.voice_active(now_ms))
    }

    /// Return the active `SessionOwner` for `room` if its context or voice is live.
    pub fn owner_for_room(&self, room: &str, device: &str, now_ms: u64) -> Option<SessionOwner> {
        let key = Self::normalize_room(room, device);
        let st = crate::lock_or_recover(&self.state, "sovereignty.state");
        st.rooms
            .get(&key)
            .filter(|o| o.voice_active(now_ms) || o.context_active(now_ms))
            .cloned()
    }

    /// Check whether `device` in `room` is authorized to execute a spoken retreat.
    ///
    /// - Explicitly room-addressed requests (e.g. `"客厅退下吧"`) are always allowed.
    /// - If `device` owns an active voice session in its own room, allowed.
    /// - If another device owns an active voice session in the same room OR
    ///   another room has an active voice session while `device` has none,
    ///   retreat is denied (`DeniedNotOwner`) so trailing `"退下吧"` bleed
    ///   never triggers a non-owner device to speak `retreat_ack`.
    /// - If no device has an active voice session, allowed (solo/idle/paused-media retreat).
    pub fn can_retreat(
        &self,
        room: &str,
        device: &str,
        explicit_room_addressed: bool,
        now_ms: u64,
    ) -> RetreatAuth {
        if explicit_room_addressed {
            return RetreatAuth::Allowed;
        }
        let key = Self::normalize_room(room, device);
        let st = crate::lock_or_recover(&self.state, "sovereignty.state");

        if let Some(room_owner) = st.rooms.get(&key) {
            if room_owner.voice_active(now_ms) {
                if room_owner.owner_device == device {
                    return RetreatAuth::Allowed;
                }
                return RetreatAuth::DeniedNotOwner {
                    owner_device: room_owner.owner_device.clone(),
                    owner_room: room_owner.room.clone(),
                };
            }
        }

        // Device does not own an active voice session in its room. If any other
        // device currently holds an active voice session, treat this as trailing
        // cross-room bleed unless explicitly room-addressed.
        if let Some(other) = st
            .rooms
            .values()
            .find(|o| o.owner_device != device && o.voice_active(now_ms))
        {
            return RetreatAuth::DeniedNotOwner {
                owner_device: other.owner_device.clone(),
                owner_room: other.room.clone(),
            };
        }

        RetreatAuth::Allowed
    }

    /// Record or update a peer's `FleetPlaybackNotice` (event-driven).
    pub fn record_playback_notice(&self, notice: FleetPlaybackNotice) {
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        if !notice.active {
            st.playback_notices.remove(&notice.device);
        } else {
            st.playback_notices.insert(notice.device.clone(), notice);
        }
    }

    /// Snapshot of currently live peer playback notices (pruning expired entries).
    pub fn live_playback_notices(&self, now_ms: u64) -> Vec<FleetPlaybackNotice> {
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        st.playback_notices.retain(|_, n| n.is_live(now_ms));
        let mut notices: Vec<_> = st.playback_notices.values().cloned().collect();
        notices.sort_by(|a, b| a.device.cmp(&b.device));
        notices
    }

    /// Local facts come only from MusicRuntime's registered device lifecycle;
    /// this path grants no remote peer ownership or room identity.
    pub(crate) fn record_local_music(&self, fact: crate::tool::music::MusicPlaybackFact) {
        let mut state = crate::lock_or_recover(&self.state, "sovereignty.state");
        state.prune_local_music();
        if state
            .local_music
            .get(&fact.device)
            .is_some_and(|(old, _)| old.sequence >= fact.sequence)
        {
            return;
        }
        if !fact.active {
            state.local_music.remove(&fact.device);
            return;
        }
        state
            .local_music
            .insert(fact.device.clone(), (fact, tokio::time::Instant::now()));
        state.prune_local_music();
    }

    pub fn local_music_snapshot(&self) -> Vec<crate::tool::music::MusicPlaybackFact> {
        let mut state = crate::lock_or_recover(&self.state, "sovereignty.state");
        state.prune_local_music();
        let mut facts: Vec<_> = state.local_music.values().map(|(f, _)| f.clone()).collect();
        facts.sort_by(|a, b| a.device.cmp(&b.device));
        facts
    }

    /// Clear all in-memory room sessions, playback notices, and local music facts.
    pub fn clear(&self) {
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        st.rooms.clear();
        st.owner_epochs.clear();
        st.playback_notices.clear();
        st.local_music.clear();
    }

    /// Check if a local wake/keyword hit on `device` is likely pure cross-room
    /// bleed from a peer's active `FleetPlaybackNotice`.
    ///
    /// Returns the matching peer device id if suppressed.
    pub fn suppress_bleed_for_phrase(
        &self,
        device: &str,
        phrase: &str,
        self_heard: bool,
        has_strong_near_end: bool,
        now_ms: u64,
    ) -> Option<FleetPlaybackNotice> {
        let st = crate::lock_or_recover(&self.state, "sovereignty.state");
        let norm_phrase: String = phrase
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
            .collect();
        for notice in st.playback_notices.values() {
            if notice.device == device || !notice.is_live(now_ms) {
                continue;
            }
            if !has_strong_near_end && notice.channel == "voice" && !norm_phrase.is_empty() {
                let norm_preview: String = notice
                    .text_preview
                    .chars()
                    .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
                    .collect();
                if norm_preview.contains(&norm_phrase) {
                    return Some(notice.clone());
                }
            }
            if self_heard && !has_strong_near_end {
                return Some(notice.clone());
            }
        }
        if self_heard && !has_strong_near_end {
            if let Some((fact, _)) = st.local_music.values().find(|(f, received)| {
                f.device != device && f.active && received.elapsed().as_millis() < f.ttl_ms as u128
            }) {
                return Some(FleetPlaybackNotice {
                    device: fact.device.clone(),
                    room: String::new(),
                    channel: "media".into(),
                    active: true,
                    text_preview: String::new(),
                    at_ms: now_ms,
                    ttl_ms: fact.ttl_ms,
                });
            }
        }
        None
    }

    /// Snapshot of all currently active `SessionOwner`s (for `/api/v0/mode` and diagnostics).
    pub fn snapshot(&self, now_ms: u64) -> Vec<SessionOwner> {
        let mut st = crate::lock_or_recover(&self.state, "sovereignty.state");
        st.rooms
            .retain(|_, o| o.voice_active(now_ms) || o.context_active(now_ms));
        st.playback_notices.retain(|_, n| n.is_live(now_ms));
        let mut out: Vec<SessionOwner> = st.rooms.values().cloned().collect();
        out.sort_by(|a, b| a.room.cmp(&b.room));
        out
    }
}

/// True when `text` explicitly names `room` (e.g. `"客厅退下吧"`).
pub fn is_explicit_room_addressed(text: &str, room: &str) -> bool {
    let r = room.trim();
    !r.is_empty() && text.contains(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_independent_rooms_never_swallow_each_other() {
        let domain = RoomSessionDomain::new();
        let now = 10_000;
        let lr = domain.claim_or_renew("客厅", "dot2-a", now, 9_000, 240_000);
        let br = domain.claim_or_renew("卧室", "dot2-c", now + 3_000, 9_000, 240_000);
        assert_ne!(lr.session_id, br.session_id);
        assert!(domain.has_active_voice_window("客厅", "dot2-a", now + 4_000));
        assert!(domain.has_active_voice_window("卧室", "dot2-c", now + 4_000));
        assert_eq!(domain.snapshot(now + 4_000).len(), 2);
    }

    #[test]
    fn owner_scoped_retreat_blocks_non_owner_and_allows_owner_or_explicit_room() {
        let domain = RoomSessionDomain::new();
        let now = 20_000;
        domain.claim_or_renew("客厅", "dot2-a", now, 9_000, 240_000);

        // Owner is allowed.
        assert_eq!(
            domain.can_retreat("客厅", "dot2-a", false, now + 1_000),
            RetreatAuth::Allowed
        );
        // Secondary speaker in same room or hallway without own session is denied.
        assert!(matches!(
            domain.can_retreat("走廊", "dot2-b", false, now + 1_000),
            RetreatAuth::DeniedNotOwner { ref owner_device, .. } if owner_device == "dot2-a"
        ));
        // Explicit room-addressed retreat is allowed.
        assert_eq!(
            domain.can_retreat("走廊", "dot2-b", true, now + 1_000),
            RetreatAuth::Allowed
        );
        // Concurrent owner in bedroom is allowed for its own room.
        domain.claim_or_renew("卧室", "dot2-c", now + 500, 9_000, 240_000);
        assert_eq!(
            domain.can_retreat("卧室", "dot2-c", false, now + 1_000),
            RetreatAuth::Allowed
        );
    }

    #[test]
    fn voice_session_closes_while_context_session_persists() {
        let domain = RoomSessionDomain::new();
        let now = 50_000;
        let first = domain.claim_or_renew("书房", "dot2-d", now, 9_000, 240_000);
        domain.close_voice_session("书房", "dot2-d", now + 9_000, false);

        assert!(!domain.has_active_voice_window("书房", "dot2-d", now + 9_500));
        let retained = domain
            .owner_for_room("书房", "dot2-d", now + 60_000)
            .unwrap();
        assert_eq!(retained.session_id, first.session_id);
        assert!(retained.context_active(now + 60_000));

        // Re-waking within 240s reuses the same ContextSession id.
        let renewed = domain.claim_or_renew("书房", "dot2-d", now + 60_000, 9_000, 240_000);
        assert_eq!(renewed.session_id, first.session_id);
    }

    #[test]
    fn fleet_playback_notice_suppresses_cross_room_bleed() {
        let domain = RoomSessionDomain::new();
        let now = 100_000;
        domain.record_playback_notice(FleetPlaybackNotice {
            device: "dot2-a".into(),
            room: "客厅".into(),
            channel: "voice".into(),
            active: true,
            text_preview: "你可以说你好高达来唤醒我".into(),
            at_ms: now,
            ttl_ms: 15_000,
        });

        let hit = domain.suppress_bleed_for_phrase("dot2-b", "你好高达", false, false, now + 1_000);
        assert!(hit.is_some());
        // Does not suppress the speaking device itself.
        assert!(domain
            .suppress_bleed_for_phrase("dot2-a", "你好高达", false, false, now + 1_000)
            .is_none());
    }

    #[test]
    fn multi_node_tables_prune_and_cap_and_cold_clock_offset_is_clamped_241() {
        let domain = RoomSessionDomain::new();
        let now = 200_000;
        for i in 0..100 {
            let dev = format!("peer-{i:03}");
            domain.apply_remote_owner(
                FleetOwnerNotice {
                    session_id: format!("s-{i}"),
                    room: String::new(),
                    owner_device: dev.clone(),
                    epoch: 1,
                    boot_id: format!("boot-{i}"),
                    active: false,
                    lease_ttl_ms: 0,
                    voice_ttl_ms: 0,
                    context_ttl_ms: 0,
                    clear_context: true,
                },
                now,
            );
            domain.record_local_music(crate::tool::music::MusicPlaybackFact {
                device: dev,
                play_id: format!("p-{i}"),
                connection_generation: 1,
                sequence: 1,
                active: true,
                ttl_ms: 60_000,
            });
        }
        {
            let st = domain.state.lock().unwrap();
            assert!(
                st.owner_epochs.len() <= MAX_TRACKED_DEVICES,
                "owner_epochs must be capped at {MAX_TRACKED_DEVICES}, got {}",
                st.owner_epochs.len()
            );
            assert!(
                st.local_music.len() <= MAX_TRACKED_DEVICES,
                "local_music must be capped at {MAX_TRACKED_DEVICES}, got {}",
                st.local_music.len()
            );
        }
        // Inactive music fact immediately removes entry from local_music.
        let sample_key = domain
            .state
            .lock()
            .unwrap()
            .local_music
            .keys()
            .next()
            .unwrap()
            .clone();
        domain.record_local_music(crate::tool::music::MusicPlaybackFact {
            device: sample_key.clone(),
            play_id: "p-retire".into(),
            connection_generation: 1,
            sequence: 2,
            active: false,
            ttl_ms: 3_000,
        });
        assert!(!domain
            .state
            .lock()
            .unwrap()
            .local_music
            .contains_key(&sample_key));

        // Arbitration peer_clocks cap & cold_offset clamping (#241).
        let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
        let arbiter = crate::arbitration::Arbiter::new(store);
        for i in 0..100 {
            arbiter.record_peer_clock_for_test(&format!("clock-peer-{i:03}"), 1000, 1000, 10);
        }
        assert!(
            arbiter.peer_clock_count_for_test() <= crate::arbitration::MAX_PEER_CLOCKS,
            "peer_clocks must be capped at {}",
            crate::arbitration::MAX_PEER_CLOCKS
        );

        let now_srv = crate::id::now_ms() as i64;
        arbiter.record_passive_presence(
            crate::arbitration::Claim::new("dot2-a", now_srv, Some(-25.0)).with_calibrated(true),
        );
        // Uncalibrated peer sends extreme +10 hour sent_at_ms skew; clamping to
        // MAX_COLD_CLOCK_OFFSET_MS (60_000 ms) bounds the normalized heard_ms shift.
        let mut skewed = crate::arbitration::Claim::new(
            "dot2-skew",
            now_srv + crate::arbitration::MAX_COLD_CLOCK_OFFSET_MS,
            Some(-15.0),
        )
        .with_calibrated(true);
        skewed.sent_at_ms = Some(now_srv + 36_000_000);
        let _ = arbiter.inbound(skewed);
        assert!(
            arbiter.won_recently("dot2-skew", std::time::Duration::from_secs(5))
                || !arbiter.won_recently("dot2-a", std::time::Duration::from_secs(5)),
            "clamped cold_offset must align raw_heard_ms = now + 60s back to ~now"
        );
    }
}
