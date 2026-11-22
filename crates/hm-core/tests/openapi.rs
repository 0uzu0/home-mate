//! R2-BM6: the OpenAPI query adapter at the HTTP boundary.
//!
//! The fake service is a real protocol partner: it checks the bearer token,
//! counts every request, and can be slow / broken / oversized. That is the
//! only honest way to prove "unconfigured = 0 HTTP", "a parameter outside the
//! subset never leaves the box" and "the token is write-only".

use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::llm::fake::{FakeLlm, Scripted};
use hm_core::tool::mocks::MockInject;
use hm_core::tool::Tool;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[path = "openapi/reviewed.rs"]
mod reviewed;

// --- fake OpenAPI service ---------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Unauthorized,
    Slow,
    ServerError,
    TooLarge,
    /// `/status` answers 302 to `/secret`; the adapter must not follow (the
    /// audit's SSRF path).
    Redirect,
    /// The document is public (no bearer); `/status` still checks auth. Used
    /// to prove that a source change without a new token can save.
    PublicSpec,
}

#[derive(Clone)]
struct FakeState {
    hits: Arc<Mutex<Vec<String>>>,
    mode: Mode,
    token: String,
}

struct Fake {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    fn count(&self, needle: &str) -> usize {
        self.hits().iter().filter(|h| h.contains(needle)).count()
    }
}

fn spec_doc() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": { "title": "Fake House API", "version": "1.2.3" },
        "paths": {
            "/status": {
                "get": {
                    "operationId": "getStatus",
                    "summary": "Service status",
                    "parameters": []
                }
            },
            "/rooms/{room}/temperature": {
                "get": {
                    "operationId": "getTemperature",
                    "summary": "Room temperature",
                    "parameters": [
                        { "name": "room", "in": "path", "required": true,
                          "schema": { "type": "string" } },
                        { "name": "unit", "in": "query",
                          "schema": { "type": "string", "enum": ["c", "f"] } }
                    ]
                }
            },
            "/write": {
                "post": {
                    "operationId": "writeThing",
                    "requestBody": { "required": true }
                }
            },
            "/tenant-only": {
                "get": {
                    "operationId": "tenantOnly",
                    "parameters": [
                        { "name": "X-Tenant", "in": "header", "required": true,
                          "schema": { "type": "string" } }
                    ]
                }
            }
        }
    })
}

async fn spawn_fake(mode: Mode) -> Fake {
    use axum::response::IntoResponse;
    let state = FakeState {
        hits: Arc::new(Mutex::new(Vec::new())),
        mode,
        token: "spec-token-123".into(),
    };
    let hits = state.hits.clone();

    async fn authorized(s: &FakeState, headers: &axum::http::HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == format!("Bearer {}", s.token))
            .unwrap_or(false)
    }

    async fn spec(
        axum::extract::State(s): axum::extract::State<FakeState>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        s.hits.lock().unwrap().push("GET /openapi.json".into());
        if s.mode == Mode::Unauthorized
            || (s.mode != Mode::PublicSpec && !authorized(&s, &headers).await)
        {
            return (axum::http::StatusCode::UNAUTHORIZED, "no").into_response();
        }
        if s.mode == Mode::ServerError {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        axum::Json(spec_doc()).into_response()
    }

    async fn status(
        axum::extract::State(s): axum::extract::State<FakeState>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        s.hits.lock().unwrap().push("GET /status".into());
        if s.mode == Mode::Unauthorized || !authorized(&s, &headers).await {
            return (axum::http::StatusCode::UNAUTHORIZED, "no").into_response();
        }
        if s.mode == Mode::Slow {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if s.mode == Mode::ServerError {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        if s.mode == Mode::TooLarge {
            return axum::Json(json!({ "blob": "x".repeat(80 * 1024) })).into_response();
        }
        if s.mode == Mode::Redirect {
            return (
                axum::http::StatusCode::FOUND,
                [(axum::http::header::LOCATION, "/secret")],
                "",
            )
                .into_response();
        }
        axum::Json(json!({ "status": "ok", "temperature": 21 })).into_response()
    }

    async fn secret(
        axum::extract::State(s): axum::extract::State<FakeState>,
    ) -> axum::response::Response {
        s.hits.lock().unwrap().push("GET /secret".into());
        axum::Json(json!({ "secret": true })).into_response()
    }

    async fn temperature(
        axum::extract::State(s): axum::extract::State<FakeState>,
        axum::extract::Path(room): axum::extract::Path<String>,
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        s.hits
            .lock()
            .unwrap()
            .push(format!("GET /rooms/{room}/temperature?{q:?}"));
        if s.mode == Mode::Unauthorized || !authorized(&s, &headers).await {
            return (axum::http::StatusCode::UNAUTHORIZED, "no").into_response();
        }
        if headers
            .get("x-hm-review-token")
            .and_then(|h| h.to_str().ok())
            == Some(s.token.as_str())
        {
            s.hits.lock().unwrap().push("custom auth verified".into());
        }
        axum::Json(json!({ "room": room, "unit": q.get("unit"), "value": 21 })).into_response()
    }

    let app = axum::Router::new()
        .route("/openapi.json", axum::routing::get(spec))
        .route("/status", axum::routing::get(status))
        .route("/secret", axum::routing::get(secret))
        .route("/rooms/{room}/temperature", axum::routing::get(temperature))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fake {
        base: format!("http://{addr}"),
        hits,
    }
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

async fn sse_events(resp: reqwest::Response) -> Vec<Value> {
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = bytes.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            if let Some(data) = line.trim().strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    events.push(v);
                }
            }
        }
    }
    events
}

