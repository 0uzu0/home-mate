//! Production agent/catalog dispatch with a model returning advertised wire IDs.
//! No HTTP listener, endpoint request, device, or external service is used.

use futures::StreamExt;
use hm_core::agent::{run_turn, AgentEnv, AgentEvent};
use hm_core::config::ConfigService;
use hm_core::llm::{
    Finish, LlmClient, LlmError, LlmEvent, LlmStream, ToolSpec, WireMessage, WireToolCall,
};
use hm_core::store::Store;
use hm_core::tool::{
    Availability, DynamicToolProvider, Registry, SideEffect, Tool, ToolCtx, ToolDescriptor,
    ToolResult,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

struct Probe {
    name: String,
    effects: Arc<AtomicUsize>,
    availability: Availability,
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone().into(),
            service: "probe".into(),
            side_effect: SideEffect::Query,
            description: "read synthetic fixture".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }
    fn availability(&self) -> Availability {
        self.availability
    }
    async fn execute(&self, _: Value, _: &ToolCtx) -> ToolResult {
        self.effects.fetch_add(1, Ordering::SeqCst);
        ToolResult::observed_ok(json!({"source": self.name}))
    }
}

fn probe(name: &str, effects: &Arc<AtomicUsize>) -> Arc<dyn Tool> {
    Arc::new(Probe {
        name: name.into(),
        effects: effects.clone(),
        availability: Availability::Ready,
    })
}

#[derive(Default)]
struct Provider(RwLock<Vec<Arc<dyn Tool>>>);
impl DynamicToolProvider for Provider {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.read().unwrap().clone()
    }
}

struct WireModel {
    target: String,
    calls: AtomicUsize,
    seen: Mutex<Vec<(Vec<WireMessage>, Vec<ToolSpec>)>>,
    mutate_before_reply: Option<Box<dyn Fn() + Send + Sync>>,
}

#[async_trait::async_trait]
impl LlmClient for WireModel {
    async fn chat_stream(
        &self,
        _: &str,
        messages: Vec<WireMessage>,
        tools: &[ToolSpec],
    ) -> Result<LlmStream, LlmError> {
        self.seen.lock().unwrap().push((messages, tools.to_vec()));
        let event = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let advertised = tools
                .iter()
                .find(|t| t.description.starts_with(&format!("{}:", self.target)))
                .expect("target must actually be advertised to the model");
            assert_eq!(advertised.name.len(), 63);
            assert!(advertised
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_'));
            if let Some(mutate) = &self.mutate_before_reply {
                mutate();
            }
            LlmEvent::End {
                finish: Finish::ToolCalls,
                content: String::new(),
                tool_calls: vec![WireToolCall {
                    id: "provider-call-7".into(),
                    name: advertised.name.clone(),
                    arguments: "{}".into(),
                }],
            }
        } else {
            LlmEvent::End {
                finish: Finish::Stop,
                content: "fixture complete".into(),
                tool_calls: vec![],
            }
        };
        Ok(Box::pin(futures::stream::iter([event])))
    }
}

fn env(registry: Arc<Registry>, model: Arc<dyn LlmClient>) -> Arc<AgentEnv> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    Arc::new(AgentEnv {
        prompt_preferences: ConfigService::new(store.clone())
            .prompt_preferences()
            .unwrap_or_default(),
        llm: model,
        model: "wire-fixture".into(),
        llm_fallback: None,
        registry,
        sessions: Arc::new(hm_core::session::SessionStore::new(store.clone())),
        tasks: Arc::new(hm_core::task::TaskRegistry::new(store.clone())),
        traces: Arc::new(hm_core::trace::TraceRecorder::new(store.clone())),
        memory: hm_core::memory::MemoryService::new(store.clone()),
        working_memory: hm_core::working_memory::WorkingMemoryStore::new(store),
        shared_memory: None,
        user_md: None,
        scope: Default::default(),
        assistant_name: "fixture".into(),
        llm_first_event_timeout: std::time::Duration::from_secs(2),
    })
}

fn model(target: &str, mutate: Option<Box<dyn Fn() + Send + Sync>>) -> Arc<WireModel> {
    Arc::new(WireModel {
        target: target.into(),
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
        mutate_before_reply: mutate,
    })
}

