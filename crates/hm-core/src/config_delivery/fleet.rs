//! Household enrollment invitations and sequential fleet orchestration (#110).
use super::*;
use crate::fleet::FleetService;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;

const DEFAULT_INVITATION_TTL_MS: u64 = 10 * 60 * 1000;
const MAX_INVITATION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const READBACK_TIMEOUT_MS: u64 = 5_000;
/// Blocks that dispatch `ConfigBlock::Kws` (`reflex`, `mode`, `node`) wait up
/// to `KWS_APPLY_TIMEOUT` (25s) on the target Core while `hm-kws` reloads on
/// Dot2/LX06; keep fleet readback strictly above that budget.
const KWS_READBACK_TIMEOUT_MS: u64 = 30_000;
const READBACK_POLL_MS: u64 = 20;

pub(super) fn readback_timeout_for(required_blocks: &[String]) -> Duration {
    let may_touch_kws = required_blocks.is_empty()
        || required_blocks
            .iter()
            .any(|b| matches!(b.as_str(), "reflex" | "mode" | "node"));
    Duration::from_millis(if may_touch_kws {
        KWS_READBACK_TIMEOUT_MS
    } else {
        READBACK_TIMEOUT_MS
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueInvitationRequest {
    pub expected_node_id: String,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedeemInvitationRequest {
    pub token: String,
    pub node_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
    pub token: String,
    #[serde(default)]
    pub issuer_node_id: Option<String>,
    #[serde(default)]
    pub issuer_base_url: Option<String>,
    #[serde(default)]
    pub issuer_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetPreflightRequest {
    pub package: Value,
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    #[serde(default)]
    pub sanitize_family: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetTargetPlan {
    pub node_id: String,
    pub plan_id: String,
    pub plan_hash: String,
    pub expected_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRunRequest {
    #[serde(default)]
    pub run_id: Option<String>,
    pub package: Value,
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    #[serde(default)]
    pub sanitize_family: bool,
    #[serde(default)]
    pub plans: Option<Vec<FleetTargetPlan>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetRollbackRequest {
    #[serde(default)]
    pub expected_revision: Option<u64>,
    #[serde(default = "default_true")]
    pub commit: bool,
}

fn default_true() -> bool {
    true
}

fn token_hash(token: &str) -> Result<String> {
    digest(&json!(token))
}

pub fn issue_invitation(delivery: &ConfigDelivery, req: IssueInvitationRequest) -> Result<Value> {
    let expected = req.expected_node_id.trim();
    if !crate::fleet::valid_peer_id(expected) {
        return Err(DeliveryError::new(
            "NODE_ID_REQUIRED",
            "expected_node_id must match [A-Za-z0-9._-]{1,64}",
        ));
    }
    let ttl_ms = req.ttl_ms.unwrap_or(DEFAULT_INVITATION_TTL_MS);
    if !(1_000..=MAX_INVITATION_TTL_MS).contains(&ttl_ms) {
        return Err(DeliveryError::new(
            "INVALID_FIELD_VALUE",
            "ttl_ms must be between 1000 and 86400000",
        ));
    }
    let now = crate::id::now_ms();
    let expires_ms = now.saturating_add(ttl_ms);
    let invitation_id = uuid()?;
    let raw_token = format!("{}{}", uuid()?, uuid()?);
    let hash = token_hash(&raw_token)?;

    delivery.store.with(|c| {
        let tx = c.unchecked_transaction()?;
        let (epoch, household, self_node): (i64, String, String) = tx.query_row(
            "SELECT data_epoch, household_id, node_id FROM config_head WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if household.is_empty() {
            return Ok(Err(DeliveryError::conflict(
                "ENROLLMENT_REQUIRED",
                "enroll this issuer node before issuing household invitations",
            )));
        }
        if expected == self_node {
            return Ok(Err(DeliveryError::new(
                "INVALID_INVITATION_TARGET",
                "cannot issue a household invitation for the issuer's own node_id",
            )));
        }
        let generation: i64 = tx.query_row(
            "SELECT COALESCE(MAX(membership_generation), 0) + 1 FROM config_household_invitations WHERE data_epoch=?1 AND household_id=?2",
            params![epoch, household],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO config_household_invitations(invitation_id,data_epoch,household_id,expected_node_id,token_hash,membership_generation,created_ms,expires_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![invitation_id, epoch, household, expected, hash, generation, now as i64, expires_ms as i64],
        )?;
        tx.commit()?;
        Ok(Ok(json!({
            "invitation_id": invitation_id,
            "household_id": household,
            "expected_node_id": expected,
            "token": raw_token,
            "membership_generation": generation as u64,
            "data_epoch": epoch as u64,
            "created_ms": now,
            "expires_ms": expires_ms,
        })))
    }).map_err(DeliveryError::internal)?
}

pub fn redeem_invitation(delivery: &ConfigDelivery, req: RedeemInvitationRequest) -> Result<Value> {
    let node_id = req.node_id.trim();
    if !crate::fleet::valid_peer_id(node_id) {
        return Err(DeliveryError::new(
            "NODE_ID_REQUIRED",
            "valid node_id is required to redeem invitation",
        ));
    }
    let hash = token_hash(req.token.trim())?;
    let now = crate::id::now_ms();
    delivery.store.with(|c| {
        let tx = c.unchecked_transaction()?;
        let epoch: i64 = tx.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| r.get(0))?;
        let row: Option<(String, String, String, i64, i64, Option<i64>)> = tx.query_row(
            "SELECT invitation_id, household_id, expected_node_id, membership_generation, expires_ms, consumed_ms FROM config_household_invitations WHERE token_hash=?1 AND data_epoch=?2",
            params![hash, epoch],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        ).optional()?;
        let Some((invitation_id, household_id, expected_node_id, generation, expires_ms, consumed_ms)) = row else {
            return Ok(Err(DeliveryError::new("INVALID_INVITATION", "invitation token is invalid or reset")));
        };
        if consumed_ms.is_some() {
            return Ok(Err(DeliveryError::conflict("INVITATION_ALREADY_USED", "invitation token has already been consumed")));
        }
        if (now as i64) > expires_ms {
            return Ok(Err(DeliveryError::conflict("INVITATION_EXPIRED", "invitation token has expired")));
        }
        if expected_node_id != node_id {
            return Ok(Err(DeliveryError::conflict(
                "TARGET_IDENTITY_MISMATCH",
                format!("invitation is bound to '{expected_node_id}', not '{node_id}'"),
            )));
        }
        let changed = tx.execute(
            "UPDATE config_household_invitations SET consumed_ms=?1, consumed_by_node_id=?2 WHERE invitation_id=?3 AND consumed_ms IS NULL",
            params![now as i64, node_id, invitation_id],
        )?;
        if changed != 1 {
            return Ok(Err(DeliveryError::conflict("INVITATION_ALREADY_USED", "invitation token has already been consumed")));
        }
        tx.commit()?;
        Ok(Ok(json!({
            "invitation_id": invitation_id,
            "household_id": household_id,
            "expected_node_id": expected_node_id,
            "membership_generation": generation as u64,
        })))
    }).map_err(DeliveryError::internal)?
}

pub async fn accept_invitation(
    delivery: &ConfigDelivery,
    fleet: &FleetService,
    req: JoinRequest,
) -> Result<Value> {
    let (self_id, source) = fleet.self_id(None);
    if source == "default" || !crate::fleet::valid_peer_id(&self_id) {
        return Err(DeliveryError::conflict(
            "NODE_ID_REQUIRED",
            "configure this node's stable identity before joining a household",
        ));
    }

    let verified = if req.issuer_node_id.is_some() || req.issuer_base_url.is_some() {
        let (base_url, bearer) = if let Some(issuer_id) = req.issuer_node_id.as_deref() {
            let (url, stored_token) = fleet.peer_endpoint(issuer_id).ok_or_else(|| {
                DeliveryError::conflict(
                    "PEER_NOT_CONFIGURED",
                    format!(
                        "issuer peer '{issuer_id}' is not configured with a token in FleetService"
                    ),
                )
            })?;
            let url = req
                .issuer_base_url
                .as_deref()
                .map(crate::fleet::valid_base_url)
                .transpose()
                .map_err(|e| DeliveryError::new("INVALID_FIELD_VALUE", e))?
                .unwrap_or(url);
            let token = req
                .issuer_token
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .unwrap_or(stored_token);
            (url, token)
        } else {
            let url = crate::fleet::valid_base_url(req.issuer_base_url.as_deref().unwrap_or(""))
                .map_err(|e| DeliveryError::new("INVALID_FIELD_VALUE", e))?;
            let token = req
                .issuer_token
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    DeliveryError::new(
                        "PAIRING_REQUIRED",
                        "issuer_token is required when joining via issuer_base_url without FleetService peer",
                    )
                })?
                .to_string();
            (url, token)
        };
        let resp = fleet
            .http()
            .post(format!(
                "{base_url}/api/v1/config/household/invitations/redeem"
            ))
            .bearer_auth(&bearer)
            .timeout(Duration::from_secs(5))
            .json(&json!({ "token": req.token, "node_id": self_id }))
            .send()
            .await
            .map_err(|e| {
                DeliveryError::conflict("PEER_OFFLINE", format!("failed to contact issuer: {e}"))
            })?;
        parse_peer_json(resp).await?
    } else {
        redeem_invitation(
            delivery,
            RedeemInvitationRequest {
                token: req.token,
                node_id: self_id.clone(),
            },
        )?
    };

    let household_id = verified["household_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DeliveryError::internal("issuer response missing household_id"))?
        .to_string();
    let expected_node_id = verified["expected_node_id"].as_str().unwrap_or_default();
    if expected_node_id != self_id {
        return Err(DeliveryError::conflict(
            "TARGET_IDENTITY_MISMATCH",
            format!("invitation is bound to '{expected_node_id}', not '{self_id}'"),
        ));
    }

    delivery.store.with(|c| {
        let tx = c.unchecked_transaction()?;
        let (current_household, current_node): (String, String) = tx.query_row(
            "SELECT household_id, node_id FROM config_head WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !current_household.is_empty() && current_household != household_id {
            return Ok(Err(DeliveryError::conflict(
                "HOUSEHOLD_CONFLICT",
                "node is already enrolled in a different household; factory reset before joining another",
            )));
        }
        if !current_node.is_empty() && current_node != self_id {
            return Ok(Err(DeliveryError::conflict(
                "TARGET_IDENTITY_CHANGED",
                "enrolled node identity is immutable",
            )));
        }
        tx.execute(
            "UPDATE config_head SET household_id=?1, node_id=?2, revision=revision+1 WHERE id=1",
            params![household_id, self_id],
        )?;
        tx.commit()?;
        Ok(Ok(json!({
            "household_id": household_id,
            "node_id": self_id,
            "enrolled": true,
            "invitation_id": verified["invitation_id"],
            "membership_generation": verified["membership_generation"],
        })))
    }).map_err(DeliveryError::internal)?
}

fn order_targets(targets: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut has_dot2d = false;
    let mut rest = Vec::new();
    for raw in targets {
        let id = raw.trim().to_string();
        if !crate::fleet::valid_peer_id(&id) {
            return Err(DeliveryError::new(
                "INVALID_TARGET",
                format!("invalid fleet target node_id: '{id}'"),
            ));
        }
        if !seen.insert(id.clone()) {
            return Err(DeliveryError::new(
                "DUPLICATE_TARGET",
                format!("duplicate fleet target node_id: '{id}'"),
            ));
        }
        if id == "dot2-d" {
            has_dot2d = true;
        } else {
            rest.push(id);
        }
    }
    if seen.is_empty() {
        return Err(DeliveryError::new(
            "NO_TARGETS",
            "at least one fleet target is required",
        ));
    }
    let mut ordered = Vec::with_capacity(seen.len());
    if has_dot2d {
        ordered.push("dot2-d".to_string());
    }
    ordered.extend(rest);
    Ok(ordered)
}

fn resolve_targets(fleet: &FleetService, requested: Option<Vec<String>>) -> Result<Vec<String>> {
    let raw = match requested {
        Some(list) => list,
        None => fleet.doc().nodes.into_iter().map(|n| n.id).collect(),
    };
    order_targets(raw)
}

fn prepare_family_package(
    raw: Value,
    local_state: &Value,
    sanitize_family: bool,
) -> Result<(Package, Value)> {
    let (mut pkg, report) =
        v1::convert_for_purpose(raw, local_state, Purpose::FamilyPolicy, sanitize_family)?;
    if pkg.purpose != Purpose::FamilyPolicy {
        return Err(DeliveryError::new(
            "FORBIDDEN_SCOPE",
            "fleet rollout requires a family_policy package",
        ));
    }
    if pkg.target.node_id.is_some() {
        return Err(DeliveryError::new(
            "TARGET_MISMATCH",
            "family_policy package must not set target.node_id",
        ));
    }
    for block in &pkg.blocks {
        for op in &block.operations {
            let val = match op {
                Operation::Set { value, .. } | Operation::Replace { value, .. } => Some(value),
                Operation::Clear { .. } | Operation::Preserve { .. } => None,
            };
            inventory::validate_path(op.path(), &Purpose::FamilyPolicy, val)?;
        }
    }
    let household = local_state["target"]["household_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            DeliveryError::conflict(
                "ENROLLMENT_REQUIRED",
                "enroll this node in a household before fleet operations",
            )
        })?;
    if pkg.target.household_id.is_empty() {
        pkg.target.household_id = household.to_string();
    } else if pkg.target.household_id != household {
        return Err(DeliveryError::new(
            "TARGET_MISMATCH",
            "package target.household_id does not match local household",
        ));
    }
    pkg.content_hash = Some(pkg.hash()?);
    Ok((pkg, report))
}

async fn parse_peer_json(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| DeliveryError::conflict("PEER_IO_ERROR", e.to_string()))?;
    let val: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "detail": text }));
    if status >= 400 {
        let code = val["error"].as_str().unwrap_or("PEER_REQUEST_FAILED");
        let detail = val["detail"]
            .as_str()
            .unwrap_or("peer rejected request")
            .to_string();
        return Err(DeliveryError {
            code: std::borrow::Cow::Owned(code.to_string()),
            detail,
            pointer: val["pointer"].as_str().map(str::to_string),
            status,
        });
    }
    Ok(val)
}

enum TargetTransport<'a> {
    Local {
        delivery: &'a ConfigDelivery,
        principal: &'a str,
    },
    Remote {
        http: &'a reqwest::Client,
        base_url: String,
        token: String,
    },
}