async fn chat(base: &str, text: &str) -> Vec<Value> {
    sse_events(
        reqwest::Client::new()
            .post(format!("{base}/api/v0/chat"))
            .json(&json!({ "text": text }))
            .send()
            .await
            .unwrap(),
    )
    .await
}

fn tool_result(events: &[Value]) -> Value {
    events
        .iter()
        .find(|e| e["type"] == "tool_result")
        .cloned()
        .unwrap_or(json!(null))
}

async fn configure(base: &str, fake: &Fake, token: &str, ops: Value) -> Value {
    let resp: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({
            "enabled": true,
            "spec_url": format!("{}/openapi.json", fake.base),
            "token": token,
            "operations": ops,
            "timeout_ms": 2000,
            "retries": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp
}

fn call(op: &str, args: Value) -> Scripted {
    Scripted::Calls {
        calls: vec![(
            "openapi.call".into(),
            json!({ "operation": op, "arguments": args }).to_string(),
        )],
    }
}

// --- BM6-1: unconfigured = zero HTTP ---------------------------------------

#[tokio::test]
async fn an_unconfigured_openapi_service_makes_zero_http_requests() {
    let fake = spawn_fake(Mode::Ok).await;
    let llm = FakeLlm::new(vec![
        call("getStatus", json!({})),
        Scripted::Text {
            chunks: vec!["还没有配置查询服务。".into()],
        },
    ]);
    let (base, state) = spawn_app(llm).await;
    // Point the block at the fake without configuring it: the address alone
    // must never be touched.
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                spec_url: format!("{}/openapi.json", fake.base),
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();

    let status: Value = reqwest::get(format!("{base}/api/v0/config/services/openapi"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "unconfigured", "{status}");
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/openapi/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "unconfigured");
    assert_eq!(probe["http_requests"], 0);
    assert!(fake.hits().is_empty(), "{:?}", fake.hits());

    let events = chat(&base, "查一下状态").await;
    let r = tool_result(&events);
    assert_eq!(r["status"], "failed", "{r}");
    assert!(
        r["summary"]
            .as_str()
            .unwrap()
            .contains("OPENAPI_UNCONFIGURED"),
        "{r}"
    );
    assert!(
        fake.hits().is_empty(),
        "unconfigured must not touch the network: {:?}",
        fake.hits()
    );
}

// --- BM6-1/3: discovery, save, token handling -----------------------------

#[tokio::test]
async fn discovery_lists_get_operations_and_saving_resolves_the_whitelist() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, _state) = spawn_app(FakeLlm::new(vec![])).await;

    let discovered: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/openapi/discover"))
        .json(&json!({ "spec_url": format!("{}/openapi.json", fake.base), "token": "spec-token-123" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(discovered["ok"], true, "{discovered}");
    let ops = discovered["operations"].as_array().unwrap();
    let ids: Vec<&str> = ops.iter().filter_map(|o| o["id"].as_str()).collect();
    assert!(
        ids.contains(&"getStatus") && ids.contains(&"getTemperature"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"writeThing"),
        "POST is now discovered as callable: {ids:?}"
    );
    let write_op = ops.iter().find(|o| o["id"] == "writeThing").unwrap();
    assert_eq!(write_op["method"], "POST");
    assert_eq!(write_op["effect_level"], "mutate");
    assert!(ids.contains(&"tenantOnly"));
    let tenant = ops.iter().find(|o| o["id"] == "tenantOnly").unwrap();
    assert_eq!(
        tenant["callable"], false,
        "required header param is not callable"
    );

    let saved = configure(
        &base,
        &fake,
        "spec-token-123",
        json!(["getStatus", "getTemperature"]),
    )
    .await;
    assert_eq!(saved["state"], "ready", "{saved}");
    assert_eq!(saved["token_set"], true);
    assert_eq!(saved["operations"].as_array().unwrap().len(), 2);
    assert!(
        !saved.to_string().contains("spec-token-123"),
        "token must never be echoed: {saved}"
    );

    // Ordinary export/GET omit the token; the explicit backup includes it.
    let plain: Value = reqwest::get(format!("{base}/api/v0/config/export"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!plain.to_string().contains("spec-token-123"), "{plain}");
    assert_eq!(plain["openapi"]["token_set"], true);
    let full: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/export?secrets=1"))
        .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(full["secrets"]["openapi.token"], "spec-token-123");
}

#[tokio::test]
async fn saving_an_unknown_or_header_only_operation_is_refused() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let before = state.config.openapi().unwrap();

    let client = reqwest::Client::new();
    let unknown: Value = client
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({
            "enabled": true,
            "spec_url": format!("{}/openapi.json", fake.base),
            "token": "spec-token-123",
            "operations": ["nonExistentOp"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        unknown["error"]
            .as_str()
            .unwrap()
            .contains("OPENAPI_OPERATION_UNKNOWN"),
        "{unknown}"
    );

    let header_only: Value = client
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({
            "enabled": true,
            "spec_url": format!("{}/openapi.json", fake.base),
            "token": "spec-token-123",
            "operations": ["tenantOnly"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        header_only["error"]
            .as_str()
            .unwrap()
            .contains("OPENAPI_METHOD_UNSUPPORTED"),
        "{header_only}"
    );
    // A failed save leaves the old block untouched.
    assert_eq!(state.config.openapi().unwrap(), before);
    assert!(
        state.config.openapi_token().unwrap().is_none(),
        "a failed save must not store the token"
    );
}

// --- BM6-2: parameter subset and zero HTTP on refusal ----------------------

#[tokio::test]
async fn the_parameter_subset_is_enforced_before_any_request() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(
        &base,
        &fake,
        "spec-token-123",
        json!(["getStatus", "getTemperature"]),
    )
    .await;
    fake.hits.lock().unwrap().clear();

    let ctx = hm_core::tool::ToolCtx::new(
        "T1",
        "C1",
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );

    // 1) a legal call goes out once
    let r = state
        .registry
        .call(
            "openapi.call",
            json!({ "operation": "getTemperature", "arguments": { "room": "kids", "unit": "c" } }),
            &ctx,
        )
        .await;
    assert_eq!(r.status, hm_core::tool::ToolStatus::Completed, "{r:?}");
    assert_eq!(r.evidence, hm_core::tool::Evidence::Observed);
    assert_eq!(
        fake.count("GET /rooms/kids/temperature"),
        1,
        "{:?}",
        fake.hits()
    );
    assert!(
        fake.hits().iter().any(|h| h.contains("unit")),
        "{:?}",
        fake.hits()
    );

    // Everything below is refused locally: the hit count must not move.
    let before = fake.hits().len();
    for (args, code) in [
        (
            json!({ "operation": "getTemperature", "arguments": { "room": "kids", "fahrenheit": true } }),
            "OPENAPI_PARAM_NOT_ALLOWED",
        ),
        (
            json!({ "operation": "getTemperature", "arguments": { "unit": "c" } }),
            "OPENAPI_PARAM_REQUIRED",
        ),
        (
            json!({ "operation": "getTemperature", "arguments": { "room": "kids", "unit": "kelvin" } }),
            "OPENAPI_PARAM_ENUM",
        ),
        (
            json!({ "operation": "getTemperature", "arguments": { "room": 5 } }),
            "OPENAPI_PARAM_TYPE",
        ),
        (
            json!({ "operation": "nope", "arguments": {} }),
            "OPENAPI_OPERATION_UNKNOWN",
        ),
    ] {
        let r = state
            .registry
            .call("openapi.call", args.clone(), &ctx)
            .await;
        assert_eq!(
            r.status,
            hm_core::tool::ToolStatus::Failed,
            "{args} -> {r:?}"
        );
        assert!(
            r.error.as_ref().unwrap().code == code,
            "{args} -> {:?}",
            r.error
        );
        assert_eq!(fake.hits().len(), before, "{args}: {:?}", fake.hits());
    }
}

// --- BM6-2/3: auth, timeout, oversized response, probe ---------------------

#[tokio::test]
async fn auth_timeout_and_oversized_responses_are_named_failures() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&base, &fake, "spec-token-123", json!(["getStatus"])).await;
    // Replace the stored token only (no spec fetch on this partial update).
    let _ = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({ "enabled": true, "token": "wrong-token" }))
        .send()
        .await
        .unwrap();
    let (err, _) = state
        .openapi
        .call("getStatus", &json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "OPENAPI_AUTH");

    // Slow service → timeout with retries=0.
    let slow = spawn_fake(Mode::Slow).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                enabled: true,
                spec_url: format!("{}/openapi.json", slow.base),
                operations: vec![hm_core::config::OpenApiOperation {
                    id: "getStatus".into(),
                    method: "get".into(),
                    path: "/status".into(),
                    summary: String::new(),
                    params: vec![],
                    effect_level: hm_core::config::EffectLevel::Query,
                    verification: None,
                }],
                timeout_ms: 500,
                retries: 0,
                ..Default::default()
            },
            Some("spec-token-123"),
            "test",
        )
        .unwrap();
    let (err, _) = state
        .openapi
        .call("getStatus", &json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "OPENAPI_TIMEOUT", "{}", err.detail);

    // Oversized response → OPENAPI_RESPONSE_TOO_LARGE (the client stops
    // reading; it never buffers the whole body).
    let big = spawn_fake(Mode::TooLarge).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                enabled: true,
                spec_url: format!("{}/openapi.json", big.base),
                operations: vec![hm_core::config::OpenApiOperation {
                    id: "getStatus".into(),
                    method: "get".into(),
                    path: "/status".into(),
                    summary: String::new(),
                    params: vec![],
                    effect_level: hm_core::config::EffectLevel::Query,
                    verification: None,
                }],
                timeout_ms: 2000,
                retries: 0,
                ..Default::default()
            },
            Some("spec-token-123"),
            "test",
        )
        .unwrap();
    let (err, _) = state
        .openapi
        .call("getStatus", &json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, hm_core::tool::openapi::OPENAPI_RESPONSE_TOO_LARGE);
}

