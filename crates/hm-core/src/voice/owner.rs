//! Distributed room session ownership replication and loser revocation (`#116` / `ARB-04`, `ARB-05`, `ARB-09`).

use super::*;

impl VoicePipeline {
    fn broadcast_owner_notice(&self, notice: &crate::sovereignty::FleetOwnerNotice) {
        let fleet = crate::lock_or_recover(&self.fleet, "voice.fleet")
            .as_ref()
            .and_then(|w| w.upgrade());
        if let Some(f) = fleet {
            f.broadcast_owner_lease(notice);
        }
    }

    pub fn attach_learning(&self, learning: &crate::tool::learning::LearningRuntime) {
        *crate::lock_or_recover(&self.learning, "voice.learning") = Some(learning.clone());
    }

    fn effective_window_ms(&self, device: &str, now: u64) -> (u64, u64) {
        let (base_follow, context_ms) = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            let eff = s.effective_for(device);
            (eff.follow_window_ms, eff.context_window_ms)
        };
        let eff_follow = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .map(|st| st.participation.effective_follow_window_ms(now))
            .unwrap_or(base_follow);
        (eff_follow.max(base_follow), context_ms)
    }

    pub(super) fn sync_learning_wait(&self, device: &str, now: u64) {
        let lease = crate::lock_or_recover(&self.learning, "voice.learning")
            .as_ref()
            .and_then(|l| l.active_wait_for(Some(device), now));
        let mut renew_for_learning = false;
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            match lease {
                Some(l) => {
                    st.participation
                        .set_learning_wait(Some(l.window_ms), l.until_ms);
                    renew_for_learning = st.participation.session.state == SessionState::Listening;
                }
                None => st.participation.set_learning_wait(None, 0),
            }
        }
        if renew_for_learning && self.device_in_household_acoustic_domain(device) {
            let (follow_ms, context_ms) = self.effective_window_ms(device, now);
            let room = self.room_of(device);
            if let Some(owner) = self
                .sovereignty
                .renew_voice_window(&room, device, now, follow_ms, context_ms)
            {
                let notice = crate::sovereignty::RoomSessionDomain::notice_from_owner(&owner, now);
                self.broadcast_owner_notice(&notice);
            }
        }
    }

    pub(super) fn clear_learning_wait(&self, device: &str) {
        if let Some(l) = crate::lock_or_recover(&self.learning, "voice.learning").as_ref() {
            l.clear_wait_for(Some(device));
        }
    }

    pub(super) fn claim_sovereignty(&self, device: &str, now: u64) {
        self.sync_learning_wait(device, now);
        if !self.device_in_household_acoustic_domain(device) {
            return;
        }
        let (follow_ms, context_ms) = self.effective_window_ms(device, now);
        let room = self.room_of(device);
        let owner = self
            .sovereignty
            .claim_or_renew(&room, device, now, follow_ms, context_ms);
        let notice = crate::sovereignty::RoomSessionDomain::notice_from_owner(&owner, now);
        self.broadcast_owner_notice(&notice);
    }

    pub(super) fn renew_sovereignty(&self, device: &str, now: u64) {
        self.sync_learning_wait(device, now);
        if !self.device_in_household_acoustic_domain(device) {
            return;
        }
        let (follow_ms, context_ms) = self.effective_window_ms(device, now);
        let room = self.room_of(device);
        if let Some(owner) = self
            .sovereignty
            .renew_voice_window(&room, device, now, follow_ms, context_ms)
        {
            let notice = crate::sovereignty::RoomSessionDomain::notice_from_owner(&owner, now);
            self.broadcast_owner_notice(&notice);
        }
    }

    pub(super) fn release_sovereignty(&self, device: &str, now: u64, clear_context: bool) {
        self.clear_learning_wait(device);
        if !self.device_in_household_acoustic_domain(device) {
            return;
        }
        let room = self.room_of(device);
        if let Some(notice) =
            self.sovereignty
                .close_voice_session(&room, device, now, clear_context)
        {
            self.broadcast_owner_notice(&notice);
        }
    }

    /// Synchronously revoke all loser state (`ARB-05` / `ARB-09`): clears
    /// recent-win eligibility, passive presence, room ownership lease (with
    /// peer release broadcast), active capture/input lease, and silently
    /// retreats any open session/task/play so a loser retains zero stale
    /// eligibility and a late stronger claim never leaves a double response.
    pub(super) fn revoke_arbitration_loser(&self, device: &str, winner: &str, reason: &str) {
        let now = id::now_ms();
        self.arbiter.clear_win(device);
        self.arbiter.clear_passive_presence(device);
        self.release_sovereignty(device, now, true);

        let (expected, session_open, had_capture_or_work) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            let had_input = st.input.is_some();
            if had_input {
                st.revoke_input("cancelled");
            }
            let session_open = st.participation.session.state != SessionState::Idle;
            let had_work = !st.outputs.is_empty() || st.current_task.is_some();
            if !session_open && had_work {
                let _ = self.stop_device_locked(device, st, true);
            }
            (
                st.participation.identity(),
                session_open,
                had_input || had_work,
            )
        };

        if let Some(arc) = self.mic_arc(device) {
            crate::lock_or_recover(&arc, "voice.mic.dev").reset();
        }
        self.apply_capture_revocations();

        if session_open {
            self.retreat_session_silent(device, reason, expected);
        }
        if session_open || had_capture_or_work {
            self.note(
                "arbitration_loser_revoked",
                json!({
                    "device": device,
                    "winner": winner,
                    "reason": reason,
                }),
            );
        }
    }

    pub(super) fn check_fleet_bleed(
        &self,
        device: &str,
        keyword: &str,
        self_heard: bool,
        fed_ms: Option<u64>,
    ) -> bool {
        if !self.device_in_household_acoustic_domain(device) {
            return false;
        }
        // ARB-08: require real measured NearEndPresence SNR (never `None`,
        // because `DeviceEvent::Keyword` populates `rms_dbfs` with `snr_db=None`)
        // AND calibrated speech level above typical cross-room bleed (-32 dBFS).
        const STRONG_NEAR_END_DBFS: f32 = -32.0;
        const STRONG_NEAR_END_SNR_DB: f32 = 10.0;
        let offset_db = crate::config::ConfigService::new(self.store.clone())
            .unit_acoustic_profile(device)
            .ok()
            .map(|p| p.mic_gain_offset_db)
            .unwrap_or(0.0);
        let has_strong_near_end = self.fresh_mic_level(device).is_some_and(|s| {
            !s.clipped
                && s.echo_health
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(|h| matches!(h, "" | "healthy" | "unknown" | "idle_unlocked"))
                && s.rms_dbfs
                    .is_some_and(|db| db.is_finite() && (db + offset_db) >= STRONG_NEAR_END_DBFS)
                && s.snr_db
                    .is_some_and(|snr| snr.is_finite() && snr >= STRONG_NEAR_END_SNR_DB)
        });
        if let Some(notice) = self.sovereignty.suppress_bleed_for_phrase(
            device,
            keyword,
            self_heard,
            has_strong_near_end,
            id::now_ms(),
        ) {
            self.note(
                "bleed_suppressed_by_fleet_playback",
                json!({
                    "device": device,
                    "keyword": keyword,
                    "peer_device": notice.device,
                    "peer_room": notice.room,
                    "channel": notice.channel,
                }),
            );
            self.note(
                "sovereignty_decision",
                json!({
                    "device": device,
                    "room_label_display": self.room_of(device),
                    "room_owner": notice.device,
                    "room_owner_room": notice.room,
                    "reason": "abstained_bleed",
                    "channel": notice.channel,
                }),
            );
            self.note(
                "keyword_ignored",
                json!({ "device": device, "keyword": keyword, "reason": "fleet_playback_bleed", "fed_ms": fed_ms }),
            );
            return true;
        }
        false
    }

    pub(super) fn close_window_preserving_paused_media(
        &self,
        device: &str,
        now: u64,
        identity: Option<&mut super::participation::Identity>,
    ) {
        let mut closed = false;
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            let cur = st.participation.identity();
            if st
                .participation
                .close(cur, super::participation::CloseReason::ExplicitExit, now)
            {
                st.follow_open_last_tick = false;
                st.follow_close_pending_at = None;
                st.window_opener = None;
                st.last_wake = None;
                st.wake_flash_until_ms = 0;
                closed = true;
            }
            if let Some(id_out) = identity {
                *id_out = st.participation.identity();
            }
        }
        if closed {
            self.clear_learning_wait(device);
            self.release_sovereignty(device, now, false);
        }
        self.push_assistant_state(device, false);
    }

    pub fn arbiter_won_recently_for_test(&self, device: &str, within: std::time::Duration) -> bool {
        self.arbiter.won_recently(device, within)
    }
}
