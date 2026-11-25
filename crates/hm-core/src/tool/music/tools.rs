//! The three music tools the model may call: `music.search`, `music.play` and
//! `music.control`.
//!
//! They are thin: validation and wording live here, the searching/fetching is
//! `remote.rs` and the session/queue mechanics are `session.rs`. A play only
//! succeeds when the DEVICE confirms it started.

use serde_json::{json, Value};
use std::sync::Arc;

use crate::tool::{
    Availability, Evidence, FactBasis, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolError,
    ToolFact, ToolResult, ToolStatus,
};

use super::remote::{library_search, online_search, open_stream};
use super::{describe, MusicError, MusicRuntime, Source, Track, MAX_RESULTS};

// --- tools -----------------------------------------------------------------

pub struct MusicSearchTool {
    rt: Arc<MusicRuntime>,
}

impl MusicSearchTool {
    pub fn new(rt: Arc<MusicRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for MusicSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "music.search".into(),
            service: "music".into(),
            side_effect: SideEffect::Query,
            description: "在家庭曲库（NAS）和/或在线音乐服务里搜歌。默认先搜本地曲库（只有用户明确说「只要本地的」才填 library）；本地没命中时如实说没有，除非用户明确要求在线/网络音乐才把 source 设为 online。搜索不播放、不占播放通道。结果里的 id 只能原样交给 music.play，不许自己编；仅在要点歌或找歌时使用，询问「正在放什么 / 有没有在播放」时请调用 system.playback_reading，不要空搜".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "歌名或歌手关键词。留空只在有本地曲库时＝随机挑几首；在线源必须有歌名" },
                    "source": {
                        "type": "string",
                        "enum": ["auto", "library", "online"],
                        "description": "auto（默认）＝先本地；library＝只查本地；online＝用户明确要求在线时才用"
                    },
                    "limit": { "type": "integer", "description": "最多返回几首（默认 5，上限 8）" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        // The prompt renders this: a household with no music service must not
        // be offered a search that can only answer MUSIC_UNCONFIGURED.
        match self.rt.ready() {
            Ok(_) => Availability::Ready,
            Err(MusicError::Disabled | MusicError::SourceDisabled) => Availability::Disabled,
            Err(_) => Availability::Unconfigured,
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let (cfg, secrets) = match self.rt.ready() {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::failed(
                    e.code(),
                    "音乐服务没有启用/配置：请在控制台「音乐」里填曲库与在线服务，再点测试连接",
                    false,
                )
            }
        };
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if query.chars().count() > 80 || query.chars().any(char::is_control) {
            return ToolResult::failed("BAD_ARGS", "query 过长或含控制字符", false);
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, MAX_RESULTS as u64) as usize;
        let source = match args.get("source").and_then(Value::as_str).unwrap_or("auto") {
            "library" => Some(Source::Library),
            "online" => Some(Source::Online),
            _ => None,
        };

        if source == Some(Source::Online) && query.is_empty() {
            // The online service is query-driven; "随便放一首" cannot be a
            // search. Saying so lets the model ask for a title instead of
            // reporting "the online source returned nothing" (device log
            // 2026-11-12 13:22: an empty-query online search looked like an
            // empty catalogue).
            let msg = if cfg.library.is_none() {
                "在线源需要一个歌名或歌手（不能空着“随便”搜）；本地曲库没有配置"
            } else {
                "在线源需要一个歌名或歌手（不能空着“随便”搜）"
            };
            return ToolResult::failed("MUSIC_QUERY_REQUIRED", msg, false);
        }
        let mut tracks: Vec<Track> = Vec::new();
        let mut used_online = false;
        let mut notes: Vec<String> = Vec::new();

        // The local library first, always (RUI-local priority).
        if let (Some(conn), Some(secret)) = (cfg.library.as_ref(), secrets.library.as_ref()) {
            if source != Some(Source::Online) {
                match library_search(&self.rt.client, conn, secret, &query, limit).await {
                    Ok(mut t) => tracks.append(&mut t),
                    Err(e) => notes.push(format!("library:{}", e.code())),
                }
            }
        } else if source != Some(Source::Online) {
            notes.push("library:not_configured".into());
        }

        // When to use the online service:
        //  - explicitly asked for it (source=online);
        //  - there is NO library configured at all: "prefer local" has nothing
        //    to prefer, and refusing would make a household that only has the
        //    online source hear "no music" for every song (the state the user
        //    hit on 2026-11-12);
        //  - prefer_local is off and the library missed.
        // With prefer_local ON and a library configured, a miss is reported as
        // a miss — MU-6: an online request costs a quota and is the user's
        // decision, not a silent fallback.
        let library_hit = !tracks.is_empty();
        let library_available = cfg.library.is_some() && secrets.library.is_some();
        let want_online = match source {
            Some(Source::Online) => true,
            Some(Source::Library | Source::Story | Source::Radio) => false,
            None => !library_available || (!cfg.prefer_local && !library_hit),
        };
        if want_online {
            if let (Some(conn), Some(token)) = (cfg.online.as_ref(), secrets.online.as_ref()) {
                match online_search(&self.rt.client, conn, token, &query, limit).await {
                    Ok(mut t) => {
                        tracks.append(&mut t);
                        used_online = true;
                    }
                    Err(e) => notes.push(format!("online:{}", e.code())),
                }
            } else {
                notes.push("online:not_configured".into());
            }
        }

        self.rt.remember(&ctx.task_id, &tracks);
        // What the household actually has, so the model stops asking for a
        // library that does not exist (device log 2026-11-12: it chose
        // source=library, got no_matches and asked the person to confirm an
        // online search the person had already asked for).
        let library_configured = cfg.library.is_some() && secrets.library.is_some();
        let online_configured = cfg.online.is_some() && secrets.online.is_some();
        let hint = if tracks.is_empty() && !library_configured && online_configured {
            Value::String(
                "本家庭没有配置曲库；用 source=auto 或 online 直接搜在线源，不要用 library".into(),
            )
        } else if tracks.is_empty() && !notes.is_empty() {
            // An honest hint for the model: "本地没有" is a fact, and asking
            // for online music is the person's decision.
            Value::String(format!(
                "没有命中（{}）；需要网络音乐会消耗在线服务配额，先问用户",
                notes.join(", ")
            ))
        } else if tracks.is_empty() {
            Value::String("没有命中：如实告诉用户没有这首歌".into())
        } else if source.is_none() && library_configured && !library_hit && used_online {
            Value::String("本地曲库没有搜到，已自动通过在线音乐源搜到结果；回复用户时自然说明「本地曲库没有搜到，帮你在线搜到了……」".into())
        } else {
            Value::Null
        };
        let value = json!({
            "status": if tracks.is_empty() { "no_matches" } else { "available" },
            "tracks": tracks,
            "online_used": used_online,
            "library_configured": library_configured,
            "online_configured": online_configured,
            "hint": hint,
            // M7 residual (2026-11-13): "no match" is an ANSWER, not a
            // reason to explain. The core speaks this line and drops the
            // model's prose (an empty result counts as a failed delivery).
            "rendered_answer": if tracks.is_empty() {
                json!("没找到这首歌，换个说法或说在线搜。")
            } else {
                Value::Null
            },
        });
        ToolResult::observed_ok(value)
    }
}

