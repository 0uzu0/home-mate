//! Records real GLM (bigmodel v4) streaming responses as SSE fixture files.
//!
//! Reads GLM_API_KEY / GLM_BASE_URL from the environment (source ~/.hermes/.env
//! first; the key is never printed or written). Writes only response bodies to
//! ../fixtures/<name>.sse — SSE bodies carry no credentials.

use futures::StreamExt;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let key = std::env::var("GLM_API_KEY").expect("GLM_API_KEY must be set (source ~/.hermes/.env)");
    let base = std::env::var("GLM_BASE_URL").unwrap_or_else(|_| "https://open.bigmodel.cn/api/paas/v4".into());
    let base = base.trim_end_matches('/');
    let model = std::env::var("GLM_MODEL").unwrap_or_else(|_| "glm-4-flash".into());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();

    // Scenario 1: forces a tool call (fragmented arguments expected).
    let tool_spec = serde_json::json!({
        "type": "function",
        "function": {
            "name": "music_search",
            "description": "搜索音乐曲库，返回候选曲目列表",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "搜索关键词"},
                    "limit": {"type": "integer", "description": "返回数量"}
                },
                "required": ["query"]
            }
        }
    });
    let scenarios: Vec<(&str, serde_json::Value)> = vec![
        (
            "glm-tool-normal",
            serde_json::json!({
                "model": model,
                "stream": true,
                "messages": [
                    {"role": "system", "content": "你是家庭助手。需要查外部信息时必须调用工具。"},
                    {"role": "user", "content": "帮我搜一下周杰伦的晴天，放第一首"}
                ],
                "tools": [tool_spec]
            }),
        ),
        (
            "glm-text-normal",
            serde_json::json!({
                "model": model,
                "stream": true,
                "messages": [{"role": "user", "content": "用一句话介绍你自己"}]
            }),
        ),
    ];

    let out_dir = std::env::var("HM_SPIKE_FIXTURES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("../fixtures"));
    std::fs::create_dir_all(&out_dir).unwrap();

    for (name, body) in scenarios {
        let started = Instant::now();
        let resp = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .expect("request failed");
        let status = resp.status();
        let ttfb = started.elapsed();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            eprintln!("[capture] {name}: HTTP {status} (ttfb {ttfb:?}) — body length {} (not saved)", text.len());
            continue;
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut first_chunk: Option<Duration> = None;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("body chunk error");
            if first_chunk.is_none() {
                first_chunk = Some(started.elapsed());
            }
            buf.extend_from_slice(&chunk);
        }
        let total = started.elapsed();
        let path = out_dir.join(format!("{name}.sse"));
        std::fs::write(&path, &buf).unwrap();
        println!(
            "[capture] {name}: {} bytes, headers {ttfb:?}, first chunk {:?}, total {total:?} -> {}",
            buf.len(),
            first_chunk.unwrap_or_default(),
            path.display()
        );
    }
}
