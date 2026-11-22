//! One-click self check (R2-BM6): where in the chain is it broken?
//!
//! The five stages are fixed — `mic → asr → model → playback → services` —
//! and each answers with a NAMED state (`ready`, `no_device`, `inactive`,
//! `stale`, `unconfigured`, `disabled`, `failed`, `unknown`). The console
//! renders this exact JSON; the words are the same ones the service cards use.
//!
//! **Default is passive**: no outbound request, no device action. It reads
//! the `/mode` snapshot, the config state, the voice event counters, the hub
//! snapshot and the latest trace. `deep=true` additionally runs the EXISTING
//! real probes (LLM ping, ASR sample, TTS synthesis, weather/music/home/news/
//! OpenAPI probes) — still never an audible play. That separation is the same
//! one the diagnostics API already documents: passive observation / active
//! test / repair are different operations.

use crate::config::ConfigService;
use crate::gateway::DeviceHub;
use crate::tool::home::HomeRuntime;
use crate::tool::music::MusicRuntime;
use crate::tool::news::NewsRuntime;
use crate::tool::openapi::OpenApiRuntime;
use crate::tool::search::SearchRuntime;
use crate::tool::weather::WeatherRuntime;
use crate::trace::TraceRecorder;
use crate::voice::VoicePipeline;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub struct SelfCheckInput<'a> {
    pub config: &'a ConfigService,
    pub voice: &'a Arc<VoicePipeline>,
    pub hub: &'a Arc<DeviceHub>,
    pub traces: &'a Arc<TraceRecorder>,
    pub music: &'a Arc<MusicRuntime>,
    pub weather: &'a Arc<WeatherRuntime>,
    pub home: &'a Arc<HomeRuntime>,
    pub news: &'a Arc<NewsRuntime>,
    pub openapi: &'a Arc<OpenApiRuntime>,
    pub search: &'a Arc<SearchRuntime>,
}

fn stage(stage: &str, state: &str, detail: impl Into<String>, evidence: Value) -> Value {
    json!({
        "stage": stage,
        "state": state,
        "detail": detail.into(),
        "evidence": evidence,
    })
}

