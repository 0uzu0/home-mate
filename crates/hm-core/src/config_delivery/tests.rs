use super::*;
use crate::store::Store;
use serde_json::{json, Value};
use std::sync::Arc;

fn fixture() -> (Arc<Store>, ConfigDelivery) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.allow_loopback_public_for_test();
    let service = ConfigDelivery::new(store.clone());
    service.enroll("test-a").unwrap();
    (store, service)
}
fn package(service: &ConfigDelivery, operations: Value) -> Package {
    let state = service.state("actor-a").unwrap();
    serde_json::from_value(json!({"format":"home-mate-config","version":2,"package_id":uuid().unwrap(),"package_revision":1,"created_ms":1000,
        "purpose":"node_config","source":{"node_id":"test-a","config_revision":state["config_revision"],"core_version":"test","schema_version":10},
        "target":{"household_id":state["target"]["household_id"],"node_id":"test-a"},
        "compatibility":{"core_api_schema":2,"gateway_protocol_min":0,"gateway_protocol_max":0,"required_capabilities":[]},
        "change_reason":"Synthetic behavior verification","blocks":[{"name":"persona","schema_version":1,"operations":operations}],"bindings":[]})).unwrap()
}
fn plan(service: &ConfigDelivery) -> Value {
    let package = package(
        service,
        json!([{"action":"set","path":"persona.agent_md.content","value":"new persona"}]),
    );
    service
        .preflight_at("actor-a", package, json!([]), 1000)
        .unwrap()
}
fn commit(plan: &Value) -> Commit {
    Commit {
        plan_id: plan["plan_id"].as_str().unwrap().into(),
        plan_hash: plan["plan_hash"].as_str().unwrap().into(),
        expected_revision: plan["expected_revision"].as_u64().unwrap(),
        application_id: uuid().unwrap(),
    }
}
#[test]
fn reviewed_transaction_replays_and_principal_is_bound() {
    let (_, service) = fixture();
    let p = plan(&service);
    let request = commit(&p);
    assert_eq!(
        service
            .commit_at("actor-b", &request, 1001, None)
            .unwrap_err()
            .code,
        "PLAN_AUTHORITY_MISMATCH"
    );
    let result = service.commit_at("actor-a", &request, 1001, None).unwrap();
    assert_eq!(result["state"], "saved_pending");
    assert_eq!(
        service.state("actor-a").unwrap()["resolved"]["persona.agent_md.content"],
        "new persona"
    );
    assert_eq!(
        service.commit_at("actor-a", &request, 1002, None).unwrap(),
        result
    );
    let other = Commit {
        plan_hash: "different".into(),
        ..request
    };
    assert_eq!(
        service
            .commit_at("actor-a", &other, 1003, None)
            .unwrap_err()
            .code,
        "IDEMPOTENCY_CONFLICT"
    );
    assert!(service
        .application("actor-b", result["application_id"].as_str().unwrap())
        .is_err());
}
#[test]
fn stale_legacy_writer_expiry_and_epoch_never_commit() {
    for variant in ["legacy", "expiry", "epoch"] {
        let (store, service) = fixture();
        let p = plan(&service);
        let request = commit(&p);
        if variant == "legacy" {
            store.config_set("other_configuration", "changed").unwrap();
        }
        if variant == "epoch" {
            store
                .with(|c| {
                    c.execute(
                        "UPDATE config_head SET data_epoch=data_epoch+1 WHERE id=1",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let now = if variant == "expiry" { 601_000 } else { 1001 };
        assert_eq!(
            service
                .commit_at("actor-a", &request, now, None)
                .unwrap_err()
                .code,
            "PLAN_STALE"
        );
        assert_eq!(
            service.state("actor-a").unwrap()["stored"]["persona.agent_md.content"],
            ""
        );
    }
}
#[test]
fn every_transaction_write_boundary_rolls_back() {
    let mut reached_success = false;
    for boundary in 1..12 {
        let (store, service) = fixture();
        let p = plan(&service);
        let request = commit(&p);
        let before = service.state("actor-a").unwrap();
        if service
            .commit_at("actor-a", &request, 1001, Some(boundary))
            .is_ok()
        {
            reached_success = true;
            break;
        }
        let after = service.state("actor-a").unwrap();
        assert_eq!(before["stored"], after["stored"], "boundary {boundary}");
        assert_eq!(
            before["config_revision"], after["config_revision"],
            "boundary {boundary}"
        );
        assert_eq!(after["applications"], json!([]));
        store
            .with(|c| {
                for table in [
                    "config_applications",
                    "config_application_blocks",
                    "config_outbox",
                ] {
                    let n: i64 =
                        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                    assert_eq!(n, 0, "{table} boundary {boundary}");
                }
                Ok(())
            })
            .unwrap();
    }
    assert!(reached_success);
}
#[test]
fn family_keeps_legacy_node_overrides_until_explicit_inherit() {
    let (_, service) = fixture();
    let mut pkg = package(
        &service,
        json!([{"action":"set","path":"persona.agent_md.content","value":"family"}]),
    );
    pkg.purpose = Purpose::FamilyPolicy;
    pkg.target.node_id = None;
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap();
    assert_eq!(p["diff"][0]["after"], "");
    assert_eq!(p["diff"][0]["layer_after"], "family");
    let family = service
        .commit_at("actor-a", &commit(&p), 1001, None)
        .unwrap();
    assert_eq!(
        service
            .readback("actor-a", family["application_id"].as_str().unwrap())
            .unwrap()["fields"][0]["state"],
        "shadowed"
    );
    let pkg = package(
        &service,
        json!([{"action":"clear","path":"persona.agent_md.content"}]),
    );
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1002)
        .unwrap();
    assert_eq!(p["diff"][0]["after"], "family");
    service
        .commit_at("actor-a", &commit(&p), 1003, None)
        .unwrap();
    assert_eq!(
        service.state("actor-a").unwrap()["resolved"]["persona.agent_md.content"],
        "family"
    );
}
#[test]
fn strict_unknown_duplicate_protected_and_late_invalid_have_no_effect() {
    assert!(v1::parse(br#"{"a":{"x":1,"x":2}}"#).is_err());
    let (_, service) = fixture();
    let before = service.state("actor-a").unwrap();
    let pkg = package(
        &service,
        json!([{"action":"set","path":"persona.agent_md.content","value":"would-change"},{"action":"set","path":"persona.secret","value":"bad"}]),
    );
    assert_eq!(
        service
            .preflight_at("actor-a", pkg, json!([]), 1000)
            .unwrap_err()
            .code,
        "UNKNOWN_FIELD"
    );
    let mut pkg = package(&service, json!([]));
    pkg.blocks = vec![Block {
        name: "services".into(),
        schema_version: 1,
        operations: vec![Operation::Set {
            path: "services.llm.api_key_env".into(),
            value: json!("PRIVATE_ENV"),
        }],
    }];
    pkg.purpose = Purpose::FamilyPolicy;
    pkg.target.node_id = None;
    assert_eq!(
        service
            .preflight_at("actor-a", pkg, json!([]), 1000)
            .unwrap_err()
            .code,
        "FORBIDDEN_SCOPE"
    );
    assert_eq!(
        service.state("actor-a").unwrap()["stored"],
        before["stored"]
    );
}
#[test]
fn v1_adapter_preserves_omissions_and_blocks_full_unsupported_input() {
    let (_, service) = fixture();
    let state = service.state("actor-a").unwrap();
    let (pkg,report)=v1::convert(json!({"format":"home-mate-config","version":1,"services":{"llm":{"model":"new-model","api_key_set":true}}}),&state).unwrap();
    assert_eq!(pkg.blocks[0].operations.len(), 1);
    assert!(report.as_array().unwrap().len() > 1);
    let before = state["stored"].clone();
    let p = service.preflight_at("actor-a", pkg, report, 1000).unwrap();
    service
        .commit_at("actor-a", &commit(&p), 1001, None)
        .unwrap();
    let after = service.state("actor-a").unwrap()["stored"].clone();
    assert_eq!(
        before["services.llm.base_url"],
        after["services.llm.base_url"]
    );
    assert!(v1::convert(json!({"format":"home-mate-config","version":1,"services":{"llm":{"model":"ignored"}},"mode":{"media_trim_db":-2}}),&state).is_err());
}
#[test]
fn factory_reset_redacts_all_config_payload_and_rejects_old_plan() {
    let (store, service) = fixture();
    let p = plan(&service);
    let request = commit(&p);
    service.commit_at("actor-a", &request, 1001, None).unwrap();
    let pending = plan(&service);
    let pending_request = commit(&pending);
    store.factory_reset(false).unwrap();
    assert_eq!(
        service
            .commit_at("actor-a", &commit(&p), 1001, None)
            .unwrap_err()
            .code,
        "PLAN_STALE"
    );
    assert_eq!(
        crate::agent_md::AgentMdService::new(store.clone()).current_content(),
        ""
    );
    assert!(service
        .commit_at("actor-a", &pending_request, 1002, None)
        .is_err());
    let state = service.state("actor-a").unwrap();
    assert_eq!(state["stored"]["persona.agent_md.content"], "");
    store
        .with(|c| {
            for table in [
                "config_plans",
                "config_applications",
                "config_application_blocks",
                "config_outbox",
            ] {
                let n: i64 =
                    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(n, 0);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn every_transaction_boundary_rolls_back_desired_persona_journal_and_outbox() {
    let (store, service) = fixture();
    let p = plan(&service);
    let request = commit(&p);
    let revision = p["expected_revision"].as_u64().unwrap();
    for boundary in 1..=5 {
        assert_eq!(
            service
                .commit_at("actor-a", &request, 1001, Some(boundary))
                .unwrap_err()
                .code,
            "CONFIG_STORAGE_ERROR"
        );
        let state = service.state("actor-a").unwrap();
        assert_eq!(state["config_revision"], revision);
        assert_eq!(state["stored"]["persona.agent_md.content"], "");
        assert_eq!(state["applications"], json!([]));
        store
            .with(|c| {
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM config_outbox", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    c.query_row("SELECT applying FROM config_head WHERE id=1", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    service.commit_at("actor-a", &request, 1001, None).unwrap();
}

#[test]
fn ttl_epoch_and_legacy_and_secret_writes_fence_reviewed_plan() {
    let (store, service) = fixture();
    let p = plan(&service);
    assert_eq!(
        service
            .commit_at("actor-a", &commit(&p), 601000, None)
            .unwrap_err()
            .code,
        "PLAN_STALE"
    );
    store
        .config_set("voice_mode", r#"{"media_trim_db":-3}"#)
        .unwrap();
    assert_eq!(
        service
            .commit_at("actor-a", &commit(&p), 1001, None)
            .unwrap_err()
            .code,
        "PLAN_STALE"
    );
    let p = plan(&service);
    store
        .with(|c| {
            c.execute(
                "INSERT INTO secrets(name,value) VALUES ('llm.api_key','synthetic-only')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        service
            .commit_at("actor-a", &commit(&p), 1001, None)
            .unwrap_err()
            .code,
        "PLAN_STALE"
    );
    let p = plan(&service);
    store
        .with(|c| {
            c.execute(
                "UPDATE config_head SET data_epoch=data_epoch+1 WHERE id=1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        service
            .commit_at("actor-a", &commit(&p), 1001, None)
            .unwrap_err()
            .code,
        "PLAN_STALE"
    );
}

#[test]
fn concurrent_distinct_applications_have_one_winning_cas_commit() {
    let (_, service) = fixture();
    let p = plan(&service);
    let service = Arc::new(service);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let service = service.clone();
            let barrier = barrier.clone();
            let request = commit(&p);
            std::thread::spawn(move || {
                barrier.wait();
                service.commit_at("actor-a", &request, 1001, None)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results.into_iter().find_map(|r| r.err()).unwrap().code,
        "PLAN_STALE"
    );
    assert_eq!(
        service.applications("actor-a").unwrap()["applications"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn journal_plan_and_layers_survive_reopen_and_replay() {
    let dir = std::env::temp_dir().join(format!("hm-config-v2-{}", uuid().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.db");
    let store = Arc::new(Store::open(&path).unwrap());
    let service = ConfigDelivery::new(store.clone());
    service.enroll("test-a").unwrap();
    let p = plan(&service);
    let request = commit(&p);
    let result = service.commit_at("actor-a", &request, 1001, None).unwrap();
    drop(service);
    drop(store);
    let store = Arc::new(Store::open(&path).unwrap());
    let service = ConfigDelivery::new(store.clone());
    assert_eq!(
        service.commit_at("actor-a", &request, 1002, None).unwrap(),
        result
    );
    assert_eq!(
        service
            .application("actor-a", &request.application_id)
            .unwrap(),
        result
    );
    assert_eq!(
        service.state("actor-a").unwrap()["resolved"]["persona.agent_md.content"],
        "new persona"
    );
    assert_eq!(
        service
            .readback("actor-a", &request.application_id)
            .unwrap()["fields"][0]["state"],
        "saved_pending"
    );
    assert_eq!(
        crate::agent_md::AgentMdService::new(store.clone()).current_content(),
        ""
    );
    super::worker::run_once(&store).unwrap();
    assert_eq!(
        service
            .readback("actor-a", &request.application_id)
            .unwrap()["fields"][0]["state"],
        "core_readback_verified"
    );
    service
        .rollback_preflight(
            "actor-a",
            &request.application_id,
            result["config_revision"].as_u64().unwrap(),
        )
        .unwrap();
    drop(service);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn all_three_supported_blocks_rollback_at_every_persistence_boundary() {
    let (store, service) = fixture();
    let mut pkg = package(
        &service,
        json!([{"action":"set","path":"persona.agent_md.content","value":"all-blocks"}]),
    );
    pkg.blocks.push(Block {
        name: "services".into(),
        schema_version: 1,
        operations: vec![Operation::Set {
            path: "services.llm.model".into(),
            value: json!("new-model"),
        }],
    });
    pkg.blocks.push(Block {
        name: "shared_memory".into(),
        schema_version: 1,
        operations: vec![Operation::Set {
            path: "shared_memory.nas_base_url".into(),
            value: json!("http://nas.example"),
        }],
    });
    let before = service.state("actor-a").unwrap();
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap();
    let request = commit(&p);
    for boundary in 1..=8 {
        service
            .commit_at("actor-a", &request, 1001, Some(boundary))
            .unwrap_err();
        let state = service.state("actor-a").unwrap();
        assert_eq!(state["stored"], before["stored"]);
        assert_eq!(state["config_revision"], before["config_revision"]);
        assert_eq!(state["applications"], json!([]));
        store
            .with(|c| {
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM config_outbox", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
    service.commit_at("actor-a", &request, 1001, None).unwrap();
    let state = service.state("actor-a").unwrap();
    assert_eq!(state["stored"]["services.llm.model"], "deepseek-chat");
    assert_eq!(state["resolved"]["services.llm.model"], "new-model");
    assert_eq!(
        state["stored"]["shared_memory.nas_base_url"],
        "http://nas.example"
    );
    assert_eq!(state["stored"]["persona.agent_md.content"], "");
    assert_eq!(state["resolved"]["persona.agent_md.content"], "all-blocks");
}

#[test]
fn url_credentials_are_rejected_even_for_disabled_services_before_plan_persistence() {
    let (store, service) = fixture();
    for (block, path, url) in [
        (
            "services",
            "services.asr.base_url",
            "https://name:synthetic-secret@example.test",
        ),
        (
            "services",
            "services.asr.base_url",
            "https://example.test?api_key=synthetic-secret",
        ),
        (
            "shared_memory",
            "shared_memory.nas_base_url",
            "https://name:synthetic-secret@example.test",
        ),
        (
            "shared_memory",
            "shared_memory.nas_base_url",
            "http://[malformed",
        ),
    ] {
        let mut pkg = package(&service, json!([]));
        pkg.blocks = vec![Block {
            name: block.into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: path.into(),
                value: json!(url),
            }],
        }];
        assert_eq!(
            service
                .preflight_at("actor-a", pkg, json!([]), 1000)
                .unwrap_err()
                .code,
            "INVALID_SERVICE_URL"
        );
    }
    store
        .with(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM config_plans", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn authentication_usage_metadata_does_not_stale_plan_but_real_config_does() {
    let (store, service) = fixture();
    let p = plan(&service);
    store
        .config_set("api_tokens", r#"[{"last_used_ms":5000}]"#)
        .unwrap();
    service
        .commit_at("actor-a", &commit(&p), 1001, None)
        .unwrap();
    let p = plan(&service);
    store
        .config_set("services", r#"{"llm":{"provider":"unconfigured"}}"#)
        .unwrap();
    assert_eq!(
        service
            .commit_at("actor-a", &commit(&p), 1001, None)
            .unwrap_err()
            .code,
        "PLAN_STALE"
    );
}

#[test]
fn shadowed_family_values_are_validated_before_being_stored_in_layers() {
    let (store, service) = fixture();
    for (block, path, value) in [
        (
            "services",
            "services.asr.base_url",
            json!("https://user:synthetic-secret@example.test"),
        ),
        (
            "persona",
            "persona.agent_md.content",
            json!("x".repeat(crate::agent_md::MAX_AGENT_MD_CHARS + 1)),
        ),
    ] {
        let mut pkg = package(&service, json!([]));
        pkg.purpose = Purpose::FamilyPolicy;
        pkg.target.node_id = None;
        pkg.blocks = vec![Block {
            name: block.into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: path.into(),
                value,
            }],
        }];
        assert!(service
            .preflight_at("actor-a", pkg, json!([]), 1000)
            .is_err());
    }
    store
        .with(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM config_plans", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn partial_legacy_target_uses_runtime_defaults_and_preserves_unknown_existing_storage() {
    let (store, service) = fixture();
    store
        .config_set(
            "services",
            r#"{"llm":{"provider":"unconfigured","future_local_metadata":"keep"}}"#,
        )
        .unwrap();
    let state = service.state("actor-a").unwrap();
    assert_eq!(state["stored"]["services.llm.base_url"], "");
    let mut pkg = package(&service, json!([]));
    pkg.blocks = vec![Block {
        name: "services".into(),
        schema_version: 1,
        operations: vec![Operation::Set {
            path: "services.llm.model".into(),
            value: json!("selected-model"),
        }],
    }];
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap();
    service
        .commit_at("actor-a", &commit(&p), 1001, None)
        .unwrap();
    super::worker::run_once(&store).unwrap();
    let raw: Value = serde_json::from_str(&store.config_get("services").unwrap().unwrap()).unwrap();
    assert_eq!(raw["llm"]["future_local_metadata"], "keep");
    assert_eq!(raw["llm"]["base_url"], "");
}

#[test]
fn missing_binding_blocks_plan_and_noop_does_not_queue_projection() {
    let (store, service) = fixture();
    let mut pkg = package(
        &service,
        json!([{"action":"preserve","path":"persona.agent_md.content"}]),
    );
    pkg.bindings = vec![Binding::RequireBinding {
        binding_id: "tts.api_key".into(),
    }];
    assert_eq!(
        service
            .preflight_at("actor-a", pkg.clone(), json!([]), 1000)
            .unwrap_err()
            .code,
        "BINDING_REQUIRED"
    );
    pkg.bindings = vec![Binding::Preserve {
        binding_id: "tts.api_key".into(),
    }];
    let before = service.state("actor-a").unwrap();
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap();
    let result = service
        .commit_at("actor-a", &commit(&p), 1001, None)
        .unwrap();
    assert_eq!(result["state"], "unchanged");
    assert_eq!(
        service.state("actor-a").unwrap()["config_revision"],
        before["config_revision"]
    );
    store
        .with(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM config_outbox", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn native_readback_keeps_receipt_immutable_and_rollback_is_reviewed_cas() {
    let (store, service) = fixture();
    let p = plan(&service);
    let request = commit(&p);
    let receipt = service.commit_at("actor-a", &request, 1001, None).unwrap();
    assert_eq!(
        service
            .readback("actor-a", &request.application_id)
            .unwrap()["fields"][0]["state"],
        "saved_pending"
    );
    super::worker::run_once(&store).unwrap();
    let readback = service
        .readback("actor-a", &request.application_id)
        .unwrap();
    assert_eq!(readback["initial_receipt"], receipt);
    assert_eq!(readback["fields"][0]["state"], "core_readback_verified");
    assert_eq!(readback["fields"][0]["value"], "new persona");
    assert_eq!(readback["publication_isolation"], "persona");
    assert_eq!(readback["publication"]["state"], "applied");
    store
        .with(|c| {
            let payload: String = c.query_row(
                "SELECT payload_json FROM config_outbox WHERE application_id=?1",
                [&request.application_id],
                |r| r.get(0),
            )?;
            let payload: Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(payload["revision"], receipt["config_revision"]);
            assert_eq!(payload["required_projection"], "saved_pending");
            assert_eq!(payload["readback"], readback);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        service
            .application("actor-a", &request.application_id)
            .unwrap(),
        receipt
    );
    assert_eq!(
        service.commit_at("actor-a", &request, 1002, None).unwrap(),
        receipt
    );
    assert!(service
        .readback("actor-b", &request.application_id)
        .is_err());
    let rollback = service
        .rollback_preflight(
            "actor-a",
            &request.application_id,
            receipt["config_revision"].as_u64().unwrap(),
        )
        .unwrap();
    assert_eq!(rollback["diff"][0]["after"], "");
    assert_eq!(
        crate::config::ConfigService::new(store.clone())
            .prompt_preferences()
            .unwrap()
            .agent_md,
        "new persona",
        "preview must not publish"
    );
    let restored = service.commit("actor-a", &commit(&rollback)).unwrap();
    assert_eq!(
        crate::agent_md::AgentMdService::new(store.clone()).current_content(),
        "new persona"
    );
    super::worker::run_once(&store).unwrap();
    assert_eq!(
        crate::config::ConfigService::new(store.clone())
            .prompt_preferences()
            .unwrap()
            .agent_md,
        ""
    );
    assert_eq!(
        service
            .readback("actor-a", &request.application_id)
            .unwrap()["state"],
        "superseded"
    );
    assert_eq!(
        service
            .rollback_preflight(
                "actor-a",
                &request.application_id,
                restored["config_revision"].as_u64().unwrap()
            )
            .unwrap_err()
            .code,
        "ROLLBACK_STALE"
    );
}

#[test]
fn readback_distinguishes_unused_inactive_and_dependency_after_services_publication() {
    let (store, service) = fixture();
    let mut pkg = package(&service, json!([]));
    pkg.blocks = vec![
        Block {
            name: "services".into(),
            schema_version: 1,
            operations: vec![
                Operation::Set {
                    path: "services.llm.voice".into(),
                    value: json!("unused"),
                },
                Operation::Set {
                    path: "services.asr.model".into(),
                    value: json!("not-enabled"),
                },
            ],
        },
        Block {
            name: "shared_memory".into(),
            schema_version: 1,
            operations: vec![Operation::Set {
                path: "shared_memory.nas_base_url".into(),
                value: json!("http://nas.example"),
            }],
        },
    ];
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap();
    let request = commit(&p);
    let receipt = service.commit_at("actor-a", &request, 1001, None).unwrap();
    assert_eq!(
        service
            .readback("actor-a", &request.application_id)
            .unwrap()["fields"][0]["state"],
        "saved_pending"
    );
    super::worker::run_once(&store).unwrap();
    let proof = service
        .readback("actor-a", &request.application_id)
        .unwrap();
    assert_eq!(proof["fields"][0]["state"], "not_consumed");
    assert_eq!(proof["fields"][1]["state"], "inactive");
    assert_eq!(proof["fields"][2]["state"], "pending_dependency");
    assert_eq!(proof["fields"][2]["dependency"], "#107");
    assert_eq!(
        service
            .rollback_preflight(
                "actor-a",
                &request.application_id,
                receipt["config_revision"].as_u64().unwrap()
            )
            .unwrap_err()
            .code,
        "ROLLBACK_UNVERIFIED"
    );
}

#[test]
fn later_secret_or_legacy_writes_and_reset_fence_live_evidence_and_rollback() {
    for kind in ["legacy", "secret", "reset"] {
        let (store, service) = fixture();
        let p = plan(&service);
        let request = commit(&p);
        let receipt = service.commit_at("actor-a", &request, 1001, None).unwrap();
        if kind == "legacy" {
            store.config_set("unrelated", "changed").unwrap();
        }
        if kind == "secret" {
            store.secret_set("llm.api_key", "synthetic-change").unwrap();
        }
        if kind == "reset" {
            store.factory_reset(false).unwrap();
        }
        if kind == "reset" {
            assert!(service
                .readback("actor-a", &request.application_id)
                .is_err());
        } else {
            assert_eq!(
                service
                    .readback("actor-a", &request.application_id)
                    .unwrap()["state"],
                "superseded"
            );
        }
        assert!(service
            .rollback_preflight(
                "actor-a",
                &request.application_id,
                receipt["config_revision"].as_u64().unwrap()
            )
            .is_err());
    }
}

#[test]
fn evidence_write_failure_never_changes_immutable_receipt() {
    let (store, service) = fixture();
    let p = plan(&service);
    let request = commit(&p);
    let receipt = service.commit_at("actor-a", &request, 1001, None).unwrap();
    store.with(|c| {c.execute_batch("CREATE TRIGGER fail_evidence BEFORE UPDATE ON config_outbox BEGIN SELECT RAISE(ABORT,'injected evidence write failure'); END;")?;Ok(())}).unwrap();
    assert!(service
        .readback("actor-a", &request.application_id)
        .is_err());
    assert_eq!(
        service
            .application("actor-a", &request.application_id)
            .unwrap(),
        receipt
    );
    assert_eq!(
        service.commit_at("actor-a", &request, 1002, None).unwrap(),
        receipt
    );
}

#[test]
fn persona_desired_is_isolated_and_prepared_old_workers_are_fenced() {
    for later in ["desired", "legacy_persona", "legacy_other", "epoch"] {
        let (store, service) = fixture();
        let first = commit(&plan(&service));
        service.commit_at("actor-a", &first, 1001, None).unwrap();
        let jobs = super::worker::prepare(&store).unwrap();
        let native = crate::agent_md::AgentMdService::new(store.clone());
        assert_eq!(native.current_content(), "");
        for _ in 0..3 {
            let state = service.state("actor-a").unwrap();
            assert_eq!(state["stored"]["persona.agent_md.content"], "");
            assert_eq!(state["resolved"]["persona.agent_md.content"], "new persona");
        }
        if later == "desired" {
            let pkg = package(
                &service,
                json!([{"action":"set","path":"persona.agent_md.content","value":"newest"}]),
            );
            let p = service
                .preflight_at("actor-a", pkg, json!([]), 1002)
                .unwrap();
            service
                .commit_at("actor-a", &commit(&p), 1003, None)
                .unwrap();
        } else if later == "legacy_persona" {
            native.update("legacy edit", "console", "").unwrap();
        } else if later == "legacy_other" {
            store.config_set("unrelated", "edit").unwrap();
        } else {
            store
                .with(|c| {
                    c.execute(
                        "UPDATE config_head SET data_epoch=data_epoch+1 WHERE id=1",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        for job in jobs {
            super::worker::publish(&store, job).unwrap();
        }
        assert_ne!(
            native.current_content(),
            "new persona",
            "old worker: {later}"
        );
        if later != "epoch" {
            assert_eq!(
                service.readback("actor-a", &first.application_id).unwrap()["fields"][0]["state"],
                "superseded"
            );
        }
        if later == "desired" {
            super::worker::run_once(&store).unwrap();
            assert_eq!(native.current_content(), "newest");
        }
        if later == "legacy_other" {
            assert_eq!(
                service.state("actor-a").unwrap()["resolved"]["persona.agent_md.content"],
                "new persona",
                "unrelated edits must retain desired"
            );
            let p = plan(&service);
            service
                .commit_at("actor-a", &commit(&p), 1004, None)
                .unwrap();
            super::worker::run_once(&store).unwrap();
            assert_eq!(native.current_content(), "new persona");
        }
        if later == "legacy_persona" {
            assert_eq!(
                service.state("actor-a").unwrap()["resolved"]["persona.agent_md.content"],
                "legacy edit"
            );
        }
    }
}

#[test]
fn persona_failure_keeps_old_effective_and_reviewed_retry_can_publish() {
    let (store, service) = fixture();
    let request = commit(&plan(&service));
    let receipt = service.commit_at("actor-a", &request, 1001, None).unwrap();
    store.with(|c| {c.execute_batch("CREATE TRIGGER fail_persona BEFORE INSERT ON agent_md_revisions BEGIN SELECT RAISE(ABORT,'synthetic private SQLite detail'); END;")?;Ok(())}).unwrap();
    super::worker::run_once(&store).unwrap();
    let proof = service
        .readback("actor-a", &request.application_id)
        .unwrap();
    assert_eq!(proof["publication"]["state"], "failed");
    assert_eq!(proof["publication"]["error"], "PERSONA_PUBLICATION_FAILED");
    assert_eq!(proof["fields"][0]["value"], "");
    assert_eq!(proof["fields"][0]["state"], "failed");
    assert!(!proof.to_string().contains("private SQLite"));
    assert_eq!(
        service
            .application("actor-a", &request.application_id)
            .unwrap(),
        receipt
    );
    assert_eq!(
        service.state("actor-a").unwrap()["resolved"]["persona.agent_md.content"],
        "new persona"
    );
    store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_persona;")?;
            Ok(())
        })
        .unwrap();
    let p = plan(&service);
    let retry = commit(&p);
    service.commit_at("actor-a", &retry, 1003, None).unwrap();
    super::worker::run_once(&store).unwrap();
    assert_eq!(
        service.readback("actor-a", &retry.application_id).unwrap()["fields"][0]["state"],
        "core_readback_verified"
    );
}

#[test]
fn pending_persona_is_carried_by_later_service_commit_and_blocks_publish_independently() {
    let (store, service) = fixture();
    let first = commit(&plan(&service));
    service.commit_at("actor-a", &first, 1001, None).unwrap();
    let mut pkg = package(&service, json!([]));
    pkg.blocks = vec![Block {
        name: "services".into(),
        schema_version: 1,
        operations: vec![Operation::Set {
            path: "services.llm.model".into(),
            value: json!("later-model"),
        }],
    }];
    let p = service
        .preflight_at("actor-a", pkg, json!([]), 1002)
        .unwrap();
    let second = commit(&p);
    service.commit_at("actor-a", &second, 1003, None).unwrap();
    assert_eq!(
        crate::agent_md::AgentMdService::new(store.clone()).current_content(),
        ""
    );
    super::worker::run_once(&store).unwrap();
    assert_eq!(
        crate::agent_md::AgentMdService::new(store.clone()).current_content(),
        "new persona"
    );
    store.with(|c| {
        let state:String=c.query_row("SELECT state FROM config_outbox WHERE application_id=?1",[&second.application_id],|r|r.get(0))?;
        assert_eq!(state,"applied");
        let state:String=c.query_row("SELECT state FROM config_application_blocks WHERE application_id=?1 AND block='services'",[&second.application_id],|r|r.get(0))?;
        assert_eq!(state,"applied");
        Ok(())
    }).unwrap();
}

#[test]
fn older_pending_receipts_verify_only_exact_current_persona() {
    for current in ["new persona", "different"] {
        let (store, service) = fixture();
        let request = commit(&plan(&service));
        service.commit_at("actor-a", &request, 1001, None).unwrap();
        // Reproduce the old synchronous projection without bumping the head.
        store.with(|c| {
            c.execute("UPDATE config_head SET applying=1 WHERE id=1",[])?;
            c.execute("INSERT INTO agent_md_revisions(version,content,author,note,created_ms) VALUES(1,?1,'legacy','',0)",[current])?;
            c.execute("UPDATE config_head SET applying=0 WHERE id=1",[])?;
            c.execute("UPDATE config_outbox SET payload_json=json_remove(payload_json,'$.persona')",[])?;
            Ok(())
        }).unwrap();
        super::worker::run_once(&store).unwrap();
        assert_eq!(
            crate::agent_md::AgentMdService::new(store.clone()).current_content(),
            current
        );
        let proof = service
            .readback("actor-a", &request.application_id)
            .unwrap();
        assert_eq!(proof["publication"]["isolation"], "legacy_projection");
        assert_eq!(
            proof["publication"]["state"],
            if current == "new persona" {
                "applied"
            } else {
                "failed"
            }
        );
    }
}

#[test]
fn applied_persona_does_not_hide_legacy_write_back_to_prepublication_content() {
    for publish_first in [false, true] {
        let (store, service) = fixture();
        let request = commit(&plan(&service));
        service.commit_at("actor-a", &request, 1001, None).unwrap();
        if publish_first {
            super::worker::run_once(&store).unwrap();
        }
        crate::agent_md::AgentMdService::new(store.clone())
            .update("", "console", "Explicit legacy restore")
            .unwrap();
        let state = service.state("actor-a").unwrap();
        assert_eq!(state["stored"]["persona.agent_md.content"], "");
        assert_eq!(
            state["resolved"]["persona.agent_md.content"], "",
            "legacy restore must be adopted even if content matches a job's expected value"
        );
        let pkg = package(
            &service,
            json!([{"action":"preserve","path":"persona.agent_md.content"}]),
        );
        let next = service
            .preflight_at("actor-a", pkg, json!([]), 1002)
            .unwrap();
        assert_eq!(next["diff"][0]["action"], "adopt_external_write");
        assert_eq!(next["diff"][0]["after"], "");
        service
            .commit_at("actor-a", &commit(&next), 1003, None)
            .unwrap();
        super::worker::run_once(&store).unwrap();
        assert_eq!(
            crate::agent_md::AgentMdService::new(store.clone()).current_content(),
            ""
        );
    }
}

fn services_plan(service: &ConfigDelivery, model: &str) -> Value {
    let mut pkg = package(service, json!([]));
    pkg.blocks = vec![Block {
        name: "services".into(),
        schema_version: 1,
        operations: vec![
            Operation::Set {
                path: "services.llm.model".into(),
                value: json!(model),
            },
            Operation::Set {
                path: "services.llm.base_url".into(),
                value: json!("http://127.0.0.1:1/synthetic"),
            },
        ],
    }];
    service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap()
}

#[test]
fn services_publish_atomically_fail_without_changing_effective_and_rollback_through_worker() {
    let (store, service) = fixture();
    let original = crate::config::ConfigService::new(store.clone())
        .load()
        .unwrap();
    let request = commit(&services_plan(&service, "new-model"));
    let receipt = service.commit_at("actor-a", &request, 1001, None).unwrap();
    assert_eq!(
        service.state("actor-a").unwrap()["stored"]["services.llm.model"],
        original.llm.model
    );
    assert_eq!(
        service.state("actor-a").unwrap()["resolved"]["services.llm.model"],
        "new-model"
    );
    store.with(|c| {c.execute_batch("CREATE TRIGGER fail_services BEFORE INSERT ON config_kv WHEN NEW.key='services' BEGIN SELECT RAISE(ABORT,'synthetic private detail'); END;")?;Ok(())}).unwrap();
    super::worker::run_once(&store).unwrap();
    let failed = service
        .readback("actor-a", &request.application_id)
        .unwrap();
    assert_eq!(failed["publication"]["state"], "failed");
    assert_eq!(
        failed["publication"]["error"],
        "SERVICES_PUBLICATION_FAILED"
    );
    assert_eq!(failed["fields"][0]["value"], original.llm.model);
    assert_eq!(failed["fields"][1]["value"], original.llm.base_url);
    assert_eq!(
        service
            .application("actor-a", &request.application_id)
            .unwrap(),
        receipt
    );
    store
        .with(|c| {
            c.execute_batch("DROP TRIGGER fail_services;")?;
            Ok(())
        })
        .unwrap();
    let cancel = service
        .rollback_preflight(
            "actor-a",
            &request.application_id,
            receipt["config_revision"].as_u64().unwrap(),
        )
        .unwrap();
    service.commit("actor-a", &commit(&cancel)).unwrap();
    super::worker::run_once(&store).unwrap();
    let retry = commit(&services_plan(&service, "new-model"));
    let published = service.commit_at("actor-a", &retry, 1002, None).unwrap();
    super::worker::run_once(&store).unwrap();
    let proof = service.readback("actor-a", &retry.application_id).unwrap();
    assert_eq!(proof["publication_isolation"], "core_blocks");
    assert!(proof["fields"]
        .as_array()
        .unwrap()
        .iter()
        .all(|field| field["state"] == "core_readback_verified"));
    let rollback = service
        .rollback_preflight(
            "actor-a",
            &retry.application_id,
            published["config_revision"].as_u64().unwrap(),
        )
        .unwrap();
    let restored = commit(&rollback);
    service.commit("actor-a", &restored).unwrap();
    assert_eq!(
        crate::config::ConfigService::new(store.clone())
            .load()
            .unwrap()
            .llm
            .model,
        "new-model"
    );
    super::worker::run_once(&store).unwrap();
    assert_eq!(
        crate::config::ConfigService::new(store.clone())
            .load()
            .unwrap()
            .llm,
        original.llm
    );
}

#[test]
fn prepared_services_jobs_are_fenced_by_new_desired_legacy_credentials_and_epoch() {
    for later in ["desired", "legacy", "credential", "metadata", "epoch"] {
        let (store, service) = fixture();
        let request = commit(&services_plan(&service, "old-job"));
        service.commit_at("actor-a", &request, 1001, None).unwrap();
        let jobs = super::worker::prepare(&store).unwrap();
        if later == "desired" {
            let next = commit(&services_plan(&service, "newest"));
            service.commit_at("actor-a", &next, 1002, None).unwrap();
        } else if later == "legacy" {
            store.config_set("services",r#"{"llm":{"provider":"deepseek","base_url":"http://127.0.0.1:1/legacy","model":"legacy"},"unknown":{"keep":17}}"#).unwrap();
        } else if later == "metadata" {
            let mut native = serde_json::to_value(
                crate::config::ConfigService::new(store.clone())
                    .load()
                    .unwrap(),
            )
            .unwrap();
            native["unknown"] = json!({"keep":17});
            store.config_set("services", &native.to_string()).unwrap();
        } else if later == "credential" {
            store
                .secret_set("llm.api_key", "synthetic-rotated")
                .unwrap();
        } else {
            store
                .with(|c| {
                    c.execute(
                        "UPDATE config_head SET data_epoch=data_epoch+1 WHERE id=1",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        for job in jobs {
            super::worker::publish(&store, job).unwrap();
        }
        assert_ne!(
            crate::config::ConfigService::new(store.clone())
                .load()
                .unwrap()
                .llm
                .model,
            "old-job"
        );
        if later == "desired" {
            super::worker::run_once(&store).unwrap();
            assert_eq!(
                crate::config::ConfigService::new(store.clone())
                    .load()
                    .unwrap()
                    .llm
                    .model,
                "newest"
            );
        }
        if later == "legacy" {
            assert_eq!(
                service.state("actor-a").unwrap()["resolved"]["services.llm.model"],
                "legacy"
            );
        }
        if later == "credential" || later == "metadata" {
            assert_eq!(
                service.state("actor-a").unwrap()["resolved"]["services.llm.model"],
                "old-job"
            );
        }
    }
}

#[test]
fn legacy_services_queue_verifies_captured_schema_without_overwriting_unknowns_or_defaults() {
    for complete in [true, false] {
        let (store, service) = fixture();
        store.config_set("services",r#"{"llm":{"provider":"deepseek","base_url":"http://127.0.0.1:1/old","model":"old"},"unknown":{"keep":17}}"#).unwrap();
        let request = commit(&services_plan(&service, "new-model"));
        service.commit_at("actor-a", &request, 1001, None).unwrap();
        super::worker::run_once(&store).unwrap();
        let projected = store.config_get("services").unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&projected).unwrap()["unknown"]["keep"],
            17
        );
        store.with(|c| {
            c.execute("UPDATE config_application_blocks SET state='saved_pending' WHERE block='services'",[])?;
            c.execute("UPDATE config_outbox SET payload_json=json_remove(payload_json,'$.services')",[])?;
            if !complete {c.execute("UPDATE config_applications SET after_json=json_remove(after_json,'$.fields.\"services.llm.model\"')",[])?;}
            Ok(())
        }).unwrap();
        super::worker::run_once(&store).unwrap();
        assert_eq!(
            store.config_get("services").unwrap().unwrap(),
            projected,
            "legacy scan must not rewrite native JSON"
        );
        let proof = service
            .readback("actor-a", &request.application_id)
            .unwrap();
        assert_eq!(proof["publication"]["isolation"], "legacy_projection");
        assert_eq!(
            proof["publication"]["state"],
            if complete { "applied" } else { "failed" }
        );
    }
}

#[test]
fn independent_block_terminal_metadata_survives_later_failure_and_reopen() {
    let dir = std::env::temp_dir().join(format!("hm-block-publication-{}", uuid().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.db");
    let store = Arc::new(Store::open(&path).unwrap());
    let service = ConfigDelivery::new(store.clone());
    service.enroll("test-a").unwrap();
    let mut pkg = package(
        &service,
        json!([{"action":"set","path":"persona.agent_md.content","value":"published-persona"}]),
    );
    pkg.blocks.push(Block {
        name: "services".into(),
        schema_version: 1,
        operations: vec![Operation::Set {
            path: "services.llm.model".into(),
            value: json!("unpublished-service"),
        }],
    });
    let plan = service
        .preflight_at("actor-a", pkg, json!([]), 1000)
        .unwrap();
    let request = commit(&plan);
    service.commit_at("actor-a", &request, 1001, None).unwrap();
    store.with(|c| {c.execute_batch("CREATE TRIGGER fail_services BEFORE INSERT ON config_kv WHEN NEW.key='services' BEGIN SELECT RAISE(ABORT,'synthetic block failure'); END;")?;Ok(())}).unwrap();
    super::worker::run_once(&store).unwrap();
    let payload = store
        .with(|c| {
            Ok(c.query_row(
                "SELECT payload_json FROM config_outbox WHERE application_id=?1",
                [&request.application_id],
                |r| r.get::<_, String>(0),
            )?)
        })
        .unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["persona"]["state"], "applied");
    assert!(payload["persona"]["checked_ms"].is_u64());
    assert_eq!(payload["persona"]["error"], Value::Null);
    assert_eq!(payload["services"]["state"], "failed");
    assert_eq!(payload["services"]["error"], "SERVICES_PUBLICATION_FAILED");
    drop(service);
    drop(store);
    let store = Arc::new(Store::open(&path).unwrap());
    super::worker::run_once(&store).unwrap();
    let proof = ConfigDelivery::new(store.clone())
        .readback("actor-a", &request.application_id)
        .unwrap();
    assert_eq!(proof["publications"][0]["state"], "applied");
    assert_eq!(proof["publications"][1]["state"], "failed");
    assert_eq!(
        proof["publications"][1]["error"],
        "SERVICES_PUBLICATION_FAILED"
    );
    assert_eq!(proof["fields"][0]["value"], "published-persona");
    assert_eq!(proof["fields"][1]["value"], "deepseek-chat");
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
