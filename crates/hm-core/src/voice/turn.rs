use super::*;

impl VoicePipeline {
    /// Execute a session-end intent: stop ALL playback (voice+media,
    /// task cancel, epoch bump — the stop-button semantics), close the
    /// follow window immediately (no 30 s timeout tone afterwards), and
    /// answer with the local retreat ack when its WAV is ready.
    #[cfg(test)]
    pub(super) fn retreat_session(self: &Arc<Self>, device: &str, text: &str, via: &str) {
        let expected = crate::lock_or_recover(&self.states, "voice.states")
            .entry(device.to_string())
            .or_default()
            .participation
            .identity();
        self.retreat_session_at(device, text, via, expected);
    }

    /// The management close checks generation in the same owner lock as revocation.
    pub fn close_session(
        self: &Arc<Self>,
        device: &str,
        generation: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        self.retreat_session_guarded(
            device,
            "",
            "console",
            SessionCloseGuard::Management(generation),
        )
    }

    pub fn model_close_session(
        self: &Arc<Self>,
        ctx: &crate::tool::ToolCtx,
        confirm: bool,
    ) -> Result<serde_json::Value, String> {
        let task = self
            .tasks
            .get(&ctx.task_id)
            .ok_or("session.close requires a device request")?;
        let device = ctx
            .authority()
            .and_then(|a| a.physical_device_id())
            .ok_or("session.close requires a device request")?;
        ctx.authority().unwrap().bind_session(&task.session_id)?;
        self.retreat_session_guarded(
            device,
            "",
            "model",
            SessionCloseGuard::Model {
                flag: ctx.cancel_flag(),
                confirm,
                call_id: ctx.call_id.clone(),
            },
        )
    }

    pub(super) fn retreat_session_at(
        self: &Arc<Self>,
        device: &str,
        text: &str,
        via: &str,
        expected: Identity,
    ) -> bool {
        self.retreat_session_guarded(device, text, via, SessionCloseGuard::Identity(expected))
            .is_ok_and(|r| r["closed"] == true)
    }

