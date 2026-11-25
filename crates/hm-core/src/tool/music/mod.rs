//! Music: a household library and an online service, both CONFIGURED BY THE
//! USER (R1e, [docs/music-service-20261112]).
//!
//! Nothing here has an address or a credential baked in. Two connections:
//!
//! - `library` — Subsonic/OpenSubsonic (Navidrome on the NAS): `search3` /
//!   `getRandomSongs` to search, `stream` to fetch. Auth is the Subsonic
//!   salted-token form `u/t/md5(secret+salt)/s` (verified against A).
//! - `online` — Personal Services: `GET /v1/search?q=` and
//!   `GET /v1/stream/<id>`, authenticated with `X-Internal-Token`.
//!
//! Playback is a real one: the service hands out the original file (mp3/flac)
//! and the device only takes PCM, so the stream is decoded (symphonia) and
//! pushed over the v0.3 `play_stream` frames at a bounded lead. The tool
//! returns as soon as the DEVICE confirms the play started — the audio keeps
//! streaming in the background, and a stop (any stop) releases the decoder.
//!
//! Unconfigured = `music_unconfigured`, zero plays, zero silent "pretend"
//! playback. The old mock library is gone from the production registry.
//!
//! The module is split by concern: `remote.rs` searches and opens the stream,
//! `decode.rs` turns it into PCM, `session.rs` owns the queue, the position and
//! the device handshake, and `tools.rs` holds `music.search` / `music.play` /
//! `music.control`. This file keeps the shared types and the runtime.

pub(crate) mod decode;
mod lifecycle;
mod remote;
#[cfg(test)]
mod repeat_tests;
mod session;
mod tools;
#[cfg(test)]
mod transport_tests;

pub use lifecycle::MusicPlaybackFact;
pub use session::ControlOutcome;
pub use tools::{MusicControlTool, MusicPlayTool, MusicSearchTool};

// Helpers the tests at the bottom of this file call directly. They are the
// module's own (not public API), so they are imported only for a test build.
#[cfg(test)]
use crate::config::MusicConnection;
#[cfg(test)]
use md5::{Digest, Md5};
#[cfg(test)]
use remote::{subsonic_auth, valid_library_id, valid_online_id};

use remote::{library_search, online_search};

use crate::config::{ConfigService, MusicConfig};
use crate::gateway::DeviceHub;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// How far ahead of the speaker the core may push (seconds of audio) on local
/// loopback gateways. The firmware has its own 180 s backstop; this keeps the
/// device buffer small enough that a stop is heard immediately.
pub(crate) const PLAY_AHEAD_SECS: f64 = 20.0;
/// Smaller play-ahead cap (seconds of audio) for non-loopback LAN/Tailscale
/// gateways so media PCM never fills multi-megabyte TCP send buffers and stalls
/// `Stop` / `wake_ack` preemption (`#282`).
pub(crate) const REMOTE_PLAY_AHEAD_SECS: f64 = 3.0;
/// Initial startup burst (seconds of audio) pushed immediately on non-loopback
/// media start (`#297`): comfortably exceeds Android's 480 ms initial prebuffer
/// so music starts with zero extra delay on LAN and WAN, while avoiding an
/// instantaneous 3.0 s (~200 KB) `SO_SNDBUF` flood right before `"正在播放..."`.
pub(crate) const REMOTE_INITIAL_PLAY_AHEAD_SECS: f64 = 0.65;
/// Smooth ramp rate (extra seconds of play-ahead gained per elapsed second)
/// from `REMOTE_INITIAL_PLAY_AHEAD_SECS` up to `REMOTE_PLAY_AHEAD_SECS`
/// (`0.65 s -> 3.0 s` over ~5.2 s, i.e. 1.45x real-time pace).
pub(crate) const REMOTE_PLAY_AHEAD_RAMP_RATE: f64 = 0.45;
/// Soft user-space `media_tx` queue backpressure threshold (chunks = 1.28 s)
/// for non-loopback media pumps (`#297`) so stalled remote sockets do not
/// accumulate 200+ chunk backlogs while wall clock advances.
pub(crate) const REMOTE_MEDIA_QUEUE_HIGH_WATER: usize = 32;
/// Continuous queue-backpressure duration after which `MusicRuntime::pump`
/// latches `media_congested` on a slow non-loopback link so subsequent tracks
/// automatically drop to `<= 24 kHz` compact mode.
pub(crate) const REMOTE_MEDIA_CONGESTION_WAIT_MS: u64 = 1_500;

