//! Host integration test for the whole M8a release path: pack → install →
//! activate → health → rollback, against a stub management API and a fake
//! device root. The real device drills are in docs/d/m8a-20261113.md.
//!
//! A debug build accepts `HM_RELEASE_PUBKEY_OVERRIDE`; this test signs with a
//! throwaway key so the production private key never enters the repository.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hm_release::device::Paths;
use hm_release::manifest::Manifest;
use hm_release::{activate, crypto, install, pack, state};

const TEST_SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

fn test_pubkey() -> String {
    use ed25519_dalek::SigningKey;
    use std::convert::TryInto;
    let bytes: [u8; 32] = hm_release::hexutil::from_hex(TEST_SEED)
        .unwrap()
        .try_into()
        .unwrap();
    hm_release::hexutil::to_hex(SigningKey::from_bytes(&bytes).verifying_key().as_bytes())
}

struct Stub {
    base: String,
    state: Arc<Mutex<StubState>>,
}

struct StubState {
    build_id: String,
    firmware: String,
    console_index: String,
    /// A node that has never had a word list: the spotter is DOWN until
    /// something deploys one. This is what a freshly seeded machine looks
    /// like (dot2-b, 2026-11-15) and what the pre-health word push is for.
    kws_running: bool,
    /// How many `/reflex/deploy` calls this node received.
    deploys: u32,
}

fn start_stub(root: PathBuf, build_id: &str, console_index: &str) -> Stub {
    start_stub_with(root, build_id, console_index, true)
}

fn start_stub_with(root: PathBuf, build_id: &str, console_index: &str, kws_running: bool) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stub_state = Arc::new(Mutex::new(StubState {
        build_id: build_id.to_string(),
        firmware: "hm-firmware 0.0.44 testbuild (emos)".into(),
        console_index: console_index.to_string(),
        kws_running,
        deploys: 0,
    }));
    let shared = stub_state.clone();
    let root_for_thread = root.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            // Read until the request headers (and any body) are complete;
            // answering before draining the body resets the connection.
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let mut want_body = 0usize;
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if want_body == 0 {
                    if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
                        want_body = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if raw.len() >= split + 4 + want_body {
                            break;
                        }
                    }
                } else if raw.len()
                    >= raw
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4 + want_body)
                        .unwrap_or(usize::MAX)
                {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&raw).to_string();
            // "GET /api/v0/mode": the method decides, because /reflex/deploy
            // answers differently from a read of the same path.
            let mut parts = req.split_whitespace();
            let method = parts.next().unwrap_or("GET").to_string();
            let path = parts.next().unwrap_or("/").to_string();
            let (code, body) = respond(&format!("{method} {path}"), &root_for_thread, &shared);
            let head = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    Stub {
        base: format!("http://{addr}"),
        state: stub_state,
    }
}

fn respond(path: &str, root: &Path, st: &Mutex<StubState>) -> (u16, String) {
    let mut st = st.lock().unwrap();
    let (method, path) = path.split_once(' ').unwrap_or(("GET", path));
    match (method, path) {
        ("GET", "/api/v0/status") => (
            200,
            serde_json::json!({
                "build": {"version": "0.0.1", "id": st.build_id},
                "gateway": {
                    "connected": true,
                    "gateway": "home-mate-device-gateway 0.0.44 (dot2-direct)",
                    "devices": [{"online": true, "model": "AEOBC", "firmware": st.firmware}]
                }
            })
            .to_string(),
        ),
        ("GET", "/api/v0/mode") => (
            200,
            serde_json::json!({
                "devices": [{
                    "model": "AEOBC",
                    "firmware": st.firmware,
                    "kws": {"state": if st.kws_running { "running" } else { "down" }},
                    "reflex": {"deploy": {"loaded_hash": if st.kws_running { "cafe0001" } else { "" }}},
                    "mic": {"vad": {"endpoint": {
                        "kind": "silero", "fallback": null,
                        "model": "/data/hm/data/models/silero_vad.onnx",
                        "model_detail": "loaded in 10 ms"
                    }}}
                }]
            })
            .to_string(),
        ),
        ("POST", "/api/v0/reflex/deploy") => {
            // The real core compiles the saved table into kw.txt and the
            // spotter comes up on it: from here on the node reports running.
            st.deploys += 1;
            st.kws_running = true;
            (
                200,
                serde_json::json!({"state": "sent", "hash": "cafe0001", "words": 9}).to_string(),
            )
        }
        (_, "/api/v0/release/snapshot") => {
            let dir = root.join("data/db-snapshots");
            std::fs::create_dir_all(&dir).unwrap();
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = dir.join(format!("snap-{n}.db"));
            std::fs::write(&path, b"fake-db-with-wal-content").unwrap();
            (
                200,
                serde_json::json!({"path": path.display().to_string(), "schema": 5, "ms": 1})
                    .to_string(),
            )
        }
        ("POST", "/api/v0/system/restart") => (200, "{}".into()),
        ("GET", "/") => (
            200,
            format!(
                "<html><body><script src=\"/{}\"></script></body></html>",
                st.console_index
            ),
        ),
        _ => (404, "{}".into()),
    }
}