    /// Silent arbitration-late revocation: closes the listening window,
    /// extinguishes the wake LED, and cancels any unvoiced in-flight task
    /// WITHOUT speaking `retreat_ack`, wiping `sessions`, or discarding paused
    /// music / program snapshots.
    pub(super) fn retreat_session_silent(
        &self,
        device: &str,
        via: &str,
        expected: Identity,
    ) -> bool {
        let now = id::now_ms();
        let stop = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            if !st
                .participation
                .close(expected, CloseReason::ExplicitExit, now)
            {
                return false;
            }
            st.invalidate_live_turn();
            st.follow_open_last_tick = false;
            st.follow_close_pending_at = None;
            st.window_opener = None;
            st.barge.reset();
            st.suppress_cue_until_ms = None;
            st.last_wake = None;
            st.wake_flash_until_ms = 0;
            if let Some(arc) = self.mic_arc(device) {
                crate::lock_or_recover(&arc, "voice.mic.dev").reset();
            }
            self.apply_capture_revocation_locked(device, st);
            self.stop_device_locked(device, st, true)
        };
        self.push_assistant_state(device, false);
        self.note(
            "session_closed",
            json!({"device":device,"via":via,"reason":"arbitration_late_silent"}),
        );
        self.note(
            "session_retreat",
            json!({ "device": device, "text": "", "via": via, "silent": true,
                    "stop_requests_sent": stop.get("stop_requests_sent").cloned().unwrap_or(json!(0)) }),
        );
        true
    }

    pub(super) fn retreat_session_guarded(
        self: &Arc<Self>,
        device: &str,
        text: &str,
        via: &str,
        guard: SessionCloseGuard,
    ) -> Result<serde_json::Value, String> {
        let now = id::now_ms();
        let room = self.room_of(device);
        let hub_live = self.hub_live(device);
        let media_present = self.music().is_some_and(|m| m.media_present(device));
        if matches!(&guard, SessionCloseGuard::Identity(_)) {
            let explicit_room = crate::sovereignty::is_explicit_room_addressed(text, &room);
            let local_has_active_work = hub_live.voice
                || hub_live.media
                || media_present
                || crate::lock_or_recover(&self.states, "voice.states")
                    .get(device)
                    .is_some_and(|st| {
                        st.current_task.is_some()
                            || st.live_program.is_some()
                            || st.paused_program.is_some()
                            || st.outputs.values().any(|o| stage_is_live(&o.stage))
                    });
            if !local_has_active_work {
                if let crate::sovereignty::RetreatAuth::DeniedNotOwner {
                    owner_device,
                    owner_room,
                } = self
                    .sovereignty
                    .can_retreat(&room, device, explicit_room, now)
                {
                    self.note(
                        "retreat_ignored_not_owner",
                        json!({
                            "device": device,
                            "room": room,
                            "owner_device": owner_device,
                            "owner_room": owner_room,
                            "via": via,
                            "text": text,
                        }),
                    );
                    return Ok(json!({
                        "closed": false,
                        "ignored": "not_owner",
                        "owner_device": owner_device,
                        "owner_room": owner_room,
                    }));
                }
            }
        }
        let model = matches!(&guard, SessionCloseGuard::Model { .. });
        let extraction_authority = crate::execution::Authority::internal(
            self.store.clone(),
            crate::working_memory::ScopeContext {
                device_id: Some(device.into()),
                person_id: None,
                room: None,
            },
        )?;
        let has_inline_wake = matches!(via, "transcript" | "utterance")
            && self
                .effective_wake_words_for(device)
                .iter()
                .any(|w| mic::wake_span_near(text, w).is_some());
        let (stop, discarded, cleared, transcript, media, permit, snapshot, should_speak_ack) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            let had_active_or_playback = has_inline_wake
                || hub_live.voice
                || hub_live.media
                || media_present
                || st.session_open(now)
                || st.participation.session.state == SessionState::Listening
                || st.participation.request_running()
                || st.current_task.is_some()
                || st.live_program.is_some()
                || st.paused_program.is_some()
                || st.outputs.values().any(|o| stage_is_live(&o.stage))
                || st
                    .local_stop
                    .as_ref()
                    .is_some_and(|(_, at)| now.saturating_sub(*at) <= LOCAL_STOP_TRUST_MS);
            if matches!(via, "keyword" | "transcript") && !had_active_or_playback {
                let session = st.participation.snapshot();
                drop(states);
                self.note("keyword_ignored", json!({"device":device,"keyword":text,"via":via,"reason":"reflex_precondition:no_session"}));
                return Ok(
                    json!({"session":session,"closed":false,"ignored":"no_active_session_or_playback"}),
                );
            }
            let expected = match &guard {
                SessionCloseGuard::Identity(expected) => *expected,
                SessionCloseGuard::Management(generation) => {
                    if generation.is_some_and(|g| g != st.participation.generation()) {
                        return Err("generation_mismatch".into());
                    }
                    st.participation.identity()
                }
                SessionCloseGuard::Model { flag, confirm, .. } => {
                    if flag.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err("stale_request".into());
                    }
                    let request = st
                        .participation
                        .request_for_flag(flag)
                        .ok_or("stale_request")?;
                    if *confirm
                        && !st.participation.close_confirmation.is_some_and(|r| {
                            r.generation == request.generation && r.request_id != request.request_id
                        })
                    {
                        st.participation.close_confirmation = Some(request);
                        let session = st.participation.snapshot();
                        drop(states);
                        self.note(
                            "session_close_tool",
                            json!({"device":device,"confirmed":false}),
                        );
                        return Ok(json!({"session":session,"confirmed":false,"closed":false,
                            "rendered_answer":"要结束对话吗？", "replace_answer":true}));
                    }
                    st.participation.identity()
                }
            };
            if !st
                .participation
                .close(expected, CloseReason::ExplicitExit, now)
            {
                if matches!(&guard, SessionCloseGuard::Identity(_)) {
                    return Err("stale_or_closed".into());
                }
                return Ok(json!({"session":st.participation.snapshot(),"closed":false}));
            }
            st.invalidate_live_turn();
            st.follow_open_last_tick = false;
            st.follow_close_pending_at = None;
            st.window_opener = None;
            st.barge.reset();
            st.suppress_cue_until_ms = None;
            st.last_wake = None;
            if let Some(arc) = self.mic_arc(device) {
                crate::lock_or_recover(&arc, "voice.mic.dev").reset();
            }
            self.apply_capture_revocation_locked(device, st);
            let discarded = st.live_program.take().is_some() | st.paused_program.take().is_some();
            let stop = self.stop_device_locked(device, st, true);
            let sessions = crate::session::SessionStore::new(self.store.clone());
            let session_id = sessions.device_session_id(device)?;
            self.tasks.cancel_cross_turn(&session_id);
            let transcript = Self::memory_transcript(&sessions, &session_id);
            let (session_id, cleared) = sessions
                .archive_and_rotate_device_context(device, now)
                .unwrap_or((session_id, 0));
            // Drop an established paused/playing media session. In-flight
            // fetch cancellation is intentionally still D2a.
            let media = self.music().and_then(|m| m.stop_now(device, "retreat"));
            let permit = OutputPermit {
                generation: st.participation.generation(),
                stop_epoch: st.stop_epoch,
                request: None,
            };
            let snapshot = st.participation.snapshot();
            if let SessionCloseGuard::Model {
                confirm, call_id, ..
            } = &guard
            {
                // Close owns this receipt: the original task has been revoked.
                // Write a complete exchange after clear, before another activation.
                let call = crate::llm::WireToolCall {
                    id: call_id.clone(),
                    name: "session_close".into(),
                    arguments: json!({"confirm":confirm}).to_string(),
                };
                let receipt =
                    json!({"closed":true,"confirmed":true,"scope":"device","session":snapshot,
                    "detail":"核心会话已结束；设备停播以回执为准"})
                    .to_string();
                let _ = sessions.append_wire(
                    &session_id,
                    "assistant",
                    "[调用 session.close]",
                    &crate::llm::WireMessage::assistant_calls(vec![call]),
                );
                let _ = sessions.append_wire(
                    &session_id,
                    "tool",
                    "[回执 session.close] 核心会话已结束；设备停播以回执为准",
                    &crate::llm::WireMessage::tool_result(call_id.clone(), receipt),
                );
            }
            (
                stop,
                discarded,
                cleared,
                transcript,
                media,
                permit,
                snapshot,
                had_active_or_playback,
            )
        };
        self.release_sovereignty(device, now, true);
        self.start_memory_extraction_from(
            device,
            "retreat",
            Some(transcript),
            Some(extraction_authority),
        );
        self.push_assistant_state(device, false);
        let manifest = self.load_ack_manifest();
        let ack_phrase = crate::lock_or_recover(&self.settings, "voice.settings")
            .retreat_ack
            .clone();
        if model {
            let pipe = self.clone();
            let device = device.to_string();
            tokio::spawn(async move {
                pipe.speak_inner_at(
                    &device,
                    &format!("会话已结束。{ack_phrase}"),
                    None,
                    "session_close_fact",
                    false,
                    permit,
                )
                .await;
            });
        } else if should_speak_ack && self.ack_ready(&ack_phrase, &manifest) {
            self.play_local_ack_at(device, &ack_phrase, "retreat_ack", permit);
        }
        if model {
            self.note(
                "session_close_tool",
                json!({"device":device,"confirmed":true}),
            );
        }
        self.note(
            "session_closed",
            json!({"device":device,"via":via,"reason":"explicit_exit"}),
        );
        self.note(
            "session_retreat",
            json!({ "device": device, "text": text, "via": via,
                    "program_discarded": discarded, "media_discarded": media,
                    "session_messages_cleared": cleared,
                    "stop_requests_sent": stop.get("stop_requests_sent").cloned().unwrap_or(json!(0)) }),
        );
        Ok(json!({"session":snapshot,"closed":true,"confirmed":true}))
    }

    pub(super) fn open_participation(&self, device: &str, grant: CreditGrant) -> Identity {
        let now = id::now_ms();
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let policy = FollowWindowPolicy::for_device(
            &crate::lock_or_recover(&self.settings, "voice.settings"),
            device,
        );
        let st = states.entry(device.to_string()).or_default();
        st.invalidate_live_turn();
        let expected = st.participation.identity();
        let identity = st
            .participation
            .activate(expected, policy, grant, now)
            .unwrap();
        st.follow_close_pending_at = None;
        st.window_opener = None;
        identity
    }

    pub(super) fn activate_session_at(
        &self,
        device: &str,
        grant: CreditGrant,
        expected: Identity,
        opened_by: &'static str,
    ) -> Option<Identity> {
        let (policy, context_ms) = self.context_policy_for(device);
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get_mut(device)?;
        st.invalidate_live_turn();
        let identity = st
            .participation
            .activate(expected, policy, grant, id::now_ms())?;
        self.admit_context_locked(device, st, id::now_ms(), context_ms, true)?;
        st.participation.opened_by = Some(opened_by);
        st.follow_close_pending_at = None;
        st.window_opener = None;
        Some(identity)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn context_policy(&self) -> (FollowWindowPolicy, u64) {
        self.context_policy_for("global")
    }

    pub(super) fn context_policy_for(&self, device: &str) -> (FollowWindowPolicy, u64) {
        let settings = crate::lock_or_recover(&self.settings, "voice.settings");
        let eff = settings.effective_for(device);
        (FollowWindowPolicy::from(&eff), eff.context_window_ms)
    }
    pub(super) fn accept_context_transcript(&self, device: &str, input_id: u64) -> bool {
        let captured = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| st.input.as_ref())
            .filter(|i| i.id == input_id)
            .and_then(|i| i.context);
        if let Some(context) = captured {
            let current = crate::session::SessionStore::new(self.store.clone())
                .current_device_context(device)
                .ok()
                .flatten();
            if current.as_ref().map(|c| c.ticket()) != Some(context) {
                self.note(
                    "stale_context_transcript_rejected",
                    json!({"device":device,"input_id":input_id,"context":context}),
                );
                return false;
            }
        }
        true
    }

    pub(super) fn admit_context_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
        now: u64,
        window_ms: u64,
        rotate: bool,
    ) -> Option<crate::session::ContextTicket> {
        let context = match crate::session::SessionStore::new(self.store.clone())
            .admit_device_context(device, now, window_ms, rotate)
        {
            Ok(context) => context,
            Err(error) => {
                self.note(
                    "context_admission_failed",
                    json!({"device":device,"error":error}),
                );
                return None;
            }
        };
        // A KWS winner can authorize its already-open current capture. ASR
        // dispatched before this wake retains its original context ticket.
        if self
            .mic_arc(device)
            .is_some_and(|arc| crate::lock_or_recover(&arc, "voice.mic.dev").in_speech())
        {
            Self::bind_capture_input(st, context.ticket());
        } else if st.input.is_some() {
            st.revoke_input("cancelled");
        }
        Some(context.ticket())
    }

    pub(super) fn permit_for_identity(
        &self,
        device: &str,
        identity: Identity,
    ) -> Option<OutputPermit> {
        let states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get(device)?;
        (st.participation.identity() == identity).then_some(OutputPermit {
            generation: st.participation.generation(),
            stop_epoch: st.stop_epoch,
            request: None,
        })
    }

    pub(super) fn reserve_turn(&self, device: &str, expected: Identity) -> Option<TurnTicket> {
        self.reserve_turn_with_credit(device, expected, false)
    }

    pub(super) fn debit_control(&self, device: &str, identity: &mut Identity) -> bool {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let Some(st) = states.get_mut(device) else {
            return false;
        };
        let Some(next) = st.participation.debit_control(*identity) else {
            return false;
        };
        *identity = next;
        true
    }

    pub(super) fn reserve_turn_with_credit(
        &self,
        device: &str,
        expected: Identity,
        debit: bool,
    ) -> Option<TurnTicket> {
        let now = id::now_ms();
        let context_ms = crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .context_window_ms;
        let ticket = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get_mut(device)?;
            if !st.session_open(now) {
                return None;
            }
            let request = st.begin_window_request(st.stop_epoch, expected, debit)?;
            st.invalidate_live_turn();
            st.follow_close_pending_at = None;
            st.window_opener = None;
            let context = crate::session::SessionStore::new(self.store.clone())
                .admit_device_context(device, now, context_ms, false)
                .ok()?
                .ticket();
            TurnTicket {
                request,
                stop_epoch: st.stop_epoch,
                context: Some(context),
            }
        };
        self.renew_sovereignty(device, now);
        Some(ticket)
    }

    /// Was this window opened by the spotter and not yet used by a turn?
    /// Consumes the marker: only the FIRST ASR segment of a keyword-opened
    /// window may claim the bare-wake ack.
    pub(super) fn take_keyword_window(&self, device: &str) -> bool {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let Some(st) = states.get_mut(device) else {
            return false;
        };
        let opened = st.window_opener == Some("keyword");
        st.window_opener = None;
        opened
    }

    /// This device's room, for the arbitration record. `device-meta` is the
    /// one place a room is configured (user 2026-11-15: room is a DEVICE fact,
    /// not a hand-written memory), so the claim reads it instead of holding a
    /// second copy.
    /// Run one conversational turn for a device utterance, then speak the
    /// Pull the first speakable segment out of `buf`: a hard sentence
    /// boundary (。！？!?；;\n) or, failing that, a soft-break valve at
    /// ~60 chars (，、,。 space / end) so a long comma-spliced answer
    /// still starts speaking early. None = nothing speakable yet.
    pub fn take_speak_segment(buf: &mut String) -> Option<String> {
        const HARD: &str = "。！？!?；;\n";
        const SOFT: &str = "，、,. ";
        let chars: Vec<char> = buf.chars().collect();
        let mut cut: Option<usize> = None;
        for (i, c) in chars.iter().enumerate() {
            if HARD.contains(*c) {
                cut = Some(i + 1);
                break;
            }
        }
        if cut.is_none() && chars.len() >= 60 {
            // Valve: speak EARLY — the first soft break past ~24 chars
            // (taking the LAST soft break would wait the whole valve
            // window and defeat the first-sound latency this exists for).
            cut = (24..chars.len())
                .find(|&i| SOFT.contains(chars[i]))
                .map(|i| i + 1)
                .or(Some(chars.len()));
        }
        let n = cut?;
        let head: String = chars[..n].iter().collect();
        let tail: String = chars[n..].iter().collect();
        let head = head.trim().to_string();
        *buf = tail;
        if head.is_empty() {
            None
        } else {
            Some(head)
        }
    }

    /// One streaming TTS segment (v0.3/T3): synthesize incrementally and
    /// forward raw PCM chunks to the device stream while synthesis runs.
    /// Opens the stream lazily on first use. Returns false when the caller
    /// must give up streaming (error/stop/cancel) — the classic whole-play
    /// path at `Answer` is the safety net.
    pub(super) async fn speak_stream_segment(
        &self,
        device: &str,
        segment: &str,
        stream: &mut Option<TurnStream>,
        task_id: &Option<String>,
        turn_epoch: u64,
        turn_id: u64,
        request: Option<RequestTicket>,
    ) -> bool {
        let Some(permit) = self.output_permit(device, request) else {
            return false;
        };
        // Same suppression rules as speak(): before synthesis. The epoch is
        // the TURN's, captured before the agent loop — NOT re-read here. A
        // barge-in that lands between two sentences bumps it, and re-reading
        // it per sentence made every later sentence look "fresh" (the
        // 2026-11-09 stutter: answer cut, next sentence plays, cut, …).
        let is_live = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| st.live_turn == Some(turn_id));
        if !is_live
            || self.output_permit(device, request).is_none()
            || self.task_cancelled(task_id)
            || self.stop_epoch_of(device) != turn_epoch
        {
            return false;
        }
        if !Self::speakable(segment) {
            self.note(
                "tts_segment_skipped",
                json!({ "device": device, "chars": segment.chars().count(), "segment": segment }),
            );
            return true; // nothing to say here, but the turn goes on
        }
        let started_epoch = turn_epoch;
        let tts = match (self.tts_factory)() {
            Ok(t) => t,
            Err(e) => {
                self.note("tts_unconfigured", json!({ "device": device, "reason": e }));
                return false;
            }
        };
        let seg_start = id::now_ms();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::speech::TtsPiece>();
        let synth = tts.synthesize_streaming(segment, &tx);
        tokio::pin!(synth);
        // Send pacing (2026-11-10): synthesis runs ~2x realtime, so an
        // unpaced forwarder outruns playback without bound and eventually
        // trips the firmware's ahead-of-playback cap — a story-length
        // answer died mid-word at the 10s cap (0.0.7) and again at the
        // 60s backstop (0.0.8, real user round). The core now holds
        // synthesized pieces in a local queue and forwards only while
        // ahead-of-playback (estimated as elapsed wall time since the
        // first send) stays under TTS_AHEAD_LIMIT_S. The firmware cap
        // remains as the memory backstop; stop semantics are unchanged
        // (hub.stop flushes the device's pending side).
        let mut queue: VecDeque<crate::speech::TtsPiece> = VecDeque::new();
        let mut synth_done = false;
        // Pre-open accounting only: as soon as the stream exists, the
        // per-STREAM counters in TurnStream take over (the budget must not
        // restart per sentence).
        let pre_open_secs: f64 = 0.0;
        let pre_open_from_ms: Option<u64> = None;
        let mut paced_wait: Option<tokio::time::Instant> = None;
        loop {
            // Forward buffered pieces while within the ahead budget.
            while let Some(piece) = queue.front() {
                let piece_secs = piece.pcm.len() as f64 / 2.0 / piece.sample_rate as f64;
                let (sent_secs, from_ms) = match stream.as_ref() {
                    Some(ts) => (ts.sent_secs, ts.paced_from_ms),
                    None => (pre_open_secs, pre_open_from_ms),
                };
                let played_secs = from_ms
                    .map(|f0| id::now_ms().saturating_sub(f0) as f64 / 1000.0)
                    .unwrap_or(0.0);
                let paced =
                    stream.is_some() && sent_secs + piece_secs - played_secs > TTS_AHEAD_LIMIT_S;
                if paced {
                    // Re-check the budget when the oldest queued piece
                    // would just fit: played_secs grows 1s per second.
                    let need = sent_secs + piece_secs - played_secs - TTS_AHEAD_LIMIT_S;
                    paced_wait = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_secs_f64(need.max(0.05)),
                    );
                    break;
                }
                let piece = queue.pop_front().unwrap();
                // Stop-epoch / cancel re-check on every piece: a stopped
                // stream must never keep feeding the speaker.
                let cancelled = self.task_cancelled(task_id);
                let (epoch_now, live_ok) = crate::lock_or_recover(&self.states, "voice.states")
                    .get(device)
                    .map(|st| (st.stop_epoch, st.live_turn == Some(turn_id)))
                    .unwrap_or((started_epoch, false));
                if cancelled || epoch_now != started_epoch || !live_ok {
                    if let Some(ts) = stream.take() {
                        self.fail_voice_stream(&ts.play_id);
                        self.note(
                            "tts_stream_suppressed",
                            json!({
                                "device": device, "task": task_id, "play": ts.play_id,
                                "stage": "chunk forwarding", "epoch": [started_epoch, epoch_now],
                            }),
                        );
                    }
                    return false;
                }
                // Open the stream lazily on the FIRST piece — the rate
                // rides the piece precisely so this needs no provider
                // metadata promise.
                if stream.is_none() {
                    // B-M0: the answer's first audio replaces the waiting
                    // cue. Cancelled BEFORE the stream opens so this piece
                    // counts as a new voice play (re-arms the D/T5 R1
                    // answer protection). R1j/P0-a: the device flushes each
                    // play on its own, so WAIT for that flush — submitting
                    // here used to run the cue's tail under our first
                    // syllable.
                    self.wait_filler_finish(device, FILLER_COMPLETE_WAIT_MS)
                        .await;
                    let (_, filler_wait) =
                        self.cancel_filler_tracked_at(device, "answer_started", Some(permit));
                    self.settle_filler(device, "answer_started", filler_wait)
                        .await;
                    // The wait is a suspension point: a stop that landed in
                    // it must still win over the audio we are about to open.
                    let stale_after_wait =
                        self.task_cancelled(task_id) || self.stop_epoch_of(device) != started_epoch;
                    if stale_after_wait {
                        self.note(
                            "tts_suppressed_after_stop_epoch",
                            json!({
                                "device": device, "task": task_id,
                                "stage": "after filler settle",
                                "started_epoch": started_epoch,
                                "current_epoch": self.stop_epoch_of(device),
                            }),
                        );
                        return false;
                    }
                    let submitted = {
                        let protect_ms = crate::lock_or_recover(&self.settings, "voice.settings")
                            .effective_for(device)
                            .barge_protect_ms
                            .min(3_000);
                        let mut states = crate::lock_or_recover(&self.states, "voice.states");
                        if !states.get(device).is_some_and(|st| {
                            st.live_turn == Some(turn_id)
                                && st.stop_epoch == turn_epoch
                                && request.is_none_or(|r| st.participation.is_active_request(r))
                        }) {
                            return false;
                        }
                        self.hub
                            .play_begin(
                                device,
                                "voice",
                                piece.sample_rate,
                                Some(segment.to_string()),
                            )
                            .map(|ticket| {
                                if let Some(st) = states.get_mut(device) {
                                    Self::note_voice_play_submitted_locked(
                                        st,
                                        id::now_ms(),
                                        protect_ms,
                                    );
                                    Self::register_stream_output(
                                        st,
                                        &ticket.play_id,
                                        task_id,
                                        id::now_ms(),
                                        request,
                                    );
                                    if let Some(rec) = st.outputs.get_mut(&ticket.play_id) {
                                        rec.answer_text = segment.to_string();
                                    }
                                }
                                ticket
                            })
                    };
                    match submitted {
                        Ok(ticket) => {
                            let pid = ticket.play_id.clone();
                            // Audit Q5: a stop can land between the epoch
                            // check above and play_begin. The stream is now
                            // registered, so compensate exactly like the
                            // whole-WAV path: send Stop for THIS play. A
                            // one-piece stream would otherwise escape (the
                            // first chunk is forwarded without another
                            // epoch check).
                            if self.task_cancelled(task_id)
                                || self.stop_epoch_of(device) != started_epoch
                            {
                                self.fail_voice_stream(&pid);
                                self.note(
                                    "tts_stream_stopped_after_submit",
                                    json!({
                                        "device": device, "task": task_id, "play": pid,
                                        "started_epoch": started_epoch,
                                        "current_epoch": self.stop_epoch_of(device),
                                    }),
                                );
                                return false;
                            }
                            *stream = Some(TurnStream {
                                play_id: pid,
                                seq: 0,
                                first_forward_ms: None,
                                sent_secs: pre_open_secs,
                                paced_from_ms: pre_open_from_ms,
                                _ticket: ticket,
                            });
                        }
                        Err(e) => {
                            self.note(
                                "tts_stream_begin_failed",
                                json!({ "device": device, "error": e }),
                            );
                            return false;
                        }
                    }
                }
                let Some(ts) = stream.as_mut() else {
                    return false;
                };
                if ts.first_forward_ms.is_none() {
                    ts.first_forward_ms = Some(id::now_ms().saturating_sub(seg_start));
                }
                if ts.paced_from_ms.is_none() {
                    ts.paced_from_ms = Some(id::now_ms());
                }
                let sent = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let Some(st) = states.get_mut(device) else {
                        return false;
                    };
                    if st.live_turn != Some(turn_id)
                        || st.stop_epoch != turn_epoch
                        || !request.is_none_or(|r| st.participation.is_active_request(r))
                    {
                        return false;
                    }
                    if let Some(rec) = st.outputs.get_mut(&ts.play_id) {
                        if !rec.answer_text.ends_with(segment) {
                            rec.answer_text.push_str(segment);
                        }
                    }
                    self.hub.play_chunk(&ts.play_id, ts.seq, &piece.pcm)
                };
                if sent.is_err() {
                    let dead = stream.take();
                    if let Some(d) = dead {
                        self.fail_voice_stream(&d.play_id);
                    }
                    return false;
                }
                ts.seq += 1;
                ts.sent_secs += piece_secs;
                paced_wait = None;
            }
            // Everything forwarded and synthesis finished: done.
            if queue.is_empty() && synth_done {
                return true;
            }
            // Wait for more pieces, synthesis completion, or the pacing
            // deadline — whichever comes first.
            let deadline = paced_wait;
            tokio::select! {
                piece = rx.recv(), if !synth_done => {
                    if let Some(piece) = piece {
                        queue.push_back(piece);
                    }
                }
                res = &mut synth, if !synth_done => {
                    match res {
                        Ok(_audio) => {
                            // Drain anything the provider queued during the
                            // final poll into the queue (tx is still alive
                            // here, so recv Some/None both end below). The
                            // default TtsClient impls push their single
                            // piece just before returning — this must not
                            // drop it.
                            while let Ok(piece) = rx.try_recv() {
                                queue.push_back(piece);
                            }
                            synth_done = true;
                        }
                        Err(e) => {
                            // R1f: a failure caused by OUR OWN stop/barge is
                            // expected, not an error to show. Only a turn that
                            // is still valid may flash the ring red (the user
                            // saw red after 退下, 2026-11-12).
                            let still_valid = !self.task_cancelled(task_id)
                                && self.stop_epoch_of(device) == turn_epoch;
                            if still_valid {
                                self.flash_error(device);
                            }
                            // What was actually sent matters: a TTS error is
                            // only diagnosable with the text and its size.
                            self.note("tts_failed", json!({
                                "device": device, "error": e.to_string(), "streaming": true,
                                "cancelled": !still_valid,
                                "chars": segment.chars().count(),
                                "segment": segment.chars().take(40).collect::<String>(),
                            }));
                            // A mid-stream synthesis failure cuts THIS
                            // segment's stream. The next sentence gets its own
                            // attempt: one unreadable sentence must not silence
                            // the whole answer (device log 2026-11-12: a long
                            // joke with quoted dialogue produced 8 failures and
                            // no speech at all).
                            if let Some(ts) = stream.take() {
                                self.fail_voice_stream(&ts.play_id);
                            }
                            return true;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or(tokio::time::Instant::now() + std::time::Duration::from_secs(3600))), if deadline.is_some() => {}
            }
        }
    }

    /// Speak ONE already-admitted segment on the device stream and move the
    /// program cursor to it (the resume point for 「继续」). Used by both the
    /// model's tokens and the core's own fact sentences, so the two can never
    /// disagree about what has been heard.
    pub(super) async fn stream_segment_and_advance(
        &self,
        device: &str,
        seg: &str,
        tstream: &mut Option<TurnStream>,
        task: &Option<String>,
        turn_epoch: u64,
        turn_id: u64,
        spoken_end: &mut usize,
        request: Option<RequestTicket>,
    ) -> bool {
        let from = {
            let mut st = crate::lock_or_recover(&self.states, "voice.states");
            match st
                .get_mut(device)
                .filter(|st| {
                    st.live_turn == Some(turn_id)
                        && st.stop_epoch == turn_epoch
                        && request.is_none_or(|r| st.participation.is_active_request(r))
                })
                .and_then(|s| s.live_program.as_mut())
            {
                Some(p) => {
                    let from = Self::clamp_boundary(&p.text, *spoken_end);
                    p.text[from..].find(seg).map(|i| from + i).unwrap_or(from)
                }
                None => Self::clamp_boundary("", 0),
            }
        };
        let keep = self
            .speak_stream_segment(device, seg, tstream, task, turn_epoch, turn_id, request)
            .await;
        if keep || tstream.is_some() {
            *spoken_end += seg.len();
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                .get_mut(device)
                .filter(|st| {
                    st.live_turn == Some(turn_id)
                        && st.stop_epoch == turn_epoch
                        && request.is_none_or(|r| st.participation.is_active_request(r))
                })
            {
                if let Some(p) = st.live_program.as_mut() {
                    p.cursor = from.min(p.text.len());
                }
            }
        }
        keep
    }

    /// Register a streamed play in the device's outputs view at submit
    /// time — same record shape speak() inserts, so LED state, the echo
    /// gate's outputs term, the status page and latency accounting all see
    /// the stream from its first receipt.
    pub(super) fn register_stream_output(
        st: &mut DeviceVoiceState,
        play_id: &str,
        task_id: &Option<String>,
        seg_start: u64,
        request: Option<RequestTicket>,
    ) {
        // M6b: the answer audio of the CURRENT turn started here.
        st.turn_audio.note_submit(seg_start, task_id.as_deref());
        if let Some(ticket) = request {
            st.participation.answer_started(ticket, play_id);
        }
        st.outputs.insert(
            play_id.to_string(),
            OutputRecord {
                answer_text: String::new(),
                request,
                task_id: task_id.clone(),
                channel: "voice".into(),
                kind: "answer".into(),
                content_ref: String::new(),
                stage: "submitted".into(),
                started_ms: seg_start,
                stage_ms: id::now_ms(),
                end_sent_ms: None,
                expected_ms: 0,
                synth_ms: id::now_ms().saturating_sub(seg_start),
            },
        );
    }

    /// M6b: the turn's readable timeline for `turn_end`. Every number is an
    /// OBSERVED moment: `first_byte` from the model stream, audio from the
    /// device-facing plays (submit → terminal receipt), the stop from the
    /// epoch check. Nothing is inferred from intent.
    pub(super) fn turn_timeline_json(
        &self,
        device: &str,
        turn_started_ms: u64,
        first_byte_ms: Option<u64>,
        stop: Option<(&str, u64)>,
    ) -> serde_json::Value {
        let states = crate::lock_or_recover(&self.states, "voice.states");
        let audio = states
            .get(device)
            .map(|st| st.turn_audio.clone())
            .unwrap_or_default();
        let live_at_end = states
            .get(device)
            .map(|st| {
                st.outputs
                    .values()
                    .any(|o| o.kind == "answer" && stage_is_live(&o.stage))
            })
            .unwrap_or(false);
        drop(states);
        let rel = |t: Option<u64>| t.map(|x| x.saturating_sub(turn_started_ms));
        let last = audio.last_end_ms.or(audio.last_submit_ms);
        json!({
            "turn_ms": id::now_ms().saturating_sub(turn_started_ms),
            // Model/answer text. A tool-only turn has a first byte but no
            // audio; a turn that never produced model output has neither.
            "first_byte_ms": first_byte_ms,
            // Answer audio only: ack / prompt / filler are separate outputs
            // and would otherwise masquerade as "the answer started".
            "first_audio_ms": rel(audio.first_submit_ms),
            "last_audio_ms": rel(last),
            // True = the answer was still draining when this record was
            // written; `turn_audio_end` carries the real end (joined by task).
            "audio_live_at_end": live_at_end,
            "played_ms": audio.played_ms,
            "answer_plays": audio.plays,
            "stop": stop.map(|(why, at)| json!({ "why": why, "at_ms": at })),
        })
    }

    /// Tickets are issued synchronously before dispatch, never after an await.
    pub(super) async fn run_device_turn_at(
        self: Arc<Self>,
        device: String,
        text: String,
        source: String,
        ticket: TurnTicket,
    ) {
        let claimed = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get_mut(&device)
                .filter(|st| {
                    st.stop_epoch == ticket.stop_epoch
                        && st.participation.is_active_request(ticket.request)
                })
                .map(|st| st.claim_turn())
        };
        if let Some(turn_id) = claimed {
            self.note(
                "request_started",
                json!({ "device": device, "request": ticket.request, "turn_id": turn_id }),
            );
            self.clone()
                .run_device_turn_inner(device.clone(), text, source, ticket, turn_id)
                .await;
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
            {
                st.release_turn(turn_id);
            }
        }
        let now_end = id::now_ms();
        self.sync_learning_wait(&device, now_end);
        let single_closed = if let Some(st) =
            crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
        {
            st.finish_window_request(ticket.request, now_end);
            st.participation.session.state == SessionState::Idle
        } else {
            false
        };
        if single_closed {
            self.release_sovereignty(&device, now_end, false);
        }
        self.apply_capture_revocations();
        self.push_assistant_state(&device, false);
    }

    pub(super) async fn run_device_turn_inner(
        self: Arc<Self>,
        device: String,
        text: String,
        source: String,
        ticket: TurnTicket,
        turn_id: u64,
    ) {
        let turn_epoch = ticket.stop_epoch;
        let request = Some(ticket.request);
        // R2: the same soft targets go into the prompt and the runaway backstop cap.
        let voice_policy =
            crate::lock_or_recover(&self.settings, "voice.settings").effective_for(&device);
        let continue_hint = voice_policy.effective_continue_hint();
        let spoken_settings = voice_policy.spoken;
        let short_chars = spoken_settings.max_chars;
        let cont_budget = spoken_settings.backstop_budget();
        let delivery = crate::agent::prompt::Delivery::VoiceAuto {
            short: spoken_settings.budget(),
            detail: spoken_settings.detail_budget(),
        };
        let mut cap = SpokenCap::new(spoken_settings.backstop_budget());
        let mut hint_said = false;

        let mut env = match (self.env_factory)() {
            Ok(e) => e,
            Err(e) => {
                self.note("turn_error", json!({ "device": device, "error": e }));
                return;
            }
        };
        let turn_env = Arc::make_mut(&mut env);
        turn_env.assistant_name = if voice_policy.assistant_name.trim().is_empty() {
            crate::ASSISTANT_NAME.into()
        } else {
            voice_policy.assistant_name.trim().to_owned()
        };
        turn_env.prompt_preferences.assistant_name = voice_policy.assistant_name;
        turn_env.prompt_preferences.persona_prompt = voice_policy.persona_prompt;
        turn_env.prompt_preferences.system_prompt_override = voice_policy.system_prompt_override;
        turn_env.prompt_preferences.voice_continue_words =
            Some(if voice_policy.stop_intents.enabled {
                voice_policy.stop_intents.continue_words
            } else {
                Vec::new()
            });
        let sessions = crate::session::SessionStore::new(self.store.clone());
        let frozen_session = match ticket.context {
            Some(context) => sessions.context_session_id(&device, context),
            None => sessions.device_session_id(&device),
        };
        let session = match frozen_session {
            Ok(session) => session,
            Err(error) => {
                self.note("turn_error", json!({"device":device,"error":error}));
                return;
            }
        };

        let Some((scope, wake)) = self.take_turn_wake(&device, &source, ticket.request) else {
            return;
        };
        // Pure "几点/周几/几号" questions are answered locally without the LLM.
        if let Some(answer) = crate::tool::time::clock_reply(&text) {
            let sessions = crate::session::SessionStore::new(self.store.clone());
            {
                let states = crate::lock_or_recover(&self.states, "voice.states");
                if !states.get(&device).is_some_and(|s| {
                    s.live_turn == Some(turn_id)
                        && s.stop_epoch == turn_epoch
                        && s.participation.is_active_request(ticket.request)
                }) {
                    return;
                }
                let _ = sessions.append(&session, "user", &text);
                let _ = sessions.append(&session, "assistant", &answer);
                if let Some(context) = ticket.context {
                    let _ = sessions.complete_context(&session, context, id::now_ms());
                }
            }
            env.traces.record_local_fast_path(
                &session,
                &source,
                &text,
                &answer,
                wake.as_ref().map(|w| w.0.as_str()),
                wake.as_ref()
                    .map(|w| w.1.as_str())
                    .filter(|s| !s.is_empty()),
            );
            self.note(
                "clock_answered_locally",
                json!({ "device": device, "text": text, "answer": answer, "source": source }),
            );
            self.cancel_filler_tracked_at(
                &device,
                "clock_local",
                Some(OutputPermit {
                    generation: ticket.request.generation,
                    stop_epoch: turn_epoch,
                    request,
                }),
            );
            self.speak_inner(&device, &answer, None, "answer", false, request)
                .await;
            self.push_assistant_state(&device, false);
            return;
        }

        // This turn's delivery epoch was resolved from the ticket above —
        // a barge-in invalidates the whole turn, not merely the piece that
        // was playing (D/T5 R5, 2026-11-09).
        // B-M0 waiting cue: the phrase is chosen once, up front, from the
        // transcript (before the model runs — the same input A's M0 uses).
        let cue_suppressed = {
            let now = id::now_ms();
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get_mut(&device)
                .filter(|st| {
                    st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                })
                .and_then(|st| st.suppress_cue_until_ms.take())
                .map(|until| now <= until)
                .unwrap_or(false)
        };
        let filler = if cue_suppressed {
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                .get_mut(&device)
                .filter(|st| {
                    st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                })
            {
                st.suppress_cue_until_ms = None;
            }
            self.note(
                "filler_skipped",
                json!({ "device": device, "skipped": "wake_ack_played" }),
            );
            None
        } else {
            self.pick_filler(&device, &text)
        };
        let filler_delay = crate::lock_or_recover(&self.settings, "voice.settings")
            .filler
            .delay_ms;
        let scope_ctx = crate::working_memory::ScopeContext {
            device_id: Some(device.clone()),
            room: None,
            person_id: None,
        };
        let lan_exempt = self.hub.is_device_lan_exempt(&device);
        let mut stream = crate::agent::run_turn_with_scope_and_wake(
            env,
            Some(session),
            text,
            source,
            delivery,
            scope,
            scope_ctx,
            wake,
            lan_exempt,
        );
        // This turn's OWN task id — captured from the Task event, never
        // re-read from shared state (an old turn answering late must not
        // inherit a newer turn's task identity).
        let mut own_task: Option<String> = None;
        // v0.3 (T3): streamed answer speech. Token deltas accumulate in
        // `delta_buf`; the first speakable segment opens the device
        // stream; tool phases cut whatever was streaming (their content is
        // not the answer — the classic whole-path at Answer is untouched
        // for devices without `play_stream`).
        let mut delta_buf = String::new();
        // R1e: bytes of the answer already handed to TTS — the resume point.
        let mut spoken_end: usize = 0;
        let mut tstream: Option<TurnStream> = None;
        let stream_capable = self.hub.device_supports(&device, "play_stream");
        let mut turn_barged = false;
        let mut saw_home_action = false;
        let mut saw_non_home_tool = false;
        let mut saw_fact = false;
        // The cue timer races the event stream: a ready answer wins (the cue
        // never plays), a slow one plays exactly one cue, and the answer's
        // first audio replaces it (cancel_filler in speak_stream_segment /
        // speak).
        let mut filler_deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(filler_delay);
        let mut filler_armed = filler.is_some();
        // M5 budget: accumulate ONLY the stretches with no voice playback of
        // ours on the device, so the two kinds of waiting stay apart.
        let (first_budget_ms, total_budget_ms) =
            crate::lock_or_recover(&self.turn_wait_budget, "voice.turn_wait_budget")
                .unwrap_or((TURN_WAIT_FIRST_MS, TURN_WAIT_TOTAL_MS));
        let mut wait_ms: u64 = 0;
        // Latched: once this turn put ANY answer audio on the device, the
        // remaining budget is the TOTAL one. Reading "is a play live right
        // now" would fall back to the 15 s first-answer budget the moment the
        // first sentence finished.
        let mut answered_once = false;
        let turn_started_ms = id::now_ms();
        // M6b: this turn's audible timeline starts clean. The event pump
        // records submits/ends from here on; the turn loop records the
        // model's first byte and the moment it noticed a stop.
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
            .get_mut(&device)
            .filter(|st| st.stop_epoch == turn_epoch && st.participation.contains(ticket.request))
        {
            st.turn_audio = TurnAudio::default();
        }
        let mut first_byte_ms: Option<u64> = None;
        let mut stop_seen_ms: Option<u64> = None;
        let mut last_budget_tick = tokio::time::Instant::now();
        let mut budget_deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(TURN_BUDGET_TICK_MS);
        loop {
            let ev = tokio::select! {
                // Bias toward the turn's own events: a ready answer always
                // wins over an overdue cue timer, so the cue can never fire
                // after the answer it was waiting for (the timer branch is
                // also the one that must not outlive the stream: when the
                // stream is exhausted this select returns None and the turn
                // ends — a cue for a finished turn is meaningless).
                biased;
                ev = stream.next() => ev,
                // M5: the waiting budget. Ticks regardless of the cue.
                _ = tokio::time::sleep_until(budget_deadline) => {
                    let now_i = tokio::time::Instant::now();
                    let dt = now_i.duration_since(last_budget_tick).as_millis() as u64;
                    last_budget_tick = now_i;
                    answered_once |= self.answer_audio_started(&device);
                    let answered = answered_once;
                    if !self.live_content(&device).voice {
                        wait_ms = wait_ms.saturating_add(dt);
                    }
                    let (phase, limit) = if answered {
                        ("total", total_budget_ms)
                    } else {
                        ("first_answer", first_budget_ms)
                    };
                    if wait_ms >= limit {
                        // Bounded end: the turn stops waiting, says why, and
                        // the loop tail still emits its ONE turn_end.
                        let _ = filler_armed;
                        self.cancel_filler_tracked_at(&device, "turn_budget",Some(OutputPermit {generation:ticket.request.generation,stop_epoch:turn_epoch,request}));
                        if let Some(ts) = tstream.take() {
                            self.fail_voice_stream(&ts.play_id);
                        }
                        if !answered {
                            self.flash_error(&device);
                            self.note("turn_first_packet_timeout_notice", json!({
                                "device": device,
                                "wait_ms": wait_ms,
                                "limit_ms": limit,
                                "task": own_task,
                                "notice": "抱歉，刚才响应超时了，请再试一次。",
                            }));
                            let notice_permit = OutputPermit {
                                generation: ticket.request.generation,
                                stop_epoch: turn_epoch,
                                request: None,
                            };
                            let pipe = self.clone();
                            let dev = device.clone();
                            tokio::spawn(async move {
                                let _ = pipe.speak_inner_at(
                                    &dev,
                                    "抱歉，刚才响应超时了，请再试一次。",
                                    None,
                                    "notice",
                                    false,
                                    notice_permit,
                                ).await;
                            });
                        }
                        self.note("turn_budget_exceeded", json!({
                            "device": device, "phase": phase, "wait_ms": wait_ms,
                            "limit_ms": limit, "task": own_task,
                            "why": "model/tool waiting exceeded the budget; playback time is not counted",
                        }));
                        // Cancelling the task is not cosmetic: the agent runs
                        // detached, so without it a late model answer could
                        // still make tool calls after the turn gave up.
                        if let Some(task) = own_task.as_ref() {
                            let _ = self.tasks.request_cancel(task);
                        }
                        // One terminal for this turn, exactly like the normal
                        // end (the End arm is skipped by this break).
                        let timeline = self.turn_timeline_json(
                            &device,
                            turn_started_ms,
                            first_byte_ms,
                            Some(("turn_budget", id::now_ms().saturating_sub(turn_started_ms))),
                        );
                        self.note("turn_end", json!({
                            "device": device, "status": "budget_exceeded",
                            "duration_ms": id::now_ms().saturating_sub(turn_started_ms),
                            "task": own_task,
                            "timeline": timeline,
                        }));
                        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(&device).filter(|st| st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)) {
                            if own_task.as_ref() == st.current_task.as_ref() {
                                st.current_task = None;
                            }
                        }
                        self.push_assistant_state(&device, false);
                        break;
                    }
                    budget_deadline = now_i
                        + std::time::Duration::from_millis(TURN_BUDGET_TICK_MS);
                    continue;
                }
                _ = tokio::time::sleep_until(filler_deadline), if filler_armed => {
                    // The answer's first audio can land WHILE this loop is
                    // awaiting it (TTS synthesis suspends the select). On the
                    // real device that cost an answer: the overdue cue was
                    // submitted on top of the live stream and superseded it
                    // (2026-11-12). The cue only ever covers SILENCE.
                    if self.answer_audio_started(&device) {
                        filler_armed = false;
                        self.note("filler_skipped", json!({
                            "device": device,
                            "scene": filler.as_ref().map(|(s, _)| s.clone()),
                            "skipped": "answer_already_playing",
                        }));
                        continue;
                    }
                    // Audit Q5: the timer branch never checked the turn's
                    // cancellation/epoch. A stop while the agent is still
                    // blocked could start a cue AFTER the stop.
                    if self.task_cancelled(&own_task) || self.stop_epoch_of(&device) != turn_epoch {
                        filler_armed = false;
                        self.note("filler_skipped", json!({
                            "device": device,
                            "scene": filler.as_ref().map(|(s, _)| s.clone()),
                            "skipped": "turn_stopped_before_cue",
                        }));
                        continue;
                    }
                    // 2026-11-13: a live VOICE play owns the channel — the
                    // wake ack is the common case (it is longer than the cue's
                    // 300 ms timer). Cutting it to start the cue is exactly the
                    // "ack disappears" the user reported. Retry shortly instead
                    // of dropping the cue on the floor.
                    let mode = crate::lock_or_recover(&self.settings, "voice.settings").filler.mode;
                    if !cue_may_start(mode, self.active_content(&device)) {
                        filler_deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_millis(200);
                        continue;
                    }
                    filler_armed = false;
                    if let Some((scene, phrase)) = filler.as_ref() {
                        self.play_filler(&device, scene, phrase, &own_task, request);
                    }
                    continue;
                }
            };
            let Some(ev) = ev else { break };
            // A barge-in (or any stop) invalidates the REST of this turn:
            // no later sentence may start playing. The events still flow so
            // the turn ends through its normal TurnEnd path (an aborted
            // stream would leave turn_end missing from the audit trail).
            let stale = !crate::lock_or_recover(&self.states, "voice.states")
                .get(&device)
                .is_some_and(|st| {
                    st.live_turn == Some(turn_id)
                        && st.stop_epoch == turn_epoch
                        && st.participation.is_active_request(ticket.request)
                });
            if stale && !turn_barged {
                turn_barged = true;
                filler_armed = false;
                // The stop already killed every play; end the cue's bookkeeping
                // here so the log says why it never became an answer.
                self.cancel_filler_tracked_at(
                    &device,
                    "turn_stopped",
                    Some(OutputPermit {
                        generation: ticket.request.generation,
                        stop_epoch: turn_epoch,
                        request,
                    }),
                );
                self.note(
                    "turn_barged_in",
                    json!({ "device": device, "task": own_task }),
                );
            }
            // M6b: the turn's own timeline. "First byte" is the first event
            // that IS model output (a token, a whole answer, a tool call) —
            // never the turn's start, and never a core-owned fact.
            if first_byte_ms.is_none()
                && matches!(
                    &ev,
                    AgentEvent::Token { .. }
                        | AgentEvent::Answer { .. }
                        | AgentEvent::ToolStart { .. }
                )
            {
                first_byte_ms = Some(id::now_ms().saturating_sub(turn_started_ms));
            }
            if stale && stop_seen_ms.is_none() {
                stop_seen_ms = Some(id::now_ms().saturating_sub(turn_started_ms));
            }
            match ev {
                AgentEvent::Token { .. } if stale => {
                    delta_buf.clear();
                }
                AgentEvent::Answer { .. } if stale => {
                    delta_buf.clear();
                }
                AgentEvent::Task { task_id } => {
                    own_task = Some(task_id.clone());
                    if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                        .get_mut(&device)
                        .filter(|st| {
                            st.live_turn == Some(turn_id)
                                && st.stop_epoch == turn_epoch
                                && st.participation.is_active_request(ticket.request)
                        })
                    {
                        if st.turn_audio.task.is_none() {
                            st.turn_audio.task = Some(task_id.clone());
                        }
                        st.current_task = Some(task_id.clone());
                    } else {
                        self.tasks.request_cancel(&task_id);
                        self.note(
                            "stale_session_event_rejected",
                            json!({"device":device,"via":"task","request":request,"task":task_id}),
                        );
                    }
                    self.push_assistant_state(&device, false);
                }
                AgentEvent::Token { text } => {
                    if saw_home_action && !saw_non_home_tool && saw_fact {
                        delta_buf.clear();
                        continue;
                    }
                    if !stream_capable {
                        continue; // whole-path at Answer speaks it
                    }
                    // R1e: the program is the whole generated answer, not
                    // just what has been spoken — a pause must be able to
                    // keep the rest.
                    if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                        .get_mut(&device)
                        .filter(|st| {
                            st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                        })
                    {
                        let p = st.live_program.get_or_insert_with(|| Program {
                            origin_task: own_task.clone(),
                            created_ms: id::now_ms(),
                            resumable: false,
                            short_chars,
                            cont: Some(cont_budget),
                            ..Program::default()
                        });
                        p.text.push_str(&text);
                    }
                    // The budget already stopped this delivery: the rest of
                    // the answer still lands in the program (above) for
                    // 「继续」, but nothing more is spoken.
                    if cap.exhausted {
                        delta_buf.clear();
                        continue;
                    }
                    delta_buf.push_str(&text);
                    while let Some(seg) = Self::take_speak_segment(&mut delta_buf) {
                        // Position the segment in the program text and advance
                        // the cursor only after the segment is delivered.
                        let Some(speak_text) = cap.admit(&seg) else {
                            self.note("spoken_budget_cut", json!({
                                "device": device, "max_sentences": cap.max_sentences,
                                "max_chars": cap.max_chars, "spoken_sentences": cap.sentences,
                                "spoken_chars": cap.chars, "backstop": true,
                                "why": "the runaway backstop cut in; the rest stays in the program",
                            }));
                            delta_buf.clear();
                            break;
                        };
                        let keep = self
                            .stream_segment_and_advance(
                                &device,
                                &speak_text,
                                &mut tstream,
                                &own_task,
                                turn_epoch,
                                turn_id,
                                &mut spoken_end,
                                request,
                            )
                            .await;
                        if !keep && tstream.is_none() {
                            // Stream died (stop/cancel/failure) — drain the
                            // buffer; the Answer fallback only speaks when
                            // nothing was delivered, matching speak()'s
                            // suppression semantics.
                            delta_buf.clear();
                            break;
                        }
                    }
                }
                // R2: the CORE's own sentence (an action fact or a refusal).
                // It is exempt from the spoken cap by construction: a length
                // limit must never be the reason a person does not hear what
                // actually happened.
                AgentEvent::Fact { text } => {
                    saw_fact = true;
                    if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                        .get_mut(&device)
                        .filter(|st| {
                            st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                        })
                    {
                        if saw_home_action && !saw_non_home_tool {
                            st.participation.mark_close_after_answer(ticket.request);
                        }
                        let p = st.live_program.get_or_insert_with(|| Program {
                            origin_task: own_task.clone(),
                            created_ms: id::now_ms(),
                            resumable: false,
                            short_chars,
                            cont: Some(cont_budget),
                            ..Program::default()
                        });
                        p.text.push_str(&text);
                    }
                    if !stream_capable {
                        continue; // whole-path at Answer speaks it
                    }
                    if !Self::speakable(&text) {
                        continue;
                    }
                    let _ = self
                        .stream_segment_and_advance(
                            &device,
                            &text,
                            &mut tstream,
                            &own_task,
                            turn_epoch,
                            turn_id,
                            &mut spoken_end,
                            request,
                        )
                        .await;
                }
                AgentEvent::ToolStart { tool, .. } => {
                    if tool == "home.set" || tool.starts_with("home.") {
                        saw_home_action = true;
                    } else {
                        saw_non_home_tool = true;
                    }
                    // R2: move the waiting cue earlier on the first tool call.
                    if filler_armed {
                        let tool_delay = crate::lock_or_recover(&self.settings, "voice.settings")
                            .filler
                            .tool_delay_ms;
                        let tool_deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_millis(tool_delay);
                        if tool_deadline < filler_deadline {
                            filler_deadline = tool_deadline;
                            self.note(
                                "filler_tool_armed",
                                json!({
                                    "device": device, "task": own_task,
                                    "tool_delay_ms": tool_delay,
                                }),
                            );
                        }
                    }
                    // Close any in-flight pre-tool speech stream so the current
                    // sentence drains without cutting mid-word, while dropping
                    // any unstreamed pending buffer.
                    if let Some(ts) = tstream.take() {
                        self.close_voice_stream(&ts.play_id, (ts.sent_secs * 1000.0) as u64);
                        self.note("tts_stream_closed_for_tools", json!({
                            "device": device, "task": own_task, "play": ts.play_id,
                            "note": "the sentence in flight finishes; nothing after the tool call is read",
                        }));
                    }
                    delta_buf.clear();
                }
                AgentEvent::Answer { text } => {
                    // R1e: generation finished — a paused bookmark can now
                    // hold the complete answer, so 继续 reads to the real end.
                    {
                        let mut states = crate::lock_or_recover(&self.states, "voice.states");
                        if let Some(st) = states.get_mut(&device).filter(|st| {
                            st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                        }) {
                            if saw_home_action && !saw_non_home_tool {
                                st.participation.mark_close_after_answer(ticket.request);
                            }
                            // R1e: generation finished, but the program is NOT
                            // over — it is still being read sentence by
                            // sentence, and a pause 20 s into a 60 s story must
                            // still find it. The program ends when its last
                            // sentence has played (PlayFinished).
                            if let Some(l) = st.live_program.as_mut() {
                                l.complete = true;
                            }
                            if let Some(mut p) = st.paused_program.take() {
                                if p.origin_task.is_none() || p.origin_task == own_task {
                                    if let Some(l) = st.live_program.as_ref() {
                                        p.text = l.text.clone();
                                    }
                                    p.complete = true;
                                }
                                st.paused_program = Some(p);
                            }
                        }
                    }
                    let mut streamed = false;
                    // Bytes of the ANSWER handed to TTS on the whole path —
                    // the resume point when the cap cut it.
                    let mut whole_spoken_len: usize = 0;
                    if tstream.is_some() {
                        // Flush a trailing partial sentence, then close.
                        let residue = std::mem::take(&mut delta_buf);
                        let residue = residue.trim().to_string();
                        if !residue.is_empty() {
                            if let Some(speak_text) = cap.admit(&residue) {
                                let _ = self
                                    .stream_segment_and_advance(
                                        &device,
                                        &speak_text,
                                        &mut tstream,
                                        &own_task,
                                        turn_epoch,
                                        turn_id,
                                        &mut spoken_end,
                                        request,
                                    )
                                    .await;
                            }
                        }
                        // R2 (2026-11-13): the cap cut this answer. Say the
                        // continuation clue in the SAME stream (no second
                        // voice play, so the half-duplex gate closes once),
                        // and only when something really was left out.
                        if cap.exhausted && !hint_said && !continue_hint.is_empty() {
                            hint_said = true;
                            let _ = self
                                .speak_stream_segment(
                                    &device,
                                    &continue_hint,
                                    &mut tstream,
                                    &own_task,
                                    turn_epoch,
                                    turn_id,
                                    request,
                                )
                                .await;
                            self.note(
                                "spoken_continue_hint",
                                json!({
                                    "device": device, "hint": continue_hint,
                                }),
                            );
                        }
                        if let Some(ts) = tstream.take() {
                            self.close_voice_stream(&ts.play_id, (ts.sent_secs * 1000.0) as u64);
                            self.note(
                                "tts_streamed",
                                json!({
                                    "device": device, "task": own_task, "play": ts.play_id,
                                    "chunks": ts.seq,
                                    "first_chunk_ms": ts.first_forward_ms,
                                }),
                            );
                            streamed = true;
                        }
                    }
                    if !streamed {
                        // R1e: the whole-answer path is a program too (no
                        // stream, nothing to bookmark at first otherwise).
                        {
                            let mut states = crate::lock_or_recover(&self.states, "voice.states");
                            if let Some(st) = states.get_mut(&device).filter(|st| {
                                st.stop_epoch == turn_epoch
                                    && st.participation.contains(ticket.request)
                            }) {
                                if st.live_program.is_none() {
                                    st.live_program = Some(Program {
                                        text: text.clone(),
                                        cursor: 0,
                                        complete: true,
                                        origin_task: own_task.clone(),
                                        created_ms: id::now_ms(),
                                        resumable: false,
                                        short_chars,
                                        cont: Some(cont_budget),
                                        system_cut: false,
                                        lead_said: false,
                                    });
                                }
                            }
                        }
                        // The same cap, on the whole-answer path: a device
                        // without `play_stream` is not a reason to read 40 s.
                        let (say, cut) = cap_text(&mut cap, &text);
                        if cut {
                            self.note("spoken_budget_cut", json!({
                                "device": device, "max_sentences": cap.max_sentences,
                                "max_chars": cap.max_chars, "spoken_sentences": cap.sentences,
                                "spoken_chars": cap.chars, "backstop": true,
                                "why": "the runaway backstop cut in; the rest stays in the program",
                            }));
                        }
                        let spoken_len = say.len();
                        whole_spoken_len = spoken_len;
                        let mut say = say;
                        if cut && !continue_hint.is_empty() {
                            say.push_str(&continue_hint);
                            self.note(
                                "spoken_continue_hint",
                                json!({
                                    "device": device, "hint": continue_hint,
                                }),
                            );
                        }
                        if !say.trim().is_empty()
                            && crate::lock_or_recover(&self.states, "voice.states")
                                .get(&device)
                                .is_some_and(|st| {
                                    st.live_turn == Some(turn_id)
                                        && st.stop_epoch == turn_epoch
                                        && st.participation.is_active_request(ticket.request)
                                })
                        {
                            self.speak_inner(
                                &device,
                                &say,
                                own_task.clone(),
                                "answer",
                                false,
                                request,
                            )
                            .await;
                        }
                        // The bookmark is the point AFTER what was heard, so
                        // 「继续」 does not replay the beginning.
                        if cut {
                            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                                .get_mut(&device)
                                .filter(|st| {
                                    st.stop_epoch == turn_epoch
                                        && st.participation.contains(ticket.request)
                                })
                            {
                                if let Some(p) = st.live_program.as_mut() {
                                    p.cursor = Self::clamp_boundary(&p.text, spoken_len);
                                }
                            }
                        }
                    }
                    // R2 (2026-11-13): the cap left text behind, so this is a
                    // PAUSED delivery, not a finished one — book it where the
                    // person stopped. Without this 「继续」 had nothing to
                    // resume (the live program is retired by the play's own
                    // PlayFinished).
                    if cap.exhausted {
                        let cut_at = if streamed {
                            spoken_end
                        } else {
                            whole_spoken_len
                        };
                        let mut states = crate::lock_or_recover(&self.states, "voice.states");
                        if let Some(st) = states.get_mut(&device).filter(|st| {
                            st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                        }) {
                            let mut p = match st.live_program.take() {
                                Some(mut p) => {
                                    if p.text.is_empty() {
                                        p.text = text.clone();
                                    }
                                    p
                                }
                                None => Program {
                                    text: text.clone(),
                                    cursor: 0,
                                    complete: true,
                                    origin_task: own_task.clone(),
                                    created_ms: id::now_ms(),
                                    resumable: false,
                                    short_chars,
                                    cont: Some(cont_budget),
                                    system_cut: false,
                                    lead_said: false,
                                },
                            };
                            p.complete = true;
                            // The bookmark is AFTER the last sentence the
                            // person heard — a cap cut lands between
                            // sentences, so 继续 must not replay the one
                            // that just finished (a PAUSE's bookmark, by
                            // contrast, points AT the sentence in flight).
                            p.cursor = Self::clamp_boundary(&p.text, cut_at);
                            // The cap, not the person, created this bookmark
                            // (user decision 2026-11-14): it lights no amber
                            // and dies with the follow window.
                            p.system_cut = true;
                            st.paused_program = Some(p);
                        }
                    }
                }
                AgentEvent::ConfirmationRequired { action } => {
                    filler_armed = false;
                    self.cancel_filler_tracked_at(
                        &device,
                        "confirmation_required",
                        Some(OutputPermit {
                            generation: ticket.request.generation,
                            stop_epoch: turn_epoch,
                            request,
                        }),
                    );
                    self.note(
                        "confirmation_required",
                        json!({"device":device,"action_id":action["id"],"task":own_task}),
                    );
                }
                AgentEvent::TurnEnd {
                    status,
                    trace_id,
                    duration_ms,
                } => {
                    filler_armed = false;
                    // R2-BM5: a turn that ended WITHOUT an Answer (e.g. the
                    // model call failed after a core fact was spoken) must not
                    // leave its voice stream open — the device would wait for
                    // blocks that will never come. A normal Answer already
                    // took the stream, so this is a no-op there.
                    if let Some(ts) = tstream.take() {
                        self.close_voice_stream(&ts.play_id, (ts.sent_secs * 1000.0) as u64);
                        self.note(
                            "tts_stream_closed_at_turn_end",
                            json!({
                                "device": device, "task": own_task, "play": ts.play_id,
                                "status": status,
                            }),
                        );
                    }
                    // A cue outliving its turn would be a waiting cue for
                    // nothing (B-M0: never more than one, never after end).
                    self.cancel_filler_tracked_at(
                        &device,
                        "turn_end",
                        Some(OutputPermit {
                            generation: ticket.request.generation,
                            stop_epoch: turn_epoch,
                            request,
                        }),
                    );
                    // M6b: the timeline rides ON turn_end, so every turn has
                    // one record that answers "how long to the first byte,
                    // the first audio, the last audio, and did a stop cut it".
                    let stopped = self.stop_epoch_of(&device) != turn_epoch;
                    let stop = stop_seen_ms
                        .or_else(|| stopped.then(|| id::now_ms().saturating_sub(turn_started_ms)))
                        .map(|at| ("stop_epoch_changed", at));
                    let timeline =
                        self.turn_timeline_json(&device, turn_started_ms, first_byte_ms, stop);
                    self.note(
                        "turn_end",
                        json!({ "device": device, "status": status, "trace_id": trace_id,
                                "duration_ms": duration_ms, "task": own_task,
                                "timeline": timeline }),
                    );
                    // Only retire OUR task: a stop or a newer turn may have
                    // replaced current_task in the meantime — clearing it
                    // unconditionally would orphan the live turn's state.
                    if let Some(st) = crate::lock_or_recover(&self.states, "voice.states")
                        .get_mut(&device)
                        .filter(|st| {
                            st.stop_epoch == turn_epoch && st.participation.contains(ticket.request)
                        })
                    {
                        if own_task.as_ref() == st.current_task.as_ref() {
                            st.current_task = None;
                        }
                    }
                    self.push_assistant_state(&device, false);
                }
                AgentEvent::Error { message } => {
                    if let Some(ts) = tstream.take() {
                        self.fail_voice_stream(&ts.play_id);
                        self.note(
                            "tts_stream_cut_on_error",
                            json!({ "device": device, "play": ts.play_id }),
                        );
                    }
                    // B-M0: an error ends the wait — the cue must not keep
                    // talking after the turn already failed.
                    filler_armed = false;
                    self.cancel_filler_tracked_at(
                        &device,
                        "turn_error",
                        Some(OutputPermit {
                            generation: ticket.request.generation,
                            stop_epoch: turn_epoch,
                            request,
                        }),
                    );
                    self.flash_error(&device);
                    self.note("turn_error", json!({ "device": device, "error": message }));
                }
                _ => {}
            }
        }
    }

    /// Error indication (presentation): a bounded red window, then the
    /// underlying state resumes via the ticker.
    pub(super) fn flash_error(&self, device: &str) {
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.error_flash_until_ms = id::now_ms() + ERROR_FLASH_MS;
        }
        self.push_assistant_state(device, false);
    }

    /// True when the task was cancelled in the meantime (barge-in, stop).
    pub(super) fn task_cancelled(&self, task_id: &Option<String>) -> bool {
        task_id
            .as_deref()
            .and_then(|id| self.tasks.get(id))
            .map(|t| t.is_cancelled())
            .unwrap_or(false)
    }

    pub(super) async fn speak_inner(
        &self,
        device: &str,
        text: &str,
        task_id: Option<String>,
        kind: &str,
        wait_terminal: bool,
        request: Option<RequestTicket>,
    ) -> Option<SpokenTerminal> {
        let Some(permit) = self.output_permit(device, request) else {
            self.note(
                "stale_session_event_rejected",
                json!({"device":device,"via":"tts","request":request}),
            );
            return None;
        };
        self.speak_inner_at(device, text, task_id, kind, wait_terminal, permit)
            .await
    }

    pub(super) async fn speak_inner_at(
        &self,
        device: &str,
        text: &str,
        task_id: Option<String>,
        kind: &str,
        wait_terminal: bool,
        permit: OutputPermit,
    ) -> Option<SpokenTerminal> {
        let request = permit.request;
        if !crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| permit.valid(st))
        {
            self.note(
                "stale_session_event_rejected",
                json!({"device":device,"via":"deferred_tts","request":request}),
            );
            return None;
        }
        if !self.hub.device_online_with_cap(device, "audio_out") {
            self.note(
                "speak_skipped",
                json!({ "device": device, "reason": "device has no audio_out / not connected" }),
            );
            return None;
        }
        if self.task_cancelled(&task_id) {
            self.note(
                "tts_suppressed_after_cancel",
                json!({ "device": device, "task": task_id, "stage": "before synthesis" }),
            );
            return None;
        }
        // Snapshot the delivery epoch: any stop or barge-in from here on
        // invalidates THIS answer, even if its task already completed.
        let started_epoch = permit.stop_epoch;
        if !Self::speakable(text) {
            self.note(
                "tts_segment_skipped",
                json!({ "device": device, "chars": text.chars().count(), "whole_answer": true }),
            );
            return None;
        }
        let tts = match (self.tts_factory)() {
            Ok(t) => t,
            Err(e) => {
                self.note("tts_unconfigured", json!({ "device": device, "reason": e }));
                return None;
            }
        };
        let started = id::now_ms();
        let audio = match tts.synthesize(text).await {
            Ok(a) => a,
            Err(e) => {
                // Same rule as the streaming path: a cancelled turn's
                // synthesis failure is expected, not a red ring.
                let still_valid =
                    !self.task_cancelled(&task_id) && self.stop_epoch_of(device) == started_epoch;
                if still_valid {
                    self.flash_error(device);
                }
                self.note(
                    "tts_failed",
                    json!({ "device": device, "error": e.to_string(), "cancelled": !still_valid }),
                );
                return None;
            }
        };
        let synth_ms = id::now_ms() - started;
        if !crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| permit.valid(st))
        {
            self.note(
                "stale_session_event_rejected",
                json!({"device":device,"via":"tts_result","request":request}),
            );
            if self.stop_epoch_of(device) != started_epoch {
                self.note("tts_suppressed_after_stop_epoch",json!({"device":device,"task":task_id,"stage":"after synthesis","started_epoch":started_epoch,"current_epoch":self.stop_epoch_of(device)}));
            }
            return None;
        }
        if self.task_cancelled(&task_id) {
            self.note("tts_suppressed_after_cancel", json!({ "device": device, "task": task_id, "stage": "after synthesis", "synth_ms": synth_ms }));
            return None;
        }
        let current_epoch = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .map(|st| st.stop_epoch)
            .unwrap_or(started_epoch);
        if current_epoch != started_epoch {
            self.note("tts_suppressed_after_stop_epoch", json!({
                "device": device, "task": task_id, "stage": "after synthesis",
                "synth_ms": synth_ms, "started_epoch": started_epoch, "current_epoch": current_epoch,
            }));
            return None;
        }
        // Test seam (§0.3 item 2): the one-shot hook parks THIS speak in the
        // window between the epoch check above and the submission below.
        let race_hook = crate::lock_or_recover(
            &self.before_play_submit_hook,
            "voice.before_play_submit_hook",
        )
        .take();
        if let Some(hook) = race_hook {
            hook().await;
        }
        let audio_meta = audio.meta();
        let req = PlayRequest {
            device: device.to_string(),
            content_ref: format!("tts:{}", id::next("V")),
            audio: Some(audio.wav),
            audio_meta,
            channel: "voice".into(),
            text: Some(text.to_string()),
            simulated_ms: None,
        };
        // B-M0: the whole-answer path replaces the waiting cue the same way
        // the streamed path does (before submission, so it counts as new).
        // R1j/P0-a: wait for the device's flush before speaking over it.
        self.wait_filler_finish(device, FILLER_COMPLETE_WAIT_MS)
            .await;
        let (_, filler_wait) =
            self.cancel_filler_tracked_at(device, "answer_started", Some(permit));
        self.settle_filler(device, "answer_started", filler_wait)
            .await;
        if self.stop_epoch_of(device) != started_epoch || self.task_cancelled(&task_id) {
            self.note(
                "tts_suppressed_after_stop_epoch",
                json!({
                    "device": device, "task": task_id, "stage": "after filler settle",
                    "started_epoch": started_epoch,
                    "current_epoch": self.stop_epoch_of(device),
                }),
            );
            return None;
        }
        let (mut wait_tx, wait_rx) = if wait_terminal {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let submitted = {
            let protect_ms = crate::lock_or_recover(&self.settings, "voice.settings")
                .effective_for(device)
                .barge_protect_ms
                .min(3_000);
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            if !states.get(device).is_some_and(|st| permit.valid(st)) {
                drop(states);
                self.note(
                    "stale_session_event_rejected",
                    json!({"device":device,"via":"tts_submit","request":request}),
                );
                return None;
            }
            self.hub.play(req).map(|ticket| {
                let play_id = ticket.play_id.clone();
                if let Some(tx) = wait_tx.take() {
                    crate::lock_or_recover(&self.terminal_waiters, "voice.terminal_waiters")
                        .insert(play_id.clone(), tx);
                }
                if let Some(st) = states.get_mut(device) {
                    Self::note_voice_play_submitted_locked(st, id::now_ms(), protect_ms);
                    if kind == "answer" {
                        // M6b: this turn's answer audio started here
                        // (whole path; the streamed path records in
                        // register_stream_output).
                        st.turn_audio.note_submit(id::now_ms(), task_id.as_deref());
                        if let Some(ticket) = request {
                            st.participation.answer_started(ticket, &play_id);
                        }
                    }
                    st.outputs.insert(
                        play_id.clone(),
                        OutputRecord {
                            answer_text: text.to_string(),
                            request: if kind == "answer" { request } else { None },
                            task_id: task_id.clone(),
                            channel: "voice".into(),
                            kind: kind.to_string(),
                            content_ref: String::new(),
                            stage: "submitted".into(),
                            started_ms: id::now_ms(),
                            stage_ms: id::now_ms(),
                            end_sent_ms: None,
                            expected_ms: 0,
                            synth_ms,
                        },
                    );
                }
                ticket
            })
        };
        match submitted {
            Ok(ticket) => {
                let play_id = ticket.play_id.clone();
                self.note(
                    "tts_played",
                    json!({ "device": device, "play_id": play_id, "task": task_id,
                            // `note()` owns the `kind` key (it is the event
                            // name), so the delivery class goes under its own
                            // name: answer | prompt | ...
                            "delivery_kind": kind,
                            "synth_ms": synth_ms, "duration_ms": audio.duration_ms,
                            "provider": audio.provider, "voice": audio.voice }),
                );
                self.push_assistant_state(device, false);
                // Terminal receipt bookkeeping flows through the event pump
                // (DeviceEvent::PlayStage) — no parallel writer here.
                if !wait_terminal {
                    return None;
                }
                // M5: this sentence's OWN terminal. The bound is its audio
                // duration plus slack; a device that never answers yields
                // `timeout`, which must not be read as "heard" either.
                //
                // The stop epoch is watched alongside the receipt: a stop that
                // reaches the DEVICE but whose receipt is lost (link down) must
                // still end this wait in bounded time instead of holding the
                // program for the whole timeout.
                let bound =
                    std::time::Duration::from_millis(audio.duration_ms + SPOKEN_TERMINAL_SLACK_MS);
                let deadline = tokio::time::Instant::now() + bound;
                // Listen on the pipeline's own terminal map, not on
                // `ticket.receipt`: the hub resolves that one on
                // `device_confirmed` as well (a diagnostic wants exactly
                // that), which is NOT this play's end.
                let mut rx = wait_rx.unwrap();
                let mut terminal = SpokenTerminal {
                    play_id: play_id.clone(),
                    stage: "timeout".into(),
                    ..Default::default()
                };
                loop {
                    let tick = tokio::time::sleep(std::time::Duration::from_millis(25));
                    tokio::select! {
                        r = &mut rx => {
                            match r {
                                Ok(t) => terminal = t,
                                Err(_) => terminal.stage = "link_lost".into(),
                            }
                            break;
                        }
                        _ = tick => {
                            if self.stop_epoch_of(device) != started_epoch {
                                terminal.stage = "stopped_epoch".into();
                                break;
                            }
                            if tokio::time::Instant::now() >= deadline {
                                terminal.stage = "timeout".into();
                                break;
                            }
                        }
                    }
                }
                crate::lock_or_recover(&self.terminal_waiters, "voice.terminal_waiters")
                    .remove(&play_id);
                if matches!(
                    terminal.stage.as_str(),
                    "timeout" | "link_lost" | "stopped_epoch"
                ) {
                    if let Some(st) =
                        crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                    {
                        if let Some(mut rec) = st.outputs.get(&play_id).cloned() {
                            rec.stage = "unknown".into();
                            rec.stage_ms = id::now_ms();
                            st.outputs.insert(play_id.clone(), rec.clone());
                            st.note_output_terminal(&play_id, &rec, id::now_ms());
                        }
                    }
                }
                self.note(
                    "spoken_terminal",
                    json!({ "device": device, "play_id": play_id, "task": task_id,
                            "stage": terminal.stage, "played_ms": terminal.played_ms,
                            "consumed_ms": terminal.consumed_ms,
                            "progress_basis": terminal.progress_basis,
                            "heard": terminal.heard() }),
                );
                Some(terminal)
            }
            Err(e) => {
                // A submit that lost the race to our own stop is not an
                // error to show; only a live turn's failure is.
                let still_valid =
                    !self.task_cancelled(&task_id) && self.stop_epoch_of(device) == started_epoch;
                if still_valid {
                    self.flash_error(device);
                }
                self.note(
                    "play_submit_failed",
                    json!({ "device": device, "error": e, "cancelled": !still_valid }),
                );
                None
            }
        }
    }
}
