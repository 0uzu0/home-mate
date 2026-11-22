//! M12a-1 (v0.26): credential vault, node-to-node config push.
//!
//! These are the roadmap §0.1.1 acceptance criteria driven through the real
//! HTTP surface, with a REAL peer: a second `AppState` on its own port with
//! its own store, its own wake word and its own reflex table. Nothing here
//! stubs the import path — the sender posts to the peer's
//! `/api/v0/config/import` exactly as it does on a LAN.
//!
//! Covered:
//! 3. `POST /fleet/ping` returns the peer's `/status.build.id`.
//! 4. `include_secrets:false` moves provider/model/base_url and leaves the
//!    peer's stored key alone.
//! 5. `include_secrets:true` installs the key AND the peer's history names
//!    `actor=fleet:<sender>`.
//! 6. `dry_run:true` returns a plan and changes nothing on either side.
//! 7. A push to one peer leaves the other one untouched.
//! 8. The peer's `wake_words` and its wake-intent reflex word are byte-for-byte
//!    unchanged, even when the sender's differ.

use hm_core::api::{router, AppState};
use hm_core::store::Store;
use hm_core::tool::mocks::MockInject;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hm-fleet-{tag}-{}-{}",
        std::process::id(),
        hm_core::id::now_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One node: real listener, real store, its own data dir.
struct Node {
    base: String,
    state: Arc<AppState>,
}

async fn spawn_node(tag: &str) -> Node {
    let data_dir = temp_dir(tag);
    let store = Arc::new(Store::open(&data_dir.join("home-mate.db")).unwrap());
    let state = AppState::new(store, data_dir, MockInject::default(), None);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Node {
        base: format!("http://{addr}"),
        state,
    }
}

