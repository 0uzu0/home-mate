//! Active LAN discovery responder and periodic beacon over UDP 8933 (#271).
//!
//! Mobile and smart-display clients (e.g. `home-mate-gateway-android` v2.2.0+)
//! broadcast `{"type":"home_mate_discover"}` to UDP 8933 or listen passively
//! for periodic `home_mate_beacon` announcements so household Cores are found
//! instantaneously without brute-force 254-IP subnet scanning.
//!
//! Security boundary:
//! - Discovery beacons carry only public household metadata (`version`,
//!   `assistant_name`, `room_name`, `web_port`, `gateway_port`,
//!   `voice_gateway_open`, `auth_required`, `multi_gateway`).
//! - Secrets, Bearer tokens, and credentials are NEVER included.

use crate::api::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

pub const DISCOVERY_UDP_PORT: u16 = 8933;
pub const DISCOVERY_REQUEST_TYPE: &str = "home_mate_discover";
pub const DISCOVERY_BEACON_TYPE: &str = "home_mate_beacon";
pub const DEFAULT_BEACON_INTERVAL_SECS: u64 = 20;
const MAX_UDP_PROBE_BYTES: usize = 2048;
const BEACON_CACHE_TTL: Duration = Duration::from_secs(2);
const PEER_REPLY_COOLDOWN: Duration = Duration::from_millis(150);
const MAX_TRACKED_PEERS: usize = 256;

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub bind_addr: SocketAddr,
    pub web_port: u16,
    pub gateway_port: u16,
    pub broadcast_interval: Option<Duration>,
}