fn fake_elf_aarch64(build: &str) -> Vec<u8> {
    let mut v = vec![0u8; 1_100_000];
    v[0..4].copy_from_slice(b"\x7fELF");
    v[4] = 2; // ELFCLASS64
    v[18] = 183;
    let marker = format!("hm-core 0.0.1 {build}\0");
    v[4096..4096 + marker.len()].copy_from_slice(marker.as_bytes());
    // The fat-core marker: pack refuses a core without it (same shape the
    // real build.rs emits for the default feature set).
    let vad = b"hm-core-vad silero\0";
    v[8192..8192 + vad.len()].copy_from_slice(vad);
    v
}

fn fake_elf_arm() -> Vec<u8> {
    let mut v = vec![0u8; 200_000];
    v[0..4].copy_from_slice(b"\x7fELF");
    v[4] = 1; // ELFCLASS32
    v[18] = 40;
    let a = b"hm-firmware 0.0.44 testbuild (emos)\0";
    v[4096..4096 + a.len()].copy_from_slice(a);
    let b = b"home-mate-device-gateway 0.0.44 (dot2-direct)\0";
    v[8192..8192 + b.len()].copy_from_slice(b);
    v
}

fn build_release_dir(root: &Path, build: &str) -> PathBuf {
    let dir = root.join(format!("build-{build}"));
    std::fs::create_dir_all(dir.join("components/web")).unwrap();
    std::fs::create_dir_all(dir.join("components/model")).unwrap();
    std::fs::write(dir.join("components/core-aarch64"), fake_elf_aarch64(build)).unwrap();
    std::fs::write(dir.join("components/firmware-armv7"), fake_elf_arm()).unwrap();
    std::fs::write(
        dir.join("components/web/index.html"),
        b"<html><script src=\"/index-test.js\"></script></html>",
    )
    .unwrap();
    std::fs::write(
        dir.join("components/web/index-test.js"),
        b"console.log('hi')",
    )
    .unwrap();
    std::fs::write(
        dir.join("components/model/silero_vad.onnx"),
        b"fake-onnx-model",
    )
    .unwrap();
    dir
}

fn pack_release(root: &Path, build: &str, serial: u64, model: &str) -> PathBuf {
    let dir = build_release_dir(root, build);
    pack_release_from(root, &dir, build, serial, model, false)
}

/// The same packer with the emergency slim-core switch: a test uses it to
/// produce the drill package `pack --allow-slim-core` would build, so install
/// admission can be shown to refuse it.
fn pack_release_from(
    root: &Path,
    dir: &Path,
    build: &str,
    serial: u64,
    model: &str,
    allow_slim_core: bool,
) -> PathBuf {
    let key = root.join("test-key.hex");
    if !key.exists() {
        std::fs::write(&key, TEST_SEED).unwrap();
    }
    let prefix = root.join(format!("pkg-{build}"));
    let opts = pack::PackOpts {
        dir: dir.to_path_buf(),
        out_prefix: prefix.clone(),
        serial,
        version: format!("test-{serial}"),
        notes: String::new(),
        model: model.into(),
        profile: "dot2d-emos".into(),
        firmware_min: "0.0.44".into(),
        firmware_max: "0.0.99".into(),
        key,
        allow_dirty: true,
        allow_slim_core,
        web_commit: Some("test-commit".into()),
        kws_bundle: None,
    };
    // Fixture model: the admission pin is proven by pack's own unit tests
    // against a real hash; these packages carry a synthetic model, so the
    // packer/installer seams are told that hash explicitly.
    let packed =
        pack::run_with_expected_model(&opts, &hm_release::crypto::sha256_bytes(FIXTURE_MODEL))
            .unwrap();
    packed.tar
}

