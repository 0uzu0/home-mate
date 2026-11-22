//! Real router, pairing and HTTP API. No Gateway or injected AgentEnv required.
use hm_core::{
    api::{router, AppState},
    store::Store,
    tool::mocks::MockInject,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};

async fn publication(client: &reqwest::Client, base: &str, token: &str, id: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let proof = client
                .get(format!("{base}/api/v1/config/applications/{id}/readback"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            if proof["publications"].as_array().is_some_and(|items| {
                !items.is_empty() && items.iter().all(|item| item["state"] == "applied")
            }) {
                return proof;
            }
            assert!(
                !proof["publications"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["state"] == "failed")),
                "{proof}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("supervised persona publication")
}

#[tokio::test]
async fn paired_router_reviews_commits_reads_and_refuses_stale_authority() {
    let dir = std::env::temp_dir().join(format!(
        "hm-config-api-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    let store = Arc::new(Store::open_in_memory().unwrap());
    let unit = r#"{"test-a":{"volume_cap":0.4,"mix_gain":0.8,"mic_gain_offset_db":2.5}}"#;
    store.config_set("unit_acoustic_profiles", unit).unwrap();
    let state = AppState::with_overrides(
        store.clone(),
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // Synthetic non-loopback ConnectInfo exercises the real bearer middleware
    // on this listener. It is not a claim of LAN/socket-origin verification.
    let remote = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remote_layer = remote.clone();
    let app = router(state.clone()).layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let remote = remote_layer.clone();
            async move {
                if remote.load(std::sync::atomic::Ordering::SeqCst) {
                    req.extensions_mut().insert(axum::extract::ConnectInfo(
                        "192.0.2.10:5000".parse::<SocketAddr>().unwrap(),
                    ));
                }
                next.run(req).await
            }
        },
    ));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{base}/api/v1/config/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        401,
        "v2 must not inherit loopback/unpaired mutation bypass"
    );
    assert_eq!(
        client
            .put(format!("{base}/api/v1/config/enrollment"))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    remote.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        client
            .get(format!("{base}/api/v1/config/state"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let token = client
        .post(format!("{base}/api/v0/auth/provision"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .put(format!("{base}/api/v0/fleet"))
        .bearer_auth(&token)
        .json(&json!({"self_id":"test-a"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let response = client
        .put(format!("{base}/api/v1/config/enrollment"))
        .bearer_auth(&token)
        .json(&json!({"household_id":"caller-cannot-select"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let enrollment = client
        .put(format!("{base}/api/v1/config/enrollment"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(hm_core::config_delivery::valid_uuid(
        enrollment["household_id"].as_str().unwrap()
    ));
    let snapshot = client
        .get(format!("{base}/api/v1/config/state"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(snapshot["target"]["enrolled"], true);
    let pkg = json!({"format":"home-mate-config","version":2,"package_id":hm_core::config_delivery::uuid().unwrap(),"package_revision":1,"created_ms":1,"purpose":"node_config",
        "source":{"node_id":"test-a","config_revision":snapshot["config_revision"],"core_version":snapshot["core_version"],"schema_version":snapshot["schema_version"]},
        "target":{"household_id":enrollment["household_id"],"node_id":"test-a"},
        "compatibility":{"core_api_schema":2,"gateway_protocol_min":0,"gateway_protocol_max":0,"required_capabilities":[]},
        "change_reason":"Reviewable API fixture","blocks":[{"name":"persona","schema_version":1,"operations":[{"action":"set","path":"persona.agent_md.content","value":"API persona fixture"}]}],"bindings":[]});
    let p = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({"package":pkg}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        hm_core::agent_md::AgentMdService::new(store.clone()).current_content(),
        ""
    );
    assert_eq!(p["diff"][0]["after"], "API persona fixture");
    let request = json!({"plan_id":p["plan_id"],"plan_hash":p["plan_hash"],"expected_revision":p["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()});
    let mut tokens: Value =
        serde_json::from_str(&store.config_get("api_tokens").unwrap().unwrap()).unwrap();
    for entry in tokens.as_array_mut().unwrap() {
        entry["last_used_ms"] = json!(0);
    }
    store.config_set("api_tokens", &tokens.to_string()).unwrap();
    let response = client
        .post(format!("{base}/api/v1/config/applications"))
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let applied = response.json::<Value>().await.unwrap();
    assert_eq!(applied["state"], "saved_pending");
    assert_eq!(
        store
            .config_get("unit_acoustic_profiles")
            .unwrap()
            .as_deref(),
        Some(unit)
    );
    let detail = client
        .get(format!(
            "{base}/api/v1/config/applications/{}",
            request["application_id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail, applied);
    let history = client
        .get(format!("{base}/api/v1/config/applications"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(history["applications"].as_array().unwrap().len(), 1);
    let replay = client
        .post(format!("{base}/api/v1/config/applications"))
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(replay, applied);
    let application_url = format!(
        "{base}/api/v1/config/applications/{}",
        request["application_id"].as_str().unwrap()
    );
    assert_eq!(
        client
            .get(format!("{application_url}/readback"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let proof = client
        .get(format!("{application_url}/readback"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(proof["fields"][0]["state"], "saved_pending");
    assert_eq!(
        hm_core::agent_md::AgentMdService::new(store.clone()).current_content(),
        ""
    );
    let _worker = hm_core::config_delivery::ConfigWorker::start(store.clone());
    let proof = publication(
        &client,
        &base,
        &token,
        request["application_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(proof["initial_receipt"], applied);
    assert_eq!(proof["fields"][0]["state"], "core_readback_verified");
    let rollback = client
        .post(format!("{application_url}/rollback-preflight"))
        .bearer_auth(&token)
        .json(&json!({"expected_revision":applied["config_revision"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(rollback["diff"][0]["after"], "");
    assert_eq!(
        hm_core::agent_md::AgentMdService::new(store.clone()).current_content(),
        "API persona fixture"
    );
    assert_eq!(client.post(format!("{application_url}/rollback-preflight")).bearer_auth(&token).json(&json!({"expected_revision":applied["config_revision"],"replacement":"not accepted"})).send().await.unwrap().status(),400);
    let restored = client.post(format!("{base}/api/v1/config/applications")).bearer_auth(&token).json(&json!({"plan_id":rollback["plan_id"],"plan_hash":rollback["plan_hash"],"expected_revision":rollback["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    assert_eq!(restored["state"], "saved_pending");
    publication(
        &client,
        &base,
        &token,
        restored["application_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(
        hm_core::agent_md::AgentMdService::new(store.clone()).current_content(),
        ""
    );
    assert_eq!(
        client
            .get(format!("{application_url}/readback"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["state"],
        "superseded"
    );
    let mut mismatch = request.clone();
    mismatch["plan_hash"] = json!("different");
    assert_eq!(
        client
            .post(format!("{base}/api/v1/config/applications"))
            .bearer_auth(&token)
            .json(&mismatch)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let malformed = json!({"format":"home-mate-config","version":1,"services":{"llm":{"model":"must-not-apply"}},"mode":{"media_trim_db":-1}});
    assert_eq!(
        client
            .post(format!("{base}/api/v1/config/preflight"))
            .bearer_auth(&token)
            .json(&json!({"package":malformed}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let rotated = client
        .post(format!("{base}/api/v0/auth/rotate"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let new_token = rotated["token"].as_str().unwrap();
    assert_eq!(
        client
            .get(format!("{base}/api/v1/config/state"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = client
        .post(format!("{base}/api/v1/config/applications"))
        .bearer_auth(new_token)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "PLAN_AUTHORITY_MISMATCH"
    );
    server.abort();
    let _ = server.await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn next_chat_fallback_prompt_and_speech_requests_consume_reviewed_values() {
    use axum::{
        body::Bytes,
        extract::{OriginalUri, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        Json, Router,
    };
    type Hits = Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>;
    async fn partner(
        State((hits, arrived, mut gate)): State<(
            Hits,
            tokio::sync::mpsc::UnboundedSender<String>,
            tokio::sync::watch::Receiver<bool>,
        )>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let path = uri.path().to_string();
        hits.lock().unwrap().push((
            path.clone(),
            headers
                .get("authorization")
                .and_then(|s| s.to_str().ok())
                .unwrap_or("")
                .into(),
            body.to_vec(),
        ));
        if path.contains("-old/") {
            arrived.send(path.clone()).unwrap();
            while !*gate.borrow() {
                gate.changed().await.unwrap();
            }
        }
        if path.ends_with("audio/transcriptions") {
            return Json(json!({"text":"verified speech input"})).into_response();
        }
        if path.ends_with("audio/speech") {
            return (
                [("content-type", "audio/wav")],
                hm_core::speech::pcm_to_wav(&[0; 320], 16000, 1),
            )
                .into_response();
        }
        if path.contains("primary-") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":"synthetic primary refusal"}})),
            )
                .into_response();
        }
        let body: Value = serde_json::from_slice(&body).unwrap();
        if body["stream"] == true {
            return ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Native fallback verified.\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").into_response();
        }
        Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})).into_response()
    }
    let hits: Hits = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let partner_base = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let (release, gate) = tokio::sync::watch::channel(false);
    let partner_app = Router::new()
        .fallback(partner)
        .with_state((hits.clone(), arrived, gate));
    let partner_server =
        tokio::spawn(async move { axum::serve(listener, partner_app).await.unwrap() });
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let unit = r#"{"test-a":{"volume_cap":0.41,"mix_gain":0.72,"mic_gain_offset_db":1.25}}"#;
    store.config_set("unit_acoustic_profiles", unit).unwrap();
    for slot in ["llm", "llm_fallback", "asr", "tts"] {
        store
            .secret_set(&format!("{slot}.api_key"), &format!("synthetic-{slot}-old"))
            .unwrap();
    }
    let mut old_config = json!({});
    for (slot, provider, endpoint) in [
        ("llm", "deepseek", "primary"),
        ("llm_fallback", "deepseek", "fallback"),
        ("asr", "openai-asr", "asr"),
        ("tts", "omni", "tts"),
    ] {
        old_config[slot] = json!({"provider":provider,"base_url":format!("{partner_base}/{endpoint}-old"),"model":format!("{endpoint}-model-old"),"api_key_env":"","thinking":"disabled","voice":if slot=="tts" {"voice-old"} else {""}});
    }
    store
        .config_set("services", &old_config.to_string())
        .unwrap();
    hm_core::agent_md::AgentMdService::new(store.clone())
        .update("CONFIG_NATIVE_PERSONA_OLD", "console", "")
        .unwrap();
    let dir = std::env::temp_dir().join(format!(
        "hm-config-consumer-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    let state = AppState::with_overrides(
        store.clone(),
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    state.voice.set_memory_extraction(false);
    let token = state.auth.provision("configuration-consumer-test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::new();
    let delivery = hm_core::config_delivery::ConfigDelivery::new(store.clone());
    delivery.enroll("test-a").unwrap();
    let old_response = client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"text":"old operation","session_id":"old-service-snapshot"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let old_chat = tokio::spawn(async move { old_response.text().await.unwrap() });
    let mut old_probes = vec![];
    for slot in ["llm_fallback", "asr", "tts"] {
        let client = client.clone();
        let base = base.clone();
        let token = token.clone();
        old_probes.push(tokio::spawn(async move {
            client
                .post(format!("{base}/api/v0/config/services/{slot}/test"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        for _ in 0..4 {
            arrivals.recv().await.unwrap();
        }
    })
    .await
    .expect("all four old service requests reach the real HTTP barrier");
    for slot in ["llm", "llm_fallback", "asr", "tts"] {
        store
            .secret_set(&format!("{slot}.api_key"), &format!("synthetic-{slot}"))
            .unwrap();
    }
    let snapshot = delivery.state("unused").unwrap();
    let mut operations = vec![];
    for (slot, provider, model, endpoint) in [
        ("llm", "deepseek", "primary-model-new", "primary-new"),
        (
            "llm_fallback",
            "deepseek",
            "fallback-model-new",
            "fallback-new",
        ),
        ("asr", "openai-asr", "asr-model-new", "asr-new"),
        ("tts", "omni", "tts-model-new", "tts-new"),
    ] {
        for (field, value) in [
            ("provider", provider.to_string()),
            ("model", model.into()),
            ("base_url", format!("{partner_base}/{endpoint}")),
        ] {
            operations.push(
                json!({"action":"set","path":format!("services.{slot}.{field}"),"value":value}),
            );
        }
    }
    operations.push(json!({"action":"set","path":"services.tts.voice","value":"voice-new"}));
    let pkg = json!({"format":"home-mate-config","version":2,"package_id":hm_core::config_delivery::uuid().unwrap(),"package_revision":1,"created_ms":1,"purpose":"node_config",
      "source":{"node_id":"test-a","config_revision":snapshot["config_revision"],"core_version":snapshot["core_version"],"schema_version":snapshot["schema_version"]},"target":{"household_id":snapshot["target"]["household_id"],"node_id":"test-a"},
      "compatibility":{"core_api_schema":2,"gateway_protocol_min":0,"gateway_protocol_max":0,"required_capabilities":[]},"change_reason":"Native next operation verification","bindings":[],
      "blocks":[{"name":"services","schema_version":1,"operations":operations},{"name":"persona","schema_version":1,"operations":[{"action":"set","path":"persona.agent_md.content","value":"CONFIG_NATIVE_PERSONA_NEW"}]}]});
    let plan = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({"package":pkg}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(
        hits.lock().unwrap().len() == 4,
        "preflight and constructors make zero remote requests"
    );
    let request = json!({"plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],"expected_revision":plan["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()});
    let receipt = client
        .post(format!("{base}/api/v1/config/applications"))
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(receipt["state"], "saved_pending");
    assert_eq!(
        hm_core::config::ConfigService::new(store.clone())
            .load()
            .unwrap()
            .llm
            .model,
        "primary-model-old",
        "desired commit must not publish services"
    );
    let _worker = hm_core::config_delivery::ConfigWorker::start(store.clone());
    publication(
        &client,
        &base,
        &token,
        request["application_id"].as_str().unwrap(),
    )
    .await;
    let proof = client
        .get(format!(
            "{base}/api/v1/config/applications/{}/readback",
            request["application_id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(proof["fields"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["state"] == "core_readback_verified"));
    assert!(
        hits.lock().unwrap().len() == 4,
        "readback makes no paid health probes"
    );
    let reply = client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"text":"Say a brief greeting","session_id":"config-next-native"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(reply.contains("Native fallback verified"), "{reply}");
    for slot in ["asr", "tts"] {
        let result = client
            .post(format!("{base}/api/v0/config/services/{slot}/test"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "{slot}: {result}");
    }
    release.send(true).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), old_chat)
            .await
            .unwrap()
            .unwrap()
            .contains("Native fallback verified")
    );
    for probe in old_probes {
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), probe)
                .await
                .unwrap()
                .unwrap()["ok"],
            true
        );
    }
    let requests = hits.lock().unwrap().clone();
    for (endpoint, slot) in [
        ("primary", "llm"),
        ("fallback", "llm_fallback"),
        ("asr", "asr"),
        ("tts", "tts"),
    ] {
        let old = requests
            .iter()
            .filter(|r| r.0.starts_with(&format!("/{endpoint}-old/")))
            .collect::<Vec<_>>();
        assert!(!old.is_empty());
        for request in old {
            assert_eq!(request.1, format!("Bearer synthetic-{slot}-old"));
            if slot == "asr" {
                assert!(String::from_utf8_lossy(&request.2).contains("asr-model-old"));
            } else {
                let body: Value = serde_json::from_slice(&request.2).unwrap();
                assert_eq!(body["model"], format!("{endpoint}-model-old"));
                if slot == "tts" {
                    assert_eq!(body["voice"], "voice-old");
                }
            }
        }
    }
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.0 == "/fallback-old/chat/completions")
            .count(),
        2,
        "old Agent fallback and in-flight fallback probe both retain old config"
    );
    let primary = requests
        .iter()
        .find(|r| r.0 == "/primary-new/chat/completions")
        .expect("actual next Agent primary request");
    let primary_body: Value = serde_json::from_slice(&primary.2).unwrap();
    assert_eq!(primary_body["model"], "primary-model-new");
    assert!(primary_body["messages"]
        .to_string()
        .contains("CONFIG_NATIVE_PERSONA_NEW"));
    assert_eq!(primary.1, "Bearer synthetic-llm");
    let fallback = requests
        .iter()
        .find(|r| r.0 == "/fallback-new/chat/completions")
        .expect("actual fallback request");
    assert_eq!(
        serde_json::from_slice::<Value>(&fallback.2).unwrap()["model"],
        "fallback-model-new"
    );
    assert_eq!(fallback.1, "Bearer synthetic-llm_fallback");
    let asr = requests
        .iter()
        .find(|r| r.0 == "/asr-new/audio/transcriptions")
        .unwrap();
    assert!(String::from_utf8_lossy(&asr.2).contains("asr-model-new"));
    assert_eq!(asr.1, "Bearer synthetic-asr");
    let tts = requests
        .iter()
        .find(|r| r.0 == "/tts-new/audio/speech")
        .unwrap();
    let body: Value = serde_json::from_slice(&tts.2).unwrap();
    assert_eq!(body["model"], "tts-model-new");
    assert_eq!(body["voice"], "voice-new");
    assert_eq!(tts.1, "Bearer synthetic-tts");
    assert_eq!(
        store
            .config_get("unit_acoustic_profiles")
            .unwrap()
            .as_deref(),
        Some(unit)
    );
    for slot in ["llm", "llm_fallback", "asr", "tts"] {
        assert_eq!(
            store.secret_get(&format!("{slot}.api_key")).unwrap(),
            Some(format!("synthetic-{slot}"))
        );
    }
    let rollback = client
        .post(format!(
            "{base}/api/v1/config/applications/{}/rollback-preflight",
            request["application_id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .json(&json!({"expected_revision":receipt["config_revision"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let restored=client.post(format!("{base}/api/v1/config/applications")).bearer_auth(&token).json(&json!({"plan_id":rollback["plan_id"],"plan_hash":rollback["plan_hash"],"expected_revision":rollback["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    publication(
        &client,
        &base,
        &token,
        restored["application_id"].as_str().unwrap(),
    )
    .await;
    client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"text":"after rollback","session_id":"rollback-service-snapshot"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let rolled_back = hits.lock().unwrap().last().unwrap().clone();
    assert_eq!(rolled_back.0, "/fallback-old/chat/completions");
    assert_eq!(
        serde_json::from_slice::<Value>(&rolled_back.2).unwrap()["model"],
        "fallback-model-old"
    );
    assert_eq!(
        rolled_back.1, "Bearer synthetic-llm_fallback",
        "rollback must not restore credentials"
    );
    if let Ok(directory) = std::env::var("HM_TEST_CONFIG_EVIDENCE_DIR") {
        let path = std::path::Path::new(&directory);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("synthetic-service-http-requests.json"),
            serde_json::to_vec_pretty(&*hits.lock().unwrap()).unwrap(),
        )
        .unwrap();
    }
    let serialized = proof.to_string();
    assert!(!serialized.contains("synthetic-"));
    server.abort();
    let _ = server.await;
    partner_server.abort();
    let _ = partner_server.await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn supervised_persona_publication_preserves_inflight_batch_and_rollback_next_operation() {
    use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
    #[derive(Clone, Default)]
    struct Partner {
        requests: Arc<std::sync::Mutex<Vec<Value>>>,
        arrived: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    async fn partner(State(state): State<Partner>, Json(body): Json<Value>) -> impl IntoResponse {
        state.requests.lock().unwrap().push(body.clone());
        let messages = body["messages"].as_array().unwrap();
        let user = messages.iter().rev().find(|m| m["role"] == "user").unwrap()["content"]
            .as_str()
            .unwrap();
        let first = user == "old-batch" && messages.last().unwrap()["role"] != "tool";
        if first {
            state.arrived.notify_one();
            state.release.notified().await;
        }
        let mut chunks = vec![];
        if first {
            let wire = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| {
                    t["function"]["description"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("workmem.get:")
                })
                .unwrap()["function"]["name"]
                .as_str()
                .unwrap();
            chunks.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"persona_snapshot_tool","type":"function","function":{"name":wire,"arguments":"{}"}}]},"finish_reason":null}]}));
            chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
        } else {
            chunks.push(json!({"choices":[{"index":0,"delta":{"content":"snapshot checked"},"finish_reason":null}]}));
            chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
        }
        let mut data = chunks
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>();
        data.push_str("data: [DONE]\n\n");
        ([("content-type", "text/event-stream")], data).into_response()
    }
    let provider = Partner::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/chat/completions", post(partner))
        .with_state(provider.clone());
    let provider_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    store.config_set("services",&json!({"llm":{"provider":"deepseek","base_url":provider_base,"model":"synthetic-model","api_key_env":""}}).to_string()).unwrap();
    hm_core::agent_md::AgentMdService::new(store.clone())
        .update("PERSONA_OLD", "console", "")
        .unwrap();
    let dir = std::env::temp_dir().join(format!(
        "hm-persona-worker-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    let state = AppState::with_overrides(
        store.clone(),
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    state.voice.set_memory_extraction(false);
    let token = state.auth.provision("persona-publication-test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let delivery = hm_core::config_delivery::ConfigDelivery::new(store.clone());
    delivery.enroll("test-a").unwrap();
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"text":"old-batch","session_id":"persona-old-batch"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let old_batch = tokio::spawn(async move { response.text().await.unwrap() });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        provider.arrived.notified(),
    )
    .await
    .expect("actual first provider request reaches barrier");
    let snapshot = delivery.state("unused").unwrap();
    let pkg = json!({"format":"home-mate-config","version":2,"package_id":hm_core::config_delivery::uuid().unwrap(),"package_revision":1,"created_ms":1,"purpose":"node_config",
        "source":{"node_id":"test-a","config_revision":snapshot["config_revision"],"core_version":snapshot["core_version"],"schema_version":snapshot["schema_version"]},"target":{"household_id":snapshot["target"]["household_id"],"node_id":"test-a"},
        "compatibility":{"core_api_schema":2,"gateway_protocol_min":0,"gateway_protocol_max":0,"required_capabilities":[]},"change_reason":"Publish persona at operation boundary","bindings":[],
        "blocks":[{"name":"persona","schema_version":1,"operations":[{"action":"set","path":"persona.agent_md.content","value":"PERSONA_NEW"}]}]});
    let plan = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({"package":pkg}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let request = json!({"plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],"expected_revision":plan["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()});
    let receipt = client
        .post(format!("{base}/api/v1/config/applications"))
        .bearer_auth(&token)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(receipt["state"], "saved_pending");
    let pending = client
        .get(format!("{base}/api/v1/config/state"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(pending["stored"]["persona.agent_md.content"], "PERSONA_OLD");
    assert_eq!(
        pending["resolved"]["persona.agent_md.content"],
        "PERSONA_NEW"
    );
    // Start the real production supervisor only after proving desired-only
    // commit. Its startup scan recovers this already-persisted pending job.
    let _worker = hm_core::config_delivery::ConfigWorker::start(store.clone());
    let proof = publication(
        &client,
        &base,
        &token,
        request["application_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(proof["publication_isolation"], "persona");
    assert_eq!(proof["fields"][0]["value"], "PERSONA_NEW");
    assert_eq!(proof["initial_receipt"], receipt);
    let next = client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"text":"new-operation","session_id":"persona-new-operation"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(next.contains("snapshot checked"));
    provider.release.notify_one();
    let old_reply = tokio::time::timeout(std::time::Duration::from_secs(3), old_batch)
        .await
        .unwrap()
        .unwrap();
    assert!(
        old_reply.contains("tool_result")
            && old_reply.contains("turn_end")
            && old_reply.contains("completed"),
        "{old_reply}"
    );
    {
        let requests = provider.requests.lock().unwrap();
        let old = requests
            .iter()
            .filter(|r| {
                r["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"] == "old-batch")
            })
            .collect::<Vec<_>>();
        assert_eq!(old.len(), 2, "real query tool must cause a second LLM step");
        for request in old {
            let system = request["messages"][0]["content"].as_str().unwrap();
            assert!(
                system.contains("PERSONA_OLD") && !system.contains("PERSONA_NEW"),
                "inflight batch is immutable"
            );
        }
        let new = requests
            .iter()
            .find(|r| {
                r["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"] == "new-operation")
            })
            .unwrap();
        assert!(new["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("PERSONA_NEW"));
    }
    let rollback = client
        .post(format!(
            "{base}/api/v1/config/applications/{}/rollback-preflight",
            request["application_id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .json(&json!({"expected_revision":receipt["config_revision"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        hm_core::agent_md::AgentMdService::new(store.clone()).current_content(),
        "PERSONA_NEW",
        "rollback preview is read-only"
    );
    let restored=client.post(format!("{base}/api/v1/config/applications")).bearer_auth(&token).json(&json!({"plan_id":rollback["plan_id"],"plan_hash":rollback["plan_hash"],"expected_revision":rollback["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    publication(
        &client,
        &base,
        &token,
        restored["application_id"].as_str().unwrap(),
    )
    .await;
    client
        .post(format!("{base}/api/v0/chat"))
        .bearer_auth(&token)
        .json(&json!({"text":"rollback-operation","session_id":"persona-rollback-operation"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let system = requests.last().unwrap()["messages"][0]["content"]
        .as_str()
        .unwrap();
    assert!(system.contains("PERSONA_OLD") && !system.contains("PERSONA_NEW"));
    drop(requests);
    server.abort();
    provider_server.abort();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn production_binary_automatically_publishes_reviewed_persona() {
    struct CoreProcess(std::process::Child, std::path::PathBuf);
    impl Drop for CoreProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    drop(port);
    let dir = std::env::temp_dir().join(format!(
        "hm-config-process-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pending_store = Arc::new(Store::open(&dir.join("home-mate.db")).unwrap());
    let auth = hm_core::auth::AuthStore::new(pending_store.clone());
    let token = auth.provision("production-restart-fixture").unwrap();
    let actor = auth.verify(&token).unwrap();
    let delivery = hm_core::config_delivery::ConfigDelivery::new(pending_store.clone());
    delivery.enroll("test-a").unwrap();
    let pending_state = delivery.state(&actor).unwrap();
    let pending_package=serde_json::from_value(json!({"format":"home-mate-config","version":2,"package_id":hm_core::config_delivery::uuid().unwrap(),"package_revision":1,"created_ms":1,"purpose":"node_config",
        "source":{"node_id":"test-a","config_revision":pending_state["config_revision"],"core_version":pending_state["core_version"],"schema_version":pending_state["schema_version"]},"target":{"household_id":pending_state["target"]["household_id"],"node_id":"test-a"},
        "compatibility":{"core_api_schema":2,"gateway_protocol_min":0,"gateway_protocol_max":0,"required_capabilities":[]},"change_reason":"Persisted restart worker fixture","bindings":[],
        "blocks":[{"name":"services","schema_version":1,"operations":[{"action":"set","path":"services.llm.model","value":"PROCESS_RESTART_SERVICES"},{"action":"set","path":"services.llm.base_url","value":"http://192.168.254.254:1/synthetic"}]}]})).unwrap();
    let pending_plan = delivery
        .preflight(&actor, pending_package, json!([]))
        .unwrap();
    let pending_id = hm_core::config_delivery::uuid().unwrap();
    let pending_request = hm_core::config_delivery::Commit {
        plan_id: pending_plan["plan_id"].as_str().unwrap().into(),
        plan_hash: pending_plan["plan_hash"].as_str().unwrap().into(),
        expected_revision: pending_plan["expected_revision"].as_u64().unwrap(),
        application_id: pending_id.clone(),
    };
    delivery.commit(&actor, &pending_request).unwrap();
    assert_eq!(
        hm_core::config::ConfigService::new(pending_store.clone())
            .load()
            .unwrap()
            .llm
            .model,
        "deepseek-flash"
    );
    drop(delivery);
    drop(auth);
    drop(pending_store);
    // No inherited provider credentials, configured endpoints or live devices.
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_hm-core"))
        .env_clear()
        .env("HM_DATA_DIR", &dir)
        .env("HM_BIND", address.to_string())
        .env("HM_GATEWAY_BIND", "127.0.0.1:0")
        .env("HM_DB_RETENTION_DAYS", "0")
        .env("HM_TIME_BOOTSTRAP", "0")
        .env("RUST_LOG", "error")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut core = CoreProcess(child, dir);
    let base = format!("http://{address}");
    let client = reqwest::Client::new();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            assert!(
                core.0.try_wait().unwrap().is_none(),
                "production Core must stay running"
            );
            if client
                .get(format!("{base}/api/v0/status"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("production Core starts");
    let recovered = publication(&client, &base, &token, &pending_id).await;
    assert_eq!(recovered["fields"][0]["value"], "PROCESS_RESTART_SERVICES");
    assert_eq!(
        recovered["publication"]["revision"], recovered["observed_revision"],
        "startup operational maintenance must not supersede persisted desired"
    );
    client
        .put(format!("{base}/api/v0/fleet"))
        .bearer_auth(&token)
        .json(&json!({"self_id":"test-a"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    client
        .put(format!("{base}/api/v1/config/enrollment"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let state = client
        .get(format!("{base}/api/v1/config/state"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let package = json!({"format":"home-mate-config","version":2,"package_id":hm_core::config_delivery::uuid().unwrap(),"package_revision":1,"created_ms":1,"purpose":"node_config",
        "source":{"node_id":"test-a","config_revision":state["config_revision"],"core_version":state["core_version"],"schema_version":state["schema_version"]},"target":{"household_id":state["target"]["household_id"],"node_id":"test-a"},
        "compatibility":{"core_api_schema":2,"gateway_protocol_min":0,"gateway_protocol_max":0,"required_capabilities":[]},"change_reason":"Production worker smoke","bindings":[],
        "blocks":[{"name":"persona","schema_version":1,"operations":[{"action":"set","path":"persona.agent_md.content","value":"PROCESS_PERSONA_VERIFIED"}]},{"name":"services","schema_version":1,"operations":[{"action":"set","path":"services.llm.model","value":"PROCESS_SERVICE_VERIFIED"},{"action":"set","path":"services.llm.base_url","value":"http://192.168.254.254:1/synthetic"}]}]});
    let plan = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({"package":package}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let id = hm_core::config_delivery::uuid().unwrap();
    let receipt=client.post(format!("{base}/api/v1/config/applications")).bearer_auth(&token).json(&json!({"plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],"expected_revision":plan["expected_revision"],"application_id":id})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    assert_eq!(receipt["state"], "saved_pending");
    // No manually constructed worker or projection call exists in this test.
    let proof = publication(&client, &base, &token, &id).await;
    assert_eq!(proof["fields"][0]["state"], "core_readback_verified");
    assert_eq!(proof["fields"][0]["value"], "PROCESS_PERSONA_VERIFIED");
    assert_eq!(proof["initial_receipt"], receipt);
    let native = client
        .get(format!("{base}/api/v0/agent-md"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(native["content"], "PROCESS_PERSONA_VERIFIED");
    assert!(proof["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|field| field["path"] == "services.llm.model"
            && field["value"] == "PROCESS_SERVICE_VERIFIED"
            && field["state"] == "core_readback_verified"));
}

#[tokio::test]
async fn exports_endpoint_returns_redacted_v2_default_and_v1_with_loss_report() {
    let dir = std::env::temp_dir().join(format!(
        "hm-config-export-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.secret_set("llm.api_key", "sk-never-export").unwrap();
    let state = AppState::with_overrides(
        store.clone(),
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let token = state.auth.provision("export-test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::new();

    // Default v2 redacted export
    let v2_res: Value = client
        .post(format!("{base}/api/v1/config/exports"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v2_res["format_version"], 2);
    assert_eq!(v2_res["redacted"], true);
    assert_eq!(v2_res["package"]["compatibility"]["core_api_schema"], 2);
    assert!(!v2_res["package"]["content_hash"]
        .as_str()
        .unwrap()
        .is_empty());
    let v2_text = serde_json::to_string(&v2_res).unwrap();
    assert!(!v2_text.contains("sk-never-export"));

    // v1 compatibility export with loss report
    let v1_res: Value = client
        .post(format!("{base}/api/v1/config/exports"))
        .bearer_auth(&token)
        .json(&json!({ "format": "v1" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v1_res["format_version"], 1);
    assert_eq!(v1_res["redacted"], true);
    assert_eq!(v1_res["document"]["version"], 1);
    assert!(v1_res["loss_report"].as_array().unwrap().len() >= 5);
    assert!(!serde_json::to_string(&v1_res)
        .unwrap()
        .contains("sk-never-export"));

    // Rejects forbidden export block
    let forbidden = client
        .post(format!("{base}/api/v1/config/exports"))
        .bearer_auth(&token)
        .json(&json!({ "blocks": ["secrets"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 400);
    let err: Value = forbidden.json().await.unwrap();
    assert_eq!(err["error"], "FORBIDDEN_EXPORT_BLOCK");

    let _ = std::fs::remove_dir_all(&dir);
}
