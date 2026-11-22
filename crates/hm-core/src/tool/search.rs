//! General web information search (M2.5), shaped like weather/news: a
//! REGISTERED service, `unconfigured` until a household stores an endpoint
//! and a key, and ZERO HTTP in that state.
//!
//! The provider is the DeepSeek Search API shape (`POST /tools/search`,
//! bearer key). Only a 200 with a non-empty result list is a billable answer;
//! everything else is a named failure, never silence and never an invented
//! headline.
//!
//! Honesty rules:
//! - results are DATA. Every field is cleaned (tags stripped, controls
//!   removed, whitespace collapsed), length-bounded, and a result without an
//!   http(s) URL is dropped — the point of this tool is a SOURCE the person
//!   can be told;
//! - the model never sees more than `MAX_SEARCH_RESULTS` rows, and the HTTP
//!   body is read through a 256 KiB bound;
//! - a redirect is an error, not something to follow: the configured endpoint
//!   is the only host this tool may talk to.

use super::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use crate::config::{ConfigService, SearchConfig, MAX_SEARCH_COUNT, MAX_SEARCH_RESULTS};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A search answer is text, not an archive.
pub const MAX_SEARCH_BODY: usize = 256 * 1024;
/// What one spoken result can carry. A 4,000-character "snippet" is an attack
/// on the voice pipeline, not information.
pub const MAX_SNIPPET_CHARS: usize = 200;
pub const MAX_SUMMARY_CHARS: usize = 200;
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_URL_CHARS: usize = 500;
pub const MAX_DATE_CHARS: usize = 40;
pub const MAX_AUTHORITY_CHARS: usize = 40;
/// The provider's documented bound on `query`.
pub const MAX_QUERY_CHARS: usize = 128;

/// The probe is a real query (an authenticated search IS the proof). It is
/// deliberately cheap and non-personal.
const PROBE_QUERY: &str = "今天天气";

pub const SEARCH_UNCONFIGURED: &str = "SEARCH_UNCONFIGURED";
pub const SEARCH_DISABLED: &str = "SEARCH_DISABLED";
pub const SEARCH_BAD_ARGS: &str = "SEARCH_BAD_ARGS";
pub const SEARCH_TIMEOUT: &str = "SEARCH_TIMEOUT";
pub const SEARCH_OFFLINE: &str = "SEARCH_OFFLINE";
pub const SEARCH_HTTP: &str = "SEARCH_HTTP";
pub const SEARCH_RESPONSE_TOO_LARGE: &str = "SEARCH_RESPONSE_TOO_LARGE";
pub const SEARCH_PARSE: &str = "SEARCH_PARSE";
pub const SEARCH_EMPTY: &str = "SEARCH_EMPTY";

#[derive(Debug, Clone)]
pub struct SearchError {
    pub code: &'static str,
    pub detail: String,
}

impl SearchError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub summary: Option<String>,
    pub date: Option<String>,
    pub authority: Option<String>,
}

impl SearchHit {
    pub fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "url": self.url,
            "snippet": self.snippet,
            "summary": self.summary,
            "date": self.date,
            "authority": self.authority,
        })
    }
}

