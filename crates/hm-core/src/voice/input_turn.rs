use super::*;

impl VoicePipeline {
    /// Feed authenticated PCM to the existing VAD for control recognition.
    /// Retained spans captured during voice/media/tail are control-only:
    /// an exit or explicit wake may act, ordinary playback echo may not.
    /// A certified barge episode still retains its original head/provenance.
    #[cfg(test)]
    pub(super) fn feed_chunk(
        &self,
        device: String,
        pcm: Vec<i16>,
        sample_rate: u32,
        channels: u16,
        ts: u64,
        provenance: crate::gateway::InputProvenance,
    ) {
        self.feed_chunk_with_vad(device, pcm, sample_rate, channels, ts, provenance, None);
    }

    pub(super) fn feed_chunk_with_vad(
        &self,
        device: String,
        pcm: Vec<i16>,
        sample_rate: u32,
        channels: u16,
        ts: u64,
        provenance: crate::gateway::InputProvenance,
        remote_vad: Option<bool>,
    ) {
        if self.source_identity(&device, &provenance, "pcm").is_none() {
            return;
        }
        let mut provenance =
            crate::mic::CaptureProvenance::chunk(provenance, pcm.len() / channels.max(1) as usize);
        let now = id::now_ms();
        // Diagnostic capture FIRST: this is the post-AEC mono the spotter
        // consumes, and a capture must not be silently shortened by the
        // self-echo/barge gates below (the device-side spotter has no such
        // gates). Bounded by `target`; writes one WAV when full.
        {
            let mono: Vec<i16> = if channels == 2 {
                pcm.chunks_exact(2).map(|p| p[0]).collect()
            } else {
                pcm.clone()
            };
            let mut slot = crate::lock_or_recover(&self.mic_capture, "voice.mic_capture");
            if let Some(cap) = slot.as_mut() {
                if !cap.done && cap.device == device {
                    let room = cap.target.saturating_sub(cap.samples.len());
                    cap.samples.extend(mono.iter().take(room));
                    if cap.samples.len() >= cap.target {
                        cap.done = true;
                        let wav = MicCapture::to_wav(&cap.samples, cap.rate);
                        let path = cap.path.clone();
                        let device = cap.device.clone();
                        let seconds = cap.samples.len() as f64 / cap.rate as f64;
                        let written = std::fs::write(&path, &wav);
                        drop(slot);
                        self.note(
                            "mic_capture_done",
                            json!({ "device": device, "path": path.to_string_lossy(),
                                    "seconds": (seconds * 10.0).round() / 10.0,
                                    "bytes": wav.len(), "ok": written.is_ok() }),
                        );
                    }
                }
            }
        }
        #[derive(Clone, Copy)]
        enum BargePhase {
            None,
            Stopped(u64),
            Feeding,
        }
        let (voice_live, raw_media_live, blind, muted, barge_phase) = {
            let hub_live = self.hub_live(&device);
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get_mut(&device)
                .map(|st| {
                    let live = |o: &OutputRecord| {
                        !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                    };
                    st.barge.expire(now);
                    let phase = match st.barge {
                        Barge::Stopped { guard_until_ms, .. } => {
                            BargePhase::Stopped(guard_until_ms)
                        }
                        Barge::Feeding { .. } => BargePhase::Feeding,
                        Barge::Idle => BargePhase::None,
                    };
                    (
                        hub_live.voice
                            || st.outputs.values().any(|o| o.channel == "voice" && live(o)),
                        hub_live.media
                            || st.outputs.values().any(|o| o.channel == "media" && live(o)),
                        now < st.self_play_blind_until_ms,
                        st.muted.unwrap_or(false),
                        phase,
                    )
                })
                .unwrap_or((
                    hub_live.voice,
                    hub_live.media,
                    false,
                    false,
                    BargePhase::None,
                ))
        };
        if muted {
            return; // hardware mute: nothing usable can arrive
        }
        match barge_phase {
            BargePhase::Stopped(utterance_from) => {
                let mut procs = crate::lock_or_recover(&self.mic, "voice.mic");
                if let Some(p) = procs.get_mut(&device) {
                    p.reset();
                }
                drop(procs);
                let mono: Vec<i16> = if channels == 2 {
                    pcm.chunks_exact(2).map(|p| p[0]).collect()
                } else {
                    pcm
                };
                let first = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.clone()).or_default();
                    let first = st.mic_counters.barge_tail_dropped == 0;
                    st.mic_counters.barge_tail_dropped += 1;
                    st.barge.append_tail(&mono, sample_rate, &provenance);
                    first
                };
                if first {
                    self.note(
                        "barge_in_tail_dropped",
                        json!({ "device": device, "until_ms": utterance_from,
                                "why": "not fed to the VAD; kept as the segment head (stop flush + DAC ring + room reverb)" }),
                    );
                }
                return;
            }
            BargePhase::Feeding => {
                let mono: Vec<i16> = if channels == 2 {
                    pcm.chunks_exact(2).map(|p| p[0]).collect()
                } else {
                    pcm
                };
                let head = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.clone()).or_default();
                    st.barge.take_head()
                };
                if let Some((h, _, _)) = head.as_ref().filter(|(h, _, _)| !h.is_empty()) {
                    self.note(
                        "barge_head_released",
                        json!({ "device": device, "samples": h.len(), "rate": sample_rate,
                                "why": "the guard is over: the user's first syllable goes into the VAD" }),
                    );
                }
                let (vad_start_db, uses_silero) = {
                    let procs = crate::lock_or_recover(&self.mic, "voice.mic");
                    procs
                        .get(&device)
                        .map(|p| (p.effective_thresholds().0, p.uses_silero()))
                        .unwrap_or_else(|| {
                            (self.live_vad_config().start_db, self.silero.model.is_some())
                        })
                };
                let live_loud = if uses_silero {
                    crate::mic::block_db(&mono) >= vad_start_db
                } else {
                    remote_vad.unwrap_or_else(|| crate::mic::block_db(&mono) >= vad_start_db)
                };
                let outcome = {
                    let mut procs = crate::lock_or_recover(&self.mic, "voice.mic");
                    let p = procs
                        .entry(device.clone())
                        .or_insert_with(|| self.new_mic_processor());
                    let head_vad = if p.uses_silero() {
                        None
                    } else {
                        remote_vad.map(|_| true)
                    };
                    let mut outcome = None;
                    if let Some((h, head_rate, sources)) =
                        head.filter(|(h, r, _)| !h.is_empty() && *r == sample_rate)
                    {
                        let block = (head_rate as usize * 80 / 1000).max(1);
                        for (index, slice) in h.chunks(block).enumerate() {
                            let o = p.feed_with_provenance_and_vad(
                                slice.to_vec(),
                                head_rate,
                                sources.slice(index * block, slice.len()),
                                head_vad,
                            );
                            if !matches!(o, FeedOutcome::None) {
                                outcome = Some(o);
                            }
                        }
                    }
                    let live =
                        p.feed_with_provenance_and_vad(mono, sample_rate, provenance, remote_vad);
                    if matches!(live, FeedOutcome::None) {
                        if let Some(prev) = outcome.take() {
                            outcome = Some(prev);
                        }
                    } else {
                        outcome = Some(live);
                    }
                    outcome.unwrap_or(FeedOutcome::None)
                };
                {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.clone()).or_default();
                    st.last_chunk_ms = Some(id::now_ms());
                    st.chunk_window_count += 1;
                    if live_loud {
                        st.barge.note_loud(now);
                    }
                }
                self.handle_mic_outcome(&device, outcome, sample_rate, ts);
                return;
            }
            BargePhase::None => {}
        }
        let media_playing = raw_media_live && !self.media_pause_in_flight(&device, now);
        if voice_live || blind || media_playing {
            let wake_ack_overlap =
                self.capture_is_wake_ack_only(&device, voice_live, media_playing, now);
            for span in &mut provenance.spans {
                span.control_only = true;
                span.wake_ack_overlap = wake_ack_overlap;
            }
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(&device)
            {
                if voice_live {
                    st.mic_counters.suppressed_during_voice += 1;
                } else if blind {
                    st.mic_counters.tail_suppressed += 1;
                }
            }
        }
        let mono: Vec<i16> = if channels == 2 {
            pcm.chunks_exact(2).map(|p| p[0]).collect()
        } else {
            pcm
        };
        let outcome = {
            let mut procs = crate::lock_or_recover(&self.mic, "voice.mic");
            let p = procs
                .entry(device.clone())
                .or_insert_with(|| self.new_mic_processor());
            p.feed_with_provenance_and_vad(mono, sample_rate, provenance, remote_vad)
        };
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.clone()).or_default();
            let now = id::now_ms();
            st.last_chunk_ms = Some(now);
            let span = now.saturating_sub(st.chunk_window_start_ms);
            if st.chunk_window_start_ms == 0 {
                st.chunk_window_start_ms = now;
            } else if span >= 1000 {
                st.chunks_per_sec =
                    Some(st.chunk_window_count as f64 * 1000.0 / span.max(1) as f64);
                st.chunk_window_start_ms = now;
                st.chunk_window_count = 0;
            }
            st.chunk_window_count += 1;
        }
        self.handle_mic_outcome(&device, outcome, sample_rate, ts);
    }

    /// Shared tail of every mic feed path: counters, notes, ASR dispatch.
    pub(super) fn handle_mic_outcome(
        &self,
        device: &str,
        outcome: FeedOutcome,
        sample_rate: u32,
        ts: u64,
    ) {
        match outcome {
            FeedOutcome::SpeechStarted => {
                self.start_input(device);
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.mic_counters.speech_started += 1;
                }
                let provenance = crate::lock_or_recover(&self.mic, "voice.mic")
                    .get(device)
                    .map(|p| p.open_provenance());
                self.note(
                    "mic_speech_started",
                    json!({ "device": device, "ts": ts, "provenance": provenance }),
                );
                self.push_assistant_state(device, false);
            }
            FeedOutcome::Segment(Some(seg)) if !seg.pcm.is_empty() => {
                let close_reason = match seg.close_reason {
                    crate::mic::CloseReason::Quiet => "quiet",
                    crate::mic::CloseReason::Cap => "cap",
                };
                let (tail_only, barge_served) = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.to_string()).or_default();
                    st.barge.finish_segment()
                };
                if tail_only {
                    if let Some(st) =
                        crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                    {
                        st.mic_counters.tail_suppressed += 1;
                    }
                    self.note(
                        "barge_in_tail_only_segment_dropped",
                        json!({ "device": device, "blocks": seg.blocks,
                                "why": "no loud audio outside the guard: this is the stopped answer" }),
                    );
                    return;
                }
                let duration_ms = seg.pcm.len() as u64 * 1000 / sample_rate as u64;
                let ticket = seg.provenance;
                let lead_pcm = seg.lead_pcm;
                let (pcm, head_ms) = (seg.pcm, 0u64);
                {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let st = states.entry(device.to_string()).or_default();
                    st.mic_counters.segments += 1;
                    match seg.close_reason {
                        crate::mic::CloseReason::Quiet => st.mic_counters.close_quiet += 1,
                        crate::mic::CloseReason::Cap => st.mic_counters.close_cap += 1,
                    }
                }
                let (vad_start, vad_end, floor) = {
                    let procs = crate::lock_or_recover(&self.mic, "voice.mic");
                    match procs.get(device) {
                        Some(p) => {
                            let (s, e) = p.effective_thresholds();
                            (s, e, p.noise_floor_db())
                        }
                        None => (0.0, 0.0, None),
                    }
                };
                if head_ms > 0 {
                    self.note(
                        "barge_in_head_kept",
                        json!({ "device": device, "head_ms": head_ms,
                                "why": "first syllable the guard refused to segment on" }),
                    );
                }
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.mic_last_close_ms = Some(id::now_ms());
                }
                self.arbiter.refresh_passive_presence(device);
                self.note(
                    "mic_segment_closed",
                    json!({ "device": device, "blocks": seg.blocks, "duration_ms": duration_ms,
                            "head_ms": head_ms, "barge_in": barge_served,
                            "close_reason": close_reason,
                            "floor_db": floor, "start_db": vad_start, "end_db": vad_end,
                            "ticket": ticket }),
                );
                let reservation = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let Some(st) = states.get_mut(device) else {
                        return;
                    };
                    let Some(input) = st.input.as_ref() else {
                        return;
                    };
                    if input.recognizing {
                        return;
                    }
                    if !input.valid(st) || !input.admitted() || !input.confirmed {
                        st.mic_counters.asr_skipped += 1;
                        st.revoke_input("rejected");
                        self.note(
                            "mic_asr_skipped",
                            json!({"device":device,"why":"no valid capture-time input lease"}),
                        );
                        return;
                    }
                    let Some(input) = st.input.as_mut() else {
                        return;
                    };
                    input.recognizing = true;
                    let asr_deadline = (tokio::time::Instant::now()
                        + input::ASR_RECOGNITION_TIMEOUT)
                        .min(input.start + input::INPUT_LIFETIME);
                    let reservation = (input.id, input.stop_epoch, asr_deadline);
                    st.mic_counters.asr_inflight += 1;
                    reservation
                };
                // #216: prepend the 160 ms (2 blocks) pre-onset lead-in PCM (or
                // 160 ms leading silence pad if no pre-onset blocks were buffered)
                // when encoding the WAV for ASR so initial plosives/affricates
                // (e.g. "几" in "几点了") and log-Mel frontend normalization have
                // true pre-onset context without mutating `ticket` or `duration_ms`.
                let min_lead_samples = (sample_rate as usize * 160) / 1000;
                let mut wav_bytes =
                    Vec::with_capacity((pcm.len() + lead_pcm.len().max(min_lead_samples)) * 2);
                if lead_pcm.len() < min_lead_samples {
                    wav_bytes.resize((min_lead_samples - lead_pcm.len()) * 2, 0u8);
                }
                for &s in &lead_pcm {
                    wav_bytes.extend_from_slice(&s.to_le_bytes());
                }
                for &s in &pcm {
                    wav_bytes.extend_from_slice(&s.to_le_bytes());
                }
                let wav = crate::speech::pcm_to_wav(&wav_bytes, sample_rate, 1);
                let tx = self.mic_tx.clone();
                let factory = self.asr_factory.clone();
                let dev = device.to_string();
                let (input_id, window_epoch, deadline) = reservation;
                let control_only = ticket.spans.iter().any(|s| s.control_only);
                let capture_seq = seg.total_blocks_seen;
                // Register the worker under the same owner lock as cancellation.
                // A revoked reservation cannot start an HTTP request afterward.
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                let Some(st) = states.get_mut(device) else {
                    return;
                };
                if !st.input.as_ref().is_some_and(|i| {
                    i.id == input_id && i.recognizing && !i.consumed && i.valid(st)
                }) {
                    return;
                }
                let worker = tokio::spawn(async move {
                    let result = tokio::time::timeout_at(deadline, async {
                        match (factory)() {
                            Ok(asr) => asr.transcribe(&wav).await.map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        }
                    })
                    .await
                    .unwrap_or_else(|_| Err("input recognition deadline expired".into()));
                    let _ = tx
                        .send(MicTranscript {
                            leased: true,
                            device: dev,
                            result,
                            duration_ms,
                            input_id,
                            capture_seq,
                            window_epoch,
                            ticket,
                            control_only,
                            enqueued_at: std::time::Instant::now(),
                        })
                        .await;
                });
                if let Some(input) = st.input.as_mut() {
                    input.abort = Some(worker.abort_handle());
                }
            }
            FeedOutcome::Segment(_) => {
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(device)
                {
                    st.mic_counters.segments_discarded += 1;
                    if st.input.as_ref().is_some_and(|i| !i.recognizing) {
                        st.revoke_input("discarded");
                    }
                }
            }
            FeedOutcome::None => {
                self.observe_input_onset(device);
            }
        }
    }

    /// The VAD config new mic processors start with: measured settings win
    /// over env/defaults (G4 calibration flow).
    pub(super) fn live_vad_config(&self) -> VadConfig {
        let mut cfg = VadConfig::from_env();
        if let Some(v) = crate::lock_or_recover(&self.settings, "voice.settings").vad {
            cfg.start_db = v.start_db;
            cfg.end_db = v.end_db;
        }
        cfg
    }

    pub(super) fn clear_passive_presence_if_idle(&self, device: &str) {
        let active = crate::lock_or_recover(&self.mic, "voice.mic")
            .get(device)
            .is_some_and(|p| p.in_speech() || p.onset_pending());
        if !active {
            self.arbiter.clear_passive_presence(device);
        }
    }

    /// M5: an ASR failure or an empty text. Counted and logged, but it must
    /// not consume the identity watermark — nothing was acted on.
    pub(super) async fn handle_mic_transcript_unusable(self: &Arc<Self>, t: MicTranscript) {
        self.clear_passive_presence_if_idle(&t.device);
        match t.result {
            Ok(_) => {
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
                {
                    st.mic_counters.segment_empty += 1;
                }
                self.note(
                    "mic_segment_empty",
                    json!({ "device": t.device, "input_id": t.input_id }),
                );
            }
            Err(e) => {
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
                {
                    st.mic_counters.asr_failed += 1;
                }
                self.note(
                    "mic_asr_failed",
                    json!({ "device": t.device, "error": e, "input_id": t.input_id }),
                );
            }
        }
    }

    pub(super) async fn handle_mic_transcript(self: &Arc<Self>, t: MicTranscript) {
        let device = t.device.clone();
        let input_id = t.input_id;
        let leased = t.leased;
        self.handle_mic_transcript_at(t).await;
        if leased {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            if let Some(st) = states
                .get_mut(&device)
                .filter(|s| s.input.as_ref().is_some_and(|i| i.id == input_id))
            {
                let valid = st.input.as_ref().is_some_and(|i| i.valid(st));
                st.revoke_input(if valid { "consumed" } else { "expired" });
            }
        }
        #[cfg(test)]
        self.input_handoff_finished.notify_one();
    }

    async fn handle_mic_transcript_at(self: &Arc<Self>, t: MicTranscript) {
        let outcome = match &t.result {
            Ok(text) if !crate::mic::has_speech_chars(text) => "empty",
            Ok(_) => "ok",
            Err(_) => "failed",
        };
        self.note(
            "mic_transcript_source",
            json!({ "device": t.device, "input_id": t.input_id,
                "capture_seq": t.capture_seq, "outcome": outcome, "ticket": t.ticket }),
        );
        // Frozen context is checked before consuming the exact slot so a new
        // context cannot inherit an old recognition result.
        if !self.accept_context_transcript(&t.device, t.input_id) {
            return;
        }
        let (leased_session, mut frozen_context, input_generation) = if t.leased {
            let Some(lease) = self.consume_input(&t.device, t.input_id) else {
                self.note(
                    "mic_input_result_rejected",
                    json!({"device":t.device,"input_id":t.input_id}),
                );
                return;
            };
            lease
        } else {
            if let Some(st) =
                crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
            {
                st.mic_counters.asr_inflight = st.mic_counters.asr_inflight.saturating_sub(1);
            }
            (false, None, 0)
        };
        let usable = matches!(&t.result, Ok(text) if crate::mic::has_speech_chars(text));
        if !usable {
            return self.handle_mic_transcript_unusable(t).await;
        }
        let mut input_identity = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let Some(st) = states.get_mut(&t.device) else {
                return;
            };
            if (t.leased
                && (st.participation.generation() != input_generation
                    || !self.consumed_input_valid_locked(&t.device, st, t.input_id)))
                || t.window_epoch != st.stop_epoch
            {
                let captured = t.window_epoch;
                let current = st.stop_epoch;
                drop(states);
                self.note(
                    "stale_session_event_rejected",
                    json!({"device":t.device,"via":"transcript","input_id":t.input_id}),
                );
                self.note(
                    "transcript_stale_after_epoch",
                    json!({
                        "device": t.device, "input_id": t.input_id,
                        "capture_seq": t.capture_seq, "captured_epoch": captured,
                        "current_epoch": current,
                        "why": "a stop/retreat landed after this audio was captured",
                    }),
                );
                return;
            }
            if t.input_id <= st.last_transcript_input {
                let last = st.last_transcript_input;
                drop(states);
                self.note(
                    "transcript_out_of_order",
                    json!({
                        "device": t.device, "input_id": t.input_id, "last_acted": last,
                        "capture_seq": t.capture_seq,
                        "why": "an older capture's text arrived after a newer one",
                    }),
                );
                return;
            }
            for span in &t.ticket.spans {
                if !st.participation.accepts_origin(&span.provenance.origin)
                    || span.provenance.transport.as_ref().is_some_and(|t| {
                        st.input_connection_generation
                            .is_some_and(|g| g != t.generation)
                    })
                {
                    drop(states);
                    self.note(
                        "stale_session_event_rejected",
                        json!({"device":t.device,"via":"transcript","ticket":t.ticket}),
                    );
                    return;
                }
            }
            st.last_transcript_input = t.input_id;
            st.participation.identity()
        };
        let text = match t.result {
            Ok(text) => text,
            Err(_) => unreachable!("unusable results are handled before the identity gate"),
        };
        if self.exit_text(&text) {
            self.clear_passive_presence_if_idle(&t.device);
            if self.reflex_deduped(&t.device, interaction::KeywordIntent::Retreat, "transcript") {
                return;
            }
            self.retreat_session_at(&t.device, &text, "transcript", input_identity);
            return;
        }
        let media_playing = self.media_actively_playing(&t.device);
        if !t.control_only
            && media_playing
            && pause_match(&text)
            && self.handle_stop_intent_at(
                &t.device,
                &text,
                "transcript",
                Some(&mut input_identity),
                None,
            )
        {
            return;
        }
        let wakes = self.effective_wake_words();
        let has_wake = wakes
            .iter()
            .any(|w| mic::wake_span_near(&text, w).is_some());
        if !has_wake && self.music_blocks_transcript(&t.device, t.input_id, t.duration_ms) {
            self.reject_passive_music_input(&t.device, t.input_id, &text);
            return;
        }
        let ack_overlap_only = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get(&t.device)
                .map(|st| {
                    !media_playing
                        && !st.outputs.values().any(|r| {
                            r.kind != "ack" && r.channel == "voice" && stage_is_live(&r.stage)
                        })
                })
                .unwrap_or(false)
        };
        if t.control_only && !has_wake && !ack_overlap_only {
            self.note(
                "mic_ignored_unwoken",
                json!({"device":t.device,"text":text}),
            );
            self.note(
                "mic_control_only_rejected",
                json!({"device":t.device,"text":text}),
            );
            self.note(
                "mic_followup_gated_by_media",
                json!({"device":t.device,"text":text,"control_only":true}),
            );
            return;
        }
        if t.control_only
            && crate::lock_or_recover(&self.states, "voice.states")
                .get(&t.device)
                .is_some_and(|st| {
                    st.outputs.values().any(|r| {
                        r.kind == "answer"
                            && stage_is_live(&r.stage)
                            && wakes
                                .iter()
                                .any(|w| mic::wake_span_near(&r.answer_text, w).is_some())
                    })
                })
        {
            self.note(
                "mic_suppressed_self_echo",
                json!({"device":t.device,"control_wake":true}),
            );
            return;
        }
        let text = self.strip_cue_echo(&t.device, &text);
        if !crate::mic::has_speech_chars(&text) {
            self.note(
                "mic_segment_was_own_cue",
                json!({ "device": t.device, "reason": "cue-only segment after the device spoke" }),
            );
            return;
        }
        if let Some((phrase, from, at_ms)) = self.foreign_cue_echo(&t.device, &text) {
            self.note(
                "mic_foreign_cue_echo",
                json!({ "device": t.device, "text": text, "from": from,
                        "phrase": phrase, "age_ms": id::now_ms().saturating_sub(at_ms) }),
            );
            return;
        }
        let voice_live = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get(&t.device)
                .map(|st| {
                    let live = |o: &OutputRecord| {
                        !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                    };
                    st.outputs.values().any(|o| o.channel == "voice" && live(o))
                })
                .unwrap_or(false)
        };
        if voice_live && !(t.control_only && has_wake) {
            if let Some(st) =
                crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
            {
                st.mic_counters.self_echo_suppressed += 1;
            }
            self.note("mic_suppressed_self_echo", json!({ "device": t.device }));
            return;
        }
        let keyword_window_pending = has_wake
            && !media_playing
            && !voice_live
            && crate::lock_or_recover(&self.states, "voice.states")
                .get(&t.device)
                .is_some_and(|st| st.window_opener == Some("keyword"));
        let allowed_by_capture = if t.control_only {
            (ack_overlap_only && t.ticket.allows_wake_ack_followup()) || keyword_window_pending
        } else {
            true
        };
        let now_ms = id::now_ms();
        let (in_conversation, awaiting_close_confirm) = if !media_playing
            && allowed_by_capture
            && crate::lock_or_recover(&self.states, "voice.states")
                .get(&t.device)
                .is_some_and(|st| leased_session || st.session_open(now_ms))
        {
            let close_confirm = crate::lock_or_recover(&self.states, "voice.states")
                .get(&t.device)
                .is_some_and(|st| {
                    st.participation
                        .close_confirmation
                        .as_ref()
                        .is_some_and(|c| c.generation == st.participation.generation())
                });
            (true, close_confirm)
        } else {
            (false, false)
        };

        let awaiting_confirmation = in_conversation
            && (awaiting_close_confirm
                || {
                    let current_sid = crate::session::SessionStore::new(self.store.clone())
                        .current_device_session_id(&t.device)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| format!("dev:{}", t.device));
                    self.store
                        .with(|c| {
                            Ok(c.query_row(
                                "SELECT EXISTS(SELECT 1 FROM confirmations WHERE session_id=?1 AND state='pending' AND expires_ms > ?2)",
                                rusqlite::params![current_sid, now_ms as i64],
                                |r| r.get::<_, bool>(0),
                            )?)
                        })
                        .unwrap_or(false)
                });

        if crate::mic::is_filler_utterance(&text) && !awaiting_confirmation {
            self.clear_passive_presence_if_idle(&t.device);
            if let Some(st) =
                crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
            {
                st.mic_counters.ignored_unwoken += 1;
            }
            self.note(
                "mic_filler_ignored",
                json!({ "device": t.device, "text": text }),
            );
            return;
        }

        if in_conversation {
            let remaining = {
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                states.get_mut(&t.device).and_then(|st| {
                    if st.participation.identity() != input_identity
                        || (!leased_session && !st.session_open(id::now_ms()))
                    {
                        return None;
                    }
                    let left = if !st.participation.request_started() {
                        Some(st.participation.followup_credits)
                    } else {
                        st.participation.available_followup_credit()
                    };
                    if left.is_none() {
                        st.participation.last_denied = Some(json!({
                            "reason":"followup_credits_exhausted",
                            "at_ms":id::now_ms(),
                            "hint":"请重新唤醒后再提问"
                        }));
                        st.mic_counters.ignored_unwoken += 1;
                    }
                    left.map(|remaining| (remaining, st.participation.identity()))
                })
            };
            let Some((remaining, mut identity)) = remaining else {
                self.clear_passive_presence_if_idle(&t.device);
                let shown: String = text.chars().take(60).collect();
                self.note(
                    "participation_decision",
                    json!({ "device": t.device, "decision": "deny",
                            "reason": "followup_credits_exhausted", "remaining": 0,
                            "text": shown }),
                );
                self.note(
                    "mic_ignored_unwoken",
                    json!({ "device": t.device, "text": shown }),
                );
                return;
            };
            let mut debit = true;
            self.note(
                "participation_decision",
                json!({ "device": t.device, "decision": "admit", "reason": "follow_window",
                        "remaining": remaining, "opener": "transcript" }),
            );
            self.note(
                "mic_utterance",
                json!({ "device": t.device, "source": "asr", "text": text, "duration_ms": t.duration_ms }),
            );
            if let Some(st) =
                crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
            {
                st.mic_counters.followup_turns += 1;
            }
            let wakes = {
                let eff = self.effective_wake_words();
                if eff.is_empty() {
                    default_wake_words()
                } else {
                    eff
                }
            };
            let is_misrecognized_bare_wake = crate::lock_or_recover(&self.states, "voice.states")
                .get(&t.device)
                .is_some_and(|st| {
                    st.window_opener == Some("keyword")
                        && st.last_wake_transferred_input_id == Some(t.input_id)
                })
                && t.duration_ms <= 1600
                && text.chars().filter(|ch| ch.is_alphanumeric()).count() <= 5;
            let in_win = match Self::in_window_command(&text, &wakes) {
                InWindow::Command(c) if c == text && is_misrecognized_bare_wake => InWindow::Bare,
                other => other,
            };
            let command = match in_win {
                InWindow::Bare => {
                    if self.take_keyword_window(&t.device) {
                        let pipe = self.clone();
                        let d = t.device.clone();
                        self.note(
                            "mic_wake_echo_in_window",
                            json!({ "device": t.device, "text": text,
                                    "note": "keyword-opened window, wake-only transcript: the ack belongs to this confirmation" }),
                        );
                        let Some(permit) = self.permit_for_identity(&t.device, identity) else {
                            return;
                        };
                        tokio::spawn(async move {
                            let ack = pipe.speak_wake_ack(&d, permit).await;
                            pipe.note(
                                "wake_ack_after_keyword_window",
                                json!({ "device": d, "ack": ack.label() }),
                            );
                        });
                        return;
                    }
                    self.note(
                        "mic_wake_echo_in_window",
                        json!({ "device": t.device, "text": text,
                                "note": "the window is already open; the wake phrase is not a request" }),
                    );
                    return;
                }
                InWindow::Command(c) => {
                    let wake_confirmed = c != text;
                    if wake_confirmed {
                        self.note(
                            "mic_wake_stripped_in_window",
                            json!({ "device": t.device, "text": text, "command": c }),
                        );
                    }
                    (c, wake_confirmed)
                }
            };
            let (command, wake_confirmed) = command;
            #[cfg(test)]
            if wake_confirmed {
                self.pause_input_handoff("in_window").await;
            }
            let (_, context_ms) = self.context_policy();
            {
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                let Some(st) = states.get_mut(&t.device) else {
                    return;
                };
                if st.participation.identity() != identity
                    || st.participation.session.state != SessionState::Listening
                {
                    return;
                }
                if t.leased && !self.consumed_input_valid_locked(&t.device, st, t.input_id) {
                    return;
                }
                if wake_confirmed {
                    let policy = st.participation.follow_window;
                    if t.leased {
                        let Some((next, context)) = self.activate_consumed_input_locked(
                            &t.device, st, t.input_id, identity, policy, context_ms,
                        ) else {
                            return;
                        };
                        identity = next;
                        frozen_context = Some(context);
                    } else {
                        st.invalidate_live_turn();
                        let Some(next) = st.participation.activate(
                            identity,
                            policy,
                            CreditGrant::Full,
                            id::now_ms(),
                        ) else {
                            return;
                        };
                        if self
                            .admit_context_locked(&t.device, st, id::now_ms(), context_ms, true)
                            .is_none()
                        {
                            return;
                        }
                        identity = next;
                    }
                    debit = false;
                    let matched = wakes
                        .iter()
                        .find(|w| mic::wake_span_near(&text, w).is_some())
                        .cloned();
                    let gen = st.participation.generation();
                    if let Some((ch, word, _)) =
                        st.last_wake.take().filter(|(ch, _, _)| ch == "kws")
                    {
                        st.last_wake = Some((ch, word, gen));
                    } else if let Some(w) = matched {
                        st.last_wake = Some(("asr".into(), w, gen));
                    }
                }
            }
            if let Some((word, intent)) = self.reflex_text_command(&command) {
                if intent != interaction::KeywordIntent::Wake {
                    if debit && !self.debit_control(&t.device, &mut identity) {
                        return;
                    }
                    self.handle_reflex_text(&t.device, &word, intent, "followup", identity);
                    return;
                }
            }
            let next = if t.leased {
                self.barge_input(&t.device, identity, t.input_id)
            } else {
                self.barge_in_if_half_duplex(&t.device, "utterance", identity)
            };
            let Some(next) = next else {
                return;
            };
            identity = next;
            self.push_assistant_state(&t.device, false);
            let (retreat_on, retreat_words) = self.effective_retreat_policy();
            if retreat_on && retreat_match(&command, &retreat_words) {
                if debit && !self.debit_control(&t.device, &mut identity) {
                    return;
                }
                if self.reflex_deduped(&t.device, interaction::KeywordIntent::Retreat, "followup") {
                    return;
                }
                self.retreat_session_at(&t.device, &command, "followup", identity);
                return;
            }
            if self.handle_stop_intent_at(
                &t.device,
                &command,
                "followup",
                Some(&mut identity),
                Some(&mut debit),
            ) {
                return;
            }
            let pipe = self.clone();
            let (d, command) = (t.device.clone(), command);
            let turn_ticket = self.reserve_transcript_turn(
                &t.device,
                identity,
                debit,
                t.leased.then_some((t.input_id, frozen_context)),
            );
            let Some(turn_ticket) = turn_ticket else {
                return;
            };
            tokio::spawn(async move {
                pipe.run_device_turn_at(d, command, "device:asr".into(), turn_ticket)
                    .await;
            });
            return;
        }
        // Idle: wake-word check on pinyin syllables (tone-insensitive).
        let wakes = {
            let eff = self.effective_wake_words();
            if eff.is_empty() {
                default_wake_words()
            } else {
                eff
            }
        };
        for w in &wakes {
            if let Some(span) = mic::wake_span_near(&text, w) {
                #[cfg(test)]
                self.pause_input_handoff("fallback").await;
                if !self.admit_asr_wake(&t.device, w, &text).await {
                    return;
                }
                let prologue = text[..span.0].trim().to_string();
                let rest = mic::wake_tail(&text, span);
                if !prologue.is_empty() {
                    self.note(
                        "mic_wake_prologue_dropped",
                        json!({ "device": t.device, "wake": w, "text": text, "prologue": prologue }),
                    );
                }
                self.note(
                    "mic_wake",
                    json!({ "device": t.device, "wake": w, "text": text, "rest": rest }),
                );
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
                {
                    st.mic_counters.wakes += 1;
                    st.wake_flash_until_ms = id::now_ms() + WAKE_FLASH_MS;
                }
                let mut identity = if t.leased {
                    let Some((identity, context)) =
                        self.activate_consumed_input(&t.device, t.input_id, input_identity)
                    else {
                        return;
                    };
                    frozen_context = Some(context);
                    identity
                } else {
                    let Some(identity) = self.activate_session_at(
                        &t.device,
                        CreditGrant::Full,
                        input_identity,
                        "asr_wake",
                    ) else {
                        return;
                    };
                    identity
                };
                if let Some(st) =
                    crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device)
                {
                    let gen = st.participation.generation();
                    st.last_wake = Some(("asr".into(), w.clone(), gen));
                }
                if let Some((word, intent)) = self.reflex_text_command(&rest) {
                    if intent != interaction::KeywordIntent::Wake {
                        self.handle_reflex_text(&t.device, &word, intent, "wake_rest", identity);
                        self.push_assistant_state(&t.device, false);
                        return;
                    }
                }
                let next = if t.leased {
                    self.barge_input(&t.device, identity, t.input_id)
                } else {
                    self.barge_in_if_half_duplex(&t.device, "wake", identity)
                };
                let Some(next) = next else {
                    return;
                };
                identity = next;
                self.push_assistant_state(&t.device, false);
                let (retreat_on, retreat_words) = self.effective_retreat_policy();
                if retreat_on && retreat_match(&rest, &retreat_words) {
                    if self.reflex_deduped(
                        &t.device,
                        interaction::KeywordIntent::Retreat,
                        "wake_rest",
                    ) {
                        return;
                    }
                    self.retreat_session_at(&t.device, &rest, "wake_rest", identity);
                } else if self.handle_stop_intent_at(
                    &t.device,
                    &rest,
                    "wake_rest",
                    Some(&mut identity),
                    None,
                ) {
                    // Consumed.
                } else if is_greeting_only(&rest) {
                    let ack_enabled = crate::lock_or_recover(&self.settings, "voice.settings")
                        .wake_ack
                        .enabled;
                    if ack_enabled {
                        match self.pick_ack_phrase() {
                            Some(phrase) => {
                                let _ = self.play_local_ack(&t.device, &phrase, "wake_ack");
                            }
                            None => {
                                let synth = self.clone();
                                tokio::spawn(
                                    async move { synth.synthesize_wake_acks(false).await },
                                );
                                let pipe = self.clone();
                                let (d, text) = (t.device.clone(), text.clone());
                                let Some(ticket) = self.reserve_transcript_turn(
                                    &t.device,
                                    identity,
                                    false,
                                    t.leased.then_some((t.input_id, frozen_context)),
                                ) else {
                                    return;
                                };
                                tokio::spawn(async move {
                                    pipe.run_device_turn_at(d, text, "device:asr".into(), ticket)
                                        .await;
                                });
                            }
                        }
                    }
                } else {
                    let pipe = self.clone();
                    let (d, text) = (t.device.clone(), rest);
                    let Some(turn_ticket) = self.reserve_transcript_turn(
                        &t.device,
                        identity,
                        false,
                        t.leased.then_some((t.input_id, frozen_context)),
                    ) else {
                        return;
                    };
                    tokio::spawn(async move {
                        pipe.run_device_turn_at(d, text, "device:asr".into(), turn_ticket)
                            .await;
                    });
                }
                return;
            }
        }
        let shown: String = text.chars().take(60).collect();
        if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(&t.device) {
            if st.participation.identity() != input_identity {
                return;
            }
            st.mic_counters.ignored_unwoken += 1;
            st.participation.last_denied = Some(json!({
                "reason":"session_closed",
                "at_ms":id::now_ms(),
                "hint":"请重新唤醒后再提问"
            }));
        }
        if media_playing {
            self.note(
                "mic_followup_gated_by_media",
                json!({ "device": t.device, "text": shown,
                        "why": "half duplex: media playback routes commands through the wake word" }),
            );
        }
        self.clear_passive_presence_if_idle(&t.device);
        self.note(
            "mic_ignored_unwoken",
            json!({ "device": t.device, "text": shown }),
        );
    }

    /// M7-1: at session end, ask the model for memory candidates.
    pub fn start_memory_extraction(self: &Arc<Self>, device: &str, why: &'static str) {
        self.start_memory_extraction_from(device, why, None, None);
    }

    pub(super) fn memory_transcript(
        sessions: &crate::session::SessionStore,
        session_id: &str,
    ) -> Vec<(String, String)> {
        sessions
            .history(session_id, 12)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                let role = m.role;
                if role != "user" && role != "assistant" {
                    return None;
                }
                m.content
                    .filter(|c| !c.trim().is_empty())
                    .map(|c| (role, c))
            })
            .collect()
    }

    pub(super) fn start_memory_extraction_from(
        self: &Arc<Self>,
        device: &str,
        why: &'static str,
        transcript: Option<Vec<(String, String)>>,
        captured_authority: Option<crate::execution::Authority>,
    ) {
        if !self
            .memory_extract_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.note(
                "memory_extract_skipped",
                json!({ "device": device, "why": why, "reason": "extractor disabled" }),
            );
            return;
        }
        let configured = crate::config::ConfigService::new(self.store.clone())
            .load()
            .map(|c| c.llm.is_configured())
            .unwrap_or(false);
        if !configured {
            self.note(
                "memory_extract_skipped",
                json!({ "device": device, "why": why, "reason": "llm service is not configured" }),
            );
            return;
        }
        let now = id::now_ms();
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.entry(device.to_string()).or_default();
            if now.saturating_sub(st.last_memory_extract_ms) < 90_000 {
                return;
            }
            st.last_memory_extract_ms = now;
        }
        // Capture authority before reading transcript; never relabel a pre-reset snapshot.
        let authority = match captured_authority.map(Ok).unwrap_or_else(|| {
            crate::execution::Authority::internal(
                self.store.clone(),
                crate::working_memory::ScopeContext {
                    device_id: Some(device.into()),
                    person_id: None,
                    room: None,
                },
            )
        }) {
            Ok(authority) => authority,
            Err(_) => return,
        };
        if authority.check().is_err() {
            return;
        }
        let session_id =
            match crate::session::SessionStore::new(self.store.clone()).device_session_id(device) {
                Ok(session) => session,
                Err(error) => {
                    self.note(
                        "memory_extract_skipped",
                        json!({"device":device,"reason":error}),
                    );
                    return;
                }
            };
        let transcript = transcript.unwrap_or_else(|| {
            Self::memory_transcript(
                &crate::session::SessionStore::new(self.store.clone()),
                &session_id,
            )
        });
        let user_turns = transcript.iter().filter(|(r, _)| r == "user").count();
        if user_turns == 0 {
            self.note(
                "memory_extract_skipped",
                json!({ "device": device, "why": why, "reason": "no user turns in this session" }),
            );
            return;
        }
        let pipe = self.clone();
        let device = device.to_string();
        tokio::spawn(async move {
            pipe.run_memory_extraction(device, session_id, transcript, why, authority)
                .await;
        });
    }

    pub(super) async fn run_memory_extraction(
        self: Arc<Self>,
        device: String,
        session_id: String,
        transcript: Vec<(String, String)>,
        why: &'static str,
        authority: crate::execution::Authority,
    ) {
        let env = match (self.env_factory)() {
            Ok(env) => env,
            Err(e) => {
                self.note(
                    "memory_extract_skipped",
                    json!({ "device": device, "why": why, "reason": e }),
                );
                return;
            }
        };
        let mut lines = String::new();
        for (role, text) in &transcript {
            let who = if role == "user" { "用户" } else { "助手" };
            lines.push_str(&format!(
                "{who}：{}\n",
                text.chars().take(300).collect::<String>()
            ));
        }
        let existing: Vec<String> = env
            .memory
            .list_authorized(&authority)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.text)
            .collect();
        let extract_system = "你是家庭助手的记忆提取器。只输出一个 JSON 对象，不要解释。\
\n任务：从这段对话里找出值得长期记住的【家庭事实/偏好/事件/别名】。\
\n允许：称呼、习惯、长期偏好、家庭事件、家人物品与房间的叫法。\
\n禁止：运行状态（服务是否配置、音量、正在播放什么、固件/版本/端口/密钥）、\
一次性的问题与答案、助手自己说过的话、猜测。\
\n宁缺勿滥：没有合适的就输出 {\"candidates\":[]}。最多 3 条，每条不超过 60 字。\
\n输出格式：{\"candidates\":[{\"text\":\"…\",\"kind\":\"fact|preference|event|alias\",\"reason\":\"为什么值得记\"}]}";
        let extract_user = format!(
            "已经记得的事（不要重复）：\n{}\n\n本次对话：\n{}",
            if existing.is_empty() {
                "（暂无）".to_string()
            } else {
                existing
                    .iter()
                    .take(40)
                    .map(|t| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            lines
        );
        if authority.check().is_err() {
            return;
        }
        let extracted = match self
            .llm_json(&env, extract_system, &extract_user, 20_000)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                self.note(
                    "memory_extract_failed",
                    json!({ "device": device, "why": why, "stage": "extract", "error": e }),
                );
                return;
            }
        };
        let mut proposed: Vec<crate::memory::MemoryCandidate> = Vec::new();
        let mut refused: Vec<serde_json::Value> = Vec::new();
        for item in extracted["candidates"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let text = item["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }
            let kind = item["kind"].as_str().unwrap_or("fact");
            let reason = item["reason"].as_str().unwrap_or("");
            match env.memory.add_candidate_authorized(
                &authority,
                &text,
                kind,
                &session_id,
                why,
                reason,
            ) {
                Ok(c) => proposed.push(c),
                Err(e) => refused.push(json!({ "text": text, "reason": e })),
            }
            if proposed.len() >= crate::memory::MAX_PER_SESSION {
                break;
            }
        }
        if proposed.is_empty() {
            self.note(
                "memory_extract",
                json!({ "device": device, "why": why, "proposed": 0,
                        "refused": refused, "accepted": 0, "written": 0 }),
            );
            return;
        }
        let list = proposed
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{i}. [{}] {}", c.kind, c.text))
            .collect::<Vec<_>>()
            .join("\n");
        let confirm_system = "你是记忆审核器。只输出一个 JSON 对象。\
\n对每条候选判断：它是否是值得长期保留的家庭事实/偏好/事件/别名？\
\n拒绝：运行状态、配置、临时信息、助手的话、推测、过于笼统。只确认确实值得记的。\
\n输出格式：{\"decisions\":[{\"index\":0,\"ok\":true,\"reason\":\"…\"}]}";
        let confirm_user = format!("对话：\n{}\n\n候选：\n{}", lines, list);
        match self
            .llm_json(&env, confirm_system, &confirm_user, 20_000)
            .await
        {
            Ok(v) => {
                let decisions = v["decisions"].as_array().cloned().unwrap_or_default();
                let mut written = 0usize;
                let mut rejected = 0usize;
                for (i, cand) in proposed.iter().enumerate() {
                    let d = decisions
                        .iter()
                        .find(|d| d["index"].as_u64() == Some(i as u64));
                    let ok = d.and_then(|d| d["ok"].as_bool()).unwrap_or(false);
                    let reason = d
                        .and_then(|d| d["reason"].as_str())
                        .unwrap_or("审核器没有给出理由")
                        .to_string();
                    if ok {
                        match env.memory.confirm_candidate(&cand.id) {
                            Ok(saved) => {
                                written += 1;
                                if let Some(ref shared) = env.shared_memory {
                                    let room = authority.scope().room.as_deref().unwrap_or("");
                                    shared.enqueue_confirmed_memory(
                                        &device,
                                        room,
                                        &session_id,
                                        authority.epoch() as i64,
                                        &saved,
                                    );
                                }
                            }
                            Err(e) => {
                                rejected += 1;
                                let _ = env.memory.reject_candidate(&cand.id, &e);
                            }
                        }
                    } else {
                        rejected += 1;
                        let _ = env.memory.reject_candidate(&cand.id, &reason);
                    }
                }
                self.note(
                    "memory_extract",
                    json!({ "device": device, "why": why,
                            "proposed": proposed.len(), "refused": refused,
                            "accepted": written, "rejected": rejected, "written": written }),
                );
            }
            Err(e) => {
                self.note(
                    "memory_extract",
                    json!({ "device": device, "why": why,
                            "proposed": proposed.len(), "refused": refused,
                            "accepted": 0, "rejected": 0, "written": 0,
                            "pending": proposed.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                            "confirmation_failed": e }),
                );
            }
        }
    }

    pub(super) async fn llm_json(
        &self,
        env: &Arc<AgentEnv>,
        system: &str,
        user: &str,
        timeout_ms: u64,
    ) -> Result<serde_json::Value, String> {
        let messages = vec![WireMessage::system(system), WireMessage::user(user)];
        let fut = async {
            let mut stream = env
                .llm
                .chat_stream(&env.model, messages, &[])
                .await
                .map_err(|e| e.to_string())?;
            let mut text = String::new();
            while let Some(ev) = stream.next().await {
                match ev {
                    LlmEvent::Delta(t) => text.push_str(&t),
                    LlmEvent::End { content, .. } => {
                        if text.trim().is_empty() {
                            text = content;
                        }
                        break;
                    }
                    LlmEvent::Failed(m) => return Err(m),
                }
            }
            Ok::<String, String>(text)
        };
        let text = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut)
            .await
            .map_err(|_| "模型超时".to_string())??;
        parse_first_json(&text).ok_or_else(|| {
            format!(
                "模型输出不是 JSON：{}",
                text.chars().take(120).collect::<String>()
            )
        })
    }

    /// Wired by the API once the fleet plane exists (contract v2 #2).
    pub fn attach_fleet(&self, fleet: &Arc<crate::fleet::FleetService>) {
        *crate::lock_or_recover(&self.fleet, "voice.fleet") = Some(Arc::downgrade(fleet));
    }

    /// A PEER says it just spoke this cue (contract v2 #2).
    pub fn note_peer_cue(&self, device: &str, phrase: &str, at_ms: u64) {
        let phrase = phrase.trim();
        if phrase.is_empty() || phrase.chars().count() > 16 || device.trim().is_empty() {
            return;
        }
        let key = cue_key(phrase);
        if key.is_empty() {
            return;
        }
        let mut pane = crate::lock_or_recover(&self.foreign_cues, "voice.foreign_cues");
        pane.push_back((key, device.to_string(), at_ms));
        while pane.len() > HOUSE_CUE_PANE {
            pane.pop_front();
        }
        drop(pane);
        self.note(
            "mic_peer_cue",
            json!({ "from": device, "phrase": phrase, "at_ms": at_ms }),
        );
    }

    /// Contract v2 #2 (2026-11-16): remember that THIS device just said a cue,
    /// and tell the peers.
    pub(super) fn note_house_cue(&self, device: &str, phrase: &str) {
        let key = cue_key(phrase);
        if key.is_empty() {
            return;
        }
        let now = id::now_ms();
        let mut pane = crate::lock_or_recover(&self.foreign_cues, "voice.foreign_cues");
        pane.push_back((key, device.to_string(), now));
        while pane.len() > HOUSE_CUE_PANE {
            pane.pop_front();
        }
        pane.retain(|(_, _, at)| now.saturating_sub(*at) <= FOREIGN_CUE_WINDOW_MS);
        drop(pane);
        let fleet = crate::lock_or_recover(&self.fleet, "voice.fleet")
            .as_ref()
            .and_then(|w| w.upgrade());
        if let Some(f) = fleet {
            f.broadcast_cue(device, phrase, now);
        }
    }

    /// Wave 2: broadcast active TTS/media playback metadata (`FleetPlaybackNotice`)
    /// event-driven on speaking/music transitions so idle peers suppress cross-room
    /// bleed self-wakes without continuous polling.
    pub(super) fn notify_fleet_playback(
        &self,
        device: &str,
        channel: &str,
        active: bool,
        text_preview: &str,
    ) {
        let notice = crate::sovereignty::FleetPlaybackNotice {
            device: device.to_string(),
            room: self.room_of(device),
            channel: channel.to_string(),
            active,
            text_preview: text_preview.chars().take(64).collect(),
            at_ms: id::now_ms(),
            ttl_ms: crate::sovereignty::DEFAULT_PLAYBACK_NOTICE_TTL_MS,
        };
        self.sovereignty.record_playback_notice(notice.clone());
        let fleet = crate::lock_or_recover(&self.fleet, "voice.fleet")
            .as_ref()
            .and_then(|w| w.upgrade());
        if let Some(f) = fleet {
            f.broadcast_playback_notice(&notice);
        }
    }

    /// Test seam: put a cue in the house pane without a device/hub.
    #[cfg(test)]
    pub(crate) fn seed_house_cue(&self, device: &str, phrase: &str, at_ms: u64) {
        crate::lock_or_recover(&self.foreign_cues, "voice.foreign_cues").push_back((
            cue_key(phrase),
            device.to_string(),
            at_ms,
        ));
    }

    /// Is this transcript exactly a cue that ANOTHER device of this house just played?
    pub(super) fn foreign_cue_echo(
        &self,
        device: &str,
        text: &str,
    ) -> Option<(String, String, u64)> {
        let key = cue_key(text);
        if key.is_empty() {
            return None;
        }
        let now = id::now_ms();
        let mut pane = crate::lock_or_recover(&self.foreign_cues, "voice.foreign_cues");
        pane.retain(|(_, _, at)| now.saturating_sub(*at) <= FOREIGN_CUE_WINDOW_MS);
        pane.iter()
            .rev()
            .find(|(phrase, from, _)| from != device && *phrase == key)
            .cloned()
    }

    /// Record `seconds` of the post-AEC mono for this device.
    pub fn start_mic_capture(
        &self,
        device: &str,
        seconds: u64,
    ) -> Result<serde_json::Value, String> {
        let seconds = seconds.clamp(1, 30);
        let mut slot = crate::lock_or_recover(&self.mic_capture, "voice.mic_capture");
        if let Some(cap) = slot.as_ref() {
            if !cap.done {
                return Err("已有一路录音在进行".into());
            }
        }
        let rate = 16_000u32;
        let dir = self.data_dir.join("captures");
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
        let path = dir.join(format!("dot-{}.wav", id::now_ms()));
        *slot = Some(MicCapture {
            device: device.to_string(),
            samples: Vec::with_capacity((rate as u64 * seconds) as usize),
            rate,
            target: (rate as u64 * seconds) as usize,
            path: path.clone(),
            started_ms: id::now_ms(),
            done: false,
        });
        self.note(
            "mic_capture_start",
            json!({ "device": device, "seconds": seconds, "path": path.to_string_lossy() }),
        );
        Ok(json!({ "state": "recording", "seconds": seconds, "path": path.to_string_lossy() }))
    }

    pub fn mic_capture_status(&self, device: &str) -> serde_json::Value {
        mic_capture_status_json(
            crate::lock_or_recover(&self.mic_capture, "voice.mic_capture").as_ref(),
            device,
        )
    }

    /// The finished capture as WAV bytes (None until it is complete).
    pub fn mic_capture_wav(&self, device: &str) -> Option<Vec<u8>> {
        let slot = crate::lock_or_recover(&self.mic_capture, "voice.mic_capture");
        let cap = slot.as_ref().filter(|c| c.device == device && c.done)?;
        std::fs::read(&cap.path).ok()
    }

    pub(super) fn in_window_command(text: &str, wakes: &[String]) -> InWindow {
        for w in wakes {
            if let Some(span) = mic::wake_span_near(text, w) {
                let rest = mic::wake_tail(text, span);
                if rest.is_empty() || is_greeting_only(&rest) {
                    return InWindow::Bare;
                }
                return InWindow::Command(rest);
            }
        }
        InWindow::Command(text.to_string())
    }

    pub(super) fn barge_in_if_half_duplex(
        &self,
        device: &str,
        reason: &str,
        expected: Identity,
    ) -> Option<Identity> {
        let (decision, identity) = self.honor_wake_word(device, reason, expected, None)?;
        if !decision.stop {
            self.note(
                "barge_in_skipped_full_duplex",
                json!({ "device": device, "reason": reason, "why": decision.reason }),
            );
        }
        Some(identity)
    }
}
