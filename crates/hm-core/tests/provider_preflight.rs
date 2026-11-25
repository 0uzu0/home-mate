//! Actual authenticated router and file-backed provider mutation boundaries.
use hm_core::{
    api::{router, AppState},
    store::Store,
    tool::{
        mocks::MockInject,
        open_socket::{HttpMethod, OpenSocketContract, OpenSocketRuntime},
        Availability, DynamicToolProvider, Registry, SideEffect, Tool, ToolCtx, ToolDescriptor,
        ToolResult,
    },
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

struct App {
    base: String,
    token: String,
    state: Arc<AppState>,
    dir: PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for App {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl App {
    async fn start(broken: bool) -> Self {
        Self::start_with(if broken {
            Some(json!([contract("workmem.get")]))
        } else {
            None
        })
        .await
    }

    async fn start_with(persisted: Option<Value>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "hm-provider-{}",
            hm_core::config_delivery::uuid().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(&dir.join("home-mate.db")).unwrap());
        if let Some(contracts) = persisted {
            store
                .config_set("open_sockets", &contracts.to_string())
                .unwrap();
        }
        let state = AppState::with_overrides(
            store,
            dir.clone(),
            MockInject::default(),
            None,
            None,
            None,
            None,
        );
        let app = router(state.clone()).layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(axum::extract::ConnectInfo(
                    "192.168.1.10:5000".parse::<SocketAddr>().unwrap(),
                ));
                next.run(req).await
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let token = reqwest::Client::new()
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
        Self {
            base,
            token,
            state,
            dir,
            server,
        }
    }
    async fn request(&self, method: &str, name: Option<&str>, body: Value) -> reqwest::Response {
        let path = name.map(|n| format!("/{n}")).unwrap_or_default();
        reqwest::Client::new()
            .request(
                method.parse().unwrap(),
                format!("{}/api/v0/config/sockets{path}", self.base),
            )
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    fn snapshot(&self) -> Value {
        let store = &self.state.store;
        let durable = store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT value FROM secrets WHERE name LIKE 'open_socket.token.%' ORDER BY name",
                )?;
                let tokens = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                let history: i64 = c.query_row(
                    "SELECT COUNT(*) FROM config_history WHERE scope='open_sockets'",
                    [],
                    |r| r.get(0),
                )?;
                Ok(json!({"tokens":tokens,"history":history}))
            })
            .unwrap();
        json!({"list":self.state.open_sockets.list(),"config":store.config_get("open_sockets").unwrap(),
            "durable":durable,"catalog":self.state.registry.catalog().map(|c|c.revision).ok()})
    }
}
fn contract(name: &str) -> OpenSocketContract {
    OpenSocketContract {
        name: name.into(),
        service: "fixture".into(),
        description: "synthetic query".into(),
        parameters: json!({"type":"object"}),
        effect: SideEffect::Query,
        endpoint_url: "http://127.0.0.1:9/never-called".into(),
        method: HttpMethod::GET,
        enabled: true,
        timeout_ms: 500,
        auth_header: String::new(),
        default_subject: String::new(),
        default_action: "get".into(),
    }
}
struct Probe(String);
#[async_trait::async_trait]
impl Tool for Probe {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.0.clone().into(),
            service: "fixture".into(),
            side_effect: SideEffect::Query,
            description: "fixture".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn availability(&self) -> Availability {
        Availability::Ready
    }
    async fn execute(&self, _: Value, _: &ToolCtx) -> ToolResult {
        ToolResult::observed_ok(json!({"fixture":true}))
    }
}
struct Provider(Vec<Arc<dyn Tool>>);
impl DynamicToolProvider for Provider {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.clone()
    }
}

