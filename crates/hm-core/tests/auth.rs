//! M9 household-plane authentication — HTTP-level proof over a real listener.
//!
//! Two things are being proven:
//!
//! 1. the release chain: `hm-release` runs on the device and calls the core
//!    over loopback (snapshot / status / restart / health), so a loopback peer
//!    must keep working with no token;
//! 2. a LAN peer is refused until it presents the household bearer, and a
//!    rotation kills the old one.
//!
//! The LAN cases inject the peer address into the request extensions — that is
//! exactly the input `axum`'s `into_make_service_with_connect_info` supplies in
//! production (`main.rs`), and it keeps the suite independent of whichever
//! interface a dev machine happens to have (a VPN tunnel is not a LAN).

use hm_core::api::{router, AppState};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

struct App {
    state: Arc<AppState>,
    base: String,
    _dir: PathBuf,
}

/// `lan = true` makes every request look like it arrived from 192.168.1.9
/// (private RFC1918 LAN) instead of loopback.
async fn spawn(web: Option<String>, lan: bool) -> App {
    let dir = std::env::temp_dir().join(format!(
        "hm-auth-test-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let web_dir = web.map(|html| {
        let d = dir.join("web");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), html).unwrap();
        d
    });
    let store = Arc::new(Store::open_in_memory().unwrap());
    let state = AppState::with_overrides(
        store,
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        web_dir,
    );
    let mut app = router(state.clone());
    if lan {
        app = app.layer(axum::middleware::from_fn(fake_lan_peer));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    App {
        state,
        base: format!("http://127.0.0.1:{port}"),
        _dir: dir,
    }
}

async fn fake_lan_peer(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(axum::extract::ConnectInfo(
        "192.168.1.9:54321".parse::<SocketAddr>().unwrap(),
    ));
    next.run(req).await
}

async fn get(base: &str, path: &str, token: Option<&str>) -> (u16, Option<String>, Value) {
    let mut req = reqwest::Client::new().get(format!("{base}{path}"));
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let www = resp
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.text().await.unwrap();
    (
        status,
        www,
        serde_json::from_str(&body).unwrap_or(Value::Null),
    )
}

