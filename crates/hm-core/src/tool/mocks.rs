//! Simulated household tools for M1: hot chart, music search/play, light.
//!
//! Failure injection is a first-class capability (the acceptance cases are
//! about reacting to REAL tool outcomes, including failures). `MockInject`
//! selects the failure profile at construction time; tests use it, the
//! debug API can expose it later. The playback sink is a trait so M2 can
//! swap in the real gateway client without touching tool logic.

use super::{Evidence, SideEffect, Tool, ToolCtx, ToolResult};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub track_id: String,
    pub title: String,
    pub artist: String,
}

/// Fixed chart dataset. Entry #8 deliberately has no library match —
/// exercising "no match → report honestly, don't substitute".
pub fn hotlist_dataset() -> Vec<TrackInfo> {
    [
        ("t01", "晚风信笺", "林晚"),
        ("t02", "光年之外的光", "陈屿"),
        ("t03", "小城路灯", "周与"),
        ("t04", "白噪与海", "Simple Sea"),
        ("t05", "北方的南方", "李北"),
        ("t06", "口袋里的银河", "苏打三"),
        ("t07", "慢速心跳", "Molto"),
        ("t08", "午夜电台不播的歌", "佚名"),
    ]
    .into_iter()
    .map(|(id, title, artist)| TrackInfo {
        track_id: id.into(),
        title: title.into(),
        artist: artist.into(),
    })
    .collect()
}

/// The mock library: hotlist entries 1–7 plus extras; t08 missing.
fn library() -> Vec<TrackInfo> {
    let mut lib: Vec<TrackInfo> = hotlist_dataset().into_iter().take(7).collect();
    lib.push(TrackInfo {
        track_id: "x01".into(),
        title: "未上榜的午后".into(),
        artist: "阿蓝".into(),
    });
    lib
}

/// What the simulated playback sink does with a submission.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayOutcome {
    Ok,
    Fail {
        reason: String,
    },
    /// Submission accepted, then no confirmation arrives (timeout) — the
    /// honest `unknown` case the model must NOT report as success.
    TimeoutUnknown,
}

#[derive(Debug, Clone, Default)]
pub struct MockInject {
    pub hotlist_empty: bool,
    pub hotlist_delay_ms: u64,
    pub search_always_empty: bool,
    /// track_id → forced outcome; default Ok (device_confirmed).
    pub play_plan: Vec<(String, PlayOutcome)>,
}

/// Where played tracks actually go. M2 replaces this with the gateway
/// client speaking the device protocol; evidence levels stay the same.
#[async_trait::async_trait]
pub trait PlaybackSink: Send + Sync {
    async fn submit(&self, task_id: &str, track: &TrackInfo) -> (Evidence, String);
}

pub struct SimulatedPlaybackSink {
    plan: Vec<(String, PlayOutcome)>,
    pub submissions: Mutex<Vec<(String, String)>>, // (task_id, track_id)
}

impl SimulatedPlaybackSink {
    pub fn new(inject: &MockInject) -> Self {
        Self {
            plan: inject.play_plan.clone(),
            submissions: Mutex::new(Vec::new()),
        }
    }
}

impl GatewayPlaybackSink {
    pub fn create(hub: std::sync::Arc<crate::gateway::DeviceHub>, inject: &MockInject) -> Self {
        Self {
            hub,
            fallback: SimulatedPlaybackSink::new(inject),
            fail_ids: inject
                .play_plan
                .iter()
                .filter(|(_, o)| matches!(o, PlayOutcome::Fail { .. }))
                .map(|(id, _)| id.clone())
                .collect(),
        }
    }
}

