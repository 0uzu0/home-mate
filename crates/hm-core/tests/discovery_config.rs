//! Exercise reviewed publication and local edits with actual HTTP consumers.
use axum::{
    extract::Path,
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use hm_core::{
    api::{router, AppState},
    config_delivery::{uuid, ConfigWorker},
    store::Store,
    tool::mocks::MockInject,
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    format!("http://{addr}")
}

async fn read(request: RequestBuilder) -> Value {
    let res = request.send().await.unwrap();
    let status = res.status();
    let data = res.json::<Value>().await.unwrap();
    assert!(status.is_success(), "{status}: {data}");
    data
}

fn submission(plan: &Value) -> Value {
    json!({
        "plan_id": plan["plan_id"], "plan_hash": plan["plan_hash"],
        "expected_revision": plan["expected_revision"], "application_id": uuid().unwrap()
    })
}

#[tokio::test]
async fn discovery_packages_publish_and_rollback_through_real_probes() {
    let partner = serve(Router::new()
        .route("/{target}/feed", get(|Path(target): Path<String>| async move {
            Json(json!({"items": [{"title": format!("{target} headline"), "link":"https://example.org/news"}]}))
        }))
        .route("/{target}/search", post(|Path(target): Path<String>, headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer discovery-test-key");
            assert_eq!(body["count"], if target == "old" { 1 } else { 2 });
            Json(json!({"results": [{"title": format!("{target} search"), "url":"https://example.org/search"}]}))
        }))).await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::new(
        store.clone(),
        std::env::temp_dir().join(format!("hm-discovery-{}", uuid().unwrap())),
        MockInject::default(),
        None,
    );
    let news_runtime = state.news.clone();
    let base = serve(router(state)).await;
    let client = Client::new();
    let pairing = read(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let token = pairing["token"].as_str().unwrap();
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(token);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(token);
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(token);
    let preflight =
        |package: Value| post("/api/v1/config/preflight").json(&json!({"package": package}));
    let apply = |plan: &Value| post("/api/v1/config/applications").json(&submission(plan));
    let news_path = "/api/v0/config/services/news";
    let search_path = "/api/v0/config/services/search";
    let sources = |target: &str| {
        json!([{
            "alias": "home", "url": format!("{partner}/{target}/feed"), "kind": "json", "max_items": 3
        }])
    };
    read(put("/api/v0/fleet").json(&json!({"self_id":"discovery-test"}))).await;
    read(put("/api/v1/config/enrollment").json(&json!({}))).await;
    read(
        put(news_path)
            .json(&json!({"enabled":true,"sources":sources("old"),"default_alias":"home"})),
    )
    .await;
    let search_initial = read(put(search_path).json(&json!({
        "enabled":true, "base_url":format!("{partner}/old/search"), "count":1, "api_key":"discovery-test-key"
    }))).await;

    let export = read(
        post("/api/v1/config/exports").json(&json!({"format":"v2","blocks":["news","search"]})),
    )
    .await;
    assert!(!export.to_string().contains("discovery-test-key"));
    read(preflight(export["package"].clone())).await;
    let export_v1 = read(
        post("/api/v1/config/exports").json(&json!({"format":"v1","blocks":["news","search"]})),
    )
    .await;
    let migrated = read(preflight(export_v1["document"].clone())).await;
    assert!(migrated["migration_report"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["pointer"] == "/search/max_results" && row["action"] == "ignored_metadata"));
    let invalid = preflight(json!({
        "format":"home-mate-config", "version":1,
        "news":{"sources":sources("new")},
        "search":{"count":51}
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read(get(news_path)).await["sources"], sources("old"));
    let mut new_sources = sources("new");
    new_sources[0].as_object_mut().unwrap().remove("max_items");
    let plan = read(preflight(json!({
        "format":"home-mate-config", "version":1,
        "news":{"sources":new_sources},
        "search":{"base_url":format!("{partner}/new/search"),"count":2}
    })))
    .await;
    let receipt = read(apply(&plan)).await;
    assert_eq!(receipt["state"], "saved_pending");
    let probe = async |prefix: &str| {
        let search = read(post("/api/v0/config/services/search/test")).await;
        assert_eq!(search["ok"], true, "{search}");
        assert_eq!(search["samples"][0]["title"], format!("{prefix} search"));
        let news = read(post("/api/v0/config/services/news/test")).await;
        assert_eq!(news["ok"], true, "{news}");
        let headlines = news_runtime.headlines(None, None).await.unwrap();
        assert!(
            headlines
                .to_string()
                .contains(&format!("{prefix} headline")),
            "{headlines}"
        );
    };
    probe("old").await;
    let _worker = ConfigWorker::start(store.clone());
    let published = async |receipt: &Value| {
        let id = receipt["application_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let result = read(get(&format!("/api/v1/config/applications/{id}/readback"))).await;
                let blocks = result["publications"].as_array().unwrap();
                assert_eq!(blocks.len(), 2);
                if blocks.iter().all(|block| block["state"] == "applied") {
                    assert!(result["fields"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|field| field["state"] == "core_readback_verified"));
                    assert!(!result.to_string().contains("discovery-test-key"));
                    break;
                }
                assert!(
                    blocks.iter().all(|block| matches!(
                        block["state"].as_str(),
                        Some("saved_pending" | "applied")
                    )),
                    "{result}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both publications must settle");
    };
    published(&receipt).await;
    probe("new").await;
    let conflict = put(search_path)
        .json(&json!({"revision":search_initial["revision"], "count": 4}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    let id = receipt["application_id"].as_str().unwrap();
    let rollback = read(
        post(&format!(
            "/api/v1/config/applications/{id}/rollback-preflight"
        ))
        .json(&json!({"expected_revision":receipt["config_revision"]})),
    )
    .await;
    published(&read(apply(&rollback)).await).await;
    probe("old").await;

    store.with(|c| {
        c.execute_batch("CREATE TRIGGER reject_search_save BEFORE UPDATE ON config_kv WHEN OLD.key='search' BEGIN SELECT RAISE(ABORT, 'test search failure'); END")?;
        Ok(())
    }).unwrap();
    let before = read(get(search_path)).await;
    let failed = put(search_path)
        .json(&json!({"base_url":format!("{partner}/new/search"), "api_key":"replacement"}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read(get(search_path)).await["revision"], before["revision"]);
    probe("old").await;

    let marker = "discovery-config-query-secret";
    let mut unsafe_sources = sources("old");
    unsafe_sources[0]["url"] = json!(format!("{partner}/old/feed?token={marker}"));
    store
        .config_set("news", &json!({"sources":unsafe_sources}).to_string())
        .unwrap();
    let before = read(get("/api/v1/config/state")).await;
    assert_eq!(
        before["capabilities"]["unavailable_blocks"],
        json!(["news"])
    );
    assert!(!before.to_string().contains(marker));
    for format in ["v1", "v2"] {
        let export = read(post("/api/v1/config/exports").json(&json!({"format":format}))).await;
        assert!(export["excluded"]
            .as_array()
            .unwrap()
            .contains(&json!("news_unavailable")));
        assert!(!export.to_string().contains(marker));
    }
    let blocked = preflight(json!({
        "format":"home-mate-config","version":1,"news":{"enabled":true}
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    read(preflight(json!({
        "format":"home-mate-config","version":1,"search":{"count":3}
    })))
    .await;

    store
        .with(|c| {
            c.execute_batch("DROP TRIGGER reject_search_save")?;
            Ok(())
        })
        .unwrap();
    store
        .config_set("search", &json!({"count": 51}).to_string())
        .unwrap();
    let unavailable = read(get("/api/v1/config/state")).await;
    assert_eq!(
        unavailable["capabilities"]["unavailable_blocks"],
        json!(["news", "search"])
    );
    let export = read(post("/api/v1/config/exports").json(&json!({"format":"v2"}))).await;
    assert!(export["excluded"]
        .as_array()
        .unwrap()
        .contains(&json!("search_unavailable")));
    read(preflight(export["package"].clone())).await;
}
