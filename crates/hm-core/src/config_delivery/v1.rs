//! Presence-preserving legacy adapter. Converts and validates all v1 families
//! into a v2 package while preserving unit acoustic isolation and rejecting
//! secrets, unknown fields, and conflicting legacy privacy flags.
use super::*;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;

struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Strict, E> {
                Ok(Strict(json!(v)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Strict, A::Error> {
                let mut values = vec![];
                while let Some(Strict(v)) = a.next_element()? {
                    values.push(v);
                }
                Ok(Strict(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Strict, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(format!("duplicate JSON key: {key}")));
                    }
                    let Strict(v) = a.next_value()?;
                    values.insert(key, v);
                }
                Ok(Strict(Value::Object(values)))
            }
        }
        d.deserialize_any(V)
    }
}

pub fn parse(bytes: &[u8]) -> Result<Value> {
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let Strict(value) = Strict::deserialize(&mut parser)?;
    parser.end()?;
    Ok(value)
}

fn is_secret_key(key: &str) -> bool {
    matches!(
        key,
        "api_key" | "secret" | "token" | "password" | "credentials"
    )
}

fn is_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "api_key_set" | "configured" | "token_set" | "secret_set"
    )
}

fn pointer_for(path: &str) -> String {
    format!("/{}", path.replace('.', "/"))
}