#[async_trait::async_trait]
impl PlaybackSink for SimulatedPlaybackSink {
    async fn submit(&self, task_id: &str, track: &TrackInfo) -> (Evidence, String) {
        crate::lock_or_recover(&self.submissions, "mocks.submissions")
            .push((task_id.to_string(), track.track_id.clone()));
        let outcome = self
            .plan
            .iter()
            .find(|(id, _)| *id == track.track_id)
            .map(|(_, o)| o.clone())
            .unwrap_or(PlayOutcome::Ok);
        match outcome {
            PlayOutcome::Ok => (
                Evidence::DeviceConfirmed,
                format!("now playing {} — {}", track.title, track.artist),
            ),
            PlayOutcome::Fail { reason } => {
                (Evidence::Submitted, format!("sink rejected: {reason}"))
            }
            PlayOutcome::TimeoutUnknown => {
                tokio::time::sleep(Duration::from_millis(150)).await; // mimic slow confirm
                (Evidence::Unknown, "no confirmation within budget".into())
            }
        }
    }
}

// --- hotlist.query ---------------------------------------------------------

pub struct HotlistTool {
    inject: MockInject,
}

#[async_trait::async_trait]
impl Tool for HotlistTool {
    fn availability(&self) -> super::Availability {
        super::Availability::Hidden
    }

    fn descriptor(&self) -> super::ToolDescriptor {
        super::ToolDescriptor {
            name: "hotlist.query".into(),
            // A fixed dataset: registered for dispatch and the acceptance
            // suite, never advertised as a household capability (R1h). The
            // model must not offer a chart it cannot really observe.
            service: "internal".into(),
            side_effect: SideEffect::Query,
            description:
                "查询今天的模拟音乐热榜，返回按名次排序的曲目（含 hotlist_ref 和 track_id）".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "top": { "type": "integer", "description": "返回前 N 名，默认全部" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        if self.inject.hotlist_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.inject.hotlist_delay_ms)).await;
        }
        if self.inject.hotlist_empty {
            return ToolResult::failed("EMPTY_CHART", "榜单暂无数据", true);
        }
        let top = args.get("top").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
        let entries: Vec<Value> = hotlist_dataset()
            .into_iter()
            .take(top)
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "rank": i + 1,
                    "hotlist_ref": format!("hotlist:today#{}", i + 1),
                    "track_id": t.track_id,
                    "title": t.title,
                    "artist": t.artist,
                })
            })
            .collect();
        ToolResult::observed_ok(json!({ "date": "today", "entries": entries }))
    }
}

// --- music.search ----------------------------------------------------------

pub struct MusicSearchTool {
    inject: MockInject,
}

