//! §12 update-chain tests: slot write atomicity + API round-trip against a
//! tempdir layout. One test function on purpose (env-var layout setup is
//! process-global; parallel tests in this binary would race it).

use hm_core::api::AppState;
use hm_core::tool::mocks::MockInject;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn current_rss_bytes() -> u64 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb_str) = rest.split_whitespace().next() {
                    if let Ok(kb) = kb_str.parse::<u64>() {
                        return kb * 1024;
                    }
                }
            }
        }
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps -o rss=");
    let kb: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("parse rss kb");
    kb * 1024
}

fn fake_elf(machine: u16, class: u8, version: &str, pad_to: usize) -> Vec<u8> {
    let mut b = vec![0u8; pad_to];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = class;
    b[18..20].copy_from_slice(&machine.to_le_bytes());
    let marker = format!("hm-firmware {} (emos)\0", version);
    let m = marker.as_bytes();
    b[100..100 + m.len()].copy_from_slice(m);
    b
}

fn multipart(bytes: &[u8]) -> reqwest::multipart::Form {
    let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name("fw");
    reqwest::multipart::Form::new().part("file", part)
}

#[tokio::test]
async fn slot_chain_end_to_end() {
    let _guard = TEST_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("hm-fw-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("audio_a"), fake_elf(40, 1, "0.0.1", 120 * 1024)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("audio_a", dir.join("current")).unwrap();

    // AppState reads HM_FW_DIR at construction; single test fn = no races.
    std::env::set_var("HM_FW_DIR", &dir);
    std::env::set_var("HM_ALLOW_UNSIGNED_FWUPDATE", "1");
    let store_dir = std::env::temp_dir().join(format!("hm-fw-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = Arc::new(hm_core::store::Store::open(&store_dir.join("t.db")).unwrap());
    let state = AppState::with_overrides(
        store,
        store_dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );

    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    // Default fail-closed security: unsigned ELF uploads must return 403 unless explicitly enabled.
    std::env::remove_var("HM_ALLOW_UNSIGNED_FWUPDATE");
    let blocked_audio = http
        .post(format!("{base}/api/v0/firmware/audio?restart=false"))
        .multipart(multipart(&fake_elf(40, 1, "0.0.2", 120 * 1024)))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked_audio.status(), reqwest::StatusCode::FORBIDDEN);
    let blocked_core = http
        .post(format!("{base}/api/v0/firmware/core"))
        .multipart(multipart(&fake_elf(183, 2, "0.0.2", 120 * 1024)))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked_core.status(), reqwest::StatusCode::FORBIDDEN);
    std::env::set_var("HM_ALLOW_UNSIGNED_FWUPDATE", "1");

    // GET: layout visible, active slot audio_a, version extracted.
    let body: Value = http
        .get(format!("{base}/api/v0/firmware"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["audio"]["available"], true);
    assert_eq!(body["audio"]["active_slot"], "audio_a");
    assert_eq!(
        body["audio"]["slots"][0]["version"],
        "hm-firmware 0.0.1 (emos)"
    );

    // POST: a newer armv7 payload stages into audio_b and flips current.
    // restart=false keeps the test off /proc scanning. Payload >2MB pins the
    // raised DefaultBodyLimit (axum's 2MB default rejected real firmware).
    let payload = fake_elf(40, 1, "0.0.2", 2600 * 1024);
    let body: Value = http
        .post(format!("{base}/api/v0/firmware/audio?restart=false"))
        .multipart(multipart(&payload))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["staged_slot"], "audio_b");
    assert_eq!(body["version"], "hm-firmware 0.0.2 (emos)");
    assert_eq!(body["restarted"], false);

    // On-disk truth: audio_b replaced, current flipped, no staging litter.
    assert_eq!(
        std::fs::read(dir.join("audio_b")).unwrap().len(),
        2600 * 1024
    );
    assert_eq!(
        std::fs::read_link(dir.join("current"))
            .unwrap()
            .file_name()
            .unwrap(),
        "audio_b"
    );
    assert!(!dir.join(".audio_b.staging").exists());
    assert!(!dir.join("current.staging").exists());

    // Wrong arch is refused without touching slots.
    let bad = fake_elf(183, 2, "9.9.9", 130 * 1024);
    let resp = http
        .post(format!("{base}/api/v0/firmware/audio?restart=false"))
        .multipart(multipart(&bad))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert_eq!(
        std::fs::read_link(dir.join("current"))
            .unwrap()
            .file_name()
            .unwrap(),
        "audio_b"
    );

    // Second update alternates back to the (now inactive) audio_a, and
    // verifies >16 MiB (17 MiB) streaming upload works without hitting a 16 MiB
    // body limit, including marker extraction past a 64 KiB chunk boundary.
    let mut big_payload = fake_elf(40, 1, "0.0.3", 17 * 1024 * 1024);
    // Clear the early marker and place it across the 64 KiB boundary (65530..).
    big_payload[100..140].fill(0);
    let boundary_marker = b"hm-firmware 0.0.3-big (emos)\0";
    big_payload[65530..65530 + boundary_marker.len()].copy_from_slice(boundary_marker);
    let body: Value = http
        .post(format!("{base}/api/v0/firmware/audio?restart=false"))
        .multipart(multipart(&big_payload))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["staged_slot"], "audio_a");
    assert_eq!(body["version"], "hm-firmware 0.0.3-big (emos)");
    assert_eq!(body["size"], 17 * 1024 * 1024);
    assert_eq!(
        std::fs::read_link(dir.join("current"))
            .unwrap()
            .file_name()
            .unwrap(),
        "audio_a"
    );
    assert!(!dir.join(".audio_a.staging").exists());

    // Failed validation after streaming must clean up the staging file.
    let bad_stream = vec![0u8; 200 * 1024]; // not an ELF
    let resp = http
        .post(format!("{base}/api/v0/firmware/audio?restart=false"))
        .multipart(multipart(&bad_stream))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(!dir.join(".audio_b.staging").exists());
    assert_eq!(
        std::fs::read_link(dir.join("current"))
            .unwrap()
            .file_name()
            .unwrap(),
        "audio_a"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&store_dir);
}

