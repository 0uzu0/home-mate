//! R2-BM6: news as a registered service. The fake feeds are real protocol
//! partners (they count requests and can fail); the test asserts the honest
//! states, the three parsers and "unconfigured = 0 HTTP". There is no mock
//! news dataset anywhere in the product.

use futures::StreamExt;
use hm_core::api::{router, AppState};
use hm_core::config::{ConfigService, NewsConfig, NewsKind, NewsSource};
use hm_core::llm::fake::FakeLlm;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Slow,
    ServerError,
}

#[derive(Clone)]
struct FeedState {
    hits: Arc<Mutex<Vec<String>>>,
    mode: Mode,
}

struct FakeFeeds {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl FakeFeeds {
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    fn count(&self, path: &str) -> usize {
        self.hits().iter().filter(|h| h.as_str() == path).count()
    }
    fn clear(&self) {
        self.hits.lock().unwrap().clear();
    }
}

const RSS: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel>
<title>Tech</title>
<item><title>First headline</title><link>https://example.com/1</link>
  <pubDate>Thu, 13 Nov 2026 09:00:00 GMT</pubDate></item>
<item><title><![CDATA[Second &amp; best]]></title><link>https://example.com/2</link></item>
<item><title>Third</title></item>
</channel></rss>"#;

const ATOM: &str = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
<title>World</title>
<entry><title>Atom one</title><link rel="alternate" href="https://example.com/a1"/>
  <updated>2026-11-13T08:00:00Z</updated></entry>
<entry><title>Atom two</title><link href="https://example.com/a2"/></entry>
</feed>"#;

const JSON_FEED: &str = r#"{"items":[
  {"title":"Json one","link":"https://example.com/j1"},
  {"title":"Json two"}
]}"#;

