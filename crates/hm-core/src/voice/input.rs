//! One bounded microphone input per device. Participation owns permission;
//! this reservation carries that permission through capture and recognition.
use super::*;
use tokio::time::{Duration, Instant};

pub(super) const INPUT_LIFETIME: Duration = Duration::from_secs(60);
pub(super) const ASR_RECOGNITION_TIMEOUT: Duration = Duration::from_secs(12);
const CAPTURE_LIFETIME: Duration = Duration::from_secs(30);

pub(super) struct InputLease {
    pub id: u64,
    pub start_ms: u64,
    pub start: Instant,
    pub capture_deadline: Instant,
    pub stop_epoch: u64,
    pub generation: u64,
    pub connection: Option<u64>,
    pub context: Option<crate::session::ContextTicket>,
    pub origin: crate::mic::CaptureProvenance,
    pub eligibility: &'static str,
    pub confirmed: bool,
    pub recognizing: bool,
    pub consumed: bool,
    pub abort: Option<tokio::task::AbortHandle>,
}
pub(super) fn capture_epoch(origin: &crate::gateway::InputOrigin) -> Option<&str> {
    match origin {
        crate::gateway::InputOrigin::Leased { origin, .. } => capture_epoch(origin),
        crate::gateway::InputOrigin::Audio { capture_epoch, .. } => Some(capture_epoch),
        _ => None,
    }
}
impl InputLease {
    pub fn matches_capture(&self, provenance: &crate::gateway::InputProvenance) -> bool {
        fn source(origin: &crate::gateway::InputOrigin) -> &crate::gateway::InputOrigin {
            match origin {
                crate::gateway::InputOrigin::Leased { origin, .. } => source(origin),
                other => other,
            }
        }
        self.origin.spans.iter().any(|s| {
            match (source(&s.provenance.origin), source(&provenance.origin)) {
                (
                    crate::gateway::InputOrigin::Audio {
                        capture_epoch: a,
                        ticket: b,
                        input_id: c,
                        ..
                    },
                    crate::gateway::InputOrigin::Audio {
                        capture_epoch: x,
                        ticket: y,
                        input_id: z,
                        ..
                    },
                ) => (a, b, c) == (x, y, z),
                (crate::gateway::InputOrigin::Legacy, crate::gateway::InputOrigin::Legacy) => {
                    match (&s.provenance.transport, &provenance.transport) {
                        (Some(ta), Some(tb)) => ta.generation == tb.generation,
                        _ => true,
                    }
                }
                _ => false,
            }
        })
    }
    pub fn admitted(&self) -> bool {
        self.eligibility != "pending"
    }
    pub fn valid(&self, st: &DeviceVoiceState) -> bool {
        self.stop_epoch == st.stop_epoch
            && self.generation == st.participation.generation()
            && self.connection == st.input_connection_generation
            && st.muted != Some(true)
            && Instant::now() < self.start + INPUT_LIFETIME
            && (self.recognizing || Instant::now() < self.capture_deadline)
    }
}
impl DeviceVoiceState {
    pub(super) fn revoke_input(&mut self, status: &'static str) {
        let Some(input) = self.input.take() else {
            return;
        };
        if status != "consumed" {
            self.participation.invalidate_admission_revision();
        }
        if let Some(abort) = input.abort {
            abort.abort();
        }
        if input.recognizing {
            self.mic_counters.asr_inflight = self.mic_counters.asr_inflight.saturating_sub(1);
        }
        self.input_status = Some(status);
    }
    pub(super) fn input_snapshot(&self) -> serde_json::Value {
        let Some(i) = &self.input else {
            return json!({"state":self.input_status.unwrap_or("idle"),"slot_limit":1});
        };
        let transport_gen = i
            .origin
            .spans
            .iter()
            .find_map(|s| s.provenance.transport.as_ref().map(|t| t.generation))
            .or(i.connection);
        let seq_first = i
            .origin
            .spans
            .iter()
            .filter_map(|s| s.provenance.transport.as_ref().map(|t| t.inbound_seq))
            .min();
        let seq_last = i
            .origin
            .spans
            .iter()
            .filter_map(|s| s.provenance.transport.as_ref().map(|t| t.inbound_seq))
            .max();
        json!({"state":if !i.valid(self) {"expired"} else if i.recognizing {"recognizing"} else if i.admitted() {"capture"} else {"unadmitted"},
            "input_id":i.id,"eligibility":i.eligibility,"start_ms":i.start_ms,"confirmed":i.confirmed,
            "capture_deadline_ms":i.start_ms.saturating_add(i.capture_deadline.duration_since(i.start).as_millis() as u64),
            "lifetime_deadline_ms":i.start_ms.saturating_add(INPUT_LIFETIME.as_millis() as u64),
            "capture_limit_ms":i.capture_deadline.duration_since(i.start).as_millis(),
            "lifetime_limit_ms":INPUT_LIFETIME.as_millis(),"slot_limit":1,
            "generation":i.generation,"connection_generation":i.connection,"transport_generation":transport_gen,
            "inbound_seq_first":seq_first,"inbound_seq_last":seq_last,"stop_epoch":i.stop_epoch,
            "remaining_ms":(i.start+INPUT_LIFETIME).saturating_duration_since(Instant::now()).as_millis(),"origin":i.origin})
    }
}
impl VoicePipeline {
    pub(super) fn music_blocks_transcript(
        &self,
        device: &str,
        input_id: u64,
        duration_ms: u64,
    ) -> bool {
        let Some(music) = self.music() else {
            return false;
        };
        let Some((start, explicit)) = ({
            let states = crate::lock_or_recover(&self.states, "voice.states");
            states.get(device).map(|st| {
                let start = st
                    .input
                    .as_ref()
                    .filter(|i| i.id == input_id)
                    .map(|i| i.start)
                    .unwrap_or_else(|| Instant::now() - Duration::from_millis(duration_ms));
                let explicit = st
                    .participation
                    .opened_by
                    .as_deref()
                    .filter(|by| {
                        matches!(*by, "keyword" | "protocol_wake" | "text_wake" | "asr_wake")
                    })
                    .map(|_| st.participation.session.since_ms);
                (start, explicit)
            })
        }) else {
            return false;
        };
        music.blocks_passive_capture(device, start, duration_ms, explicit)
    }

