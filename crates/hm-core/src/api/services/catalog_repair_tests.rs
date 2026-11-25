//! Persisted-provider faults must leave the real management state repairable.

use super::*;
use crate::agent::{run_turn, AgentEvent};
use crate::llm::fake::{FakeLlm, Scripted};
use crate::store::Store;
use crate::tool::mocks::MockInject;
use crate::tool::open_socket::{HttpMethod, OpenSocketContract, OPEN_SOCKETS_CONFIG_KEY};
use crate::tool::{SideEffect, ToolCtx};
use futures::StreamExt;
use std::sync::atomic::AtomicBool;

#[tokio::test]
async fn persisted_provider_collision_keeps_app_status_and_management_repair_alive() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let contract = OpenSocketContract {
        name: "workmem.get".into(),
        service: "fixture".into(),
        description: "synthetic persisted collision".into(),
        parameters: json!({"type":"object"}),
        effect: SideEffect::Query,
        endpoint_url: "http://127.0.0.1:9/never-called".into(),
        method: HttpMethod::GET,
        enabled: true,
        timeout_ms: 500,
        auth_header: String::new(),
        default_subject: String::new(),
        default_action: "get".into(),
    };
    store
        .config_set(
            OPEN_SOCKETS_CONFIG_KEY,
            &serde_json::to_string(&vec![contract]).unwrap(),
        )
        .unwrap();
    let llm = FakeLlm::new(vec![Scripted::Text {
        chunks: vec!["repaired".into()],
    }]);
    // This is the production AppState composition path, not a Registry-only fixture.
    let state = AppState::new(
        store,
        std::path::PathBuf::from("/tmp/hm-catalog-repair-fixture"),
        MockInject::default(),
        Some(Arc::new(llm.clone())),
    );
    let diagnostic = status(
        State(state.clone()),
        axum::extract::Query(StatusQuery::default()),
    )
    .await
    .0;
    assert!(diagnostic["tool_catalog_error"]
        .as_str()
        .unwrap()
        .contains("workmem.get"));
    assert!(state.registry.specs().is_empty());

    let ctx = ToolCtx::new("task", "call", Arc::new(AtomicBool::new(false)));
    let result = state
        .registry
        .call("memory.remember", json!({"text":"must not be saved"}), &ctx)
        .await;
    assert_eq!(result.error.unwrap().code, "TOOL_CATALOG_INVALID");
    assert!(state.memory.list().unwrap().is_empty());
    let events: Vec<_> = run_turn(
        state.agent_env().unwrap(),
        None,
        "fixture".into(),
        "test".into(),
    )
    .collect()
    .await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::Error { message } if message.contains("duplicate canonical"))
    ));
    assert!(
        llm.seen_requests().is_empty(),
        "invalid catalog cannot reach the model"
    );

    // Existing authenticated management routes use this same domain removal;
    // the attached provider remains live, so fixing it needs no process restart.
    assert!(state
        .open_sockets
        .remove(&state.registry, "workmem.get", "test-management")
        .unwrap());
    assert!(status(
        State(state.clone()),
        axum::extract::Query(StatusQuery::default()),
    )
    .await
    .0["tool_catalog_error"]
        .is_null());
    assert!(!state.registry.specs().is_empty());
    let events: Vec<_> = run_turn(
        state.agent_env().unwrap(),
        None,
        "fixture".into(),
        "test".into(),
    )
    .collect()
    .await;
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Answer { text } if text == "repaired")));
    assert_eq!(llm.seen_requests().len(), 1);
}