async fn spawn_feeds(mode: Mode) -> FakeFeeds {
    use axum::response::IntoResponse;
    let state = FeedState {
        hits: Arc::new(Mutex::new(Vec::new())),
        mode,
    };
    let hits = state.hits.clone();

    async fn serve(
        axum::extract::State(s): axum::extract::State<FeedState>,
        axum::extract::Path(kind): axum::extract::Path<String>,
    ) -> axum::response::Response {
        s.hits.lock().unwrap().push(format!("/{kind}"));
        if s.mode == Mode::Slow {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if s.mode == Mode::ServerError {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        let body = match kind.as_str() {
            "rss" => RSS,
            "atom" => ATOM,
            "json" => JSON_FEED,
            "empty" => "<rss><channel></channel></rss>",
            "broken" => "not a feed at all",
            // An item without its closing tag used to read as an empty feed.
            "truncated" => "<rss><channel><item><title>A</title></channel></rss>",
            _ => "",
        };
        (
            [(axum::http::header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response()
    }

    let app = axum::Router::new()
        .route("/{kind}", axum::routing::get(serve))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeFeeds {
        base: format!("http://{addr}"),
        hits,
    }
}

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

fn sources(fake: &FakeFeeds) -> Vec<NewsSource> {
    vec![
        NewsSource {
            alias: "科技".into(),
            url: format!("{}/rss", fake.base),
            kind: NewsKind::Rss,
            max_items: 3,
        },
        NewsSource {
            alias: "国际".into(),
            url: format!("{}/atom", fake.base),
            kind: NewsKind::Atom,
            max_items: 3,
        },
        NewsSource {
            alias: "简报".into(),
            url: format!("{}/json", fake.base),
            kind: NewsKind::Json,
            max_items: 3,
        },
    ]
}

fn configure(state: &AppState, fake: &FakeFeeds, enabled: bool) {
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled,
                sources: sources(fake),
                default_alias: "科技".into(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
}

fn ctx() -> ToolCtx {
    ToolCtx::new("T", "C", Arc::new(AtomicBool::new(false)))
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

#[tokio::test]
async fn an_unconfigured_news_service_never_touches_the_network() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    // An address in the block is not configuration: no sources = unconfigured.
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: Vec::new(),
                default_alias: String::new(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();

    let status: Value = reqwest::get(format!("{base}/api/v0/config/services/news"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "unconfigured");
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/news/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "unconfigured");
    assert_eq!(probe["http_requests"], 0);

    let r = state
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_UNCONFIGURED");
    assert!(fake.hits().is_empty(), "{:?}", fake.hits());
}

#[tokio::test]
async fn three_feed_formats_are_parsed_and_the_cap_is_honoured() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&state, &fake, true);

    for (alias, expected) in [
        ("科技", "First headline"),
        ("国际", "Atom one"),
        ("简报", "Json one"),
    ] {
        let r = state
            .registry
            .call(
                "news.headlines",
                json!({ "source": alias, "limit": 2 }),
                &ctx(),
            )
            .await;
        assert_eq!(r.status, ToolStatus::Completed, "{alias}: {r:?}");
        let v = r.result.unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(items.len() <= 2, "{alias}: {v}");
        assert!(items.iter().any(|i| i["title"] == expected), "{alias}: {v}");
    }

    // The RSS title had CDATA + an entity; the link is http(s).
    let r = state
        .registry
        .call(
            "news.headlines",
            json!({ "source": "科技", "limit": 5 }),
            &ctx(),
        )
        .await;
    let v = r.result.unwrap();
    let titles: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["title"].as_str())
        .collect();
    assert_eq!(titles, vec!["First headline", "Second & best", "Third"]);
    assert_eq!(v["items"][0]["link"], "https://example.com/1");

    // The tool is reachable through the agent path too.
    let (base2, state2) = spawn_app(FakeLlm::new(vec![
        hm_core::llm::fake::Scripted::Calls {
            calls: vec![(
                "news.headlines".into(),
                json!({ "source": "简报", "limit": 1 }).to_string(),
            )],
        },
        hm_core::llm::fake::Scripted::Text {
            chunks: vec!["头条是 Json one。".into()],
        },
    ]))
    .await;
    configure(&state2, &fake, true);
    let events = sse_events(
        reqwest::Client::new()
            .post(format!("{base2}/api/v0/chat"))
            .json(&json!({ "text": "今天有什么新闻" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["status"], "completed", "{result}");
    let _ = base;
}

#[tokio::test]
async fn unknown_empty_and_broken_sources_are_named() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let mut srcs = sources(&fake);
    srcs.push(NewsSource {
        alias: "空".into(),
        url: format!("{}/empty", fake.base),
        kind: NewsKind::Rss,
        max_items: 3,
    });
    srcs.push(NewsSource {
        alias: "坏".into(),
        url: format!("{}/broken", fake.base),
        kind: NewsKind::Rss,
        max_items: 3,
    });
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: srcs,
                default_alias: "科技".into(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();

    let r = state
        .registry
        .call("news.headlines", json!({ "source": "不存在" }), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_SOURCE_UNKNOWN");

    let r = state
        .registry
        .call("news.headlines", json!({ "source": "空" }), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_EMPTY");

    let r = state
        .registry
        .call("news.headlines", json!({ "source": "坏" }), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_PARSE", "{r:?}");

    let _ = base;
}

#[tokio::test]
async fn a_disabled_service_is_named_and_makes_no_request() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    configure(&state, &fake, false);
    fake.clear();
    let r = state
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_DISABLED");
    assert!(fake.hits().is_empty());
    let status: Value = reqwest::get(format!("{base}/api/v0/config/services/news"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "disabled");
}

#[tokio::test]
async fn timeouts_and_http_errors_are_named_not_swallowed() {
    let slow = spawn_feeds(Mode::Slow).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: vec![NewsSource {
                    alias: "慢".into(),
                    url: format!("{}/rss", slow.base),
                    kind: NewsKind::Rss,
                    max_items: 3,
                }],
                default_alias: String::new(),
                timeout_ms: 500,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
    let r = state
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_TIMEOUT", "{r:?}");

    let broken = spawn_feeds(Mode::ServerError).await;
    let (base2, state2) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state2.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: vec![NewsSource {
                    alias: "错".into(),
                    url: format!("{}/rss", broken.base),
                    kind: NewsKind::Rss,
                    max_items: 3,
                }],
                default_alias: String::new(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
    let r = state2
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_HTTP", "{r:?}");

    // The probe names both: one readable source keeps the service usable, and
    // the failing one is still reported.
    let mixed = spawn_feeds(Mode::Ok).await;
    let (base3, state3) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state3.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: vec![
                    NewsSource {
                        alias: "好".into(),
                        url: format!("{}/rss", mixed.base),
                        kind: NewsKind::Rss,
                        max_items: 3,
                    },
                    NewsSource {
                        alias: "坏".into(),
                        url: format!("{}/broken", mixed.base),
                        kind: NewsKind::Rss,
                        max_items: 3,
                    },
                ],
                default_alias: "好".into(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
    let probe: Value = reqwest::Client::new()
        .post(format!("{base3}/api/v0/config/services/news/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "ready", "{probe}");
    assert_eq!(probe["sources"].as_array().unwrap().len(), 2);
    assert_eq!(mixed.count("/rss"), 1);
    let _ = (base, base2);
}

#[tokio::test]
async fn save_validation_refuses_duplicate_aliases_and_bad_urls() {
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let bad_dup = json!({
        "enabled": true,
        "sources": [
            { "alias": "科技", "url": "https://example.com/a", "kind": "rss", "max_items": 3 },
            { "alias": "科技", "url": "https://example.com/b", "kind": "rss", "max_items": 3 }
        ]
    });
    let resp: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/news"))
        .json(&bad_dup)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["error"].as_str().unwrap().contains("重复"), "{resp}");
    assert_eq!(
        state.config.news().unwrap().sources.len(),
        0,
        "a bad save writes nothing"
    );

    let bad_url = json!({
        "enabled": true,
        "sources": [{ "alias": "科技", "url": "file:///etc/passwd", "kind": "rss", "max_items": 3 }]
    });
    let resp: Value = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/news"))
        .json(&bad_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        resp["error"].as_str().unwrap().contains("http(s)"),
        "{resp}"
    );
}

#[tokio::test]
async fn an_empty_source_is_not_reported_as_ready_by_the_probe() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: vec![NewsSource {
                    alias: "空".into(),
                    url: format!("{}/empty", fake.base),
                    kind: NewsKind::Rss,
                    max_items: 3,
                }],
                default_alias: String::new(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
    let probe: Value = reqwest::Client::new()
        .post(format!("{base}/api/v0/config/services/news/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["state"], "failed", "{probe}");
    assert_eq!(probe["sources"][0]["state"], "empty", "{probe}");
    assert_eq!(probe["sources"][0]["code"], "NEWS_EMPTY", "{probe}");
}

#[tokio::test]
async fn a_truncated_feed_is_a_parse_failure_not_an_empty_feed() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: vec![NewsSource {
                    alias: "截断".into(),
                    url: format!("{}/truncated", fake.base),
                    kind: NewsKind::Rss,
                    max_items: 3,
                }],
                default_alias: String::new(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
    let r = state
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_PARSE", "{r:?}");
}

#[tokio::test]
async fn a_stalled_response_body_is_a_named_timeout() {
    // Headers arrive, then the body never does: the named failure must be
    // NEWS_TIMEOUT, not NEWS_OFFLINE (audit P2).
    //
    // Read the request before answering. A server that writes a response as
    // soon as the socket accepts races the client's request bytes; hyper
    // then fails the call with UnexpectedMessage, which the adapter reports
    // as NEWS_OFFLINE. That is a broken peer, not a stalled body.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let mut seen = 0usize;
        loop {
            if seen == buf.len() {
                break;
            }
            let n = s.read(&mut buf[seen..]).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            seen += n;
            if buf[..seen].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ = s
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/rss+xml\r\nConnection: close\r\n\r\n")
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });
    let (_base, state) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: vec![NewsSource {
                    alias: "卡住".into(),
                    url: format!("http://{addr}/feed"),
                    kind: NewsKind::Rss,
                    max_items: 3,
                }],
                default_alias: String::new(),
                timeout_ms: 500,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();
    let r = state
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.error.as_ref().unwrap().code, "NEWS_TIMEOUT", "{r:?}");
}

/// M7-3 (2026-11-13): with several sources configured and NO default, a bare
/// "今天有什么新闻" must be answered by merging them — not by asking the model
/// which one. Items are interleaved and each names its source, so one feed
/// cannot fill the answer.
#[tokio::test]
async fn a_bare_news_request_merges_all_sources_by_default() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: sources(&fake),
                default_alias: String::new(), // no default: merge
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();

    let r = state
        .registry
        .call("news.headlines", json!({ "limit": 5 }), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Completed, "{r:?}");
    let v = r.result.clone().unwrap();
    assert_eq!(v["merged"], json!(true), "{v}");
    let names: Vec<&str> = v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert_eq!(names.len(), 3, "all three readable sources: {v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 5, "the caller's cap is the merged total: {v}");
    // Interleaved: the first three come from three DIFFERENT sources.
    let first3: std::collections::HashSet<&str> = items
        .iter()
        .take(3)
        .map(|i| i["source"].as_str().unwrap())
        .collect();
    assert_eq!(first3.len(), 3, "round-robin, not one feed first: {v}");
    assert!(items.iter().all(|i| i["source"].is_string()));
    // Every configured feed was actually read (no hidden default).
    assert_eq!(fake.count("/rss"), 1);
    assert_eq!(fake.count("/atom"), 1);
    assert_eq!(fake.count("/json"), 1);
    let _ = base;
}

/// One broken feed does not poison the merged answer: the good ones are read,
/// the failure is NAMED in the same result, and the tool still completes.
#[tokio::test]
async fn a_broken_feed_does_not_poison_the_merged_default() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let mut srcs = sources(&fake);
    srcs.push(NewsSource {
        alias: "坏".into(),
        url: format!("{}/broken", fake.base),
        kind: NewsKind::Rss,
        max_items: 3,
    });
    ConfigService::new(state.store.clone())
        .save_news(
            NewsConfig {
                enabled: true,
                sources: srcs,
                default_alias: String::new(),
                timeout_ms: 2000,
                ..NewsConfig::default()
            },
            "test",
        )
        .unwrap();

    let r = state
        .registry
        .call("news.headlines", json!({}), &ctx())
        .await;
    assert_eq!(r.status, ToolStatus::Completed, "{r:?}");
    let v = r.result.clone().unwrap();
    assert_eq!(v["merged"], json!(true));
    let failed = v["failed_sources"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "the broken feed is named: {v}");
    assert_eq!(failed[0]["alias"], json!("坏"));
    assert!(!v["items"].as_array().unwrap().is_empty());
    let _ = base;
}

#[tokio::test]
async fn configurable_max_sources_allows_more_than_five_and_enforces_limit() {
    let fake = spawn_feeds(Mode::Ok).await;
    let (base, state) = spawn_app(FakeLlm::new(vec![])).await;
    let make_sources = |n: usize| -> Vec<NewsSource> {
        (0..n)
            .map(|i| NewsSource {
                alias: format!("源{i}"),
                url: format!("{}/rss?i={i}", fake.base),
                kind: NewsKind::Rss,
                max_items: 3,
            })
            .collect()
    };

    // Default max_sources (5) rejects 6 sources.
    let svc = ConfigService::new(state.store.clone());
    assert!(svc
        .save_news(
            NewsConfig {
                enabled: true,
                sources: make_sources(6),
                ..NewsConfig::default()
            },
            "test",
        )
        .is_err());

    // Raising max_sources to 12 via PUT allows 8 sources and persists max_sources=12.
    let client = reqwest::Client::new();
    let put_ok: Value = client
        .put(format!("{base}/api/v0/config/services/news"))
        .json(&json!({
            "enabled": true,
            "max_sources": 12,
            "sources": make_sources(8),
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put_ok["max_sources"], 12);
    assert_eq!(put_ok["sources"].as_array().unwrap().len(), 8);

    // Exceeding the configured max_sources (13 > 12) is rejected with 400.
    let resp_too_many = client
        .put(format!("{base}/api/v0/config/services/news"))
        .json(&json!({
            "enabled": true,
            "max_sources": 12,
            "sources": make_sources(13),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp_too_many.status().as_u16(), 400);

    // Config Delivery v1 preflight also accepts replacing > 5 news.sources when
    // news.max_sources is raised in the same package (exercising Block::News.checked_value).
    let cd = hm_core::config_delivery::ConfigDelivery::new(state.store.clone());
    let enrolled = cd.enroll("dot2-news").unwrap();
    let hh = enrolled["household_id"].as_str().unwrap().to_string();
    let node_id = enrolled["node_id"].as_str().unwrap().to_string();
    let pkg = hm_core::config_delivery::Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: "00000000-0000-4000-8000-000000000099".into(),
        package_revision: 1,
        created_ms: 1,
        purpose: hm_core::config_delivery::Purpose::FamilyPolicy,
        source: hm_core::config_delivery::Source {
            node_id,
            config_revision: 0,
            core_version: "0.1.0".into(),
            schema_version: 2,
        },
        target: hm_core::config_delivery::Target {
            household_id: hh,
            node_id: None,
        },
        compatibility: hm_core::config_delivery::Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 2,
            required_capabilities: vec![],
        },
        change_reason: "test news max_sources > 5 via v1".into(),
        blocks: vec![hm_core::config_delivery::Block {
            name: "news".into(),
            schema_version: 1,
            operations: vec![
                hm_core::config_delivery::Operation::Set {
                    path: "news.max_sources".into(),
                    value: json!(10),
                },
                hm_core::config_delivery::Operation::Replace {
                    path: "news.sources".into(),
                    value: json!(make_sources(7)),
                },
            ],
        }],
        bindings: vec![],
        content_hash: None,
    };
    let preflight = cd
        .preflight("test", pkg, json!({}))
        .expect("v1 preflight should accept > 5 news.sources when max_sources is raised");
    assert!(
        preflight["plan_id"].as_str().is_some_and(|s| !s.is_empty()),
        "expected plan_id in preflight response: {preflight}"
    );
}
