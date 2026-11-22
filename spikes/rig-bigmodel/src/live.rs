//! Spike A (live): rig-core 0.46.0 model-client layer against bigmodel v4
//! (GLM glm-4-flash). Verifies the two open questions from the Rig research
//! report that public materials could not answer:
//!   1. does GLM tool-call streaming (fragmented arguments) decode correctly
//!      through Rig's generic OpenAI wire pointed at open.bigmodel.cn?
//!   2. what does dropping a stream do in practice (live: next call works;
//!      deterministic close proof lives in replay.rs against wire-mock)?
//! Credentials come from the environment; nothing sensitive is printed.

use futures::StreamExt;
use rig_core::{
    completion::{CompletionRequest, Message, ToolDefinition},
    providers::openai::{wire::Dialect, OpenAIConfig},
};
use std::time::{Duration, Instant};

fn music_tool() -> ToolDefinition {
    ToolDefinition::new(
        rig_core::completion::message::ToolName::new("music_search").unwrap(),
        "搜索音乐曲库，返回候选曲目列表",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词"},
                "limit": {"type": "integer", "description": "返回数量"}
            },
            "required": ["query"]
        }),
    )
}

#[tokio::main]
async fn main() {
    let key = std::env::var("GLM_API_KEY").expect("GLM_API_KEY (source ~/.hermes/.env)");
    let base = std::env::var("GLM_BASE_URL")
        .unwrap_or_else(|_| "https://open.bigmodel.cn/api/paas/v4".into());
    let base = base.trim_end_matches('/');
    let model_id = std::env::var("GLM_MODEL").unwrap_or_else(|_| "glm-4-flash".into());

    let dialect = Dialect::gateway("bigmodel", "https://open.bigmodel.cn/api/paas/v4", "GLM_API_KEY");
    let config = OpenAIConfig::with_key(&dialect, &key).with_base_url(base);
    let client = config.client();
    let model = client.chat(model_id.clone());

    // ---- Scenario 1: tool-call streaming ----
    let started = Instant::now();
    let req = CompletionRequest::new(Message::user("帮我搜一下周杰伦的晴天，放第一首"))
        .preamble("你是家庭助手。需要查外部信息时必须调用工具。")
        .tool(music_tool());
    let mut stream = match model.stream(req) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[tool] stream open failed immediately: {e}");
            std::process::exit(2);
        }
    };
    let mut first_event: Option<Duration> = None;
    let mut text_fragments = 0usize;
    let mut arg_fragments = 0usize;
    let mut assembled_args = String::new();
    let mut tool_name: Option<String> = None;
    let mut finalized_calls = 0usize;
    let mut final_text = String::new();
    let mut terminal: Option<String> = None;
    while let Some(item) = stream.next().await {
        if first_event.is_none() {
            first_event = Some(started.elapsed());
        }
        match item {
            Ok(rig_core::streaming::Item::Event(ev)) => {
                use rig_core::streaming::StreamEvent;
                match &ev {
                    StreamEvent::Start { kind, name, .. } => {
                        println!("[tool] +{:<6?} Start kind={kind:?} name={name:?}", started.elapsed());
                    }
                    StreamEvent::Text { text, .. } => {
                        text_fragments += 1;
                        final_text.push_str(text);
                    }
                    StreamEvent::Reasoning { .. } => {}
                    StreamEvent::Arguments { json, .. } => {
                        arg_fragments += 1;
                        assembled_args.push_str(json);
                    }
                    StreamEvent::End { content, .. } => {
                        println!("[tool] +{:<6?} End content={}", started.elapsed(), kind_of(content));
                        if matches!(
                            content,
                            rig_core::completion::message::AssistantContent::ToolCall { .. }
                        ) {
                            finalized_calls += 1;
                        }
                    }
                }
                let _ = &mut tool_name;
            }
            Ok(rig_core::streaming::Item::Unknown(u)) => {
                println!("[tool] +{:<6?} Unknown payload (len {})", started.elapsed(), format!("{u:?}").len());
            }
            Err(e) => {
                terminal = Some(format!("error: {e}"));
                break;
            }
        }
    }
    if terminal.is_none() {
        terminal = Some("stream-ended".into());
    }
    // Record the tool name from the assembled Start events (re-scan not
    // possible after the fact; printed above line by line).
    println!(
        "[tool] first_event={:?} total={:?} text_frags={text_fragments} arg_frags={arg_fragments} \
         finalized_tool_calls={finalized_calls} terminal={}",
        first_event, started.elapsed(), terminal.unwrap_or_default()
    );
    println!("[tool] assembled arguments JSON: {assembled_args}");
    match serde_json::from_str::<serde_json::Value>(&assembled_args) {
        Ok(v) => println!("[tool] arguments parse: OK ({v})"),
        Err(e) => {
            if arg_fragments == 0 {
                println!("[tool] arguments parse: no Arguments fragments seen (single-chunk quirk? see End events)");
            } else {
                println!("[tool] arguments parse: FAILED {e}");
                std::process::exit(3);
            }
        }
    }
    if !final_text.trim().is_empty() {
        println!("[tool] inline text: {}", final_text.trim());
    }

    // ---- Scenario 2: text-only streaming + drop-abort ----
    let started = Instant::now();
    let req = CompletionRequest::new(Message::user("讲一个大约两百字的小故事"));
    let mut stream = model.stream(req).expect("text stream open");
    let mut got_first_text = false;
    let mut pieces = 0usize;
    while let Some(item) = stream.next().await {
        match item {
            Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { .. })) => {
                pieces += 1;
                if !got_first_text {
                    got_first_text = true;
                    println!("[text] first text delta at {:?}", started.elapsed());
                    break; // drop mid-stream right after the first delta
                }
            }
            Ok(_) => {}
            Err(e) => {
                println!("[text] error before first text: {e}");
                std::process::exit(4);
            }
        }
    }
    drop(stream); // cancellation-by-drop
    let dropped_at = Instant::now();

    // Next call must still work (no leaked connection/lock).
    let req = CompletionRequest::new(Message::user("用一个词回答：天空是什么颜色"));
    let mut s2 = model.stream(req).expect("post-drop stream open");
    let mut answer = String::new();
    while let Some(item) = s2.next().await {
        match item {
            Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { text, .. })) => {
                answer.push_str(&text);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    println!(
        "[drop] dropped after {pieces} text pieces; next call {:?} later answered: {}",
        dropped_at.elapsed(),
        answer.trim()
    );
    if !got_first_text {
        eprintln!("[drop] never saw a text delta; drop test inconclusive");
        std::process::exit(5);
    }
    println!("[live] spike A complete");
}

fn kind_of(content: &rig_core::completion::message::AssistantContent) -> &'static str {
    use rig_core::completion::message::AssistantContent::*;
    match content {
        Text { .. } => "Text",
        ToolCall { .. } => "ToolCall",
        Reasoning { .. } => "Reasoning",
        _ => "other",
    }
}
