//! Local music facts, sourced from the registered play and its device events.
//! These facts grant no peer/session ownership. Trusted replication is #116.

use super::{MediaState, MusicRuntime};
use crate::gateway::DeviceEvent;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub const RENEW_MS: u64 = 1_000;
pub const TTL_MS: u64 = 3_000;

#[derive(Debug, Clone, Serialize)]
pub struct MusicPlaybackFact {
    pub device: String,
    pub play_id: String,
    pub connection_generation: u64,
    pub sequence: u64,
    pub active: bool,
    pub ttl_ms: u64,
}

#[derive(Default)]
pub(super) struct PlaybackState {
    sequence: u64,
    active: HashMap<String, (MusicPlaybackFact, Instant)>,
    guards: HashMap<String, Guard>,
}

struct Guard {
    play_id: String,
    generation: u64,
    started: Instant,
    started_ms: u64,
    ended: Option<Instant>,
    last_media_end_ms: Option<u64>,
}

impl MusicRuntime {
    // Lock order: media -> playback. Logical pause/discard and publication
    // share the media lock so an old monitor cannot resurrect a stopped play.
    fn publish_playback(&self, device: &str, play_id: &str, generation: u64) {
        let mut state = crate::lock_or_recover(&self.playback, "music.playback");
        state.sequence += 1;
        let fact = MusicPlaybackFact {
            device: device.into(),
            play_id: play_id.into(),
            connection_generation: generation,
            sequence: state.sequence,
            active: true,
            ttl_ms: TTL_MS,
        };
        state
            .active
            .insert(device.into(), (fact.clone(), Instant::now()));
        drop(state);
        if let Some(voice) = self.voice() {
            voice.sovereignty.record_local_music(fact.clone());
        }
        self.note("music_playback_notice", serde_json::to_value(fact).unwrap());
    }

    pub(super) fn retire_playback(&self, device: &str, play_id: &str) {
        let mut state = crate::lock_or_recover(&self.playback, "music.playback");
        if !state
            .active
            .get(device)
            .is_some_and(|(f, _)| f.play_id == play_id)
        {
            return;
        }
        let (mut fact, _) = state.active.remove(device).unwrap();
        state.sequence += 1;
        fact.sequence = state.sequence;
        fact.active = false;
        drop(state);
        if let Some(voice) = self.voice() {
            voice.sovereignty.record_local_music(fact.clone());
        }
        self.note("music_playback_notice", serde_json::to_value(fact).unwrap());
    }

    pub(super) fn register_playback(&self, device: &str, play_id: &str) {
        let Some((_, play)) = self
            .hub
            .active_plays_for(device)
            .into_iter()
            .find(|(id, _)| id == play_id)
        else {
            return;
        };
        let mut state = crate::lock_or_recover(&self.playback, "music.playback");
        if let Some((old, _)) = state.active.get(device).cloned() {
            drop(state);
            self.retire_playback(device, &old.play_id);
            state = crate::lock_or_recover(&self.playback, "music.playback");
        }
        state.guards.insert(
            device.into(),
            Guard {
                play_id: play_id.into(),
                generation: play.generation,
                started: Instant::now(),
                started_ms: crate::id::now_ms(),
                ended: None,
                last_media_end_ms: None,
            },
        );
    }

    pub(super) fn confirm_terminal(&self, device: &str, play_id: &str, generation: u64) {
        let mut state = crate::lock_or_recover(&self.playback, "music.playback");
        if let Some(guard) = state
            .guards
            .get_mut(device)
            .filter(|g| g.play_id == play_id && g.generation == generation && g.ended.is_none())
        {
            guard.ended = Some(Instant::now());
            guard.last_media_end_ms = Some(crate::id::now_ms());
        }
    }