pub struct MusicPlayTool {
    rt: Arc<MusicRuntime>,
}

impl MusicPlayTool {
    pub fn new(rt: Arc<MusicRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for MusicPlayTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "music.play".into(),
            service: "music".into(),
            side_effect: SideEffect::Mutate,
            description: "播放本轮 music.search 返回的曲目（source + id 必须原样来自搜索结果）。当用户一次点播多首歌（如“放晴天和简单爱”）时，可分别调用 music.search 后在一次 music.play 中将首曲作为 id、其余按序放入 queue_ids 构建连播队列。回执只在设备确认开始出声后才算成功；失败或未确认必须如实告诉用户，不要假装在播".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "enum": ["library", "online"], "description": "搜索结果里的来源" },
                    "id": { "type": "string", "description": "首曲的搜索结果 id" },
                    "queue_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "可选：多曲连播时按播放顺序传入本轮 music.search 返回的多个曲目 id 列表（包含首曲 id）"
                    }
                },
                "required": ["source", "id"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        match self.rt.ready() {
            Ok(_) => Availability::Ready,
            Err(MusicError::Disabled | MusicError::SourceDisabled) => Availability::Disabled,
            Err(_) => Availability::Unconfigured,
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let (cfg, secrets) = match self.rt.ready() {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::failed(
                    e.code(),
                    "音乐服务没有启用/配置：没有播放任何音频",
                    false,
                )
            }
        };
        let source = match args.get("source").and_then(Value::as_str) {
            Some("library") => Source::Library,
            Some("online") => Source::Online,
            _ => return ToolResult::failed("BAD_ARGS", "source 必须是 library 或 online", false),
        };
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::failed("BAD_ARGS", "缺少 id", false);
        };
        let queue_ids: Option<Vec<String>> = args
            .get("queue_ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect::<Vec<String>>()
            })
            .filter(|v| !v.is_empty());
        // An id that this turn's search did not return is not playable. The
        // whole queue comes along: `next` walks it instead of re-searching.
        let Some((tracks, index)) =
            self.rt
                .queue_for_with_ids(&ctx.task_id, source, id, queue_ids.as_deref())
        else {
            return ToolResult::failed(
                "MUSIC_CANDIDATE_REQUIRED",
                "只能播放本轮 music.search 返回的曲目；请先搜索",
                false,
            );
        };
        let track = tracks[index].clone();
        let device = match ctx
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(explicit) => explicit.to_string(),
            None => match self.rt.hub.resolve_default_audio_out() {
                Ok(Some(d)) => d.stable_id,
                Ok(None) => {
                    return ToolResult::failed(
                        "NO_DEVICE",
                        "没有已连接的输出设备：没有播放任何音频",
                        true,
                    );
                }
                Err(msg) => {
                    return ToolResult::failed("AMBIGUOUS_DEVICE", msg, false);
                }
            },
        };
        if ctx.is_lan_exempt() && !self.rt.hub.device_is_known_lan_device(&device) {
            return ToolResult::failed(
                "PAIRING_REQUIRED",
                format!(
                    "未认证的局域网访问不允许在本机音箱「{device}」播放音乐，请使用配对令牌认证"
                ),
                false,
            );
        }
        if !self.rt.hub.device_online_with_cap(&device, "audio_out") {
            return ToolResult::failed(
                "NO_DEVICE",
                format!("输出设备 {device} 当前不在线或不支持音频输出：没有播放任何音频"),
                true,
            );
        }
        if !self.rt.hub.device_supports(&device, "play_stream") {
            return ToolResult::failed(
                "DEVICE_NO_STREAM",
                "设备不支持流式播放：音乐需要 play_stream 能力（固件太旧）",
                false,
            );
        }

        self.rt.register_control(&device, ctx.cancel_flag());
        let resp = match open_stream(&self.rt.stream_client, &cfg, &secrets, &track).await {
            Ok(r) => r,
            Err(e) => {
                // R2-BM5: a failed fetch still has a fact to speak — the core
                // says "没有完成…的播放", never the model's paraphrase.
                let fact = ToolFact {
                    subject: format!("《{}》", track.title),
                    action: "play".into(),
                    basis: FactBasis::DeviceStage,
                    confirmed: false,
                    detail: Some("取流失败，没有播放".into()),
                    value: None,
                };
                return ToolResult::failed(
                    e.code(),
                    format!("取流失败：{} — 没有播放", describe(&e)),
                    matches!(e, MusicError::Connection | MusicError::Http),
                )
                .with_fact(fact);
            }
        };

        match self
            .rt
            .start(device.clone(), tracks, index, 0, resp, ctx)
            .await
        {
            Ok((play_id, evidence, detail)) => {
                if matches!(evidence, Evidence::DeviceConfirmed | Evidence::Sent) {
                    ToolResult {
                        status: ToolStatus::Completed,
                        evidence,
                        result: Some(json!({
                            "playing": true, "source": source.as_str(), "id": track.id,
                            "title": track.title, "artist": track.artist,
                            "play_id": play_id, "detail": detail,
                        })),
                        error: None,
                        // R2-BM5: the device's own play-stage event backs a
                        // start fact; the terminal confirmation is minutes
                        // away, so it cannot gate a song.
                        fact: Some(ToolFact {
                            subject: format!("《{}》", track.title),
                            action: "play".into(),
                            basis: FactBasis::DeviceStage,
                            confirmed: true,
                            detail: Some(detail),
                            value: None,
                        }),
                    }
                } else {
                    let code = if evidence == Evidence::Submitted {
                        "MUSIC_START_UNCONFIRMED"
                    } else {
                        "CONFIRM_TIMEOUT"
                    };
                    ToolResult {
                        status: ToolStatus::Unknown,
                        evidence,
                        result: Some(
                            json!({ "playing": null, "title": track.title, "play_id": play_id }),
                        ),
                        error: Some(ToolError {
                            code: code.into(),
                            retryable: true,
                            detail: detail.clone(),
                        }),
                        fact: Some(ToolFact {
                            subject: format!("《{}》", track.title),
                            action: "play".into(),
                            basis: FactBasis::DeviceStage,
                            confirmed: false,
                            detail: Some(detail),
                            value: None,
                        }),
                    }
                }
            }
            Err((code, detail)) => {
                let fact = ToolFact {
                    subject: format!("《{}》", track.title),
                    action: "play".into(),
                    basis: FactBasis::DeviceStage,
                    confirmed: false,
                    detail: Some(detail.clone()),
                    value: None,
                };
                ToolResult::failed(&code, detail, true).with_fact(fact)
            }
        }
    }
}

