//! Real voice ingress -> durable context owner -> captured outgoing LLM history.
use super::*;

async fn set_context_window(base: &str, window: u64) {
    let _result:Value=reqwest::Client::new().put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"context_window_ms":window,"follow_window_enabled":true,"followup_credits":0,"tail_blind_ms":0}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let readback: Value = reqwest::Client::new()
        .get(format!("{base}/api/v0/mode"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(readback["context_window_ms"], window);
    assert_eq!(readback["tail_blind_ms"],0,"synthetic context tests explicitly disable playback tail masking via the real management API");
}
pub(super) async fn wake(state: &AppState, gw: &FakeGateway, seq: u64) {
    let input = hm_core::id::next("wake");
    gw.send_raw_json(json!({"type":"wake","device":"dot2-d","seq":seq,"ts":hm_core::id::now_ms(),"origin":{"kind":"leased","lease":state.voice.capture_lease("dot2-d"),"origin":{"kind":"text","input_id":input}}})).await;
    d0_wait_note(state, "wake", |v| {
        v["provenance"]["origin"]["origin"]["input_id"] == input
    })
    .await;
}
async fn request(state: &AppState, gw: &FakeGateway, seq: u64, text: &str) {
    gw.send_raw_json(json!({"type":"utterance","device":"dot2-d","seq":seq,"ts":hm_core::id::now_ms(),"source":"text","text":text,"origin":{"kind":"leased","lease":state.voice.capture_lease("dot2-d"),"origin":{"kind":"text","input_id":hm_core::id::next("request")}}})).await;
}
pub(super) async fn finish_answer(state: &AppState, gw: &FakeGateway) {
    let (play, _) = gw.until_play("voice").await;
    gw.confirm_play(&play).await;
    d1_finish_voice(gw, &play).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !state.hub.play_active(&play)
                && state.voice.mode_snapshot()["devices"][0]["current_task"].is_null()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
pub(super) async fn leased_segment(
    state: &AppState,
    gw: &FakeGateway,
    input: &CapturedInput,
    first_seq: u64,
) {
    let lease = state.voice.capture_lease("dot2-d");
    for block in 0..18 {
        let samples = if block < 10 {
            loud_block()
        } else {
            quiet_block()
        };
        let mut frame = input.pcm(first_seq + block, block * 1280, &samples);
        frame["origin"] = json!({"kind":"leased","lease":lease,"origin":frame["origin"]});
        gw.send_raw_json(frame).await;
    }
}
fn age_context(state: &AppState, session: &str, age: u64) {
    // Advance the persisted idle clock of a context produced by real ingress.
    // No fabricated session/media records and no wall-clock long wait.
    state
        .store
        .with(|c| {
            c.execute(
                "UPDATE device_contexts SET last_activity_ms=?1 WHERE session_id=?2",
                rusqlite::params![hm_core::id::now_ms().saturating_sub(age) as i64, session],
            )?;
            Ok(())
        })
        .unwrap();
}
fn prompt(llm: &FakeLlm, index: usize) -> String {
    serde_json::to_string(&llm.seen_requests()[index]).unwrap()
}

#[tokio::test]
async fn context_ttl_real_mode_api_changes_actual_prompt_and_retains_archives() {
    let llm = FakeLlm::new(
        (0..3)
            .map(|_| Scripted::Text {
                chunks: vec!["记住了。".into()],
            })
            .collect(),
    );
    let (base, state, gw) = spawn_app_with_gateway(llm.clone()).await;
    set_context_window(&base, 600_000).await;
    wake(&state, &gw, 1).await;
    request(&state, &gw, 2, "CONTEXT_OLD_PERSONAL_SENTINEL").await;
    finish_answer(&state, &gw).await;
    let original = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    age_context(&state, &original.session_id, 60_001);
    wake(&state, &gw, 3).await;
    request(&state, &gw, 4, "还有刚才那件事").await;
    finish_answer(&state, &gw).await;
    assert!(prompt(&llm, 1).contains("CONTEXT_OLD_PERSONAL_SENTINEL"));
    assert_eq!(
        state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap()
            .session_id,
        original.session_id
    );
    set_context_window(&base, 60_000).await;
    age_context(&state, &original.session_id, 60_001);
    let before = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap()
        .last_activity_ms;
    let read: Value = reqwest::Client::new()
        .get(format!("{base}/api/v0/devices/dot2-d/conversation"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(read["session_id"], original.session_id);
    assert_eq!(
        state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap()
            .last_activity_ms,
        before,
        "management read must not renew"
    );
    wake(&state, &gw, 5).await;
    let current = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    assert_ne!(current.session_id, original.session_id);
    // Even a late historical writer uses the original session identity.
    state
        .sessions
        .append(&original.session_id, "assistant", "LATE_OLD_REPLY_SENTINEL")
        .unwrap();
    let current_activity = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap()
        .last_activity_ms;
    state
        .sessions
        .complete_context(
            &original.session_id,
            original.ticket(),
            hm_core::id::now_ms() + 600_000,
        )
        .unwrap();
    assert_eq!(
        state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap()
            .last_activity_ms,
        current_activity,
        "late old completion cannot renew new context"
    );
    request(&state, &gw, 6, "新的独立话题").await;
    finish_answer(&state, &gw).await;
    let actual = prompt(&llm, 2);
    assert!(
        !actual.contains("CONTEXT_OLD_PERSONAL_SENTINEL"),
        "{actual}"
    );
    assert!(!actual.contains("LATE_OLD_REPLY_SENTINEL"), "{actual}");
    let current_api: Value = reqwest::Client::new()
        .get(format!("{base}/api/v0/devices/dot2-d/conversation"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current_api["view"], "current");
    assert_eq!(
        current_api["current_context"]["generation"],
        current.generation
    );
    assert_eq!(
        current_api["archived_contexts"][0]["session_id"],
        original.session_id
    );
    let archive: Value = reqwest::Client::new()
        .get(format!("{base}/api/v0/devices/dot2-d/conversation"))
        .query(&[("context_session_id", &original.session_id)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(archive["view"], "archive");
    assert!(archive["messages"]
        .to_string()
        .contains("LATE_OLD_REPLY_SENTINEL"));
    let rejected = reqwest::Client::new()
        .get(format!(
            "{base}/api/v0/devices/dot2-d/conversation?context_session_id=other-device-private"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::CONFLICT);
}

#[tokio::test]
async fn context_ttl_late_actual_llm_keeps_original_task_session_after_new_wake() {
    let llm = FakeLlm::new(vec![
        Scripted::Delayed {
            delay_ms: 700,
            chunks: vec!["LATE_CANCELLED_REPLY".into()],
        },
        Scripted::Text {
            chunks: vec!["新话题。".into()],
        },
    ]);
    let (base, state, gw) = spawn_app_with_gateway(llm.clone()).await;
    set_context_window(&base, 60_000).await;
    wake(&state, &gw, 1).await;
    request(&state, &gw, 2, "OLD_RUNNING_PROMPT").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while llm.seen_requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    age_context(&state, &old.session_id, 60_001);
    wake(&state, &gw, 3).await;
    request(&state, &gw, 4, "NEW_PROMPT_ONLY").await;
    finish_answer(&state, &gw).await;
    assert!(prompt(&llm, 0).contains("OLD_RUNNING_PROMPT"));
    let current = prompt(&llm, 1);
    assert!(!current.contains("OLD_RUNNING_PROMPT"), "{current}");
    assert!(!current.contains("LATE_CANCELLED_REPLY"), "{current}");
    assert!(
        serde_json::to_string(&state.sessions.messages(&old.session_id).unwrap())
            .unwrap()
            .contains("OLD_RUNNING_PROMPT")
    );
}

#[tokio::test]
async fn context_ttl_schema11_upgrade_restart_reset_and_actual_prompt() {
    for age in [1_000, 60_001] {
        let dir = std::env::temp_dir().join(hm_core::id::next("context-upgrade"));
        let path = dir.join("core.db");
        let old_session = {
            let store = Arc::new(hm_core::store::Store::open(&path).unwrap());
            let sessions = hm_core::session::SessionStore::new(store.clone());
            let session = sessions.device_session_id("dot2-d").unwrap();
            sessions
                .append(&session, "user", "SCHEMA11_OLD_HISTORY")
                .unwrap();
            store
                .with(|c| {
                    c.execute(
                        "UPDATE messages SET ts_ms=?1 WHERE session_id=?2",
                        rusqlite::params![
                            hm_core::id::now_ms().saturating_sub(age) as i64,
                            session
                        ],
                    )?;
                    c.execute_batch(
                        "DROP TABLE confirmations; DROP TABLE openapi_execution; ALTER TABLE sessions DROP COLUMN action_generation; DROP TABLE device_contexts; ALTER TABLE shared_memory_outbox DROP COLUMN source_device; ALTER TABLE shared_memory_outbox DROP COLUMN scope_device; ALTER TABLE shared_memory_outbox DROP COLUMN scope_person; ALTER TABLE shared_memory_outbox DROP COLUMN domain; ALTER TABLE shared_memory_outbox DROP COLUMN record_id; ALTER TABLE shared_memory_outbox DROP COLUMN event_kind; ALTER TABLE shared_memory_outbox DROP COLUMN dimension; ALTER TABLE shared_profile_cache DROP COLUMN source_device; ALTER TABLE shared_profile_cache DROP COLUMN version; ALTER TABLE shared_profile_cache DROP COLUMN row_revision; ALTER TABLE shared_profile_cache DROP COLUMN dimension; DELETE FROM schema_version WHERE version >= 12;",
                    )?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(store.schema_version().unwrap(), 11);
            session
        };
        let store = Arc::new(hm_core::store::Store::open(&path).unwrap());
        assert_eq!(store.schema_version().unwrap(), 15);
        let llm = FakeLlm::new(vec![Scripted::Text {
            chunks: vec!["迁移后的回答。".into()],
        }]);
        let state = AppState::with_overrides(
            store.clone(),
            dir.clone(),
            MockInject::default(),
            Some(Arc::new(llm.clone())),
            Some(Arc::new(MockTts::default())),
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        state.start_gateway_listener(([127, 0, 0, 1], port).into());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let gw = connect_gateway_audio_in(port).await;
        let app = hm_core::api::router(state.clone());
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", http.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(http, app).await.unwrap() });
        set_context_window(&base, 60_000).await;
        wake(&state, &gw, 1).await;
        let frozen = state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap();
        request(&state, &gw, 2, "升级后的独立问题").await;
        finish_answer(&state, &gw).await;
        assert_eq!(
            prompt(&llm, 0).contains("SCHEMA11_OLD_HISTORY"),
            age < 60_000
        );
        assert_eq!(frozen.session_id == old_session, age < 60_000);
        assert!(state
            .sessions
            .messages(&old_session)
            .unwrap()
            .iter()
            .any(|m| m["content"] == "SCHEMA11_OLD_HISTORY"));
        // A fresh store/session owner sees the same persisted current identity.
        let restarted = Arc::new(hm_core::store::Store::open(&path).unwrap());
        let sessions = hm_core::session::SessionStore::new(restarted.clone());
        assert_eq!(
            sessions
                .current_device_context("dot2-d")
                .unwrap()
                .unwrap()
                .ticket(),
            frozen.ticket()
        );
        let revision = store
            .with(|c| {
                Ok(
                    c.query_row("SELECT revision FROM config_head WHERE id=1", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .unwrap();
        state
            .sessions
            .complete_context(&frozen.session_id, frozen.ticket(), hm_core::id::now_ms())
            .unwrap();
        assert_eq!(
            revision,
            store
                .with(|c| Ok(c.query_row(
                    "SELECT revision FROM config_head WHERE id=1",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .unwrap(),
            "runtime renewal must not supersede config plans"
        );
        let reset = restarted.factory_reset(false).unwrap();
        assert!(reset["device_contexts"].as_u64().unwrap() > 0);
        assert!(sessions.current_device_context("dot2-d").unwrap().is_none());
        assert!(sessions
            .context_session_id("dot2-d", frozen.ticket())
            .is_err());
    }
}

#[tokio::test]
async fn context_ttl_first_speech_then_kws_is_current_but_dispatched_old_asr_is_not() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["新的首句。".into()],
        },
        Scripted::Text {
            chunks: vec!["新的上下文。".into()],
        },
        Scripted::Text {
            chunks: vec!["已准入的原上下文。".into()],
        },
    ]);
    let (tx, mut started) = mpsc::unbounded_channel();
    let (base, state, gw) =
        spawn_mic_app_stream_asr(llm.clone(), Arc::new(D0GatedAsr { started: tx })).await;
    set_context_window(&base, 60_000).await;
    wake(&state, &gw, 1).await;
    let old = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    age_context(&state, &old.session_id, 60_001);
    let input = gw.capture_input();
    for block in 0..3 {
        gw.send_raw_json(input.pcm(10 + block, block * 1280, &loud_block()))
            .await;
    }
    let aged = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap()
        .last_activity_ms;
    d0_wait_note(&state, "mic_speech_started", |_| true).await;
    assert_eq!(
        state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap()
            .last_activity_ms,
        aged,
        "speech observation alone must not renew"
    );
    // Real KWS authorizes the already-open current segment after TTL rollover.
    let mut kws = input.keyword(1280, 2560);
    kws["keyword"] = json!("你好高达");
    gw.send_captured(kws, hm_core::id::now_ms(), 13).await;
    d0_wait_note(&state, "keyword_wake", |_| true).await;
    for block in 3..18 {
        let samples = if block < 10 {
            loud_block()
        } else {
            quiet_block()
        };
        gw.send_raw_json(input.pcm(11 + block, block * 1280, &samples))
            .await;
    }
    let release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    let current = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    assert_ne!(current.ticket(), old.ticket());
    release
        .send(Ok("你好高达 FIRST_WAKE_CURRENT_SENTINEL".into()))
        .unwrap();
    finish_answer(&state, &gw).await;
    assert!(prompt(&llm, 0).contains("FIRST_WAKE_CURRENT_SENTINEL"));
    wake(&state, &gw, 40).await;
    let stale_input = gw.capture_input();
    leased_segment(&state, &gw, &stale_input, 50).await;
    let mut stale = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    age_context(&state, &current.session_id, 60_001);
    wake(&state, &gw, 100).await;
    tokio::time::timeout(Duration::from_secs(5), stale.closed())
        .await
        .unwrap();
    assert!(
        stale.is_closed(),
        "replacement wake must cancel the old recognition worker immediately"
    );
    assert!(stale
        .send(Ok("OLD_ASR_MUST_NOT_RENEW_OR_ENTER_NEW_PROMPT".into()))
        .is_err());
    assert_eq!(
        state.voice.mode_snapshot()["devices"][0]["input"]["state"],
        "cancelled"
    );
    request(&state, &gw, 101, "新的独立文字请求").await;
    finish_answer(&state, &gw).await;
    assert_eq!(llm.seen_requests().len(), 2);
    assert!(!prompt(&llm, 1).contains("OLD_ASR_MUST_NOT_RENEW_OR_ENTER_NEW_PROMPT"));
    assert!(!prompt(&llm, 1).contains("FIRST_WAKE_CURRENT_SENTINEL"));
    // An authorized ASR already dispatched before idle expiry keeps its
    // frozen context when no replacement wake has created a new one.
    wake(&state, &gw, 120).await;
    let retained = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    let retained_input = gw.capture_input();
    leased_segment(&state, &gw, &retained_input, 130).await;
    let retained_release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    age_context(&state, &retained.session_id, 60_001);
    retained_release
        .send(Ok("ADMITTED_BEFORE_CONTEXT_EXPIRY".into()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while llm.seen_requests().len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "retained context ASR must reach actual prompt: {:?}",
            state.voice.recent(100)
        )
    });
    finish_answer(&state, &gw).await;
    assert_eq!(
        state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap()
            .ticket(),
        retained.ticket()
    );
    assert!(prompt(&llm, 2).contains("ADMITTED_BEFORE_CONTEXT_EXPIRY"));
    assert!(prompt(&llm, 2).contains("新的独立文字请求"));
}

#[tokio::test]
async fn context_ttl_old_completion_cannot_renew_reset_reused_session_name() {
    let (base, state, gw) = spawn_app_with_gateway(FakeLlm::new(vec![])).await;
    set_context_window(&base, 60_000).await;
    wake(&state, &gw, 1).await;
    let old = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    state.store.factory_reset(false).unwrap();
    wake(&state, &gw, 2).await;
    let current = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    assert_eq!(
        old.session_id, current.session_id,
        "this is the real reset name-reuse edge"
    );
    assert_ne!(old.data_epoch, current.data_epoch);
    state
        .sessions
        .complete_context(
            &old.session_id,
            old.ticket(),
            hm_core::id::now_ms() + 600_000,
        )
        .unwrap();
    assert_eq!(
        state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap()
            .last_activity_ms,
        current.last_activity_ms,
        "an old completion must not renew a different epoch using the same session name"
    );
}

#[tokio::test]
async fn context_ttl_archive_pages_are_bounded_and_old_selection_stays_readable() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["留在归档。".into()],
    }]);
    let (base, state, gw) = spawn_app_with_gateway(llm).await;
    set_context_window(&base, 60_000).await;
    wake(&state, &gw, 1).await;
    request(&state, &gw, 2, "DEEPEST_REAL_CONTEXT_HISTORY").await;
    finish_answer(&state, &gw).await;
    let old = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    for seq in 3..=47 {
        let current = state
            .sessions
            .current_device_context("dot2-d")
            .unwrap()
            .unwrap();
        age_context(&state, &current.session_id, 60_001);
        wake(&state, &gw, seq).await;
    }
    let client = reqwest::Client::new();
    let endpoint = format!("{base}/api/v0/devices/dot2-d/conversation");
    let mut before = None;
    let mut ids = std::collections::HashSet::new();
    loop {
        let mut get = client.get(&endpoint);
        if let Some(cursor) = before {
            get = get.query(&[("archive_before", cursor)]);
        }
        let page: Value = get.send().await.unwrap().json().await.unwrap();
        let archives = page["archived_contexts"].as_array().unwrap();
        assert!(archives.len() <= 20);
        for archive in archives {
            assert!(ids.insert(archive["session_id"].as_str().unwrap().to_string()));
        }
        before = page["next_archive_before"].as_u64();
        if before.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 45);
    assert!(ids.contains(&old.session_id));
    for n in 0..60 {
        state
            .sessions
            .append(&old.session_id, "assistant", &format!("archived-row-{n}"))
            .unwrap();
    }
    let selected: Value = client
        .get(&endpoint)
        .query(&[("context_session_id", &old.session_id)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(selected["session_id"], old.session_id);
    assert_eq!(selected["selected_context"]["generation"], old.generation);
    assert_eq!(selected["messages"].as_array().unwrap().len(), 50);
    let earlier: Value = client
        .get(&endpoint)
        .query(&[
            ("context_session_id", old.session_id.clone()),
            (
                "message_before",
                selected["next_message_before"]
                    .as_i64()
                    .unwrap()
                    .to_string(),
            ),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(earlier["messages"]
        .to_string()
        .contains("DEEPEST_REAL_CONTEXT_HISTORY"));
    assert!(earlier["messages"].as_array().unwrap().len() <= 50);
    let denied = client
        .delete(&endpoint)
        .json(&json!({"expected_session_id":old.session_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        denied.status(),
        reqwest::StatusCode::CONFLICT,
        "server must reject archive clear regardless of UI"
    );
    assert!(state.sessions.messages(&old.session_id).unwrap().len() > 50);
}

#[tokio::test]
async fn context_ttl_in_window_explicit_asr_wake_rotates_expired_context() {
    let llm = FakeLlm::new(vec![
        Scripted::Text {
            chunks: vec!["原来的回答。".into()],
        },
        Scripted::Text {
            chunks: vec!["新上下文的回答。".into()],
        },
    ]);
    let (tx, mut started) = mpsc::unbounded_channel();
    let (base, state, gw) =
        spawn_mic_app_stream_asr(llm.clone(), Arc::new(D0GatedAsr { started: tx })).await;
    set_context_window(&base, 60_000).await;
    reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"followup_credits":3}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wake(&state, &gw, 1).await;
    request(&state, &gw, 2, "OLD_EXPLICIT_WAKE_CONTEXT_SENTINEL").await;
    finish_answer(&state, &gw).await;
    let old = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    age_context(&state, &old.session_id, 60_001);
    let input = gw.capture_input();
    leased_segment(&state, &gw, &input, 10).await;
    let release = tokio::time::timeout(Duration::from_secs(5), started.recv())
        .await
        .unwrap()
        .unwrap();
    release
        .send(Ok("你好高达，PREFIX_WAKE_NEW_SENTINEL".into()))
        .unwrap();
    finish_answer(&state, &gw).await;
    let current = state
        .sessions
        .current_device_context("dot2-d")
        .unwrap()
        .unwrap();
    assert_ne!(
        current.ticket(),
        old.ticket(),
        "an explicit wake inside a still-legal follow window selects a fresh expired context"
    );
    assert!(prompt(&llm, 1).contains("PREFIX_WAKE_NEW_SENTINEL"));
    assert!(!prompt(&llm, 1).contains("OLD_EXPLICIT_WAKE_CONTEXT_SENTINEL"));
}

// I-1 uses the production HTTP ASR and LLM clients. Only their remote servers
// are synthetic, and each request is captured before the test releases it.
#[derive(Default)]
struct InputHttpProbe {
    audio: std::sync::Mutex<Vec<Vec<u8>>>,
    prompts: std::sync::Mutex<Vec<Value>>,
}
struct InputHttpFixture {
    base: String,
    state: Arc<AppState>,
    gw: Arc<FakeGateway>,
    probe: Arc<InputHttpProbe>,
    recognition: mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<String>>,
}
async fn input_http_fixture() -> InputHttpFixture {
    let probe = Arc::new(InputHttpProbe::default());
    let (tx, recognition) = mpsc::unbounded_channel();
    let asr_probe = probe.clone();
    let llm_probe = probe.clone();
    let remote=axum::Router::new()
        .route("/v1/audio/transcriptions",axum::routing::post(move |body:bytes::Bytes| {
            let probe=asr_probe.clone(); let tx=tx.clone();
            async move {
                probe.audio.lock().unwrap().push(body.to_vec());
                let (release,result)=tokio::sync::oneshot::channel();
                tx.send(release).unwrap();
                axum::Json(json!({"text":result.await.unwrap_or_default()}))
            }
        }))
        .route("/v1/chat/completions",axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
            let probe=llm_probe.clone();
            async move {
                probe.prompts.lock().unwrap().push(body);
                ([ ("content-type","text/event-stream") ],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"收到。\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, remote).await.unwrap() });
    let state = AppState::with_overrides(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        std::env::temp_dir().join(hm_core::id::next("input-http")),
        MockInject::default(),
        Some(Arc::new(
            hm_core::llm::deepseek::DeepSeekClient::new(&remote_url, "").unwrap(),
        )),
        Some(Arc::new(MockTts::default())),
        Some(Arc::new(hm_core::speech::OpenaiAsr::new(
            remote_url,
            String::new(),
            "test-asr".into(),
        ))),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_port = listener.local_addr().unwrap().port();
    drop(listener);
    state.start_gateway_listener(([127, 0, 0, 1], gateway_port).into());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let gw = connect_gateway_audio_in_caps(gateway_port, vec!["play_stream"]).await;
    let router = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    reqwest::Client::new().put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"follow_window_enabled":true,"follow_window_ms":5000,"followup_credits":0,
            "tail_blind_ms":0,"proactive_asr_wake":false,"wake_ack":{"enabled":false},"filler":no_filler()}))
        .send().await.unwrap().error_for_status().unwrap();
    InputHttpFixture {
        base,
        state,
        gw,
        probe,
        recognition,
    }
}
async fn input_mode(f: &InputHttpFixture) -> Value {
    let snapshot: Value = reqwest::get(format!("{}/api/v0/mode", f.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    snapshot["devices"][0].clone()
}
async fn input_asr(f: &mut InputHttpFixture) -> tokio::sync::oneshot::Sender<String> {
    tokio::time::timeout(Duration::from_secs(5), f.recognition.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn input_prompt_count(f: &InputHttpFixture, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.probe.prompts.lock().unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.probe.prompts.lock().unwrap().len(), count);
}
async fn input_frames(
    f: &InputHttpFixture,
    input: &CapturedInput,
    seq: u64,
    range: std::ops::Range<u64>,
) {
    let lease = f.state.voice.capture_lease("dot2-d");
    for block in range {
        let samples = if block < 10 {
            loud_block()
        } else {
            quiet_block()
        };
        let mut frame = input.pcm(seq + block, block * 1280, &samples);
        frame["origin"] = json!({"kind":"leased","lease":lease,"origin":frame["origin"]});
        f.gw.send_raw_json(frame).await;
    }
}

#[tokio::test]
async fn input_lease_http_started_inside_closes_and_recognizes_after_window_once() {
    let mut f = input_http_fixture().await;
    wake(&f.state, &f.gw, 1).await;
    let input = f.gw.capture_input();
    input_frames(&f, &input, 10, 0..3).await;
    d0_wait_note(&f.state, "mic_speech_started", |_| true).await;
    let mode = input_mode(&f).await;
    assert_eq!(mode["input"]["state"], "capture");
    assert_eq!(mode["input"]["capture_limit_ms"], 12000);
    assert_eq!(mode["input"]["lifetime_limit_ms"], 60000);
    let input_id = mode["input"]["input_id"].as_u64().unwrap();
    let stop_epoch = mode["input"]["stop_epoch"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(5100)).await;
    let mode = input_mode(&f).await;
    assert_eq!(mode["input_window_open"], false);
    assert_eq!(mode["input"]["state"], "capture");
    input_frames(&f, &input, 10, 3..18).await;
    let release = input_asr(&mut f).await;
    assert_eq!(input_mode(&f).await["input"]["state"], "recognizing");
    // A later ambient segment cannot acquire a second capture/ASR slot.
    let mut outside = f.gw.capture_input();
    outside.epoch = input.epoch.clone();
    input_frames(&f, &outside, 100, 0..18).await;
    d0_wait_note(&f.state, "mic_segment_closed", |v| {
        v["ticket"].to_string().contains(&outside.input_id)
    })
    .await;
    assert_eq!(f.probe.audio.lock().unwrap().len(), 1);
    release
        .send("LEASE_LATE_REAL_HTTP_SENTINEL".into())
        .unwrap();
    input_prompt_count(&f, 1).await;
    let prompt = f.probe.prompts.lock().unwrap()[0].to_string();
    assert!(prompt.contains("LEASE_LATE_REAL_HTTP_SENTINEL"), "{prompt}");
    finish_answer(&f.state, &f.gw).await;
    f.state
        .voice
        .test_deliver_transcript(
            "dot2-d",
            "DUPLICATE_MUST_NOT_ENTER",
            input_id,
            18,
            stop_epoch,
        )
        .await;
    assert_eq!(f.probe.prompts.lock().unwrap().len(), 1);
    let audio = f.probe.audio.lock().unwrap();
    assert!(
        audio[0].windows(4).any(|w| w == b"RIFF"),
        "actual multipart upload contains a WAV"
    );
}

#[tokio::test]
async fn input_lease_http_outside_window_and_idle_privacy_make_zero_asr_requests() {
    let f = input_http_fixture().await;
    wake(&f.state, &f.gw, 1).await;
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert_eq!(input_mode(&f).await["input_window_open"], false);
    let input = f.gw.capture_input();
    input_frames(&f, &input, 10, 0..18).await;
    d0_wait_note(&f.state, "mic_asr_skipped", |_| true).await;
    assert!(f.probe.audio.lock().unwrap().is_empty());
    assert!(f.probe.prompts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn input_lease_http_stop_close_mute_capture_change_and_rewake_revoke_pending_asr() {
    for reason in ["stop", "close", "mute", "capture", "rewake", "reconnect"] {
        let mut f = input_http_fixture().await;
        wake(&f.state, &f.gw, 1).await;
        let input = f.gw.capture_input();
        input_frames(&f, &input, 10, 0..18).await;
        let old = input_asr(&mut f).await;
        match reason {
            "stop" => {
                reqwest::Client::new()
                    .post(format!("{}/api/v0/devices/dot2-d/stop", f.base))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
            "close" => {
                reqwest::Client::new()
                    .post(format!("{}/api/v0/session/close", f.base))
                    .json(&json!({"device":"dot2-d"}))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            }
            "mute" => {
                f.gw.send_raw_json(json!({"type":"mute_state","device":"dot2-d","muted":true,"source":"button","seq":50,"ts":hm_core::id::now_ms()})).await;
                d0_wait_note(&f.state, "mute_state", |v| v["muted"] == true).await;
            }
            "capture" => {
                let mut changed = f.gw.capture_input();
                changed.epoch = "new-capture-epoch".into();
                let mut frame = changed.pcm(50, 0, &quiet_block());
                frame["origin"] = json!({"kind":"leased","lease":f.state.voice.capture_lease("dot2-d"),"origin":frame["origin"]});
                f.gw.send_raw_json(frame).await;
            }
            "rewake" => {
                wake(&f.state, &f.gw, 50).await;
            }
            "reconnect" => {
                f.gw.close().await;
            }
            _ => unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if input_mode(&f).await["input"]["state"] == "cancelled" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{reason} must revoke the exact slot"));
        assert_eq!(
            input_mode(&f).await["mic"]["counters"]["asr_inflight"],
            0,
            "{reason}"
        );
        let _ = old.send(format!("REVOKED_{reason}_MUST_NOT_ENTER"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(f.probe.prompts.lock().unwrap().is_empty(), "{reason}");
        assert_eq!(f.probe.audio.lock().unwrap().len(), 1, "{reason}");
    }
}

#[tokio::test]
async fn input_lease_production_deadline_releases_http_asr_slot_with_virtual_time() {
    let mut f = input_http_fixture().await;
    wake(&f.state, &f.gw, 1).await;
    let input = f.gw.capture_input();
    input_frames(&f, &input, 10, 0..18).await;
    let old = input_asr(&mut f).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(59)).await;
    assert_eq!(
        f.state.voice.mode_snapshot()["devices"][0]["input"]["state"],
        "recognizing"
    );
    tokio::time::advance(Duration::from_millis(1100)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), async {
        while input_mode(&f).await["input"]["state"] != "expired" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(input_mode(&f).await["mic"]["counters"]["asr_inflight"], 0);
    let _ = old.send("EXPIRED_MUST_NOT_ENTER_PROMPT".into());
    wake(&f.state, &f.gw, 100).await;
    let input = f.gw.capture_input();
    input_frames(&f, &input, 110, 0..18).await;
    input_asr(&mut f)
        .await
        .send("RELEASED_SLOT_ACCEPTS_NEW_INPUT".into())
        .unwrap();
    input_prompt_count(&f, 1).await;
    assert_eq!(f.probe.audio.lock().unwrap().len(), 2);
    let prompt = f.probe.prompts.lock().unwrap()[0].to_string();
    assert!(prompt.contains("RELEASED_SLOT_ACCEPTS_NEW_INPUT"));
    assert!(!prompt.contains("EXPIRED_MUST_NOT_ENTER_PROMPT"));
    finish_answer(&f.state, &f.gw).await;
}

#[tokio::test]
async fn input_lease_production_capture_cap_releases_silent_open_segment() {
    let f = input_http_fixture().await;
    wake(&f.state, &f.gw, 1).await;
    let input = f.gw.capture_input();
    input_frames(&f, &input, 10, 0..3).await;
    d0_wait_note(&f.state, "mic_speech_started", |_| true).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(12001)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), async {
        while input_mode(&f).await["input"]["state"] != "expired" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(f.probe.audio.lock().unwrap().is_empty());
    assert!(f.probe.prompts.lock().unwrap().is_empty());
}
