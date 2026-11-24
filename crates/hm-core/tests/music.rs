//! MU-1 … MU-7: the music service acceptance ([docs/music-service-20261112]).
//!
//! Every test here talks to a REAL protocol partner (fake Subsonic/Personal
//! Services that check the credentials) through the production registry. The
//! one criterion that needs a person — MU-4, "the song is actually heard" —
//! lives in the field record, not here.

mod common;

use hm_core::api::AppState;
use hm_core::tool::mocks::MockInject;
use hm_core::tool::{ToolCtx, ToolStatus};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

async fn app() -> (Arc<AppState>, Arc<common::FakeMusic>) {
    let store = Arc::new(hm_core::store::Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let state = AppState::with_overrides(
        store,
        std::path::PathBuf::from("./data-test"),
        MockInject::default(),
        None,
        None,
        None,
        None,
    );
    let fake = common::spawn_fake_music().await;
    // The app's own HTTP surface (the console talks to it).
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _ = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, api).await;
    });
    (state, Arc::new(fake))
}

fn ctx(task: &str) -> ToolCtx {
    ToolCtx::new(task, "C1", Arc::new(AtomicBool::new(false)))
}

/// MU-1: nothing configured → `music_unconfigured`, zero plays, zero streams.
#[tokio::test]
async fn mu1_unconfigured_plays_nothing() {
    let (state, fake) = app().await;
    let r = state
        .registry
        .call(
            "music.play",
            json!({ "source": "library", "id": "t01" }),
            &ctx("T1"),
        )
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "MUSIC_UNCONFIGURED");
    let r = state
        .registry
        .call("music.search", json!({ "query": "晚风" }), &ctx("T1"))
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(
        fake.count("stream:"),
        0,
        "unconfigured must not fetch any audio"
    );
    assert_eq!(
        fake.count("search3:"),
        0,
        "unconfigured must not even search"
    );
}

/// A configured-but-disabled service is a different honest answer.
#[tokio::test]
async fn mu1b_disabled_says_disabled() {
    let (state, fake) = app().await;
    common::configure_music(&state.store, &fake, true, false, true, false);
    let r = state
        .registry
        .call("music.search", json!({ "query": "晚风" }), &ctx("T1"))
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "MUSIC_DISABLED");
    assert_eq!(fake.count("search3:"), 0);
}

