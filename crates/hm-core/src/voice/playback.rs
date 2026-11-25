use super::*;

impl VoicePipeline {
    /// Channel liveness of hub-registered plays for a device.
    pub(super) fn hub_live(&self, device: &str) -> HubLive {
        let mut live = HubLive::default();
        for (_, p) in self.hub.active_plays_for(device) {
            if p.channel == "media" {
                live.media = true;
            } else {
                live.voice = true;
            }
        }
        live
    }

    /// Close a streamed voice play and start the stall clock for it. Every
    /// `play_end` goes through here: the device owes a terminal receipt from
    /// this instant, and `play_stall_sweep_at` uses the stamp to decide that
    /// it never came. (Whole-WAV plays have no stream to close.)
    pub(super) fn close_voice_stream(&self, play_id: &str, expected_ms: u64) {
        let _ = self.hub.play_end(play_id);
        let now = id::now_ms();
        for st in crate::lock_or_recover(&self.states, "voice.states").values_mut() {
            if let Some(rec) = st.outputs.get_mut(play_id) {
                rec.end_sent_ms = Some(now);
                rec.stage_ms = now;
                rec.expected_ms = expected_ms;
            }
        }
    }

    pub(super) fn fail_voice_stream(&self, play_id: &str) {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let now = id::now_ms();
        for st in states.values_mut() {
            if let Some(mut rec) = st.outputs.get(play_id).cloned() {
                rec.stage = "unknown".into();
                rec.stage_ms = now;
                st.outputs.insert(play_id.to_string(), rec.clone());
                st.note_output_terminal(play_id, &rec, now);
            }
        }
        let _ = self.hub.stop(play_id);
    }

    /// A voice play is being submitted (a streamed piece or a whole answer).
    ///
    /// D/T5 R1/R2: arm the answer protection period only on a NEW answer
    /// (no other voice play live on this device AND the previous voice play
    /// did not just end — streamed pieces are one answer, not many), and
    /// close any lingering barge-in window. That window exists so the
    /// INTERRUPTING utterance survives the stop, never so our own answer
    /// can be heard back into the VAD.
    pub(super) fn note_voice_play_submitted_locked(
        st: &mut DeviceVoiceState,
        now: u64,
        protect_ms: u64,
    ) {
        let live = st.outputs.values().any(|o| {
            o.channel == "voice" && !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
        });
        if live {
            return; // continuation piece of an answer already playing
        }
        // A new answer means the barge-in episode is over: its retained
        // head is our own stopped speech, not the user's next words.
        st.barge.reset();
        let continuation = now.saturating_sub(st.last_voice_end_ms) < VOICE_CONTINUATION_GAP_MS;
        if !continuation {
            st.barge_protect_until_ms = now.saturating_add(protect_ms);
        }
    }

    /// Recompute and (on change) send the assistant state for every known
    /// device. Used by the ticker and after a gateway reconnect (the new
    /// connection has no state — resend unconditionally so presentation
    /// catches up even when the value did not change).
    /// Rebuild the pre-synthesized ack WAVs in the background. One entry
    /// point for every writer that changes an ack phrase (`set_mode` keeps its
    /// own call because it also carries the force flag), so "the phrase
    /// changed" and "this node can speak it offline" cannot drift apart again.
    pub fn spawn_ack_synthesis(self: &Arc<Self>) {
        let pipe = self.clone();
        tokio::spawn(async move { pipe.synthesize_wake_acks(false).await });
    }

    // --- local wake ack / retreat ack (pre-synthesized WAVs on device) --------------