#[tokio::test]
async fn a_redirect_is_refused_not_followed() {
    let fake = spawn_fake(Mode::Redirect).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&base, &fake, "spec-token-123", json!(["getStatus"])).await;
    fake.hits.lock().unwrap().clear();
    let (err, _) = state
        .openapi
        .call("getStatus", &json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "OPENAPI_HTTP", "{err:?}");
    assert!(!err.detail.contains("spec-token-123"), "{err:?}");
    assert_eq!(fake.count("GET /secret"), 0, "a 3xx must not be followed");
}

#[tokio::test]
async fn a_path_parameter_cannot_escape_the_template() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(
        &base,
        &fake,
        "spec-token-123",
        json!(["getStatus", "getTemperature"]),
    )
    .await;
    fake.hits.lock().unwrap().clear();
    let (err, requests) = state
        .openapi
        .call(
            "getTemperature",
            &json!({ "room": "..", "unit": "c" }),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "OPENAPI_PARAM_NOT_ALLOWED", "{err:?}");
    assert_eq!(requests, 0);
    assert!(fake.hits().is_empty(), "{:?}", fake.hits());
}

#[tokio::test]
async fn a_probe_notices_a_parameter_catalogue_change() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&base, &fake, "spec-token-123", json!(["getStatus"])).await;
    let mut cfg = state.config.openapi().unwrap();
    cfg.operations[0]
        .params
        .push(hm_core::config::OpenApiParam {
            sensitive: false,
            name: "verbose".into(),
            location: hm_core::config::OpenApiParamLocation::Query,
            required: false,
            kind: "boolean".into(),
            enum_values: vec![],
        });
    state.config.save_openapi(cfg, None, "test").unwrap();
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/openapi/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "failed", "{probe}");
    assert_eq!(probe["code"], "OPENAPI_SPEC_CHANGED");
    assert!(
        probe["detail"].as_str().unwrap().contains("参数目录"),
        "{probe}"
    );
}