impl<'a> TargetTransport<'a> {
    fn resolve(
        node_id: &str,
        self_id: &str,
        delivery: &'a ConfigDelivery,
        fleet: &'a FleetService,
        principal: &'a str,
    ) -> Result<(Self, String)> {
        if node_id == self_id {
            return Ok((
                Self::Local {
                    delivery,
                    principal,
                },
                "local".to_string(),
            ));
        }
        let (base_url, token) = fleet.peer_endpoint(node_id).ok_or_else(|| {
            DeliveryError::conflict(
                "PEER_NOT_CONFIGURED",
                format!("peer '{node_id}' is not registered with a bearer token"),
            )
        })?;
        Ok((
            Self::Remote {
                http: fleet.http(),
                base_url: base_url.clone(),
                token,
            },
            base_url,
        ))
    }

    async fn get_state(&self) -> Result<Value> {
        match self {
            Self::Local {
                delivery,
                principal,
            } => delivery.state(principal),
            Self::Remote {
                http,
                base_url,
                token,
            } => {
                let resp = http
                    .get(format!("{base_url}/api/v1/config/state"))
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .map_err(|e| DeliveryError::conflict("PEER_OFFLINE", e.to_string()))?;
                parse_peer_json(resp).await
            }
        }
    }

    async fn preflight(&self, pkg: &Package) -> Result<Value> {
        match self {
            Self::Local {
                delivery,
                principal,
            } => delivery.preflight(principal, pkg.clone(), json!([])),
            Self::Remote {
                http,
                base_url,
                token,
            } => {
                let resp = http
                    .post(format!("{base_url}/api/v1/config/preflight"))
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(5))
                    .json(&json!({ "package": pkg }))
                    .send()
                    .await
                    .map_err(|e| DeliveryError::conflict("PEER_OFFLINE", e.to_string()))?;
                parse_peer_json(resp).await
            }
        }
    }

    async fn commit(&self, commit: &Commit) -> Result<Value> {
        match self {
            Self::Local {
                delivery,
                principal,
            } => delivery.commit(principal, commit),
            Self::Remote {
                http,
                base_url,
                token,
            } => {
                let resp = http
                    .post(format!("{base_url}/api/v1/config/applications"))
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(5))
                    .json(&json!({
                        "plan_id": commit.plan_id,
                        "plan_hash": commit.plan_hash,
                        "expected_revision": commit.expected_revision,
                        "application_id": commit.application_id,
                    }))
                    .send()
                    .await
                    .map_err(|e| DeliveryError::conflict("PEER_OFFLINE", e.to_string()))?;
                parse_peer_json(resp).await
            }
        }
    }

    async fn readback(&self, app_id: &str) -> Result<Value> {
        match self {
            Self::Local {
                delivery,
                principal,
            } => delivery.readback(principal, app_id),
            Self::Remote {
                http,
                base_url,
                token,
            } => {
                let resp = http
                    .get(format!(
                        "{base_url}/api/v1/config/applications/{app_id}/readback"
                    ))
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .map_err(|e| DeliveryError::conflict("PEER_OFFLINE", e.to_string()))?;
                parse_peer_json(resp).await
            }
        }
    }

    async fn rollback_preflight(&self, app_id: &str, expected_revision: u64) -> Result<Value> {
        match self {
            Self::Local {
                delivery,
                principal,
            } => delivery.rollback_preflight(principal, app_id, expected_revision),
            Self::Remote {
                http,
                base_url,
                token,
            } => {
                let resp = http
                    .post(format!(
                        "{base_url}/api/v1/config/applications/{app_id}/rollback-preflight"
                    ))
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(5))
                    .json(&json!({ "expected_revision": expected_revision }))
                    .send()
                    .await
                    .map_err(|e| DeliveryError::conflict("PEER_OFFLINE", e.to_string()))?;
                parse_peer_json(resp).await
            }
        }
    }
}

