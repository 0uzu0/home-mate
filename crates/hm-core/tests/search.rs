//! M2.5: web search at the tool and HTTP boundaries.
//!
//! The fake provider is a REAL protocol partner: it checks the bearer, counts
//! every request, records the requested `count`, and can answer 200 / empty /
//! broken JSON / 500 / slowly / oversized / with a redirect. "Unconfigured =
//! 0 HTTP" is only believable because something is counting on the other end.

use hm_core::api::{router, AppState};
use hm_core::config::{ConfigService, SearchConfig};
use hm_core::llm::fake::FakeLlm;
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::search::{
    SearchRuntime, WebSearchTool, SEARCH_EMPTY, SEARCH_HTTP, SEARCH_PARSE,
    SEARCH_RESPONSE_TOO_LARGE, SEARCH_TIMEOUT, SEARCH_UNCONFIGURED,
};
use hm_core::tool::{Availability, Registry, Tool, ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const KEY: &str = "sk-test-SECRET-1234";

// --- the fake provider ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Empty,
    BadJson,
    Http500,
    Unauthorized,
    Slow,
    TooLarge,
    Redirect,
}

#[derive(Clone)]
struct S {
    mode: Arc<Mutex<Mode>>,
    hits: Arc<AtomicU64>,
    elsewhere: Arc<AtomicU64>,
    asked: Arc<Mutex<Vec<u64>>>,
}

struct Fake {
    base: String,
    mode: Arc<Mutex<Mode>>,
    hits: Arc<AtomicU64>,
    elsewhere: Arc<AtomicU64>,
    asked: Arc<Mutex<Vec<u64>>>,
}

impl Fake {
    fn hits(&self) -> u64 {
        self.hits.load(Ordering::SeqCst)
    }
    fn elsewhere(&self) -> u64 {
        self.elsewhere.load(Ordering::SeqCst)
    }
    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
    fn asked(&self) -> Vec<u64> {
        self.asked.lock().unwrap().clone()
    }
}

async fn spawn_fake() -> Fake {
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::post;

    async fn search(
        State(s): State<S>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        s.hits.fetch_add(1, Ordering::SeqCst);
        let mode = *s.mode.lock().unwrap();
        if mode == Mode::Redirect {
            return (
                axum::http::StatusCode::FOUND,
                [(axum::http::header::LOCATION, "/elsewhere")],
            )
                .into_response();
        }
        if mode == Mode::Unauthorized {
            return (axum::http::StatusCode::UNAUTHORIZED, "no").into_response();
        }
        let auth = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(
            auth,
            format!("Bearer {KEY}"),
            "the tool must send the stored bearer"
        );
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        s.asked
            .lock()
            .unwrap()
            .push(parsed["count"].as_u64().unwrap_or(0));
        match mode {
            Mode::Http500 => {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response()
            }
            Mode::Slow => {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                axum::Json(json!({ "results": [] })).into_response()
            }
            Mode::BadJson => "<html>not json</html>".into_response(),
            Mode::Empty => axum::Json(json!({ "results": [] })).into_response(),
            Mode::TooLarge => {
                let big = "长".repeat(120_000); // ~360 KiB UTF-8
                let body = json!({ "results": [{ "title": "big", "url": "https://example.com/big", "snippet": big }] });
                axum::Json(body).into_response()
            }
            _ => {
                let rows: Vec<Value> = (0..12)
                    .map(|i| {
                        json!({
                            "title": format!("<b>标题{i}</b>"),
                            "url": format!("https://example.com/{i}"),
                            "snippet": "摘要".repeat(300),
                            "summary": "摘要".repeat(300),
                            "date": "2026-11-13",
                            "authority": "example.com"
                        })
                    })
                    .chain(std::iter::once(json!({
                        "title": "无来源", "url": "javascript:alert(1)", "snippet": "x"
                    })))
                    .collect();
                axum::Json(json!({ "results": rows })).into_response()
            }
        }
    }

    async fn elsewhere(State(s): State<S>) -> &'static str {
        s.elsewhere.fetch_add(1, Ordering::SeqCst);
        "{\"results\":[{\"title\":\"leak\",\"url\":\"https://evil.example/x\"}]}"
    }

    let state = S {
        mode: Arc::new(Mutex::new(Mode::Ok)),
        hits: Arc::new(AtomicU64::new(0)),
        elsewhere: Arc::new(AtomicU64::new(0)),
        asked: Arc::new(Mutex::new(Vec::new())),
    };
    let app = axum::Router::new()
        .route("/tools/search", post(search))
        .route("/elsewhere", post(elsewhere))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Fake {
        base: format!("http://{addr}/tools/search"),
        mode: state.mode,
        hits: state.hits,
        elsewhere: state.elsewhere,
        asked: state.asked,
    }
}

