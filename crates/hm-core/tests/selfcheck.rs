//! R2-BM6: the one-click self check. The passive path must make ZERO
//! outbound requests and ZERO device actions; the deep path must use the
//! existing real probes (never an audible play). The stage words are the same
//! ones the console cards use.

use hm_core::api::{router, AppState};
use hm_core::config::{ServiceConfig, WeatherConfig, WeatherPlace};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

// --- fake services ----------------------------------------------------------

struct FakeHttp {
    base: String,
    hits: Arc<AtomicU64>,
}

async fn spawn_fake_weather(fail: bool) -> FakeHttp {
    use axum::response::IntoResponse;
    let hits = Arc::new(AtomicU64::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/v1/forecast",
        axum::routing::get(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                if fail {
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
                }
                axum::Json(json!({
                    "latitude": 30.25, "longitude": 120.16,
                    "timezone": "Asia/Shanghai",
                    "current": {
                        "time": "2026-11-13T10:00",
                        "temperature_2m": 18.0, "apparent_temperature": 17.0,
                        "relative_humidity_2m": 50, "precipitation": 0.0,
                        "weather_code": 0, "wind_speed_10m": 5.0
                    },
                    "daily": {
                        "time": ["2026-11-13"],
                        "temperature_2m_max": [20.0], "temperature_2m_min": [12.0],
                        "weather_code": [0], "precipitation_probability_max": [0]
                    }
                }))
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeHttp {
        base: format!("http://{addr}"),
        hits,
    }
}

async fn spawn_fake_llm(auth_ok: bool) -> FakeHttp {
    spawn_llm_expect(auth_ok, "deepseek-flash", "llm-test-key").await
}

async fn spawn_llm_expect(auth_ok: bool, model: &'static str, key: &'static str) -> FakeHttp {
    use axum::response::IntoResponse;
    let hits = Arc::new(AtomicU64::new(0));
    let h = hits.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    let ok = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .map(|v| v == key)
                        .unwrap_or(false);
                    if !auth_ok || !ok || body["model"] != model {
                        return (axum::http::StatusCode::UNAUTHORIZED, "no").into_response();
                    }
                    axum::Json(json!({
                        "choices": [{ "message": { "content": "正常" }, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
                    }))
                    .into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeHttp {
        base: format!("http://{addr}"),
        hits,
    }
}

async fn app(llm: &FakeHttp, weather: &FakeHttp) -> (String, Arc<AppState>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::new(
        store,
        PathBuf::from("./data-test"),
        MockInject::default(),
        None,
    );
    state
        .config
        .save_service(
            "llm",
            ServiceConfig {
                provider: "deepseek".into(),
                base_url: llm.base.clone(),
                model: "deepseek-flash".into(),
                api_key_env: String::new(),
                voice: String::new(),
                thinking: String::new(),
                language: String::new(),
            },
            Some("llm-test-key"),
            "test",
        )
        .unwrap();
    state
        .config
        .save_weather(
            WeatherConfig {
                enabled: true,
                provider: "open_meteo".into(),
                endpoint: format!("{}/v1/forecast", weather.base),
                api_key_env: String::new(),
                places: vec![WeatherPlace {
                    alias: "家".into(),
                    name: "杭州".into(),
                    latitude: 30.25,
                    longitude: 120.16,
                    timezone: "Asia/Shanghai".into(),
                    city_code: String::new(),
                }],
                default_alias: "家".into(),
                max_days: 1,
            },
            None,
            "test",
        )
        .unwrap();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

async fn self_check(base: &str, deep: bool) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/api/v0/diagnostics/self_check"))
        .json(&json!({ "deep": deep }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn stage<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == json!(name))
        .unwrap_or_else(|| panic!("no stage {name}: {v}"))
}

#[tokio::test]
async fn a_passive_self_check_makes_zero_external_requests() {
    let llm = spawn_fake_llm(true).await;
    let weather = spawn_fake_weather(false).await;
    let (base, _state) = app(&llm, &weather).await;
    let before_llm = llm.hits.load(Ordering::SeqCst);
    let before_weather = weather.hits.load(Ordering::SeqCst);

    let v = self_check(&base, false).await;
    assert_eq!(v["deep"], false);
    assert_eq!(v["stages"].as_array().unwrap().len(), 5, "{v}");
    let names: Vec<&str> = v["stages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["stage"].as_str())
        .collect();
    assert_eq!(names, vec!["mic", "asr", "model", "playback", "services"]);

    // Text-only deployment: no audio device, no ASR configured.
    assert_eq!(stage(&v, "mic")["state"], "no_device");
    assert_eq!(stage(&v, "asr")["state"], "unconfigured");
    assert_eq!(stage(&v, "playback")["state"], "no_device");
    assert_eq!(stage(&v, "model")["state"], "ready");
    assert_eq!(stage(&v, "services")["state"], "ready");
    assert_eq!(
        stage(&v, "services")["evidence"]["states"]["weather"],
        "ready"
    );
    assert_eq!(
        stage(&v, "services")["evidence"]["states"]["news"],
        "unconfigured"
    );
    assert_eq!(v["verdict"], "degraded");
    assert_eq!(v["verdict_stage"], "mic");

    assert_eq!(
        llm.hits.load(Ordering::SeqCst),
        before_llm,
        "passive must not call the model"
    );
    assert_eq!(
        weather.hits.load(Ordering::SeqCst),
        before_weather,
        "passive must not call weather"
    );
}

#[tokio::test]
async fn a_deep_self_check_really_calls_the_configured_services() {
    let llm = spawn_fake_llm(true).await;
    let weather = spawn_fake_weather(false).await;
    let (base, _state) = app(&llm, &weather).await;
    llm.hits.store(0, Ordering::SeqCst);
    weather.hits.store(0, Ordering::SeqCst);

    let v = self_check(&base, true).await;
    assert_eq!(v["deep"], true);
    assert_eq!(stage(&v, "model")["state"], "ready", "{v}");
    assert_eq!(stage(&v, "model")["evidence"]["probe"]["ok"], true);
    assert_eq!(stage(&v, "services")["state"], "ready", "{v}");
    assert_eq!(
        stage(&v, "services")["evidence"]["probes"]["weather"]["state"],
        "ready"
    );
    assert!(
        llm.hits.load(Ordering::SeqCst) >= 1,
        "deep must call the model"
    );
    assert!(
        weather.hits.load(Ordering::SeqCst) >= 1,
        "deep must call weather"
    );
}

#[tokio::test]
async fn a_failing_model_is_located_by_the_deep_check() {
    let llm = spawn_fake_llm(false).await;
    let weather = spawn_fake_weather(false).await;
    let (base, _state) = app(&llm, &weather).await;
    let v = self_check(&base, true).await;
    assert_eq!(stage(&v, "model")["state"], "failed", "{v}");
    assert_eq!(v["verdict"], "failed");
    assert_eq!(v["verdict_stage"], "model");
}

#[tokio::test]
async fn a_failing_service_degrades_the_verdict_and_names_the_service() {
    let llm = spawn_fake_llm(true).await;
    let weather = spawn_fake_weather(true).await;
    let (base, _state) = app(&llm, &weather).await;
    let v = self_check(&base, true).await;
    assert_eq!(stage(&v, "services")["state"], "failed", "{v}");
    assert_eq!(
        stage(&v, "services")["evidence"]["states"]["weather"],
        "failed"
    );
    // This deployment is text-only, so `mic` is the first missing required
    // stage; the service failure is still named in its own stage.
    assert_eq!(v["verdict"], "degraded");
    assert_eq!(v["verdict_stage"], "mic");
}

#[tokio::test]
async fn an_unconfigured_asr_is_named_not_probed() {
    let llm = spawn_fake_llm(true).await;
    let weather = spawn_fake_weather(false).await;
    let (base, _state) = app(&llm, &weather).await;
    let v = self_check(&base, true).await;
    assert_eq!(stage(&v, "asr")["state"], "unconfigured");
    assert!(stage(&v, "asr")["evidence"].get("probe").is_none(), "{v}");
}

#[tokio::test]
async fn a_deep_self_check_pins_service_and_key_across_a_config_change() {
    let original_llm = spawn_fake_llm(true).await;
    let replacement_llm = spawn_llm_expect(true, "replacement-model", "replacement-key").await;
    let weather = spawn_fake_weather(false).await;
    let (base, state) = app(&original_llm, &weather).await;

    let asr_started = Arc::new(Notify::new());
    let release_asr = Arc::new(Notify::new());
    let seen = asr_started.clone();
    let release = release_asr.clone();
    let asr = axum::Router::new().route(
        "/audio/transcriptions",
        axum::routing::post(move || {
            let seen = seen.clone();
            let release = release.clone();
            async move {
                seen.notify_one();
                release.notified().await;
                axum::Json(json!({"text": "已转写"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, asr).await.unwrap() });
    state
        .config
        .save_service(
            "asr",
            ServiceConfig {
                provider: "openai-asr".into(),
                base_url: format!("http://{address}"),
                model: "test-asr".into(),
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();

    let check = {
        let base = base.clone();
        tokio::spawn(async move { self_check(&base, true).await })
    };
    tokio::time::timeout(Duration::from_secs(5), asr_started.notified())
        .await
        .expect("the current self-check reached the real ASR HTTP probe");
    let updated: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(&json!({
            "provider": "deepseek",
            "base_url": replacement_llm.base,
            "model": "replacement-model",
            "api_key": "replacement-key",
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["state"], "applied");
    release_asr.notify_one();
    let pinned = check.await.unwrap();
    assert_eq!(stage(&pinned, "asr")["evidence"]["probe"]["ok"], true);
    let model = &stage(&pinned, "model")["evidence"];
    assert_eq!(model["model"], "deepseek-flash");
    assert_eq!(model["probe"]["ok"], true, "{pinned}");
    assert_eq!(original_llm.hits.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_llm.hits.load(Ordering::SeqCst), 0);

    release_asr.notify_one();
    let current = self_check(&base, true).await;
    let model = &stage(&current, "model")["evidence"];
    assert_eq!(model["model"], "replacement-model");
    assert_eq!(model["probe"]["ok"], true, "{current}");
    assert_eq!(original_llm.hits.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_llm.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_service_save_keeps_the_previous_probe_and_history() {
    let original_llm = spawn_fake_llm(true).await;
    let replacement_llm = spawn_llm_expect(true, "replacement-model", "replacement-key").await;
    let weather = spawn_fake_weather(false).await;
    let (base, state) = app(&original_llm, &weather).await;
    let history = state.config.history(10).unwrap();
    state
        .store
        .with(|c| {
            c.execute_batch(
                "CREATE TRIGGER block_llm_secret BEFORE UPDATE OF value ON secrets
                 WHEN OLD.name='llm.api_key'
                 BEGIN SELECT RAISE(ABORT, 'test credential write failure'); END;",
            )?;
            Ok(())
        })
        .unwrap();

    let response = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/llm"))
        .json(&json!({
            "provider": "deepseek",
            "base_url": replacement_llm.base,
            "model": "replacement-model",
            "api_key": "replacement-key",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(state.config.history(10).unwrap(), history);

    let check = self_check(&base, true).await;
    let model = &stage(&check, "model")["evidence"];
    assert_eq!(model["model"], "deepseek-flash");
    assert_eq!(model["probe"]["ok"], true, "{check}");
    assert_eq!(original_llm.hits.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_llm.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unreadable_service_snapshot_does_not_report_a_successful_model_probe() {
    let llm = spawn_fake_llm(true).await;
    let weather = spawn_fake_weather(false).await;
    let (base, state) = app(&llm, &weather).await;
    state.store.config_set("services", "invalid JSON").unwrap();

    let check = self_check(&base, true).await;
    let model = stage(&check, "model");
    assert_eq!(model["state"], "failed");
    assert_eq!(model["evidence"]["error"], "SERVICE_SNAPSHOT_UNAVAILABLE");
    assert!(model["evidence"].get("probe").is_none());
    assert_eq!(llm.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mock_speech_does_not_count_as_a_real_probe() {
    let llm = spawn_fake_llm(true).await;
    let weather = spawn_fake_weather(false).await;
    let (base, state) = app(&llm, &weather).await;
    for slot in ["asr", "tts"] {
        state
            .config
            .save_service(
                slot,
                ServiceConfig {
                    provider: "mock".into(),
                    ..Default::default()
                },
                None,
                "test",
            )
            .unwrap();
    }

    let check = self_check(&base, true).await;
    let asr = stage(&check, "asr");
    assert_eq!(asr["state"], "unconfigured");
    assert!(asr["evidence"].get("probe").is_none());
    assert_eq!(
        stage(&check, "playback")["evidence"]["probe"],
        json!({"state": "skipped", "provider": "mock"}),
    );
}
