//! A useful, deliberately partial dot2-d profile. No hardware configuration,
//! credentials, source persona, or personal data is copied into the artifact.
use super::*;
use serde_json::{json, Value};

const PROFILE_ID: &str = "luna-dot2d-persona-v1";
const FAMILY_PROFILE_ID: &str = "luna-dot2d-family-v1";
pub(super) const PERSONA: &str = "你是简洁、温和、耐心的家庭伙伴。日常用中文交流，默认简短回答，用户要求时再展开。按需提供英语陪练：每次一个词或一步，等待对方反馈，可以暂停、结束或重播。不要仅凭房间或声音猜测来者是孩子，也不要公开朗读个人记忆。识别不确定时先确认，不把不确定的回答直接记为错误；学习成绩与进度只能由已授权的学习流程确认。只有真实执行结果才能证明动作完成，不声称尚未验证的事情已经做成。";

pub(super) fn generate(delivery: &ConfigDelivery, principal: &str) -> Result<Value> {
    let state = delivery.state(principal)?;
    if state["target"]["enrolled"] != true {
        return Err(DeliveryError::conflict(
            "ENROLLMENT_REQUIRED",
            "initialize this Core's node identity first",
        ));
    }
    if state["target"]["node_id"] != "dot2-d" {
        return Err(DeliveryError::conflict(
            "PROFILE_TARGET_MISMATCH",
            "this profile requires the enrolled Core node dot2-d",
        ));
    }
    if state["resolved"].as_object().is_some_and(|fields| {
        fields
            .iter()
            .any(|(path, value)| path.starts_with("services.") && state["stored"][path] != *value)
    }) {
        // The shared commit path carries earlier pending services forward.
        // Do not label such a transaction persona-only or promise preservation.
        return Err(DeliveryError::conflict(
            "PROFILE_BASELINE_PENDING",
            "finish the existing service publication before reviewing this persona-only profile",
        ));
    }
    let mut package = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: uuid()?,
        package_revision: 1,
        created_ms: crate::id::now_ms(),
        purpose: Purpose::NodeConfig,
        source: Source {
            node_id: "dot2-d".into(),
            config_revision: state["config_revision"]
                .as_u64()
                .ok_or_else(|| DeliveryError::internal("missing config_revision"))?,
            core_version: crate::CORE_VERSION.into(),
            schema_version: state["schema_version"]
                .as_u64()
                .ok_or_else(|| DeliveryError::internal("missing schema_version"))?,
        },
        target: Target {
            household_id: state["target"]["household_id"]
                .as_str()
                .ok_or_else(|| DeliveryError::internal("missing target.household_id"))?
                .into(),
            node_id: Some("dot2-d".into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![],
        },
        change_reason: "dot2-d persona-only profile v1; other Luna settings remain unchanged"
            .into(),
        blocks: vec![Block {
            name: "persona".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "persona.agent_md.content".into(),
                value: json!(PERSONA),
            }],
        }],
        bindings: vec![],
        content_hash: None,
    };
    // Existing package integrity validation covers the target too. This is
    // not a signature or authorization beyond the paired management contract.
    package.content_hash = Some(package.hash()?);
    let preflight = delivery.preflight(
        principal,
        package.clone(),
        json!({"profile_id":PROFILE_ID,"scope":"persona_only"}),
    )?;
    if preflight["expected_revision"] != state["config_revision"] {
        return Err(DeliveryError::conflict(
            "PROFILE_STALE",
            "configuration changed while generating this profile; review again",
        ));
    }
    Ok(json!({
        "profile_id":PROFILE_ID,"filename":"luna-dot2d-persona-v1.json","scope":"persona_only","complete_luna_profile":false,
        "target_assurance":{"enrolled_core_node_id":"dot2-d","physical_device_identity":"not_verified"},
        "package":package,"preflight":preflight,
        "preserved":["services","credentials","custom_wake","unit_acoustics","voice_interaction","led","personal_and_learning_data"],
        "wake_policy":{"action":"preserve","recommended_default":"你好小智","gateway_effective_state":"not_verified"},
        "unavailable":[
            {"issue":108,"paths":["mode.*","privacy.idle_asr_enabled","device_led_configs.*","open_sockets"]},
            {"issue":109,"paths":["fleet.distribution","full_configuration_delivery"]},
            {"issue":111,"paths":["complete_dot2d_luna_profile"]},
            {"issue":112,"paths":["node.wake","reflex.words","mode.kws_threshold","mode.kws_score","gateway.loaded_wake_evidence"]},
            {"issue":113,"paths":["unit_acoustic_profiles.*","audio_eq","mode.vad","mode.media_trim_db","gateway.acoustic_effective_evidence"]}
        ]
    }))
}

