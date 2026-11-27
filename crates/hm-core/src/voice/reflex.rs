use super::*;

const MIC_LEVEL_FRESH_MS: u64 = 5_000;

impl VoicePipeline {
    pub(super) fn room_of(&self, device: &str) -> String {
        crate::config::ConfigService::new(self.store.clone())
            .device_meta()
            .ok()
            .and_then(|m| m.get(device).map(|d| d.room.clone()))
            .unwrap_or_default()
    }

    pub(super) fn fresh_mic_level(&self, device: &str) -> Option<MicLevelSnapshot> {
        let now = id::now_ms();
        let levels = crate::lock_or_recover(&self.mic_levels, "voice.mic_levels");
        levels
            .get(device)
            .filter(|s| s.at_ms > 0 && now.saturating_sub(s.at_ms) <= MIC_LEVEL_FRESH_MS)
            .cloned()
    }

    pub(super) fn build_claim(
        &self,
        device: &str,
        heard_ms: i64,
        rms_dbfs: Option<f32>,
        word: &str,
    ) -> crate::arbitration::Claim {
        let snap = self.fresh_mic_level(device);
        let raw_rms = rms_dbfs
            .and_then(crate::gateway::valid_db)
            .or_else(|| snap.as_ref().and_then(|s| s.rms_dbfs));
        let profile = crate::config::ConfigService::new(self.store.clone())
            .unit_acoustic_profile(device)
            .ok();
        let offset_db = profile
            .as_ref()
            .map(|p| p.mic_gain_offset_db)
            .unwrap_or(0.0);
        let calibrated = profile.as_ref().is_some_and(|p| p.mic_calibrated);
        let calibrated_rms = raw_rms.map(|db| db + offset_db);
        let room = self.room_of(device);
        let sticky = self
            .sovereignty
            .has_active_voice_window(&room, device, id::now_ms());
        let mut claim = crate::arbitration::Claim::new(device, heard_ms, calibrated_rms)
            .with_calibrated(calibrated)
            .with_room(room)
            .with_word(word)
            .with_session_sticky(sticky);
        if let Some(s) = snap {
            let nf = s
                .noise_floor_dbfs
                .or_else(|| s.rms_dbfs.zip(s.snr_db).map(|(r, snr)| r - snr));
            let derived = raw_rms
                .zip(nf)
                .filter(|(r, f)| r.is_finite() && f.is_finite())
                .map(|(r, f)| (r - f).max(0.0));
            claim.snr_db = match (s.snr_db, derived) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            claim.clipped = s.clipped;
            claim.echo_health = s.echo_health;
            claim.profile_version = s.profile_version;
        } else {
            let levels = crate::lock_or_recover(&self.mic_levels, "voice.mic_levels");
            if let Some(s) = levels.get(device) {
                claim.echo_health = s.echo_health.clone();
                claim.profile_version = s.profile_version.clone();
            }
        }
        claim
    }

    /// Build the claim for a spotted wake word. `heard_ms` is `ts - lag_ms`:
    /// the wall clock at which the phrase's last audio reached this
    /// microphone. Falls back to `ts` when the firmware reports no lag — an
    /// older build, which costs a few tens of ms of accuracy, never the fact.
    pub(super) fn wake_claim_for(
        &self,
        device: &str,
        keyword: &str,
        ts: u64,
        lag_ms: Option<u64>,
        rms_dbfs: Option<f32>,
    ) -> Option<crate::arbitration::Claim> {
        if !self.device_in_household_acoustic_domain(device) {
            // Portable/mobile gateways (e.g. non-loopback `android-v2`) sit outside
            // the fixed household speaker acoustic domain: never block them on
            // household arbitration or submit their claims into the arbiter.
            return None;
        }
        if self.arbiter.peer_count() == 0 && self.hub.online_household_acoustic_device_count() <= 1
        {
            // A node alone in the house with a single online household speaker has
            // nothing to arbitrate: behave exactly as before arbitration existed, and
            // do not fill the log with decisions that could only ever be "me".
            return None;
        }
        let heard = ts.saturating_sub(lag_ms.unwrap_or(0)) as i64;
        Some(self.build_claim(device, heard, rms_dbfs, keyword))
    }