fn per_target_package(base: &Package, household_id: &str) -> Result<Package> {
    let mut pkg = base.clone();
    pkg.package_id = uuid()?;
    pkg.purpose = Purpose::FamilyPolicy;
    pkg.target = Target {
        household_id: household_id.to_string(),
        node_id: None,
    };
    pkg.content_hash = None;
    pkg.content_hash = Some(pkg.hash()?);
    Ok(pkg)
}

pub async fn preflight_fleet(
    delivery: &ConfigDelivery,
    fleet: &FleetService,
    principal: &str,
    req: FleetPreflightRequest,
) -> Result<Value> {
    let local_state = delivery.state(principal)?;
    let (pkg, migration_report) =
        prepare_family_package(req.package, &local_state, req.sanitize_family)?;
    let household_id = pkg.target.household_id.clone();
    let ordered = resolve_targets(fleet, req.targets)?;
    let (self_id, _) = fleet.self_id(None);

    let mut targets_out = Vec::with_capacity(ordered.len());
    let mut all_ok = true;

    for node_id in &ordered {
        let entry = match TargetTransport::resolve(node_id, &self_id, delivery, fleet, principal) {
            Ok((transport, _)) => match transport.get_state().await {
                Ok(t_state) => {
                    if t_state["target"]["enrolled"] != true
                        || t_state["target"]["household_id"].as_str() != Some(household_id.as_str())
                    {
                        all_ok = false;
                        json!({
                            "node_id": node_id,
                            "ok": false,
                            "error": { "code": "TARGET_MISMATCH", "detail": "target node is not enrolled in this household" }
                        })
                    } else {
                        let target_pkg = per_target_package(&pkg, &household_id)?;
                        match transport.preflight(&target_pkg).await {
                            Ok(pf) => json!({
                                "node_id": node_id,
                                "ok": true,
                                "plan_id": pf["plan_id"],
                                "plan_hash": pf["plan_hash"],
                                "expected_revision": pf["expected_revision"],
                                "diff": pf["diff"],
                                "blocked": pf["blocked"],
                            }),
                            Err(e) => {
                                all_ok = false;
                                json!({
                                    "node_id": node_id,
                                    "ok": false,
                                    "error": { "code": e.code, "detail": e.detail, "pointer": e.pointer }
                                })
                            }
                        }
                    }
                }
                Err(e) => {
                    all_ok = false;
                    json!({
                        "node_id": node_id,
                        "ok": false,
                        "error": { "code": e.code, "detail": e.detail }
                    })
                }
            },
            Err(e) => {
                all_ok = false;
                json!({
                    "node_id": node_id,
                    "ok": false,
                    "error": { "code": e.code, "detail": e.detail }
                })
            }
        };
        targets_out.push(entry);
    }

    Ok(json!({
        "household_id": household_id,
        "ordered_targets": ordered,
        "all_ok": all_ok,
        "migration_report": migration_report,
        "package": pkg,
        "targets": targets_out,
    }))
}

