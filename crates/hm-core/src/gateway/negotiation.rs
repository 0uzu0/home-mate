use super::frames::HelloDevice;

// --- protocol v1 handshake mechanism (#8/#11, 2026-11-16) --------------------

/// Budget for writing a `HelloReject` over the handshake socket before the
/// connection is torn down anyway (main repo side; the firmware keeps its
/// own `PROTOCOL_FAIL_DISCONNECT_MS`). A stuck peer must not park the
/// accept task past this.
pub(crate) const REJECT_WRITE_TIMEOUT_MS: u64 = 1000;

//
// Compatibility is INTERVAL INTERSECTION, not "peer version inside my
// range": checked on both sides, the naive rule degenerates into "highest
// versions equal" (core [0,1] accepts firmware 0, but firmware [0,0] rejects
// core 1), which would fail every release's firmware-confirmation stage once
// the version moves. The intersection rule lets [0,0] talk to [0,1] at 0.
// An illegal interval (min > max) never intersects. This is a handshake
// truth mechanism, not security: no replay or downgrade protection is
// intended (loopback trust domain).

/// `Some(operating)` when both intervals intersect; the operating version is
/// the highest both sides accept. Both sides encode at this version; the
/// mechanism release keeps behavior driven by capabilities and optional
/// fields, never by branching on this number.
pub fn protocol_operating_version(mine: (u32, u32), peer: (u32, u32)) -> Option<u32> {
    let (my_min, my_max) = mine;
    let (peer_min, peer_max) = peer;
    if my_min > my_max || peer_min > peer_max {
        return None; // an illegal interval rejects, never sweeps under the rug
    }
    let lo = my_min.max(peer_min);
    let hi = my_max.min(peer_max);
    (lo <= hi).then_some(hi)
}

/// Negotiate the highest operating protocol version supported by both peers.
/// Unlike raw interval intersection, this respects explicit `supported_versions`
/// lists so wire version `1` (never shipped) is never invented. When
/// `peer_supported` is empty (legacy peer), any version in the peer's
/// `[min, max]` interval that is also in `my_supported` is eligible.
pub fn protocol_operating_version_with_supported(
    mine: (u32, u32),
    my_supported: &[u32],
    peer: (u32, u32),
    peer_supported: &[u32],
) -> Option<u32> {
    let (my_min, my_max) = mine;
    let (peer_min, peer_max) = peer;
    if my_min > my_max || peer_min > peer_max {
        return None;
    }
    let lo = my_min.max(peer_min);
    let hi = my_max.min(peer_max);
    if lo > hi {
        return None;
    }
    (lo..=hi).rev().find(|v| {
        my_supported.contains(v) && (peer_supported.is_empty() || peer_supported.contains(v))
    })
}

pub const REASON_PROTOCOL_NO_INTERSECTION: &str = "protocol_no_intersection";

/// `HelloReject` / `ProtocolError` reason for a version interval with no
/// overlap, with both intervals spelled out for the log and the lamp.
pub fn protocol_no_intersection_reason(mine: (u32, u32), peer: (u32, u32)) -> String {
    format!(
        "{REASON_PROTOCOL_NO_INTERSECTION}: peer accepts [{},{}], this side accepts [{},{}]",
        peer.0, peer.1, mine.0, mine.1
    )
}

/// The firmware link has no authentication of its own. Startup refuses any
/// bind that is not loopback (`127.0.0.0/8` or `::1`). `0.0.0.0` and a LAN
/// address are both rejected unless `allow_lan` (`HM_GATEWAY_ALLOW_LAN=1`)
/// is explicitly enabled for Docker / NAS deployment.
pub fn ensure_loopback_bind(addr: std::net::SocketAddr) -> Result<(), String> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "HM_GATEWAY_BIND {addr} is not loopback; the device gateway must use 127.0.0.1 or ::1 (or set HM_GATEWAY_ALLOW_LAN=1 for Docker/NAS mode)"
        ))
    }
}

/// Validate `HM_GATEWAY_BIND` against the active bind policy.
/// - `allow_lan == false` (default on embedded speakers): strictly requires loopback (`127.0.0.0/8` or `::1`).
/// - `allow_lan == true` (`HM_GATEWAY_ALLOW_LAN=1` on NAS/Docker): allows `0.0.0.0` or LAN interface addresses.
pub fn ensure_gateway_bind_with_policy(
    addr: std::net::SocketAddr,
    allow_lan: bool,
) -> Result<(), String> {
    if allow_lan {
        Ok(())
    } else {
        ensure_loopback_bind(addr)
    }
}

/// Read `HM_GATEWAY_ALLOW_LAN` from the environment (`"1"` or `"true"`, case-insensitive).
pub fn gateway_allow_lan_from_env() -> bool {
    std::env::var("HM_GATEWAY_ALLOW_LAN")
        .ok()
        .map(|v| {
            let trimmed = v.trim();
            trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Refuse a hello that announces the same stable id twice. Every per-device
/// map in the core is keyed by that id, so a duplicate would alias two
/// speakers onto one state instead of failing loudly.
pub fn ensure_unique_device_ids(devices: &[HelloDevice]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for d in devices {
        if !seen.insert(d.stable_id.as_str()) {
            return Err(format!(
                "duplicate device id '{}': a gateway must announce distinct ids",
                d.stable_id
            ));
        }
    }
    Ok(())
}
