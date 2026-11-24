//! Node-to-node configuration push (M12a-1, 2026-11-15).
//!
//! The household has more than one speaker and (today) no shared database:
//! each node owns its own config, its own credentials and its own SQLite file
//! (the user's 2026-11-13 decision — a single writer per node, manual
//! migration). What was missing is the routine between nodes: "this one is
//! configured, put the same configuration on that one" without hand-copying
//! an export file.
//!
//! Shape (user decision 2026-11-15, roadmap §0.1.1):
//!
//! - Peers live in `config_kv["fleet"]` as plain metadata — **no schema
//!   change**, because `hm-release`'s snapshot/rollback path compares schema
//!   versions and a new table would not be in an older snapshot.
//! - Each peer's bearer token lives in the `secrets` table as
//!   `fleet.<id>.token`, i.e. it is sealed by the vault like every other
//!   credential and never leaves the device (not in exports, not in the
//!   fleet JSON, not in `/status`).
//! - A push is a plain `POST /api/v0/config/import` on the peer with
//!   `X-Home-Mate-Source: <this node>`, so the peer's config history records
//!   `actor=fleet:<this node>`. The receiver reuses the import path it already
//!   has — there is no second configuration-write implementation.
//! - Two things never travel, even with `include_secrets`: the target's own
//!   wake words (`mode.wake_words`) and any reflex word with intent `wake`.
//!
//! Deliberately out of scope here (see the roadmap): HTTPS/mTLS, per-peer
//! credential rotation, discovery (peers are entered by hand), and any
//! authority over the peers' runtime state (a push never touches volumes,
//! playback, sessions or the release pointer).

use crate::store::Store;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

pub const FLEET_KEY: &str = "fleet";
/// `<prefix><peer id>` in the secrets table.
pub const PEER_TOKEN_PREFIX: &str = "fleet.";
pub const PEER_TOKEN_SUFFIX: &str = ".token";

/// One configured peer. Non-secret by construction; the token is separate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerNode {
    /// The peer's own stable id (`dot2-e`), which is what its `/status` build
    /// belongs to and what its history will name as the actor.
    pub id: String,
    /// `http://host:port` — the peer's management API base.
    pub base_url: String,
    pub added_ms: u64,
    #[serde(default)]
    pub last_ok_ms: u64,
    #[serde(default)]
    pub last_error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetConfigDoc {
    /// This node's own id, when the operator set one. Empty = derive it (see
    /// `self_id`).
    #[serde(default)]
    pub self_id: String,
    #[serde(default)]
    pub nodes: Vec<PeerNode>,
}

fn resolve_self_id(cfg: &FleetConfigDoc, device_hint: Option<&str>) -> (String, &'static str) {
    if !cfg.self_id.trim().is_empty() {
        return (cfg.self_id.trim().to_string(), "config");
    }
    if let Ok(v) = std::env::var("HM_NODE_ID") {
        if !v.trim().is_empty() {
            return (v.trim().to_string(), "env");
        }
    }
    if let Some(d) = device_hint {
        if !d.trim().is_empty() {
            return (d.trim().to_string(), "device");
        }
    }
    ("node".into(), "default")
}

fn peer_secret_name(id: &str) -> String {
    format!("{PEER_TOKEN_PREFIX}{id}{PEER_TOKEN_SUFFIX}")
}

/// Peer ids are stable ids and become SQLite keys and HTTP path segments:
/// keep them to the same alphabet the auth layer's device ids use.
pub fn valid_peer_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One peer's response to `/api/v0/fleet/wake`, carrying optional RTT-bounded
/// wall-clock metadata (`server_time_ms` vs `local_midpoint_ms`) so the arbiter
/// can normalize cross-unit wall-clock skew (#76 / #171).
#[derive(Debug, Clone)]
pub struct WakePeerReply {
    pub peer_id: String,
    pub claim: Option<serde_json::Value>,
    pub server_time_ms: Option<i64>,
    pub local_midpoint_ms: i64,
    pub rtt_ms: u64,
}

