//! Authenticated generated artifact, existing reviewed API and real consumer.
use hm_core::{
    api::{router, AppState},
    config_delivery::{ConfigDelivery, ConfigWorker},
    store::Store,
    tool::mocks::MockInject,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};

struct Core {
    store: Arc<Store>,
    state: Arc<AppState>,
    client: reqwest::Client,
    base: String,
    token: String,
    server: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}
impl Drop for Core {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Core {
    async fn start(node: &str, endpoint: &str, enroll: bool) -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        store.config_set("services",&json!({"llm":{"provider":"deepseek","base_url":endpoint,"model":"synthetic-profile-model","api_key_env":""}}).to_string()).unwrap();
        store.config_set("voice_mode",&json!({"wake_words":[format!("自定义-{node}")],"media_trim_db":-7,"vad":{"start_db":-43,"end_db":-54}}).to_string()).unwrap();
        store
            .config_set(
                "unit_acoustic_profiles",
                &json!({node:{"volume_cap":0.37,"mix_gain":0.83,"mic_gain_offset_db":2.17}})
                    .to_string(),
            )
            .unwrap();
        store
            .config_set(
                "device_meta",
                &json!({node:{"name":"dot2-d","room":"dot2-d"}}).to_string(),
            )
            .unwrap();
        store
            .secret_set("llm.api_key", "synthetic-target-credential")
            .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "hm-dot2d-profile-{}",
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
        state.fleet.set_self_id(node).unwrap();
        if enroll {
            ConfigDelivery::new(store.clone()).enroll(node).unwrap();
        }
        state
            .agent_md
            .update("ORIGINAL_PERSONA", "console", "")
            .unwrap();
        let token = state.auth.provision("dot2d-profile-test").unwrap();
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
            store,
            state,
            client: reqwest::Client::new(),
            base,
            token,
            server,
            dir,
        }
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
    async fn published(&self, id: &str) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let value = self
                    .client
                    .get(format!(
                        "{}/api/v1/config/applications/{id}/readback",
                        self.base
                    ))
                    .bearer_auth(&self.token)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap();
                if value["publication"]["state"] == "applied" {
                    return value;
                }
                assert_ne!(value["publication"]["state"], "failed", "{value}");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("profile reaches the real supervised publisher")
    }
    fn protected(&self) -> Vec<(String, String)> {
        self.store
            .with(|c| {
                let mut query = c.prepare(
                    "SELECT key,value FROM config_kv WHERE key!='api_tokens' ORDER BY key",
                )?;
                let rows = query
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap()
    }
}
fn submission(plan: &Value) -> Value {
    json!({"plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],"expected_revision":plan["expected_revision"],"application_id":hm_core::config_delivery::uuid().unwrap()})
}

#[tokio::test]
async fn generated_persona_artifact_uses_existing_reviewed_worker_consumer_and_rollback() {
    use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
    type Requests = Arc<std::sync::Mutex<Vec<Value>>>;
    async fn provider(
        State(requests): State<Requests>,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        requests.lock().unwrap().push(body);
        ([("content-type","text/event-stream")],"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"profile checked\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
    }
    let requests: Requests = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/chat/completions", post(provider))
        .with_state(requests.clone());
    let partner = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let core = Core::start("dot2-d", &endpoint, true).await;
    let protected = core.protected();
    let generated = core
        .json("/api/v1/config/profiles/dot2-d-persona", json!({}))
        .await;
    assert_eq!(generated["scope"], "persona_only");
    assert_eq!(generated["complete_luna_profile"], false);
    assert_eq!(
        generated["target_assurance"]["physical_device_identity"],
        "not_verified"
    );
    assert_eq!(generated["wake_policy"]["action"], "preserve");
    assert_eq!(generated["wake_policy"]["recommended_default"], "你好小智");
    assert_eq!(generated["package"]["target"]["node_id"], "dot2-d");
    assert_eq!(generated["package"]["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(generated["package"]["blocks"][0]["name"], "persona");
    assert_eq!(generated["package"]["bindings"], json!([]));
    let artifact = serde_json::to_vec_pretty(&generated["package"]).unwrap();
    let artifact_text = String::from_utf8(artifact.clone()).unwrap();
    for protected_value in [
        "synthetic-target-credential",
        "ORIGINAL_PERSONA",
        "volume_cap",
        "mic_gain_offset_db",
        "自定义-dot2-d",
    ] {
        assert!(!artifact_text.contains(protected_value));
    }
    let parsed: hm_core::config_delivery::Package = serde_json::from_slice(&artifact).unwrap();
    assert_eq!(
        parsed.content_hash.as_ref().unwrap(),
        &parsed.hash().unwrap()
    );
    assert_eq!(core.protected(), protected);
    assert!(
        requests.lock().unwrap().is_empty(),
        "generation and validation make no provider requests"
    );
    // The downloaded JSON is accepted by the existing preflight endpoint;
    // generation never creates a separate commit or publication path.
    let plan = core
        .json(
            "/api/v1/config/preflight",
            json!({"package":serde_json::from_slice::<Value>(&artifact).unwrap()}),
        )
        .await;
    let request = submission(&plan);
    let receipt = core
        .json("/api/v1/config/applications", request.clone())
        .await;
    assert_eq!(receipt["state"], "saved_pending");
    assert_eq!(core.state.agent_md.current_content(), "ORIGINAL_PERSONA");
    let _worker = ConfigWorker::start(core.store.clone());
    let proof = core
        .published(request["application_id"].as_str().unwrap())
        .await;
    assert_eq!(proof["fields"][0]["state"], "core_readback_verified");
    let reply = core
        .json_chat("profile operation", "profile-generated")
        .await;
    assert!(reply.contains("profile checked"));
    let prompt = requests.lock().unwrap().last().unwrap()["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains("温和")
            && prompt.contains("英语陪练")
            && !prompt.contains("ORIGINAL_PERSONA")
    );
    let rollback = core
        .json(
            &format!(
                "/api/v1/config/applications/{}/rollback-preflight",
                request["application_id"].as_str().unwrap()
            ),
            json!({"expected_revision":receipt["config_revision"]}),
        )
        .await;
    let restored = core
        .json("/api/v1/config/applications", submission(&rollback))
        .await;
    core.published(restored["application_id"].as_str().unwrap())
        .await;
    core.json_chat("after rollback", "profile-restored").await;
    let prompt = requests.lock().unwrap().last().unwrap()["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains("ORIGINAL_PERSONA") && !prompt.contains("你是简洁、温和、耐心的家庭伙伴")
    );
    assert_eq!(
        core.protected(),
        protected,
        "custom wake, local acoustics, services and metadata stay byte-identical"
    );
    assert_eq!(
        core.store.secret_get("llm.api_key").unwrap().as_deref(),
        Some("synthetic-target-credential")
    );
    if let Ok(directory) = std::env::var("HM_TEST_CONFIG_EVIDENCE_DIR") {
        let path = std::path::Path::new(&directory);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("luna-dot2d-persona-v1.json"), artifact).unwrap();
        std::fs::write(
            path.join("generated-profile-review.json"),
            serde_json::to_vec_pretty(&generated).unwrap(),
        )
        .unwrap();
        std::fs::write(
            path.join("synthetic-profile-consumer.json"),
            serde_json::to_vec_pretty(&*requests.lock().unwrap()).unwrap(),
        )
        .unwrap();
    }
    partner.abort();
}
impl Core {
    async fn json_chat(&self, text: &str, session: &str) -> String {
        self.post("/api/v0/chat", json!({"text":text,"session_id":session}))
            .await
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn profile_generation_requires_paired_enrolled_node_and_existing_target_integrity() {
    let core = Core::start("dot2-d", "http://127.0.0.1:1/synthetic", true).await;
    assert_eq!(
        core.client
            .post(format!(
                "{}/api/v1/config/profiles/dot2-d-persona",
                core.base
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        core.post(
            "/api/v1/config/profiles/dot2-d-persona",
            json!({"target":"dot2-d"})
        )
        .await
        .status(),
        400
    );
    for body in [
        "[]",
        "null",
        r#"{"extra":true}"#,
        r#"{"extra":true,"extra":false}"#,
    ] {
        let response = core
            .client
            .post(format!(
                "{}/api/v1/config/profiles/dot2-d-persona",
                core.base
            ))
            .bearer_auth(&core.token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            400,
            "only an empty JSON object is allowed, got {body}"
        );
    }
    let generated = core
        .json("/api/v1/config/profiles/dot2-d-persona", json!({}))
        .await;
    for node in ["dot2-b", "dot2-c", "dot2-e", "dot2a"] {
        let other = Core::start(node, "http://127.0.0.1:1/synthetic", true).await;
        let before = other.protected();
        assert_eq!(
            other
                .post("/api/v1/config/profiles/dot2-d-persona", json!({}))
                .await
                .json::<Value>()
                .await
                .unwrap()["error"],
            "PROFILE_TARGET_MISMATCH",
            "a matching display name/room grants no target identity"
        );
        assert_eq!(
            other
                .post(
                    "/api/v1/config/preflight",
                    json!({"package":generated["package"]})
                )
                .await
                .json::<Value>()
                .await
                .unwrap()["error"],
            "TARGET_MISMATCH"
        );
        let state = ConfigDelivery::new(other.store.clone())
            .state("fixture")
            .unwrap();
        let mut changed = generated["package"].clone();
        changed["target"] = json!({"household_id":state["target"]["household_id"],"node_id":node});
        assert_eq!(
            other
                .post("/api/v1/config/preflight", json!({"package":changed}))
                .await
                .json::<Value>()
                .await
                .unwrap()["error"],
            "PACKAGE_HASH_MISMATCH",
            "hash is integrity, not a signature or extra authority"
        );
        assert_eq!(other.protected(), before);
        assert_eq!(other.state.agent_md.current_content(), "ORIGINAL_PERSONA");
    }
    let unenrolled = Core::start("dot2-d", "http://127.0.0.1:1/synthetic", false).await;
    assert_eq!(
        unenrolled
            .post("/api/v1/config/profiles/dot2-d-persona", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap()["error"],
        "ENROLLMENT_REQUIRED"
    );
    core.store
        .config_set("synthetic_unrelated", "changed")
        .unwrap();
    assert_eq!(
        core.post(
            "/api/v1/config/applications",
            submission(&generated["preflight"])
        )
        .await
        .status(),
        409,
        "generated preview uses the existing revision CAS"
    );
    let mut pending = generated["package"].clone();
    pending["content_hash"] = Value::Null;
    pending["package_id"] = json!(hm_core::config_delivery::uuid().unwrap());
    pending["blocks"] = json!([{"name":"services","schema_version":1,"operations":[{"action":"set","path":"services.llm.model","value":"pending-service"}]}]);
    let plan = core
        .json("/api/v1/config/preflight", json!({"package":pending}))
        .await;
    core.json("/api/v1/config/applications", submission(&plan))
        .await;
    assert_eq!(
        core.post("/api/v1/config/profiles/dot2-d-persona", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap()["error"],
        "PROFILE_BASELINE_PENDING",
        "a persona-only profile must not carry earlier pending services"
    );
    core.state.fleet.set_self_id("dot2-c").unwrap();
    assert_eq!(
        core.post("/api/v1/config/profiles/dot2-d-persona", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap()["error"],
        "PROFILE_TARGET_MISMATCH",
        "configured node and enrollment must agree; display labels cannot override either"
    );
}

#[tokio::test]
async fn luna_dot2d_family_v1_complete_profile_apply_and_rollback() {
    let core = Core::start("dot2-d", "http://127.0.0.1:1/synthetic", true).await;
    let _worker = ConfigWorker::start_for_app(&core.state);
    core.store
        .config_set(
            "device_led_configs",
            &json!({"dot2-d":{"night_enabled":false,"night_start_minute":0,"night_end_minute":0,"night_brightness_pct":50,"orientation_rotation":6,"orientation_reverse":true}}).to_string(),
        )
        .unwrap();
    // Seed personal and learning progress data (`CFG-09`): applying and rolling back
    // configuration must never touch learner progress, personal profile entries, or memories.
    core.state
        .user_md
        .upsert_console(
            None,
            "learner_child_1",
            "英语等级",
            "KET-Unit-4-Score-92",
            "learning",
        )
        .unwrap();
    let sessions = hm_core::session::SessionStore::new(core.store.clone());
    sessions.ensure("learning:child_1").unwrap();
    sessions
        .append("learning:child_1", "user", "apple 拼写已掌握")
        .unwrap();

    let resp = core
        .post("/api/v1/config/profiles/dot2-d-family", json!({}))
        .await;
    let status = resp.status();
    let generated: Value = resp.json().await.unwrap();
    assert!(
        status.is_success(),
        "dot2-d-family failed ({status}): {generated:#}"
    );
    assert_eq!(generated["profile_id"], "luna-dot2d-family-v1");
    assert_eq!(generated["complete_luna_profile"], true);
    assert_eq!(
        generated["target_assurance"]["immutable_target_lock"],
        "dot2-d"
    );
    assert!(
        generated["retained_unit_refs"]["unit_acoustic_profiles.dot2-d"]
            .as_object()
            .is_some_and(|m| !m.is_empty())
    );
    assert!(generated["retained_unit_refs"]["services"]
        .as_object()
        .is_some_and(|m| !m.is_empty()));
    assert!(generated["blockers"].as_array().is_some_and(|b| b
        .iter()
        .any(|e| e["path"] == "privacy.idle_asr_enabled")
        && b.iter().any(|e| e["path"] == "mode.proactive_speech")));
    assert_eq!(
        generated["compatibility"]["readback_requirements"]["publication_state"],
        "applied"
    );

    let mode_ops = generated["package"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "mode")
        .unwrap()["operations"]
        .as_array()
        .unwrap();
    assert!(mode_ops
        .iter()
        .any(|op| op["path"] == "mode.duplex_requested.dot2-d" && op["value"] == "half"));
    assert!(mode_ops
        .iter()
        .any(|op| op["path"] == "mode.follow_window_ms" && op["value"] == 9000));
    assert!(mode_ops
        .iter()
        .any(|op| op["path"] == "mode.followup_credits" && op["value"].is_null()));
    assert!(mode_ops
        .iter()
        .any(|op| op["path"] == "mode.context_window_ms" && op["value"] == 240000));
    assert!(mode_ops
        .iter()
        .any(|op| op["path"] == "mode.follow_window_tone" && op["value"] == false));
    assert!(mode_ops
        .iter()
        .any(|op| op["path"] == "mode.filler.delay_ms" && op["value"] == 1500));

    let led_ops = generated["package"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "device_led_configs")
        .unwrap()["operations"]
        .as_array()
        .unwrap();
    assert!(led_ops.iter().any(
        |op| op["path"] == "device_led_configs.dot2-d.night_start_minute" && op["value"] == 1320
    ));
    assert!(led_ops.iter().any(
        |op| op["path"] == "device_led_configs.dot2-d.night_end_minute" && op["value"] == 420
    ));
    assert!(led_ops.iter().any(|op| op["path"]
        == "device_led_configs.dot2-d.orientation_rotation"
        && op["value"] == 6));
    assert!(led_ops.iter().any(
        |op| op["path"] == "device_led_configs.dot2-d.orientation_reverse" && op["value"] == true
    ));

    let applied = core
        .json(
            "/api/v1/config/applications",
            submission(&generated["preflight"]),
        )
        .await;
    let app_id = applied["application_id"].as_str().unwrap();
    let rb = core.published(app_id).await;
    assert_eq!(rb["publication"]["state"], "applied");
    assert_ne!(core.state.agent_md.current_content(), "ORIGINAL_PERSONA");
    assert_eq!(
        core.store.secret_get("llm.api_key").unwrap().as_deref(),
        Some("synthetic-target-credential")
    );
    assert_eq!(core.state.user_md.list(Some("active")).unwrap().len(), 1);

    // Other nodes cannot generate or apply dot2-d's family profile.
    for other_node in ["dot2-a", "dot2-b", "dot2-c", "dot2-e"] {
        let other = Core::start(other_node, "http://127.0.0.1:1/synthetic", true).await;
        assert_eq!(
            other
                .post("/api/v1/config/profiles/dot2-d-family", json!({}))
                .await
                .json::<Value>()
                .await
                .unwrap()["error"],
            "PROFILE_TARGET_MISMATCH"
        );
        assert_eq!(
            other
                .post(
                    "/api/v1/config/preflight",
                    json!({"package": generated["package"]})
                )
                .await
                .json::<Value>()
                .await
                .unwrap()["error"],
            "TARGET_MISMATCH"
        );
    }

    // Rollback preflight -> commit restores previous persona and LED state.
    let state_after = core
        .client
        .get(format!("{}/api/v1/config/state", core.base))
        .bearer_auth(&core.token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let rb_resp = core
        .post(
            &format!("/api/v1/config/applications/{app_id}/rollback-preflight"),
            json!({"expected_revision": state_after["config_revision"]}),
        )
        .await;
    let rb_status = rb_resp.status();
    let rb_pf: Value = rb_resp.json().await.unwrap();
    assert!(
        rb_status.is_success(),
        "rollback-preflight failed ({rb_status}): {rb_pf:#}"
    );
    let rb_commit = core
        .json("/api/v1/config/applications", submission(&rb_pf))
        .await;
    let rb_done = core
        .published(rb_commit["application_id"].as_str().unwrap())
        .await;
    assert_eq!(rb_done["publication"]["state"], "applied");
    assert_eq!(core.state.agent_md.current_content(), "ORIGINAL_PERSONA");
    // Verify CFG-09: learner progress & personal data remain untouched after rollback.
    let learner_entries = core.state.user_md.list(Some("active")).unwrap();
    assert_eq!(learner_entries.len(), 1);
    assert_eq!(learner_entries[0].person_id, "learner_child_1");
    assert_eq!(learner_entries[0].content, "KET-Unit-4-Score-92");
    let learner_msgs = sessions.messages("learning:child_1").unwrap();
    assert_eq!(learner_msgs.len(), 1);
    assert_eq!(
        learner_msgs[0]["content"].as_str(),
        Some("apple 拼写已掌握")
    );
    if let Ok(directory) = std::env::var("HM_TEST_CONFIG_EVIDENCE_DIR") {
        let path = std::path::Path::new(&directory);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("luna-dot2d-family-v1.json"),
            serde_json::to_vec_pretty(&generated["package"]).unwrap(),
        )
        .unwrap();
        std::fs::write(
            path.join("generated-family-profile-review.json"),
            serde_json::to_vec_pretty(&generated).unwrap(),
        )
        .unwrap();
    }
}
