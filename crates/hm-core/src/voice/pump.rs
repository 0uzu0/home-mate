use super::*;

impl VoicePipeline {
    /// Spawn the event pump. Consume control events and ASR results on the
    /// prioritized control pump, and offload `AudioChunk` / Silero VAD
    /// inference onto a dedicated audio worker task (`spawn_blocking`) so
    /// slow VAD inference or audio backpressure never blocks control events.
    pub fn start(self: &Arc<Self>) {
        let Some(mut mic_rx) = crate::lock_or_recover(&self.mic_rx, "voice.mic_rx").take() else {
            return; // already started
        };
        let mut control_rx = crate::lock_or_recover(&self.pump_control_rx, "voice.pump_control_rx")
            .take()
            .unwrap_or_else(|| self.hub.subscribe_pump_control());
        let pump_audio_queues =
            crate::lock_or_recover(&self.pump_audio_queues, "voice.pump_audio_queues").take();
        let pipe = self.clone();
        tokio::spawn(async move {
            let mut last_seq = 0u64;
            loop {
                tokio::select! {
                    biased;
                    ev = control_rx.recv() => match ev {
                        Ok(queued) => {
                            if queued.seq <= last_seq {
                                continue;
                            }
                            if queued.seq > last_seq.saturating_add(1) {
                                for missed in pipe.hub.take_critical_between(last_seq, queued.seq) {
                                    if missed.seq <= last_seq {
                                        continue;
                                    }
                                    pipe.record_control_queue_age(
                                        missed.enqueued_at.elapsed().as_millis() as u64,
                                    );
                                    last_seq = last_seq.max(missed.seq);
                                    pipe.hub.ack_critical_up_to(last_seq);
                                    pipe.dispatch_control_event(missed).await;
                                }
                            }
                            pipe.record_control_queue_age(
                                queued.enqueued_at.elapsed().as_millis() as u64,
                            );
                            last_seq = last_seq.max(queued.seq);
                            pipe.hub.ack_critical_up_to(last_seq);
                            pipe.dispatch_control_event(queued).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            pipe.control_lagged_total.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(skipped = n, "voice pipeline lagged behind control events");
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    t = mic_rx.recv() => match t {
                        Some(t) => {
                            pipe.record_control_queue_age(
                                t.enqueued_at.elapsed().as_millis() as u64,
                            );
                            pipe.handle_mic_transcript(t).await;
                        }
                        None => return,
                    },
                    else => break,
                }
                if control_rx.is_empty() {
                    for missed in pipe.hub.take_critical_after(last_seq) {
                        if missed.seq <= last_seq {
                            continue;
                        }
                        pipe.record_control_queue_age(
                            missed.enqueued_at.elapsed().as_millis() as u64
                        );
                        last_seq = last_seq.max(missed.seq);
                        pipe.hub.ack_critical_up_to(last_seq);
                        pipe.dispatch_control_event(missed).await;
                    }
                }
            }
        });
        if let Some(mut queue_rx) = pump_audio_queues {
            let audio_pipe = self.clone();
            tokio::spawn(async move {
                while let Some(mut handle) = queue_rx.recv().await {
                    let worker_pipe = audio_pipe.clone();
                    tokio::spawn(async move {
                        let sync = handle.sync.clone();
                        while let Some(queued) = handle.rx.recv().await {
                            worker_pipe.record_audio_queue_age(
                                queued.enqueued_at.elapsed().as_millis() as u64,
                            );
                            let seq = queued.seq;
                            if let DeviceEvent::AudioChunk {
                                device,
                                pcm,
                                sample_rate,
                                channels,
                                ts,
                                provenance,
                                remote_vad,
                                ..
                            } = queued.event
                            {
                                let p = worker_pipe.clone();
                                let done_sync = sync.clone();
                                if let Err(e) = tokio::task::spawn_blocking(move || {
                                    struct MarkProcessedOnDrop {
                                        sync: Arc<crate::gateway::DeviceAudioSync>,
                                        seq: u64,
                                    }
                                    impl Drop for MarkProcessedOnDrop {
                                        fn drop(&mut self) {
                                            self.sync.mark_processed(self.seq);
                                        }
                                    }
                                    let _guard = MarkProcessedOnDrop {
                                        sync: done_sync,
                                        seq,
                                    };
                                    p.feed_chunk_with_vad_seq(
                                        device,
                                        pcm,
                                        sample_rate,
                                        channels,
                                        ts,
                                        provenance,
                                        remote_vad,
                                        seq,
                                    );
                                })
                                .await
                                {
                                    tracing::warn!(
                                        device = %handle.device,
                                        error = %e,
                                        "audio VAD worker task panicked"
                                    );
                                }
                            } else {
                                sync.mark_processed(seq);
                            }
                        }
                        sync.mark_closed();
                    });
                }
            });
        }
        // Presentation ticker (G4): transitions push immediately, but
        // time-derived states (wake/error flash expiry, participation
        // timeout closing "listening") need a clock to retire them. One
        // send-on-change sweep per second — cheap and bounded.
        // Crash safety (2026-11-13): a diagnostic interrupted by a core
        // restart leaves `/run/hm-loop/arm` behind, and the firmware would
        // then write probe tracks on /run for every ordinary play. A fresh
        // core has no diagnostic in flight — disarm it.
        Self::arm_loopback_probe(false);
        let ticker = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let now = id::now_ms();
                // Device-stall fallback first: it may retire the output that
                // would otherwise keep the session open this tick.
                ticker.play_stall_sweep_at(now);
                ticker.stale_output_sweep_at(now);
                ticker.sweep_inputs();
                ticker.session_sweep_at(now);
                ticker.apply_capture_revocations();
                ticker.program_ttl_sweep_at(now);
                ticker.push_all_assistant_states();
                ticker.follow_window_tone_sweep();
                ticker.reminder_sweep();
                ticker.announce_sweep();
            }
        });
    }

    /// Dispatch a control-plane event from the pump. For `DeviceEvent::Keyword`,
    /// await draining all preceding preroll `AudioChunk`s (bounded by 250 ms) so
    /// Silero VAD updates `current_block_cursor()` and capture lease state before
    /// `on_keyword_at` samples them.
    async fn dispatch_control_event(self: &Arc<Self>, queued: crate::gateway::QueuedDeviceEvent) {
        if let DeviceEvent::Keyword { ref device, .. } = queued.event {
            if queued.audio_barrier_seq > 0 {
                let drained = self
                    .hub
                    .audio_drained_up_to(
                        device,
                        queued.audio_barrier_seq,
                        std::time::Duration::from_millis(250),
                    )
                    .await;
                if !drained {
                    tracing::warn!(
                        device = %device,
                        barrier_seq = queued.audio_barrier_seq,
                        keyword_seq = queued.seq,
                        "timed out waiting 250ms for preroll audio drain before keyword"
                    );
                }
            }
        }
        self.handle(queued.event).await;
    }

    /// Reminders that are due are spoken by the speaker that owns them
    /// (2026-11-14). A reminder is the person's own request, so it is not
    /// gated by `proactive_speech` — that setting governs what the assistant
    /// says on its own initiative.
    ///
    /// Postpone, never drop: while the speaker cannot take a proactive line
    /// (offline, hardware-muted, the person mid-sentence, a turn or an answer
    /// in progress) the reminder simply stays due for the next tick. It is
    /// marked fired only AFTER the line was heard or fully written — a core
    /// restart or a failed synthesis leaves it pending, and a failed attempt
    /// backs off 30 s so an outage is not hammered once a second. The cost of
    /// claim-on-success is a possible repeat after a crash mid-line; a
    /// repeated reminder is the smaller error than a silent one.
    pub(super) fn reminder_sweep(self: &Arc<Self>) {
        const RETRY_BACKOFF_MS: u64 = 30_000;
        let now = id::now_ms();
        let svc = crate::reminder::ReminderService::new(self.store.clone());
        let due = match svc.due(now) {
            Ok(d) if !d.is_empty() => d,
            _ => return,
        };
        let online: std::collections::HashSet<String> = self.hub.snapshot()["devices"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|d| d["online"].as_bool().unwrap_or(false))
                    .filter_map(|d| d["stable_id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for r in due {
            if !online.contains(&r.device) {
                continue;
            }
            let hub_voice = self.hub_live(&r.device).voice;
            let permit = {
                let states = crate::lock_or_recover(&self.states, "voice.states");
                match states.get(&r.device) {
                    Some(st) => {
                        let live = |o: &OutputRecord| {
                            !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                        };
                        (st.muted != Some(true)
                            && st.current_task.is_none()
                            && !st.outputs.values().any(|o| o.channel == "voice" && live(o))
                            && !hub_voice)
                            .then_some(OutputPermit {
                                generation: st.participation.generation(),
                                stop_epoch: st.stop_epoch,
                                request: None,
                            })
                    }
                    None => None,
                }
            };
            let speaking_now = self
                .mic_arc(&r.device)
                .map(|arc| crate::lock_or_recover(&arc, "voice.mic.dev").in_speech())
                .unwrap_or(false);
            // A closing segment clears `in_speech` while the person's words
            // are still on their way back from ASR. Speaking then talks over
            // someone who has just finished a sentence — the most annoying
            // possible moment (codex interaction audit 2026-11-14, finding 9).
            let (asr_inflight, last_close_age_ms) = {
                let states = crate::lock_or_recover(&self.states, "voice.states");
                match states.get(&r.device) {
                    Some(st) => (
                        st.mic_counters.asr_inflight,
                        st.mic_last_close_ms
                            .map(|t| now.saturating_sub(t))
                            .unwrap_or(u64::MAX),
                    ),
                    None => (0, u64::MAX),
                }
            };
            // `asr_inflight` alone is not enough: an empty/failed transcript
            // releases it without producing a turn, and a person who paused
            // mid-thought should still get a beat before we speak.
            let Some(permit) = permit else {
                continue;
            };
            if speaking_now || asr_inflight > 0 || last_close_age_ms < REMINDER_QUIET_AFTER_VOICE_MS
            {
                continue;
            }
            {
                let mut inflight =
                    crate::lock_or_recover(&self.reminder_inflight, "voice.reminder_inflight");
                if inflight.get(&r.id).is_some_and(|next| now < *next) {
                    continue;
                }
                inflight.insert(r.id, now + RETRY_BACKOFF_MS);
            }
            let late_ms = now.saturating_sub(r.due_ms);
            let line = Self::reminder_line(late_ms, r.due_ms, &r.note);
            let pipe = self.clone();
            tokio::spawn(async move {
                let terminal = pipe
                    .speak_inner_at(&r.device, &line, None, "prompt", true, permit)
                    .await;
                let ok = terminal
                    .as_ref()
                    .map(|t| t.heard() || t.written_fully())
                    .unwrap_or(false);
                if ok {
                    let svc = crate::reminder::ReminderService::new(pipe.store.clone());
                    let fired_ms = id::now_ms();
                    let marked = svc.mark_fired(r.id, fired_ms).unwrap_or(false);
                    pipe.note(
                        "reminder_fired",
                        json!({ "device": r.device, "id": r.id, "note": r.note,
                                "due_ms": r.due_ms, "late_ms": late_ms, "marked": marked }),
                    );
                    crate::lock_or_recover(&pipe.reminder_inflight, "voice.reminder_inflight")
                        .remove(&r.id);
                } else {
                    // Stays pending; the in-flight entry is the backoff.
                    pipe.note(
                        "reminder_speech_failed",
                        json!({ "device": r.device, "id": r.id, "note": r.note,
                                "retry_in_ms": RETRY_BACKOFF_MS,
                                "stage": terminal.as_ref().map(|t| t.stage.clone()) }),
                    );
                }
            });
        }
    }

    /// TEST SEAM (same class as `set_before_play_submit_hook`): a reminder
    /// due in ~1.5 s, so the firing sweep can be exercised without waiting
    /// out a wall-clock minute. Production reminders always come from the
    /// tool or the API, whose minimum is a full minute.
    pub fn test_only_schedule_reminder_due_soon(&self, device: &str, note: &str) -> i64 {
        let svc = crate::reminder::ReminderService::new(self.store.clone());
        let now = id::now_ms();
        svc.create(device, now + 1_500, note, now)
            .expect("a due-soon reminder is valid")
            .id
    }

    /// What the speaker says when a reminder fires. On time it is a plain
    /// line; more than two minutes late it names the original time, so a
    /// reminder that waited out a mute or an outage is not mistaken for
    /// being punctual.
    pub(super) fn reminder_line(late_ms: u64, due_ms: u64, note: &str) -> String {
        if late_ms < 120_000 {
            return format!("到点了，提醒你：{note}。");
        }
        let when = crate::tool::time::local_now()
            .map(|now| crate::reminder::spoken_when((due_ms / 1000) as i64, &now))
            .unwrap_or_default();
        if when.is_empty() {
            format!("迟到的提醒：{note}。")
        } else {
            format!("迟到的提醒，原定{when}：{note}。")
        }
    }

    pub(super) fn push_all_assistant_states(&self) {
        let devices: Vec<String> = crate::lock_or_recover(&self.states, "voice.states")
            .keys()
            .cloned()
            .collect();
        for d in devices {
            self.push_assistant_state(&d, false);
        }
    }

    /// Announce a timeout boundary once. Busy devices retain the pending
    /// attempt and retry on later ticks even without a new closing edge.
    /// Each accepted attempt freezes its original output permit; the spoken
    /// cue has no authority to reopen or extend a window.
    pub(super) fn follow_window_tone_sweep(self: &Arc<Self>) {
        let settings = crate::lock_or_recover(&self.settings, "voice.settings").clone();
        let now = id::now_ms();
        let devices: Vec<String> = crate::lock_or_recover(&self.states, "voice.states")
            .keys()
            .cloned()
            .collect();
        let hub_lives: std::collections::HashMap<String, HubLive> = devices
            .into_iter()
            .map(|d| {
                let live = self.effective_hub_live(&d);
                (d, live)
            })
            .collect();
        let mut timed_out_rooms: Vec<(String, String)> = Vec::new();
        let edges: Vec<(String, Option<usize>)> = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .iter_mut()
                .filter_map(|(device, st)| {
                    let hub_live = hub_lives.get(device).copied().unwrap_or_default();
                    // #88.2: active music/media playback NEVER triggers a Timeout
                    // session close or farewell ("拜拜"); also clear follow edge
                    // flags so finishing music never triggers a delayed bogus cue.
                    if st.media_live(hub_live) {
                        st.follow_open_last_tick = false;
                        st.follow_close_pending_at = None;
                        return None;
                    }
                    let open = st.session_open_with_live(now, hub_live);
                    // The timeout transition belongs to the session object:
                    // Listening → Idle, once, on this sweep.
                    let timeout_dropped =
                        if let Some(dropped) = st.timeout_session_with_live(now, hub_live) {
                            timed_out_rooms.push((device.clone(), self.room_of(device)));
                            dropped
                        } else {
                            None
                        };
                    let edge = st.follow_open_last_tick && !open;
                    st.follow_open_last_tick = open;
                    if edge {
                        st.follow_close_pending_at = Some(now);
                    }
                    // The session ended, and a cut answer's offer ends with
                    // it — "it left" must not leave something behind.
                    let dropped =
                        timeout_dropped.or_else(|| if edge { st.drop_system_cut() } else { None });
                    edge.then(|| (device.clone(), dropped))
                })
                .collect()
        };
        for (device, _room) in timed_out_rooms {
            // #77: silent timeout closes VoiceSession while keeping ContextSession.
            self.release_sovereignty(&device, now, false);
        }
        for (device, dropped) in &edges {
            if let Some(left) = dropped {
                self.note(
                    "program_cut_dropped",
                    json!({ "device": device, "reason": "follow_timeout", "bytes_left": left }),
                );
            }
        }
        // M7-1: the window closing BY TIMEOUT is a session end too. It runs
        // regardless of the tone setting — the tone is a UX choice, memory
        // extraction is not tied to it (and only proposes, never writes).
        for (device, _) in &edges {
            self.start_memory_extraction(device, "follow_timeout");
        }
        // The switch is a per-device preference; clear pending cues for devices
        // whose effective `follow_window_tone` is false so stale cues never linger.
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            for (device, st) in states.iter_mut() {
                if !settings.effective_for(device).follow_window_tone {
                    st.follow_close_pending_at = None;
                }
            }
        }
        // 2026-11-14 (user): the boundary is a SPOKEN cue at all times, and a
        // busy device must not lose it — it waits for playback to drain
        // instead of being skipped (the old tone path returned silently when
        // `busy`, which made "no tone = still listening" a lie).
        let due: Vec<(String, u64, OutputPermit)> = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .iter()
                .filter_map(|(device, st)| {
                    st.follow_close_pending_at.map(|at| {
                        (
                            device.clone(),
                            at,
                            OutputPermit {
                                generation: st.participation.generation(),
                                stop_epoch: st.stop_epoch,
                                request: None,
                            },
                        )
                    })
                })
                .collect()
        };
        for (device, pending_at, permit) in due {
            let media_playing = self.media_actively_playing(&device);
            let (muted, busy) = {
                let states = crate::lock_or_recover(&self.states, "voice.states");
                let Some(st) = states.get(&device) else {
                    continue;
                };
                if st.follow_close_pending_at != Some(pending_at) || !permit.valid(st) {
                    continue;
                }
                let non_media_live = |o: &OutputRecord| {
                    o.channel != "media"
                        && !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                };
                (
                    st.muted == Some(true),
                    st.current_task.is_some()
                        || st.outputs.values().any(non_media_live)
                        || media_playing,
                )
            };
            if muted {
                // A muted device makes no sound, full stop (privacy beats the
                // boundary cue).
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    st.follow_close_pending_at = None;
                }
                self.note(
                    "follow_window_close",
                    json!({ "device": device, "skipped": "muted" }),
                );
                continue;
            }
            if busy {
                if now.saturating_sub(pending_at) > FOLLOW_CLOSE_WAIT_MS {
                    if let Some(st) =
                        crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                    {
                        st.follow_close_pending_at = None;
                    }
                    self.note(
                        "follow_window_close",
                        json!({ "device": device, "skipped": "busy",
                                "waited_ms": now.saturating_sub(pending_at) }),
                    );
                }
                continue;
            }
            {
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                let Some(st) = states.get_mut(&device) else {
                    continue;
                };
                if st.follow_close_pending_at != Some(pending_at) || !permit.valid(st) {
                    continue;
                }
                st.follow_close_pending_at = None;
            }
            let pipe = self.clone();
            tokio::spawn(async move {
                pipe.speak_follow_window_close_at(&device, permit).await;
            });
        }
    }

    pub(super) async fn speak_follow_window_close_at(
        self: &Arc<Self>,
        device: &str,
        permit: OutputPermit,
    ) {
        let phrase = crate::lock_or_recover(&self.settings, "voice.settings")
            .window_close_ack
            .clone();
        match self.play_local_ack_at(device, &phrase, "follow_window_close", permit) {
            Some(mut ticket) => {
                // Contract v2 #4 (2026-11-16): this cue is recorded as kind
                // "ack", so its own PlayFinished cannot move the window it
                // announces — the rule lives in `note_voice_play_end`'s kind
                // gate, not in a per-play exception.
                let receipt =
                    tokio::time::timeout(std::time::Duration::from_secs(10), &mut ticket.receipt)
                        .await;
                let stage = match receipt {
                    Ok(Ok(crate::gateway::GatewayFrame::PlayReceipt { stage, .. })) => stage,
                    _ => "unknown".into(),
                };
                self.note(
                    "follow_window_close",
                    json!({ "device": device, "phrase": phrase, "stage": stage }),
                );
            }
            None => {
                // No pre-synthesized asset for this phrase yet: say so (never
                // pretend the boundary happened silently) AND start building
                // it, so the next close can speak. Without a TTS credential
                // the build fails loudly in `wake_ack_synth`; the event here
                // stays honest either way.
                self.spawn_ack_synthesis();
                self.note(
                    "follow_window_close",
                    json!({ "device": device, "phrase": phrase,
                            "skipped": "no asset", "synth": "requested" }),
                );
            }
        }
    }

    /// instant they want to observe instead of sleeping 30 s.
    pub fn session_sweep_at(&self, now: u64) -> Vec<String> {
        let devices: Vec<String> = crate::lock_or_recover(&self.states, "voice.states")
            .keys()
            .cloned()
            .collect();
        let hub_lives: std::collections::HashMap<String, HubLive> = devices
            .into_iter()
            .map(|d| {
                let live = self.effective_hub_live(&d);
                (d, live)
            })
            .collect();
        let mut closed: Vec<(String, Option<usize>)> = Vec::new();
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            for (device, st) in states.iter_mut() {
                let hub_live = hub_lives.get(device).copied().unwrap_or_default();
                if let Some(dropped) = st.timeout_session_with_live(now, hub_live) {
                    closed.push((device.clone(), dropped));
                }
            }
        }
        for (device, dropped) in &closed {
            self.release_sovereignty(device, now, false);
            if let Some(left) = dropped {
                self.note(
                    "program_cut_dropped",
                    json!({ "device": device, "reason": "follow_timeout", "bytes_left": left }),
                );
            }
        }
        self.apply_capture_revocations();
        closed.into_iter().map(|(device, _)| device).collect()
    }

    /// 2026-11-13, device hang: the core closed a voice stream and the device
    /// never sent `play_finished` (it stopped sending frames entirely). For
    /// that shape — `play_end` sent, no terminal receipt since — the play is
    /// marked `unknown`, removed from the hub registry (so `activity`, LED and
    /// the console stop saying "speaking"), its live program is cleared when
    /// the turn is already over, and the next turn can start. The record goes
    /// on to the stale sweep, which removes it; `play_stalled` keeps the fact.
    ///
    /// Runs on the presentation ticker. Returns `(device, play_id)` pairs for
    /// tests and logging. `now` is injected so a test need not sleep 10 s.
    /// Is this voice output stalled? `end_sent` = the core closed the stream,
    /// `expected_ms` = how much audio the play carried. A device cannot play
    /// faster than realtime, so a play is only stalled once even the earliest
    /// possible end (start + duration) is `PLAY_STALL_MS` in the past. That
    /// missing term cut 19.3 s of a healthy news program in the 2026-11-13
    /// field run: the whole answer had been submitted ahead of playback, so
    /// "10 s after play_end" arrived while the tail was still audible.
    pub(super) fn play_stall_due(rec: &OutputRecord, now: u64) -> bool {
        if rec.channel != "voice" || !stage_is_live(&rec.stage) {
            return false;
        }
        let Some(end) = rec.end_sent_ms else {
            return false;
        };
        if rec.expected_ms > 0 {
            now.saturating_sub(rec.started_ms.saturating_add(rec.expected_ms)) >= PLAY_STALL_MS
        } else {
            now.saturating_sub(end.max(rec.stage_ms)) >= PLAY_STALL_MS
        }
    }

    pub fn play_stall_sweep_at(&self, now: u64) -> Vec<(String, String)> {
        struct Stalled {
            device: String,
            play_id: String,
            task_id: Option<String>,
            stage: String,
            waited_ms: u64,
        }
        let mut stalled: Vec<Stalled> = Vec::new();
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            for (device, st) in states.iter_mut() {
                let mut hit: Vec<(String, u64)> = Vec::new();
                for (pid, rec) in st.outputs.iter() {
                    if Self::play_stall_due(rec, now) {
                        let waited =
                            now.saturating_sub(rec.end_sent_ms.unwrap_or(now).max(rec.stage_ms));
                        hit.push((pid.clone(), waited));
                    }
                }
                for (pid, waited) in hit {
                    let rec = match st.outputs.get_mut(&pid) {
                        Some(r) => r,
                        None => continue,
                    };
                    let stage_before = rec.stage.clone();
                    rec.stage = "unknown".into();
                    rec.stage_ms = now;
                    let task_id = rec.task_id.clone();
                    let terminal = rec.clone();
                    st.note_output_terminal(&pid, &terminal, now);
                    if st
                        .filler_play
                        .as_ref()
                        .map(|f| f.play_id == pid)
                        .unwrap_or(false)
                    {
                        st.filler_play = None;
                    }
                    // The program this play was reading is over for the
                    // listener only if the turn is not still working (tool
                    // phase). A finished turn's live program must not survive.
                    let belongs = st
                        .live_program
                        .as_ref()
                        .map(|p| p.origin_task.is_some() && p.origin_task == task_id)
                        .unwrap_or(false);
                    if belongs && st.current_task != task_id {
                        st.live_program = None;
                    }
                    stalled.push(Stalled {
                        device: device.clone(),
                        play_id: pid,
                        task_id,
                        stage: stage_before,
                        waited_ms: waited,
                    });
                }
            }
        }
        let mut out = Vec::new();
        for s in stalled {
            // Best effort: a wedged device will not answer, a slow one is
            // told to drop what it still holds. The registry is cleared here
            // (no receipt is coming), so the hub view cannot keep the LED on.
            let _ = self.hub.stop(&s.play_id);
            self.hub.forget_play(&s.play_id);
            self.note(
                "play_stalled",
                json!({
                    "device": s.device,
                    "play_id": s.play_id,
                    "task": s.task_id,
                    "stage_before": s.stage,
                    "waited_ms": s.waited_ms,
                    "bound_ms": PLAY_STALL_MS,
                    "action": "output_unknown_stream_closed_program_cleared",
                }),
            );
            self.push_assistant_state(&s.device, false);
            out.push((s.device, s.play_id));
        }
        out
    }

    /// Drop output records that can no longer receive a receipt or are past
    /// their terminal age. A `stopping` record whose play left the hub
    /// registry has no future `stop_receipt`/`play_finished` — that is the
    /// four-`stage=stopping`-after-idle leak (R1i observation). `failed` and
    /// `unknown` are terminal, so they are kept briefly for the console and
    /// then removed; `/api/v0/mode.outputs` converges on the truth.
    pub fn stale_output_sweep_at(&self, now: u64) -> Vec<(String, String)> {
        let devices: Vec<String> = crate::lock_or_recover(&self.states, "voice.states")
            .keys()
            .cloned()
            .collect();
        let mut removed = Vec::new();
        for device in devices {
            let hub_ids: std::collections::HashSet<String> = self
                .hub
                .active_plays_for(&device)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let Some(st) = states.get_mut(&device) else {
                continue;
            };
            let drop_ids: Vec<String> = st
                .outputs
                .iter()
                .filter_map(|(pid, rec)| {
                    let age = now.saturating_sub(rec.stage_ms);
                    let stale = match rec.stage.as_str() {
                        "stopping" => age >= OUTPUT_STALE_MS || !hub_ids.contains(pid),
                        "failed" | "unknown" => age >= OUTPUT_STALE_MS,
                        _ => false,
                    };
                    if stale {
                        Some(pid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for pid in drop_ids {
                if let Some(rec) = st.outputs.remove(&pid) {
                    st.note_output_terminal(&pid, &rec, now);
                    removed.push((device.clone(), format!("{}@{}", pid, rec.stage)));
                }
            }
        }
        removed
    }

    /// Forget bookmarks older than PROGRAM_TTL_MS. Runs on the presentation
    /// ticker: the ring must converge on its own even if nobody says anything.
    /// Expire as of `now` (the ticker passes the wall clock; a test passes a
    /// future instant). Public because the acceptance tests drive time
    /// explicitly — sleeping ten minutes is not a test.
    pub fn program_ttl_sweep_at(&self, now: u64) -> Vec<String> {
        let mut expired: Vec<(String, serde_json::Value)> = Vec::new();
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            for (dev, st) in states.iter_mut() {
                let stale = st
                    .paused_program
                    .as_ref()
                    .map(|p| now.saturating_sub(p.created_ms) >= PROGRAM_TTL_MS)
                    .unwrap_or(false);
                if stale {
                    if let Some(p) = st.paused_program.take() {
                        expired.push((
                            dev.clone(),
                            json!({ "cursor": p.cursor, "chars": p.text.len(), "complete": p.complete }),
                        ));
                    }
                }
            }
        }
        let mut devices = Vec::new();
        for (device, bookmark) in expired {
            self.note(
                "program_expired",
                json!({ "device": device, "bookmark": bookmark, "ttl_ms": PROGRAM_TTL_MS,
                        "why": "a paused program must not keep the ring amber for ever" }),
            );
            devices.push(device);
        }
        devices
    }

    /// M6c-2: push the saved output tone to one device (on connect, and after
    /// the console saves a new curve). The gateway applies it live.
    pub fn push_audio_eq(&self, device: &str) -> Result<(), String> {
        let cfg = crate::config::ConfigService::new(self.store.clone())
            .audio_eq()
            .map_err(|e| e.to_string())?;
        let bands: Vec<(f32, f32, f32)> =
            cfg.bands.iter().map(|b| (b.freq, b.gain_db, b.q)).collect();
        self.hub.audio_eq(device, &cfg.curve, &bands)?;
        self.note(
            "audio_eq_sent",
            json!({ "device": device, "curve": cfg.curve, "bands": bands.len() }),
        );
        Ok(())
    }

    pub(crate) fn push_assistant_state(&self, device: &str, force: bool) {
        const FLEET_PLAYBACK_RENEW_MS: u64 = 5_000;
        let now = id::now_ms();
        let hub_live = self.effective_hub_live(device);
        let media_preview = self
            .music()
            .and_then(|m| m.media_session(device))
            .and_then(|ms| {
                ms.current().map(|t| {
                    if t.artist.trim().is_empty() {
                        t.title.clone()
                    } else {
                        format!("{} {}", t.title, t.artist)
                    }
                })
            })
            .unwrap_or_default();
        // LED B: is a segment being captured right now? The mic port owns the
        // VAD verdict; the presentation state only reads it.
        let capturing = self
            .mic_arc(device)
            .map(|arc| crate::lock_or_recover(&arc, "voice.mic.dev").in_speech())
            .unwrap_or(false);
        let (state, fleet_notice, should_send) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let Some(st) = states.get_mut(device) else {
                return;
            };
            let state = st.assistant_state(now, hub_live, capturing);
            let changed = state != st.assistant_last;
            let active_channel = match state {
                "speaking" => "voice",
                "music" | "playing" => "media",
                _ => "",
            };
            let current_preview: String = if active_channel == "voice" {
                st.outputs
                    .values()
                    .find(|o| {
                        o.channel == "voice" && stage_is_live(&o.stage) && !o.answer_text.is_empty()
                    })
                    .map(|o| o.answer_text.clone())
                    .or_else(|| st.live_program.as_ref().map(|p| p.text.clone()))
                    .unwrap_or_default()
                    .chars()
                    .take(64)
                    .collect()
            } else if active_channel == "media" {
                let raw = if !media_preview.is_empty() {
                    media_preview
                } else {
                    st.outputs
                        .values()
                        .find(|o| {
                            o.channel == "media"
                                && stage_is_live(&o.stage)
                                && !o.content_ref.is_empty()
                        })
                        .map(|o| o.content_ref.clone())
                        .unwrap_or_default()
                };
                raw.chars().take(64).collect()
            } else {
                String::new()
            };
            let channel_changed = active_channel != st.fleet_playback_channel;
            let preview_changed =
                !active_channel.is_empty() && current_preview != st.fleet_playback_preview;
            let needs_renewal = !active_channel.is_empty()
                && now.saturating_sub(st.fleet_playback_sent_ms) >= FLEET_PLAYBACK_RENEW_MS;
            let fleet_notice = if channel_changed || preview_changed || needs_renewal {
                st.fleet_playback_channel = active_channel;
                st.fleet_playback_preview = current_preview.clone();
                st.fleet_playback_sent_ms = if active_channel.is_empty() { 0 } else { now };
                if active_channel == "voice" {
                    Some(("voice", true, current_preview))
                } else if active_channel == "media" {
                    Some(("media", true, current_preview))
                } else {
                    Some(("idle", false, String::new()))
                }
            } else {
                None
            };
            if changed || force {
                st.assistant_last = state.to_string();
            }
            (state.to_string(), fleet_notice, changed || force)
        };
        if let Some((channel, active, preview)) = fleet_notice {
            self.notify_fleet_playback(device, channel, active, &preview);
        }
        if should_send {
            if self.hub.assistant_state(device, &state).is_ok() {
                self.note(
                    "assistant_state",
                    json!({ "device": device, "state": state }),
                );
            } else {
                // No gateway on the wire: mark unsent so the next push
                // (ticker or reconnect) resends the current value.
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.assistant_last = String::new();
                }
            }
        }
    }

    pub(super) fn output_permit(
        &self,
        device: &str,
        request: Option<RequestTicket>,
    ) -> Option<OutputPermit> {
        let states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get(device)?;
        let permit = OutputPermit {
            generation: st.participation.generation(),
            stop_epoch: st.stop_epoch,
            request,
        };
        permit.valid(st).then_some(permit)
    }

    pub(super) fn apply_capture_revocations(&self) {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        for (device, st) in states.iter_mut() {
            self.apply_capture_revocation_locked(device, st);
        }
    }

    pub(super) fn apply_capture_revocation_locked(&self, device: &str, st: &mut DeviceVoiceState) {
        if !st.participation.take_capture_reset() {
            return;
        }
        st.revoke_input("cancelled");
        if let Some(arc) = self.mic_arc(device) {
            crate::lock_or_recover(&arc, "voice.mic.dev").reset();
        }
        st.barge.reset();
    }

    pub(super) fn source_identity(
        &self,
        device: &str,
        provenance: &crate::gateway::InputProvenance,
        via: &str,
    ) -> Option<Identity> {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.entry(device.to_string()).or_default();
        self.apply_capture_revocation_locked(device, st);
        if provenance.transport.as_ref().is_none_or(|t| {
            st.input_connection_generation
                .is_none_or(|g| g == t.generation)
        }) && st.participation.accepts_origin(&provenance.origin)
        {
            if let Some(epoch) = input::capture_epoch(&provenance.origin) {
                if st
                    .current_capture_epoch
                    .as_deref()
                    .is_some_and(|old| old != epoch)
                {
                    st.revoke_input("cancelled");
                    if let Some(arc) = self.mic_arc(device) {
                        crate::lock_or_recover(&arc, "voice.mic.dev").reset();
                    }
                }
                st.current_capture_epoch = Some(epoch.to_owned());
            }
            return Some(st.participation.identity());
        }
        drop(states);
        self.note(
            "stale_session_event_rejected",
            json!({"device":device,"via":via,"provenance":provenance}),
        );
        None
    }

    pub(super) fn exit_text_for(&self, device: &str, text: &str) -> bool {
        let words = crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .retreat_words;
        let wakes = self.effective_wake_words_for(device);
        let matches = |text: &str| {
            self.reflex_intent_for(device, &normalize_utterance(text))
                == Some(interaction::KeywordIntent::Retreat)
                || retreat_match(text, &words)
        };
        matches(text)
            || wakes
                .iter()
                .filter_map(|w| mic::wake_span_near(text, w))
                .any(|span| matches(&mic::wake_tail(text, span)))
    }

    /// Test setup for old behavior scenarios whose text used to self-open.
    /// The original wire say/seq helper remains unchanged.
    pub fn test_activate_session(&self, device: &str) {
        self.open_participation(device, CreditGrant::Full);
    }

    pub fn capture_lease(&self, device: &str) -> String {
        crate::lock_or_recover(&self.states, "voice.states")
            .entry(device.to_string())
            .or_default()
            .participation
            .capture_lease()
    }

    pub(super) async fn handle(self: &Arc<Self>, ev: DeviceEvent) {
        if let Some(music) = self.music() {
            music.lifecycle_event(&ev);
        }
        match ev {
            DeviceEvent::PlayRejected {
                play_id,
                device,
                reason,
                ..
            } => {
                self.note(
                    "play_rejected",
                    json!({ "play_id": play_id, "device": device, "reason": reason }),
                );
            }
            DeviceEvent::ConfigState { .. } | DeviceEvent::ConfigReceipt { .. } => {}
            DeviceEvent::DeviceJoined {
                device,
                generation,
                aec_tail_ms,
                profile_version,
            } => {
                {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.stable_id.clone()).or_default();
                    if st
                        .input_connection_generation
                        .is_some_and(|g| g != generation)
                    {
                        st.revoke_input("cancelled");
                        st.participation.reset_capture();
                    }
                    st.input_connection_generation = Some(generation);
                    st.acoustics_reconciled = false;
                    st.kws_reconciled = false;
                }
                if let Some(pv) = profile_version {
                    let mut levels = crate::lock_or_recover(&self.mic_levels, "voice.mic_levels");
                    levels
                        .entry(device.stable_id.clone())
                        .or_default()
                        .profile_version = Some(pv);
                }
                if let Some(aec_ms) = aec_tail_ms {
                    let tail_blind_ms = crate::lock_or_recover(&self.settings, "voice.settings")
                        .effective_for(&device.stable_id)
                        .tail_blind_ms;
                    if tail_blind_ms < u64::from(aec_ms) {
                        tracing::warn!(
                            device = %device.stable_id,
                            tail_blind_ms,
                            aec_tail_ms = aec_ms,
                            "tail_blind_ms is shorter than device aec_tail_ms"
                        );
                        self.note(
                            "tail_mismatch",
                            json!({
                                "device": device.stable_id,
                                "tail_blind_ms": tail_blind_ms,
                                "aec_tail_ms": aec_ms,
                            }),
                        );
                    }
                }
                self.on_device_online(&device.stable_id, &device.capabilities);
            }
            DeviceEvent::WifiState {
                device,
                ssid,
                state,
                detail,
                ..
            } => {
                if matches!(state.as_str(), "applied" | "connected" | "associated") {
                    self.clear_wifi_provisioning(&device);
                }
                // v0.2.3: wifi change outcome — visible in the voice log
                // (the wizard API holds its own waiter for the reply).
                self.note(
                    "wifi_state",
                    json!({ "device": device, "ssid": ssid, "state": state, "detail": detail }),
                );
            }
            DeviceEvent::DeviceStatus {
                device,
                online,
                reason,
                descriptor,
            } => {
                self.note(
                    "device_status",
                    json!({ "device": device, "online": online, "reason": reason }),
                );
                if online {
                    let caps = descriptor
                        .as_ref()
                        .map(|d| d.capabilities.clone())
                        .unwrap_or_default();
                    self.on_device_online(&device, &caps);
                } else {
                    self.on_device_offline(&device);
                }
            }
            DeviceEvent::DeviceLeft { device } => {
                self.on_device_offline(&device);
            }
            DeviceEvent::MicReport {
                device,
                active,
                detail,
            } => {
                self.note(
                    "mic_report",
                    json!({ "device": device, "active": active, "detail": detail }),
                );
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    st.mic_active = Some(active);
                    st.mic_detail = detail;
                }
            }
            DeviceEvent::AudioChunk {
                device,
                pcm,
                sample_rate,
                channels,
                ts,
                provenance,
                remote_vad,
                ..
            } => {
                self.feed_chunk_with_vad(
                    device,
                    pcm,
                    sample_rate,
                    channels,
                    ts,
                    provenance,
                    remote_vad,
                );
            }
            DeviceEvent::Wake {
                device,
                ts,
                provenance,
                ..
            } => {
                self.clear_live_turn(&device);
                let Some(expected) = self.source_identity(&device, &provenance, "wake") else {
                    return;
                };
                let pre_paused = self.pause_media_before_wake_activate(&device, expected, None);
                let Some(identity) =
                    self.activate_session_at(&device, CreditGrant::Full, expected, "protocol_wake")
                else {
                    return;
                };
                self.note(
                    "wake",
                    json!({ "device": device, "ts": ts, "provenance": provenance }),
                );
                self.barge_in_if_half_duplex(&device, "wake", identity, pre_paused);
            }
            DeviceEvent::Utterance {
                device,
                ts,
                source,
                text,
                provenance,
                ..
            } => {
                self.note("utterance", json!({ "device": device, "source": source, "text": text, "ts": ts, "provenance": provenance }));
                if !crate::mic::has_speech_chars(&text) {
                    if let Some(st) =
                        crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                    {
                        st.mic_counters.segment_empty += 1;
                    }
                    self.note(
                        "mic_segment_empty",
                        json!({ "device": device, "source": source, "text": text }),
                    );
                    return;
                }
                let Some(mut identity) = self.source_identity(&device, &provenance, "utterance")
                else {
                    return;
                };
                // Android MediaSession / Music Dock buttons emit deterministic
                // playback utterances (`暂停播放`/`继续播放`/`下一首`/`上一首`/`停止播放`).
                // Handle them BEFORE `exit_text` so `"停止播放"` stops media/programs
                // cleanly without wiping conversation history, speaking `retreat_ack`,
                // or failing after a prior `ExplicitExit`.
                if source == "android_media_session" {
                    let norm = normalize_utterance(last_sentence(&text));
                    let media_intent = match norm.as_str() {
                        "暂停播放" => Some(interaction::KeywordIntent::Pause),
                        "继续播放" => Some(interaction::KeywordIntent::Continue),
                        "下一首" => Some(interaction::KeywordIntent::Next),
                        "停止播放" => Some(interaction::KeywordIntent::Stop),
                        _ => None,
                    };
                    if let Some(intent) = media_intent {
                        match self.run_reflex_intent(
                            &device,
                            &text,
                            intent,
                            "android_media_session",
                            Some(identity),
                        ) {
                            Ok(detail) => self.note(
                                "reflex_text",
                                json!({
                                    "device": device,
                                    "keyword": text,
                                    "intent": Self::intent_name(intent),
                                    "via": "android_media_session",
                                    "detail": detail,
                                }),
                            ),
                            Err(reason) => self.note(
                                "keyword_ignored",
                                json!({
                                    "device": device,
                                    "keyword": text,
                                    "reason": reason,
                                    "via": "android_media_session",
                                }),
                            ),
                        }
                        self.push_assistant_state(&device, false);
                        return;
                    }
                    if norm == "上一首" {
                        let identity_ok = {
                            let states = crate::lock_or_recover(&self.states, "voice.states");
                            states
                                .get(&device)
                                .is_some_and(|st| st.participation.identity() == identity)
                        };
                        if !identity_ok {
                            return;
                        }
                        if let Some(music) = self.music() {
                            if music.media_present(&device) {
                                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                music.register_control(&device, cancel.clone());
                                let dev = device.clone();
                                let pipe = self.clone();
                                tokio::spawn(async move {
                                    match music
                                        .control_with_cancel(&dev, "previous", None, cancel, None)
                                        .await
                                    {
                                        Ok(out) => pipe.note(
                                            "reflex_text",
                                            json!({
                                                "device": dev,
                                                "keyword": "上一首",
                                                "intent": "previous",
                                                "via": "android_media_session",
                                                "detail": { "target": out.target, "title": out.subject, "confirmed": out.confirmed }
                                            }),
                                        ),
                                        Err(e) => pipe.note(
                                            "keyword_ignored",
                                            json!({
                                                "device": dev,
                                                "keyword": "上一首",
                                                "reason": e.to_string(),
                                                "via": "android_media_session"
                                            }),
                                        ),
                                    }
                                    pipe.push_assistant_state(&dev, false);
                                });
                                return;
                            }
                        }
                        self.note(
                            "keyword_ignored",
                            json!({
                                "device": device,
                                "keyword": "上一首",
                                "reason": "reflex_precondition:no_media",
                                "via": "android_media_session",
                            }),
                        );
                        return;
                    }
                }
                if self.exit_text_for(&device, &text) {
                    self.clear_live_turn(&device);
                    self.retreat_session_at(&device, &text, "utterance", identity);
                    return;
                }
                // Only a wake is an activation. Ordinary text cannot open.
                if self
                    .reflex_text_command_for(&device, &text)
                    .is_some_and(|(_, i)| i == interaction::KeywordIntent::Wake)
                {
                    self.clear_live_turn(&device);
                    let pre_paused = self.pause_media_before_wake_activate(&device, identity, None);
                    if let Some(identity) =
                        self.activate_session_at(&device, CreditGrant::Full, identity, "text_wake")
                    {
                        self.barge_in_if_half_duplex(&device, "wake", identity, pre_paused);
                    }
                    return;
                }
                // R1j/P1: playback reflex commands (`暂停`/`暂停播放`/`继续`/`继续播放`/
                // `下一首`/`停止播放`) must work even while media is playing with
                // `session_open == false` (each intent enforces its own precondition).
                if let Some((word, intent)) = self.reflex_text_command_for(&device, &text) {
                    if intent != interaction::KeywordIntent::Wake {
                        self.handle_reflex_text(&device, &word, intent, "utterance", identity);
                        return;
                    }
                }
                if normalize_utterance(last_sentence(&text)) == "上一首" {
                    let identity_ok = {
                        let states = crate::lock_or_recover(&self.states, "voice.states");
                        states
                            .get(&device)
                            .is_some_and(|st| st.participation.identity() == identity)
                    };
                    if !identity_ok {
                        return;
                    }
                    if let Some(music) = self.music() {
                        if music.media_present(&device) {
                            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            music.register_control(&device, cancel.clone());
                            let dev = device.clone();
                            let pipe = self.clone();
                            tokio::spawn(async move {
                                match music
                                    .control_with_cancel(&dev, "previous", None, cancel, None)
                                    .await
                                {
                                    Ok(out) => pipe.note(
                                        "reflex_text",
                                        json!({
                                            "device": dev,
                                            "keyword": "上一首",
                                            "intent": "previous",
                                            "via": "utterance_media",
                                            "detail": { "target": out.target, "title": out.subject, "confirmed": out.confirmed }
                                        }),
                                    ),
                                    Err(e) => pipe.note(
                                        "keyword_ignored",
                                        json!({
                                            "device": dev,
                                            "keyword": "上一首",
                                            "reason": e.to_string(),
                                            "via": "utterance_media"
                                        }),
                                    ),
                                }
                                pipe.push_assistant_state(&dev, false);
                            });
                            return;
                        }
                    }
                }
                let denied = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let Some(st) = states.get_mut(&device) else {
                        return;
                    };
                    if st.participation.identity() != identity {
                        return;
                    }
                    let reason = if !st.session_open(id::now_ms()) {
                        Some("session_closed")
                    } else if st.participation.request_started()
                        && st.participation.available_followup_credit().is_none()
                    {
                        Some("followup_credits_exhausted")
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        st.participation.last_denied = Some(
                            json!({"reason":reason,"at_ms":id::now_ms(),"hint":"请重新唤醒后再提问"}),
                        );
                    }
                    reason
                };
                if let Some(reason) = denied {
                    self.note(
                        "input_admission",
                        json!({"device":device,"decision":"deny","reason":reason}),
                    );
                    return;
                }
                // B-M1: stop / keep-going are consumed here as well — the
                // text path is the same user intent, not a model question.
                if self.handle_stop_intent_at(
                    &device,
                    &text,
                    "utterance",
                    Some(&mut identity),
                    None,
                ) {
                    return;
                }
                // A new utterance interrupts playback in half duplex (the
                // user is talking over it); the old turn, if still running,
                // is cancelled — its late results stay recorded.
                let Some(next) = self.barge_in_if_half_duplex(&device, "utterance", identity, None)
                else {
                    return;
                };
                identity = next;
                let Some(ticket) = self.reserve_turn_with_credit(&device, identity, true) else {
                    return;
                };
                let pipe = self.clone();
                tokio::spawn(async move {
                    pipe.run_device_turn_at(device, text, format!("device:{source}"), ticket)
                        .await;
                });
            }
            DeviceEvent::Keyword {
                device,
                ts,
                keyword,
                fed_ms,
                self_heard,
                rms_dbfs,
                lag_ms,
                kws_tier,
                wake_dur_ms,
                max_gap_ms,
                l2_passed,
                provenance,
            } => {
                if let Some(db) = rms_dbfs.and_then(crate::gateway::valid_db) {
                    let mut levels = crate::lock_or_recover(&self.mic_levels, "voice.mic_levels");
                    let entry = levels.entry(device.clone()).or_default();
                    entry.rms_dbfs = Some(db);
                    entry.at_ms = id::now_ms();
                }
                let acoustics = WakeAcoustics {
                    tier: kws_tier,
                    wake_dur_ms,
                    max_gap_ms,
                    l2_passed,
                    wake_end_cursor: None,
                };
                self.on_keyword_at(
                    &device, &keyword, ts, fed_ms, self_heard, provenance, rms_dbfs, lag_ms,
                    acoustics, false,
                );
            }
            DeviceEvent::KwsDeployReceipt {
                device,
                accepted,
                hash,
                keywords,
                detail,
                local_exit,
                intents_hash,
                threshold,
                score,
            } => {
                let now = id::now_ms();
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    let d = st.reflex_deploy.get_or_insert_with(ReflexDeploy::default);
                    d.state = if accepted {
                        "accepted".into()
                    } else {
                        "rejected".into()
                    };
                    d.hash = hash.clone();
                    d.keywords = keywords;
                    d.detail = detail.clone();
                    d.acked_ms = now;
                    // #5: what the DEVICE armed for local exit. A rejected
                    // deploy arms nothing; an accepted one that dropped words
                    // shows up as local_exit < retreat_words_sent.
                    d.local_exit = if accepted { local_exit } else { 0 };
                    d.intents_hash_installed = intents_hash.clone();
                    // #4: what the spotter will run with (0 = not tunable on
                    // this gateway; the settings value stays what we sent).
                    d.applied_threshold = if accepted { threshold } else { 0.0 };
                    d.applied_score = if accepted { score } else { 0.0 };
                }
                self.note(
                    "kws_deploy",
                    json!({ "device": device, "accepted": accepted, "hash": hash,
                            "keywords": keywords, "detail": detail,
                            "local_exit": local_exit, "intents_hash": intents_hash,
                            "threshold": threshold, "score": score }),
                );
                self.push_assistant_state(&device, false);
            }
            // Contract v2 §2.5 (#5): the device cut its own audio on a retreat
            // word. A fact with no session authority — the `Keyword` event
            // beside it is what closes the session, so a repeat or a late
            // fact can never produce a second answer.
            DeviceEvent::LocalExitFlushed {
                device,
                keyword,
                ts,
                latency_ms,
                channels,
                detail,
            } => {
                // A7 (#116): non-owner local exit must remain silent and must
                // never record a session exit or flush unrelated playback.
                if !self.has_active_session_or_playback(&device) {
                    self.note(
                        "local_exit_ignored_not_owner",
                        json!({
                            "device": device,
                            "keyword": keyword,
                            "ts": ts,
                            "latency_ms": latency_ms,
                            "channels": channels,
                            "detail": detail,
                            "reason": "no_active_session_or_playback_owner",
                        }),
                    );
                    return;
                }
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    if let Some(d) = st.reflex_deploy.as_mut() {
                        d.local_exits += 1;
                        d.last_local_exit = Some(json!({
                            "keyword": keyword, "ts": ts, "latency_ms": latency_ms,
                            "channels": channels, "detail": detail,
                        }));
                    }
                }
                self.note(
                    "local_exit_flushed",
                    json!({ "device": device, "keyword": keyword, "ts": ts,
                            "latency_ms": latency_ms, "channels": channels,
                            "detail": detail }),
                );
            }
            DeviceEvent::KwsReport {
                device,
                state,
                detail,
                restarts,
                detections,
                guarded,
                epoch,
                loaded_hash,
                loaded_ms,
                local_exit,
                threshold,
                score,
            } => {
                let now = id::now_ms();
                let mut epoch_reset = false;
                let mut load_match: Option<bool> = None;
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    st.kws_state = Some(state.clone());
                    st.kws_detail = Some(detail.clone());
                    st.kws_restarts = restarts;
                    st.kws_detections = detections;
                    st.kws_guarded = guarded;
                    // Contract v2 §2.5 (#5): the DEVICE's armed local-exit
                    // count. Durable across core restarts, unlike the deploy
                    // record — this is what the console and activation read.
                    st.kws_local_exit = local_exit;
                    // Contract v2 #4: the sensitivity the loaded model says
                    // it is running with (0 = an older gateway).
                    st.kws_threshold = threshold;
                    st.kws_score = score;
                    // R1g/P7: a gateway restart keeps its lifetime counters
                    // while this core's are zeroed. Remember the epoch and the
                    // gateway's count at the moment we first saw it; the two
                    // numbers are only comparable inside that window.
                    if epoch != st.kws_epoch {
                        st.kws_epoch = epoch;
                        st.kws_detections_base = detections;
                        st.kws_epoch_seen_ms = now;
                        epoch_reset = true;
                    }
                    // M6b: the LOAD PROOF. Only a non-empty hash that arrived
                    // with a running spotter counts; an empty one (old
                    // gateway, or the process just exited) clears it rather
                    // than leaving a stale "it loaded X".
                    if let Some(d) = st.reflex_deploy.as_mut() {
                        if loaded_hash.is_empty() {
                            if state != "running" {
                                d.loaded_hash = None;
                                d.loaded_ms = 0;
                            }
                        } else {
                            d.loaded_hash = Some(loaded_hash.clone());
                            d.loaded_ms = loaded_ms;
                            load_match = Some(loaded_hash == d.hash);
                            if load_match == Some(true) {
                                // The whole chain is proven: compiled → sent →
                                // installed → loaded. Only then does the state
                                // say so.
                                if d.state == "accepted" {
                                    d.state = "loaded".into();
                                }
                            }
                        }
                    }
                }
                self.note(
                    "kws_report",
                    json!({ "device": device, "state": state, "detail": detail,
                            "restarts": restarts, "detections": detections, "guarded": guarded,
                            "kws_epoch": epoch, "new_epoch": epoch_reset,
                            // M6b: the spotter's load proof and whether it
                            // matches what the device installed.
                            "loaded_hash": loaded_hash, "loaded_ms": loaded_ms,
                            "loaded_matches": load_match }),
                );
            }
            DeviceEvent::BargeIn {
                device,
                ts,
                rx_ms,
                pcm,
                sample_rate,
                corr,
                energy_db,
                ref_db,
                residual_db,
                ref_locked,
            } => {
                self.on_barge_in(
                    &device,
                    ts,
                    rx_ms,
                    pcm,
                    sample_rate,
                    corr,
                    energy_db,
                    ref_db,
                    residual_db,
                    ref_locked,
                );
            }
            // R1d: the device's own reflex already cut the voice channel.
            // This is bookkeeping, not a decision: close the output record
            // and remember the hardware fact so the BargeIn that follows can
            // still become a turn (the audio it describes is already gone).
            DeviceEvent::StoppedLocally {
                device,
                task_id,
                reason,
                played_ms,
                latency_ms,
                ahead_ms,
                ts,
            } => {
                self.on_stopped_locally(
                    &device, &task_id, &reason, played_ms, latency_ms, ahead_ms, ts,
                );
            }
            DeviceEvent::Button { device, action, .. } => {
                // Every button action lands in the pipeline log (G6 §10:
                // Mute long-press 113_hold:<ms>ms must be visible in the
                // console event page); only action_press also stops.
                // Owner rule (#16): physical button hold NEVER triggers factory
                // reset; only `mute_hold_wifi_setup` records Wi-Fi provisioning.
                self.note("button", json!({ "device": device, "action": action }));
                if action == "mute_hold_wifi_setup" {
                    let now = id::now_ms();
                    crate::lock_or_recover(&self.wifi_provisioning, "voice.wifi_provisioning")
                        .insert(device.clone(), now);
                    let prompt = "已进入配网模式，请在管理台配置无线网络。";
                    self.note(
                        "wifi_provisioning_requested",
                        json!({
                            "device": device,
                            "action": action,
                            "trigger": "mute_hold",
                            "prompt": prompt,
                        }),
                    );
                    self.note(
                        "wifi_provisioning_prompt",
                        json!({
                            "device": device,
                            "text": prompt,
                        }),
                    );
                    if let Some(permit) = self.output_permit(&device, None) {
                        let pipe = self.clone();
                        let dev = device.clone();
                        tokio::spawn(async move {
                            let _ = pipe
                                .speak_inner_at(&dev, prompt, None, "prompt", true, permit)
                                .await;
                        });
                    }
                } else if action == "action_press" {
                    self.note("button_stop", json!({ "device": device }));
                    let result = self.stop_device(&device);
                    self.note("stop_result", result);
                }
            }
            DeviceEvent::MuteState {
                device,
                muted,
                source,
                ..
            } => {
                self.note(
                    "mute_state",
                    json!({ "device": device, "muted": muted, "source": source }),
                );
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    st.muted = Some(muted);
                    if muted {
                        st.revoke_input("cancelled");
                    }
                    // v1 mechanism: the latch's last writer, straight from the
                    // device fact ("button"/"hardware"/"device_boot"/"core_set").
                    st.mute_source = Some(source);
                }
                self.push_assistant_state(&device, false);
            }
            DeviceEvent::PlayStage {
                play_id,
                stage,
                detail,
            } => {
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                let mut touched_device = None;
                for (device, st) in states.iter_mut() {
                    if let Some(rec) = st.outputs.get_mut(&play_id) {
                        if rec.stage == "stopping"
                            && matches!(
                                stage.as_str(),
                                "accepted" | "sent" | "submitted" | "device_confirmed"
                            )
                        {
                            self.note(
                                "stale_session_event_rejected",
                                json!({"device":device,"via":"play_stage","play_id":play_id}),
                            );
                            continue;
                        }
                        rec.stage = stage.clone();
                        rec.stage_ms = id::now_ms();
                        touched_device = Some(device.clone());
                        // B-M0: a cue that reached its terminal stage ended
                        // on its own (played through) — it is no longer live,
                        // so the answer must not try to stop it later.
                        if matches!(stage.as_str(), "device_confirmed" | "failed" | "unknown")
                            && st
                                .filler_play
                                .as_ref()
                                .map(|f| f.play_id == play_id)
                                .unwrap_or(false)
                        {
                            st.filler_play = None;
                        }
                        // Latency accounting (G4): the moment the device
                        // confirms a voice play is the delivery datum.
                        if stage == "device_confirmed" && rec.channel == "voice" {
                            let sample = LatencySample {
                                synth_ms: rec.synth_ms,
                                confirm_ms: id::now_ms().saturating_sub(rec.started_ms),
                            };
                            st.latency.push_back(sample);
                            while st.latency.len() > 20 {
                                st.latency.pop_front();
                            }
                        }
                    }
                    if matches!(stage.as_str(), "failed" | "unknown") {
                        if let Some(rec) = st.outputs.get(&play_id).cloned() {
                            let superseded_by_same_request = stage == "unknown"
                                && detail
                                    .as_deref()
                                    .is_some_and(|d| d.contains("superseded by a new stream"))
                                && rec.request.is_some_and(|req| {
                                    st.outputs.iter().any(|(id, o)| {
                                        id != &play_id
                                            && o.channel == "voice"
                                            && o.request == Some(req)
                                            && stage_is_live(&o.stage)
                                    })
                                });
                            if superseded_by_same_request {
                                st.outputs.remove(&play_id);
                                if let Some(req) = rec.request {
                                    st.participation.answer_ended(req, &play_id, id::now_ms());
                                }
                            } else {
                                st.note_output_terminal(&play_id, &rec, id::now_ms());
                            }
                        }
                    }
                }
                drop(states);
                // R1g/P4: the cue's MODE decides whether another play start
                // ends it (radio: music may start under the cue; interrupt:
                // the cue only ever covers silence).
                if matches!(stage.as_str(), "accepted" | "sent") {
                    self.filler_yields_to_play(&play_id);
                }
                if let Some(device) = touched_device {
                    self.push_assistant_state(&device, false);
                } else if matches!(stage.as_str(), "accepted" | "streaming") {
                    self.push_all_assistant_states();
                }
                if let Some(d) = detail {
                    self.note(
                        "play_stage",
                        json!({ "play_id": play_id, "stage": stage, "detail": d }),
                    );
                }
            }
            DeviceEvent::StopStage {
                play_id,
                stage,
                outcome,
                played_ms,
                consumed_ms,
                progress_basis,
            } => {
                let confirmed = outcome.confirmed();
                let (terminal_stage, terminal_detail) = match &outcome {
                    crate::gateway::StopOutcome::Confirmed { .. } => {
                        (format!("stopped:{stage}"), None)
                    }
                    crate::gateway::StopOutcome::Failed { reason } => {
                        ("failed".into(), Some(reason.clone()))
                    }
                    crate::gateway::StopOutcome::Unknown { reason } => {
                        ("unknown".into(), Some(reason.clone()))
                    }
                };
                if let Some(tx) =
                    crate::lock_or_recover(&self.terminal_waiters, "voice.terminal_waiters")
                        .remove(&play_id)
                {
                    let _ = tx.send(SpokenTerminal {
                        play_id: play_id.clone(),
                        stage: terminal_stage.clone(),
                        played_ms,
                        consumed_ms,
                        progress_basis: if confirmed {
                            progress_basis.clone()
                        } else {
                            None
                        },
                        detail: terminal_detail,
                    });
                }
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                let mut touched_device = None;
                let mut audio_end: Option<(String, Option<String>, TurnAudio)> = None;
                for (device, st) in states.iter_mut() {
                    let mut stopped_answer: Option<Option<String>> = None;
                    if let Some(mut rec) = st.outputs.remove(&play_id) {
                        if !confirmed {
                            rec.stage = "unknown".into();
                            st.outputs.insert(play_id.clone(), rec.clone());
                        }
                        st.note_output_terminal(&play_id, &rec, id::now_ms());
                        // M6b: a stopped answer is still part of this turn's
                        // audible timeline — and the device's own `played_ms`
                        // is the honest "how much they heard".
                        if confirmed && rec.kind == "answer" {
                            st.turn_audio.note_end(
                                id::now_ms(),
                                played_ms.unwrap_or(0),
                                rec.task_id.as_deref(),
                            );
                            stopped_answer = Some(rec.task_id.clone());
                        }
                        touched_device = Some(device.clone());
                    }
                    if let Some(task) = stopped_answer {
                        let task_matches = match (st.turn_audio.task.as_deref(), task.as_deref()) {
                            (Some(mine), Some(theirs)) => mine == theirs,
                            _ => true,
                        };
                        if task_matches
                            && !st
                                .outputs
                                .values()
                                .any(|o| o.kind == "answer" && stage_is_live(&o.stage))
                        {
                            audio_end = Some((device.clone(), task, st.turn_audio.clone()));
                        }
                    }
                    // B-M0: the cue is gone on the wire — drop the pointer so
                    // a later cancel cannot claim it was still live.
                    if st
                        .filler_play
                        .as_ref()
                        .map(|f| f.play_id == play_id)
                        .unwrap_or(false)
                    {
                        st.filler_play = None;
                    }
                }
                drop(states);
                if let Some((device, task, ta)) = audio_end {
                    self.note(
                        "turn_audio_end",
                        json!({
                            "device": device, "task": task,
                            "ended_ms": ta.last_end_ms,
                            "played_ms": ta.played_ms,
                            "answer_plays": ta.plays,
                        }),
                    );
                }
                if let Some(device) = touched_device {
                    self.push_assistant_state(&device, false);
                } else if confirmed {
                    self.push_all_assistant_states();
                }
                self.note(
                    "stop_stage",
                    json!({
                        "play_id": play_id, "stage": stage, "stop_outcome": outcome, "played_ms": played_ms,
                        // M6a: consumed vs written — the number M5's cursor
                        // will trust, and the basis that produced it.
                        "consumed_ms": consumed_ms, "progress_basis": progress_basis,
                    }),
                );
            }
            DeviceEvent::VolumeStage {
                play_id,
                effective,
                stage,
            } => {
                self.note(
                    "volume_stage",
                    json!({ "play_id": play_id, "effective": effective, "stage": stage }),
                );
            }
            DeviceEvent::PlayFinished {
                device,
                play_id,
                played_ms,
                consumed_ms,
                progress_basis,
                ..
            } => {
                if !crate::lock_or_recover(&self.states, "voice.states")
                    .get(&device)
                    .is_some_and(|s| s.outputs.contains_key(&play_id))
                {
                    if self.music().is_some_and(|m| {
                        m.playback_status(&device)["play_id"].as_str() == Some(&play_id)
                    }) {
                        self.push_assistant_state(&device, false);
                        self.note(
                            "play_finished",
                            json!({
                                "device": device, "play_id": play_id, "played_ms": played_ms,
                                "consumed_ms": consumed_ms, "progress_basis": progress_basis,
                            }),
                        );
                        return;
                    }
                    self.note(
                        "stale_session_event_rejected",
                        json!({"device":device,"via":"play_finished","play_id":play_id}),
                    );
                    return;
                }
                if let Some(tx) =
                    crate::lock_or_recover(&self.terminal_waiters, "voice.terminal_waiters")
                        .remove(&play_id)
                {
                    let _ = tx.send(SpokenTerminal {
                        play_id: play_id.clone(),
                        stage: "finished".into(),
                        played_ms: Some(played_ms),
                        consumed_ms,
                        progress_basis: progress_basis.clone(),
                        detail: None,
                    });
                }
                let blind_ms = crate::lock_or_recover(&self.settings, "voice.settings")
                    .effective_for(&device)
                    .tail_blind_ms
                    .clamp(0, 2000);
                let mut audio_end: Option<(Option<String>, TurnAudio)> = None;
                let mut voice_play_idle = false;
                let mut release_single_closed = false;
                let mut wake_ack_window_refreshed = false;
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    let finished = st.outputs.get(&play_id).cloned();
                    let was_voice = finished
                        .as_ref()
                        .map(|o| o.channel == "voice")
                        .unwrap_or(false);
                    let was_answer = finished
                        .as_ref()
                        .map(|o| o.kind == "answer")
                        .unwrap_or(false);
                    if was_answer {
                        // M6b: the answer audio for this turn ended here.
                        st.turn_audio.note_end(
                            id::now_ms(),
                            played_ms,
                            finished.as_ref().and_then(|o| o.task_id.as_deref()),
                        );
                    }
                    st.outputs.remove(&play_id);
                    // R1e: the last sentence of a finished program played
                    // through — the program is over. A still-generating one
                    // (complete=false) keeps its bookmark target.
                    if was_voice {
                        let done = st
                            .live_program
                            .as_ref()
                            .map(|p| {
                                p.complete
                                    && p.origin_task.is_some()
                                    && p.origin_task
                                        == finished.as_ref().and_then(|o| o.task_id.clone())
                            })
                            .unwrap_or(false)
                            && !st.outputs.values().any(|o| {
                                o.channel == "voice"
                                    && o.task_id.is_some()
                                    && o.task_id
                                        == finished.as_ref().and_then(|o| o.task_id.clone())
                            });
                        if done {
                            st.live_program = None;
                        }
                    }
                    // B-M0: a cue that played through is no longer live.
                    if st
                        .filler_play
                        .as_ref()
                        .map(|f| f.play_id == play_id)
                        .unwrap_or(false)
                    {
                        st.filler_play = None;
                    }
                    let single_closed = if was_voice {
                        // Contract v2 #4 (2026-11-16): the finished output's
                        // KIND decides whether this end may touch the window.
                        // `note_voice_play_end` is the single gate: an answer
                        // settles its request and sets the tail reference;
                        // every cue (ack / filler / prompt) only ends.
                        if let Some(rec) = finished.as_ref() {
                            st.note_voice_play_end(&play_id, rec, id::now_ms());
                        }
                        st.participation.session.state == SessionState::Idle
                    } else {
                        if finished.as_ref().is_some_and(|r| r.channel == "media") {
                            st.last_media_end_ms = id::now_ms();
                        }
                        false
                    };
                    if single_closed {
                        release_single_closed = true;
                    }
                    let now_finished = id::now_ms();
                    let until = now_finished.saturating_add(blind_ms);
                    if was_voice && st.wake_ack_play.as_deref() == Some(play_id.as_str()) {
                        st.wake_ack_tail_until_ms = until;
                        st.wake_ack_play = None;
                        wake_ack_window_refreshed =
                            st.participation.refresh_wake_ack_window(now_finished);
                    } else {
                        st.other_voice_tail_until_ms = until;
                    }
                    if blind_ms > 0 {
                        // Echo fix v2: our own playback just ended — the
                        // room reverb tail (beyond the AEC's 160 ms (TAIL = 2560) reference
                        // window) must not open a mic segment.
                        st.self_play_blind_until_ms = id::now_ms().saturating_add(blind_ms);
                    }
                    // M6b: the turn's LAST answer play is over — that is the
                    // only moment the audible end is a fact (turn_end may
                    // have been written while this sentence was still
                    // draining). A play from the PREVIOUS turn draining into
                    // this one is not this turn's end.
                    let ended_task = finished.as_ref().and_then(|o| o.task_id.clone());
                    let task_matches = match (st.turn_audio.task.as_deref(), ended_task.as_deref())
                    {
                        (Some(mine), Some(theirs)) => mine == theirs,
                        _ => true,
                    };
                    if was_voice
                        && !st
                            .outputs
                            .values()
                            .any(|o| o.channel == "voice" && stage_is_live(&o.stage))
                    {
                        voice_play_idle = true;
                    }
                    if was_answer
                        && task_matches
                        && !st
                            .outputs
                            .values()
                            .any(|o| o.kind == "answer" && stage_is_live(&o.stage))
                    {
                        audio_end = Some((ended_task, st.turn_audio.clone()));
                    }
                }
                if release_single_closed {
                    self.release_sovereignty(&device, id::now_ms(), false);
                } else if wake_ack_window_refreshed {
                    self.renew_sovereignty(&device, id::now_ms());
                }
                if voice_play_idle {
                    let reset_tainted_echo = {
                        if let Some(arc) = self.mic_arc(&device) {
                            let mut p = crate::lock_or_recover(&arc, "voice.mic.dev");
                            let open_spans = p.open_provenance().spans;
                            let tainted_open = p.in_speech()
                                && !open_spans.is_empty()
                                && open_spans
                                    .iter()
                                    .all(|s| s.control_only && !s.wake_ack_overlap);
                            if tainted_open {
                                p.reset();
                                true
                            } else {
                                p.clear_tainted_onset()
                            }
                        } else {
                            false
                        }
                    };
                    if reset_tainted_echo {
                        if let Some(st) =
                            crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                        {
                            if st.input.as_ref().is_some_and(|i| !i.recognizing) {
                                st.revoke_input("play_end_echo_reset");
                            }
                        }
                        self.note(
                            "mic_echo_segment_reset_on_play_end",
                            json!({ "device": device, "play_id": play_id }),
                        );
                    }
                }
                if let Some((task, ta)) = audio_end {
                    self.note(
                        "turn_audio_end",
                        json!({
                            "device": device, "task": task,
                            // Absolute, so it joins `turn_end.timeline` by task.
                            "ended_ms": ta.last_end_ms,
                            "played_ms": ta.played_ms,
                            "answer_plays": ta.plays,
                        }),
                    );
                }
                self.push_assistant_state(&device, false);
                self.note(
                    "play_finished",
                    json!({
                        "device": device, "play_id": play_id, "played_ms": played_ms,
                        // M6a: written vs consumed, with the basis. M5's
                        // resume cursor reads consumed_ms; a "written" basis
                        // means the device could not measure the DAC.
                        "consumed_ms": consumed_ms, "progress_basis": progress_basis,
                    }),
                );
            }
            DeviceEvent::ModeReport {
                device,
                duplex,
                aec,
                seq,
            } => {
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
                {
                    st.duplex = Some(duplex.clone());
                    st.duplex_report_seq = seq;
                    // M6b: the owner is a DEVICE fact. An empty/unreported
                    // owner stays unknown — the core must never fill in
                    // "core" by itself (that was the gateway's old lie, and a
                    // guess here would hide it again).
                    st.aec = (!aec.owner.trim().is_empty()).then(|| aec.clone());
                }
                self.note(
                    "mode_report",
                    json!({ "device": device, "duplex": duplex, "aec": aec }),
                );
            }
            DeviceEvent::GatewayConnected { .. } => {
                // Fresh connection, no presentation state: force a resend so
                // the gateway's LED mapping catches up immediately.
                self.push_all_assistant_states();
            }
            DeviceEvent::GatewayDisconnected { .. } => {}
            DeviceEvent::EchoHealth {
                device,
                health,
                degraded_ms,
                reason,
            } => {
                {
                    let mut levels = crate::lock_or_recover(&self.mic_levels, "voice.mic_levels");
                    let entry = levels.entry(device.clone()).or_default();
                    entry.echo_health = Some(health.clone());
                }
                if health != "healthy" {
                    self.note(
                        "echo_health_degraded",
                        json!({
                            "device": device,
                            "health": health,
                            "degraded_ms": degraded_ms,
                            "reason": reason,
                        }),
                    );
                }
            }
            DeviceEvent::NearEndPresence {
                device,
                ts_ms,
                duration_ms: _,
                rms_dbfs,
                noise_floor_dbfs,
                snr_db,
                clipped,
                echo_health,
                profile_version,
            } => {
                let now = id::now_ms();
                let speech_present =
                    snr_db.is_none_or(|s| s.is_finite() && s >= crate::arbitration::MIN_SNR_DB);
                {
                    let mut levels = crate::lock_or_recover(&self.mic_levels, "voice.mic_levels");
                    let entry = levels.entry(device.clone()).or_default();
                    let holds_fresh_speech = entry.at_ms > 0
                        && now.saturating_sub(entry.at_ms)
                            <= crate::arbitration::COALESCE_MS as u64
                        && entry
                            .snr_db
                            .is_some_and(|s| s.is_finite() && s >= crate::arbitration::MIN_SNR_DB);
                    if let Some(nf) = noise_floor_dbfs.and_then(crate::gateway::valid_db) {
                        entry.noise_floor_dbfs = Some(nf);
                    }
                    if speech_present {
                        if let Some(db) = rms_dbfs.and_then(crate::gateway::valid_db) {
                            entry.rms_dbfs = Some(if holds_fresh_speech {
                                entry.rms_dbfs.map_or(db, |prev| prev.max(db))
                            } else {
                                db
                            });
                        }
                        if let Some(snr) = snr_db.filter(|v| v.is_finite()) {
                            entry.snr_db = Some(if holds_fresh_speech {
                                entry.snr_db.map_or(snr, |prev| prev.max(snr))
                            } else {
                                snr
                            });
                        }
                        entry.clipped = clipped;
                        entry.at_ms = now;
                    } else if !holds_fresh_speech {
                        if let Some(db) = rms_dbfs.and_then(crate::gateway::valid_db) {
                            entry.rms_dbfs = Some(db);
                        }
                        if let Some(snr) = snr_db.filter(|v| v.is_finite()) {
                            entry.snr_db = Some(snr);
                        }
                        entry.clipped = clipped;
                        entry.at_ms = now;
                    }
                    if let Some(h) = echo_health {
                        entry.echo_health = Some(h);
                    }
                    if let Some(pv) = profile_version {
                        entry.profile_version = Some(pv);
                    }
                }
                // Wave 2 (#73 §3): event-driven passive contention for fresh
                // `NearEndPresence` without requiring a local KWS hit.
                // Ignore trailing `just_ended` quiet blocks (`snr_db < MIN_SNR_DB`)
                // so post-speech silence never clobbers the utterance's speech peak.
                if speech_present && self.device_in_household_acoustic_domain(&device) {
                    if let Some(db) = rms_dbfs.and_then(crate::gateway::valid_db) {
                        if db >= crate::arbitration::MIN_LOUDNESS_EVIDENCE_DBFS && !clipped {
                            let heard_ms = if ts_ms == 0 { now } else { ts_ms } as i64;
                            let claim = self.build_claim(&device, heard_ms, Some(db), "");
                            self.arbiter.record_passive_presence(claim);
                        }
                    }
                }
            }
        }
        self.apply_capture_revocations();
    }

    /// Device online (hello-announced join or v0.2 device_status): set up
    /// interaction state, re-apply the persisted duplex request, and open
    /// the mic session when the device can stream audio.
    pub(super) fn on_device_online(self: &Arc<Self>, device: &str, capabilities: &[String]) {
        crate::lock_or_recover(&self.states, "voice.states")
            .entry(device.to_string())
            .or_default();
        self.project_follow_window_policy();
        // Re-apply persisted settings on (re)connect. On v2 devices, only
        // reconcile blocks that actually have an explicit persisted row in
        // SQLite, and dispatch them sequentially in a single task so they
        // never collide on Gateway's `expected_state_revision`.
        let explicit_duplex = crate::lock_or_recover(&self.settings, "voice.settings")
            .duplex_requested
            .get(device)
            .cloned();
        let has_v2 = capabilities.iter().any(|c| c == "config_apply_v2");
        let has_v2_duplex = has_v2 && capabilities.iter().any(|c| c == "config_block_duplex");
        let has_v2_audio_eq = has_v2 && capabilities.iter().any(|c| c == "config_block_audio_eq");
        let has_v2_acoustics = has_v2 && capabilities.iter().any(|c| c == "config_block_acoustics");
        let has_v2_led = has_v2 && capabilities.iter().any(|c| c == "config_block_led");
        let has_v2_kws = has_v2 && capabilities.iter().any(|c| c == "config_block_kws");

        if !has_v2_duplex {
            let requested = explicit_duplex.as_deref().unwrap_or("half");
            let _ = self.hub.mode_set(device, requested);
        }
        // Reconcile persisted per-device LED night-dimming & orientation config
        // on (re)connect so offline saves and reboots never drift.
        let saved_led = crate::config::ConfigService::new(self.store.clone())
            .device_led_configs()
            .ok()
            .and_then(|mut map| map.remove(device));
        if !has_v2_led {
            if let Some(led) = &saved_led {
                if let Err(e) = self.hub.send_led_config(
                    device,
                    led.night_enabled,
                    led.night_start_minute,
                    led.night_end_minute,
                    led.night_brightness_pct,
                    led.orientation_rotation,
                    led.orientation_reverse,
                ) {
                    self.note(
                        "led_config_push_failed",
                        json!({ "device": device, "error": e }),
                    );
                }
            }
        }
        // M6c-2: the output tone is configuration; a reconnected legacy device
        // (or a v2 device that explicitly advertises legacy `audio_eq`) gets
        // the saved curve. V2 gateways without EQ capability (e.g. Android)
        // are skipped cleanly.
        if !has_v2 || (!has_v2_audio_eq && capabilities.iter().any(|c| c == "audio_eq")) {
            if let Err(e) = self.push_audio_eq(device) {
                self.note(
                    "audio_eq_push_failed",
                    json!({ "device": device, "error": e }),
                );
            }
        }

        let v2_duplex = if has_v2_duplex { explicit_duplex } else { None };
        let v2_led = if has_v2_led { saved_led } else { None };
        let v2_audio_eq = has_v2_audio_eq
            && self
                .store
                .config_get(crate::config::AUDIO_EQ_KEY)
                .ok()
                .flatten()
                .is_some();
        let runtime_handle = tokio::runtime::Handle::try_current().ok();
        let v2_acoustics = if has_v2_acoustics && runtime_handle.is_some() {
            let saved_profile = crate::config::ConfigService::new(self.store.clone())
                .unit_acoustic_profiles()
                .ok()
                .and_then(|mut map| map.remove(device));
            if let Some(profile) = saved_profile {
                let should_reconcile = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.to_string()).or_default();
                    if st.acoustics_reconciled {
                        false
                    } else {
                        st.acoustics_reconciled = true;
                        true
                    }
                };
                should_reconcile.then_some(profile)
            } else {
                None
            }
        } else {
            None
        };

        let kws_needs_reconcile = has_v2_kws
            && (self
                .store
                .config_get(crate::config::REFLEX_KEY)
                .ok()
                .flatten()
                .is_some()
                || {
                    let s = crate::lock_or_recover(&self.settings, "voice.settings");
                    let eff = s.effective_for(device);
                    let kws_class = self.hub.kws_device_class(device);
                    let (kws_threshold, kws_score) = s.effective_kws_for(device, kws_class);
                    let (base_threshold, base_score) = match kws_class {
                        DeviceKwsClass::Android => {
                            (default_android_kws_threshold(), default_android_kws_score())
                        }
                        DeviceKwsClass::Standard | DeviceKwsClass::Lx06 => {
                            (default_kws_threshold(), default_kws_score())
                        }
                    };
                    eff.wake_words != default_wake_words()
                        || eff.retreat_words != default_retreat_words()
                        || (kws_threshold - base_threshold).abs() > f64::EPSILON
                        || (kws_score - base_score).abs() > f64::EPSILON
                });
        let v2_kws = if kws_needs_reconcile && runtime_handle.is_some() {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            if st.kws_reconciled {
                false
            } else {
                st.kws_reconciled = true;
                true
            }
        } else {
            false
        };

        if v2_duplex.is_some()
            || v2_led.is_some()
            || v2_audio_eq
            || v2_acoustics.is_some()
            || v2_kws
        {
            if let Some(handle) = runtime_handle {
                let pipe = self.clone();
                let store = self.store.clone();
                let hub = self.hub.clone();
                let settings = self.settings_snapshot();
                let device_id = device.to_string();
                handle.spawn(async move {
                    if let Some(req) = v2_duplex {
                        let outcome = crate::config_delivery::dispatch_duplex_v2(
                            &store, &hub, &device_id, &req,
                        )
                        .await;
                        if outcome.state != "applied" {
                            tracing::debug!(
                                device = %device_id,
                                state = outcome.state,
                                error = ?outcome.error,
                                "duplex reconnect config_apply_v2 did not apply"
                            );
                        }
                    }
                    if let Some(led) = v2_led {
                        let outcome =
                            crate::config_delivery::dispatch_led_v2(&store, &hub, &device_id, &led)
                                .await;
                        if outcome.state != "applied" {
                            tracing::warn!(
                                device = %device_id,
                                state = outcome.state,
                                error = ?outcome.error,
                                "led reconnect config_apply_v2 did not apply"
                            );
                        }
                    }
                    if v2_audio_eq {
                        let outcome =
                            crate::config_delivery::dispatch_audio_eq_v2(&store, &hub, &device_id)
                                .await;
                        if outcome.state != "applied" {
                            tracing::warn!(
                                device = %device_id,
                                state = outcome.state,
                                error = ?outcome.error,
                                "audio_eq reconnect config_apply_v2 did not apply"
                            );
                        }
                    }
                    if let Some(profile) = v2_acoustics {
                        let outcome = crate::config_delivery::dispatch_unit_acoustics_parts(
                            &store, &hub, &settings, &device_id, &profile, false,
                        )
                        .await;
                        if outcome.state != "applied" {
                            if let Some(st) = crate::lock_or_recover(&pipe.states, "voice.states")
                                .get_mut(&device_id)
                            {
                                st.acoustics_reconciled = false;
                            }
                            tracing::warn!(
                                device = %device_id,
                                state = outcome.state,
                                error = ?outcome.error,
                                "unit acoustics reconnect reconciliation did not apply"
                            );
                        }
                    }
                    if v2_kws {
                        match crate::config_delivery::dispatch_reflex_v2_reconcile(
                            &store, &hub, &settings, &device_id,
                        )
                        .await
                        {
                            Ok((rec, note, _)) => {
                                if let Some(st) =
                                    crate::lock_or_recover(&pipe.states, "voice.states")
                                        .get_mut(&device_id)
                                {
                                    st.reflex_deploy = Some(rec);
                                }
                                pipe.note("kws_deploy_sent", note);
                            }
                            Err(err) => {
                                if let Some(st) =
                                    crate::lock_or_recover(&pipe.states, "voice.states")
                                        .get_mut(&device_id)
                                {
                                    st.kws_reconciled = false;
                                }
                                tracing::warn!(
                                    device = %device_id,
                                    error = %err,
                                    "kws reconnect config_apply_v2 did not apply"
                                );
                            }
                        }
                    }
                });
            }
        }
        if capabilities.iter().any(|c| c == "audio_in") {
            match self.hub.mic_session(device, "start") {
                Ok(()) => self.note(
                    "mic_session_requested",
                    json!({ "device": device, "action": "start" }),
                ),
                Err(e) => self.note(
                    "mic_session_request_failed",
                    json!({ "device": device, "error": e }),
                ),
            }
        }
        // Presentation catches up with the (re)connected device immediately
        // (the gateway repaints its LED on register from its own state; this
        // frame keeps that state truthful after any core-side gap).
        self.push_assistant_state(device, true);
    }

    /// Device offline/left: clear outputs and the CURRENT turn; also reset
    /// the mic processor so a half-heard segment never stitches into the
    /// next connection's stream (review P3-6). Presentation drops to idle —
    /// nothing meaningful can be said about a device that is not there.
    pub(super) fn on_device_offline(&self, device: &str) {
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
            st.outputs.clear();
            if let Some(task) = st.current_task.take() {
                self.tasks.request_cancel(&task);
            }
            st.stop_epoch += 1;
            st.revoke_input("cancelled");
            st.participation.reset_capture();
            st.participation.cancel_requests(id::now_ms());
            st.mic_active = Some(false);
            st.wake_flash_until_ms = 0;
            st.error_flash_until_ms = 0;
            st.acoustics_reconciled = false;
            st.kws_reconciled = false;
        }
        if let Some(arc) = self.mic_arc(device) {
            crate::lock_or_recover(&arc, "voice.mic.dev").reset();
        }
        {
            let mut slot = crate::lock_or_recover(&self.mic_capture, "voice.mic_capture");
            if slot.get(device).is_some_and(|c| !c.done) {
                slot.remove(device);
            }
        }
        self.clear_learning_wait(device);
        self.push_assistant_state(device, false);
    }
}