/// Compute the smooth remote play-ahead budget (in seconds) at `elapsed_secs`
/// since playback started (`#297`). Loopback speakers keep `PLAY_AHEAD_SECS`.
pub(crate) fn remote_play_ahead_budget_secs(elapsed_secs: f64) -> f64 {
    let elapsed = elapsed_secs.max(0.0);
    (REMOTE_INITIAL_PLAY_AHEAD_SECS + elapsed * REMOTE_PLAY_AHEAD_RAMP_RATE)
        .min(REMOTE_PLAY_AHEAD_SECS)
}
/// Chunk size pushed to the device (PCM s16le mono ≈ 40 ms at 24 kHz).
const PUSH_CHUNK_SECS: f64 = 0.04;
/// A search answer is at most this many tracks (the model picks, the user
/// hears one thing at a time).
const MAX_RESULTS: usize = 8;
/// Bounded response bodies (a compromised/odd service must not OOM the box).
const MAX_BODY: usize = 512 * 1024;
/// The query the connection probe actually sends (Personal Services has no
/// "ping"; an authenticated search IS the proof).
const PROBE_QUERY: &str = "夜曲";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Library,
    Online,
    Story,
    Radio,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Library => "library",
            Source::Online => "online",
            Source::Story => "story",
            Source::Radio => "radio",
        }
    }
}

/// One search result. `id` is the service's own id and the ONLY thing
/// `music.play` accepts (the model must never invent one).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    pub source: Source,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    /// The connection this id belongs to (endpoint + user). An id from
    /// another server/account cannot be played from this one.
    pub binding: String,
}

/// M3: what the media channel is doing. A session outlives one `play` call —
/// that is the whole point of the milestone: the queue and the position must
/// survive a pause, so 「继续」 plays the same song from the same place and
/// 「下一首」 never re-searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    Playing,
    Paused,
}

/// Music-only repeat mode for a `MediaSession`. Radio has no natural end event
/// and story/program playback is excluded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}

impl RepeatMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::One => "one",
            RepeatMode::All => "all",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "关闭" | "关闭循环" | "不循环" | "顺序" | "顺序播放" => {
                Some(RepeatMode::Off)
            }
            "one" | "single" | "track" | "单曲" | "单曲循环" => Some(RepeatMode::One),
            "all" | "list" | "queue" | "playlist" | "列表" | "列表循环" | "全部循环" => {
                Some(RepeatMode::All)
            }
            _ => None,
        }
    }

    pub fn label_zh(&self) -> &'static str {
        match self {
            RepeatMode::Off => "关闭循环",
            RepeatMode::One => "单曲循环",
            RepeatMode::All => "列表循环",
        }
    }
}

/// How long a PAUSED session stays resumable (user decision 2026-11-13:
/// 10 minutes). A playing session never expires; `stop` removes it at once.
pub const MEDIA_SESSION_TTL_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaSession {
    /// The queue this session started from (this turn's search results).
    /// `next` moves inside it; nothing is re-searched.
    pub tracks: Vec<Track>,
    pub index: usize,
    pub state: MediaState,
    /// Heard position of the current track (ms). On pause this is corrected
    /// against the wall clock, because the pump stays up to 20 s AHEAD of the
    /// speaker — the buffered-ahead part was never heard.
    pub position_ms: u64,
    /// The device's play id while playing; `None` while paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_id: Option<String>,
    /// Wall clock when this track started, and the position it started at.
    pub started_ms: u64,
    pub base_ms: u64,
    /// Last state change (the TTL is measured from here).
    pub updated_ms: u64,
    /// Repeat mode for music playback (`off | one | all`).
    #[serde(default)]
    pub repeat: RepeatMode,
}

