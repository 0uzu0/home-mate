//! Production HTTP model adapter → advertised catalog → actual destructive sink.
use crate::{
    api::{router, AppState},
    config::{
        EffectLevel, OpenApiConfig, OpenApiOperation, OpenApiParam, OpenApiParamLocation,
        VerificationStrategy,
    },
    store::Store,
    tool::mocks::MockInject,
};
use axum::{
    extract::State,
    response::IntoResponse,
    routing::{any, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
struct Partner {
    sink: Arc<Mutex<Vec<Value>>>,
    models: Arc<Mutex<Vec<Value>>>,
    model_gate: Arc<Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    sink_gate: Arc<Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    spec_gate: Arc<Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    mode: Arc<Mutex<u16>>,
    reply: Arc<Mutex<Option<Value>>>,
    generated_args: Arc<Mutex<Option<Value>>>,
    model_arrived: Arc<tokio::sync::Notify>,
    sink_arrived: Arc<tokio::sync::Notify>,
    spec_arrived: Arc<tokio::sync::Notify>,
}
async fn wait(gate: &Mutex<Option<tokio::sync::watch::Receiver<bool>>>) {
    let mut gate = gate.lock().unwrap().clone();
    if let Some(gate) = gate.as_mut() {
        while !*gate.borrow() {
            if gate.changed().await.is_err() {
                break;
            }
        }
    }
}
async fn model(State(p): State<Partner>, Json(body): Json<Value>) -> impl IntoResponse {
    p.models.lock().unwrap().push(body.clone());
    p.model_arrived.notify_one();
    wait(&p.model_gate).await;
    let messages = body["messages"].as_array().unwrap();
    let mut outstanding = std::collections::HashSet::<String>::new();
    for message in messages {
        if message["role"] == "tool" {
            let id = message["tool_call_id"].as_str().unwrap_or("");
            assert!(
                outstanding.remove(id),
                "external provider rejects orphan/duplicate tool response: {body}"
            );
        } else {
            assert!(outstanding.is_empty(),"external provider rejects unresolved assistant tool IDs before next message: {body}");
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    outstanding.insert(call["id"].as_str().unwrap().into());
                }
            }
        }
    }
    assert!(
        outstanding.is_empty(),
        "external provider rejects unresolved terminal tool IDs: {body}"
    );
    let command = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(json!({}));
    let chunks = if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
        && messages.last().unwrap()["role"] != "tool"
        && command["query_only"] != true
    {
        let wire = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| {
                t["function"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("openapi.call:")
            })
            .unwrap()["function"]["name"]
            .as_str()
            .unwrap();
        let args = p
            .generated_args
            .lock()
            .unwrap()
            .clone()
            .or_else(|| command.get("args").cloned())
            .as_ref()
            .cloned()
            .unwrap_or(json!({"operation":"destructive","arguments":{},"confirmed":true}));

        let mut calls = vec![
            json!({"index":0,"id":"call_confirmation","type":"function","function":{"name":wire,"arguments":args.to_string()}}),
        ];
        if command["second"] == true {
            calls.push(json!({"index":1,"id":"must_not_dispatch","type":"function","function":{"name":wire,"arguments":args.to_string()}}));
        }
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":"{\"candidates\":[],\"decisions\":[],\"entries\":[]}"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]
    };
    let mut output = chunks
        .into_iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>();
    output.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], output)
}
async fn sink(
    State(p): State<Partner>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    p.sink.lock().unwrap().push(json!({"method":method.as_str(),"body":String::from_utf8_lossy(&body),"authorized":headers.get("authorization").and_then(|h|h.to_str().ok())==Some("Bearer synthetic-confirmation-token")}));
    p.sink_arrived.notify_one();
    wait(&p.sink_gate).await;
    let status = *p.mode.lock().unwrap();
    (
        axum::http::StatusCode::from_u16(if status == 0 { 200 } else { status }).unwrap(),
        Json(
            p.reply
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(json!({"changed":true})),
        ),
    )
}
async fn spec(State(p): State<Partner>) -> Json<Value> {
    p.spec_arrived.notify_one();
    wait(&p.spec_gate).await;
    Json(
        json!({"openapi":"3.0.3","info":{"title":"C1 test","version":"1"},"paths":{"/effect":{"delete":{"operationId":"destructive","parameters":[]}}}}),
    )
}
async fn listen(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, task)
}
struct Fixture {
    base: String,
    partner_base: String,
    token: String,
    state: Arc<AppState>,
    p: Partner,
    client: reqwest::Client,
    dir: PathBuf,
    servers: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    async fn new() -> Self {
        let p = Partner::default();
        let (partner_base, partner_server) = listen(
            Router::new()
                .route("/v1/chat/completions", post(model))
                .route("/effect", any(sink))
                .route("/openapi.json", get(spec))
                .with_state(p.clone()),
        )
        .await;
        let dir = std::env::temp_dir().join(format!(
            "hm-confirmation-{}",
            crate::config_delivery::uuid().unwrap()
        ));
        let store = Arc::new(Store::open(&dir.join("home-mate.db")).unwrap());
        store.allow_loopback_public_for_test();
        let state = AppState::with_overrides(
            store,
            dir.clone(),
            MockInject::default(),
            None,
            None,
            None,
            None,
        );
        let (base, server) = listen(router(state.clone())).await;
        let client = reqwest::Client::new();
        let token = client
            .post(format!("{base}/api/v0/auth/provision"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let f = Self {
            base,
            partner_base,
            token,
            state,
            p,
            client,
            dir,
            servers: vec![server, partner_server],
        };
        let response=f.client.put(format!("{}/api/v0/config/services/llm",f.base)).bearer_auth(&f.token).json(&json!({"provider":"deepseek","base_url":format!("{}/v1",f.partner_base),"model":"confirmation-fixture","api_key_env":""})).send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        f.configure("delete", false);
        f
    }
    fn configure(&self, method: &str, idempotency: bool) {
        self.state
            .config
            .save_openapi(
                OpenApiConfig {
                    enabled: true,
                    spec_url: format!("{}/openapi.json", self.partner_base),
                    base_url: self.partner_base.clone(),
                    timeout_ms: 500,
                    retries: 3,
                    operations: vec![OpenApiOperation {
                        id: "destructive".into(),
                        method: method.into(),
                        path: "/effect".into(),
                        summary: "Test destructive effect".into(),
                        params: if idempotency {
                            vec![OpenApiParam {
                                sensitive: false,
                                name: "idempotency_key".into(),
                                location: OpenApiParamLocation::Query,
                                required: false,
                                kind: "string".into(),
                                enum_values: vec![],
                            }]
                        } else {
                            vec![]
                        },
                        effect_level: EffectLevel::Destructive,
                        verification: Some(VerificationStrategy::TransportOnly),
                    }],
                    ..Default::default()
                },
                Some("synthetic-confirmation-token"),
                "test",
            )
            .unwrap();
    }
    async fn request(&self, method: &str, path: &str, body: Value) -> reqwest::Response {
        self.client
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                format!("{}{path}", self.base),
            )
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn action(&self, args: Value, second: bool) -> (Value, Value, String) {
        let binding = self
            .request("POST", "/api/v1/chat/scopes", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap();
        let raw=self.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":args,"second":second}).to_string()})).await.text().await.unwrap();
        let action = raw
            .lines()
            .filter_map(|s| s.strip_prefix("data: "))
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .find(|v| v["type"] == "confirmation_required")
            .unwrap_or_else(|| panic!("pending action missing: {raw}"))["action"]
            .clone();
        (action, binding, raw)
    }
    async fn pending(&self) -> (Value, Value, String) {
        self.action(
            json!({"operation":"destructive","arguments":{},"confirmed":true}),
            false,
        )
        .await
    }
    async fn change(&self, action: &Value, operation: &str) -> reqwest::Response {
        self.request(
            "POST",
            &format!(
                "/api/v1/confirmations/{}/{}",
                action["id"].as_str().unwrap(),
                operation
            ),
            json!({"expected_state":action["state"],"expected_revision":action["revision"]}),
        )
        .await
    }
    async fn approve(&self, action: &Value) -> Value {
        let response = self.change(action, "approve").await;
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        response.json().await.unwrap()
    }
    async fn settled(&self, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let a = self
                    .request("GET", &format!("/api/v1/confirmations/{id}"), Value::Null)
                    .await
                    .json::<Value>()
                    .await
                    .unwrap();
                if a["outcome"]["status"] != "running" {
                    return a;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}
#[tokio::test]
async fn real_model_preview_approve_zero_effect_then_separate_concurrent_execute_once() {
    let f = Fixture::new().await;
    let (action, _, raw) = f
        .action(
            json!({"operation":"destructive","arguments":{},"confirmed":true}),
            true,
        )
        .await;
    assert!(
        raw.contains("confirmation_required") && raw.contains("awaiting_confirmation"),
        "{raw}"
    );
    assert!(
        !raw.contains("\"type\":\"answer\""),
        "pending is not an answer/completion"
    );
    assert_eq!(
        f.p.models.lock().unwrap().len(),
        1,
        "pending must stop the model loop"
    );
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert_eq!(
        f.state
            .tasks
            .get(action["task_id"].as_str().unwrap())
            .unwrap()
            .status(),
        crate::task::TaskStatus::AwaitingConfirmation
    );
    let id = action["id"].as_str().unwrap();
    assert_eq!(
        f.request("GET", &format!("/api/v1/confirmations/{id}"), Value::Null)
            .await
            .status(),
        200
    );
    let listed = f
        .request(
            "GET",
            "/api/v1/confirmations?state=pending,approved",
            Value::Null,
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(listed["actions"].as_array().unwrap().len(), 1);
    assert_eq!(
        f.client
            .get(format!("{}/api/v1/confirmations/{id}", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "loopback still requires pairing"
    );
    let bypass = f
        .request(
            "POST",
            &format!("/api/v1/confirmations/{id}/execute"),
            json!({"expected_state":"approved","expected_revision":1}),
        )
        .await;
    assert_eq!(
        bypass.status(),
        409,
        "expected_state is not approval authority"
    );
    assert!(f.p.sink.lock().unwrap().is_empty());
    let stale_approval = f
        .request(
            "POST",
            &format!("/api/v1/confirmations/{id}/approve"),
            json!({"expected_state":"pending","expected_revision":999}),
        )
        .await;
    assert_eq!(stale_approval.status(), 409);
    let approved = f.approve(&action).await;
    assert!(f.p.sink.lock().unwrap().is_empty());
    let (a, b) = tokio::join!(
        f.change(&approved, "execute"),
        f.change(&approved, "execute")
    );
    assert_eq!(a.status(), 200);
    assert_eq!(b.status(), 200);
    let final_action = f.settled(id).await;
    assert_eq!(final_action["state"], "consumed");
    assert_eq!(final_action["outcome"]["evidence"], "sent");
    assert_eq!(final_action["outcome"]["fact"]["confirmed"], false);
    assert_eq!(f.p.sink.lock().unwrap().len(), 1);
    assert_eq!(f.p.sink.lock().unwrap()[0]["method"], "DELETE");
    assert_eq!(f.p.sink.lock().unwrap()[0]["authorized"], true);
    assert_eq!(
        f.change(&approved, "execute").await.status(),
        200,
        "lost response/retry returns retained consumption"
    );
    assert_eq!(f.p.sink.lock().unwrap().len(), 1);
    assert!(!serde_json::to_string(&final_action)
        .unwrap()
        .contains("synthetic-confirmation-token"));
}
#[tokio::test]
async fn invalid_args_and_direct_runtime_boolean_and_probe_cannot_bypass() {
    let f = Fixture::new().await;
    let binding = f
        .request("POST", "/api/v1/chat/scopes", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let raw=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":{"operation":"destructive","arguments":{"typo":true},"confirmed":true}}).to_string()})).await.text().await.unwrap();
    assert!(!raw.contains("\"type\":\"confirmation_required\""));
    assert_eq!(
        f.request("GET", "/api/v1/confirmations", Value::Null)
            .await
            .json::<Value>()
            .await
            .unwrap()["actions"],
        json!([])
    );
    assert!(f
        .state
        .openapi
        .call("destructive", &json!({}), None)
        .await
        .is_err());
    let result = f
        .state
        .openapi
        .execute_operation("destructive", &json!({}), true, None)
        .await;
    assert_eq!(result.status, crate::tool::ToolStatus::Failed);
    f.configure("get", false);
    let _ = f.state.openapi.probe().await;
    assert!(f.p.sink.lock().unwrap().is_empty());
}
#[tokio::test]
async fn every_destructive_method_one_attempt_on_server_error_despite_retry_or_key() {
    for method in ["get", "post", "put", "patch", "delete"] {
        let f = Fixture::new().await;
        f.configure(method, true);
        *f.p.mode.lock().unwrap() = 500;
        let (a,_,_)=f.action(json!({"operation":"destructive","arguments":{"idempotency_key":"not-authority"},"confirmed":true}),false).await;
        let approved = f.approve(&a).await;
        assert_eq!(f.change(&approved, "execute").await.status(), 200);
        let end = f.settled(a["id"].as_str().unwrap()).await;
        assert_eq!(end["outcome"]["status"], "unknown", "{method}: {end}");
        assert_eq!(f.p.sink.lock().unwrap().len(), 1, "{method}");
    }
}
#[tokio::test]
async fn pending_authority_session_cancel_binding_and_revision_fences_send_zero() {
    for reason in [
        "session", "cancel", "binding", "token", "endpoint", "reset", "expiry",
    ] {
        let f = Fixture::new().await;
        let (a, binding, _) = f.pending().await;
        let approved = f.approve(&a).await;
        match reason {
            "session" => {
                f.state
                    .sessions
                    .clear(a["session_id"].as_str().unwrap())
                    .unwrap();
            }
            "cancel" => {
                let response = f
                    .request(
                        "POST",
                        "/api/v0/session/cancel",
                        json!({"task_id":a["task_id"]}),
                    )
                    .await;
                assert!(response.status().is_success());
            }
            "binding" => {
                assert!(f
                    .request(
                        "DELETE",
                        &format!(
                            "/api/v1/chat/scopes/{}",
                            binding["binding_id"].as_str().unwrap()
                        ),
                        Value::Null
                    )
                    .await
                    .status()
                    .is_success());
            }
            "token" => {
                assert!(f
                    .request(
                        "PUT",
                        "/api/v0/config/services/openapi",
                        json!({"token":"rotated-synthetic"})
                    )
                    .await
                    .status()
                    .is_success());
            }
            "endpoint" => {
                assert!(f
                    .request(
                        "PUT",
                        "/api/v0/config/services/openapi",
                        json!({"base_url":format!("{}/other",f.partner_base)})
                    )
                    .await
                    .status()
                    .is_success());
            }
            "reset" => {
                assert!(f
                    .request(
                        "POST",
                        "/api/v0/system/factory-reset",
                        json!({"confirm":"FACTORY_RESET","include_secrets":false})
                    )
                    .await
                    .status()
                    .is_success());
            }
            "expiry" => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(301)).await;
                tokio::time::resume();
            }
            _ => unreachable!(),
        }
        let response = f.change(&approved, "execute").await;
        assert!(!response.status().is_success(), "{reason}");
        assert!(f.p.sink.lock().unwrap().is_empty(), "{reason}");
    }
}
#[tokio::test]
async fn wrong_principal_and_changed_selection_are_not_action_authority() {
    let f = Fixture::new().await;
    let (a, _, _) = f.pending().await;
    let response=f.request("POST",&format!("/api/v1/confirmations/{}/approve",a["id"].as_str().unwrap()),json!({"expected_state":"pending","expected_revision":1,"person_id":"another","arguments":{},"session_id":"guessed"})).await;
    assert!(!response.status().is_success());
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert_eq!(f.change(&a, "reject").await.status(), 200);
    assert!(!f.change(&a, "execute").await.status().is_success());
    let (a, _, _) = f.pending().await;
    let other = f.state.auth.rotate("new paired actor").unwrap();
    for suffix in ["", "/approve", "/execute"] {
        let response = f
            .client
            .request(
                if suffix.is_empty() {
                    reqwest::Method::GET
                } else {
                    reqwest::Method::POST
                },
                format!(
                    "{}/api/v1/confirmations/{}{}",
                    f.base,
                    a["id"].as_str().unwrap(),
                    suffix
                ),
            )
            .bearer_auth(&other)
            .json(&json!({"expected_state":"pending","expected_revision":1}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "CONFIRMATION_NOT_FOUND"
        );
    }
}

#[tokio::test]
async fn token_only_rotation_advances_once_no_op_and_failed_atomic_save_leave_everything() {
    let f = Fixture::new().await;
    let old = f.state.config.openapi_revision().unwrap();
    let cfg = f.state.config.openapi().unwrap();
    let history = || {
        f.state
            .store
            .with(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM config_history WHERE scope='openapi'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap()
    };
    let count = history();
    f.state
        .config
        .save_openapi(cfg.clone(), Some("synthetic-confirmation-token"), "same")
        .unwrap();
    assert_eq!(f.state.config.openapi_revision().unwrap(), old);
    assert_eq!(history(), count);
    f.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_openapi_history BEFORE INSERT ON config_history WHEN NEW.scope='openapi' BEGIN SELECT RAISE(ABORT,'injected history failure'); END;")?;Ok(())}).unwrap();
    assert!(f
        .state
        .config
        .save_openapi(cfg.clone(), Some("different-synthetic-token"), "failed")
        .is_err());
    assert_eq!(
        f.state.config.openapi_token().unwrap().as_deref(),
        Some("synthetic-confirmation-token")
    );
    assert_eq!(f.state.config.openapi_revision().unwrap(), old);
    assert_eq!(history(), count);
    f.state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_openapi_history;")?;
            Ok(())
        })
        .unwrap();
    f.state
        .config
        .save_openapi(cfg.clone(), Some("different-synthetic-token"), "changed")
        .unwrap();
    assert_ne!(f.state.config.openapi_revision().unwrap(), old);
    assert_eq!(history(), count + 1);
    let revision = f.state.config.openapi_revision().unwrap();
    assert!(f
        .state
        .config
        .save_openapi_expected(cfg, None, "stale", Some(&old))
        .is_err());
    assert_eq!(f.state.config.openapi_revision().unwrap(), revision);
}
#[tokio::test]
async fn failed_consumption_transaction_sends_nothing_and_preserves_approved() {
    let f = Fixture::new().await;
    let (a, _, _) = f.pending().await;
    let approved = f.approve(&a).await;
    f.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_consumption BEFORE UPDATE OF state ON confirmations WHEN NEW.state='consumed' BEGIN SELECT RAISE(ABORT,'injected consumption failure'); END;")?;Ok(())}).unwrap();
    let response = f.change(&approved, "execute").await;
    assert_eq!(response.status(), 503);
    assert!(f.p.sink.lock().unwrap().is_empty());
    let actual = f
        .request(
            "GET",
            &format!("/api/v1/confirmations/{}", a["id"].as_str().unwrap()),
            Value::Null,
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(actual, approved);
}
#[tokio::test]
async fn paused_real_model_result_cannot_create_after_cancel_reset_or_config_change() {
    for reason in ["cancel", "reset", "config"] {
        let f = Fixture::new().await;
        let binding = f
            .request("POST", "/api/v1/chat/scopes", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap();
        let (release, gate) = tokio::sync::watch::channel(false);
        *f.p.model_gate.lock().unwrap() = Some(gate);
        let response=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":{"operation":"destructive","arguments":{},"confirmed":true}}).to_string()})).await;
        let read = tokio::spawn(async move { response.text().await.unwrap() });
        tokio::time::timeout(Duration::from_secs(3), f.p.model_arrived.notified())
            .await
            .unwrap();
        match reason {
            "cancel" => {
                let task = f.state.tasks.list(1).unwrap()[0]["id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                assert!(f
                    .request("POST", "/api/v0/session/cancel", json!({"task_id":task}))
                    .await
                    .status()
                    .is_success());
            }
            "reset" => {
                assert!(f
                    .request(
                        "POST",
                        "/api/v0/system/factory-reset",
                        json!({"confirm":"FACTORY_RESET"})
                    )
                    .await
                    .status()
                    .is_success());
            }
            "config" => {
                assert!(f
                    .request(
                        "PUT",
                        "/api/v0/config/services/openapi",
                        json!({"token":"new-synthetic"})
                    )
                    .await
                    .status()
                    .is_success());
            }
            _ => unreachable!(),
        }
        release.send(true).unwrap();
        let raw = read.await.unwrap();
        assert!(
            !raw.contains("\"type\":\"confirmation_required\""),
            "{reason}: {raw}"
        );
        assert!(f.p.sink.lock().unwrap().is_empty());
        let count = f
            .state
            .store
            .with(|c| {
                Ok(c.query_row("SELECT count(*) FROM confirmations", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .unwrap();
        assert_eq!(count, 0, "{reason}");
    }
}
#[tokio::test]
async fn admitted_attempt_outlives_api_response_and_reset_cannot_recreate_private_journal() {
    let f = Fixture::new().await;
    let (a, _, _) = f.pending().await;
    let approved = f.approve(&a).await;
    let (release, gate) = tokio::sync::watch::channel(false);
    *f.p.sink_gate.lock().unwrap() = Some(gate);
    let response = f.change(&approved, "execute").await;
    assert_eq!(response.status(), 200);
    drop(response);
    tokio::time::timeout(Duration::from_secs(3), f.p.sink_arrived.notified())
        .await
        .unwrap();
    assert_eq!(f.p.sink.lock().unwrap().len(), 1);
    assert!(f
        .request(
            "POST",
            "/api/v0/system/factory-reset",
            json!({"confirm":"FACTORY_RESET"})
        )
        .await
        .status()
        .is_success());
    release.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if f.state
                .tasks
                .get(a["task_id"].as_str().unwrap())
                .unwrap()
                .status()
                != crate::task::TaskStatus::AwaitingConfirmation
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for table in ["confirmations", "tasks"] {
        assert_eq!(
            f.state
                .store
                .with(|c| Ok(c
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))?))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        f.p.sink.lock().unwrap().len(),
        1,
        "reset cannot recall the previously admitted request"
    );
}
#[tokio::test]
async fn resolved_spec_save_rechecks_revision_after_real_http_await() {
    let f = Fixture::new().await;
    let before = f.state.config.openapi_revision().unwrap();
    let (release, gate) = tokio::sync::watch::channel(false);
    *f.p.spec_gate.lock().unwrap() = Some(gate);
    let client = f.client.clone();
    let base = f.base.clone();
    let token = f.token.clone();
    let captured = before.clone();
    let save = tokio::spawn(async move {
        client.put(format!("{base}/api/v0/config/services/openapi")).bearer_auth(token).json(&json!({"operations":[{"id":"destructive","effect_level":"destructive"}],"revision":captured})).send().await.unwrap()
    });
    tokio::time::timeout(Duration::from_secs(3), f.p.spec_arrived.notified())
        .await
        .unwrap();
    assert!(f
        .request(
            "PUT",
            "/api/v0/config/services/openapi",
            json!({"token":"independent-new-token"})
        )
        .await
        .status()
        .is_success());
    let expected = f.state.config.openapi_revision().unwrap();
    release.send(true).unwrap();
    let response = save.await.unwrap();
    assert_eq!(response.status(), 409, "{}", response.text().await.unwrap());
    assert_eq!(f.state.config.openapi_revision().unwrap(), expected);
    assert_eq!(
        f.state.config.openapi_token().unwrap().as_deref(),
        Some("independent-new-token")
    );
    assert!(f.p.sink.lock().unwrap().is_empty());
}
#[tokio::test]
async fn reopen_invalidates_approvals_and_consumed_running_becomes_unknown() {
    let f = Fixture::new().await;
    let (a, _, _) = f.pending().await;
    let approved = f.approve(&a).await;
    let (b, _, _) = f.pending().await;
    let mut consumed = f.approve(&b).await;
    consumed["state"] = json!("consumed");
    consumed["outcome"] = json!({"status":"running","evidence":"accepted","result":null});
    f.state
        .store
        .with(|c| {
            c.execute(
                "UPDATE confirmations SET state='consumed',action_json=?1 WHERE id=?2",
                rusqlite::params![consumed.to_string(), b["id"].as_str().unwrap()],
            )?;
            Ok(())
        })
        .unwrap();
    let reopened = AppState::with_overrides(
        Arc::new(Store::open(&f.dir.join("home-mate.db")).unwrap()),
        f.dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let principal = reopened.execution.principal(&f.token).unwrap();
    let old = reopened
        .confirmations
        .read(a["id"].as_str().unwrap(), &principal)
        .unwrap();
    assert_eq!(old["state"], "cancelled");
    let unknown = reopened
        .confirmations
        .read(b["id"].as_str().unwrap(), &principal)
        .unwrap();
    assert_eq!(unknown["state"], "consumed");
    assert_eq!(unknown["outcome"]["status"], "unknown");
    assert!(reopened
        .confirmations
        .execute(
            a["id"].as_str().unwrap(),
            &principal,
            "approved",
            approved["revision"].as_u64().unwrap()
        )
        .await
        .is_err());
    assert_eq!(
        reopened
            .confirmations
            .execute(b["id"].as_str().unwrap(), &principal, "approved", 2)
            .await
            .unwrap()["outcome"]["status"],
        "unknown"
    );
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert_eq!(
        reopened
            .store
            .with(|c| Ok(c.query_row(
                "SELECT count(*) FROM confirmations WHERE private_payload IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn paused_actual_execute_pre_admission_cancel_reset_and_config_win_with_zero_send() {
    for reason in ["cancel", "reset", "config"] {
        let f = Fixture::new().await;
        let (a, _, _) = f.pending().await;
        let approved = f.approve(&a).await;
        let (release, gate) = tokio::sync::watch::channel(false);
        let arrived = Arc::new(tokio::sync::Notify::new());
        *f.state.confirmations.before_admission.lock().unwrap() = Some((arrived.clone(), gate));
        let client = f.client.clone();
        let base = f.base.clone();
        let token = f.token.clone();
        let request = approved.clone();
        let execute = tokio::spawn(async move {
            client
                .post(format!(
                    "{base}/api/v1/confirmations/{}/execute",
                    request["id"].as_str().unwrap()
                ))
                .bearer_auth(token)
                .json(&json!({"expected_state":"approved","expected_revision":request["revision"]}))
                .send()
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(3), arrived.notified())
            .await
            .unwrap();
        match reason {
            "cancel" => {
                assert!(f
                    .request(
                        "POST",
                        "/api/v0/session/cancel",
                        json!({"task_id":a["task_id"]})
                    )
                    .await
                    .status()
                    .is_success());
            }
            "reset" => {
                assert!(f
                    .request(
                        "POST",
                        "/api/v0/system/factory-reset",
                        json!({"confirm":"FACTORY_RESET"})
                    )
                    .await
                    .status()
                    .is_success());
            }
            "config" => {
                assert!(f
                    .request(
                        "PUT",
                        "/api/v0/config/services/openapi",
                        json!({"token":"new-pre-dispatch-token"})
                    )
                    .await
                    .status()
                    .is_success());
            }
            _ => unreachable!(),
        }
        release.send(true).unwrap();
        let response = execute.await.unwrap();
        assert!(!response.status().is_success(), "{reason}");
        assert!(f.p.sink.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn connected_gateway_request_is_discoverable_but_spoken_yes_never_approves_and_close_revokes()
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut f = Fixture::new().await;
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let hub = f.state.hub.clone();
    f.servers.push(tokio::spawn(async move {
        let _ = hub.listen(address).await;
    }));
    let socket = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match tokio::net::TcpStream::connect(address).await {
                Ok(socket) => break socket,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap();
    let (reader, mut writer) = socket.into_split();
    let frames = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = frames.clone();
    f.servers.push(tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str(&line) {
                observed.lock().unwrap().push(frame);
            }
        }
    }));
    let hello = json!({"type":"hello","protocol":0,"gateway":"confirmation-wire","devices":[{"stable_id":"confirmation-dot","model":"fixture","capabilities":["audio_out","stop","play_stream"],"audio_profile":"fixture"}]});
    writer
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !frames
            .lock()
            .unwrap()
            .iter()
            .any(|f| f["type"] == "hello_ack")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut seq = 0;
    for ordinal in 0..2 {
        seq += 1;
        let wake =
            json!({"type":"wake","device":"confirmation-dot","seq":seq,"ts":crate::id::now_ms()});
        writer
            .write_all(format!("{wake}\n").as_bytes())
            .await
            .unwrap();
        seq += 1;
        let request = json!({"type":"utterance","device":"confirmation-dot","seq":seq,"ts":crate::id::now_ms(),"source":"text","text":json!({"args":{"operation":"destructive","arguments":{},"confirmed":true}}).to_string()});
        writer
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let action = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let listed = f
                    .request("GET", "/api/v1/confirmations?state=pending", Value::Null)
                    .await
                    .json::<Value>()
                    .await
                    .unwrap();
                if let Some(action) = listed["actions"].as_array().and_then(|a| a.first()) {
                    break action.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(action["requester"]["kind"], "physical");
        assert_eq!(action["requester"]["device_id"], "confirmation-dot");
        assert_eq!(action["preview"]["scope"]["device_id"], "confirmation-dot");
        assert_eq!(action["approver_policy"], "paired_management");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mode = f
                    .request("GET", "/api/v0/mode", Value::Null)
                    .await
                    .json::<Value>()
                    .await
                    .unwrap();
                if mode["devices"].as_array().unwrap().iter().any(|d| {
                    d["device"] == "confirmation-dot"
                        && d["participation"] == "idle"
                        && d["session"]["close_reason"] == "single_request"
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(
            "default listening window returns idle without cancelling pending context authority",
        );
        let task = f
            .state
            .tasks
            .get(action["task_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(task.status(), crate::task::TaskStatus::AwaitingConfirmation);
        assert!(
            !task.is_cancelled(),
            "window settle must retain original confirmation CancellationScope"
        );

        assert!(f
            .state
            .voice
            .recent(100)
            .iter()
            .all(|e| e["kind"] != "answer"));
        let approved = f.approve(&action).await;
        if ordinal == 0 {
            assert_eq!(f.change(&approved, "execute").await.status(), 200);
            let end = f.settled(action["id"].as_str().unwrap()).await;
            assert_eq!(end["outcome"]["status"], "completed");
        } else {
            let before = f.p.sink.lock().unwrap().len();
            seq += 1;
            let yes = json!({"type":"utterance","device":"confirmation-dot","seq":seq,"ts":crate::id::now_ms(),"source":"text","text":"yes"});
            writer
                .write_all(format!("{yes}\n").as_bytes())
                .await
                .unwrap();
            // Close through the real management operation; no guessed session
            // string or direct cancellation helper grants/revokes authority.
            assert!(f
                .request(
                    "POST",
                    "/api/v0/session/close",
                    json!({"device":"confirmation-dot"})
                )
                .await
                .status()
                .is_success());
            assert!(!f.change(&approved, "execute").await.status().is_success());
            assert_eq!(f.p.sink.lock().unwrap().len(), before);
        }
    }
}

#[tokio::test]
async fn next_actual_provider_turn_has_complete_tool_batches_and_truthful_outcome_continuation() {
    for terminal in ["pending", "reject", "execute"] {
        let f = Fixture::new().await;
        let (a, binding, _) = f
            .action(
                json!({"operation":"destructive","arguments":{},"confirmed":true}),
                true,
            )
            .await;
        match terminal {
            "reject" => {
                assert_eq!(f.change(&a, "reject").await.status(), 200);
            }
            "execute" => {
                let approved = f.approve(&a).await;
                assert_eq!(f.change(&approved, "execute").await.status(), 200);
                f.settled(a["id"].as_str().unwrap()).await;
            }
            _ => {}
        }
        let raw=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"query_only":true}).to_string()})).await.text().await.unwrap();
        assert!(
            raw.contains("\"status\":\"completed\""),
            "{terminal}: {raw}"
        );
        let requests = f.p.models.lock().unwrap();
        let last = requests.last().unwrap();
        let serialized = last["messages"].to_string();
        assert!(
            serialized.contains("must_not_dispatch") && serialized.contains("confirmation_pending"),
            "skipped sibling tool ID has explicit no-effect result"
        );
        if terminal != "pending" {
            assert!(
                serialized.contains("Core confirmation outcome"),
                "{terminal}: {serialized}"
            );
        }
        if terminal == "execute" {
            assert!(
                serialized.contains("\\\"evidence\\\":\\\"sent\\\""),
                "transport-only remains unconfirmed: {serialized}"
            );
        }
    }
}

#[tokio::test]
async fn action_capacity_never_evicts_live_authority_and_pending_tasks_survive_trim() {
    let f = Fixture::new().await;
    let (first, _, _) = f.pending().await;
    for _ in 1..16 {
        f.pending().await;
    }
    let binding = f
        .request("POST", "/api/v1/chat/scopes", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let raw=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":{"operation":"destructive","arguments":{}}}).to_string()})).await.text().await.unwrap();
    assert!(!raw.contains("\"type\":\"confirmation_required\""));
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert_eq!(
        f.request(
            "GET",
            "/api/v1/confirmations?state=pending&limit=64",
            Value::Null
        )
        .await
        .json::<Value>()
        .await
        .unwrap()["actions"]
            .as_array()
            .unwrap()
            .len(),
        16
    );
    for _ in 0..300 {
        let task = f.state.tasks.create("unrelated-trim-fixture").unwrap();
        f.state
            .tasks
            .finish(&task, crate::task::TaskStatus::Completed)
            .unwrap();
    }
    assert_eq!(
        f.state
            .tasks
            .get(first["task_id"].as_str().unwrap())
            .unwrap()
            .status(),
        crate::task::TaskStatus::AwaitingConfirmation
    );
    assert_eq!(f.change(&first, "reject").await.status(), 200);
    f.pending().await;
}
#[tokio::test]
async fn secret_sql_failure_and_import_rotation_share_the_atomic_revision_boundary() {
    let f = Fixture::new().await;
    let (a, _, _) = f.pending().await;
    let approved = f.approve(&a).await;
    let previous = f.state.config.openapi_revision().unwrap();
    let config = f.state.config.openapi().unwrap();
    f.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_secret BEFORE UPDATE ON secrets WHEN NEW.name='openapi.token' BEGIN SELECT RAISE(ABORT,'injected secret failure'); END;")?;Ok(())}).unwrap();
    assert!(!f
        .request(
            "PUT",
            "/api/v0/config/services/openapi",
            json!({"token":"injected-no-commit","base_url":format!("{}/new-target",f.partner_base)})
        )
        .await
        .status()
        .is_success());
    assert_eq!(f.state.config.openapi_revision().unwrap(), previous);
    assert_eq!(f.state.config.openapi().unwrap(), config);
    assert_eq!(
        f.state.config.openapi_token().unwrap().as_deref(),
        Some("synthetic-confirmation-token")
    );
    f.state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_secret;")?;
            Ok(())
        })
        .unwrap();
    let response = f
        .request(
            "POST",
            "/api/v0/config/import",
            json!({"openapi":config,"secrets":{"openapi.token":"imported-new-synthetic"}}),
        )
        .await;
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    assert_ne!(f.state.config.openapi_revision().unwrap(), previous);
    assert!(!f.change(&approved, "execute").await.status().is_success());
    assert!(f.p.sink.lock().unwrap().is_empty());
}
#[tokio::test]
async fn destructive_query_readback_and_secret_target_preview_are_refused_before_action() {
    let f = Fixture::new().await;
    let mut cfg = f.state.config.openapi().unwrap();
    let mut dangerous = cfg.operations[0].clone();
    dangerous.id = "dangerousReadback".into();
    dangerous.method = "get".into();
    cfg.operations[0].verification = Some(VerificationStrategy::ReadbackOp {
        op_id: dangerous.id.clone(),
        json_path: "changed".into(),
        expected: json!(true),
    });
    cfg.operations.push(dangerous);
    assert!(f
        .state
        .config
        .save_openapi(cfg, None, "bad-readback")
        .is_err());
    let mut cfg = f.state.config.openapi().unwrap();
    cfg.operations[0].path = "/synthetic-confirmation-token".into();
    let rejected = f
        .state
        .config
        .save_openapi(cfg.clone(), None, "unsafe-target")
        .unwrap_err();
    assert!(rejected
        .to_string()
        .contains("OPENAPI_SECRET_IN_PUBLIC_CONFIG"));
    // Exercise the executor's protection against a pre-existing unsafe row.
    f.state
        .store
        .config_set(
            crate::config::OPENAPI_KEY,
            &serde_json::to_string(&cfg).unwrap(),
        )
        .unwrap();
    let binding = f
        .request("POST", "/api/v1/chat/scopes", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let raw=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":{"operation":"destructive","arguments":{}}}).to_string()})).await.text().await.unwrap();
    assert!(!raw.contains("\"type\":\"confirmation_required\""));
    assert!(!raw.contains("synthetic-confirmation-token"));
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
async fn failed_outcome_bookkeeping_retries_no_http_and_writes_one_continuation() {
    let f = Fixture::new().await;
    let (a, _, _) = f.pending().await;
    let approved = f.approve(&a).await;
    f.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_outcome BEFORE UPDATE OF action_json ON confirmations WHEN json_extract(NEW.action_json,'$.outcome.status')='completed' BEGIN SELECT RAISE(ABORT,'injected outcome bookkeeping failure'); END;")?;Ok(())}).unwrap();
    assert_eq!(f.change(&approved, "execute").await.status(), 200);
    tokio::time::timeout(Duration::from_secs(3), f.p.sink_arrived.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let done = f
                .state
                .confirmations
                .live
                .lock()
                .unwrap()
                .get(a["id"].as_str().unwrap())
                .is_some_and(|live| live.completed.is_some());
            if done {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.p.sink.lock().unwrap().len(), 1);
    f.state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_outcome;")?;
            Ok(())
        })
        .unwrap();
    let end = f.settled(a["id"].as_str().unwrap()).await;
    assert_eq!(end["outcome"]["status"], "completed");
    assert_eq!(f.p.sink.lock().unwrap().len(), 1);
    let messages = f
        .state
        .sessions
        .messages(a["session_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["content"]
                .as_str()
                .is_some_and(|s| s.contains("Core confirmation outcome")))
            .count(),
        1
    );
}

#[tokio::test]
async fn pending_task_write_failure_rolls_back_action_payload_and_quota() {
    let f = Fixture::new().await;
    f.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_pending_task BEFORE UPDATE OF status ON tasks WHEN NEW.status='awaiting_confirmation' BEGIN SELECT RAISE(ABORT,'injected pending task write failure'); END;")?;Ok(())}).unwrap();
    let binding = f
        .request("POST", "/api/v1/chat/scopes", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    let raw=f.request("POST","/api/v0/chat",json!({"scope_binding_id":binding["binding_id"],"text":json!({"args":{"operation":"destructive","arguments":{},"confirmed":true}}).to_string()})).await.text().await.unwrap();
    assert!(!raw.contains("\"type\":\"confirmation_required\""), "{raw}");
    assert!(f.p.sink.lock().unwrap().is_empty());
    assert!(f.state.confirmations.live.lock().unwrap().is_empty());
    let counts = f
        .state
        .store
        .with(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM confirmations", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                c.query_row(
                    "SELECT count(*) FROM tasks WHERE status='awaiting_confirmation'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(
        counts,
        (0, 0),
        "no durable orphan/private payload or occupied quota"
    );
    f.state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_pending_task;")?;
            Ok(())
        })
        .unwrap();
    let (action, _, _) = f.pending().await;
    assert_eq!(
        action["state"], "pending",
        "the failure leaves creation usable without restart"
    );
}

#[path = "tests/review.rs"]
mod review;