#[tokio::test]
async fn streaming_20mib_upload_succeeds_with_bounded_rss() {
    let _guard = TEST_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("hm-fw-20m-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("audio_a"), fake_elf(40, 1, "0.0.1", 120 * 1024)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("audio_a", dir.join("current")).unwrap();

    std::env::set_var("HM_FW_DIR", &dir);
    std::env::set_var("HM_ALLOW_UNSIGNED_FWUPDATE", "1");
    let store_dir = std::env::temp_dir().join(format!("hm-fw-20m-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = Arc::new(hm_core::store::Store::open(&store_dir.join("t.db")).unwrap());
    let state = AppState::with_overrides(
        store,
        store_dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );

    let app = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Warm up allocator / HTTP pipeline with a tiny GET first before sampling RSS.
    {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            b"GET /api/v0/firmware HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
    }

    let rss_before = current_rss_bytes();
    const PAYLOAD_BYTES: usize = 20 * 1024 * 1024;
    const CHUNK_BYTES: usize = 64 * 1024;
    const NUM_CHUNKS: usize = PAYLOAD_BYTES / CHUNK_BYTES;

    let boundary = "----hmfw20mibboundary7d9a";
    let part_header = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fw-20m.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    );
    let part_trailer = format!("\r\n--{boundary}--\r\n");
    let content_length = part_header.len() + PAYLOAD_BYTES + part_trailer.len();

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req_header = format!(
        "POST /api/v0/firmware/audio?restart=false HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req_header.as_bytes()).await.unwrap();
    stream.write_all(part_header.as_bytes()).await.unwrap();

    // First 64 KiB chunk holds the ELF header + version marker; remaining 319 chunks are zeroed.
    let first_chunk = fake_elf(40, 1, "0.0.4-20m", CHUNK_BYTES);
    let zero_chunk = [0u8; CHUNK_BYTES];
    stream.write_all(&first_chunk).await.unwrap();
    let mut max_rss_during = current_rss_bytes();
    for i in 1..NUM_CHUNKS {
        stream.write_all(&zero_chunk).await.unwrap();
        if i % 64 == 0 {
            max_rss_during = max_rss_during.max(current_rss_bytes());
        }
    }
    stream.write_all(part_trailer.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw_resp = Vec::new();
    stream.read_to_end(&mut raw_resp).await.unwrap();
    let rss_after = current_rss_bytes().max(max_rss_during);

    let resp_str = String::from_utf8_lossy(&raw_resp);
    assert!(
        resp_str.starts_with("HTTP/1.1 200 OK"),
        "expected HTTP 200 OK, got: {resp_str}"
    );
    let body_start = resp_str.find("\r\n\r\n").unwrap() + 4;
    let body: Value = serde_json::from_str(&resp_str[body_start..]).unwrap();
    assert_eq!(body["staged_slot"], "audio_b");
    assert_eq!(body["version"], "hm-firmware 0.0.4-20m (emos)");
    assert_eq!(body["size"], PAYLOAD_BYTES as u64);

    let staged_meta = std::fs::metadata(dir.join("audio_b")).unwrap();
    assert_eq!(staged_meta.len(), PAYLOAD_BYTES as u64);

    let rss_growth = rss_after.saturating_sub(rss_before);
    assert!(
        rss_growth < 15 * 1024 * 1024,
        "RSS grew by {} bytes ({:.2} MiB) during 20 MiB streaming upload; expected < 15 MiB",
        rss_growth,
        rss_growth as f64 / (1024.0 * 1024.0)
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&store_dir);
}