/// The five stages, in order. `required` stages decide `verdict`.
pub async fn run(input: &SelfCheckInput<'_>, deep: bool) -> Value {
    let mode = input.voice.mode_snapshot();
    let hub = input.hub.snapshot();
    let events = input.voice.recent(100);
    let devices = hub["devices"].as_array().cloned().unwrap_or_default();

    let with_cap = |cap: &str| -> Option<Value> {
        devices
            .iter()
            .find(|d| {
                d["online"].as_bool().unwrap_or(false)
                    && d["capabilities"]
                        .as_array()
                        .map(|c| c.iter().any(|x| x == cap))
                        .unwrap_or(false)
            })
            .or_else(|| {
                devices.iter().find(|d| {
                    d["capabilities"]
                        .as_array()
                        .map(|c| c.iter().any(|x| x == cap))
                        .unwrap_or(false)
                })
            })
            .cloned()
    };
    let mic_device = with_cap("audio_in");
    let out_device = with_cap("audio_out");
    let device_mode = |stable_id: Option<&str>| -> Option<Value> {
        let id = stable_id?;
        mode["devices"]
            .as_array()?
            .iter()
            .find(|d| d["device"].as_str() == Some(id))
            .cloned()
    };
    let count =
        |kind: &str| -> usize { events.iter().filter(|e| e["kind"] == json!(kind)).count() };

    // --- mic ---------------------------------------------------------------
    let mic = match &mic_device {
        None => stage(
            "mic",
            "no_device",
            "没有带麦克风的设备：语音输入不可用（文本路径不受影响）",
            json!({ "devices": devices.len() }),
        ),
        Some(d) => {
            let id = d["stable_id"].as_str().unwrap_or("");
            let online = d["online"].as_bool().unwrap_or(false);
            let m = device_mode(Some(id));
            let muted = m
                .as_ref()
                .and_then(|m| m["muted"].as_bool())
                .unwrap_or(false);
            let session = m
                .as_ref()
                .and_then(|m| m["mic"]["session"].as_bool())
                .unwrap_or(false);
            let last = m
                .as_ref()
                .and_then(|m| m["mic"]["last_chunk_ms"].as_u64())
                .unwrap_or(0);
            let now = crate::id::now_ms();
            let age = if last == 0 {
                None
            } else {
                Some(now.saturating_sub(last))
            };
            let cps = m
                .as_ref()
                .and_then(|m| m["mic"]["chunks_per_sec"].as_f64())
                .unwrap_or(0.0);
            let counters = m
                .as_ref()
                .map(|m| m["mic"]["counters"].clone())
                .unwrap_or(json!({}));
            let (state, detail) = if !online {
                ("inactive", format!("{id} 不在线"))
            } else if muted {
                ("inactive", format!("{id} 已静音：按设备上的隐私键恢复"))
            } else if !session {
                ("inactive", format!("{id} 的麦克风会话没有建立"))
            } else if age.is_none() {
                ("stale", "麦克风会话在，但还没有收到音频块".to_string())
            } else if age.unwrap_or(0) > 3_000 {
                (
                    "stale",
                    format!(
                        "最近一个音频块是 {} ms 前（链路可能卡住）",
                        age.unwrap_or(0)
                    ),
                )
            } else if cps < 1.0 {
                (
                    "stale",
                    format!("音频块速率只有 {cps:.1}/s（应在 12/s 左右）"),
                )
            } else {
                (
                    "ready",
                    format!("{id} 在线，麦克风会话 active，{cps:.1} 块/s"),
                )
            };
            stage(
                "mic",
                state,
                detail,
                json!({
                    "device": id, "online": online, "muted": muted,
                    "mic_session": session, "last_chunk_age_ms": age,
                    "chunks_per_sec": cps, "counters": counters,
                }),
            )
        }
    };

    // --- asr ---------------------------------------------------------------
    let runtime = deep.then(|| input.config.runtime_snapshot()).transpose();
    let fallback;
    let services = match &runtime {
        Ok(Some(runtime)) => &runtime.config,
        Ok(None) => {
            fallback = input.config.load().unwrap_or_default();
            &fallback
        }
        Err(_) => {
            fallback = Default::default();
            &fallback
        }
    };
    let asr_configured = services.asr.is_configured() && services.asr.provider != "mock";
    let tts_configured = services.tts.is_configured() && services.tts.provider != "mock";
    let asr_failed = count("mic_asr_failed");
    let asr_ok = count("mic_utterance") + count("mic_wake");
    let mut asr = if !asr_configured {
        stage(
            "asr",
            "unconfigured",
            "语音识别未配置：唤醒与语音输入不可用（文本路径不受影响）",
            json!({ "provider": services.asr.provider, "configured": false }),
        )
    } else if asr_failed > 0 && asr_ok == 0 {
        stage(
            "asr",
            "failed",
            format!("最近有 {asr_failed} 次识别失败、0 次成功：检查 ASR 服务"),
            json!({ "provider": services.asr.provider, "recent_failures": asr_failed, "recent_successes": asr_ok }),
        )
    } else {
        stage(
            "asr",
            "ready",
            format!(
                "{} · {}（最近 {} 次成功 / {} 次失败）",
                services.asr.provider, services.asr.model, asr_ok, asr_failed
            ),
            json!({ "provider": services.asr.provider, "model": services.asr.model,
                    "recent_failures": asr_failed, "recent_successes": asr_ok }),
        )
    };

    // --- model -------------------------------------------------------------
    let (has_llm_end, has_llm_fail) = latest_trace_flags(input.traces);
    let mut model = if !services.llm.is_configured() {
        stage(
            "model",
            "unconfigured",
            "对话模型未配置：只能做本地固定答复",
            json!({ "provider": services.llm.provider, "configured": false }),
        )
    } else if has_llm_fail && !has_llm_end {
        stage(
            "model",
            "failed",
            "最近一轮模型调用失败：检查模型地址/密钥/额度",
            json!({ "provider": services.llm.provider, "model": services.llm.model }),
        )
    } else {
        stage(
            "model",
            "ready",
            format!("{} · {}", services.llm.provider, services.llm.model),
            json!({ "provider": services.llm.provider, "model": services.llm.model,
                    "last_turn_llm_end": has_llm_end }),
        )
    };

    // --- playback ----------------------------------------------------------
    let tts_ok = count("tts_streamed");
    let tts_failed = count("tts_failed") + count("play_submit_failed");
    let last_stage = out_device
        .as_ref()
        .and_then(|d| d["stable_id"].as_str())
        .and_then(|id| device_mode(Some(id)))
        .and_then(|m| m["outputs"].as_array().and_then(|a| a.first()).cloned())
        .and_then(|o| o["stage"].as_str().map(str::to_string));
    let mut playback = match &out_device {
        None => stage(
            "playback",
            "no_device",
            "没有带扬声器的设备：只能文本回复",
            json!({ "devices": devices.len() }),
        ),
        Some(d) => {
            let id = d["stable_id"].as_str().unwrap_or("");
            let online = d["online"].as_bool().unwrap_or(false);
            let (state, detail) = if !tts_configured {
                (
                    "unconfigured",
                    "语音合成未配置：设备不会出声（文本路径不受影响）".to_string(),
                )
            } else if !online {
                ("inactive", format!("{id} 不在线：无法播放"))
            } else if tts_failed > 0 && tts_ok == 0 {
                (
                    "failed",
                    format!("最近有 {tts_failed} 次合成/提交失败、0 次成功"),
                )
            } else {
                (
                    "ready",
                    format!(
                        "{id} 在线；TTS {} · {}（最近 {} 次成功 / {} 次失败）",
                        services.tts.provider, services.tts.model, tts_ok, tts_failed
                    ),
                )
            };
            stage(
                "playback",
                state,
                detail,
                json!({
                    "device": id, "online": online,
                    "tts_provider": services.tts.provider, "tts_model": services.tts.model,
                    "recent_successes": tts_ok, "recent_failures": tts_failed,
                    "last_output_stage": last_stage,
                }),
            )
        }
    };

    // --- services ----------------------------------------------------------
    let music_cfg = input.music.config();
    let music_configured = music_cfg.library.is_some() || music_cfg.online.is_some();
    let music_state = if !music_configured {
        "unconfigured"
    } else if !music_cfg.enabled {
        "disabled"
    } else {
        "ready"
    };
    let weather_state = input.weather.config().state_word();
    let home_state = match input.home.availability() {
        crate::tool::Availability::Ready => "ready",
        crate::tool::Availability::Disabled => "disabled",
        _ => "unconfigured",
    };
    let news_state = input.news.config().state_word();
    let openapi_state = input.openapi.config().state_word();
    let search_state = input.search.state_word();
    let mut service_states = json!({
        "weather": weather_state,
        "music": music_state,
        "home": home_state,
        "news": news_state,
        "openapi": openapi_state,
        "search": search_state,
    });
    let mut service_probes = json!({});
    let mut services_state = "ready";
    let mut services_detail = format!(
        "weather {} · music {} · home {} · news {} · openapi {} · search {}",
        weather_state, music_state, home_state, news_state, openapi_state, search_state
    );

    // --- deep probes (opt-in; real calls, never audible) -------------------
    if let Ok(Some(runtime)) = &runtime {
        if asr_configured {
            let probe = tokio::time::timeout(Duration::from_secs(15), async {
                match runtime.asr() {
                    Ok(client) => {
                        const SAMPLE: &[u8] = include_bytes!("../assets/asr-sample-zh.wav");
                        match client.transcribe(SAMPLE).await {
                            Ok(text) => json!({ "ok": true, "text": text }),
                            Err(e) => json!({ "ok": false, "error": e.to_string() }),
                        }
                    }
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                }
            })
            .await
            .unwrap_or_else(|_| json!({ "ok": false, "error": "ASR 探针超时" }));
            asr["evidence"]["probe"] = probe.clone();
            if probe["ok"] != json!(true) {
                asr["state"] = json!("failed");
                asr["detail"] = json!(format!(
                    "ASR 真实转写失败：{}",
                    probe["error"].as_str().unwrap_or("未知错误")
                ));
            }
        }
        if services.llm.is_configured() {
            let probe = tokio::time::timeout(Duration::from_secs(15), async {
                match runtime.llm("llm") {
                    Ok(client) => match client.ping(&services.llm.model).await {
                        Ok((reply, ms)) => json!({ "ok": true, "reply": reply, "latency_ms": ms }),
                        Err(e) => json!({ "ok": false, "error": e.to_string() }),
                    },
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                }
            })
            .await
            .unwrap_or_else(|_| json!({ "ok": false, "error": "模型探针超时" }));
            model["evidence"]["probe"] = probe.clone();
            if probe["ok"] != json!(true) {
                model["state"] = json!("failed");
                model["detail"] = json!(format!(
                    "模型真实调用失败：{}",
                    probe["error"].as_str().unwrap_or("未知错误")
                ));
            }
        }
        if tts_configured {
            let probe = tokio::time::timeout(Duration::from_secs(15), async {
                match runtime.tts() {
                    Ok(client) => match client.synthesize("连接测试").await {
                        Ok(audio) => json!({ "ok": true, "audio_ms": audio.duration_ms, "bytes": audio.wav.len() }),
                        Err(e) => json!({ "ok": false, "error": e.to_string() }),
                    },
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                }
            })
            .await
            .unwrap_or_else(|_| json!({ "ok": false, "error": "TTS 探针超时" }));
            playback["evidence"]["probe"] = probe.clone();
            if probe["ok"] != json!(true) {
                playback["state"] = json!("failed");
                playback["detail"] = json!(format!(
                    "TTS 真实合成失败：{}",
                    probe["error"].as_str().unwrap_or("未知错误")
                ));
            }
        } else if services.tts.provider == "mock" {
            playback["evidence"]["probe"] = json!({"state": "skipped", "provider": "mock"});
        }
    } else if runtime.is_err() {
        let config_unavailable = |name| {
            stage(
                name,
                "failed",
                "无法读取一致的服务配置，请重试",
                json!({"error": "SERVICE_SNAPSHOT_UNAVAILABLE"}),
            )
        };
        asr = config_unavailable("asr");
        model = config_unavailable("model");
        if out_device.is_some() {
            playback = config_unavailable("playback");
        }
    }
    if deep {
        // Service probes: only configured/ready services are called (the
        // unconfigured ones stay zero-HTTP by construction).
        let mut probes = serde_json::Map::new();
        if weather_state == "ready" {
            probes.insert("weather".into(), input.weather.probe().await);
        }
        if music_state == "ready" {
            probes.insert("music".into(), input.music.probe().await);
        }
        if home_state == "ready" {
            probes.insert("home".into(), input.home.probe().await);
        }
        if news_state == "ready" {
            probes.insert("news".into(), input.news.probe().await);
        }
        if openapi_state == "ready" {
            probes.insert("openapi".into(), input.openapi.probe().await);
        }
        if search_state == "ready" {
            probes.insert("search".into(), input.search.probe().await);
        }
        let mut failures = Vec::new();
        for (name, probe) in &probes {
            let ok = probe["ok"].as_bool().unwrap_or(false);
            if !ok {
                failures.push(name.clone());
                if let Some(obj) = service_states.as_object_mut() {
                    obj.insert(name.clone(), json!("failed"));
                }
            }
        }
        if !failures.is_empty() {
            services_state = "failed";
            services_detail = format!("{} 探针失败：{}", services_detail, failures.join("、"));
        }
        service_probes = Value::Object(probes);
    }

    let services_stage = stage(
        "services",
        services_state,
        services_detail,
        json!({ "states": service_states, "probes": service_probes, "deep": deep }),
    );

    let stages = vec![mic, asr, model, playback, services_stage];
    let required = ["mic", "asr", "model", "playback"];
    let mut verdict = "ready";
    let mut verdict_stage: Option<&str> = None;
    for s in &stages {
        let name = s["stage"].as_str().unwrap_or("");
        if !required.contains(&name) {
            continue;
        }
        let state = s["state"].as_str().unwrap_or("unknown");
        if state == "failed" {
            verdict = "failed";
            verdict_stage = Some(name);
            break;
        }
        if state != "ready" && verdict_stage.is_none() {
            verdict = "degraded";
            verdict_stage = Some(name);
        }
    }
    // A configured service that failed its real probe is a degraded assistant,
    // not a broken voice chain — but it is still the first thing to look at
    // when the voice stages are fine.
    if verdict == "ready" && services_state == "failed" {
        verdict = "degraded";
        verdict_stage = Some("services");
    }

    // M6b (S3): the last turn's OBSERVED timeline + the journal's bounded
    // health. Both are passive reads (the in-memory ring and a local
    // directory scan) — the passive self-check still makes ZERO requests.
    let last_turn = events.iter().find(|e| e["kind"] == "turn_end").map(|e| {
        json!({
            "ts_ms": e["ts_ms"],
            "status": e["status"],
            "task": e["task"],
            "timeline": e["timeline"],
        })
    });
    let journal = input.voice.events_file_stats();

    json!({
        "verdict": verdict,
        "verdict_stage": verdict_stage,
        "deep": deep,
        "generated_ms": crate::id::now_ms(),
        "note": if deep {
            "深度自检尝试探测已配置的真实服务，不在设备上播放"
        } else {
            "被动自检：没有发起任何外部请求，也没有任何设备动作"
        },
        "stages": stages,
        "last_turn": last_turn,
        "voice_journal": journal,
    })
}

/// Did the latest trace contain an `llm_end` / an `llm_failed`? Only the
/// latest turn is inspected: old failures are history, not current state.
fn latest_trace_flags(traces: &TraceRecorder) -> (bool, bool) {
    let Ok(list) = traces.list(1) else {
        return (false, false);
    };
    let Some(id) = list.first().and_then(|t| t["id"].as_str()) else {
        return (false, false);
    };
    let Ok(Some(detail)) = traces.detail(id) else {
        return (false, false);
    };
    let events = detail["events"].as_array().cloned().unwrap_or_default();
    let mut end = false;
    let mut fail = false;
    for e in events {
        match e["kind"].as_str() {
            Some("llm_end") => end = true,
            Some("llm_failed") | Some("llm_fallback_failed") => fail = true,
            _ => {}
        }
    }
    (end, fail)
}
