use super::*;
use hm_core::config_delivery::{uuid, ConfigWorker};
use hm_core::tool::{ToolCtx, ToolResult, ToolStatus};
use std::{sync::atomic::AtomicBool, time::Duration};

async fn response(request: reqwest::RequestBuilder) -> Value {
    let resp = request.send().await.unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}

#[tokio::test]
async fn reviewed_catalogue_publishes_and_rolls_back_at_the_actual_executor() {
    let fake = spawn_fake(Mode::Ok).await;
    let (_, state) = spawn_app(FakeLlm::new(vec![])).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = router(state.clone());
    tokio::spawn(async move {
        axum::serve(
            listener,
            api.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let initial = configure(
        &base,
        &fake,
        "spec-token-123",
        json!(["getStatus", "getTemperature"]),
    )
    .await;
    assert_eq!(initial["enabled"], true);
    let client = reqwest::Client::new();
    let pairing = response(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let token = pairing["token"].as_str().unwrap();
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(token);
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(token);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(token);
    let preflight = |openapi: Value| {
        post("/api/v1/config/preflight").json(&json!({
            "package": {"format":"home-mate-config","version":1,"openapi":openapi}
        }))
    };
    let apply = |plan: &Value| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id":plan["plan_id"],"plan_hash":plan["plan_hash"],
            "expected_revision":plan["expected_revision"],"application_id":uuid().unwrap()
        }))
    };
    let readback = |id: &str| get(&format!("/api/v1/config/applications/{id}/readback"));
    let wait = async |receipt: &Value, expected: &str| {
        let id = receipt["application_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let proof = response(readback(id)).await;
                let publication = &proof["publications"][0];
                assert_eq!(publication["block"], "openapi");
                if publication["state"] == expected {
                    return proof;
                }
                assert_eq!(publication["state"], "saved_pending", "{proof}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    };
    let execute = async |op: &str, arguments: Value| -> ToolResult {
        let ctx = ToolCtx::new(
            "reviewed-openapi",
            uuid().unwrap(),
            Arc::new(AtomicBool::new(false)),
        );
        state
            .registry
            .call(
                "openapi.call",
                json!({"operation":op,"arguments":arguments}),
                &ctx,
            )
            .await
    };

    let baseline = state.config.openapi().unwrap();
    let mut old_settings = baseline.clone();
    old_settings.base_url = format!("{}/operation?view=old", fake.base);
    state
        .store
        .config_set(
            hm_core::config::OPENAPI_KEY,
            &serde_json::to_string(&old_settings).unwrap(),
        )
        .unwrap();
    let before_guard = fake.hits();
    let unsafe_base = execute("getStatus", json!({})).await;
    assert_eq!(unsafe_base.error.unwrap().code, "OPENAPI_BASE_URL");
    assert_eq!(fake.hits(), before_guard);
    state
        .store
        .config_set(
            hm_core::config::OPENAPI_KEY,
            &serde_json::to_string(&baseline).unwrap(),
        )
        .unwrap();

    response(put("/api/v0/fleet").json(&json!({"self_id":"openapi-fixture"}))).await;
    response(put("/api/v1/config/enrollment").json(&json!({}))).await;
    let hits = fake.hits();
    let foreign = preflight(json!({"base_url":"https://another.example"}))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 409);
    assert_eq!(
        foreign.json::<Value>().await.unwrap()["error"],
        "OPENAPI_SOURCE_BINDING_CONFLICT"
    );
    let legacy_import = post("/api/v0/config/import")
        .json(&json!({
            "format":"home-mate-config","version":1,
            "openapi":{
                "enabled":true,"spec_url":initial["spec_url"],"base_url":"https://another.example",
                "auth_header":"","operations":initial["operations"],
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy_import.status(), 400);
    assert!(legacy_import.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("OPENAPI_SOURCE_BINDING_CONFLICT"));
    let invalid = preflight(json!({"operations":[{"id":"bad","path":"https://another.example"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    let choice = preflight(json!({"enabled":true,"token_set":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(choice.status(), 400);
    assert_eq!(
        choice.json::<Value>().await.unwrap()["error"],
        "OPENAPI_AUTH_HEADER_CHOICE"
    );

    let mut family = response(post("/api/v1/config/exports").json(&json!({
        "format":"v2", "purpose":"family_policy", "blocks":["openapi"]
    })))
    .await["package"]
        .clone();
    family.as_object_mut().unwrap().remove("content_hash");
    let family_plan =
        |value: &Value| post("/api/v1/config/preflight").json(&json!({"package":value}));
    for operations in [
        json!([{"action":"set","path":"openapi.spec_url","value":"https://another.example/schema?token=fixture-inline-value"}]),
        json!([{"action":"set","path":"openapi.base_url","value":"https://another.example/base?view=plain"}]),
        json!([{"action":"set","path":"openapi.auth_header","value":"not a header"}]),
        json!([{"action":"replace","path":"openapi.operations","value":[{"id":"bad","method":"patch","path":"/bad"}]}]),
        json!([{"action":"replace","path":"openapi.operations","value":[
            {"id":"bad","method":"post","path":"/bad","effect_level":"mutate",
             "verification":{"type":"response_assert","json_path":"$.echo","expected":{"copy":["spec-token-123"]}}}
        ]}]),
    ] {
        family["blocks"][0]["operations"] = operations;
        let rejected = family_plan(&family).send().await.unwrap();
        assert_eq!(rejected.status(), 400);
        let detail = rejected.text().await.unwrap();
        assert!(!detail.contains("fixture-inline-value") && !detail.contains("spec-token-123"));
    }
    let catalogue = json!([
        {"id":"query", "path":"/status"},
        {"id":"write", "method":"post", "path":"/write","effect_level":"mutate",
         "verification":{"type":"readback_op","op_id":" query ","json_path":"$.status","expected":"ok"}}
    ]);
    family["blocks"][0]["operations"] = json!([
        {"action":"replace","path":"openapi.operations","value":catalogue}
    ]);
    assert_eq!(family_plan(&family).send().await.unwrap().status(), 400);
    let normalized = response(preflight(json!({"operations":catalogue}))).await;
    assert!(normalized["migration_report"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |row| row["pointer"] == "/openapi/operations/1/verification/op_id"
                && row["action"] == "normalization"
        ));
    family["blocks"][0]["operations"] = json!([
        {"action":"set","path":"openapi.auth_header","value":"fixture-later-bound-token"}
    ]);
    let shadowed = response(family_plan(&family)).await;
    assert_eq!(shadowed["diff"][0]["after"], "");
    let pending_family = response(apply(&shadowed)).await;
    assert_eq!(pending_family["state"], "saved_pending");
    let rotated = put("/api/v0/config/services/openapi")
        .json(&json!({"token":"fixture-later-bound-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rotated.status(), 400);
    let detail = rotated.text().await.unwrap();
    assert!(detail.contains("OPENAPI_SECRET_IN_PUBLIC_CONFIG"));
    assert!(!detail.contains("fixture-later-bound-token"));
    assert_eq!(fake.hits(), hits);

    let mut temperature = initial["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["id"] == "getTemperature")
        .unwrap()
        .clone();
    temperature["params"][1]["enum_values"] = json!(["f"]);
    let old_revision = state.config.openapi_revision().unwrap();
    let plan = response(preflight(json!({
        "auth_header":"X-HM-Review-Token",
        "operations":[temperature],
        "timeout_ms":1100,
        "retries":1,
    })))
    .await;
    let receipt = response(apply(&plan)).await;
    assert_eq!(receipt["state"], "saved_pending");
    assert_eq!(
        (response(readback(receipt["application_id"].as_str().unwrap())).await)["publications"][0]
            ["state"],
        "saved_pending"
    );
    assert_eq!(state.config.openapi_revision().unwrap(), old_revision);
    assert_eq!(
        execute("getStatus", json!({})).await.status,
        ToolStatus::Completed
    );
    assert_eq!(fake.count("/status"), 1);

    let _worker = ConfigWorker::start(state.store.clone());
    let proof = wait(&receipt, "applied").await;
    assert!(
        proof["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|field| field["path"].as_str().unwrap().starts_with("openapi."))
            .all(|field| field["state"] == "core_readback_verified"),
        "{proof}"
    );
    assert_ne!(state.config.openapi_revision().unwrap(), old_revision);
    assert_eq!(
        execute("getStatus", json!({})).await.error.unwrap().code,
        "OPENAPI_OPERATION_UNKNOWN"
    );
    assert_eq!(
        execute("getTemperature", json!({"room":"bedroom","unit":"c"}))
            .await
            .error
            .unwrap()
            .code,
        "OPENAPI_PARAM_ENUM"
    );
    assert_eq!(
        execute("getTemperature", json!({"room":"bedroom","unit":"f"}))
            .await
            .status,
        ToolStatus::Completed
    );
    assert_eq!(fake.count("GET /rooms/bedroom/temperature"), 1);
    assert_eq!(fake.count("custom auth verified"), 1);

    for (format, content) in [("v1", "document"), ("v2", "package")] {
        let export = response(post("/api/v1/config/exports").json(&json!({
            "format": format, "blocks":["openapi"]
        })))
        .await;
        assert!(!export.to_string().contains("spec-token-123"));
        assert!(export.to_string().contains("X-HM-Review-Token"));
        response(post("/api/v1/config/preflight").json(&json!({"package":export[content]}))).await;
    }

    let legacy_plan = response(preflight(json!({"enabled":true,"token_set":true}))).await;
    assert!(legacy_plan["migration_report"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["pointer"] == "/openapi/auth_header" && row["action"] == "preserved"));
    let mut legacy = response(post("/api/v1/config/exports").json(&json!({
        "format":"v1","blocks":["openapi"]
    })))
    .await["document"]
        .clone();
    legacy["openapi"]
        .as_object_mut()
        .unwrap()
        .remove("auth_header");
    let imported = response(post("/api/v0/config/import").json(&legacy)).await;
    assert!(imported["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("omitted auth_header"))));
    assert_eq!(
        response(get("/api/v0/config/services/openapi")).await["auth_header"],
        "X-HM-Review-Token"
    );

    let reversal = response(
        post(&format!(
            "/api/v1/config/applications/{}/rollback-preflight",
            receipt["application_id"].as_str().unwrap()
        ))
        .json(&json!({"expected_revision":receipt["config_revision"]})),
    )
    .await;
    let restored = response(apply(&reversal)).await;
    wait(&restored, "applied").await;
    assert_eq!(
        execute("getStatus", json!({})).await.status,
        ToolStatus::Completed
    );
    assert_eq!(fake.count("/status"), 2);
    let current = response(get("/api/v0/config/services/openapi")).await;
    assert_eq!(current["auth_header"], "");
    assert_eq!(current["token_set"], true);
    assert_eq!(current["operations"], initial["operations"]);

    state.store.with(|c| {
        c.execute_batch("CREATE TRIGGER reject_openapi_publication BEFORE INSERT ON config_history WHEN NEW.scope='openapi' BEGIN SELECT RAISE(ABORT, 'fixture failed history'); END")?;
        Ok(())
    }).unwrap();
    let plan = response(preflight(json!({"enabled":false}))).await;
    let failed = response(apply(&plan)).await;
    let proof = wait(&failed, "failed").await;
    assert_eq!(
        proof["publications"][0]["error"],
        "OPENAPI_PUBLICATION_FAILED"
    );
    assert_eq!(
        response(get("/api/v0/config/services/openapi")).await,
        current
    );
    assert_eq!(
        execute("getStatus", json!({})).await.status,
        ToolStatus::Completed
    );
}
