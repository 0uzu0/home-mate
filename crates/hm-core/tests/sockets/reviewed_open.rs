use super::*;
use axum::http::HeaderMap;
use hm_core::config_delivery::{uuid, ConfigApplyCoordinator};
use hm_core::tool::{
    DynamicToolProvider, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult, ToolStatus,
};
use std::{sync::atomic::AtomicBool, time::Duration};

const TOKEN: &str = "open-socket-fixture-local-secret";

struct CompetingTool(&'static str);

#[async_trait::async_trait]
impl Tool for CompetingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.0.into(),
            service: "fixture".into(),
            side_effect: SideEffect::Query,
            description: "Competing registry tool".into(),
            parameters: json!({"type":"object"}),
        }
    }
    async fn execute(&self, _: Value, _: &ToolCtx) -> ToolResult {
        ToolResult::observed_ok(json!({"origin":"competing"}))
    }
}

struct ConflictingProvider;

impl DynamicToolProvider for ConflictingProvider {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(CompetingTool("desk.later"))]
    }
}

async fn response(request: reqwest::RequestBuilder, status: u16) -> Value {
    let resp = request.send().await.unwrap();
    let actual = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(actual, status, "{body}");
    assert!(!body.to_string().contains(TOKEN));
    body
}

fn socket(name: &str, url: &str) -> Value {
    json!({"name": name, "description": format!("Read {name}"),
        "effect":"Query","endpoint_url":url})
}