#[tokio::test]
async fn changing_the_document_source_clears_the_bound_token() {
    let fake = spawn_fake(Mode::PublicSpec).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&base, &fake, "spec-token-123", json!(["getStatus"])).await;
    assert_eq!(
        state.config.openapi_token().unwrap().as_deref(),
        Some("spec-token-123")
    );

    // Same URL without a token: the stored one stays.
    let same: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({ "enabled": true, "timeout_ms": 3000 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(same["token_set"], true, "{same}");

    // A DIFFERENT URL without a token: the old credential must not survive.
    let changed: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(
            &json!({ "enabled": true, "spec_url": format!("{}/openapi.json?other=1", fake.base) }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed["token_set"], false, "{changed}");
    assert!(state.config.openapi_token().unwrap().is_none());
}

#[tokio::test]
async fn clearing_the_whitelist_needs_no_document_fetch() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&base, &fake, "spec-token-123", json!(["getStatus"])).await;
    fake.hits.lock().unwrap().clear();
    let resp: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({ "enabled": false, "operations": [] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["state"], "unconfigured", "{resp}");
    assert!(resp["operations"].as_array().unwrap().is_empty());
    assert!(
        fake.hits().is_empty(),
        "clearing must not read the document"
    );
    assert_eq!(state.config.openapi().unwrap().state_word(), "unconfigured");
}

#[tokio::test]
async fn a_probe_really_calls_the_service_and_reports_requests() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, _state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(
        &base,
        &fake,
        "spec-token-123",
        json!(["getStatus", "getTemperature"]),
    )
    .await;
    fake.hits.lock().unwrap().clear();
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/openapi/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "ready", "{probe}");
    assert_eq!(
        probe["sample"]["operation"], "getStatus",
        "the no-arg operation is sampled: {probe}"
    );
    assert!(probe["http_requests"].as_u64().unwrap() >= 2, "{probe}");
    assert_eq!(fake.count("GET /openapi.json"), 1);
    assert_eq!(fake.count("GET /status"), 1);
}