#[async_trait::async_trait]
impl Tool for MusicSearchTool {
    fn descriptor(&self) -> super::ToolDescriptor {
        super::ToolDescriptor {
            name: "music.search".into(),
            service: "music".into(),
            side_effect: SideEffect::Query,
            description: "在模拟曲库中找歌：可按歌名/歌手关键词，或按热榜引用（hotlist_ref，形如 hotlist:today#1）。无匹配时如实返回空，不要用别的歌顶替".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "歌名或歌手关键词" },
                    "hotlist_ref": { "type": "string", "description": "热榜引用，优先于 query" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        if self.inject.search_always_empty {
            return ToolResult::observed_ok(json!({ "candidates": [] }));
        }
        let lib = library();
        let candidates: Vec<Value> =
            if let Some(href) = args.get("hotlist_ref").and_then(Value::as_str) {
                // A hotlist ref resolves through the CHART first; only chart
                // entries that exist in the library can be candidates.
                let n = href
                    .rsplit('#')
                    .next()
                    .and_then(|s| s.parse::<usize>().ok());
                let chart = hotlist_dataset();
                let chart_hit = n.and_then(|n| chart.get(n.saturating_sub(1))).cloned();
                match chart_hit.and_then(|t| lib.iter().find(|l| l.track_id == t.track_id)) {
                    Some(t) => vec![track_json(t)],
                    None => vec![],
                }
            } else if let Some(q) = args.get("query").and_then(Value::as_str) {
                lib.iter()
                    .filter(|t| t.title.contains(q) || t.artist.contains(q))
                    .map(track_json)
                    .collect()
            } else {
                return ToolResult::failed("BAD_ARGS", "需要 query 或 hotlist_ref", false);
            };
        ToolResult::observed_ok(json!({ "candidates": candidates }))
    }
}

fn track_json(t: &TrackInfo) -> Value {
    json!({ "track_id": t.track_id, "title": t.title, "artist": t.artist, "source": "mock_library" })
}

// --- music.play ------------------------------------------------------------

pub struct MusicPlayTool {
    sink: std::sync::Arc<dyn PlaybackSink>,
}

#[async_trait::async_trait]
impl Tool for MusicPlayTool {
    fn descriptor(&self) -> super::ToolDescriptor {
        super::ToolDescriptor {
            name: "music.play".into(),
            service: "music".into(),
            side_effect: SideEffect::Mutate,
            description: "在房间输出设备上播放指定 track_id 的歌。结果以回执为准：失败或未确认时必须如实告诉用户，不要假装在播".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "track_id": { "type": "string", "description": "music.search 返回的曲目 ID" }
                },
                "required": ["track_id"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(track_id) = args.get("track_id").and_then(Value::as_str) else {
            return ToolResult::failed("BAD_ARGS", "缺少 track_id", false);
        };
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let Some(track) = library().into_iter().find(|t| t.track_id == track_id) else {
            return ToolResult::failed("NO_SUCH_TRACK", format!("曲库中没有 {track_id}"), false);
        };
        let (evidence, detail) = self.sink.submit(&ctx.task_id, &track).await;
        match evidence {
            Evidence::DeviceConfirmed => ToolResult {
                status: super::ToolStatus::Completed,
                evidence,
                result: Some(
                    json!({ "played": true, "title": track.title, "artist": track.artist, "detail": detail }),
                ),
                error: None,
                fact: None,
            },
            Evidence::Unknown => ToolResult {
                status: super::ToolStatus::Unknown,
                evidence,
                result: Some(json!({ "played": null, "title": track.title, "detail": detail })),
                error: Some(super::ToolError {
                    code: "CONFIRM_TIMEOUT".into(),
                    retryable: true,
                    detail,
                }),
                fact: None,
            },
            // Sink rejected the submission: the request DID reach the sink.
            _ => ToolResult {
                status: super::ToolStatus::Failed,
                evidence: Evidence::Submitted,
                result: None,
                error: Some(super::ToolError {
                    code: "PLAY_FAILED".into(),
                    retryable: true,
                    detail,
                }),
                fact: None,
            },
        }
    }
}

// --- registry assembly -----------------------------------------------------

/// Build the tool registry. Music, weather and home are REAL tools backed by
/// user-configured services (R1e/R1h/R1j); news and the OpenAPI adapter are
/// registered the same way (R2-BM6): a household with none of them hears the
/// matching `*_unconfigured` state instead of a fake answer.
/// Registration is the single assembly point — the prompt's capability list
/// is rendered from what is registered here.
pub fn build_registry(
    inject: MockInject,
    music: std::sync::Arc<super::music::MusicRuntime>,
    weather: std::sync::Arc<super::weather::WeatherRuntime>,
    home: std::sync::Arc<super::home::HomeRuntime>,
    news: std::sync::Arc<super::news::NewsRuntime>,
    openapi: std::sync::Arc<super::openapi::OpenApiRuntime>,
    management: std::sync::Arc<super::management::ManagementRuntime>,
) -> super::Registry {
    let mut reg = super::Registry::new();
    reg.register(std::sync::Arc::new(HotlistTool {
        inject: inject.clone(),
    }))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::music::MusicSearchTool::new(
        music.clone(),
    )))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::music::MusicPlayTool::new(
        music.clone(),
    )))
    .expect("valid unique tool registration");
    // M3: one control tool for whatever is playing (media first, then a
    // program bookmark). Registered with the same runtime so the queue and
    // position are the ones the play path wrote.
    reg.register(std::sync::Arc::new(super::music::MusicControlTool::new(
        music,
    )))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::time::TimeTool))
        .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::weather::WeatherNowTool::new(
        weather.clone(),
    )))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(
        super::weather::WeatherForecastTool::new(weather),
    ))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::home::HomeSetTool::new(home)))
        .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::news::NewsHeadlinesTool::new(
        news,
    )))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::openapi::OpenApiCallTool::new(
        openapi,
    )))
    .expect("valid unique tool registration");
    // R2-BM6: management tools call the SAME VoicePipeline methods the console
    // uses. The pipeline is attached after construction (Weak handle).
    reg.register(std::sync::Arc::new(super::management::VolumeTool::new(
        management.clone(),
    )))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::management::VolumeReadTool::new(
        management.clone(),
    )))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(
        super::management::PlaybackReadTool::new(management.clone()),
    ))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(
        super::management::SessionCloseTool::new(management.clone()),
    ))
    .expect("valid unique tool registration");
    reg.register(std::sync::Arc::new(super::management::ModeTool::new(
        management,
    )))
    .expect("valid unique tool registration");
    reg
}