    pub(super) fn wake_ack_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("assets").join("wake-acks")
    }

    pub(super) fn load_ack_manifest(&self) -> AckManifest {
        let p = self.wake_ack_dir().join("manifest.json");
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// A phrase is ready when its WAV exists and was synthesized under the
    /// currently configured TTS voice (config read is a cheap local KV).
    pub(super) fn ack_ready(&self, phrase: &str, manifest: &AckManifest) -> bool {
        let Some(file) = manifest.files.get(phrase) else {
            return false;
        };
        if !self.wake_ack_dir().join(file).is_file() {
            return false;
        }
        // Voice-consistency: keep acks and answers in the same voice.
        let current = self.current_tts_identity();
        !manifest.voice.is_empty() && manifest.voice == current
    }

    /// Readiness and latest synthesis receipt for all configured local cue WAVs.
    pub fn cue_readiness(&self) -> serde_json::Value {
        let settings = crate::lock_or_recover(&self.settings, "voice.settings").clone();
        self.cue_readiness_for(&settings)
    }

    pub(super) fn cue_readiness_for(&self, s: &VoiceSettings) -> serde_json::Value {
        let mut phrases = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut push_unique = |phrase: String| {
            if !phrase.is_empty() && seen.insert(phrase.clone()) {
                phrases.push(phrase);
            }
        };
        for p in &s.wake_ack.phrases {
            push_unique(p.trim().to_string());
        }
        for ov in s.device_overrides.values() {
            if let Some(ack) = &ov.wake_ack {
                for p in &ack.phrases {
                    push_unique(p.trim().to_string());
                }
            }
        }
        push_unique(s.retreat_ack.trim().to_string());
        push_unique(s.window_close_ack.trim().to_string());
        push_unique(s.truncation_notice.trim().to_string());
        for p in s.filler.all_phrases() {
            push_unique(p.trim().to_string());
        }
        let manifest = self.load_ack_manifest();
        let tts_identity = self.current_tts_identity();
        let mut ready_phrases = Vec::new();
        let mut missing_phrases = Vec::new();
        for phrase in phrases {
            if self.ack_ready(&phrase, &manifest) {
                ready_phrases.push(phrase);
            } else {
                missing_phrases.push(phrase);
            }
        }
        let last_synthesis =
            crate::lock_or_recover(&self.last_cue_synth, "voice.last_cue_synth").clone();
        json!({
            "ready": missing_phrases.is_empty(),
            "wake_ack_enabled": s.wake_ack.enabled,
            "tts_identity": tts_identity,
            "manifest_voice": manifest.voice,
            "total_phrases": ready_phrases.len() + missing_phrases.len(),
            "ready_phrases": ready_phrases,
            "missing_phrases": missing_phrases,
            "last_synthesis": last_synthesis,
        })
    }

    pub(super) fn current_tts_identity(&self) -> String {
        // voice||model from the LIVE services config (cheap local KV read)
        // — a voice/model change invalidates the ack cache without restart.
        let raw = self
            .store
            .config_get(crate::config::CONFIG_KEY)
            .ok()
            .flatten()
            .unwrap_or_default();
        let cfg: crate::config::ServicesConfig = serde_json::from_str(&raw).unwrap_or_default();
        format!("{}|{}", cfg.tts.voice, cfg.tts.model)
    }

    /// Pick a random ready phrase (xorshift on the clock — no rand dep).
    pub(super) fn pick_ack_phrase_for(&self, device: &str) -> Option<String> {
        let wake_ack = crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .wake_ack;
        if !wake_ack.enabled || wake_ack.phrases.is_empty() {
            return None;
        }
        let phrases = wake_ack.phrases;
        let manifest = self.load_ack_manifest();
        let ready: Vec<String> = phrases
            .into_iter()
            .filter(|p| self.ack_ready(p, &manifest))
            .collect();
        if ready.is_empty() {
            return None;
        }
        let mut x = id::now_ms() as u64 ^ 0x9E3779B97F4A7C15;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        Some(ready[(x as usize) % ready.len()].clone())
    }

    /// Play one pre-synthesized ack WAV (wake ack or retreat ack). Local
    /// disk read + loopback submit only — no cloud anywhere on this path.
    pub(super) fn play_local_ack(
        &self,
        device: &str,
        phrase: &str,
        kind: &str,
    ) -> Option<crate::gateway::PlayTicket> {
        let permit = self.output_permit(device, None)?;
        self.play_local_ack_at(device, phrase, kind, permit)
    }

    pub(super) fn play_local_ack_at(
        &self,
        device: &str,
        phrase: &str,
        kind: &str,
        permit: OutputPermit,
    ) -> Option<crate::gateway::PlayTicket> {
        if !crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| permit.valid(st))
        {
            self.note(
                "stale_session_event_rejected",
                json!({"device":device,"via":kind,"generation":permit.generation}),
            );
            return None;
        }
        let manifest = self.load_ack_manifest();
        let Some(file) = manifest.files.get(phrase) else {
            self.note(
                kind,
                json!({ "device": device, "skipped": "no manifest entry" }),
            );
            return None;
        };
        let wav = match std::fs::read(self.wake_ack_dir().join(file)) {
            Ok(w) => w,
            Err(e) => {
                self.note(
                    kind,
                    json!({ "device": device, "skipped": format!("read: {e}") }),
                );
                return None;
            }
        };
        let req = PlayRequest {
            device: device.to_string(),
            content_ref: format!("ack:{phrase}"),
            audio: Some(wav),
            audio_meta: crate::gateway::AudioMeta {
                format: "wav".into(),
                sample_rate: 16000,
                channels: 1,
            },
            channel: "voice".into(),
            text: None,
            simulated_ms: None,
        };
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        if !states
            .get(device)
            .is_some_and(|st| permit.valid(st) && st.muted != Some(true))
        {
            drop(states);
            self.note(
                "stale_session_event_rejected",
                json!({"device":device,"via":kind,"generation":permit.generation}),
            );
            return None;
        }
        let submitted = match self.hub.play(req) {
            Err(e) => {
                self.note(
                    kind,
                    json!({ "device": device, "skipped": format!("play: {e}") }),
                );
                None
            }
            Ok(ticket) => {
                if let Some(st) = states.get_mut(device) {
                    // R2-BM5: the ack is an output of its own class. Keyed by
                    // the hub's play id so its terminal receipt retires it;
                    // it is not an answer and must never be counted as one.
                    let now = id::now_ms();
                    if kind == "wake_ack" {
                        st.wake_ack_play = Some(ticket.play_id.clone());
                    }
                    st.outputs.insert(
                        ticket.play_id.clone(),
                        OutputRecord {
                            answer_text: String::new(),
                            request: None,
                            task_id: None,
                            channel: "voice".into(),
                            kind: "ack".into(),
                            content_ref: format!("ack:{phrase}"),
                            stage: "submitted".into(),
                            started_ms: now,
                            stage_ms: now,
                            end_sent_ms: None,
                            expected_ms: 0,
                            synth_ms: 0,
                        },
                    );
                    // B-M1: remember what the device just said, so an ASR
                    // transcript that begins with it can be attributed to our
                    // own voice.
                    st.last_cue = Some((phrase.to_string(), now));
                }
                // #2 (2026-11-16): a cue is also a HOUSE fact — the neighbour
                // may hear it and transcribe it as a request.
                self.note_house_cue(device, phrase);
                Some(ticket)
            }
        };
        drop(states);
        // The receipt lands as a normal play receipt. Callers that must not
        // talk over the ack (the ASR-confirmed wake) keep the ticket.
        self.note(kind, json!({ "device": device, "phrase": phrase }));
        submitted
    }
    pub(super) async fn speak_wake_ack(&self, device: &str, permit: OutputPermit) -> AckOutcome {
        if !crate::lock_or_recover(&self.settings, "voice.settings")
            .effective_for(device)
            .wake_ack
            .enabled
        {
            return AckOutcome::NotPlayed;
        }
        let Some(phrase) = self.pick_ack_phrase_for(device) else {
            return AckOutcome::NotPlayed;
        };
        let Some(ticket) = self.play_local_ack_at(device, &phrase, "wake_ack", permit) else {
            return AckOutcome::NotPlayed;
        };
        let outcome = match tokio::time::timeout(
            std::time::Duration::from_millis(ACK_SETTLE_MS),
            ticket.receipt,
        )
        .await
        {
            // The device finished the audio: the strongest fact there is.
            Ok(Ok(GatewayFrame::PlayFinished { played_ms, .. })) => {
                self.note(
                    "wake_ack_played",
                    json!({ "device": device, "phrase": phrase, "played_ms": played_ms }),
                );
                AckOutcome::Finished
            }
            // `device_confirmed` is the device's last write reaching the DAC;
            // a device without `play_finished` is as done as it can say.
            Ok(Ok(GatewayFrame::PlayReceipt { stage, detail, .. })) => {
                if stage == "device_confirmed" {
                    AckOutcome::Finished
                } else {
                    self.note(
                        "wake_ack_device_failed",
                        json!({ "device": device, "phrase": phrase, "stage": stage, "detail": detail }),
                    );
                    AckOutcome::Failed
                }
            }
            // Sender dropped: stop, disconnect, or the play was superseded.
            Ok(Err(_)) => AckOutcome::Dropped,
            Err(_) => AckOutcome::Timeout,
            Ok(Ok(_)) => AckOutcome::Dropped,
        };
        self.note(
            "wake_ack_settled",
            json!({ "device": device, "phrase": phrase,
                    "settled": outcome.covers_the_wait(), "outcome": outcome.label() }),
        );
        outcome
    }

    /// Strip a leading phrase OUR OWN device just spoke (B-M1). The wake ack
    /// is short and pre-synthesized, and its tail lands in the next mic
    /// segment often enough that the user's question arrives as
    /// "在呢。今天几号" — the answer then ignored the question and its source
    /// was unexplainable. Only a phrase this device actually played inside
    /// the window is a candidate, and every strip is recorded with the age
    /// and the phrase, so the transcript's source stays auditable.
    pub(super) fn strip_cue_echo(&self, device: &str, text: &str) -> String {
        let recent = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| st.last_cue.clone());
        let Some((phrase, at)) = recent else {
            return text.to_string();
        };
        let age = id::now_ms().saturating_sub(at);
        if age > CUE_ECHO_WINDOW_MS {
            return text.to_string();
        }
        let mut current = text.to_string();
        let mut count = 0;
        // Repeat: "在呢。在呢。今天几号" is possible after two acks.
        while let Some(rest) = strip_leading_phrase(&current, &phrase) {
            count += 1;
            current = rest;
            if current.is_empty() {
                break;
            }
        }
        if count > 0 {
            self.note(
                "cue_echo_stripped",
                json!({ "device": device, "phrase": phrase, "count": count,
                        "age_ms": age, "rest": current }),
            );
        }
        current
    }

    /// Deterministic stop / keep-going intents (B-M1). Returns true when the
    /// utterance was CONSUMED — no model turn, no chatter: talking back is
    /// not what "停" asks for, and the LLM would only say that it stops.
    ///
    /// The keep-going case is honest about its limit: the device's semantic
    /// barge-in stops the playback at the speech ONSET, hundreds of ms before
    /// ASR knows the words. So this records a false stop (and counts it for
    /// the acceptance metric) instead of pretending it resumed anything.
    /// R1e: pause words. Kept tight on purpose — 「停」 is the discard intent
    /// and must not become "pause"; these only freeze a program at a sentence
    /// boundary so 继续 can pick it up.
    /// Anything a speech engine could read: letters (CJK included) or digits.
    /// A segment left over from quote-aware splitting can be `”` alone, and
    /// Volc answers that with 45002001 "No readable text!" — which our pipeline
    /// treated as an answer-level failure (device log 2026-11-12: a long joke
    /// with quoted dialogue produced one punctuation-only segment and eight
    /// tts_failed events). Punctuation is not speech: skip it, say so in the
    /// log, and keep the rest of the answer.
    pub(super) fn speakable(text: &str) -> bool {
        text.chars().any(|c| c.is_alphanumeric())
    }

    /// A byte offset that is safe to slice at. A program cursor is advanced by
    /// text that was TRIMMED before it was spoken (`take_speak_segment`), so
    /// an offset can land one byte inside a multi-byte character; slicing
    /// there panics the request handler (seen on the device 2026-11-12), and a
    /// bookmark is not worth a crash.
    pub(super) fn clamp_boundary(text: &str, i: usize) -> usize {
        let mut i = i.min(text.len());
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    pub(super) fn sentence_len(text: &str) -> usize {
        let mut chars = 0usize;
        for ch in text.chars() {
            chars += 1;
            if matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n') {
                return text
                    .char_indices()
                    .nth(chars)
                    .map(|(i, _)| i)
                    .unwrap_or(text.len());
            }
            if chars >= 96 {
                // Punctuation-free text (a model that forgot to punctuate):
                // bound the chunk instead of synthesizing the whole answer.
                return text
                    .char_indices()
                    .nth(chars)
                    .map(|(i, _)| i)
                    .unwrap_or(text.len());
            }
        }
        text.len()
    }

    /// Freeze the answer being spoken at its current sentence boundary and
    /// stop the audio. The bookmark is the whole point: 继续 repeats the
    /// sentence the user cut, not the story from its first word.
    pub fn pause_program(&self, device: &str, why: &str) -> bool {
        self.pause_program_with_cancel(device, why, None)
    }

    pub fn pause_program_with_cancel(
        &self,
        device: &str,
        why: &str,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> bool {
        let hub_voice = self.hub_live(device).voice;
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let Some(st) = states.get_mut(device) else {
            return false;
        };
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst)) {
            return false;
        }
        let voice_live = hub_voice
            || st
                .outputs
                .values()
                .any(|o| o.channel == "voice" && stage_is_live(&o.stage));
        let mut program = st.live_program.take();
        let stop = if voice_live {
            self.stop_device_locked(device, st, true)
        } else {
            json!({"stop_requests_sent":0,"note":"nothing was playing"})
        };
        if let Some(p) = program.as_mut() {
            p.created_ms = id::now_ms();
            p.system_cut = false;
        }
        if program.is_some() {
            st.paused_program = program;
        }
        let bookmark = st.paused_program.as_ref().map(|p|json!({"cursor":p.cursor,"chars":p.text.len(),"complete":p.complete,
            "next":p.text[Self::clamp_boundary(&p.text,p.cursor)..].chars().take(40).collect::<String>()}));
        drop(states);
        self.note(
            if bookmark.is_some() {
                "program_paused"
            } else {
                "program_pause_noop"
            },
            json!({"device":device,"why":why,"bookmark":bookmark,"stop":stop}),
        );
        bookmark.is_some()
    }

    pub fn stop_program_with_cancel(
        &self,
        device: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> bool {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let Some(st) = states.get_mut(device) else {
            return false;
        };
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        st.live_program = None;
        st.paused_program = None;
        self.stop_device_locked(device, st, true);
        true
    }

    /// Contract v2 #3 (2026-11-16): a stop-type TOOL stops the CONTENT, never
    /// the turn that asked for it.
    ///
    /// `stop_program_with_cancel` above is the person's stop (「停止播放」 /
    /// 退下): it bumps the stop epoch and cancels every request, which is
    /// exactly right for the highest-priority command. When `music.control
    /// stop` calls it from INSIDE a turn, that same invalidation kills the
    /// turn's own request: dot2-b 2026-11-16 08:31:10 (「睡觉吧。」→ the model
    /// turned the study lights off and stopped a program) spoke 853 ms of its
    /// answer and then went silent with `turn_barged_in`,
    /// `stop.why=stop_epoch_changed` — the person heard a cut sentence
    /// instead of the fact「当前内容已停止。」.
    ///
    /// This variant keeps the turn: no epoch bump, no request cancellation,
    /// and every play that is NOT the caller's is stopped by name. The
    /// bookmarks still go (stop means discard; pause is the word that keeps
    /// one). Returns whether anything was actually stopped — the tool must
    /// not claim a stop that had nothing to stop.
    pub fn stop_program_for_tool(
        &self,
        device: &str,
        keep_task: Option<&str>,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let Some(st) = states.get_mut(device) else {
            return false;
        };
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        let keep = keep_task.map(str::to_string);
        // Record what the program was, so the log can say what was discarded.
        let origin = st
            .live_program
            .as_ref()
            .and_then(|p| p.origin_task.clone())
            .or_else(|| {
                st.paused_program
                    .as_ref()
                    .and_then(|p| p.origin_task.clone())
            });
        let discarded = st.live_program.take().is_some() | st.paused_program.take().is_some();
        // Stop the plays: the program's own task when known, otherwise every
        // play except the caller's own turn. Never the caller's.
        let mut stopped = Vec::new();
        for (pid, rec) in st.outputs.iter_mut() {
            if let Some(k) = keep.as_deref() {
                if rec.task_id.as_deref() == Some(k) {
                    continue;
                }
            }
            if let Some(o) = origin.as_deref() {
                if rec.task_id.as_deref() != Some(o) {
                    continue;
                }
            }
            rec.stage = "stopping".into();
            stopped.push(pid.clone());
        }
        // The program's producer (a previous turn still synthesizing) must not
        // keep feeding the channel it just lost.
        if let Some(o) = origin.as_deref() {
            if keep.as_deref() != Some(o) {
                self.tasks.request_cancel(o);
            }
        }
        drop(states);
        for pid in &stopped {
            let _ = self.hub.stop(pid);
        }
        self.note(
            "program_stopped_for_tool",
            json!({ "device": device, "task": keep, "program_task": origin,
                    "bookmark_discarded": discarded, "plays_stopped": stopped }),
        );
        discarded || !stopped.is_empty()
    }

    pub fn paused_bookmark(&self, device: &str) -> Option<serde_json::Value> {
        self.paused_summary(device)
    }

    // --- M3: the media half of the interaction ---------------------------

    /// Wired by the API once both runtimes exist. Weak both ways: the API
    /// owns both, neither owns the other.
    pub fn attach_music(&self, music: &Arc<crate::tool::music::MusicRuntime>) {
        *crate::lock_or_recover(&self.music, "voice.music") = Some(Arc::downgrade(music));
    }

    pub(super) fn music(&self) -> Option<Arc<crate::tool::music::MusicRuntime>> {
        crate::lock_or_recover(&self.music, "voice.music")
            .as_ref()
            .and_then(|w| w.upgrade())
    }

    /// Does this device have a program a control word can act on (being read
    /// now, or bookmarked)? The media half asks the music runtime.
    pub fn program_controllable(&self, device: &str) -> bool {
        crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .map(|st| st.live_program.is_some() || st.paused_program.is_some())
            .unwrap_or(false)
    }

    pub(super) fn media_state(&self, device: &str) -> Option<crate::tool::music::MediaState> {
        self.music().and_then(|m| m.media_state(device))
    }

    /// Wake = take over. Must run OUTSIDE `self.states` and BEFORE stop-all.
    pub(super) fn pause_media_for_takeover(&self, device: &str) -> Option<serde_json::Value> {
        let music = self.music()?;
        if let Some((title, position_ms)) = music.pause_now(device, "wake_takeover") {
            return Some(json!({ "title": title, "position_ms": position_ms }));
        }
        if let Some(s) = music
            .peek_live_session(device)
            .filter(|s| s.state == crate::tool::music::MediaState::Paused)
        {
            let title = s.current().map(|t| t.title.clone()).unwrap_or_default();
            return Some(
                json!({ "title": title, "position_ms": s.position_ms, "already_paused": true }),
            );
        }
        if let Some((title, position_ms)) = music.pause_now(device, "wake_takeover_retry") {
            return Some(json!({ "title": title, "position_ms": position_ms, "retried": true }));
        }
        self.note(
            "media_takeover_pause_failed",
            json!({ "device": device,
            "why": "pause_now returned None during active media takeover",
            "media_state": music.media_state(device), "notice": "刚才那首没能接上" }),
        );
        None
    }

    /// The pause word's target, in the same order as `music.control`:
    /// foreground media, then a live program, then anything merely paused.
    pub(super) fn pause_current(
        &self,
        device: &str,
        why: &str,
        expected: Option<Identity>,
    ) -> Option<&'static str> {
        let music = self.music();
        {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get(device)?;
            if expected.is_some_and(|e| st.participation.identity() != e) {
                return None;
            }
        }
        let media = music.as_ref().and_then(|m| m.media_state(device));
        if media == Some(crate::tool::music::MediaState::Playing) {
            music.as_ref()?.pause_now(device, why)?;
            return Some("media");
        }
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.get_mut(device)?;
        if expected.is_some_and(|e| st.participation.identity() != e) {
            return None;
        }
        if let Some(mut program) = st.live_program.take() {
            let stop = self.stop_device_locked(device, st, true);
            program.created_ms = id::now_ms();
            program.system_cut = false;
            let bookmark = json!({"cursor":program.cursor,"chars":program.text.len(),"complete":program.complete});
            st.paused_program = Some(program);
            drop(states);
            self.note(
                "program_paused",
                json!({"device":device,"why":why,"bookmark":bookmark,"stop":stop}),
            );
            return Some("program");
        }
        if media.is_some() {
            return Some("media");
        }
        st.paused_program.as_ref().map(|_| "program")
    }

    pub(super) fn resume_current(
        self: &Arc<Self>,
        device: &str,
        why: &str,
        expected: Option<Identity>,
    ) -> Option<&'static str> {
        let music = self.music();
        let identity = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get(device)?;
            if expected.is_some_and(|e| e != st.participation.identity()) {
                return None;
            }
            st.participation.identity()
        };
        let media = music.as_ref().and_then(|m| m.media_state(device));
        if media == Some(crate::tool::music::MediaState::Paused) {
            let music = music.clone()?;
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            music.register_control(device, cancel.clone());
            let (me, d, why) = (self.clone(), device.to_string(), why.to_string());
            tokio::spawn(async move {
                let outcome = music
                    .control_with_cancel(&d, "resume", None, cancel, None)
                    .await;
                me.note(
                    "media_resumed",
                    json!({"device":d,"why":why,"ok":outcome.is_ok(),
                    "detail":outcome.map(|o|o.detail).unwrap_or_else(|e|e.code().to_string())}),
                );
            });
            return Some("media");
        }
        let has_paused = crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|st| st.paused_program.is_some());
        if !has_paused {
            return (media == Some(crate::tool::music::MediaState::Playing)).then_some("media");
        }
        self.resume_program_at(device, identity)
            .then_some("program")
    }

    pub(super) fn paused_summary(&self, device: &str) -> Option<serde_json::Value> {
        crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .and_then(|st| st.paused_program.as_ref())
            .map(|p| {
                json!({
                    "cursor": p.cursor, "chars": p.text.len(), "complete": p.complete,
                    "since_ms": p.created_ms,
                    "next": p.text[Self::clamp_boundary(&p.text, p.cursor)..].chars().take(60).collect::<String>(),
                })
            })
    }

    /// Discard the program state (R1e: 停 is a discard, not a pause). Used by
    /// the management stop: the console's stop button must mean what the word
    /// means, not leave a resume point behind.
    pub fn discard_program(&self, device: &str) -> bool {
        let dropped = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            match states.get_mut(device) {
                Some(st) => {
                    let had = st.live_program.is_some() || st.paused_program.is_some();
                    st.live_program = None;
                    st.paused_program = None;
                    had
                }
                None => false,
            }
        };
        if dropped {
            self.note(
                "program_discarded",
                json!({ "device": device, "why": "stop means discard" }),
            );
        }
        dropped
    }

    /// Play a paused program again from its bookmark. Spawned so the event
    /// pump is never blocked by synthesis.
    pub fn resume_program(self: &Arc<Self>, device: &str) -> bool {
        let expected = crate::lock_or_recover(&self.states, "voice.states")
            .entry(device.to_string())
            .or_default()
            .participation
            .identity();
        self.resume_program_at(device, expected)
    }

    pub fn resume_program_with_cancel(
        self: &Arc<Self>,
        device: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> bool {
        let expected = crate::lock_or_recover(&self.states, "voice.states")
            .entry(device.to_string())
            .or_default()
            .participation
            .identity();
        self.resume_program_guarded(device, expected, Some(cancel))
    }

    pub(super) fn resume_program_at(self: &Arc<Self>, device: &str, expected: Identity) -> bool {
        self.resume_program_guarded(device, expected, None)
    }

    pub(super) fn resume_program_guarded(
        self: &Arc<Self>,
        device: &str,
        expected: Identity,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> bool {
        // The epoch is read INSIDE the take: the continuation belongs to the
        // conversation that was on hold when the bookmark was taken, not to
        // whatever conversation exists when the spawned task starts. Reading
        // it in the task let a retreat that landed in between look like a
        // fresh conversation, so the story played on after 退下 (codex
        // interaction audit 2026-11-14, finding 7).
        let (program, epoch, request) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let Some(st) = states.get_mut(device) else {
                return false;
            };
            if st.participation.identity() != expected {
                return false;
            }
            if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst)) {
                return false;
            }
            let epoch = st.stop_epoch;
            match st.paused_program.take() {
                Some(p) => {
                    let Some(request) = st
                        .participation
                        .resume_request(st.participation.identity(), id::now_ms())
                    else {
                        st.paused_program = Some(p);
                        return false;
                    };
                    st.follow_close_pending_at = None;
                    (p, epoch, request)
                }
                None => return false,
            }
        };
        let me = self.clone();
        let d = device.to_string();
        tokio::spawn(async move {
            me.clone()
                .speak_program(d.clone(), program, epoch, Some(request))
                .await;
            if let Some(st) = crate::lock_or_recover(&me.states, "voice.states").get_mut(&d) {
                st.finish_window_request(request, id::now_ms());
            }
        });
        true
    }

    /// What the delivery's consumed number rests on: `hw_delay` only when
    /// EVERY sentence came with a hardware measurement; one "written" sentence
    /// marks the whole delivery as written (the conservative direction).
    pub(super) fn consumed_basis(bases: &[&'static str]) -> &'static str {
        if bases.is_empty() {
            "none"
        } else if bases.iter().all(|b| *b == "hw_delay") {
            "hw_delay"
        } else if bases.iter().any(|b| *b == "hw_delay") {
            "mixed"
        } else {
            "written"
        }
    }

    /// Speak a program sentence by sentence, advancing the bookmark BEFORE
    /// each sentence so a pause lands on the sentence in flight. No model is
    /// involved: the text already exists.
    pub(super) async fn speak_program(
        self: Arc<Self>,
        device: String,
        mut program: Program,
        epoch: u64,
        request: Option<RequestTicket>,
    ) {
        let Some(permit) = self.output_permit(&device, request) else {
            return;
        };
        // The epoch came from `resume_program` (the moment the bookmark was
        // taken). If the conversation has moved on since — 退下, a stop, a
        // new wake — the continuation is DISCARDED, not played into the new
        // one (codex interaction audit 2026-11-14, finding 7).
        if self.stop_epoch_of(&device) != epoch {
            self.note(
                "program_resume_dropped",
                json!({ "device": device, "why": "stop_epoch changed between take and start" }),
            );
            return;
        }
        // R2 (2026-11-13): a continuation is budgeted like any other spoken
        // delivery — the person asked for MORE, not for all of it at once.
        let (spoken_settings, hint) = {
            let settings = crate::lock_or_recover(&self.settings, "voice.settings");
            (settings.spoken.clone(), settings.effective_continue_hint())
        };
        // 2026-11-14: read a PARAGRAPH per 「继续」 — the program's own
        // snapshot of the backstop, not the two-sentence short budget that
        // made every continuation ask for the next one.
        let mut cap = Self::continuation_cap(&program, &spoken_settings);
        // A resumed CUT answer announces itself once: without it the tail of
        // an answer from a minute ago is indistinguishable from a new reply
        // (device 2026-11-14: 「继续」 played "比如说…" and was heard as the
        // assistant asking what to continue).
        let lead = spoken_settings.resume_lead.trim().to_string();
        let mut cursor = Self::clamp_boundary(&program.text, program.cursor);
        // M5: the three positions of this delivery, kept apart.
        let mut sentences_submitted: u64 = 0;
        let mut played_ms_total: u64 = 0;
        let mut consumed_ms_total: u64 = 0;
        let mut bases: Vec<&'static str> = Vec::new();
        while cursor < program.text.len() {
            let end = Self::clamp_boundary(
                &program.text,
                cursor + Self::sentence_len(&program.text[cursor..]),
            );
            program.cursor = cursor;
            {
                let mut states = crate::lock_or_recover(&self.states, "voice.states");
                if let Some(st) = states.get_mut(&device).filter(|st| {
                    st.stop_epoch == epoch && request.is_none_or(|r| st.participation.contains(r))
                }) {
                    st.live_program = Some(program.clone());
                    st.paused_program = None;
                }
            }
            let seg = program.text[cursor..end].trim().to_string();
            if !seg.is_empty() {
                match cap.admit(&seg) {
                    Some(speak_text) => {
                        // M5: wait for THIS sentence's terminal before the next
                        // one is submitted. Sequencing used to be left to the
                        // device queue, so a pause could land between a
                        // sentence's synthesis and its playback and the
                        // bookmark (advanced before speaking) then pointed
                        // PAST audio nobody heard.
                        sentences_submitted += 1;
                        // Once per program: the lead labels the first
                        // continuation, later ones just read on.
                        let speak_text = match Self::take_resume_lead(&mut program, &lead) {
                            Some(l) => {
                                self.note(
                                    "spoken_resume_lead",
                                    json!({ "device": device, "lead": l }),
                                );
                                format!("{l}{speak_text}")
                            }
                            None => speak_text,
                        };
                        let terminal = self
                            .speak_inner(&device, &speak_text, None, "answer", true, request)
                            .await;
                        match &terminal {
                            Some(t) => {
                                played_ms_total += t.played_ms.unwrap_or(0);
                                match t.consumed() {
                                    Some(ms) => {
                                        consumed_ms_total += ms;
                                        bases.push("hw_delay");
                                    }
                                    None if t.written_fully() => bases.push("written"),
                                    None => {}
                                }
                            }
                            None => {}
                        }
                        let finished = terminal
                            .as_ref()
                            .map(|t| t.heard() || t.written_fully())
                            .unwrap_or(false);
                        if !finished {
                            // The sentence did not run to its end (stop, link
                            // loss, device failure, or never submitted).
                            // A stop/退下 DISCARDS the reading (the codex
                            // review fix, 2026-11-13): recreating a bookmark
                            // from the interrupted resume made the ring go
                            // amber after a session end and 「继续」 could
                            // replay a program the person had ended. Only a
                            // failure the user did NOT ask for (link loss,
                            // device fault, timeout) keeps the position —
                            // repeating a sentence is a smaller error than
                            // skipping audio nobody heard.
                            // `stopped` is decided INSIDE the same guard that
                            // restores the bookmark: a retreat landing between
                            // a check and the restore used to resurrect a
                            // program the person had ended.
                            let stopped = {
                                let mut states =
                                    crate::lock_or_recover(&self.states, "voice.states");
                                match states.get_mut(&device) {
                                    Some(st) => {
                                        let stopped = st.stop_epoch != epoch
                                            || request
                                                .is_some_and(|r| !st.participation.contains(r));
                                        if !stopped
                                            || st.live_program.as_ref().is_some_and(|p| {
                                                p.origin_task == program.origin_task
                                                    && p.text == program.text
                                            })
                                        {
                                            st.live_program = None;
                                        }
                                        if !stopped && st.paused_program.is_none() {
                                            let mut p = program.clone();
                                            p.cursor = cursor;
                                            p.resumable = true;
                                            st.paused_program = Some(p);
                                        }
                                        stopped
                                    }
                                    None => true,
                                }
                            };
                            self.note(if stopped { "program_interrupted" } else { "program_sentence_unheard" }, json!({
                                "device": device, "cursor": cursor, "bookmark_kept": !stopped,
                                "play_id": terminal.as_ref().map(|t| t.play_id.clone()),
                                "stage": terminal.as_ref().map(|t| t.stage.clone()),
                                "consumed_ms": terminal.as_ref().and_then(|t| t.consumed_ms),
                                "progress_basis": terminal.as_ref().and_then(|t| t.progress_basis.clone()),
                            }));
                            let basis = Self::consumed_basis(&bases);
                            self.note(
                                "program_delivery",
                                json!({
                                    "device": device, "sentences": sentences_submitted,
                                    "chars": program.text.len(), "played_ms": played_ms_total,
                                    "consumed_ms": consumed_ms_total, "basis": basis,
                                    "cursor": cursor, "advanced": false,
                                }),
                            );
                            return;
                        }
                    }
                    None => {
                        // The cap paused the reading; the rest stays booked
                        // exactly where it stopped — but ONLY while this
                        // delivery is still the current one. A stop/退下 that
                        // landed means discard, never a resurrected bookmark.
                        if self.stop_epoch_of(&device) != epoch {
                            let mut states = crate::lock_or_recover(&self.states, "voice.states");
                            if let Some(st) = states.get_mut(&device) {
                                if st.live_program.as_ref().is_some_and(|p| {
                                    p.origin_task == program.origin_task && p.text == program.text
                                }) {
                                    st.live_program = None;
                                }
                            }
                            self.note(
                                "program_interrupted",
                                json!({"device": device, "cursor": cursor, "bookmark_kept": false}),
                            );
                            return;
                        }
                        let mut p = program.clone();
                        p.cursor = cursor;
                        // The cap spent itself again: a system cut, whatever
                        // this program was before it was resumed.
                        p.system_cut = true;
                        {
                            let mut states = crate::lock_or_recover(&self.states, "voice.states");
                            if let Some(st) = states.get_mut(&device).filter(|st| {
                                st.stop_epoch == epoch
                                    && request.is_none_or(|r| st.participation.contains(r))
                            }) {
                                st.paused_program = Some(p);
                                st.live_program = None;
                            }
                        }
                        if !hint.is_empty() {
                            self.speak_inner(&device, &hint, None, "answer", false, request)
                                .await;
                        }
                        self.note(
                            "program_budget_paused",
                            json!({
                                "device": device, "cursor": cursor,
                                "max_sentences": cap.max_sentences, "max_chars": cap.max_chars,
                                "why": "the spoken cap is spent; the rest stays booked",
                            }),
                        );
                        return;
                    }
                }
            }
            if self.stop_epoch_of(&device) != epoch {
                let bookmark_kept = {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    match states.get_mut(&device) {
                        Some(st) => {
                            if st.live_program.as_ref().is_some_and(|p| {
                                p.origin_task == program.origin_task && p.text == program.text
                            }) {
                                st.live_program = None;
                            }
                            st.paused_program.is_some()
                        }
                        None => false,
                    }
                };
                self.note(
                    "program_interrupted",
                    json!({"device": device, "cursor": cursor, "bookmark_kept": bookmark_kept}),
                );
                return;
            }
            cursor = end;
        }
        // M5: the delivery ran to its end. Generated / submitted / consumed
        // stay three separate numbers — a single `played_ms` cannot say which
        // of them it is, and that ambiguity is what made "was I heard?"
        // unanswerable.
        self.note(
            "program_delivery",
            json!({
                "device": device, "sentences": sentences_submitted,
                "chars": program.text.len(), "played_ms": played_ms_total,
                "consumed_ms": consumed_ms_total, "basis": Self::consumed_basis(&bases),
                "cursor": cursor, "advanced": true,
            }),
        );
        // R1h/P5: the program was cut while the model was still generating,
        // so what the person just heard is everything that ever existed.
        // LABEL it — do not regenerate (user decision 2026-11-12). The notice
        // is the program's LAST SENTENCE, submitted on the voice channel
        // before the program is retired, so the half-duplex gate closes once
        // for the whole program (an independent ack after the program ended
        // would open the mic between the two, and R1g measured an ack
        // swallowing the command in the same breath).
        // Only real long content is labelled: a short answer that never
        // finished is not a truncated story, and saying so (with the default
        // notice wording) was simply wrong.
        if !program.complete && Self::resumable_program(&program) {
            self.speak_truncation_notice(&device, &program, permit)
                .await;
        }
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            if let Some(st) = states.get_mut(&device).filter(|st| {
                st.stop_epoch == epoch && request.is_none_or(|r| st.participation.contains(r))
            }) {
                st.live_program = None;
                st.paused_program = None;
            }
        }
        self.note(
            "program_finished",
            json!({ "device": device, "chars": program.text.len(), "complete": program.complete }),
        );
    }

    /// Speak the configured truncation notice for a program that ended
    /// mid-generation. Prefers the pre-synthesized cue WAV (no cloud wait at
    /// the end of a story); falls back to the TTS path when the cache is not
    /// ready yet, so the person always hears it once the phrase is set.
    pub(super) async fn speak_truncation_notice(
        &self,
        device: &str,
        program: &Program,
        permit: OutputPermit,
    ) {
        let phrase = crate::lock_or_recover(&self.settings, "voice.settings")
            .truncation_notice
            .trim()
            .to_string();
        if phrase.is_empty() {
            self.note(
                "program_truncation_notice_skipped",
                json!({ "device": device, "reason": "no phrase configured" }),
            );
            return;
        }
        // A stop that landed while the last sentence was being submitted
        // invalidates the tail of this program too.
        if !crate::lock_or_recover(&self.states, "voice.states")
            .get(device)
            .is_some_and(|s| permit.valid(s))
        {
            self.note(
                "program_truncation_notice_skipped",
                json!({ "device": device, "reason": "stopped before the notice" }),
            );
            return;
        }
        let cached = {
            let manifest = self.load_ack_manifest();
            self.ack_ready(&phrase, &manifest)
                .then(|| manifest.files.get(&phrase).cloned())
                .flatten()
        };
        let source = match cached {
            Some(file) => {
                match std::fs::read(self.wake_ack_dir().join(&file)) {
                    Ok(wav) => {
                        let req = PlayRequest {
                            device: device.to_string(),
                            content_ref: format!("notice:{phrase}"),
                            audio: Some(wav),
                            audio_meta: crate::gateway::AudioMeta {
                                format: "wav".into(),
                                sample_rate: 16000,
                                channels: 1,
                            },
                            channel: "voice".into(),
                            text: None,
                            simulated_ms: None,
                        };
                        let mut states = crate::lock_or_recover(&self.states, "voice.states");
                        if !states.get(device).is_some_and(|st| permit.valid(st)) {
                            return;
                        }
                        match self.hub.play(req) {
                            Ok(ticket) => {
                                let notice_play = ticket.play_id.clone();
                                if let Some(st) = states.get_mut(device) {
                                    // Same self-echo bookkeeping as every
                                    // other local phrase.
                                    st.last_cue = Some((phrase.clone(), id::now_ms()));
                                    // R2-BM5: the truncation notice is a
                                    // PROMPT, not an answer — the receipt
                                    // now carries its output class.
                                    st.outputs.insert(
                                        ticket.play_id.clone(),
                                        OutputRecord {
                                            answer_text: String::new(),
                                            request: None,
                                            task_id: None,
                                            channel: "voice".into(),
                                            kind: "prompt".into(),
                                            content_ref: format!("notice:{phrase}"),
                                            stage: "submitted".into(),
                                            started_ms: id::now_ms(),
                                            stage_ms: id::now_ms(),
                                            end_sent_ms: None,
                                            expected_ms: 0,
                                            synth_ms: 0,
                                        },
                                    );
                                }
                                // M5: the same submission evidence the cloud
                                // path writes — a cached phrase must not be
                                // the one play whose class nobody recorded.
                                self.note("tts_played", json!({
                                    "device": device, "play_id": notice_play, "task": null,
                                    "delivery_kind": "prompt", "phrase": phrase, "source": "cache",
                                }));
                                "cache"
                            }
                            Err(e) => {
                                self.note(
                                    "program_truncation_notice_error",
                                    json!({ "device": device, "error": format!("{e}") }),
                                );
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        self.note(
                            "program_truncation_notice_error",
                            json!({ "device": device, "error": format!("read: {e}") }),
                        );
                        return;
                    }
                }
            }
            None => {
                self.speak_inner_at(device, &phrase, None, "prompt", false, permit)
                    .await;
                "tts"
            }
        };
        self.note(
            "program_truncation_notice",
            json!({ "device": device, "phrase": phrase, "source": source,
                    "cursor": program.cursor, "chars": program.text.len() }),
        );
    }

    pub(super) fn handle_stop_intent_at(
        self: &Arc<Self>,
        device: &str,
        text: &str,
        via: &str,
        mut identity: Option<&mut Identity>,
        debit: Option<&mut bool>,
    ) -> bool {
        let (enabled, stop_words, continue_words) = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            (
                s.stop_intents.enabled,
                s.stop_intents.stop_words.clone(),
                s.stop_intents.continue_words.clone(),
            )
        };
        if !enabled {
            return false;
        }
        let now = id::now_ms();
        // R1e: intents read the last sentence (see `last_sentence`).
        let said = last_sentence(text);
        // A locally recognized interruption must begin with its command (or
        // polite particles). Other wording belongs to model semantics, so an
        // embedded stop word cannot consume an intended session exit.
        if !continue_match(said, &continue_words) && !pause_match(said) {
            let normalized = normalize_utterance(said);
            let has_command_prefix = stop_words.iter().any(|word| {
                let word = normalize_utterance(word);
                let Some((prefix, _)) = normalized.split_once(&word).filter(|_| !word.is_empty())
                else {
                    return false;
                };
                let mut prefix = prefix.to_string();
                for particle in ["麻烦", "帮我", "请", "你", "先"] {
                    prefix = prefix.replace(particle, "");
                }
                prefix.is_empty()
            });
            if !has_command_prefix {
                return false;
            }
        }
        let matches_control = pause_match(said)
            || continue_match(said, &continue_words)
            || stop_remainder(said, &stop_words).is_some();
        if matches_control {
            if let Some(debit) = debit {
                if *debit {
                    let Some(expected) = identity.as_deref_mut() else {
                        return true;
                    };
                    if !self.debit_control(device, expected) {
                        return true;
                    }
                    *debit = false;
                }
            }
        }
        let (voice_live, barged, recent_barge_ms) = {
            let hub_voice = self.hub_live(device).voice;
            let states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get(device)
                .map(|st| {
                    let live = |o: &OutputRecord| {
                        !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                    };
                    let v =
                        hub_voice || st.outputs.values().any(|o| o.channel == "voice" && live(o));
                    let b = st.last_barge_in_ms > 0
                        && now.saturating_sub(st.last_barge_in_ms) < CUE_ECHO_WINDOW_MS;
                    (v, b, st.last_barge_in_ms)
                })
                .unwrap_or((hub_voice, false, 0))
        };
        // R1e: 暂停 is its own intent (RUI-local separates chat interrupt,
        // application pause and session end). It NEVER reaches the model: the
        // 2026-11-12 device log shows 「暂停。」 transcribed and answered as a
        // question, which is exactly the "暂停没接上" the user reported.
        if pause_match(said) {
            // R1j/P1: the spotter's pause already ran for this speech.
            if self.reflex_deduped(device, interaction::KeywordIntent::Pause, via) {
                return true;
            }
            // M3: media first (foreground), then the program. One word, one
            // meaning; the target is what the person is hearing.
            let was_live = self.media_state(device)
                == Some(crate::tool::music::MediaState::Playing)
                || crate::lock_or_recover(&self.states, "voice.states")
                    .get(device)
                    .is_some_and(|st| st.live_program.is_some());
            let target = self.pause_current(device, "pause_intent", identity.as_deref().copied());
            let already_paused = !was_live && target.is_some();
            if already_paused {
                self.close_window_preserving_paused_media(device, now, identity);
            }
            self.note(
                "pause_intent",
                json!({ "device": device, "text": text, "said": said, "via": via,
                        "target": target, "paused": target.is_some() || already_paused,
                        "already_paused": already_paused }),
            );
            return true;
        }
        // R1e: 继续 with a paused program resumes it from its bookmark. Only
        // this explicit word consumes the bookmark — an interjected question
        // leaves it untouched (RUI-local: 聊天插问不自动恢复).
        if continue_match(said, &continue_words) {
            let can_resume = self.paused_summary(device).is_some()
                || self.media_state(device) == Some(crate::tool::music::MediaState::Paused);
            if can_resume {
                if self.reflex_deduped(device, interaction::KeywordIntent::Continue, via) {
                    return true;
                }
                let target =
                    self.resume_current(device, "continue_intent", identity.as_deref().copied());
                self.note(
                    "program_resumed",
                    json!({ "device": device, "text": text, "said": said, "via": via,
                            "target": target, "resumed": target.is_some() }),
                );
                return true;
            }
            let session = match crate::session::SessionStore::new(self.store.clone())
                .device_session_id(device)
            {
                Ok(session) => session,
                Err(error) => {
                    self.note(
                        "session_scope_error",
                        json!({"device":device,"error":error}),
                    );
                    return true;
                }
            };
            if self.tasks.has_pending_cross_turn(&session) {
                // M6: no paused program or media, but a cross-turn task is
                // awaiting continue — let the agent turn handle 「继续」.
                return false;
            }
        }
        if continue_match(said, &continue_words) {
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                st.continue_intents += 1;
            }
            self.note(
                "continue_intent",
                json!({ "device": device, "text": text, "said": said, "via": via,
                        "voice_live": voice_live, "barged": barged,
                        "last_barge_in_ms": recent_barge_ms,
                        "resumed": false,
                        "reason": "no paused program or media to resume; passing to model" }),
            );
            return false;
        }
        if let Some(rest) = stop_remainder(said, &stop_words) {
            // R1e: right after an accepted barge-in the guard has eaten the
            // first syllable, so 「暂停」 and 「停」 are the SAME transcript.
            // The audio already stopped at the barge; the only open question
            // is whether 继续 can pick it up. A bare stop word there keeps
            // the bookmark (losing a 30 s story is the worse error); an
            // explicit 停 outside that window still discards it.
            let ambiguous = barged && !remainder_is_content(&rest);
            if ambiguous {
                let target =
                    self.pause_current(device, "stop_after_barge", identity.as_deref().copied());
                self.note(
                    "stop_intent_after_barge",
                    json!({ "device": device, "text": text, "said": said, "via": via,
                            "paused": target.is_some(), "target": target,
                            "why": "bare stop word right after a barge-in: the first syllable is the guard's, so 暂停 and 停 are indistinguishable" }),
                );
                return true;
            }
            let stop = if voice_live {
                if let Some(identity) = identity {
                    let mut states = crate::lock_or_recover(&self.states, "voice.states");
                    let Some(st) = states.get_mut(device) else {
                        return true;
                    };
                    if st.participation.identity() != *identity {
                        return true;
                    }
                    let stop = self.stop_device_locked(device, st, true);
                    *identity = st.participation.identity();
                    stop
                } else {
                    self.stop_device(device)
                }
            } else {
                json!({ "stop_requests_sent": 0, "note": "nothing was playing" })
            };
            // "停，换个短的" is a stop AND a new request: the stop happens here,
            // the turn still runs so the remainder is actually answered.
            let further = remainder_is_content(&rest);
            // R1e: 停 is a discard, not a pause (RUI-local: 普通旧回答丢弃).
            if let Some(st) = crate::lock_or_recover(&self.states, "voice.states").get_mut(device) {
                st.paused_program = None;
                st.live_program = None;
            }
            match crate::session::SessionStore::new(self.store.clone()).device_session_id(device) {
                Ok(session) => {
                    self.tasks.cancel_cross_turn(&session);
                }
                Err(error) => self.note(
                    "session_scope_error",
                    json!({"device":device,"error":error}),
                ),
            };
            // M3: the same word discards a media session (a paused song is not
            // left behind as a surprise 「继续」 target).
            let media_stopped = self.music().and_then(|m| m.stop_now(device, "stop_intent"));
            self.note(
                "stop_intent",
                json!({ "device": device, "text": text, "said": said, "via": via,
                        "voice_live": voice_live, "barged": barged,
                        "remainder": rest, "further_request": further, "stop": stop,
                        "media_stopped": media_stopped }),
            );
            return !further;
        }
        false
    }

    /// The cue this turn would play, if any (B-M0). Selection happens once,
    /// from the transcript, BEFORE the model runs — the same input A uses.
    /// Not ready (no WAV yet / voice changed) = no cue this turn, and a
    /// background self-heal so a later slow turn has one.
    pub(super) fn pick_filler(
        self: &Arc<Self>,
        device: &str,
        text: &str,
    ) -> Option<(String, String)> {
        if !self.hub.device_supports(device, "audio_out") {
            return None;
        }
        // A bare wake knocking on the door is not a request to wait for.
        if is_greeting_only(text) {
            return None;
        }
        // Settings → states is the module's lock order; never nest these two
        // the other way round (note_voice_play_submitted does the same).
        let filler = crate::lock_or_recover(&self.settings, "voice.settings")
            .filler
            .clone();
        let seq = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            match states.get_mut(device) {
                Some(st) => {
                    // 0-based per device: the first turn reads phrase 0.
                    let seq = st.filler_turn_seq;
                    st.filler_turn_seq = st.filler_turn_seq.wrapping_add(1);
                    seq
                }
                None => return None,
            }
        };
        let (scene, phrase) = filler.pick(text, seq)?;
        let delay_ms = filler.delay_ms;
        let manifest = self.load_ack_manifest();
        if !self.ack_ready(&phrase, &manifest) {
            // The cue cache is per-voice; missing here means "not synthesized
            // yet". Build it off the interaction path (never awaited).
            let pipe = self.clone();
            let cue = phrase.clone();
            tokio::spawn(async move { pipe.synthesize_wake_acks(false).await });
            tracing::info!(cue = %cue, "waiting-cue cache missing; synthesizing in background");
            return None;
        }
        // Delay rides with the selection so the caller has one deadline.
        self.note(
            "filler_armed",
            json!({ "device": device, "scene": scene, "phrase": phrase, "delay_ms": delay_ms }),
        );
        Some((scene.to_string(), phrase))
    }

    /// Is any NON-cue voice audio live on this device? Used by the cue timer:
    /// once the answer itself is sounding, the cue has nothing left to cover.
    pub(super) fn answer_audio_started(&self, device: &str) -> bool {
        let states = crate::lock_or_recover(&self.states, "voice.states");
        let Some(st) = states.get(device) else {
            return false;
        };
        let filler_id = st.filler_play.as_ref().map(|f| f.play_id.as_str());
        let live =
            |o: &OutputRecord| !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping");
        st.outputs
            .iter()
            .any(|(id, o)| o.channel == "voice" && Some(id.as_str()) != filler_id && live(o))
    }

    /// Play one waiting cue from the device-local cache. Keeps the play id
    /// so the answer's first audio can replace it; deliberately does NOT arm
    /// the answer-protection period (a cached WAV has no TTS onset
    /// transient, and the cue must stay interruptible).
    pub(super) fn play_filler(
        &self,
        device: &str,
        scene: &str,
        phrase: &str,
        task: &Option<String>,
        request: Option<RequestTicket>,
    ) -> bool {
        // R1g/P4: in `interrupt` mode the cue only ever covers SILENCE, so it
        // must not start on top of a live play (a song that started while the
        // model was thinking). In `radio` mode it may: that is the DJ talking
        // over the music.
        let Some(permit) = self.output_permit(device, request) else {
            return false;
        };
        let mode = crate::lock_or_recover(&self.settings, "voice.settings")
            .filler
            .mode;
        let content = self.active_content(device);
        if !cue_may_start(mode, content) {
            self.note(
                "filler_skipped",
                json!({ "device": device, "scene": scene, "phrase": phrase,
                        "skipped": "other_play_live", "mode": mode,
                        "content": { "voice": content.voice, "media": content.media } }),
            );
            return false;
        }
        let manifest = self.load_ack_manifest();
        let Some(file) = manifest.files.get(phrase) else {
            self.note("filler_skipped", json!({ "device": device, "scene": scene, "phrase": phrase, "skipped": "no manifest entry" }));
            return false;
        };
        let wav = match std::fs::read(self.wake_ack_dir().join(file)) {
            Ok(w) => w,
            Err(e) => {
                self.note("filler_skipped", json!({ "device": device, "scene": scene, "phrase": phrase, "skipped": format!("read: {e}") }));
                return false;
            }
        };
        let req = PlayRequest {
            device: device.to_string(),
            content_ref: format!("filler:{phrase}"),
            audio: Some(wav),
            audio_meta: crate::gateway::AudioMeta {
                format: "wav".into(),
                sample_rate: 16000,
                channels: 1,
            },
            channel: "voice".into(),
            text: None,
            simulated_ms: None,
        };
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        if !states.get(device).is_some_and(|st| permit.valid(st)) {
            return false;
        }
        let ticket = match self.hub.play(req) {
            Ok(t) => t,
            Err(e) => {
                self.note("filler_skipped", json!({ "device": device, "scene": scene, "phrase": phrase, "skipped": format!("play: {e}") }));
                return false;
            }
        };
        let play_id = ticket.play_id.clone();
        // The terminal receipt arrives through the event pump; the ticket's
        // waiter is not parked on (same as play_local_ack).
        let now = id::now_ms();
        {
            if let Some(st) = states.get_mut(device) {
                st.outputs.insert(
                    play_id.clone(),
                    OutputRecord {
                        answer_text: String::new(),
                        request: None,
                        task_id: task.clone(),
                        channel: "voice".into(),
                        kind: "filler".into(),
                        content_ref: format!("filler:{phrase}"),
                        stage: "submitted".into(),
                        started_ms: now,
                        stage_ms: now,
                        end_sent_ms: None,
                        expected_ms: 0,
                        synth_ms: 0,
                    },
                );
                st.filler_play = Some(FillerPlay {
                    play_id: play_id.clone(),
                    scene: scene.to_string(),
                    phrase: phrase.to_string(),
                });
                // B-M1: the cue is also our own voice — its tail can land in
                // the next mic segment just like a wake ack's.
                st.last_cue = Some((phrase.to_string(), now));
            }
            // #2 (2026-11-16): and it is a HOUSE cue, like the acks.
            self.note_house_cue(device, phrase);
        }
        drop(states);
        self.note(
            "filler_played",
            json!({ "device": device, "scene": scene, "phrase": phrase, "play_id": play_id, "task": task }),
        );
        self.push_assistant_state(device, false);
        true
    }

    /// R1g/P4: another play started while a cue may be live. The MODE is the
    /// only policy here:
    /// - `interrupt`: the cue only covers silence, so ANY other play ends it;
    /// - `radio` (default): the cue is a radio DJ — a MUSIC start does not cut
    ///   it. The answer's own voice still replaces it in the normal order
    ///   (one owner per channel), which `speak*` does by itself.
    ///
    /// `play_id` is the play that started; the cue's own receipts arrive here
    /// too and must not cancel it.
    pub(super) fn filler_yields_to_play(&self, play_id: &str) {
        if crate::lock_or_recover(&self.settings, "voice.settings")
            .filler
            .mode
            != FillerMode::Interrupt
        {
            return;
        }
        let accepted = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            if states.values().any(|st| {
                st.filler_play
                    .as_ref()
                    .is_some_and(|f| f.play_id == play_id)
            }) {
                return;
            }
            if !self.hub.play_accepts_output(play_id) {
                return;
            }
            let Some((device, channel)) = self.hub.play_info(play_id) else {
                return;
            };
            let Some(st) = states.get(&device) else {
                return;
            };
            (
                device,
                channel,
                OutputPermit {
                    generation: st.participation.generation(),
                    stop_epoch: st.stop_epoch,
                    request: None,
                },
            )
        };
        self.cancel_filler_tracked_at(
            &accepted.0,
            &format!("other_play_started:{}", accepted.1),
            Some(accepted.2),
        );
    }

    /// R2 (2026-11-13): let the cue FINISH before the answer's first audio
    /// when it is nearly over. The user heard the answer cut "我想想。" at
    /// 597 of 775 ms: the cue was stopped the instant the first sentence was
    /// synthesized. Waiting for the natural `play_finished` (bounded — a
    /// stuck device must not hold the answer) keeps the two separated; if the
    /// cue is longer than the bound it is stopped exactly as before.
    pub(super) async fn wait_filler_finish(&self, device: &str, bound_ms: u64) -> bool {
        let started = id::now_ms();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(bound_ms);
        loop {
            let live = crate::lock_or_recover(&self.states, "voice.states")
                .get(device)
                .map(|st| st.filler_play.is_some())
                .unwrap_or(false);
            if !live {
                let waited = id::now_ms().saturating_sub(started);
                if waited > 0 {
                    self.note(
                        "filler_finished_before_answer",
                        json!({
                            "device": device, "waited_ms": waited,
                        }),
                    );
                }
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// R1j/P0-a: the cue's cancellation plus a receipt hook. The answer's
    /// first audio must not be submitted until the device has flushed the
    /// cue (measured overlap: the stop took 34–158 ms and each play flushes
    /// only itself, so "我看看" ran under "……灯已经关好了"). The second
    /// return value resolves when the flush is a hardware fact; None means
    /// there was nothing live to flush.
    pub(super) fn cancel_filler_tracked_at(
        &self,
        device: &str,
        reason: &str,
        permit: Option<OutputPermit>,
    ) -> (bool, Option<crate::gateway::StopSubmission>) {
        let (fp, was_live) = {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let Some(st) = states.get_mut(device) else {
                return (false, None);
            };
            if permit.is_some_and(|p| !p.valid(st)) {
                return (false, None);
            }
            let Some(fp) = st.filler_play.take() else {
                return (false, None);
            };
            let mut was_live = true;
            if let Some(rec) = st.outputs.get_mut(&fp.play_id) {
                // A stop already in flight (stop_device marked every record
                // `stopping`) needs no second frame; the cue still counts as
                // cancelled for the log and for the answer's replace path.
                was_live = rec.stage != "stopping";
                rec.stage = "stopping".into();
            }
            (fp, was_live)
        };
        let wait = if was_live {
            Some(self.hub.stop_and_wait(&fp.play_id))
        } else {
            None
        };
        let sent = wait
            .as_ref()
            .is_some_and(crate::gateway::StopSubmission::accepted);
        self.note(
            "filler_cancelled",
            json!({ "device": device, "play_id": fp.play_id, "scene": fp.scene,
                    "phrase": fp.phrase, "reason": reason, "stop_sent": sent,
                    "already_stopping": !was_live }),
        );
        self.push_assistant_state(device, false);
        (sent, wait)
    }

    /// R1j/P0-a: wait (bounded) until the cancelled cue is off the channel.
    /// The device confirms a stop only AFTER its flush, so the receipt IS
    /// the "silent now" fact. A missing receipt is reported, never hidden —
    /// and the answer starts anyway (a late cue is better than a mute turn).
    pub(super) async fn settle_filler(
        &self,
        device: &str,
        reason: &str,
        wait: Option<crate::gateway::StopSubmission>,
    ) {
        let Some(wait) = wait else { return };
        let started = id::now_ms();
        let outcome = wait
            .settle(std::time::Duration::from_millis(FILLER_STOP_WAIT_MS), None)
            .await;
        let settled = outcome.confirmed();
        let detail = match &outcome {
            crate::gateway::StopOutcome::Confirmed { .. } => "flushed",
            crate::gateway::StopOutcome::Failed { reason }
            | crate::gateway::StopOutcome::Unknown { reason } => reason.as_str(),
        };
        self.note(
            "filler_stop_settled",
            json!({ "device": device, "reason": reason, "settled": settled,
                    "detail": detail, "stop_outcome": outcome, "waited_ms": id::now_ms().saturating_sub(started),
                    "bound_ms": FILLER_STOP_WAIT_MS }),
        );
    }

    /// Synthesize missing/stale cue WAVs (wake acks, retreat/window-close acks, waiting
    /// cues) with the configured TTS. Cloud work belongs to the
    /// OFFLINE-of-interaction path: it runs on save/re-sync and as a
    /// best-effort self-heal from a turn that found a cue missing, so no
    /// interaction ever waits on it. `force` re-synthesizes all.
    pub async fn synthesize_wake_acks(&self, force: bool) {
        let phrases = {
            let s = crate::lock_or_recover(&self.settings, "voice.settings");
            let mut all = s.wake_ack.phrases.clone();
            for ov in s.device_overrides.values() {
                if let Some(ack) = &ov.wake_ack {
                    all.extend(ack.phrases.iter().cloned());
                }
            }
            all.push(s.retreat_ack.clone());
            all.push(s.window_close_ack.clone());
            if !s.truncation_notice.trim().is_empty() {
                all.push(s.truncation_notice.trim().to_string());
            }
            all.extend(s.filler.all_phrases());
            all
        };
        let identity = self.current_tts_identity();
        let dir = self.wake_ack_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut manifest = self.load_ack_manifest();
        let stale = manifest.voice != identity;
        if stale || manifest.files.is_empty() {
            manifest = AckManifest {
                voice: identity.clone(),
                model: String::new(),
                files: HashMap::new(),
            };
        }
        let tts = match (self.tts_factory)() {
            Ok(t) => t,
            Err(e) => {
                *crate::lock_or_recover(&self.last_cue_synth, "voice.last_cue_synth") =
                    Some(json!({
                        "at_ms": id::now_ms(),
                        "voice": identity,
                        "force": force,
                        "synthesized": 0,
                        "failed": 0,
                        "error": e,
                    }));
                self.note("wake_ack_synth", json!({ "error": e }));
                return;
            }
        };
        let mut done = 0usize;
        let mut failed = 0usize;
        for phrase in phrases {
            let have = manifest
                .files
                .get(&phrase)
                .is_some_and(|f| dir.join(f).is_file());
            if have && !force && !stale {
                continue;
            }
            match tts.synthesize(&phrase).await {
                Ok(audio) => {
                    let name = format!("{}.wav", crate::fwupdate::sha256_prefix(phrase.as_bytes()));
                    if std::fs::write(dir.join(&name), &audio.wav).is_ok() {
                        manifest.files.insert(phrase, name);
                        done += 1;
                    } else {
                        failed += 1;
                    }
                }
                Err(e) => {
                    failed += 1;
                    self.note(
                        "wake_ack_synth_failed",
                        json!({ "phrase": phrase, "error": format!("{e}") }),
                    );
                }
            }
        }
        if done > 0 || stale {
            // Two synthesizers can run at once (a console save racing a
            // turn-time self-heal). Writing our own map wholesale would drop
            // the other's entries and silently un-ready phrases whose WAVs
            // exist — merge instead, and only with a manifest of THIS voice
            // (another voice's files are stale by definition).
            let on_disk = self.load_ack_manifest();
            if on_disk.voice == identity {
                for (phrase, file) in on_disk.files {
                    manifest.files.entry(phrase).or_insert(file);
                }
            }
            manifest.voice = identity.clone();
            if let Ok(raw) = serde_json::to_string_pretty(&manifest) {
                let _ = std::fs::write(dir.join("manifest.json"), raw);
            }
        }
        *crate::lock_or_recover(&self.last_cue_synth, "voice.last_cue_synth") = Some(json!({
            "at_ms": id::now_ms(),
            "voice": identity,
            "force": force,
            "synthesized": done,
            "failed": failed,
        }));
        self.note(
            "wake_ack_synth",
            json!({ "synthesized": done, "force": force }),
        );
    }

    /// Compute the current presentation state for a device and send it when
    /// it differs from the last one actually on the wire (or when `force`).
    /// Fire-and-forget by design: a failed send (no gateway) leaves
    /// assistant_last untouched so the next opportunity resends it.
    /// Close follow windows whose inactivity ran out (R1g). The window is
    /// still DERIVED from the wall clock (`session_open`), but the session
    /// OBJECT must also stop claiming "listening" — the console and the
    /// matrix both read that label. Runs on the ticker; tests pass the
    pub(super) fn live_content(&self, device: &str) -> Content {
        let hub_live = self.hub_live(device);
        let states = crate::lock_or_recover(&self.states, "voice.states");
        match states.get(device) {
            Some(st) => {
                let live = |o: &OutputRecord| {
                    !matches!(o.stage.as_str(), "failed" | "unknown" | "stopping")
                };
                Content {
                    voice: hub_live.voice
                        || st.outputs.values().any(|o| o.channel == "voice" && live(o)),
                    media: hub_live.media
                        || st.outputs.values().any(|o| o.channel == "media" && live(o)),
                }
            }
            None => Content {
                voice: hub_live.voice,
                media: hub_live.media,
            },
        }
    }

    pub(super) fn bookmark_live_program_locked(
        st: &mut DeviceVoiceState,
        now: u64,
    ) -> (bool, serde_json::Value) {
        match st.live_program.take() {
            Some(p) if !Self::resumable_program(&p) => (false, json!(null)),
            Some(mut p) => {
                p.created_ms = now;
                p.system_cut = false;
                let summary = json!({ "cursor": p.cursor, "chars": p.text.len(), "complete": p.complete,
                    "next": p.text[Self::clamp_boundary(&p.text, p.cursor)..].chars().take(40).collect::<String>() });
                if st.paused_program.is_none() {
                    st.paused_program = Some(p);
                }
                (true, summary)
            }
            None => (false, json!(null)),
        }
    }

    /// Is this program real content (worth a bookmark on a take-over, worth
    /// a "it was cut short" notice) rather than an ordinary reply? An explicit
    /// `resumable` (a fault-restored reading, a fixture) always is; otherwise
    /// the program's own SIZE decides — larger than one short answer is
    /// content. This is the old "long or detailed" class, measured from what
    /// the model actually wrote instead of guessed from the transcript.
    ///
    /// A program the model is STILL WRITING has no size yet, so it cannot be
    /// dismissed as a short reply: a story interrupted two seconds in holds a
    /// few dozen characters and is exactly the thing 「继续」 exists for. A
    /// short answer finishes generating within its first second, so the
    /// window in which this keeps one is small (and it is then labelled as
    /// cut short, which is true).
    pub(super) fn resumable_program(p: &Program) -> bool {
        if p.resumable {
            return true;
        }
        p.short_chars > 0 && (!p.complete || p.text.chars().count() > p.short_chars)
    }

    /// The cap for one continuation: the program's own snapshot, or the live
    /// backstop for a program that predates the snapshot.
    pub(super) fn continuation_cap(
        program: &Program,
        settings: &SpokenDeliverySettings,
    ) -> SpokenCap {
        SpokenCap::new(program.cont.unwrap_or_else(|| settings.backstop_budget()))
    }

    /// The lead-in for a resumed system cut — once per program.
    pub(super) fn take_resume_lead(program: &mut Program, lead: &str) -> Option<String> {
        if !program.system_cut || program.lead_said || lead.is_empty() {
            return None;
        }
        program.lead_said = true;
        Some(lead.to_string())
    }

    /// Read-only snapshot of what `device` is currently playing, speaking, or
    /// holding paused. Ignoring `caller_task_id` prevents the turn that asked
    /// the question from mistaking its own state for pre-existing speech.
    pub fn playback_capabilities(
        &self,
        device: &str,
        caller_task_id: Option<&str>,
    ) -> PlaybackCapabilities {
        let now = id::now_ms();
        let media_session = self.music().and_then(|m| m.media_session(device));
        let (has_live_voice, has_paused_prog) = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get(device);
            let live_voice = st.is_some_and(|st| {
                let lp_live = st.live_program.as_ref().is_some_and(|lp| {
                    let same_task =
                        caller_task_id.is_some() && lp.origin_task.as_deref() == caller_task_id;
                    !same_task && !lp.text.trim().is_empty()
                });
                lp_live
                    || st.outputs.values().any(|r| {
                        let same_task =
                            caller_task_id.is_some() && r.task_id.as_deref() == caller_task_id;
                        r.channel == "voice"
                            && r.kind == "answer"
                            && stage_is_live(&r.stage)
                            && !same_task
                            && !r.answer_text.trim().is_empty()
                    })
            });
            let paused_prog = st.is_some_and(|st| {
                st.program_paused(now)
                    && st
                        .paused_program
                        .as_ref()
                        .is_some_and(|p| !p.text.trim().is_empty())
            });
            (live_voice, paused_prog)
        };

        let media_caps =
            |ms: &crate::tool::music::MediaSession, state: PlaybackState, playing: bool| {
                let can_next = ms.tracks.len() > 1
                    && (ms.index + 1 < ms.tracks.len()
                        || ms.repeat == crate::tool::music::RepeatMode::All);
                let can_previous = ms.tracks.len() > 1
                    && (ms.index > 0 || ms.repeat == crate::tool::music::RepeatMode::All);
                let can_repeat = ms.current().is_some_and(|t| {
                    matches!(
                        t.source,
                        crate::tool::music::Source::Library | crate::tool::music::Source::Online
                    )
                });
                PlaybackCapabilities {
                    state,
                    source: ms.current().map(|t| t.source.as_str().to_string()),
                    can_pause: playing,
                    can_resume: !playing,
                    can_stop: true,
                    can_next,
                    can_previous,
                    can_repeat,
                }
            };

        if let Some(ms) = media_session
            .as_ref()
            .filter(|s| s.state == crate::tool::music::MediaState::Playing)
        {
            return media_caps(ms, PlaybackState::Playing, true);
        }
        if has_live_voice {
            return PlaybackCapabilities {
                state: PlaybackState::Speaking,
                source: Some("voice".into()),
                can_pause: true,
                can_resume: false,
                can_stop: true,
                can_next: false,
                can_previous: false,
                can_repeat: false,
            };
        }
        let paused_media =
            media_session.filter(|s| s.state == crate::tool::music::MediaState::Paused);
        match (paused_media.as_ref(), has_paused_prog) {
            (Some(ms), true) => media_caps(ms, PlaybackState::PausedBoth, false),
            (Some(ms), false) => media_caps(ms, PlaybackState::PausedMedia, false),
            (None, true) => PlaybackCapabilities {
                state: PlaybackState::PausedProgram,
                source: Some("voice".into()),
                can_pause: false,
                can_resume: true,
                can_stop: true,
                can_next: false,
                can_previous: false,
                can_repeat: false,
            },
            (None, false) => PlaybackCapabilities {
                state: PlaybackState::Idle,
                source: None,
                can_pause: false,
                can_resume: false,
                can_stop: false,
                can_next: false,
                can_previous: false,
                can_repeat: false,
            },
        }
    }

    pub fn playback_reading(
        &self,
        device: &str,
        caller_task_id: Option<&str>,
    ) -> serde_json::Value {
        let now = id::now_ms();
        let caps = self.playback_capabilities(device, caller_task_id);
        let media_session = self.music().and_then(|m| m.media_session(device));
        let (live_voice_text, paused_prog_text) = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            let st = states.get(device);
            let live_voice = st.and_then(|st| {
                if let Some(lp) = st.live_program.as_ref() {
                    let same_task =
                        caller_task_id.is_some() && lp.origin_task.as_deref() == caller_task_id;
                    if !same_task && !lp.text.trim().is_empty() {
                        return Some(lp.text.clone());
                    }
                }
                st.outputs.values().find_map(|r| {
                    let same_task =
                        caller_task_id.is_some() && r.task_id.as_deref() == caller_task_id;
                    (r.channel == "voice"
                        && r.kind == "answer"
                        && stage_is_live(&r.stage)
                        && !same_task
                        && !r.answer_text.trim().is_empty())
                    .then(|| r.answer_text.clone())
                })
            });
            let paused_prog = st.and_then(|st| {
                st.program_paused(now)
                    .then(|| st.paused_program.as_ref().map(|p| p.text.clone()))
                    .flatten()
                    .filter(|t| !t.trim().is_empty())
            });
            (live_voice, paused_prog)
        };

        if let Some(ms) = media_session
            .as_ref()
            .filter(|s| s.state == crate::tool::music::MediaState::Playing)
        {
            let subject = format_media_subject(ms);
            return json!({
                "device": device,
                "state": "playing",
                "capabilities": caps,
                "media": media_summary(ms, now),
                "rendered_answer": format!("正在播放{subject}。"),
                "replace_answer": true,
            });
        }

        if let Some(text) = live_voice_text {
            let excerpt = program_excerpt(&text);
            let rendered = if excerpt.is_empty() {
                "正在播报语音内容。".to_string()
            } else {
                format!("正在播报：{excerpt}。")
            };
            return json!({
                "device": device,
                "state": "speaking",
                "capabilities": caps,
                "program_excerpt": excerpt,
                "rendered_answer": rendered,
                "replace_answer": true,
            });
        }

        let paused_media =
            media_session.filter(|s| s.state == crate::tool::music::MediaState::Paused);
        match (paused_media, paused_prog_text) {
            (Some(ms), Some(prog)) => {
                let subject = format_media_subject(&ms);
                let excerpt = program_excerpt(&prog);
                json!({
                    "device": device,
                    "state": "paused_both",
                    "capabilities": caps,
                    "media": media_summary(&ms, now),
                    "program_excerpt": excerpt,
                    "rendered_answer": format!("当前已暂停，保留了{subject}和一段播报内容。"),
                    "replace_answer": true,
                })
            }
            (Some(ms), None) => {
                let subject = format_media_subject(&ms);
                json!({
                    "device": device,
                    "state": "paused_media",
                    "capabilities": caps,
                    "media": media_summary(&ms, now),
                    "rendered_answer": format!("当前已暂停在{subject}，没有正在外放的声音。"),
                    "replace_answer": true,
                })
            }
            (None, Some(prog)) => {
                let excerpt = program_excerpt(&prog);
                let rendered = if excerpt.is_empty() {
                    "当前有一段已暂停的播报内容，没有正在外放的声音。".to_string()
                } else {
                    format!("当前播报已暂停在「{excerpt}」，没有正在外放的声音。")
                };
                json!({
                    "device": device,
                    "state": "paused_program",
                    "capabilities": caps,
                    "program_excerpt": excerpt,
                    "rendered_answer": rendered,
                    "replace_answer": true,
                })
            }
            (None, None) => json!({
                "device": device,
                "state": "idle",
                "capabilities": caps,
                "rendered_answer": "当前没有在播放任何内容。",
                "replace_answer": true,
            }),
        }
    }
}