#[tokio::test]
async fn actual_router_preflight_refuses_conflicts_and_invalid_without_mutation() {
    let app = App::start(false).await;
    assert_eq!(
        reqwest::Client::new()
            .post(format!("{}/api/v0/config/sockets", app.base))
            .json(&contract("fixture.denied"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(app
        .state
        .registry
        .register_dynamic(Arc::new(Probe("fixture.dynamic".into()))));
    app.state
        .registry
        .add_provider(Arc::new(Provider(vec![Arc::new(Probe(
            "fixture.provider".into(),
        ))])))
        .unwrap();
    for name in ["workmem.get", "fixture.dynamic", "fixture.provider"] {
        let before = app.snapshot();
        let mut body = serde_json::to_value(contract(name)).unwrap();
        body["token"] = json!("synthetic-rejected-token");
        let r = app.request("POST", None, body).await;
        assert_eq!(r.status(), 400);
        let response = r.text().await.unwrap();
        assert!(response.contains("duplicate canonical"));
        assert!(!response.contains("synthetic-rejected-token"));
        assert!(
            before == app.snapshot(),
            "rejected collision changed durable/live state"
        );
    }
    for field in ["method", "auth_header", "parameters"] {
        let before = app.snapshot();
        let mut body = serde_json::to_value(contract("fixture.valid")).unwrap();
        body[field] = match field {
            "method" => json!("DELETE"),
            "auth_header" => json!("bad: header"),
            _ => json!([]),
        };
        assert_eq!(app.request("POST", None, body).await.status(), 400);
        assert!(before == app.snapshot(), "validation failure changed state");
    }
}

#[tokio::test]
async fn actual_router_sql_failures_rollback_config_token_history_and_publication() {
    let app = App::start(false).await;
    let mut initial = serde_json::to_value(contract("fixture.atomic")).unwrap();
    initial["token"] = json!("synthetic-original");
    assert_eq!(app.request("POST", None, initial).await.status(), 200);
    for (table, op, predicate) in [
        ("config_kv", "UPDATE", "WHEN NEW.key='open_sockets'"),
        (
            "secrets",
            "UPDATE",
            "WHEN NEW.name='open_socket.token.fixture.atomic'",
        ),
        ("config_history", "INSERT", "WHEN NEW.scope='open_sockets'"),
    ] {
        app.state.store.with(|c|{c.execute_batch(&format!("CREATE TRIGGER fail_provider BEFORE {op} ON {table} {predicate} BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END"))?;Ok(())}).unwrap();
        let before = app.snapshot();
        let mut body = serde_json::to_value(contract("fixture.atomic")).unwrap();
        body["endpoint_url"] = json!("http://127.0.0.1:9/replacement");
        body["token"] = json!("synthetic-replacement");
        assert_eq!(
            app.request("PUT", Some("fixture.atomic"), body)
                .await
                .status(),
            400
        );
        assert!(
            before == app.snapshot(),
            "failed transaction changed durable/live state"
        );
        app.state
            .store
            .with(|c| {
                c.execute_batch("DROP TRIGGER fail_provider")?;
                Ok(())
            })
            .unwrap();
    }
    app.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_provider BEFORE DELETE ON secrets BEGIN SELECT RAISE(ABORT,'synthetic delete failure'); END")?;Ok(())}).unwrap();
    let before = app.snapshot();
    assert_eq!(
        app.request("DELETE", Some("fixture.atomic"), json!({}))
            .await
            .status(),
        500
    );
    assert!(before == app.snapshot(), "failed removal changed state");
    let reopened = Store::open(&app.dir.join("home-mate.db")).unwrap();
    assert!(
        reopened.config_get("open_sockets").unwrap()
            == app.state.store.config_get("open_sockets").unwrap()
    );
    assert!(
        reopened
            .secret_get("open_socket.token.fixture.atomic")
            .unwrap()
            .as_deref()
            == Some("synthetic-original")
    );
}

#[tokio::test]
async fn actual_router_success_noop_token_clear_reopen_delete_and_persisted_repair() {
    let app = App::start(false).await;
    let mut body = serde_json::to_value(contract("fixture.saved")).unwrap();
    body["token"] = json!("synthetic-token");
    assert_eq!(app.request("POST", None, body.clone()).await.status(), 200);
    let before = app.snapshot();
    assert_eq!(
        app.request("PUT", Some("fixture.saved"), body.clone())
            .await
            .status(),
        200
    );
    assert!(
        before == app.snapshot(),
        "no-op must preserve generation and history"
    );
    body.as_object_mut().unwrap().remove("token");
    body["endpoint_url"] = json!("http://127.0.0.1:9/new-endpoint");
    assert_eq!(
        app.request("PUT", Some("fixture.saved"), body.clone())
            .await
            .status(),
        200
    );
    assert!(
        app.state
            .store
            .secret_get("open_socket.token.fixture.saved")
            .unwrap()
            .as_deref()
            == Some("synthetic-token")
    );
    body["token"] = json!("synthetic-replacement");
    assert_eq!(
        app.request("PUT", Some("fixture.saved"), body.clone())
            .await
            .status(),
        200
    );
    let persisted = Store::open(&app.dir.join("home-mate.db")).unwrap();
    assert!(
        persisted
            .secret_get("open_socket.token.fixture.saved")
            .unwrap()
            .as_deref()
            == Some("synthetic-replacement")
    );
    assert!(persisted
        .config_get("open_sockets")
        .unwrap()
        .unwrap()
        .contains("new-endpoint"));
    let history = app
        .state
        .store
        .with(|c| {
            let mut q = c.prepare(
                "SELECT summary||payload FROM config_history WHERE scope='open_sockets'",
            )?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(history
        .iter()
        .all(|row| !row.contains("synthetic-token") && !row.contains("synthetic-replacement")));
    let old = app.state.registry.catalog().unwrap();
    let wire = old
        .specs()
        .iter()
        .find(|s| s.canonical_name == "fixture.saved")
        .unwrap()
        .name
        .clone();
    // Existing corrupt sealed values are unreadable but still must be clearable.
    app.state.store.with(|c| {
        c.execute("UPDATE secrets SET value='enc:v1:synthetic-corrupt' WHERE name='open_socket.token.fixture.saved'",[])?;
        Ok(())
    }).unwrap();
    body["token"] = json!("");
    assert_eq!(
        app.request("PUT", Some("fixture.saved"), body)
            .await
            .status(),
        200
    );
    let ctx = ToolCtx::new("test", "test", Arc::new(AtomicBool::new(false)));
    assert_eq!(
        app.state
            .registry
            .call_advertised(&old, &wire, json!({}), &ctx)
            .await
            .error
            .unwrap()
            .code,
        "TOOL_STALE"
    );
    let public = app
        .request("GET", None, json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(public["sockets"][0]["token_set"], false);
    assert!(!public.to_string().contains("synthetic-token"));
    let reopened = Arc::new(Store::open(&app.dir.join("home-mate.db")).unwrap());
    assert!(reopened
        .secret_get("open_socket.token.fixture.saved")
        .unwrap()
        .is_none());
    let token_rows: i64 = reopened
        .with(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM secrets WHERE name='open_socket.token.fixture.saved'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        token_rows, 0,
        "clear must physically delete even an unreadable sealed token"
    );
    assert_eq!(OpenSocketRuntime::new(reopened).list().len(), 1);
    assert_eq!(
        app.request("DELETE", Some("fixture.saved"), json!({}))
            .await
            .status(),
        200
    );
    assert!(app
        .state
        .registry
        .catalog()
        .unwrap()
        .specs()
        .iter()
        .all(|s| s.canonical_name != "fixture.saved"));
    let reopened = Arc::new(Store::open(&app.dir.join("home-mate.db")).unwrap());
    assert!(OpenSocketRuntime::new(reopened).list().is_empty());
    let broken = App::start(true).await;
    assert!(broken.state.registry.catalog().is_err());
    assert_eq!(
        broken
            .request("DELETE", Some("workmem.get"), json!({}))
            .await
            .status(),
        200
    );
    assert!(broken.state.registry.catalog().is_ok());
    assert!(broken
        .state
        .registry
        .names()
        .contains(&"workmem.get".into()));
}

#[tokio::test]
async fn actual_http_monotonic_deletion_repairs_multiple_and_unroutable_persisted_faults() {
    let app = App::start_with(Some(json!([
        contract("workmem.get"),
        contract("workmem.set"),
        contract(""),
        contract("unsafe/slash")
    ])))
    .await;
    for name in ["workmem.get", "workmem.set", ""] {
        let before = app.snapshot();
        let response = app.request("DELETE", None, json!({"name":name})).await;
        assert_eq!(response.status(), 200);
        assert!(before != app.snapshot());
        assert!(
            app.state.registry.catalog().is_err(),
            "remaining invalid entries must stay fail-closed"
        );
        let status = reqwest::Client::new()
            .get(format!("{}/api/v0/status", app.base))
            .bearer_auth(&app.token)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert!(status["tool_catalog_error"].is_string());
        let ctx = ToolCtx::new("repair", "repair", Arc::new(AtomicBool::new(false)));
        assert_eq!(
            app.state
                .registry
                .call(
                    "memory.remember",
                    json!({"text":"must not be stored"}),
                    &ctx
                )
                .await
                .error
                .unwrap()
                .code,
            "TOOL_CATALOG_INVALID"
        );
        let before = app.snapshot();
        assert_eq!(
            app.request("POST", None, json!(contract("fixture.new")))
                .await
                .status(),
            400
        );
        assert!(
            before == app.snapshot(),
            "repair cannot authorize additions to broken catalog"
        );
    }
    let before = app.snapshot();
    app.state.store.with(|c|{c.execute_batch("CREATE TRIGGER fail_repair BEFORE INSERT ON config_history WHEN NEW.scope='open_sockets' BEGIN SELECT RAISE(ABORT,'synthetic repair failure'); END")?;Ok(())}).unwrap();
    assert_eq!(
        app.request("DELETE", None, json!({"name":"unsafe/slash"}))
            .await
            .status(),
        500
    );
    assert!(
        before == app.snapshot(),
        "failed repair must retain all prior state"
    );
    app.state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_repair")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        app.request("DELETE", None, json!({"name":"unsafe/slash"}))
            .await
            .status(),
        200
    );
    assert!(app.state.registry.catalog().is_ok());
    assert!(app.state.open_sockets.list().is_empty());
    let reopened = Arc::new(Store::open(&app.dir.join("home-mate.db")).unwrap());
    assert!(OpenSocketRuntime::new(reopened).list().is_empty());
    // Body deletion does not reserve a suffix that shadows existing item APIs.
    assert_eq!(
        app.request("POST", None, json!(contract("remove")))
            .await
            .status(),
        200
    );
    assert_eq!(
        app.request("PUT", Some("remove"), json!(contract("remove")))
            .await
            .status(),
        200
    );
    assert_eq!(
        app.request("DELETE", Some("remove"), json!({}))
            .await
            .status(),
        200
    );
}

#[test]
fn capacity_failure_and_missing_removal_leave_catalog_and_storage_unchanged() {
    let registry = Registry::new();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let runtime = OpenSocketRuntime::new(store.clone());
    registry.add_provider(runtime.clone()).unwrap();
    for i in 0..hm_core::tool::open_socket::MAX_OPEN_SOCKETS {
        runtime
            .register(
                &registry,
                contract(&format!("fixture.capacity{i}")),
                None,
                "test",
            )
            .unwrap();
    }
    let before = store.config_get("open_sockets").unwrap();
    let revision = registry.catalog().unwrap().revision;
    assert!(runtime
        .register(
            &registry,
            contract("fixture.over-capacity"),
            Some("synthetic-rejected"),
            "test"
        )
        .is_err());
    assert!(!runtime.remove(&registry, "fixture.absent", "test").unwrap());
    assert!(before == store.config_get("open_sockets").unwrap());
    assert_eq!(revision, registry.catalog().unwrap().revision);
    assert!(store
        .secret_get("open_socket.token.fixture.over-capacity")
        .unwrap()
        .is_none());
}

struct BlockingProvider {
    armed: AtomicBool,
    arrived: std::sync::mpsc::SyncSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl DynamicToolProvider for BlockingProvider {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.arrived.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        Vec::new()
    }
}
#[test]
fn deterministic_candidate_races_dynamic_registration_and_provider_attach() {
    for attach in [false, true] {
        let registry = Arc::new(Registry::new());
        let runtime = OpenSocketRuntime::new(Arc::new(Store::open_in_memory().unwrap()));
        registry.add_provider(runtime.clone()).unwrap();
        let (arrived_tx, arrived_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let blocker = Arc::new(BlockingProvider {
            armed: AtomicBool::new(false),
            arrived: arrived_tx,
            release: Mutex::new(release_rx),
        });
        registry.add_provider(blocker.clone()).unwrap();
        blocker.armed.store(true, Ordering::SeqCst);
        let r = registry.clone();
        let rt = runtime.clone();
        let candidate =
            std::thread::spawn(move || rt.register(&r, contract("fixture.race"), None, "test"));
        arrived_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let r = registry.clone();
        let competitor = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            if attach {
                r.add_provider(Arc::new(Provider(vec![Arc::new(Probe(
                    "fixture.race".into(),
                ))])))
                .is_ok()
            } else {
                r.register_dynamic(Arc::new(Probe("fixture.race".into())))
            }
        });
        started_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        assert!(candidate.join().unwrap().is_ok());
        assert!(!competitor.join().unwrap());
        assert_eq!(
            registry
                .names()
                .iter()
                .filter(|n| n.as_str() == "fixture.race")
                .count(),
            1
        );
        assert!(registry.catalog().is_ok());
    }
}