#[tokio::test]
async fn a_probe_notices_that_the_document_no_longer_matches_the_saved_whitelist() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&base, &fake, "spec-token-123", json!(["getStatus"])).await;
    // Simulate an edited document by storing an operation the fake no longer
    // has (the storage is the thing the model may call).
    let mut cfg = state.config.openapi().unwrap();
    cfg.operations[0].id = "vanishedOperation".into();
    state.config.save_openapi(cfg, None, "test").unwrap();
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/openapi/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "failed", "{probe}");
    assert_eq!(probe["code"], "OPENAPI_SPEC_CHANGED");
}

#[tokio::test]
async fn importing_a_catalogue_never_performs_http_and_rejects_non_get() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let good = json!({
        "format": "home-mate-config",
        "version": 1,
        "openapi": {
            "enabled": true,
            "spec_url": format!("{}/openapi.json", fake.base),
            "base_url": "",
            "auth_header": "",
            "operations": [{
                "id": "getStatus", "method": "get", "path": "/status",
                "summary": "", "params": []
            }],
            "timeout_ms": 2000, "retries": 0
        },
        "secrets": { "openapi.token": "spec-token-123" }
    });
    let resp: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/import"))
        .json(&good)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["state"], "applied", "{resp}");
    assert!(
        fake.hits().is_empty(),
        "import must not fetch the document: {:?}",
        fake.hits()
    );
    assert_eq!(
        state.config.openapi_token().unwrap().as_deref(),
        Some("spec-token-123")
    );

    let mut bad = good.clone();
    bad["openapi"]["operations"][0]["method"] = json!("head");
    let resp: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/import"))
        .json(&bad)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["error"].as_str().unwrap().contains("不支持"), "{resp}");
}

// --- audit R1-3: disabling must not need the document ----------------------