/// Parse one provider body. Pure and bounded, so the shape can be tested
/// without a socket.
pub fn parse_results(body: &str, cap: usize) -> Result<Vec<SearchHit>, SearchError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        SearchError::new(SEARCH_PARSE, format!("搜索服务返回了无法解析的内容：{e}"))
    })?;
    // The documented shape is `{results: […]}`; `{data:{results: […]}}` is
    // accepted as the same thing (a compatible proxy), not as a second API.
    let rows = v
        .get("results")
        .and_then(Value::as_array)
        .or_else(|| {
            v.get("data")
                .and_then(|d| d.get("results"))
                .and_then(Value::as_array)
        })
        .ok_or_else(|| SearchError::new(SEARCH_PARSE, "搜索服务没有返回 results 列表"))?;
    let mut out = Vec::new();
    for row in rows {
        let Some(title) = bounded(row.get("title"), MAX_TITLE_CHARS) else {
            continue;
        };
        let Some(url) = bounded(row.get("url"), MAX_URL_CHARS) else {
            continue;
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            // No source = no citation. Dropping is the honest move: the model
            // may only relay what it can attribute.
            continue;
        }
        let snippet = bounded(row.get("snippet"), MAX_SNIPPET_CHARS).unwrap_or_default();
        let summary = bounded(row.get("summary"), MAX_SUMMARY_CHARS)
            .filter(|s| !s.is_empty() && Some(s.as_str()) != Some(snippet.as_str()));
        if title.is_empty() && snippet.is_empty() && summary.is_none() {
            continue;
        }
        out.push(SearchHit {
            title,
            url,
            snippet,
            summary,
            date: bounded(row.get("date"), MAX_DATE_CHARS).filter(|s| !s.is_empty()),
            authority: bounded(row.get("authority"), MAX_AUTHORITY_CHARS).filter(|s| !s.is_empty()),
        });
        if out.len() >= cap.max(1) {
            break;
        }
    }
    Ok(out)
}

/// Clean and bound one string field. Markup is stripped, entities decoded,
/// control characters (a newline in a title is not a line break for TTS)
/// collapsed into single spaces, and the tail is cut with an ellipsis.
fn bounded(value: Option<&Value>, cap: usize) -> Option<String> {
    let raw = value?.as_str()?;
    let text = clean_text(raw);
    if text.is_empty() {
        return None;
    }
    let mut out: String = text.chars().take(cap).collect();
    if text.chars().count() > cap {
        out.push('…');
    }
    Some(out)
}

fn clean_text(raw: &str) -> String {
    // Strip the simplest markup (<b>, <a href>) that providers sometimes
    // leave in snippets.
    let mut no_tags = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => no_tags.push(c),
            _ => {}
        }
    }
    let collapsed = no_tags.split_whitespace().collect::<Vec<_>>().join(" ");
    // `split_whitespace` already drops controls that count as whitespace;
    // remove the remaining ones (NUL, ESC, bidi overrides are an injection
    // surface in a prompt, not content).
    collapsed
        .chars()
        .filter(|c| {
            !c.is_control()
                && *c != '\u{202a}'
                && *c != '\u{202b}'
                && *c != '\u{202c}'
                && *c != '\u{202d}'
                && *c != '\u{202e}'
                && *c != '\u{2066}'
                && *c != '\u{2067}'
                && *c != '\u{2068}'
                && *c != '\u{2069}'
        })
        .collect()
}

// --- runtime ---------------------------------------------------------------

pub struct SearchRuntime {
    store: Arc<crate::store::Store>,
    client: reqwest::Client,
    event_sink: std::sync::Mutex<Option<Arc<dyn Fn(&str, Value) + Send + Sync>>>,
}