/// MU-2 + MU-3: write through the management API, read back `secret_set` only,
/// and the probe really connects (count > 0, latency reported).
#[tokio::test]
async fn mu2_mu3_write_read_back_and_probe() {
    let (state, fake) = app().await;
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let client = reqwest::Client::new();

    // MU-2: the password goes in, only "it is set" comes back.
    let put: Value = client
        .put(format!("{base}/api/v0/config/services/music"))
        .json(&json!({
            "enabled": true,
            "prefer_local": true,
            "library": { "endpoint": fake.base, "user": "demo", "secret": common::LIB_SECRET },
            "online": { "endpoint": fake.base, "secret": common::ONLINE_TOKEN },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(put["library"]["secret_set"], true);
    assert_eq!(put["online"]["secret_set"], true);
    let raw = serde_json::to_string(&put).unwrap();
    assert!(
        !raw.contains(common::LIB_SECRET),
        "the library password must never be echoed"
    );
    assert!(
        !raw.contains(common::ONLINE_TOKEN),
        "the online token must never be echoed"
    );
    assert_eq!(put["library"]["user"], "demo");

    let get: Value = client
        .get(format!("{base}/api/v0/config/services/music"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get["enabled"], true);
    assert_eq!(get["prefer_local"], true);
    assert!(!serde_json::to_string(&get)
        .unwrap()
        .contains(common::LIB_SECRET));

    // MU-3: the probe is a real round trip on the LAN, and it reports numbers.
    let probe: Value = client
        .post(format!("{base}/api/v0/config/services/music/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["ok"], true, "both sources must answer: {probe}");
    assert_eq!(probe["library"]["ok"], true);
    assert_eq!(probe["library"]["count"], 1);
    assert!(probe["library"]["latency_ms"].as_u64().is_some());
    assert_eq!(probe["online"]["ok"], true);
    assert!(probe["online"]["count"].as_u64().unwrap() >= 1);
    assert!(
        fake.count("random") >= 1,
        "the library probe really queried the library"
    );
    assert!(
        fake.count("online_search:") >= 1,
        "the online probe really searched"
    );

    // A wrong password fails the probe honestly (no secret leaked in detail).
    let bad: Value = client
        .put(format!("{base}/api/v0/config/services/music"))
        .json(&json!({
            "enabled": true,
            "library": { "endpoint": fake.base, "user": "demo", "secret": "wrong" },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bad["library"]["secret_set"], true);
    let probe: Value = client
        .post(format!("{base}/api/v0/config/services/music/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        probe["library"]["ok"], false,
        "a wrong password must not probe ok"
    );
    assert!(!serde_json::to_string(&probe).unwrap().contains("wrong"));
}

/// A public endpoint is refused (credentials must not leave the LAN).
#[tokio::test]
async fn mu2b_public_endpoint_is_refused() {
    let (state, _fake) = app().await;
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let resp = reqwest::Client::new()
        .put(format!("{base}/api/v0/config/services/music"))
        .json(&json!({ "enabled": true, "library": { "endpoint": "http://8.8.8.8:4533", "user": "u", "secret": "x" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// MU-5: `prefer_local` + a library hit → the online service is not asked.
#[tokio::test]
async fn mu5_prefer_local_never_touches_online() {
    let (state, fake) = app().await;
    common::configure_music(&state.store, &fake, true, true, true, true);
    let r = state
        .registry
        .call("music.search", json!({ "query": "晚风" }), &ctx("T1"))
        .await;
    assert_eq!(r.status, ToolStatus::Completed);
    let v = r.result.unwrap();
    assert_eq!(v["status"], "available");
    assert_eq!(v["online_used"], false);
    assert_eq!(v["tracks"].as_array().unwrap().len(), 1);
    assert_eq!(v["tracks"][0]["source"], "library");
    assert_eq!(v["tracks"][0]["title"], "晚风信笺");
    assert_eq!(
        fake.count("online_search:"),
        0,
        "a local hit must stop the search there"
    );
}

/// MU-6: a miss is reported as a miss; online is only asked when the user
/// asked for online music (the tool schema's `source`, not a hidden retry).
#[tokio::test]
async fn mu6_online_only_when_asked() {
    let (state, fake) = app().await;
    common::configure_music(&state.store, &fake, true, true, true, true);

    // Default (auto) with no local match: nothing is played, nothing is
    // quietly fetched from online.
    let r = state
        .registry
        .call(
            "music.search",
            json!({ "query": "午夜电台不播的歌" }),
            &ctx("T1"),
        )
        .await;
    let v = r.result.unwrap();
    assert_eq!(v["status"], "no_matches");
    assert_eq!(
        fake.count("online_search:"),
        0,
        "no explicit ask = no online request"
    );
    assert!(
        v["hint"].as_str().unwrap_or("").contains("用户"),
        "the model is told to ask first"
    );

    // Explicitly online (the user asked for online music).
    let r = state
        .registry
        .call(
            "music.search",
            json!({ "query": "光年之外", "source": "online" }),
            &ctx("T2"),
        )
        .await;
    let v = r.result.unwrap();
    assert_eq!(v["online_used"], true);
    assert_eq!(v["tracks"][0]["source"], "online");
    assert_eq!(fake.count("online_search:"), 1);
    // An explicit online search does not bother the library at all.
    assert_eq!(
        fake.count("search3:"),
        1,
        "only the first (auto) search used the library"
    );
}

/// A household with ONLY the online source configured must actually use it:
/// "prefer local" has nothing to prefer, so the online search is the search.
#[tokio::test]
async fn an_online_only_household_asks_online() {
    let (state, fake) = app().await;
    common::configure_music(&state.store, &fake, false, true, true, true);
    let r = state
        .registry
        .call("music.search", json!({ "query": "夜曲" }), &ctx("T1"))
        .await;
    assert_eq!(r.status, ToolStatus::Completed);
    let v = r.result.unwrap();
    assert_eq!(v["status"], "available", "{v}");
    assert_eq!(v["online_used"], true);
    assert_eq!(v["tracks"][0]["source"], "online");
    assert_eq!(fake.count("online_search:"), 1);
    assert_eq!(
        fake.count("search3:"),
        0,
        "no library configured, nothing to search there"
    );
}

/// MU-7: the credentials never appear in the event log, config history or
/// probes — the places the console and the traces read.
#[tokio::test]
async fn mu7_secrets_never_reach_logs_history_or_trace() {
    let (state, fake) = app().await;
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let client = reqwest::Client::new();
    let _: Value = client
        .put(format!("{base}/api/v0/config/services/music"))
        .json(&json!({
            "enabled": true,
            "prefer_local": true,
            "library": { "endpoint": fake.base, "user": "demo", "secret": common::LIB_SECRET },
            "online": { "endpoint": fake.base, "secret": common::ONLINE_TOKEN },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = state
        .registry
        .call("music.search", json!({ "query": "晚风" }), &ctx("T1"))
        .await;
    assert_eq!(r.status, ToolStatus::Completed);
    let _: Value = client
        .post(format!("{base}/api/v0/config/services/music/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let history: Value = client
        .get(format!("{base}/api/v0/config/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = serde_json::to_string(&state.voice.recent(400)).unwrap();
    let hist = serde_json::to_string(&history).unwrap();
    for (name, hay) in [("event log", &events), ("config history", &hist)] {
        assert!(
            !hay.contains(common::LIB_SECRET),
            "library secret leaked into {name}"
        );
        assert!(
            !hay.contains(common::ONLINE_TOKEN),
            "online token leaked into {name}"
        );
    }
    // The history row exists (an applied change is recorded) and says a
    // secret is set, not what it is.
    assert!(
        hist.contains("\"scope\":\"music\""),
        "the music change was recorded: {hist}"
    );
    assert!(hist.contains("secret_set"));
}

/// An id that this turn's search did not return is not playable (the model
/// must not invent one), and the local-first rule never widens to online.
#[tokio::test]
async fn play_requires_a_candidate_from_this_turn() {
    let (state, fake) = app().await;
    common::configure_music(&state.store, &fake, true, true, true, true);
    let r = state
        .registry
        .call(
            "music.play",
            json!({ "source": "library", "id": "t01" }),
            &ctx("T1"),
        )
        .await;
    assert_eq!(r.status, ToolStatus::Failed);
    assert_eq!(r.error.unwrap().code, "MUSIC_CANDIDATE_REQUIRED");
    assert_eq!(fake.count("stream:"), 0);
}

async fn api_response(request: reqwest::RequestBuilder) -> Value {
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}

#[tokio::test]
async fn reviewed_music_publication_and_rollback_switch_the_actual_providers() {
    use hm_core::config_delivery::{uuid, ConfigWorker};
    use std::net::SocketAddr;
    use std::time::Duration;

    let (state, previous) = app().await;
    let candidate = common::spawn_fake_music().await;
    common::configure_music(&state.store, &previous, true, true, true, true);
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            api.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::new();
    let pairing = api_response(client.post(format!("{base}/api/v0/auth/provision"))).await;
    let token = pairing["token"].as_str().unwrap();
    let post = |path: &str| client.post(format!("{base}{path}")).bearer_auth(token);
    let put = |path: &str| client.put(format!("{base}{path}")).bearer_auth(token);
    let get = |path: &str| client.get(format!("{base}{path}")).bearer_auth(token);
    let preflight = |music: Value| {
        post("/api/v1/config/preflight").json(&json!({
            "package": {"format": "home-mate-config", "version": 1, "music": music}
        }))
    };
    let submit = |plan: &Value, application_id: &str| {
        post("/api/v1/config/applications").json(&json!({
            "plan_id": plan["plan_id"], "plan_hash": plan["plan_hash"],
            "expected_revision": plan["expected_revision"], "application_id": application_id
        }))
    };
    let apply = |plan: &Value| submit(plan, &uuid().unwrap());
    let published = async |id: &str| {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let proof =
                    api_response(get(&format!("/api/v1/config/applications/{id}/readback"))).await;
                let publication = &proof["publications"][0];
                assert_eq!(publication["block"], "music");
                if publication["state"] == "applied" {
                    return proof;
                }
                assert_eq!(publication["state"], "saved_pending", "{proof}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    };
    let search = async || {
        let result = state
            .registry
            .call(
                "music.search",
                json!({"query": "fixture"}),
                &ctx("music-package"),
            )
            .await;
        assert_eq!(result.status, ToolStatus::Completed, "{result:?}");
        result.result.unwrap()
    };

    api_response(put("/api/v0/fleet").json(&json!({"self_id":"music-fixture"}))).await;
    api_response(put("/api/v1/config/enrollment").json(&json!({}))).await;

    let bad = preflight(json!({"library": {"endpoint": "http://127.0.0.1/?token=inline"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(!bad.text().await.unwrap().contains("token=inline"));
    let invalid_endpoint = preflight(json!({"library": {"endpoint": "///"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_endpoint.status(), 400);
    assert_eq!(
        invalid_endpoint.json::<Value>().await.unwrap()["error"],
        "INVALID_MUSIC_CONFIG"
    );
    let bad = preflight(json!({"online": {"secret": "fixture-secret"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(!bad.text().await.unwrap().contains("fixture-secret"));
    assert_eq!(previous.count("search3:"), 0);
    assert_eq!(candidate.count("search3:"), 0);

    let mut v2 = api_response(post("/api/v1/config/exports").json(&json!({
        "format":"v2", "blocks":["music"]
    })))
    .await["package"]
        .clone();
    v2.as_object_mut().unwrap().remove("content_hash");
    v2["bindings"] = json!([{"action":"require_binding","binding_id":"music.online.secret"}]);
    let preflight_v2 =
        |package: &Value| post("/api/v1/config/preflight").json(&json!({"package":package}));
    let mut incomplete = v2.clone();
    incomplete["blocks"][0]["operations"] = json!([
        {"action":"replace","path":"music.library","value":{"user":"demo"}}
    ]);
    let rejected = preflight_v2(&incomplete).send().await.unwrap();
    assert_eq!(rejected.status(), 400);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"],
        "INCOMPLETE_MUSIC_CONNECTION"
    );

    let stale_plan = api_response(preflight_v2(&v2)).await;
    state
        .store
        .with(|c| {
            c.execute(
                "UPDATE secrets SET value='enc:v1:invalid' WHERE name='music.online.secret'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let rejected = apply(&stale_plan).send().await.unwrap();
    assert_eq!(rejected.status(), 409);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"],
        "PLAN_STALE"
    );
    let state_view = api_response(get("/api/v1/config/state")).await;
    let online = state_view["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding_id"] == "music.online.secret")
        .unwrap();
    assert_eq!(online["bound"], false);
    let rejected = preflight_v2(&v2).send().await.unwrap();
    assert_eq!(rejected.status(), 400);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"],
        "BINDING_REQUIRED"
    );
    state
        .store
        .secret_set("music.online.secret", common::ONLINE_TOKEN)
        .unwrap();

    let plan = api_response(preflight(json!({
        "enabled": true,
        "prefer_local": false,
        "library": {"endpoint": format!("{}/", candidate.base)},
        "online": {"endpoint": candidate.base, "user": " candidate-catalog "}
    })))
    .await;
    let library = plan["diff"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "music.library")
        .unwrap();
    assert_eq!(library["after"]["user"], "demo");
    assert_eq!(library["after"]["endpoint"], candidate.base);
    assert!(plan["migration_report"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["path"] == "music.library" && row["normalized"] == true));
    let app_id = uuid().unwrap();
    let saved = api_response(submit(&plan, &app_id)).await;
    assert_eq!(api_response(submit(&plan, &app_id)).await, saved);
    let id = saved["application_id"].as_str().unwrap();
    assert_eq!(saved["state"], "saved_pending");
    let pending = api_response(get(&format!("/api/v1/config/applications/{id}/readback"))).await;
    assert_eq!(pending["publications"][0]["state"], "saved_pending");
    search().await;
    assert_eq!(previous.count("search3:"), 1);
    assert_eq!(previous.count("online_search:"), 0);
    assert_eq!(candidate.count("search3:"), 0);

    let _worker = ConfigWorker::start(state.store.clone());
    let proof = published(id).await;
    assert!(
        proof["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["path"].as_str().unwrap().starts_with("music."))
            .all(|row| row["state"] == "core_readback_verified"),
        "{proof}"
    );
    let active = search().await;
    assert!(active["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|track| track["source"] == "online"
            && track["binding"] == format!("{}|candidate-catalog", candidate.base)));
    assert_eq!(previous.count("search3:"), 1);
    assert_eq!(candidate.count("search3:"), 1);
    assert_eq!(candidate.count("online_search:"), 1);
    let probe = api_response(post("/api/v0/config/services/music/test")).await;
    assert_eq!(probe["ok"], true, "{probe}");
    assert_eq!(candidate.count("random"), 1);
    assert_eq!(candidate.count("online_search:"), 2);

    for (format, field) in [("v1", "document"), ("v2", "package")] {
        let export = api_response(post("/api/v1/config/exports").json(&json!({
            "format": format, "blocks": ["music"]
        })))
        .await;
        assert!(!export.to_string().contains(common::LIB_SECRET));
        assert!(!export.to_string().contains(common::ONLINE_TOKEN));
        api_response(post("/api/v1/config/preflight").json(&json!({"package": export[field]})))
            .await;
    }

    let rollback = api_response(
        post(&format!(
            "/api/v1/config/applications/{id}/rollback-preflight"
        ))
        .json(&json!({"expected_revision": saved["config_revision"]})),
    )
    .await;
    let restored = api_response(apply(&rollback)).await;
    published(restored["application_id"].as_str().unwrap()).await;
    search().await;
    assert_eq!(previous.count("search3:"), 2);
    assert_eq!(previous.count("online_search:"), 0);
    assert_eq!(candidate.count("search3:"), 1);

    let disable = api_response(preflight(json!({"enabled": false, "library": null}))).await;
    let library = disable["diff"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "music.library")
        .unwrap();
    assert_eq!(library["layer_after_null"], true);
    assert!(library["layer_after"].is_null());
    let disabled_receipt = api_response(apply(&disable)).await;
    let did = disabled_receipt["application_id"].as_str().unwrap();
    published(did).await;
    let disabled = state
        .registry
        .call(
            "music.search",
            json!({"query":"fixture"}),
            &ctx("music-disabled"),
        )
        .await;
    assert_eq!(disabled.error.unwrap().code, "MUSIC_DISABLED");
    let reverse = api_response(
        post(&format!(
            "/api/v1/config/applications/{did}/rollback-preflight"
        ))
        .json(&json!({"expected_revision": disabled_receipt["config_revision"]})),
    )
    .await;
    let receipt = api_response(apply(&reverse)).await;
    published(receipt["application_id"].as_str().unwrap()).await;
    search().await;
    assert_eq!(previous.count("search3:"), 3);

    state.store.with(|c| {
        c.execute_batch("CREATE TRIGGER reject_music_publication BEFORE UPDATE ON config_kv WHEN OLD.key='music' BEGIN SELECT RAISE(ABORT, 'music publication test failure'); END")?;
        Ok(())
    }).unwrap();
    let plan = api_response(preflight(json!({"library":{"endpoint":candidate.base}}))).await;
    let failed = api_response(apply(&plan)).await;
    let failed_id = failed["application_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let proof = api_response(get(&format!(
                "/api/v1/config/applications/{failed_id}/readback"
            )))
            .await;
            let publication = &proof["publications"][0];
            if publication["state"] == "failed" {
                assert_eq!(publication["error"], "MUSIC_PUBLICATION_FAILED", "{proof}");
                break;
            }
            assert_eq!(publication["state"], "saved_pending", "{proof}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    search().await;
    assert_eq!(previous.count("search3:"), 4);
    assert_eq!(candidate.count("search3:"), 1);
}

#[tokio::test]
async fn direct_music_credentials_config_and_history_commit_together() {
    let (state, fixture) = app().await;
    let api = hm_core::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let client = reqwest::Client::new();
    let put = || client.put(format!("{base}/api/v0/config/services/music"));
    let probe = || client.post(format!("{base}/api/v0/config/services/music/test"));
    let save = |revision: Value, library: &str, online: &str| {
        put().json(&json!({
            "enabled": true,
            "library": {"endpoint": fixture.base, "user":"demo", "secret":library},
            "online": {"endpoint": fixture.base, "secret":online},
            "revision": revision,
        }))
    };
    let initial = api_response(put().json(&json!({
        "enabled": true,
        "library": {"endpoint":fixture.base, "user":"demo", "secret":common::LIB_SECRET},
        "online": {"endpoint":fixture.base, "user":"catalog", "secret":common::ONLINE_TOKEN}
    })))
    .await;
    let older_client = api_response(put().json(&json!({
        "enabled":true,
        "library":{"endpoint":fixture.base,"user":"demo"},
        "online":{"endpoint":fixture.base},
        "revision":initial["revision"]
    })))
    .await;
    assert_eq!(older_client, initial);
    assert_eq!(older_client["online"]["user"], "catalog");
    for invalid_user in ["with\0nul".into(), "a".repeat(4097)] {
        let rejected = put()
            .json(&json!({
                "enabled":true, "library":{"endpoint":fixture.base,"user":invalid_user}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), 400);
    }
    let invalid_endpoint = put()
        .json(&json!({"library":{"endpoint":"///"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_endpoint.status(), 400);
    let initial_history = state.config.history(30).unwrap();
    state.store.with(|c| {
        c.execute_batch("CREATE TRIGGER reject_music_history BEFORE INSERT ON config_history WHEN NEW.scope='music' BEGIN SELECT RAISE(ABORT, 'music test failure'); END")?;
        Ok(())
    }).unwrap();
    assert_eq!(
        save(initial["revision"].clone(), "new-library", "")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let rejected_import = client.post(format!("{base}/api/v0/config/import")).json(&json!({
        "format":"home-mate-config", "version":1,
        "music":{"enabled":true,"prefer_local":false,"library":{"endpoint":"http://127.0.0.1:1","user":"different"}},
        "secrets":{"music.library.secret":"discard-library","music.online.secret":""}
    })).send().await.unwrap();
    assert_eq!(rejected_import.status(), 400);
    assert!(!rejected_import
        .text()
        .await
        .unwrap()
        .contains("discard-library"));
    assert_eq!(state.config.history(30).unwrap(), initial_history);
    let current = api_response(client.get(format!("{base}/api/v0/config/services/music"))).await;
    assert_eq!(current, initial);
    assert_eq!(api_response(probe()).await["ok"], true);
    state
        .store
        .with(|c| {
            c.execute_batch("DROP TRIGGER reject_music_history")?;
            Ok(())
        })
        .unwrap();

    let rotated = api_response(save(
        initial["revision"].clone(),
        "new-library",
        common::ONLINE_TOKEN,
    ))
    .await;
    assert_ne!(rotated["revision"], initial["revision"]);
    assert_eq!(rotated["library"]["secret_set"], true);
    let stale = save(initial["revision"].clone(), common::LIB_SECRET, "")
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    assert_eq!(
        stale.json::<Value>().await.unwrap()["revision"],
        rotated["revision"]
    );
    let probe = api_response(probe()).await;
    assert_eq!(probe["library"]["ok"], false);
    assert_eq!(probe["online"]["ok"], true);
}