    /// Called by the production control-event consumer before voice-only
    /// output filtering. DeviceHub already rejects retired connection frames.
    pub(crate) fn lifecycle_event(&self, event: &DeviceEvent) {
        let id = match event {
            DeviceEvent::PlayFinished { play_id, .. } => play_id,
            DeviceEvent::StopStage {
                play_id, outcome, ..
            } if outcome.confirmed() => play_id,
            _ => return,
        };
        let identity = {
            let state = crate::lock_or_recover(&self.playback, "music.playback");
            state
                .guards
                .iter()
                .find(|(_, g)| g.play_id == *id)
                .map(|(device, g)| (device.clone(), g.generation))
        };
        if let Some((device, generation)) = identity {
            self.confirm_terminal(&device, id, generation);
            self.retire_playback(&device, id);
            if let Some(voice) = self.voice() {
                voice.push_assistant_state(&device, false);
            }
        }
    }

    /// Capture uses Core monotonic onset, frozen before asynchronous ASR.
    /// Unknown stop never invents an end. A later explicit wake authorizes a
    /// new input independently of uncertain old physical playback.
    pub(crate) fn blocks_passive_capture(
        &self,
        device: &str,
        start: Instant,
        duration_ms: u64,
        explicit_wake_ms: Option<u64>,
    ) -> bool {
        let state = crate::lock_or_recover(&self.playback, "music.playback");
        state.guards.get(device).is_some_and(|g| {
            if explicit_wake_ms.is_some_and(|wake| wake > g.started_ms) {
                return false;
            }
            start + Duration::from_millis(duration_ms) >= g.started
                && g.ended.is_none_or(|end| {
                    start
                        < end
                            + Duration::from_millis(crate::sovereignty::POST_MEDIA_FOLLOW_GUARD_MS)
                })
        })
    }

    pub fn playback_status(&self, device: &str) -> serde_json::Value {
        let state = crate::lock_or_recover(&self.playback, "music.playback");
        match state.guards.get(device) {
            Some(g) => serde_json::json!({"play_id":g.play_id,"connection_generation":g.generation,
                "uncertain":g.ended.is_none(),"last_media_end_ms":g.last_media_end_ms,
                "guard_remaining_ms":g.ended.map(|e|(e+Duration::from_millis(crate::sovereignty::POST_MEDIA_FOLLOW_GUARD_MS)).saturating_duration_since(Instant::now()).as_millis()).unwrap_or(0)}),
            None => serde_json::Value::Null,
        }
    }

    /// Bounded local readback. No sender wall clock can extend this lease.
    pub fn playback_facts(&self) -> Vec<MusicPlaybackFact> {
        let mut state = crate::lock_or_recover(&self.playback, "music.playback");
        state
            .active
            .retain(|_, (_, received)| received.elapsed() < Duration::from_millis(TTL_MS));
        let mut facts: Vec<_> = state.active.values().map(|(f, _)| f.clone()).collect();
        facts.sort_by(|a, b| a.device.cmp(&b.device));
        facts
    }