/// `music.control`: pause / resume / next / stop for whatever is playing —
/// media first, a program bookmark second, an explicit `target` when the
/// person named one. Two candidates and no target = ask, never guess.
pub struct MusicControlTool {
    rt: Arc<MusicRuntime>,
}

impl MusicControlTool {
    pub fn new(rt: Arc<MusicRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for MusicControlTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "music.control".into(),
            service: "music".into(),
            side_effect: SideEffect::Mutate,
            description: "控制当前正在播放/暂停的内容：action=pause 暂停、resume 继续（从暂停位置）、next 下一首/下一集/换个台（不重新搜索，电台会切到队列里的下一个台）、previous 上一首/上一集、stop 停止并丢弃、repeat 设置音乐循环模式（需配合 mode=off|one|all：off 关闭循环、one 单曲循环、all 列表循环；仅支持音乐，电台不能循环）。target 不填就按「当前可控对象」：正在放的音乐/故事/电台优先，其次暂停的节目；两者都在时会反问是哪一项".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["pause", "resume", "next", "previous", "stop", "repeat"] },
                    "mode": {
                        "type": "string",
                        "enum": ["off", "one", "all"],
                        "description": "仅 action=repeat 时必填：off 关闭循环、one 单曲循环、all 列表循环"
                    },
                    "target": { "type": "string", "enum": ["media", "program"],
                                "description": "可选：明确指定控制音乐还是正在讲的内容" }
                },
                "required": ["action"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        // Always ready: `music.control` governs stories, radios, and paused
        // spoken programs in addition to Navidrome/online music.
        Availability::Ready
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let Some(action) = args.get("action").and_then(Value::as_str) else {
            return ToolResult::failed("BAD_ARGS", "缺少 action", false);
        };
        if !matches!(
            action,
            "pause" | "resume" | "next" | "previous" | "stop" | "repeat"
        ) {
            return ToolResult::failed(
                "BAD_ARGS",
                "action 必须是 pause/resume/next/previous/stop/repeat",
                false,
            );
        }
        let mode = args.get("mode").and_then(Value::as_str);
        if action == "repeat" && mode.and_then(super::RepeatMode::parse).is_none() {
            return ToolResult::failed("BAD_ARGS", "设置循环时 mode 必须是 off/one/all", false);
        }
        let target = args.get("target").and_then(Value::as_str);
        let device = match ctx
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(explicit) => explicit.to_string(),
            None => match self.rt.hub.resolve_default_audio_out() {
                Ok(Some(d)) => d.stable_id,
                Ok(None) => {
                    return ToolResult::failed("NO_DEVICE", "没有已连接的输出设备", true);
                }
                Err(msg) => {
                    return ToolResult::failed("AMBIGUOUS_DEVICE", msg, false);
                }
            },
        };
        if ctx.is_lan_exempt() && !self.rt.hub.device_is_known_lan_device(&device) {
            return ToolResult::failed(
                "PAIRING_REQUIRED",
                format!("未认证的局域网访问不允许控制本机音箱「{device}」播放，请使用配对令牌认证"),
                false,
            );
        }
        if !self.rt.hub.device_online_with_cap(&device, "audio_out") {
            return ToolResult::failed(
                "NO_DEVICE",
                format!("输出设备 {device} 当前不在线或不支持音频输出"),
                true,
            );
        }
        match self
            .rt
            .control_with_mode(
                &device,
                action,
                target,
                mode,
                ctx.cancel_flag(),
                Some(&ctx.task_id),
            )
            .await
        {
            Ok(out) => {
                let failed = matches!(
                    out.stop_outcome,
                    Some(crate::gateway::StopOutcome::Failed { .. })
                );
                let status = if failed {
                    ToolStatus::Failed
                } else if out.confirmed || out.no_op {
                    ToolStatus::Completed
                } else {
                    ToolStatus::Unknown
                };
                let mut payload = json!({
                    "device": device, "target": out.target, "action": out.action,
                    "subject": out.subject, "detail": out.detail,
                    "no_op": out.no_op,
                    "stop_outcome": out.stop_outcome,
                });
                if let Some(repeat) = out.repeat {
                    payload["repeat"] = json!(repeat.as_str());
                }
                let mut result = ToolResult {
                    status,
                    evidence: out.evidence.clone(),
                    result: Some(payload),
                    error: if out.confirmed || out.no_op {
                        None
                    } else {
                        Some(ToolError {
                            code: if failed {
                                "CONTROL_FAILED"
                            } else {
                                "CONTROL_UNCONFIRMED"
                            }
                            .into(),
                            retryable: true,
                            detail: out.detail.clone(),
                        })
                    },
                    fact: None,
                };
                if !out.no_op {
                    let (basis, value) = match out.repeat {
                        Some(repeat) => (FactBasis::ReadBack, Some(repeat.label_zh().to_string())),
                        None => (FactBasis::DeviceStage, None),
                    };
                    result = result.with_fact(ToolFact {
                        subject: out.subject.clone(),
                        action: out.action.clone(),
                        basis,
                        confirmed: out.confirmed,
                        detail: Some(out.detail.clone()),
                        value,
                    });
                }
                result
            }
            Err(e) => {
                let detail = match e {
                    MusicError::NoMedia => "现在没有在放音乐",
                    MusicError::NoNext => "已经是最后一首了",
                    MusicError::NoProgram => "现在没有在讲的内容",
                    MusicError::NothingToControl => "现在没有可以暂停或继续的内容",
                    MusicError::AmbiguousTarget => {
                        "现在既有暂停的音乐，也有暂停的故事：要控制哪一个？"
                    }
                    MusicError::RepeatUnsupportedRadio => "电台不能循环",
                    MusicError::RepeatUnsupported => "只有音乐支持循环播放",
                    MusicError::InvalidRepeatMode => "设置循环时 mode 必须是 off/one/all",
                    _ => "这次没能控制播放",
                };
                ToolResult::failed(e.code(), detail, false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigService, MusicConfig, MusicConnection};
    use crate::gateway::DeviceHub;
    use crate::store::Store;
    use axum::{routing::get, Json, Router};
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn music_search_online_fallback_emits_hint() {
        let app = Router::new()
            .route(
                "/rest/search3.view",
                get(
                    |axum::extract::Query(params): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        let q = params.get("query").map(String::as_str).unwrap_or("");
                        let songs = if q.contains("稻香") {
                            json!([{ "id": "lib-1", "title": "稻香", "artist": "周杰伦" }])
                        } else {
                            json!([])
                        };
                        Json(json!({
                            "subsonic-response": {
                                "status": "ok",
                                "searchResult3": { "song": songs }
                            }
                        }))
                    },
                ),
            )
            .route(
                "/v1/search",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "1001",
                            "title": "晴天",
                            "artist": "周杰伦"
                        }]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let svc = ConfigService::new(store.clone());
        svc.save_music(
            MusicConfig {
                enabled: true,
                prefer_local: false,
                library: Some(MusicConnection {
                    endpoint: base.clone(),
                    user: "u".into(),
                }),
                online: Some(MusicConnection {
                    endpoint: base,
                    user: String::new(),
                }),
            },
            Some("lib-secret"),
            Some("online-token"),
            "test",
        )
        .unwrap();

        let rt = MusicRuntime::new(store, DeviceHub::new());
        let tool = MusicSearchTool::new(rt);
        let ctx = ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));

        // 1. Implicit fallback (source=None, library missed, online hit) -> emits hint.
        let res = tool.execute(json!({ "query": "晴天" }), &ctx).await;
        assert_eq!(res.status, ToolStatus::Completed);
        let val = res.result.unwrap();
        assert_eq!(val["online_used"], true);
        assert_eq!(
            val["hint"].as_str(),
            Some("本地曲库没有搜到，已自动通过在线音乐源搜到结果；回复用户时自然说明「本地曲库没有搜到，帮你在线搜到了……」")
        );

        // 2. Explicit source="online" -> no fallback hint.
        let res = tool
            .execute(json!({ "query": "晴天", "source": "online" }), &ctx)
            .await;
        assert_eq!(res.status, ToolStatus::Completed);
        let val = res.result.unwrap();
        assert_eq!(val["online_used"], true);
        assert!(
            val["hint"].is_null(),
            "explicit online must not emit fallback hint"
        );

        // 3. Local library hit -> no fallback hint.
        let res = tool.execute(json!({ "query": "稻香" }), &ctx).await;
        assert_eq!(res.status, ToolStatus::Completed);
        let val = res.result.unwrap();
        assert_eq!(val["online_used"], false);
        assert!(
            val["hint"].is_null(),
            "library hit must not emit fallback hint"
        );
    }
}
