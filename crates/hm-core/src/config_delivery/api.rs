//! All v1 configuration routes require a real paired bearer, including loopback.
use super::*;
use crate::api::AppState;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

type Response = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;
fn error(e: DeliveryError) -> (StatusCode, Json<Value>) {
    (
        StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"error":e.code,"detail":e.detail,"pointer":e.pointer})),
    )
}
fn principal(state: &AppState, headers: &HeaderMap) -> Result<String> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    state.auth.verify(token).ok_or_else(|| DeliveryError {
        status: 401,
        ..DeliveryError::new(
            "PAIRING_REQUIRED",
            "a valid paired management bearer is required, including on loopback",
        )
    })
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/config/state", get(state_get))
        .route("/api/v1/config/enrollment", put(enrollment))
        .route("/api/v1/config/preflight", post(preflight))
        .route("/api/v1/config/exports", post(exports))
        .route(
            "/api/v1/config/profiles/dot2-d-persona",
            post(dot2d_profile),
        )
        .route(
            "/api/v1/config/profiles/dot2-d-family",
            post(dot2d_family_profile),
        )
        .route(
            "/api/v1/config/household/invitations",
            post(issue_invitation),
        )
        .route(
            "/api/v1/config/household/invitations/redeem",
            post(redeem_invitation),
        )
        .route("/api/v1/config/household/join", post(join_household))
        .route("/api/v1/config/fleet/preflight", post(fleet_preflight))
        .route("/api/v1/config/fleet/runs", post(fleet_commit))
        .route("/api/v1/config/fleet/runs/{id}", get(fleet_run_status))
        .route("/api/v1/config/fleet/runs/{id}/retry", post(fleet_retry))
        .route(
            "/api/v1/config/fleet/runs/{id}/targets/{node}/rollback-preflight",
            post(fleet_rollback),
        )
        .route("/api/v1/config/applications", post(apply).get(applications))
        .route("/api/v1/config/applications/{id}", get(application))
        .route("/api/v1/config/applications/{id}/readback", get(readback))
        .route(
            "/api/v1/config/applications/{id}/rollback-preflight",
            post(rollback),
        )
}
async fn dot2d_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let body = v1::parse(&body).map_err(error)?;
    if !body.as_object().is_some_and(|object| object.is_empty()) {
        return Err(error(DeliveryError::new(
            "EMPTY_OBJECT_REQUIRED",
            "profile generation accepts only an empty JSON object",
        )));
    }
    // Configured Core identity and enrollment are the existing management
    // authority. A display name/room is never used as a target or hardware proof.
    let (node, source) = state.fleet.self_id(None);
    if source == "default" || node != "dot2-d" {
        return Err(error(DeliveryError::conflict(
            "PROFILE_TARGET_MISMATCH",
            "the configured Core node must be dot2-d",
        )));
    }
    let generated =
        super::profile::generate(&ConfigDelivery::for_app(&state), &actor).map_err(error)?;
    let (current, current_source) = state.fleet.self_id(None);
    if current != node || current_source == "default" {
        return Err(error(DeliveryError::conflict(
            "PROFILE_TARGET_MISMATCH",
            "configured Core identity changed during review",
        )));
    }
    Ok(Json(generated))
}
async fn dot2d_family_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let body = v1::parse(&body).map_err(error)?;
    if !body.as_object().is_some_and(|object| object.is_empty()) {
        return Err(error(DeliveryError::new(
            "EMPTY_OBJECT_REQUIRED",
            "profile generation accepts only an empty JSON object",
        )));
    }
    let (node, source) = state.fleet.self_id(None);
    if source == "default" || node != "dot2-d" {
        return Err(error(DeliveryError::conflict(
            "PROFILE_TARGET_MISMATCH",
            "the configured Core node must be dot2-d",
        )));
    }
    let generated =
        super::profile::generate_family(&ConfigDelivery::for_app(&state), &actor).map_err(error)?;
    let (current, current_source) = state.fleet.self_id(None);
    if current != node || current_source == "default" {
        return Err(error(DeliveryError::conflict(
            "PROFILE_TARGET_MISMATCH",
            "configured Core identity changed during review",
        )));
    }
    Ok(Json(generated))
}
async fn issue_invitation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    principal(&state, &headers).map_err(error)?;
    let req: super::fleet::IssueInvitationRequest =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    super::fleet::issue_invitation(&ConfigDelivery::for_app(&state), req)
        .map(Json)
        .map_err(error)
}
async fn redeem_invitation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    principal(&state, &headers).map_err(error)?;
    let req: super::fleet::RedeemInvitationRequest =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    super::fleet::redeem_invitation(&ConfigDelivery::for_app(&state), req)
        .map(Json)
        .map_err(error)
}
async fn join_household(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    principal(&state, &headers).map_err(error)?;
    let req: super::fleet::JoinRequest =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    super::fleet::accept_invitation(&ConfigDelivery::for_app(&state), &state.fleet, req)
        .await
        .map(Json)
        .map_err(error)
}
async fn fleet_preflight(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let req: super::fleet::FleetPreflightRequest =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    super::fleet::preflight_fleet(&ConfigDelivery::for_app(&state), &state.fleet, &actor, req)
        .await
        .map(Json)
        .map_err(error)
}
async fn fleet_commit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let req: super::fleet::FleetRunRequest =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    super::fleet::commit_fleet(&ConfigDelivery::for_app(&state), &state.fleet, &actor, req)
        .await
        .map(Json)
        .map_err(error)
}
async fn fleet_run_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    super::fleet::run_status(&ConfigDelivery::for_app(&state), &actor, &id)
        .map(Json)
        .map_err(error)
}
async fn fleet_retry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    super::fleet::retry_target(&ConfigDelivery::for_app(&state), &state.fleet, &actor, &id)
        .await
        .map(Json)
        .map_err(error)
}
async fn fleet_rollback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, node)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let parsed = if body.is_empty() {
        json!({})
    } else {
        v1::parse(&body).map_err(error)?
    };
    let req: super::fleet::FleetRollbackRequest =
        serde_json::from_value(parsed).map_err(|e| error(e.into()))?;
    super::fleet::rollback_fleet_target(
        &ConfigDelivery::for_app(&state),
        &state.fleet,
        &actor,
        &id,
        &node,
        req,
    )
    .await
    .map(Json)
    .map_err(error)
}
async fn state_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let mut snapshot = ConfigDelivery::for_app(&state)
        .state(&actor)
        .map_err(error)?;
    let (node, source) = state.fleet.self_id(None);
    snapshot["capabilities"]["can_enroll"] = json!(source != "default");
    snapshot["target"]["node_id_source"] = json!(if snapshot["target"]["enrolled"] == true {
        "enrollment"
    } else {
        source
    });
    if snapshot["target"]["enrolled"] != true && source != "default" {
        snapshot["target"]["node_id"] = json!(node);
    }
    Ok(Json(snapshot))
}
async fn enrollment(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    principal(&state, &headers).map_err(error)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    let _: Empty =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    let (node, source) = state.fleet.self_id(None);
    if source == "default" {
        return Err(error(DeliveryError::new(
            "NODE_ID_REQUIRED",
            "configure this node's stable identity before enrollment",
        )));
    }
    ConfigDelivery::for_app(&state)
        .enroll(&node)
        .map(Json)
        .map_err(error)
}
async fn preflight(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        package: Value,
    }
    let body: Body =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    let service = ConfigDelivery::for_app(&state);
    let snapshot = service.state(&actor).map_err(error)?;
    let converted_at_revision = if body.package["version"] == 1
        && (body.package["music"].is_object() || body.package["openapi"].is_object())
    {
        snapshot["config_revision"].as_u64()
    } else {
        None
    };
    let (package, report) = v1::convert(body.package, &snapshot).map_err(error)?;
    service
        .preflight_at_revision(
            &actor,
            package,
            report,
            crate::id::now_ms(),
            converted_at_revision,
        )
        .map(Json)
        .map_err(error)
}
async fn apply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let actor = principal(&state, &headers).map_err(error)?;
    let request: Commit =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    ConfigDelivery::for_app(&state)
        .commit(&actor, &request)
        .map(|v| (StatusCode::ACCEPTED, Json(v)))
        .map_err(error)
}
async fn applications(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    ConfigDelivery::for_app(&state)
        .applications(&actor)
        .map(Json)
        .map_err(error)
}
async fn application(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    ConfigDelivery::for_app(&state)
        .application(&actor, &id)
        .map(Json)
        .map_err(error)
}