// --- helpers ----------------------------------------------------------------

fn store() -> Arc<Store> {
    let s = Arc::new(Store::open_in_memory().unwrap());
    s.allow_loopback_public_for_test();
    s
}

fn ctx() -> ToolCtx {
    ToolCtx::new(
        "T1",
        "C1",
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

fn runtime(store: &Arc<Store>) -> Arc<SearchRuntime> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    SearchRuntime::with_client(store.clone(), client)
}

fn configure(store: &Arc<Store>, base: &str, enabled: bool, timeout_ms: u64) {
    ConfigService::new(store.clone())
        .save_search(
            SearchConfig {
                enabled,
                base_url: base.to_string(),
                count: 8,
                timeout_ms,
            },
            Some(KEY),
            "test",
        )
        .unwrap();
}

// --- runtime behaviour ------------------------------------------------------

#[tokio::test]
async fn unconfigured_search_makes_zero_requests_and_refuses() {
    let fake = spawn_fake().await;
    let store = store();
    // Endpoint present, NO key stored: the config gate is before any socket.
    ConfigService::new(store.clone())
        .save_search(
            SearchConfig {
                enabled: true,
                base_url: fake.base.clone(),
                count: 6,
                timeout_ms: 1000,
            },
            Some(""),
            "test",
        )
        .unwrap();
    let rt = runtime(&store);
    assert_eq!(rt.availability(), Availability::Unconfigured);
    let tool = WebSearchTool::new(rt);
    let r = tool
        .execute(json!({ "query": "今晚有什么体育比赛" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, SEARCH_UNCONFIGURED);
    assert_eq!(
        fake.hits(),
        0,
        "an unconfigured service must not touch the network"
    );
}

#[tokio::test]
async fn a_disabled_service_is_named_and_silent() {
    let fake = spawn_fake().await;
    let store = store();
    configure(&store, &fake.base, false, 1000);
    let rt = runtime(&store);
    assert_eq!(rt.availability(), Availability::Disabled);
    let tool = WebSearchTool::new(rt);
    let r = tool.execute(json!({ "query": "x" }), &ctx()).await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "SEARCH_DISABLED");
    assert_eq!(fake.hits(), 0);
}

#[tokio::test]
async fn a_real_answer_is_bounded_cleaned_and_cited() {
    let fake = spawn_fake().await;
    let store = store();
    configure(&store, &fake.base, true, 2000);
    let rt = runtime(&store);
    assert_eq!(rt.availability(), Availability::Ready);
    let tool = WebSearchTool::new(rt);
    let r = tool
        .execute(json!({ "query": "今晚有什么体育比赛" }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Completed, "{r:?}");
    let v = r.result.unwrap();
    let rows = v["results"].as_array().unwrap();
    assert!(
        rows.len() <= 8,
        "the model reads at most 8 rows: {}",
        rows.len()
    );
    assert_eq!(
        rows.len(),
        8,
        "12 rows came back; the cap is what trims them"
    );
    for row in rows {
        let url = row["url"].as_str().unwrap();
        assert!(url.starts_with("http://") || url.starts_with("https://"));
        let snippet = row["snippet"].as_str().unwrap();
        assert!(snippet.chars().count() <= 201, "snippet cap 200 + ellipsis");
        assert!(
            !row["title"].as_str().unwrap().contains('<'),
            "markup stripped"
        );
        assert_eq!(
            row["date"], "2026-11-13",
            "dates reach the model: it must cite them"
        );
    }
    assert!(
        !v.to_string().contains("javascript:"),
        "a result with no http(s) source is dropped, not relayed"
    );
    assert_eq!(
        fake.asked(),
        vec![8],
        "the provider is asked for the configured count"
    );
}

#[tokio::test]
async fn provider_failures_are_named_not_guessed() {
    let fake = spawn_fake().await;
    let store = store();
    configure(&store, &fake.base, true, 600);
    let rt = runtime(&store);
    let tool = WebSearchTool::new(rt);

    let code = |r: hm_core::tool::ToolResult| r.error.unwrap().code;
    fake.set(Mode::Http500);
    assert_eq!(
        code(tool.execute(json!({ "query": "x" }), &ctx()).await),
        SEARCH_HTTP
    );
    fake.set(Mode::Unauthorized);
    assert_eq!(
        code(tool.execute(json!({ "query": "x" }), &ctx()).await),
        SEARCH_HTTP
    );
    fake.set(Mode::BadJson);
    assert_eq!(
        code(tool.execute(json!({ "query": "x" }), &ctx()).await),
        SEARCH_PARSE
    );
    fake.set(Mode::Empty);
    assert_eq!(
        code(tool.execute(json!({ "query": "x" }), &ctx()).await),
        SEARCH_EMPTY
    );
    fake.set(Mode::TooLarge);
    assert_eq!(
        code(tool.execute(json!({ "query": "x" }), &ctx()).await),
        SEARCH_RESPONSE_TOO_LARGE
    );
    fake.set(Mode::Slow);
    assert_eq!(
        code(tool.execute(json!({ "query": "x" }), &ctx()).await),
        SEARCH_TIMEOUT
    );
}

#[tokio::test]
async fn a_redirect_is_an_error_and_never_followed() {
    let fake = spawn_fake().await;
    let store = store();
    configure(&store, &fake.base, true, 2000);
    fake.set(Mode::Redirect);
    let rt = runtime(&store);
    let tool = WebSearchTool::new(rt);
    let r = tool.execute(json!({ "query": "x" }), &ctx()).await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, SEARCH_HTTP);
    assert_eq!(fake.hits(), 1);
    assert_eq!(
        fake.elsewhere(),
        0,
        "the bearer must never travel to a host we did not configure"
    );
}

#[tokio::test]
async fn the_probe_is_one_real_query_and_zero_when_unconfigured() {
    let fake = spawn_fake().await;
    let store = store();
    let rt = runtime(&store);
    let p = rt.probe().await;
    assert_eq!(p["ok"], false);
    assert_eq!(p["http_requests"], 0);
    assert_eq!(fake.hits(), 0);

    configure(&store, &fake.base, true, 2000);
    let rt = runtime(&store);
    let p = rt.probe().await;
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(p["http_requests"], 1);
    assert_eq!(fake.hits(), 1);
    assert_eq!(p["samples"].as_array().unwrap().len(), 3);
}

#[test]
fn the_capability_list_names_search_only_when_configured() {
    let store = store();
    let rt = runtime(&store);
    let mut reg = Registry::new();
    reg.register(Arc::new(WebSearchTool::new(rt.clone())))
        .expect("valid unique tool registration");

    let caps = reg.capabilities();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].service, "search");
    assert_eq!(caps[0].availability, Availability::Unconfigured);
    assert!(caps[0].tools.iter().any(|t| t.name == "web.search"));
    assert!(reg.ready_capabilities().is_empty());

    ConfigService::new(store.clone())
        .save_search(
            SearchConfig {
                enabled: true,
                ..SearchConfig::default()
            },
            Some("k"),
            "test",
        )
        .unwrap();
    let rt = runtime(&store);
    let mut reg = Registry::new();
    reg.register(Arc::new(WebSearchTool::new(rt)))
        .expect("valid unique tool registration");
    assert_eq!(reg.ready_capabilities().len(), 1);
}