impl DiscoveryConfig {
    pub fn for_ports(web_port: u16, gateway_port: u16) -> Self {
        let port = std::env::var("HM_DISCOVERY_PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(DISCOVERY_UDP_PORT);
        Self {
            bind_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
            web_port,
            gateway_port,
            broadcast_interval: Some(Duration::from_secs(DEFAULT_BEACON_INTERVAL_SECS)),
        }
    }
}

pub fn discovery_disabled_from_env() -> bool {
    std::env::var("HM_DISCOVERY_DISABLE")
        .ok()
        .map(|v| {
            let trimmed = v.trim();
            trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Resolve the best human-readable room name for this Core without leaking any
/// private fields: prefer the primary online device's configured room, then
/// any configured device room, else empty string.
fn resolve_room_name(state: &AppState) -> String {
    let Ok(meta) = state.config.device_meta() else {
        return String::new();
    };
    if let Some(primary) = state.hub.primary_audio_out() {
        if let Some(m) = meta.get(&primary.stable_id) {
            let r = m.room.trim();
            if !r.is_empty() {
                return r.to_string();
            }
        }
    }
    let mut keys: Vec<&String> = meta.keys().collect();
    keys.sort();
    for k in keys {
        if let Some(m) = meta.get(k) {
            let r = m.room.trim();
            if !r.is_empty() {
                return r.to_string();
            }
        }
    }
    String::new()
}

/// Build the public JSON beacon payload advertised on UDP 8933.
/// Never includes secrets or tokens.
pub fn build_beacon_payload(state: &AppState, web_port: u16, gateway_port: u16) -> Value {
    json!({
        "type": DISCOVERY_BEACON_TYPE,
        "core_id": state.auth.core_id(),
        "version": crate::CORE_VERSION,
        "assistant_name": state.effective_assistant_name(),
        "room_name": resolve_room_name(state),
        "web_port": web_port,
        "gateway_port": gateway_port,
        "voice_gateway_open": state.hub.voice_gateway_open(),
        "auth_required": state.auth.configured(),
        "lan_gateway_exempt": state.auth.lan_gateway_exempt(),
        "multi_gateway": true,
    })
}

fn is_discover_probe(raw: &[u8]) -> bool {
    let Ok(val) = serde_json::from_slice::<Value>(raw) else {
        return false;
    };
    val.get("type").and_then(Value::as_str) == Some(DISCOVERY_REQUEST_TYPE)
}

/// Bind a UDP discovery socket and enable broadcast (best-effort).
pub async fn bind_discovery_socket(
    bind_addr: SocketAddr,
) -> std::io::Result<(UdpSocket, SocketAddr)> {
    let socket = UdpSocket::bind(bind_addr).await?;
    let _ = socket.set_broadcast(true);
    let local = socket.local_addr()?;
    Ok((socket, local))
}

struct BeaconCache {
    bytes: Vec<u8>,
    key: (bool, bool, bool),
    built_at: Instant,
}

fn cached_beacon_bytes(
    cache: &mut Option<BeaconCache>,
    state: &AppState,
    web_port: u16,
    gateway_port: u16,
) -> Option<Vec<u8>> {
    let key = (
        state.auth.configured(),
        state.auth.lan_gateway_exempt(),
        state.hub.voice_gateway_open(),
    );
    let now = Instant::now();
    if let Some(c) = cache.as_ref() {
        if c.key == key && now.duration_since(c.built_at) < BEACON_CACHE_TTL {
            return Some(c.bytes.clone());
        }
    }
    let payload = build_beacon_payload(state, web_port, gateway_port);
    let bytes = serde_json::to_vec(&payload).ok()?;
    *cache = Some(BeaconCache {
        bytes: bytes.clone(),
        key,
        built_at: now,
    });
    Some(bytes)
}

fn allow_peer_reply(peers: &mut HashMap<IpAddr, Instant>, peer: SocketAddr) -> bool {
    let now = Instant::now();
    if peers.len() >= MAX_TRACKED_PEERS {
        peers.retain(|_, ts| now.duration_since(*ts) < Duration::from_secs(10));
        if peers.len() >= MAX_TRACKED_PEERS {
            peers.clear();
        }
    }
    if let Some(last) = peers.get(&peer.ip()) {
        if now.duration_since(*last) < PEER_REPLY_COOLDOWN {
            return false;
        }
    }
    peers.insert(peer.ip(), now);
    true
}

/// Serve discovery probes (unicast reply) and optional periodic LAN broadcasts
/// until the task is aborted.
pub async fn run_discovery_loop(
    socket: UdpSocket,
    state: Arc<AppState>,
    web_port: u16,
    gateway_port: u16,
    broadcast_interval: Option<Duration>,
) {
    let broadcast_port = socket
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(DISCOVERY_UDP_PORT);
    let broadcast_target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, broadcast_port));
    let mut ticker = broadcast_interval.map(tokio::time::interval);
    if let Some(ref mut t) = ticker {
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }

    let mut buf = vec![0u8; MAX_UDP_PROBE_BYTES];
    let mut cache: Option<BeaconCache> = None;
    let mut peer_cooldown: HashMap<IpAddr, Instant> = HashMap::new();
    loop {
        if let Some(ref mut tick) = ticker {
            tokio::select! {
                recv = socket.recv_from(&mut buf) => {
                    match recv {
                        Ok((n, peer)) => {
                            if is_discover_probe(&buf[..n]) && allow_peer_reply(&mut peer_cooldown, peer) {
                                if let Some(bytes) = cached_beacon_bytes(&mut cache, &state, web_port, gateway_port) {
                                    if let Err(e) = socket.send_to(&bytes, peer).await {
                                        tracing::debug!(%peer, error = %e, "discovery udp unicast send error");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "discovery udp recv error");
                        }
                    }
                }
                _ = tick.tick() => {
                    if let Some(bytes) = cached_beacon_bytes(&mut cache, &state, web_port, gateway_port) {
                        if let Err(e) = socket.send_to(&bytes, broadcast_target).await {
                            tracing::debug!(%broadcast_target, error = %e, "discovery udp broadcast send error");
                        }
                    }
                }
            }
        } else {
            match socket.recv_from(&mut buf).await {
                Ok((n, peer)) => {
                    if is_discover_probe(&buf[..n]) && allow_peer_reply(&mut peer_cooldown, peer) {
                        if let Some(bytes) =
                            cached_beacon_bytes(&mut cache, &state, web_port, gateway_port)
                        {
                            if let Err(e) = socket.send_to(&bytes, peer).await {
                                tracing::debug!(%peer, error = %e, "discovery udp unicast send error");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "discovery udp recv error");
                }
            }
        }
    }
}

/// Best-effort production startup for the LAN UDP discovery responder/beacon.
/// A bind conflict (e.g. multiple cores on localhost during multi-core smoke
/// tests) logs a warning and degrades cleanly without crashing the core.
pub fn spawn_discovery_beacon(
    state: Arc<AppState>,
    cfg: DiscoveryConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match bind_discovery_socket(cfg.bind_addr).await {
            Ok((socket, local)) => {
                tracing::info!(
                    %local,
                    web_port = cfg.web_port,
                    gateway_port = cfg.gateway_port,
                    "LAN UDP discovery beacon up"
                );
                run_discovery_loop(
                    socket,
                    state,
                    cfg.web_port,
                    cfg.gateway_port,
                    cfg.broadcast_interval,
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    bind_addr = %cfg.bind_addr,
                    error = %e,
                    "LAN UDP discovery beacon skipped (bind unavailable)"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn udp_discovery_responds_with_beacon_and_zero_secrets() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "hm-core-test-discovery-{}-{}",
            std::process::id(),
            crate::id::now_ms()
        ));
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let state = AppState::with_overrides(
            store,
            tmp_dir.clone(),
            crate::tool::mocks::MockInject::default(),
            None,
            None,
            None,
            None,
        );
        let token = state.auth.provision("owner").unwrap();
        state
            .config
            .save_device_meta("dot2-living", "客厅音箱", "客厅", "test")
            .unwrap();
        state.hub.set_voice_gateway_open(true);

        let (sock, local_addr) = bind_discovery_socket("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let handle = tokio::spawn(run_discovery_loop(sock, state.clone(), 8930, 8932, None));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(br#"{"type":"home_mate_discover"}"#, local_addr)
            .await
            .unwrap();

        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .expect("discovery reply arrived")
            .unwrap();
        let raw = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(!raw.contains(&token), "beacon must never leak bearer token");
        let beacon: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(beacon["type"], "home_mate_beacon");
        assert!(
            beacon["core_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("hmcore_") && s.len() > 10),
            "beacon must advertise stable core_id"
        );
        assert_eq!(beacon["version"], crate::CORE_VERSION);
        assert_eq!(beacon["room_name"], "客厅");
        assert_eq!(beacon["web_port"], 8930);
        assert_eq!(beacon["gateway_port"], 8932);
        assert_eq!(beacon["voice_gateway_open"], true);
        assert_eq!(beacon["auth_required"], true);
        assert_eq!(beacon["lan_gateway_exempt"], false);
        assert_eq!(beacon["multi_gateway"], true);

        state.auth.set_lan_gateway_exempt(true).unwrap();
        // Wait past the per-peer unicast cooldown so the 2nd real UDP probe receives an immediate reply.
        tokio::time::sleep(PEER_REPLY_COOLDOWN + Duration::from_millis(20)).await;
        client
            .send_to(br#"{"type":"home_mate_discover"}"#, local_addr)
            .await
            .unwrap();
        let (n2, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .expect("second discovery reply arrived")
            .unwrap();
        let raw2 = std::str::from_utf8(&buf[..n2]).unwrap();
        assert!(
            !raw2.contains(&token),
            "beacon must never leak bearer token"
        );
        let updated: Value = serde_json::from_str(raw2).unwrap();
        assert_eq!(updated["lan_gateway_exempt"], true);
        assert_eq!(updated["core_id"], beacon["core_id"]);

        handle.abort();
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