/// Test registry: the mock chart plus the simulated sink, with music,
/// weather and home pointed at a store that has none of them configured (so
/// the tests that need one configure it first).
pub fn mock_registry(
    inject: MockInject,
) -> (
    super::Registry,
    std::sync::Arc<super::music::MusicRuntime>,
    std::sync::Arc<super::weather::WeatherRuntime>,
) {
    let store = std::sync::Arc::new(crate::store::Store::open_in_memory().unwrap());
    let hub = crate::gateway::DeviceHub::new();
    let music = super::music::MusicRuntime::new(store.clone(), hub.clone());
    let weather = super::weather::WeatherRuntime::new(store.clone());
    let home = super::home::HomeRuntime::new(store.clone());
    let news = super::news::NewsRuntime::new(store.clone());
    let openapi = super::openapi::OpenApiRuntime::new(store.clone());
    let management = super::management::ManagementRuntime::new(store, hub);
    let reg = build_registry(
        inject,
        music.clone(),
        weather.clone(),
        home,
        news,
        openapi,
        management,
    );
    (reg, music, weather)
}

/// M2 production sink: submit plays through the device gateway when an
/// audio_out device is connected; otherwise fall back to the simulated sink
/// (marked as such — the no-speaker text path stays usable and honest).
pub struct GatewayPlaybackSink {
    pub hub: std::sync::Arc<crate::gateway::DeviceHub>,
    pub fallback: SimulatedPlaybackSink,
    /// track_ids forced to "sink rejected" (HM_INJECT_PLAY_FAIL) so the M1
    /// acceptance cases behave the same whichever sink is active.
    pub fail_ids: Vec<String>,
}

/// Simulated duration for mock-library tracks (no real audio exists yet).
const SIMULATED_TRACK_MS: u64 = 30_000;