// --- HTTP boundary ----------------------------------------------------------

async fn spawn_app() -> (String, Arc<AppState>, Fake) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::new(
        store,
        PathBuf::from("./data-test"),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
    );
    let fake = spawn_fake().await;
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state, fake)
}

#[tokio::test]
async fn the_console_contract_keeps_the_key_write_only() {
    let (base, _state, fake) = spawn_app().await;
    let c = reqwest::Client::new();

    let got: Value = c
        .get(format!("{base}/api/v0/config/services/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["state"], "unconfigured");
    assert_eq!(got["api_key_set"], false);
    assert_eq!(got["base_url"], "https://api.deepseek.com/v1/tools/search");

    // Unconfigured probe: zero requests, honestly reported.
    let probe: Value = c
        .post(format!("{base}/api/v0/config/services/search/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["ok"], false);
    assert_eq!(probe["http_requests"], 0);
    assert_eq!(fake.hits(), 0);

    let put: Value = c
        .put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({
            "enabled": true, "base_url": fake.base, "count": 6,
            "timeout_ms": 2000, "api_key": KEY,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["state"], "ready", "{put}");
    assert_eq!(put["api_key_set"], true);
    assert!(
        !put.to_string().contains(KEY),
        "the key must never be echoed: {put}"
    );

    let got: Value = c
        .get(format!("{base}/api/v0/config/services/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["api_key_set"], true);
    assert!(!got.to_string().contains(KEY));
    assert_eq!(got["count"], 6);

    // An explicit "" clears the stored key (the card's clear button).
    let cleared: Value = c
        .put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({ "api_key": "" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["api_key_set"], false, "{cleared}");
    assert_eq!(cleared["state"], "unconfigured");
    // Put it back for the remaining assertions.
    c.put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({ "api_key": KEY }))
        .send()
        .await
        .unwrap();

    // Clearing and re-binding the key invalidates a stale editor even when
    // the public settings and key-presence flag match their previous values.
    let stale = got["revision"].as_str().unwrap().to_string();
    let rotated_conflict = c
        .put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({ "enabled": false, "revision": stale }))
        .send()
        .await
        .unwrap();
    assert_eq!(rotated_conflict.status(), 409);
    let conflict = c
        .put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({ "enabled": false, "revision": "deadbeefdeadbeef" }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    let got: Value = c
        .get(format!("{base}/api/v0/config/services/search"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["enabled"], true, "the conflicts wrote nothing");

    // A non-http endpoint never becomes the stored value.
    let bad = c
        .put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({ "base_url": "ftp://example.com/x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn the_probe_and_mode_agree_once_configured() {
    let (base, _state, fake) = spawn_app().await;
    let c = reqwest::Client::new();
    c.put(format!("{base}/api/v0/config/services/search"))
        .json(&json!({
            "enabled": true, "base_url": fake.base, "count": 3,
            "timeout_ms": 2000, "api_key": KEY,
        }))
        .send()
        .await
        .unwrap();

    let probe: Value = c
        .post(format!("{base}/api/v0/config/services/search/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["ok"], true, "{probe}");
    assert_eq!(probe["http_requests"], 1);
    assert_eq!(fake.hits(), 1);
    assert_eq!(fake.asked(), vec![3], "the probe asks for exactly three");

    let mode: Value = c
        .get(format!("{base}/api/v0/mode"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mode["search"]["state"], "ready");
    assert_eq!(mode["search"]["count"], 3);
}