fn ctx() -> ToolCtx {
    ToolCtx::new("task", "call", Arc::new(AtomicBool::new(false)))
}

#[tokio::test]
async fn actual_agent_roundtrips_static_dynamic_and_provider_wire_names_with_history() {
    for domain in ["static", "dynamic", "provider"] {
        let effects = Arc::new(AtomicUsize::new(0));
        let name = "family_light.start.part_two";
        let mut registry = Registry::new();
        match domain {
            "static" => registry.register(probe(name, &effects)).unwrap(),
            "dynamic" => assert!(registry.register_dynamic(probe(name, &effects))),
            _ => registry
                .add_provider(Arc::new(Provider(RwLock::new(vec![probe(name, &effects)]))))
                .unwrap(),
        }
        let registry = Arc::new(registry);
        let llm = model(name, None);
        let events: Vec<_> = run_turn(
            env(registry, llm.clone()),
            None,
            "read fixture".into(),
            "test".into(),
        )
        .collect()
        .await;
        assert_eq!(effects.load(Ordering::SeqCst), 1, "{domain}");
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { tool, .. } if tool == name)));
        let seen = llm.seen.lock().unwrap();
        let first_wire = &seen[0].1[0].name;
        let replay = seen[1]
            .0
            .iter()
            .find_map(|m| m.tool_calls.as_ref())
            .unwrap();
        assert_eq!(replay[0].name, *first_wire);
        assert_eq!(replay[0].id, "provider-call-7");
        assert!(seen[1]
            .0
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("provider-call-7")));
    }
}

#[tokio::test]
async fn removed_or_replaced_tools_are_stale_before_any_effect_in_actual_agent() {
    for replace_provider in [false, true] {
        let original = Arc::new(AtomicUsize::new(0));
        let replacement = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(Registry::new());
        let name = "family_light.start";
        let mutate: Box<dyn Fn() + Send + Sync> = if replace_provider {
            let provider = Arc::new(Provider(RwLock::new(vec![probe(name, &original)])));
            registry.add_provider(provider.clone()).unwrap();
            let replacement = replacement.clone();
            Box::new(move || *provider.0.write().unwrap() = vec![probe(name, &replacement)])
        } else {
            assert!(registry.register_dynamic(probe(name, &original)));
            let registry = registry.clone();
            Box::new(move || assert!(registry.unregister_dynamic(name)))
        };
        let llm = model(name, Some(mutate));
        let events: Vec<_> = run_turn(
            env(registry, llm),
            None,
            "read fixture".into(),
            "test".into(),
        )
        .collect()
        .await;
        assert_eq!(original.load(Ordering::SeqCst), 0);
        assert_eq!(replacement.load(Ordering::SeqCst), 0);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { summary, .. } if summary.contains("TOOL_STALE"))));
    }
}

#[tokio::test]
async fn underscore_dot_names_are_distinct_and_direct_dispatch_never_guesses_wire_names() {
    let effects = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    for name in [
        "family_light.start",
        "family.light_start",
        "family.light.start",
    ] {
        assert!(registry.register_dynamic(probe(name, &effects)));
    }
    let catalog = registry.catalog().unwrap();
    let specs = catalog.specs();
    let distinct: std::collections::HashSet<_> = specs.iter().map(|t| &t.name).collect();
    assert_eq!(distinct.len(), 3);
    for spec in specs {
        assert_eq!(spec.name.len(), 63);
        assert!(
            spec.name.starts_with("hm_family_light_start_"),
            "wire ID must carry sanitized readable prefix, got {}",
            spec.name
        );
        assert!(registry
            .call_advertised(&catalog, &spec.name, json!({}), &ctx())
            .await
            .error
            .is_none());
        assert!(registry
            .call(&spec.canonical_name, json!({}), &ctx())
            .await
            .error
            .is_none());
        assert_eq!(
            registry
                .call(&spec.name, json!({}), &ctx())
                .await
                .error
                .unwrap()
                .code,
            "UNKNOWN_TOOL"
        );
    }
    assert_eq!(effects.load(Ordering::SeqCst), 6);

    // 64-char max-length canonical name stays 63 chars with truncated readable prefix + 15-hex suffix.
    let long_name = format!("weather.{}", "a".repeat(56));
    assert_eq!(long_name.len(), 64);
    assert!(registry.register_dynamic(probe(&long_name, &effects)));
    let long_catalog = registry.catalog().unwrap();
    let long_spec = long_catalog
        .specs()
        .into_iter()
        .find(|s| s.canonical_name == long_name)
        .unwrap();
    assert_eq!(long_spec.name.len(), 63);
    assert!(long_spec
        .name
        .starts_with("hm_weather_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_"));
}

