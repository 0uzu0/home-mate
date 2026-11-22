//! R2-BM5: the Home Assistant adapter and the answer contract, at the HTTP
//! boundary. The fake HA is a real protocol partner: it checks the bearer
//! token, counts every request and can refuse to apply a change — the only
//! way to prove "service accepted ≠ confirmed" and "unconfigured = 0 HTTP".

use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::config::{HomeBackend, HomeConfig, HomeEntity};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// --- fake Home Assistant ----------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum HaMode {
    Apply,
    Ignore,
    Slow,
    Unauthorized,
    /// POST succeeds; the read-back GET returns 401. The write may have
    /// happened — the outcome must stay Unknown, not Failed.
    ReadBackAuth,
}

#[derive(Clone)]
struct HaState {
    hits: Arc<Mutex<Vec<String>>>,
    entity_state: Arc<Mutex<String>>,
    mode: HaMode,
    token: String,
    hold_post: Option<Arc<HeldHomePost>>,
}

#[derive(Default)]
struct HeldHomePost {
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct FakeHa {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
    entity_state: Arc<Mutex<String>>,
}

impl FakeHa {
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    fn state(&self) -> String {
        self.entity_state.lock().unwrap().clone()
    }
}

async fn spawn_fake_ha(mode: HaMode) -> FakeHa {
    spawn_ha(mode, "ha-test-token", None).await
}

async fn spawn_ha(mode: HaMode, token: &str, hold_post: Option<Arc<HeldHomePost>>) -> FakeHa {
    let state = HaState {
        hits: Arc::new(Mutex::new(Vec::new())),
        entity_state: Arc::new(Mutex::new("on".into())),
        mode,
        token: token.into(),
        hold_post,
    };
    let hits = state.hits.clone();
    let entity_state = state.entity_state.clone();

    async fn authorized(s: &HaState, headers: &axum::http::HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == format!("Bearer {}", s.token))
            .unwrap_or(false)
    }

