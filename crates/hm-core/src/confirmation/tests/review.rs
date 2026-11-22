use super::*;

#[tokio::test]
async fn terminal_action_before_original_receipt_defers_history_until_whole_batch_is_durable() {
    for terminal in ["reject", "execute"] {
        let f = Fixture::new().await;
        let binding = f
            .request("POST", "/api/v1/chat/scopes", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap();
        let (release, gate) = tokio::sync::watch::channel(false);
        let arrived = Arc::new(tokio::sync::Notify::new());
        *f.state.confirmations.before_receipt.lock().unwrap() = Some((arrived.clone(), gate));
        let response=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":{"operation":"destructive","arguments":{},"confirmed":true},"second":true}).to_string()})).await;
        let read = tokio::spawn(async move { response.text().await.unwrap() });
        tokio::time::timeout(Duration::from_secs(3), arrived.notified())
            .await
            .unwrap();
        let action = f
            .request("GET", "/api/v1/confirmations?state=pending", Value::Null)
            .await
            .json::<Value>()
            .await
            .unwrap()["actions"][0]
            .clone();
        assert!(action["id"].is_string());
        if terminal == "reject" {
            assert_eq!(f.change(&action, "reject").await.status(), 200);
        } else {
            let approved = f.approve(&action).await;
            assert_eq!(f.change(&approved, "execute").await.status(), 200);
            f.settled(action["id"].as_str().unwrap()).await;
        }
        let messages = f
            .state
            .sessions
            .messages(action["session_id"].as_str().unwrap())
            .unwrap();
        assert!(
            !messages.iter().any(|m| m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Core confirmation outcome"))),
            "no assistant continuation can split unresolved tool batch"
        );
        release.send(true).unwrap();
        let raw = read.await.unwrap();
        assert!(raw.contains("confirmation_required"), "{raw}");
        let next=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"query_only":true}).to_string()})).await.text().await.unwrap();
        assert!(
            next.contains("\"status\":\"completed\""),
            "external provider validates next turn: {next}"
        );
        let requests = f.p.models.lock().unwrap();
        let history = requests.last().unwrap()["messages"].to_string();
        assert!(
            history.contains("must_not_dispatch")
                && history.contains("confirmation_pending")
                && history.contains("Core confirmation outcome")
        );
        assert_eq!(
            f.p.sink.lock().unwrap().len(),
            if terminal == "execute" { 1 } else { 0 }
        );
    }
}