#[tokio::test]
async fn collisions_are_rejected_across_all_sources_and_bad_refresh_fails_closed() {
    let effects = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    registry.register(probe("family.read", &effects)).unwrap();
    assert!(registry.register(probe("family.read", &effects)).is_err());
    assert!(!registry.register_dynamic(probe("family.read", &effects)));
    assert!(registry
        .add_provider(Arc::new(Provider(RwLock::new(vec![probe(
            "family.read",
            &effects
        )]))))
        .is_err());
    let provider = Arc::new(Provider::default());
    registry.add_provider(provider.clone()).unwrap();
    *provider.0.write().unwrap() = vec![probe("family.read", &effects)];
    assert!(registry.catalog().is_err());
    assert_eq!(
        registry
            .call("family.read", json!({}), &ctx())
            .await
            .error
            .unwrap()
            .code,
        "TOOL_CATALOG_INVALID"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_names_are_rejected_while_existing_availability_policy_is_preserved() {
    let effects = Arc::new(AtomicUsize::new(0));
    let registry = Registry::new();
    for name in ["", "family light", "家庭.read", &"x".repeat(65)] {
        assert!(!registry.register_dynamic(probe(name, &effects)));
    }
    for availability in [
        Availability::Disabled,
        Availability::Unconfigured,
        Availability::Hidden,
    ] {
        assert!(registry.register_dynamic(Arc::new(Probe {
            name: format!("test.{availability:?}"),
            effects: effects.clone(),
            availability,
        })));
    }
    let catalog = registry.catalog().unwrap();
    assert_eq!(
        catalog.specs().len(),
        3,
        "all registered tools keep their prior specification membership"
    );
    assert!(registry.ready_capabilities().is_empty());
    assert!(
        registry
            .capabilities()
            .iter()
            .all(|c| c.tools.iter().all(|t| t.name != "test.Hidden")),
        "Hidden tools remain absent from displayed capabilities"
    );
    for spec in catalog.specs() {
        assert_eq!(
            catalog.canonical_name(&spec.name),
            Some(spec.canonical_name.as_str())
        );
    }
}

#[tokio::test]
async fn real_open_socket_endpoint_update_and_remove_readd_invalidate_old_catalog() {
    use hm_core::tool::open_socket::{HttpMethod, OpenSocketContract, OpenSocketRuntime};
    let runtime = OpenSocketRuntime::new(Arc::new(Store::open_in_memory().unwrap()));
    let registry = Registry::new();
    registry.add_provider(runtime.clone()).unwrap();
    let mut contract = OpenSocketContract {
        name: "family_light.start".into(),
        service: "fixture".into(),
        description: "fixture".into(),
        parameters: json!({"type":"object"}),
        effect: SideEffect::Query,
        endpoint_url: "http://127.0.0.1:9/old".into(),
        method: HttpMethod::GET,
        enabled: true,
        timeout_ms: 500,
        auth_header: String::new(),
        default_subject: String::new(),
        default_action: "get".into(),
    };
    runtime
        .register(&registry, contract.clone(), None, "test")
        .unwrap();
    let old = registry.catalog().unwrap();
    let old_wire = old.specs()[0].name.clone();
    contract.endpoint_url = "http://127.0.0.1:9/new".into();
    runtime
        .register(&registry, contract.clone(), None, "test")
        .unwrap();
    assert_eq!(
        registry
            .call_advertised(&old, &old_wire, json!({}), &ctx())
            .await
            .error
            .unwrap()
            .code,
        "TOOL_STALE"
    );
    let before_remove = registry.catalog().unwrap();
    runtime.remove(&registry, &contract.name, "test").unwrap();
    runtime.register(&registry, contract, None, "test").unwrap();
    assert_eq!(
        registry
            .call_advertised(&before_remove, &old_wire, json!({}), &ctx())
            .await
            .error
            .unwrap()
            .code,
        "TOOL_STALE"
    );
    assert_eq!(
        old_wire,
        registry.catalog().unwrap().specs()[0].name,
        "wire ID remains stable"
    );
}

#[tokio::test]
async fn actual_agent_old_socket_endpoint_token_or_removal_advertisement_has_zero_http_effects() {
    use hm_core::tool::open_socket::{HttpMethod, OpenSocketContract, OpenSocketRuntime};
    for mutation in ["endpoint", "token", "remove"] {
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = hits.clone();
        let app = axum::Router::new().fallback(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({"observed":true}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let runtime = OpenSocketRuntime::new(Arc::new(Store::open_in_memory().unwrap()));
        let registry = Arc::new(Registry::new());
        registry.add_provider(runtime.clone()).unwrap();
        let contract = OpenSocketContract {
            name: "fixture.socket".into(),
            service: "fixture".into(),
            description: "fixture".into(),
            parameters: json!({"type":"object"}),
            effect: SideEffect::Query,
            endpoint_url: format!("{endpoint}/old"),
            method: HttpMethod::GET,
            enabled: true,
            timeout_ms: 500,
            auth_header: String::new(),
            default_subject: String::new(),
            default_action: "get".into(),
        };
        runtime
            .register(&registry, contract.clone(), Some("synthetic-first"), "test")
            .unwrap();
        let rt = runtime.clone();
        let reg = registry.clone();
        let mut changed = contract.clone();
        changed.endpoint_url = format!("{endpoint}/new");
        let original = contract.clone();
        let llm = model(
            "fixture.socket",
            Some(Box::new(move || match mutation {
                "remove" => {
                    assert!(rt.remove(&reg, "fixture.socket", "test").unwrap());
                }
                "token" => {
                    rt.register(&reg, original.clone(), Some("synthetic-second"), "test")
                        .unwrap();
                }
                _ => {
                    rt.register(&reg, changed.clone(), None, "test").unwrap();
                }
            })),
        );
        let events: Vec<_> = run_turn(
            env(registry.clone(), llm),
            None,
            "query".into(),
            "test".into(),
        )
        .collect()
        .await;
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::ToolResult {summary,..} if summary.contains("TOOL_STALE"))
        ));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "old advertisement reached HTTP"
        );
        if mutation == "remove" {
            runtime.register(&registry, contract, None, "test").unwrap();
        }
        let fresh_events: Vec<_> = run_turn(
            env(registry.clone(), model("fixture.socket", None)),
            None,
            "query current".into(),
            "test".into(),
        )
        .collect()
        .await;
        assert!(fresh_events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult {status,..} if status == "completed")));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "fresh invocation must be usable"
        );
        server.abort();
    }
}

