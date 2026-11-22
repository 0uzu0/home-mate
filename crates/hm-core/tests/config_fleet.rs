//! 5-node household enrollment, FamilyPolicy allowlist, sequential fleet rollout,
//! stop-on-failure, retry idempotency, and per-unit rollback tests (#110).
use hm_core::{
    api::{router, AppState},
    config_delivery::{v1, ConfigWorker, Purpose},
    fleet::strip_node_local,
    store::Store,
    tool::mocks::MockInject,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};

struct CoreNode {
    node_id: String,
    _store: Arc<Store>,
    state: Arc<AppState>,
    client: reqwest::Client,
    base: String,
    token: String,
    server: Option<tokio::task::JoinHandle<()>>,
    _worker: ConfigWorker,
    dir: std::path::PathBuf,
}

impl Drop for CoreNode {
    fn drop(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl CoreNode {
    async fn start(node: &str, set_identity: bool) -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .config_set(
                "unit_acoustic_profiles",
                &json!({ node: { "volume_cap": 0.41, "mix_gain": 0.85, "mic_gain_offset_db": 1.5 } })
                    .to_string(),
            )
            .unwrap();
        store
            .config_set(
                "node",
                &json!({ "wake": { "phrases": [format!("你好-{node}")] }, "led": { "timezone": "Asia/Shanghai" } })
                    .to_string(),
            )
            .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "hm-fleet-test-{node}-{}",
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
        if set_identity {
            state.fleet.set_self_id(node).unwrap();
        }
        let worker = ConfigWorker::start_for_app(&state);
        let token = state.auth.provision(&format!("fleet-test-{node}")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Self {
            node_id: node.to_string(),
            _store: store,
            state,
            client: reqwest::Client::new(),
            base,
            token,
            server: Some(server),
            _worker: worker,
            dir,
        }
    }

    fn stop_server(&mut self) {
        if let Some(h) = self.server.take() {
            h.abort();
        }
    }

    async fn restart_server(&mut self) {
        self.stop_server();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(self.state.clone());
        self.server = Some(tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        }));
    }

    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn json(&self, path: &str, body: Value) -> Value {
        self.post(path, body)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn put(&self, path: &str, body: Value) -> Value {
        self.client
            .put(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

async fn setup_five_node_household() -> (CoreNode, CoreNode, CoreNode, CoreNode, CoreNode) {
    let dot2_d = CoreNode::start("dot2-d", true).await;
    let dot2_a = CoreNode::start("dot2-a", true).await;
    let dot2_b = CoreNode::start("dot2-b", true).await;
    let dot2_c = CoreNode::start("dot2-c", true).await;
    let dot2_e = CoreNode::start("dot2-e", true).await;

    let enrolled = dot2_d.put("/api/v1/config/enrollment", json!({})).await;
    let household_id = enrolled["household_id"].as_str().unwrap().to_string();

    for peer in [&dot2_a, &dot2_b, &dot2_c, &dot2_e] {
        dot2_d
            .state
            .fleet
            .upsert(&peer.node_id, &peer.base, Some(&peer.token), false)
            .unwrap();
        let inv = dot2_d
            .json(
                "/api/v1/config/household/invitations",
                json!({ "expected_node_id": peer.node_id }),
            )
            .await;
        let joined = peer
            .json(
                "/api/v1/config/household/join",
                json!({
                    "token": inv["token"],
                    "issuer_base_url": dot2_d.base,
                    "issuer_token": dot2_d.token,
                }),
            )
            .await;
        assert_eq!(joined["household_id"], household_id);
        assert_eq!(joined["node_id"], peer.node_id);
    }

    (dot2_d, dot2_a, dot2_b, dot2_c, dot2_e)
}

fn family_policy_v2(household_id: &str, persona_text: &str) -> Value {
    json!({
        "format": "home-mate-config",
        "version": 2,
        "package_id": hm_core::config_delivery::uuid().unwrap(),
        "package_revision": 1,
        "created_ms": 1731900000000u64,
        "purpose": "family_policy",
        "source": {
            "node_id": "dot2-d",
            "config_revision": 1,
            "core_version": hm_core::CORE_VERSION,
            "schema_version": 15
        },
        "target": {
            "household_id": household_id
        },
        "compatibility": {
            "core_api_schema": 2,
            "gateway_protocol_min": 0,
            "gateway_protocol_max": 0,
            "required_capabilities": []
        },
        "change_reason": "household family policy update",
        "blocks": [
            {
                "name": "persona",
                "schema_version": 1,
                "operations": [
                    { "action": "set", "path": "persona.agent_md.content", "value": persona_text }
                ]
            },
            {
                "name": "mode",
                "schema_version": 1,
                "operations": [
                    { "action": "set", "path": "mode.follow_window_ms", "value": 9000 }
                ]
            }
        ],
        "bindings": []
    })
}

#[tokio::test]
async fn household_invitation_join_and_identity_binding() {
    let dot2_d = CoreNode::start("dot2-d", true).await;
    let dot2_a = CoreNode::start("dot2-a", true).await;
    let dot2_b = CoreNode::start("dot2-b", true).await;
    let default_node = CoreNode::start("unnamed", false).await;

    let state = dot2_d.get("/api/v1/config/state").await;
    assert_eq!(state["capabilities"]["fleet"], true);

    dot2_d.put("/api/v1/config/enrollment", json!({})).await;
    let inv_a = dot2_d
        .json(
            "/api/v1/config/household/invitations",
            json!({ "expected_node_id": "dot2-a" }),
        )
        .await;

    // Default identity node is refused.
    let err_default = default_node
        .post(
            "/api/v1/config/household/join",
            json!({
                "token": inv_a["token"],
                "issuer_base_url": dot2_d.base,
                "issuer_token": dot2_d.token,
            }),
        )
        .await;
    assert_eq!(
        err_default.json::<Value>().await.unwrap()["error"],
        "NODE_ID_REQUIRED"
    );

    // Wrong node identity is refused even with valid token + X-Home-Mate-Source header.
    let err_wrong = dot2_b
        .client
        .post(format!("{}/api/v1/config/household/join", dot2_b.base))
        .bearer_auth(&dot2_b.token)
        .header("X-Home-Mate-Source", "dot2-a")
        .json(&json!({
            "token": inv_a["token"],
            "issuer_base_url": dot2_d.base,
            "issuer_token": dot2_d.token,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        err_wrong.json::<Value>().await.unwrap()["error"],
        "TARGET_IDENTITY_MISMATCH"
    );

    // Intended node joins successfully.
    let joined = dot2_a
        .json(
            "/api/v1/config/household/join",
            json!({
                "token": inv_a["token"],
                "issuer_base_url": dot2_d.base,
                "issuer_token": dot2_d.token,
            }),
        )
        .await;
    assert_eq!(joined["enrolled"], true);
    assert_eq!(joined["node_id"], "dot2-a");

    // Single-use token cannot be replayed.
    let replay = dot2_a
        .post(
            "/api/v1/config/household/join",
            json!({
                "token": inv_a["token"],
                "issuer_base_url": dot2_d.base,
                "issuer_token": dot2_d.token,
            }),
        )
        .await;
    assert_eq!(
        replay.json::<Value>().await.unwrap()["error"],
        "INVITATION_ALREADY_USED"
    );
}

#[tokio::test]
async fn family_policy_allowlist_v1_migration_and_malicious_injection_rejected() {
    let (dot2_d, _a, _b, _c, _e) = setup_five_node_household().await;
    let state = dot2_d.get("/api/v1/config/state").await;
    let household_id = state["target"]["household_id"].as_str().unwrap();

    // v1 migration under FamilyPolicy surfaces requires_choice/blocked for N/U/S/D/B.
    let v1_doc = json!({
        "format": "home-mate-config",
        "version": 1,
        "persona": { "agent_md": { "content": "FAMILY_PERSONA" } },
        "device_led_configs": { "dot2-d": { "night_enabled": true } },
        "unit_acoustic_profiles": { "dot2-d": { "volume_cap": 0.5 } },
        "secrets": { "llm.api_key": "secret" },
        "learning": { "progress": 1 },
        "fleet": { "self_id": "dot2-d" },
        "reflex": {
            "words": [
                { "text": "你好小智", "tokens": "ni hao xiao zhi", "intent": "wake", "enabled": true },
                { "text": "暂停", "tokens": "zan ting", "intent": "pause", "enabled": true }
            ]
        }
    });

    let (_pkg_unsanitized, report_unsanitized) =
        v1::convert_for_purpose(v1_doc.clone(), &state, Purpose::FamilyPolicy, false).unwrap();
    let blocked_ptrs: Vec<&str> = report_unsanitized
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "requires_choice" && e["blocked"] == true)
        .filter_map(|e| e["pointer"].as_str())
        .collect();
    for expected in [
        "/device_led_configs/dot2-d/night_enabled",
        "/unit_acoustic_profiles",
        "/secrets",
        "/learning",
        "/fleet",
        "/reflex/words",
    ] {
        assert!(
            blocked_ptrs.contains(&expected),
            "expected blocked pointer {expected} in {blocked_ptrs:?}"
        );
    }

    let (pkg_sanitized, report_sanitized) =
        v1::convert_for_purpose(v1_doc, &state, Purpose::FamilyPolicy, true).unwrap();
    assert_eq!(pkg_sanitized.purpose, Purpose::FamilyPolicy);
    assert!(pkg_sanitized.target.node_id.is_none());
    assert!(report_sanitized
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "stripped_for_family"));
    let reflex_block = pkg_sanitized
        .blocks
        .iter()
        .find(|b| b.name == "reflex")
        .unwrap();
    assert_eq!(reflex_block.operations.len(), 1);

    // Malicious HTTP injection of reflex.words intent=wake is rejected with FORBIDDEN_SCOPE.
    let mut bad_wake = family_policy_v2(household_id, "FAMILY_PERSONA");
    bad_wake["blocks"].as_array_mut().unwrap().push(json!({
        "name": "reflex",
        "schema_version": 1,
        "operations": [{
            "action": "replace",
            "path": "reflex.words",
            "value": [{ "text": "你好小智", "tokens": "ni hao xiao zhi", "intent": "wake", "enabled": true }]
        }]
    }));
    let res = dot2_d
        .post(
            "/api/v1/config/fleet/preflight",
            json!({ "package": bad_wake, "targets": ["dot2-d"] }),
        )
        .await;
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"],
        "FORBIDDEN_SCOPE"
    );

    // Malicious HTTP injection of unit_acoustic_profiles is rejected with FORBIDDEN_SCOPE.
    let mut bad_unit = family_policy_v2(household_id, "FAMILY_PERSONA");
    bad_unit["blocks"].as_array_mut().unwrap().push(json!({
        "name": "unit_acoustic_profiles",
        "schema_version": 1,
        "operations": [{ "action": "set", "path": "unit_acoustic_profiles.dot2-d.volume_cap", "value": 0.9 }]
    }));
    let res2 = dot2_d
        .post(
            "/api/v1/config/fleet/runs",
            json!({ "package": bad_unit, "targets": ["dot2-d"] }),
        )
        .await;
    assert_eq!(
        res2.json::<Value>().await.unwrap()["error"],
        "FORBIDDEN_SCOPE"
    );

    // v0 strip_node_local parity check.
    let mut v0 = json!({
        "device_led_configs": { "dot2-d": { "night_enabled": true } },
        "node": { "wake": { "phrases": ["你好小智"] } },
        "privacy": { "idle_asr_enabled": false },
        "weather": { "enabled": true, "api_key_env": "WEATHER_KEY" },
        "mode": { "follow_window_ms": 9000, "wake_words": ["你好小智"], "kws_threshold": 0.25, "duplex_requested": { "dot2-d": "half" } },
        "reflex": { "words": [{ "text": "w", "intent": "wake" }, { "text": "p", "intent": "pause" }] }
    });
    strip_node_local(&mut v0);
    assert!(v0.get("device_led_configs").is_none());
    assert!(v0.get("node").is_none());
    assert!(v0.get("privacy").is_none());
    assert!(v0["weather"].get("api_key_env").is_none());
    assert!(v0["mode"].get("wake_words").is_none());
    assert!(v0["mode"].get("kws_threshold").is_none());
    assert!(v0["mode"].get("duplex_requested").is_none());
    assert_eq!(v0["mode"]["follow_window_ms"], 9000);
    assert_eq!(v0["reflex"]["words"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn sequential_dot2d_first_rollout_stop_on_failure_retry_idempotency_and_rollback() {
    let (dot2_d, dot2_a, mut dot2_b, dot2_c, dot2_e) = setup_five_node_household().await;
    let household_id = dot2_d.get("/api/v1/config/state").await["target"]["household_id"]
        .as_str()
        .unwrap()
        .to_string();

    let pkg = family_policy_v2(&household_id, "SHARED_FAMILY_PERSONA");

    // Preflight enforces dot2-d first regardless of caller target order.
    let pf = dot2_d
        .json(
            "/api/v1/config/fleet/preflight",
            json!({
                "package": pkg,
                "targets": ["dot2-a", "dot2-b", "dot2-d", "dot2-c", "dot2-e"]
            }),
        )
        .await;
    assert_eq!(pf["all_ok"], true);
    assert_eq!(
        pf["ordered_targets"],
        json!(["dot2-d", "dot2-a", "dot2-b", "dot2-c", "dot2-e"])
    );

    // Take dot2-b offline before committing: dot2-d and dot2-a succeed, dot2-b fails,
    // dot2-c and dot2-e are skipped immediately.
    dot2_b.stop_server();
    let run = dot2_d
        .json(
            "/api/v1/config/fleet/runs",
            json!({
                "package": pkg,
                "targets": ["dot2-a", "dot2-b", "dot2-d", "dot2-c", "dot2-e"]
            }),
        )
        .await;
    let run_id = run["run_id"].as_str().unwrap().to_string();
    assert_eq!(run["state"], "failed");
    let targets = run["targets"].as_array().unwrap();
    assert_eq!(targets[0]["node_id"], "dot2-d", "{run:#}");
    assert_eq!(targets[0]["state"], "applied", "{run:#}");
    assert_eq!(targets[1]["node_id"], "dot2-a");
    assert_eq!(targets[1]["state"], "applied");
    assert_eq!(targets[2]["node_id"], "dot2-b");
    assert_eq!(targets[2]["state"], "failed");
    assert_eq!(targets[3]["node_id"], "dot2-c");
    assert_eq!(targets[3]["state"], "skipped");
    assert_eq!(targets[4]["node_id"], "dot2-e");
    assert_eq!(targets[4]["state"], "skipped");

    let app_id_d = targets[0]["application_id"].as_str().unwrap().to_string();
    let app_id_a = targets[1]["application_id"].as_str().unwrap().to_string();
    let app_id_b = targets[2]["application_id"].as_str().unwrap().to_string();

    assert_eq!(
        dot2_d.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert_eq!(
        dot2_a.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert!(dot2_c.get("/api/v1/config/state").await["layers"]["family"]
        ["persona.agent_md.content"]
        .is_null());
    assert!(dot2_e.get("/api/v1/config/state").await["layers"]["family"]
        ["persona.agent_md.content"]
        .is_null());

    // Bring dot2-b back up and retry: reuses persisted application_ids without duplicating commits.
    dot2_b.restart_server().await;
    dot2_d
        .state
        .fleet
        .upsert("dot2-b", &dot2_b.base, Some(&dot2_b.token), false)
        .unwrap();

    let retried = dot2_d
        .json(
            &format!("/api/v1/config/fleet/runs/{run_id}/retry"),
            json!({}),
        )
        .await;
    assert_eq!(retried["state"], "applied");
    let r_targets = retried["targets"].as_array().unwrap();
    assert_eq!(r_targets[0]["application_id"], app_id_d);
    assert_eq!(r_targets[1]["application_id"], app_id_a);
    assert_eq!(r_targets[2]["application_id"], app_id_b);
    for t in r_targets {
        assert_eq!(t["state"], "applied");
    }
    assert_eq!(
        dot2_b.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert_eq!(
        dot2_c.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert_eq!(
        dot2_e.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );

    // Per-unit rollback on dot2-a restores only dot2-a without reverting the other 4 nodes.
    let rb = dot2_d
        .json(
            &format!("/api/v1/config/fleet/runs/{run_id}/targets/dot2-a/rollback-preflight"),
            json!({}),
        )
        .await;
    assert_eq!(rb["committed"], true);
    assert_eq!(rb["readback"]["publication"]["state"], "applied");
    assert!(dot2_a.get("/api/v1/config/state").await["layers"]["family"]
        ["persona.agent_md.content"]
        .is_null());
    assert_eq!(
        dot2_d.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert_eq!(
        dot2_b.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert_eq!(
        dot2_c.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
    assert_eq!(
        dot2_e.get("/api/v1/config/state").await["layers"]["family"]["persona.agent_md.content"],
        "SHARED_FAMILY_PERSONA"
    );
}

#[tokio::test]
async fn direct_receiver_preflight_blocks_out_of_scope_and_disk_reopen_preserves_per_unit_state() {
    let dir = std::env::temp_dir().join(format!(
        "hm-fleet-disk-{}",
        hm_core::config_delivery::uuid().unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("home-mate.db");

    let store = Arc::new(Store::open(&db_path).unwrap());
    store
        .config_set(
            "unit_acoustic_profiles",
            &json!({ "dot2-a": { "volume_cap": 0.33, "mix_gain": 0.77, "mic_gain_offset_db": -3.5 } })
                .to_string(),
        )
        .unwrap();
    store
        .config_set(
            "device_led_configs",
            &json!({ "dot2-a": { "night_enabled": true, "night_brightness_pct": 15 } }).to_string(),
        )
        .unwrap();
    store
        .config_set(
            "node",
            &json!({ "wake": { "phrases": ["你好-dot2-a"] }, "led": { "timezone": "Asia/Shanghai" } })
                .to_string(),
        )
        .unwrap();

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
    state.fleet.set_self_id("dot2-a").unwrap();
    let mut voice_cfg = state.voice.settings_snapshot();
    voice_cfg.kws_threshold = 0.41;
    voice_cfg.media_trim_db = -5.5;
    voice_cfg.vad = Some(hm_core::voice::VadSettings {
        start_db: -34.0,
        end_db: -44.0,
    });
    state.voice.restore_settings(voice_cfg, "test").unwrap();

    let worker = ConfigWorker::start_for_app(&state);
    let token = state.auth.provision("receiver-disk-test").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn({
        let app = router(state.clone());
        async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        }
    });
    let client = reqwest::Client::new();

    let enroll = client
        .put(format!("{base}/api/v1/config/enrollment"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(enroll.status(), reqwest::StatusCode::OK);
    let enroll_body: Value = enroll.json().await.unwrap();
    let household_id = enroll_body["household_id"].as_str().unwrap().to_string();

    // Direct receiver preflight must reject injected unit_acoustic_profiles and wake reflex words.
    let mut bad_unit = family_policy_v2(&household_id, "FAMILY_PERSONA");
    bad_unit["blocks"].as_array_mut().unwrap().push(json!({
        "name": "unit_acoustic_profiles",
        "schema_version": 1,
        "operations": [{ "action": "set", "path": "unit_acoustic_profiles.dot2-a.volume_cap", "value": 0.99 }]
    }));
    let pf_bad = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({ "package": bad_unit }))
        .send()
        .await
        .unwrap();
    assert_eq!(pf_bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let pf_bad_body: Value = pf_bad.json().await.unwrap();
    assert_eq!(pf_bad_body["error"], "FORBIDDEN_SCOPE", "{pf_bad_body}");

    let mut bad_wake = family_policy_v2(&household_id, "FAMILY_PERSONA");
    bad_wake["blocks"].as_array_mut().unwrap().push(json!({
        "name": "reflex",
        "schema_version": 1,
        "operations": [{
            "action": "replace",
            "path": "reflex.words",
            "value": [{ "text": "你好小智", "tokens": "ni hao xiao zhi", "intent": "wake", "enabled": true }]
        }]
    }));
    let pf_wake = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({ "package": bad_wake }))
        .send()
        .await
        .unwrap();
    assert_eq!(pf_wake.status(), reqwest::StatusCode::BAD_REQUEST);
    let pf_wake_body: Value = pf_wake.json().await.unwrap();
    assert_eq!(pf_wake_body["error"], "FORBIDDEN_SCOPE", "{pf_wake_body}");

    // Apply valid FamilyPolicy directly to target node and commit.
    let valid_pkg = family_policy_v2(&household_id, "DISK_FAMILY_PERSONA");
    let pf_ok = client
        .post(format!("{base}/api/v1/config/preflight"))
        .bearer_auth(&token)
        .json(&json!({ "package": valid_pkg }))
        .send()
        .await
        .unwrap();
    assert_eq!(pf_ok.status(), reqwest::StatusCode::OK);
    let pf_ok_body: Value = pf_ok.json().await.unwrap();
    assert_eq!(pf_ok_body["blocked"], json!([]), "{pf_ok_body}");

    let app_id = hm_core::config_delivery::uuid().unwrap();
    let commit = client
        .post(format!("{base}/api/v1/config/applications"))
        .bearer_auth(&token)
        .json(&json!({
            "plan_id": pf_ok_body["plan_id"],
            "plan_hash": pf_ok_body["plan_hash"],
            "expected_revision": pf_ok_body["expected_revision"],
            "application_id": app_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(commit.status(), reqwest::StatusCode::ACCEPTED);

    // Wait for ConfigWorker publication readback to report "applied".
    let applied = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let rb: Value = client
                .get(format!(
                    "{base}/api/v1/config/applications/{app_id}/readback"
                ))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if rb["publication"]["state"] == "applied" {
                return rb;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("v2 application readback applied");
    assert_eq!(applied["publication"]["state"], "applied");

    server.abort();
    drop(worker);
    drop(state);
    drop(store);

    // Reopen file-backed SQLite store in a fresh AppState and verify family + per-unit survival.
    let store2 = Arc::new(Store::open(&db_path).unwrap());
    let state2 = AppState::with_overrides(
        store2.clone(),
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let state2_snapshot = hm_core::config_delivery::ConfigDelivery::for_app(&state2)
        .state("disk-reopen-test")
        .unwrap();
    assert_eq!(
        state2_snapshot["layers"]["family"]["persona.agent_md.content"],
        "DISK_FAMILY_PERSONA"
    );
    assert_eq!(state2.voice.settings_snapshot().follow_window_ms, 9000);
    assert!((state2.voice.settings_snapshot().kws_threshold - 0.41).abs() < 1e-6);
    assert!((state2.voice.settings_snapshot().media_trim_db - (-5.5)).abs() < 1e-6);
    assert!((state2.voice.settings_snapshot().vad.unwrap().start_db - (-34.0)).abs() < 1e-6);

    let prof = state2.config.unit_acoustic_profile("dot2-a").unwrap();
    assert_eq!(prof.volume_cap, 0.33);
    assert!((prof.mix_gain - 0.77).abs() < 1e-6);
    assert!((prof.mic_gain_offset_db - (-3.5)).abs() < 1e-6);
    let led = state2.config.device_led_config("dot2-a").unwrap();
    assert!(led.night_enabled);
    assert_eq!(led.night_brightness_pct, 15);
    let node_raw: Value =
        serde_json::from_str(&store2.config_get("node").unwrap().unwrap()).unwrap();
    assert_eq!(node_raw["wake"]["phrases"], json!(["你好-dot2-a"]));

    let _ = std::fs::remove_dir_all(&dir);
}
