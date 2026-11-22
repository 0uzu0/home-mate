//! Health confirmation: a release is "active" only after the device says so.
//!
//! The criteria are the ones in the S4 brief (core `/status`,
//! `gateway.connected`, `kws.state=running`, model loaded) plus the two
//! identities that make "the new version is running" checkable rather than
//! assumed: the core build id and the served console entry file.

use serde_json::Value;
use std::time::{Duration, Instant};

use crate::device::Paths;
use crate::http;
use crate::manifest::{marker_version, Manifest};
use crate::state::{Check, ReleaseRef};

#[derive(Clone, Debug)]
pub struct Expect {
    pub model: String,
    pub build_id: String,
    pub firmware_version: String,
    pub firmware_build_id: String,
    pub gateway_version: String,
    pub console_index: String,
}

impl Expect {
    pub fn from_manifest(m: &Manifest) -> Self {
        Self {
            model: m.target.model.clone(),
            build_id: m.components.core.build_id.clone(),
            firmware_version: m.components.firmware.version.clone(),
            firmware_build_id: m.components.firmware.build_id.clone(),
            gateway_version: m.components.firmware.gateway_version.clone(),
            console_index: m.components.console.index.clone(),
        }
    }

    pub fn from_ref(r: &ReleaseRef) -> Self {
        Self {
            model: r.model.clone(),
            build_id: r.build_id.clone(),
            firmware_version: r.firmware_version.clone(),
            firmware_build_id: r.firmware_build_id.clone(),
            gateway_version: r.gateway_version.clone(),
            console_index: r.console_index.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub ok: bool,
    pub checks: Vec<Check>,
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok,
        detail: detail.into(),
    }
}

fn status_get(paths: &Paths) -> Option<Value> {
    paths.status().ok()
}

fn mode_get(paths: &Paths) -> Option<Value> {
    paths.mode().ok()
}

fn first_device(mode: &Value) -> Option<&Value> {
    mode.get("devices")?.as_array()?.first()
}

/// One evaluation pass. Every criterion is reported with its evidence even
/// when an earlier one failed (a rollback receipt should say what was wrong).
pub fn evaluate(paths: &Paths, want: &Expect) -> Report {
    let status = status_get(paths);
    let mode = mode_get(paths);
    let mut checks = Vec::new();

    // core_status: reachable AND the build we intended to activate.
    let got_build = status
        .as_ref()
        .and_then(|s| s.get("build"))
        .and_then(|b| b.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let core_ok = status.is_some() && got_build == want.build_id;
    checks.push(check(
        "core_status",
        core_ok,
        if status.is_none() {
            "no /api/v0/status response".to_string()
        } else {
            format!("build {got_build} (expected {})", want.build_id)
        },
    ));

    // gateway: connected, device online, gateway version marker.
    let gw = status.as_ref().and_then(|s| s.get("gateway"));
    let gw_connected = gw
        .and_then(|g| g.get("connected"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let gw_version = gw
        .and_then(|g| g.get("gateway"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let device_online = gw
        .and_then(|g| g.get("devices"))
        .and_then(|d| d.as_array())
        .map(|d| {
            d.iter()
                .any(|x| x.get("online").and_then(|v| v.as_bool()).unwrap_or(false))
        })
        .unwrap_or(false);
    let gw_version_ok = want.gateway_version.is_empty()
        || gw_version.contains(&format!(" {}", want.gateway_version))
        || gw_version.contains(&want.gateway_version);
    checks.push(check(
        "gateway",
        gw_connected && device_online && gw_version_ok,
        format!(
            "connected={gw_connected} device_online={device_online} gateway={gw_version:?} (expected {})",
            want.gateway_version
        ),
    ));

    // Device model + firmware come from the gateway descriptor in /status;
    // /mode reports behavior state, not the device identity.
    let gw_dev = status
        .as_ref()
        .and_then(|s| s.get("gateway"))
        .and_then(|g| g.get("devices"))
        .and_then(|d| d.as_array())
        .and_then(|d| d.first());

    let got_model = gw_dev
        .and_then(|d| d.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    checks.push(check(
        "device_model",
        got_model == want.model,
        format!("model {got_model:?} (expected {:?})", want.model),
    ));

    // firmware version (device fact, not the manifest).
    let firmware = gw_dev
        .and_then(|d| d.get("firmware"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let fw_version = marker_version(firmware, "hm-firmware ").unwrap_or_default();
    // The build id is compared when the release records one: a rebuild can
    // keep the same version, so the version alone is not the binary identity.
    let fw_build_ok =
        want.firmware_build_id.is_empty() || firmware.contains(&want.firmware_build_id);
    let want_build = if want.firmware_build_id.is_empty() {
        "not recorded".to_string()
    } else {
        want.firmware_build_id.clone()
    };
    checks.push(check(
        "firmware",
        fw_version == want.firmware_version && fw_build_ok,
        format!(
            "firmware {fw_version:?} build {} (expected {:?} build {want_build})",
            if want.firmware_build_id.is_empty() {
                "not recorded".to_string()
            } else if fw_build_ok {
                "matches".to_string()
            } else {
                "missing".to_string()
            },
            want.firmware_version
        ),
    ));

    // KWS running.
    let kws_state = first_device_opt(mode.as_ref())
        .and_then(|d| d.get("kws"))
        .and_then(|k| k.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    checks.push(check(
        "kws",
        kws_state == "running",
        format!("kws.state={kws_state:?}"),
    ));

    // VAD model actually loaded (not the energy fallback).
    let endpoint = first_device_opt(mode.as_ref())
        .and_then(|d| d.get("mic"))
        .and_then(|m| m.get("vad"))
        .and_then(|v| v.get("endpoint"));
    let kind = endpoint
        .and_then(|e| e.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let fallback = endpoint
        .and_then(|e| e.get("fallback"))
        .and_then(|v| v.as_str());
    let detail = endpoint
        .and_then(|e| e.get("model_detail"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    checks.push(check(
        "model",
        vad_endpoint_ok(kind, fallback, detail),
        format!(
            "endpoint kind={kind:?} fallback={:?} detail={detail:?}",
            fallback.unwrap_or("none")
        ),
    ));

    // Console: the served index references the bundle this release shipped.
    let console = http::request(
        "GET",
        &format!("{}/", paths.core_url),
        None,
        Duration::from_secs(5),
    );
    let (console_ok, console_detail) = match console {
        Ok(r) if r.status == 200 => {
            let body = String::from_utf8_lossy(&r.body);
            let ok = body.contains(&want.console_index);
            (
                ok,
                format!("index references {:?}: {ok}", want.console_index),
            )
        }
        Ok(r) => (false, format!("GET / → HTTP {}", r.status)),
        Err(e) => (false, format!("GET / failed: {e}")),
    };
    checks.push(check("console", console_ok, console_detail));

    let ok = checks.iter().all(|c| c.ok);
    Report { ok, checks }
}

/// `gateway_vad` is the Stage 2 endpoint (the gateway runs Silero and the core
/// consumes its verdicts); with `silero` both mean a real model is loaded.
fn vad_endpoint_ok(kind: &str, fallback: Option<&str>, detail: &str) -> bool {
    matches!(kind, "silero" | "gateway_vad") && fallback.is_none() && !detail.is_empty()
}

fn first_device_opt(mode: Option<&Value>) -> Option<&Value> {
    mode.and_then(first_device)
}

/// Poll until every criterion passes or the budget runs out.
pub fn wait_healthy(paths: &Paths, want: &Expect, budget: Duration) -> (Report, u64) {
    let start = Instant::now();
    let mut last = evaluate(paths, want);
    loop {
        if last.ok || start.elapsed() >= budget {
            return (last, start.elapsed().as_millis() as u64);
        }
        std::thread::sleep(Duration::from_millis(500));
        last = evaluate(paths, want);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn vad_endpoint_accepts_silero_and_gateway_vad_only() {
        assert!(super::vad_endpoint_ok("silero", None, "loaded in 10 ms"));
        assert!(super::vad_endpoint_ok(
            "gateway_vad",
            None,
            "loaded in 1262 ms"
        ));
        assert!(!super::vad_endpoint_ok("energy", None, "x"));
        assert!(!super::vad_endpoint_ok("gateway_vad", Some("energy"), "x"));
        assert!(!super::vad_endpoint_ok("gateway_vad", None, ""));
    }

    use super::*;

    #[test]
    fn expectation_comes_from_the_manifest() {
        let r = ReleaseRef {
            id: "3-v".into(),
            serial: 3,
            version: "v".into(),
            build_id: "abc".into(),
            firmware_version: "0.0.44".into(),
            gateway_version: "0.0.44".into(),
            console_index: "index-x.js".into(),
            model: "AEOBC".into(),
            ..Default::default()
        };
        let e = Expect::from_ref(&r);
        assert_eq!(e.build_id, "abc");
        assert_eq!(e.console_index, "index-x.js");
        assert_eq!(e.model, "AEOBC");
    }
}
