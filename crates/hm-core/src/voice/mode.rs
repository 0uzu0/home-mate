use super::*;

impl VoicePipeline {
    // --- management operations (page + future voice share these) ---------------

    /// The persisted duplex request for a device (default "half").
    pub fn requested_duplex(&self, device: &str) -> String {
        crate::lock_or_recover(&self.settings, "voice.settings")
            .duplex_requested
            .get(device)
            .cloned()
            .unwrap_or_else(|| "half".into())
    }

    /// ACTIVE diagnostic (audible!): synthesize one short phrase through the
    /// configured TTS and play it on the connected audio_out device, then
    /// report the receipt stage with timings. Exercises the whole
    /// TTS → hub → gateway → device chain with real evidence — no staging
    /// invented. Fails honestly when TTS or the device is missing.
    pub async fn diagnostic_voice_play(&self) -> serde_json::Value {
        let Some(device) = self.hub.primary_audio_out().map(|d| d.stable_id) else {
            return json!({ "ok": false, "error": "没有已连接的 audio_out 设备；网关未连接或无扬声器能力" });
        };
        let tts = match (self.tts_factory)() {
            Ok(t) => t,
            Err(e) => return json!({ "ok": false, "error": format!("TTS 不可用：{e}") }),
        };
        let started = id::now_ms();
        let audio = match tts.synthesize("语音链路测试").await {
            Ok(a) => a,
            Err(e) => return json!({ "ok": false, "error": format!("合成失败：{e}") }),
        };
        let synth_ms = id::now_ms() - started;
        let audio_meta = audio.meta();
        let req = PlayRequest {
            device: device.clone(),
            content_ref: format!("tts:diag:{}", id::next("V")),
            audio: Some(audio.wav),
            audio_meta,
            channel: "voice".into(),
            text: Some("语音链路测试".into()),
            simulated_ms: None,
        };
        let mut ticket = match self.hub.play(req) {
            Ok(t) => t,
            Err(e) => return json!({ "ok": false, "error": format!("提交失败：{e}") }),
        };
        // Terminal receipt with a bounded budget; missing confirmation is
        // reported as unknown — never success.
        let receipt =
            tokio::time::timeout(std::time::Duration::from_secs(4), &mut ticket.receipt).await;
        let stage = match receipt {
            Ok(Ok(crate::gateway::GatewayFrame::PlayReceipt { stage, .. })) => stage,
            Ok(Ok(_)) => "unknown".into(),
            Ok(Err(_)) => "unknown".into(),
            Err(_) => "unknown".into(),
        };
        let total_ms = id::now_ms() - started;
        let ok = stage == "device_confirmed";
        self.note(
            "diag_voice_play_test",
            json!({ "device": device, "stage": stage, "synth_ms": synth_ms, "total_ms": total_ms }),
        );
        json!({
            "ok": ok, "device": device, "stage": stage,
            "synth_ms": synth_ms, "total_ms": total_ms,
            "duration_ms": audio.duration_ms, "provider": audio.provider, "voice": audio.voice,
            "play_id": ticket.play_id,
        })
    }

