//! M3.5 §5 deterministic comparison: the same SSE fixtures (recorded GLM wire
//! transcripts + synthetic quirks) through three model clients —
//!   1. hm-core's own DeepSeekClient (the 419-line baseline),
//!   2. GenaiLlmClient (genai 0.7.3 behind the real LlmClient trait),
//!   3. rig-core 0.46.0 model client,
//! all served by the local wire-mock (model id selects the fixture). Scenarios
//! map to agent-kernel-research §5: decode correctness, quirk tolerance,
//! stall recovery, drop-cancellation close latency.

use futures::StreamExt;
use hm_core::llm::{LlmClient, LlmEvent, ToolSpec, WireMessage};
use rig_core::completion::{CompletionRequest, Message};
use rig_core::providers::openai::{wire::Dialect, OpenAIConfig};
use std::time::{Duration, Instant};

const BASE: &str = "http://127.0.0.1:8977/v1";

fn tool() -> ToolSpec {
    ToolSpec {
        name: "music.search".into(),
        description: "搜索音乐曲库".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
    }
}

struct Row {
    client: &'static str,
    fixture: &'static str,
    deltas: usize,
    calls: usize,
    args_ok: bool,
    error: Option<String>,
    elapsed: Duration,
}

#[tokio::main]
async fn main() {
    let fixtures = [
        "glm-tool-normal",
        "glm-text-normal",
        "quirk-fragmented-tool",
        "quirk-missing-index",
    ];

    let own = hm_core::llm::deepseek::DeepSeekClient::new(BASE, "spike").expect("own client");
    let rig_config = OpenAIConfig::with_key(&Dialect::gateway("wiremock", BASE, "HM_SPIKE_KEY"), "spike")
        .with_base_url(BASE);

    let mut rows = Vec::new();

    for fixture in fixtures {
        // --- own client (model id = fixture name) ---
        {
            let started = Instant::now();
            let msgs = vec![
                WireMessage::system("你是家庭助手。需要查外部信息时必须调用工具。"),
                WireMessage::user("搜一下周杰伦的晴天"),
            ];
            let tools = vec![tool()];
            let outcome = match own.chat_stream(fixture, msgs, &tools).await {
                Ok(mut s) => {
                    let (mut deltas, mut calls, mut args_ok, mut err) = (0, 0, false, None);
                    while let Some(ev) = s.next().await {
                        match ev {
                            LlmEvent::Delta(_) => deltas += 1,
                            LlmEvent::End { tool_calls, .. } => {
                                calls = tool_calls.len();
                                if let Some(c) = tool_calls.first() {
                                    args_ok = serde_json::from_str::<serde_json::Value>(&c.arguments).is_ok();
                                }
                            }
                            LlmEvent::Failed(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    }
                    (deltas, calls, args_ok, err)
                }
                Err(e) => (0, 0, false, Some(e.to_string())),
            };
            rows.push(Row { client: "own(419)", fixture, deltas: outcome.0, calls: outcome.1, args_ok: outcome.2, error: outcome.3, elapsed: started.elapsed() });
        }

        // --- genai behind LlmClient (model id = fixture name via target rebuild per fixture) ---
        {
            let started = Instant::now();
            let target = genai::ServiceTarget {
                model: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, fixture),
                auth: genai::resolver::AuthData::from_single("spike"),
                endpoint: genai::resolver::Endpoint::from_owned(BASE),
            };
            let g = genai_swap_spike::GenaiLlmClient::new("openai", fixture)
                .expect("genai client")
                .with_timeouts(Duration::from_secs(2), Duration::from_secs(2))
                .with_service_target(target);
            let msgs = vec![
                WireMessage::system("你是家庭助手。需要查外部信息时必须调用工具。"),
                WireMessage::user("搜一下周杰伦的晴天"),
            ];
            let tools = vec![tool()];
            let outcome = match g.chat_stream(fixture, msgs, &tools).await {
                Ok(mut s) => {
                    let (mut deltas, mut calls, mut args_ok, mut err) = (0, 0, false, None);
                    while let Some(ev) = s.next().await {
                        match ev {
                            LlmEvent::Delta(_) => deltas += 1,
                            LlmEvent::End { tool_calls, .. } => {
                                calls = tool_calls.len();
                                if let Some(c) = tool_calls.first() {
                                    args_ok = serde_json::from_str::<serde_json::Value>(&c.arguments).is_ok();
                                }
                            }
                            LlmEvent::Failed(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    }
                    (deltas, calls, args_ok, err)
                }
                Err(e) => (0, 0, false, Some(e.to_string())),
            };
            rows.push(Row { client: "genai0.7.3", fixture, deltas: outcome.0, calls: outcome.1, args_ok: outcome.2, error: outcome.3, elapsed: started.elapsed() });
        }

        // --- rig model client ---
        {
            let started = Instant::now();
            let client = rig_config.clone().client();
            let model = client.chat(fixture); // model id selects the fixture
            let req = CompletionRequest::new(Message::user("搜一下周杰伦的晴天"))
                .tool(rig_core::completion::ToolDefinition::new(
                    rig_core::completion::message::ToolName::new("music_search").unwrap(),
                    "搜索音乐曲库",
                    serde_json::json!({
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"]
                    }),
                ));
            let outcome = match model.stream(req) {
                Ok(mut s) => {
                    let (mut deltas, mut calls, mut args_ok, mut err) = (0, 0, false, None);
                    let mut args = String::new();
                    while let Some(item) = s.next().await {
                        match item {
                            Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { .. })) => deltas += 1,
                            Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Arguments { json, .. })) => args.push_str(&json),
                            Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::End {
                                content: rig_core::completion::message::AssistantContent::ToolCall(call),
                                part: _,
                            })) => {
                                calls += 1;
                                // rig finalizes arguments as a parsed Map already
                                args_ok = call.function.arguments.contains_key("query");
                            }
                            Ok(_) => {}
                            Err(e) => {
                                err = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    if calls > 0 && !args_ok && !args.is_empty() {
                        args_ok = serde_json::from_str::<serde_json::Value>(&args).is_ok();
                    }
                    (deltas, calls, args_ok, err)
                }
                Err(e) => (0, 0, false, Some(e.to_string())),
            };
            rows.push(Row { client: "rig0.46.0", fixture, deltas: outcome.0, calls: outcome.1, args_ok: outcome.2, error: outcome.3, elapsed: started.elapsed() });
        }
    }

    println!("=== §5 fixture decode comparison (same fixtures, three clients) ===");
    println!("{:<12} {:<24} {:>6} {:>5} {:>7} {:>9}  {}", "client", "fixture", "deltas", "calls", "args_ok", "elapsed", "error");
    for r in &rows {
        let err = r.error.as_deref().map(|e| e.chars().take(300).collect::<String>()).unwrap_or_default();
        println!("{:<12} {:<24} {:>6} {:>5} {:>7} {:>9.1?}  {}", r.client, r.fixture, r.deltas, r.calls, r.args_ok, r.elapsed, err);
    }

    // ---- stall recovery (mock-stall-body: one event, then hang 20s) ----
    println!("\n=== stall-after-first-event recovery ===");
    {
        let started = Instant::now();
        let msgs = vec![WireMessage::user("x")];
        let mut s = own.chat_stream("mock-stall-body", msgs, &[]).await.expect("own stall open");
        let mut out = "no-end".to_string();
        while let Some(ev) = s.next().await {
            if let LlmEvent::Failed(e) = ev {
                out = format!("failed: {e}");
                break;
            }
            if matches!(ev, LlmEvent::End { .. }) {
                out = "end".into();
                break;
            }
        }
        println!("own(419):   {out} after {:?}", started.elapsed());
    }
    {
        let started = Instant::now();
        let target = genai::ServiceTarget {
            model: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "mock-stall-body"),
            auth: genai::resolver::AuthData::from_single("spike"),
            endpoint: genai::resolver::Endpoint::from_owned(BASE),
        };
        let g = genai_swap_spike::GenaiLlmClient::new("openai", "x").unwrap()
            .with_timeouts(Duration::from_secs(2), Duration::from_secs(2))
            .with_service_target(target);
        let mut s = g.chat_stream("mock-stall-body", vec![WireMessage::user("x")], &[]).await.expect("genai stall open");
        let mut out = "no-end".to_string();
        while let Some(ev) = s.next().await {
            if let LlmEvent::Failed(e) = ev {
                out = format!("failed: {}", e.chars().take(300).collect::<String>());
                break;
            }
            if matches!(ev, LlmEvent::End { .. }) {
                out = "end".into();
                break;
            }
        }
        println!("genai0.7.3: {out} after {:?}", started.elapsed());
    }
    {
        let started = Instant::now();
        let client = rig_config.clone().client();
        let model = client.chat("mock-stall-body").with_idle_timeout(Some(Duration::from_secs(2)));
        let req = CompletionRequest::new(Message::user("x"));
        let mut s = model.stream(req).expect("rig stall open");
        let mut out = "no-end".to_string();
        while let Some(item) = s.next().await {
            if let Err(e) = item {
                out = format!("failed: {}", e.to_string().chars().take(60).collect::<String>());
                break;
            }
        }
        if out == "no-end" {
            out = "stream-ended-cleanly".into();
        }
        println!("rig0.46.0:  {out} after {:?}", started.elapsed());
    }

    // ---- drop-cancellation close latency (wire-mock /__events is the judge) ----
    println!("\n=== drop-cancellation: connection close latency ===");
    let http = reqwest::Client::new();
    let _ = http.post("http://127.0.0.1:8977/__reset").send().await;

    // own
    {
        let mut s = own.chat_stream("glm-text-normal", vec![WireMessage::user("x")], &[]).await.unwrap();
        while let Some(ev) = s.next().await {
            if matches!(ev, LlmEvent::Delta(_)) {
                break;
            }
        }
        drop(s);
    }
    // genai
    {
        let target = genai::ServiceTarget {
            model: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "glm-text-normal"),
            auth: genai::resolver::AuthData::from_single("spike"),
            endpoint: genai::resolver::Endpoint::from_owned(BASE),
        };
        let g = genai_swap_spike::GenaiLlmClient::new("openai", "x").unwrap().with_service_target(target);
        let mut s = g.chat_stream("glm-text-normal", vec![WireMessage::user("x")], &[]).await.unwrap();
        while let Some(ev) = s.next().await {
            if matches!(ev, LlmEvent::Delta(_)) {
                break;
            }
        }
        drop(s);
    }
    // rig
    {
        let client = rig_config.clone().client();
        let model = client.chat("glm-text-normal");
        let mut s = model.stream(CompletionRequest::new(Message::user("x"))).unwrap();
        while let Some(item) = s.next().await {
            if matches!(item, Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { .. }))) {
                break;
            }
        }
        drop(s);
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let events: serde_json::Value = http.get("http://127.0.0.1:8977/__events").send().await.unwrap().json().await.unwrap();
    println!("wire-mock connection log (closed_ms - connected_ms = connection lifetime):");
    println!("{events}");
}
