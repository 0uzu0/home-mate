use super::transport_metrics::{OutboundMetrics, OUTBOUND_CONTROL_CAP, OUTBOUND_PLAY_CAP};
use super::*;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutboundSendError {
    Full(&'static str),
    Closed,
}

#[derive(Clone)]
pub(super) struct OutboundSender {
    control_tx: mpsc::Sender<CoreFrame>,
    play_tx: mpsc::Sender<CoreFrame>,
    metrics: Arc<OutboundMetrics>,
}

pub(super) struct OutboundReceiver {
    control_rx: mpsc::Receiver<CoreFrame>,
    play_rx: mpsc::Receiver<CoreFrame>,
}

#[cfg(test)]
pub(super) fn outbound_channel() -> (OutboundSender, OutboundReceiver) {
    outbound_channel_with_metrics(Arc::new(OutboundMetrics::default()))
}

pub(super) fn outbound_channel_with_metrics(
    metrics: Arc<OutboundMetrics>,
) -> (OutboundSender, OutboundReceiver) {
    let (control_tx, control_rx) = mpsc::channel(OUTBOUND_CONTROL_CAP);
    let (play_tx, play_rx) = mpsc::channel(OUTBOUND_PLAY_CAP);
    (
        OutboundSender {
            control_tx,
            play_tx,
            metrics,
        },
        OutboundReceiver {
            control_rx,
            play_rx,
        },
    )
}

impl OutboundSender {
    /// Urgent preemption and link-control frames that must bypass a backed-up
    /// audio chunk queue (`Stop`, `MuteSet`, `VolumeSet`, `HelloAck`, `TimeSet`).
    /// Presentation frames (`AssistantState`) share the play FIFO queue so
    /// `AssistantState { state: "speaking" }` never overtakes the `PlayBegin`
    /// that triggered it.
    fn is_priority_control_frame(frame: &CoreFrame) -> bool {
        matches!(
            frame,
            CoreFrame::HelloAck { .. }
                | CoreFrame::HelloReject { .. }
                | CoreFrame::TimeSet { .. }
                | CoreFrame::Stop { .. }
                | CoreFrame::MuteSet { .. }
                | CoreFrame::VolumeSet { .. }
        )
    }

    pub fn queued_control(&self) -> usize {
        self.control_tx
            .max_capacity()
            .saturating_sub(self.control_tx.capacity())
    }

    pub fn queued_play(&self) -> usize {
        self.play_tx
            .max_capacity()
            .saturating_sub(self.play_tx.capacity())
    }

    pub fn send(&self, frame: CoreFrame) -> Result<(), OutboundSendError> {
        if !Self::is_priority_control_frame(&frame) {
            match self.play_tx.try_send(frame) {
                Ok(()) => {
                    self.metrics.observe_play_depth(self.queued_play());
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.metrics.play_dropped.fetch_add(1, Ordering::SeqCst);
                    Err(OutboundSendError::Full("play"))
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(OutboundSendError::Closed),
            }
        } else {
            match self.control_tx.try_send(frame) {
                Ok(()) => {
                    self.metrics.observe_control_depth(self.queued_control());
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.metrics.control_dropped.fetch_add(1, Ordering::SeqCst);
                    Err(OutboundSendError::Full("control"))
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(OutboundSendError::Closed),
            }
        }
    }
}

impl OutboundReceiver {
    pub async fn recv(&mut self) -> Option<CoreFrame> {
        tokio::select! {
            biased;
            ctrl = self.control_rx.recv() => match ctrl {
                Some(f) => Some(f),
                None => self.play_rx.recv().await,
            },
            play = self.play_rx.recv() => match play {
                Some(f) => Some(f),
                None => self.control_rx.recv().await,
            },
        }
    }

    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<CoreFrame, mpsc::error::TryRecvError> {
        match self.control_rx.try_recv() {
            Ok(f) => Ok(f),
            Err(mpsc::error::TryRecvError::Empty) => self.play_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected) => match self.play_rx.try_recv() {
                Ok(f) => Ok(f),
                Err(e) => Err(e),
            },
        }
    }
}

impl DeviceHub {
    // --- outbound API used by the pipeline and tools ---------------------------

    fn resolve_device_outbound(
        st: &HubState,
        device: &str,
    ) -> Result<(u64, OutboundSender), String> {
        if let Some(&conn_id) = st.device_to_conn.get(device) {
            if let Some(conn) = st.connections.get(&conn_id) {
                return Ok((conn_id, conn.outbound.clone()));
            }
        }
        if st.connections.is_empty() {
            return Err("no gateway connected".into());
        }
        if let Some(slot) = st.devices.get(device) {
            if !slot.online {
                return Err(format!("device '{device}' is offline"));
            }
            if let Some(conn) = st.connections.get(&slot.generation) {
                return Ok((slot.generation, conn.outbound.clone()));
            }
            return Err(format!("no live gateway for device '{device}'"));
        }
        // Test-only fallback for unit tests that attach one synthetic outbound
        // channel without registering a HelloDevice. Never runs in production
        // or when any real gateway has registered devices.
        #[cfg(test)]
        if st.connections.len() == 1 {
            if let Some((&conn_id, conn)) = st.connections.iter().next() {
                if conn.devices.is_empty() {
                    return Ok((conn_id, conn.outbound.clone()));
                }
            }
        }
        Err(format!("no live gateway for device '{device}'"))
    }

    fn send_via_outbound(&self, tx: &OutboundSender, frame: CoreFrame) -> Result<(), String> {
        match tx.send(frame) {
            Ok(()) => Ok(()),
            Err(OutboundSendError::Full(tier)) => {
                let mut st = self.lock_state();
                st.record(
                    "outbound_backpressure",
                    json!({
                        "tier": tier,
                        "control_queued": tx.queued_control(),
                        "play_queued": tx.queued_play(),
                        "control_dropped": self.transport_tasks.outbound.control_dropped.load(Ordering::SeqCst),
                        "play_dropped": self.transport_tasks.outbound.play_dropped.load(Ordering::SeqCst),
                    }),
                );
                Err(format!("gateway outbound {tier} queue full"))
            }
            Err(OutboundSendError::Closed) => Err("gateway connection closed".into()),
        }
    }

    pub(super) fn send_to_device(&self, device: &str, frame: CoreFrame) -> Result<(), String> {
        let tx = {
            let st = self.lock_state();
            Self::resolve_device_outbound(&st, device)?.1
        };
        self.send_via_outbound(&tx, frame)
    }

    pub(super) fn send_to_play(&self, play_id: &str, frame: CoreFrame) -> Result<(), String> {
        let tx = {
            let st = self.lock_state();
            if let Some(play) = st.active_plays.get(play_id) {
                st.connections
                    .get(&play.generation)
                    .map(|c| c.outbound.clone())
                    .ok_or_else(|| "gateway connection closed".to_string())?
            } else if st.connections.is_empty() {
                return Err("no gateway connected".into());
            } else {
                return Err(format!("no active gateway for play '{play_id}'"));
            }
        };
        self.send_via_outbound(&tx, frame)
    }

    pub(super) fn device_generation(st: &HubState, device: &str) -> u64 {
        st.device_to_conn
            .get(device)
            .copied()
            .or_else(|| st.devices.get(device).map(|d| d.generation))
            .unwrap_or(st.generation)
    }

    /// Submit a play; returns a ticket whose receipt resolves on the first
    /// terminal stage (device_confirmed / failed / unknown). The play id is
    /// fresh per submission so concurrent plays never collide. The play is
    /// registered as active until a terminal/stop/finish receipt retires it,
    /// so stops can reach it whatever channel it runs on.
    /// Send a WifiConfig change and return a waiter for the watchdog's
    /// WifiState reply (oneshot per device; a second concurrent change
    /// replaces the first waiter).
    pub fn wifi_config(
        &self,
        device: &str,
        ssid: String,
        psk: String,
    ) -> Result<oneshot::Receiver<DeviceEvent>, String> {
        // v1 mechanism: a privileged frame only goes to a device that
        // declares the capability. An undeclared device keeps every OTHER
        // function — this is an operation-level refusal, never a disconnect.
        if !self.device_supports(device, "wifi") {
            return Err(format!("device '{device}' has no wifi capability"));
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.lock_state();
            let gen_id = Self::device_generation(&st, device);
            st.pending_wifi.insert(device.to_string(), tx);
            st.pending_wifi_gen.insert(device.to_string(), gen_id);
        }
        if let Err(e) = self.send_to_device(device, CoreFrame::WifiConfig { ssid, psk }) {
            let mut st = self.lock_state();
            st.pending_wifi.remove(device);
            st.pending_wifi_gen.remove(device);
            return Err(e);
        }
        Ok(rx)
    }

    /// v1 mechanism (#8/#13): set the device's mute latch. Returns the
    /// task id and a waiter for the correlated `MuteState` fact — the
    /// DEVICE's receipt is the only success proof (a send is not an
    /// application). The waiter resolves when a MuteState naming this
    /// task_id arrives on the SAME connection generation; a key press, a
    /// boot report, or a receipt that crossed a generation settles nothing,
    /// and the caller's timeout then reports an honest `unknown`.
    pub fn mute_set(
        &self,
        device: &str,
        muted: bool,
    ) -> Result<(String, oneshot::Receiver<GatewayFrame>), String> {
        if !self.device_supports(device, "remote_mute") {
            return Err(format!("device '{device}' has no remote_mute capability"));
        }
        let task_id = id::next("M");
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.lock_state();
            // Codex audit P1: a cached descriptor is not an online device.
            // A mute command to an offline speaker would silently do nothing
            // and time out — refuse it up front with the real reason.
            if !st.devices.get(device).is_some_and(|d| d.online) {
                return Err(format!("device '{device}' is offline"));
            }
            st.record(
                "mute_request",
                json!({ "device": device, "muted": muted, "task_id": task_id }),
            );
            let generation = Self::device_generation(&st, device);
            st.pending.insert(
                task_id.clone(),
                PendingWaiter {
                    sender: tx,
                    generation,
                },
            );
        }
        let frame = CoreFrame::MuteSet {
            task_id: task_id.clone(),
            device: device.to_string(),
            muted,
        };
        if let Err(e) = self.send_to_device(device, frame) {
            let mut st = self.lock_state();
            st.pending.remove(&task_id);
            return Err(e);
        }
        Ok((task_id, rx))
    }

    /// (#35): send LED ring configuration (`CoreFrame::LedConfig`) to the gateway.
    pub fn send_led_config(
        &self,
        device: &str,
        night_enabled: bool,
        night_start_minute: u16,
        night_end_minute: u16,
        night_brightness_pct: u8,
        orientation_rotation: u8,
        orientation_reverse: bool,
    ) -> Result<(), String> {
        self.send_to_device(
            device,
            CoreFrame::LedConfig {
                device: device.to_string(),
                night_enabled,
                night_start_minute,
                night_end_minute,
                night_brightness_pct,
                orientation_rotation,
                orientation_reverse,
            },
        )
    }

    /// Update the media trim (dB, ≤0). Called on startup and on every
    /// accepted PUT /mode so a saved value takes effect without a restart.
    pub fn set_media_trim_db(&self, db: f64) {
        *crate::lock_or_recover(&self.media_trim_db, "hub.media_trim_db") = db.clamp(-12.0, 0.0);
    }

    pub fn media_trim_db(&self) -> f64 {
        *crate::lock_or_recover(&self.media_trim_db, "hub.media_trim_db")
    }

    pub fn play(&self, req: PlayRequest) -> Result<PlayTicket, String> {
        let play_id = id::next("P");
        let (tx, rx) = oneshot::channel();
        let (generation, outbound) = {
            let mut st = self.lock_state();
            let (generation, outbound) = Self::resolve_device_outbound(&st, &req.device)?;
            st.pending.insert(
                play_id.clone(),
                PendingWaiter {
                    sender: tx,
                    generation,
                },
            );
            st.active_plays.insert(
                play_id.clone(),
                ActivePlay {
                    device: req.device.clone(),
                    channel: req.channel.clone(),
                    generation,
                },
            );
            (generation, outbound)
        };
        let _ = generation;
        if let Some(wav) = req.audio {
            use base64::Engine;
            let wav = if req.channel == "media" {
                let db = self.media_trim_db();
                if db < 0.0 {
                    crate::speech::wav_gain_db(&wav, db)
                } else {
                    wav
                }
            } else {
                wav
            };
            let (sample_rate, channels, pcm) = match crate::speech::wav_pcm(&wav) {
                Some((sr, ch, pcm)) => (sr, ch, pcm),
                None => (
                    req.audio_meta.sample_rate.max(8_000),
                    req.audio_meta.channels.max(1),
                    wav.as_slice(),
                ),
            };
            let cleanup = || {
                let mut st = self.lock_state();
                st.pending.remove(&play_id);
                st.active_plays.remove(&play_id);
                st.stop_sent.remove(&play_id);
            };
            let begin = CoreFrame::PlayBegin {
                task_id: play_id.clone(),
                device: req.device,
                content_ref: req.content_ref,
                audio: AudioMeta {
                    format: "pcm_s16le".into(),
                    sample_rate,
                    channels,
                },
                channel: Some(req.channel),
                text: req.text,
                barge_in: Some("stop_current".into()),
            };
            if let Err(e) = self.send_via_outbound(&outbound, begin) {
                cleanup();
                return Err(e);
            }
            let frame_bytes = (channels.max(1) as usize) * 2;
            let chunk_bytes = (8192 / frame_bytes).max(1) * frame_bytes;
            for (seq, chunk) in pcm.chunks(chunk_bytes).enumerate() {
                let frame = CoreFrame::PlayChunk {
                    task_id: play_id.clone(),
                    seq: seq as u64,
                    audio_b64: base64::engine::general_purpose::STANDARD.encode(chunk),
                };
                if let Err(e) = self.send_via_outbound(&outbound, frame) {
                    cleanup();
                    return Err(e);
                }
            }
            if let Err(e) = self.send_via_outbound(
                &outbound,
                CoreFrame::PlayEnd {
                    task_id: play_id.clone(),
                },
            ) {
                cleanup();
                return Err(e);
            }
            return Ok(PlayTicket {
                play_id,
                receipt: rx,
            });
        }
        let frame = CoreFrame::Play {
            task_id: play_id.clone(),
            device: req.device,
            content_ref: req.content_ref,
            audio: req.audio_meta,
            audio_b64: None,
            channel: Some(req.channel),
            text: req.text,
            simulated_ms: req.simulated_ms,
            barge_in: Some("stop_current".into()),
        };
        if let Err(e) = self.send_via_outbound(&outbound, frame) {
            let mut st = self.lock_state();
            st.pending.remove(&play_id);
            st.active_plays.remove(&play_id);
            st.stop_sent.remove(&play_id);
            return Err(e);
        }
        Ok(PlayTicket {
            play_id,
            receipt: rx,
        })
    }

    /// Does the (last-seen) descriptor for this device advertise a
    /// capability? The v0.3 stream gate (`play_stream`) and any future
    /// additive frame consult this before sending.
    pub fn device_supports(&self, device: &str, cap: &str) -> bool {
        let st = self.lock_state();
        st.devices
            .get(device)
            .map(|d| d.descriptor.capabilities.iter().any(|c| c == cap))
            .unwrap_or(false)
    }

    /// v0.3: open a streamed play. Registers pending/active exactly like
    /// [`play`] so stops and receipts find it; the ticket resolves on the
    /// first terminal stage (device_confirmed after the device drained the
    /// last chunk, or failed). Errors when the device lacks `play_stream`
    /// — callers fall back to whole-WAV `play`.
    pub fn play_begin(
        &self,
        device: &str,
        channel: &str,
        sample_rate: u32,
        text: Option<String>,
    ) -> Result<PlayTicket, String> {
        self.play_begin_ref(device, channel, sample_rate, text, None)
    }

    /// v0.3 with an explicit provenance ref: music needs `music:<source>:<id>`
    /// so a trace says what was played (the TTS default is meaningless there).
    pub fn play_begin_ref(
        &self,
        device: &str,
        channel: &str,
        sample_rate: u32,
        text: Option<String>,
        content_ref: Option<String>,
    ) -> Result<PlayTicket, String> {
        if !self.device_supports(device, "play_stream") {
            return Err(format!("device '{device}' has no play_stream capability"));
        }
        let play_id = id::next("P");
        let (tx, rx) = oneshot::channel();
        let outbound = {
            let mut st = self.lock_state();
            let (generation, outbound) = Self::resolve_device_outbound(&st, device)?;
            st.pending.insert(
                play_id.clone(),
                PendingWaiter {
                    sender: tx,
                    generation,
                },
            );
            st.active_plays.insert(
                play_id.clone(),
                ActivePlay {
                    device: device.to_string(),
                    channel: channel.to_string(),
                    generation,
                },
            );
            outbound
        };
        let frame = CoreFrame::PlayBegin {
            task_id: play_id.clone(),
            device: device.to_string(),
            content_ref: content_ref.unwrap_or_else(|| format!("tts:stream-{}", id::next("V"))),
            audio: AudioMeta {
                format: "pcm_s16le".into(),
                sample_rate,
                channels: 1,
            },
            channel: Some(channel.to_string()),
            text,
            barge_in: Some("stop_current".into()),
        };
        if let Err(e) = self.send_via_outbound(&outbound, frame) {
            let mut st = self.lock_state();
            st.pending.remove(&play_id);
            st.active_plays.remove(&play_id);
            st.stop_sent.remove(&play_id);
            return Err(e);
        }
        Ok(PlayTicket {
            play_id,
            receipt: rx,
        })
    }

    /// v0.3: append raw S16LE PCM bytes to an open stream. The caller owns
    /// the seq (monotonic from 0, exactly +1 per chunk).
    pub fn play_chunk(&self, play_id: &str, seq: u64, pcm: &[u8]) -> Result<(), String> {
        use base64::Engine;
        self.send_to_play(
            play_id,
            CoreFrame::PlayChunk {
                task_id: play_id.to_string(),
                seq,
                audio_b64: base64::engine::general_purpose::STANDARD.encode(pcm),
            },
        )
    }

    /// v0.3: close a stream (the last chunk was already sent). The device
    /// finishes per its EOS semantics and answers device_confirmed.
    pub fn play_end(&self, play_id: &str) -> Result<(), String> {
        self.send_to_play(
            play_id,
            CoreFrame::PlayEnd {
                task_id: play_id.to_string(),
            },
        )
    }

    /// Send Stop for every active play on a device (any channel). Returns
    /// the play ids the stop requests were submitted for; confirmations
    /// arrive later as stop_receipts.
    pub fn stop_all_plays(&self, device: &str) -> Vec<String> {
        let ids: Vec<String> = {
            let st = self.lock_state();
            st.active_plays
                .iter()
                .filter(|(id, p)| p.device == device && !st.stop_sent.contains(*id))
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in &ids {
            if self.stop(id).is_err() {
                tracing::warn!(play = %id, "stop submit failed");
            }
        }
        ids
    }

    /// Drop a device's active plays from the registry WITHOUT asking the
    /// device to stop (R1d). The device already cut the channel on its own
    /// near-end reflex; this is the bookkeeping half only, so no Stop is put
    /// behind the core's TTS queue (the exact 1.9 s the local reflex removed).
    pub fn forget_plays(&self, device: &str) -> Vec<String> {
        let mut st = self.lock_state();
        let ids: Vec<String> = st
            .active_plays
            .iter()
            .filter(|(_, p)| p.device == device)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            st.active_plays.remove(id);
            st.stop_sent.remove(id);
            Self::resolve_stop_waiters(&mut st, id, StopOutcome::unknown("forgotten_play"));
        }
        ids
    }

    /// Forget ONE play without asking the device to stop. The core uses this
    /// when no receipt can arrive any more (device stalled after `play_end`,
    /// or a local stop already cleared the channel): keeping the entry would
    /// leave `activity`/LED claiming the device is speaking.
    pub fn forget_play(&self, play_id: &str) {
        let mut st = self.lock_state();
        st.active_plays.remove(play_id);
        st.stop_sent.remove(play_id);
        st.pending.remove(play_id);
        Self::resolve_stop_waiters(&mut st, play_id, StopOutcome::unknown("forgotten_play"));
    }

    /// Active plays on a device (voice + media) as (play_id, info), for
    /// status views.
    /// Is this play still live? A stop receipt (or the device's own
    /// PlayFinished) removes it — that is the cancellation signal a media
    /// streamer polls between chunks, so a stop releases the decoder and the
    /// HTTP response instead of draining the rest of the song.
    pub fn play_active(&self, play_id: &str) -> bool {
        self.lock_state().active_plays.contains_key(play_id)
    }

    pub fn active_plays_for(&self, device: &str) -> Vec<(String, ActivePlay)> {
        let st = self.lock_state();
        let mut v: Vec<(String, ActivePlay)> = st
            .active_plays
            .iter()
            .filter(|(_, p)| p.device == device)
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect();
        v.sort_by(|a, b| a.1.channel.cmp(&b.1.channel));
        v
    }

    /// Which device/channel a play id belongs to. The waiting cue's policy
    /// (R1g/P4) needs to know WHAT started besides the cue — media starts
    /// are not in the pipeline's own output map.
    /// A late accepted/sent receipt for a stopped play has no authority to
    /// supersede a newer cue, even while its stop receipt is outstanding.
    pub fn play_accepts_output(&self, play_id: &str) -> bool {
        let state = self.lock_state();
        state.active_plays.contains_key(play_id) && !state.stop_sent.contains(play_id)
    }

    pub fn play_info(&self, play_id: &str) -> Option<(String, String)> {
        let st = self.lock_state();
        st.active_plays
            .get(play_id)
            .map(|p| (p.device.clone(), p.channel.clone()))
    }

    /// Does the hub know this device id (online or last-seen)?
    pub fn device_exists(&self, device: &str) -> bool {
        self.lock_state().devices.contains_key(device)
    }

    pub fn stop(&self, play_id: &str) -> Result<(), String> {
        self.lock_state().stop_sent.insert(play_id.to_string());
        self.send_to_play(
            play_id,
            CoreFrame::Stop {
                task_id: play_id.into(),
                flush: true,
            },
        )
    }

    /// Register before enqueueing, atomically bound to the play's connection
    /// generation so a reconnect can never interleave and send a stale Stop
    /// onto a newer connection. Only a matching hardware flush can confirm.
    pub fn stop_and_wait(&self, play_id: &str) -> StopSubmission {
        let (tx, receiver) = oneshot::channel();
        let mut st = self.lock_state();
        let Some(play) = st.active_plays.get(play_id) else {
            return StopSubmission::NoActivePlay;
        };
        let generation = play.generation;
        let outbound = match st.connections.get(&generation) {
            Some(conn) => conn.outbound.clone(),
            None if st.connections.is_empty() => {
                return StopSubmission::SendFailed {
                    reason: "no gateway connected".into(),
                };
            }
            None => {
                return StopSubmission::SendFailed {
                    reason: format!(
                        "stale_connection_generation:play={generation},current={}",
                        st.generation
                    ),
                };
            }
        };
        let stop_id = id::next("STOP");
        st.stop_sent.insert(play_id.to_string());
        st.stop_waiters
            .entry(play_id.to_string())
            .or_default()
            .push(tx);
        st.record(
            "stop_submitted",
            json!({"stop_id": stop_id, "play_id": play_id, "connection_generation": generation}),
        );
        if let Err(err) = outbound.send(CoreFrame::Stop {
            task_id: play_id.into(),
            flush: true,
        }) {
            let reason = match err {
                OutboundSendError::Full(tier) => {
                    st.record(
                        "outbound_backpressure",
                        json!({
                            "tier": tier,
                            "play_id": play_id,
                            "control_queued": outbound.queued_control(),
                            "play_queued": outbound.queued_play(),
                        }),
                    );
                    format!("gateway outbound {tier} queue full")
                }
                OutboundSendError::Closed => "gateway connection closed".to_string(),
            };
            st.stop_sent.remove(play_id);
            Self::resolve_stop_waiters(
                &mut st,
                play_id,
                StopOutcome::unknown(format!("send_failed:{reason}")),
            );
            return StopSubmission::SendFailed { reason };
        }
        StopSubmission::Accepted {
            stop_id,
            play_id: play_id.into(),
            connection_generation: generation,
            receiver,
        }
    }

    pub(super) fn resolve_stop_waiters(
        st: &mut HubState,
        play_id: &str,
        outcome: StopOutcome,
    ) -> usize {
        let waiters = st.stop_waiters.remove(play_id).unwrap_or_default();
        let n = waiters.len();
        for tx in waiters {
            let _ = tx.send(outcome.clone());
        }
        n
    }

    /// M6a (旧代回执不结算): true when this receipt belongs to a play from a
    /// DIFFERENT connection generation, or names a play a teardown already
    /// retired. Such a fact is recorded and dropped — the gateway clears its
    /// audio state when its link dies, so a receipt that crosses a generation
    /// is stale evidence, and letting it settle a waiter or emit an event
    /// would turn old state into a new action.
    pub(super) fn receipt_is_stale(
        st: &mut HubState,
        play_id: &str,
        generation: u64,
        payload: serde_json::Value,
    ) -> bool {
        let origin = st
            .active_plays
            .get(play_id)
            .map(|p| p.generation)
            .or_else(|| st.pending.get(play_id).map(|w| w.generation));
        let stale = match origin {
            Some(g) => g != generation,
            // No live record: either an unknown id on a LIVE connection
            // (harmless late fact) or one this hub retired (cross-generation).
            None => st.retired_plays.contains(play_id) || !st.conn_is_live(generation),
        };
        if stale {
            st.record(
                "stale_receipt",
                json!({
                    "play_id": play_id,
                    "origin_generation": origin,
                    "generation": generation,
                    "payload": payload,
                }),
            );
        }
        stale
    }

    /// R1j/P1: hand the compiled keyword file to the device gateway, which
    /// owns asset validation/write/restart. The receipt arrives later.
    pub fn kws_deploy(
        &self,
        device: &str,
        keywords: &str,
        retreat_words: &[String],
        intents_hash: &str,
        threshold: Option<f32>,
        score: Option<f32>,
    ) -> Result<(), String> {
        self.send_to_device(
            device,
            CoreFrame::KwsDeploy {
                device: device.into(),
                keywords: keywords.into(),
                retreat_words: retreat_words.to_vec(),
                intents_hash: intents_hash.into(),
                threshold,
                score,
            },
        )
    }

    /// M6c-2: hand the output tone to one device. The gateway clamps and
    /// applies it live; this is configuration, not a play.
    pub fn audio_eq(
        &self,
        device: &str,
        curve: &str,
        bands: &[(f32, f32, f32)],
    ) -> Result<(), String> {
        self.send_to_device(
            device,
            CoreFrame::AudioEq {
                device: device.into(),
                curve: curve.into(),
                bands: bands
                    .iter()
                    .map(|&(freq, gain_db, q)| EqBand { freq, gain_db, q })
                    .collect(),
            },
        )
    }

    pub fn volume_set(&self, device: &str, value: f64) -> Result<String, String> {
        let play_id = id::next("P");
        {
            let mut st = self.lock_state();
            let gen_id = Self::device_generation(&st, device);
            st.volume_pending
                .insert(play_id.clone(), device.to_string());
            st.volume_pending_gen.insert(play_id.clone(), gen_id);
        }
        if let Err(e) = self.send_to_device(
            device,
            CoreFrame::VolumeSet {
                task_id: play_id.clone(),
                device: device.into(),
                value,
                unit: "linear".into(),
            },
        ) {
            let mut st = self.lock_state();
            st.volume_pending.remove(&play_id);
            st.volume_pending_gen.remove(&play_id);
            return Err(e);
        }
        Ok(play_id)
    }

    pub fn mode_set(&self, device: &str, requested: &str) -> Result<(), String> {
        self.send_to_device(
            device,
            CoreFrame::ModeSet {
                device: device.into(),
                duplex_requested: requested.into(),
            },
        )
    }

    /// v0.2: request a device mic session (start/stop, continuous mode).
    pub fn mic_session(&self, device: &str, action: &str) -> Result<(), String> {
        self.send_to_device(
            device,
            CoreFrame::MicSession {
                device: device.into(),
                action: action.into(),
                mode: Some("continuous".into()),
            },
        )
    }

    /// v0.2 (G4): project the assistant interaction state for a device
    /// (LED presentation mapping lives in the gateway). Fire-and-forget on
    /// purpose: presentation must never back-pressure interaction semantics.
    pub fn assistant_state(&self, device: &str, state: &str) -> Result<(), String> {
        self.send_to_device(
            device,
            CoreFrame::AssistantState {
                device: device.into(),
                state: state.into(),
            },
        )
    }

    /// A device with audio_out capability, currently online: prefers the local
    /// loopback speaker gateway over remote LAN/Android clients, then stable_id.
    pub fn primary_audio_out(&self) -> Option<HelloDevice> {
        let st = self.lock_state();
        let mut devs: Vec<(bool, &DeviceSlot)> = st
            .devices
            .values()
            .filter(|d| d.online && d.descriptor.capabilities.iter().any(|c| c == "audio_out"))
            .map(|d| {
                let is_loopback = st
                    .connections
                    .get(&d.generation)
                    .is_some_and(HubState::conn_is_loopback);
                (is_loopback, d)
            })
            .collect();
        devs.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.descriptor.stable_id.cmp(&b.1.descriptor.stable_id))
        });
        devs.first().map(|(_, d)| d.descriptor.clone())
    }

    /// Every online device with audio_out, sorted — the tone is a device
    /// property, so a push goes to all of them.
    pub fn audio_out_devices(&self) -> Vec<String> {
        let st = self.lock_state();
        let mut devs: Vec<String> = st
            .devices
            .values()
            .filter(|d| d.online && d.descriptor.capabilities.iter().any(|c| c == "audio_out"))
            .map(|d| d.descriptor.stable_id.clone())
            .collect();
        devs.sort();
        devs
    }

    /// The hub's arrival sequence right now. A management command captures
    /// this AFTER sending and then requires a receipt with a greater
    /// sequence — frames that arrived before the command cannot confirm it.
    pub fn frame_seq(&self) -> u64 {
        self.lock_state().frame_seq
    }

    pub fn connected(&self) -> bool {
        !self.lock_state().connections.is_empty()
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let st = self.lock_state();
        let devices: Vec<serde_json::Value> = {
            let mut v: Vec<(bool, &DeviceSlot)> = st
                .devices
                .values()
                .map(|d| {
                    let is_loopback = st
                        .connections
                        .get(&d.generation)
                        .is_some_and(HubState::conn_is_loopback);
                    (is_loopback, d)
                })
                .collect();
            v.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| a.1.descriptor.stable_id.cmp(&b.1.descriptor.stable_id))
            });
            v.into_iter()
                .map(|(is_loopback, d)| {
                    let conn_live = st.conn_is_live(d.generation)
                        && st
                            .device_to_conn
                            .get(&d.descriptor.stable_id)
                            .is_none_or(|&owner| owner == d.generation);
                    json!({
                        "stable_id": d.descriptor.stable_id,
                        "model": d.descriptor.model,
                        "firmware": d.descriptor.firmware,
                        "capabilities": d.descriptor.capabilities,
                        "audio_profile": d.descriptor.audio_profile,
                        "wifi_ssid": d.descriptor.wifi_ssid,
                        "online": d.online,
                        "loopback": is_loopback,
                        "generation": d.generation,
                        "last_seq": d.last_seq,
                        "volume": d.volume,
                        "volume_at_ms": d.volume_at_ms,
                        "volume_seq": d.volume_seq,
                        "aec_state": match d.aec_enabled {
                            Some(true) => "enabled",
                            Some(false) => "disabled",
                            None => "unknown",
                        },
                        "aec_enabled": d.aec_enabled,
                        "aec_tail_ms": d.aec_tail_ms,
                        "effective_tail_samples": d.aec_tail_samples,
                        "aec_sample_rate": d.aec_sample_rate,
                        "aec_provenance": d.aec_provenance,
                        "profile_version": d.profile_version,
                        "echo_health": d.echo_health,
                        "echo_degraded_ms": d.echo_degraded_ms,
                        "echo_health_reason": d.echo_health_reason,
                        "northbound": super::northbound::NorthboundRecord::snapshot(
                            d.northbound.as_ref(), d.generation,
                            d.online && conn_live),
                        "gateway_boot_id": d.gateway_boot_id,
                        "state_revision": d.state_revision,
                        "capability_fingerprint": d.capability_fingerprint,
                        "config_blocks": d.config_blocks.values().collect::<Vec<_>>(),
                    })
                })
                .collect()
        };
        let mut conns: Vec<(&u64, &ConnSlot)> = st.connections.iter().collect();
        conns.sort_by_key(|(&gen_id, _)| gen_id);
        let connections_json: Vec<serde_json::Value> = conns
            .iter()
            .map(|(&gen_id, c)| {
                let mut devs: Vec<&str> = c.devices.iter().map(String::as_str).collect();
                devs.sort();
                json!({
                    "generation": gen_id,
                    "gateway": c.gateway,
                    "gateway_version": c.gateway_version,
                    "protocol_operating": c.protocol_operating,
                    "peer_addr": c.peer_addr.map(|p| p.to_string()),
                    "authenticated": c.authenticated,
                    "devices": devs,
                })
            })
            .collect();
        let (control_queued, play_queued) =
            st.connections
                .values()
                .fold((0usize, 0usize), |(c, p), conn| {
                    (
                        c + conn.outbound.queued_control(),
                        p + conn.outbound.queued_play(),
                    )
                });
        json!({
            "connected": !st.connections.is_empty(),
            "multi_gateway": true,
            "voice_gateway_open": self.voice_gateway_open(),
            "connection_count": st.connections.len(),
            "connections": connections_json,
            "gateway": st.connected_gateway,
            // v1 mechanism: negotiated link facts + the last link-level
            // error the peer REPORTED (cleared by the next good handshake).
            "gateway_version": st.connected_gateway_version,
            "protocol_operating": st.protocol_operating,
            "protocol_min_accepted": crate::DEVICE_PROTOCOL_MIN_VERSION,
            "protocol_self": crate::DEVICE_PROTOCOL_VERSION,
            "protocol_supported_versions": crate::DEVICE_PROTOCOL_SUPPORTED_VERSIONS,
            "last_link_error": st.last_link_error,
            "generation": st.generation,
            "bind_addr": *crate::lock_or_recover(&self.bind_addr, "hub.bind_addr"),
            "devices": devices,
            "pending_plays": st.pending.len(),
            "active_plays": st.active_plays.len(),
            "pending_stop_sent": st.stop_sent.len(),
            "pending_stop_plays": st.stop_waiters.len(),
            "pending_stop_waiters": st.stop_waiters.values().map(Vec::len).sum::<usize>(),
            "pending_wifi": st.pending_wifi.len(),
            "pending_volume": st.volume_pending.len(),
            "outbound_queue": {
                "control_capacity": OUTBOUND_CONTROL_CAP,
                "play_capacity": OUTBOUND_PLAY_CAP,
                "control_queued": control_queued,
                "play_queued": play_queued,
                "control_high_water": self.transport_tasks.outbound.control_high_water.load(Ordering::SeqCst),
                "play_high_water": self.transport_tasks.outbound.play_high_water.load(Ordering::SeqCst),
                "control_dropped": self.transport_tasks.outbound.control_dropped.load(Ordering::SeqCst),
                "play_dropped": self.transport_tasks.outbound.play_dropped.load(Ordering::SeqCst),
            },
            "transport_tasks": self.transport_tasks.snapshot(),
        })
    }

    pub fn device_aec_tails(&self) -> Vec<(String, u32)> {
        let st = self.lock_state();
        st.devices
            .iter()
            .filter_map(|(id, slot)| slot.aec_tail_ms.map(|ms| (id.clone(), ms)))
            .collect()
    }

    pub fn recent(&self, limit: usize) -> Vec<serde_json::Value> {
        let st = self.lock_state();
        st.ring.iter().rev().take(limit).cloned().collect()
    }
}