    /// DIAGNOSTIC (G6 ERLE procedure, acceptance §0): play a FIXED audio
    /// asset — the archived 30s speech stimulus — through the real Play
    /// path so the device's dual-track capture hears exactly one known
    /// signal at a known level. Assets are bare file names under
    /// `<data_dir>/assets/`; no path traversal, no arbitrary reads. Same
    /// honest-stage semantics as the TTS probe: missing confirmation is
    /// reported as unknown, never as success.
    pub async fn diagnostic_play_asset(
        &self,
        assets_dir: &std::path::Path,
        asset: &str,
        device: Option<&str>,
        channel: &str,
        volume: Option<f64>,
    ) -> serde_json::Value {
        if asset.is_empty()
            || asset.contains('/')
            || asset.contains('\\')
            || asset.contains("..")
            || !asset.ends_with(".wav")
        {
            return json!({ "ok": false, "error": "asset 必须是 assets 目录下的 .wav 文件名（不含路径）" });
        }
        let device = match device.map(|d| d.to_string()) {
            Some(d) => d,
            None => match self.hub.primary_audio_out().map(|d| d.stable_id) {
                Some(d) => d,
                None => {
                    return json!({ "ok": false, "error": "没有已连接的 audio_out 设备；网关未连接或无扬声器能力" })
                }
            },
        };
        let path = assets_dir.join(asset);
        let wav = match std::fs::read(&path) {
            Ok(w) => w,
            Err(e) => return json!({ "ok": false, "error": format!("读取资产失败 {asset}：{e}") }),
        };
        // Minimal canonical-WAV sanity: RIFF/WAVE magic and a parseable
        // fmt block for the duration estimate (the device decodes fully).
        let rd = |off: usize| -> u32 {
            wav.get(off..off + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .unwrap_or(0)
        };
        if wav.len() < 44
            || &wav[0..4] != b"RIFF"
            || &wav[8..12] != b"WAVE"
            || &wav[12..16] != b"fmt "
            || rd(16) != 16
        {
            return json!({ "ok": false, "error": format!("{asset} 不是可识别的 WAV 文件") });
        }
        let sample_rate = rd(24);
        let byte_rate = rd(28).max(1);
        let duration_ms = ((wav.len() as u64 - 44) * 1000) / byte_rate as u64;
        if let Some(v) = volume {
            match self.set_volume(&device, v) {
                Ok(_) => {}
                Err(e) => return json!({ "ok": false, "error": format!("音量设置失败：{e}") }),
            }
        }
        let started = id::now_ms();
        let req = PlayRequest {
            device: device.clone(),
            content_ref: format!("asset:{asset}"),
            audio: Some(wav),
            audio_meta: crate::gateway::AudioMeta {
                format: "wav".into(),
                sample_rate,
                channels: 1,
            },
            channel: channel.into(),
            text: None,
            simulated_ms: None,
        };
        let mut ticket = match self.hub.play(req) {
            Ok(t) => t,
            Err(e) => return json!({ "ok": false, "error": format!("提交失败：{e}") }),
        };
        // Real playback takes real time: budget the asset duration plus
        // slack, then report the stage that actually arrived.
        let budget = std::time::Duration::from_millis(duration_ms + 15_000);
        let receipt = tokio::time::timeout(budget, &mut ticket.receipt).await;
        let stage = match receipt {
            Ok(Ok(crate::gateway::GatewayFrame::PlayReceipt { stage, .. })) => stage,
            Ok(Ok(_)) => "unknown".into(),
            Ok(Err(_)) => "unknown".into(),
            Err(_) => "unknown".into(),
        };
        let total_ms = id::now_ms() - started;
        let ok = stage == "device_confirmed";
        self.note(
            "diag_play_asset",
            json!({ "device": device, "asset": asset, "channel": channel, "volume": volume,
                    "stage": stage, "duration_ms": duration_ms, "total_ms": total_ms }),
        );
        json!({
            "ok": ok, "device": device, "asset": asset, "stage": stage,
            "channel": channel, "duration_ms": duration_ms, "total_ms": total_ms,
            "play_id": ticket.play_id,
        })
    }

    /// Task B (2026-11-10): ch8 playback-loopback self-check. Plays a known
    /// broadband asset through the real media path (firmware must run with
    /// `--loopback-probe`), then compares the DAC-side track the firmware
    /// actually wrote with the ch8 capture track. Reports correlation, lag,
    /// sample-exact run and silent/clipped stretches; a silent capture is
    /// reported as such — this is also how the G6 "ch8 cross-boot silent"
    /// observation stops being a rumour.
    /// Probe tracks are armed per diagnostic play: the firmware only opens
    /// its /run tracks when this file exists (zero steady-state I/O — the
    /// always-on probe measurably dipped chunks/s, 2026-11-10).
    pub(super) fn loopback_dir() -> String {
        std::env::var("HM_LOOP_DIR").unwrap_or_else(|_| "/run/hm-loop".into())
    }

    pub(super) fn arm_loopback_probe(armed: bool) {
        let dir = Self::loopback_dir();
        let path = format!("{dir}/arm");
        if armed {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, b"");
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    pub(super) fn arm_loopback_probe_guard() -> LoopbackProbeGuard {
        Self::arm_loopback_probe(true);
        LoopbackProbeGuard
    }

    pub async fn diagnostic_playback_loopback(
        &self,
        assets_dir: &std::path::Path,
        device: Option<&str>,
        asset: Option<&str>,
    ) -> serde_json::Value {
        let asset = asset.unwrap_or("loopback-probe.wav");
        let _probe_guard = Self::arm_loopback_probe_guard();
        let play = self
            .diagnostic_play_asset(assets_dir, asset, device, "media", None)
            .await;
        if play.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return json!({
                "ok": false,
                "error": "播放未确认（无设备/未确认/资产缺失）；自检不猜结果",
                "play": play,
            });
        }
        // The tracks are written while the play runs; the last ch8 block
        // lands within one 80 ms capture block of the EOS.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let dir = std::env::var("HM_LOOP_DIR").unwrap_or_else(|_| "/run/hm-loop".into());
        let read = |name: &str| -> Result<Vec<i16>, String> {
            let bytes =
                std::fs::read(format!("{dir}/{name}")).map_err(|e| format!("{dir}/{name}: {e}"))?;
            Ok(bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect())
        };
        drop(_probe_guard);
        let (write, capture) = match (read("write.pcm"), read("capture.pcm")) {
            (Ok(w), Ok(c)) => (w, c),
            (Err(e), _) | (_, Err(e)) => {
                return json!({
                    "ok": false,
                    "error": format!("读不到探针轨：{e}（固件需以 --loopback-probe 运行）"),
                    "play": play,
                })
            }
        };
        let report = crate::loopback::analyze(&write, &capture);
        let ok = matches!(
            report.verdict.as_str(),
            "loopback_intact" | "loopback_transformed" | "weak_relationship"
        );
        self.note(
            "diag_playback_loopback",
            json!({ "device": play.get("device"), "verdict": report.verdict,
                    "lag_samples": report.lag_samples, "correlation": report.correlation,
                    "exact_run": report.exact_run }),
        );
        json!({ "ok": ok, "play": play, "loopback": report })
    }

    /// Task C (2026-11-10): 7-mic array measurement. Plays the probe asset
    /// through the real media path (firmware probe mode dumps 9ch), then
    /// reports per-channel level/floor/SNR and arrival-time differences vs
    /// ch0. Measurement only — no beamforming/DOA is implemented.
    pub async fn diagnostic_mic_array(
        &self,
        assets_dir: &std::path::Path,
        device: Option<&str>,
        asset: Option<&str>,
    ) -> serde_json::Value {
        let asset = asset.unwrap_or("loopback-probe.wav");
        let _probe_guard = Self::arm_loopback_probe_guard();
        let play = self
            .diagnostic_play_asset(assets_dir, asset, device, "media", None)
            .await;
        if play.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return json!({ "ok": false, "error": "播放未确认；阵列测量不做", "play": play });
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let dir = std::env::var("HM_LOOP_DIR").unwrap_or_else(|_| "/run/hm-loop".into());
        drop(_probe_guard);
        let path = format!("{dir}/mics.pcm");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                return json!({
                    "ok": false,
                    "error": format!("读不到 {path}：{e}（固件需带 --loopback-probe）"),
                    "play": play,
                })
            }
        };
        let pcm: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let report = crate::mics::analyze(&pcm, 9);
        let ok = report.frames > 0;
        self.note(
            "diag_mic_array",
            json!({ "device": play.get("device"), "frames": report.frames,
                    "active_ms": report.active_ms }),
        );
        json!({ "ok": ok, "play": play, "mics": report })
    }

    /// REPAIR: re-send the persisted duplex request to a device (e.g. after
    /// a reconnect glitch where the mode_set raced the link). Config
    /// projection only — the negotiation fact arrives later as mode_report.
    pub fn repair_reapply_mode(&self, device: &str) -> Result<serde_json::Value, String> {
        let requested = self.requested_duplex(device);
        match self.hub.mode_set(device, &requested) {
            Ok(()) => {
                self.note(
                    "repair_reapply_mode",
                    json!({ "device": device, "requested": requested }),
                );
                Ok(json!({
                    "device": device, "requested": requested, "state": "submitted",
                    "note": "mode_set 已发送；协商与生效以设备 mode_report 为准",
                }))
            }
            Err(e) => Err(format!("无法发送 mode_set：{e}")),
        }
    }

    /// Stop everything on a device: cancel the running turn, stop all
    /// outputs. Returns the honest stage snapshot (stop requests submitted;
    /// confirmations arrive as receipts).
    pub fn stop_device(&self, device: &str) -> serde_json::Value {
        self.stop_device_inner(device, true)
    }

    /// R1d: the device already stopped this device's voice channel on its own
    /// near-end reflex. Same invalidation (epoch bump, task cancel, output
    /// records closed), but NOTHING goes south — a Stop behind the core's TTS
    /// queue is the 1.9 s the local reflex exists to remove, and there is no
    /// audio left to stop.
    pub fn stop_device_locally(&self, device: &str) -> serde_json::Value {
        self.stop_device_inner(device, false)
    }

    pub(super) fn stop_device_inner(&self, device: &str, ask_device: bool) -> serde_json::Value {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let st = states.entry(device.to_string()).or_default();
        self.stop_device_locked(device, st, ask_device)
    }

    /// Synchronous effects stay under states: a later activation cannot have
    /// its task or outputs removed by the tail of an earlier stop.
    pub(super) fn stop_device_locked(
        &self,
        device: &str,
        st: &mut DeviceVoiceState,
        ask_device: bool,
    ) -> serde_json::Value {
        let task = st.current_task.take();
        if let Some(cue) = st.filler_play.take() {
            self.note("filler_cancelled",json!({"device":device,"play_id":cue.play_id,"scene":cue.scene,"phrase":cue.phrase,"reason":"turn_stopped"}));
        }
        st.stop_epoch += 1;
        st.live_program = None;
        st.revoke_input("cancelled");
        for rec in st.outputs.values_mut() {
            rec.stage = "stopping".into();
        }
        st.participation.cancel_requests(id::now_ms());
        let cancelled = task
            .as_ref()
            .is_some_and(|t| self.tasks.request_cancel(t) == Some(true));
        let stopped = if ask_device {
            self.hub.stop_all_plays(device).len()
        } else {
            self.hub.forget_plays(device).len()
        };
        let answerable: Vec<String> = if ask_device {
            self.hub
                .active_plays_for(device)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        } else {
            Vec::new()
        };
        st.outputs.retain(|pid, _| answerable.contains(pid));
        json!({
            "device": device, "task_cancel_requested": cancelled, "task": task,
            "stop_requests_sent": stopped, "stop_sent_south": ask_device,
            "note": if !ask_device {
                "the device already stopped locally; registry cleared, nothing sent"
            } else if stopped == 0 && !cancelled { "nothing active on this device" }
            else { "stop requests submitted; confirmations recorded on receipt" },
        })
    }

    pub fn set_volume(&self, device: &str, value: f64) -> Result<serde_json::Value, String> {
        if !(0.0..=1.0).contains(&value) {
            return Err("value must be within 0..1 (linear)".into());
        }
        // This speaker's driver crackles above ~90 % (the vendor's own MBCL
        // note and our listening tests both put it there), so voice, console
        // and tools share one cap instead of each remembering it (user
        // decision 2026-11-14).
        let value = value.min(VOLUME_CAP);
        let play_id = self.hub.volume_set(device, value)?;
        self.note(
            "volume_set",
            json!({ "device": device, "value": value, "op_id": play_id, "cap": VOLUME_CAP }),
        );
        Ok(json!({ "device": device, "value": value, "state": "submitted" }))
    }

    /// Update mode settings. Duplex requests validate against the device's
    /// REPORTED capability; a downgrade is applied honestly (requested
    /// recorded, negotiated/effective stay what the link supports).
    /// Wake words and VAD thresholds are global voice settings edited on
    /// the same endpoint (they ride the same history/restore chain).
    /// Every change lands in the config history with the full new snapshot
    /// (restorable), attributed to `actor` (console / voice / restore).
    pub fn set_mode(
        self: &Arc<Self>,
        device: &str,
        duplex_requested: Option<String>,
        proactive_speech: Option<bool>,
        wake_words: Option<Vec<String>>,
        vad: Option<VadSettings>,
        follow_window_tone: Option<bool>,
        wake_ack: Option<WakeAckSettings>,
        retreat_intent: Option<bool>,
        retreat_words: Option<Vec<String>>,
        retreat_ack: Option<String>,
        tail_blind_ms: Option<u64>,
        media_trim_db: Option<f64>,
        keyword_interrupt: Option<bool>,
        barge_in: Option<bool>,
        barge_protect_ms: Option<u64>,
        filler: Option<FillerSettings>,
        stop_intents: Option<StopIntentSettings>,
        // R1h: the sentence spoken after a truncated program's last line.
        truncation_notice: Option<String>,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        self.set_mode_with_follow_window(
            device,
            duplex_requested,
            proactive_speech,
            wake_words,
            vad,
            follow_window_tone,
            wake_ack,
            retreat_intent,
            retreat_words,
            retreat_ack,
            tail_blind_ms,
            media_trim_db,
            keyword_interrupt,
            barge_in,
            barge_protect_ms,
            filler,
            stop_intents,
            truncation_notice,
            None,
            FollowWindowPatch::default(),
            actor,
        )
    }

    /// Shared prompt management also works without any audio device.
    pub fn set_prompts(
        self: &Arc<Self>,
        assistant_name: Option<String>,
        persona_prompt: Option<String>,
        system_prompt_override: Option<Option<String>>,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let patch = FollowWindowPatch {
            assistant_name,
            persona_prompt,
            system_prompt_override,
            ..Default::default()
        };
        self.set_mode_with_follow_window(
            "global", None, None, None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, patch, actor,
        )?;
        let settings = self.settings_snapshot();
        Ok(json!({
            "assistant_name": settings.assistant_name,
            "persona_prompt": settings.persona_prompt,
            "system_prompt_override": settings.system_prompt_override,
        }))
    }

    pub fn set_mode_with_follow_window(
        self: &Arc<Self>,
        device: &str,
        duplex_requested: Option<String>,
        proactive_speech: Option<bool>,
        wake_words: Option<Vec<String>>,
        vad: Option<VadSettings>,
        follow_window_tone: Option<bool>,
        wake_ack: Option<WakeAckSettings>,
        retreat_intent: Option<bool>,
        retreat_words: Option<Vec<String>>,
        retreat_ack: Option<String>,
        tail_blind_ms: Option<u64>,
        media_trim_db: Option<f64>,
        keyword_interrupt: Option<bool>,
        barge_in: Option<bool>,
        barge_protect_ms: Option<u64>,
        filler: Option<FillerSettings>,
        stop_intents: Option<StopIntentSettings>,
        // R1h: the sentence spoken after a truncated program's last line.
        truncation_notice: Option<String>,
        window_close_ack: Option<String>,
        follow_window: FollowWindowPatch,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
        follow_window.validate()?;
        let spoken = follow_window
            .spoken
            .map(SpokenDeliverySettings::normalized_for_console)
            .transpose()?;

        if let Some(req) = &duplex_requested {
            if !matches!(req.as_str(), "half" | "full") {
                return Err("duplex_requested must be \"half\" or \"full\"".into());
            }
        }
        if let Some(ms) = barge_protect_ms {
            if ms > 3000 {
                return Err("barge_protect_ms must be 0–3000".into());
            }
        }
        if let Some(db) = media_trim_db {
            if !(-12.0..=0.0).contains(&db) {
                return Err("media_trim_db must be within -12..=0".into());
            }
        }
        if let Some(ms) = tail_blind_ms {
            if ms > 2000 {
                return Err("tail_blind_ms must be 0–2000".into());
            }
        }
        if let Some(words) = &retreat_words {
            validate_retreat_words(words)?;
        }
        if let Some(ack) = &retreat_ack {
            validate_short_ack("retreat_ack", ack)?;
        }
        if let Some(ack) = &window_close_ack {
            validate_short_ack("window_close_ack", ack)?;
        }
        if let Some(words) = &wake_words {
            let cleaned: Vec<String> = words.iter().map(|w| w.trim().to_string()).collect();
            validate_wake_words(&cleaned)?;
        }
        if let Some(v) = &vad {
            validate_vad(v)?;
        }
        if let Some(ack) = &wake_ack {
            validate_wake_ack(ack)?;
        }
        if let Some(f) = &filler {
            f.validate()?;
        }
        if let Some(si) = &stop_intents {
            validate_word_list("stop_words", &si.stop_words, 12)?;
            validate_word_list("continue_words", &si.continue_words, 12)?;
        }
        if let Some(n) = &truncation_notice {
            validate_truncation_notice(n)?;
        }
        let capability = {
            let states = crate::lock_or_recover(&self.states, "voice.states");
            states
                .get(device)
                .and_then(|st| st.duplex.as_ref().map(|d| d.capability.clone()))
        };
        let mut reason = None;
        if let (Some(req), Some(cap)) = (&duplex_requested, &capability) {
            if req == "full" && cap != "full" {
                reason = Some(format!(
                    "device capability is '{cap}'; requested 'full' recorded but negotiation will stay '{cap}'"
                ));
            }
        }
        let mut summary = format!(
                "device {device}: duplex={duplex_requested:?} proactive={proactive_speech:?} wake_words={:?} retreat_words={:?} retreat_ack={retreat_ack:?} window_close_ack={window_close_ack:?} truncation_notice={truncation_notice:?} vad={:?} follow_window_tone={:?} tail_blind_ms={:?} keyword_interrupt={keyword_interrupt:?} barge_in={barge_in:?} barge_protect_ms={barge_protect_ms:?} filler_delay_ms={:?} filler_tool_delay_ms={:?}",
                wake_words.as_deref().unwrap_or(&[]),
                retreat_words.as_deref().unwrap_or(&[]),
                vad,
                follow_window_tone,
                tail_blind_ms,
                filler.as_ref().map(|f| f.delay_ms),
                filler.as_ref().map(|f| f.tool_delay_ms)
            );
        if let Some(sp) = &spoken {
            summary.push_str(&format!(
                " spoken short={}/{} detail={}/{} backstop={}/{}",
                sp.max_sentences,
                sp.max_chars,
                sp.detail_max_sentences,
                sp.detail_max_chars,
                sp.backstop_max_sentences,
                sp.backstop_max_chars
            ));
        }
        self.edit_settings_checked(actor, &summary, |s| {
            if let Some(req) = &duplex_requested {
                s.duplex_requested.insert(device.to_string(), req.clone());
            }
            if let Some(p) = proactive_speech {
                s.proactive_speech = p;
            }
            if let Some(words) = &wake_words {
                s.wake_words = words.iter().map(|w| w.trim().to_string()).collect();
            }
            if let Some(v) = vad {
                s.vad = Some(v);
            }
            if let Some(v) = follow_window.follow_window_enabled {
                s.follow_window_enabled = v;
            }
            if let Some(v) = follow_window.follow_window_ms {
                s.follow_window_ms = v;
            }
            if let Some(v) = follow_window.context_window_ms {
                s.context_window_ms = v;
            }
            if let Some(v) = follow_window.followup_credits {
                s.followup_credits = Some(v);
            }
            if let Some(v) = follow_window.kws_threshold {
                s.kws_threshold = v;
            }
            if let Some(v) = follow_window.kws_score {
                s.kws_score = v;
            }
            if let Some(name) = &follow_window.assistant_name {
                s.assistant_name = name.trim().to_string();
            }
            if let Some(text) = &follow_window.persona_prompt {
                s.persona_prompt = text.trim().to_string();
            }
            if let Some(text) = &follow_window.system_prompt_override {
                s.system_prompt_override = text
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            if let Some(spoken) = &spoken {
                s.spoken = spoken.clone();
            }
            if let Some(enabled) = follow_window.idle_asr_enabled {
                s.proactive_asr_wake = enabled;
                s.asr_wake_fallback = enabled;
            }
            if let Some(mode) = follow_window.wake_arbitration {
                s.wake_arbitration = mode;
            }
            if let Some(t) = follow_window_tone {
                s.follow_window_tone = t;
            }
            if let Some(ack) = wake_ack.clone() {
                s.wake_ack.phrases = ack.phrases.iter().map(|p| p.trim().to_string()).collect();
                s.wake_ack.enabled = ack.enabled;
            }
            if let Some(r) = retreat_intent {
                s.retreat_intent = r;
            }
            if let Some(words) = &retreat_words {
                s.retreat_words = words.iter().map(|w| w.trim().to_string()).collect();
            }
            if let Some(ack) = &retreat_ack {
                s.retreat_ack = ack.trim().to_string();
            }
            if let Some(ack) = &window_close_ack {
                s.window_close_ack = ack.trim().to_string();
            }
            if let Some(n) = &truncation_notice {
                s.truncation_notice = n.trim().to_string();
            }
            if let Some(ms) = tail_blind_ms {
                s.tail_blind_ms = ms;
                for (dev_id, aec_ms) in self.hub.device_aec_tails() {
                    if ms < u64::from(aec_ms) {
                        tracing::warn!(
                            device = %dev_id,
                            tail_blind_ms = ms,
                            aec_tail_ms = aec_ms,
                            "tail_blind_ms is shorter than device aec_tail_ms"
                        );
                    }
                }
            }
            if let Some(db) = media_trim_db {
                s.media_trim_db = db;
            }
            if let Some(k) = keyword_interrupt {
                s.keyword_interrupt = k;
            }
            if let Some(b) = barge_in {
                s.barge_in = b;
            }
            if let Some(ms) = barge_protect_ms {
                s.barge_protect_ms = ms;
            }
            if let Some(si) = &stop_intents {
                s.stop_intents = StopIntentSettings {
                    enabled: si.enabled,
                    stop_words: si.stop_words.iter().map(|w| w.trim().to_string()).collect(),
                    continue_words: si
                        .continue_words
                        .iter()
                        .map(|w| w.trim().to_string())
                        .collect(),
                };
            }
            if let Some(f) = &filler {
                // `resynth` is a command flag, never a persisted value.
                s.filler = FillerSettings {
                    delay_ms: f.delay_ms,
                    tool_delay_ms: f.tool_delay_ms,
                    mode: f.mode,
                    thinking: f.thinking.iter().map(|p| p.trim().to_string()).collect(),
                    search: f.search.iter().map(|p| p.trim().to_string()).collect(),
                    device: f.device.iter().map(|p| p.trim().to_string()).collect(),
                    story: f.story.iter().map(|p| p.trim().to_string()).collect(),
                    resynth: None,
                };
            }
            if retreat_words.is_some() || stop_intents.is_some() {
                s.validate_lexicon_values()?;
                s.validate_lexicon_combinations()?;
            }
            Ok(())
        })?;
        self.project_follow_window_policy();
        // Media trim applies immediately (hub reads it at every play submit).
        if let Some(db) = media_trim_db {
            self.hub.set_media_trim_db(db);
        }
        // Ack phrase changes (and forced re-synthesis) pre-build WAVs in
        // the background so the wake path stays cloud-free.
        if wake_ack.is_some()
            || retreat_ack.is_some()
            || window_close_ack.is_some()
            || filler.is_some()
            || truncation_notice.is_some()
        {
            let force = wake_ack
                .as_ref()
                .and_then(|a| a.resynth)
                .or_else(|| filler.as_ref().and_then(|f| f.resynth))
                .unwrap_or(false);
            if force {
                let pipe = self.clone();
                tokio::spawn(async move { pipe.synthesize_wake_acks(true).await });
            } else {
                self.spawn_ack_synthesis();
            }
        }
        // VAD changes apply to RUNNING processors immediately (calibration
        // must not wait for a reconnect to take effect).
        if let Some(v) = vad {
            let mut procs = crate::lock_or_recover(&self.mic, "voice.mic");
            for p in procs.values_mut() {
                p.set_thresholds(v.start_db, v.end_db);
            }
        }

        if let Some(req) = &duplex_requested {
            // Sent for half too: the device's ModeReport then reflects the
            // CURRENT request instead of a stale one from a previous set.
            if self.hub.mode_set(device, req).is_err() {
                // Recorded request stands; negotiation happens on reconnect.
            }
        }
        self.note(
            "mode_set",
            json!({ "device": device, "duplex_requested": duplex_requested,
                    "proactive_speech": proactive_speech, "wake_words": wake_words,
                    "retreat_words": retreat_words, "retreat_ack": retreat_ack,
                    "window_close_ack": window_close_ack,
                    "truncation_notice": truncation_notice,
                    "vad": vad, "tail_blind_ms": tail_blind_ms,
                    "keyword_interrupt": keyword_interrupt, "barge_in": barge_in,
                    "barge_protect_ms": barge_protect_ms,
                    "filler": filler,
                    "stop_intents": stop_intents,
                    "reason": reason }),
        );
        let snapshot = self.mode_snapshot();
        let mut entry = snapshot["devices"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|d| d["device"] == json!(device))
            .unwrap_or(json!({}));
        entry["reason"] = json!(reason);
        Ok(entry)
    }

    fn edit_settings(
        &self,
        actor: &str,
        summary: &str,
        edit: impl FnOnce(&mut VoiceSettings),
    ) -> Result<(), String> {
        self.edit_settings_checked(actor, summary, |next| {
            edit(next);
            Ok(())
        })
    }

    fn edit_settings_checked(
        &self,
        actor: &str,
        summary: &str,
        edit: impl FnOnce(&mut VoiceSettings) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut effective = crate::lock_or_recover(&self.settings, "voice.settings");
        let mut next = effective.clone();
        edit(&mut next)?;
        crate::config_delivery::ConfigDelivery::save_voice_settings_edit(
            &self.store,
            &effective,
            &next,
            actor,
            summary,
        )
        .map_err(|error| error.to_string())?;
        *effective = next;
        Ok(())
    }

    pub(crate) fn publish_reviewed_settings(
        self: &Arc<Self>,
        publish: impl FnOnce(&VoiceSettings) -> crate::config_delivery::Result<Option<VoiceSettings>>,
    ) -> crate::config_delivery::Result<()> {
        let mut synth_cues = false;
        {
            let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
            // The voice readers acquire states before settings when both are needed.
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let mut effective = crate::lock_or_recover(&self.settings, "voice.settings");
            if let Some(next) = publish(&effective)? {
                synth_cues = cue_phrases_changed(&effective, &next);
                let policy = FollowWindowPolicy::from(&next);
                let now = id::now_ms();
                for state in states.values_mut() {
                    state.participation.apply_policy(policy, now);
                }
                *effective = next;
            }
        }
        if synth_cues && tokio::runtime::Handle::try_current().is_ok() {
            self.spawn_ack_synthesis();
        }
        Ok(())
    }

    /// Validate a full VoiceSettings snapshot (restore path): the same rules
    /// set_mode enforces field-by-field, applied to the whole object so a
    /// restored snapshot can never smuggle in invalid values.
    pub(crate) fn validate_settings(s: &VoiceSettings) -> Result<(), String> {
        FollowWindowPatch {
            follow_window_enabled: Some(s.follow_window_enabled),
            follow_window_ms: Some(s.follow_window_ms),
            context_window_ms: Some(s.context_window_ms),
            followup_credits: s.followup_credits,
            assistant_name: Some(s.assistant_name.clone()),
            persona_prompt: Some(s.persona_prompt.clone()),
            system_prompt_override: Some(s.system_prompt_override.clone()),
            kws_threshold: Some(s.kws_threshold),
            kws_score: Some(s.kws_score),
            ..Default::default()
        }
        .validate()?;
        for req in s.duplex_requested.values() {
            if !matches!(req.as_str(), "half" | "full") {
                return Err(format!("invalid persisted duplex request '{req}'"));
            }
        }
        validate_wake_words(&s.wake_words)?;
        s.spoken.validate()?;
        if let Some(v) = &s.vad {
            validate_vad(v)?;
        }
        if !s.kws_threshold.is_finite()
            || s.kws_threshold < KWS_THRESHOLD_RANGE.0
            || s.kws_threshold > KWS_THRESHOLD_RANGE.1
        {
            return Err(format!(
                "invalid persisted kws_threshold {}",
                s.kws_threshold
            ));
        }
        if !s.kws_score.is_finite()
            || s.kws_score < KWS_SCORE_RANGE.0
            || s.kws_score > KWS_SCORE_RANGE.1
        {
            return Err(format!("invalid persisted kws_score {}", s.kws_score));
        }
        if s.tail_blind_ms > 2000 {
            return Err(format!(
                "invalid persisted tail_blind_ms {}",
                s.tail_blind_ms
            ));
        }
        if s.barge_protect_ms > 3000 {
            return Err(format!(
                "invalid persisted barge_protect_ms {}",
                s.barge_protect_ms
            ));
        }
        if !(-12.0..=0.0).contains(&s.media_trim_db) {
            return Err(format!(
                "invalid persisted media_trim_db {}",
                s.media_trim_db
            ));
        }
        if s.retreat_words.is_empty() || s.retreat_words.len() > 12 {
            return Err("invalid persisted retreat_words".into());
        }
        validate_short_ack("retreat_ack", &s.retreat_ack)
            .map_err(|_| "invalid persisted retreat_ack".to_string())?;
        validate_short_ack("window_close_ack", &s.window_close_ack)
            .map_err(|_| "invalid persisted window_close_ack".to_string())?;
        validate_truncation_notice(&s.truncation_notice)
            .map_err(|_| "invalid persisted truncation_notice".to_string())?;
        if let Err(e) = s.filler.validate() {
            return Err(format!("invalid persisted filler: {e}"));
        }
        validate_word_list("stop_words", &s.stop_intents.stop_words, 12)?;
        validate_word_list("continue_words", &s.stop_intents.continue_words, 12)?;
        Ok(())
    }

    /// Restore a voice-settings snapshot from config history. This is a
    /// CONFIG restore, not an action replay: values are re-validated, the
    /// snapshot replaces the current settings, and the persisted duplex
    /// requests are re-projected to connected devices as mode_set — the
    /// same operation a fresh connect performs. Plays, volumes and stops
    /// from the original change are NOT replayed (they are task history,
    /// not configuration).
    fn preserve_node_local_fields(settings: &mut VoiceSettings, current: &VoiceSettings) {
        settings.wake_words = current.wake_words.clone();
        settings.kws_threshold = current.kws_threshold;
        settings.kws_score = current.kws_score;
        settings.media_trim_db = current.media_trim_db;
        settings.tail_blind_ms = current.tail_blind_ms;
        settings.vad = current.vad.clone();
        settings.duplex_requested = current.duplex_requested.clone();
        settings.keyword_interrupt = current.keyword_interrupt;
        settings.barge_in = current.barge_in;
        settings.barge_protect_ms = current.barge_protect_ms;
        settings.asr_wake_fallback = current.asr_wake_fallback;
        settings.proactive_asr_wake = current.proactive_asr_wake;
    }

    pub(crate) fn prevalidate_settings_patch(
        &self,
        patch: &serde_json::Value,
    ) -> Result<(), String> {
        let current = crate::lock_or_recover(&self.settings, "voice.settings").clone();
        let settings =
            crate::config_delivery::ConfigDelivery::patch_voice_settings(&current, patch)
                .map_err(|error| error.detail)?;
        Self::validate_settings(&settings)
    }

    pub(crate) fn prevalidate_settings_restore(
        &self,
        mut settings: VoiceSettings,
        from_peer: bool,
    ) -> Result<(), String> {
        if from_peer {
            let current = crate::lock_or_recover(&self.settings, "voice.settings").clone();
            Self::preserve_node_local_fields(&mut settings, &current);
        }
        Self::validate_settings(&settings)
    }

    pub(crate) fn overwrite_in_memory_settings(&self, restored: VoiceSettings) {
        let trim_db = restored.media_trim_db;
        *crate::lock_or_recover(&self.settings, "voice.settings") = restored;
        self.project_follow_window_policy();
        self.hub.set_media_trim_db(trim_db);
    }

    pub fn restore_settings(
        self: &Arc<Self>,
        settings: VoiceSettings,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        self.restore_settings_with(actor, None, None, |_| Ok(settings))
    }

    pub(crate) fn restore_settings_from_peer(
        self: &Arc<Self>,
        mut settings: VoiceSettings,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        self.restore_settings_with(actor, None, None, |current| {
            Self::preserve_node_local_fields(&mut settings, current);
            Ok(settings)
        })
    }

    pub(crate) fn restore_settings_patch(
        self: &Arc<Self>,
        patch: &serde_json::Value,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        self.restore_settings_with(actor, Some(patch), None, |current| {
            crate::config_delivery::ConfigDelivery::patch_voice_settings(current, patch)
                .map_err(|error| error.detail)
        })
    }

    pub(crate) fn restore_settings_document(
        self: &Arc<Self>,
        settings: VoiceSettings,
        document: &serde_json::Value,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        self.restore_settings_with(actor, None, Some(document), |_| Ok(settings))
    }

    fn restore_settings_with(
        self: &Arc<Self>,
        actor: &str,
        patch: Option<&serde_json::Value>,
        document: Option<&serde_json::Value>,
        restore: impl FnOnce(&VoiceSettings) -> Result<VoiceSettings, String>,
    ) -> Result<serde_json::Value, String> {
        let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
        let (before, settings) = {
            let mut current = crate::lock_or_recover(&self.settings, "voice.settings");
            let before = current.clone();
            let mut settings = restore(&current)?;
            Self::validate_settings(&settings)?;
            settings.assistant_name = settings.assistant_name.trim().to_string();
            settings.persona_prompt = settings.persona_prompt.trim().to_string();
            settings.system_prompt_override = settings
                .system_prompt_override
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let summary = format!(
                "voice settings restored: duplex={:?} proactive={}",
                settings.duplex_requested, settings.proactive_speech
            );
            use crate::config_delivery::ConfigDelivery;
            match patch {
                Some(patch) => ConfigDelivery::save_voice_settings_patch(
                    &self.store,
                    &settings,
                    patch,
                    actor,
                    &summary,
                ),
                None => ConfigDelivery::save_voice_settings_snapshot(
                    &self.store,
                    &settings,
                    document,
                    actor,
                    &summary,
                ),
            }
            .map_err(|error| error.to_string())?;
            *current = settings.clone();
            (before, settings)
        };
        self.hub.set_media_trim_db(settings.media_trim_db);
        self.project_follow_window_policy();
        if patch.is_none() {
            if let Some(vad) = settings.vad {
                let mut processors = crate::lock_or_recover(&self.mic, "voice.mic");
                for processor in processors.values_mut() {
                    processor.set_thresholds(vad.start_db, vad.end_db);
                }
            }
            for (device, requested) in &settings.duplex_requested {
                let _ = self.hub.mode_set(device, requested);
            }
        }
        if cue_phrases_changed(&before, &settings) && tokio::runtime::Handle::try_current().is_ok()
        {
            self.spawn_ack_synthesis();
        }

        self.note(
            "voice_settings_restored",
            json!({ "actor": actor, "settings": settings }),
        );
        Ok(self.mode_snapshot())
    }

    pub(crate) fn factory_reset_configuration(
        &self,
        include_secrets: bool,
    ) -> crate::store::Result<serde_json::Value> {
        let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
        let cleared = self.store.factory_reset(include_secrets)?;
        self.clear_runtime_settings();
        Ok(cleared)
    }

    fn clear_runtime_settings(&self) {
        {
            let mut states = crate::lock_or_recover(&self.states, "voice.states");
            let mut settings = crate::lock_or_recover(&self.settings, "voice.settings");
            *settings = VoiceSettings::default();
            states.clear();
        }
        crate::lock_or_recover(&self.log, "voice.log").clear();
        crate::lock_or_recover(&self.reminder_inflight, "voice.reminder_inflight").clear();
        *crate::lock_or_recover(&self.last_arbitration, "voice.last_arbitration") = None;
        *crate::lock_or_recover(&self.last_cue_synth, "voice.last_cue_synth") = None;
        crate::lock_or_recover(&self.foreign_cues, "voice.foreign_cues").clear();
        crate::lock_or_recover(&self.wifi_provisioning, "voice.wifi_provisioning").clear();
        self.sovereignty.clear();
        if let Some(j) = &self.events_file {
            j.clear();
        }
    }

    pub(super) fn project_follow_window_policy(&self) {
        let mut states = crate::lock_or_recover(&self.states, "voice.states");
        let policy =
            FollowWindowPolicy::from(&*crate::lock_or_recover(&self.settings, "voice.settings"));
        for st in states.values_mut() {
            st.participation.apply_policy(policy, id::now_ms());
        }
    }

    /// The persisted voice settings as a plain value (config backup, R1f).
    /// Device facts are NOT included: `duplex_requested` already carries the
    /// per-device request, and this must round-trip through `set_mode`.
    pub fn settings_snapshot(&self) -> VoiceSettings {
        crate::lock_or_recover(&self.settings, "voice.settings").clone()
    }

    /// R2 (2026-11-13): the spoken-delivery policy is edited on its own card
    /// and its own endpoint. It rides the same persistence key, validation
    /// and history/restore chain as every other voice setting (the mode
    /// endpoint's parameter list is already at its practical limit), so a
    /// console save and a restore behave exactly like the rest of the page.
    pub fn set_spoken_delivery(
        self: &Arc<Self>,
        device: &str,
        spoken: SpokenDeliverySettings,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
        let normalized = spoken.normalized_for_console()?;
        let summary = format!(
                "device {device}: spoken short={}/{} detail={}/{} backstop={}/{} continue_hint={:?} resume_lead={:?}",
                normalized.max_sentences, normalized.max_chars,
                normalized.detail_max_sentences, normalized.detail_max_chars,
                normalized.backstop_max_sentences, normalized.backstop_max_chars,
                normalized.continue_hint, normalized.resume_lead
            );
        self.edit_settings(actor, &summary, |s| {
            s.spoken = normalized.clone();
        })?;
        self.note(
            "spoken_set",
            json!({ "device": device, "spoken": normalized, "actor": actor }),
        );
        Ok(json!({ "device": device, "spoken": normalized, "state": "applied" }))
    }

    /// Wake arbitration mode (v0.30, 2026-11-15).
    ///
    /// Global, like the other interaction policies: every node in a house has
    /// to run the same mode or the household answers twice (one node enforcing
    /// and one observing would produce exactly one ack from the enforcer and
    /// an extra answer from the observer). `device` is only the attribution.
    pub fn set_wake_arbitration(
        self: &Arc<Self>,
        device: &str,
        mode: WakeArbitration,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
        let summary = format!("device {device}: wake_arbitration={}", mode.as_str());
        self.edit_settings(actor, &summary, |s| {
            s.wake_arbitration = mode;
        })?;
        self.note(
            "wake_arbitration_set",
            json!({ "device": device, "mode": mode.as_str(), "actor": actor,
                    "peers": self.arbiter.peer_count() }),
        );
        Ok(json!({ "device": device, "wake_arbitration": mode.as_str(),
                   "peers": self.arbiter.peer_count(), "state": "applied" }))
    }

    /// ASR wake fallback switch (user decision 2026-11-14, commercial privacy).
    ///
    /// This is a GLOBAL capture policy, not a per-device one: it decides
    /// whether idle room speech is transcribed at all. It rides the same
    /// persistence key, history and restore chain as every other voice
    /// setting, so a console save and a config restore behave identically.
    /// `device` is only the attribution in the change record.
    pub fn set_asr_wake_fallback(
        self: &Arc<Self>,
        device: &str,
        enabled: bool,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let _edit = crate::lock_or_recover(&self.settings_edit_gate, "voice.settings_edit");
        let summary = format!("device {device}: asr_wake_fallback={enabled}");
        self.edit_settings(actor, &summary, |s| {
            s.asr_wake_fallback = enabled;
            s.proactive_asr_wake = enabled;
        })?;
        self.note(
            "asr_wake_fallback_set",
            json!({ "device": device, "enabled": enabled, "actor": actor,
                    "why": "idle room speech is transcribed only when this is on" }),
        );
        Ok(json!({ "device": device, "asr_wake_fallback": enabled, "state": "applied" }))
    }

    pub fn mode_snapshot(&self) -> serde_json::Value {
        let now = id::now_ms();
        let known_devices: Vec<String> = crate::lock_or_recover(&self.states, "voice.states")
            .keys()
            .cloned()
            .collect();
        let media_paused_by_device: std::collections::HashMap<String, bool> = known_devices
            .into_iter()
            .map(|d| {
                let paused = self.media_pause_in_flight(&d, now);
                (d, paused)
            })
            .collect();
        let states = crate::lock_or_recover(&self.states, "voice.states");
        let settings = crate::lock_or_recover(&self.settings, "voice.settings");
        // R1j/P1: the reflex vocabulary is configuration, not per-device
        // state; read it once and merge each device's own counters into it.
        let reflex_cfg = crate::config::ConfigService::new(self.store.clone())
            .reflex()
            .unwrap_or_default();
        // Fallback VAD view for devices with no processor yet (the settings
        // lock is already held here — do NOT call live_vad_config()).
        let vad_fallback = {
            let mut c = VadConfig::from_env();
            if let Some(v) = &settings.vad {
                c.start_db = v.start_db;
                c.end_db = v.end_db;
            }
            c
        };
        let devices: Vec<serde_json::Value> = states
            .iter()
            .map(|(device, st)| {
                let requested = settings
                    .duplex_requested
                    .get(device)
                    .cloned()
                    .unwrap_or_else(|| "half".into());
                let mut duplex = st.duplex.clone().unwrap_or(DuplexState {
                    capability: "unknown".into(),
                    requested: requested.clone(),
                    negotiated: "unknown".into(),
                    effective: "unknown".into(),
                });
                // The persisted request is the user's current intent; the
                // device report echoes what it last negotiated.
                duplex.requested = requested.clone();
                // Media plays live in the hub registry (tool submissions);
                // merge them into the outputs view for honest status.
                let hub_plays = self.hub.active_plays_for(device);
                let media_paused = media_paused_by_device.get(device).copied().unwrap_or(false);
                let hub_live = HubLive {
                    voice: hub_plays.iter().any(|(_, p)| p.channel != "media"),
                    media: hub_plays.iter().any(|(_, p)| p.channel == "media") && !media_paused,
                };
                let participation_active = st.session_open(now);
                // Merged outputs view: voice plays (this map, with task +
                // stage provenance) plus hub-registered plays it lacks
                // (tool-submitted media).
                let mut outputs: Vec<serde_json::Value> = st
                    .outputs
                    .iter()
                    .map(|(pid, rec)| {
                        json!({
                            "play_id": pid, "channel": rec.channel, "kind": rec.kind,
                            "stage": rec.stage,
                            "task": rec.task_id, "content_ref": rec.content_ref,
                            "started_ms": rec.started_ms,
                        })
                    })
                    .collect();
                for (pid, p) in &hub_plays {
                    if !st.outputs.contains_key(pid) {
                        outputs.push(json!({
                            "play_id": pid, "channel": p.channel,
                            "kind": if p.channel == "media" { "media" } else { "answer" },
                            "stage": "active",
                        }));
                    }
                }
                // Mic stream telemetry (console debt, G4): achieved session
                // state, flow liveness, block levels and lifetime counters.
                let (in_speech, level_stats, vad_cfg, blocks_seen, noise_db, effective, last_close, endpoint) = {
                    let procs = crate::lock_or_recover(&self.mic, "voice.mic");
                    procs
                        .get(device)
                        .map(|p| {
                            (
                                p.in_speech(),
                                p.level_stats(),
                                p.config().clone(),
                                p.blocks_seen,
                                p.noise_floor_db(),
                                p.effective_thresholds(),
                                p.last_close(),
                                json!({
                                    "kind": p.endpoint_kind(),
                                    "stats": p.endpoint_stats().map(|(frames, speech, thr, gain, last)| json!({
                                        "frames": frames, "speech_frames": speech,
                                        "threshold": thr, "gain": gain, "last_prob": last,
                                    })),
                                    "fallback": p.endpoint_failure(),
                                    "model": self.silero.path,
                                    "model_detail": if p.endpoint_kind() == "gateway_vad" {
                                        "gateway neural VAD decisions received"
                                    } else { self.silero.detail.as_str() },
                                }),
                            )
                        })
                        .unwrap_or((false, None, vad_fallback.clone(), 0, None, (
                            vad_fallback.start_db,
                            vad_fallback.end_db,
                        ), None, json!({
                            "kind": if self.silero.model.is_some() { "silero" } else { "energy" },
                            "stats": null, "fallback": null,
                            "model": self.silero.path, "model_detail": self.silero.detail,
                        })))
                };
                let last_close = match last_close {
                    Some((reason, blocks)) => json!({
                        "reason": match reason {
                            crate::mic::CloseReason::Quiet => "quiet",
                            crate::mic::CloseReason::Cap => "cap",
                        },
                        "blocks": blocks,
                    }),
                    None => json!(null),
                };
                let c = &st.mic_counters;
                // Moving average: last completed window blended with the
                // current one — a single live window oscillates badly
                // right after each rotation.
                let chunks_per_sec = {
                    let live_count = st.chunk_window_count;
                    let live_span = if st.chunk_window_start_ms > 0 {
                        now.saturating_sub(st.chunk_window_start_ms).clamp(200, 10_000)
                    } else { 0 };
                    let last = st.chunks_per_sec.unwrap_or(0.0);
                    let est = if live_span >= 200 {
                        live_count as f64 * 1000.0 / live_span as f64
                    } else { last };
                    if last <= 0.0 { est } else { (last + est) / 2.0 }
                };
                let latency_summary = if st.latency.is_empty() {
                    json!(null)
                } else {
                    let n = st.latency.len() as u64;
                    let synth: u64 = st.latency.iter().map(|s| s.synth_ms).sum();
                    let confirm: u64 = st.latency.iter().map(|s| s.confirm_ms).sum();
                    let confirm_max = st.latency.iter().map(|s| s.confirm_ms).max().unwrap_or(0);
                    json!({
                        "samples": n,
                        "synth_mean_ms": synth / n,
                        "confirm_mean_ms": confirm / n,
                        "confirm_max_ms": confirm_max,
                    })
                };
                json!({
                    "device": device,
                    "duplex": duplex,
                    // R2-BM6 audit: the hub ARRIVAL sequence of the last
                    // ModeReport, so a management tool can require a report
                    // that arrived after the command it sent.
                    "duplex_report_seq": st.duplex_report_seq,
                    "aec": st.aec,
                    "muted": st.muted,
                    // v1 mechanism: who last wrote the latch (device fact).
                    "mute_source": st.mute_source,
                    "participation": if participation_active { "conversation" } else { "idle" },
                    // Explicit session object (R1f): state + since. The wire
                    // keeps `participation`/`participation_since_ms` for the
                    // console and existing tools.
                    "session": st.participation.snapshot(),
                    "input": st.input_snapshot(),
                    "input_window_open": st.session_covers_with_live(now, now, now, hub_live),
                    "participation_since_ms": (st.participation.session.state == SessionState::Listening)
                        .then_some(st.participation.session.since_ms),
                    "proactive_speech": settings.proactive_speech,
                    "follow_window_tone": settings.follow_window_tone,
                    "retreat_intent": settings.retreat_intent,
                    "wake_ack": {
                        "enabled": settings.wake_ack.enabled,
                        "phrases": settings.wake_ack.phrases,
                    },
                    // B-M0: the cue is a per-turn fact — the console shows
                    // whether one is live right now, not just the config.
                    "filler": {
                        "live": st.filler_play.as_ref().map(|f| json!({
                            "play_id": f.play_id,
                            "scene": f.scene,
                            "phrase": f.phrase,
                        })),
                        "delay_ms": settings.filler.delay_ms,
                        // R1g/P4: radio (music may start under the cue) or
                        // interrupt (the cue only ever covers silence).
                        "mode": settings.filler.mode,
                    },
                    "activity": st.activity(now, hub_live),
                    // R1e: the spoken program's bookmark — the console and the
                    // acceptance tools need to see WHAT 继续 would replay, not
                    // just that something was paused.
                    "program": {
                        "live": st.live_program.as_ref().map(|p| json!({
                            "cursor": p.cursor, "chars": p.text.len(), "complete": p.complete,
                            "next": p.text[Self::clamp_boundary(&p.text, p.cursor)..].chars().take(60).collect::<String>(),
                        })),
                        "paused": st.paused_program.as_ref().map(|p| json!({
                            "cursor": p.cursor, "chars": p.text.len(), "complete": p.complete,
                            "since_ms": p.created_ms,
                            // true = the spoken cap left it (no amber, gone
                            // when the follow window closes); false = the
                            // person's pause (amber, 10 min).
                            "system_cut": p.system_cut,
                            "next": p.text[Self::clamp_boundary(&p.text, p.cursor)..].chars().take(60).collect::<String>(),
                        })),
                    },
                    "assistant_state": if st.assistant_last.is_empty() { json!(null) } else { json!(st.assistant_last) },
                    "current_task": st.current_task,
                    "outputs": outputs,
                    "mic": {
                        "session": st.mic_active,
                        "detail": st.mic_detail,
                        "in_speech": in_speech,
                        "last_chunk_ms": st.last_chunk_ms,
                        "chunks_per_sec": chunks_per_sec,
                        "blocks_seen": blocks_seen,
                        "levels": match level_stats {
                            Some((count, min, mean, p95, max)) => json!({
                                "count": count,
                                "min_db": min, "mean_db": mean,
                                "p95_db": p95, "max_db": max,
                            }),
                            None => json!(null),
                        },
                        "vad": {
                            // Configured values are the SENSITIVITY CEILING;
                            // `effective_*` is what the detector is using in
                            // this room right now. They differ only when the
                            // room is louder than the calibration assumed —
                            // the console shows both so a drift is visible
                            // without reading four-digit log lines.
                            "start_db": vad_cfg.start_db,
                            "end_db": vad_cfg.end_db,
                            "effective_start_db": effective.0,
                            "effective_end_db": effective.1,
                            "noise_db": noise_db,
                            "noise_margin_db": vad_cfg.noise_margin_db,
                            "speak_margin_db": vad_cfg.speak_margin_db,
                            "last_close": last_close,
                            // M1b: WHICH detector is deciding, with its own
                            // telemetry. `kind` is the processor's live answer
                            // (a latched model failure shows up as `fallback`).
                            "endpoint": endpoint,
                        },
                        "counters": mic_counters_json(c),
                    },
                    "latency": latency_summary,
                    // D/T4: device-side keyword spotter truth + how many
                    // detections this pipeline acted on / ignored.
                    "kws": {
                        "state": st.kws_state,
                        "detail": st.kws_detail,
                        "restarts": st.kws_restarts,
                        "detections": st.kws_detections,
                        "guarded": st.kws_guarded,
                        "stops": st.keyword_stops,
                        "wakes": st.keyword_wakes,
                        "ignored": st.keyword_ignored,
                        "last_keyword_ms": st.last_keyword_ms,
                        "phrase": settings.wake_words.first().map(String::as_str).unwrap_or(""),
                        // R1g/P7: two processes, two epochs. The gateway's
                        // lifetime `detections` outlive this core's counters,
                        // so compare `detections_since_epoch` with `stops`
                        // only inside [counted_since_ms, now].
                        "epoch": st.kws_epoch,
                        "detections_since_epoch": st.kws_detections.saturating_sub(st.kws_detections_base),
                        "counted_since_ms": st.kws_epoch_seen_ms,
                        "core_epoch": self.started_ms,
                        // Contract v2 §2.5 (#5): retreat words the DEVICE has
                        // armed for a local exit. Durable across core restarts
                        // (the deploy record below is not) — the console and
                        // the release activation read this one.
                        "local_exit": st.kws_local_exit,
                        // Contract v2 #4: what the LOADED model reports it
                        // runs with (not the config — the two differ until a
                        // deploy lands).
                        "threshold": round3(st.kws_threshold as f64),
                        "score": round3(st.kws_score as f64),
                    },
                    // D/T5: semantic barge-in truth — how many near-end
                    // speech facts this pipeline acted on / suppressed.
                    "barge": {
                        "stops": st.barge_in_stops,
                        "ignored": st.barge_in_ignored,
                        // R1g: half-duplex normal facts (no stop, no window).
                        "suppressed": st.barge_in_suppressed,
                        "last_barge_in_ms": st.last_barge_in_ms,
                        "preroll_held": st.barge.is_preroll_held(),
                        "window_open": st.barge.is_window_open(now),
                        // R1 truth: the current answer is still inside its
                        // onset protection period (facts are being ignored).
                        "protected": now < st.barge_protect_until_ms,
                        "protect_ms": settings.barge_protect_ms,
                    },
                    // R1j/P1: the reflex table + this device's per-word counts
                    // and the last deployment as the DEVICE answered it.
                    "reflex": {
                        "words": reflex_cfg.words.iter().map(|w| json!({
                            "text": w.text, "tokens": w.tokens,
                            "intent": w.intent.word(), "enabled": w.enabled,
                            "hits": st.reflex_hits.get(&w.text).copied().unwrap_or(0),
                            "ignored": st.reflex_ignored.get(&w.text).copied().unwrap_or(0),
                            "text_hits": st.reflex_text_hits.get(&w.text).copied().unwrap_or(0),
                        })).collect::<Vec<_>>(),
                        "deploy": st.reflex_deploy.as_ref().map(|d| {
                            // M6b: sent → installed → loaded. `loaded_matches`
                            // is true only when the spotter's own load proof
                            // equals the installed file; null = not reported.
                            // `state` is derived from the SAME facts here, so
                            // the snapshot can never say "loaded" next to
                            // loaded_matches:false.
                            let matches = d.loaded_hash.as_ref().map(|h| *h == d.hash);
                            let state = match (d.state.as_str(), matches) {
                                ("accepted" | "loaded" | "loaded_mismatch", Some(true)) => "loaded",
                                ("accepted" | "loaded" | "loaded_mismatch", Some(false)) => {
                                    "loaded_mismatch"
                                }
                                (s, _) => s,
                            };
                            json!({
                                "state": state, "hash": d.hash, "keywords": d.keywords,
                                "sent_hash": d.sent_hash,
                                "loaded_hash": d.loaded_hash,
                                "loaded_ms": d.loaded_ms,
                                "loaded_matches": matches,
                                "detail": d.detail, "sent_ms": d.sent_ms, "acked_ms": d.acked_ms,
                            })
                        }),
                        "dedupe_ms": REFLEX_DEDUPE_MS,
                    },
                    // B-M1: stop/don't-stop truth. `continue_intents` counts
                    // the "别停" requests that arrived after the barge-in had
                    // already stopped the audio (a measured false stop).
                    "stop_intents": {
                        "continue_intents": st.continue_intents,
                    },
                })
            })
            .collect::<Vec<_>>();
        // The literal above sits at the `json!` recursion ceiling: further
        // keys are inserted here instead of being added to it (2026-11-14).
        let mut devices: Vec<serde_json::Value> = devices;
        let device_names: Vec<String> = devices
            .iter()
            .map(|d| d["device"].as_str().unwrap_or_default().to_string())
            .collect();
        for dev in devices.iter_mut() {
            let name = dev["device"].as_str().unwrap_or_default().to_string();
            if let Some(st) = states.get(&name) {
                if let Some(music) = self.music() {
                    let explicit = st
                        .participation
                        .opened_by
                        .as_deref()
                        .filter(|by| {
                            matches!(*by, "keyword" | "protocol_wake" | "text_wake" | "asr_wake")
                        })
                        .map(|_| st.participation.session.since_ms);
                    if music.blocks_passive_capture(&name, tokio::time::Instant::now(), 0, explicit)
                    {
                        dev["input_window_open"] = json!(false);
                    }
                    dev["media_guard"] = music.playback_status(&name);
                }
                dev["mic"]["counters"]["asr_skipped"] = json!(st.mic_counters.asr_skipped);
                // Contract v2 §2.5 (#5): the local-exit half of a deploy.
                // `local_exit` is the DEVICE's armed count, `local_exit_matches`
                // says it equals what was sent, and `local_exits` counts the
                // facts it reported. Inserted here because the literal above is
                // at the `json!` recursion ceiling.
                if dev["reflex"]["deploy"].is_object() {
                    if let Some(d) = st.reflex_deploy.as_ref() {
                        dev["reflex"]["deploy"]["retreat_words_sent"] = json!(d.retreat_words_sent);
                        dev["reflex"]["deploy"]["local_exit"] = json!(d.local_exit);
                        dev["reflex"]["deploy"]["local_exit_matches"] =
                            json!(d.local_exit == d.retreat_words_sent);
                        dev["reflex"]["deploy"]["intents_hash"] = json!(d.intents_hash);
                        dev["reflex"]["deploy"]["intents_hash_installed"] =
                            json!(d.intents_hash_installed);
                        dev["reflex"]["deploy"]["intents_hash_matches"] = json!(
                            !d.intents_hash_installed.is_empty()
                                && d.intents_hash_installed == d.intents_hash
                        );
                        dev["reflex"]["deploy"]["local_exits"] = json!(d.local_exits);
                        // #4: what this deploy asked the spotter to run with,
                        // and what the model reported after loading it.
                        dev["reflex"]["deploy"]["kws_threshold"] = json!(d.kws_threshold);
                        dev["reflex"]["deploy"]["kws_score"] = json!(d.kws_score);
                        dev["reflex"]["deploy"]["applied_threshold"] =
                            json!(round3(d.applied_threshold as f64));
                        dev["reflex"]["deploy"]["applied_score"] =
                            json!(round3(d.applied_score as f64));
                        dev["reflex"]["deploy"]["last_local_exit"] =
                            d.last_local_exit.clone().map_or(json!(null), |v| v);
                    }
                }
            }
        }
        drop(states);
        drop(settings);
        for (dev, name) in devices.iter_mut().zip(device_names.iter()) {
            dev["playback"] = json!(self.playback_capabilities(name, None));
        }
        let settings = crate::lock_or_recover(&self.settings, "voice.settings");
        let mut snap = json!({
            "default_duplex": "half",
            "wake_words": settings.wake_words,
            "retreat_words": settings.retreat_words,
            "retreat_ack": settings.retreat_ack,
            "window_close_ack": settings.window_close_ack,
            "truncation_notice": settings.truncation_notice,
            "vad": settings.vad,
            "tail_blind_ms": settings.tail_blind_ms,
            "media_trim_db": settings.media_trim_db,
            "keyword_interrupt": settings.keyword_interrupt,
            "barge_in": settings.barge_in,
            "barge_protect_ms": settings.barge_protect_ms,
            "filler": settings.filler,
            "stop_intents": settings.stop_intents,
            // R2: the spoken-delivery policy in force — the console shows the
            // same numbers the prompt and the runtime cap use.
            "spoken": settings.spoken,
            "keyword_phrase": settings.wake_words.first().cloned().unwrap_or_default(),
            // M6b: the persistent journal's bounded health (written files,
            // dropped enqueues, bytes vs the cap). The console shows it so
            // "the disk is full" is a number, not a guess.
            "voice_journal": self.events_file_stats(),
            "devices": devices,
        });
        // 2026-11-14: whether idle room speech is transcribed (the ASR wake
        // fallback). Global; the console states the recall/privacy trade-off.
        snap["assistant_name"] = json!(settings.assistant_name);
        snap["persona_prompt"] = json!(settings.persona_prompt);
        snap["system_prompt_override"] = json!(settings.system_prompt_override);
        snap["asr_wake_fallback"] = json!(settings.proactive_asr_wake);
        snap["proactive_asr_wake"] = json!(settings.proactive_asr_wake);
        snap["sovereignty"] = json!(self.sovereignty.snapshot(now));
        snap["local_music_notices"] = json!(self.sovereignty.local_music_snapshot());
        // v0.30 (wake arbitration): the mode AND the household it applies to,
        // so the console can say "enforce, 4 peers" instead of implying that
        // one speaker decides alone.
        snap["wake_arbitration"] = json!({
            "mode": settings.wake_arbitration.as_str(),
            "peers": self.arbiter.peer_count(),
        });
        snap["follow_window_enabled"] = json!(settings.follow_window_enabled);
        snap["follow_window_ms"] = json!(settings.follow_window_ms);
        snap["context_window_ms"] = json!(settings.context_window_ms);
        snap["followup_credits"] = json!(settings.followup_credits);
        snap["follow_window_tone"] = json!(settings.follow_window_tone);
        // Contract v2 #4: the spotter's tunable sensitivity (the CONFIG; the
        // applied numbers per device are in `devices[*].kws`).
        snap["kws_threshold"] = json!(settings.kws_threshold);
        snap["kws_score"] = json!(settings.kws_score);
        snap["cue_readiness"] = self.cue_readiness_for(&settings);
        snap
    }
}