/// A household revoking access is exactly the moment the document may have
/// moved, the token may have been revoked, or the host may be down. The old
/// save path refetched whenever `operations` was present (the console always
/// sends it), so "turn it off" failed with the document unreachable.
#[tokio::test]
async fn disabling_the_adapter_never_fetches_the_document() {
    let fake = spawn_fake(Mode::Ok).await;
    let (base, _state) = spawn_app(FakeLlm::new(vec![])).await;
    let configured = configure(
        &base,
        &fake,
        "spec-token-123",
        json!(["getStatus", "getTemperature"]),
    )
    .await;
    assert_eq!(configured["state"], "ready", "{configured}");
    let hits_after_configure = fake.hits().len();
    assert_eq!(
        hits_after_configure,
        1,
        "one document fetch: {:?}",
        fake.hits()
    );

    // Disable through the same URL the console would send. No new fetch, and
    // the whitelist stays for the day it is switched back on.
    let disabled: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({
            "enabled": false,
            "spec_url": format!("{}/openapi.json", fake.base),
            "operations": ["getStatus", "getTemperature"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(disabled["enabled"], false, "{disabled}");
    assert_eq!(
        fake.hits().len(),
        hits_after_configure,
        "disabling fetched the document: {:?}",
        fake.hits()
    );
    assert_eq!(
        disabled["operations"].as_array().unwrap().len(),
        2,
        "{disabled}"
    );

    // Disabling *and* pointing at an unreachable document: still saves. The
    // old whitelist is dropped because its ids belonged to the old document.
    let moved: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/openapi"))
        .json(&json!({
            "enabled": false,
            "spec_url": "http://127.0.0.1:1/openapi.json",
            "operations": ["getStatus"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(moved["enabled"], false, "{moved}");
    assert_eq!(
        moved["operations"].as_array().unwrap().len(),
        0,
        "a changed document must not keep ids resolved against the old one: {moved}"
    );
    assert_eq!(
        fake.hits().len(),
        hits_after_configure,
        "the unreachable save still fetched: {:?}",
        fake.hits()
    );
}

// --- Wave 4a: OpenAPI Write Operations & Verification Contracts -------------

#[tokio::test]
async fn w4a_post_with_readback_op_passes_and_confirms() {
    use axum::{
        extract::State,
        routing::{get, post},
        Json as AxumJson, Router,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let state_power = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));

    let spec = json!({
        "openapi": "3.0.3",
        "info": { "title": "Write API", "version": "1.0" },
        "paths": {
            "/devices/{id}/state": {
                "get": {
                    "operationId": "getDeviceState",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }]
                }
            },
            "/devices/{id}/power": {
                "post": {
                    "operationId": "setDevicePower",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["on"],
                                    "properties": { "on": { "type": "boolean" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let app = Router::new()
        .route(
            "/openapi.json",
            get({
                let spec = spec.clone();
                move || async move { AxumJson(spec.clone()) }
            }),
        )
        .route(
            "/devices/{id}/state",
            get(
                |State((power, hits)): State<(Arc<AtomicBool>, Arc<AtomicUsize>)>| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    AxumJson(json!({ "power": power.load(Ordering::SeqCst) }))
                },
            ),
        )
        .route(
            "/devices/{id}/power",
            post(
                |State((power, hits)): State<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
                 AxumJson(body): AxumJson<Value>| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if let Some(on) = body.get("on").and_then(Value::as_bool) {
                        power.store(on, Ordering::SeqCst);
                    }
                    AxumJson(json!({ "accepted": true }))
                },
            ),
        )
        .with_state((state_power.clone(), hits.clone()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_app_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let saved: Value = reqwest::Client::new()
        .put(format!("{_app_base}/api/v0/config/services/openapi"))
        .json(&json!({
            "enabled": true,
            "spec_url": format!("{base_url}/openapi.json"),
            "operations": [
                { "id": "getDeviceState", "effect_level": "query" },
                {
                    "id": "setDevicePower",
                    "effect_level": "mutate",
                    "verification": {
                        "type": "readback_op",
                        "op_id": "getDeviceState",
                        "json_path": "power",
                        "expected": true
                    }
                }
            ]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["state"], "ready", "{saved}");

    let tool = hm_core::tool::openapi::OpenApiCallTool::new(state.openapi.clone());
    assert_eq!(
        tool.descriptor().side_effect,
        hm_core::tool::SideEffect::Mutate
    );

    let ctx = hm_core::tool::ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));
    let res = tool
        .execute(
            json!({ "operation": "setDevicePower", "arguments": { "id": "lamp1", "on": true } }),
            &ctx,
        )
        .await;
    assert_eq!(
        res.status,
        hm_core::tool::ToolStatus::Completed,
        "{:?}",
        res.error
    );
    assert_eq!(res.evidence, hm_core::tool::Evidence::Observed);
    let fact = res.fact.as_ref().expect("fact");
    assert!(hm_core::agent::facts::is_confirmed(&res, fact));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn w4a_post_with_failed_readback_returns_truthful_failure() {
    use axum::{
        routing::{get, post},
        Json as AxumJson, Router,
    };
    use std::sync::atomic::AtomicBool;

    let spec = json!({
        "openapi": "3.0.3",
        "paths": {
            "/state": { "get": { "operationId": "getState" } },
            "/state/set": { "post": { "operationId": "setState" } }
        }
    });
    let app = Router::new()
        .route(
            "/openapi.json",
            get(move || {
                let s = spec.clone();
                async move { AxumJson(s) }
            }),
        )
        .route(
            "/state",
            get(|| async { AxumJson(json!({ "status": "off" })) }),
        )
        .route(
            "/state/set",
            post(|| async { AxumJson(json!({ "ok": true })) }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_app_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                enabled: true,
                spec_url: format!("{base_url}/openapi.json"),
                operations: vec![
                    hm_core::config::OpenApiOperation {
                        id: "getState".into(),
                        method: "get".into(),
                        path: "/state".into(),
                        summary: String::new(),
                        params: vec![],
                        effect_level: hm_core::config::EffectLevel::Query,
                        verification: None,
                    },
                    hm_core::config::OpenApiOperation {
                        id: "setState".into(),
                        method: "post".into(),
                        path: "/state/set".into(),
                        summary: String::new(),
                        params: vec![],
                        effect_level: hm_core::config::EffectLevel::Mutate,
                        verification: Some(hm_core::config::VerificationStrategy::ReadbackOp {
                            op_id: "getState".into(),
                            json_path: "status".into(),
                            expected: json!("on"),
                        }),
                    },
                ],
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();

    let tool = hm_core::tool::openapi::OpenApiCallTool::new(state.openapi.clone());
    let ctx = hm_core::tool::ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));
    let res = tool
        .execute(json!({ "operation": "setState", "arguments": {} }), &ctx)
        .await;
    assert_eq!(res.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(
        res.error.as_ref().unwrap().code,
        "OPENAPI_READBACK_MISMATCH"
    );
    let fact = res.fact.as_ref().expect("fact");
    assert!(!hm_core::agent::facts::is_confirmed(&res, fact));
}

#[tokio::test]
async fn w4a_transport_only_downgraded_to_evidence_sent() {
    use axum::{routing::post, Json as AxumJson, Router};
    use std::sync::atomic::AtomicBool;

    let app = Router::new().route(
        "/notify",
        post(|| async { AxumJson(json!({ "ok": true })) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_app_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                enabled: true,
                spec_url: format!("{base_url}/openapi.json"),
                operations: vec![hm_core::config::OpenApiOperation {
                    id: "sendNotify".into(),
                    method: "post".into(),
                    path: "/notify".into(),
                    summary: String::new(),
                    params: vec![],
                    effect_level: hm_core::config::EffectLevel::Mutate,
                    verification: Some(hm_core::config::VerificationStrategy::TransportOnly),
                }],
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();

    let tool = hm_core::tool::openapi::OpenApiCallTool::new(state.openapi.clone());
    let ctx = hm_core::tool::ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));
    let res = tool
        .execute(json!({ "operation": "sendNotify", "arguments": {} }), &ctx)
        .await;
    assert_eq!(res.status, hm_core::tool::ToolStatus::Completed);
    assert_eq!(res.evidence, hm_core::tool::Evidence::Sent);
    let fact = res.fact.as_ref().expect("fact");
    assert!(!hm_core::agent::facts::is_confirmed(&res, fact));
    let rendered = hm_core::agent::facts::render_fact(&res, fact);
    assert!(
        rendered.contains("已发送请求，未验证最终状态"),
        "expected unverified notice in rendered fact: {rendered}"
    );
}

#[tokio::test]
async fn w4a_post_retry_disabled_on_500_and_timeout() {
    use axum::{http::StatusCode, routing::post, Router};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let hits_500 = Arc::new(AtomicUsize::new(0));
    let hits_to = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/err500",
            post({
                let h = hits_500.clone();
                move || {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                }
            }),
        )
        .route(
            "/slow",
            post({
                let h = hits_to.clone();
                move || {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                        StatusCode::OK
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_app_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                enabled: true,
                spec_url: format!("{base_url}/openapi.json"),
                operations: vec![
                    hm_core::config::OpenApiOperation {
                        id: "post500".into(),
                        method: "post".into(),
                        path: "/err500".into(),
                        summary: String::new(),
                        params: vec![],
                        effect_level: hm_core::config::EffectLevel::Mutate,
                        verification: Some(hm_core::config::VerificationStrategy::TransportOnly),
                    },
                    hm_core::config::OpenApiOperation {
                        id: "postSlow".into(),
                        method: "post".into(),
                        path: "/slow".into(),
                        summary: String::new(),
                        params: vec![],
                        effect_level: hm_core::config::EffectLevel::Mutate,
                        verification: Some(hm_core::config::VerificationStrategy::TransportOnly),
                    },
                ],
                timeout_ms: 500,
                retries: 2,
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();

    let tool = hm_core::tool::openapi::OpenApiCallTool::new(state.openapi.clone());
    let ctx = hm_core::tool::ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));

    let r1 = tool
        .execute(json!({ "operation": "post500", "arguments": {} }), &ctx)
        .await;
    assert_eq!(r1.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(
        hits_500.load(Ordering::SeqCst),
        1,
        "POST 500 must not retry: {:?}",
        r1.error
    );

    let r2 = tool
        .execute(json!({ "operation": "postSlow", "arguments": {} }), &ctx)
        .await;
    assert_eq!(r2.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(
        hits_to.load(Ordering::SeqCst),
        1,
        "POST timeout must not retry"
    );
}

#[tokio::test]
async fn destructive_model_boolean_never_grants_execution_authority() {
    use axum::{routing::delete, Json as AxumJson, Router};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/items/1",
        delete({
            let h = hits.clone();
            move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    AxumJson(json!({ "deleted": true }))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_app_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    state
        .config
        .save_openapi(
            hm_core::config::OpenApiConfig {
                enabled: true,
                spec_url: format!("{base_url}/openapi.json"),
                operations: vec![hm_core::config::OpenApiOperation {
                    id: "deleteItem".into(),
                    method: "delete".into(),
                    path: "/items/1".into(),
                    summary: String::new(),
                    params: vec![],
                    effect_level: hm_core::config::EffectLevel::Destructive,
                    verification: Some(hm_core::config::VerificationStrategy::ResponseAssert {
                        json_path: "deleted".into(),
                        expected: json!(true),
                    }),
                }],
                ..Default::default()
            },
            None,
            "test",
        )
        .unwrap();

    let tool = hm_core::tool::openapi::OpenApiCallTool::new(state.openapi.clone());
    assert_eq!(
        tool.descriptor().side_effect,
        hm_core::tool::SideEffect::Mutate
    );
    let ctx = hm_core::tool::ToolCtx::new("t1", "c1", Arc::new(AtomicBool::new(false)));

    let blocked = tool
        .execute(json!({ "operation": "deleteItem", "arguments": {} }), &ctx)
        .await;
    assert_eq!(blocked.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(blocked.error.as_ref().unwrap().code, "PAIRING_REQUIRED");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "blocked destructive op must make zero HTTP"
    );

    let allowed = tool
        .execute(
            json!({ "operation": "deleteItem", "arguments": {}, "confirmed": true }),
            &ctx,
        )
        .await;
    assert_eq!(allowed.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(allowed.error.as_ref().unwrap().code, "PAIRING_REQUIRED");
    assert!(tool.descriptor().parameters["properties"]
        .get("confirmed")
        .is_none());
    let direct = state
        .openapi
        .execute_operation("deleteItem", &json!({}), true, None)
        .await;
    assert_eq!(direct.status, hm_core::tool::ToolStatus::Failed);
    assert_eq!(direct.error.unwrap().code, "OPENAPI_CONFIRMATION_REQUIRED");
    assert!(state
        .openapi
        .call("deleteItem", &json!({}), None)
        .await
        .is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
