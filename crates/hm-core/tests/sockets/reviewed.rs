use super::*;
use axum::http::{HeaderMap, StatusCode};
use hm_core::config_delivery::{uuid, ConfigWorker};
use hm_core::tool::{remote_socket::ComputerQueryTool, Tool, ToolCtx, ToolResult, ToolStatus};
use std::{sync::atomic::AtomicBool, time::Duration};

const ROUTE: &str = "/api/v0/config/services/remote_socket";
const TOKEN: &str = "review-fixture-remote-secret";
const FUTURE_TOKEN: &str = "later-fixture-remote-secret";

async fn response(request: reqwest::RequestBuilder) -> Value {
    let resp = request.send().await.unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}

async fn reject(request: reqwest::RequestBuilder, status: u16, reason: &str) {
    let resp = request.send().await.unwrap();
    let actual = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(actual, status, "{reason}: {body}");
    assert!(body.contains(reason), "{body}");
    assert!(
        !body.contains(TOKEN) && !body.contains(FUTURE_TOKEN),
        "{body}"
    );
}

#[tokio::test]
async fn reviewed_remote_socket_publishes_rolls_back_and_fails_closed_at_the_actual_reader() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = Router::new()
        .route(
            "/tools",
            get(|headers: HeaderMap| async move {
                if headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    != Some(concat!("Bearer ", "review-fixture-remote-secret"))
                {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                Ok(Json(
                    json!({"tools":[{"name":"status","description":TOKEN}]}),
                ))
            }),
        )
        .route(
            "/call",
            post(
                |AxumState(calls): AxumState<Arc<Mutex<Vec<Value>>>>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    if headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        != Some(concat!("Bearer ", "review-fixture-remote-secret"))
                    {
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                    calls.lock().unwrap().push(body.clone());
                    Ok(Json(json!({
                        "status":"completed", "evidence":"observed",
                        "result":{"tool":body["tool"], "echo":TOKEN}
                    })))
                },
            ),
        )
        .with_state(calls.clone());
    let partner = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", partner.local_addr().unwrap());
    let partner_task = tokio::spawn(async move { axum::serve(partner, server).await.unwrap() });

    let (_, state) = spawn_app(FakeLlm::new(vec![])).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = router(state.clone());
    let api_task = tokio::spawn(async move {
        axum::serve(
            listener,
            api.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::new();
    let paired = response(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let bearer = paired["token"].as_str().unwrap();
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(bearer);
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(bearer);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(bearer);
    let preflight = |remote: Value| {
        post("/api/v1/config/preflight").json(&json!({
            "package":{"format":"home-mate-config","version":1,"remote_socket":remote}
        }))
    };
    let apply = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"],"application_id":uuid().unwrap()
        }))
    };
    let readback = |receipt: &Value| {
        let id = receipt["application_id"].as_str().unwrap();
        get(&format!("/api/v1/config/applications/{id}/readback"))
    };
    let wait = async |receipt: &Value, expected: &str| {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let evidence = response(readback(receipt)).await;
                let pub_status = evidence["publications"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["block"] == "remote_socket")
                    .unwrap();
                if pub_status["state"] == expected {
                    return evidence;
                }
                assert_eq!(pub_status["state"], "saved_pending", "{evidence}");
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .unwrap()
    };
    let execute = async |tool: &str, capability: &str, args: Value| -> ToolResult {
        let ctx = ToolCtx::new(
            "reviewed-remote",
            uuid().unwrap(),
            Arc::new(AtomicBool::new(false)),
        );
        state
            .registry
            .call(
                tool,
                json!({"capability": capability, "arguments": args}),
                &ctx,
            )
            .await
    };
    let hits = || calls.lock().unwrap().len();

    let saved = response(put(ROUTE).json(&json!({
        "enabled":true, "endpoint_url":endpoint, "service_name":"Fixture before",
        "timeout_ms":1700, "token":TOKEN
    })))
    .await;
    assert_eq!(saved["token_set"], true);
    assert!(!saved.to_string().contains(TOKEN));
    assert_eq!(response(post(&format!("{ROUTE}/test"))).await["ok"], true);
    let discovered = response(post(&format!("{ROUTE}/discover"))).await;
    assert_eq!(discovered["capabilities"][0]["description"], "[redacted]");
    let actual = execute("computer.query", "legacy-status", json!({})).await;
    assert_eq!(actual.status, ToolStatus::Completed);
    assert_eq!(actual.result.unwrap()["echo"], "[redacted]");

    response(put("/api/v0/fleet").json(&json!({"self_id":"remote-fixture"}))).await;
    response(put("/api/v1/config/enrollment").json(&json!({}))).await;
    let exports = |purpose: &str| {
        post("/api/v1/config/exports").json(&json!({
            "format":"v2","purpose":purpose,"blocks":["remote_socket"]
        }))
    };
    let family_export = response(exports("family_policy")).await;
    assert_eq!(family_export["package"]["blocks"], json!([]));
    let v1_family = response(post("/api/v1/config/exports").json(&json!({
        "format":"v1","purpose":"family_policy","blocks":["remote_socket"]
    })))
    .await;
    assert!(v1_family["document"].get("remote_socket").is_none());
    let v1_node = response(post("/api/v1/config/exports").json(&json!({
        "format":"v1","purpose":"node_config","blocks":["remote_socket"]
    })))
    .await;
    assert_eq!(
        v1_node["document"]["remote_socket"]["source_node"],
        "remote-fixture"
    );
    let restored_plan =
        response(post("/api/v1/config/preflight").json(&json!({"package":v1_node["document"]})))
            .await;
    assert!(restored_plan["plan_id"].is_string());
    let node_export = response(exports("node_config")).await;
    assert_eq!(node_export["package"]["blocks"][0]["name"], "remote_socket");
    assert!(!node_export.to_string().contains(TOKEN));
    let mut wrong_scope = node_export["package"].clone();
    wrong_scope.as_object_mut().unwrap().remove("content_hash");
    wrong_scope["purpose"] = json!("family_policy");
    wrong_scope["target"]
        .as_object_mut()
        .unwrap()
        .remove("node_id");
    reject(
        post("/api/v1/config/preflight").json(&json!({"package":wrong_scope})),
        400,
        "FORBIDDEN_SCOPE",
    )
    .await;
    reject(
        preflight(json!({"endpoint_url":"https://other.example"})),
        409,
        "REMOTE_SOCKET_SOURCE_BINDING_CONFLICT",
    )
    .await;
    reject(
        put(ROUTE).json(&json!({"endpoint_url":"https://other.example"})),
        409,
        "REMOTE_SOCKET_SOURCE_BINDING_CONFLICT",
    )
    .await;
    reject(
        preflight(
            json!({"capabilities":[{"name":"status","parameters":{"properties":{(TOKEN):{}}}}]}),
        ),
        400,
        "REMOTE_SOCKET_SECRET_IN_PUBLIC_CONFIG",
    )
    .await;
    reject(
        preflight(json!({"capabilities":[{"name":"status"},{"name":"status"}]})),
        400,
        "INVALID_REMOTE_SOCKET_CONFIG",
    )
    .await;
    reject(
        preflight(json!({"endpoint_url":"https:///"})),
        400,
        "INVALID_REMOTE_SOCKET_CONFIG",
    )
    .await;

    reject(
        preflight(json!({"capabilities":[{"name":"status","parameters":{
            "type":"object","properties":{"room":{"pattern":"^desk$"}}
        }}]})),
        400,
        "INVALID_REMOTE_SOCKET_CONFIG",
    )
    .await;
    let admitted = ComputerQueryTool::new(state.remote_socket.clone());
    let advertised = state.registry.catalog().unwrap();
    let wire = advertised
        .specs()
        .into_iter()
        .find(|spec| spec.canonical_name == "computer.query")
        .unwrap()
        .name;
    let ctx = ToolCtx::new(
        "pinned-remote",
        uuid().unwrap(),
        Arc::new(AtomicBool::new(false)),
    );
    let before_calls = hits();
    let plan = response(preflight(json!({
        "service_name":"Fixture after", "timeout_ms":1300,
        "capabilities":[{"name":"status","description":"Temperature status", "parameters":{
            "type":"object", "required":["room"], "additionalProperties":false,
            "properties":{
                "room":{"type":"string"},
                "filter":{"type":"object","required":["scale"],"additionalProperties":false,
                    "properties":{"scale":{"enum":["room","unit"]}}},
                "tags":{"type":"array","items":{"type":"string"}},
                "debug":false
            }
        }},{"name":"acknowledge","side_effect":"Mutate"}]
    })))
    .await;
    let inserted = response(apply(&plan)).await;
    assert_eq!(inserted["state"], "saved_pending");
    assert_eq!(response(get(ROUTE)).await, saved);
    assert_eq!(
        response(readback(&inserted)).await["publication"]["state"],
        "saved_pending"
    );
    assert_eq!(
        execute("computer.query", "legacy-status", json!({}))
            .await
            .status,
        ToolStatus::Completed
    );
    assert_eq!(hits(), before_calls + 1);

    let _worker = ConfigWorker::start(state.store.clone());
    let applied = wait(&inserted, "applied").await;
    assert_eq!(applied["remote_health"], "not_probed");
    assert!(applied["fields"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["state"] == "core_readback_verified"));
    let stale = state
        .registry
        .call_advertised(
            &advertised,
            &wire,
            json!({"capability":"legacy-status"}),
            &ctx,
        )
        .await;
    assert_eq!(stale.error.unwrap().code, "TOOL_STALE");
    assert_eq!(hits(), before_calls + 1);
    let status = response(get(ROUTE)).await;
    assert_eq!(status["service_name"], "Fixture after");
    assert_eq!(status["timeout_ms"], 1300);
    assert_eq!(status["token_set"], true);
    assert_ne!(status["revision"], saved["revision"]);
    let v0 = response(get("/api/v0/config/export")).await;
    assert_eq!(v0["remote_socket"]["capabilities"], status["capabilities"]);
    assert!(!v0.to_string().contains(TOKEN));
    reject(
        put(ROUTE).json(&json!({"revision":saved["revision"],"service_name":"stale"})),
        409,
        "REMOTE_SOCKET_REVISION_CONFLICT",
    )
    .await;
    for (tool, capability, args) in [
        ("computer.query", "legacy-status", json!({})),
        ("computer.action", "status", json!({"room":"desk"})),
        ("computer.query", "status", json!({})),
        ("computer.query", "status", json!({"room":4})),
        (
            "computer.query",
            "status",
            json!({"room":"desk","extra":true}),
        ),
        (
            "computer.query",
            "status",
            json!({"room":"desk","filter":{}}),
        ),
        (
            "computer.query",
            "status",
            json!({"room":"desk","filter":{"scale":"other"}}),
        ),
        (
            "computer.query",
            "status",
            json!({"room":"desk","filter":{"scale":"room","extra":true}}),
        ),
        (
            "computer.query",
            "status",
            json!({"room":"desk","tags":[1]}),
        ),
        (
            "computer.query",
            "status",
            json!({"room":"desk","debug":false}),
        ),
    ] {
        let result = execute(tool, capability, args).await;
        assert_eq!(result.error.unwrap().code, "REMOTE_SOCKET_ARGUMENTS");
    }
    assert_eq!(hits(), before_calls + 1);
    let valid = execute(
        "computer.query",
        "status",
        json!({"room":"desk","filter":{"scale":"room"},"tags":["current"]}),
    )
    .await;
    assert_eq!(valid.status, ToolStatus::Completed);
    assert_eq!(valid.result.unwrap()["tool"], "status");
    assert_eq!(
        execute("computer.action", "acknowledge", json!({}))
            .await
            .status,
        ToolStatus::Unknown
    );
    assert_eq!(hits(), before_calls + 3);

    let id = inserted["application_id"].as_str().unwrap();
    let revision = inserted["config_revision"].clone();
    let rollback = response(
        post(&format!(
            "/api/v1/config/applications/{id}/rollback-preflight"
        ))
        .json(&json!({"expected_revision":revision})),
    )
    .await;
    let reversed = response(apply(&rollback)).await;
    let restored = wait(&reversed, "applied").await;
    assert!(restored["fields"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["state"] == "core_readback_verified"));
    let mut saved = saved;
    let restored_status = response(get(ROUTE)).await;
    saved["revision"] = restored_status["revision"].clone();
    assert_eq!(restored_status, saved);
    assert_eq!(
        execute("computer.query", "legacy-status", json!({}))
            .await
            .status,
        ToolStatus::Completed
    );

    let prepared = response(preflight(json!({"service_name":FUTURE_TOKEN}))).await;
    reject(
        put(ROUTE).json(&json!({"token":FUTURE_TOKEN})),
        400,
        "REMOTE_SOCKET_SECRET_IN_PUBLIC_CONFIG",
    )
    .await;
    assert_eq!(response(get(ROUTE)).await, saved);
    assert_eq!(response(post(&format!("{ROUTE}/test"))).await["ok"], true);
    assert!(prepared["plan_id"].is_string());

    state
        .store
        .with(|c| {
            c.execute_batch(
                "CREATE TRIGGER fixture_fail_remote_update BEFORE UPDATE ON config_kv
            WHEN new.key='remote_socket' BEGIN SELECT RAISE(ABORT, 'fixture storage error'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    let failed_plan = response(preflight(json!({"service_name":"Unavailable publication"}))).await;
    let attempt = response(apply(&failed_plan)).await;
    let failed = wait(&attempt, "failed").await;
    assert_eq!(
        failed["publication"]["error"],
        "REMOTE_SOCKET_PUBLICATION_FAILED"
    );
    assert_eq!(response(get(ROUTE)).await, saved);
    reject(
        put(ROUTE).json(
            &json!({"service_name":"Unavailable local write","token":"replacement-test-token"}),
        ),
        500,
        "fixture storage error",
    )
    .await;
    assert_eq!(response(get(ROUTE)).await, saved);
    assert_eq!(response(post(&format!("{ROUTE}/test"))).await["ok"], true);

    state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fixture_fail_remote_update;")?;
            Ok(())
        })
        .unwrap();
    let blank_binding = response(put(ROUTE).json(&json!({"token":""}))).await;
    assert_eq!(blank_binding["token_set"], false);

    let (alternate, alternate_calls, alternate_task) = spawn_mock_remote().await;
    let endpoint_plan = response(preflight(json!({
        "endpoint_url": alternate, "enabled": false
    })))
    .await;
    let change = response(apply(&endpoint_plan)).await;
    let evidence = wait(&change, "applied").await;
    assert!(evidence["fields"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f["state"] == "core_readback_verified"));
    assert_eq!(response(get(ROUTE)).await["endpoint_url"], alternate);
    let disabled = execute("computer.query", "clipboard.read", json!({})).await;
    assert_eq!(disabled.error.unwrap().code, "REMOTE_SOCKET_DISABLED");
    assert!(alternate_calls.lock().unwrap().is_empty());

    let enable_plan = response(preflight(json!({"enabled":true}))).await;
    let enable = response(apply(&enable_plan)).await;
    wait(&enable, "applied").await;
    let alternate_result = execute("computer.query", "clipboard.read", json!({})).await;
    assert_eq!(alternate_result.status, ToolStatus::Completed);
    assert_eq!(alternate_calls.lock().unwrap().len(), 1);

    let before_pinned = hits();
    let old = admitted
        .execute(json!({"capability":"legacy-status"}), &ctx)
        .await;
    assert_eq!(old.status, ToolStatus::Completed);
    assert_eq!(old.result.unwrap()["echo"], "[redacted]");
    assert_eq!(hits(), before_pinned + 1);
    assert_eq!(alternate_calls.lock().unwrap().len(), 1);

    let legacy_doc = json!({"format":"home-mate-config","version":1,"remote_socket": {
        "endpoint_url":alternate, "enabled":true, "service_name":"Restored via legacy import",
        "token_set":true, "source_node":"remote-fixture"
    }});
    let skipped = response(
        post("/api/v0/config/import")
            .header("x-home-mate-source", "another-fixture")
            .json(&legacy_doc),
    )
    .await;
    assert_eq!(skipped["applied"], json!([]));
    let mut foreign = legacy_doc.clone();
    foreign["remote_socket"]["source_node"] = json!("some-other-core");
    reject(
        post("/api/v0/config/import").json(&foreign),
        409,
        "REMOTE_SOCKET_RESTORE_TARGET_MISMATCH",
    )
    .await;
    reject(
        preflight(foreign["remote_socket"].clone()),
        409,
        "REMOTE_SOCKET_RESTORE_TARGET_MISMATCH",
    )
    .await;
    assert_ne!(
        response(get(ROUTE)).await["service_name"],
        "Restored via legacy import"
    );
    let imported = response(post("/api/v0/config/import").json(&legacy_doc)).await;
    assert_eq!(imported["applied"], json!(["remote_socket"]));
    let imported_status = response(get(ROUTE)).await;
    assert_eq!(
        imported_status["service_name"],
        "Restored via legacy import"
    );
    assert_eq!(imported_status["token_set"], false);

    let quarantined = json!({
        "enabled":true, "endpoint_url":alternate, "capabilities":[{"name":"legacy-rule",
            "parameters":{"type":"object","properties":{"query":{"type":"string","pattern":"legacy"}}}
        }]
    }).to_string();
    state
        .store
        .config_set("remote_socket", &quarantined)
        .unwrap();
    let backup = response(get("/api/v0/config/export")).await;
    assert!(
        backup.get("remote_socket").is_none(),
        "unreadable legacy data must not export as defaults"
    );
    let restored = response(post("/api/v0/config/import").json(&backup)).await;
    assert!(restored["applied"]
        .as_array()
        .unwrap()
        .iter()
        .all(|block| block != "remote_socket"));
    assert_eq!(
        state.store.config_get("remote_socket").unwrap().as_deref(),
        Some(quarantined.as_str())
    );

    alternate_task.abort();
    partner_task.abort();
    api_task.abort();
}
