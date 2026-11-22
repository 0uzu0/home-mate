//! The media session: the queue, the play position and the device handshake.
//!
//! A session outlives one `play` call — the queue and the position survive a
//! pause, so 「继续」 plays the same song from the same place and 「下一首」 never
//! re-searches. `start` returns as soon as the DEVICE confirms the audio began;
//! the pump then streams at a bounded lead in the background, and any stop
//! (device, tool, cancel flag) releases the decoder.

use serde_json::{json, Value};
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::gateway::{PlayTicket, StopOutcome, StopSubmission};
use crate::tool::{Evidence, ToolCtx};

use super::decode::{spawn_decoder, PcmBlock};
use super::remote::open_stream;
use super::{
    describe, MediaSession, MediaState, MusicError, MusicRuntime, RepeatMode, Source, Track,
    MEDIA_SESSION_TTL_MS, PLAY_AHEAD_SECS, PUSH_CHUNK_SECS,
};

/// Pure repeat decision for natural end-of-track transitions (music only).
pub(crate) fn next_repeat_index(session: &MediaSession) -> Option<usize> {
    let track = session.current()?;
    if !matches!(track.source, Source::Library | Source::Online) {
        return None;
    }
    match session.repeat {
        RepeatMode::Off => None,
        RepeatMode::One => Some(session.index),
        RepeatMode::All => {
            (!session.tracks.is_empty()).then(|| (session.index + 1) % session.tracks.len())
        }
    }
}

impl MusicRuntime {
    /// Upper bound on how long a `wake_takeover` / `pause_now` stop receipt may
    /// remain in flight on `DeviceHub` (`pause_now` settles within 2 s).
    pub const MEDIA_PAUSE_FLUSH_WINDOW_MS: u64 = 3_000;

    fn media_epoch(&self, device: &str) -> u64 {
        crate::lock_or_recover(&self.media_epochs, "music.media_epochs")
            .get(device)
            .copied()
            .unwrap_or(0)
    }

    fn bump_media_epoch(&self, device: &str) -> u64 {
        let mut epochs = crate::lock_or_recover(&self.media_epochs, "music.media_epochs");
        let slot = epochs.entry(device.to_string()).or_insert(0);
        *slot += 1;
        *slot
    }

    /// Open the streamed play, start the background decoder/pusher, and wait
    /// for the device's confirmation. Returns as soon as the DEVICE says the
    /// audio started — a song must not hold the turn open for four minutes.
    pub(super) async fn start(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        start_ms: u64,
        resp: reqwest::Response,
        ctx: &ToolCtx,
    ) -> Result<(String, Evidence, String), (String, String)> {
        self.start_with_repeat(device, tracks, index, start_ms, resp, ctx, RepeatMode::Off)
            .await
    }

