use super::*;

#[tokio::test]
async fn wake_ack_tail_must_not_discard_the_first_question() {
    check_ack_tail(true).await;
}

#[tokio::test]
async fn wake_ack_tail_followed_only_by_silence_does_not_become_a_question() {
    check_ack_tail(false).await;
}

async fn check_ack_tail(clean_speech: bool) {
    let (asr, reply) = SwitchableAsr::new("你好，高达");
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["没什么新鲜事。".into()],
    }]);
    let (base, state, gw) = spawn_mic_app_stream_asr(llm.clone(), Arc::new(asr)).await;
    let _ = state.voice.set_mode(
        "dot2-d",
        None,
        None,
        None,
        None,
        None,
        Some(hm_core::voice::WakeAckSettings {
            enabled: true,
            phrases: vec!["在呢".into()],
            resynth: None,
        }),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(no_filler()),
        None,
        None,
        "test",
    );
    assert!(reqwest::Client::new()
        .put(format!("{base}/api/v0/mode/dot2-d"))
        .json(&json!({"tail_blind_ms":400}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    state.voice.synthesize_wake_acks(false).await;
    let loud = loud_block();
    let quiet = quiet_block();
    // 1) Bare wake -> the pre-synthesized ack plays from the device.
    for seq in 1..=10 {
        gw.send(&neural_chunk(seq, &loud, true)).await;
    }
    for seq in 11..=20 {
        gw.send(&neural_chunk(seq, &quiet, false)).await;
    }
    let ack_id = match gw
        .until(|f| {
            matches!(
                f,
                CoreFrame::Play { content_ref, .. } | CoreFrame::PlayBegin { content_ref, .. }
                    if content_ref.starts_with("ack:")
            )
        })
        .await
    {
        CoreFrame::Play { task_id, .. } | CoreFrame::PlayBegin { task_id, .. } => task_id,
        f => panic!("expected ack play, got {f:?}"),
    };
    // The question begins during the ACK tail and continues beyond it.
    gw.send(&GatewayFrame::PlayFinished {
        device: "dot2-d".into(),
        task_id: ack_id.clone(),
        played_ms: 600,
        ts: hm_core::id::now_ms(),

        consumed_ms: None,
        progress_basis: None,
    })
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .voice
                .recent(100)
                .iter()
                .any(|e| e["kind"] == "play_finished" && e["play_id"] == ack_id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for seq in 21..=23 {
        gw.send(&neural_chunk(seq, &loud, true)).await;
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .voice
                .recent(100)
                .iter()
                .filter(|e| e["kind"] == "mic_speech_started")
                .count()
                >= 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Cross the configured tail boundary while retaining the captured onset.
    tokio::time::sleep(Duration::from_millis(450)).await;
    // 2) The next segment transcribes OUR ack plus the real question.
    *reply.lock().unwrap() = "在呢。外面下雨吗".to_string();
    for seq in 24..=30 {
        gw.send(&neural_chunk(
            seq,
            if clean_speech { &loud } else { &quiet },
            clean_speech,
        ))
        .await;
    }
    for seq in 31..=40 {
        gw.send(&neural_chunk(seq, &quiet, false)).await;
    }
    if !clean_speech {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if state
                    .voice
                    .recent(100)
                    .iter()
                    .any(|e| e["kind"] == "mic_ignored_unwoken")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            llm.seen_requests().is_empty(),
            "ACK speech plus quiet must not reach the model"
        );
        return;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !llm.seen_requests().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the question must reach the actual model entry point");

    let log = state.voice.recent(300);
    assert!(
        log.iter()
            .any(|e| e["kind"] == "cue_echo_stripped" && e["phrase"] == "在呢"),
        "the strip must be recorded with its phrase: {log:?}"
    );
    let segment = log
        .iter()
        .find(|e| e["kind"] == "mic_segment_closed")
        .unwrap();
    let spans = segment["ticket"]["spans"].as_array().unwrap();
    assert!(
        spans
            .iter()
            .any(|s| s["control_only"] == true && s["wake_ack_overlap"] == true),
        "{segment:?}"
    );
    assert!(spans
        .iter()
        .any(|s| s["control_only"] != true && s["speech"] == true));
    let mode: Value = reqwest::get(format!("{base}/api/v0/mode"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let endpoint = &mode["devices"][0]["mic"]["vad"]["endpoint"];
    assert_eq!(endpoint["kind"], "gateway_vad");
    assert_eq!(
        endpoint["model_detail"],
        "gateway neural VAD decisions received"
    );
    let reqs = llm.seen_requests();
    let last = reqs.last().expect("a turn must reach the model");
    let user = last
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert_eq!(
        user, "外面下雨吗",
        "the model sees the question without our own cue"
    );
}

fn neural_chunk(seq: u64, samples: &[i16], speech: bool) -> GatewayFrame {
    let mut frame = audio_chunk(seq, samples);
    if let GatewayFrame::AudioChunk { vad_speech, .. } = &mut frame {
        *vad_speech = Some(speech);
    }
    frame
}