const FIXTURE_MODEL: &[u8] = b"fake-onnx-model";

/// `install::run` for fixture packages: same flow, fixture model hash.
fn install_fixture(
    paths: &Paths,
    package: &str,
    activate: bool,
    drill: Option<u64>,
) -> Result<i32, String> {
    install::run_with_model(
        paths,
        package,
        activate,
        drill,
        &hm_release::crypto::sha256_bytes(FIXTURE_MODEL),
    )
}

fn device(paths: &Paths, stub: &Stub) {
    std::fs::create_dir_all(paths.core_bin_dir()).unwrap();
    std::fs::create_dir_all(&paths.fw_root).unwrap();
    std::fs::create_dir_all(paths.web()).unwrap();
    std::fs::create_dir_all(paths.model_path().parent().unwrap()).unwrap();
    std::fs::write(paths.core_bin_dir().join("core_a"), b"old-core").unwrap();
    std::fs::write(&paths.fw_root.join("audio_a"), b"old-firmware").unwrap();
    std::fs::write(paths.web().join("index.html"), b"old console").unwrap();
    std::fs::write(paths.model_path(), b"old-model").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("core_a", paths.core_bin_dir().join("current")).unwrap();
        std::os::unix::fs::symlink("audio_a", paths.fw_root.join("current")).unwrap();
    }
    let _ = stub;
}