pub(super) fn validate_array_elements(path: &str, items: &[Value]) -> Result<()> {
    for (idx, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(
                DeliveryError::new("INVALID_FIELD_TYPE", "expected object element")
                    .at(&format!("{path}[{idx}]")),
            );
        };
        for (k, v) in obj {
            let elem_path = format!("{path}[{idx}].{k}");
            if is_secret_key(k) {
                return Err(DeliveryError::new(
                    "SECRET_VALUES_FORBIDDEN",
                    "ordinary migration cannot restore credentials",
                )
                .at(&elem_path));
            }
            let desc = inventory::descriptor(&elem_path).ok_or_else(|| {
                DeliveryError::new("UNKNOWN_FIELD", "unregistered configuration path")
                    .at(&elem_path)
            })?;
            inventory::validate_descriptor_value(&desc, &elem_path, v)?;
            if desc.kind == "array" && desc.array_item_kind.as_deref() == Some("object") {
                if let Some(sub_items) = v.as_array() {
                    validate_array_elements(&elem_path, sub_items)?;
                }
            } else if desc.kind == "object" && !(path == "open_sockets" && k == "parameters") {
                // Remote parameters are a schema document with dynamic property names.
                if let Some(sub_obj) = v.as_object() {
                    for (sk, sv) in sub_obj {
                        let sub_path = format!("{elem_path}.{sk}");
                        let sub_desc = inventory::descriptor(&sub_path).ok_or_else(|| {
                            DeliveryError::new("UNKNOWN_FIELD", "unregistered configuration path")
                                .at(&sub_path)
                        })?;
                        inventory::validate_descriptor_value(&sub_desc, &sub_path, sv)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn family_choice_or_strip(
    pointer: String,
    scope: &str,
    detail: &str,
    sanitize_family: bool,
    report: &mut Vec<Value>,
) {
    if sanitize_family {
        report.push(json!({
            "pointer": pointer,
            "action": "stripped_for_family",
            "scope": scope,
            "detail": detail,
        }));
    } else {
        report.push(json!({
            "pointer": pointer,
            "action": "requires_choice",
            "blocked": true,
            "scope": scope,
            "detail": detail,
        }));
    }
}

fn convert_path(
    path: &str,
    leaf_key: &str,
    value: &Value,
    purpose: &Purpose,
    sanitize_family: bool,
    blocks: &mut BTreeMap<String, Vec<Operation>>,
    report: &mut Vec<Value>,
) -> Result<()> {
    if is_secret_key(leaf_key) {
        if *purpose == Purpose::FamilyPolicy {
            family_choice_or_strip(
                pointer_for(path),
                "secret",
                "credentials cannot be included in family_policy",
                sanitize_family,
                report,
            );
            return Ok(());
        }
        return Err(DeliveryError::new(
            "SECRET_VALUES_FORBIDDEN",
            "ordinary migration cannot restore credentials",
        )
        .at(path));
    }
    if is_metadata_key(leaf_key) {
        if !value.is_boolean() {
            return Err(DeliveryError::new(
                "INVALID_FIELD_TYPE",
                "legacy credential status must be boolean",
            )
            .at(path));
        }
        report.push(json!({
            "pointer": pointer_for(path),
            "action": "ignored_metadata",
            "detail": "presence is not a credential; target binding preserved"
        }));
        return Ok(());
    }
    let discovery_metadata = match path {
        "music.revision" | "news.state" | "news.revision" | "search.state" | "search.revision"
        | "search.detail" => Some(value.is_string()),
        "search.max_results" => Some(value.is_u64()),
        _ => None,
    };
    if let Some(well_typed) = discovery_metadata {
        if !well_typed {
            return Err(DeliveryError::new(
                "INVALID_FIELD_TYPE",
                "invalid legacy discovery status field",
            )
            .at(path));
        }
        report.push(json!({"pointer": pointer_for(path), "action": "ignored_metadata"}));
        return Ok(());
    }

    let desc = inventory::descriptor(path).ok_or_else(|| {
        DeliveryError::new("UNKNOWN_FIELD", "unregistered configuration path").at(path)
    })?;

    if *purpose == Purpose::FamilyPolicy && desc.scope != "family_or_node" {
        family_choice_or_strip(
            pointer_for(path),
            &desc.scope,
            "node/unit/protected field cannot be included in family_policy",
            sanitize_family,
            report,
        );
        return Ok(());
    }
    if desc.scope == "unit" {
        return Err(DeliveryError::new(
            "FORBIDDEN_SCOPE",
            "unit acoustic calibration cannot be imported via configuration packages",
        )
        .at(path));
    }
    if matches!(desc.scope.as_str(), "boot" | "data" | "runtime" | "secret") {
        return Err(DeliveryError::new(
            "FORBIDDEN_SCOPE",
            "protected authority cannot be imported via configuration packages",
        )
        .at(path));
    }

    inventory::validate_descriptor_value(&desc, path, value)?;

    if *purpose == Purpose::FamilyPolicy && path == "reflex.words" {
        if let Some(items) = value.as_array() {
            let has_wake = items
                .iter()
                .any(|w| w.get("intent").and_then(Value::as_str) == Some("wake"));
            if has_wake {
                if !sanitize_family {
                    family_choice_or_strip(
                        pointer_for(path),
                        "node",
                        "reflex.words entries with intent 'wake' cannot be included in family_policy",
                        false,
                        report,
                    );
                    return Ok(());
                }
                let filtered: Vec<Value> = items
                    .iter()
                    .filter(|w| w.get("intent").and_then(Value::as_str) != Some("wake"))
                    .cloned()
                    .collect();
                family_choice_or_strip(
                    pointer_for(path),
                    "node",
                    "stripped wake-intent reflex words for family_policy",
                    true,
                    report,
                );
                if filtered.is_empty() {
                    return Ok(());
                }
                let filtered_val = Value::Array(filtered);
                validate_array_elements(path, filtered_val.as_array().unwrap())?;
                blocks
                    .entry("reflex".into())
                    .or_default()
                    .push(Operation::Replace {
                        path: path.into(),
                        value: filtered_val,
                    });
                return Ok(());
            }
        }
    }

    if matches!(
        path,
        "mode.spoken.detail_words" | "mode.spoken.long_form_words"
    ) {
        report.push(json!({"pointer":pointer_for(path), "action":"preserved",
            "detail":"legacy answer-classification words are stored only"}));
        return Ok(());
    }
    let block_name = path.split('.').next().unwrap_or("").to_string();
    match desc.kind.as_str() {
        "array" => {
            if desc.array_item_kind.as_deref() == Some("object") {
                if let Some(items) = value.as_array() {
                    validate_array_elements(path, items)?;
                }
            }
            let mut value = value.clone();
            if path == "openapi.operations" {
                if let Some(operations) = value.as_array_mut() {
                    for (index, op) in operations.iter_mut().enumerate() {
                        if let Some(op_id) = op.pointer_mut("/verification/op_id") {
                            if let Some(raw) = op_id.as_str() {
                                if raw != raw.trim() {
                                    *op_id = json!(raw.trim());
                                    report.push(json!({
                                        "pointer": format!("/openapi/operations/{index}/verification/op_id"),
                                        "action": "normalization",
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            blocks
                .entry(block_name)
                .or_default()
                .push(Operation::Replace {
                    path: path.to_string(),
                    value,
                });
            report.push(json!({
                "pointer": pointer_for(path),
                "action": "mapped",
                "operation": "replace",
                "path": path
            }));
        }
        "json" => {
            blocks
                .entry(block_name)
                .or_default()
                .push(Operation::Replace {
                    path: path.to_string(),
                    value: value.clone(),
                });
            report.push(json!({
                "pointer": pointer_for(path),
                "action": "mapped",
                "operation": "replace",
                "path": path
            }));
        }
        "object" => {
            if value.is_null() {
                blocks.entry(block_name).or_default().push(Operation::Set {
                    path: path.to_string(),
                    value: Value::Null,
                });
                report.push(json!({
                    "pointer": pointer_for(path),
                    "action": "mapped",
                    "operation": "set",
                    "path": path
                }));
            } else if path == "mode.duplex_requested" {
                let map = value.as_object().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected object").at(path)
                })?;
                for (peer, val) in map {
                    let sub_path = format!("{path}.{peer}");
                    let sub_desc = inventory::descriptor(&sub_path).ok_or_else(|| {
                        DeliveryError::new("UNKNOWN_FIELD", "unregistered peer id").at(&sub_path)
                    })?;
                    inventory::validate_descriptor_value(&sub_desc, &sub_path, val)?;
                }
                blocks
                    .entry(block_name)
                    .or_default()
                    .push(Operation::Replace {
                        path: path.to_string(),
                        value: value.clone(),
                    });
                report.push(json!({
                    "pointer": pointer_for(path),
                    "action": "mapped",
                    "operation": "replace",
                    "path": path
                }));
            } else {
                let obj = value.as_object().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected object").at(path)
                })?;
                for (k, v) in obj {
                    convert_path(
                        &format!("{path}.{k}"),
                        k,
                        v,
                        purpose,
                        sanitize_family,
                        blocks,
                        report,
                    )?;
                }
            }
        }
        _ => {
            blocks.entry(block_name).or_default().push(Operation::Set {
                path: path.to_string(),
                value: value.clone(),
            });
            report.push(json!({
                "pointer": pointer_for(path),
                "action": "mapped",
                "operation": "set",
                "path": path
            }));
        }
    }
    Ok(())
}

fn music_connection_v1(
    path: &str,
    object: &serde_json::Map<String, Value>,
    state: &Value,
    blocks: &mut BTreeMap<String, Vec<Operation>>,
    report: &mut Vec<Value>,
) -> Result<()> {
    let mut cfg: crate::config::MusicConnection = state["resolved"][path]
        .as_object()
        .map(|object| serde_json::from_value(Value::Object(object.clone())))
        .transpose()?
        .unwrap_or_default();
    let mut changed = false;
    for (name, value) in object {
        let field = format!("{path}.{name}");
        match name.as_str() {
            "secret_set" => {
                if !value.is_boolean() {
                    return Err(DeliveryError::new(
                        "INVALID_FIELD_TYPE",
                        "legacy credential status must be boolean",
                    )
                    .at(&field));
                }
                report.push(json!({"pointer":pointer_for(&field),"action":"ignored_metadata"}));
            }
            "endpoint" | "user" => {
                let val = value.as_str().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected string").at(&field)
                })?;
                if name == "endpoint" {
                    cfg.endpoint = val.into();
                } else {
                    cfg.user = val.into();
                }
                changed = true;
            }
            _ => {
                return Err(
                    DeliveryError::new("UNKNOWN_FIELD", "unknown music connection field")
                        .at(&field),
                )
            }
        }
    }
    if changed {
        let mut canonical = cfg.clone();
        let source = if path == "music.library" {
            "library"
        } else {
            "online"
        };
        canonical.normalize_with(source, true).map_err(|error| {
            DeliveryError::new("INVALID_MUSIC_CONFIG", error.to_string()).at(path)
        })?;
        blocks
            .entry("music".into())
            .or_default()
            .push(Operation::Replace {
                path: path.into(),
                value: serde_json::to_value(&canonical)?,
            });
        report.push(json!({
            "pointer": pointer_for(path), "action": "mapped", "operation": "replace",
            "path": path, "normalized": canonical != cfg,
        }));
    }
    Ok(())
}

pub fn convert(value: Value, state: &Value) -> Result<(Package, Value)> {
    convert_for_purpose(value, state, Purpose::NodeConfig, false)
}

pub fn convert_for_purpose(
    value: Value,
    state: &Value,
    purpose: Purpose,
    sanitize_family: bool,
) -> Result<(Package, Value)> {
    if value.get("version") == Some(&json!(2)) {
        return Ok((serde_json::from_value(value)?, json!([])));
    }
    let object = value
        .as_object()
        .ok_or_else(|| DeliveryError::new("INVALID_DOCUMENT", "package must be an object"))?;
    if object.get("format") != Some(&json!("home-mate-config"))
        || object.get("version") != Some(&json!(1))
    {
        return Err(DeliveryError::new(
            "UNSUPPORTED_VERSION",
            "explicit format/version 1 or 2 is required",
        ));
    }

    if object.contains_key("mode") && object.contains_key("mode_policy") {
        return Err(DeliveryError::new(
            "DUPLICATE_OR_MISPLACED_PATH",
            "choose either a legacy mode snapshot or a reviewed mode policy",
        )
        .at("mode"));
    }

    let mut blocks: BTreeMap<String, Vec<Operation>> = BTreeMap::new();
    let mut report = vec![];
    let mut legacy_idle_asr: Option<bool> = None;

    let mode = "mode".to_string();
    for (block, body) in object {
        let block = if block == "mode_policy" {
            let base = crate::voice::VoiceSettings::default();
            super::mode::check_scope(body, &super::mode::projection(&base), "mode")?;
            &mode
        } else {
            block
        };
        if ["format", "version", "exported_ms"].contains(&block.as_str()) {
            continue;
        }
        if block == "secrets" {
            if purpose == Purpose::FamilyPolicy {
                family_choice_or_strip(
                    "/secrets".into(),
                    "secret",
                    "secrets cannot be included in family_policy",
                    sanitize_family,
                    &mut report,
                );
                continue;
            }
            return Err(DeliveryError::new(
                "SECRET_VALUES_FORBIDDEN",
                "ordinary migration cannot restore credentials",
            )
            .at(block));
        }
        if block == "unit_acoustic_profiles" {
            if purpose == Purpose::FamilyPolicy {
                family_choice_or_strip(
                    "/unit_acoustic_profiles".into(),
                    "unit",
                    "unit acoustic profiles cannot be included in family_policy",
                    sanitize_family,
                    &mut report,
                );
                continue;
            }
            return Err(DeliveryError::new(
                "FORBIDDEN_SCOPE",
                "unit acoustic profiles are measured per-unit and cannot be imported",
            )
            .at(block));
        }
        if matches!(
            block.as_str(),
            "fleet" | "learning" | "user_md" | "working_memory" | "runtime" | "db"
        ) {
            if purpose == Purpose::FamilyPolicy {
                let scope = if matches!(block.as_str(), "learning" | "user_md" | "working_memory") {
                    "data"
                } else {
                    "boot"
                };
                family_choice_or_strip(
                    format!("/{block}"),
                    scope,
                    "protected authority cannot be included in family_policy",
                    sanitize_family,
                    &mut report,
                );
                continue;
            }
            return Err(DeliveryError::new(
                "FORBIDDEN_SCOPE",
                "protected authority cannot be imported",
            )
            .at(block));
        }
        if block == "open_sockets_source_node" {
            if purpose == Purpose::FamilyPolicy {
                family_choice_or_strip(
                    "/open_sockets_source_node".into(),
                    "node",
                    "open_sockets_source_node is node-local",
                    sanitize_family,
                    &mut report,
                );
                continue;
            }
            if body.as_str().is_none_or(|node| {
                node.is_empty() || Some(node) != state["target"]["node_id"].as_str()
            }) {
                return Err(DeliveryError::conflict(
                    "OPEN_SOCKETS_RESTORE_TARGET_MISMATCH",
                    "Open socket backups can only be restored on their source node",
                )
                .at(block));
            }
            report.push(json!({"pointer":"/open_sockets_source_node","action":"verified"}));
            continue;
        }
        if block == "open_sockets" {
            convert_path(
                "open_sockets",
                "open_sockets",
                body,
                &purpose,
                sanitize_family,
                &mut blocks,
                &mut report,
            )?;
            continue;
        }

        if block == "remote_socket" {
            report.push(json!({
                "pointer": "/remote_socket",
                "action": "ignored_retired",
                "detail": "remote_socket was retired in favor of master_brain"
            }));
            continue;
        }

        let known_object_blocks = [
            "services",
            "shared_memory",
            "persona",
            "music",
            "weather",
            "home",
            "news",
            "search",
            "openapi",
            "mode",
            "reflex",
            "audio_eq",
            "devices",
            "device_led_configs",
            "privacy",
            "node",
        ];
        if !known_object_blocks.contains(&block.as_str()) {
            return Err(
                DeliveryError::new("UNKNOWN_FIELD", "unknown legacy configuration block").at(block),
            );
        }

        let map = body.as_object().ok_or_else(|| {
            DeliveryError::new("INVALID_FIELD_TYPE", format!("{block} must be an object")).at(block)
        })?;

        if block == "openapi" && !map.contains_key("auth_header") {
            let has_binding = state["bindings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|binding| {
                    binding["binding_id"] == crate::config::OPENAPI_TOKEN_SECRET
                        && binding["bound"] == true
                });
            let credentialed = has_binding || map.get("token_set") == Some(&json!(true));
            if map.get("enabled") == Some(&json!(true))
                && credentialed
                && state["resolved"]["openapi.auth_header"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            {
                return Err(DeliveryError::new(
                    "OPENAPI_AUTH_HEADER_CHOICE",
                    "choose an authentication header or explicitly enter an empty string for Bearer only",
                ).at("openapi.auth_header"));
            }
            report.push(json!({
                "pointer": "/openapi/auth_header",
                "action": "preserved",
                "detail": "legacy backup omitted the header; target authentication was retained",
            }));
        }

        if block == "mode" {
            let a = map.get("asr_wake_fallback");
            let b = map.get("proactive_asr_wake");
            if a.is_some() && b.is_some() && a != b {
                return Err(DeliveryError::new(
                    "LEGACY_PRIVACY_CONFLICT",
                    "choose one idle-ASR value explicitly",
                )
                .at("mode"));
            }
            if let Some(v) = a.or(b) {
                let enabled = v.as_bool().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "expected boolean").at("mode")
                })?;
                legacy_idle_asr = Some(enabled);
                if purpose == Purpose::FamilyPolicy {
                    family_choice_or_strip(
                        "/privacy/idle_asr_enabled".into(),
                        "node",
                        "idle ASR setting is node-local and cannot be included in family_policy",
                        sanitize_family,
                        &mut report,
                    );
                } else {
                    blocks
                        .entry("privacy".into())
                        .or_default()
                        .push(Operation::Set {
                            path: "privacy.idle_asr_enabled".into(),
                            value: json!(enabled),
                        });
                    report.push(json!({
                        "pointer": "/mode/asr_wake_fallback",
                        "action": "mapped",
                        "operation": "set",
                        "path": "privacy.idle_asr_enabled"
                    }));
                }
            }
        }

        for (key, val) in map {
            if block == "music" && matches!(key.as_str(), "library" | "online") {
                if let Some(connection) = val.as_object() {
                    music_connection_v1(
                        &format!("music.{key}"),
                        connection,
                        state,
                        &mut blocks,
                        &mut report,
                    )?;
                    continue;
                }
            }
            if block == "mode" && (key == "asr_wake_fallback" || key == "proactive_asr_wake") {
                continue;
            }
            if block == "privacy" && key == "idle_asr_enabled" {
                if let Some(prev) = legacy_idle_asr {
                    if val.as_bool() != Some(prev) {
                        return Err(DeliveryError::new(
                            "LEGACY_PRIVACY_CONFLICT",
                            "privacy.idle_asr_enabled conflicts with legacy mode flag",
                        )
                        .at("privacy.idle_asr_enabled"));
                    }
                    continue;
                }
            }
            if block == "devices"
                || block == "device_led_configs"
                || (block == "node" && (key == "led" || key == "wake"))
                || (block == "persona" && key == "agent_md")
            {
                let entry = val.as_object().ok_or_else(|| {
                    DeliveryError::new("INVALID_FIELD_TYPE", "nested node entry must be an object")
                        .at(&format!("{block}.{key}"))
                })?;
                for (sub_k, sub_v) in entry {
                    convert_path(
                        &format!("{block}.{key}.{sub_k}"),
                        sub_k,
                        sub_v,
                        &purpose,
                        sanitize_family,
                        &mut blocks,
                        &mut report,
                    )?;
                }
                continue;
            }
            convert_path(
                &format!("{block}.{key}"),
                key,
                val,
                &purpose,
                sanitize_family,
                &mut blocks,
                &mut report,
            )?;
        }
    }

    report.push(json!({
        "action": "preserved",
        "detail": "all omitted paths, calibration, bindings and protected domains remain target-local"
    }));

    let target_node_id = if purpose == Purpose::FamilyPolicy {
        None
    } else {
        Some(
            state["target"]["node_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
        )
    };
    let package = Package {
        format: "home-mate-config".into(),
        version: 2,
        package_id: uuid()?,
        package_revision: 1,
        created_ms: crate::id::now_ms(),
        purpose,
        source: Source {
            node_id: "legacy-unspecified".into(),
            config_revision: 0,
            core_version: "legacy-v1".into(),
            schema_version: 1,
        },
        target: Target {
            household_id: state["target"]["household_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
            node_id: target_node_id,
        },
        compatibility: Compatibility {
            core_api_schema: 2,
            gateway_protocol_min: 0,
            gateway_protocol_max: 0,
            required_capabilities: vec![],
        },
        change_reason: "Review legacy v1 configuration migration".into(),
        blocks: blocks
            .into_iter()
            .map(|(name, operations)| Block {
                name,
                schema_version: 1,
                operations,
            })
            .collect(),
        bindings: vec![],
        content_hash: None,
    };
    Ok((package, Value::Array(report)))
}
