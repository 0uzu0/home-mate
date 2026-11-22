//! Actual registered music tools, decoded synthetic source, protocol bytes,
//! lifecycle producer, sovereignty projection and management readback.
use super::*;

async fn mode(base: &str) -> Value {
    reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
fn device(mode: &Value) -> &Value {
    mode["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == "dot2-d")
        .unwrap()
}
async fn wait_mode(base: &str, check: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let m = mode(base).await;
            if check(&m) {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("lifecycle state within contract")
}
async fn play(state: &Arc<AppState>, gw: &Arc<FakeGateway>, task: &str) -> String {
    let ctx = ToolCtx::new(task, "music-lifecycle", Arc::new(AtomicBool::new(false)));
    let found = state
        .registry
        .call("music.search", json!({"query":"晚风"}), &ctx)
        .await;
    assert_eq!(found.status, ToolStatus::Completed);
    let state = state.clone();
    let start = tokio::spawn(async move {
        state
            .registry
            .call("music.play", json!({"source":"library","id":"t01"}), &ctx)
            .await
    });
    let (id, _) = gw.until_media_play_opts(false).await;
    for stage in ["accepted", "streaming"] {
        gw.send(&GatewayFrame::PlayReceipt {
            task_id: id.clone(),
            stage: stage.into(),
            detail: None,
            ts: hm_core::id::now_ms(),
        })
        .await;
    }
    assert_eq!(start.await.unwrap().status, ToolStatus::Completed);
    id
}
async fn control(
    state: &Arc<AppState>,
    action: &str,
) -> tokio::task::JoinHandle<hm_core::tool::ToolResult> {
    let state = state.clone();
    let action = action.to_string();
    tokio::spawn(async move {
        state
            .registry
            .call(
                "music.control",
                json!({"action":action,"target":"media"}),
                &ToolCtx::new(
                    "control-lifecycle",
                    "music-lifecycle",
                    Arc::new(AtomicBool::new(false)),
                ),
            )
            .await
    })
}

#[tokio::test]
async fn music_lifecycle_renews_after_fifteen_seconds_then_real_finish_guards() {
    let (base, state, gw, source) = spawn_app_with_gateway_media(FakeLlm::new(vec![])).await;
    let peer_store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let peer = AppState::new(
        peer_store,
        std::env::temp_dir().join(format!("hm-media-peer-{}", hm_core::id::now_ms())),
        MockInject::default(),
        None,
    );
    let peer_token = peer.auth.provision("peer-b").unwrap();
    let peer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_base = format!("http://{}", peer_listener.local_addr().unwrap());
    tokio::spawn({
        let app = hm_core::api::router(peer.clone());
        async move { axum::serve(peer_listener, app).await.unwrap() }
    });
    state
        .fleet
        .upsert("peer-b", &peer_base, Some(&peer_token), false)
        .unwrap();

    source
        .duration_secs
        .store(60, std::sync::atomic::Ordering::SeqCst);
    let id = play(&state, &gw, "renew").await;
    let first = wait_mode(&base, |m| {
        m["music_playback"].as_array().unwrap().len() == 1
    })
    .await;
    assert_eq!(first["local_music_notices"][0]["play_id"], id);
    let seq = first["music_playback"][0]["sequence"].as_u64().unwrap();

    let peer_initial = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let list = peer
                .voice
                .sovereignty
                .live_playback_notices(hm_core::id::now_ms());
            if let Some(n) = list.into_iter().find(|n| n.device == "dot2-d") {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("peer received initial media FleetPlaybackNotice");
    assert_eq!(peer_initial.channel, "media");
    assert!(peer_initial.active);

    tokio::time::sleep(Duration::from_millis(15_200)).await;
    let renewed = mode(&base).await;
    assert_eq!(renewed["music_playback"][0]["play_id"], id);
    assert!(renewed["music_playback"][0]["sequence"].as_u64().unwrap() >= seq + 14);
    assert_eq!(renewed["local_music_notices"][0]["play_id"], id);
    assert_eq!(
        device(&renewed)["media_guard"]["last_media_end_ms"],
        Value::Null
    );

    let peer_live = peer
        .voice
        .sovereignty
        .live_playback_notices(hm_core::id::now_ms());
    let peer_renewed = peer_live
        .into_iter()
        .find(|n| n.device == "dot2-d")
        .expect("ARB-07: continuous >15s playback must stay live on peer Core");
    assert!(peer_renewed.at_ms > peer_initial.at_ms);

    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: id,
        played_ms: 15_200,
        ts: hm_core::id::now_ms(),
        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    let ended = wait_mode(&base, |m| device(m)["media_guard"]["uncertain"] == false).await;
    assert!(ended["music_playback"].as_array().unwrap().is_empty());
    assert!(ended["local_music_notices"].as_array().unwrap().is_empty());
    assert!(
        device(&ended)["media_guard"]["last_media_end_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        device(&ended)["media_guard"]["guard_remaining_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(device(&ended)["input_window_open"], false);
    assert_eq!(source.count("stream:"), 1);

    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if peer
                .voice
                .sovereignty
                .live_playback_notices(hm_core::id::now_ms())
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("peer notice cleared immediately on PlayFinished");
}

#[tokio::test]
async fn music_lifecycle_pause_resume_unknown_stop_and_stale_finish() {
    let (base, state, gw, source) = spawn_app_with_gateway_media(FakeLlm::new(vec![])).await;
    source
        .duration_secs
        .store(60, std::sync::atomic::Ordering::SeqCst);
    let old = play(&state, &gw, "pause").await;
    wait_mode(&base, |m| {
        !m["music_playback"].as_array().unwrap().is_empty()
    })
    .await;
    let pause = control(&state, "pause").await;
    gw.until(|f| matches!(f,CoreFrame::Stop{task_id,..} if task_id == &old))
        .await;
    let logical = mode(&base).await;
    assert_eq!(logical["media"]["dot2-d"]["state"], "paused");
    assert_eq!(device(&logical)["media_guard"]["uncertain"], true);
    assert_eq!(
        device(&logical)["media_guard"]["last_media_end_ms"],
        Value::Null
    );
    assert!(logical["music_playback"].as_array().unwrap().is_empty());
    gw.confirm_stop(&old).await;
    assert_eq!(pause.await.unwrap().status, ToolStatus::Completed);
    wait_mode(&base, |m| device(m)["media_guard"]["uncertain"] == false).await;
    let resume = control(&state, "resume").await;
    let (fresh, _) = gw.until_media_play_opts(false).await;
    for stage in ["accepted", "streaming"] {
        gw.send(&GatewayFrame::PlayReceipt {
            task_id: fresh.clone(),
            stage: stage.into(),
            detail: None,
            ts: hm_core::id::now_ms(),
        })
        .await;
    }
    assert_eq!(resume.await.unwrap().status, ToolStatus::Completed);
    wait_mode(&base, |m| m["music_playback"][0]["play_id"] == fresh).await;
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: old,
        played_ms: 100,
        ts: hm_core::id::now_ms(),
        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    assert_eq!(mode(&base).await["music_playback"][0]["play_id"], fresh);
    let stop = control(&state, "stop").await;
    gw.until(|f| matches!(f,CoreFrame::Stop{task_id,..} if task_id == &fresh))
        .await;
    gw.send(&GatewayFrame::StopReceipt {
        task_id: fresh.clone(),
        stage: "unknown".into(),
        played_ms: None,
        consumed_ms: None,
        progress_basis: None,
        detail: Some("unverified_flush".into()),
    })
    .await;
    assert_eq!(stop.await.unwrap().status, ToolStatus::Unknown);
    let uncertain = mode(&base).await;
    assert_eq!(device(&uncertain)["media_guard"]["uncertain"], true);
    assert_eq!(
        device(&uncertain)["media_guard"]["last_media_end_ms"],
        Value::Null
    );
    assert!(uncertain["music_playback"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn music_lifecycle_disconnect_revokes_notice_without_inventing_end() {
    let (base, state, gw, source) = spawn_app_with_gateway_media(FakeLlm::new(vec![])).await;
    let peer_store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let peer = AppState::new(
        peer_store,
        std::env::temp_dir().join(format!("hm-media-disc-peer-{}", hm_core::id::now_ms())),
        MockInject::default(),
        None,
    );
    let peer_token = peer.auth.provision("peer-b").unwrap();
    let peer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_base = format!("http://{}", peer_listener.local_addr().unwrap());
    tokio::spawn({
        let app = hm_core::api::router(peer.clone());
        async move { axum::serve(peer_listener, app).await.unwrap() }
    });
    state
        .fleet
        .upsert("peer-b", &peer_base, Some(&peer_token), false)
        .unwrap();

    source
        .duration_secs
        .store(60, std::sync::atomic::Ordering::SeqCst);
    play(&state, &gw, "disconnect").await;
    wait_mode(&base, |m| {
        !m["music_playback"].as_array().unwrap().is_empty()
    })
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !peer
                .voice
                .sovereignty
                .live_playback_notices(hm_core::id::now_ms())
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("peer received media notice before disconnect");

    gw.close().await;
    let lost = wait_mode(&base, |m| {
        m["music_playback"].as_array().unwrap().is_empty()
    })
    .await;
    assert!(lost["local_music_notices"].as_array().unwrap().is_empty());
    assert_eq!(device(&lost)["media_guard"]["uncertain"], true);
    assert_eq!(
        device(&lost)["media_guard"]["last_media_end_ms"],
        Value::Null
    );

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if peer
                .voice
                .sovereignty
                .live_playback_notices(hm_core::id::now_ms())
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("peer notice revoked on DeviceLeft");
}

#[tokio::test]
async fn music_lifecycle_tail_capture_remains_denied_after_delayed_asr_with_fallback_on_and_off() {
    for fallback in [false, true] {
        let (started, mut pending) = mpsc::unbounded_channel();
        let (base, state, gw, source) = spawn_app_with_gateway_media_asr(
            FakeLlm::new(vec![Scripted::Text {
                chunks: vec!["有效新输入".into()],
            }]),
            Arc::new(D0GatedAsr { started }),
        )
        .await;
        source
            .duration_secs
            .store(60, std::sync::atomic::Ordering::SeqCst);
        reqwest::Client::new().put(format!("{base}/api/v0/mode/dot2-d"))
            .json(&json!({"asr_wake_fallback":fallback,"wake_ack":{"enabled":false},"tail_blind_ms":0}))
            .send().await.unwrap().error_for_status().unwrap();
        let wake = |seq| GatewayFrame::Keyword {
            origin: hm_core::gateway::InputOrigin::Legacy,
            device: "dot2-d".into(),
            ts: hm_core::id::now_ms(),
            seq,
            keyword: "你好高达".into(),
            fed_ms: None,
            self_heard: false,
            rms_dbfs: None,
            lag_ms: None,
            bundle_id: None,
            kws_generation: None,
        };
        gw.send(&wake(1)).await;
        wait_mode(&base, |m| device(m)["input_window_open"] == true).await;
        let id = play(&state, &gw, "tail").await;
        gw.send(&GatewayFrame::PlayFinished {
            device: "dot2-d".into(),
            task_id: id,
            played_ms: 100,
            ts: hm_core::id::now_ms(),
            consumed_ms: None,
            progress_basis: None,
        })
        .await;
        wait_mode(&base, |m| device(m)["media_guard"]["uncertain"] == false).await;
        feed_one_segment(&gw, 2).await;
        let release = tokio::time::timeout(Duration::from_secs(3), pending.recv())
            .await
            .unwrap()
            .unwrap();
        // The transcript arrives AFTER the 2 s guard; its frozen capture was
        // inside the tail, so checking receipt time would incorrectly admit it.
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        release.send(Ok("PASSIVE_TAIL_SENTINEL".into())).unwrap();
        d0_wait_note(&state, "mic_followup_gated_by_media", |e| {
            e["reason"] == "actual_media_lifecycle"
        })
        .await;
        assert!(!state
            .voice
            .recent(100)
            .iter()
            .any(|e| e["kind"] == "request_started"));
        gw.send(&wake(30)).await;
        wait_mode(&base, |m| device(m)["input_window_open"] == true).await;
        feed_one_segment(&gw, 31).await;
        let release = tokio::time::timeout(Duration::from_secs(3), pending.recv())
            .await
            .unwrap()
            .unwrap();
        release.send(Ok("有效新输入".into())).unwrap();
        d0_wait_note(&state, "request_started", |_| true).await;
    }
}

#[tokio::test]
async fn music_lifecycle_failed_pause_keeps_physical_uncertainty_and_no_end() {
    let (base, state, gw, source) = spawn_app_with_gateway_media(FakeLlm::new(vec![])).await;
    source
        .duration_secs
        .store(60, std::sync::atomic::Ordering::SeqCst);
    let id = play(&state, &gw, "failed-pause").await;
    wait_mode(&base, |m| {
        !m["music_playback"].as_array().unwrap().is_empty()
    })
    .await;
    let paused = control(&state, "pause").await;
    gw.until(|f| matches!(f,CoreFrame::Stop{task_id,..} if task_id == &id))
        .await;
    gw.send(&GatewayFrame::StopReceipt {
        task_id: id,
        stage: "flush_failed".into(),
        played_ms: None,
        consumed_ms: None,
        progress_basis: None,
        detail: Some("synthetic_failure".into()),
    })
    .await;
    assert_eq!(paused.await.unwrap().status, ToolStatus::Failed);
    let m = mode(&base).await;
    assert_eq!(m["media"]["dot2-d"]["state"], "paused");
    assert_eq!(device(&m)["media_guard"]["uncertain"], true);
    assert_eq!(device(&m)["media_guard"]["last_media_end_ms"], Value::Null);
    assert!(m["music_playback"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn music_lifecycle_wake_during_play_can_admit_after_confirmed_pause_inside_tail() {
    let (started, mut pending) = mpsc::unbounded_channel();
    let (base, state, gw, source) = spawn_app_with_gateway_media_asr(
        FakeLlm::new(vec![Scripted::Text {
            chunks: vec!["有效接管输入".into()],
        }]),
        Arc::new(D0GatedAsr { started }),
    )
    .await;
    source
        .duration_secs
        .store(60, std::sync::atomic::Ordering::SeqCst);
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"asr_wake_fallback":false,"wake_ack":{"enabled":false},"tail_blind_ms":0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let play_id = play(&state, &gw, "wake-during-play").await;
    wait_mode(&base, |m| m["music_playback"][0]["play_id"] == play_id).await;
    gw.send(&GatewayFrame::Keyword {
        origin: hm_core::gateway::InputOrigin::Legacy,
        device: "dot2-d".into(),
        ts: hm_core::id::now_ms(),
        seq: 1,
        keyword: "你好高达".into(),
        fed_ms: None,
        self_heard: false,
        rms_dbfs: None,
        lag_ms: None,
        bundle_id: None,
        kws_generation: None,
    })
    .await;
    gw.until(|frame| matches!(frame, CoreFrame::Stop { task_id, .. } if task_id == &play_id))
        .await;
    let paused = mode(&base).await;
    assert_eq!(paused["media"]["dot2-d"]["state"], "paused");
    assert_eq!(
        device(&paused)["media_guard"]["last_media_end_ms"],
        Value::Null
    );
    gw.confirm_stop(&play_id).await;
    let settled = wait_mode(&base, |m| device(m)["media_guard"]["uncertain"] == false).await;
    assert!(
        device(&settled)["media_guard"]["guard_remaining_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    // The wake predates this terminal receipt. Its new input remains valid
    // even though the old music's two-second tail has not elapsed.
    assert_eq!(device(&settled)["input_window_open"], true);
    feed_one_segment(&gw, 2).await;
    let release = tokio::time::timeout(Duration::from_secs(3), pending.recv())
        .await
        .unwrap()
        .unwrap();
    release.send(Ok("有效接管输入".into())).unwrap();
    d0_wait_note(&state, "request_started", |_| true).await;
    assert!(!state
        .voice
        .recent(100)
        .iter()
        .any(|e| e["kind"] == "mic_followup_gated_by_media"));
}