#[test]
fn release_lifecycle_install_activate_rollback() {
    // Debug-only test seam: a throwaway signing key.
    std::env::set_var("HM_RELEASE_PUBKEY_OVERRIDE", test_pubkey());
    std::env::set_var("HM_RELEASE_PROFILE", "dot2d-emos");
    std::env::set_var("HM_RELEASE_HEALTH_BUDGET_S", "3");
    std::env::set_var("HM_RELEASE_FIRMWARE_BUDGET_S", "2");
    std::env::set_var("HM_RELEASE_ROLLBACK_BUDGET_S", "3");

    let root = std::env::temp_dir().join(format!("hm-rel-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let paths = Paths::for_test(root.join("hm"), root.join("fw"));
    let stub = start_stub(paths.root.clone(), "aaaa1111", "index-test.js");
    device(&paths, &stub);
    let paths = Paths {
        root: paths.root,
        fw_root: paths.fw_root,
        core_url: stub.base.clone(),
    };

    // --- R1: pack, install, activate -------------------------------------
    let r1 = pack_release(&root, "aaaa1111", 1, "AEOBC");
    install_fixture(&paths, r1.to_str().unwrap(), false, None).unwrap();
    let id1 = "1-test-1";
    assert!(paths.releases().join(id1).join("manifest.json").is_file());
    let st = state::State::load(&paths.state_path()).unwrap();
    assert_eq!(st.last.as_ref().unwrap().status, "ok");
    assert!(st.active.is_none(), "install must not activate");

    let code = activate::run(&paths, Some(id1.into()), false, "e2e").unwrap();
    assert_eq!(code, 0, "R1 activation must succeed");
    let st = state::State::load(&paths.state_path()).unwrap();
    let active = st.active.as_ref().expect("active release recorded");
    assert_eq!(active.id, id1);
    assert_eq!(active.build_id, "aaaa1111");
    assert_eq!(st.previous, None);
    let last = st.last.as_ref().unwrap();
    assert_eq!(last.status, "ok");
    assert!(last.criteria.iter().all(|c| c.ok), "{:?}", last.criteria);
    assert_eq!(
        last.to.as_ref().map(|t| t.id.as_str()),
        Some(id1),
        "history records where it activated to"
    );
    assert!(last.ended_ms > 0);

    // Re-activating the same release is a repair, not a new generation: the
    // previous release must not become a self-reference.
    let code = activate::run(&paths, Some(id1.into()), false, "e2e re-activate").unwrap();
    assert_eq!(code, 0);
    let st = state::State::load(&paths.state_path()).unwrap();
    assert_eq!(
        st.previous, None,
        "re-activation must not overwrite previous with itself: {:?}",
        st.previous
    );
    assert_eq!(st.active.as_ref().unwrap().id, id1);

    // Slots after the repair activation: the pointer rotated to the other
    // slot, which now carries the verified release bytes.
    assert_eq!(
        std::fs::read_link(paths.core_bin_dir().join("current")).unwrap(),
        Path::new("core_a")
    );
    assert_eq!(
        std::fs::read_link(paths.fw_root.join("current")).unwrap(),
        Path::new("audio_a")
    );
    assert_eq!(
        crypto::sha256_file(&paths.core_bin_dir().join("core_a")).unwrap(),
        crypto::sha256_bytes(&fake_elf_aarch64("aaaa1111"))
    );
    // Console + model now point into the release; the originals were adopted.
    assert!(std::fs::read_link(paths.web())
        .unwrap()
        .ends_with("components/web"));
    assert!(std::fs::read_link(paths.model_path())
        .unwrap()
        .ends_with("components/model/silero_vad.onnx"));
    assert!(paths
        .releases()
        .join(".bootstrap")
        .read_dir()
        .unwrap()
        .any(|e| {
            e.map(|e| e.file_name().to_string_lossy().starts_with("web-"))
                .unwrap_or(false)
        }));

    // --- Rejections: tampered bytes, wrong model, replay ------------------
    let r2 = pack_release(&root, "bbbb2222", 2, "AEOBC");
    let raw = std::fs::read(&r2).unwrap();
    let pos = raw.windows(9).position(|w| w == b"hm-core 0").unwrap();
    let mut bad = raw.clone();
    bad[pos + 16] ^= 0x01; // inside the build marker in the core payload
    let bad_path = root.join("tampered.tar");
    std::fs::write(&bad_path, &bad).unwrap();
    let err = install_fixture(&paths, bad_path.to_str().unwrap(), false, None).unwrap_err();
    assert!(err.contains("sha256 mismatch"), "{err}");
    assert!(
        state::State::load(&paths.state_path())
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .id
            == id1
    );

    let wrong_model = pack_release(&root, "cccc3333", 9, "OTHERMODEL");
    let err = install_fixture(&paths, wrong_model.to_str().unwrap(), false, None).unwrap_err();
    assert!(err.contains("model mismatch"), "{err}");

    let err = install_fixture(&paths, r1.to_str().unwrap(), false, None).unwrap_err();
    assert!(err.contains("replay rejected"), "{err}");

    // --- R2: installs, but activation health fails → automatic rollback ---
    // Simulate the new core's migration: the sidecar now says schema 6 while
    // the snapshot was taken at 5, so the rollback must schedule a DB restore.
    std::fs::write(
        paths.schema_sidecar(),
        serde_json::json!({"schema": 6, "build": "bbbb2222", "ms": 1}).to_string(),
    )
    .unwrap();
    install_fixture(&paths, r2.to_str().unwrap(), false, None).unwrap();
    let id2 = "2-test-2";
    // The stub keeps reporting aaaa1111, so the new build identity never
    // confirms: this is the "new core came up but is not the expected build"
    // failure mode, and it must roll back without human help.
    let code = activate::run(&paths, Some(id2.into()), false, "e2e poison").unwrap();
    assert_eq!(code, 1, "failed activation reports a completed rollback");
    let st = state::State::load(&paths.state_path()).unwrap();
    assert_eq!(
        st.active.as_ref().unwrap().id,
        id1,
        "active release unchanged"
    );
    let last = st.last.as_ref().unwrap();
    assert_eq!(last.status, "rolled_back");
    let failed = last
        .criteria
        .iter()
        .find(|c| c.name == "core_status")
        .unwrap();
    assert!(!failed.ok);
    let rollback = last
        .criteria
        .iter()
        .find(|c| c.name == "rollback_core_status")
        .unwrap();
    assert!(rollback.ok, "{rollback:?}");
    assert_eq!(
        last.to.as_ref().map(|t| t.id.as_str()),
        Some(id1),
        "rolled back to R1"
    );
    assert!(last.ended_ms > 0);
    for run in &st.history {
        if run.status != "running" && run.status != "rolling_back" {
            assert!(
                run.ended_ms > 0,
                "finished history rows must have an end time: {run:?}"
            );
        }
    }
    assert_eq!(
        std::fs::read_link(paths.core_bin_dir().join("current")).unwrap(),
        Path::new("core_a")
    );
    assert_eq!(
        std::fs::read_link(paths.fw_root.join("current")).unwrap(),
        Path::new("audio_a")
    );
    assert!(std::fs::read_link(paths.web())
        .unwrap()
        .ends_with("components/web"));
    let marker: serde_json::Value = serde_json::from_slice(
        &std::fs::read(paths.releases().join("restore-on-boot.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(marker["for_build"], "aaaa1111");
    let manifest: Manifest = Manifest::from_bytes(
        &std::fs::read(paths.releases().join(id1).join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.release.serial, 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn verify_rejects_a_package_whose_manifest_was_edited() {
    std::env::set_var("HM_RELEASE_PUBKEY_OVERRIDE", test_pubkey());
    let root = std::env::temp_dir().join(format!("hm-rel-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let tar = pack_release(&root, "dddd4444", 4, "AEOBC");
    // Editing the manifest inside the archive changes the signed bytes even
    // if the JSON stays parseable.
    let raw = std::fs::read(&tar).unwrap();
    let pos = raw
        .windows(16)
        .position(|w| w == b"manifest_version")
        .unwrap();
    let mut edited = raw.clone();
    // Flip a byte inside the signed JSON body (the version digit).
    let body = pos + 20;
    edited[body] = if edited[body] == b'1' { b'2' } else { b'1' };
    let p = root.join("edited.tar");
    std::fs::write(&p, edited).unwrap();
    // Verify with the fixture model hash so the failure under test is the
    // edited manifest, not the admission model pin.
    let f = std::fs::File::open(&p).unwrap();
    assert!(hm_release::package::verify_for_device_with_model(
        std::io::BufReader::new(f),
        None,
        |_| Ok(()),
        &hm_release::crypto::sha256_bytes(FIXTURE_MODEL),
    )
    .is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// dot2-b, 2026-11-15: a freshly seeded node has no `kw.txt` (by design), so
/// the keyword sidecar is down and the health gate's `kws.state = running`
/// could never pass — the FIRST `install --activate` on an empty machine
/// always rolled back. Activation now pushes the node's word list before the
/// health gate, so one command brings the node up. Without that push this
/// test hangs at health until the budget expires and returns a rollback.
#[test]
fn a_fresh_node_activates_because_activation_pushes_the_word_list() {
    std::env::set_var("HM_RELEASE_PUBKEY_OVERRIDE", test_pubkey());
    std::env::set_var("HM_RELEASE_PROFILE", "dot2d-emos");
    std::env::set_var("HM_RELEASE_HEALTH_BUDGET_S", "3");
    std::env::set_var("HM_RELEASE_FIRMWARE_BUDGET_S", "2");
    std::env::set_var("HM_RELEASE_ROLLBACK_BUDGET_S", "2");

    let root = std::env::temp_dir().join(format!("hm-rel-fresh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = Paths::for_test(root.join("hm"), root.join("fw"));
    // kws_running = false: this node has never been given words.
    let stub = start_stub_with(base.root.clone(), "eeee5555", "index-test.js", false);
    device(&base, &stub);
    let paths = Paths {
        root: base.root,
        fw_root: base.fw_root,
        core_url: stub.base.clone(),
    };

    let tar = pack_release(&root, "eeee5555", 5, "AEOBC");
    install_fixture(&paths, tar.to_str().unwrap(), false, None).unwrap();
    let id = "5-test-5";
    let code = activate::run(&paths, Some(id.into()), false, "fresh node").unwrap();
    assert_eq!(
        code, 0,
        "the first activation on an empty node must not roll back"
    );

    let st = state::State::load(&paths.state_path()).unwrap();
    assert_eq!(st.active.as_ref().map(|a| a.id.as_str()), Some(id));
    assert_eq!(st.last.as_ref().unwrap().status, "ok");
    assert_eq!(
        stub.state.lock().unwrap().deploys,
        1,
        "exactly one word push: enough to start the spotter, not a restart loop"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The other half of the rule: a node whose spotter is already running must
/// not be disturbed. A deploy restarts the sidecar (~7 s), so a re-activation
/// that pushed words anyway would turn every repair activation into a
/// several-second deaf window.
#[test]
fn a_healthy_node_is_not_sent_words_again() {
    std::env::set_var("HM_RELEASE_PUBKEY_OVERRIDE", test_pubkey());
    std::env::set_var("HM_RELEASE_PROFILE", "dot2d-emos");
    std::env::set_var("HM_RELEASE_HEALTH_BUDGET_S", "3");
    std::env::set_var("HM_RELEASE_FIRMWARE_BUDGET_S", "2");
    std::env::set_var("HM_RELEASE_ROLLBACK_BUDGET_S", "2");

    let root = std::env::temp_dir().join(format!("hm-rel-happy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = Paths::for_test(root.join("hm"), root.join("fw"));
    let stub = start_stub_with(base.root.clone(), "ffff6666", "index-test.js", true);
    device(&base, &stub);
    let paths = Paths {
        root: base.root,
        fw_root: base.fw_root,
        core_url: stub.base.clone(),
    };

    let tar = pack_release(&root, "ffff6666", 6, "AEOBC");
    install_fixture(&paths, tar.to_str().unwrap(), false, None).unwrap();
    let code = activate::run(&paths, Some("6-test-6".into()), false, "healthy node").unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        stub.state.lock().unwrap().deploys,
        0,
        "a running spotter must be left alone"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `pack --allow-slim-core` exists for a field drill, and the package it
/// produces still cannot be installed: admission reads the `hm-core-vad`
/// marker out of the extracted core and refuses a slim one. Nothing lands in
/// `releases/`, so the device keeps running exactly what it had.
#[test]
fn a_slim_core_package_is_refused_by_install_admission() {
    std::env::set_var("HM_RELEASE_PUBKEY_OVERRIDE", test_pubkey());
    std::env::set_var("HM_RELEASE_PROFILE", "dot2d-emos");

    let root = std::env::temp_dir().join(format!("hm-rel-slim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = Paths::for_test(root.join("hm"), root.join("fw"));
    let stub = start_stub_with(base.root.clone(), "11110000", "index-test.js", true);
    device(&base, &stub);
    let paths = Paths {
        root: base.root,
        fw_root: base.fw_root,
        core_url: stub.base.clone(),
    };

    // Turn the fixture's fat core into the slim shape build.rs emits for
    // `--no-default-features`: same bytes, marker says `none`.
    let dir = build_release_dir(&root, "11110000");
    let core_path = dir.join("components/core-aarch64");
    let mut core = std::fs::read(&core_path).unwrap();
    let fat = b"hm-core-vad silero\0";
    let at = core
        .windows(fat.len())
        .position(|w| w == fat)
        .expect("fixture core carries the fat marker");
    let mut slim = b"hm-core-vad none\0".to_vec();
    slim.resize(fat.len(), 0);
    core[at..at + fat.len()].copy_from_slice(&slim);
    std::fs::write(&core_path, &core).unwrap();

    let tar = pack_release_from(&root, &dir, "11110000", 5, "AEOBC", true);
    let err = install_fixture(&paths, tar.to_str().unwrap(), false, None).unwrap_err();
    assert!(err.contains("vad backend 'none'"), "{err}");
    assert!(
        err.starts_with("core: "),
        "install must name the core: {err}"
    );
    assert!(
        !paths.releases().join("5-test-5").exists(),
        "a rejected package must not be committed"
    );
    assert!(state::State::load(&paths.state_path())
        .unwrap()
        .active
        .is_none());
    let _ = std::fs::remove_dir_all(&root);
}