    pub(super) async fn start_with_repeat(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        start_ms: u64,
        resp: reqwest::Response,
        ctx: &ToolCtx,
        repeat: RepeatMode,
    ) -> Result<(String, Evidence, String), (String, String)> {
        self.start_with_repeat_guarded(device, tracks, index, start_ms, resp, ctx, repeat, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start_with_repeat_guarded(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        start_ms: u64,
        resp: reqwest::Response,
        ctx: &ToolCtx,
        repeat: RepeatMode,
        repeat_guard: Option<(u64, String)>,
    ) -> Result<(String, Evidence, String), (String, String)> {
        let Some(track) = tracks.get(index).cloned() else {
            return Err(("MUSIC_NO_NEXT".into(), "这一首不存在".into()));
        };
        if ctx.is_cancelled() {
            return Err(("CANCELLED".into(), "播放已撤销".into()));
        }
        // Probe the stream first: the device must be told the real rate.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PcmBlock>(64);
        let is_live = tracks.get(index).is_some_and(|t| t.source == Source::Radio);
        // Diagnostic (serial 93): a repeat advance that fails at the decoder
        // must say WHAT came back, not just "unsupported format". Without the
        // status and content-type an error page and a broken MP3 look alike.
        let opened = json!({
            "device": device,
            "track": track.title,
            "source": track.source.as_str(),
            "status": resp.status().as_u16(),
            "content_type": resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or(""),
            "content_length": resp.content_length(),
            "advance": repeat_guard.is_some(),
        });
        self.note("music_stream_open", opened);
        let stream_err = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let decoder = spawn_decoder(resp, tx, is_live, stream_err.clone());
        let first = tokio::time::timeout(Duration::from_secs(12), rx.recv())
            .await
            .map_err(|_| ("MUSIC_DECODE".to_string(), "解码超时：没有播放".to_string()))?
            .ok_or_else(|| {
                (
                    "MUSIC_DECODE".to_string(),
                    "音频流无法解码（格式不支持）：没有播放".to_string(),
                )
            })?;
        if first.pcm.is_empty() {
            return Err(("MUSIC_DECODE".into(), "音频流是空的：没有播放".into()));
        }
        let rate = first.sample_rate;
        // The device's `accepted` stage is an EVENT, not the terminal receipt:
        // the ticket resolves only on device_confirmed/failed/unknown, and a
        // song's device_confirmed arrives when the stream has DRAINED (minutes
        // later). Subscribe first so the stage cannot be missed.
        let mut stages = self.hub.subscribe_control();
        let lifecycle_events = stages.resubscribe();
        let (_ticket, play_id) = {
            let _controls = crate::lock_or_recover(&self.controls, "music.controls");
            let mut m = crate::lock_or_recover(&self.media, "music.media");
            if ctx.is_cancelled() {
                return Err(("CANCELLED".into(), "播放已撤销".into()));
            }
            if let Some((expected_epoch, expected_play_id)) = repeat_guard.as_ref() {
                let epoch_ok = self.media_epoch(&device) == *expected_epoch;
                let session_ok = m.get(&device).is_some_and(|s| {
                    s.state == MediaState::Playing
                        && s.play_id.as_deref() == Some(expected_play_id.as_str())
                });
                if !epoch_ok || !session_ok {
                    return Err(("CANCELLED".into(), "循环推进已被暂停或停止打断".into()));
                }
            }
            let ticket: PlayTicket = self
                .hub
                .play_begin_ref(
                    &device,
                    "media",
                    rate,
                    Some(format!("{} — {}", track.title, track.artist)),
                    Some(format!("music:{}:{}", track.source.as_str(), track.id)),
                )
                .map_err(|e| ("PLAY_FAILED".to_string(), e))?;
            let play_id = ticket.play_id.clone();
            let now = crate::id::now_ms();
            {
                m.insert(
                    device.clone(),
                    MediaSession {
                        tracks: tracks.clone(),
                        index,
                        state: MediaState::Playing,
                        position_ms: start_ms,
                        play_id: Some(play_id.clone()),
                        started_ms: now,
                        base_ms: start_ms,
                        updated_ms: now,
                        repeat,
                    },
                );
            }
            self.bump_media_epoch(&device);
            self.register_playback(&device, &play_id);
            (ticket, play_id)
        };
        let stream_ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.supervise_playback(
            device.clone(),
            play_id.clone(),
            lifecycle_events,
            ctx.cancel_flag(),
            stream_ended.clone(),
        );
        self.note(
            "music_play_start",
            json!({ "device": device, "play_id": play_id, "source": track.source.as_str(),
                    "id": track.id, "title": track.title, "sample_rate": rate,
                    "start_ms": start_ms, "index": index, "tracks": tracks.len(),
                    "repeat": repeat.as_str() }),
        );

        // Background: feed the decoder output at a bounded lead, stop when the
        // device stopped the play (any stop) or the task is cancelled.
        {
            let rt = self.clone();
            let device = device.clone();
            let play_id = play_id.clone();
            let cancel = ctx.cancel_flag();
            tokio::spawn(async move {
                rt.pump(
                    device,
                    play_id,
                    rate,
                    start_ms,
                    first,
                    rx,
                    cancel,
                    stream_ended,
                    stream_err,
                )
                .await;
                let _ = decoder.await;
            });
        }

        // Wait for the device's `streaming`/`device_confirmed` stage before
        // returning (terminal confirmation arrives only when the track drains).
        let mut accepted: Option<Option<String>> = None;
        let started = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match stages.recv().await {
                    Ok(crate::gateway::DeviceEvent::PlayStage {
                        play_id: id,
                        stage,
                        detail,
                    }) if id == play_id => {
                        match stage.as_str() {
                            // R2-BM5/audit: `accepted` is the device's BEGIN
                            // validation, not evidence that audio reached the
                            // speaker. `streaming` = the first block was
                            // consumed; that is the first stage that may back
                            // an 「已开始播放」 fact. Keep waiting for it.
                            "streaming" | "device_confirmed" => return Ok((stage, detail)),
                            "failed" | "unknown" => return Err((stage, detail)),
                            "accepted" => {
                                accepted = Some(detail.clone());
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return Err(("closed".to_string(), None)),
                }
            }
        })
        .await;
        match started {
            Ok(Ok((stage, detail))) if stage == "device_confirmed" => Ok((
                play_id,
                Evidence::DeviceConfirmed,
                detail.unwrap_or_else(|| "device_confirmed".into()),
            )),
            Ok(Ok((_stage, detail))) => Ok((
                play_id.clone(),
                Evidence::Sent,
                format!(
                    "device streaming{}",
                    detail.map(|d| format!(": {d}")).unwrap_or_default()
                ),
            )),
            Ok(Err((stage, detail))) => Ok((
                play_id.clone(),
                Evidence::Submitted,
                format!(
                    "device reported '{stage}'{}",
                    detail.map(|d| format!(": {d}")).unwrap_or_default()
                ),
            )),
            Err(_) => match accepted {
                // The begin was validated but the device never reported the
                // first block: the stream MAY be playing. Unknown, not success.
                Some(detail) => Ok((
                    play_id,
                    Evidence::Submitted,
                    format!(
                        "device accepted the begin but reported no streaming{}",
                        detail.map(|d| format!(": {d}")).unwrap_or_default()
                    ),
                )),
                None => Ok((
                    play_id,
                    Evidence::Unknown,
                    "no confirmation within budget".into(),
                )),
            },
        }
    }

    /// Push decoded PCM to the device, no more than `PLAY_AHEAD_SECS` ahead of
    /// the speaker's own clock. Ends the stream on the last block.
    #[allow(clippy::too_many_arguments)]
    async fn pump(
        self: Arc<Self>,
        device: String,
        play_id: String,
        rate: u32,
        start_ms: u64,
        first: PcmBlock,
        mut rx: tokio::sync::mpsc::Receiver<PcmBlock>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        stream_ended: Arc<std::sync::atomic::AtomicBool>,
        stream_err: Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;
        let chunk_samples = ((rate as f64 * PUSH_CHUNK_SECS) as usize).max(320);
        let started = std::time::Instant::now();
        let mut last_progress = started;
        let idle_timeout =
            Duration::from_millis(self.stream_idle_timeout_ms.load(Ordering::SeqCst));
        // M3 resume: fetch from the start and discard the first `start_ms` of
        // PCM so every source/format supports resume without server-side seek.
        let to_skip: u64 = start_ms.saturating_mul(rate as u64) / 1000;
        let mut skipped: u64 = 0;
        // Samples PUSHED by this play (lead control), and the absolute
        // position (start offset + pushed) the session reports.
        let mut pushed: u64 = 0;
        let mut seq: u64 = 0;
        let mut pending: Vec<i16> = Vec::with_capacity(chunk_samples * 2);
        let mut first_block = Some(first);
        let mut ended = false;
        let total_ms = |pushed: u64| start_ms + pushed * 1000 / rate as u64;

        loop {
            let cancelled = cancel.load(Ordering::SeqCst);
            if cancelled || !self.hub.play_active(&play_id) {
                self.note(
                    "music_play_stopped",
                    json!({ "device": device, "play_id": play_id, "sent_ms": total_ms(pushed), "cancelled": cancelled }),
                );
                if cancelled {
                    if self.hub.play_accepts_output(&play_id) {
                        let _ = self.hub.stop(&play_id);
                    }
                } else {
                    let _ = self.hub.play_end(&play_id);
                }
                self.forget(&device, &play_id);
                return;
            }
            // `recv() == None` is the only "decoder finished" signal (`is_closed()`
            // can be true while decoded blocks are still buffered in `rx`).
            enum Step {
                Block(PcmBlock),
                Idle,
                Done,
            }
            let step = match first_block.take() {
                Some(b) => Step::Block(b),
                None => tokio::select! {
                    biased;
                    b = rx.recv() => match b {
                        Some(b) => Step::Block(b),
                        None => Step::Done,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(100)) => Step::Idle,
                },
            };
            let mut pcm = match step {
                Step::Block(PcmBlock { pcm, .. }) => {
                    last_progress = std::time::Instant::now();
                    pcm
                }
                Step::Idle => {
                    if last_progress.elapsed() >= idle_timeout {
                        self.note(
                            "music_stream_stalled",
                            json!({ "device": device, "play_id": play_id, "sent_ms": total_ms(pushed) }),
                        );
                        if self.hub.play_accepts_output(&play_id) {
                            let _ = self.hub.stop(&play_id);
                        }
                        self.forget(&device, &play_id);
                        return;
                    }
                    continue;
                }
                Step::Done => {
                    if stream_err.load(Ordering::SeqCst) {
                        self.note(
                            "music_stream_error",
                            json!({ "device": device, "play_id": play_id, "sent_ms": total_ms(pushed) }),
                        );
                        if self.hub.play_accepts_output(&play_id) {
                            let _ = self.hub.stop(&play_id);
                        }
                        self.forget(&device, &play_id);
                        return;
                    }
                    if !ended {
                        // Decoder finished: the remainder goes out at the same
                        // lead. Pushing it all at once fills the device buffer
                        // with minutes of audio, which makes a stop inaudible.
                        while !pending.is_empty() {
                            if cancel.load(Ordering::SeqCst) || !self.hub.play_active(&play_id) {
                                break;
                            }
                            let ahead =
                                pushed as f64 / rate as f64 - started.elapsed().as_secs_f64();
                            if ahead > PLAY_AHEAD_SECS {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                continue;
                            }
                            let take = pending.len().min(chunk_samples);
                            let chunk: Vec<i16> = pending.drain(..take).collect();
                            if self.push(&play_id, &mut seq, &chunk).is_err() {
                                break;
                            }
                            pushed += chunk.len() as u64;
                            self.record_progress(&device, &play_id, total_ms(pushed));
                        }
                        let cancelled = cancel.load(Ordering::SeqCst);
                        if cancelled || !self.hub.play_active(&play_id) {
                            self.note(
                                "music_play_stopped",
                                json!({ "device": device, "play_id": play_id, "sent_ms": total_ms(pushed), "cancelled": cancelled }),
                            );
                            if cancelled && self.hub.play_accepts_output(&play_id) {
                                let _ = self.hub.stop(&play_id);
                            }
                            self.forget(&device, &play_id);
                            return;
                        }
                        if self.hub.play_end(&play_id).is_ok() {
                            stream_ended.store(true, Ordering::SeqCst);
                            self.note(
                                "music_play_finished",
                                json!({ "device": device, "play_id": play_id, "sent_ms": total_ms(pushed) }),
                            );
                            ended = true;
                        }
                    }
                    // `supervise_playback` is the single deterministic owner of
                    // post-stream drain, `PlayFinished` repeat advancement, and
                    // final `forget` once `play_end` has been accepted.
                    if !ended {
                        self.forget(&device, &play_id);
                    }
                    return;
                }
            };
            if skipped < to_skip {
                let drop = (to_skip - skipped).min(pcm.len() as u64) as usize;
                pcm.drain(..drop);
                skipped += drop as u64;
                if pcm.is_empty() {
                    continue;
                }
            }
            pending.extend_from_slice(&pcm);
            while pending.len() >= chunk_samples {
                // Lead control: wall clock since start is the speaker's
                // position (the device answers stops in ~40 ms; nothing else
                // reports playhead). Only what THIS play pushed counts — the
                // resume offset is already behind us.
                let ahead = pushed as f64 / rate as f64 - started.elapsed().as_secs_f64();
                if ahead > PLAY_AHEAD_SECS {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                let chunk: Vec<i16> = pending.drain(..chunk_samples).collect();
                if self.push(&play_id, &mut seq, &chunk).is_err() {
                    self.forget(&device, &play_id);
                    return;
                }
                pushed += chunk_samples as u64;
                self.record_progress(&device, &play_id, total_ms(pushed));
            }
        }
    }

    fn push(&self, play_id: &str, seq: &mut u64, chunk: &[i16]) -> Result<(), ()> {
        let bytes: Vec<u8> = chunk.iter().flat_map(|&s| s.to_le_bytes()).collect();
        let r = self.hub.play_chunk(play_id, *seq, &bytes);
        *seq += 1;
        r.map_err(|_| ())
    }

    /// Natural playback end hook (never called on user stop/cancel/pause):
    /// if `repeat` is `One` or `All` for a still-playing music session whose
    /// `play_id` matches, start the next track from `start_ms = 0`; otherwise
    /// retire the session via `forget`.
    /// Boxed to break the async opaque-type cycle (`start_with_repeat_guarded -> supervise_playback -> on_natural_play_end -> start_with_repeat_guarded`).
    pub(crate) fn on_natural_play_end(
        self: &Arc<Self>,
        device: &str,
        play_id: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let this = self.clone();
        let device = device.to_string();
        let play_id = play_id.to_string();
        let cancelled = cancel.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if cancelled {
                this.forget(&device, &play_id);
                return;
            }
            // Register the advance cancel token and snapshot the session epoch
            // inside one `controls -> media` critical section so a concurrent
            // `begin_pause` or `discard_media` cannot miss the token or be
            // overwritten later.
            let next_job = {
                let mut controls = crate::lock_or_recover(&this.controls, "music.controls");
                let m = crate::lock_or_recover(&this.media, "music.media");
                match m.get(&device) {
                    Some(s)
                        if s.state == MediaState::Playing
                            && s.play_id.as_deref() == Some(play_id.as_str()) =>
                    {
                        next_repeat_index(s).map(|next_idx| {
                            let expected_epoch = this.media_epoch(&device);
                            let next_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let tokens = controls.entry(device.clone()).or_default();
                            tokens.retain(|t| t.strong_count() != 0);
                            tokens.push(Arc::downgrade(&next_cancel));
                            (
                                s.tracks.clone(),
                                next_idx,
                                s.repeat,
                                expected_epoch,
                                next_cancel,
                            )
                        })
                    }
                    _ => return,
                }
            };
            let Some((tracks, next_idx, repeat, expected_epoch, next_cancel)) = next_job else {
                this.forget(&device, &play_id);
                return;
            };
            this.retire_playback(&device, &play_id);
            let next_title = tracks
                .get(next_idx)
                .map(|t| t.title.clone())
                .unwrap_or_default();
            let track_count = tracks.len();
            match this
                .play_index_with_repeat_guarded(
                    device.clone(),
                    tracks,
                    next_idx,
                    0,
                    next_cancel,
                    repeat,
                    Some((expected_epoch, play_id.clone())),
                )
                .await
            {
                Ok((new_play_id, _, _)) => {
                    this.note(
                        "media_repeat_advance",
                        json!({
                            "device": device,
                            "prev_play_id": play_id,
                            "play_id": new_play_id,
                            "repeat": repeat.as_str(),
                            "index": next_idx,
                            "tracks": track_count,
                            "title": next_title,
                        }),
                    );
                }
                Err((code, _)) if code == "CANCELLED" => {
                    // Interrupted by a concurrent pause/stop/new play: do not
                    // emit failure or overwrite the newer session state.
                }
                Err((code, detail)) => {
                    this.note(
                        "media_repeat_failed",
                        json!({
                            "device": device,
                            "play_id": play_id,
                            "repeat": repeat.as_str(),
                            "code": code,
                            "detail": detail,
                        }),
                    );
                    this.forget(&device, &play_id);
                }
            }
        })
    }

    /// The pump is done with this play. A session that still points at it is
    /// over (the song finished, or someone stopped it without asking us to
    /// keep it) — remove it. A session that was PAUSED first already cleared
    /// `play_id`, so a pause survives the pump exiting.
    pub(crate) fn forget(&self, device: &str, play_id: &str) {
        let mut m = crate::lock_or_recover(&self.media, "music.media");
        if m.get(device)
            .map(|s| s.state == MediaState::Playing && s.play_id.as_deref() == Some(play_id))
            .unwrap_or(false)
        {
            m.remove(device);
            self.retire_playback(device, play_id);
            drop(m);
            self.note(
                "media_session_ended",
                json!({ "device": device, "play_id": play_id }),
            );
        }
    }

    /// The pump reports how much it has pushed; the session keeps it so a
    /// pause can say how far the song got.
    fn record_progress(&self, device: &str, play_id: &str, position_ms: u64) {
        let mut m = crate::lock_or_recover(&self.media, "music.media");
        if let Some(s) = m.get_mut(device) {
            if s.play_id.as_deref() == Some(play_id) && s.state == MediaState::Playing {
                s.position_ms = position_ms;
            }
        }
    }

    pub(super) fn note(&self, kind: &str, payload: Value) {
        // Media events go to the trace ring the console reads, through the
        // same recorder the voice pipeline uses.
        tracing::info!(kind, %payload, "music");
        if let Some(sink) = crate::lock_or_recover(&self.event_sink, "music.event_sink").as_ref() {
            sink(kind, payload);
        }
    }

    /// Wired by the API/voice layer so music events show up in the same event
    /// ring as every other device fact.
    pub fn set_event_sink(&self, sink: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        *crate::lock_or_recover(&self.event_sink, "music.event_sink") = Some(sink);
    }

    // --- M3: media control -------------------------------------------------

    /// The voice pipeline owns program bookmarks; the resolver asks it.
    pub fn attach_voice(&self, voice: &Arc<crate::voice::VoicePipeline>) {
        *crate::lock_or_recover(&self.voice, "music.voice") = Some(Arc::downgrade(voice));
    }

    pub(super) fn voice(&self) -> Option<Arc<crate::voice::VoicePipeline>> {
        crate::lock_or_recover(&self.voice, "music.voice")
            .as_ref()
            .and_then(|w| w.upgrade())
    }

    /// The session for this device, dropping an expired PAUSED one. A paused
    /// session is resumable for `MEDIA_SESSION_TTL_MS` (10 min, user decision
    /// 2026-11-13); after that it is gone and 「继续」 says there is nothing.
    fn live_session(&self, device: &str) -> Option<MediaSession> {
        let now = crate::id::now_ms();
        let mut m = crate::lock_or_recover(&self.media, "music.media");
        let expired = match m.get(device) {
            Some(s) => {
                s.state == MediaState::Paused
                    && now.saturating_sub(s.updated_ms) > MEDIA_SESSION_TTL_MS
            }
            None => return None,
        };
        if expired {
            m.remove(device);
            drop(m);
            self.note(
                "media_session_expired",
                json!({ "device": device, "ttl_ms": MEDIA_SESSION_TTL_MS }),
            );
            return None;
        }
        m.get(device).cloned()
    }

    /// Every live session, keyed by device (`/mode` and the console read this).
    pub fn sessions_json(&self) -> Value {
        let devices: Vec<String> = {
            let m = crate::lock_or_recover(&self.media, "music.media");
            m.keys().cloned().collect()
        };
        let mut out = serde_json::Map::new();
        for d in devices {
            if let Some(s) = self.live_session(&d) {
                out.insert(d, s.to_json());
            }
        }
        Value::Object(out)
    }

    pub fn media_present(&self, device: &str) -> bool {
        self.live_session(device).is_some()
    }

    /// Playing / paused / nothing — the precedence rule needs the difference
    /// (foreground media outranks a program bookmark; two PAUSED things ask).
    pub fn media_state(&self, device: &str) -> Option<MediaState> {
        self.live_session(device).map(|s| s.state)
    }

    /// Side-effect-free check for hot voice paths: true only when `device`
    /// transitioned to `MediaState::Paused` within `MEDIA_PAUSE_FLUSH_WINDOW_MS`
    /// (while its `StopReceipt` may still be in flight), without cloning
    /// `MediaSession`, mutating session entries, or emitting notes.
    pub fn media_pause_in_flight(&self, device: &str, now_ms: u64) -> bool {
        let m = crate::lock_or_recover(&self.media, "music.media");
        m.get(device).is_some_and(|s| {
            s.state == MediaState::Paused
                && now_ms.saturating_sub(s.updated_ms) <= Self::MEDIA_PAUSE_FLUSH_WINDOW_MS
        })
    }

    pub fn media_session(&self, device: &str) -> Option<MediaSession> {
        self.peek_live_session(device)
    }

    /// Read-only view of the session: an expired PAUSED one reads as `None`
    /// but stays in the map (no `media_session_expired` note, no removal).
    /// Callers that only want to answer "is there anything to resume?" must not
    /// mutate session state or emit an event. Eviction is not this function's
    /// job — there is no sweep in `music`; the TTL is enforced (and the stale
    /// entry dropped) by `live_session`, which is also where a caller gets the
    /// `media_session_expired` note.
    pub fn peek_live_session(&self, device: &str) -> Option<MediaSession> {
        let now = crate::id::now_ms();
        let m = crate::lock_or_recover(&self.media, "music.media");
        let session = m.get(device)?;
        let expired = session.state == MediaState::Paused
            && now.saturating_sub(session.updated_ms) > MEDIA_SESSION_TTL_MS;
        if expired {
            None
        } else {
            Some(session.clone())
        }
    }

    #[cfg(test)]
    pub(crate) fn seed_media_session_for_test(&self, device: &str, session: MediaSession) {
        crate::lock_or_recover(&self.media, "music.media").insert(device.to_string(), session);
    }

    /// Mark the session paused and ask the device to flush. Synchronous on
    /// purpose: the WAKE path needs the position captured before anything
    /// else stops the play. The receipt (if any) is awaited by the caller.
    fn begin_pause(
        this: &Arc<Self>,
        device: &str,
        why: &str,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<(String, u64, StopSubmission, bool)> {
        let now = crate::id::now_ms();
        let (title, position, play_id) = {
            let controls = crate::lock_or_recover(&this.controls, "music.controls");
            let mut m = crate::lock_or_recover(&this.media, "music.media");
            if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst)) {
                return None;
            }
            let s = m.get_mut(device)?;
            if s.state != MediaState::Playing {
                return None;
            }
            // Sweep any in-flight play/repeat-advance tokens so an opening
            // next-track stream cannot overwrite this Paused state.
            if let Some(tokens) = controls.get(device) {
                for token in tokens.iter().filter_map(Weak::upgrade) {
                    if cancel.is_some_and(|caller| std::ptr::eq(caller, token.as_ref())) {
                        continue;
                    }
                    token.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let title = s.current().map(|t| t.title.clone()).unwrap_or_default();
            let heard = s.heard_ms(now);
            let play_id = s.play_id.take();
            if let Some(id) = play_id.as_deref() {
                this.retire_playback(device, id);
            }
            s.state = MediaState::Paused;
            s.position_ms = heard;
            s.updated_ms = now;
            this.bump_media_epoch(device);
            (title, heard, play_id)
        };
        let rx = play_id
            .as_deref()
            .map(|p| this.hub.stop_and_wait(p))
            .unwrap_or(StopSubmission::NoActivePlay);
        this.note(
            "media_paused",
            json!({ "device": device, "why": why, "title": title,
                    "position_ms": position, "play_id": play_id }),
        );
        let had_play = play_id.is_some();
        Some((title, position, rx, had_play))
    }

    /// Voice/takeover path: record the pause now, watch the flush later.
    pub fn pause_now(self: &Arc<Self>, device: &str, why: &str) -> Option<(String, u64)> {
        let (title, position, rx, had_play) = Self::begin_pause(self, device, why, None)?;
        let this = self.clone();
        let device = device.to_string();
        tokio::spawn(async move {
            let outcome = rx.settle(Duration::from_secs(2), None).await;
            this.note("media_pause_receipt", json!({ "device": device, "flushed": outcome.confirmed(), "stop_outcome":outcome, "no_live_play":!had_play }));
        });
        Some((title, position))
    }

    /// Tool path: the person is waiting for the sentence, so the flush is
    /// awaited (bounded). Hardware evidence remains typed through the caller.
    async fn pause_and_wait(
        this: &Arc<Self>,
        device: &str,
        why: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(String, u64, StopOutcome, bool), MusicError> {
        let Some((title, position, rx, had_play)) =
            Self::begin_pause(this, device, why, Some(cancel))
        else {
            // Already paused (handled by the caller as a no-op) or the
            // session vanished between the read and the pause: nothing was
            // paused, so do not hand back a success.
            return Err(MusicError::NoMedia);
        };
        let outcome = rx.settle(Duration::from_millis(1500), Some(cancel)).await;
        Ok((title, position, outcome, had_play))
    }

    /// Tool path for stop: DISCARD the session now, then wait (bounded) for
    /// the device's flush receipt — the fact may only say 「已停止」 when the
    /// device answered.
    async fn stop_and_wait(
        self: &Arc<Self>,
        device: &str,
        why: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Option<(String, StopOutcome, bool)> {
        let (title, play_id) = self.discard_media(device, Some(cancel))?;
        let submission = play_id
            .as_deref()
            .map(|p| self.hub.stop_and_wait(p))
            .unwrap_or(StopSubmission::NoActivePlay);
        let outcome = submission
            .settle(Duration::from_millis(1500), Some(cancel))
            .await;
        self.note("media_stopped", json!({ "device": device, "why": why, "title": title, "confirmed": outcome.confirmed(), "stop_outcome":outcome, "no_live_play":play_id.is_none() }));
        Some((title, outcome, play_id.is_some()))
    }

    pub(super) fn discard_media(
        &self,
        device: &str,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<(String, Option<String>)> {
        let controls = crate::lock_or_recover(&self.controls, "music.controls");
        let mut media = crate::lock_or_recover(&self.media, "music.media");
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst)) {
            return None;
        }
        if let Some(tokens) = controls.get(device) {
            for token in tokens.iter().filter_map(Weak::upgrade) {
                if cancel.is_some_and(|caller| std::ptr::eq(caller, token.as_ref())) {
                    continue;
                }
                token.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.bump_media_epoch(device);
        let session = media.remove(device)?;
        if let Some(id) = session.play_id.as_deref() {
            self.retire_playback(device, id);
        }
        Some((
            session
                .current()
                .map(|t| t.title.clone())
                .unwrap_or_default(),
            session.play_id,
        ))
    }

    /// Stop means DISCARD (the session is gone). Pause keeps it.
    pub fn stop_now(self: &Arc<Self>, device: &str, why: &str) -> Option<String> {
        let (title, play_id) = self.discard_media(device, None)?;
        if let Some(play_id) = play_id {
            let _ = self.hub.stop(&play_id);
        }
        self.note(
            "media_stopped",
            json!({ "device": device, "why": why, "title": title }),
        );
        Some(title)
    }

    /// Start a `story.play` stream or multi-episode album queue as a first-class
    /// resumable `MediaSession` (`Source::Story`, `#118` / `#252`) so pause,
    /// wake-interrupt, `"继续播放"` bookmark resume, and `"下一首"` episode
    /// switching work identically to music tracks.
    pub(crate) async fn start_story_queue(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        ctx: &ToolCtx,
    ) -> Result<(String, Evidence, String), (String, String)> {
        let Some(track) = tracks.get(index) else {
            return Err(("STORY_NOT_FOUND".into(), "无效的故事分集索引".into()));
        };
        self.register_control(&device, ctx.cancel_flag());
        let resp = self.open_story_stream(track).await?;
        self.start(device, tracks, index, 0, resp, ctx).await
    }

    async fn open_story_stream(
        &self,
        track: &Track,
    ) -> Result<reqwest::Response, (String, String)> {
        let svc = crate::config::ConfigService::new(self.store.clone());
        let cfg = svc
            .personal_endpoint(crate::config::STORY_KEY)
            .unwrap_or_default();
        if !cfg.is_ready() {
            return Err(("STORY_UNCONFIGURED".into(), "故事服务未配置".into()));
        }
        let expected_binding = format!("story|{}", cfg.url.trim_end_matches('/'));
        if track.binding != expected_binding {
            return Err((
                "STORY_BINDING_MISMATCH".into(),
                "故事服务配置已变更，请重新点播".into(),
            ));
        }
        if track.id.is_empty()
            || track.id.contains("..")
            || track
                .id
                .chars()
                .any(|c| matches!(c, '/' | '\\' | '?' | '#') || c.is_whitespace() || c.is_control())
        {
            return Err(("STORY_INVALID_ID".into(), "无效的故事曲目 ID".into()));
        }
        let url = format!("{}/v1/stream/{}", cfg.url.trim_end_matches('/'), track.id);
        if let Err(e) =
            crate::tool::http::validate_news_url(&url, self.store.allows_loopback_public())
        {
            return Err(("INVALID_URL".into(), e.to_string()));
        }
        let mut req = self.story_stream_client.get(&url);
        if let Ok(Some(tok)) = svc.personal_token(crate::config::STORY_TOKEN_SECRET) {
            if !tok.trim().is_empty() {
                req = req.bearer_auth(tok.trim());
            }
        }
        req.send()
            .await
            .map_err(|e| {
                (
                    "STORY_STREAM_ERROR".into(),
                    format!("故事续播取流失败：{e}"),
                )
            })?
            .error_for_status()
            .map_err(|e| {
                (
                    "STORY_STREAM_ERROR".into(),
                    format!("故事续播 HTTP 错误：{e}"),
                )
            })
    }

    /// Start a `radio.play` live stream as a first-class `MediaSession`
    /// (`Source::Radio`, C-003) so pause/resume (always re-opening the live
    /// stream from `start_ms = 0`) and `"下一首"` / `"换个台"` across candidate
    /// stations work seamlessly.
    pub(crate) async fn start_radio(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        ctx: &ToolCtx,
    ) -> Result<(String, Evidence, String), (String, String)> {
        let Some(track) = tracks.get(index).cloned() else {
            return Err(("RADIO_NOT_FOUND".into(), "没有可播放的电台".into()));
        };
        self.register_control(&device, ctx.cancel_flag());
        let resp = self.open_radio_stream(&track).await?;
        self.start(device, tracks, index, 0, resp, ctx).await
    }

    async fn open_radio_stream(
        &self,
        track: &Track,
    ) -> Result<reqwest::Response, (String, String)> {
        let svc = crate::config::ConfigService::new(self.store.clone());
        let cfg = svc
            .personal_endpoint(crate::config::RADIO_KEY)
            .unwrap_or_default();
        if !cfg.is_ready() {
            return Err(("RADIO_UNCONFIGURED".into(), "电台服务未配置".into()));
        }
        let expected_binding = format!("radio|{}", cfg.url.trim_end_matches('/'));
        if track.binding != expected_binding {
            return Err((
                "RADIO_BINDING_MISMATCH".into(),
                "电台服务配置已变更，请重新点播".into(),
            ));
        }
        if track.id.is_empty()
            || track.id.contains("..")
            || track
                .id
                .chars()
                .any(|c| matches!(c, '/' | '\\' | '?' | '#') || c.is_whitespace() || c.is_control())
        {
            return Err(("RADIO_INVALID_ID".into(), "无效的电台 ID".into()));
        }
        let url = format!(
            "{}/v1/stream/{}?format=mp3",
            cfg.url.trim_end_matches('/'),
            track.id
        );
        if let Err(e) =
            crate::tool::http::validate_news_url(&url, self.store.allows_loopback_public())
        {
            return Err(("INVALID_URL".into(), e.to_string()));
        }
        let mut req = self.story_stream_client.get(&url);
        if let Ok(Some(tok)) = svc.personal_token(crate::config::RADIO_TOKEN_SECRET) {
            if !tok.trim().is_empty() {
                req = req.bearer_auth(tok.trim());
            }
        }
        req.send()
            .await
            .map_err(|e| ("RADIO_STREAM_ERROR".into(), format!("电台取流失败：{e}")))?
            .error_for_status()
            .map_err(|e| ("RADIO_STREAM_ERROR".into(), format!("电台 HTTP 错误：{e}")))
    }

    /// Open the stream for `tracks[index]` and start it at `start_ms`.
    async fn play_index_with_repeat(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        start_ms: u64,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        repeat: RepeatMode,
    ) -> Result<(String, Evidence, String), (String, String)> {
        self.play_index_with_repeat_guarded(device, tracks, index, start_ms, cancel, repeat, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn play_index_with_repeat_guarded(
        self: &Arc<Self>,
        device: String,
        tracks: Vec<Track>,
        index: usize,
        start_ms: u64,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        repeat: RepeatMode,
        repeat_guard: Option<(u64, String)>,
    ) -> Result<(String, Evidence, String), (String, String)> {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(("CANCELLED".into(), "播放已撤销".into()));
        }
        let Some(track) = tracks.get(index).cloned() else {
            return Err(("MUSIC_NO_NEXT".into(), "没有下一首了".into()));
        };
        let (resp, effective_start_ms) = match track.source {
            Source::Story => (self.open_story_stream(&track).await?, start_ms),
            Source::Radio => (self.open_radio_stream(&track).await?, 0),
            Source::Library | Source::Online => {
                let (cfg, secrets) = self
                    .ready()
                    .map_err(|e| (e.code().to_string(), describe(&e)))?;
                let r = open_stream(&self.stream_client, &cfg, &secrets, &track)
                    .await
                    .map_err(|e| (e.code().to_string(), format!("取流失败：{}", describe(&e))))?;
                (r, start_ms)
            }
        };
        let ctx = ToolCtx::new(format!("voice:{device}"), crate::id::next("C"), cancel);
        self.start_with_repeat_guarded(
            device,
            tracks,
            index,
            effective_start_ms,
            resp,
            &ctx,
            repeat,
            repeat_guard,
        )
        .await
    }

    /// The "current controllable object" resolver: an explicit target wins;
    /// otherwise media (foreground) over a program bookmark; both present =
    /// ask (a wrong guess stops the wrong thing). Program presence comes from
    /// the voice pipeline, which owns the bookmarks.
    fn resolve_target(
        &self,
        device: &str,
        explicit: Option<&str>,
    ) -> Result<&'static str, MusicError> {
        let media = self.media_present(device);
        let program = self
            .voice()
            .map(|v| v.program_controllable(device))
            .unwrap_or(false);
        match explicit.map(str::trim).filter(|t| !t.is_empty()) {
            Some("media") => {
                if media {
                    Ok("media")
                } else {
                    Err(MusicError::NoMedia)
                }
            }
            Some("program") => {
                if program {
                    Ok("program")
                } else {
                    Err(MusicError::NoProgram)
                }
            }
            Some(_) => Err(MusicError::AmbiguousTarget),
            None => {
                // 前台媒体 first: a song that is PLAYING is what the person
                // is hearing, even when a story bookmark exists. Two things
                // that are both only-paused cannot be told apart, so ask.
                let playing = self
                    .live_session(device)
                    .map(|s| s.state == MediaState::Playing)
                    .unwrap_or(false);
                match (media, program) {
                    (true, true) if !playing => Err(MusicError::AmbiguousTarget),
                    (true, _) => Ok("media"),
                    (false, true) => Ok("program"),
                    (false, false) => Err(MusicError::NothingToControl),
                }
            }
        }
    }

    /// One control action, resolved and executed. Returns the fact the core
    /// will speak; the model never phrases these.
    pub async fn control(
        self: &Arc<Self>,
        device: &str,
        action: &str,
        target: Option<&str>,
    ) -> Result<ControlOutcome, MusicError> {
        self.control_with_cancel(
            device,
            action,
            target,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        )
        .await
    }

    /// Call synchronously when a control is accepted, before spawning work.
    pub fn register_control(&self, device: &str, cancel: Arc<std::sync::atomic::AtomicBool>) {
        let mut controls = crate::lock_or_recover(&self.controls, "music.controls");
        let tokens = controls.entry(device.into()).or_default();
        tokens.retain(|t| t.strong_count() != 0);
        tokens.push(Arc::downgrade(&cancel));
    }

    /// `keep_task`: the CALLER's turn, when a tool is asking (contract v2 #3).
    /// A program stop must not cancel the turn that requested it, or the fact
    /// it is about to say is never spoken. `None` = the person's own control
    /// (voice path), which keeps the full stop semantics.
    pub async fn control_with_cancel(
        self: &Arc<Self>,
        device: &str,
        action: &str,
        target: Option<&str>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        keep_task: Option<&str>,
    ) -> Result<ControlOutcome, MusicError> {
        self.control_with_mode(device, action, target, None, cancel, keep_task)
            .await
    }

    pub async fn control_with_mode(
        self: &Arc<Self>,
        device: &str,
        action: &str,
        target: Option<&str>,
        mode: Option<&str>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        keep_task: Option<&str>,
    ) -> Result<ControlOutcome, MusicError> {
        self.register_control(device, cancel.clone());
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MusicError::NotAvailable);
        }
        let resolved = self.resolve_target(device, target)?;
        match (resolved, action) {
            ("media", "pause") => {
                let session = self.live_session(device).ok_or(MusicError::NoMedia)?;
                if session.state == MediaState::Paused {
                    return Ok(ControlOutcome::media(
                        "pause",
                        session
                            .current()
                            .map(|t| t.title.clone())
                            .unwrap_or_default(),
                        "已经是暂停的".into(),
                        Evidence::Observed,
                        false,
                        "media",
                    )
                    .into_no_op());
                }
                let (title, _pos, outcome, had_play) =
                    Self::pause_and_wait(self, device, "tool", &cancel).await?;
                let confirmed = outcome.confirmed();
                let out = ControlOutcome::media(
                    "pause",
                    title,
                    if confirmed {
                        "设备已停稳"
                    } else {
                        "停止请求已发出，设备没有回报停稳"
                    }
                    .into(),
                    if confirmed {
                        Evidence::DeviceConfirmed
                    } else {
                        Evidence::Unknown
                    },
                    confirmed,
                    "media",
                );
                let out = out.with_stop_outcome(outcome);
                Ok(if had_play { out } else { out.into_no_op() })
            }
            ("media", "resume") => {
                let session = self.live_session(device).ok_or(MusicError::NoMedia)?;
                if session.state == MediaState::Playing {
                    return Ok(ControlOutcome::media(
                        "play",
                        session
                            .current()
                            .map(|t| t.title.clone())
                            .unwrap_or_default(),
                        "已经在播放".into(),
                        Evidence::DeviceConfirmed,
                        true,
                        "media",
                    ));
                }
                let index = session.index;
                let position = session.position_ms;
                let tracks = session.tracks.clone();
                let repeat = session.repeat;
                let (play_id, evidence, detail) = self
                    .play_index_with_repeat(
                        device.to_string(),
                        tracks.clone(),
                        index,
                        position,
                        cancel,
                        repeat,
                    )
                    .await
                    .map_err(|(code, detail)| {
                        self.note(
                            "media_resume_failed",
                            json!({ "device": device, "code": code, "detail": detail }),
                        );
                        MusicError::NotAvailable
                    })?;
                let confirmed = matches!(evidence, Evidence::DeviceConfirmed | Evidence::Sent);
                Ok(ControlOutcome::media(
                    "resume",
                    tracks
                        .get(index)
                        .map(|t| t.title.clone())
                        .unwrap_or_default(),
                    format!("{detail}; play_id={play_id}"),
                    evidence,
                    confirmed,
                    "media",
                ))
            }
            ("media", "next") => {
                let session = self.live_session(device).ok_or(MusicError::NoMedia)?;
                let next = if session.index + 1 < session.tracks.len() {
                    session.index + 1
                } else if session.repeat == RepeatMode::All && !session.tracks.is_empty() {
                    0
                } else {
                    return Err(MusicError::NoNext);
                };
                // A new track supersedes whatever is playing; the device's
                // `barge_in: stop_current` does the actual switch.
                let tracks = session.tracks.clone();
                let repeat = session.repeat;
                let (play_id, evidence, detail) = self
                    .play_index_with_repeat(
                        device.to_string(),
                        tracks.clone(),
                        next,
                        0,
                        cancel,
                        repeat,
                    )
                    .await
                    .map_err(|(code, detail)| {
                        self.note(
                            "media_next_failed",
                            json!({ "device": device, "code": code, "detail": detail }),
                        );
                        MusicError::NotAvailable
                    })?;
                let confirmed = matches!(evidence, Evidence::DeviceConfirmed | Evidence::Sent);
                Ok(ControlOutcome::media(
                    "play",
                    tracks
                        .get(next)
                        .map(|t| t.title.clone())
                        .unwrap_or_default(),
                    format!("{detail}; play_id={play_id}"),
                    evidence,
                    confirmed,
                    "media",
                ))
            }
            ("media", "stop") => {
                let (title, outcome, had_play) = self
                    .stop_and_wait(device, "tool", &cancel)
                    .await
                    .ok_or(MusicError::NoMedia)?;
                let confirmed = outcome.confirmed();
                let out = ControlOutcome::media(
                    "stop",
                    title,
                    if confirmed {
                        "设备已停稳，会话已丢弃"
                    } else {
                        "停止请求已发出，设备没有回报停稳"
                    }
                    .into(),
                    if confirmed {
                        Evidence::DeviceConfirmed
                    } else {
                        Evidence::Unknown
                    },
                    confirmed,
                    "media",
                );
                let out = out.with_stop_outcome(outcome);
                Ok(if had_play { out } else { out.into_no_op() })
            }
            ("program", "pause") => {
                let voice = self.voice().ok_or(MusicError::NoProgram)?;
                let paused =
                    voice.pause_program_with_cancel(device, "music_control", Some(&cancel));
                if !paused {
                    return Err(MusicError::NoProgram);
                }
                Ok(ControlOutcome::media(
                    "pause",
                    "当前内容".into(),
                    "书签保留，说继续接着讲".into(),
                    Evidence::Sent,
                    true,
                    "program",
                ))
            }
            ("program", "resume") => {
                let voice = self.voice().ok_or(MusicError::NoProgram)?;
                if !voice.resume_program_with_cancel(device, &cancel) {
                    return Err(MusicError::NoProgram);
                }
                Ok(ControlOutcome::media(
                    "resume",
                    "当前内容".into(),
                    "从书签继续".into(),
                    Evidence::Sent,
                    true,
                    "program",
                ))
            }
            ("program", "stop") => {
                let voice = self.voice().ok_or(MusicError::NoProgram)?;
                // Contract v2 #3 (2026-11-16): stop the content, not the turn
                // that asked. The person's 「停止播放」 still goes through the
                // full stop (epoch bump + request cancel); this tool must not
                // cancel its own answer, or the fact it is about to say is
                // never spoken (dot2-b 08:31:10).
                if !voice.stop_program_for_tool(device, keep_task, &cancel) {
                    return Err(MusicError::NotAvailable);
                }
                Ok(ControlOutcome::media(
                    "stop",
                    "当前内容".into(),
                    "已丢弃，不保留续播".into(),
                    Evidence::Sent,
                    true,
                    "program",
                ))
            }
            ("media", "repeat") => {
                let session = self.live_session(device).ok_or(MusicError::NoMedia)?;
                match session.current().map(|t| t.source) {
                    Some(Source::Radio) => return Err(MusicError::RepeatUnsupportedRadio),
                    Some(Source::Library | Source::Online) => {}
                    _ => return Err(MusicError::RepeatUnsupported),
                }
                let Some(repeat) = mode.and_then(RepeatMode::parse) else {
                    return Err(MusicError::InvalidRepeatMode);
                };
                let title = {
                    let mut m = crate::lock_or_recover(&self.media, "music.media");
                    let s = m.get_mut(device).ok_or(MusicError::NoMedia)?;
                    s.repeat = repeat;
                    s.updated_ms = crate::id::now_ms();
                    s.current().map(|t| t.title.clone()).unwrap_or_default()
                };
                self.note(
                    "media_repeat_changed",
                    json!({ "device": device, "repeat": repeat.as_str(), "title": title }),
                );
                Ok(ControlOutcome::media(
                    "repeat",
                    title,
                    format!("已设为{}", repeat.label_zh()),
                    Evidence::Observed,
                    true,
                    "media",
                )
                .with_repeat(repeat))
            }
            // 下一首 is a queue operation; a program has no queue.
            ("program", "next") => Err(MusicError::NoMedia),
            ("program", "repeat") => Err(MusicError::RepeatUnsupported),
            _ => Err(MusicError::NotAvailable),
        }
    }
}

/// What one `music.control` call did, in the shape the fact renderer needs.
#[derive(Debug, Clone)]
pub struct ControlOutcome {
    pub action: String,
    pub subject: String,
    pub detail: String,
    pub evidence: Evidence,
    pub confirmed: bool,
    pub target: &'static str,
    pub no_op: bool,
    pub stop_outcome: Option<StopOutcome>,
    pub repeat: Option<RepeatMode>,
}

impl ControlOutcome {
    fn with_stop_outcome(mut self, outcome: StopOutcome) -> Self {
        if let StopOutcome::Failed { reason } | StopOutcome::Unknown { reason } = &outcome {
            self.detail = format!("{}；{}", self.detail, reason);
        }
        self.stop_outcome = Some(outcome);
        self
    }
    fn with_repeat(mut self, repeat: RepeatMode) -> Self {
        self.repeat = Some(repeat);
        self
    }
    fn into_no_op(mut self) -> Self {
        self.no_op = true;
        self.confirmed = false;
        self.evidence = Evidence::Observed;
        self.detail = "没有活动播放；仅更新本地会话，没有设备停稳回执".into();
        self
    }
    fn media(
        action: &str,
        title: String,
        detail: String,
        evidence: Evidence,
        confirmed: bool,
        target: &'static str,
    ) -> Self {
        let subject = if title.trim().is_empty() {
            "音乐".to_string()
        } else if target == "media" {
            format!("《{}》", title.trim())
        } else {
            title
        };
        Self {
            action: action.to_string(),
            subject,
            detail,
            evidence,
            confirmed,
            target,
            no_op: false,
            stop_outcome: None,
            repeat: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn runtime() -> Arc<MusicRuntime> {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        MusicRuntime::new(store, crate::gateway::DeviceHub::new())
    }

    fn paused_at(updated_ms: u64) -> MediaSession {
        MediaSession {
            tracks: vec![Track {
                source: Source::Library,
                id: "t1".into(),
                title: "晴天".into(),
                artist: "周杰伦".into(),
                binding: "lib".into(),
            }],
            index: 0,
            state: MediaState::Paused,
            position_ms: 8_000,
            play_id: None,
            started_ms: updated_ms,
            base_ms: 0,
            updated_ms,
            repeat: RepeatMode::Off,
        }
    }

    /// `media_session` is a READ: an expired paused session reads as idle but
    /// stays in the map, and no `media_session_expired` note is emitted (the
    /// eviction sweep owns both).
    #[test]
    fn peeking_an_expired_paused_session_returns_none_without_mutating_state() {
        let music = runtime();
        let notes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = notes.clone();
        music.set_event_sink(Arc::new(move |kind: &str, _| {
            sink.lock().unwrap().push(kind.to_string());
        }));
        let dev = "dot2-d";
        let expired_at = crate::id::now_ms() - MEDIA_SESSION_TTL_MS - 1_000;
        music.seed_media_session_for_test(dev, paused_at(expired_at));

        assert!(
            music.media_session(dev).is_none(),
            "an expired paused session must read as idle"
        );
        assert!(
            music.peek_live_session(dev).is_none(),
            "the peek must apply the same TTL filter"
        );
        assert!(
            crate::lock_or_recover(&music.media, "music.media").contains_key(dev),
            "the peek must not evict: activation/rollback still needs the record"
        );
        assert!(
            notes
                .lock()
                .unwrap()
                .iter()
                .all(|k| k != "media_session_expired"),
            "the peek must not emit media_session_expired: {:?}",
            notes.lock().unwrap()
        );
        // A still-fresh paused session is visible to both readers.
        music.seed_media_session_for_test(dev, paused_at(crate::id::now_ms()));
        assert!(music.media_session(dev).is_some());
        assert!(music.peek_live_session(dev).is_some());
    }

    #[test]
    fn media_pause_in_flight_is_side_effect_free_and_time_bounded() {
        let music = runtime();
        let notes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = notes.clone();
        music.set_event_sink(Arc::new(move |kind: &str, _| {
            sink.lock().unwrap().push(kind.to_string());
        }));
        let dev = "dot2-d";
        let now = crate::id::now_ms();

        let expired_at = now - MEDIA_SESSION_TTL_MS - 1_000;
        music.seed_media_session_for_test(dev, paused_at(expired_at));
        assert!(!music.media_pause_in_flight(dev, now));
        assert!(crate::lock_or_recover(&music.media, "music.media").contains_key(dev));
        assert!(notes.lock().unwrap().is_empty());

        music.seed_media_session_for_test(dev, paused_at(now));
        assert!(music.media_pause_in_flight(dev, now + 2_500));
        assert!(
            !music.media_pause_in_flight(dev, now + MusicRuntime::MEDIA_PAUSE_FLUSH_WINDOW_MS + 1)
        );

        let mut playing = paused_at(now);
        playing.state = MediaState::Playing;
        music.seed_media_session_for_test(dev, playing);
        assert!(!music.media_pause_in_flight(dev, now + 100));
    }
}