#[async_trait::async_trait]
impl PlaybackSink for GatewayPlaybackSink {
    async fn submit(&self, task_id: &str, track: &TrackInfo) -> (Evidence, String) {
        if self.fail_ids.iter().any(|id| id == &track.track_id) {
            return (
                Evidence::Submitted,
                "sink rejected: 输出设备忙（注入）".into(),
            );
        }
        let Some(device) = self.hub.primary_audio_out() else {
            // No device connected: keep the text-only path working, and say
            // so — "played" claims must not silently mean "simulated".
            let (evidence, detail) = self.fallback.submit(task_id, track).await;
            return (evidence, format!("{detail}（模拟回执：无已连接设备）"));
        };
        let req = crate::gateway::PlayRequest {
            device: device.stable_id.clone(),
            content_ref: format!("mocklib:{}", track.track_id),
            audio: None,
            audio_meta: crate::gateway::AudioMeta {
                format: "wav".into(),
                sample_rate: 16000,
                channels: 1,
            },
            channel: "media".into(),
            text: Some(format!("{} — {}", track.title, track.artist)),
            simulated_ms: Some(SIMULATED_TRACK_MS),
        };
        match self.hub.play(req) {
            Ok(mut ticket) => {
                // Await a terminal receipt with a bounded budget; a missing
                // confirmation stays unknown (never "success").
                match tokio::time::timeout(std::time::Duration::from_secs(4), &mut ticket.receipt)
                    .await
                {
                    Ok(Ok(crate::gateway::GatewayFrame::PlayReceipt { stage, .. }))
                        if stage == "device_confirmed" =>
                    {
                        (
                            Evidence::DeviceConfirmed,
                            format!("now playing {} — {}", track.title, track.artist),
                        )
                    }
                    // The device could not observe the outcome: unknown stays
                    // unknown (the tool layer reports ToolStatus::Unknown).
                    Ok(Ok(crate::gateway::GatewayFrame::PlayReceipt { stage, detail, .. }))
                        if stage == "unknown" =>
                    {
                        let d = detail.unwrap_or_else(|| "device reported unknown".into());
                        (Evidence::Unknown, d)
                    }
                    Ok(Ok(crate::gateway::GatewayFrame::PlayReceipt { stage, detail, .. })) => (
                        Evidence::Submitted,
                        format!(
                            "device reported '{stage}'{}",
                            detail.map(|d| format!(": {d}")).unwrap_or_default()
                        ),
                    ),
                    Ok(Ok(_)) => (Evidence::Unknown, "unexpected receipt frame".into()),
                    Ok(Err(_)) => (
                        Evidence::Unknown,
                        "connection closed before confirmation".into(),
                    ),
                    Err(_) => (Evidence::Unknown, "no confirmation within budget".into()),
                }
            }
            Err(e) => (Evidence::Submitted, format!("cannot submit to device: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;

    fn ctx() -> ToolCtx {
        ToolCtx::new(
            "T1",
            "C1",
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    #[tokio::test]
    async fn hotlist_returns_ranked_entries() {
        let (reg, _, _) = mock_registry(MockInject::default());
        let r = reg.call("hotlist.query", json!({ "top": 3 }), &ctx()).await;
        assert_eq!(r.status, crate::tool::ToolStatus::Completed);
        let entries = r.result.unwrap()["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["rank"], 1);
        assert_eq!(entries[0]["hotlist_ref"], "hotlist:today#1");
    }

    /// MU-1: with no music service configured, `music.play` says so, plays
    /// nothing, and does not pretend. The mock library that "played" silence
    /// is gone from the production registry (R1e).
    #[tokio::test]
    async fn music_is_honest_when_unconfigured() {
        let (reg, _, _) = mock_registry(MockInject::default());
        let r = reg
            .call("music.search", json!({ "query": "晚风" }), &ctx())
            .await;
        assert_eq!(r.status, crate::tool::ToolStatus::Failed);
        assert_eq!(r.error.unwrap().code, "MUSIC_UNCONFIGURED");

        let r = reg
            .call(
                "music.play",
                json!({ "source": "library", "id": "t01" }),
                &ctx(),
            )
            .await;
        assert_eq!(r.status, crate::tool::ToolStatus::Failed);
        assert_eq!(r.error.unwrap().code, "MUSIC_UNCONFIGURED");
        // No play reached any sink: the failure happens before submission.
    }

    #[tokio::test]
    async fn cancel_flag_blocks_execution() {
        let (reg, _, _) = mock_registry(MockInject::default());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let c = ToolCtx::new("T1", "C1", cancel);
        let r = reg.call("hotlist.query", json!({ "top": 1 }), &c).await;
        assert_eq!(r.status, crate::tool::ToolStatus::Cancelled);
        // The same call with a live ctx runs.
        let live = ToolCtx::new(
            "T2",
            "C2",
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let r = reg.call("hotlist.query", json!({ "top": 1 }), &live).await;
        assert_eq!(r.status, crate::tool::ToolStatus::Completed);
    }

    #[tokio::test]
    async fn unknown_tool_is_structured_failure() {
        let (reg, _, _) = mock_registry(MockInject::default());
        let r = reg.call("music.dance", json!({}), &ctx()).await;
        assert_eq!(r.status, crate::tool::ToolStatus::Failed);
        assert_eq!(r.error.unwrap().code, "UNKNOWN_TOOL");
    }
}
