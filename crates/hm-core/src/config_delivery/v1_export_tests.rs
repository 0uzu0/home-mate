use super::*;
use crate::store::Store;
use serde_json::json;
use std::sync::Arc;

fn delivery() -> ConfigDelivery {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let svc = ConfigDelivery::new(store);
    svc.enroll("dot2-d").unwrap();
    svc
}

#[test]
fn v1_convert_supports_all_non_unit_families_and_emits_replace_for_collections() {
    let svc = delivery();
    let state = svc.state("actor-a").unwrap();
    let legacy = json!({
        "format": "home-mate-config",
        "version": 1,
        "exported_ms": 12345,
        "services": { "llm": { "model": "deepseek-chat", "api_key_set": true } },
        "shared_memory": { "enabled": true, "nas_base_url": "http://127.0.0.1:8080" },
        "persona": { "agent_md": { "content": "hello" } },
        "music": { "enabled": true, "library": null },
        "weather": { "enabled": true, "max_days": 5, "places": [{"alias": "sh", "name": "Shanghai", "latitude": 31.2, "longitude": 121.4, "timezone": "Asia/Shanghai", "city_code": "101020100"}] },
        "home": { "backend": "ha", "timeout_ms": 2000, "entities": [{"name": "lamp", "entity": "light.lamp"}], "token_set": true },
        "news": { "enabled": true, "timeout_ms": 3000, "sources": [{"alias": "rss", "url": "http://127.0.0.1/rss", "max_items": 5}] },
        "search": { "enabled": true, "count": 5, "timeout_ms": 4000, "api_key_set": false },
        "openapi": { "enabled": false, "timeout_ms": 5000, "retries": 1, "operations": [{"id": "op1", "path": "/v1/ping"}], "token_set": false },
        "mode": { "assistant_name": "Luna", "retreat_words": ["退下"], "spoken":{"detail_words":["讲讲"],"long_form_words":["故事"],"max_chars":96}, "asr_wake_fallback": false, "proactive_asr_wake": false, "duplex_requested": {"dot2-d": "half"} },
        "reflex": { "words": [{"text": "停", "tokens": "t ing", "intent": "stop", "enabled": true}] },
        "audio_eq": { "curve": "echomuse", "bands": [{"freq": 1000.0, "gain_db": 0.0, "q": 1.0}] },
        "devices": { "dot2-d": { "name": "Living Room", "room": "living" } },
        "remote_socket": { "enabled": false, "timeout_ms": 3000, "capabilities": [{"name": "ping"}] },
        "open_sockets": [{"name": "s1", "description": "d", "effect": "Query", "endpoint_url": "http://127.0.0.1/s1"}],
        "device_led_configs": { "dot2-d": { "night_enabled": true, "night_start_minute": 1320, "night_end_minute": 420 } },
        "node": { "led": { "timezone": "Asia/Shanghai" }, "wake": { "phrases": ["你好小智"] } }
    });

    let (pkg, report) = v1::convert(legacy, &state).unwrap();
    assert_eq!(pkg.compatibility.core_api_schema, 2);
    assert!(report.as_array().unwrap().len() > 10);

    let has_replace = |block_name: &str, target_path: &str| {
        pkg.blocks
            .iter()
            .find(|b| b.name == block_name)
            .is_some_and(|b| {
                b.operations
                    .iter()
                    .any(|op| op.action() == "replace" && op.path() == target_path)
            })
    };
    assert!(has_replace("weather", "weather.places"));
    assert!(has_replace("open_sockets", "open_sockets"));
    assert!(report.as_array().unwrap().iter().any(|entry| {
        entry["pointer"] == "/remote_socket" && entry["action"] == "ignored_retired"
    }));
    assert!(has_replace("mode", "mode.retreat_words"));
    for path in ["detail_words", "long_form_words"] {
        assert!(!has_replace("mode", &format!("mode.spoken.{path}")));
        assert!(report
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["pointer"] == format!("/mode/spoken/{path}")
                && entry["action"] == "preserved"));
    }
    assert!(has_replace("mode", "mode.duplex_requested"));
    assert!(pkg
        .blocks
        .iter()
        .find(|b| b.name == "privacy")
        .is_some_and(|b| b
            .operations
            .iter()
            .any(|op| op.path() == "privacy.idle_asr_enabled")));

    // Reviewed sockets need the actual attached Registry.
    let err = svc
        .preflight("actor-a", pkg.clone(), report.clone())
        .unwrap_err();
    assert_eq!(err.code, "OPEN_SOCKETS_RUNTIME_UNAVAILABLE");
    let mut remaining = pkg;
    remaining
        .blocks
        .retain(|block| block.name != "open_sockets");
    let err = svc.preflight("actor-a", remaining, report).unwrap_err();
    assert_eq!(err.code, "UNSUPPORTED_BLOCK");
}