impl SearchRuntime {
    pub fn new(store: Arc<crate::store::Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        Self::with_client(
            store,
            crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                crate::tool::http::OutboundPolicy::Public { allow_loopback },
            )
            // Per-request timeout is applied at the call site from the
            // household's own setting; this is only the connect bound.
            .connect_timeout(Duration::from_secs(5))
            // The configured endpoint is the only host this tool may
            // talk to (audit P1 on the news path: the default follows up
            // to 10 hops and would carry the bearer to another host).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default(),
        )
    }

    pub fn with_client(store: Arc<crate::store::Store>, client: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {
            store,
            client,
            event_sink: std::sync::Mutex::new(None),
        })
    }

    pub fn set_event_sink(&self, sink: Arc<dyn Fn(&str, Value) + Send + Sync>) {
        *crate::lock_or_recover(&self.event_sink, "search.event_sink") = Some(sink);
    }

    fn note(&self, kind: &str, payload: Value) {
        tracing::info!(kind, %payload, "search");
        if let Some(sink) = crate::lock_or_recover(&self.event_sink, "search.event_sink").as_ref() {
            sink(kind, payload);
        }
    }

    fn snapshot(&self) -> (SearchConfig, Option<String>) {
        ConfigService::new(self.store.clone())
            .search_snapshot()
            .unwrap_or_default()
    }

    pub fn state_word(&self) -> &'static str {
        let (cfg, key) = self.snapshot();
        cfg.state_word(key.is_some())
    }

    pub fn availability(&self) -> Availability {
        match self.state_word() {
            "ready" => Availability::Ready,
            "disabled" => Availability::Disabled,
            _ => Availability::Unconfigured,
        }
    }

    /// The tool path: config gate, one request, bounded parse, honest refusal.
    pub async fn search(
        &self,
        query: &str,
        count: Option<u64>,
    ) -> Result<Value, (SearchError, usize)> {
        let (cfg, key) = self.snapshot();
        let Some(key) = key else {
            return Err((SearchError::new(SEARCH_UNCONFIGURED, "搜索服务还没配置"), 0));
        };
        if !cfg.enabled {
            return Err((SearchError::new(SEARCH_DISABLED, "搜索服务已关闭"), 0));
        }
        let query = clean_text(query);
        if query.is_empty() {
            return Err((SearchError::new(SEARCH_BAD_ARGS, "没说清要搜什么"), 0));
        }
        let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
        // The caller's count is a CEILING on what it will read, and the
        // provider is asked for exactly that: forwarding 8 while reading 3
        // buys rows the model never sees (and makes the log lie about what
        // was requested).
        let cap = count
            .map(|c| c.clamp(1, MAX_SEARCH_COUNT as u64) as usize)
            .unwrap_or_else(|| cfg.effective_count() as usize)
            .min(MAX_SEARCH_RESULTS)
            .min(cfg.result_cap())
            .max(1);
        let asked = cap as u8;
        let started = Instant::now();
        let hits = self.request(&cfg, &key, &query, asked, cap).await?;
        let latency_ms = started.elapsed().as_millis();
        if hits.is_empty() {
            // A 200 with nothing in it is NOT a billable answer and must not
            // read as "the world has nothing on this".
            return Err((SearchError::new(SEARCH_EMPTY, "没有搜到相关内容"), 1));
        }
        self.note(
            "search_completed",
            json!({ "query": query, "asked": asked, "results": hits.len(),
                    "latency_ms": latency_ms }),
        );
        Ok(json!({
            "query": query,
            "count": hits.len(),
            "fetched_ms": crate::id::now_ms(),
            "latency_ms": latency_ms,
            "results": hits.iter().map(SearchHit::to_json).collect::<Vec<_>>(),
            "note": "以上是搜索服务返回的内容，不是助手的知识；引用时带上来源与日期",
        }))
    }

    async fn request(
        &self,
        cfg: &SearchConfig,
        key: &str,
        query: &str,
        count: u8,
        cap: usize,
    ) -> Result<Vec<SearchHit>, (SearchError, usize)> {
        let body = json!({
            "query": query,
            "count": count,
            "need_content": false,
        });
        let resp = self
            .client
            .post(cfg.base_url.trim())
            .timeout(Duration::from_millis(cfg.timeout_ms.clamp(500, 15_000)))
            .bearer_auth(key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let (code, detail) = if e.is_timeout() {
                    (SEARCH_TIMEOUT, "搜索超时，没查到".to_string())
                } else {
                    (SEARCH_OFFLINE, "连不上搜索服务，没查到".to_string())
                };
                return Err((SearchError::new(code, detail), 1));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            // The person hears this line (agent::failure_say speaks a short
            // Chinese detail verbatim), so it names the situation, not a code.
            let detail = match status.as_u16() {
                401 | 403 => "搜索服务没通过认证，查不到".to_string(),
                402 => "搜索服务余额不足，查不到".to_string(),
                429 => "搜索太频繁了，稍后再试".to_string(),
                _ => format!("搜索服务出错了（HTTP {}），没查到", status.as_u16()),
            };
            return Err((SearchError::new(SEARCH_HTTP, detail), 1));
        }
        let bytes = crate::tool::http::read_bounded(resp, MAX_SEARCH_BODY)
            .await
            .map_err(|e| {
                let (code, detail) = match e {
                    crate::tool::http::BodyReadError::TooLarge { .. } => (
                        SEARCH_RESPONSE_TOO_LARGE,
                        "搜索结果太大了，没有读完".to_string(),
                    ),
                    crate::tool::http::BodyReadError::Timeout => {
                        (SEARCH_TIMEOUT, "搜索超时，没查到".to_string())
                    }
                    crate::tool::http::BodyReadError::PrematureClose(_) => (
                        SEARCH_OFFLINE,
                        "读取搜索结果时连接提前关闭，没查到".to_string(),
                    ),
                    crate::tool::http::BodyReadError::Protocol(_) => {
                        (SEARCH_OFFLINE, "读取搜索结果时协议异常，没查到".to_string())
                    }
                };
                (SearchError::new(code, detail), 1)
            })?;
        let text = String::from_utf8_lossy(&bytes);
        parse_results(&text, cap).map_err(|e| (e, 1))
    }

    /// A REAL probe: one query, no audio, no device. Unconfigured/disabled =
    /// zero requests, reported as such.
    pub async fn probe(&self) -> Value {
        let started = Instant::now();
        match self.search(PROBE_QUERY, Some(3)).await {
            Ok(v) => {
                let samples: Vec<Value> = v["results"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .map(|r| {
                                json!({
                                    "title": r["title"],
                                    "url": r["url"],
                                    "date": r["date"],
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "state": "ready",
                    "detail": format!("搜索服务可用：{} 条结果", v["count"]),
                    "http_requests": 1,
                    "latency_ms": started.elapsed().as_millis(),
                    "count": v["count"],
                    "samples": samples,
                })
            }
            Err((e, requests)) => json!({
                "ok": false,
                "state": match e.code {
                    SEARCH_UNCONFIGURED => "unconfigured",
                    SEARCH_DISABLED => "disabled",
                    _ => "failed",
                },
                "detail": e.detail,
                "code": e.code,
                "http_requests": requests,
                "latency_ms": started.elapsed().as_millis(),
                "count": 0,
                "samples": [],
            }),
        }
    }
}

fn state_detail(state: &str, count: usize, timeout_ms: u64) -> String {
    match state {
        "ready" => format!(
            "已配置（每次最多回 {} 条，超时 {} ms）；答案会带来源与日期",
            count, timeout_ms
        ),
        "disabled" => "搜索已配置但当前关闭：助手不会联网检索".into(),
        _ => "搜索服务未配置：请在控制台「搜索」里填接口地址与 key；助手不会编造「查到的」信息"
            .into(),
    }
}

/// Read-back shape for `/mode`, the config GET and the console card.
pub fn status_json(rt: &SearchRuntime) -> Value {
    let st = ConfigService::new(rt.store.clone())
        .search_status()
        .unwrap_or(crate::config::SearchStatus {
            enabled: false,
            base_url: crate::config::default_search_base_url(),
            count: crate::config::DEFAULT_SEARCH_COUNT,
            timeout_ms: crate::config::DEFAULT_SEARCH_TIMEOUT_MS,
            api_key_set: false,
            state: "unconfigured".into(),
            revision: String::new(),
        });
    json!({
        "enabled": st.enabled,
        "base_url": st.base_url,
        "count": st.count,
        "timeout_ms": st.timeout_ms,
        "api_key_set": st.api_key_set,
        "state": st.state,
        "detail": state_detail(&st.state, usize::from(st.count.clamp(1, MAX_SEARCH_COUNT)).min(MAX_SEARCH_RESULTS), st.timeout_ms),
        "max_results": MAX_SEARCH_RESULTS,
        "revision": st.revision,
    })
}

// --- the tool --------------------------------------------------------------

pub struct WebSearchTool {
    rt: Arc<SearchRuntime>,
}

impl WebSearchTool {
    pub fn new(rt: Arc<SearchRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        let description = if self.rt.availability() == Availability::Ready {
            "联网搜索公开信息（只读）：返回标题、链接、摘要、日期。回答必须说明这是搜到的内容并带上来源与日期；区分于你自己的知识；没有结果就如实说没有，不要编造"
        } else {
            "联网搜索公开信息（搜索服务尚未配置：不会发出任何请求，也不会编造「查到的」信息）"
        };
        ToolDescriptor {
            name: "web.search".into(),
            service: "search".into(),
            side_effect: SideEffect::Query,
            description: description.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "要搜索的内容（≤128 字，可用关键词）" },
                    "count": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_COUNT,
                               "description": "最多几条（默认按服务设置；助手最多读 8 条）" }
                },
                "required": ["query"]
            }),
        }
    }

    fn availability(&self) -> Availability {
        self.rt.availability()
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::cancelled();
        }
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return ToolResult::failed(SEARCH_BAD_ARGS, "缺少 query", false);
        };
        let count = args.get("count").and_then(Value::as_u64);
        match self.rt.search(query, count).await {
            Ok(v) => ToolResult::observed_ok(v),
            Err((e, requests)) => {
                let mut r = ToolResult::failed(e.code, e.detail, false);
                r.result = Some(json!({ "http_requests": requests }));
                r
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(results: Value) -> String {
        json!({ "results": results }).to_string()
    }

    #[test]
    fn a_result_without_an_http_source_is_dropped() {
        let rows = json!([
            { "title": "A", "url": "https://example.com/a", "snippet": "s" },
            { "title": "B", "url": "javascript:alert(1)", "snippet": "s" },
            { "title": "C", "snippet": "no url at all" }
        ]);
        let hits = parse_results(&body(rows), 8).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/a");
    }

    #[test]
    fn snippet_and_title_are_bounded_and_cleaned() {
        let rows = json!([{
            "title": "<b>标</b>题\n换行",
            "url": "https://example.com/x",
            "snippet": "长".repeat(500),
        }]);
        let hits = parse_results(&body(rows), 8).unwrap();
        assert_eq!(hits[0].title, "标题 换行");
        assert_eq!(hits[0].snippet.chars().count(), MAX_SNIPPET_CHARS + 1);
        assert!(hits[0].snippet.ends_with('…'));
    }

    #[test]
    fn a_duplicate_summary_is_not_repeated() {
        let rows = json!([{
            "title": "A", "url": "https://example.com/a",
            "snippet": "同一段", "summary": "同一段"
        }]);
        let hits = parse_results(&body(rows), 8).unwrap();
        assert_eq!(hits[0].snippet, "同一段");
        assert_eq!(hits[0].summary, None);
    }

    #[test]
    fn the_cap_is_enforced() {
        let rows: Vec<Value> = (0..20)
            .map(|i| json!({ "title": format!("t{i}"), "url": format!("https://example.com/{i}") }))
            .collect();
        let hits = parse_results(&body(json!(rows)), 8).unwrap();
        assert_eq!(hits.len(), 8);
    }

    #[test]
    fn empty_and_malformed_bodies_are_named_failures() {
        assert!(parse_results(r#"{"results":[]}"#, 8).unwrap().is_empty());
        assert_eq!(parse_results("<html>", 8).unwrap_err().code, SEARCH_PARSE);
        assert_eq!(
            parse_results(r#"{"nope":1}"#, 8).unwrap_err().code,
            SEARCH_PARSE
        );
    }

    #[test]
    fn the_data_wrapper_is_the_same_shape() {
        let body = json!({ "data": { "results": [
            { "title": "A", "url": "https://example.com/a" }
        ]}});
        assert_eq!(parse_results(&body.to_string(), 8).unwrap().len(), 1);
    }
}