/// `http://host:port` or `http://host`. Everything else is refused: a peer URL
/// is dialled with this household's bearer token attached, so a typo must not
/// become an exfiltration path (`file://`, a userinfo segment, a path prefix,
/// or a link-local / cloud metadata address).
pub fn valid_base_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/').to_string();
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| "base_url must start with http:// or https://".to_string())?;
    if rest.is_empty() {
        return Err("base_url has no host".into());
    }
    if rest.contains('@') || rest.contains('?') || rest.contains('#') || rest.contains(' ') {
        return Err("base_url must not contain userinfo, query or fragment".into());
    }
    if rest.contains('/') {
        return Err("base_url is scheme://host[:port] only; no path".into());
    }
    crate::tool::http::validate_outbound_url(
        &url,
        crate::tool::http::OutboundPolicy::PublicOrLan {
            allow_loopback: true,
        },
    )
    .map_err(|e| format!("base_url rejected by outbound policy: {e}"))?;
    Ok(url)
}

pub struct FleetService {
    store: Arc<Store>,
    http: reqwest::Client,
}

impl FleetService {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let http = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::PublicOrLan {
                allow_loopback: true,
            },
        )
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            store,
            // One client; per-request timeouts because a ping and a push have
            // very different budgets.
            http,
        })
    }

    pub fn doc(&self) -> FleetConfigDoc {
        self.store
            .config_get(FLEET_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<FleetConfigDoc>(&raw).ok())
            .unwrap_or_default()
    }

    pub(crate) fn peer_endpoint(&self, id: &str) -> Option<(String, String)> {
        let node = self.doc().nodes.into_iter().find(|n| n.id == id)?;
        let token = self
            .store
            .secret_get(&peer_secret_name(id))
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())?;
        Some((node.base_url, token))
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    fn save(&self, doc: &FleetConfigDoc) -> Result<(), String> {
        let raw = serde_json::to_string(doc).map_err(|e| e.to_string())?;
        self.store
            .config_set(FLEET_KEY, &raw)
            .map_err(|e| e.to_string())
    }

    /// The id this node uses as the `X-Home-Mate-Source` of a push and in the
    /// console. Order: an explicit `self_id` → `HM_NODE_ID` → the single
    /// audio device this core is talking to → `"node"`.
    ///
    /// The last two matter on the speakers: the deployment already tells the
    /// gateway its id (`start_server.sh --device-id`), and a core with exactly
    /// one online speaker IS that speaker.
    pub fn self_id(&self, device_hint: Option<&str>) -> (String, &'static str) {
        resolve_self_id(&self.doc(), device_hint)
    }

    pub(crate) fn stable_self_id_on(c: &Connection) -> crate::store::Result<Option<String>> {
        let raw: Option<String> = c
            .query_row(
                "SELECT value FROM config_kv WHERE key=?1",
                [FLEET_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let doc = raw
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default();
        let (node, source) = resolve_self_id(&doc, None);
        Ok((source != "default").then_some(node))
    }

    pub fn set_self_id(&self, id: &str) -> Result<String, String> {
        let id = id.trim();
        if !id.is_empty() && !valid_peer_id(id) {
            return Err("self_id must match [A-Za-z0-9._-]{1,64}".into());
        }
        let mut cfg = self.doc();
        cfg.self_id = id.to_string();
        self.save(&cfg)?;
        Ok(cfg.self_id)
    }

    /// Console view: peers with `token_set`, never a token.
    pub fn snapshot(&self, self_id: &str, self_id_source: &str) -> Value {
        let cfg = self.doc();
        json!({
            "self_id": self_id,
            "self_id_source": self_id_source,
            "nodes": cfg.nodes.iter().map(|n| json!({
                "id": n.id,
                "base_url": n.base_url,
                "added_ms": n.added_ms,
                "last_ok_ms": n.last_ok_ms,
                "last_error": n.last_error,
                "token_set": self
                    .store
                    .secret_get(&peer_secret_name(&n.id))
                    .ok()
                    .flatten()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false),
            })).collect::<Vec<_>>(),
        })
    }

    /// Add or update a peer. `token` absent/empty keeps the stored one (the
    /// write-only convention every other secret in this codebase follows);
    /// `token_clear` removes it.
    pub fn upsert(
        &self,
        id: &str,
        base_url: &str,
        token: Option<&str>,
        token_clear: bool,
    ) -> Result<PeerNode, String> {
        let id = id.trim();
        if !valid_peer_id(id) {
            return Err("peer id must match [A-Za-z0-9._-]{1,64}".into());
        }
        let base_url = valid_base_url(base_url)?;
        let mut cfg = self.doc();
        let now = crate::id::now_ms();
        let node = match cfg.nodes.iter_mut().find(|n| n.id == id) {
            Some(n) => {
                n.base_url = base_url;
                n.clone()
            }
            None => {
                let n = PeerNode {
                    id: id.to_string(),
                    base_url,
                    added_ms: now,
                    last_ok_ms: 0,
                    last_error: String::new(),
                };
                cfg.nodes.push(n.clone());
                n
            }
        };
        self.save(&cfg)?;
        let secret = peer_secret_name(id);
        if token_clear {
            self.store
                .secret_delete(&secret)
                .map_err(|e| e.to_string())?;
        } else if let Some(t) = token.map(str::trim) {
            if !t.is_empty() {
                self.store
                    .secret_set(&secret, t)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(node)
    }

    pub fn remove(&self, id: &str) -> Result<bool, String> {
        let mut cfg = self.doc();
        let before = cfg.nodes.len();
        cfg.nodes.retain(|n| n.id != id);
        if cfg.nodes.len() == before {
            return Ok(false);
        }
        self.save(&cfg)?;
        self.store
            .secret_delete(&peer_secret_name(id))
            .map_err(|e| e.to_string())?;
        Ok(true)
    }

    fn record(&self, id: &str, ok: bool, error: &str) {
        let mut cfg = self.doc();
        if let Some(n) = cfg.nodes.iter_mut().find(|n| n.id == id) {
            if ok {
                n.last_ok_ms = crate::id::now_ms();
                n.last_error.clear();
            } else {
                n.last_error = error.to_string();
            }
            let _ = self.save(&cfg);
        }
    }

    fn token(&self, id: &str) -> Option<String> {
        self.store
            .secret_get(&peer_secret_name(id))
            .ok()
            .flatten()
            .filter(|t| !t.trim().is_empty())
    }

    /// `GET /api/v0/status` on the peer: reachability, its build id (the
    /// acceptance criterion: a ping returns the peer's build), and whether it
    /// is paired. Read-only on the peer.
    pub async fn ping(&self, ids: &[String]) -> Vec<Value> {
        let cfg = self.doc();
        let mut out = Vec::new();
        for id in ids {
            let Some(node) = cfg.nodes.iter().find(|n| &n.id == id) else {
                out.push(
                    json!({ "id": id, "ok": false, "error": "not a configured peer",
                                 "applied": [], "warnings": [] }),
                );
                continue;
            };
            let Some(token) = self.token(id) else {
                out.push(json!({ "id": id, "ok": false,
                                 "error": "no token stored for this peer",
                                 "applied": [], "warnings": [] }));
                continue;
            };
            let url = format!("{}/api/v0/status", node.base_url);
            let started = std::time::Instant::now();
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&token)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status();
                    let body = r.json::<Value>().await.unwrap_or(Value::Null);
                    let ms = started.elapsed().as_millis() as u64;
                    if status.is_success() {
                        self.record(id, true, "");
                        out.push(json!({
                            "id": id,
                            "ok": true,
                            "latency_ms": ms,
                            "build": body.pointer("/build/id").cloned().unwrap_or(Value::Null),
                            "version": body.pointer("/build/version").cloned().unwrap_or(Value::Null),
                            "product": body.get("product").cloned().unwrap_or(Value::Null),
                            "data_dir": body.get("data_dir").cloned().unwrap_or(Value::Null),
                            "auth_configured": body.pointer("/auth/configured").cloned().unwrap_or(Value::Null),
                            "gateway_devices": body.pointer("/gateway/devices").cloned().unwrap_or(Value::Null),
                        }));
                    } else {
                        let err = format!("HTTP {status}: {}", brief(&body));
                        self.record(id, false, &err);
                        out.push(
                            json!({ "id": id, "ok": false, "http_status": status.as_u16(),
                                         "latency_ms": ms, "error": err }),
                        );
                    }
                }
                Err(e) => {
                    let err = describe_reqwest(&e);
                    self.record(id, false, &err);
                    out.push(json!({ "id": id, "ok": false, "error": err }));
                }
            }
        }
        out
    }

    /// Publish a wake claim to every peer and collect how many answered.
    ///
    /// The endpoint is symmetric: the peer records our claim and answers with
    /// its OWN claim in the same response, so one round trip gives both sides
    /// the evidence they need to decide the same winner.
    ///
    /// CONCURRENT and bounded. A wake cannot wait for four unreachable peers
    /// one after another: every request is its own task with the caller's
    /// budget, so the worst case is one budget, not four. Failures are
    /// counted, never fatal — a node whose peers are unreachable answers
    /// alone, exactly as it did before arbitration existed.
    pub async fn wake_claim(
        &self,
        claim: &serde_json::Value,
        budget: std::time::Duration,
    ) -> (usize, Vec<WakePeerReply>) {
        let cfg = self.doc();
        if cfg.nodes.is_empty() {
            return (0, Vec::new());
        }
        let mut tasks = Vec::new();
        for node in cfg.nodes {
            // No token = this peer was never paired with us (or the operator
            // cleared it): nothing to ask, and the caller counts it as
            // unreachable evidence.
            let Some(token) = self.token(&node.id) else {
                continue;
            };
            let http = self.http.clone();
            let mut body = claim.clone();
            let peer_id = node.id.clone();
            tasks.push(tokio::spawn(async move {
                let url = format!("{}/api/v0/fleet/wake", node.base_url);
                let t0_ms = crate::id::now_ms() as i64;
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("sent_at_ms".into(), serde_json::json!(t0_ms));
                }
                let sent = http
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .timeout(budget)
                    .send()
                    .await;
                let t1_ms = crate::id::now_ms() as i64;
                match sent {
                    Ok(r) if r.status().is_success() => {
                        let answer = r
                            .json::<serde_json::Value>()
                            .await
                            .unwrap_or(serde_json::Value::Null);
                        let peer_claim = answer.get("claim").filter(|c| !c.is_null()).cloned();
                        let server_time_ms = answer.get("server_time_ms").and_then(|v| v.as_i64());
                        let rtt_ms = t1_ms.saturating_sub(t0_ms).max(0) as u64;
                        let midpoint_ms = t0_ms.saturating_add((rtt_ms / 2) as i64);
                        (
                            true,
                            Some(WakePeerReply {
                                peer_id,
                                claim: peer_claim,
                                server_time_ms,
                                local_midpoint_ms: midpoint_ms,
                                rtt_ms,
                            }),
                        )
                    }
                    _ => (false, None),
                }
            }));
        }
        let mut answered = 0usize;
        let mut replies = Vec::new();
        for t in tasks {
            if let Ok((ok, reply)) = t.await {
                if ok {
                    answered += 1;
                    if let Some(r) = reply {
                        replies.push(r);
                    }
                }
            }
        }
        (answered, replies)
    }

    /// Contract v2 #2 (2026-11-16): tell the peers what cue this device just
    /// spoke, so a neighbour can recognise its own microphone picking up
    /// "来了" as a cue and not as the person's question.
    ///
    /// Fire-and-forget: the audio path calls this and never waits. A house
    /// with no peers (or no runtime, as in unit tests) costs nothing; an
    /// unreachable peer is simply not told.
    pub fn broadcast_cue(self: &Arc<Self>, device: &str, phrase: &str, at_ms: u64) {
        let cfg = self.doc();
        if cfg.nodes.is_empty() || phrase.trim().is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = self.clone();
        let body = serde_json::json!({ "device": device, "phrase": phrase, "at_ms": at_ms });
        handle.spawn(async move {
            let mut tasks = Vec::new();
            for node in cfg.nodes {
                let Some(token) = this.token(&node.id) else {
                    continue;
                };
                let http = this.http.clone();
                let body = body.clone();
                tasks.push(tokio::spawn(async move {
                    let url = format!("{}/api/v0/fleet/cue", node.base_url);
                    let _ = http
                        .post(&url)
                        .bearer_auth(&token)
                        .json(&body)
                        .timeout(std::time::Duration::from_millis(1500))
                        .send()
                        .await;
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
        });
    }

    /// Wave 2: event-driven broadcast of lightweight active TTS/media playback
    /// metadata (`FleetPlaybackNotice`) so peers can suppress pure cross-room
    /// bleed self-wakes without continuous polling or PCM fingerprints.
    pub fn broadcast_playback_notice(
        self: &Arc<Self>,
        notice: &crate::sovereignty::FleetPlaybackNotice,
    ) {
        let cfg = self.doc();
        if cfg.nodes.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = self.clone();
        let body = serde_json::to_value(notice).unwrap_or(serde_json::Value::Null);
        handle.spawn(async move {
            let mut tasks = Vec::new();
            for node in cfg.nodes {
                let Some(token) = this.token(&node.id) else {
                    continue;
                };
                let http = this.http.clone();
                let body = body.clone();
                tasks.push(tokio::spawn(async move {
                    let url = format!("{}/api/v0/fleet/playback", node.base_url);
                    let _ = http
                        .post(&url)
                        .bearer_auth(&token)
                        .json(&body)
                        .timeout(std::time::Duration::from_millis(500))
                        .send()
                        .await;
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
        });
    }

    /// L4 (`#116` / `ARB-04`): replicate room session ownership commit, renewal,
    /// or release (`FleetOwnerNotice`) across paired household peers.
    pub fn broadcast_owner_lease(self: &Arc<Self>, notice: &crate::sovereignty::FleetOwnerNotice) {
        let cfg = self.doc();
        if cfg.nodes.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = self.clone();
        let is_release = !notice.active;
        let body = serde_json::to_value(notice).unwrap_or(serde_json::Value::Null);
        handle.spawn(async move {
            let mut tasks = Vec::new();
            for node in cfg.nodes {
                let Some(token) = this.token(&node.id) else {
                    continue;
                };
                let http = this.http.clone();
                let body = body.clone();
                let peer_id = node.id.clone();
                tasks.push(tokio::spawn(async move {
                    let url = format!("{}/api/v0/fleet/owner", node.base_url);
                    let attempts = if is_release { 2 } else { 1 };
                    for attempt in 0..attempts {
                        match http
                            .post(&url)
                            .bearer_auth(&token)
                            .json(&body)
                            .timeout(std::time::Duration::from_millis(500))
                            .send()
                            .await
                        {
                            Ok(r) if r.status().is_success() => break,
                            Ok(r) => {
                                tracing::warn!(
                                    peer = %peer_id,
                                    active = !is_release,
                                    attempt = attempt + 1,
                                    status = r.status().as_u16(),
                                    "fleet owner lease broadcast rejected"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    peer = %peer_id,
                                    active = !is_release,
                                    attempt = attempt + 1,
                                    error = %e,
                                    "fleet owner lease broadcast failed"
                                );
                            }
                        }
                        if is_release && attempt == 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
        });
    }

    /// Send a config document to the peers in `targets`.
    ///
    /// `doc` is the local export document (already built, already stripped of
    /// anything node-local by [`strip_node_local`]); `include_secrets` decides
    /// whether the caller put the credentials in it. `dry_run` never writes
    /// anything: not on the peer, not here — it only reports what WOULD be
    /// sent, so the answer is honest without a reachability guess.
    pub async fn push(
        &self,
        source: &str,
        doc: &Value,
        targets: &[String],
        include_secrets: bool,
        dry_run: bool,
    ) -> Vec<Value> {
        let cfg = self.doc();
        let mut out = Vec::new();
        if targets.is_empty() {
            return vec![json!({
                "id": "", "ok": false, "error": "targets is empty: name at least one peer",
                "applied": [], "warnings": [],
            })];
        }
        for id in targets {
            let Some(node) = cfg.nodes.iter().find(|n| &n.id == id) else {
                out.push(
                    json!({ "id": id, "ok": false, "error": "not a configured peer",
                                 "applied": [], "warnings": [] }),
                );
                continue;
            };
            if dry_run {
                out.push(json!({
                    "id": id,
                    "ok": true,
                    "dry_run": true,
                    "base_url": node.base_url,
                    // What would be written, in the same vocabulary the peer's
                    // import reports back, so a plan and a result are comparable.
                    "applied": planned_blocks(doc),
                    "warnings": secret_plan_warnings(doc, include_secrets),
                    "error": Value::Null,
                }));
                continue;
            }
            let Some(token) = self.token(id) else {
                out.push(json!({ "id": id, "ok": false,
                                 "error": "no token stored for this peer (add it again with its bearer token)",
                                 "applied": [], "warnings": [] }));
                continue;
            };
            let url = format!("{}/api/v0/config/import", node.base_url);
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&token)
                .header(crate::api::FLEET_SOURCE_HEADER, source)
                .json(doc)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status();
                    let body = r.json::<Value>().await.unwrap_or(Value::Null);
                    if status.is_success() {
                        self.record(id, true, "");
                        out.push(json!({
                            "id": id,
                            "ok": true,
                            "http_status": status.as_u16(),
                            "actor": body.get("actor").cloned().unwrap_or(Value::Null),
                            "applied": body.get("applied").cloned().unwrap_or_else(|| json!([])),
                            "warnings": body.get("warnings").cloned().unwrap_or_else(|| json!([])),
                            "error": Value::Null,
                        }));
                    } else {
                        let err = format!("HTTP {status}: {}", brief(&body));
                        self.record(id, false, &err);
                        out.push(
                            json!({ "id": id, "ok": false, "http_status": status.as_u16(),
                                         "applied": [], "warnings": [], "error": err }),
                        );
                    }
                }
                Err(e) => {
                    let err = describe_reqwest(&e);
                    self.record(id, false, &err);
                    out.push(
                        json!({ "id": id, "ok": false, "applied": [], "warnings": [],
                                     "error": err }),
                    );
                }
            }
        }
        out
    }
}

/// Remove everything that belongs to the SENDING node rather than to the
/// household. Called before a push, so the discipline holds even if a future
/// exporter starts carrying more per-device state.
///
/// - `mode.wake_words`: each speaker keeps its own wake word (user decision
///   2026-11-14). Removing it is not enough on its own — the receiver injects
///   its own list back (see `api::apply_config_import`), otherwise serde's
///   default (你好高达) would rename every target.
/// - `reflex.words` with intent `wake`: same reason, on the spotter's table.
/// - `unit_acoustic_profiles`: acoustic parameters (volume_cap, mix_gain,
///   vad_threshold) are measured per physical unit and never copied across units.
/// - Socket endpoints, contracts and backup provenance belong to the source node.
/// Node-local `mode` fields that belong to the physical unit and must never
/// be copied or overwritten during household v0 fleet push or import.
pub const NODE_LOCAL_MODE_KEYS: &[&str] = &[
    "wake_words",
    "kws_threshold",
    "kws_score",
    "media_trim_db",
    "tail_blind_ms",
    "vad",
    "duplex_requested",
    "keyword_interrupt",
    "barge_in",
    "barge_protect_ms",
    "asr_wake_fallback",
    "proactive_asr_wake",
];

pub fn strip_node_local(doc: &mut Value) {
    if let Some(obj) = doc.as_object_mut() {
        for key in [
            "unit_acoustic_profiles",
            "open_sockets",
            "open_sockets_source_node",
            "device_led_configs",
            "node",
            "privacy",
        ] {
            obj.remove(key);
        }
        for (_, child) in obj.iter_mut() {
            if let Some(child_obj) = child.as_object_mut() {
                child_obj.remove("api_key_env");
            }
        }
    }
    if let Some(mode) = doc.get_mut("mode").and_then(Value::as_object_mut) {
        for key in NODE_LOCAL_MODE_KEYS {
            mode.remove(*key);
        }
    }
    if let Some(words) = doc
        .get_mut("reflex")
        .and_then(|r| r.get_mut("words"))
        .and_then(Value::as_array_mut)
    {
        words.retain(|w| w.get("intent").and_then(Value::as_str) != Some("wake"));
    }
}

/// What a push would change on the peer, named the way the peer's import
/// reports it. Used by `dry_run` so a plan reads like a result.
///
/// NOTE (2026-11-15): every per-target result carries `applied` and `warnings`
/// as ARRAYS, including the early failures (unknown peer, no token). A console
/// that renders `r.applied.length` unmounts the whole card when a field is
/// missing — a peer added without a token was enough to blank the card.
pub fn planned_blocks(doc: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if doc.get("mode").is_some() {
        out.push("mode".to_string());
    }
    if let Some(devices) = doc.get("devices").and_then(Value::as_object) {
        for id in devices.keys() {
            out.push(format!("devices.{id}"));
        }
    }
    if let Some(services) = doc.get("services").and_then(Value::as_object) {
        for name in services.keys() {
            out.push(format!("services.{name}"));
        }
    }
    for block in [
        "music", "home", "weather", "news", "search", "openapi", "reflex", "audio_eq",
    ] {
        if doc.get(block).is_some() {
            out.push(block.to_string());
        }
    }
    out
}

fn secret_plan_warnings(doc: &Value, include_secrets: bool) -> Vec<String> {
    let mut out = Vec::new();
    let present = doc
        .get("secrets")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if include_secrets {
        if present.is_empty() {
            out.push(
                "include_secrets is on but this node stores no keys: the target keeps its own"
                    .into(),
            );
        } else {
            out.push(format!(
                "include_secrets: {} stored key(s) would be sent over plain HTTP: {}",
                present.len(),
                present.join(", ")
            ));
        }
    } else if !present.is_empty() {
        // Cannot happen through the API (the document is built with the same
        // flag); kept so a caller passing a prebuilt doc is told the truth.
        out.push("the document carries secrets but include_secrets is off".into());
    }
    out
}

fn brief(body: &Value) -> String {
    let s = body
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.to_string());
    s.chars().take(200).collect()
}

fn describe_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        format!("cannot connect: {e}")
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> Arc<FleetService> {
        FleetService::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    #[test]
    fn peer_tokens_are_sealed_and_never_in_the_snapshot() {
        let s = svc();
        s.upsert(
            "dot2-e",
            "http://192.168.1.69:8930",
            Some("hm1_secret"),
            false,
        )
        .unwrap();
        let raw = s.store.secret_raw("fleet.dot2-e.token").unwrap().unwrap();
        assert!(raw.starts_with(crate::vault::SEALED_PREFIX), "{raw}");
        assert!(!raw.contains("hm1_secret"));
        let snap = s.snapshot("dot2-d", "config");
        let text = snap.to_string();
        assert!(text.contains("\"token_set\":true"), "{text}");
        assert!(!text.contains("hm1_secret"), "snapshot leaked the token");
    }

    #[test]
    fn upsert_keeps_a_stored_token_and_clear_removes_it() {
        let s = svc();
        s.upsert("dot2-e", "http://192.168.1.69:8930", Some("hm1_a"), false)
            .unwrap();
        // An edit that only changes the URL must not wipe the credential.
        s.upsert("dot2-e", "http://192.168.1.70:8930", None, false)
            .unwrap();
        assert_eq!(s.token("dot2-e").unwrap(), "hm1_a");
        assert_eq!(s.doc().nodes[0].base_url, "http://192.168.1.70:8930");
        s.upsert("dot2-e", "http://192.168.1.70:8930", None, true)
            .unwrap();
        assert!(s.token("dot2-e").is_none());
    }

    #[test]
    fn remove_drops_the_node_and_its_token() {
        let s = svc();
        s.upsert("dot2-e", "http://192.168.1.69:8930", Some("hm1_a"), false)
            .unwrap();
        assert!(s.remove("dot2-e").unwrap());
        assert!(s.doc().nodes.is_empty());
        assert!(s.store.secret_raw("fleet.dot2-e.token").unwrap().is_none());
        assert!(
            !s.remove("dot2-e").unwrap(),
            "removing twice reports absent"
        );
    }

    #[test]
    fn base_url_and_id_validation_refuses_dialing_somewhere_else() {
        assert!(valid_base_url("http://192.168.1.69:8930/").is_ok());
        assert_eq!(
            valid_base_url("https://box.local").unwrap(),
            "https://box.local"
        );
        for bad in [
            "ftp://x",
            "http://user:pw@host:8930",
            "http://host:8930/api/v0",
            "http://host:8930?x=1",
            "http://169.254.169.254",
            "http://169.254.1.1:8930",
            "http://[fe80::1]:8930",
            "http://metadata.google.internal",
            "http://0.0.0.0:8930",
            "",
        ] {
            assert!(valid_base_url(bad).is_err(), "{bad} must be refused");
        }
        assert!(valid_peer_id("dot2-e"));
        assert!(!valid_peer_id("dot2 e"));
        assert!(!valid_peer_id(""));
    }

    #[test]
    fn self_id_prefers_config_then_env_then_the_device() {
        let s = svc();
        // No config, no env var in this test process → the device hint wins.
        let (id, source) = s.self_id(Some("dot2-d"));
        assert_eq!((id.as_str(), source), ("dot2-d", "device"));
        let (id, source) = s.self_id(None);
        assert_eq!((id.as_str(), source), ("node", "default"));
        s.set_self_id("dot2-d").unwrap();
        let (id, source) = s.self_id(Some("dot2-e"));
        assert_eq!((id.as_str(), source), ("dot2-d", "config"));
    }

    #[test]
    fn strip_removes_wake_words_and_wake_reflex_words_only() {
        let mut doc = json!({
            "mode": { "wake_words": ["你好建国"], "media_trim_db": -6.0, "follow_window_ms": 9000 },
            "reflex": { "words": [
                { "text": "你好建国", "tokens": "n ǐ h ǎo", "intent": "wake", "enabled": true },
                { "text": "停止播放", "tokens": "t íng", "intent": "retreat", "enabled": true },
            ]},
            "services": { "llm": { "provider": "deepseek" } },
        });
        strip_node_local(&mut doc);
        assert!(doc.pointer("/mode/wake_words").is_none());
        assert!(doc.pointer("/mode/media_trim_db").is_none());
        assert_eq!(doc["mode"]["follow_window_ms"], 9000);
        let words = doc["reflex"]["words"].as_array().unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0]["text"], "停止播放");
        // The rest of the document is untouched.
        assert_eq!(doc["services"]["llm"]["provider"], "deepseek");
    }

    #[test]
    fn a_dry_run_plan_names_the_blocks_a_result_would_name() {
        let doc = json!({
            "mode": {}, "devices": { "dot2-d": {} },
            "services": { "llm": {}, "tts": {} },
            "audio_eq": {}, "reflex": {},
        });
        let blocks = planned_blocks(&doc);
        assert_eq!(
            blocks,
            vec![
                "mode",
                "devices.dot2-d",
                "services.llm",
                "services.tts",
                "reflex",
                "audio_eq"
            ]
        );
    }
}