    pub(super) fn supervise_playback(
        self: &Arc<Self>,
        device: String,
        play_id: String,
        mut events: tokio::sync::broadcast::Receiver<DeviceEvent>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        stream_ended: Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;
        let runtime = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let generation = match runtime.upgrade().and_then(|r| {
                r.hub
                    .active_plays_for(&device)
                    .into_iter()
                    .find(|(id, _)| id == &play_id)
            }) {
                Some((_, p)) => p.generation,
                None => return,
            };
            let mut accepted = false;
            let mut terminal = false;
            let mut finished_naturally = false;
            let mut stopped_by_device = false;
            // Bound for a device that accepted `play_end` but never reported a
            // terminal fact (see the stall guard inside the loop).
            const STALL_AFTER_STREAM_END: Duration = Duration::from_secs(60);
            let mut stream_end_seen_at: Option<Instant> = None;
            let mut last_notice: Option<Instant> = None;
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(DeviceEvent::PlayStage { play_id: id, stage, .. }) if id == play_id => {
                            if matches!(stage.as_str(), "accepted" | "streaming" | "device_confirmed") { accepted = true; }
                        }
                        Ok(DeviceEvent::PlayFinished { play_id: id, .. }) if id == play_id => {
                            terminal = true;
                            finished_naturally = true;
                        }
                        Ok(DeviceEvent::StopStage { play_id: id, outcome, .. }) if id == play_id => {
                            stopped_by_device = true;
                            if outcome.confirmed() { terminal = true; }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        // On lag, the authoritative hub and media snapshots
                        // below still revoke; missed starts never invent facts.
                        _ => {}
                    },
                    _ = tick.tick() => {}
                }
                let Some(runtime) = runtime.upgrade() else {
                    return;
                };
                // Stall guard: `play_end` was accepted but the device never sent a
                // terminal fact. The pump used to forget the session 60 s after the
                // stream drained; that bound lives here now, or a lost receipt pins a
                // ghost `Playing` session forever (in-stock status, wake-pause, repeat).
                if stream_ended.load(Ordering::SeqCst) {
                    let seen = *stream_end_seen_at.get_or_insert_with(Instant::now);
                    if !terminal && seen.elapsed() >= STALL_AFTER_STREAM_END {
                        runtime.retire_playback(&device, &play_id);
                        runtime.forget(&device, &play_id);
                        if let Some(voice) = runtime.voice() {
                            voice.push_assistant_state(&device, false);
                        }
                        return;
                    }
                }
                // Drain any queued terminal control events so a tick waking
                // right after `active_plays.remove` still sees `PlayFinished`
                // or `StopStage`, even if intermediate control events lagged.
                loop {
                    match events.try_recv() {
                        Ok(DeviceEvent::PlayFinished { play_id: id, .. }) if id == play_id => {
                            terminal = true;
                            finished_naturally = true;
                        }
                        Ok(DeviceEvent::StopStage {
                            play_id: id,
                            outcome,
                            ..
                        }) if id == play_id => {
                            stopped_by_device = true;
                            if outcome.confirmed() {
                                terminal = true;
                            }
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                            continue
                        }
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                        | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                    }
                }
                let mut notify_voice = false;
                let decision = {
                    let media = crate::lock_or_recover(&runtime.media, "music.media");
                    let current = media.get(&device).is_some_and(|s| {
                        s.state == MediaState::Playing && s.play_id.as_deref() == Some(&play_id)
                    });
                    let play = runtime
                        .hub
                        .active_plays_for(&device)
                        .into_iter()
                        .find(|(id, _)| id == &play_id);
                    if terminal {
                        runtime.confirm_terminal(&device, &play_id, generation);
                    }
                    if !current || play.is_none() {
                        runtime.retire_playback(&device, &play_id);
                        notify_voice = true;
                        // Three facts must agree before a repeat advance: no stop receipt from
                        // the device, a `play_finished` frame, and the pump having
                        // read EOF (it sets `stream_ended` only after `play_end` is
                        // accepted, and never when a stop cut the stream short).
                        // `play_finished` alone cannot tell "the track ran out" from
                        // "the device stopped it".
                        let natural = !stopped_by_device
                            && finished_naturally
                            && stream_ended.load(Ordering::SeqCst);
                        let should_repeat = current && natural && !cancel.load(Ordering::SeqCst);
                        Some(should_repeat)
                    } else {
                        if accepted
                            && last_notice.is_none_or(|last| {
                                last.elapsed() >= Duration::from_millis(RENEW_MS)
                            })
                        {
                            runtime.publish_playback(&device, &play_id, play.unwrap().1.generation);
                            last_notice = Some(Instant::now());
                            notify_voice = true;
                        }
                        None
                    }
                };
                if notify_voice {
                    if let Some(voice) = runtime.voice() {
                        voice.push_assistant_state(&device, false);
                    }
                }
                if let Some(should_repeat) = decision {
                    if should_repeat {
                        runtime
                            .on_natural_play_end(&device, &play_id, &cancel)
                            .await;
                    } else {
                        runtime.forget(&device, &play_id);
                    }
                    if let Some(voice) = runtime.voice() {
                        voice.push_assistant_state(&device, false);
                    }
                    return;
                }
            }
            if let Some(runtime) = runtime.upgrade() {
                runtime.retire_playback(&device, &play_id);
                if let Some(voice) = runtime.voice() {
                    voice.push_assistant_state(&device, false);
                }
            }
        });
    }
}