/// The mic counters as the console reads them. A free function because the
/// enclosing `mode_snapshot` macro is already at serde_json's recursion limit
/// — one more nested object inside it does not compile (2026-11-14).
pub(super) fn mic_counters_json(c: &MicCounters) -> serde_json::Value {
    json!({
        "speech_started": c.speech_started,
        "segments": c.segments,
        "segments_discarded": c.segments_discarded,
        "close_quiet": c.close_quiet,
        "close_cap": c.close_cap,
        "wakes": c.wakes,
        "followup_turns": c.followup_turns,
        "ignored_unwoken": c.ignored_unwoken,
        "asr_failed": c.asr_failed,
        "asr_skipped": c.asr_skipped,
        "self_echo_suppressed": c.self_echo_suppressed,
        "tail_suppressed": c.tail_suppressed,
        // Chunks dropped because our own voice was live (half duplex).
        // Counted since 2026-11-14: an ignored interruption used to leave no
        // trace at all (codex interaction audit, finding 5).
        "suppressed_during_voice": c.suppressed_during_voice,
        "barge_tail_dropped": c.barge_tail_dropped,
        // R1g/P7: an empty transcript used to be only a log line, so
        // "6.48 s -> nothing" could not be counted against anything.
        "segment_empty": c.segment_empty,
        "asr_inflight": c.asr_inflight,
    })
}

