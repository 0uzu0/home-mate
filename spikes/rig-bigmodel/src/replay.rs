//! Spike A (replay): rig-core model client against the local wire-mock.
//! Deterministic counterpart of live.rs — same fixtures drive hm-core's own
//! client and genai in the comparison harness. Verifies:
//!   - fixture decode (real recorded GLM transcripts + synthetic quirks)
//!   - idle-timeout behavior on a stalled body
//!   - drop-cancellation: connection closes promptly (via wire-mock /__events)

use futures::StreamExt;
use rig_core::{
    completion::{CompletionRequest, Message, ToolDefinition},
    providers::openai::{wire::Dialect, OpenAIConfig},
};
use std::time::{Duration, Instant};

fn tool() -> ToolDefinition {
    ToolDefinition::new(
        rig_core::completion::message::ToolName::new("music_search").unwrap(),
        "搜索音乐曲库",
        serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
    )
}

#[tokio::main]
async fn main() {
    let base = std::env::var("HM_SPIKE_BASE").unwrap_or_else(|_| "http://127.0.0.1:8977/v1".into());
    let dialect = Dialect::gateway("wiremock", "http://127.0.0.1:8977/v1", "HM_SPIKE_KEY");
    let config = OpenAIConfig::with_key(&dialect, "spike").with_base_url(&base);

    // Scenario 1+2: recorded GLM transcripts (tool + text).
    for (name, with_tool) in [("glm-tool-normal", true), ("glm-text-normal", false)] {
        let client = config.clone().client();
        let model = client.chat(name); // model id selects the fixture on wire-mock
        let mut req = CompletionRequest::new(Message::user("x"));
        if with_tool {
            req = req.tool(tool());
        }
        let started = Instant::now();
        let mut stream = match model.stream(req) {
            Ok(s) => s,
            Err(e) => {
                println!("[{name}] open failed: {e}");
                continue;
            }
        };
        let (mut text, mut args, mut ends, mut errors) = (0usize, 0usize, 0usize, 0usize);
        while let Some(item) = stream.next().await {
            match item {
                Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { .. })) => text += 1,
                Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Arguments { .. })) => args += 1,
                Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::End { .. })) => ends += 1,
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    println!("[{name}] stream error: {e}");
                }
            }
        }
        println!("[{name}] text={text} args={args} ends={ends} errors={errors} in {:?}", started.elapsed());
    }

    // Scenario 3: idle timeout on a stalled body (mock-stall-body).
    {
        let client = config.clone().client();
        let model = client.chat("mock-stall-body").with_idle_timeout(Some(Duration::from_secs(2)));
        let req = CompletionRequest::new(Message::user("x"));
        let started = Instant::now();
        let mut stream = model.stream(req).expect("stall stream open");
        let mut outcome = "ended".to_string();
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                outcome = format!("error: {e}");
                break;
            }
        }
        println!("[stall-body] outcome={outcome} after {:?}", started.elapsed());
    }

    // Scenario 4: drop-cancellation, proven by wire-mock's connection log.
    {
        let http = reqwest::Client::new();
        let _ = http.post(format!("{base}/../__reset")).send().await;
        let client = config.clone().client();
        let model = client.chat("glm-text-normal");
        let req = CompletionRequest::new(Message::user("x"));
        let started = Instant::now();
        let mut stream = model.stream(req).expect("drop stream open");
        let mut dropped_after_text = false;
        while let Some(item) = stream.next().await {
            if matches!(
                item,
                Ok(rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { .. }))
            ) {
                dropped_after_text = true;
                break;
            }
        }
        drop(stream);
        let dropped_at = Instant::now();
        // Give the server a moment to observe the disconnect.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let events: serde_json::Value = http
            .get(format!("{base}/../__events"))
            .send()
            .await
            .expect("events fetch")
            .json()
            .await
            .expect("events json");
        println!("[drop] saw_text_before_drop={dropped_after_text} total_to_drop={:?}", dropped_at.duration_since(started));
        println!("[drop] wire-mock connection log: {events}");
    }

    println!("[replay] spike A replay complete");
}
