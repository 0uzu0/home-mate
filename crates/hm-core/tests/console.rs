//! Console file serving: cache policy.
//!
//! A release replaces the bundle under the same `/` path. If the entry
//! document is cacheable, a browser keeps an old bundle running against a
//! newer API — measured 2026-11-14 as 269 `api_auth_denied` polls from one
//! stale tab after the auth release. Hashed assets are the opposite: they may
//! be cached forever because the name changes with the content.

use hm_core::api::{router, AppState};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

async fn spawn(html: &str, asset: &str) -> (String, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "hm-console-test-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    std::fs::create_dir_all(dir.join("web/assets")).unwrap();
    std::fs::write(dir.join("web/index.html"), html).unwrap();
    std::fs::write(dir.join("web/assets/index-abc123.js"), asset).unwrap();
    let state = AppState::with_overrides(
        Arc::new(Store::open_in_memory().unwrap()),
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        Some(dir.join("web")),
    );
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (format!("http://127.0.0.1:{port}"), dir)
}

async fn cache_of(base: &str, path: &str) -> (u16, Option<String>) {
    let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = resp.status().as_u16();
    let cache = resp
        .headers()
        .get("cache-control")
        .map(|v| v.to_str().unwrap().to_string());
    (status, cache)
}

#[tokio::test]
async fn the_entry_document_revalidates_and_hashed_assets_are_immutable() {
    let (base, dir) = spawn(
        "<!doctype html><script src=\"/assets/index-abc123.js\"></script>",
        "console.log(1)",
    )
    .await;
    let (status, cache) = cache_of(&base, "/").await;
    assert_eq!(status, 200);
    assert_eq!(cache.as_deref(), Some("no-cache"));

    // A deep link is the same entry document (the SPA route).
    let (status, cache) = cache_of(&base, "/status").await;
    assert_eq!(status, 200);
    assert_eq!(
        cache.as_deref(),
        Some("no-cache"),
        "SPA routes revalidate too"
    );

    let (status, cache) = cache_of(&base, "/assets/index-abc123.js").await;
    assert_eq!(status, 200);
    assert!(cache.unwrap().contains("immutable"));
    let _ = std::fs::remove_dir_all(dir);
}