#[tokio::test]
async fn rejected_task_write_failure_preserves_action_and_retry_owner() {
    let f = Fixture::new().await;
    let (action, _, _) = f.pending().await;
    f.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_terminal_task BEFORE UPDATE OF status ON tasks WHEN NEW.status='cancelled' BEGIN SELECT RAISE(ABORT,'injected terminal task failure'); END;")?;Ok(())}).unwrap();
    assert_eq!(f.change(&action, "reject").await.status(), 503);
    let read = f
        .request(
            "GET",
            &format!("/api/v1/confirmations/{}", action["id"].as_str().unwrap()),
            Value::Null,
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(read, action);
    assert!(f
        .state
        .confirmations
        .live
        .lock()
        .unwrap()
        .contains_key(action["id"].as_str().unwrap()));
    f.state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_terminal_task;")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(f.change(&action, "reject").await.status(), 200);
    assert_eq!(
        f.state
            .tasks
            .get(action["task_id"].as_str().unwrap())
            .unwrap()
            .status(),
        crate::task::TaskStatus::Cancelled
    );
    assert!(!f
        .state
        .confirmations
        .live
        .lock()
        .unwrap()
        .contains_key(action["id"].as_str().unwrap()));
}

#[tokio::test]
async fn cursor_hidden_id_and_missing_id_have_identical_page_results() {
    let f = Fixture::new().await;
    let (old, _, _) = f.pending().await;
    let next_token = f.state.auth.rotate("next actor").unwrap();
    let fetch = |cursor: String| {
        let client = f.client.clone();
        let base = f.base.clone();
        let token = next_token.clone();
        async move {
            client
                .get(format!("{base}/api/v1/confirmations?cursor={cursor}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let hidden = fetch(old["id"].as_str().unwrap().into()).await;
    let missing = fetch("missing-action".into()).await;
    assert_eq!(hidden, missing);
}

#[tokio::test]
async fn normalized_preview_matches_actual_wire_and_sensitive_echoes_never_reach_public_outcomes() {
    for verification in ["transport", "assert", "readback"] {
        let f = Fixture::new().await;
        let password = "synthetic-private-password";
        *f.p.generated_args.lock().unwrap() = Some(
            json!({"operation":"destructive","arguments":{"password":password,"amount":1.0},"confirmed":true}),
        );
        *f.p.reply.lock().unwrap() = Some(
            json!({"proof":"wrong","password":password,"another_key":password,"configured_token":"synthetic-confirmation-token"}),
        );
        let mut cfg = f.state.config.openapi().unwrap();
        cfg.operations[0].params = vec![
            OpenApiParam {
                sensitive: false,
                name: "password".into(),
                location: OpenApiParamLocation::Body,
                required: true,
                kind: "string".into(),
                enum_values: vec![],
            },
            OpenApiParam {
                sensitive: false,
                name: "amount".into(),
                location: OpenApiParamLocation::Body,
                required: true,
                kind: "integer".into(),
                enum_values: vec![],
            },
        ];
        cfg.operations[0].verification = Some(match verification {
            "assert" => VerificationStrategy::ResponseAssert {
                json_path: "proof".into(),
                expected: json!("synthetic-mismatch-expectation"),
            },
            "readback" => VerificationStrategy::ReadbackOp {
                op_id: "readback".into(),
                json_path: "proof".into(),
                expected: json!("synthetic-mismatch-expectation"),
            },
            _ => VerificationStrategy::TransportOnly,
        });
        if verification == "readback" {
            cfg.operations.push(OpenApiOperation {
                id: "readback".into(),
                method: "get".into(),
                path: "/effect".into(),
                summary: "query readback".into(),
                params: vec![],
                effect_level: EffectLevel::Query,
                verification: None,
            });
        }
        f.state
            .config
            .save_openapi(cfg, None, "sensitive-fixture")
            .unwrap();
        let (action, _, raw) = f.pending().await;
        assert_eq!(action["preview"]["parameters"]["amount"].as_i64(), Some(1));
        assert_eq!(action["preview"]["parameters"]["password"], "[redacted]");
        assert!(!raw.contains(password) && !raw.contains("synthetic-confirmation-token"));
        let approved = f.approve(&action).await;
        assert!(f.p.sink.lock().unwrap().is_empty());
        assert_eq!(f.change(&approved, "execute").await.status(), 200);
        let final_action = f.settled(action["id"].as_str().unwrap()).await;
        let public = final_action.to_string();
        if verification != "transport" {
            assert_eq!(final_action["outcome"]["status"], "failed");
        }
        assert!(!public.contains("synthetic-mismatch-expectation"));
        assert!(
            !public.contains(password) && !public.contains("synthetic-confirmation-token"),
            "sensitive values remain private"
        );
        let hits = f.p.sink.lock().unwrap();
        let sent: Value = serde_json::from_str(hits[0]["body"].as_str().unwrap()).unwrap();
        assert!(sent["amount"].is_i64());
        assert_eq!(sent["amount"], action["preview"]["parameters"]["amount"]);
        assert_eq!(sent["password"], password);
        assert_eq!(hits.len(), if verification == "readback" { 2 } else { 1 });
        drop(hits);
        let messages = serde_json::to_string(
            &f.state
                .sessions
                .messages(action["session_id"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(!messages.contains(password) && !messages.contains("synthetic-confirmation-token"));
        let trace = f
            .state
            .store
            .with(|c| {
                let mut q = c.prepare("SELECT payload FROM trace_events")?;
                let rows = q
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows.join("\n"))
            })
            .unwrap();
        assert!(!trace.contains(password) && !trace.contains("synthetic-confirmation-token"));
    }
}

#[tokio::test]
async fn lossy_integer_is_rejected_without_action_or_sink_request() {
    let f = Fixture::new().await;
    let mut cfg = f.state.config.openapi().unwrap();
    cfg.operations[0].params = vec![OpenApiParam {
        sensitive: false,
        name: "amount".into(),
        location: OpenApiParamLocation::Body,
        required: true,
        kind: "integer".into(),
        enum_values: vec![],
    }];
    f.state
        .config
        .save_openapi(cfg, None, "integer-fixture")
        .unwrap();
    *f.p.generated_args.lock().unwrap() =
        Some(json!({"operation":"destructive","arguments":{"amount":1e20},"confirmed":true}));
    let binding = f
        .request("POST", "/api/v1/chat/scopes", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let raw = f
        .request(
            "POST",
            "/api/v0/chat",
            json!({"scope_binding_id":binding["binding_id"],"text":"bounded-integer-fixture"}),
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(!raw.contains("\"type\":\"confirmation_required\""));
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert_eq!(
        f.request("GET", "/api/v1/confirmations", Value::Null)
            .await
            .json::<Value>()
            .await
            .unwrap()["actions"],
        json!([])
    );
}

#[tokio::test]
async fn reviewed_openapi_publication_invalidates_an_approved_operation() {
    use crate::config_delivery::{uuid, ConfigWorker};

    let f = Fixture::new().await;
    let (action, _, _) = f.pending().await;
    let approved = f.approve(&action).await;
    let revision = f.state.config.openapi_revision().unwrap();
    for (route, body) in [
        ("/api/v0/fleet", json!({"self_id":"confirmation-fixture"})),
        ("/api/v1/config/enrollment", json!({})),
    ] {
        let result = f.request("PUT", route, body).await;
        assert!(
            result.status().is_success(),
            "{}",
            result.text().await.unwrap()
        );
    }

    let expected = json!({"state":"changed","steps":[1,true,null]});
    let mut catalogue = serde_json::to_value(f.state.config.openapi().unwrap().operations).unwrap();
    catalogue[0]["verification"] = json!({
        "type":"response_assert", "json_path":"$.reported", "expected":expected
    });
    let plan = f
        .request(
            "POST",
            "/api/v1/config/preflight",
            json!({
                "package":{"format":"home-mate-config", "version":1, "openapi":{"timeout_ms":3000,"operations":catalogue}}
            }),
        )
        .await;
    assert!(plan.status().is_success());
    let plan: Value = plan.json().await.unwrap();
    let id = uuid().unwrap();
    let receipt = f
        .request(
            "POST",
            "/api/v1/config/applications",
            json!({
                "plan_id":plan["plan_id"], "plan_hash":plan["plan_hash"],
                "expected_revision":plan["expected_revision"], "application_id":id
            }),
        )
        .await;
    assert_eq!(receipt.status(), 202);
    assert_eq!(f.state.config.openapi_revision().unwrap(), revision);
    assert!(f.p.sink.lock().unwrap().is_empty());

    let _worker = ConfigWorker::start(f.state.store.clone());
    tokio::time::timeout(Duration::from_secs(3), async {
        let path = format!("/api/v1/config/applications/{id}/readback");
        loop {
            let readback: Value = f
                .request("GET", &path, Value::Null)
                .await
                .json()
                .await
                .unwrap();
            let publication = &readback["publications"][0];
            if publication["state"] == "applied" {
                assert_eq!(publication["block"], "openapi");
                break;
            }
            assert_eq!(publication["state"], "saved_pending", "{readback}");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();

    assert_ne!(f.state.config.openapi_revision().unwrap(), revision);
    let rejected = f.change(&approved, "execute").await;
    assert_eq!(rejected.status(), 409);
    assert!(f.p.sink.lock().unwrap().is_empty());

    *f.p.reply.lock().unwrap() = Some(json!({"reported":expected}));
    let (fresh, _, _) = f.pending().await;
    let approved = f.approve(&fresh).await;
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert_eq!(f.change(&approved, "execute").await.status(), 200);
    let settled = f.settled(fresh["id"].as_str().unwrap()).await;
    assert_eq!(settled["outcome"]["evidence"], "observed");
    assert_eq!(settled["outcome"]["fact"]["confirmed"], true);
    assert_eq!(f.p.sink.lock().unwrap().len(), 1);
}