    async fn root(
        axum::extract::State(s): axum::extract::State<HaState>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        s.hits.lock().unwrap().push("GET:api".into());
        if !authorized(&s, &headers).await {
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
        "API running.".into_response()
    }

    async fn state_get(
        axum::extract::State(s): axum::extract::State<HaState>,
        axum::extract::Path(entity): axum::extract::Path<String>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        s.hits.lock().unwrap().push(format!("GET:state:{entity}"));
        if s.mode == HaMode::ReadBackAuth || !authorized(&s, &headers).await {
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
        let st = s.entity_state.lock().unwrap().clone();
        axum::Json(json!({
            "entity_id": entity,
            "state": st,
            "last_changed": "2026-11-13T00:00:00+00:00",
        }))
        .into_response()
    }

    async fn service_call(
        axum::extract::State(s): axum::extract::State<HaState>,
        axum::extract::Path((domain, service)): axum::extract::Path<(String, String)>,
        headers: axum::http::HeaderMap,
        axum::Json(_body): axum::Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        s.hits
            .lock()
            .unwrap()
            .push(format!("POST:{domain}.{service}"));
        if s.mode == HaMode::Unauthorized || !authorized(&s, &headers).await {
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
        if s.mode == HaMode::Slow {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return axum::Json(json!([])).into_response();
        }
        if s.mode == HaMode::Apply {
            let mut st = s.entity_state.lock().unwrap();
            *st = match service.as_str() {
                "turn_on" => "on",
                "turn_off" => "off",
                "open_cover" => "open",
                "close_cover" => "closed",
                _ => st.as_str(),
            }
            .to_string();
        }
        if let Some(gate) = &s.hold_post {
            gate.arrived.notify_one();
            gate.release.notified().await;
        }
        axum::Json(json!([])).into_response()
    }

    /// R2-BM6.1: the house catalogue. The fake is a real protocol partner:
    /// bearer-checked, request-counted, with one entity per interesting domain.
    async fn states_list(
        axum::extract::State(s): axum::extract::State<HaState>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        s.hits.lock().unwrap().push("GET:states".into());
        if s.mode == HaMode::ReadBackAuth || !authorized(&s, &headers).await {
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
        axum::Json(json!([
            { "entity_id": "light.children", "state": "on",
              "attributes": { "friendly_name": "儿童房的灯" } },
            { "entity_id": "switch.coffee", "state": "off",
              "attributes": { "friendly_name": "咖啡机" } },
            { "entity_id": "cover.curtain", "state": "closed",
              "attributes": { "friendly_name": "客厅窗帘" } },
            { "entity_id": "sensor.temperature", "state": "22.4",
              "attributes": { "friendly_name": "客厅温度" } },
            { "entity_id": "lock.front_door", "state": "locked",
              "attributes": { "friendly_name": "大门锁" } },
            { "entity_id": "scene.movie", "state": "scening",
              "attributes": { "friendly_name": "<b>观影</b>" } }
        ]))
        .into_response()
    }

    let app = axum::Router::new()
        .route("/api/", axum::routing::get(root))
        .route("/api/states", axum::routing::get(states_list))
        .route("/api/states/{entity}", axum::routing::get(state_get))
        .route(
            "/api/services/{domain}/{service}",
            axum::routing::post(service_call),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeHa {
        base: format!("http://{addr}"),
        hits,
        entity_state,
    }
}

#[tokio::test]
async fn home_write_and_readback_keep_the_same_token_during_a_local_edit() {
    use hm_core::tool::{home::HomeSetTool, Evidence, Tool, ToolCtx, ToolStatus};
    use std::{sync::atomic::AtomicBool, time::Duration};

    let gate = Arc::new(HeldHomePost::default());
    let old = spawn_ha(HaMode::Apply, "ha-test-token", Some(gate.clone())).await;
    let new = spawn_ha(HaMode::Apply, "ha-next-test-token", None).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&state, &old, Some("ha-test-token"), Value::Null).await;
    let tool = HomeSetTool::new(state.home.clone());
    let action = tokio::spawn(async move {
        let ctx = ToolCtx::new("home-held", "home-held", Arc::new(AtomicBool::new(false)));
        tool.execute(json!({"entity":"儿童房的灯","state":"off"}), &ctx)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), gate.arrived.notified())
        .await
        .unwrap();
    let client = reqwest::Client::new();
    client
        .put(format!("{base}/api/v0/config/services/home"))
        .json(&json!({"base_url":new.base,"token":"ha-next-test-token"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    gate.release.notify_one();
    let result = action.await.unwrap();
    assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
    assert_eq!(result.evidence, Evidence::DeviceConfirmed);
    assert!(old
        .hits()
        .iter()
        .any(|hit| hit == "GET:state:light.children"));
    let probe: Value = client
        .post(format!("{base}/api/v0/config/services/home/test"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["ok"], true, "{probe}");
    assert!(new
        .hits()
        .iter()
        .any(|hit| hit == "GET:state:light.children"));
}

// --- app under test ---------------------------------------------------------

async fn spawn_app(llm: FakeLlm) -> (String, Arc<AppState>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::new(
        store,
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(llm)),
    );
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

#[tokio::test]
async fn reviewed_home_publication_and_rollback_reach_the_expected_provider() {
    use hm_core::config_delivery::{uuid, ConfigWorker};
    use hm_core::tool::{home::HomeSetTool, Evidence, Tool, ToolCtx, ToolStatus};
    use std::{sync::atomic::AtomicBool, time::Duration};

    async fn response(request: reqwest::RequestBuilder) -> Value {
        let res = request.send().await.unwrap();
        let status = res.status();
        let result = res.json::<Value>().await.unwrap();
        assert!(status.is_success(), "{status}: {result}");
        result
    }

    let old = spawn_fake_ha(HaMode::Apply).await;
    let new = spawn_fake_ha(HaMode::Apply).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let client = reqwest::Client::new();
    let pairing = response(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let token = pairing["token"].as_str().unwrap();
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(token);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(token);
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(token);
    let preflight = |home: Value| {
        post("/api/v1/config/preflight").json(&json!({
            "package": {"format":"home-mate-config","version":1,"home":home}
        }))
    };
    let apply = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"],"application_id":uuid().unwrap()
        }))
    };
    let readback = |id: &str| get(&format!("/api/v1/config/applications/{id}/readback"));
    let publish = async |id: &str| {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let proof = response(readback(id)).await;
                let publication = &proof["publications"][0];
                assert_eq!(publication["block"], "home");
                if publication["state"] == "applied" {
                    assert!(!proof.to_string().contains("ha-test-token"));
                    assert!(
                        proof["fields"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|row| row["state"] == "core_readback_verified"),
                        "{proof}"
                    );
                    break;
                }
                assert_eq!(publication["state"], "saved_pending", "{proof}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    };
    let home = "/api/v0/config/services/home";
    response(put("/api/v0/fleet").json(&json!({"self_id":"home-fixture"}))).await;
    response(put("/api/v1/config/enrollment").json(&json!({}))).await;
    let mut family = response(post("/api/v1/config/exports").json(&json!({
        "format":"v2", "purpose":"family_policy", "blocks":["home"]
    })))
    .await["package"]
        .clone();
    family.as_object_mut().unwrap().remove("content_hash");
    for operation in family["blocks"][0]["operations"].as_array_mut().unwrap() {
        match operation["path"].as_str().unwrap() {
            "home.base_url" => operation["value"] = json!(old.base),
            "home.backend" => operation["value"] = json!("home_assistant"),
            "home.entities" => operation["value"] = json!([{"name":"旧灯","entity":"light.old"}]),
            _ => {}
        }
    }
    let family_plan =
        response(post("/api/v1/config/preflight").json(&json!({"package":family}))).await;
    let family_receipt = response(apply(&family_plan)).await;
    {
        let _worker = ConfigWorker::start(state.store.clone());
        publish(family_receipt["application_id"].as_str().unwrap()).await;
    }
    response(put(home).json(&json!({"token":"ha-test-token"}))).await;
    let layers = response(get("/api/v1/config/state")).await;
    assert!(layers["layers"]["node"].get("home.entities").is_none());

    for (format, result_field) in [("v1", "document"), ("v2", "package")] {
        let export = response(
            post("/api/v1/config/exports").json(&json!({"format":format,"blocks":["home"]})),
        )
        .await;
        assert!(!export.to_string().contains("ha-test-token"));
        response(post("/api/v1/config/preflight").json(&json!({"package":export[result_field]})))
            .await;
    }
    let omitted = preflight(json!({"entity_source":"exposed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(omitted.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(omitted
        .text()
        .await
        .unwrap()
        .contains("HOME_DIRECTORY_REPLACEMENT_REQUIRED"));
    let forbidden = preflight(json!({
        "base_url":new.base, "entities":[{"name":"excluded","entity":"lock.fixture"}]
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(forbidden.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(
        old.hits().is_empty() && new.hits().is_empty(),
        "preflight never contacts a provider"
    );

    let plan = response(preflight(json!({
        "base_url":new.base, "entity_source":"exposed",
        "entities":[{"name":"新灯","entity":"light.new"}], "timeout_ms":2000, "retries":0
    })))
    .await;
    let receipt = response(apply(&plan)).await;
    assert_eq!(receipt["state"], "saved_pending");
    let id = receipt["application_id"].as_str().unwrap();
    let pending = response(readback(id)).await;
    assert_eq!(pending["publications"][0]["state"], "saved_pending");
    let premature_reverse = preflight(json!({"entity_source":"api"}))
        .send()
        .await
        .unwrap();
    assert_eq!(premature_reverse.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(premature_reverse
        .text()
        .await
        .unwrap()
        .contains("HOME_DIRECTORY_REPLACEMENT_REQUIRED"));
    // An unrelated review preserves the already approved pending source/list.
    response(preflight(json!({"timeout_ms":2500}))).await;

    let tool = HomeSetTool::new(state.home.clone());
    let confirmed = async |name: &str, entity: &str| {
        let ctx = ToolCtx::new(
            "home-config-test",
            "home-config-test",
            Arc::new(AtomicBool::new(false)),
        );
        let result = tool
            .execute(json!({"entity":name,"state":"off"}), &ctx)
            .await;
        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        assert_eq!(result.evidence, Evidence::DeviceConfirmed);
        assert_eq!(result.result.unwrap()["readback"]["entity"], entity);
    };
    confirmed("旧灯", "light.old").await;
    assert!(new.hits().is_empty());
    let _worker = ConfigWorker::start(state.store.clone());
    publish(id).await;
    let old_hits = old.hits().len();
    confirmed("新灯", "light.new").await;
    let probed = response(post("/api/v0/config/services/home/test")).await;
    assert_eq!(probed["entity"]["entity"], "light.new");
    assert_eq!(old.hits().len(), old_hits);
    assert!(new.hits().iter().any(|hit| hit == "GET:state:light.new"));

    let mut shadowed = family.clone();
    shadowed["package_id"] = json!(uuid().unwrap());
    shadowed["blocks"][0]["operations"] = json!([
        {"action":"set","path":"home.entity_source","value":"exposed"}
    ]);
    let hidden_mismatch = post("/api/v1/config/preflight")
        .json(&json!({"package":shadowed}))
        .send()
        .await
        .unwrap();
    assert_eq!(hidden_mismatch.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(hidden_mismatch
        .text()
        .await
        .unwrap()
        .contains("HOME_DIRECTORY_REPLACEMENT_REQUIRED"));

    let rollback = response(
        post(&format!(
            "/api/v1/config/applications/{id}/rollback-preflight"
        ))
        .json(&json!({"expected_revision": receipt["config_revision"]})),
    )
    .await;
    assert!(rollback["diff"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| { row["path"] == "home.entities" && row["action"] == "clear" }));
    let restored = response(apply(&rollback)).await;
    publish(restored["application_id"].as_str().unwrap()).await;
    let status = response(get(home)).await;
    assert_eq!(status["entity_source"], "api");
    assert_eq!(status["base_url"], old.base);
    assert_eq!(status["token_set"], true);
    confirmed("旧灯", "light.old").await;
    assert!(old.hits().len() > old_hits);

    state.store.with(|c| {
        c.execute_batch("CREATE TRIGGER reject_home_save BEFORE UPDATE ON config_kv WHEN OLD.key='home' BEGIN SELECT RAISE(ABORT, 'test home failure'); END")?;
        Ok(())
    }).unwrap();
    let failed = put(home)
        .json(&json!({"base_url":new.base,"token":"another-test-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let plan = response(preflight(json!({"base_url":new.base}))).await;
    let failed_application = response(apply(&plan)).await;
    let failed_id = failed_application["application_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let proof = response(readback(failed_id)).await;
            if proof["publications"][0]["state"] == "failed" {
                assert_eq!(proof["publications"][0]["error"], "HOME_PUBLICATION_FAILED");
                assert!(proof["fields"].as_array().unwrap().iter().any(|row| {
                    row["path"] == "home.base_url"
                        && row["state"] == "failed"
                        && row["value"] == old.base
                }));
                break;
            }
            assert_eq!(
                proof["publications"][0]["state"], "saved_pending",
                "{proof}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    confirmed("旧灯", "light.old").await;

    state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER reject_home_save")?;
            Ok(())
        })
        .unwrap();
    let mut exposed_family = family;
    exposed_family["package_id"] = json!(uuid().unwrap());
    exposed_family["blocks"][0]["operations"] = json!([
        {"action":"set","path":"home.entity_source","value":"exposed"},
        {"action":"replace","path":"home.entities","value":[{"name":"家庭灯","entity":"light.family"}]}
    ]);
    let plan =
        response(post("/api/v1/config/preflight").json(&json!({"package":exposed_family}))).await;
    let receipt = response(apply(&plan)).await;
    publish(receipt["application_id"].as_str().unwrap()).await;
    let plan = response(preflight(
        json!({"entities":[{"name":"本机灯","entity":"light.node"}]}),
    ))
    .await;
    let receipt = response(apply(&plan)).await;
    publish(receipt["application_id"].as_str().unwrap()).await;
    let mut family_clear = exposed_family;
    family_clear["package_id"] = json!(uuid().unwrap());
    family_clear["blocks"][0]["operations"] = json!([
        {"action":"clear","path":"home.entity_source"},
        {"action":"clear","path":"home.entities"}
    ]);
    let rejected = post("/api/v1/config/preflight")
        .json(&json!({"package":family_clear}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(rejected
        .text()
        .await
        .unwrap()
        .contains("HOME_DIRECTORY_REPLACEMENT_REQUIRED"));

    let marker = "fixture-inline-credential";
    let mut legacy = state.config.home().unwrap();
    legacy.base_url = format!("{}?token={marker}", old.base);
    state
        .store
        .config_set("home", &serde_json::to_string(&legacy).unwrap())
        .unwrap();
    hm_core::config::append_history(
        &state.store,
        "legacy-fixture",
        "home",
        "configured Home",
        &json!({"base_url":legacy.base_url}),
    )
    .unwrap();
    let history = response(get("/api/v0/config/history")).await;
    assert!(!history.to_string().contains(marker));
    assert!(history["history"].as_array().unwrap().iter().any(|entry| {
        entry["actor"] == "legacy-fixture" && entry["payload"]["base_url"].is_null()
    }));
    let old_calls = old.hits().len();
    let unsafe_probe = response(post("/api/v0/config/services/home/test")).await;
    assert_eq!(unsafe_probe["http_requests"], 0);
    assert_eq!(old.hits().len(), old_calls);
    assert!(!response(get(home)).await.to_string().contains(marker));
    let bad_baseline = preflight(json!({"timeout_ms":1500})).send().await.unwrap();
    assert_eq!(bad_baseline.status(), reqwest::StatusCode::CONFLICT);
    for format in ["v1", "v2"] {
        let report = response(post("/api/v1/config/exports").json(&json!({"format":format}))).await;
        assert!(report["excluded"]
            .as_array()
            .unwrap()
            .contains(&json!("home_unavailable")));
        assert!(!report.to_string().contains(marker));
    }
}

async fn sse_events(resp: reqwest::Response) -> Vec<Value> {
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim();
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    events.push(v);
                }
            }
        }
    }
    events
}

/// Everything the speaker is told, in order: the core's own sentences
/// (`fact`) and the model's prose (`token`). R2 split the fact onto its own
/// event so the voice layer can exempt it from the spoken budget.
fn spoken(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "token" || e["type"] == "fact")
        .filter_map(|e| e["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn answer(events: &[Value]) -> String {
    events
        .iter()
        .find(|e| e["type"] == "answer")
        .and_then(|e| e["text"].as_str())
        .unwrap_or("")
        .to_string()
}

async fn configure(state: &AppState, ha: &FakeHa, token: Option<&str>, extra: Value) {
    let mut cfg = HomeConfig {
        backend: HomeBackend::HomeAssistant,
        base_url: ha.base.clone(),
        entities: vec![HomeEntity {
            name: "儿童房的灯".into(),
            entity: "light.children".into(),
        }],
        timeout_ms: 3_000,
        retries: 0,
        entity_source: hm_core::config::EntitySource::Api,
    };
    if let Some(v) = extra.get("timeout_ms").and_then(Value::as_u64) {
        cfg.timeout_ms = v;
    }
    state.config.save_home(cfg, "test").unwrap();
    state.config.save_home_token(token).unwrap();
}

async fn trace_events(base: &str, events: &[Value]) -> Vec<Value> {
    let trace_id = events
        .iter()
        .find(|e| e["type"] == "turn_end")
        .and_then(|e| e["trace_id"].as_str())
        .unwrap()
        .to_string();
    let detail: Value = reqwest::get(format!("{base}/api/v0/trace/{trace_id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    detail["events"].as_array().cloned().unwrap_or_default()
}

// --- BM5-1: unconfigured = zero HTTP ---------------------------------------

#[tokio::test]
async fn an_unconfigured_home_makes_zero_http_requests() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"儿童房的灯","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["还没有接入，我没有办法关灯。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    // With no backend: the fake address must not even be touched.
    state
        .config
        .save_home(
            HomeConfig {
                backend: HomeBackend::None,
                base_url: ha.base.clone(),
                ..Default::default()
            },
            "test",
        )
        .unwrap();
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        ha.hits().is_empty(),
        "unconfigured must be zero HTTP: {:?}",
        ha.hits()
    );
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "failed");
    assert!(result["summary"]
        .as_str()
        .unwrap()
        .contains("HOME_UNCONFIGURED"));
    assert!(!spoken(&events).contains("已关闭"), "{}", spoken(&events));

    // The probe is honest too: 0 requests, state word unconfigured.
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/home/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "unconfigured");
    assert_eq!(probe["http_requests"], 0);
    assert!(ha.hits().is_empty());
}

// --- BM5-2: read-back is the only confirmation ----------------------------

#[tokio::test]
async fn a_matching_readback_confirms_and_the_core_speaks_the_action() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"儿童房的灯","state":"off"}"#.into(),
            )],
        },
        // A model that tries to restate the fact: the core must drop it.
        Scripted::Text {
            chunks: vec!["好的，已经关好了。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    configure(&state, &ha, Some("ha-test-token"), json!({})).await;

    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["evidence"], "deviceconfirmed");
    assert!(
        ha.hits().iter().any(|h| h == "POST:light.turn_off"),
        "{:?}",
        ha.hits()
    );
    assert!(
        ha.hits().iter().any(|h| h == "GET:state:light.children"),
        "{:?}",
        ha.hits()
    );
    assert_eq!(ha.state(), "off");

    let said = spoken(&events);
    assert!(
        said.contains("儿童房的灯已关闭。"),
        "the core sentence must be spoken: {said}"
    );
    assert!(
        !said.contains("已经关好了"),
        "the model's claim must not be spoken: {said}"
    );
    assert_eq!(answer(&events), "儿童房的灯已关闭。");

    let trace = trace_events(&base, &events).await;
    let rendered = trace
        .iter()
        .find(|e| e["kind"] == "answer_rendered")
        .unwrap();
    assert_eq!(rendered["payload"]["source"], "tool");
    assert_eq!(rendered["payload"]["confirmed"], true);
    assert_eq!(rendered["payload"]["suppressed_claims"], 1);
    assert!(trace
        .iter()
        .any(|e| e["kind"] == "fact_rendered" && e["payload"]["confirmed"] == true));
}

// --- BM5-3: accepted but not observed = Unknown ----------------------------

#[tokio::test]
async fn a_service_accept_without_a_matching_readback_is_unknown() {
    let ha = spawn_fake_ha(HaMode::Ignore).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"儿童房的灯","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经关好了。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    configure(&state, &ha, Some("ha-test-token"), json!({})).await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "unknown", "{result}");
    assert_eq!(result["evidence"], "unknown");
    assert!(
        result["summary"]
            .as_str()
            .unwrap()
            .contains("HOME_READBACK_MISMATCH"),
        "{result}"
    );
    let said = spoken(&events);
    assert!(said.contains("没能确认儿童房的灯是否关闭。"), "{said}");
    assert!(!said.contains("已关闭"), "{said}");
    let trace = trace_events(&base, &events).await;
    let rendered = trace
        .iter()
        .find(|e| e["kind"] == "answer_rendered")
        .unwrap();
    assert_eq!(rendered["payload"]["source"], "tool");
    assert_eq!(rendered["payload"]["confirmed"], false);
}

// --- BM5-3b: a timeout is Unknown, not success ----------------------------

#[tokio::test]
async fn a_timeout_is_unknown_and_says_so() {
    let ha = spawn_fake_ha(HaMode::Slow).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"儿童房的灯","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经关好了。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    configure(
        &state,
        &ha,
        Some("ha-test-token"),
        json!({ "timeout_ms": 500 }),
    )
    .await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "unknown", "{result}");
    assert!(
        result["summary"].as_str().unwrap().contains("HOME_TIMEOUT"),
        "{result}"
    );
    let said = spoken(&events);
    assert!(said.contains("没能确认"), "{said}");
    assert!(!said.contains("已关闭"), "{said}");
}

// --- BM5-4: a claim without a receipt is refused ---------------------------

#[tokio::test]
async fn a_model_claim_without_a_receipt_is_refused() {
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["好的，灯已经关好了。".into()],
    }]);
    let (base, state) = spawn_app(llm).await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let said = spoken(&events);
    assert!(
        said.contains("我没有执行这个操作，也不能确认结果。"),
        "{said}"
    );
    assert!(!said.contains("已经关好了"), "{said}");
    let trace = trace_events(&base, &events).await;
    let rendered = trace
        .iter()
        .find(|e| e["kind"] == "answer_rendered")
        .unwrap();
    assert_eq!(rendered["payload"]["source"], "refusal");
    assert_eq!(rendered["payload"]["reason"], "unsupported_action_claim");
}

// --- the whitelist is the only vocabulary ----------------------------------

#[tokio::test]
async fn an_invented_entity_never_reaches_the_backend() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"children_room.light","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经关好了。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    configure(&state, &ha, Some("ha-test-token"), json!({})).await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "failed");
    assert!(result["summary"]
        .as_str()
        .unwrap()
        .contains("HOME_ENTITY_NOT_ALLOWED"));
    assert!(
        ha.hits().is_empty(),
        "whitelist refusal must be zero HTTP: {:?}",
        ha.hits()
    );
    assert!(!spoken(&events).contains("已关闭"));
}

// --- BM5-7: probe + token handling ----------------------------------------

#[tokio::test]
async fn the_probe_reads_one_entity_and_never_echoes_the_token() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let llm = FakeLlm::new(vec![]);
    let (base, state) = spawn_app(llm).await;
    configure(&state, &ha, Some("ha-test-token"), json!({})).await;
    let client = reqwest::Client::new();

    let probe: Value = client
        .post(format!("{base}/api/v0/config/services/home/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "ready", "{probe}");
    assert_eq!(probe["entity"]["state"], "on");
    assert_eq!(probe["http_requests"], 2);
    assert_eq!(probe["token_set"], true);
    assert!(!probe.to_string().contains("ha-test-token"), "{probe}");

    let cfg: Value = client
        .get(format!("{base}/api/v0/config/services/home"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["state"], "ready");
    assert_eq!(cfg["token_set"], true);
    assert!(!cfg.to_string().contains("ha-test-token"), "{cfg}");
    // The token is never in a plain export either.
    let export: Value = client
        .get(format!("{base}/api/v0/config/export"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!export.to_string().contains("ha-test-token"), "{export}");
    assert_eq!(export["home"]["backend"], "home_assistant");
    assert_eq!(export["home"]["token_set"], true);
    // The EXPLICIT backup is the one place the value may appear (R1f: a local
    // backup must be restorable; secrets are included only when asked).
    let backup: Value = client
        .post(format!("{base}/api/v0/config/export?secrets=1"))
        .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(backup["secrets"]["home.token"], "ha-test-token", "{backup}");
}

#[tokio::test]
async fn a_wrong_token_is_a_named_failure_and_the_claim_stays_honest() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"儿童房的灯","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经关好了。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    configure(&state, &ha, Some("wrong-token"), json!({})).await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "failed");
    assert!(
        result["summary"].as_str().unwrap().contains("HOME_AUTH"),
        "{result}"
    );
    assert!(!spoken(&events).contains("已关闭"));
}

// --- audit Q2: a failed OBSERVATION after an accepted write is unknown ----

#[tokio::test]
async fn a_readback_failure_after_an_accepted_write_is_unknown_not_failed() {
    let ha = spawn_fake_ha(HaMode::ReadBackAuth).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"儿童房的灯","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["已经关好了。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    configure(&state, &ha, Some("ha-test-token"), json!({})).await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关闭儿童房的灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    // The POST was accepted; the GET cannot observe. That is an unknown
    // ACTION outcome — never "failed" (which would deny a write that may
    // have happened) and never success.
    assert_eq!(result["status"], "unknown", "{result}");
    assert_eq!(result["evidence"], "unknown");
    assert!(
        result["summary"]
            .as_str()
            .unwrap()
            .contains("HOME_READBACK_UNOBSERVED"),
        "{result}"
    );
    assert!(
        ha.hits().iter().any(|h| h == "POST:light.turn_off"),
        "{:?}",
        ha.hits()
    );
    let said = spoken(&events);
    assert!(said.contains("没能确认儿童房的灯是否关闭。"), "{said}");
    assert!(!said.contains("已关闭"), "{said}");
}

// --- BM5-5: call cap keeps history well formed ----------------------------

#[tokio::test]
async fn more_tool_calls_than_the_cap_stay_well_formed() {
    let six: Vec<(String, String)> = (0..6)
        .map(|i| {
            (
                "home.set".to_string(),
                format!(r#"{{"entity":"e{i}","state":"off"}}"#),
            )
        })
        .collect();
    let llm = FakeLlm::new(vec![
        Scripted::Calls { calls: six },
        Scripted::Text {
            chunks: vec!["好的。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关一堆灯" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        events.iter().filter(|e| e["type"] == "tool_start").count(),
        4
    );
    let sid = events[0]["session_id"].as_str().unwrap();
    let hist = state.sessions.history(sid, 50).unwrap();
    let calls: Vec<&str> = hist
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(|v| v.iter())
        .map(|c| c.id.as_str())
        .collect();
    let results: Vec<&str> = hist
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert_eq!(
        calls.len(),
        4,
        "the assistant turn must persist exactly the executed calls"
    );
    assert_eq!(results.len(), 4, "and one tool row per call");
    for id in &calls {
        assert!(
            results.contains(id),
            "dangling tool_call_id {id}: {calls:?} / {results:?}"
        );
    }
    // No assistant prose masquerading as a tool receipt.
    assert!(
        !hist
            .iter()
            .any(|m| m.role == "assistant" && m.content.as_deref().unwrap_or("").contains("[回执")),
        "receipts must be role=tool"
    );
}

// --- R2-BM6.1: entity discovery and the wider controllable domains ---------

#[tokio::test]
async fn discovery_lists_the_house_and_marks_unsupported_domains() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&state, &ha, Some("ha-test-token"), json!({})).await;

    let v: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/home/discover"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["http_requests"], 1, "{v}");
    let rows = v["entities"].as_array().unwrap();
    let by_id = |id: &str| rows.iter().find(|e| e["entity"] == json!(id)).unwrap();
    assert_eq!(by_id("light.children")["name"], "儿童房的灯");
    assert_eq!(by_id("light.children")["supported"], true);
    assert_eq!(by_id("switch.coffee")["supported"], true);
    assert_eq!(by_id("cover.curtain")["supported"], true);
    assert_eq!(by_id("sensor.temperature")["supported"], false);
    assert_eq!(by_id("lock.front_door")["supported"], false);
    // The name is cleaned: HA's markup does not reach the card.
    assert_eq!(by_id("scene.movie")["name"], "观影");
    assert_eq!(v["domains"]["light"], 1);
    assert_eq!(v["supported"], 3);
    assert!(
        ha.hits().iter().any(|h| h == "GET:states"),
        "{:?}",
        ha.hits()
    );

    // Unconfigured = zero requests.
    ha.hits.lock().unwrap().clear();
    state
        .config
        .save_home(
            HomeConfig {
                backend: HomeBackend::None,
                base_url: ha.base.clone(),
                ..Default::default()
            },
            "test",
        )
        .unwrap();
    let v: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/home/discover"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["state"], "unconfigured", "{v}");
    assert_eq!(v["http_requests"], 0);
    assert!(ha.hits().is_empty(), "{:?}", ha.hits());
}

#[tokio::test]
async fn discovery_needs_a_connection_not_a_whitelist() {
    // The chicken-and-egg guard: an empty whitelist is WHY the household
    // discovers, so it must not turn discovery into "unconfigured".
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_home(
            HomeConfig {
                backend: HomeBackend::HomeAssistant,
                base_url: ha.base.clone(),
                entities: vec![],
                timeout_ms: 2000,
                retries: 0,
                entity_source: hm_core::config::EntitySource::Api,
            },
            "test",
        )
        .unwrap();
    state.config.save_home_token(Some("ha-test-token")).unwrap();
    let v: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/home/discover"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["http_requests"], 1);
    assert!(!v["entities"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_lock_entity_cannot_enter_the_whitelist() {
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let bad = json!({
        "backend": "home_assistant",
        "base_url": "http://127.0.0.1:1",
        "entities": [{ "name": "大门锁", "entity": "lock.front_door" }]
    });
    let resp: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/home"))
        .json(&bad)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["error"].as_str().unwrap().contains("可控域"), "{resp}");
    assert!(state.config.home().unwrap().entities.is_empty());
}

#[tokio::test]
async fn a_switch_and_a_cover_are_controlled_with_a_readback() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let llm = FakeLlm::new(vec![
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"咖啡机","state":"off"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["好。".into()],
        },
        Scripted::Calls {
            calls: vec![(
                "home.set".into(),
                r#"{"entity":"客厅窗帘","state":"open"}"#.into(),
            )],
        },
        Scripted::Text {
            chunks: vec!["好。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    state
        .config
        .save_home(
            HomeConfig {
                backend: HomeBackend::HomeAssistant,
                base_url: ha.base.clone(),
                entities: vec![
                    HomeEntity {
                        name: "咖啡机".into(),
                        entity: "switch.coffee".into(),
                    },
                    HomeEntity {
                        name: "客厅窗帘".into(),
                        entity: "cover.curtain".into(),
                    },
                ],
                timeout_ms: 2000,
                retries: 0,
                entity_source: hm_core::config::EntitySource::Api,
            },
            "test",
        )
        .unwrap();
    state.config.save_home_token(Some("ha-test-token")).unwrap();

    // switch off
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "关掉咖啡机" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let r = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(r["status"], "completed", "{r}");
    assert!(
        ha.hits().iter().any(|h| h == "POST:switch.turn_off"),
        "{:?}",
        ha.hits()
    );
    assert_eq!(ha.state(), "off");

    // cover open: the service is open_cover, the read-back state is "open"
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": "打开客厅窗帘" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let r = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(r["status"], "completed", "{r}");
    assert!(
        ha.hits().iter().any(|h| h == "POST:cover.open_cover"),
        "{:?}",
        ha.hits()
    );
    assert_eq!(ha.state(), "open");
    assert!(
        spoken(&events).contains("客厅窗帘已打开"),
        "{}",
        spoken(&events)
    );
}

// --- R2-BM6.3: the two connection modes --------------------------------------

/// A Home Assistant that serves BOTH halves on one port: the WebSocket
/// `/api/websocket` handshake (auth → expose_entity/list) and one REST
/// `GET /api/states`. Decided by peeking the request head, so the WS handshake
/// bytes are untouched when the connection is upgraded.
struct FakeExposedHa {
    base: String,
    ws_commands: Arc<std::sync::atomic::AtomicUsize>,
    hits: Arc<Mutex<Vec<String>>>,
}

async fn spawn_fake_exposed_ha(auth_ok: bool) -> FakeExposedHa {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ws_commands = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ws_commands2 = ws_commands.clone();
    let hits2 = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let ws_commands = ws_commands2.clone();
            let hits = hits2.clone();
            tokio::spawn(async move {
                use futures::{SinkExt, StreamExt};
                use tokio_tungstenite::tungstenite::Message;
                let mut probe = [0u8; 4096];
                let n = sock.peek(&mut probe).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&probe[..n]).to_ascii_lowercase();
                if head.starts_with("get /api/websocket") {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else {
                        return;
                    };
                    let _ = ws
                        .send(Message::text(
                            json!({"type":"auth_required","ha_version":"2026.11"}).to_string(),
                        ))
                        .await;
                    let mut authed = false;
                    while let Some(Ok(msg)) = ws.next().await {
                        let Message::Text(t) = msg else { continue };
                        let v: Value = serde_json::from_str(&t).unwrap_or_default();
                        match v["type"].as_str().unwrap_or("") {
                            "auth" => {
                                if auth_ok && v["access_token"] == "ha-test-token" {
                                    authed = true;
                                    let _ = ws
                                        .send(Message::text(
                                            json!({"type":"auth_ok","ha_version":"2026.11"})
                                                .to_string(),
                                        ))
                                        .await;
                                } else {
                                    let _ = ws
                                        .send(Message::text(
                                            json!({"type":"auth_invalid","message":"bad token"})
                                                .to_string(),
                                        ))
                                        .await;
                                    break;
                                }
                            }
                            "homeassistant/expose_entity/list" if authed => {
                                ws_commands.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                let _ = ws
                                    .send(Message::text(
                                        json!({
                                            "id": v["id"],
                                            "type": "result",
                                            "success": true,
                                            "result": { "exposed_entities": {
                                                "light.children": { "conversation": true },
                                                "cover.curtain": { "conversation": true },
                                                "sensor.temperature": { "conversation": false },
                                                // Bare boolean shape, tolerated on purpose.
                                                "fan.desk": true
                                            }}
                                        })
                                        .to_string(),
                                    ))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                    return;
                }
                // Plain HTTP: read the request, answer /api/states.
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("GET /api/states ") {
                    hits.lock().unwrap().push("GET:states".into());
                    json!([
                        { "entity_id": "light.children", "state": "on",
                          "attributes": { "friendly_name": "儿童房的灯" } },
                        { "entity_id": "cover.curtain", "state": "closed",
                          "attributes": { "friendly_name": "客厅窗帘" } },
                        { "entity_id": "fan.desk", "state": "off",
                          "attributes": { "friendly_name": "书房风扇" } }
                    ])
                    .to_string()
                } else {
                    hits.lock().unwrap().push("GET:other".into());
                    "{}".into()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    FakeExposedHa {
        base: format!("http://{addr}"),
        ws_commands,
        hits,
    }
}

fn exposed_runtime(
    base: &str,
) -> (
    Arc<hm_core::tool::home::HomeRuntime>,
    Arc<hm_core::store::Store>,
) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let cfg = HomeConfig {
        backend: HomeBackend::HomeAssistant,
        entity_source: hm_core::config::EntitySource::Exposed,
        base_url: base.into(),
        ..Default::default()
    };
    hm_core::config::ConfigService::new(store.clone())
        .save_home(cfg, "test")
        .unwrap();
    hm_core::config::ConfigService::new(store.clone())
        .save_home_token(Some("ha-test-token"))
        .unwrap();
    (hm_core::tool::home::HomeRuntime::new(store.clone()), store)
}

/// Mode A: HA exposes the conversation set; only those entities appear, the
/// rest of the house does not, and names still come from the REST enrichment.
#[tokio::test]
async fn exposed_mode_reads_only_the_conversation_set() {
    let ha = spawn_fake_exposed_ha(true).await;
    let (rt, _store) = exposed_runtime(&ha.base);
    let d = rt.discover().await;
    assert_eq!(d["ok"], true, "{d}");
    assert_eq!(d["state"], "ready", "{d}");
    assert_eq!(d["entity_source"], "exposed", "{d}");
    let ids: Vec<String> = d["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["cover.curtain", "fan.desk", "light.children"],
        "{d}"
    );
    assert!(
        !ids.iter().any(|i| i == "sensor.temperature"),
        "conversation:false must not appear: {d}"
    );
    assert_eq!(
        d["supported"], 3,
        "all three exposed ones are controllable domains: {d}"
    );
    assert_eq!(d["ws_commands"], 1, "{d}");
    assert_eq!(
        ha.ws_commands.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the command was actually sent once"
    );
    assert!(ha.hits.lock().unwrap().iter().any(|h| h == "GET:states"));
    let light = d["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["entity"] == "light.children")
        .unwrap();
    assert_eq!(
        light["name"], "儿童房的灯",
        "the REST enrichment supplies the household name: {d}"
    );
}

/// Mode A failure is NAMED and never silently becomes mode B: an HA that has
/// no WebSocket endpoint must produce HOME_EXPOSURE_UNAVAILABLE with zero
/// `/api/states` requests (the REST server here would happily answer).
#[tokio::test]
async fn exposed_mode_failure_never_falls_back_to_the_states_api() {
    let ha = spawn_fake_ha(HaMode::Apply).await;
    let (rt, _store) = exposed_runtime(&ha.base);
    let d = rt.discover().await;
    assert_eq!(d["ok"], false, "{d}");
    assert_eq!(d["state"], "unavailable", "{d}");
    assert_eq!(d["code"], "HOME_EXPOSURE_UNAVAILABLE", "{d}");
    assert_eq!(d["http_requests"], 0, "no REST fallback: {d}");
    assert!(d["entities"].as_array().unwrap().is_empty(), "{d}");
    assert!(
        ha.hits().is_empty(),
        "the /api/states endpoint must not be touched: {:?}",
        ha.hits()
    );
}

/// The A/B switch drops the old directory: what the other mechanism found is
/// not a valid half-configuration of this one.
#[test]
fn switching_the_entity_source_clears_the_old_directory() {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let svc = hm_core::config::ConfigService::new(store);
    let api = HomeConfig {
        backend: HomeBackend::HomeAssistant,
        base_url: "http://ha:8123".into(),
        entities: vec![HomeEntity {
            name: "儿童房的灯".into(),
            entity: "light.children".into(),
        }],
        ..Default::default()
    };
    let saved = svc.save_home(api, "test").unwrap();
    assert_eq!(saved.entities.len(), 1);
    let mut switched = saved.clone();
    switched.entity_source = hm_core::config::EntitySource::Exposed;
    let saved = svc.save_home(switched, "test").unwrap();
    assert!(saved.entities.is_empty(), "switch clears: {saved:?}");
    // Switching back also starts empty (the exposed list is not an api list).
    let mut back = saved.clone();
    back.entity_source = hm_core::config::EntitySource::Api;
    let saved = svc.save_home(back, "test").unwrap();
    assert!(saved.entities.is_empty(), "{saved:?}");
}