fn collect_prefix(map: Option<&serde_json::Map<String, Value>>, prefix: &str) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(m) = map {
        for (k, v) in m {
            if k.starts_with(prefix) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

pub(super) fn generate_family(delivery: &ConfigDelivery, principal: &str) -> Result<Value> {
    let state = delivery.state(principal)?;
    if state["target"]["enrolled"] != true {
        return Err(DeliveryError::conflict(
            "ENROLLMENT_REQUIRED",
            "initialize this Core's node identity first",
        ));
    }
    if state["target"]["node_id"] != "dot2-d" {
        return Err(DeliveryError::conflict(
            "PROFILE_TARGET_MISMATCH",
            "this profile requires the enrolled Core node dot2-d",
        ));
    }
    let resolved = state["resolved"].as_object();
    let stored = state["stored"].as_object();

    let wake_phrases = resolved
        .and_then(|m| m.get("node.wake.phrases"))
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .map(|arr| Value::Array(arr.clone()))
        .or_else(|| {
            resolved
                .and_then(|m| m.get("reflex.words"))
                .and_then(Value::as_array)
                .map(|words| {
                    words
                        .iter()
                        .filter(|w| {
                            w.get("intent").and_then(Value::as_str) == Some("wake")
                                && w.get("enabled").and_then(Value::as_bool).unwrap_or(true)
                        })
                        .filter_map(|w| w.get("text").and_then(Value::as_str).map(|s| json!(s)))
                        .collect::<Vec<_>>()
                })
                .filter(|arr| !arr.is_empty())
                .map(Value::Array)
        })
        .unwrap_or_else(|| json!(["你好小智"]));

    let led_timezone = resolved
        .and_then(|m| m.get("node.led.timezone"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Asia/Shanghai")
        .to_string();

    let orientation_rotation = resolved
        .and_then(|m| m.get("device_led_configs.dot2-d.orientation_rotation"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let orientation_reverse = resolved
        .and_then(|m| m.get("device_led_configs.dot2-d.orientation_reverse"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut package = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: uuid()?,
        package_revision: 1,
        created_ms: crate::id::now_ms(),
        purpose: Purpose::NodeConfig,
        source: Source {
            node_id: "dot2-d".into(),
            config_revision: state["config_revision"]
                .as_u64()
                .ok_or_else(|| DeliveryError::internal("missing config_revision"))?,
            core_version: crate::CORE_VERSION.into(),
            schema_version: state["schema_version"]
                .as_u64()
                .ok_or_else(|| DeliveryError::internal("missing schema_version"))?,
        },
        target: Target {
            household_id: state["target"]["household_id"]
                .as_str()
                .ok_or_else(|| DeliveryError::internal("missing target.household_id"))?
                .into(),
            node_id: Some("dot2-d".into()),
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![
                "config_apply_v2".into(),
                "config_block_led".into(),
                "config_block_duplex".into(),
            ],
        },
        change_reason: "dot2-d complete family profile v1 (luna-dot2d-family-v1)".into(),
        blocks: vec![
            Block {
                name: "persona".into(),
                schema_version: 1,
                operations: vec![Operation::Set {
                    path: "persona.agent_md.content".into(),
                    value: json!(PERSONA),
                }],
            },
            Block {
                name: "mode".into(),
                schema_version: 1,
                operations: vec![
                    Operation::Set {
                        path: "mode.duplex_requested.dot2-d".into(),
                        value: json!("half"),
                    },
                    Operation::Set {
                        path: "mode.follow_window_enabled".into(),
                        value: json!(true),
                    },
                    Operation::Set {
                        path: "mode.follow_window_ms".into(),
                        value: json!(9000),
                    },
                    Operation::Set {
                        path: "mode.followup_credits".into(),
                        value: Value::Null,
                    },
                    Operation::Set {
                        path: "mode.context_window_ms".into(),
                        value: json!(240000),
                    },
                    Operation::Set {
                        path: "mode.follow_window_tone".into(),
                        value: json!(false),
                    },
                    Operation::Set {
                        path: "mode.wake_ack.enabled".into(),
                        value: json!(true),
                    },
                    Operation::Replace {
                        path: "mode.wake_ack.phrases".into(),
                        value: json!(["在呢", "你说"]),
                    },
                    Operation::Set {
                        path: "mode.spoken.max_sentences".into(),
                        value: json!(2),
                    },
                    Operation::Set {
                        path: "mode.spoken.max_chars".into(),
                        value: json!(120),
                    },
                    Operation::Set {
                        path: "mode.filler.delay_ms".into(),
                        value: json!(1500),
                    },
                ],
            },
            Block {
                name: "node".into(),
                schema_version: 1,
                operations: vec![
                    Operation::Replace {
                        path: "node.wake.phrases".into(),
                        value: wake_phrases,
                    },
                    Operation::Set {
                        path: "node.led.timezone".into(),
                        value: json!(led_timezone),
                    },
                ],
            },
            Block {
                name: "device_led_configs".into(),
                schema_version: 1,
                operations: vec![
                    Operation::Set {
                        path: "device_led_configs.dot2-d.night_enabled".into(),
                        value: json!(true),
                    },
                    Operation::Set {
                        path: "device_led_configs.dot2-d.night_start_minute".into(),
                        value: json!(1320),
                    },
                    Operation::Set {
                        path: "device_led_configs.dot2-d.night_end_minute".into(),
                        value: json!(420),
                    },
                    Operation::Set {
                        path: "device_led_configs.dot2-d.night_brightness_pct".into(),
                        value: json!(10),
                    },
                    Operation::Set {
                        path: "device_led_configs.dot2-d.orientation_rotation".into(),
                        value: json!(orientation_rotation),
                    },
                    Operation::Set {
                        path: "device_led_configs.dot2-d.orientation_reverse".into(),
                        value: json!(orientation_reverse),
                    },
                ],
            },
        ],
        bindings: vec![],
        content_hash: None,
    };
    package.content_hash = Some(package.hash()?);
    let preflight = delivery.preflight(
        principal,
        package.clone(),
        json!({"profile_id": FAMILY_PROFILE_ID, "scope": "dot2d_family"}),
    )?;
    if preflight["expected_revision"] != state["config_revision"] {
        return Err(DeliveryError::conflict(
            "PROFILE_STALE",
            "configuration changed while generating this profile; review again",
        ));
    }
    Ok(json!({
        "profile_id": FAMILY_PROFILE_ID,
        "filename": "luna-dot2d-family-v1.json",
        "scope": "dot2d_family",
        "complete_luna_profile": true,
        "target_assurance": {
            "enrolled_core_node_id": "dot2-d",
            "immutable_target_lock": "dot2-d",
            "physical_device_identity": "not_verified",
        },
        "package": package,
        "preflight": preflight,
        "retained_unit_refs": {
            "unit_acoustic_profiles.dot2-d": collect_prefix(stored, "unit_acoustic_profiles.dot2-d."),
            "audio_eq": collect_prefix(resolved, "audio_eq."),
            "services": collect_prefix(resolved, "services."),
            "bindings": state["bindings"],
        },
        "compatibility": {
            "core_api_schema": 2,
            "required_capabilities": package.compatibility.required_capabilities,
            "readback_requirements": {
                "required_blocks": ["persona", "mode", "node", "device_led_configs"],
                "publication_state": "applied",
            },
        },
        "rollback_instructions": {
            "preflight_endpoint": "/api/v1/config/applications/{application_id}/rollback-preflight",
            "commit_endpoint": "/api/v1/config/applications",
            "expected_revision": state["config_revision"],
        },
        "blockers": [
            {
                "path": "privacy.idle_asr_enabled",
                "reason": "UNSUPPORTED_FIELD",
                "detail": "privacy.idle_asr_enabled is registered as unsupported in this slice; reported explicitly rather than guessed",
            },
            {
                "path": "mode.proactive_speech",
                "reason": "UNSUPPORTED_FIELD",
                "detail": "mode.proactive_speech is registered as unsupported in this slice; reported explicitly rather than guessed",
            },
        ],
    }))
}
