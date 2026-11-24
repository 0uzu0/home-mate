//! M8a: the core side of release management. The core orchestrates and
//! reports; verification and pointer writes live in `hm-release` (covered by
//! its own crate tests + the device drills). These tests fix the contract the
//! console and the installer depend on: build identity, schema visibility,
//! DB snapshots, detached job spawning, and the empty-body activate/rollback
//! calls (the S3 lesson: urllib/console send Content-Length: 0).

use hm_core::api::{router, AppState};
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("hm-core-api-release-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    root
}

async fn app(root: &Path) -> (String, Arc<AppState>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    let state = AppState::new(store, root.join("data"), MockInject::default(), None);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn install_fake_tool(root: &Path) {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("hm-release");
    std::fs::write(
        &tool,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/called.txt\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn called(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("bin/called.txt"))
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

async fn wait_called(root: &Path, n: usize) -> Vec<String> {
    for _ in 0..100 {
        let lines = called(root);
        if lines.len() >= n {
            return lines;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    called(root)
}

async fn get(base: &str, path: &str) -> Value {
    reqwest::get(format!("{base}{path}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn call(base: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{base}{path}"));
    // These two endpoints must accept an empty body (S3: urllib sends CL 0).
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let value = res.json().await.unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn status_reports_the_build_identity_and_db_schema() {
    let root = temp_root("status");
    let (base, _state) = app(&root).await;
    let v = get(&base, "/api/v0/status").await;
    assert_eq!(v["build"]["id"], hm_core::BUILD_ID);
    assert_eq!(v["build"]["version"], hm_core::CORE_VERSION);
    // The in-memory store applied every migration.
    assert_eq!(v["db"]["schema"], 15, "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_release_view_is_honest_before_anything_is_installed() {
    let root = temp_root("empty");
    let (base, _state) = app(&root).await;
    let v = get(&base, "/api/v0/release").await;
    assert_eq!(v["installer"]["present"], false, "{v}");
    assert_eq!(v["state"], Value::Null);
    assert_eq!(v["live"]["build"]["id"], hm_core::BUILD_ID);
    assert_eq!(v["live"]["schema"], 15);
    assert!(v["releases_dir"].as_str().unwrap().ends_with("releases"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_snapshot_is_a_real_file_and_keeps_two() {
    let root = temp_root("snapshot");
    let (base, _state) = app(&root).await;
    let mut paths = Vec::new();
    for i in 0..4 {
        let (status, v) = call(
            &base,
            "/api/v0/release/snapshot",
            Some(json!({"tag": format!("t{i}")})),
        )
        .await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["schema"], 15, "{v}");
        let p = PathBuf::from(v["path"].as_str().unwrap());
        assert!(p.is_file(), "{}", p.display());
        assert!(std::fs::metadata(&p).unwrap().len() > 0);
        paths.push(p);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The tag is sanitised into the filename.
    assert!(paths[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains("home-mate-t0-"));
    // Keep the two newest: the first two are pruned.
    let dir = root.join("data/db-snapshots");
    let remaining = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(remaining, 2);
    assert!(!paths[0].exists());
    assert!(!paths[1].exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn install_and_activate_spawn_the_detached_tool_and_reject_when_busy() {
    let root = temp_root("spawn");
    install_fake_tool(&root);
    let (base, _state) = app(&root).await;

    let (status, v) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "http://192.168.1.27:8000/pkg.tar", "activate": true})),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"], "started");
    let lines = wait_called(&root, 1).await;
    assert_eq!(
        lines[0], "install --package http://192.168.1.27:8000/pkg.tar --activate",
        "{lines:?}"
    );

    // Empty body is legal for activate and rollback (no 415).
    let (status, v) = call(&base, "/api/v0/release/activate", None).await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = call(&base, "/api/v0/release/rollback", None).await;
    assert_eq!(status, 200, "{v}");
    let lines = wait_called(&root, 3).await;
    // The installer is detached, so two jobs started back to back race to
    // append their own line; the order between them is not guaranteed and
    // must not be asserted. What matters is that each request produced its
    // own call, with the reason it was given.
    assert!(
        lines.contains(&"activate --reason console".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"rollback --reason console rollback".to_string()),
        "{lines:?}"
    );

    // A live pid with status=running blocks a second job...
    std::fs::create_dir_all(root.join("releases")).unwrap();
    std::fs::write(
        root.join("releases/state.json"),
        json!({"last": {"status": "running", "kind": "activate", "pid": std::process::id(), "started_ms": now_ms()}}).to_string(),
    )
    .unwrap();
    let (status, v) = call(&base, "/api/v0/release/activate", None).await;
    assert_eq!(status, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("already running"));
    let (status, _) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "/data/hm/releases/x.tar"})),
    )
    .await;
    assert_eq!(status, 409);

    // ...but a power cut leaves status=running behind with a dead pid. That
    // must not lock the console out forever (the installer's flock is the
    // hard guard, and a new job will take it).
    std::fs::write(
        root.join("releases/state.json"),
        json!({"last": {"status": "running", "kind": "activate", "pid": 999999, "started_ms": now_ms() - 11 * 60 * 1000}}).to_string(),
    )
    .unwrap();
    let (status, v) = call(&base, "/api/v0/release/activate", None).await;
    assert_eq!(status, 200, "{v}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn install_rejects_a_non_http_or_relative_url_and_reports_a_missing_installer() {
    let root = temp_root("validate");
    let (base, _state) = app(&root).await;
    let (status, v) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "ftp://x"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let (status, v) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "pkg.tar"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let (status, v) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "/tmp/x.tar"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let (status, v) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "http://169.254.169.254/pkg.tar"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let (status, v) = call(
        &base,
        "/api/v0/release/activate",
        Some(json!({"id": "--flag"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    let (status, v) = call(
        &base,
        "/api/v0/release/activate",
        Some(json!({"reason": "--bad"})),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    // Valid URL, but no bootstrapped installer on this host.
    let (status, v) = call(
        &base,
        "/api/v0/release/install",
        Some(json!({"url": "http://192.168.1.27:8000/pkg.tar"})),
    )
    .await;
    assert_eq!(status, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("installer not found"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_log_endpoint_reads_the_bounded_installer_log() {
    let root = temp_root("log");
    let releases = root.join("releases");
    std::fs::create_dir_all(&releases).unwrap();
    let lines: Vec<String> = (0..250).map(|i| format!("{i} line-{i}")).collect();
    std::fs::write(releases.join("installer.log"), lines.join("\n") + "\n").unwrap();
    let (base, _state) = app(&root).await;
    let v = get(&base, "/api/v0/release/log?lines=5").await;
    let got: Vec<&str> = v["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(got.len(), 5);
    assert!(got[4].ends_with("line-249"), "{got:?}");
    assert!(v["path"].as_str().unwrap().ends_with("installer.log"));
    let _ = std::fs::remove_dir_all(&root);
}