#[test]
fn v1_convert_rejects_privacy_conflict_secrets_unknown_and_unit_acoustics() {
    let svc = delivery();
    let state = svc.state("actor-a").unwrap();

    let conflict = json!({
        "format": "home-mate-config",
        "version": 1,
        "mode": { "asr_wake_fallback": true, "proactive_asr_wake": false }
    });
    assert_eq!(
        v1::convert(conflict, &state).unwrap_err().code,
        "LEGACY_PRIVACY_CONFLICT"
    );

    let secret_block = json!({
        "format": "home-mate-config",
        "version": 1,
        "secrets": { "llm.api_key": "sk-secret" }
    });
    assert_eq!(
        v1::convert(secret_block, &state).unwrap_err().code,
        "SECRET_VALUES_FORBIDDEN"
    );

    let secret_field = json!({
        "format": "home-mate-config",
        "version": 1,
        "services": { "llm": { "api_key": "sk-secret" } }
    });
    assert_eq!(
        v1::convert(secret_field, &state).unwrap_err().code,
        "SECRET_VALUES_FORBIDDEN"
    );

    let unknown_block = json!({
        "format": "home-mate-config",
        "version": 1,
        "mystery": {}
    });
    assert_eq!(
        v1::convert(unknown_block, &state).unwrap_err().code,
        "UNKNOWN_FIELD"
    );

    let unknown_elem = json!({
        "format": "home-mate-config",
        "version": 1,
        "weather": { "places": [{"alias": "sh", "bogus": 1}] }
    });
    assert_eq!(
        v1::convert(unknown_elem, &state).unwrap_err().code,
        "UNKNOWN_FIELD"
    );

    let unit_block = json!({
        "format": "home-mate-config",
        "version": 1,
        "unit_acoustic_profiles": { "dot2-d": { "volume_cap": 0.5 } }
    });
    assert_eq!(
        v1::convert(unit_block, &state).unwrap_err().code,
        "FORBIDDEN_SCOPE"
    );
}

#[test]
fn replace_operation_is_rejected_on_scalar_and_schema_1_is_rejected() {
    let svc = delivery();
    let state = svc.state("actor-a").unwrap();
    let household_id = state["target"]["household_id"]
        .as_str()
        .unwrap()
        .to_string();

    let bad_schema: Package = serde_json::from_value(json!({
        "format": "home-mate-config",
        "version": 2,
        "package_id": uuid().unwrap(),
        "package_revision": 1,
        "created_ms": 1000,
        "purpose": "node_config",
        "source": { "node_id": "dot2-d", "config_revision": 1, "core_version": "0.1", "schema_version": 1 },
        "target": { "household_id": household_id, "node_id": "dot2-d" },
        "compatibility": { "core_api_schema": 1, "gateway_protocol_min": 0, "gateway_protocol_max": 0, "required_capabilities": [] },
        "change_reason": "test schema 1",
        "blocks": [{ "name": "services", "schema_version": 1, "operations": [{ "action": "set", "path": "services.llm.model", "value": "deepseek-chat" }] }],
        "bindings": []
    }))
    .unwrap();
    assert_eq!(
        svc.preflight("actor-a", bad_schema, json!([]))
            .unwrap_err()
            .code,
        "UNSUPPORTED_CAPABILITY"
    );

    let bad_replace: Package = serde_json::from_value(json!({
        "format": "home-mate-config",
        "version": 2,
        "package_id": uuid().unwrap(),
        "package_revision": 1,
        "created_ms": 1000,
        "purpose": "node_config",
        "source": { "node_id": "dot2-d", "config_revision": 1, "core_version": "0.1", "schema_version": 1 },
        "target": { "household_id": household_id, "node_id": "dot2-d" },
        "compatibility": { "core_api_schema": 2, "gateway_protocol_min": 0, "gateway_protocol_max": 0, "required_capabilities": [] },
        "change_reason": "test replace on scalar",
        "blocks": [{ "name": "services", "schema_version": 1, "operations": [{ "action": "replace", "path": "services.llm.model", "value": "deepseek-chat" }] }],
        "bindings": []
    }))
    .unwrap();
    assert_eq!(
        svc.preflight("actor-a", bad_replace, json!([]))
            .unwrap_err()
            .code,
        "INVALID_OPERATION"
    );
}