/// Short-word list rules shared by the stop/continue intents: 1–`max`
/// entries, 1–8 chars each after trim, no quotes or slashes (they are matched
/// against ASR text and echoed in the event log).
fn validate_retreat_words(words: &[String]) -> Result<(), String> {
    if words.is_empty() {
        return Err("retreat_words 不能为空：至少保留一个结束词".into());
    }
    if words.len() > 12 {
        return Err("retreat_words 最多 12 个（默认集 9 个）".into());
    }
    for word in words {
        let len = word.trim().chars().count();
        if len == 0 || len > 8 || word.contains('"') || word.contains('/') {
            return Err(format!("结束词 '{word}'：1–8 字，不含引号或斜杠"));
        }
    }
    Ok(())
}

impl VoiceSettings {
    fn validate_lexicon_values(&self) -> Result<(), String> {
        validate_retreat_words(&self.retreat_words)?;
        validate_word_list("stop_words", &self.stop_intents.stop_words, 12)?;
        validate_word_list("continue_words", &self.stop_intents.continue_words, 12)?;
        let voice_words = self
            .retreat_words
            .iter()
            .chain(&self.stop_intents.stop_words)
            .chain(&self.stop_intents.continue_words);
        if voice_words
            .clone()
            .any(|word| normalize_utterance(word).is_empty() || word.trim() != word)
        {
            return Err(
                "voice words must contain text and all phrases must use canonical spacing".into(),
            );
        }
        if self
            .stop_intents
            .stop_words
            .iter()
            .chain(&self.stop_intents.continue_words)
            .any(|word| pause_match(word) || is_retreat(&normalize_utterance(word)))
            || self.retreat_words.iter().any(|word| pause_match(word))
        {
            return Err("voice words must not replace a reserved Pause or retreat command".into());
        }
        Ok(())
    }