struct PausedWireModel {
    inner: WireModel,
    captured: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl LlmClient for PausedWireModel {
    async fn chat_stream(
        &self,
        model: &str,
        messages: Vec<WireMessage>,
        tools: &[ToolSpec],
    ) -> Result<LlmStream, LlmError> {
        let first = self.inner.calls.load(Ordering::SeqCst) == 0;
        let stream = self.inner.chat_stream(model, messages, tools).await?;
        if first {
            self.captured.notify_one();
            self.resume.notified().await;
        }
        Ok(stream)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_arc_unregister_readd_cannot_revive_an_advertised_call() {
    let effects = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(Registry::new());
    let same_tool = probe("family_light.start", &effects);
    assert!(registry.register_dynamic(same_tool.clone()));
    let before = registry.catalog().unwrap();
    let wire = before.specs()[0].name.clone();
    let llm = Arc::new(PausedWireModel {
        inner: WireModel {
            target: "family_light.start".into(),
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            mutate_before_reply: None,
        },
        captured: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let turn_env = env(registry.clone(), llm.clone());
    let drive = tokio::spawn(async move {
        run_turn(turn_env, None, "fixture".into(), "test".into())
            .collect::<Vec<_>>()
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), llm.captured.notified())
        .await
        .unwrap();
    let mutating = registry.clone();
    tokio::task::spawn_blocking(move || {
        assert!(mutating.unregister_dynamic("family_light.start"));
        assert!(mutating.register_dynamic(same_tool));
    })
    .await
    .unwrap();
    llm.resume.notify_one();
    let events = tokio::time::timeout(std::time::Duration::from_secs(2), drive)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { summary, .. } if summary.contains("TOOL_STALE"))
    ));
    let after = registry.catalog().unwrap();
    assert_ne!(before.revision, after.revision);
    assert_eq!(
        after.specs()[0].name,
        wire,
        "canonical wire identity remains stable"
    );
    assert_eq!(
        registry
            .call_advertised(&before, &wire, json!({}), &ctx())
            .await
            .error
            .unwrap()
            .code,
        "TOOL_STALE"
    );
    assert!(registry
        .call_advertised(&after, &wire, json!({}), &ctx())
        .await
        .error
        .is_none());
    assert_eq!(
        effects.load(Ordering::SeqCst),
        1,
        "only a fresh advertisement may execute"
    );
}

#[tokio::test]
async fn original_hotlist_preserves_slow_late_result_and_cancellation() {
    let (registry, _, _) = hm_core::tool::mocks::mock_registry(hm_core::tool::mocks::MockInject {
        hotlist_delay_ms: 300,
        ..Default::default()
    });
    let registry = Arc::new(registry);
    let llm = model("hotlist.query", None);
    let turn_env = env(registry, llm.clone());
    let started = std::time::Instant::now();
    let mut stream = run_turn(turn_env.clone(), None, "fixture".into(), "test".into());
    let mut task_id = None;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if let AgentEvent::Task { task_id: id } = &event {
            task_id = Some(id.clone());
        }
        let sleeping =
            matches!(&event, AgentEvent::ToolStart { tool, .. } if tool == "hotlist.query");
        events.push(event);
        if sleeping {
            assert!(turn_env
                .tasks
                .request_cancel(task_id.as_deref().unwrap())
                .unwrap());
            break;
        }
    }
    events.extend(stream.collect::<Vec<_>>().await);
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(280),
        "real 300ms backend completed late"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::ToolResult {
                    late: Some(true),
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Cancelled { .. })));
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnEnd { status, .. }) if status == "cancelled")
    );
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        1,
        "no model call after cancellation"
    );
}

