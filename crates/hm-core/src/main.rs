//! hm-core service entry: binds 127.0.0.1 (default :8930), opens the data
//! directory, loads ./data/env.local (KEY=VALUE) into the environment if
//! not already set, serves the management API — and, when a built console
//! is found (HM_WEB_DIR, default ./web/dist), serves it on the same port
//! so a deployment needs nothing else.

use hm_core::api::{router, AppState};
use hm_core::tool::mocks::MockInject;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

fn load_env_file(path: &std::path::Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            if std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
    }
}

// This is runtime evidence, not a desired configuration change. Keep the
// report and trigger suppression in one short transaction so startup cannot
// invalidate pending jobs or leave applying set after a failed report write.
fn record_maintenance(
    store: &hm_core::store::Store,
    report: &serde_json::Value,
) -> hm_core::store::Result<()> {
    store.with(|c| {
        let tx=rusqlite::Transaction::new_unchecked(c,rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("UPDATE config_head SET applying=1 WHERE id=1",[])?;
        tx.execute("INSERT INTO config_kv(key,value) VALUES('db.maintenance',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[report.to_string()])?;
        tx.execute("UPDATE config_head SET applying=0 WHERE id=1",[])?;
        tx.commit()?;
        Ok(())
    })
}

/// Resolve the console directory: HM_WEB_DIR wins (a set-but-missing dir is
/// reported, not silently ignored); otherwise ./web/dist when present (dev
/// checkout shape); otherwise API-only mode.
fn web_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("HM_WEB_DIR") {
        let p = PathBuf::from(&dir);
        if p.join("index.html").is_file() {
            return Some(p);
        }
        eprintln!("HM_WEB_DIR={dir} has no index.html — serving API only");
        return None;
    }
    let default = PathBuf::from("./web/dist");
    if default.join("index.html").is_file() {
        Some(default)
    } else {
        None
    }
}

#[tokio::main]
async fn main() {
    // `hm-core --version` prints the same identity the release tooling reads
    // out of `.rodata`, plus the compiled VAD backend: an operator can check
    // "is this the fat build we meant to ship" on the host before packaging.
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--version") | Some("-V")
    ) {
        // Read the backend FROM the marker (not just the const): taking its
        // address here is what keeps the `hm-core-vad <backend>` bytes in
        // .rodata, where `hm-release pack` looks for them.
        let marker = hm_core::HM_CORE_VAD_MARKER;
        let vad = marker
            .trim_end_matches('\0')
            .strip_prefix("hm-core-vad ")
            .unwrap_or(hm_core::VAD_BACKEND);
        println!(
            "hm-core {} {} (vad {vad})",
            hm_core::CORE_VERSION,
            hm_core::BUILD_ID,
        );
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hm_core=info,tower_http=info".into()),
        )
        .init();

    // Speaker deployment: HM_CORE_CPU (supervisor = 2) keeps the core off the
    // audio firmware's core. Opt-in, degrade-never-panic (D/CPU task); the
    // tokio workers already exist here, so pin_process_to_cpu walks the tasks.
    if let Some(res) = hm_core::cpu::pin_from_env("HM_CORE_CPU") {
        match res {
            Ok(n) => {
                let cpu = std::env::var("HM_CORE_CPU").unwrap_or_default();
                tracing::info!("cpu affinity: {n} thread(s) pinned to cpu{cpu}");
            }
            Err(e) => tracing::warn!("cpu affinity: running unpinned ({e})"),
        }
    }

    let data_dir = PathBuf::from(std::env::var("HM_DATA_DIR").unwrap_or_else(|_| "./data".into()));
    load_env_file(&data_dir.join("env.local"));

    // Speaker deployments (HM_TIME_BOOTSTRAP=1): fix a cold-booted 2010 clock
    // before any cloud TLS starts. Best-effort, bounded, logged — never fatal.
    if let Some(secs) = hm_core::net::time_bootstrap_if_needed().await {
        tracing::info!("system clock bootstrapped to {secs}");
    }

    // M8a boot guard: a deliberate rollback or a power cut during an
    // activation may have left a newer-schema DB for this (older) core. The
    // restore happens here, before the store opens the file.
    if let Some(record) = hm_core::release::boot_guard(&data_dir, hm_core::BUILD_ID) {
        tracing::warn!(record = %record, "release boot guard ran before opening the database");
    }

    let db = data_dir.join("home-mate.db");
    let store = match hm_core::store::Store::open(&db) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("cannot open store at {}: {e}", db.display());
            std::process::exit(1);
        }
    };

    // M12a-1 (v0.26) credentials pass, before anything can serve a request:
    // 1. rows written by older releases are plaintext — seal them in place;
    // 2. a key that only exists in the environment (`data/env.local`) becomes a
    //    stored, sealed secret, which is what makes it exportable and pushable.
    // Both are idempotent; only names are logged, never values.
    {
        match store.seal_plaintext_rows() {
            Ok(done) if !done.is_empty() => {
                tracing::info!(sealed = %done.join(","), "vault: sealed {} plaintext secret row(s)", done.len());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("vault: sealing plaintext rows failed: {e}"),
        }
        let config = hm_core::config::ConfigService::new(store.clone());
        match config.backfill_env_secrets() {
            Ok(done) if !done.is_empty() => {
                tracing::info!(backfilled = %done.join(","), "vault: stored {} credential(s) that only lived in the environment", done.len());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("vault: env backfill failed: {e}"),
        }
    }

    // What the core actually applied. `hm-release` reads this sidecar to
    // decide whether a rollback must restore the pre-activation snapshot.
    match store.schema_version() {
        Ok(schema) => {
            if let Err(e) =
                hm_core::release::write_schema_sidecar(&data_dir, schema, hm_core::BUILD_ID, None)
            {
                tracing::warn!("schema sidecar: {e}");
            }
        }
        Err(e) => tracing::warn!("schema version unavailable: {e}"),
    }

    // Retention + WAL maintenance (2026-11-13, roadmap §10): rows older than
    // the window go, the WAL is checkpointed/truncated, and the report is
    // persisted for `/api/v0/status` (`db`). HM_DB_RETENTION_DAYS: default 14
    // days, 0 keeps everything (checkpoint only), capped at 10 years.
    let retention_days: u64 = std::env::var("HM_DB_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(14)
        .min(3650);
    {
        let store = store.clone();
        match store.maintain(retention_days) {
            Ok(report) => {
                let _ = record_maintenance(&store, &report);
                tracing::info!("db maintenance: {report}");
            }
            Err(e) => tracing::warn!("db maintenance failed: {e}"),
        }
        // Every six hours: bounded growth without a cron/daemon dependency.
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
                match store.maintain(retention_days) {
                    Ok(report) => {
                        let _ = record_maintenance(&store, &report);
                        tracing::info!("db maintenance: {report}");
                    }
                    Err(e) => tracing::warn!("db maintenance failed: {e}"),
                }
            }
        });
    }

    // Demo-scale failure injection via env (see scripts/verify_m1.py):
    //   HM_INJECT_HOTLIST_DELAY_MS=3000   — slow the chart tool down
    //   HM_INJECT_PLAY_FAIL=t01           — force play failures for a track
    //   HM_INJECT_LIGHT_OFFLINE=1         — home backend "offline"
    let inject = MockInject {
        hotlist_delay_ms: std::env::var("HM_INJECT_HOTLIST_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        play_plan: std::env::var("HM_INJECT_PLAY_FAIL")
            .ok()
            .map(|ids| {
                ids.split(',')
                    .map(|id| id.trim().to_string())
                    .filter(|id| !id.is_empty())
                    .map(|id| {
                        (
                            id,
                            hm_core::tool::mocks::PlayOutcome::Fail {
                                reason: "输出设备忙（注入）".into(),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ..MockInject::default()
    };
    let bind: SocketAddr = std::env::var("HM_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8930".into())
        .parse()
        .expect("HM_BIND must be host:port");
    if bind.ip().is_loopback() && bind.port() != 8930 {
        store.allow_loopback_public_for_test();
    }

    let state = AppState::with_overrides(
        store.clone(),
        data_dir.clone(),
        inject,
        None,
        None,
        None,
        web_dir(),
    );

    let _config_worker = hm_core::config_delivery::ConfigApplyCoordinator::start_for_app(&state);

    // Device-gateway listener. The gateway dials in; default 127.0.0.1:8932.
    // A non-loopback bind is refused: the firmware link has no authentication
    // of its own, so it must stay on this machine.
    let gateway_bind: SocketAddr = std::env::var("HM_GATEWAY_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8932".into())
        .parse()
        .expect("HM_GATEWAY_BIND must be host:port");
    if let Err(e) = hm_core::gateway::ensure_loopback_bind(gateway_bind) {
        eprintln!("refusing to start: {e}");
        std::process::exit(1);
    }
    state.start_gateway_listener(gateway_bind);

    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {bind}: {e}"));

    // Keep the build marker in the binary: `hm-release pack` reads it from
    // .rodata so the manifest's build id cannot drift from the artifact.
    // (Referencing it here is what stops LTO from dropping the unused static.)
    let marker = String::from_utf8_lossy(&hm_core::HM_CORE_BUILD_MARKER);
    tracing::info!(
        "Home Mate core listening on http://{bind} (data: {}, assistant: {}); build {}",
        state.data_dir.display(),
        state.assistant_name,
        marker.trim_end_matches('\0')
    );
    println!("Home Mate core → http://{bind}  (console dev: web/ on :8931)");

    // M9: the auth gate distinguishes loopback (trusted: hm-release) from LAN
    // peers, so the peer address must reach the handlers. Serving without
    // connect info would make every LAN request look like "unknown peer" and
    // the gate fails closed — keep this service builder.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await
    .expect("server error");
}