fn error_json(e: &DeliveryError) -> Value {
    json!({
        "error": e.code,
        "detail": e.detail,
        "pointer": e.pointer,
        "status": e.status,
    })
}

async fn poll_required_readback(
    transport: &TargetTransport<'_>,
    app_id: &str,
    required_blocks: &[String],
) -> std::result::Result<Value, (DeliveryError, Option<Value>)> {
    let deadline = tokio::time::Instant::now() + readback_timeout_for(required_blocks);
    loop {
        let rb = transport.readback(app_id).await.map_err(|e| (e, None))?;
        let pub_state = rb["publication"]["state"].as_str().unwrap_or("unknown");
        let publications = rb["publications"].as_array();
        if pub_state == "failed" {
            return Err((
                DeliveryError::conflict(
                    "PUBLICATION_FAILED",
                    "target application publication failed",
                ),
                Some(rb),
            ));
        }
        let mut any_failed_or_unknown = false;
        let mut all_applied = true;
        for name in required_blocks {
            let b_state = publications
                .and_then(|arr| {
                    arr.iter()
                        .find(|p| p["block"].as_str() == Some(name.as_str()))
                })
                .and_then(|b| b["state"].as_str())
                .unwrap_or("unknown");
            if b_state == "failed" {
                any_failed_or_unknown = true;
                all_applied = false;
                break;
            }
            if b_state != "applied" {
                all_applied = false;
                if pub_state == "applied" && b_state == "unknown" {
                    any_failed_or_unknown = true;
                    break;
                }
            }
        }
        if any_failed_or_unknown {
            return Err((
                DeliveryError::conflict(
                    "REQUIRED_BLOCK_FAILED",
                    "one or more required blocks failed or reported unknown state",
                ),
                Some(rb),
            ));
        }
        if pub_state == "applied" && all_applied {
            return Ok(rb);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err((
                DeliveryError::conflict(
                    "READBACK_TIMEOUT",
                    "timed out waiting for target readback confirmation",
                ),
                Some(rb),
            ));
        }
        tokio::time::sleep(Duration::from_millis(READBACK_POLL_MS)).await;
    }
}