impl MediaSession {
    pub fn current(&self) -> Option<&Track> {
        self.tracks.get(self.index)
    }

    /// What a person would hear was played: the wall clock since the track
    /// started plus the offset it started at, capped by what we actually
    /// pushed (a starved stream must not claim more than it sent).
    fn heard_ms(&self, now: u64) -> u64 {
        let elapsed = now.saturating_sub(self.started_ms);
        (self.base_ms + elapsed).min(self.position_ms)
    }

    pub fn to_json(&self) -> Value {
        self.to_json_at(crate::id::now_ms())
    }

    /// The same shape at a given instant: a PLAYING session reports what has
    /// been HEARD (wall clock since the start), not the buffered-ahead
    /// position the pump has already sent (up to 20 s more).
    pub fn to_json_at(&self, now: u64) -> Value {
        let track = self.current();
        let position = if self.state == MediaState::Playing {
            self.heard_ms(now)
        } else {
            self.position_ms
        };
        json!({
            "state": self.state,
            "title": track.map(|t| t.title.as_str()),
            "artist": track.map(|t| t.artist.as_str()),
            "source": track.map(|t| t.source.as_str()),
            "index": self.index,
            "tracks": self.tracks.len(),
            "position_ms": position,
            "since_ms": self.started_ms,
            "updated_ms": self.updated_ms,
            "repeat": self.repeat,
        })
    }
}
#[derive(Debug, thiserror::Error)]
pub enum MusicError {
    #[error("music_unconfigured")]
    Unconfigured,
    #[error("music_disabled")]
    Disabled,
    #[error("music_source_disabled")]
    SourceDisabled,
    #[error("music_credential")]
    Credential,
    #[error("music_connection")]
    Connection,
    #[error("music_http")]
    Http,
    #[error("music_response")]
    Response,
    #[error("music_rejected")]
    Rejected,
    #[error("music_candidate_required")]
    CandidateRequired,
    #[error("music_behind")]
    NotAvailable,
    // --- M3: media control -------------------------------------------------
    #[error("music_no_media")]
    NoMedia,
    #[error("music_no_next")]
    NoNext,
    #[error("music_no_program")]
    NoProgram,
    #[error("music_nothing_to_control")]
    NothingToControl,
    #[error("music_target_ambiguous")]
    AmbiguousTarget,
    #[error("music_repeat_unsupported_radio")]
    RepeatUnsupportedRadio,
    #[error("music_repeat_unsupported")]
    RepeatUnsupported,
    #[error("music_invalid_repeat_mode")]
    InvalidRepeatMode,
}

impl MusicError {
    pub fn code(&self) -> &'static str {
        match self {
            MusicError::Unconfigured => "MUSIC_UNCONFIGURED",
            MusicError::Disabled => "MUSIC_DISABLED",
            MusicError::SourceDisabled => "MUSIC_SOURCE_DISABLED",
            MusicError::Credential => "MUSIC_CREDENTIAL_MISSING",
            MusicError::Connection => "MUSIC_CONNECTION",
            MusicError::Http => "MUSIC_HTTP",
            MusicError::Response => "MUSIC_RESPONSE",
            MusicError::Rejected => "MUSIC_REJECTED",
            MusicError::CandidateRequired => "MUSIC_CANDIDATE_REQUIRED",
            MusicError::NotAvailable => "MUSIC_NOT_AVAILABLE",
            MusicError::NoMedia => "MUSIC_NO_MEDIA",
            MusicError::NoNext => "MUSIC_NO_NEXT",
            MusicError::NoProgram => "MUSIC_NO_PROGRAM",
            MusicError::NothingToControl => "MUSIC_NOTHING_TO_CONTROL",
            MusicError::AmbiguousTarget => "MUSIC_TARGET_AMBIGUOUS",
            MusicError::RepeatUnsupportedRadio | MusicError::RepeatUnsupported => {
                "MUSIC_REPEAT_UNSUPPORTED"
            }
            MusicError::InvalidRepeatMode => "BAD_ARGS",
        }
    }
}
/// The two connection secrets resolved for use (never logged, never serialized).
#[derive(Default)]
struct Secrets {
    library: Option<String>,
    online: Option<String>,
}

