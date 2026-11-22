mod common;

use axum::{routing::get, Json, Router};
use hm_core::api::AppState;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{Evidence, Tool, ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn tiny_wav_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    let samples = 1600u32;
    let data_len = samples * 2;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&(16000u32 * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(44 + data_len as usize, 0);
    out
}

#[tokio::test]
async fn story_and_joke_http_api_and_play_stream_end_to_end() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let state = AppState::with_overrides(
        Arc::clone(&store),
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let base = format!("http://{api_addr}");
    let client = reqwest::Client::new();

    // Production policy rejects 127.0.0.1 until allow_loopback_public_for_test is enabled.
    let reject = client
        .put(format!("{base}/api/v0/config/services/story"))
        .json(&json!({ "enabled": true, "url": "http://127.0.0.1:18083" }))
        .send()
        .await
        .unwrap();
    assert_eq!(reject.status(), reqwest::StatusCode::BAD_REQUEST);

    store.allow_loopback_public_for_test();

    // Mock NAS story-svc + joke-svc
    let nas_app = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route(
            "/v1/joke",
            get(|| async { Json(json!({ "id": "j1", "kind": "joke", "text": "哈哈" })) }),
        )
        .route(
            "/v1/search",
            get(|| async {
                Json(json!({
                    "query": "童话",
                    "results": [{ "id": "cat:123:1", "kind": "album", "title": "童话" }]
                }))
            }),
        )
        .route(
            "/v1/album/cat:123",
            get(|| async {
                Json(json!({
                    "id": "cat:123",
                    "episodes": [{ "id": "cat:123:1", "title": "小星星" }]
                }))
            }),
        )
        .route("/v1/stream/cat:123:1", get(|| async { tiny_wav_bytes() }));
    let nas_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_addr = nas_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(nas_listener, nas_app).await.unwrap() });
    let nas_url = format!("http://{nas_addr}");

    // PUT & GET story + joke config (write-only bearer_token_set)
    let put_s: Value = client
        .put(format!("{base}/api/v0/config/services/story"))
        .json(&json!({ "enabled": true, "url": nas_url, "bearer_token": "secret-s" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_s["state"], "ready");
    assert_eq!(put_s["bearer_token_set"], true);
    assert!(put_s.get("bearer_token").is_none());

    let put_j: Value = client
        .put(format!("{base}/api/v0/config/services/joke"))
        .json(&json!({ "enabled": true, "url": nas_url, "bearer_token": "secret-j" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_j["state"], "ready");
    assert_eq!(put_j["bearer_token_set"], true);

    let test_s: Value = client
        .post(format!("{base}/api/v0/config/services/story/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(test_s["ok"], true);

    let test_j: Value = client
        .post(format!("{base}/api/v0/config/services/joke/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(test_j["ok"], true);

    // Connect fake Gateway over TCP and verify StoryPlayTool succeeds with DeviceConfirmed + ToolFact
    let gw_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gw_addr = gw_probe.local_addr().unwrap();
    drop(gw_probe);
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(gw_addr).await;
    });
    let _dev = common::spawn_fake_device(gw_addr.port()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.hub.primary_audio_out().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake device registered in GatewayHub");

    let play_tool = hm_core::tool::personal_media::StoryPlayTool::new(state.personal_media.clone());
    let ctx = ToolCtx::new("T-story", "C-story", Arc::new(AtomicBool::new(false)));
    let res = play_tool.execute(json!({ "query": "童话" }), &ctx).await;
    assert_eq!(res.status, ToolStatus::Completed, "{:?}", res.error);
    assert!(matches!(
        res.evidence,
        Evidence::Sent | Evidence::DeviceConfirmed
    ));
    let fact = res.fact.expect("ToolFact required");
    assert!(fact.confirmed);
    assert_eq!(fact.subject, "《小星星》");
    assert_eq!(fact.action, "play");
}

fn wav_bytes_secs(secs: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let samples = 16_000 * secs;
    let data_len = samples * 2;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&(16_000u32 * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(44 + data_len as usize, 0);
    out
}

#[tokio::test]
async fn story_play_registers_in_music_runtime_emits_notices_and_arms_two_second_tail_guard() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let data_dir = std::env::temp_dir().join(format!("hm-story-guard-{}", hm_core::id::now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let state = AppState::with_overrides(
        Arc::clone(&store),
        data_dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let base = format!("http://{api_addr}");
    let client = reqwest::Client::new();

    // Mock 4.5s story stream so playback stays active > 1.2s while PLAY_AHEAD_SECS = 3.0.
    let nas_app = Router::new().route("/v1/stream/cat:long:1", get(|| async { wav_bytes_secs(5) }));
    let nas_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_addr = nas_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(nas_listener, nas_app).await.unwrap() });

    let put_s: Value = client
        .put(format!("{base}/api/v0/config/services/story"))
        .json(&json!({ "enabled": true, "url": format!("http://{nas_addr}") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_s["state"], "ready");

    let gw_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gw_addr = gw_probe.local_addr().unwrap();
    drop(gw_probe);
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(gw_addr).await;
    });
    let _dev = common::spawn_fake_device(gw_addr.port()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.hub.primary_audio_out().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake device registered in GatewayHub");

    let play_tool = hm_core::tool::personal_media::StoryPlayTool::new(state.personal_media.clone());
    let ctx = ToolCtx::new(
        "T-story-long",
        "C-story-long",
        Arc::new(AtomicBool::new(false)),
    );
    let res = play_tool
        .execute(json!({ "track_id": "cat:long:1" }), &ctx)
        .await;
    assert_eq!(res.status, ToolStatus::Completed, "{:?}", res.error);
    let play_id = res.result.unwrap()["play_id"].as_str().unwrap().to_string();

    // While streaming, MusicRuntime + Sovereignty must observe the active story play.
    let active_mode = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let m: Value = client
                .get(format!("{base}/api/v0/mode"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let mp = m["music_playback"].as_array().cloned().unwrap_or_default();
            let ln = m["local_music_notices"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if mp.iter().any(|p| p["play_id"] == play_id)
                && ln.iter().any(|p| p["play_id"] == play_id)
            {
                return m;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("story.play published active MusicPlaybackFact");
    let dev_entry = active_mode["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert_eq!(dev_entry["media_guard"]["play_id"], play_id);
    assert_eq!(dev_entry["media_guard"]["uncertain"], true);

    // Wait for natural stream finish (5s - 3s lookahead = ~2s) -> FakeDevice emits PlayFinished.
    let ended_mode = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let m: Value = client
                .get(format!("{base}/api/v0/mode"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let dev = m["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["device"] == "dot2-d")
                .unwrap()
                .clone();
            if dev["media_guard"]["uncertain"] == false
                && m["music_playback"].as_array().unwrap().is_empty()
            {
                return m;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("story.play terminal transition armed 2s guard and retired notice");
    let dev_after = ended_mode["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap();
    assert!(
        dev_after["media_guard"]["last_media_end_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        dev_after["media_guard"]["guard_remaining_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    drop(_dev);
    drop(state);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn story_play_cancel_flag_dispatches_hardware_stop() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let data_dir = std::env::temp_dir().join(format!("hm-story-cancel-{}", hm_core::id::now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let state = AppState::with_overrides(
        Arc::clone(&store),
        data_dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let base = format!("http://{api_addr}");
    let client = reqwest::Client::new();

    // Must exceed `MusicRuntime::PLAY_AHEAD_SECS` (20 s) so the background pump
    // stays mid-stream instead of sending `PlayEnd` (which `FakeDevice` confirms
    // synchronously as `PlayFinished`) before `cancel_flag` flips.
    let nas_app = Router::new().route(
        "/v1/stream/cat:cancel:1",
        get(|| async { wav_bytes_secs(25) }),
    );
    let nas_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_addr = nas_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(nas_listener, nas_app).await.unwrap() });

    let put_s: Value = client
        .put(format!("{base}/api/v0/config/services/story"))
        .json(&json!({ "enabled": true, "url": format!("http://{nas_addr}") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_s["state"], "ready");

    let gw_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gw_addr = gw_probe.local_addr().unwrap();
    drop(gw_probe);
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(gw_addr).await;
    });
    let _dev = common::spawn_fake_device(gw_addr.port()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.hub.primary_audio_out().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake device registered in GatewayHub");

    let cancel = Arc::new(AtomicBool::new(false));
    let play_tool = hm_core::tool::personal_media::StoryPlayTool::new(state.personal_media.clone());
    let ctx = ToolCtx::new("T-story-cancel", "C-story-cancel", cancel.clone());
    let res = play_tool
        .execute(json!({ "track_id": "cat:cancel:1" }), &ctx)
        .await;
    assert_eq!(res.status, ToolStatus::Completed, "{:?}", res.error);
    let play_id = res.result.unwrap()["play_id"].as_str().unwrap().to_string();

    // Flip cancel_flag while background pump is streaming -> must issue StopStage instead of PlayFinished.
    let mut ctrl = state.hub.subscribe_control();
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let saw_stop = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            match ctrl.recv().await {
                Ok(hm_core::gateway::DeviceEvent::StopStage { play_id: pid, .. })
                    if pid == play_id =>
                {
                    return true;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .expect("cancelling StoryPlayTool must dispatch hardware Stop");
    assert!(saw_stop);

    drop(_dev);
    drop(state);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn story_play_pause_and_resume_bookmark_without_music_configured() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let data_dir = std::env::temp_dir().join(format!("hm-story-resume-{}", hm_core::id::now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let state = AppState::with_overrides(
        Arc::clone(&store),
        data_dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let base = format!("http://{api_addr}");
    let client = reqwest::Client::new();

    let stream_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = stream_hits.clone();
    let nas_app = Router::new().route(
        "/v1/stream/cat:resume:1",
        get(move || {
            let h = hits_clone.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                wav_bytes_secs(8)
            }
        }),
    );
    let nas_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_addr = nas_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(nas_listener, nas_app).await.unwrap() });

    let put_s: Value = client
        .put(format!("{base}/api/v0/config/services/story"))
        .json(&json!({ "enabled": true, "url": format!("http://{nas_addr}") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_s["state"], "ready");

    let gw_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gw_addr = gw_probe.local_addr().unwrap();
    drop(gw_probe);
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(gw_addr).await;
    });
    let _dev = common::spawn_fake_device(gw_addr.port()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.hub.primary_audio_out().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake device registered in GatewayHub");

    // 1. Start story.play (music service itself is completely unconfigured).
    let play_tool = hm_core::tool::personal_media::StoryPlayTool::new(state.personal_media.clone());
    let ctx = ToolCtx::new(
        "T-story-resume",
        "C-story-resume",
        Arc::new(AtomicBool::new(false)),
    );
    let res = play_tool
        .execute(
            json!({ "track_id": "cat:resume:1", "title": "小王子" }),
            &ctx,
        )
        .await;
    assert_eq!(res.status, ToolStatus::Completed, "{:?}", res.error);
    assert_eq!(
        state.music.media_state("dot2-d"),
        Some(hm_core::tool::music::MediaState::Playing)
    );
    assert_eq!(stream_hits.load(Ordering::SeqCst), 1);
    assert_eq!(res.fact.as_ref().unwrap().subject, "《小王子》");

    // 2. Pause via wake-interrupt / pause_now -> bookmark retained as Paused.
    let paused = state.music.pause_now("dot2-d", "keyword_interrupt");
    assert!(
        paused.is_some(),
        "story.play must be pausable via MusicRuntime"
    );
    assert_eq!(
        state.music.media_state("dot2-d"),
        Some(hm_core::tool::music::MediaState::Paused)
    );

    // 3. Resume via MusicRuntime::control("resume") (same path as "继续播放" reflex).
    let resumed = state.music.control("dot2-d", "resume", None).await;
    assert!(
        resumed.is_ok(),
        "story.play bookmark resume must succeed: {:?}",
        resumed.err()
    );
    assert_eq!(
        state.music.media_state("dot2-d"),
        Some(hm_core::tool::music::MediaState::Playing)
    );
    assert_eq!(
        stream_hits.load(Ordering::SeqCst),
        2,
        "resume must re-open story stream"
    );

    drop(_dev);
    drop(state);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn story_play_uses_cached_search_title_and_auto_fallbacks_past_music_decode_failure() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let data_dir =
        std::env::temp_dir().join(format!("hm-story-fallback-{}", hm_core::id::now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let state = AppState::with_overrides(
        Arc::clone(&store),
        data_dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );

    let nas_app = Router::new()
        .route(
            "/v1/search",
            get(|| async {
                Json(json!({
                    "query": "故事",
                    "results": [
                        { "id": "69044752:231", "kind": "episode", "title": "关于房子的悲哀故事！", "source": "rss" },
                        { "id": "web:2015509843", "kind": "web-episode", "title": "安全警长啦咘啦哆 第20集", "source": "web" }
                    ]
                }))
            }),
        )
        .route(
            "/v1/stream/69044752:231",
            get(|| async { b"not-a-valid-wav-or-mp3-stream".to_vec() }),
        )
        .route(
            "/v1/stream/web:2015509843",
            get(|| async { wav_bytes_secs(2) }),
        );
    let nas_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nas_addr = nas_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(nas_listener, nas_app).await.unwrap() });

    let svc = hm_core::config::ConfigService::new(store.clone());
    svc.save_personal_endpoint(
        hm_core::config::STORY_KEY,
        hm_core::config::STORY_TOKEN_SECRET,
        hm_core::config::PersonalEndpointConfig {
            enabled: true,
            url: format!("http://{nas_addr}"),
            timeout_ms: 4000,
        },
        None,
        "test",
    )
    .unwrap();

    let gw_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gw_addr = gw_probe.local_addr().unwrap();
    drop(gw_probe);
    let hub = state.hub.clone();
    tokio::spawn(async move {
        let _ = hub.listen(gw_addr).await;
    });
    let _dev = common::spawn_fake_device(gw_addr.port()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.hub.primary_audio_out().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake device registered");

    let search_tool =
        hm_core::tool::personal_media::StorySearchTool::new(state.personal_media.clone());
    let play_tool = hm_core::tool::personal_media::StoryPlayTool::new(state.personal_media.clone());
    let ctx = ToolCtx::new("T-fb", "C-fb", Arc::new(AtomicBool::new(false)));

    let sr = search_tool.execute(json!({ "query": "故事" }), &ctx).await;
    assert_eq!(sr.status, ToolStatus::Completed);

    // Calling story.play with the unplayable track_id ("69044752:231") and NO title argument
    // must automatically fall back to "web:2015509843" AND resolve its Chinese title from cache!
    let pr = play_tool
        .execute(json!({ "track_id": "69044752:231" }), &ctx)
        .await;
    assert_eq!(pr.status, ToolStatus::Completed, "{:?}", pr.error);
    let res_val = pr.result.unwrap();
    assert_eq!(res_val["track_id"], "web:2015509843");
    assert_eq!(res_val["title"], "安全警长啦咘啦哆 第20集");
    assert_eq!(pr.fact.unwrap().subject, "《安全警长啦咘啦哆 第20集》");

    // Subsequent search filters out the known-unplayable track_id.
    let sr2 = search_tool.execute(json!({ "query": "故事" }), &ctx).await;
    let sr2_val = sr2.result.unwrap();
    let ids: Vec<&str> = sr2_val["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["web:2015509843"]);

    drop(_dev);
    drop(state);
    let _ = std::fs::remove_dir_all(&data_dir);
}