#[tokio::test]
async fn original_hotlist_preserves_actual_empty_backend_error() {
    let (registry, _, _) = hm_core::tool::mocks::mock_registry(hm_core::tool::mocks::MockInject {
        hotlist_empty: true,
        ..Default::default()
    });
    let registry = Arc::new(registry);
    let llm = model("hotlist.query", None);
    let events: Vec<_> = run_turn(
        env(registry, llm.clone()),
        None,
        "fixture".into(),
        "test".into(),
    )
    .collect()
    .await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { summary, .. } if summary.contains("EMPTY_CHART"))
    ));
    let seen = llm.seen.lock().unwrap();
    assert!(serde_json::to_string(&seen[1].0)
        .unwrap()
        .contains("EMPTY_CHART"));
}

#[tokio::test]
async fn actual_agent_keeps_the_named_unconfigured_home_refusal() {
    let (registry, _, _) = hm_core::tool::mocks::mock_registry(Default::default());
    let registry = Arc::new(registry);
    assert!(registry
        .unavailable_services()
        .iter()
        .any(|s| s.service == "home"));
    let llm = model("home.set", None);
    let events: Vec<_> = run_turn(
        env(registry, llm.clone()),
        None,
        "关闭儿童房的灯".into(),
        "test".into(),
    )
    .collect()
    .await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolResult { tool, status, summary, .. }
        if tool == "home.set" && status == "failed" && summary.contains("HOME_UNCONFIGURED"))
    ));
    let seen = llm.seen.lock().unwrap();
    let reply_context = serde_json::to_string(&seen[1].0).unwrap();
    assert!(reply_context.contains("HOME_UNCONFIGURED"));
    assert!(!reply_context.contains("NOT_ADVERTISED"));
}