#[tokio::test]
async fn whole_socket_registry_publishes_real_tools_and_rolls_back_with_local_credentials() {
    let partner = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", partner.local_addr().unwrap());
    let interactions: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let partner_app = Router::new()
        .route(
            "/{point}",
            get(
                |axum::extract::Path(point): axum::extract::Path<String>,
                 AxumState(rows): AxumState<Arc<Mutex<Vec<(String, bool)>>>>,
                 headers: HeaderMap| async move {
                    let authenticated = headers.get("authorization").is_some_and(|value| {
                        value
                            .to_str()
                            .is_ok_and(|value| value.strip_prefix("Bearer ") == Some(TOKEN))
                    });
                    rows.lock().unwrap().push((point.clone(), authenticated));
                    Json(json!({"point":point,"authenticated":authenticated}))
                },
            ),
        )
        .with_state(interactions.clone());
    let partner_task = tokio::spawn(async move {
        axum::serve(partner, partner_app).await.unwrap();
    });
    let scratch = std::env::temp_dir().join(format!("hm-reviewed-sockets-{}", uuid().unwrap()));
    std::fs::create_dir(&scratch).unwrap();
    let state = AppState::new(
        Arc::new(hm_core::store::Store::open_in_memory().unwrap()),
        scratch.clone(),
        MockInject::default(),
        Some(Arc::new(FakeLlm::new(vec![]))),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state.clone());
    let api_task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::new();
    let pair = response(client.post(format!("{base}/api/v0/auth/provision")), 200).await;
    let bearer = pair["token"].as_str().unwrap();
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(bearer);
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(bearer);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(bearer);
    let preflight = |list: Value| {
        post("/api/v1/config/preflight")
            .json(&json!({"package":{"format":"home-mate-config","version":1,"open_sockets":list}}))
    };
    let submit = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"],"application_id":uuid().unwrap(),
        }))
    };
    let wait = async |id: &str, target: &str| {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let proof = response(
                    get(&format!("/api/v1/config/applications/{id}/readback")),
                    200,
                )
                .await;
                let publication = proof["publications"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|block| block["block"] == "open_sockets")
                    .unwrap();
                if publication["state"] == target {
                    return proof;
                }
                assert_eq!(publication["state"], "saved_pending", "{proof}");
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .unwrap()
    };
    let invoke = async |name: &str, args: Value| {
        let ctx = ToolCtx::new(
            "registry-fixture",
            uuid().unwrap(),
            Arc::new(AtomicBool::new(false)),
        );
        state.registry.call(name, args, &ctx).await
    };

    let first = socket("desk.current", &format!("{endpoint}/first"));
    let mut initial = first.clone();
    initial["token"] = json!(TOKEN);
    response(post("/api/v0/config/sockets").json(&initial), 200).await;
    response(
        put("/api/v0/fleet").json(&json!({"self_id":"socket-fixture"})),
        200,
    )
    .await;
    response(put("/api/v1/config/enrollment").json(&json!({})), 200).await;
    let active = response(get("/api/v0/config/sockets"), 200).await;
    assert_eq!(active["sockets"][0]["token_set"], true);
    assert_eq!(
        invoke("desk.current", json!({})).await.result.unwrap()["authenticated"],
        true
    );

    let family = response(
        post("/api/v1/config/exports").json(&json!({
            "format":"v1","purpose":"family_policy","blocks":["open_sockets"]
        })),
        200,
    )
    .await;
    assert!(family["document"].get("open_sockets").is_none());
    let own = response(
        post("/api/v1/config/exports").json(&json!({
            "format":"v1","purpose":"node_config","blocks":["open_sockets"]
        })),
        200,
    )
    .await;
    assert_eq!(
        own["document"]["open_sockets_source_node"],
        "socket-fixture"
    );
    response(
        post("/api/v1/config/preflight").json(&json!({"package":own["document"]})),
        200,
    )
    .await;

    response(
        put("/api/v0/fleet").json(&json!({"self_id":"socket-fixture-renamed"})),
        200,
    )
    .await;
    let renamed = response(
        post("/api/v1/config/exports").json(&json!({
            "format":"v1","purpose":"node_config","blocks":["open_sockets"]
        })),
        200,
    )
    .await;
    assert!(renamed["document"].get("open_sockets").is_none());
    assert_eq!(
        response(
            post("/api/v1/config/preflight").json(&json!({"package":own["document"]})),
            409
        )
        .await["error"],
        "OPEN_SOCKETS_ENROLLED_NODE_MISMATCH"
    );
    response(
        put("/api/v0/fleet").json(&json!({"self_id":"socket-fixture"})),
        200,
    )
    .await;

    let other = socket("desk.extra", &format!("{endpoint}/extra"));
    let example = socket("desk.conflict", &format!("{endpoint}/conflict"));
    assert!(state
        .registry
        .register_dynamic(Arc::new(CompetingTool("desk.conflict"))));
    assert_eq!(
        response(preflight(json!([first, example])), 409).await["error"],
        "OPEN_SOCKETS_CATALOG_CONFLICT"
    );
    assert_eq!(
        response(preflight(json!([other])), 409).await["error"],
        "OPEN_SOCKETS_SOURCE_BINDING_CONFLICT"
    );
    let mut change_source = first.clone();
    change_source["endpoint_url"] = json!("https://elsewhere.example.invalid/second");
    assert_eq!(
        response(preflight(json!([change_source])), 409).await["error"],
        "OPEN_SOCKETS_SOURCE_BINDING_CONFLICT"
    );
    let mut leak = first.clone();
    leak["description"] = json!(TOKEN);
    assert_eq!(
        response(preflight(json!([leak])), 400).await["error"],
        "OPEN_SOCKETS_SECRET_IN_PUBLIC_CONFIG"
    );
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, active);

    let mut patch = first.clone();
    patch["description"] = json!("Revised real registry entry");
    patch["parameters"] = json!({"type":"object","required":["selector"],"additionalProperties":false,
        "properties":{"selector":{"type":"object","required":["room"], "properties":{"room":{"type":"string"}}}}});
    let plan = response(preflight(json!([patch, other])), 200).await;
    let initial_catalog = state.registry.catalog().unwrap();
    let wire = initial_catalog
        .specs()
        .into_iter()
        .find(|entry| entry.canonical_name == "desk.current")
        .unwrap()
        .name;
    let receipt = response(submit(&plan), 202).await;
    assert_eq!(receipt["state"], "saved_pending");
    let applied_id = receipt["application_id"].as_str().unwrap();
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, active);
    assert_eq!(
        wait(applied_id, "saved_pending").await["fields"][0]["state"],
        "saved_pending"
    );
    assert_eq!(
        invoke("desk.extra", json!({})).await.error.unwrap().code,
        "UNKNOWN_TOOL"
    );

    let worker = ConfigApplyCoordinator::start_for_app(&state);
    let published = wait(applied_id, "applied").await;
    assert_eq!(published["fields"][0]["state"], "core_readback_verified");
    assert_eq!(published["fields"][0]["value"][1]["name"], "desk.extra");
    let ctx = ToolCtx::new(
        "stale-fixture",
        "call-old",
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(
        state
            .registry
            .call_advertised(&initial_catalog, &wire, json!({}), &ctx)
            .await
            .error
            .unwrap()
            .code,
        "TOOL_STALE"
    );
    let prior_hits = interactions.lock().unwrap().len();
    assert_eq!(
        invoke("desk.current", json!({"selector":{}}))
            .await
            .error
            .unwrap()
            .code,
        "SOCKET_ARGUMENTS"
    );
    assert_eq!(interactions.lock().unwrap().len(), prior_hits);
    let live = invoke("desk.current", json!({"selector":{"room":"desk"}})).await;
    assert_eq!(live.status, ToolStatus::Completed);
    assert_eq!(live.result.unwrap()["authenticated"], true);
    assert_eq!(
        invoke("desk.extra", json!({})).await.result.unwrap()["point"],
        "extra"
    );

    let rollback = response(
        post(&format!(
            "/api/v1/config/applications/{applied_id}/rollback-preflight"
        ))
        .json(&json!({"expected_revision":receipt["config_revision"]})),
        200,
    )
    .await;
    let back = response(submit(&rollback), 202).await;
    let restored = wait(back["application_id"].as_str().unwrap(), "applied").await;
    assert_eq!(restored["fields"][0]["state"], "core_readback_verified");
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, active);
    assert_eq!(
        invoke("desk.current", json!({})).await.result.unwrap()["authenticated"],
        true
    );
    assert!(!state.registry.names().contains(&"desk.extra".into()));
    drop(worker);

    let later = socket("desk.later", &format!("{endpoint}/later"));
    let race_plan = response(preflight(json!([first, later])), 200).await;
    let race = response(submit(&race_plan), 202).await;
    assert!(state
        .registry
        .register_dynamic(Arc::new(CompetingTool("desk.later"))));
    let _worker = ConfigApplyCoordinator::start_for_app(&state);
    let failed = wait(race["application_id"].as_str().unwrap(), "failed").await;
    assert_eq!(
        failed["publication"]["error"],
        "OPEN_SOCKETS_CATALOG_CONFLICT"
    );
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, active);
    assert_eq!(
        invoke("desk.later", json!({})).await.result.unwrap()["origin"],
        "competing"
    );

    assert!(state
        .registry
        .attach_provider_for_repair(Arc::new(ConflictingProvider))
        .is_err());
    assert_eq!(
        response(preflight(json!([first, other])), 409).await["error"],
        "OPEN_SOCKETS_CATALOG_CONFLICT"
    );
    assert!(state.registry.unregister_dynamic("desk.later"));

    let incoming = Arc::new(Mutex::new(None));
    let receiver = Router::new()
        .route(
            "/api/v0/config/import",
            axum::routing::post(
                |AxumState(incoming): AxumState<Arc<Mutex<Option<Value>>>>,
                 Json(doc): Json<Value>| async move {
                    *incoming.lock().unwrap() = Some(doc);
                    Json(json!({"applied":[],"warnings":[]}))
                },
            ),
        )
        .with_state(incoming.clone());
    let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_url = format!("http://{}", peer.local_addr().unwrap());
    let peer_task = tokio::spawn(async move {
        axum::serve(peer, receiver).await.unwrap();
    });
    response(
        put("/api/v0/fleet/receiver-fixture").json(&json!({
            "base_url":peer_url, "token":"synthetic-fleet-private"
        })),
        200,
    )
    .await;
    response(
        post("/api/v0/fleet/push").json(&json!({"targets":["receiver-fixture"]})),
        200,
    )
    .await;
    let delivered = incoming.lock().unwrap().take().unwrap();
    for key in ["open_sockets", "open_sockets_source_node", "remote_socket"] {
        assert!(delivered.get(key).is_none(), "fleet forwarded {key}");
    }
    peer_task.abort();

    let old_backup = response(get("/api/v0/config/export"), 200).await;
    let copy = json!({"format":"home-mate-config","version":1,
        "open_sockets":old_backup["open_sockets"],"open_sockets_source_node":"another-fixture"});
    assert_eq!(
        response(post("/api/v0/config/import").json(&copy), 409).await["code"],
        "OPEN_SOCKETS_RESTORE_TARGET_MISMATCH"
    );
    let mut own_backup = copy;
    own_backup["open_sockets_source_node"] = json!("socket-fixture");
    own_backup["open_sockets"][0]["description"] = json!("Owner restore");
    let before_mode = response(get("/api/v0/mode"), 200).await;
    let mut bad_list = own_backup.clone();
    bad_list["mode_policy"] = json!({"assistant_name":"Mismatched fixture"});
    bad_list["open_sockets"] = json!({"not":"an array"});
    response(post("/api/v0/config/import").json(&bad_list), 400).await;
    assert_eq!(
        response(get("/api/v0/mode"), 200).await["assistant_name"],
        before_mode["assistant_name"]
    );
    let mut bad_reflex = own_backup.clone();
    bad_reflex["reflex"] = json!({"words":"not an array"});
    response(post("/api/v0/config/import").json(&bad_reflex), 400).await;
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, active);
    let result = response(post("/api/v0/config/import").json(&own_backup), 200).await;
    assert_eq!(result["applied"], json!(["open_sockets"]));
    let read = response(get("/api/v0/config/sockets"), 200).await;
    assert_eq!(read["sockets"][0]["description"], "Owner restore");
    assert_eq!(read["sockets"][0]["token_set"], true);

    state.store.with(|c| {
        c.execute_batch("CREATE TRIGGER fixture_open_history_fail BEFORE INSERT ON config_history
            WHEN new.scope='open_sockets' BEGIN SELECT RAISE(ABORT, 'fixture history failure'); END;")?;
        Ok(())
    }).unwrap();
    let storage_plan = response(preflight(json!([first, other])), 200).await;
    let attempt = response(submit(&storage_plan), 202).await;
    let failed = wait(attempt["application_id"].as_str().unwrap(), "failed").await;
    assert_eq!(
        failed["publication"]["error"],
        "OPEN_SOCKETS_PUBLICATION_FAILED"
    );
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, read);
    assert!(!state.registry.names().contains(&"desk.extra".into()));
    assert_eq!(
        invoke("desk.current", json!({})).await.result.unwrap()["authenticated"],
        true
    );

    state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fixture_open_history_fail")?;
            Ok(())
        })
        .unwrap();
    drop(_worker);
    let pending_identity = response(preflight(json!([first, other])), 200).await;
    let pending_identity = response(submit(&pending_identity), 202).await;
    // Force an identity mismatch without a config revision write. An
    // environment-backed node rename across restart has the same boundary.
    state
        .store
        .with(|c| {
            c.execute(
                "UPDATE config_head SET node_id='stored-mismatch' WHERE id=1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let mismatch_worker = ConfigApplyCoordinator::start_for_app(&state);
    let mismatch = wait(
        pending_identity["application_id"].as_str().unwrap(),
        "failed",
    )
    .await;
    assert_eq!(
        mismatch["publication"]["error"],
        "OPEN_SOCKETS_ENROLLED_NODE_MISMATCH"
    );
    assert_eq!(response(get("/api/v0/config/sockets"), 200).await, read);
    assert!(!state.registry.names().contains(&"desk.extra".into()));
    drop(mismatch_worker);
    state
        .store
        .with(|c| {
            c.execute(
                "UPDATE config_head SET node_id='socket-fixture' WHERE id=1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let before_reset = response(preflight(json!([first, other])), 200).await;
    let queued = response(submit(&before_reset), 202).await;
    assert_eq!(queued["state"], "saved_pending");
    response(
        post("/api/v0/system/factory-reset").json(&json!({"confirm":"FACTORY_RESET"})),
        200,
    )
    .await;
    assert_eq!(
        response(get("/api/v0/config/sockets"), 200).await["sockets"],
        json!([])
    );
    assert_eq!(
        invoke("desk.current", json!({})).await.error.unwrap().code,
        "UNKNOWN_TOOL"
    );
    assert_eq!(
        state
            .store
            .secret_get("open_socket.token.desk.current")
            .unwrap()
            .as_deref(),
        Some(TOKEN)
    );
    assert!(state.store.config_get("open_sockets").unwrap().is_none());
    let _worker = ConfigApplyCoordinator::start_for_app(&state);
    assert!(!state.registry.names().contains(&"desk.extra".into()));

    response(
        put("/api/v0/fleet").json(&json!({"self_id":"socket-fixture"})),
        200,
    )
    .await;
    let fresh = socket("desk.fresh", &format!("{endpoint}/fresh"));
    response(post("/api/v0/config/sockets").json(&fresh), 200).await;
    let after_reset = response(get("/api/v0/config/sockets"), 200).await;
    assert_eq!(after_reset["sockets"].as_array().unwrap().len(), 1);
    assert_eq!(after_reset["sockets"][0]["name"], "desk.fresh");
    response(
        client
            .delete(format!("{base}/api/v0/config/sockets/desk.fresh"))
            .bearer_auth(bearer),
        200,
    )
    .await;
    state.store.config_set("open_sockets", "{").unwrap();
    let unreadable = response(get("/api/v0/config/export"), 200).await;
    assert!(unreadable.get("open_sockets").is_none());
    assert!(unreadable.get("open_sockets_source_node").is_none());
    assert_eq!(
        state.store.config_get("open_sockets").unwrap().as_deref(),
        Some("{")
    );

    partner_task.abort();
    api_task.abort();
    std::fs::remove_dir_all(scratch).unwrap();
}