async fn readback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    ConfigDelivery::for_app(&state)
        .readback(&actor, &id)
        .map(Json)
        .map_err(error)
}
async fn rollback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        expected_revision: u64,
    }
    let body: Body =
        serde_json::from_value(v1::parse(&body).map_err(error)?).map_err(|e| error(e.into()))?;
    ConfigDelivery::for_app(&state)
        .rollback_preflight(&actor, &id, body.expected_revision)
        .map(Json)
        .map_err(error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    #[serde(default = "default_export_format")]
    format: String,
    #[serde(default)]
    purpose: Option<Purpose>,
    #[serde(default)]
    blocks: Option<Vec<String>>,
}
fn default_export_format() -> String {
    "v2".into()
}

const EXPORT_EXCLUDED: &[&str] = &[
    "secrets",
    "unit_acoustic_profiles",
    "mode.kws_threshold",
    "mode.kws_score",
    "mode.media_trim_db",
    "mode.tail_blind_ms",
    "mode.vad",
    "learning",
    "user_md",
    "working_memory",
    "runtime",
    "fleet",
];

async fn exports(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let actor = principal(&state, &headers).map_err(error)?;
    let parsed = if body.is_empty() {
        json!({})
    } else {
        v1::parse(&body).map_err(error)?
    };
    let req: ExportRequest = serde_json::from_value(parsed).map_err(|e| error(e.into()))?;
    if req.format != "v2" && req.format != "v1" {
        return Err(error(DeliveryError::new(
            "UNSUPPORTED_VERSION",
            "export format must be 'v2' or 'v1'",
        )));
    }
    if let Some(ref requested_blocks) = req.blocks {
        for b in requested_blocks {
            if matches!(
                b.as_str(),
                "secrets"
                    | "unit_acoustic_profiles"
                    | "learning"
                    | "user_md"
                    | "working_memory"
                    | "runtime"
                    | "fleet"
                    | "db"
            ) {
                return Err(error(
                    DeliveryError::new(
                        "FORBIDDEN_EXPORT_BLOCK",
                        "protected or secret domain cannot be exported",
                    )
                    .at(b),
                ));
            }
        }
    }

    let snapshot = ConfigDelivery::for_app(&state)
        .state(&actor)
        .map_err(error)?;
    let (fleet_node, _) = state.fleet.self_id(None);
    let node_id = snapshot["target"]["node_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&fleet_node)
        .to_string();
    let household_id = snapshot["target"]["household_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let mut excluded: Vec<String> = EXPORT_EXCLUDED.iter().map(|s| (*s).to_string()).collect();
    let weather_reviewable = snapshot["capabilities"]["weather_reviewable"] == true;
    if !weather_reviewable {
        excluded.push("weather_unavailable".into());
    }

    let unavailable = snapshot["capabilities"]["unavailable_blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    excluded.extend(
        unavailable
            .iter()
            .map(|block| format!("{block}_unavailable")),
    );

    if req.format == "v1" {
        let mut doc = crate::api::services::system::build_config_document(&state, false)
            .map_err(|e| error(DeliveryError::internal(e)))?;
        if let Some(obj) = doc.as_object_mut() {
            if req.purpose == Some(Purpose::FamilyPolicy) {
                obj.remove("remote_socket");
                obj.remove("open_sockets");
                obj.remove("open_sockets_source_node");
                excluded.push("remote_socket_node_scope".into());
                excluded.push("open_sockets_node_scope".into());
            }
            if !weather_reviewable {
                obj.remove("weather");
            }
            for block in &unavailable {
                obj.remove(*block);
            }
            if !obj.contains_key("open_sockets") {
                obj.remove("open_sockets_source_node");
            }
            if let Some(ref filter) = req.blocks {
                obj.retain(|k, _| {
                    ["format", "version", "exported_ms"].contains(&k.as_str())
                        || filter.iter().any(|b| b == k)
                        || (k == "open_sockets_source_node"
                            && filter.iter().any(|b| b == "open_sockets"))
                });
            }
            if obj.remove("mode").is_some() {
                let mut policy = super::mode::projection(&state.voice.settings_snapshot());
                if let Some(fields) = policy.as_object_mut() {
                    fields.remove("kws_threshold");
                    fields.remove("kws_score");
                    if req.purpose == Some(Purpose::FamilyPolicy) {
                        fields.remove("system_prompt_override");
                        fields.remove("wake_words");
                        fields.remove("duplex_requested");
                    }
                }
                obj.insert("mode_policy".into(), policy);
            }
        }
        let loss_report = json!([
            {"domain": "secrets", "status": "redacted", "detail": "credential values excluded; presence flags only"},
            {"domain": "unit_acoustic_profiles", "status": "excluded", "detail": "per-unit acoustic calibration is never exported"},
            {"domain": "mode", "status": "partial", "detail": "mode_policy contains reviewed prompt, participation, Core ASR vocabulary and spoken delivery fields"},
            {"domain": "mode.unit_acoustics", "status": "excluded", "detail": "kws_threshold, kws_score, media_trim_db, tail_blind_ms and vad are never exported"},
            {"domain": "shared_memory", "status": "omitted", "detail": "v1 compatibility export does not include v2 shared_memory block"},
            {"domain": "persona", "status": "omitted", "detail": "v1 compatibility export does not include v2 persona block"},
            {"domain": "layers", "status": "flattened", "detail": "v1 compatibility export flattens family/node policy layers"},
            {"domain": "data_and_runtime", "status": "excluded", "detail": "learning, user_md, working_memory, runtime and fleet are excluded"}
        ]);
        return Ok(Json(json!({
            "format_version": 1,
            "filename": format!("home-mate-config-v1-{node_id}.json"),
            "redacted": true,
            "excluded": excluded,
            "document": doc,
            "loss_report": loss_report,
        })));
    }

    let purpose = req.purpose.unwrap_or(Purpose::NodeConfig);
    if purpose == Purpose::FamilyPolicy {
        excluded.push("node_scoped_fields".into());
    }
    let mut block_ops: std::collections::BTreeMap<String, Vec<Operation>> =
        std::collections::BTreeMap::new();
    if let Some(resolved) = snapshot["resolved"].as_object() {
        for (path, val) in resolved {
            let block_name = path.split('.').next().unwrap_or("");
            if (block_name == "weather" && !weather_reviewable) || unavailable.contains(&block_name)
            {
                continue;
            }
            if let Some(ref filter) = req.blocks {
                if !filter.iter().any(|b| b == block_name) {
                    continue;
                }
            }
            if let Some(desc) = inventory::descriptor(path) {
                if purpose == Purpose::FamilyPolicy && desc.scope != "family_or_node" {
                    continue;
                }
                if desc.scope == "unit"
                    || matches!(desc.scope.as_str(), "boot" | "data" | "runtime" | "secret")
                {
                    continue;
                }
            }
            let operation = if val.is_array() || val.is_object() {
                Operation::Replace {
                    path: path.clone(),
                    value: val.clone(),
                }
            } else {
                Operation::Set {
                    path: path.clone(),
                    value: val.clone(),
                }
            };
            block_ops
                .entry(block_name.to_string())
                .or_default()
                .push(operation);
        }
    }
    let bindings = snapshot["bindings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["bound"] == true)
        .filter_map(|b| b["binding_id"].as_str())
        .map(|id| Binding::Preserve {
            binding_id: id.to_string(),
        })
        .collect();

    let mut package = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: uuid().map_err(error)?,
        package_revision: 1,
        created_ms: crate::id::now_ms(),
        purpose: purpose.clone(),
        source: Source {
            node_id: node_id.clone(),
            config_revision: snapshot["config_revision"].as_u64().unwrap_or(0),
            core_version: crate::CORE_VERSION.into(),
            schema_version: snapshot["schema_version"].as_u64().unwrap_or(0),
        },
        target: Target {
            household_id,
            node_id: if purpose == Purpose::FamilyPolicy {
                None
            } else {
                Some(node_id.clone())
            },
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![],
        },
        change_reason: "Redacted v2 configuration export".into(),
        blocks: block_ops
            .into_iter()
            .map(|(name, operations)| Block {
                name,
                schema_version: 1,
                operations,
            })
            .collect(),
        bindings,
        content_hash: None,
    };
    package.content_hash = Some(package.hash().map_err(error)?);

    Ok(Json(json!({
        "format_version": 2,
        "filename": format!("home-mate-config-v2-{node_id}.json"),
        "redacted": true,
        "excluded": excluded,
        "package": package,
    })))
}