    fn validate_lexicon_combinations(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        let words = self
            .retreat_words
            .iter()
            .chain(&self.stop_intents.stop_words)
            .chain(&self.stop_intents.continue_words);
        if words
            .into_iter()
            .any(|word| !seen.insert(normalize_utterance(word).to_lowercase()))
        {
            return Err("voice intent commands must have distinct normalized words".into());
        }
        Ok(())
    }

    pub(crate) fn validate_delivery_policy_values(&self) -> Result<(), String> {
        self.validate_lexicon_values()?;
        self.spoken.validate_delivery_values()?;
        if [&self.spoken.continue_hint, &self.spoken.resume_lead]
            .iter()
            .any(|text| text.trim() != *text)
        {
            return Err("voice phrases must use canonical spacing".into());
        }
        validate_short_ack("retreat_ack", &self.retreat_ack)?;
        validate_short_ack("window_close_ack", &self.window_close_ack)?;
        validate_wake_ack(&self.wake_ack)?;
        self.filler.validate()?;
        if self.truncation_notice.trim().is_empty() {
            return Err("truncation_notice 不能为空".into());
        }
        validate_truncation_notice(&self.truncation_notice)?;
        let cue_phrases = [
            &self.retreat_ack,
            &self.window_close_ack,
            &self.truncation_notice,
        ]
        .into_iter()
        .chain(&self.wake_ack.phrases)
        .chain(&self.filler.thinking)
        .chain(&self.filler.search)
        .chain(&self.filler.device)
        .chain(&self.filler.story);
        if cue_phrases.into_iter().any(|text| text.trim() != text) {
            return Err("voice cue phrases must use canonical spacing".into());
        }
        Ok(())
    }