/// Unified playback state across media (`Music` / `Story` / `Radio`) and
/// spoken `Program` (`voice`) (`#118` / `#223` Item D).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    Idle,
    Playing,
    Speaking,
    PausedMedia,
    PausedProgram,
    PausedBoth,
}

/// Unified transport and repeat capabilities for the active or paused playback
/// on a device (`#118` / `#223` Item D).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaybackCapabilities {
    pub state: PlaybackState,
    pub source: Option<String>,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_stop: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_repeat: bool,
}

fn media_summary(ms: &crate::tool::music::MediaSession, now: u64) -> serde_json::Value {
    let raw = ms.to_json_at(now);
    json!({
        "state": raw["state"],
        "title": raw["title"],
        "artist": raw["artist"],
        "source": raw["source"],
        "position_ms": raw["position_ms"],
        "queue_len": raw["tracks"],
    })
}

fn format_media_subject(ms: &crate::tool::music::MediaSession) -> String {
    use crate::tool::music::Source;
    let Some(track) = ms.current() else {
        return "媒体内容".to_string();
    };
    let title = track.title.trim();
    let artist = track.artist.trim();
    match track.source {
        Source::Radio => {
            if title.is_empty() {
                "电台节目".to_string()
            } else {
                format!("电台「{title}」")
            }
        }
        Source::Story => {
            if title.is_empty() {
                "有声故事".to_string()
            } else {
                format!("故事《{title}》")
            }
        }
        Source::Library | Source::Online => {
            if !artist.is_empty() && !title.is_empty() {
                format!("{artist}的《{title}》")
            } else if !title.is_empty() {
                format!("《{title}》")
            } else {
                "音乐".to_string()
            }
        }
    }
}