async fn execute_fleet_targets(
    delivery: &ConfigDelivery,
    fleet: &FleetService,
    principal: &str,
    run_id: &str,
    household_id: &str,
    pkg: &Package,
) -> Result<()> {
    let (self_id, _) = fleet.self_id(None);
    let required_blocks: Vec<String> = pkg.blocks.iter().map(|b| b.name.clone()).collect();

    let targets: Vec<(String, i64, String, String, u64, String, String)> = delivery
        .store
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT node_id, ord, plan_id, plan_hash, expected_revision, application_id, state FROM config_fleet_targets WHERE run_id=?1 ORDER BY ord ASC",
            )?;
            let rows = stmt
                .query_map([run_id], |r| {
                    let rev: i64 = r.get(4)?;
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        rev as u64,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .map_err(DeliveryError::internal)?;

    for (node_id, ord, mut plan_id, mut plan_hash, mut expected_rev, app_id, current_state) in
        targets
    {
        if current_state == "applied" {
            continue;
        }
        let now = crate::id::now_ms();
        delivery
            .store
            .with(|c| {
                c.execute(
                    "UPDATE config_fleet_targets SET state='committing', updated_ms=?1 WHERE run_id=?2 AND node_id=?3",
                    params![now as i64, run_id, node_id],
                )?;
                Ok(())
            })
            .map_err(DeliveryError::internal)?;

        let step_result: std::result::Result<Value, (DeliveryError, Option<Value>)> = async {
            let (transport, _) =
                TargetTransport::resolve(&node_id, &self_id, delivery, fleet, principal)
                    .map_err(|e| (e, None))?;

            // Idempotent resume: if this application_id already exists on the target, poll it!
            if let Ok(existing_rb) = transport.readback(&app_id).await {
                if existing_rb["publication"]["state"] == "applied" {
                    return Ok(existing_rb);
                }
                if existing_rb["publication"]["state"] == "pending"
                    || existing_rb["publication"]["state"] == "applying"
                {
                    return poll_required_readback(&transport, &app_id, &required_blocks).await;
                }
            }

            if plan_id.is_empty() || plan_hash.is_empty() {
                let t_state = transport.get_state().await.map_err(|e| (e, None))?;
                if t_state["target"]["enrolled"] != true
                    || t_state["target"]["household_id"].as_str() != Some(household_id)
                {
                    return Err((
                        DeliveryError::conflict(
                            "TARGET_MISMATCH",
                            "target node is not enrolled in this household",
                        ),
                        None,
                    ));
                }
                let target_pkg = per_target_package(pkg, household_id).map_err(|e| (e, None))?;
                let pf = transport
                    .preflight(&target_pkg)
                    .await
                    .map_err(|e| (e, None))?;
                plan_id = pf["plan_id"].as_str().unwrap_or_default().to_string();
                plan_hash = pf["plan_hash"].as_str().unwrap_or_default().to_string();
                expected_rev = pf["expected_revision"].as_u64().unwrap_or(0);
                let pf_now = crate::id::now_ms();
                delivery
                    .store
                    .with(|c| {
                        c.execute(
                            "UPDATE config_fleet_targets SET plan_id=?1, plan_hash=?2, expected_revision=?3, updated_ms=?4 WHERE run_id=?5 AND node_id=?6",
                            params![plan_id, plan_hash, expected_rev as i64, pf_now as i64, run_id, node_id],
                        )?;
                        Ok(())
                    })
                    .map_err(|e| (DeliveryError::internal(e), None))?;
            }

            let commit = Commit {
                plan_id: plan_id.clone(),
                plan_hash: plan_hash.clone(),
                expected_revision: expected_rev,
                application_id: app_id.clone(),
            };
            transport.commit(&commit).await.map_err(|e| (e, None))?;
            poll_required_readback(&transport, &app_id, &required_blocks).await
        }
        .await;

        let done_ms = crate::id::now_ms();
        match step_result {
            Ok(rb) => {
                let rb_str = serde_json::to_string(&rb)?;
                delivery
                    .store
                    .with(|c| {
                        c.execute(
                            "UPDATE config_fleet_targets SET state='applied', result_json=?1, updated_ms=?2 WHERE run_id=?3 AND node_id=?4",
                            params![rb_str, done_ms as i64, run_id, node_id],
                        )?;
                        Ok(())
                    })
                    .map_err(DeliveryError::internal)?;
            }
            Err((err, maybe_rb)) => {
                let err_val = error_json(&err);
                let result_val = json!({
                    "error": err_val,
                    "readback": maybe_rb,
                });
                let result_str = serde_json::to_string(&result_val)?;
                let err_str = serde_json::to_string(&err_val)?;
                delivery
                    .store
                    .with(|c| {
                        let tx = c.unchecked_transaction()?;
                        tx.execute(
                            "UPDATE config_fleet_targets SET state='failed', result_json=?1, updated_ms=?2 WHERE run_id=?3 AND node_id=?4",
                            params![result_str, done_ms as i64, run_id, node_id],
                        )?;
                        tx.execute(
                            "UPDATE config_fleet_targets SET state='skipped', updated_ms=?1 WHERE run_id=?2 AND ord>?3 AND state!='applied'",
                            params![done_ms as i64, run_id, ord],
                        )?;
                        tx.execute(
                            "UPDATE config_fleet_runs SET state='failed', error_json=?1, updated_ms=?2 WHERE run_id=?3",
                            params![err_str, done_ms as i64, run_id],
                        )?;
                        tx.commit()?;
                        Ok(())
                    })
                    .map_err(DeliveryError::internal)?;
                return Ok(());
            }
        }
    }

    let finish_ms = crate::id::now_ms();
    delivery
        .store
        .with(|c| {
            c.execute(
                "UPDATE config_fleet_runs SET state='applied', error_json=NULL, updated_ms=?1 WHERE run_id=?2",
                params![finish_ms as i64, run_id],
            )?;
            Ok(())
        })
        .map_err(DeliveryError::internal)?;
    Ok(())
}

pub async fn commit_fleet(
    delivery: &ConfigDelivery,
    fleet: &FleetService,
    principal: &str,
    req: FleetRunRequest,
) -> Result<Value> {
    let run_id = match req.run_id {
        Some(id) if model::valid_uuid(&id) => id,
        Some(_) => {
            return Err(DeliveryError::new(
                "INVALID_RUN_ID",
                "run_id must be a valid UUID",
            ))
        }
        None => uuid()?,
    };

    if let Ok(existing) = run_status(delivery, principal, &run_id) {
        return Ok(existing);
    }

    let local_state = delivery.state(principal)?;
    let (pkg, _) = prepare_family_package(req.package, &local_state, req.sanitize_family)?;
    let household_id = pkg.target.household_id.clone();
    let ordered = resolve_targets(fleet, req.targets)?;
    let pkg_hash = pkg.hash()?;
    let pkg_json = serde_json::to_string(&pkg)?;
    let now = crate::id::now_ms();
    let (self_id, _) = fleet.self_id(None);

    let plans_by_node: std::collections::HashMap<String, FleetTargetPlan> = req
        .plans
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.node_id.clone(), p))
        .collect();

    let target_urls: Vec<(String, String)> = ordered
        .iter()
        .map(|node_id| {
            let base_url = if node_id == &self_id {
                "local".to_string()
            } else {
                fleet
                    .peer_endpoint(node_id)
                    .map(|(u, _)| u)
                    .unwrap_or_default()
            };
            (node_id.clone(), base_url)
        })
        .collect();

    delivery
        .store
        .with(|c| {
            let tx = c.unchecked_transaction()?;
            let epoch: i64 =
                tx.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO config_fleet_runs(run_id,principal,data_epoch,household_id,package_json,package_hash,state,created_ms,updated_ms) VALUES(?1,?2,?3,?4,?5,?6,'running',?7,?7)",
                params![run_id, principal, epoch, household_id, pkg_json, pkg_hash, now as i64],
            )?;
            for (ord, (node_id, base_url)) in target_urls.iter().enumerate() {
                let (plan_id, plan_hash, expected_rev) = plans_by_node
                    .get(node_id)
                    .map(|p| (p.plan_id.clone(), p.plan_hash.clone(), p.expected_revision))
                    .unwrap_or_default();
                let app_id = uuid().map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.detail))))?;
                tx.execute(
                    "INSERT INTO config_fleet_targets(run_id,node_id,ord,base_url,plan_id,plan_hash,expected_revision,application_id,state,updated_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending',?9)",
                    params![run_id, node_id, ord as i64, base_url, plan_id, plan_hash, expected_rev as i64, app_id, now as i64],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .map_err(DeliveryError::internal)?;

    execute_fleet_targets(delivery, fleet, principal, &run_id, &household_id, &pkg).await?;
    run_status(delivery, principal, &run_id)
}