    pub(crate) fn validate_reviewed_delivery(&self) -> Result<(), String> {
        self.validate_delivery_policy_values()?;
        self.validate_lexicon_combinations()?;
        self.spoken.validate_delivery()
    }
}

pub(super) fn validate_word_list(name: &str, words: &[String], max: usize) -> Result<(), String> {
    if words.is_empty() {
        return Err(format!("{name} 不能为空：至少保留一个词"));
    }
    if words.len() > max {
        return Err(format!("{name} 最多 {max} 个"));
    }
    for w in words {
        let n = w.trim().chars().count();
        if n == 0 || n > 8 || w.contains('"') || w.contains('/') {
            return Err(format!("{name} 词 '{w}'：1–8 字，不含引号或斜杠"));
        }
    }
    Ok(())
}

/// M7-1: the first balanced `{…}` object in a model answer. Providers wrap
/// JSON in prose or fences often enough that a strict parse would make the
/// memory extractor fail on a working model.
pub(super) fn parse_first_json(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// Loudest level the assistant may ask for: 90 % of the scale. Above it this
/// driver crackles, so the cap is enforced where every VOLUME path meets:
/// voice, console, and tools.
pub const VOLUME_CAP: f64 = 0.9;

/// The truncation notice is ONE sentence, spoken from the local cue cache
/// (so it costs no cloud round-trip at the very end of a story). Empty is
/// legal and means "stay silent"; anything else must be speakable in one
/// breath (≤60 chars) and free of quotes/slashes that break the phrase
/// cache's file naming.
pub(super) fn validate_wake_ack(ack: &WakeAckSettings) -> Result<(), String> {
    if ack.phrases.is_empty() || ack.phrases.len() > 8 {
        return Err("wake ack phrases: 1–8 entries".into());
    }
    for ph in &ack.phrases {
        let n = ph.trim().chars().count();
        if n == 0 || n > 6 || ph.contains('"') || ph.contains('/') {
            return Err(format!(
                "wake ack phrase '{ph}': 1–6 chars, no quotes or slashes"
            ));
        }
    }
    Ok(())
}

fn cue_phrases_changed(before: &VoiceSettings, after: &VoiceSettings) -> bool {
    before.wake_ack.phrases != after.wake_ack.phrases
        || (!before.wake_ack.enabled && after.wake_ack.enabled)
        || before.retreat_ack != after.retreat_ack
        || before.window_close_ack != after.window_close_ack
        || before.truncation_notice != after.truncation_notice
        || before.filler.thinking != after.filler.thinking
        || before.filler.search != after.filler.search
        || before.filler.device != after.filler.device
        || before.filler.story != after.filler.story
}

pub(super) fn validate_truncation_notice(notice: &str) -> Result<(), String> {
    let n = notice.trim().chars().count();
    if n == 0 {
        return Ok(());
    }
    if n > 60 || notice.contains('"') || notice.contains('/') || notice.contains('\\') {
        return Err("truncation_notice（截断提示）最多 60 字，且不含引号或斜杠".into());
    }
    if !notice.chars().any(|c| c.is_alphanumeric()) {
        return Err("truncation_notice 需要可朗读的文字".into());
    }
    Ok(())
}

/// Wake-word rules (G4 console debt): at least one word, each 1–16 chars
/// after trim, at most 8, and each must map to at least one pinyin (or
/// alphanumeric) syllable — a word the matcher can never see is a typo.
pub(super) fn validate_wake_words(words: &[String]) -> Result<(), String> {
    if words.is_empty() {
        return Err("wake_words 不能为空：至少保留一个唤醒词".into());
    }
    if words.len() > 8 {
        return Err("wake_words 最多 8 个".into());
    }
    for w in words {
        let chars = w.chars().count();
        if chars == 0 {
            return Err("唤醒词不能为空白".into());
        }
        if chars > 16 {
            return Err(format!("唤醒词 '{w}' 过长（≤16 字符）"));
        }
        if mic::pinyin_syllables(w).is_empty() {
            return Err(format!("唤醒词 '{w}' 无法匹配（需包含汉字或字母数字）"));
        }
    }
    Ok(())
}

/// VAD threshold rules: sane dBFS range with strict hysteresis (start must
/// sit above end or speech would never close).
pub(super) fn validate_vad(v: &VadSettings) -> Result<(), String> {
    if !(-80.0..=0.0).contains(&v.start_db) || !(-80.0..=0.0).contains(&v.end_db) {
        return Err("VAD 阈值须在 -80..0 dBFS 范围内".into());
    }
    if v.end_db >= v.start_db {
        return Err(format!(
            "VAD 迟滞无效：end_db ({}) 必须低于 start_db ({})",
            v.end_db, v.start_db
        ));
    }
    Ok(())
}

pub(super) struct LoopbackProbeGuard;

impl Drop for LoopbackProbeGuard {
    fn drop(&mut self) {
        VoicePipeline::arm_loopback_probe(false);
    }
}