    pub(super) fn reject_passive_music_input(&self, device: &str, input_id: u64, text: &str) {
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.mic_counters.ignored_unwoken += 1;
            st.participation.last_denied =
                Some(json!({"reason":"media_lifecycle","at_ms":id::now_ms()}));
        }
        let text: String = text.chars().take(60).collect();
        self.note(
            "mic_followup_gated_by_media",
            json!({"device":device,"input_id":input_id,"reason":"actual_media_lifecycle"}),
        );
        self.note("mic_ignored_unwoken", json!({"device":device,"text":text}));
    }

    #[cfg(test)]
    pub(super) async fn pause_input_handoff(&self, stage: &str) {
        let hook = {
            let mut hook = self.input_handoff_hook.lock().unwrap();
            if hook.as_ref().is_some_and(|(target, _, _)| *target == stage) {
                hook.take()
            } else {
                None
            }
        };
        if let Some((_, entered, resume)) = hook {
            let _ = entered.send(());
            let _ = resume.await;
        }
    }

    pub(super) fn consumed_input_valid_locked(
        &self,
        device: &str,
        st: &DeviceVoiceState,
        input_id: u64,
    ) -> bool {
        let Some(input) = st
            .input
            .as_ref()
            .filter(|i| i.id == input_id && i.consumed && i.valid(st))
        else {
            return false;
        };
        input.context.is_none_or(|context| {
            crate::session::SessionStore::new(self.store.clone())
                .current_device_context(device)
                .ok()
                .flatten()
                .is_some_and(|current| current.ticket() == context)
        })
    }

    /// The exact reservation remains owned until turn admission. A recognized
    /// explicit wake may rotate its context/generation but never its deadline.
    pub(super) fn activate_consumed_input_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
        input_id: u64,
        expected: Identity,
        policy: FollowWindowPolicy,
        context_ms: u64,
    ) -> Option<(Identity, crate::session::ContextTicket)> {
        if st.participation.identity() != expected
            || !self.consumed_input_valid_locked(device, st, input_id)
        {
            return None;
        }
        let mut input = st.input.take()?;
        st.invalidate_live_turn();
        let identity =
            st.participation
                .activate(expected, policy, CreditGrant::Full, id::now_ms())?;
        let context = self.admit_context_locked(device, st, id::now_ms(), context_ms, true)?;
        input.generation = st.participation.generation();
        input.stop_epoch = st.stop_epoch;
        input.context = Some(context);
        input.eligibility = "session";
        st.input = Some(input);
        st.participation.opened_by = Some("asr_wake");
        st.follow_close_pending_at = None;
        st.window_opener = None;
        Some((identity, context))
    }
    pub(super) fn activate_consumed_input(
        &self,
        device: &str,
        input_id: u64,
        expected: Identity,
    ) -> Option<(Identity, crate::session::ContextTicket)> {
        let (policy, context_ms) = self.context_policy();
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        self.activate_consumed_input_locked(
            device,
            states.get_mut(device)?,
            input_id,
            expected,
            policy,
            context_ms,
        )
    }
    pub(super) fn reserve_transcript_turn(
        &self,
        device: &str,
        expected: Identity,
        debit: bool,
        input: Option<(u64, Option<crate::session::ContextTicket>)>,
    ) -> Option<TurnTicket> {
        match input {
            Some((id, context)) => self.reserve_input_turn(device, expected, debit, id, context),
            None if !debit => self.reserve_turn(device, expected),
            None => self.reserve_turn_with_credit(device, expected, debit),
        }
    }

    /// Moving this admitted input into its turn may stop an earlier answer.
    /// Preserve only this consumed reservation across that internal stop.
    pub(super) fn stop_for_input_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
        input_id: Option<u64>,
    ) -> Option<serde_json::Value> {
        if input_id.is_some_and(|id| !self.consumed_input_valid_locked(device, st, id)) {
            return None;
        }
        let input = if st
            .input
            .as_ref()
            .is_some_and(|i| Some(i.id) == input_id && i.consumed && i.valid(st))
        {
            st.input.take()
        } else {
            None
        };
        let result = self.stop_device_locked(device, st, true);
        if let Some(mut input) = input {
            input.stop_epoch = st.stop_epoch;
            st.input = Some(input);
        }
        Some(result)
    }
    pub(super) fn barge_input(
        &self,
        device: &str,
        expected: Identity,
        input_id: u64,
    ) -> Option<Identity> {
        self.honor_wake_word(device, "utterance", expected, Some(input_id))
            .map(|(_, identity)| identity)
    }

    /// Only the exact active physical onset may cross this one KWS wake.
    /// Called under states; microphone observation follows states -> mic.
    fn wake_capture_source_current(
        st: &mut DeviceVoiceState,
        input: &InputLease,
        provenance: &crate::gateway::InputProvenance,
    ) -> bool {
        let connection = st.input_connection_generation;
        if input.connection != connection
            || provenance.transport.as_ref().map(|t| t.generation) != connection
            || !input.matches_capture(provenance)
            || !st.participation.accepts_origin(&provenance.origin)
        {
            return false;
        }
        input.origin.spans.iter().all(|span| {
            let source = &span.provenance;
            source.transport.as_ref().map(|t| t.generation) == connection
                && st.participation.accepts_origin(&source.origin)
                && capture_epoch(&source.origin)
                    .is_none_or(|epoch| st.current_capture_epoch.as_deref() == Some(epoch))
        })
    }

    pub(super) fn adopt_live_wake_onset_if_blocked_by_old_recognition_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
    ) {
        let Some((origin, onset, cap, confirmed)) = ({
            let mic = crate::lock_or_recover(&self.mic, "voice.mic");
            mic.get(device)
                .filter(|p| p.in_speech() || p.onset_pending())
                .and_then(|p| {
                    p.onset().map(|onset| {
                        (
                            p.onset_provenance(),
                            onset,
                            Duration::from_millis((p.config().max_blocks as u64 * 80).min(30_000)),
                            p.in_speech(),
                        )
                    })
                })
        }) else {
            return;
        };
        let blocked_by_old_recognition = match st.input.as_ref() {
            Some(i) => i.recognizing && !i.consumed && i.start != onset.1,
            None => st.input_status == Some("consumed"),
        };
        if !blocked_by_old_recognition {
            return;
        }
        if st.input.is_some() {
            st.revoke_input("cancelled");
        }
        st.mic_input_seq += 1;
        st.input = Some(InputLease {
            id: st.mic_input_seq,
            start_ms: onset.0,
            start: onset.1,
            capture_deadline: onset.1 + cap.min(CAPTURE_LIFETIME),
            stop_epoch: st.stop_epoch,
            generation: st.participation.generation(),
            connection: st.input_connection_generation,
            context: None,
            origin,
            eligibility: "pending",
            confirmed,
            recognizing: false,
            consumed: false,
            abort: None,
        });
        st.input_status = None;
    }

    pub(super) fn take_wake_capture_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
        provenance: &crate::gateway::InputProvenance,
    ) -> Option<InputLease> {
        let input = st.input.take()?;
        let valid = !input.recognizing
            && !input.consumed
            && input.abort.is_none()
            && input.valid(st)
            && Self::wake_capture_source_current(st, &input, provenance)
            && input.context.is_none_or(|context| {
                crate::session::SessionStore::new(self.store.clone())
                    .current_device_context(device)
                    .ok()
                    .flatten()
                    .is_some_and(|current| current.ticket() == context)
            })
            && crate::lock_or_recover(&self.mic, "voice.mic")
                .get(device)
                .is_some_and(|mic| {
                    (mic.in_speech() || mic.onset_pending())
                        && mic.onset().is_some_and(|(_, start)| start == input.start)
                });
        if valid {
            Some(input)
        } else {
            // The normal stop must still cancel/abort every rejected slot.
            st.input = Some(input);
            None
        }
    }

    pub(super) fn restore_wake_capture_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
        candidate: Option<InputLease>,
        provenance: &crate::gateway::InputProvenance,
        context: crate::session::ContextTicket,
    ) {
        let Some(mut input) = candidate else {
            return;
        };
        if st.input.is_some()
            || st.muted == Some(true)
            || Instant::now() >= input.start + INPUT_LIFETIME
            || Instant::now() >= input.capture_deadline
            || !Self::wake_capture_source_current(st, &input, provenance)
            || !crate::session::SessionStore::new(self.store.clone())
                .current_device_context(device)
                .ok()
                .flatten()
                .is_some_and(|current| current.ticket() == context)
        {
            return;
        }
        let mut mic = crate::lock_or_recover(&self.mic, "voice.mic");
        let Some(p) = mic.get_mut(device) else {
            return;
        };
        if !((p.in_speech() || p.onset_pending())
            && p.onset().is_some_and(|(_, start)| start == input.start))
        {
            return;
        }
        p.promote_wake_capture();
        drop(mic);
        let old_generation = input.generation;
        input.generation = st.participation.generation();
        input.stop_epoch = st.stop_epoch;
        input.context = Some(context);
        input.eligibility = "session";
        self.note("wake_capture_transferred", json!({"device":device,"input_id":input.id,
            "old_generation":old_generation,"generation":input.generation,
            "start_ms":input.start_ms,"confirmed":input.confirmed,
            "capture_limit_ms":input.capture_deadline.duration_since(input.start).as_millis(),
            "remaining_ms":(input.start+INPUT_LIFETIME).saturating_duration_since(Instant::now()).as_millis()}));
        st.last_wake_transferred_input_id = Some(input.id);
        st.input = Some(input);
    }

    /// True only while `device` is in the bounded post-pause `StopReceipt` flush
    /// window (`MEDIA_PAUSE_FLUSH_WINDOW_MS = 3_000`). Once `StopReceipt` arrives,
    /// `DeviceHub` retires the play so `hub_live(device).media` is already `false`;
    /// if `StopReceipt` is lost and the hub play remains active past 3s, callers
    /// stop folding `media` to `false` so unconfirmed live audio cannot trigger a
    /// false `Timeout` session close or boundary cue (#88.2).
    pub(super) fn media_pause_in_flight(&self, device: &str, now_ms: u64) -> bool {
        self.music()
            .is_some_and(|m| m.media_pause_in_flight(device, now_ms))
    }

    pub(super) fn effective_hub_live(&self, device: &str) -> HubLive {
        let mut live = self.hub_live(device);
        if live.media && self.media_pause_in_flight(device, id::now_ms()) {
            live.media = false;
        }
        live
    }

    pub(super) fn active_content(&self, device: &str) -> Content {
        let mut content = self.live_content(device);
        if content.media && self.media_pause_in_flight(device, id::now_ms()) {
            content.media = false;
        }
        content
    }

    /// True when `device` has a live media play that is not within the bounded
    /// post-pause `StopReceipt` flush window (`MEDIA_PAUSE_FLUSH_WINDOW_MS`).
    pub(super) fn media_actively_playing(&self, device: &str) -> bool {
        self.active_content(device).media
    }

    pub(super) fn capture_is_wake_ack_only(
        &self,
        device: &str,
        voice_live: bool,
        media_playing: bool,
        now: u64,
    ) -> bool {
        if media_playing {
            return false;
        }
        let hub_plays = self.hub.active_plays_for(device);
        let pause_flush_in_flight = hub_plays.iter().any(|(_, p)| p.channel == "media")
            && self.media_pause_in_flight(device, now);
        crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| {
                now >= st.other_voice_tail_until_ms
                    && st
                        .outputs
                        .iter()
                        .filter(|(_, o)| o.channel == "voice" && stage_is_live(&o.stage))
                        .all(|(id, _)| st.wake_ack_play.as_ref() == Some(id))
                    && hub_plays.iter().all(|(id, play)| {
                        st.wake_ack_play.as_ref() == Some(id)
                            || (play.channel == "media" && pause_flush_in_flight)
                    })
                    && if voice_live {
                        st.wake_ack_play.as_ref().is_some_and(|ack| {
                            st.outputs
                                .get(ack)
                                .is_some_and(|o| o.channel == "voice" && stage_is_live(&o.stage))
                                || hub_plays.iter().any(|(id, _)| id == ack)
                        })
                    } else {
                        now < st.wake_ack_tail_until_ms
                    }
            })
    }

    pub(super) fn start_input(&self, device: &str) {
        let (origin, onset, cap, confirmed) = {
            let mic = crate::lock_or_recover(&self.mic, "voice.mic");
            let Some(p) = mic.get(device) else {
                return;
            };
            (
                p.onset_provenance(),
                p.onset().unwrap_or((id::now_ms(), Instant::now())),
                Duration::from_millis((p.config().max_blocks as u64 * 80).min(30_000)),
                p.in_speech(),
            )
        };
        let proactive = crate::lock_or_recover(&self.settings, "voice.settings").proactive_asr_wake;
        let live = self.effective_hub_live(device);
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.entry(device.to_owned()).or_default();
        if st.input.as_ref().is_some_and(|i| i.valid(st)) {
            if st.input.as_ref().is_some_and(|i| i.start == onset.1) {
                if let Some(i) = st.input.as_mut().filter(|i| !i.recognizing) {
                    i.confirmed |= confirmed;
                }
                return;
            }
            let supersede_prior = st.input.as_ref().is_some_and(|i| {
                !i.consumed && (!i.recognizing || (confirmed && i.eligibility != "session"))
            });
            if !supersede_prior {
                return;
            }
            st.revoke_input("cancelled");
        }
        if st.input.is_some() {
            st.revoke_input("expired");
        }
        let control = origin.spans.iter().any(|s| s.control_only);
        let eligibility = if control {
            "control"
        } else if st.session_covers_with_live(onset.0, onset.0, onset.0, live) {
            "session"
        } else if proactive {
            "asr_fallback"
        } else {
            "pending"
        };
        st.mic_input_seq += 1;
        let context = if eligibility == "session"
            || (control
                && origin
                    .spans
                    .iter()
                    .all(|s| !s.control_only || s.wake_ack_overlap)
                && st.session_covers_with_live(onset.0, onset.0, onset.0, live))
        {
            crate::session::SessionStore::new(self.store.clone())
                .current_device_context(device)
                .ok()
                .flatten()
                .map(|c| c.ticket())
        } else {
            None
        };
        st.input = Some(InputLease {
            id: st.mic_input_seq,
            start_ms: onset.0,
            start: onset.1,
            capture_deadline: onset.1 + cap.min(CAPTURE_LIFETIME),
            stop_epoch: st.stop_epoch,
            generation: st.participation.generation(),
            connection: st.input_connection_generation,
            context,
            origin,
            eligibility,
            confirmed,
            recognizing: false,
            consumed: false,
            abort: None,
        });
        st.input_status = None;
    }

    /// Reserve onset authority before the detector's three-block confirmation
    /// lag crosses the idle boundary; no upload occurs until SpeechStarted.
    pub(super) fn observe_input_onset(&self, device: &str) {
        let (pending, speaking) = {
            let mic = crate::lock_or_recover(&self.mic, "voice.mic");
            let Some(p) = mic.get(device) else {
                return;
            };
            (p.onset_pending(), p.in_speech())
        };
        if pending || speaking {
            self.start_input(device);
        } else if !speaking {
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                if st
                    .input
                    .as_ref()
                    .is_some_and(|i| !i.confirmed && !i.recognizing)
                {
                    st.revoke_input("discarded");
                }
            }
        }
    }

    /// KWS may bind the current, still-capturing first sentence. Dispatched
    /// recognition keeps its old authority and can never be rebound.
    pub(super) fn bind_capture_input(
        st: &mut DeviceVoiceState,
        context: crate::session::ContextTicket,
    ) {
        if st.input.as_ref().is_some_and(|i| i.recognizing) {
            st.revoke_input("cancelled");
            return;
        }
        if let Some(i) = st.input.as_mut() {
            i.generation = st.participation.generation();
            i.stop_epoch = st.stop_epoch;
            i.context = Some(context);
            i.eligibility = "session";
        }
    }

    pub(super) fn sweep_inputs(&self) {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        for (device, st) in states.iter_mut() {
            if st.input.as_ref().is_some_and(|i| !i.valid(st)) {
                let expired = st.input.as_ref().is_some_and(|i| {
                    Instant::now() >= i.start + INPUT_LIFETIME
                        || (!i.recognizing && Instant::now() >= i.capture_deadline)
                });
                st.revoke_input(if expired { "expired" } else { "cancelled" });
                if let Some(p) = crate::lock_or_recover(&self.mic, "voice.mic").get_mut(device) {
                    p.reset();
                }
                self.note(
                    "mic_input_released",
                    json!({"device":device,"state":st.input_status}),
                );
            }
        }
    }

    pub(super) fn consume_input(
        &self,
        device: &str,
        input_id: u64,
    ) -> Option<(bool, Option<crate::session::ContextTicket>, u64)> {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get_mut(device)?;
        let input = st.input.as_ref()?;
        if input.id != input_id || !input.recognizing || input.consumed || !input.valid(st) {
            return None;
        }
        let input = st.input.as_mut()?;
        input.consumed = true;
        input.abort = None;
        Some((
            input.eligibility == "session",
            input.context,
            input.generation,
        ))
    }

    pub(super) fn reserve_input_turn(
        &self,
        device: &str,
        expected: Identity,
        debit: bool,
        input_id: u64,
        context: Option<crate::session::ContextTicket>,
    ) -> Option<TurnTicket> {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get_mut(device)?;
        let context = context?;
        if !self.consumed_input_valid_locked(device, st, input_id)
            || !st
                .input
                .as_ref()
                .is_some_and(|i| i.context == Some(context))
        {
            return None;
        }
        if crate::session::SessionStore::new(self.store.clone())
            .current_device_context(device)
            .ok()
            .flatten()?
            .ticket()
            != context
        {
            return None;
        }
        let request = st.begin_window_request(st.stop_epoch, expected, debit)?;
        st.invalidate_live_turn();
        st.follow_close_pending_at = None;
        st.window_opener = None;
        Some(TurnTicket {
            request,
            stop_epoch: st.stop_epoch,
            context: Some(context),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn pipeline() -> Arc<VoicePipeline> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        VoicePipeline::new(
            DeviceHub::new(),
            store.clone(),
            Arc::new(TaskRegistry::new(store)),
            Arc::new(|| Err("no agent".into())),
            Arc::new(|| Err("no TTS".into())),
            Arc::new(|| Err("no ASR".into())),
            std::env::temp_dir().join(id::next("input-boundary")),
        )
    }

    #[test]
    fn input_lease_capture_admission_uses_half_open_window() {
        let mut st = DeviceVoiceState::default();
        st.participation
            .activate(
                st.participation.identity(),
                FollowWindowPolicy {
                    ms: 5000,
                    ..Default::default()
                },
                CreditGrant::Full,
                1000,
            )
            .unwrap();
        assert!(!st.session_covers(999, 999, 1000));
        assert!(st.session_covers(1000, 1000, 1000));
        assert!(st.session_covers(5999, 5999, 5999));
        assert!(!st.session_covers(6000, 6000, 6000));
        assert!(!st.session_covers(6001, 6001, 6001));
    }

    #[tokio::test]
    async fn input_onset_uses_speech_permission_without_quiet_preroll_contamination() {
        for control_onset in [false, true] {
            let p = pipeline();
            p.open_participation("test", CreditGrant::Full);
            let provenance = |control| {
                let mut source = crate::mic::CaptureProvenance::legacy(1280);
                source.spans[0].control_only = control;
                source
            };
            let mut mic = MicProcessor::new(VadConfig::default());
            for _ in 0..2 {
                mic.feed_with_provenance(vec![0; 1280], 16000, provenance(true));
            }
            mic.feed_with_provenance(vec![12000; 1280], 16000, provenance(control_onset));
            p.mic.lock().unwrap().insert("test".into(), mic);
            p.start_input("test");
            let states = p.states.lock().unwrap();
            let input = states["test"].input.as_ref().unwrap();
            assert_eq!(
                input.eligibility,
                if control_onset { "control" } else { "session" }
            );
            assert_eq!(
                input.origin.samples(),
                1280,
                "only the actual onset block determines permission"
            );
            assert!(input
                .origin
                .spans
                .iter()
                .all(|s| s.control_only == control_onset));
            drop(states);
            // The DSP still retains its complete pre-roll; admission does not
            // rewrite or delete source spans to obtain the desired permission.
            let mut processors = p.mic.lock().unwrap();
            let mic = processors.get_mut("test").unwrap();
            mic.feed_with_provenance(vec![12000; 1280], 16000, provenance(false));
            assert!(matches!(
                mic.feed_with_provenance(vec![12000; 1280], 16000, provenance(false)),
                FeedOutcome::SpeechStarted
            ));
            let retained = mic.open_provenance();
            assert_eq!(retained.samples(), 3 * 1280);
            assert_eq!(
                retained.spans[0].control_only, control_onset,
                "actual control-only speech remains in the segment"
            );
        }
    }

    #[tokio::test]
    async fn input_lease_first_loud_block_survives_confirmation_crossing_window_expiry() {
        let p = pipeline();
        p.settings.lock().unwrap().follow_window_enabled = true;
        p.open_participation("test", CreditGrant::Full);
        let feed = || {
            p.feed_chunk(
                "test".into(),
                vec![12000; 1280],
                16000,
                1,
                id::now_ms(),
                Default::default(),
            )
        };
        feed();
        let original = {
            let mut states = p.states.lock().unwrap();
            let st = states.get_mut("test").unwrap();
            let i = st.input.as_ref().unwrap();
            assert!(!i.confirmed);
            let original = i.id;
            let closed_at = id::now_ms() + 10_000;
            assert!(!st.session_open(closed_at));
            assert!(
                !st.timeout_session(closed_at),
                "already reserved onset must survive the timeout sweep"
            );
            original
        };
        feed();
        feed();
        let states = p.states.lock().unwrap();
        let st = &states["test"];
        let i = st.input.as_ref().unwrap();
        assert_eq!(i.id, original);
        assert!(i.confirmed);
        assert!(i.valid(st));
        assert!(
            !st.session_open(i.start_ms + 10_000),
            "the pending input does not reopen eligibility"
        );
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    use crate::api::AppState;
    use tokio::sync::{mpsc, oneshot};

    async fn cancelled_handoff(stage: &'static str, cause: &'static str) {
        let (asr_tx, mut asr_rx) = mpsc::unbounded_channel();
        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let llm_prompts = prompts.clone();
        let remote=axum::Router::new()
            .route("/v1/audio/transcriptions",axum::routing::post(move |body:bytes::Bytes| {
                let tx=asr_tx.clone(); async move {
                    assert!(body.windows(4).any(|w|w==b"RIFF"));
                    let (release,rx)=oneshot::channel::<String>();tx.send(release).unwrap();
                    axum::Json(json!({"text":rx.await.unwrap()}))
                }
            }))
            .route("/v1/chat/completions",axum::routing::post(move |axum::Json(body):axum::Json<serde_json::Value>| {
                let prompts=llm_prompts.clone();async move {
                    assert!(body["messages"].is_array());prompts.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                    ([("content-type","text/event-stream")],"data: {\"choices\":[{\"delta\":{\"content\":\"收到。\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, remote).await.unwrap() });
        let state = AppState::with_overrides(
            Arc::new(Store::open_in_memory().unwrap()),
            std::env::temp_dir().join(id::next("input-handoff")),
            crate::tool::mocks::MockInject::default(),
            Some(Arc::new(
                crate::llm::deepseek::DeepSeekClient::new(&url, "").unwrap(),
            )),
            Some(Arc::new(crate::speech::MockTts::default())),
            Some(Arc::new(crate::speech::OpenaiAsr::new(
                url,
                String::new(),
                "handoff".into(),
            ))),
            None,
        );
        // Register a synthetic Gateway through the real loopback transport so
        // management operations exercise the same known-device checks.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        state.start_gateway_listener(([127, 0, 0, 1], port).into());
        let mut gateway = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                    break stream;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let hello = json!({"type":"hello","protocol":0,"min_protocol":0,"gateway":"fake-handoff","gateway_version":"0.0.1","devices":[{"stable_id":"test","model":"synthetic","capabilities":["audio_in","audio_out","stop"],"audio_profile":"synthetic"}]});
        gateway
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        let mut hello_reply = String::new();
        tokio::io::BufReader::new(&mut gateway)
            .read_line(&mut hello_reply)
            .await
            .unwrap();
        assert!(hello_reply.contains("hello_ack"));
        let router = crate::api::router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        client
            .put(format!("{base}/api/v0/mode/test"))
            .json(&json!({"follow_window_enabled":true,"followup_credits":0,
            "asr_wake_fallback":true,"tail_blind_ms":0,"wake_ack":{"enabled":false}}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let p = &state.voice;
        if stage == "in_window" {
            p.open_participation("test", CreditGrant::Full);
            let expected = p.states.lock().unwrap()["test"].participation.identity();
            p.activate_session_at("test", CreditGrant::Full, expected, "protocol_wake")
                .unwrap();
        }
        let (entered_tx, entered_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        *p.input_handoff_hook.lock().unwrap() = Some((stage, entered_tx, resume_rx));
        let origin = |epoch: &str, start: u64| crate::gateway::InputProvenance {
            origin: crate::gateway::InputOrigin::Audio {
                capture_epoch: epoch.into(),
                ticket: "same-ticket".into(),
                input_id: "same-input".into(),
                start_sample: start,
                end_sample: start + 1280,
            },
            transport: None,
        };
        for block in 0..18 {
            p.feed_chunk(
                "test".into(),
                vec![if block < 10 { 12000 } else { 0 }; 1280],
                16000,
                1,
                id::now_ms(),
                origin("old-capture", block * 1280),
            );
        }
        let release = tokio::time::timeout(Duration::from_secs(5), asr_rx.recv())
            .await
            .unwrap()
            .unwrap();
        release
            .send("你好高达，REVOKED_HANDOFF_SENTINEL".into())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let generation = p.states.lock().unwrap()["test"].participation.generation();
        assert!(
            p.states.lock().unwrap()["test"]
                .input
                .as_ref()
                .unwrap()
                .consumed,
            "barrier is after real HTTP ASR consumption"
        );
        match cause {
            "capture" => p.feed_chunk(
                "test".into(),
                vec![0; 1280],
                16000,
                1,
                id::now_ms(),
                origin("new-capture", 0),
            ),
            "expiry" => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(61)).await;
                tokio::time::resume();
                p.sweep_inputs();
            }
            "stop" => {
                client
                    .post(format!("{base}/api/v0/devices/test/stop"))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            p.states.lock().unwrap()["test"].input.is_none(),
            "exact consumed input was revoked"
        );
        resume_tx.send(()).unwrap();
        // Completion barrier follows the production handler's final cleanup.
        tokio::time::timeout(Duration::from_secs(5), p.input_handoff_finished.notified())
            .await
            .unwrap();
        let mode: serde_json::Value = client
            .get(format!("{base}/api/v0/mode"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let device = mode["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == "test")
            .unwrap();
        if stage == "fallback" {
            assert_eq!(
                device["session"]["state"], "idle",
                "cancelled fallback cannot reopen Listening: {mode}"
            );
            assert_eq!(
                device["input_window_open"], false,
                "cancelled fallback cannot grant a new input window"
            );
        }
        assert_eq!(
            device["session"]["generation"], generation,
            "revoked input cannot activate another generation"
        );
        assert!(
            !p.recent(100).iter().any(|v| v["kind"] == "request_started"),
            "no new request may be reserved"
        );
        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no actual HTTP LLM prompt"
        );
    }
    #[tokio::test]
    async fn input_handoff_fallback_capture_revoke_stays_closed() {
        cancelled_handoff("fallback", "capture").await;
    }
    #[tokio::test]
    async fn input_handoff_fallback_expiry_stays_closed() {
        cancelled_handoff("fallback", "expiry").await;
    }
    #[tokio::test]
    async fn input_handoff_in_window_capture_revoke_cannot_reactivate() {
        cancelled_handoff("in_window", "capture").await;
    }
    #[tokio::test]
    async fn input_handoff_fallback_stop_stays_closed() {
        cancelled_handoff("fallback", "stop").await;
    }
}

#[cfg(test)]
#[path = "input_wake_tests.rs"]
mod wake_tests;