const CANDIDATE_CACHE_CAP: usize = 64;

/// Bounded FIFO/oldest-entry eviction cache for per-turn track candidates (`MG-P1-06`).
/// Evicting only the oldest entry preserves sibling devices' in-flight selections.
#[derive(Default)]
struct BoundedCandidateCache {
    map: HashMap<String, Vec<Track>>,
    order: VecDeque<String>,
}

impl BoundedCandidateCache {
    fn touch_order(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
    }

    fn entry_mut(&mut self, key: &str) -> &mut Vec<Track> {
        if !self.map.contains_key(key) {
            while self.map.len() >= CANDIDATE_CACHE_CAP {
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                } else {
                    break;
                }
            }
            self.map.insert(key.to_string(), Vec::new());
        }
        self.touch_order(key);
        self.map.get_mut(key).expect("just inserted")
    }

    fn get(&self, key: &str) -> Option<&Vec<Track>> {
        self.map.get(key)
    }
}

/// Shared music state: config access, per-turn search results, and the live
/// media sessions (one per device; a new play stops the old one).
pub struct MusicRuntime {
    pub store: Arc<crate::store::Store>,
    pub hub: Arc<DeviceHub>,
    client: reqwest::Client,
    /// Streaming uses its OWN client: reqwest's `timeout` is a TOTAL request
    /// deadline (connect → last body byte). Applied to an audio stream it
    /// aborts the body mid-song — every track ended after ~65 s on the device
    /// (20 s deadline + the 20 s send-ahead buffer, 2026-11-13). A stream
    /// needs per-read limits, never a total.
    stream_client: reqwest::Client,
    /// Story streams use the same per-read streaming timeouts as `stream_client`
    /// plus `Policy::none()` redirects so `validate_news_url` cannot be bypassed.
    story_stream_client: reqwest::Client,
    /// task_id → ids this turn's search actually returned (bounded FIFO eviction).
    candidates: Mutex<BoundedCandidateCache>,
    /// M3: device → the media session (queue + position). One per device;
    /// a new play replaces it, a pause keeps it, a stop removes it.
    media: Mutex<HashMap<String, MediaSession>>,
    /// Monotonic per-device epoch bumped on every start/pause/discard so an
    /// in-flight repeat advance can never overwrite a newer pause/stop/play.
    media_epochs: Mutex<HashMap<String, u64>>,
    playback: Mutex<lifecycle::PlaybackState>,
    /// Registration and stop share this lock through the final hub submit.
    controls: Mutex<HashMap<String, Vec<Weak<std::sync::atomic::AtomicBool>>>>,
    /// Where music facts go (the voice event ring the console/tests read).
    event_sink: Mutex<Option<Arc<dyn Fn(&str, Value) + Send + Sync>>>,
    /// M3: the voice pipeline owns program bookmarks, so the "current
    /// controllable object" resolver can ask whether a story is paused. Weak:
    /// the pipeline owns the tool layer, not the other way round.
    voice: Mutex<Option<Weak<crate::voice::VoicePipeline>>>,
    /// Max decoder/downstream no-progress idle window in `pump` before
    /// stopping and reclaiming the session (default 35 s, slightly above
    /// `stream_client`'s 30 s HTTP read timeout).
    stream_idle_timeout_ms: std::sync::atomic::AtomicU64,
}
impl MusicRuntime {
    pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 35_000;