#[tokio::test]
async fn d1_5_fleet_push_carries_both_independent_ack_phrases() {
    let sender = spawn_node("d1-5-sender").await;
    let peer = spawn_node("d1-5-peer").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;
    set_self_id(&sender, "d1-5-source").await;
    add_peer(&sender, &peer, "d1-5-peer").await;
    let mut settings = serde_json::to_value(sender.state.voice.settings_snapshot()).unwrap();
    settings["retreat_ack"] = json!("遵命");
    settings["window_close_ack"] = json!("回头见啦");
    sender
        .state
        .voice
        .restore_settings(serde_json::from_value(settings).unwrap(), "test")
        .unwrap();
    let result = push(&sender, &["d1-5-peer"], false, false).await;
    assert!(
        result["results"][0]["ok"].as_bool().unwrap_or(false),
        "{result}"
    );
    let mode = peer.state.voice.mode_snapshot();
    assert_eq!(mode["retreat_ack"], "遵命");
    assert_eq!(mode["window_close_ack"], "回头见啦");
    assert_eq!(mode["wake_words"], json!(["你好建国"]));
    let export: Value = client()
        .get(format!("{}/api/v0/config/export", peer.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["mode"]["window_close_ack"], "回头见啦");
    assert!(peer
        .state
        .config
        .history(100)
        .unwrap()
        .iter()
        .any(|row| row["actor"] == "fleet:d1-5-source"
            && row["payload"]["window_close_ack"] == "回头见啦"));
}

/// Give a node its own household identity: a wake word and a reflex table that
/// includes it. Every push test asserts these survive. Both writes go through
/// the same save path a person uses (the `mode` endpoint needs an online
/// device, which a headless test node does not have).
async fn give_it_a_wake_word(node: &Node, wake: &str, tokens: &str) {
    node.state
        .voice
        .set_mode(
            "dot2-local",
            None,
            None,
            Some(vec![wake.to_string()]),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test",
        )
        .expect("wake word saved");
    let reflex = hm_core::config::ReflexConfig {
        words: vec![
            hm_core::config::ReflexWord {
                text: wake.into(),
                tokens: tokens.into(),
                intent: hm_core::config::ReflexIntent::Wake,
                enabled: true,
            },
            hm_core::config::ReflexWord {
                text: "暂停播放".into(),
                tokens: "z àn t íng b ō f àng".into(),
                intent: hm_core::config::ReflexIntent::Pause,
                enabled: true,
            },
        ],
    };
    node.state.config.save_reflex(reflex, "test").unwrap();
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn add_peer(sender: &Node, peer: &Node, id: &str) {
    let resp = client()
        .put(format!("{}/api/v0/fleet/{id}", sender.base))
        .json(&json!({ "base_url": peer.base, "token": "hm1_test_token_not_checked_on_loopback" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
}

async fn set_self_id(node: &Node, id: &str) {
    let resp = client()
        .put(format!("{}/api/v0/fleet", node.base))
        .json(&json!({ "self_id": id }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
}

async fn push(sender: &Node, targets: &[&str], secrets: bool, dry_run: bool) -> Value {
    let resp = client()
        .post(format!("{}/api/v0/fleet/push", sender.base))
        .json(&json!({ "targets": targets, "include_secrets": secrets, "dry_run": dry_run }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "push failed: {body}");
    body
}

/// Everything an import could change, as one comparable string. Used to prove
/// a dry run wrote nothing.
fn config_fingerprint(state: &AppState) -> String {
    let store = &state.store;
    let keys = [
        "services",
        "music",
        "weather",
        "home",
        "news",
        "search",
        "openapi",
        "reflex",
        "audio_eq",
        "device_meta",
        "voice",
    ];
    let mut parts = Vec::new();
    for k in keys {
        parts.push(format!("{k}={:?}", store.config_get(k).unwrap()));
    }
    let mut secrets: Vec<String> = store
        .secret_names()
        .unwrap()
        .into_iter()
        .map(|n| format!("{n}={:?}", store.secret_raw(&n).unwrap()))
        .collect();
    secrets.sort();
    parts.push(format!("secrets={secrets:?}"));
    parts.push(format!("history={:?}", state.config.history(500).unwrap()));
    parts.push(format!(
        "mode={}",
        serde_json::to_string(&state.voice.settings_snapshot()).unwrap()
    ));
    parts.join("\n")
}

#[tokio::test]
async fn ping_returns_the_peer_build_id() {
    let sender = spawn_node("ping-sender").await;
    let peer = spawn_node("ping-peer").await;
    add_peer(&sender, &peer, "dot2-e").await;

    let resp: Value = client()
        .post(format!("{}/api/v0/fleet/ping", sender.base))
        .json(&json!({ "id": "dot2-e" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["ok"], json!(1), "{resp}");
    let first = &resp["results"][0];
    assert_eq!(first["ok"], json!(true), "{resp}");
    // Acceptance 3: the peer's build id, not ours.
    assert_eq!(
        first["build"],
        json!(hm_core::BUILD_ID),
        "ping must report the peer's /status.build.id: {resp}"
    );
    // And the sender recorded the contact.
    let fleet: Value = client()
        .get(format!("{}/api/v0/fleet", sender.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        fleet["nodes"][0]["last_ok_ms"].as_u64().unwrap() > 0,
        "{fleet}"
    );
    assert_eq!(fleet["nodes"][0]["last_error"], json!(""));
    assert_eq!(fleet["nodes"][0]["token_set"], json!(true));
}

#[tokio::test]
async fn a_ping_without_a_token_says_so_instead_of_failing_obscurely() {
    let sender = spawn_node("ping-notoken-sender").await;
    let peer = spawn_node("ping-notoken-peer").await;
    // Added with a URL only: no credential.
    let resp = client()
        .put(format!("{}/api/v0/fleet/dot2-e", sender.base))
        .json(&json!({ "base_url": peer.base }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let out: Value = client()
        .post(format!("{}/api/v0/fleet/ping", sender.base))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(out["results"][0]["ok"], json!(false), "{out}");
    assert!(
        out["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("no token"),
        "{out}"
    );
}

#[tokio::test]
async fn push_without_secrets_moves_the_config_and_keeps_the_peers_key() {
    let sender = spawn_node("nosecret-sender").await;
    let peer = spawn_node("nosecret-peer").await;
    add_peer(&sender, &peer, "dot2-e").await;
    set_self_id(&sender, "dot2-d").await;

    // The sender is configured; the peer has its own key and older settings.
    let mut services = sender.state.config.load().unwrap();
    services.llm.provider = "deepseek".into();
    services.llm.base_url = "https://api.deepseek.com".into();
    services.llm.model = "deepseek-flash".into();
    sender
        .state
        .config
        .save_service(
            "llm",
            services.llm.clone(),
            Some("sk-sender-key"),
            "console",
        )
        .unwrap();
    let mut peer_services = peer.state.config.load().unwrap();
    peer_services.llm.provider = "omlx".into();
    peer_services.llm.base_url = "http://192.168.1.9:8000".into();
    peer_services.llm.model = "local-model".into();
    peer.state
        .config
        .save_service(
            "llm",
            peer_services.llm.clone(),
            Some("sk-peer-own-key"),
            "console",
        )
        .unwrap();

    let out = push(&sender, &["dot2-e"], false, false).await;
    assert_eq!(out["ok"], json!(1), "{out}");
    assert_eq!(out["actor"], json!("fleet:dot2-d"), "{out}");

    // Acceptance 4: the configuration moved.
    let peer_llm = peer.state.config.load().unwrap().llm;
    assert_eq!(peer_llm.provider, "deepseek");
    assert_eq!(peer_llm.base_url, "https://api.deepseek.com");
    assert_eq!(peer_llm.model, "deepseek-flash");
    // ... and the credential did not.
    assert_eq!(
        peer.state.config.resolve_key("llm").unwrap().as_deref(),
        Some("sk-peer-own-key"),
        "a push without secrets must not touch the target's stored key"
    );
    assert!(peer.state.config.status("llm").unwrap().api_key_set);
}

#[tokio::test]
async fn push_with_secrets_installs_the_key_and_history_names_the_sender() {
    let sender = spawn_node("secret-sender").await;
    let peer = spawn_node("secret-peer").await;
    add_peer(&sender, &peer, "dot2-e").await;
    set_self_id(&sender, "dot2-d").await;
    sender
        .state
        .config
        .save_service(
            "llm",
            sender.state.config.load().unwrap().llm,
            Some("sk-installed-by-push"),
            "console",
        )
        .unwrap();

    let out = push(&sender, &["dot2-e"], true, false).await;
    assert_eq!(out["ok"], json!(1), "{out}");
    // The peer's import echoes who it thought the sender was.
    assert_eq!(out["results"][0]["actor"], json!("fleet:dot2-d"), "{out}");

    // Acceptance 5: the key is now the sender's, sealed at rest on the peer.
    assert_eq!(
        peer.state.config.resolve_key("llm").unwrap().as_deref(),
        Some("sk-installed-by-push")
    );
    let raw = peer.state.store.secret_raw("llm.api_key").unwrap().unwrap();
    assert!(raw.starts_with("enc:v1:"), "peer stored it sealed: {raw}");

    // ... and the peer's history says so.
    let history = peer.state.config.history(50).unwrap();
    let fleet_rows: Vec<&Value> = history
        .iter()
        .filter(|r| r["actor"] == json!("fleet:dot2-d"))
        .collect();
    assert!(
        !fleet_rows.is_empty(),
        "no actor=fleet:dot2-d row: {history:?}"
    );
    assert!(
        fleet_rows.iter().any(|r| r["scope"] == json!("llm")),
        "the llm save must be attributed to the push: {fleet_rows:?}"
    );
    // No secret value ever reaches the history payload.
    for row in &fleet_rows {
        assert!(
            !row.to_string().contains("sk-installed-by-push"),
            "history leaked a key: {row}"
        );
    }
}

#[tokio::test]
async fn a_dry_run_returns_a_plan_and_writes_nothing() {
    let sender = spawn_node("dry-sender").await;
    let peer = spawn_node("dry-peer").await;
    add_peer(&sender, &peer, "dot2-e").await;
    set_self_id(&sender, "dot2-d").await;

    let peer_before = config_fingerprint(&peer.state);
    let mtime_before = std::fs::metadata(peer.state.data_dir.join("home-mate.db"))
        .ok()
        .and_then(|m| m.modified().ok());
    let sender_fleet_before = sender.state.store.config_get("fleet").unwrap();

    let out = push(&sender, &["dot2-e"], true, true).await;
    assert_eq!(out["state"], json!("planned"), "{out}");
    assert_eq!(out["dry_run"], json!(true), "{out}");
    let first = &out["results"][0];
    assert_eq!(first["ok"], json!(true), "{out}");
    assert_eq!(first["dry_run"], json!(true), "{out}");
    let applied = first["applied"].as_array().unwrap();
    assert!(
        applied.iter().any(|b| b == &json!("services.llm")),
        "the plan names the blocks: {first}"
    );

    // Acceptance 6: nothing changed on the target, and the sender recorded no
    // contact either (a dry run is not a probe).
    assert_eq!(
        config_fingerprint(&peer.state),
        peer_before,
        "dry_run wrote to the peer"
    );
    let mtime_after = std::fs::metadata(peer.state.data_dir.join("home-mate.db"))
        .ok()
        .and_then(|m| m.modified().ok());
    assert_eq!(mtime_before, mtime_after, "dry_run touched the peer DB");
    assert_eq!(
        sender.state.store.config_get("fleet").unwrap(),
        sender_fleet_before,
        "dry_run updated the sender's peer bookkeeping"
    );
}

#[tokio::test]
async fn only_the_named_target_is_written() {
    let sender = spawn_node("scope-sender").await;
    let peer_e = spawn_node("scope-e").await;
    let peer_c = spawn_node("scope-c").await;
    add_peer(&sender, &peer_e, "dot2-e").await;
    add_peer(&sender, &peer_c, "dot2-c").await;
    set_self_id(&sender, "dot2-d").await;

    // Give the sender a distinguishable value to move.
    let mut services = sender.state.config.load().unwrap();
    services.tts.model = "seed-tts-2.0-pushed".into();
    sender
        .state
        .config
        .save_service("tts", services.tts, None, "console")
        .unwrap();

    let c_before = config_fingerprint(&peer_c.state);
    let out = push(&sender, &["dot2-e"], false, false).await;
    assert_eq!(out["ok"], json!(1), "{out}");
    assert_eq!(
        peer_e.state.config.load().unwrap().tts.model,
        "seed-tts-2.0-pushed"
    );
    // Acceptance 7: the untouched node is untouched.
    assert_eq!(
        config_fingerprint(&peer_c.state),
        c_before,
        "a push to dot2-e wrote to dot2-c"
    );
}

#[tokio::test]
async fn a_push_never_carries_the_wake_word() {
    // Acceptance 8, and the household rule behind it: every speaker keeps its
    // own wake word (user decision 2026-11-14).
    let sender = spawn_node("wake-sender").await;
    let peer = spawn_node("wake-peer").await;
    add_peer(&sender, &peer, "dot2-e").await;
    set_self_id(&sender, "dot2-d").await;

    give_it_a_wake_word(&sender, "你好高达", "n ǐ h ǎo g āo d á").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;
    // Sender-only command words that SHOULD travel.
    let mut reflex = sender.state.config.reflex().unwrap();
    reflex.words.push(hm_core::config::ReflexWord {
        text: "继续播放".into(),
        tokens: "j ì x ù b ō f àng".into(),
        intent: hm_core::config::ReflexIntent::Continue,
        enabled: true,
    });
    sender.state.config.save_reflex(reflex, "console").unwrap();

    let wake_before = peer.state.voice.settings_snapshot().wake_words;
    let peer_wake_reflex: Vec<_> = peer
        .state
        .config
        .reflex()
        .unwrap()
        .words
        .into_iter()
        .filter(|w| w.intent == hm_core::config::ReflexIntent::Wake)
        .collect();

    let out = push(&sender, &["dot2-e"], true, false).await;
    assert_eq!(out["ok"], json!(1), "{out}");

    // The wake words are byte-for-byte the peer's own.
    assert_eq!(wake_before, vec!["你好建国".to_string()]);
    assert_eq!(
        peer.state.voice.settings_snapshot().wake_words,
        wake_before,
        "the push renamed the peer's wake word"
    );
    let after = peer.state.config.reflex().unwrap();
    let wake_after: Vec<_> = after
        .words
        .iter()
        .filter(|w| w.intent == hm_core::config::ReflexIntent::Wake)
        .cloned()
        .collect();
    assert_eq!(
        wake_after, peer_wake_reflex,
        "the peer's wake reflex word changed"
    );
    assert_eq!(wake_after[0].text, "你好建国");
    assert_eq!(wake_after[0].tokens, "n ǐ h ǎo j iàn g uó");

    // The rest of the vocabulary DID travel (one meaning per word, v0.25).
    let texts: Vec<String> = after.words.iter().map(|w| w.text.clone()).collect();
    assert!(texts.contains(&"继续播放".to_string()), "{texts:?}");
    // ... and the sender's wake word never appears.
    assert!(!texts.contains(&"你好高达".to_string()), "{texts:?}");
}

#[tokio::test]
async fn the_receiver_ignores_wake_words_in_a_raw_import_from_a_peer() {
    // Belt and braces: even a sender that (wrongly) includes wake_words must
    // not be able to rename the receiver's wake word.
    let peer = spawn_node("raw-peer").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j j iàn g uó").await;
    let before = peer.state.voice.settings_snapshot().wake_words;

    let mut mode = serde_json::to_value(peer.state.voice.settings_snapshot()).unwrap();
    mode["wake_words"] = json!(["你好高达"]);
    let resp = client()
        .post(format!("{}/api/v0/config/import", peer.base))
        .header("X-Home-Mate-Source", "dot2-d")
        .json(&json!({ "mode": mode }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "{body}");
    assert_eq!(body["actor"], json!("fleet:dot2-d"), "{body}");
    assert!(
        body["warnings"].to_string().contains("wake_words"),
        "the refusal must be visible: {body}"
    );
    assert_eq!(
        peer.state.voice.settings_snapshot().wake_words,
        before,
        "a fleet import renamed the wake word"
    );
}

#[tokio::test]
async fn a_bad_source_header_is_refused_not_recorded() {
    let peer = spawn_node("badheader-peer").await;
    let resp = client()
        .post(format!("{}/api/v0/config/import", peer.base))
        .header("X-Home-Mate-Source", "dot2-d; rm -rf /")
        .json(&json!({ "format": "home-mate-config", "version": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("x-home-mate-source"),
        "{body}"
    );
}

#[tokio::test]
async fn export_carries_the_reflex_table_and_the_tone() {
    // Acceptance: the export used to lose `reflex` and `audio_eq`, so a
    // restored node came back mute to its command words and with the default
    // curve.
    let node = spawn_node("export-node").await;
    give_it_a_wake_word(&node, "你好高达", "n ǐ h ǎo g āo d á").await;
    node.state
        .config
        .save_audio_eq(
            hm_core::config::AudioEqConfig {
                curve: "custom".into(),
                bands: vec![hm_core::config::EqBandCfg {
                    freq: 120.0,
                    gain_db: -3.5,
                    q: 0.8,
                }],
            },
            "console",
        )
        .unwrap();
    let doc: Value = client()
        .get(format!("{}/api/v0/config/export", node.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        doc["reflex"]["words"][0]["text"],
        json!("你好高达"),
        "{doc}"
    );
    assert_eq!(doc["audio_eq"]["curve"], json!("custom"), "{doc}");
    // A plain export still carries no secret values.
    assert!(doc.get("secrets").is_none(), "export leaked secrets: {doc}");
}

#[tokio::test]
async fn fleet_nodes_roundtrip_and_remove() {
    let node = spawn_node("nodes-node").await;
    let peer = spawn_node("nodes-peer").await;
    add_peer(&node, &peer, "dot2-e").await;
    let fleet: Value = client()
        .get(format!("{}/api/v0/fleet", node.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fleet["nodes"].as_array().unwrap().len(), 1, "{fleet}");
    assert_eq!(fleet["nodes"][0]["id"], json!("dot2-e"));
    assert_eq!(fleet["nodes"][0]["token_set"], json!(true));
    assert!(
        !fleet
            .to_string()
            .contains("hm1_test_token_not_checked_on_loopback"),
        "the fleet listing leaked the token: {fleet}"
    );
    // The token is sealed at rest, like every other credential.
    let raw = node
        .state
        .store
        .secret_raw("fleet.dot2-e.token")
        .unwrap()
        .unwrap();
    assert!(raw.starts_with("enc:v1:"), "{raw}");

    let resp = client()
        .delete(format!("{}/api/v0/fleet/dot2-e", node.base))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let fleet: Value = client()
        .get(format!("{}/api/v0/fleet", node.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(fleet["nodes"].as_array().unwrap().is_empty(), "{fleet}");
    assert!(node
        .state
        .store
        .secret_raw("fleet.dot2-e.token")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_pushed_reflex_table_is_saved_and_a_missing_deploy_is_reported() {
    // A vocabulary the spotter never loads is a half-done push. This test node
    // has no speaker online, so the import must still succeed AND say that
    // nothing loaded the table instead of claiming it is live.
    let peer = spawn_node("reflex-peer").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;
    let resp = client()
        .post(format!("{}/api/v0/config/import", peer.base))
        .header("X-Home-Mate-Source", "dot2-d")
        .json(&json!({
            "format": "home-mate-config",
            "version": 1,
            "reflex": { "words": [
                { "text": "你好高达", "tokens": "n ǐ h ǎo g āo d á", "intent": "wake", "enabled": true },
                { "text": "下一首", "tokens": "x ià y ì sh ǒu", "intent": "next", "enabled": true },
            ]},
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "{body}");
    let applied = body["applied"].as_array().unwrap();
    assert!(applied.iter().any(|a| a == &json!("reflex")), "{body}");
    assert!(
        body["warnings"].to_string().contains("no audio device"),
        "the missing deploy must be visible: {body}"
    );
    // The table did change (the non-wake word arrived) and the wake word stayed.
    let reflex = peer.state.config.reflex().unwrap();
    let texts: Vec<&str> = reflex.words.iter().map(|w| w.text.as_str()).collect();
    assert_eq!(texts, vec!["你好建国", "下一首"], "{texts:?}");
}

/// 2026-11-15 (user, first end-to-end provision): a pushed/restored config can
/// carry an ack phrase this node has never spoken — `retreat_ack` plays from a
/// pre-synthesized WAV, and only `set_mode` used to rebuild that set. The close
/// announcement was therefore configured everywhere and audible nowhere.
/// An import must leave the node able to speak its own boundary phrase.
#[tokio::test]
async fn an_imported_retreat_ack_gets_synthesized_on_the_receiver() {
    use hm_core::speech::MockTts;
    let data_dir = temp_dir("ack-peer");
    let store = Arc::new(Store::open(&data_dir.join("home-mate.db")).unwrap());
    let state = AppState::with_overrides(
        store,
        data_dir.clone(),
        MockInject::default(),
        None,
        Some(Arc::new(MockTts::default())),
        None,
        None,
    );
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");

    let ack_dir = data_dir.join("assets").join("wake-acks");
    assert!(
        !ack_dir.join("manifest.json").exists(),
        "nothing synthesized yet"
    );

    let mut mode = serde_json::to_value(state.voice.settings_snapshot()).unwrap();
    mode["retreat_ack"] = json!("拜拜啦");
    let resp = client()
        .post(format!("{base}/api/v0/config/import"))
        .header("X-Home-Mate-Source", "dot2-d")
        .json(&json!({ "format": "home-mate-config", "version": 1, "mode": mode }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "{body}");
    assert_eq!(state.voice.settings_snapshot().retreat_ack, "拜拜啦");

    // Synthesis runs in the background: wait for the manifest to name it.
    let mut found = None;
    for _ in 0..40 {
        if let Ok(raw) = std::fs::read_to_string(ack_dir.join("manifest.json")) {
            let m: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            if let Some(file) = m["files"].get("拜拜啦").and_then(|v| v.as_str()) {
                found = Some(ack_dir.join(file));
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let wav = found.expect("the imported retreat_ack must be synthesized, not just stored");
    let bytes = std::fs::read(&wav).unwrap();
    assert_eq!(&bytes[..4], b"RIFF", "the ack asset must be a real WAV");
    assert!(
        bytes.len() > 44,
        "an empty WAV is not an ack: {} bytes",
        bytes.len()
    );
}

/// Every per-target result has the same shape, including the early failures.
/// A console that renders `applied.length` unmounted its whole card when a peer
/// had no token (found in the browser on 2026-11-15) — the field must exist.
#[tokio::test]
async fn a_peer_without_a_token_still_reports_applied_and_warnings() {
    let sender = spawn_node("shape-sender").await;
    let peer = spawn_node("shape-peer").await;
    // Registered with a URL only: no credential.
    client()
        .put(format!("{}/api/v0/fleet/dot2-e", sender.base))
        .json(&json!({ "base_url": peer.base }))
        .send()
        .await
        .unwrap();

    let resp: Value = client()
        .post(format!("{}/api/v0/fleet/push", sender.base))
        .json(&json!({ "targets": ["dot2-e"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = &resp["results"][0];
    assert_eq!(r["ok"], json!(false), "{resp}");
    assert!(r["applied"].is_array(), "applied must be an array: {resp}");
    assert!(
        r["warnings"].is_array(),
        "warnings must be an array: {resp}"
    );
    assert!(r["error"].as_str().unwrap().contains("no token"), "{resp}");

    // Same for a peer that was never configured at all.
    let resp: Value = client()
        .post(format!("{}/api/v0/fleet/push", sender.base))
        .json(&json!({ "targets": ["dot2-ghost"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = &resp["results"][0];
    assert!(
        r["applied"].is_array() && r["warnings"].is_array(),
        "{resp}"
    );
}

#[tokio::test]
async fn d3_fleet_push_carries_persona_override_and_zero_followups() {
    let sender = spawn_node("d3-prompts-sender").await;
    let peer = spawn_node("d3-prompts-peer").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;
    set_self_id(&sender, "d3-source").await;
    add_peer(&sender, &peer, "d3-peer").await;
    let mut settings = serde_json::to_value(sender.state.voice.settings_snapshot()).unwrap();
    settings["persona_prompt"] = json!("叫我老板，回答简洁。");
    settings["system_prompt_override"] = json!("测试系统骨架");
    settings["followup_credits"] = json!(0);
    sender
        .state
        .voice
        .restore_settings(serde_json::from_value(settings).unwrap(), "test")
        .unwrap();
    let result = push(&sender, &["d3-peer"], false, false).await;
    assert!(
        result["results"][0]["ok"].as_bool().unwrap_or(false),
        "{result}"
    );
    let mode = peer.state.voice.mode_snapshot();
    assert_eq!(mode["persona_prompt"], "叫我老板，回答简洁。");
    assert_eq!(mode["system_prompt_override"], "测试系统骨架");
    assert_eq!(mode["followup_credits"], 0);
    assert_eq!(mode["wake_words"], json!(["你好建国"]));
    let export: Value = client()
        .get(format!("{}/api/v0/config/export", peer.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["mode"]["persona_prompt"], "叫我老板，回答简洁。");
    assert_eq!(export["mode"]["system_prompt_override"], "测试系统骨架");
    assert_eq!(export["mode"]["followup_credits"], 0);
    assert!(peer
        .state
        .config
        .history(100)
        .unwrap()
        .iter()
        .any(|row| row["actor"] == "fleet:d3-source"
            && row["payload"]["system_prompt_override"] == "测试系统骨架"));
}

#[tokio::test]
async fn device_meta_room_push_updates_peer_room_without_touching_wake_words() {
    let sender = spawn_node("meta-room-sender").await;
    let peer = spawn_node("meta-room-peer").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;
    set_self_id(&sender, "dot2-a").await;
    add_peer(&sender, &peer, "dot2-b").await;

    sender
        .state
        .config
        .save_device_meta("dot2-b", "小达儿童房", "儿童房", "test")
        .unwrap();

    let result = push(&sender, &["dot2-b"], false, false).await;
    assert!(
        result["results"][0]["ok"].as_bool().unwrap_or(false),
        "{result}"
    );

    let peer_meta = peer.state.config.device_meta().unwrap();
    let b = peer_meta.get("dot2-b").expect("dot2-b meta pushed to peer");
    assert_eq!(b.room, "儿童房");
    assert_eq!(b.name, "小达儿童房");

    let mode = peer.state.voice.mode_snapshot();
    assert_eq!(
        mode["wake_words"],
        json!(["你好建国"]),
        "pushing device_meta room must leave the peer's own wake words untouched"
    );
}

#[tokio::test]
async fn fleet_push_never_copies_unit_acoustic_profiles() {
    let sender = spawn_node("acoustic-sender").await;
    let peer = spawn_node("acoustic-peer").await;
    set_self_id(&sender, "dot2-a").await;
    add_peer(&sender, &peer, "dot2-b").await;

    sender
        .state
        .config
        .save_unit_acoustic_profile(
            "dot2-a",
            hm_core::config::UnitAcousticProfile {
                volume_cap: 0.72,
                mix_gain: 1.15,
                vad_threshold: 0.42,
                ..Default::default()
            },
            "test",
        )
        .unwrap();
    sender
        .state
        .config
        .save_unit_acoustic_profile(
            "dot2-b",
            hm_core::config::UnitAcousticProfile {
                volume_cap: 0.55,
                mix_gain: 0.80,
                vad_threshold: 0.70,
                ..Default::default()
            },
            "test",
        )
        .unwrap();

    peer.state
        .config
        .save_unit_acoustic_profile(
            "dot2-b",
            hm_core::config::UnitAcousticProfile {
                volume_cap: 0.90,
                mix_gain: 1.05,
                vad_threshold: 0.48,
                ..Default::default()
            },
            "test",
        )
        .unwrap();

    let result = push(&sender, &["dot2-b"], false, false).await;
    assert!(
        result["results"][0]["ok"].as_bool().unwrap_or(false),
        "{result}"
    );

    let peer_b = peer.state.config.unit_acoustic_profile("dot2-b").unwrap();
    assert_eq!(peer_b.volume_cap, 0.90);
    assert!((peer_b.mix_gain - 1.05).abs() < 1e-5);
    assert!((peer_b.vad_threshold - 0.48).abs() < 1e-5);
    assert!(
        !peer
            .state
            .config
            .unit_acoustic_profiles()
            .unwrap()
            .contains_key("dot2-a"),
        "sender's unit_acoustic_profiles must never travel to peer"
    );
}

#[tokio::test]
async fn fleet_push_preserves_all_node_local_mode_fields_and_survives_reopen() {
    use hm_core::voice::{VadSettings, VoiceSettings};

    let sender = spawn_node("local-fields-sender").await;
    let peer_dir = temp_dir("local-fields-peer");
    let peer_db = peer_dir.join("home-mate.db");
    let peer_store = Arc::new(Store::open(&peer_db).unwrap());
    let peer_state = AppState::new(
        peer_store.clone(),
        peer_dir.clone(),
        MockInject::default(),
        None,
    );
    let peer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_base = format!("http://{}", peer_listener.local_addr().unwrap());
    let peer_srv = tokio::spawn({
        let app = router(peer_state.clone());
        async move { axum::serve(peer_listener, app).await.unwrap() }
    });
    let peer = Node {
        base: peer_base,
        state: peer_state,
    };

    give_it_a_wake_word(&sender, "你好小智", "ni hao xiao zhi").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;

    let mut s_cfg: VoiceSettings = sender.state.voice.settings_snapshot();
    s_cfg.persona_prompt = "家庭共享性格".into();
    s_cfg.follow_window_ms = 8500;
    s_cfg.context_window_ms = 210_000;
    s_cfg.kws_threshold = 0.15;
    s_cfg.kws_score = 4.5;
    s_cfg.media_trim_db = -9.0;
    s_cfg.tail_blind_ms = 900;
    s_cfg.vad = Some(VadSettings {
        start_db: -25.0,
        end_db: -35.0,
    });
    s_cfg
        .duplex_requested
        .insert("dot2-b".into(), "half".into());
    s_cfg.keyword_interrupt = true;
    s_cfg.barge_in = true;
    s_cfg.barge_protect_ms = 300;
    s_cfg.asr_wake_fallback = true;
    s_cfg.proactive_asr_wake = true;
    sender.state.voice.restore_settings(s_cfg, "test").unwrap();

    let mut p_cfg: VoiceSettings = peer.state.voice.settings_snapshot();
    p_cfg.kws_threshold = 0.37;
    p_cfg.kws_score = 2.2;
    p_cfg.media_trim_db = -4.5;
    p_cfg.tail_blind_ms = 480;
    p_cfg.vad = Some(VadSettings {
        start_db: -33.0,
        end_db: -43.0,
    });
    p_cfg
        .duplex_requested
        .insert("dot2-b".into(), "full".into());
    p_cfg.keyword_interrupt = false;
    p_cfg.barge_in = false;
    p_cfg.barge_protect_ms = 1200;
    p_cfg.asr_wake_fallback = false;
    p_cfg.proactive_asr_wake = false;
    peer.state.voice.restore_settings(p_cfg, "test").unwrap();

    set_self_id(&sender, "dot2-d").await;
    add_peer(&sender, &peer, "dot2-b").await;
    let res = push(&sender, &["dot2-b"], false, false).await;
    assert!(res["results"][0]["ok"].as_bool().unwrap_or(false), "{res}");

    let after = peer.state.voice.settings_snapshot();
    assert_eq!(after.persona_prompt, "家庭共享性格");
    assert_eq!(after.follow_window_ms, 8500);
    assert_eq!(after.context_window_ms, 210_000);
    assert_eq!(after.wake_words, vec!["你好建国".to_string()]);
    assert!((after.kws_threshold - 0.37).abs() < 1e-6);
    assert!((after.kws_score - 2.2).abs() < 1e-6);
    assert!((after.media_trim_db - (-4.5)).abs() < 1e-6);
    assert_eq!(after.tail_blind_ms, 480);
    assert!((after.vad.unwrap().start_db - (-33.0)).abs() < 1e-6);
    assert!((after.vad.unwrap().end_db - (-43.0)).abs() < 1e-6);
    assert_eq!(
        after.duplex_requested.get("dot2-b").map(String::as_str),
        Some("full")
    );
    assert!(!after.keyword_interrupt);
    assert!(!after.barge_in);
    assert_eq!(after.barge_protect_ms, 1200);
    assert!(!after.asr_wake_fallback);
    assert!(!after.proactive_asr_wake);

    // Full store + AppState reopen proves persistence on disk.
    peer_srv.abort();
    drop(peer);
    drop(peer_store);
    let reopened_store = Arc::new(Store::open(&peer_db).unwrap());
    let reopened = AppState::new(
        reopened_store,
        peer_dir.clone(),
        MockInject::default(),
        None,
    );
    let r_cfg = reopened.voice.settings_snapshot();
    assert_eq!(r_cfg.follow_window_ms, 8500);
    assert!((r_cfg.vad.unwrap().start_db - (-33.0)).abs() < 1e-6);
    assert!((r_cfg.kws_threshold - 0.37).abs() < 1e-6);
    let _ = std::fs::remove_dir_all(&peer_dir);
}

#[tokio::test]
async fn direct_fleet_import_blocks_node_local_mode_overwrites() {
    use hm_core::voice::{VadSettings, VoiceSettings};

    let peer = spawn_node("direct-import-peer").await;
    give_it_a_wake_word(&peer, "你好建国", "n ǐ h ǎo j iàn g uó").await;
    let mut initial: VoiceSettings = peer.state.voice.settings_snapshot();
    initial.kws_threshold = 0.44;
    initial.media_trim_db = -3.0;
    initial.vad = Some(VadSettings {
        start_db: -38.0,
        end_db: -48.0,
    });
    peer.state.voice.restore_settings(initial, "test").unwrap();

    let mut malicious_mode = serde_json::to_value(peer.state.voice.settings_snapshot()).unwrap();
    malicious_mode["follow_window_ms"] = json!(7500);
    malicious_mode["wake_words"] = json!(["你好篡改"]);
    malicious_mode["kws_threshold"] = json!(0.11);
    malicious_mode["media_trim_db"] = json!(-11.0);
    malicious_mode["vad"] = json!({ "start_db": -15.0, "end_db": -25.0 });

    let resp = client()
        .post(format!("{}/api/v0/config/import", peer.base))
        .header("X-Home-Mate-Source", "dot2-d")
        .json(&json!({ "mode": malicious_mode }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let warnings_str = body["warnings"].to_string();
    assert!(warnings_str.contains("wake_words"), "{body}");
    assert!(warnings_str.contains("kws_threshold"), "{body}");
    assert!(warnings_str.contains("media_trim_db"), "{body}");

    let kept = peer.state.voice.settings_snapshot();
    assert_eq!(kept.follow_window_ms, 7500);
    assert_eq!(kept.wake_words, vec!["你好建国".to_string()]);
    assert!((kept.kws_threshold - 0.44).abs() < 1e-6);
    assert!((kept.media_trim_db - (-3.0)).abs() < 1e-6);
    assert!((kept.vad.unwrap().start_db - (-38.0)).abs() < 1e-6);

    // Direct mode_policy injection of a node-local field or system_prompt_override is rejected with FORBIDDEN_SCOPE.
    let policy_resp = client()
        .post(format!("{}/api/v0/config/import", peer.base))
        .header("X-Home-Mate-Source", "dot2-d")
        .json(&json!({ "mode_policy": { "kws_threshold": 0.1 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(policy_resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let policy_body: Value = policy_resp.json().await.unwrap();
    assert_eq!(
        policy_body["code"],
        json!("FORBIDDEN_SCOPE"),
        "{policy_body}"
    );

    let prompt_policy_resp = client()
        .post(format!("{}/api/v0/config/import", peer.base))
        .header("X-Home-Mate-Source", "dot2-d")
        .json(&json!({ "mode_policy": { "system_prompt_override": "X" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        prompt_policy_resp.status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    let prompt_policy_body: Value = prompt_policy_resp.json().await.unwrap();
    assert_eq!(
        prompt_policy_body["code"],
        json!("FORBIDDEN_SCOPE"),
        "{prompt_policy_body}"
    );
}