fn program_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let first_clause = trimmed
        .split(['。', '！', '？', '\n', '；'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(trimmed)
        .trim();
    let chars: Vec<char> = first_clause.chars().collect();
    if chars.len() <= 22 {
        first_clause.to_string()
    } else {
        format!("{}…", chars[..22].iter().collect::<String>().trim_end())
    }
}

#[cfg(test)]
mod playback_reading_tests {
    use super::super::{OutputRecord, Program};
    use super::*;
    use crate::gateway::DeviceHub;
    use crate::store::Store;
    use crate::task::TaskRegistry;
    use crate::tool::music::{MediaSession, MediaState, MusicRuntime, Source, Track};
    use std::sync::Arc;

    fn test_pipeline() -> (Arc<VoicePipeline>, Arc<MusicRuntime>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let hub = DeviceHub::new();
        let tasks = Arc::new(TaskRegistry::new(store.clone()));
        let music = MusicRuntime::new(store.clone(), hub.clone());
        let env_factory: crate::voice::EnvFactory = Arc::new(|| Err("no env".into()));
        let tts_factory: crate::voice::TtsFactory = Arc::new(|| Err("no tts".into()));
        let asr_factory: crate::voice::AsrFactory = Arc::new(|| Err("no asr".into()));
        let pipe = VoicePipeline::new(
            hub,
            store,
            tasks,
            env_factory,
            tts_factory,
            asr_factory,
            std::env::temp_dir(),
        );
        pipe.attach_music(&music);
        (pipe, music)
    }

    #[test]
    fn playback_reading_covers_idle_playing_speaking_and_paused_states() {
        let (pipe, music) = test_pipeline();
        let dev = "dot2-d";

        let idle = pipe.playback_reading(dev, Some("task-1"));
        assert_eq!(idle["state"], json!("idle"));
        assert_eq!(idle["rendered_answer"], json!("当前没有在播放任何内容。"));
        assert_eq!(idle["replace_answer"], json!(true));
        assert!(idle.get("status").is_none());
        assert!(idle.get("tracks").is_none());

        let now = id::now_ms();
        music.seed_media_session_for_test(
            dev,
            MediaSession {
                tracks: vec![Track {
                    source: Source::Library,
                    id: "t1".into(),
                    title: "晴天".into(),
                    artist: "周杰伦".into(),
                    binding: "lib".into(),
                }],
                index: 0,
                state: MediaState::Playing,
                position_ms: 8_000,
                play_id: Some("p1".into()),
                started_ms: now,
                base_ms: 0,
                updated_ms: now,
                repeat: crate::tool::music::RepeatMode::Off,
            },
        );
        let playing = pipe.playback_reading(dev, Some("task-1"));
        assert_eq!(playing["state"], json!("playing"));
        assert_eq!(
            playing["rendered_answer"],
            json!("正在播放周杰伦的《晴天》。")
        );

        music.seed_media_session_for_test(
            dev,
            MediaSession {
                tracks: vec![Track {
                    source: Source::Radio,
                    id: "r1".into(),
                    title: "中国之声".into(),
                    artist: String::new(),
                    binding: "radio|http://127.0.0.1".into(),
                }],
                index: 0,
                state: MediaState::Paused,
                position_ms: 0,
                play_id: None,
                started_ms: now,
                base_ms: 0,
                updated_ms: now,
                repeat: crate::tool::music::RepeatMode::Off,
            },
        );
        let paused_media = pipe.playback_reading(dev, Some("task-1"));
        assert_eq!(paused_media["state"], json!("paused_media"));
        assert_eq!(
            paused_media["rendered_answer"],
            json!("当前已暂停在电台「中国之声」，没有正在外放的声音。")
        );

        {
            let mut states = pipe.states.lock().unwrap();
            let st = states.entry(dev.into()).or_default();
            st.paused_program = Some(Program {
                text: "今天的新闻头条是本地语音升级。".into(),
                created_ms: now,
                resumable: true,
                ..Program::default()
            });
            st.outputs.insert(
                "self-out".into(),
                OutputRecord {
                    answer_text: "自身回答不应被当作背景播报".into(),
                    request: None,
                    task_id: Some("task-self".into()),
                    channel: "voice".into(),
                    kind: "answer".into(),
                    content_ref: "tts".into(),
                    stage: "accepted".into(),
                    started_ms: now,
                    stage_ms: now,
                    end_sent_ms: None,
                    expected_ms: 0,
                    synth_ms: 0,
                },
            );
        }
        let paused_both = pipe.playback_reading(dev, Some("task-self"));
        assert_eq!(paused_both["state"], json!("paused_both"));
        assert_eq!(paused_both["capabilities"]["state"], json!("paused_both"));
        assert_eq!(paused_both["capabilities"]["can_resume"], json!(true));
        assert_eq!(paused_both["capabilities"]["can_pause"], json!(false));

        let speaking = pipe.playback_reading(dev, Some("task-other"));
        assert_eq!(speaking["state"], json!("speaking"));
        assert_eq!(speaking["capabilities"]["state"], json!("speaking"));
        assert_eq!(speaking["capabilities"]["source"], json!("voice"));
        assert_eq!(speaking["capabilities"]["can_pause"], json!(true));
        assert_eq!(speaking["capabilities"]["can_repeat"], json!(false));

        let mode_snap = pipe.mode_snapshot();
        let dev_snap = mode_snap["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == dev)
            .unwrap();
        assert_eq!(dev_snap["playback"]["state"], json!("speaking"));
        assert_eq!(dev_snap["playback"]["can_pause"], json!(true));
    }
}
