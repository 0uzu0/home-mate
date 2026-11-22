//! Deterministic publication boundary after actual HTTP decoding, before Store writes.
//! The separate held-server fixture is unproven and tracked by issue144.
use super::*;
use crate::api::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
use tokio::sync::{mpsc, oneshot};

struct Nas {
    profiles: Mutex<(StatusCode, Value)>,
    gets: AtomicUsize,
    posts: AtomicUsize,
}
struct Fixture {
    state: Arc<AppState>,
    client: reqwest::Client,
    base: String,
    nas_url: String,
    nas: Arc<Nas>,
    llm_url: String,
    prompts: mpsc::UnboundedReceiver<Value>,
    data: PathBuf,
    servers: Vec<tokio::task::JoinHandle<()>>,
    preparation_ms: u128,
}
async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    (
        base,
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    )
}
fn profiles(summary: &str, person: &str) -> Value {
    json!({"profiles":[{"id":"same-profile","scope_person":person,"scope_device":"","summary":summary,"updated_ms":1}]})
}
impl Fixture {
    async fn new() -> Self {
        let began = Instant::now();
        let nas = Arc::new(Nas {
            profiles: Mutex::new((StatusCode::OK, profiles("OLD_RESET_MARKER", ""))),
            gets: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
        });
        let (nas_url, nas_task) = serve(
            Router::new()
                .route(
                    "/api/v1/profiles",
                    get(|State(nas): State<Arc<Nas>>| async move {
                        nas.gets.fetch_add(1, Ordering::SeqCst);
                        let (status, body) = nas.profiles.lock().unwrap().clone();
                        (status, Json(body))
                    }),
                )
                .route(
                    "/api/v1/observations",
                    post(
                        |State(nas): State<Arc<Nas>>, Json(body): Json<Value>| async move {
                            assert!(body["observations"].is_array());
                            nas.posts.fetch_add(1, Ordering::SeqCst);
                            Json(json!({"accepted":true}))
                        },
                    ),
                )
                .with_state(nas.clone()),
        )
        .await;
        let (prompt_tx, prompts) = mpsc::unbounded_channel();
        let (llm_url,llm_task)=serve(Router::new().route("/v1/chat/completions",post(move |Json(body):Json<Value>| {
            let tx=prompt_tx.clone();async move {
                tx.send(body).unwrap();
                ([("content-type","text/event-stream")],"data: {\"choices\":[{\"delta\":{\"content\":\"Synthetic reply.\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            }
        }))).await;
        let data = std::env::temp_dir().join(crate::id::next("shared-reset-fixture"));
        std::fs::create_dir_all(&data).unwrap();
        let state = AppState::with_overrides(
            Arc::new(Store::open_in_memory().unwrap()),
            data.clone(),
            Default::default(),
            None,
            None,
            None,
            None,
        );
        tokio::task::yield_now().await; // Initial unconfigured worker tick, before installing attempt hooks.
        let (base, api_task) = serve(crate::api::router(state.clone())).await;
        let token: Value = reqwest::Client::new()
            .post(format!("{base}/api/v0/auth/provision"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token["token"].as_str().unwrap())
                .parse()
                .unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        Self {
            state,
            client,
            base,
            nas_url,
            nas,
            llm_url,
            prompts,
            data,
            servers: vec![nas_task, llm_task, api_task],
            preparation_ms: began.elapsed().as_millis(),
        }
    }
    fn gate(
        &self,
        stage: TestStage,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<SyncOutcome>,
    ) {
        let (reached, entered) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        let (completed, outcome) = oneshot::channel();
        *self.state.shared_memory.test_gate.lock().unwrap() = Some((stage, reached, resume));
        *self.state.shared_memory.test_completion.lock().unwrap() = Some(completed);
        (entered, release, outcome)
    }
    fn completion(&self) -> oneshot::Receiver<SyncOutcome> {
        let (completed, outcome) = oneshot::channel();
        *self.state.shared_memory.test_completion.lock().unwrap() = Some(completed);
        outcome
    }
    async fn status(&self) -> Value {
        let response = self
            .client
            .get(format!("{}/api/v0/shared-memory/status", self.base))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        response.json().await.unwrap()
    }
    fn rows(&self) -> Value {
        self.state.store.with(|c| {
            let mut stmt=c.prepare("SELECT id,scope_person,scope_device,summary,updated_ms,fetched_ms FROM shared_profile_cache ORDER BY id")?;
            let rows=stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"person":r.get::<_,String>(1)?,"device":r.get::<_,String>(2)?,"summary":r.get::<_,String>(3)?,"updated_ms":r.get::<_,i64>(4)?,"fetched_ms":r.get::<_,i64>(5)?})))?;
            Ok(Value::Array(rows.collect::<Result<Vec<_>,_>>()?))
        }).unwrap()
    }
    fn enable_upload_without_sync(&self) {
        self.state
            .shared_memory
            .save_config(
                SharedMemoryConfig {
                    enabled: true,
                    nas_base_url: self.nas_url.clone(),
                    upload_enabled: true,
                },
                "test",
            )
            .unwrap();
    }
    async fn configure(&self, upload: bool) {
        let response = self
            .client
            .put(format!("{}/api/v0/shared-memory/config", self.base))
            .json(&json!({"enabled":true,"nas_base_url":self.nas_url,"upload_enabled":upload}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
    }
    async fn reset(&mut self) -> Value {
        let response = self
            .client
            .post(format!("{}/api/v0/system/factory-reset", self.base))
            .json(&json!({"confirm":"FACTORY_RESET","include_secrets":false}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let result = response.json().await.unwrap();
        // Factory reset clears pairing metadata. Re-pair through the real API.
        let paired: Value = reqwest::Client::new()
            .post(format!("{}/api/v0/auth/provision", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", paired["token"].as_str().unwrap())
                .parse()
                .unwrap(),
        );
        self.client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        result
    }
    async fn prompt(&mut self) -> Value {
        let response=self.client.put(format!("{}/api/v0/config/services/llm",self.base)).json(&json!({"provider":"deepseek","base_url":format!("{}/v1",self.llm_url),"model":"reset-sync-fixture","api_key_env":""})).send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let response = self
            .client
            .post(format!("{}/api/v0/chat", self.base))
            .json(&json!({"text":"Synthetic post-reset question"}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let text = response.text().await.unwrap();
        let events: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|body| serde_json::from_str(body).ok())
            .collect();
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "turn_end" && event["status"] == "completed"),
            "{text}"
        );
        tokio::time::timeout(Duration::from_secs(1), self.prompts.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
        let _ = std::fs::remove_dir_all(&self.data);
    }
}
fn evidence(name: &str, value: Value) {
    if let Some(root) = std::env::var_os("HM_RESET_EVIDENCE_DIR") {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
}
#[tokio::test]
async fn decoded_old_profile_cannot_repopulate_reset_cache_or_next_http_prompt() {
    let mut fixture = Fixture::new().await;
    let (entered, release, outcome) = fixture.gate(TestStage::ProfilesDecoded);
    fixture.configure(false).await; // One real API-triggered sync; no second manual attempt.
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture.nas.gets.load(Ordering::SeqCst),
        1,
        "exact management-triggered attempt owns this decoded body"
    );
    let reset_began = Instant::now();
    let reset = fixture.reset().await;
    let reset_ms = reset_began.elapsed().as_millis();
    let empty = fixture.state.shared_memory.cache_count().unwrap();
    release.send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(1), outcome)
        .await
        .unwrap()
        .unwrap();
    let cached = fixture.state.shared_memory.cache_count().unwrap();
    let prompt = fixture.prompt().await;
    let leaked = prompt.to_string().contains("OLD_RESET_MARKER");
    evidence(
        "post-decode-reset-prompt",
        json!({"scope":"actual-HTTP-decoded-body-before-publication","store_backend":"isolated_in_memory","preparation_ms":fixture.preparation_ms,"HTTP_budget_ms":SYNC_HTTP_BUDGET.as_millis(),"decoded_before_reset":true,"reset_ms":reset_ms,"reset":reset,"empty_after_reset":empty,"cache_after_old_publication":cached,"outcome":format!("{outcome:?}"),"old_marker_in_actual_llm_prompt":leaked,"actual_llm_request":prompt}),
    );
    assert_eq!(empty, 0);
    assert_eq!(outcome, SyncOutcome::DiscardedReset);
    let status = fixture.status().await;
    assert_eq!(status["last_sync_ms"], Value::Null);
    assert_eq!(status["consecutive_failures"], 0);
    assert!(!leaked && cached==0,"decoded pre-reset result crossed actual reset: cache={cached}, old_marker_in_next_http_prompt={leaked}");
}

async fn finished(receiver: oneshot::Receiver<SyncOutcome>) -> SyncOutcome {
    tokio::time::timeout(Duration::from_secs(1), receiver)
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
async fn normal_real_upload_and_profile_reach_actual_prompt_and_status() {
    let mut fixture = Fixture::new().await;
    fixture.enable_upload_without_sync();
    fixture
        .state
        .shared_memory
        .enqueue_observation("synthetic-device", "", "", "trace", "synthetic observation")
        .unwrap();
    let completed = fixture.completion();
    fixture.configure(true).await;
    let outcome = finished(completed).await;
    assert_eq!(
        outcome,
        SyncOutcome::Synced {
            uploaded: 1,
            cached: 1
        }
    );
    assert_eq!(fixture.nas.posts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.nas.gets.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state.shared_memory.backlog_count().unwrap(), 0);
    let status = fixture.status().await;
    assert_eq!(status["cache_count"], 1);
    assert_eq!(status["backlog_count"], 0);
    assert!(status["last_sync_ms"].as_u64().is_some());
    assert_eq!(status["last_error"], Value::Null);
    let prompt = fixture.prompt().await;
    assert!(prompt.to_string().contains("OLD_RESET_MARKER"));
    evidence(
        "normal-http-sync",
        json!({"outcome":format!("{outcome:?}"),"status":status,"actual_llm_request":prompt,"posts":1,"gets":1}),
    );
}
#[tokio::test]
async fn old_upload_ack_cannot_delete_new_epoch_rows_or_dispatch_profiles() {
    let mut fixture = Fixture::new().await;
    fixture.enable_upload_without_sync();
    let old_id = fixture
        .state
        .shared_memory
        .enqueue_observation("old-device", "", "", "old-trace", "old observation")
        .unwrap()
        .unwrap();
    let (entered, release, completed) = fixture.gate(TestStage::UploadResponse);
    fixture.configure(true).await;
    finished_signal(entered).await;
    assert_eq!(fixture.nas.posts.load(Ordering::SeqCst), 1);
    fixture.reset().await;
    fixture.enable_upload_without_sync();
    let new_id = fixture
        .state
        .shared_memory
        .enqueue_observation("new-device", "", "", "new-trace", "new epoch observation")
        .unwrap()
        .unwrap();
    // Adversarial same-ID reuse proves the fence, independently of random IDs.
    fixture.state.store.with(|c| {c.execute("INSERT INTO shared_memory_outbox (id,device_id,room,person_id,trace_id,content,created_ms) VALUES (?1,'new-device','','','new-trace','new epoch reused ID',1)",[&old_id])?;Ok(())}).unwrap();
    release.send(()).unwrap();
    let outcome = finished(completed).await;
    assert_eq!(outcome, SyncOutcome::DiscardedReset);
    assert_eq!(
        fixture.nas.gets.load(Ordering::SeqCst),
        0,
        "stale upload must not dispatch profiles"
    );
    let rows = fixture.state.shared_memory.list_outbox(50).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.id == new_id));
    assert!(rows
        .iter()
        .any(|row| row.id == old_id && row.content == "new epoch reused ID"));
    let status = fixture.status().await;
    assert_eq!(status["backlog_count"], 2);
    assert_eq!(status["last_sync_ms"], Value::Null);
    assert_eq!(status["last_error"], Value::Null);
    evidence(
        "old-upload-ack",
        json!({"outcome":format!("{outcome:?}"),"remote_upload_already_received":true,"gets_after_old_ack":0,"surviving_rows":rows,"status":status}),
    );
}
async fn finished_signal(receiver: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(1), receiver)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn old_broad_profile_cannot_overwrite_new_scope_or_success_status() {
    new_epoch_wins(false).await;
}
#[tokio::test]
async fn old_http_failure_cannot_poison_new_epoch_circuit() {
    new_epoch_wins(true).await;
}
async fn new_epoch_wins(old_failure: bool) {
    let mut fixture = Fixture::new().await;
    if old_failure {
        *fixture.nas.profiles.lock().unwrap() = (
            StatusCode::BAD_GATEWAY,
            json!({"error":"old synthetic failure"}),
        );
    }
    let stage = if old_failure {
        TestStage::ProfilesResponse
    } else {
        TestStage::ProfilesDecoded
    };
    let (entered, release, old_completed) = fixture.gate(stage);
    fixture.configure(false).await;
    finished_signal(entered).await;
    fixture.reset().await;
    *fixture.nas.profiles.lock().unwrap() = (
        StatusCode::OK,
        profiles("NEW_RESTRICTED_MARKER", "synthetic-person"),
    );
    let new_completed = fixture.completion();
    fixture.configure(false).await;
    assert_eq!(
        finished(new_completed).await,
        SyncOutcome::Synced {
            uploaded: 0,
            cached: 1
        }
    );
    let new_rows = fixture.rows();
    let new_status = fixture.status().await;
    release.send(()).unwrap();
    let outcome = finished(old_completed).await;
    assert_eq!(outcome, SyncOutcome::DiscardedReset);
    assert_eq!(
        fixture.rows(),
        new_rows,
        "old same-ID body cannot broaden new scope"
    );
    let final_status = fixture.status().await;
    assert_eq!(final_status, new_status);
    assert_eq!(final_status["circuit_open"], false);
    assert_eq!(final_status["consecutive_failures"], 0);
    let prompt = fixture.prompt().await;
    let body = prompt.to_string();
    assert!(!body.contains("OLD_RESET_MARKER"));
    assert!(
        !body.contains("NEW_RESTRICTED_MARKER"),
        "anonymous actual consumer cannot see restricted profile"
    );
    assert_eq!(fixture.nas.gets.load(Ordering::SeqCst), 2);
    evidence(
        if old_failure {
            "old-failure-new-epoch"
        } else {
            "old-broad-new-scope"
        },
        json!({"outcome":format!("{outcome:?}"),"new_rows":new_rows,"status":final_status,"actual_llm_request":prompt,"actual_gets":2}),
    );
}
#[tokio::test]
async fn profile_batch_rolls_back_on_actual_database_failure() {
    let fixture = Fixture::new().await;
    fixture.state.store.with(|c| {c.execute_batch("CREATE TRIGGER reject_fixture_profile BEFORE INSERT ON shared_profile_cache WHEN NEW.id='reject-profile' BEGIN SELECT RAISE(ABORT,'synthetic publication failure'); END;")?;Ok(())}).unwrap();
    *fixture.nas.profiles.lock().unwrap() = (
        StatusCode::OK,
        json!({"profiles":[{"id":"first-profile","summary":"must rollback"},{"id":"reject-profile","summary":"synthetic rejected row"}]}),
    );
    let completed = fixture.completion();
    fixture.configure(false).await;
    let outcome = finished(completed).await;
    assert!(
        matches!(&outcome,SyncOutcome::Failed(error) if error.contains("synthetic publication failure"))
    );
    assert_eq!(fixture.state.shared_memory.cache_count().unwrap(), 0);
    let status = fixture.status().await;
    assert_eq!(status["cache_count"], 0);
    assert_eq!(status["circuit_open"], true);
    assert_eq!(status["consecutive_failures"], 1);
    evidence(
        "atomic-publication-failure",
        json!({"outcome":format!("{outcome:?}"),"status":status,"actual_profiles_gets":fixture.nas.gets.load(Ordering::SeqCst)}),
    );
}

#[tokio::test]
async fn prepared_old_configuration_cannot_dispatch_after_reset() {
    let mut fixture = Fixture::new().await;
    fixture.enable_upload_without_sync();
    fixture
        .state
        .shared_memory
        .enqueue_observation("synthetic-device", "", "", "trace", "synthetic observation")
        .unwrap();
    let (entered, release, completed) = fixture.gate(TestStage::AttemptPrepared);
    fixture.configure(true).await;
    finished_signal(entered).await;
    fixture.reset().await;
    release.send(()).unwrap();
    let outcome = finished(completed).await;
    assert_eq!(outcome, SyncOutcome::DiscardedReset);
    assert_eq!(fixture.nas.posts.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.nas.gets.load(Ordering::SeqCst), 0);
    let status = fixture.status().await;
    assert_eq!(status["enabled"], false);
    assert_eq!(status["last_sync_ms"], Value::Null);
    evidence(
        "pre-dispatch-reset",
        json!({"outcome":format!("{outcome:?}"),"actual_posts":0,"actual_gets":0,"status":status}),
    );
}
#[tokio::test]
async fn late_old_success_cannot_clear_new_epoch_failure() {
    let mut fixture = Fixture::new().await;
    let (entered, release, old_completed) = fixture.gate(TestStage::ProfilesPublished);
    fixture.configure(false).await;
    finished_signal(entered).await;
    assert_eq!(fixture.state.shared_memory.cache_count().unwrap(), 1);
    fixture.reset().await;
    *fixture.nas.profiles.lock().unwrap() = (
        StatusCode::BAD_GATEWAY,
        json!({"error":"new epoch failure"}),
    );
    let new_completed = fixture.completion();
    fixture.configure(false).await;
    assert!(matches!(
        finished(new_completed).await,
        SyncOutcome::Failed(_)
    ));
    let new_status = fixture.status().await;
    assert_eq!(new_status["circuit_open"], true);
    release.send(()).unwrap();
    let outcome = finished(old_completed).await;
    assert_eq!(outcome, SyncOutcome::DiscardedReset);
    assert_eq!(fixture.status().await, new_status);
    assert_eq!(fixture.state.shared_memory.cache_count().unwrap(), 0);
    evidence(
        "old-success-new-failure",
        json!({"outcome":format!("{outcome:?}"),"status":new_status,"actual_gets":fixture.nas.gets.load(Ordering::SeqCst)}),
    );
}