pub fn run_status(delivery: &ConfigDelivery, principal: &str, run_id: &str) -> Result<Value> {
    delivery
        .store
        .with(|c| {
            let epoch: i64 =
                c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| r.get(0))?;
            let row: Option<(String, String, String, i64, i64, Option<String>)> = c
                .query_row(
                    "SELECT household_id, package_hash, state, created_ms, updated_ms, error_json FROM config_fleet_runs WHERE run_id=?1 AND principal=?2 AND data_epoch=?3",
                    params![run_id, principal, epoch],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?;
            let Some((household_id, package_hash, state, created_ms, updated_ms, error_json)) = row else {
                return Ok(Err(DeliveryError {
                    status: 404,
                    ..DeliveryError::new("FLEET_RUN_NOT_FOUND", "fleet run not found")
                }));
            };
            let mut stmt = c.prepare(
                "SELECT node_id, ord, base_url, plan_id, plan_hash, expected_revision, application_id, state, result_json, updated_ms FROM config_fleet_targets WHERE run_id=?1 ORDER BY ord ASC",
            )?;
            let targets = stmt
                .query_map([run_id], |r| {
                    let result_raw: String = r.get(8)?;
                    let result_val: Value =
                        serde_json::from_str(&result_raw).unwrap_or_else(|_| json!({}));
                    Ok(json!({
                        "node_id": r.get::<_, String>(0)?,
                        "ord": r.get::<_, i64>(1)?,
                        "base_url": r.get::<_, String>(2)?,
                        "plan_id": r.get::<_, String>(3)?,
                        "plan_hash": r.get::<_, String>(4)?,
                        "expected_revision": r.get::<_, i64>(5)? as u64,
                        "application_id": r.get::<_, String>(6)?,
                        "state": r.get::<_, String>(7)?,
                        "result": result_val,
                        "updated_ms": r.get::<_, i64>(9)? as u64,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let error_val: Value = error_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            Ok(Ok(json!({
                "run_id": run_id,
                "household_id": household_id,
                "data_epoch": epoch as u64,
                "package_hash": package_hash,
                "state": state,
                "created_ms": created_ms as u64,
                "updated_ms": updated_ms as u64,
                "error": error_val,
                "targets": targets,
            })))
        })
        .map_err(DeliveryError::internal)?
}

pub async fn retry_target(
    delivery: &ConfigDelivery,
    fleet: &FleetService,
    principal: &str,
    run_id: &str,
) -> Result<Value> {
    let (household_id, pkg): (String, Package) = delivery
        .store
        .with(|c| {
            let epoch: i64 =
                c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| r.get(0))?;
            let row: Option<(String, String)> = c
                .query_row(
                    "SELECT household_id, package_json FROM config_fleet_runs WHERE run_id=?1 AND principal=?2 AND data_epoch=?3",
                    params![run_id, principal, epoch],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((household_id, pkg_raw)) = row else {
                return Ok(Err(DeliveryError {
                    status: 404,
                    ..DeliveryError::new("FLEET_RUN_NOT_FOUND", "fleet run not found")
                }));
            };
            let pkg: Package = serde_json::from_str(&pkg_raw)?;
            let now = crate::id::now_ms() as i64;
            c.execute(
                "UPDATE config_fleet_runs SET state='running', error_json=NULL, updated_ms=?1 WHERE run_id=?2",
                params![now, run_id],
            )?;
            c.execute(
                "UPDATE config_fleet_targets SET state='pending', plan_id='', plan_hash='', updated_ms=?1 WHERE run_id=?2 AND state IN ('failed', 'skipped')",
                params![now, run_id],
            )?;
            Ok(Ok((household_id, pkg)))
        })
        .map_err(DeliveryError::internal)??;

    execute_fleet_targets(delivery, fleet, principal, run_id, &household_id, &pkg).await?;
    run_status(delivery, principal, run_id)
}

pub async fn rollback_fleet_target(
    delivery: &ConfigDelivery,
    fleet: &FleetService,
    principal: &str,
    run_id: &str,
    node_id: &str,
    req: FleetRollbackRequest,
) -> Result<Value> {
    let app_id: String = delivery
        .store
        .with(|c| {
            let epoch: i64 =
                c.query_row("SELECT data_epoch FROM config_head WHERE id=1", [], |r| r.get(0))?;
            let exists: Option<i64> = c
                .query_row(
                    "SELECT 1 FROM config_fleet_runs WHERE run_id=?1 AND principal=?2 AND data_epoch=?3",
                    params![run_id, principal, epoch],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Ok(Err(DeliveryError {
                    status: 404,
                    ..DeliveryError::new("FLEET_RUN_NOT_FOUND", "fleet run not found")
                }));
            }
            let app_id: Option<String> = c
                .query_row(
                    "SELECT application_id FROM config_fleet_targets WHERE run_id=?1 AND node_id=?2",
                    params![run_id, node_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(app_id.ok_or_else(|| DeliveryError {
                status: 404,
                ..DeliveryError::new("FLEET_TARGET_NOT_FOUND", "fleet target not found in run")
            }))
        })
        .map_err(DeliveryError::internal)??;

    let (self_id, _) = fleet.self_id(None);
    let (transport, _) = TargetTransport::resolve(node_id, &self_id, delivery, fleet, principal)?;
    let expected_revision = match req.expected_revision {
        Some(rev) => rev,
        None => transport.get_state().await?["config_revision"]
            .as_u64()
            .unwrap_or(0),
    };
    let pf = transport
        .rollback_preflight(&app_id, expected_revision)
        .await?;
    if !req.commit {
        return Ok(json!({
            "run_id": run_id,
            "node_id": node_id,
            "source_application_id": app_id,
            "preflight": pf,
            "committed": false,
        }));
    }

    let rollback_app_id = uuid()?;
    let commit = Commit {
        plan_id: pf["plan_id"].as_str().unwrap_or_default().to_string(),
        plan_hash: pf["plan_hash"].as_str().unwrap_or_default().to_string(),
        expected_revision: pf["expected_revision"]
            .as_u64()
            .unwrap_or(expected_revision),
        application_id: rollback_app_id.clone(),
    };
    let commit_resp = transport.commit(&commit).await?;
    let rb = poll_required_readback(&transport, &rollback_app_id, &[])
        .await
        .map_err(|(e, _)| e)?;

    let now = crate::id::now_ms();
    let result_str = serde_json::to_string(&json!({
        "rolled_back_from": app_id,
        "rollback_application_id": rollback_app_id,
        "readback": rb,
    }))?;
    delivery
        .store
        .with(|c| {
            c.execute(
                "UPDATE config_fleet_targets SET state='rolled_back', result_json=?1, updated_ms=?2 WHERE run_id=?3 AND node_id=?4",
                params![result_str, now as i64, run_id, node_id],
            )?;
            Ok(())
        })
        .map_err(DeliveryError::internal)?;

    Ok(json!({
        "run_id": run_id,
        "node_id": node_id,
        "source_application_id": app_id,
        "rollback_application_id": rollback_app_id,
        "committed": true,
        "preflight": pf,
        "commit": commit_resp,
        "readback": rb,
    }))
}