    /// Watch the open round until the utterance window closes and re-decide if
    /// a claim arrived that is stronger than ours.
    ///
    /// This is what makes "one answer" true on real hardware. Two speakers
    /// reported the SAME phrase 757 ms apart (dot2-b/dot2-c, 2026-11-15)
    /// because detection latency depends on how each microphone and model saw
    /// the phrase — so a node that decided at 200 ms is often deciding
    /// against an incomplete picture. It has already opened its window
    /// (silently); nothing has been spoken yet, because a bare wake's ack
    /// belongs to the ASR transcript that arrives a second later. Losing late
    /// therefore costs nothing audible: the window closes, the verdict is
    /// recorded as lost, and the transcript path is vetoed by
    /// [`Self::asr_wake_arbitrated_out`].
    pub(super) fn spawn_late_check(self: &Arc<Self>, id: String, me: String, device: String) {
        let pipe = self.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let window = std::time::Duration::from_millis(crate::arbitration::COALESCE_MS as u64);
            let remaining = window.saturating_sub(started.elapsed());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
            let claims = pipe.arbiter.claims_so_far(&id);
            pipe.arbiter.finish(&id);
            let local = claims.iter().find(|c| c.device == me).cloned();
            let Some(decision) = local
                .as_ref()
                .and_then(|l| crate::arbitration::decide_around(l, &claims, &[]))
            else {
                return;
            };
            if decision.winner.device == me || claims.len() < 2 {
                return;
            }
            // A late claim took it. Record the verdict first: the ASR path
            // reads it when the transcript of this utterance arrives.
            pipe.record_last_arbitration(
                &me,
                false,
                crate::id::now_ms(),
                decision.winner.device.clone(),
                false,
            );
            pipe.note(
                "arbitration_lost",
                json!({ "device": me, "late": true, "claims": claims,
                        "decision": { "why": decision.why, "winner": decision.winner.device,
                                      "winner_room": decision.winner.room,
                                      "margin_ms": decision.margin_ms, "margin_db": decision.margin_db },
                        "action": "a claim arrived after the decision: window closed silently, ack suppressed" }),
            );
            // Synchronously revoke loser state (`ARB-05` / `ARB-09`): clear
            // recent-win, release room ownership across peers, and silently
            // retreat any open session, task, or output so no double response remains.
            pipe.revoke_arbitration_loser(&device, &decision.winner.device, "arbitration_late");
        });
    }

    /// Record a per-device arbitration verdict with bounded map growth so
    /// long-running multi-gateway Cores never accumulate unbounded device keys.
    pub(super) fn record_last_arbitration(
        &self,
        device: &str,
        won: bool,
        at_ms: u64,
        winner: String,
        consumed: bool,
    ) {
        const MAX_LAST_ARBITRATION_DEVICES: usize = 64;
        const STALE_ARBITRATION_MS: u64 = 30_000;
        let mut slots = crate::lock_or_recover(&self.last_arbitration, "voice.last_arbitration");
        if slots.len() >= MAX_LAST_ARBITRATION_DEVICES && !slots.contains_key(device) {
            slots.retain(|_, (_, ts, _, _)| at_ms.saturating_sub(*ts) <= STALE_ARBITRATION_MS);
            if slots.len() >= MAX_LAST_ARBITRATION_DEVICES {
                if let Some(oldest_key) = slots
                    .iter()
                    .min_by_key(|(_, (_, ts, _, _))| *ts)
                    .map(|(k, _)| k.clone())
                {
                    slots.remove(&oldest_key);
                }
            }
        }
        slots.insert(device.to_string(), (won, at_ms, winner, consumed));
    }

    /// Did the spotter path already decide that ANOTHER speaker owns the
    /// utterance this transcript belongs to?
    ///
    /// Only in `enforce`, only within [`ASR_ARBITRATION_GRACE_MS`] of the
    /// verdict, and only when this node lost. Any other state answers "no":
    /// a node whose spotter missed the word (or a house with arbitration off)
    /// must keep waking from the transcript exactly as before.
    pub(super) fn asr_wake_arbitrated_out(&self, device: &str) -> bool {
        if !self.device_in_household_acoustic_domain(device) {
            return false;
        }
        if crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .wake_arbitration
            != WakeArbitration::Enforce
        {
            return false;
        }
        const ASR_ARBITRATION_GRACE_MS: u64 = 4000;
        let winner = {
            let mut slots =
                crate::lock_or_recover(&self.last_arbitration, "voice.last_arbitration");
            let Some((won, at_ms, winner, consumed)) = slots.get_mut(device) else {
                return false;
            };
            if *won
                || *consumed
                || crate::id::now_ms().saturating_sub(*at_ms) > ASR_ARBITRATION_GRACE_MS
            {
                return false;
            }
            // ONE transcript may be refused: the one carrying the utterance we
            // just lost. A sentence after it is a new request.
            *consumed = true;
            winner.clone()
        };
        self.note(
            "arbitration_lost",
            json!({ "device": device, "via": "asr_wake", "winner": winner,
                    "action": "the transcript of an utterance another speaker owns cannot open a session" }),
        );
        true
    }

    /// #45: Unified ASR fallback wake admission & dedup gate.
    /// Rejects if multi-room arbitration already awarded the utterance to a
    /// peer (via `last_arbitration`, an in-flight `WINDOW_MS` round, or
    /// `Arbiter`'s sliding window of recent peer claims), or if the spotter
    /// (`Keyword` wake) already fired within `REFLEX_DEDUPE_MS`.
    pub(super) async fn admit_asr_wake(&self, device: &str, wake: &str, text: &str) -> bool {
        let arbitration = crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .wake_arbitration;
        if arbitration == WakeArbitration::Enforce
            && self.device_in_household_acoustic_domain(device)
        {
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(crate::arbitration::WINDOW_MS + 50);
            while self.arbiter.is_exchanging(device) && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
            if self.asr_wake_arbitrated_out(device) {
                return false;
            }
            let recently_won = {
                let slots =
                    crate::lock_or_recover(&self.last_arbitration, "voice.last_arbitration");
                slots.get(device).is_some_and(|(won, at_ms, winner, _)| {
                    *won && winner == device
                        && id::now_ms().saturating_sub(*at_ms)
                            <= (crate::arbitration::COALESCE_MS as u64) * 2
                })
            };
            if !recently_won {
                let local = self
                    .arbiter
                    .passive_presence_for(device)
                    .unwrap_or_else(|| self.build_claim(device, id::now_ms() as i64, None, ""));
                let all = if self.arbiter.peer_count() > 0
                    || self.hub.online_household_acoustic_device_count() > 1
                {
                    let round_id = self.arbiter.begin(local.clone());
                    let (claims, _answered, _peers) = self
                        .arbiter
                        .exchange(
                            &round_id,
                            std::time::Duration::from_millis(crate::arbitration::WINDOW_MS),
                        )
                        .await;
                    self.arbiter.finish(&round_id);
                    claims
                } else {
                    let foreign = self.arbiter.recent_foreign_claims(device);
                    let mut v = Vec::with_capacity(1 + foreign.len());
                    v.push(local.clone());
                    v.extend(foreign);
                    v
                };
                if all.len() > 1 {
                    if let Some(d) = crate::arbitration::decide_around(&local, &all, &[]) {
                        if d.winner.device != device {
                            self.record_last_arbitration(
                                device,
                                false,
                                id::now_ms(),
                                d.winner.device.clone(),
                                true,
                            );
                            self.revoke_arbitration_loser(
                                device,
                                &d.winner.device,
                                "asr_wake_sliding_window",
                            );
                            self.note(
                                "arbitration_lost",
                                json!({ "device": device, "via": "asr_wake_sliding_window",
                                        "winner": d.winner.device,
                                        "action": "peer claim in sliding window owns this utterance" }),
                            );
                            return false;
                        } else {
                            self.record_last_arbitration(
                                device,
                                true,
                                id::now_ms(),
                                device.to_string(),
                                false,
                            );
                            self.arbiter.record_win(device);
                            self.claim_sovereignty(device, id::now_ms());
                        }
                    }
                }
            }
        }
        if self.reflex_deduped(device, interaction::KeywordIntent::Wake, "asr_wake") {
            return false;
        }
        self.note(
            "asr_wake_admitted",
            json!({
                "device": device,
                "wake": wake,
                "text": text,
                "wake_arbitration": format!("{arbitration:?}"),
            }),
        );
        true
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn effective_retreat_policy(&self) -> (bool, Vec<String>) {
        self.effective_retreat_policy_for("global")
    }

    pub(super) fn effective_retreat_policy_for(&self, device: &str) -> (bool, Vec<String>) {
        let (enabled, has_override, settings_words) = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            let eff = s.effective_for(device);
            let has_ov = s
                .device_overrides
                .get(device)
                .is_some_and(|ov| ov.retreat_words.is_some());
            (eff.retreat_intent, has_ov, eff.retreat_words)
        };
        if has_override {
            let words: Vec<String> = settings_words
                .into_iter()
                .map(|w| w.trim().to_string())
                .filter(|w| !w.is_empty())
                .collect();
            return (enabled, words);
        }
        let cfg = crate::config::ConfigService::new(self.store.clone())
            .reflex()
            .unwrap_or_default();
        let (mut words, _) = crate::config::reflex_intent_map(&cfg);
        let disabled: std::collections::HashSet<String> = cfg
            .words
            .iter()
            .filter(|w| !w.enabled)
            .map(|w| w.text.trim().to_string())
            .collect();
        for w in settings_words {
            let t = w.trim().to_string();
            if !t.is_empty() && !disabled.contains(&t) && !words.contains(&t) {
                words.push(t);
            }
        }
        (enabled, words)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn effective_wake_words(&self) -> Vec<String> {
        self.effective_wake_words_for("global")
    }

    pub(super) fn effective_wake_words_for(&self, device: &str) -> Vec<String> {
        let override_words = crate::lock_or_recover(&self.settings, "voice.settings")
            .device_overrides
            .get(device)
            .and_then(|ov| ov.wake_words.clone());
        if let Some(ov_words) = override_words {
            return ov_words
                .into_iter()
                .map(|w| w.trim().to_string())
                .filter(|w| !w.is_empty())
                .collect();
        }
        let cfg = crate::config::ConfigService::new(self.store.clone())
            .reflex()
            .unwrap_or_default();
        let disabled: std::collections::HashSet<String> = cfg
            .words
            .iter()
            .filter(|w| !w.enabled)
            .map(|w| w.text.trim().to_string())
            .collect();
        let mut words: Vec<String> = cfg
            .words
            .iter()
            .filter(|w| w.enabled && w.intent == crate::config::ReflexIntent::Wake)
            .map(|w| w.text.trim().to_string())
            .filter(|w| !w.is_empty())
            .collect();
        for w in crate::lock_or_recover(&self.settings, "voice.settings")
            .wake_words
            .clone()
        {
            let t = w.trim().to_string();
            if !t.is_empty() && !disabled.contains(&t) && !words.contains(&t) {
                words.push(t);
            }
        }
        if let Some(arr) = self
            .store
            .config_get("node")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("wake")?.get("phrases")?.as_array().cloned())
        {
            for w in arr
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
            {
                let t = w.trim().to_string();
                if !t.is_empty() && !words.contains(&t) {
                    words.push(t);
                }
            }
        }
        words
    }

    /// Exchange claims with the household, decide, and act on it.
    ///
    /// `enforce = false` (observe mode) answers FIRST and records what the
    /// decision would have been: the rollout step has to be inaudible.
    /// `enforce = true` answers only if this node wins; a loser records
    /// `arbitration_lost` and does nothing else — no ack, no window, no
    /// credit, no LED flash.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_arbitration(
        self: &Arc<Self>,
        claim: crate::arbitration::Claim,
        enforce: bool,
        device: String,
        keyword: String,
        ts: u64,
        fed_ms: Option<u64>,
        self_heard: bool,
        rms_dbfs: Option<f32>,
        lag_ms: Option<u64>,
        acoustics: WakeAcoustics,
        provenance: crate::gateway::InputProvenance,
    ) {
        let pipe = self.clone();
        tokio::spawn(async move {
            let me = claim.device.clone();
            let id = pipe.arbiter.begin(claim.clone());
            pipe.note(
                "wake_claim",
                json!({ "device": me, "word": claim.word, "heard_ms": claim.heard_ms,
                        "rms_dbfs": claim.rms_dbfs, "lag_ms": lag_ms, "room": claim.room,
                        "mode": if enforce { "enforce" } else { "observe" },
                        "peers": pipe.arbiter.peer_count() }),
            );
            if !enforce {
                // Unchanged behaviour: answer now, then look at the evidence.
                pipe.on_keyword_at(
                    &device,
                    &keyword,
                    ts,
                    fed_ms,
                    self_heard,
                    provenance.clone(),
                    rms_dbfs,
                    lag_ms,
                    acoustics.clone(),
                    true,
                );
            }
            let (claims, answered, peers) = pipe
                .arbiter
                .exchange(
                    &id,
                    std::time::Duration::from_millis(crate::arbitration::WINDOW_MS),
                )
                .await;
            // The round stays open for the late check below; it is the only
            // way a claim that arrives 700 ms later can still be honoured.
            // Lexicographic device id is the fixed fallback order: every node
            // computes the same one without sharing configuration.
            let decision = crate::arbitration::decide_around(&claim, &claims, &[]);
            let won = decision
                .as_ref()
                .map(|d| d.winner.device == me)
                .unwrap_or(true);
            let detail = decision.as_ref().map(|d| {
                json!({
                    "why": d.why,
                    "winner": d.winner.device,
                    "winner_room": d.winner.room,
                    "margin_ms": d.margin_ms,
                    "margin_db": d.margin_db,
                    "considered": d.considered,
                })
            });
            if !enforce {
                pipe.arbiter.finish(&id);
                pipe.note(
                    "arbitration_observed",
                    json!({ "device": me, "would_win": won, "answered": answered,
                            "peers": peers, "claims": claims, "decision": detail }),
                );
                return;
            }
            if peers > 0 && answered == 0 && claims.len() < 2 {
                pipe.arbiter.finish(&id);
                // No remote peer could be reached and no local sibling contended:
                // this node is on its own, exactly as before. Said out loud, never silent.
                pipe.note(
                    "arbitration_unavailable",
                    json!({ "device": me, "peers": peers, "why": "no peer answered the claim",
                            "action": "answered locally, single-node behaviour" }),
                );
                pipe.on_keyword_at(
                    &device, &keyword, ts, fed_ms, self_heard, provenance, rms_dbfs, lag_ms,
                    acoustics, true,
                );
                return;
            }
            // Remember the verdict for the ASR path of the SAME utterance
            // (see `last_arbitration`): a loser must not wake from the
            // transcript a second later.
            {
                let winner = decision
                    .as_ref()
                    .map(|d| d.winner.device.clone())
                    .unwrap_or_else(|| me.clone());
                pipe.record_last_arbitration(&me, won, crate::id::now_ms(), winner, false);
            }
            let (winner_dev, winner_room, sov_reason) = match decision.as_ref() {
                Some(d) => {
                    let r = match d.why {
                        crate::arbitration::Why::SessionSticky => "owner_room_sticky",
                        crate::arbitration::Why::Louder
                        | crate::arbitration::Why::LouderUncalibrated
                        | crate::arbitration::Why::First => "acoustic_winner",
                        crate::arbitration::Why::TieBreak
                        | crate::arbitration::Why::Solo
                        | crate::arbitration::Why::IncompleteFallback => "incomplete_fallback",
                    };
                    (d.winner.device.clone(), d.winner.room.clone(), r)
                }
                None => (me.clone(), pipe.room_of(&me), "incomplete_fallback"),
            };
            pipe.note(
                "sovereignty_decision",
                json!({
                    "device": me,
                    "room_label_display": pipe.room_of(&me),
                    "room_owner": winner_dev,
                    "room_owner_room": winner_room,
                    "reason": sov_reason,
                    "won": won,
                }),
            );
            if won {
                pipe.arbiter.record_win(&me);
                pipe.claim_sovereignty(&device, crate::id::now_ms());
                // Claims keep arriving for up to COALESCE_MS after ours (the
                // same phrase was measured 757 ms apart on two speakers), so
                // "won" here is provisional and `spawn_late_check` revokes if a
                // stronger peer claim arrives within COALESCE_MS.
                pipe.spawn_late_check(id.clone(), me.clone(), device.clone());
                pipe.note(
                    if decision
                        .as_ref()
                        .map(|d| {
                            d.why == crate::arbitration::Why::TieBreak
                                || d.why == crate::arbitration::Why::Solo
                        })
                        .unwrap_or(false)
                    {
                        "arbitration_tiebreak"
                    } else {
                        "arbitration_won"
                    },
                    json!({ "device": me, "answered": answered, "peers": peers,
                            "claims": claims, "decision": detail }),
                );
                pipe.on_keyword_at(
                    &device, &keyword, ts, fed_ms, self_heard, provenance, rms_dbfs, lag_ms,
                    acoustics, true,
                );
            } else {
                pipe.arbiter.finish(&id);
                pipe.revoke_arbitration_loser(&device, &winner_dev, "arbitration_initial");
                pipe.note(
                    "arbitration_lost",
                    json!({ "device": me, "answered": answered, "peers": peers,
                            "claims": claims, "decision": detail,
                            "action": "stayed silent: no ack, no window, no credit" }),
                );
            }
        });
    }

    /// D/T4 (R1g): a device-side keyword detection arrived. The gateway only
    /// reports the fact; the MATRIX (`interaction::decide`) is the policy.
    ///
    /// R1g changed the scope: the phrase used to be voice-playback-only
    /// (`media_playback_out_of_scope`, `no_voice_playback`). The user's rule
    /// is that a half-duplex link's ONE command entry is the wake word, so a
    /// keyword hit must work in every content state:
    /// - something playing (voice or media) → stop every channel + open the
    ///   window (the stop is the acknowledgement);
    /// - nothing playing → this is a wake: open the window + speak the local
    ///   ack, instead of dropping the fact.
    pub(super) fn calibrated_keyword_rms(
        &self,
        device: &str,
        rms_dbfs: Option<f32>,
    ) -> Option<f32> {
        let raw_rms = rms_dbfs.and_then(crate::gateway::valid_db)?;
        let offset_db = crate::config::ConfigService::new(self.store.clone())
            .unit_acoustic_profile(device)
            .ok()
            .map(|p| p.mic_gain_offset_db)
            .unwrap_or(0.0);
        Some(raw_rms + offset_db)
    }

    pub(super) fn retreat_in_own_live_text(&self, device: &str, keyword: &str) -> bool {
        let kw_norm = normalize_utterance(keyword);
        if kw_norm.is_empty() {
            return false;
        }
        let matches_kw = |t: &str| normalize_utterance(t).contains(&kw_norm);
        let now = crate::id::now_ms();
        crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| {
                st.outputs.values().any(|r| {
                    r.channel == "voice" && stage_is_live(&r.stage) && matches_kw(&r.answer_text)
                }) || st
                    .live_program
                    .as_ref()
                    .is_some_and(|p| matches_kw(&p.text))
                    || st
                        .last_cue
                        .as_ref()
                        .is_some_and(|(t, at)| now.saturating_sub(*at) <= 5_000 && matches_kw(t))
            })
    }

    pub(super) fn retreat_is_own_overlap(
        &self,
        device: &str,
        keyword: &str,
        p: &crate::gateway::InputProvenance,
    ) -> bool {
        use crate::gateway::InputOrigin;
        if matches!(&p.origin, InputOrigin::Legacy) {
            let now = crate::id::now_ms();
            // Serial 92: on Legacy gateway links, when the device's echo guard
            // flags `self_heard=true` while any voice output, live program, or
            // recent (<= 5 s) cue is active, suppress the retreat word outright
            // instead of requiring the spoken text to literally contain it (KWS
            // can hallucinate "退下吧" from unrelated TTS echo).
            return crate::lock_or_recover(&self.states, "voice.states")
                .get(device)
                .is_some_and(|st| {
                    st.outputs
                        .values()
                        .any(|r| r.channel == "voice" && stage_is_live(&r.stage))
                        || st.live_program.is_some()
                        || st
                            .last_cue
                            .as_ref()
                            .is_some_and(|(_, at)| now.saturating_sub(*at) <= 5_000)
                });
        }
        let kw_norm = normalize_utterance(keyword);
        let matches_kw = |t: &str| !kw_norm.is_empty() && normalize_utterance(t).contains(&kw_norm);
        let InputOrigin::Leased {
            origin,
            overlap: Some(o),
            ..
        } = &p.origin
        else {
            return false;
        };
        let InputOrigin::Audio {
            capture_epoch,
            ticket,
            start_sample,
            end_sample,
            ..
        } = &**origin
        else {
            return false;
        };
        if &o.capture_epoch != capture_epoch
            || &o.ticket != ticket
            || o.start_sample >= o.end_sample
            || start_sample >= end_sample
            || *start_sample >= o.end_sample
            || *end_sample <= o.start_sample
        {
            return false;
        }
        let states = crate::lock_or_recover(&self.states, "voice.states");
        states
            .get(device)
            .and_then(|st| {
                st.outputs.get(&o.play_id).filter(|r| {
                    r.kind == "answer"
                        && stage_is_live(&r.stage)
                        && r.request
                            .is_some_and(|t| t.generation == st.participation.generation())
                })
            })
            .is_some_and(|r| matches_kw(&r.answer_text))
    }

    /// Owner rule (2026-11-18): a retreat phrase may only act and speak `retreat_ack`
    /// when the device has an active session, an in-flight turn, or active/paused playback.
    pub(super) fn has_active_session_or_playback(&self, device: &str) -> bool {
        let now = id::now_ms();
        if self.live_content(device).any() || self.music().is_some_and(|m| m.media_present(device))
        {
            return true;
        }
        let states = crate::lock_or_recover(&self.states, "voice.states");
        states.get(device).is_some_and(|st| {
            st.session_open(now)
                || st.participation.session.state == SessionState::Listening
                || st.participation.request_running()
                || st.current_task.is_some()
                || st.live_program.is_some()
                || st.paused_program.is_some()
                || st.outputs.values().any(|o| stage_is_live(&o.stage))
                || st
                    .local_stop
                    .as_ref()
                    .is_some_and(|(_, at)| now.saturating_sub(*at) <= LOCAL_STOP_TRUST_MS)
        })
    }

    #[cfg(test)]
    pub(super) fn on_keyword(
        self: &Arc<Self>,
        device: &str,
        keyword: &str,
        ts: u64,
        fed_ms: Option<u64>,
        self_heard: bool,
        provenance: crate::gateway::InputProvenance,
    ) {
        self.on_keyword_at(
            device,
            keyword,
            ts,
            fed_ms,
            self_heard,
            provenance,
            None,
            None,
            WakeAcoustics::default(),
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_keyword_at(
        self: &Arc<Self>,
        device: &str,
        keyword: &str,
        ts: u64,
        fed_ms: Option<u64>,
        self_heard: bool,
        provenance: crate::gateway::InputProvenance,
        rms_dbfs: Option<f32>,
        lag_ms: Option<u64>,
        mut acoustics: WakeAcoustics,
        arbitrated: bool,
    ) {
        if !arbitrated {
            if self.check_fleet_bleed(device, keyword, self_heard, fed_ms) {
                return;
            }
            self.note("keyword_source", json!({ "device": device, "keyword": keyword, "ts": ts, "fed_ms": fed_ms, "provenance": provenance, "rms_dbfs": rms_dbfs, "lag_ms": lag_ms, "self_heard": self_heard, "kws_tier": acoustics.tier, "wake_dur_ms": acoustics.wake_dur_ms, "max_gap_ms": acoustics.max_gap_ms, "l2_passed": acoustics.l2_passed }));
        }
        let Some(identity) = self.source_identity(device, &provenance, "keyword") else {
            return;
        };
        if self.reflex_intent_for(device, keyword) == Some(interaction::KeywordIntent::Retreat) {
            let suppress_own_retreat =
                if matches!(&provenance.origin, crate::gateway::InputOrigin::Legacy) {
                    let cal_rms = self.calibrated_keyword_rms(device, rms_dbfs);
                    let in_own_text = self.retreat_in_own_live_text(device, keyword);
                    (in_own_text && cal_rms.is_none_or(|db| db < -26.0))
                        || (self_heard
                            && self.retreat_is_own_overlap(device, keyword, &provenance)
                            && cal_rms.is_none_or(|db| db < -26.0))
                } else {
                    self_heard && self.retreat_is_own_overlap(device, keyword, &provenance)
                };
            if suppress_own_retreat {
                self.note("keyword_ignored", json!({"device":device,"keyword":keyword,"reason":"self_heard_retreat_overlap"}));
                return;
            }
            if !self.has_active_session_or_playback(device) {
                self.bump_reflex(device, keyword, false);
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.keyword_ignored += 1;
                }
                self.note("keyword_ignored", json!({"device":device,"keyword":keyword,"reason":"reflex_precondition:no_session","fed_ms":fed_ms}));
                return;
            }
            // Order only: log acceptance once this retreat will actually act.
            if self_heard {
                self.note("keyword_self_heard_accepted", json!({"device":device,"keyword":keyword,"fed_ms":fed_ms,"why":"the guard suspected our speakers, but our own live text does not contain the retreat phrase"}));
            }
            self.clear_live_turn(device);
            let (listening, bookmark) = crate::lock_or_recover(&self.states, "voice.states")
                .get(device)
                .map(|s| {
                    (
                        s.participation.session.state == SessionState::Listening,
                        s.paused_program.is_some(),
                    )
                })
                .unwrap_or((false, false));
            if self.retreat_session_at(device, keyword, "keyword", identity) {
                self.bump_reflex(device, keyword, true);
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.last_reflex = Some((interaction::KeywordIntent::Retreat, id::now_ms()));
                }
                self.note("reflex_retreat", json!({"device":device,"keyword":keyword,"via":"keyword",
                    "precondition":{"listening":listening,"bookmark":bookmark,"idle":!listening && !bookmark}}));
            }
            return;
        }
        let enabled = crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .keyword_interrupt;
        let muted = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| st.muted)
            .unwrap_or(false);
        let now = id::now_ms();
        let reason = if !enabled {
            "disabled"
        } else if muted {
            "muted"
        } else {
            ""
        };
        if !reason.is_empty() {
            self.bump_reflex(device, keyword, false);
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                st.keyword_ignored += 1;
            }
            self.note(
                "keyword_ignored",
                json!({ "device": device, "keyword": keyword, "reason": reason, "fed_ms": fed_ms }),
            );
            return;
        }
        // R1j/P1: the WORD decides what a hit means. The vocabulary is one
        // configuration block compiled into the device keywords file; this is
        // the only place a spotted string is mapped to an intent. A word the
        // device still spots but the table no longer has (a stale file) is a
        // no-action fact, never a guess.
        let Some(intent) = self.reflex_intent_for(device, keyword) else {
            self.bump_reflex(device, keyword, false);
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                st.keyword_ignored += 1;
            }
            self.note(
                "keyword_ignored",
                json!({ "device": device, "keyword": keyword, "reason": "not_in_reflex_table",
                        "fed_ms": fed_ms }),
            );
            return;
        };
        // R1g: the device's echo guard is a HINT. On a half-duplex link the
        // wake word is the only interruption path, and the guard cannot tell
        // "a person said it over loud playback" from "our own answer said it"
        // — on 2026-11-12 it ate the user's wake phrase while a story played,
        // leaving no way to interrupt at all. The core CAN tell: it knows
        // whether our live program text contains the phrase right now.
        // Run BEFORE `spawn_arbitration` so a self-heard wake in our own
        // answer never broadcasts a bogus claim to the fleet.
        if self_heard {
            let in_our_own_text = crate::lock_or_recover(&self.states, "voice.states")
                .get(device)
                .and_then(|st| st.live_program.as_ref().map(|p| p.text.clone()))
                .map(|text| {
                    mic::wake_span(&text, keyword).is_some()
                        || self
                            .effective_wake_words_for(device)
                            .iter()
                            .any(|w| mic::wake_span(&text, w).is_some())
                })
                .unwrap_or(false);
            if in_our_own_text {
                self.bump_reflex(device, keyword, false);
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.keyword_ignored += 1;
                }
                self.note(
                    "keyword_ignored",
                    json!({ "device": device, "keyword": keyword,
                            "reason": "self_heard_phrase_in_our_own_answer", "fed_ms": fed_ms }),
                );
                return;
            }
            if !arbitrated {
                self.note(
                    "keyword_self_heard_accepted",
                    json!({ "device": device, "keyword": keyword, "fed_ms": fed_ms,
                            "why": "the guard suspected our speakers, but our own live text does not contain the phrase" }),
                );
            }
        }
        // Tier-3 PreWakeup acoustic gate (#261): runs BEFORE multi-room
        // arbitration so a suspect hit or idle mid-sentence false trigger
        // produces zero LED flash, zero ack, zero media pause, and zero
        // household wake claim. Orthogonal to `proactive_asr_wake`: when off,
        // the idle VAD segment stays `pending` and is skipped locally (100%
        // XiaoAi native silent drop); when on, `asr_fallback` can still rescue
        // genuine speech via ASR. Legacy gateways (`tier == None`) and active
        // playback interruptions (where residual echo lowers barge-in SNR)
        // bypass the suspect/mid-segment drop.
        if !arbitrated && intent == interaction::KeywordIntent::Wake {
            let playback_active = self.active_content(device).any()
                || self.media_state(device) == Some(crate::tool::music::MediaState::Playing);
            if acoustics.tier.as_deref() == Some("suspect") && !playback_active {
                self.bump_reflex(device, keyword, false);
                let proactive = crate::lock_or_recover(&self.settings, "voice.settings")
                    .effective_for(device)
                    .proactive_asr_wake;
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.keyword_ignored += 1;
                    st.mic_counters.kws_suspect_dropped += 1;
                }
                self.note(
                    "kws_suspect_wake_dropped",
                    json!({
                        "device": device,
                        "keyword": keyword,
                        "reason": "l2_or_duration_rejected",
                        "wake_dur_ms": acoustics.wake_dur_ms,
                        "max_gap_ms": acoustics.max_gap_ms,
                        "l2_passed": acoustics.l2_passed,
                        "proactive_asr_wake": proactive,
                    }),
                );
                return;
            }
            if acoustics.tier.as_deref() == Some("real") {
                // Capture the exact `wakeup_end` block cursor BEFORE any 200 ms
                // arbitration delay so Tier-2 timeline slicing does not drift
                // into a one-shot command spoken during arbitration.
                let cursor = self
                    .mic_arc(device)
                    .map(|arc| crate::lock_or_recover(&arc, "voice.mic.dev").current_block_cursor())
                    .unwrap_or(0);
                acoustics.wake_end_cursor = Some(cursor.saturating_sub(lag_ms.unwrap_or(0) / 80));
                let session_open_now = crate::lock_or_recover(&self.states, "voice.states")
                    .get(device)
                    .is_some_and(|st| st.session_open(now));
                if !playback_active && !session_open_now {
                    let (elapsed_ms, control_only) = self
                        .mic_arc(device)
                        .map(|arc| {
                            let p = crate::lock_or_recover(&arc, "voice.mic.dev");
                            (p.open_speech_elapsed_ms(), p.open_is_control_only())
                        })
                        .unwrap_or((0, false));
                    if !control_only {
                        let wake_span_ms = acoustics
                            .wake_dur_ms
                            .unwrap_or(700)
                            .saturating_add(lag_ms.unwrap_or(0));
                        let pre_wake_ms = elapsed_ms.saturating_sub(wake_span_ms);
                        if pre_wake_ms > 800 {
                            self.bump_reflex(device, keyword, false);
                            let proactive =
                                crate::lock_or_recover(&self.settings, "voice.settings")
                                    .effective_for(device)
                                    .proactive_asr_wake;
                            if let Some(st) =
                                crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                            {
                                st.keyword_ignored += 1;
                                st.mic_counters.kws_suspect_dropped += 1;
                            }
                            self.note(
                                "kws_suspect_wake_dropped",
                                json!({
                                    "device": device,
                                    "keyword": keyword,
                                    "reason": "mid_segment_wake_trigger",
                                    "pre_wake_speech_ms": pre_wake_ms,
                                    "open_speech_elapsed_ms": elapsed_ms,
                                    "wake_dur_ms": acoustics.wake_dur_ms,
                                    "lag_ms": lag_ms,
                                    "proactive_asr_wake": proactive,
                                }),
                            );
                            return;
                        }
                    }
                }
            }
        }
        // Wake arbitration (2026-11-15): only a WAKE is contested. A retreat
        // or a transport word is this device's own business and must never
        // wait for another room's opinion.
        if !arbitrated && intent == interaction::KeywordIntent::Wake {
            if let Some(claim) = self.wake_claim_for(device, keyword, ts, lag_ms, rms_dbfs) {
                match crate::lock_or_recover(&self.settings, "voice.settings")
                    .effective_for(device)
                    .wake_arbitration
                {
                    WakeArbitration::Off => {}
                    WakeArbitration::Observe => {
                        // Record what WOULD have happened, then answer as
                        // always: this is the rollout step that produces
                        // evidence without changing what a person hears.
                        self.spawn_arbitration(
                            claim,
                            false,
                            device.to_string(),
                            keyword.to_string(),
                            ts,
                            fed_ms,
                            self_heard,
                            rms_dbfs,
                            lag_ms,
                            acoustics,
                            provenance,
                        );
                        return; // the spawned task answers immediately
                    }
                    WakeArbitration::Enforce => {
                        self.spawn_arbitration(
                            claim,
                            true,
                            device.to_string(),
                            keyword.to_string(),
                            ts,
                            fed_ms,
                            self_heard,
                            rms_dbfs,
                            lag_ms,
                            acoustics,
                            provenance,
                        );
                        return; // the spawned task decides, then re-enters
                    }
                }
            }
        }
        let mut content = self.active_content(device);
        if self.media_state(device) == Some(crate::tool::music::MediaState::Playing) {
            content.media = true;
        }
        if intent == interaction::KeywordIntent::Wake {
            self.clear_live_turn(device);
        }
        let decision = interaction::decide(
            self.effective_duplex(device),
            Trigger::Keyword(intent),
            content,
        );
        let expected = match intent {
            interaction::KeywordIntent::Wake => decision.honor && decision.stop,
            _ => decision.intent == Some(intent),
        };
        if !expected {
            self.bump_reflex(device, keyword, false);
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                st.keyword_ignored += 1;
            }
            self.note("keyword_ignored", json!({ "device": device, "keyword": keyword, "reason": decision.reason, "fed_ms": fed_ms }));
            return;
        }
        let was_playing = content.any();
        let epoch = match intent {
            interaction::KeywordIntent::Wake => {
                // M3 / serial 91-A: pause media OUTSIDE `voice.states` and strictly BEFORE stop-all.
                let (policy, context_ms) = self.context_policy_for(device);
                if !crate::lock_or_recover(&self.states, "voice.states")
                    .get(device)
                    .is_some_and(|st| st.participation.identity() == identity)
                {
                    return;
                }
                let media_paused = if content.media {
                    self.pause_media_for_takeover(device)
                } else {
                    None
                };
                let (result, epoch, permit, bookmark, bare_wake_closed_during_arbitration) = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let Some(st) = states.get_mut(device) else {
                        return;
                    };
                    if st.participation.identity() != identity {
                        return;
                    }
                    self.adopt_live_wake_onset_if_blocked_by_old_recognition_locked(device, st);
                    let capture = self.take_wake_capture_locked(device, st, &provenance);
                    let (_, bookmark) = Self::bookmark_live_program_locked(st, now);
                    let result = self.stop_device_locked(device, st, true);
                    if was_playing {
                        st.keyword_stops += 1;
                    } else {
                        st.keyword_wakes += 1;
                    }
                    st.last_keyword_ms = now;
                    st.wake_flash_until_ms = now + WAKE_FLASH_MS;
                    let Some(_) = st.participation.activate(
                        st.participation.identity(),
                        policy,
                        CreditGrant::Full,
                        now,
                    ) else {
                        return;
                    };
                    let Some(context) =
                        self.admit_context_locked(device, st, now, context_ms, true)
                    else {
                        return;
                    };
                    self.restore_wake_capture_locked(device, st, capture, &provenance, context);
                    let mic_closed_ms = self.mic_arc(device).and_then(|arc| {
                        crate::lock_or_recover(&arc, "voice.mic.dev").last_close_ms()
                    });
                    let last_close_ms = match (st.mic_last_close_ms, mic_closed_ms) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                    let bare_wake_close_window_ms =
                        600u64.saturating_add(lag_ms.unwrap_or(0).min(800));
                    let bare_wake_closed = !content.media
                        && st.input.is_none()
                        && last_close_ms.is_some_and(|closed_ms| {
                            now.saturating_sub(closed_ms) <= bare_wake_close_window_ms
                        });
                    st.participation.opened_by = Some("keyword");
                    st.last_wake = Some((
                        "kws".into(),
                        keyword.to_string(),
                        st.participation.generation(),
                    ));
                    st.follow_close_pending_at = None;
                    st.window_opener = if content.media || bare_wake_closed {
                        None
                    } else {
                        Some("keyword")
                    };
                    // Arm Tier-2 physical timeline slicing while still holding `voice.states`
                    // (respecting `voice.states -> voice.mic -> voice.mic.dev` lock order) so
                    // the `spawn_blocking` VAD worker cannot close the segment before arming.
                    if !content.media
                        && st.input.is_some()
                        && acoustics.tier.as_deref() == Some("real")
                        && wake_timeline_slice_enabled()
                    {
                        if let Some(end_cursor) = acoustics.wake_end_cursor {
                            if let Some(arc) = self.mic_arc(device) {
                                crate::lock_or_recover(&arc, "voice.mic.dev")
                                    .arm_wake_slice_with_dur(end_cursor, acoustics.wake_dur_ms);
                            }
                        }
                    }
                    let permit = OutputPermit {
                        generation: st.participation.generation(),
                        stop_epoch: st.stop_epoch,
                        request: None,
                    };
                    (result, st.kws_epoch, permit, bookmark, bare_wake_closed)
                };
                self.claim_sovereignty(device, now);
                if !bookmark.is_null() {
                    self.note(
                        "program_bookmarked_after_interrupt",
                        json!({"device":device,"why":"keyword_stop","bookmark":bookmark}),
                    );
                }
                self.note(
                    if was_playing {
                        "keyword_stop"
                    } else {
                        "keyword_wake"
                    },
                    json!({ "device": device, "keyword": keyword, "ts": ts, "fed_ms": fed_ms,
                            "latency_ms": now.saturating_sub(ts),
                            "content": { "voice": content.voice, "media": content.media },
                            "kws_epoch": epoch, "stop": result, "ack": decision.ack,
                            "media_paused": media_paused }),
                );
                // User decision 2026-11-13 (device report: "播放的时候说你好
                // 高达，得等一秒多才说来了"): when the wake takes over MEDIA
                // the ack must NOT wait for the ASR confirmation. The music
                // stops within ~300 ms, so the silence before 「来了」 is what
                // the person reads as slowness. Likewise, when a bare wake's
                // VAD segment already closed during the 200 ms arbitration
                // exchange (`bare_wake_closed_during_arbitration`), no ASR
                // confirmation will follow, so the ack must fire now.
                if content.media {
                    let pipe = self.clone();
                    let ack_device = device.to_string();
                    tokio::spawn(async move {
                        let ack = pipe.speak_wake_ack(&ack_device, permit).await;
                        pipe.note(
                            "wake_ack_on_media_interrupt",
                            json!({ "device": ack_device, "ack": ack.label(),
                                    "why": "media was playing: the ack belongs to the takeover, not to the ASR confirmation" }),
                        );
                    });
                } else if bare_wake_closed_during_arbitration {
                    let pipe = self.clone();
                    let ack_device = device.to_string();
                    tokio::spawn(async move {
                        let ack = pipe.speak_wake_ack(&ack_device, permit).await;
                        pipe.note(
                            "wake_ack_after_keyword_window",
                            json!({ "device": ack_device, "ack": ack.label(),
                                    "why": "bare wake segment closed during arbitration exchange before session activation" }),
                        );
                    });
                }
                epoch
            }
            // Every command word (退下/暂停/继续/下一首/停止播放) runs through
            // the SAME intent executor as the transcript fallback; the note
            // kind only says which entry point fired.
            other => {
                let kind = match other {
                    interaction::KeywordIntent::Retreat => "reflex_retreat",
                    interaction::KeywordIntent::Pause => "reflex_pause",
                    interaction::KeywordIntent::Continue => "reflex_continue",
                    interaction::KeywordIntent::Next => "reflex_next",
                    interaction::KeywordIntent::Stop => "reflex_stop",
                    interaction::KeywordIntent::Wake => unreachable!("wake is its own arm"),
                };
                match self.run_reflex_intent(device, keyword, other, "keyword", Some(identity)) {
                    Err(reason) => {
                        self.bump_reflex(device, keyword, false);
                        if let Some(st) =
                            crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                        {
                            st.keyword_ignored += 1;
                        }
                        self.note(
                            "keyword_ignored",
                            json!({ "device": device, "keyword": keyword,
                                    "reason": reason, "fed_ms": fed_ms }),
                        );
                        return;
                    }
                    Ok(detail) => {
                        self.note(
                            kind,
                            merge_detail(
                                json!({ "device": device, "keyword": keyword, "ts": ts,
                                    "fed_ms": fed_ms, "via": "keyword",
                                    "latency_ms": now.saturating_sub(ts) }),
                                detail,
                            ),
                        );
                    }
                }
                crate::lock_or_recover(&self.states, "voice.states")
                    .get(device)
                    .map(|s| s.kws_epoch)
                    .unwrap_or(0)
            }
        };
        // Executed: count it per word and remember it for the ~1.5 s dedupe
        // of the same intent arriving later as an ASR transcript.
        self.bump_reflex(device, keyword, true);
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.last_reflex = Some((intent, now));
        }
        // NO ack here (R1g, device 2026-11-12): the ack is a VOICE play, and a
        // voice play closes the half-duplex mic gate — so playing it at the
        // keyword hit swallows the rest of the sentence ("你好高达，讲个故事"
        // lost 「讲个故事」: the phrase is spotted ~1 s in, the ack covers the
        // command). A bare wake still gets its "我在" from the ASR path,
        // which knows whether a command followed (`is_greeting_only(rest)`).
        let _ = epoch;
        self.push_assistant_state(device, false);
    }

    /// R1j/P1: compile the reflex vocabulary and hand it to the device. The
    /// file the device ends up with is the device's fact (receipt + hash);
    /// this only says the frame left.
    ///
    /// Resolve effective `(kws_threshold, kws_score)` for `device` using live
    /// hardware-family classification from `DeviceHub` plus per-device overrides.
    pub fn effective_kws_for(&self, device: &str) -> (f64, f64) {
        let kws_class = self.hub.kws_device_class(device);
        crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_kws_for(device, kws_class)
    }

    /// Contract v2 §2.5 (#5, 2026-11-16): the same frame also carries the
    /// words whose intent is `retreat` and a hash of the word→intent map, so
    /// the device can cut its own audio on an exit word without waiting for
    /// this core. It still reports the hit north; `close_session` remains the
    /// single authority over the session.
    pub fn deploy_reflex(&self, device: &str) -> Result<serde_json::Value, String> {
        let settings_snap = self.settings_snapshot();
        let cfg = crate::config_delivery::resolve_reflex_config_for_device(
            &self.store,
            &settings_snap,
            device,
        )?;
        let compiled = crate::config::compile_keywords(&cfg)?;
        let hash = crate::config::fnv1a_hex(compiled.as_bytes());
        let words = compiled.lines().count() as u64;
        let (retreat_words, intents_hash) = crate::config::reflex_intent_map(&cfg);
        // Contract v2 #4 (2026-11-16): the spotter's sensitivity rides every
        // deploy, so "save the slider" and "the device uses it" are one path.
        let (kws_threshold, kws_score) =
            settings_snap.effective_kws_for(device, self.hub.kws_device_class(device));
        let sent_ms = id::now_ms();
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.reflex_deploy = Some(ReflexDeploy {
                state: "sent".into(),
                hash: hash.clone(),
                sent_hash: hash.clone(),
                loaded_hash: None,
                loaded_ms: 0,
                keywords: words,
                detail: format!("compiled {words} words; awaiting the device"),
                sent_ms,
                acked_ms: 0,
                retreat_words_sent: retreat_words.len() as u32,
                local_exit: 0,
                intents_hash: intents_hash.clone(),
                intents_hash_installed: String::new(),
                local_exits: 0,
                last_local_exit: None,
                kws_threshold,
                kws_score,
                applied_threshold: 0.0,
                applied_score: 0.0,
            });
        }
        self.hub.kws_deploy(
            device,
            &compiled,
            &retreat_words,
            &intents_hash,
            Some(kws_threshold as f32),
            Some(kws_score as f32),
        )?;
        self.note(
            "kws_deploy_sent",
            json!({ "device": device, "hash": hash, "words": words,
                    "bytes": compiled.len(),
                    "retreat_words": retreat_words, "intents_hash": intents_hash,
                    "kws_threshold": kws_threshold, "kws_score": kws_score }),
        );
        Ok(json!({ "state": "sent", "hash": hash, "words": words,
                   "device": device, "keywords_file": compiled,
                   "retreat_words": retreat_words, "intents_hash": intents_hash,
                   "kws_threshold": kws_threshold, "kws_score": kws_score }))
    }

    pub async fn deploy_reflex_auto(&self, device: &str) -> Result<serde_json::Value, String> {
        if self
            .hub
            .supports_config_apply_v2(device, crate::gateway::ConfigBlock::Kws)
            .is_err()
        {
            return self.deploy_reflex(device);
        }
        let (rec, note, out) = crate::config_delivery::dispatch_reflex_v2(
            &self.store,
            &self.hub,
            &self.settings_snapshot(),
            device,
        )
        .await?;
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.reflex_deploy = Some(rec);
        }
        self.note("kws_deploy_sent", note);
        Ok(out)
    }

    /// R1j/P1: execute one intent (preconditions + the SAME action functions
    /// the text layer uses). Shared by the spotter path and the transcript
    /// fallback — one implementation, so a word cannot mean two things.
    /// Err(reason) = a precondition fact, never an execution attempt.
    pub(super) fn run_reflex_intent(
        self: &Arc<Self>,
        device: &str,
        word: &str,
        intent: interaction::KeywordIntent,
        via: &str,
        expected: Option<Identity>,
    ) -> Result<serde_json::Value, &'static str> {
        match intent {
            interaction::KeywordIntent::Wake => Ok(json!({ "note": "wake uses the matrix path" })),
            interaction::KeywordIntent::Retreat => {
                if !self.has_active_session_or_playback(device) {
                    return Err("reflex_precondition:no_session");
                }
                let (listening, bookmark, expected) = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.to_string()).or_default();
                    (
                        st.participation.session.state == SessionState::Listening,
                        st.paused_program.is_some(),
                        expected.unwrap_or_else(|| st.participation.identity()),
                    )
                };
                if !self.retreat_session_at(device, word, via, expected) {
                    return Err("stale_close_identity");
                }
                Ok(
                    json!({ "precondition": { "listening": listening, "bookmark": bookmark,
                                             "idle": !listening && !bookmark } }),
                )
            }
            interaction::KeywordIntent::Pause => {
                let (program, bookmark) = {
                    let states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.get(device);
                    (
                        st.map(|s| s.live_program.is_some()).unwrap_or(false),
                        st.map(|s| s.paused_program.is_some()).unwrap_or(false),
                    )
                };
                // M3: media is a valid target too. The precedence lives in
                // `pause_current` (foreground media > live program > paused).
                let Some(target) = self.pause_current(device, &format!("{via}_pause"), expected)
                else {
                    return Err("reflex_precondition:no_program");
                };
                Ok(json!({ "target": target, "paused": true,
                           "precondition": { "program": program, "bookmark": bookmark,
                                             "media": self.media_state(device) } }))
            }
            interaction::KeywordIntent::Continue => {
                let Some(target) =
                    self.resume_current(device, &format!("{via}_continue"), expected)
                else {
                    return Err("reflex_precondition:no_bookmark");
                };
                Ok(json!({ "target": target, "resumed": true }))
            }
            interaction::KeywordIntent::Next => {
                // 下一首 walks the session queue; it never re-searches.
                let Some(music) = self.music() else {
                    return Err("reflex_precondition:no_media");
                };
                {
                    let states = crate::lock_or_recover(&self.states, "voice.states");
                    let Some(st) = states.get(device) else {
                        return Err("stale_control_identity");
                    };
                    if expected.is_some_and(|e| st.participation.identity() != e) {
                        return Err("stale_control_identity");
                    }
                }
                if !music.media_present(device) {
                    return Err("reflex_precondition:no_media");
                }
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                music.register_control(device, cancel.clone());
                let music = music.clone();
                let d = device.to_string();
                let me = self.clone();
                tokio::spawn(async move {
                    let outcome = music
                        .control_with_cancel(&d, "next", None, cancel, None)
                        .await;
                    me.note(
                        "media_next",
                        json!({ "device": d, "ok": outcome.is_ok(),
                                "detail": outcome.map(|o| o.detail).unwrap_or_else(|e| e.code().to_string()) }),
                    );
                });
                Ok(json!({ "target": "media", "action": "next" }))
            }
            interaction::KeywordIntent::Stop => {
                // 停止播放 DISCARDs (pause is the word that keeps a rescue
                // point). Media session gone, program bookmark dropped, the
                // current answer cancelled — the conversation stays open.
                let (stopped, dropped) = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.to_string()).or_default();
                    if expected.is_some_and(|e| e != st.participation.identity()) {
                        return Err("stale_control_identity");
                    }
                    let dropped =
                        st.live_program.take().is_some() | st.paused_program.take().is_some();
                    let stopped = self.stop_device_locked(device, st, true);
                    (stopped, dropped)
                };
                let media = self.music().and_then(|m| m.stop_now(device, "reflex_stop"));
                Ok(
                    json!({ "target": if media.is_some() { "media" } else { "none" },
                           "media_title": media, "program_discarded": dropped,
                           "stop": stopped }),
                )
            }
        }
    }

    /// R1j/P1: execute a reflex word that arrived as a TRANSCRIPT (the
    /// spotter missed it, or it already ran and this is the same speech).
    /// Dedupe first, then the shared intent execution. Recorded as
    /// `reflex_text` — the spotter's per-word hit counters stay its own.
    pub(super) fn handle_reflex_text(
        self: &Arc<Self>,
        device: &str,
        word: &str,
        intent: interaction::KeywordIntent,
        via: &str,
        expected: Identity,
    ) {
        if self.reflex_deduped(device, intent, via) {
            return;
        }
        match self.run_reflex_intent(device, word, intent, via, Some(expected)) {
            Ok(detail) => {
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    *st.reflex_text_hits.entry(word.to_string()).or_default() += 1;
                }
                self.note(
                    "reflex_text",
                    json!({ "device": device, "keyword": word, "intent": Self::intent_name(intent),
                            "via": via, "detail": detail }),
                );
            }
            Err(reason) => {
                self.note(
                    "keyword_ignored",
                    json!({ "device": device, "keyword": word, "reason": reason, "via": via }),
                );
            }
        }
        self.push_assistant_state(device, false);
    }

    /// R1j/P1 text fallback: does this transcript mean a configured reflex
    /// word (whole last sentence, so a story containing the word is not a
    /// command)? The spotter and the text layer share the TABLE, not the
    /// matching mechanism.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn reflex_text_command(
        &self,
        text: &str,
    ) -> Option<(String, interaction::KeywordIntent)> {
        self.reflex_text_command_for("global", text)
    }

    pub(super) fn reflex_text_command_for(
        &self,
        device: &str,
        text: &str,
    ) -> Option<(String, interaction::KeywordIntent)> {
        let said = normalize_utterance(last_sentence(text));
        if said.is_empty() || said.chars().count() > 12 {
            return None;
        }
        if let Some(w) = self
            .effective_wake_words_for(device)
            .into_iter()
            .find(|w| normalize_utterance(w) == said)
        {
            return Some((w, interaction::KeywordIntent::Wake));
        }
        let retreat_ov = crate::lock_or_recover(&self.settings, "voice.settings")
            .device_overrides
            .get(device)
            .and_then(|o| o.retreat_words.clone());
        if let Some(words) = &retreat_ov {
            if let Some(w) = words.iter().find(|w| normalize_utterance(w) == said) {
                return Some((w.clone(), interaction::KeywordIntent::Retreat));
            }
        }
        let cfg = crate::config::ConfigService::new(self.store.clone())
            .reflex()
            .ok()?;
        let w = cfg
            .words
            .iter()
            .find(|w| w.enabled && normalize_utterance(&w.text) == said)?;
        let intent = self.reflex_intent_for(device, &w.text)?;
        Some((w.text.clone(), intent))
    }

    /// R1j/P1: one lookup from a spotted string to the intent the reflex
    /// table assigns it. Disabled words and unknown words are None.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn reflex_intent(&self, keyword: &str) -> Option<interaction::KeywordIntent> {
        self.reflex_intent_for("global", keyword)
    }

    pub(super) fn reflex_intent_for(
        &self,
        device: &str,
        keyword: &str,
    ) -> Option<interaction::KeywordIntent> {
        let (wake_ov, retreat_ov) = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            let ov = s.device_overrides.get(device);
            (
                ov.and_then(|o| o.wake_words.clone()),
                ov.and_then(|o| o.retreat_words.clone()),
            )
        };
        if let Some(words) = &wake_ov {
            if words
                .iter()
                .any(|w| crate::config::reflex_label_matches(w, keyword))
            {
                return Some(interaction::KeywordIntent::Wake);
            }
        }
        if let Some(words) = &retreat_ov {
            if words
                .iter()
                .any(|w| crate::config::reflex_label_matches(w, keyword))
            {
                return Some(interaction::KeywordIntent::Retreat);
            }
        }
        let cfg = crate::config::ConfigService::new(self.store.clone())
            .reflex()
            .ok()?;
        if let Some(w) = cfg
            .words
            .iter()
            .find(|w| w.enabled && crate::config::reflex_label_matches(&w.text, keyword))
        {
            return match w.intent {
                crate::config::ReflexIntent::Wake => {
                    if wake_ov.is_some() {
                        None
                    } else {
                        Some(interaction::KeywordIntent::Wake)
                    }
                }
                crate::config::ReflexIntent::Retreat => {
                    if retreat_ov.is_some() {
                        None
                    } else {
                        Some(interaction::KeywordIntent::Retreat)
                    }
                }
                crate::config::ReflexIntent::Pause => Some(interaction::KeywordIntent::Pause),
                crate::config::ReflexIntent::Continue => Some(interaction::KeywordIntent::Continue),
                crate::config::ReflexIntent::Next => Some(interaction::KeywordIntent::Next),
                crate::config::ReflexIntent::Stop => Some(interaction::KeywordIntent::Stop),
            };
        }
        if wake_ov.is_some() {
            return None;
        }
        self.effective_wake_words_for(device)
            .iter()
            .any(|w| crate::config::reflex_label_matches(w, keyword))
            .then_some(interaction::KeywordIntent::Wake)
    }

    /// Per-word hit/ignore counters (RJ: every word is judged on its own).
    pub(super) fn bump_reflex(&self, device: &str, keyword: &str, hit: bool) {
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            let map = if hit {
                &mut st.reflex_hits
            } else {
                &mut st.reflex_ignored
            };
            *map.entry(keyword.to_string()).or_default() += 1;
        }
    }

    /// Stable event name for an intent (the console groups by it).
    pub(super) fn intent_name(intent: interaction::KeywordIntent) -> &'static str {
        match intent {
            interaction::KeywordIntent::Wake => "wake",
            interaction::KeywordIntent::Retreat => "retreat",
            interaction::KeywordIntent::Pause => "pause",
            interaction::KeywordIntent::Continue => "continue",
            interaction::KeywordIntent::Next => "next",
            interaction::KeywordIntent::Stop => "stop",
        }
    }

    /// R1j/P1 dedupe: record and answer whether the SAME intent already ran
    /// off the spotter inside the transcript-latency window. One window, one
    /// tag (`intent_deduped{via}`), both layers keep their own matching.
    pub(super) fn reflex_deduped(
        &self,
        device: &str,
        intent: interaction::KeywordIntent,
        via: &str,
    ) -> bool {
        if !self.reflex_recent(device, intent) {
            return false;
        }
        self.note(
            "intent_deduped",
            json!({ "device": device, "intent": Self::intent_name(intent),
                    "via": via, "window_ms": REFLEX_DEDUPE_MS }),
        );
        true
    }

    /// R1j/P1 dedupe: did the spotter already execute this intent within the
    /// window the ASR transcript for the same speech would arrive in?
    pub(super) fn reflex_recent(&self, device: &str, intent: interaction::KeywordIntent) -> bool {
        crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| st.last_reflex.map(|(i, ts)| (i, ts)))
            .map(|(i, ts)| i == intent && id::now_ms().saturating_sub(ts) < REFLEX_DEDUPE_MS)
            .unwrap_or(false)
    }

    /// D/T5 semantic barge-in policy (mirror of [`Self::on_keyword`]): the
    /// device reported genuine near-end speech while a playback was live —
    /// energy onset + near-end/DAC-reference cross-correlation guard (RUI
    /// Local defense ①) + the onset pre-roll (defense ②) are the device's
    /// facts; THIS decides what they become.
    ///
    /// R1g: the MATRIX decides first. On a HALF-duplex link a direct near-end
    /// fact is a normal fact that must not stop anything and must not open a
    /// window (the user's rule: the wake word is the only command entry);
    /// the acoustic gates below only run in the full-duplex cell, where the
    /// fact can become a turn. Media playback is no longer a separate scope:
    /// the content is an input to the matrix, not a reason to drop the fact.
    pub(super) fn on_barge_in(
        &self,
        device: &str,
        ts: u64,
        rx_ms: u64,
        preroll: Vec<i16>,
        sample_rate: u32,
        corr: f32,
        energy_db: f32,
        ref_db: f32,
        residual_db: f32,
        ref_locked: bool,
    ) {
        let (enabled, protect_ms) = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            let eff = s.effective_for(device);
            (eff.barge_in, eff.barge_protect_ms)
        };
        let now = id::now_ms();
        // M6b: the diagnostic fields carry `null` for the device's "no
        // measurement" sentinel instead of -3.4e38 (see gateway::valid_db).
        let energy_field = crate::gateway::valid_db(energy_db);
        let ref_field = crate::gateway::valid_db(ref_db);
        let residual_field = crate::gateway::valid_db(residual_db);
        let muted = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| st.muted)
            .unwrap_or(false);
        // R1d + R1g: a FRESH local-stop marker means the device cut this audio
        // itself, so "is a voice play live?" cannot answer — the hardware fact
        // does (the play is gone, the person is still talking).
        let local_stopped = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| {
                st.local_stop
                    .as_ref()
                    .map(|(_, at)| now.saturating_sub(*at) <= LOCAL_STOP_TRUST_MS)
            })
            .unwrap_or(false);
        let mut content = self.live_content(device);
        if local_stopped {
            content.voice = true;
        }
        let gate_reason = if !enabled {
            "disabled"
        } else if muted {
            "muted"
        } else {
            ""
        };
        // The matrix is the FIRST gate: duplex policy, not acoustics.
        let decision = interaction::decide(
            self.effective_duplex(device),
            Trigger::DirectNearEnd,
            content,
        );
        let reason = if !gate_reason.is_empty() {
            gate_reason
        } else if !decision.honor {
            decision.reason
        } else {
            ""
        };
        if !reason.is_empty() {
            // Two different facts, two counters: the half-duplex rule is a
            // normal policy cell (the device's AEC proved a person, the link
            // says "wake word first"); everything else is an untrusted or
            // empty fact.
            let half_suppressed = reason == interaction::REASON_DIRECT_SUPPRESSED_HALF;
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                if half_suppressed {
                    st.barge_in_suppressed += 1;
                } else {
                    st.barge_in_ignored += 1;
                }
            }
            self.note(
                if half_suppressed { "barge_in_suppressed" } else { "barge_in_ignored" },
                json!({ "device": device, "reason": reason, "corr": corr, "energy_db": energy_field,
                        "ref_db": ref_field, "residual_db": residual_field, "ref_locked": ref_locked,
                        "content": { "voice": content.voice, "media": content.media },
                        "why": if half_suppressed {
                            "half duplex routes interruption through the wake word; this fact stops nothing"
                        } else {
                            "nothing actionable on this device"
                        } }),
            );
            return;
        }
        let (protected, episode_open) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get_mut(device)
                .map(|st| {
                    st.barge.expire(now);
                    (
                        // R1: the answer we are speaking is inside its own
                        // onset protection period — a "near-end" fact now is
                        // our TTS onset transient, not a person.
                        now < st.barge_protect_until_ms,
                        // One interruption per episode: while the window we
                        // just opened is still open, further facts are
                        // duplicates (the device may fire once per block).
                        st.barge.is_window_open(now),
                    )
                })
                .unwrap_or((false, false))
        };
        let reason = if protected {
            // Necessary-not-sufficient: low correlation during a fresh
            // answer's onset is expected (AEC still converging), never proof
            // of a person. This is the R1 self-trigger breaker.
            "answer_protect"
        } else if episode_open {
            "episode_active"
        // R1 (2026-11-12): a fact is only actionable when the device can
        // prove the evidence is trustworthy. Old firmware (fields default
        // 0/false) is refused here rather than self-answering.
        } else if !ref_locked {
            "reference_not_locked"
        } else if ref_db < -50.0 {
            "reference_too_quiet"
        } else if residual_db <= -15.0 {
            // The device's own near-end margin is 15 dB (`bargein.rs`:
            // energy >= ref - 15). This is a sanity check that the fact
            // matches that contract, NOT a second acoustic decision.
            //
            // Do not add a level floor here. An absolute -45 dBFS floor was
            // tried on 2026-11-12 and rejected the user's real interruptions
            // at energy -47.97/-52.22 dBFS (device had already stopped
            // locally at 32/35 ms; the core then dropped the fact, so no
            // follow-up turn). Those levels overlap the measured false
            // stops (-49.4/-56.5), so no floor can separate them. A false
            // stop's follow-up segment is dropped downstream by
            // `is_filler_utterance` (R2-3), which is where that belongs.
            "residual_too_low"
        } else {
            ""
        };
        if !reason.is_empty() {
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                st.barge_in_ignored += 1;
            }
            self.note(
                "barge_in_ignored",
                json!({ "device": device, "reason": reason, "corr": corr, "energy_db": energy_field,
                        "ref_db": ref_field, "residual_db": residual_field, "ref_locked": ref_locked,
                        "protect_ms": protect_ms }),
            );
            return;
        }
        // Accept: stop every channel, open the follow window, hold the
        // pre-roll, and exempt the incoming stream from the half-duplex
        // drop + tail-blind window until the first post-barge-in segment
        // closes (the device guard already proved this audio is a person,
        // not our own echo). The TAIL-BLIND window is deliberately NOT
        // cleared (R1): it protects the reverb tail of the play we just
        // stopped, and the barge window already pokes through it for the
        // interrupting speech.
        //
        // R1d: when the device already stopped locally, the stop command is
        // pointless — `on_stopped_locally` did the same invalidation when
        // that frame arrived, and a second one would bump the epoch again
        // (killing a turn that started in between). Only the window/preroll
        // half remains.
        let preroll_ms = preroll.len() as u64 * 1000 / sample_rate.max(1) as u64;
        let result = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            let result = if local_stopped {
                json!({ "stop_requests_sent": 0, "note": "already stopped locally by the device reflex" })
            } else {
                let _ = Self::bookmark_live_program_locked(st, now);
                self.stop_device_locked(device, st, true)
            };
            st.barge_in_stops += 1;
            st.last_barge_in_ms = now;
            // R1e: the onset lives in the pre-roll. It is the head of the
            // words the user is saying right now.
            // R1 split: stop is immediate; the utterance window opens
            // only after the stopped answer's flush + DAC/room tail.
            let head: Vec<i16> = preroll.into_iter().take(BARGE_HEAD_MAX_SAMPLES).collect();
            let head_provenance = crate::mic::CaptureProvenance::legacy(head.len());
            st.barge = Barge::Stopped {
                until_ms: now + BARGE_IN_WINDOW_MS,
                guard_until_ms: now + BARGE_UTTERANCE_GUARD_MS,
                head,
                head_sample_rate: sample_rate,
                head_provenance,
                head_until_ms: now + BARGE_HEAD_TTL_MS,
            };
            st.wake_flash_until_ms = now + WAKE_FLASH_MS;
            // The marker has done its job (this fact); the next fact must
            // prove itself against a live play or a fresh local stop.
            if local_stopped {
                st.local_stop = None;
            }
            result
        };
        self.open_participation(device, CreditGrant::One);
        // R1e: the pre-roll is never FED to the VAD (it is recorded while our
        // own answer plays, so the device's own voice could open a segment —
        // 2026-11-10: "我在。"). It is kept as the HEAD of the segment the
        // user's own speech opens, so the first syllable survives.
        self.note(
            "barge_in_preroll_held",
            json!({ "device": device, "samples": preroll_ms * sample_rate as u64 / 1000,
                    "preroll_ms": preroll_ms, "rate": sample_rate,
                    "why": "not fed to the VAD; kept as the segment head so the first syllable survives" }),
        );
        self.note(
            "barge_in_stop",
            json!({ "device": device, "ts": ts, "corr": corr, "energy_db": energy_field,
                    "ref_db": ref_field, "residual_db": residual_field, "ref_locked": ref_locked,
                    "preroll_ms": preroll_ms, "latency_ms": now.saturating_sub(ts),
                    // R1d attribution: wire = device send → this process
                    // decoded it; pump = decoded → this handler ran. Together
                    // they say where the 2026-11-12 3.078 s went.
                    "wire_ms": rx_ms.saturating_sub(ts),
                    "pump_ms": now.saturating_sub(rx_ms),
                    "locally_stopped": local_stopped,
                    "utterance_from_ms": now + BARGE_UTTERANCE_GUARD_MS,
                    "stop": result }),
        );
        self.push_assistant_state(device, false);
    }

    /// R1d: the device cut its own voice playback on the near-end reflex.
    /// Treat it exactly like a stop we issued ourselves — same epoch bump,
    /// same task cancellation, same output bookkeeping — because a turn that
    /// is still synthesizing must not keep pushing samples at a channel the
    /// device already flushed. The difference is only that no Stop goes
    /// south (there is nothing left to stop) and the hardware fact is kept so
    /// the BargeIn that follows can still become a turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_stopped_locally(
        &self,
        device: &str,
        task_id: &str,
        reason: &str,
        played_ms: u64,
        latency_ms: u64,
        ahead_ms: u64,
        ts: u64,
    ) {
        let now = id::now_ms();
        let (result, epoch) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let Some(st) = states.get_mut(device) else {
                return;
            };
            if !st
                .outputs
                .get(task_id)
                .is_some_and(|r| stage_is_live(&r.stage))
            {
                drop(states);
                self.note(
                    "stale_session_event_rejected",
                    json!({"device":device,"via":"local_stop","play_id":task_id}),
                );
                return;
            }
            let live_program = st.live_program.take();
            let result = self.stop_device_locked(device, st, false);
            st.local_stop = Some((task_id.to_string(), ts));
            // R1e: the device cut the audio — the program is no longer
            // being read. Keep it as a bookmark (RUI-local: a story
            // keeps its place and is never auto-resumed) instead of
            // leaving it "live" in the console for ever.
            if let Some(mut p) = live_program {
                p.created_ms = now;
                p.system_cut = false; // the person stopped it
                if st.paused_program.is_none() {
                    st.paused_program = Some(p);
                }
            }
            (result, Some(st.stop_epoch))
        };
        self.note(
            "stopped_locally",
            json!({ "device": device, "task_id": task_id, "reason": reason,
                    "played_ms": played_ms,
                    // The device's own fire→flush time is the number a slow
                    // north link must never be able to inflate.
                    "latency_ms": latency_ms,
                    // Read-ahead at the cut: the measured proof that the
                    // stream pacing budget is spent per play, not per
                    // sentence.
                    "ahead_ms": ahead_ms,
                    "fact_age_ms": now.saturating_sub(ts),
                    "epoch": epoch,
                    "stop": result }),
        );
        self.push_assistant_state(device, false);
    }

    /// Current delivery epoch for a device (0 when unknown). A stop or a
    /// barge-in bumps it; a turn captured before the bump is stale.
    pub(super) fn stop_epoch_of(&self, device: &str) -> u64 {
        crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .map(|st| st.stop_epoch)
            .unwrap_or(0)
    }

    /// R1g/P7: the link's EFFECTIVE duplex. A device that has not reported
    /// is half — the safe direction is "the wake word is required".
    pub(super) fn effective_duplex(&self, device: &str) -> Duplex {
        let states = crate::lock_or_recover(&self.states, "voice.states");
        let effective = states
            .get(device)
            .and_then(|st| st.duplex.as_ref())
            .map(|d| d.effective.as_str())
            .unwrap_or("half");
        Duplex::from_effective(effective)
    }

    /// What is live on the device right now (the matrix's `content` input).
    /// One definition, used by every trigger: the hub registry knows the
    /// tool-submitted plays this map never saw.
    /// R1g: honour a WAKE-WORD fact (ASR wake or a follow-up inside an open
    /// window). The matrix decides whether the audio stops — half duplex
    /// stops it (the wake word is the command entry), full duplex does not
    /// (the assistant may keep speaking; R3b). Returns the decision so the
    /// caller can route the residue and the ack.
    pub(super) fn honor_wake_word(
        &self,
        device: &str,
        reason: &str,
        expected: Identity,
        admitted_input: Option<u64>,
        pre_paused_media: Option<serde_json::Value>,
    ) -> Option<(interaction::Decision, Identity)> {
        let hub_live = self.effective_hub_live(device);
        let media_playing = pre_paused_media.is_some()
            || self.media_state(device) == Some(crate::tool::music::MediaState::Playing);
        let (content, decision) = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get(device)?;
            if st.participation.identity() != expected
                || admitted_input
                    .is_some_and(|id| !self.consumed_input_valid_locked(device, st, id))
            {
                return None;
            }
            let live = |ch: &str| {
                st.outputs
                    .values()
                    .any(|o| o.channel == ch && stage_is_live(&o.stage))
            };
            let content = Content {
                voice: hub_live.voice || live("voice"),
                media: hub_live.media || live("media") || media_playing,
            };
            let duplex = Duplex::from_effective(
                st.duplex
                    .as_ref()
                    .map(|d| d.effective.as_str())
                    .unwrap_or("half"),
            );
            let decision = interaction::decide(duplex, Trigger::WakeWord, content);
            if !(decision.honor && decision.stop) {
                return Some((decision, st.participation.identity()));
            }
            (content, decision)
        };
        // Serial 91-A: pause media OUTSIDE `voice.states` and strictly BEFORE stop-all.
        let media_paused = if let Some(paused) = pre_paused_media {
            Some(paused)
        } else if content.media {
            self.pause_media_for_takeover(device)
        } else {
            None
        };
        let (stop, had, bookmark, final_identity) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get_mut(device)?;
            if st.participation.identity() != expected
                || admitted_input
                    .is_some_and(|id| !self.consumed_input_valid_locked(device, st, id))
            {
                return None;
            }
            let (had, bookmark) = Self::bookmark_live_program_locked(st, id::now_ms());
            let stop = self.stop_for_input_locked(device, st, admitted_input)?;
            (stop, had, bookmark, st.participation.identity())
        };
        if had {
            self.note("program_bookmarked_after_interrupt", json!({ "device": device, "why": reason,
                "bookmark": bookmark, "note": "the wake word stopped the audio; 继续 resumes from here" }));
        }
        self.note(
            if content.any() {
                "wake_word_stop"
            } else {
                "wake_word_open"
            },
            json!({ "device": device,
            "via": reason, "content": { "voice": content.voice, "media": content.media },
            "stop": stop, "ack": decision.ack, "media_paused": media_paused }),
        );
        Some((decision, final_identity))
    }

    pub(super) fn take_turn_wake(
        &self,
        device: &str,
        source: &str,
        request: crate::voice::participation::RequestTicket,
    ) -> Option<(
        Arc<crate::task::CancellationScope>,
        Option<(String, String)>,
    )> {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get_mut(device)?;
        let scope = st.participation.request_cancel_scope(request)?;
        let fresh_wake = (source == "device:asr")
            .then(|| {
                st.last_wake
                    .take()
                    .filter(|(_, _, g)| *g == request.generation)
                    .map(|(ch, word, _)| (ch, word))
            })
            .flatten();
        let wake = fresh_wake.or_else(|| {
            (source == "device:asr" && st.participation.is_active_request(request))
                .then(|| ("followup".into(), String::new()))
        });
        Some((scope, wake))
    }
}