    pub fn new(store: Arc<crate::store::Store>, hub: Arc<DeviceHub>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        let lan_policy = crate::tool::http::OutboundPolicy::Lan {
            reject_public_ip_literals: false,
            allow_loopback,
        };
        Arc::new(Self {
            store,
            hub,
            client: crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                lan_policy,
            )
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default(),
            stream_client: crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                lan_policy,
            )
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default(),
            story_stream_client: crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
            )
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default(),
            candidates: Mutex::new(BoundedCandidateCache::default()),
            media: Mutex::new(HashMap::new()),
            media_epochs: Mutex::new(HashMap::new()),
            playback: Mutex::new(lifecycle::PlaybackState::default()),
            controls: Mutex::new(HashMap::new()),
            event_sink: Mutex::new(None),
            voice: Mutex::new(None),
            stream_idle_timeout_ms: std::sync::atomic::AtomicU64::new(
                Self::DEFAULT_STREAM_IDLE_TIMEOUT_MS,
            ),
        })
    }

    /// Test/service seam: a runtime with its own HTTP client (short timeouts,
    /// no connection reuse) so a local fake service answers instantly.
    pub fn with_client(
        store: Arc<crate::store::Store>,
        hub: Arc<DeviceHub>,
        client: reqwest::Client,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            stream_client: client.clone(),
            story_stream_client: client.clone(),
            client,
            candidates: Mutex::new(BoundedCandidateCache::default()),
            media: Mutex::new(HashMap::new()),
            media_epochs: Mutex::new(HashMap::new()),
            playback: Mutex::new(lifecycle::PlaybackState::default()),
            controls: Mutex::new(HashMap::new()),
            event_sink: Mutex::new(None),
            voice: Mutex::new(None),
            stream_idle_timeout_ms: std::sync::atomic::AtomicU64::new(
                Self::DEFAULT_STREAM_IDLE_TIMEOUT_MS,
            ),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_stream_idle_timeout_ms_for_test(&self, ms: u64) {
        self.stream_idle_timeout_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn config(&self) -> MusicConfig {
        self.snapshot()
            .map(|(config, _)| config)
            .unwrap_or_default()
    }

    fn snapshot(&self) -> Result<(MusicConfig, Secrets), MusicError> {
        let snapshot = ConfigService::new(self.store.clone())
            .music_snapshot(false)
            .map_err(|_| MusicError::Unconfigured)?;
        Ok((
            snapshot.config,
            Secrets {
                library: snapshot.library,
                online: snapshot.online,
            },
        ))
    }

    /// Is music usable at all? A household with no music service says so
    /// instead of inventing a song.
    fn ready(&self) -> Result<(MusicConfig, Secrets), MusicError> {
        let (cfg, secrets) = self.snapshot()?;
        if cfg.library.is_none() && cfg.online.is_none() {
            return Err(MusicError::Unconfigured);
        }
        if !cfg.enabled {
            return Err(MusicError::Disabled);
        }
        Ok((cfg, secrets))
    }

    /// A REAL probe (MU-3): one request per configured source, no audio
    /// played. Reports latency and how many results came back — "the endpoint
    /// answers" is only true if it answered.
    pub async fn probe(&self) -> Value {
        let (cfg, secrets) = self.snapshot().unwrap_or_default();
        let mut out = serde_json::Map::new();
        let mut configured = 0;
        let mut all_ok = true;

        let library = match (&cfg.library, &secrets.library) {
            (Some(conn), Some(secret)) if !conn.endpoint.trim().is_empty() => {
                configured += 1;
                let started = std::time::Instant::now();
                let r = library_search(&self.client, conn, secret, "", 1).await;
                let latency_ms = started.elapsed().as_millis() as u64;
                match r {
                    Ok(t) => json!({ "configured": true, "ok": true, "latency_ms": latency_ms,
                                     "count": t.len(), "detail": "" }),
                    Err(e) => {
                        all_ok = false;
                        json!({ "configured": true, "ok": false, "latency_ms": latency_ms,
                                "count": 0, "detail": describe(&e) })
                    }
                }
            }
            (Some(_), None) => {
                configured += 1;
                all_ok = false;
                json!({ "configured": true, "ok": false, "latency_ms": 0, "count": 0,
                        "detail": "缺少口令：请在控制台填写曲库密码" })
            }
            (Some(_), Some(_)) => json!({ "configured": false, "ok": false, "latency_ms": 0,
                                          "count": 0, "detail": "未填 endpoint" }),
            (None, _) => json!({ "configured": false, "ok": false, "latency_ms": 0, "count": 0,
                                 "detail": "未配置" }),
        };
        out.insert("library".into(), library);

        let online = match (&cfg.online, &secrets.online) {
            (Some(conn), Some(token)) if !conn.endpoint.trim().is_empty() => {
                configured += 1;
                let started = std::time::Instant::now();
                // A search is the only authenticated read Personal Services
                // offers; a probe query is a real query.
                let r = online_search(&self.client, conn, token, PROBE_QUERY, 1).await;
                let latency_ms = started.elapsed().as_millis() as u64;
                match r {
                    Ok(t) => json!({ "configured": true, "ok": true, "latency_ms": latency_ms,
                                     "count": t.len(), "detail": "" }),
                    Err(e) => {
                        all_ok = false;
                        json!({ "configured": true, "ok": false, "latency_ms": latency_ms,
                                "count": 0, "detail": describe(&e) })
                    }
                }
            }
            (Some(_), None) => {
                configured += 1;
                all_ok = false;
                json!({ "configured": true, "ok": false, "latency_ms": 0, "count": 0,
                        "detail": "缺少 token：请在控制台填写在线服务令牌" })
            }
            (Some(_), Some(_)) => json!({ "configured": false, "ok": false, "latency_ms": 0,
                                          "count": 0, "detail": "未填 endpoint" }),
            (None, _) => json!({ "configured": false, "ok": false, "latency_ms": 0, "count": 0,
                                 "detail": "未配置" }),
        };
        out.insert("online".into(), online);

        let mut v = Value::Object(out);
        v["enabled"] = json!(cfg.enabled);
        v["configured_sources"] = json!(configured);
        v["ok"] = json!(configured > 0 && all_ok && cfg.enabled);
        v["checked_at_ms"] = json!(crate::id::now_ms());
        v
    }

    pub fn remember_candidates(&self, task_id: &str, tracks: &[Track]) {
        self.remember(task_id, tracks);
    }

    pub fn has_candidate(&self, task_id: &str, source: Source, id: &str) -> bool {
        self.queue_for_with_ids(task_id, source, id, None).is_some()
    }

    fn remember(&self, task_id: &str, tracks: &[Track]) {
        let mut m = crate::lock_or_recover(&self.candidates, "music.candidates");
        let slot = m.entry_mut(task_id);
        for t in tracks {
            if let Some(existing) = slot
                .iter_mut()
                .find(|e| e.source == t.source && e.id == t.id)
            {
                *existing = t.clone();
            } else {
                slot.push(t.clone());
            }
        }
    }

    /// M3: the whole queue this turn searched, plus the index of the id the
    /// model picked. The queue is what `next` walks — never a new search.
    #[cfg(test)]
    fn queue_for(&self, task_id: &str, source: Source, id: &str) -> Option<(Vec<Track>, usize)> {
        self.queue_for_with_ids(task_id, source, id, None)
    }

    fn queue_for_with_ids(
        &self,
        task_id: &str,
        source: Source,
        id: &str,
        queue_ids: Option<&[String]>,
    ) -> Option<(Vec<Track>, usize)> {
        let m = crate::lock_or_recover(&self.candidates, "music.candidates");
        let tracks = m.get(task_id)?;
        let primary = tracks
            .iter()
            .find(|t| t.source == source && t.id == id)?
            .clone();
        if let Some(req_ids) = queue_ids.filter(|ids| !ids.is_empty()) {
            let mut ordered: Vec<Track> = Vec::with_capacity(req_ids.len() + 1);
            for qid in req_ids {
                if let Some(t) = tracks
                    .iter()
                    .find(|t| t.source == source && t.id == qid.trim())
                    .or_else(|| tracks.iter().find(|t| t.id == qid.trim()))
                {
                    if !ordered.iter().any(|e| e.source == t.source && e.id == t.id) {
                        ordered.push(t.clone());
                    }
                }
            }
            if !ordered
                .iter()
                .any(|e| e.source == primary.source && e.id == primary.id)
            {
                ordered.insert(0, primary.clone());
            }
            let index = ordered
                .iter()
                .position(|t| t.source == primary.source && t.id == primary.id)
                .unwrap_or(0);
            return Some((ordered, index));
        }
        let index = tracks
            .iter()
            .position(|t| t.source == source && t.id == id)?;
        Some((tracks.clone(), index))
    }
}
fn describe(e: &MusicError) -> String {
    match e {
        MusicError::Connection => "服务连不上（检查地址与网络）".into(),
        MusicError::Http => "服务返回错误状态".into(),
        MusicError::Rejected => "曲库拒绝了请求（检查用户名与口令）".into(),
        MusicError::Credential => "缺少凭据（控制台里没填口令）".into(),
        MusicError::SourceDisabled => "该来源未配置".into(),
        MusicError::Response => "服务返回了无法解析的内容".into(),
        other => other.code().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_must_be_lan_and_http() {
        assert!(crate::config::valid_music_endpoint(
            "http://192.168.1.27:4533"
        ));
        assert!(crate::config::valid_music_endpoint(
            "http://nas.local:4533/music"
        ));
        assert!(crate::config::valid_music_endpoint("https://music.lan"));
        assert!(!crate::config::valid_music_endpoint(
            "http://127.0.0.1:9999"
        ));
        assert!(crate::config::valid_music_endpoint_with(
            "http://127.0.0.1:9999",
            true
        ));
        assert!(!crate::config::valid_music_endpoint("http://8.8.8.8:4533"));
        assert!(!crate::config::valid_music_endpoint("ftp://nas.local"));
        assert!(!crate::config::valid_music_endpoint("nas.local:4533"));
        assert!(!crate::config::valid_music_endpoint("http://"));
    }

    #[test]
    fn subsonic_token_is_md5_of_secret_plus_salt() {
        let conn = MusicConnection {
            endpoint: "http://nas.local:4533".into(),
            user: "u".into(),
        };
        let auth = subsonic_auth(&conn, "secret").unwrap();
        assert!(auth.contains("u=u&"));
        assert!(auth.contains("v=1.16.1"));
        assert!(auth.contains("c=home-mate"));
        // t = md5(secret+salt): recompute from the auth line itself.
        let salt = auth.split("s=").nth(1).unwrap().split('&').next().unwrap();
        let expect = format!("{:x}", Md5::digest(format!("secret{salt}")));
        assert!(auth.contains(&format!("t={expect}")));
        assert!(
            !auth.contains("secret"),
            "the password must never appear on the wire"
        );
    }

    /// M3: a paused session is resumable for 10 minutes, then it is gone. A
    /// playing session never expires (it is being used).
    #[test]
    fn a_paused_session_expires_after_ten_minutes() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let hub = crate::gateway::DeviceHub::new();
        let rt = MusicRuntime::new(store, hub);
        let track = Track {
            source: Source::Library,
            id: "t01".into(),
            title: "晚风信笺".into(),
            artist: "林晚".into(),
            binding: "x".into(),
        };
        let now = crate::id::now_ms();
        let base = MediaSession {
            tracks: vec![track.clone()],
            index: 0,
            state: MediaState::Paused,
            position_ms: 12_000,
            play_id: None,
            started_ms: now,
            base_ms: 0,
            updated_ms: now,
            repeat: RepeatMode::Off,
        };
        rt.media
            .lock()
            .unwrap()
            .insert("dot2-d".into(), base.clone());
        assert!(rt.media_present("dot2-d"), "a fresh pause is resumable");
        {
            let mut m = rt.media.lock().unwrap();
            let s = m.get_mut("dot2-d").unwrap();
            s.updated_ms = now - MEDIA_SESSION_TTL_MS - 1;
        }
        assert!(!rt.media_present("dot2-d"), "an expired pause is gone");
        assert!(rt.sessions_json().get("dot2-d").is_none());
        // A PLAYING session is never expired: the person is listening to it.
        let mut playing = base;
        playing.state = MediaState::Playing;
        playing.updated_ms = now - MEDIA_SESSION_TTL_MS - 1;
        playing.started_ms = now;
        playing.position_ms = 1_000;
        rt.media.lock().unwrap().insert("dot2-d".into(), playing);
        assert!(rt.media_present("dot2-d"));
    }

    #[test]
    fn d2_media_stop_revokes_other_controls_without_cancelling_its_caller() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let runtime = MusicRuntime::new(store, crate::gateway::DeviceHub::new());
        let caller = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let old_control = Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.register_control("test", caller.clone());
        runtime.register_control("test", old_control.clone());
        runtime.discard_media("test", Some(&caller));
        assert!(old_control.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !caller.load(std::sync::atomic::Ordering::SeqCst),
            "a successful stop must keep its own fact reply deliverable"
        );
        runtime.discard_media("test", None);
        assert!(
            caller.load(std::sync::atomic::Ordering::SeqCst),
            "session close still revokes every control"
        );
    }

    #[test]
    fn ids_are_validated_per_source() {
        assert!(valid_library_id("4731a2b3c4d5e6f7"));
        assert!(valid_library_id("tr-1_2.3"));
        assert!(!valid_library_id("../etc/passwd"));
        assert!(!valid_library_id(""));
        assert!(valid_online_id("123456"));
        assert!(!valid_online_id("12a"));
    }

    #[test]
    fn rc5_same_turn_multiple_searches_accumulate_and_support_queue_ids() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let rt = MusicRuntime::new(store, crate::gateway::DeviceHub::new());
        let t1 = Track {
            source: Source::Library,
            id: "qingtian".into(),
            title: "晴天".into(),
            artist: "周杰伦".into(),
            binding: "lib".into(),
        };
        let t2 = Track {
            source: Source::Library,
            id: "jiandanai".into(),
            title: "简单爱".into(),
            artist: "周杰伦".into(),
            binding: "lib".into(),
        };
        rt.remember("task-multi", std::slice::from_ref(&t1));
        rt.remember("task-multi", std::slice::from_ref(&t2));

        // First search track must remain playable after second search in the same turn
        let (queue, idx) = rt
            .queue_for("task-multi", Source::Library, "qingtian")
            .unwrap();
        assert_eq!(idx, 0);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].id, "qingtian");
        assert_eq!(queue[1].id, "jiandanai");

        // Explicit queue_ids orders the multi-song playback queue
        let explicit = vec!["qingtian".to_string(), "jiandanai".to_string()];
        let (ordered, oidx) = rt
            .queue_for_with_ids("task-multi", Source::Library, "qingtian", Some(&explicit))
            .unwrap();
        assert_eq!(oidx, 0);
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[1].id, "jiandanai");
    }
}