async fn post(base: &str, path: &str, token: Option<&str>) -> (u16, Value) {
    let mut req = reqwest::Client::new().post(format!("{base}{path}"));
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn unpaired_api_is_open_and_says_so() {
    let app = spawn(None, true).await;
    let (status, _, body) = get(&app.base, "/api/v0/status", None).await;
    assert_eq!(status, 200, "unpaired device must be pair-able");
    assert_eq!(body["auth"]["configured"], false);
    let (status, _, body) = get(&app.base, "/api/v0/auth", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["configured"], false);
}

/// The release chain: `hm-release` speaks to 127.0.0.1 and carries no token.
#[tokio::test]
async fn loopback_stays_trusted_after_pairing() {
    let app = spawn(None, false).await;
    let (status, body) = post(&app.base, "/api/v0/auth/provision", None).await;
    assert_eq!(status, 200);
    assert!(body["token"].as_str().unwrap().starts_with("hm1_"));
    let (status, _, body) = get(&app.base, "/api/v0/status", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["auth"]["configured"], true);
    // The exact endpoints hm-release calls (restart is omitted on purpose: its
    // handler exits the process after 400 ms).
    let (status, _) = post(&app.base, "/api/v0/release/snapshot", None).await;
    assert_ne!(status, 401, "snapshot must not be gated on loopback");
    for path in ["/api/v0/status", "/api/v0/mode", "/api/v0/release"] {
        let (status, _, _) = get(&app.base, path, None).await;
        assert_eq!(status, 200, "{path} must stay readable for the installer");
    }
    // Non-release management endpoints on loopback require either the household
    // Bearer token or `<data_dir>/local-release.token` (#231).
    let (status, _, _) = get(&app.base, "/api/v0/config/services/llm", None).await;
    assert_eq!(
        status, 401,
        "non-release routes on loopback must reject unauthenticated requests once paired"
    );
    let local_tok =
        std::fs::read_to_string(hm_core::release::local_token_path(&app.state.data_dir))
            .expect("local-release.token created on startup");
    let local_tok = local_tok.trim();
    let (status, _, _) = get(&app.base, "/api/v0/status", Some(local_tok)).await;
    assert_eq!(
        status, 200,
        "local-release.token must authenticate loopback release routes"
    );
}

#[tokio::test]
async fn lan_requires_a_bearer_and_survives_rotation() {
    let app = spawn(None, true).await;
    let (_, body) = post(&app.base, "/api/v0/auth/provision", None).await;
    let token = body["token"].as_str().unwrap().to_string();

    let (status, www, _) = get(&app.base, "/api/v0/status", None).await;
    assert_eq!(status, 401);
    assert!(www.unwrap().starts_with("Bearer"));

    let (status, _, body) = get(&app.base, "/api/v0/status", Some(&token)).await;
    assert_eq!(status, 200);
    assert_eq!(body["auth"]["configured"], true);

    // A token-shaped wrong value is still wrong.
    let wrong = format!("hm1_{}", "A".repeat(43));
    let (status, _, _) = get(&app.base, "/api/v0/status", Some(&wrong)).await;
    assert_eq!(status, 401);

    // Rotation (authenticated) invalidates the old token everywhere.
    let (status, body) = post(&app.base, "/api/v0/auth/rotate", Some(&token)).await;
    assert_eq!(status, 200);
    let fresh = body["token"].as_str().unwrap().to_string();
    assert_ne!(fresh, token);
    let (status, _, _) = get(&app.base, "/api/v0/status", Some(&token)).await;
    assert_eq!(status, 401, "old token must die on rotation");
    let (status, _, _) = get(&app.base, "/api/v0/status", Some(&fresh)).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn bootstrap_entries_are_open_but_provision_is_one_shot() {
    let app = spawn(None, true).await;
    // A LAN client can pair the device only while it was never paired.
    let (status, body) = post(&app.base, "/api/v0/auth/provision", None).await;
    assert_eq!(status, 200);
    assert!(body["token"].is_string());
    let (status, _) = post(&app.base, "/api/v0/auth/provision", None).await;
    assert_eq!(status, 409, "second provisioning must not overwrite");
    // Status stays readable without a token (the console needs it to decide),
    // but omits token metadata until authenticated.
    let token = body["token"].as_str().unwrap().to_string();
    let (status, _, body) = get(&app.base, "/api/v0/auth", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["configured"], true);
    assert_eq!(body["corrupt"], false);
    assert!(
        body.get("tokens").is_none(),
        "unauthenticated /auth must omit token metadata"
    );
    // Authenticated callers receive the full summary (without hashes).
    let (status, _, body) = get(&app.base, "/api/v0/auth", Some(&token)).await;
    assert_eq!(status, 200);
    assert_eq!(body["configured"], true);
    assert_eq!(body["tokens"].as_array().unwrap().len(), 1);
    assert!(
        body["tokens"][0].get("sha256").is_none(),
        "hash is never exposed"
    );
    // Rotation is not a bootstrap entry.
    let (status, _) = post(&app.base, "/api/v0/auth/rotate", None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn api_responses_carry_security_headers() {
    let app = spawn(None, true).await;
    let (_, body) = post(&app.base, "/api/v0/auth/provision", None).await;
    let token = body["token"].as_str().unwrap().to_string();

    let resp = reqwest::Client::new()
        .get(format!("{}/api/v0/status", app.base))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers()
            .get("x-frame-options")
            .and_then(|v| v.to_str().ok()),
        Some("DENY")
    );
    assert_eq!(
        resp.headers()
            .get("referrer-policy")
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );

    // 401 responses on /api/* also include the security headers.
    let denied = reqwest::Client::new()
        .get(format!("{}/api/v0/status", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 401);
    assert_eq!(
        denied
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        denied
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
}

#[tokio::test]
async fn console_files_are_open_but_api_is_not() {
    let app = spawn(Some("<!doctype html><title>hm</title>".into()), true).await;
    let (_, body) = post(&app.base, "/api/v0/auth/provision", None).await;
    let token = body["token"].as_str().unwrap().to_string();
    let resp = reqwest::get(format!("{}/", app.base)).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.text().await.unwrap().contains("<!doctype html>"));
    let (status, _, _) = get(&app.base, "/api/v0/auth", None).await;
    assert_eq!(status, 200, "the auth status entry is the console's gate");
    let (status, _, _) = get(&app.base, "/api/v0/nope", None).await;
    assert_eq!(status, 401, "unknown API paths are gated too");
    let (status, _, _) = get(&app.base, "/api/v0/nope", Some(&token)).await;
    assert_eq!(
        status, 404,
        "authenticated unknown path stays an honest 404"
    );
}

#[tokio::test]
async fn no_endpoint_echoes_the_token() {
    let app = spawn(None, true).await;
    let (_, body) = post(&app.base, "/api/v0/auth/provision", None).await;
    let token = body["token"].as_str().unwrap().to_string();
    for path in ["/api/v0/auth", "/api/v0/status"] {
        let (status, _, body) = get(&app.base, path, Some(&token)).await;
        assert_eq!(status, 200);
        assert!(
            !body.to_string().contains(&token),
            "{path} leaked the token"
        );
    }
    assert_eq!(app.state.auth.summaries().len(), 1);
}

#[tokio::test]
async fn repeated_invalid_bearer_tokens_trigger_429_without_affecting_loopback() {
    let lan_app = spawn(None, true).await;
    let (_, body) = post(&lan_app.base, "/api/v0/auth/provision", None).await;
    let token = body["token"].as_str().unwrap().to_string();

    // Valid token works before hitting the failure threshold.
    let (status, _, _) = get(&lan_app.base, "/api/v0/status", Some(&token)).await;
    assert_eq!(status, 200);

    let wrong = format!("hm1_{}", "B".repeat(43));
    for i in 0..hm_core::auth::AUTH_FAIL_THRESHOLD {
        let (status, _, _) = get(&lan_app.base, "/api/v0/status", Some(&wrong)).await;
        assert_eq!(status, 401, "attempt {i} before threshold must be 401");
    }

    // Threshold reached: subsequent requests from the same LAN IP are throttled with 429 + Retry-After.
    let resp = reqwest::Client::new()
        .get(format!("{}/api/v0/status", lan_app.base))
        .header("Authorization", format!("Bearer {wrong}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 429);
    assert!(resp.headers().get("retry-after").is_some());

    // Loopback is never rate-limited, even after repeated unauthenticated/invalid attempts.
    let loopback_app = spawn(None, false).await;
    let _ = post(&loopback_app.base, "/api/v0/auth/provision", None).await;
    for _ in 0..(hm_core::auth::AUTH_FAIL_THRESHOLD + 5) {
        let (status, _, _) = get(&loopback_app.base, "/api/v0/status", None).await;
        assert_eq!(status, 200, "loopback must never be rate-limited");
    }
}

async fn spawn_with_peer(peer: &'static str) -> App {
    let dir = std::env::temp_dir().join(format!(
        "hm-auth-peer-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let state = AppState::with_overrides(
        store,
        dir.clone(),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let peer_addr: SocketAddr = peer.parse().unwrap();
    let app = router(state.clone()).layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(peer_addr));
            next.run(req).await
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    App {
        state,
        base: format!("http://127.0.0.1:{port}"),
        _dir: dir,
    }
}

#[tokio::test]
async fn option_1a_lan_gateway_exempt_allows_private_everyday_routes_and_gates_public_and_high_risk(
) {
    let lan_app = spawn_with_peer("192.168.1.50:54321").await;
    let (_, body) = post(&lan_app.base, "/api/v0/auth/provision", None).await;
    let token = body["token"].as_str().unwrap().to_string();

    // 1. Default fail-closed: core_id & lan_gateway_exempt=false advertised on /api/v0/auth;
    //    private LAN peer gets 401 on /api/v0/status before opt-in.
    let (code, _, auth_body) = get(&lan_app.base, "/api/v0/auth", None).await;
    assert_eq!(code, 200);
    let core_id = auth_body["core_id"].as_str().unwrap().to_string();
    assert!(core_id.starts_with("hmcore_"));
    assert_eq!(auth_body["lan_gateway_exempt"], false);
    assert!(auth_body.get("tokens").is_none());

    let (code, _, _) = get(&lan_app.base, "/api/v0/status", None).await;
    assert_eq!(code, 401, "fail-closed by default on private LAN");

    // 2. Toggling lan_gateway_exempt requires admin Bearer token.
    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/api/v0/auth/lan-exempt", lan_app.base))
        .json(&serde_json::json!({ "lan_gateway_exempt": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let resp = client
        .put(format!("{}/api/v0/auth/lan-exempt", lan_app.base))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "lan_gateway_exempt": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let toggled: Value = resp.json().await.unwrap();
    assert_eq!(toggled["lan_gateway_exempt"], true);
    assert_eq!(toggled["core_id"], core_id);

    // /api/v0/auth reflects lan_gateway_exempt=true immediately, and config_history records the change.
    let (code, _, auth_body2) = get(&lan_app.base, "/api/v0/auth", None).await;
    assert_eq!(code, 200);
    assert_eq!(auth_body2["lan_gateway_exempt"], true);
    assert_eq!(auth_body2["core_id"], core_id);

    let (code, _, hist_body) = get(&lan_app.base, "/api/v0/config/history", Some(&token)).await;
    assert_eq!(code, 200);
    let has_auth_entry = hist_body["history"].as_array().is_some_and(|arr| {
        arr.iter().any(|e| {
            e["scope"] == "auth"
                && e["summary"]
                    .as_str()
                    .is_some_and(|s| s.contains("lan_gateway_exempt") && s.contains("true"))
        })
    });
    assert!(
        has_auth_entry,
        "config_history must record lan_gateway_exempt toggle, got {hist_body}"
    );

    // 3. Everyday read/mode/chat/media routes succeed from RFC1918 192.168.1.50 without token.
    let (code, _, status_body) = get(&lan_app.base, "/api/v0/status", None).await;
    assert_eq!(code, 200);
    assert_eq!(status_body["core_id"], core_id);
    assert_eq!(status_body["auth"]["lan_gateway_exempt"], true);

    for path in [
        "/api/v0/mode",
        "/api/v0/devices",
        "/api/v0/fleet",
        "/api/v0/device-meta",
    ] {
        let (code, _, _) = get(&lan_app.base, path, None).await;
        assert_eq!(code, 200, "everyday route {path} should be LAN-exempt");
    }
    // Parameterized device conversation route also bypasses 401 (returns 404 only because
    // no session exists yet for dot2-d in the empty test DB).
    let (code, _, _) = get(&lan_app.base, "/api/v0/devices/dot2-d/conversation", None).await;
    assert_eq!(
        code, 404,
        "everyday route /api/v0/devices/dot2-d/conversation should bypass 401 auth gate"
    );

    // Presenting a WRONG token or non-Bearer Authorization header from private LAN still returns 401.
    let wrong = format!("hm1_{}", "Z".repeat(43));
    let (code, _, _) = get(&lan_app.base, "/api/v0/status", Some(&wrong)).await;
    assert_eq!(code, 401, "invalid presented bearer token must still fail");
    let non_bearer_resp = client
        .get(format!("{}/api/v0/status", lan_app.base))
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .unwrap();
    assert_eq!(
        non_bearer_resp.status().as_u16(),
        401,
        "non-Bearer Authorization header must NOT bypass auth via lan_gateway_exempt"
    );

    // 4. High-risk admin/security/OTA/vault/credential routes stay STRICTLY 401 even when exempt!
    for (method, path) in [
        ("PUT", "/api/v0/auth/lan-exempt"),
        ("POST", "/api/v0/auth/rotate"),
        ("GET", "/api/v0/release"),
        ("POST", "/api/v0/release/install"),
        ("GET", "/api/v0/firmware"),
        ("GET", "/api/v0/config/services"),
        ("GET", "/api/v0/config/services/master_brain"),
        ("GET", "/api/v0/config/export"),
        ("POST", "/api/v0/config/import"),
        ("POST", "/api/v0/system/restart"),
        ("POST", "/api/v0/system/factory-reset"),
        ("POST", "/api/v0/devices/dot2-d/wifi"),
        ("POST", "/api/v0/reflex/deploy"),
        ("PUT", "/api/v0/prompts"),
        ("DELETE", "/api/v0/devices/dot2-d/conversation"),
    ] {
        let req = match method {
            "GET" => client.get(format!("{}{path}", lan_app.base)),
            "POST" => client
                .post(format!("{}{path}", lan_app.base))
                .json(&serde_json::json!({})),
            "PUT" => client
                .put(format!("{}{path}", lan_app.base))
                .json(&serde_json::json!({})),
            "DELETE" => client
                .delete(format!("{}{path}", lan_app.base))
                .json(&serde_json::json!({ "expected_session_id": "", "include_archived": true })),
            _ => unreachable!(),
        };
        let resp = req.send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            401,
            "high-risk {method} {path} must stay strictly Bearer-gated even with lan_gateway_exempt=true"
        );
    }

    // 5. Tailscale CGNAT (100.64.0.0/10) is also private-LAN exempt for everyday routes,
    //    and strictly 401 on high-risk routes.
    let ts_app = spawn_with_peer("100.64.10.20:54321").await;
    let (_, ts_body) = post(&ts_app.base, "/api/v0/auth/provision", None).await;
    let ts_tok = ts_body["token"].as_str().unwrap().to_string();
    client
        .put(format!("{}/api/v0/auth/lan-exempt", ts_app.base))
        .header("Authorization", format!("Bearer {ts_tok}"))
        .json(&serde_json::json!({ "lan_gateway_exempt": true }))
        .send()
        .await
        .unwrap();
    let (code, _, _) = get(&ts_app.base, "/api/v0/status", None).await;
    assert_eq!(
        code, 200,
        "Tailscale CGNAT peer should be LAN-exempt on /status"
    );
    let (code, _, _) = get(&ts_app.base, "/api/v0/config/export", None).await;
    assert_eq!(
        code, 401,
        "Tailscale CGNAT peer must still be 401 on /config/export"
    );

    // 6. Public IP (203.0.113.9) is NEVER exempt even when lan_gateway_exempt=true!
    let pub_app = spawn_with_peer("203.0.113.9:54321").await;
    let pub_tok = pub_app.state.auth.provision("family").unwrap();
    client
        .put(format!("{}/api/v0/auth/lan-exempt", pub_app.base))
        .header("Authorization", format!("Bearer {pub_tok}"))
        .json(&serde_json::json!({ "lan_gateway_exempt": true }))
        .send()
        .await
        .unwrap();
    for path in ["/api/v0/status", "/api/v0/mode", "/api/v0/devices"] {
        let (code, _, _) = get(&pub_app.base, path, None).await;
        assert_eq!(
            code, 401,
            "public IP must NEVER be exempt on {path} even with lan_gateway_exempt=true"
        );
    }
}

#[tokio::test]
async fn unpaired_core_restricts_public_peers_and_high_risk_lan_endpoints() {
    let client = reqwest::Client::new();

    // 1. Loopback unpaired stays fully open for local dev, smoke tests, and unit tests.
    let lb_app = spawn(None, false).await;
    let (code, _, body) = get(&lb_app.base, "/api/v0/status", None).await;
    assert_eq!(code, 200);
    assert_eq!(body["auth"]["configured"], false);
    let resp = client
        .post(format!("{}/api/v0/config/export?secrets=1", lb_app.base))
        .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "loopback unpaired export with confirm header must keep working"
    );
    let (code, prov) = post(&lb_app.base, "/api/v0/auth/provision", None).await;
    assert_eq!(code, 200);
    assert!(prov["token"].as_str().unwrap().starts_with("hm1_"));

    // 2. Non-private / public IPs (IPv4 TEST-NET & global IPv6) on an unpaired Core
    //    must be rejected outright with 403 Forbidden on ALL /api/* routes,
    //    including /api/v0/auth/provision, /api/v0/status, and /api/v0/config/export.
    for public_peer in ["203.0.113.9:54321", "[2001:db8::1]:54321"] {
        let pub_app = spawn_with_peer(public_peer).await;
        let (code, _) = post(&pub_app.base, "/api/v0/auth/provision", None).await;
        assert_eq!(
            code, 403,
            "public peer {public_peer} must get 403 on /api/v0/auth/provision while unpaired"
        );
        assert!(
            !pub_app.state.auth.configured(),
            "rejected public provision must leave Core unpaired"
        );
        for path in ["/api/v0/auth", "/api/v0/status", "/api/v0/config/export"] {
            let (code, _, _) = get(&pub_app.base, path, None).await;
            assert_eq!(
                code, 403,
                "public peer {public_peer} must get 403 on GET {path} while unpaired"
            );
        }
        let resp = client
            .post(format!("{}/api/v0/config/export?secrets=1", pub_app.base))
            .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            403,
            "public peer {public_peer} must get 403 on POST /api/v0/config/export?secrets=1"
        );
        let resp = client
            .post(format!("{}/api/v0/system/factory-reset", pub_app.base))
            .json(&serde_json::json!({ "confirm": "FACTORY_RESET" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            403,
            "public peer {public_peer} must get 403 on POST /api/v0/system/factory-reset"
        );
    }

    // 3. Private LAN (RFC1918), Tailscale CGNAT (100.64.0.0/10), and IPv6 ULA (fc00::/7)
    //    on an unpaired Core:
    //    - Read-only /api/v0/auth and /api/v0/status succeed.
    //    - High-risk irreversible / secret-exfiltration routes are blocked with 401.
    //    - One-shot POST /api/v0/auth/provision succeeds so the household owner can pair.
    for lan_peer in [
        "192.168.1.50:54321",
        "100.64.10.20:54321",
        "[fd12:3456::1]:54321",
    ] {
        let lan_app = spawn_with_peer(lan_peer).await;
        let (code, _, auth_body) = get(&lan_app.base, "/api/v0/auth", None).await;
        assert_eq!(code, 200, "private peer {lan_peer} can read /api/v0/auth");
        assert_eq!(auth_body["configured"], false);
        let (code, _, _) = get(&lan_app.base, "/api/v0/status", None).await;
        assert_eq!(code, 200, "private peer {lan_peer} can read /api/v0/status");

        let export_resp = client
            .post(format!("{}/api/v0/config/export?secrets=1", lan_app.base))
            .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
            .send()
            .await
            .unwrap();
        assert_eq!(
            export_resp.status().as_u16(),
            401,
            "unpaired LAN peer {lan_peer} must be blocked (401) on POST /api/v0/config/export?secrets=1"
        );
        let export_body: Value = export_resp.json().await.unwrap();
        assert!(
            export_body["detail"]
                .as_str()
                .is_some_and(|d| d.contains("pair a household token")),
            "expected pairing detail, got {export_body}"
        );

        let reset_resp = client
            .post(format!("{}/api/v0/system/factory-reset", lan_app.base))
            .json(&serde_json::json!({ "confirm": "FACTORY_RESET" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            reset_resp.status().as_u16(),
            401,
            "unpaired LAN peer {lan_peer} must be blocked (401) on POST /api/v0/system/factory-reset"
        );

        for (method, path) in [
            ("GET", "/api/v0/config/export"),
            ("POST", "/api/v0/config/import"),
            ("POST", "/api/v0/devices/dot2-d/mic/capture"),
            ("GET", "/api/v0/devices/dot2-d/mic/capture.wav"),
            ("POST", "/api/v0/release/install"),
            ("GET", "/api/v0/release"),
            ("POST", "/api/v0/firmware/core"),
            ("GET", "/api/v0/firmware"),
        ] {
            let req = match method {
                "GET" => client.get(format!("{}{path}", lan_app.base)),
                "POST" => client
                    .post(format!("{}{path}", lan_app.base))
                    .json(&serde_json::json!({})),
                _ => unreachable!(),
            };
            let resp = req.send().await.unwrap();
            assert_eq!(
                resp.status().as_u16(),
                401,
                "unpaired LAN peer {lan_peer} must get 401 on {method} {path}"
            );
        }

        // Pairing via one-shot /api/v0/auth/provision succeeds from private LAN / Tailscale / ULA.
        let (code, body) = post(&lan_app.base, "/api/v0/auth/provision", None).await;
        assert_eq!(
            code, 200,
            "private peer {lan_peer} must be able to pair via POST /api/v0/auth/provision"
        );
        let token = body["token"].as_str().unwrap();
        assert!(token.starts_with("hm1_"));

        // Once paired, presenting the Bearer token unlocks /api/v0/config/export?secrets=1.
        let paired_export = client
            .post(format!("{}/api/v0/config/export?secrets=1", lan_app.base))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-HM-Confirm-Secrets", "EXPORT_SECRETS")
            .send()
            .await
            .unwrap();
        assert_eq!(
            paired_export.status().as_u16(),
            200,
            "paired LAN peer {lan_peer} with Bearer token must be allowed to export secrets"
        );
    }
}
