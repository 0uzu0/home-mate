//! Spike B (live): drive `GenaiLlmClient` (a real `LlmClient` impl) against
//! bigmodel v4 with glm-4-flash, through hm-core's own contract types. The
//! assertions are the trait contract itself: zero or more Delta, then exactly
//! one End (or a single Failed) — plus tool-call correctness on the wire quirk.
//! BIGMODEL_API_KEY is read from the environment (never printed).

use futures::StreamExt;
use genai_swap_spike::GenaiLlmClient;
use hm_core::llm::{Finish, LlmClient, LlmEvent, ToolSpec, WireMessage};

fn music_tool() -> ToolSpec {
    ToolSpec {
        name: "music.search".into(),
        description: "搜索音乐曲库，返回候选曲目列表".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词"},
                "limit": {"type": "integer", "description": "返回数量"}
            },
            "required": ["query"]
        }),
    }
}

#[tokio::main]
async fn main() {
    let model = std::env::var("GLM_MODEL").unwrap_or_else(|_| "glm-4-flash".into());
    let client = GenaiLlmClient::new("bigmodel", &model).expect("genai client build")
        .with_timeouts(std::time::Duration::from_secs(35), std::time::Duration::from_secs(35));

    // ---- Scenario 1: tool call through the trait contract ----
    let messages = vec![
        WireMessage::system("你是家庭助手。需要查外部信息时必须调用工具。"),
        WireMessage::user("帮我搜一下周杰伦的晴天，放第一首"),
    ];
    let tools = vec![music_tool()];
    let started = std::time::Instant::now();
    let mut stream = client.chat_stream(&model, messages, &tools).await.expect("stream open");
    let (mut deltas, mut ends, mut failed) = (0usize, 0usize, None);
    let mut final_event: Option<LlmEvent> = None;
    while let Some(ev) = stream.next().await {
        match &ev {
            LlmEvent::Delta(_) => deltas += 1,
            LlmEvent::End { .. } => {
                ends += 1;
                final_event = Some(ev);
            }
            LlmEvent::Failed(e) => {
                failed = Some(e.clone());
                break;
            }
        }
    }
    if let Some(e) = failed {
        eprintln!("[tool] Failed event: {e}");
        std::process::exit(2);
    }
    assert_eq!(ends, 1, "contract: exactly one End, got {ends}");
    match final_event {
        Some(LlmEvent::End { finish, content, tool_calls }) => {
            println!(
                "[tool] +{:?} deltas={deltas} finish={finish:?} calls={} content_len={}",
                started.elapsed(),
                tool_calls.len(),
                content.len()
            );
            assert_eq!(finish, Finish::ToolCalls, "expected a tool call");
            let call = &tool_calls[0];
            println!("[tool] name={} id={}", call.name, call.id);
            assert_eq!(call.name, "music.search", "wire name must map back to dotted internal name");
            let args: serde_json::Value = serde_json::from_str(&call.arguments).expect("args parse");
            println!("[tool] args={args}");
        }
        other => panic!("unexpected terminal event: {other:?}"),
    }

    // ---- Scenario 2: text-only + contract re-check ----
    let messages = vec![WireMessage::user("用一句话介绍你自己")];
    let started = std::time::Instant::now();
    let mut stream = client.chat_stream(&model, messages, &[]).await.expect("text stream open");
    let (mut deltas, mut ends) = (0usize, 0usize);
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            LlmEvent::Delta(t) => {
                deltas += 1;
                text.push_str(&t);
            }
            LlmEvent::End { finish, tool_calls, .. } => {
                ends += 1;
                assert_eq!(finish, Finish::Stop);
                assert!(tool_calls.is_empty());
            }
            LlmEvent::Failed(e) => {
                eprintln!("[text] Failed: {e}");
                std::process::exit(3);
            }
        }
    }
    assert_eq!(ends, 1);
    println!("[text] +{:?} deltas={deltas}: {}", started.elapsed(), &text[..text.len().min(60)]);

    // ---- Scenario 3: tool round-trip (assistant tool_calls + tool result back in) ----
    // Proves the reverse mapping (WireMessage -> genai) accepts our history shape.
    let history = vec![
        WireMessage::system("你是家庭助手。"),
        WireMessage::user("搜一下周杰伦的晴天"),
        WireMessage::assistant_calls(vec![hm_core::llm::WireToolCall {
            id: "call_rt_1".into(),
            name: "music.search".into(),
            arguments: r#"{"query":"周杰伦 晴天"}"#.into(),
        }]),
        WireMessage::tool_result("call_rt_1", r#"[{"title":"晴天","artist":"周杰伦"}]"#),
    ];
    let mut stream = client
        .chat_stream(&model, history, &tools)
        .await
        .expect("round-trip stream open");
    let mut ok = false;
    while let Some(ev) = stream.next().await {
        if let LlmEvent::End { .. } = ev {
            ok = true;
        }
        if let LlmEvent::Failed(e) = ev {
            eprintln!("[round-trip] Failed: {e}");
            std::process::exit(4);
        }
    }
    assert!(ok, "round-trip must end with End");
    println!("[round-trip] tool history accepted, +{:?}", started.elapsed());
    println!("[live] spike B complete — swap is feasible, adapter is one lib file");
}
