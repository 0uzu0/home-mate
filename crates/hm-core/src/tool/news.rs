//! News as a REGISTERED SERVICE (R2-BM6), shaped like weather (R1h).
//!
//! The household configures one or more feeds (RSS 2.0 / Atom / JSON); the
//! tool returns HEADLINES only. There is no built-in sample feed and no mock
//! mode: with no source configured the service is `unconfigured`, the tool
//! returns `NEWS_UNCONFIGURED` and ZERO HTTP happens.
//!
//! Honesty rules:
//! - a feed's items are DATA. Titles are stripped of HTML, entities decoded,
//!   whitespace collapsed, length-bounded and links restricted to http(s);
//!   a title that reads like an instruction is still just a title (the prompt
//!   already says tool data is not an instruction);
//! - an empty feed is `NEWS_EMPTY`, not silence;
//! - one bad source does not poison a multi-source service: the probe names
//!   each source, and the tool refuses only the source that failed.

use super::{Availability, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolResult};
use crate::config::{ConfigService, NewsConfig, NewsKind, NewsSource, MAX_NEWS_ITEMS};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A feed response is a list of headlines, not a podcast archive.
pub const MAX_FEED_BODY: usize = 256 * 1024;
/// Titles are spoken; a 4,000-character "title" is an attack on the voice
/// pipeline, not news.
pub const MAX_TITLE_CHARS: usize = 80;
/// Parse at most this many entries before applying the caller's cap.
const PARSE_LIMIT: usize = 50;

pub const NEWS_UNCONFIGURED: &str = "NEWS_UNCONFIGURED";
pub const NEWS_DISABLED: &str = "NEWS_DISABLED";
pub const NEWS_SOURCE_UNKNOWN: &str = "NEWS_SOURCE_UNKNOWN";
pub const NEWS_SOURCE_REQUIRED: &str = "NEWS_SOURCE_REQUIRED";
pub const NEWS_TIMEOUT: &str = "NEWS_TIMEOUT";
pub const NEWS_OFFLINE: &str = "NEWS_OFFLINE";
pub const NEWS_HTTP: &str = "NEWS_HTTP";
pub const NEWS_RESPONSE_TOO_LARGE: &str = "NEWS_RESPONSE_TOO_LARGE";
pub const NEWS_PARSE: &str = "NEWS_PARSE";
pub const NEWS_EMPTY: &str = "NEWS_EMPTY";

#[derive(Debug, Clone)]
pub struct NewsError {
    pub code: &'static str,
    pub detail: String,
}

impl NewsError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewsItem {
    pub title: String,
    pub link: Option<String>,
    pub date: Option<String>,
}

impl NewsItem {
    pub fn to_json(&self) -> Value {
        json!({ "title": self.title, "link": self.link, "date": self.date })
    }
}

/// Parse one feed body. Pure and bounded, so the protocols can be tested
/// without a socket.
pub fn parse_feed(kind: NewsKind, body: &str) -> Result<Vec<NewsItem>, NewsError> {
    let items = match kind {
        NewsKind::Rss => {
            // "not a feed" must not read as "an empty feed": an empty feed is
            // a fact, a non-feed is a parse failure.
            if !contains_ci(body, "<rss") && !contains_ci(body, "<rdf:RDF") {
                return Err(NewsError::new(NEWS_PARSE, "内容不是 RSS 文档"));
            }
            // A truncated document must not read as an empty (but valid) feed
            // (audit round 2).
            if !contains_ci(body, "</rss>") && !contains_ci(body, "</rdf:RDF>") {
                return Err(NewsError::new(NEWS_PARSE, "RSS 文档没有闭合标签"));
            }
            parse_xml(body, "item", |block| NewsItem {
                title: tag(block, "title")
                    .map(|t| clean_title(&t))
                    .unwrap_or_default(),
                link: tag(block, "link")
                    .map(|l| clean_link(&l))
                    .filter(|l| !l.is_empty()),
                date: tag(block, "pubDate")
                    .map(|d| clean_text(&d))
                    .filter(|d| !d.is_empty()),
            })?
        }
        NewsKind::Atom => {
            if !contains_ci(body, "<feed") {
                return Err(NewsError::new(NEWS_PARSE, "内容不是 Atom 文档"));
            }
            if !contains_ci(body, "</feed>") {
                return Err(NewsError::new(NEWS_PARSE, "Atom 文档没有闭合标签"));
            }
            parse_xml(body, "entry", |block| {
                // Atom links are attributes: <link rel="alternate" href="…"/>
                let link = tag(block, "link")
                    .map(|l| clean_link(&l))
                    .filter(|l| !l.is_empty())
                    .or_else(|| {
                        attr(block, "link", "href")
                            .map(|h| clean_link(&h))
                            .filter(|h| !h.is_empty())
                    });
                NewsItem {
                    title: tag(block, "title")
                        .map(|t| clean_title(&t))
                        .unwrap_or_default(),
                    link,
                    date: tag(block, "updated")
                        .or_else(|| tag(block, "published"))
                        .map(|d| clean_text(&d))
                        .filter(|d| !d.is_empty()),
                }
            })?
        }
        NewsKind::Json => parse_json_items(body)?,
    };
    Ok(items
        .into_iter()
        .filter(|i| !i.title.is_empty())
        .take(PARSE_LIMIT)
        .collect())
}

fn parse_xml<F>(body: &str, block_tag: &str, mut f: F) -> Result<Vec<NewsItem>, NewsError>
where
    F: FnMut(&str) -> NewsItem,
{
    let open = format!("<{block_tag}");
    let close = format!("</{block_tag}>");
    let mut rest = body;
    let mut out = Vec::new();
    while let Some(start) = find_ci(rest, &open) {
        let after = &rest[start..];
        // Skip the opening tag itself (`<item>` or `<item attr=…>`).
        // Audit P2: a truncated document used to end the loop silently and
        // read as "an empty feed"; an unclosed item is a parse failure.
        let Some(gt) = after.find('>') else {
            return Err(NewsError::new(
                NEWS_PARSE,
                format!("<{block_tag}> 开标签没有闭合"),
            ));
        };
        let content_start = start + gt + 1;
        let Some(end_rel) = find_ci(&rest[content_start..], &close) else {
            return Err(NewsError::new(
                NEWS_PARSE,
                format!("<{block_tag}> 没有闭合标签"),
            ));
        };
        let content = &rest[content_start..content_start + end_rel];
        out.push(f(content));
        rest = &rest[content_start + end_rel + close.len()..];
        if out.len() >= PARSE_LIMIT {
            break;
        }
    }
    Ok(out)
}

/// A tolerant "first `<tag>` content" reader: handles CDATA, nested simple
/// tags are stripped later. Returns None when the tag is absent.
fn tag(block: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let start = find_ci(block, &open)?;
    let after = &block[start..];
    let gt = after.find('>')?;
    // Self-closing (`<link href="…"/>`) has no content.
    if after[..gt].ends_with('/') {
        return None;
    }
    let content_start = start + gt + 1;
    let close = format!("</{name}>");
    let end = find_ci(&block[content_start..], &close)?;
    Some(block[content_start..content_start + end].to_string())
}

/// Read an attribute from the first matching tag (`<link href="…">`).
fn attr(block: &str, tag_name: &str, attr_name: &str) -> Option<String> {
    let open = format!("<{tag_name}");
    let start = find_ci(block, &open)?;
    let after = &block[start..];
    let gt = after.find('>')?;
    let head = &after[..gt];
    let needle = format!("{attr_name}=");
    let at = find_ci(head, &needle)? + needle.len();
    let rest = head[at..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn parse_json_items(body: &str) -> Result<Vec<NewsItem>, NewsError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| NewsError::new(NEWS_PARSE, format!("JSON 源无法解析：{e}")))?;
    let array = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(o) => o
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| NewsError::new(NEWS_PARSE, "JSON 源需要 {\"items\":[…]} 或数组"))?,
        _ => return Err(NewsError::new(NEWS_PARSE, "JSON 源需要对象或数组")),
    };
    Ok(array
        .iter()
        .filter_map(|item| {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(clean_title)
                .filter(|t| !t.is_empty())?;
            let link = item
                .get("link")
                .and_then(Value::as_str)
                .map(clean_link)
                .filter(|l| !l.is_empty());
            let date = item
                .get("date")
                .or_else(|| item.get("published"))
                .and_then(Value::as_str)
                .map(clean_text)
                .filter(|d| !d.is_empty());
            Some(NewsItem { title, link, date })
        })
        .collect())
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}

fn clean_title(raw: &str) -> String {
    let t = clean_text(raw);
    let mut out: String = t.chars().take(MAX_TITLE_CHARS).collect();
    if t.chars().count() > MAX_TITLE_CHARS {
        out.push('…');
    }
    out
}

/// CDATA unwrapped, tags stripped, entities decoded, whitespace collapsed.
fn clean_text(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if let Some(stripped) = s.strip_prefix("<![CDATA[") {
        if let Some(end) = stripped.find("]]>") {
            s = stripped[..end].to_string();
        }
    }
    // Strip any remaining markup (titles sometimes carry <b> or <a>).
    let mut no_tags = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => no_tags.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&no_tags);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_link(raw: &str) -> String {
    let s = clean_text(raw);
    let trimmed = s.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.chars().take(500).collect()
    } else {
        String::new()
    }
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        // Audit P2: scan at most 12 bytes for ';' — `tail.find(';')` over the
        // whole suffix made a title full of bare '&' quadratic.
        let mut semi = None;
        for (off, b) in tail.bytes().enumerate().take(13) {
            if off > 0 && b == b';' {
                semi = Some(off);
                break;
            }
        }
        let Some(semi) = semi else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match replacement {
            Some(c) if !c.is_control() => out.push(c),
            Some(_) => {}
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

// --- runtime ---------------------------------------------------------------

pub struct NewsRuntime {
    store: Arc<crate::store::Store>,
    client: reqwest::Client,
    event_sink: std::sync::Mutex<Option<Arc<dyn Fn(&str, Value) + Send + Sync>>>,
}

impl NewsRuntime {
    pub fn new(store: Arc<crate::store::Store>) -> Arc<Self> {
        let allow_loopback = store.allows_loopback_public();
        Self::with_client(
            store,
            crate::tool::http::apply_policy_resolver(
                reqwest::Client::builder(),
                crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
            )
            .timeout(Duration::from_secs(8))
            // A feed must not redirect the request to another host
            // (audit P1: the default follows up to 10 hops).
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
        *crate::lock_or_recover(&self.event_sink, "news.event_sink") = Some(sink);
    }

    fn note(&self, kind: &str, payload: Value) {
        if let Some(sink) = crate::lock_or_recover(&self.event_sink, "news.event_sink").as_ref() {
            sink(kind, payload);
        }
    }

    pub fn config(&self) -> NewsConfig {
        ConfigService::new(self.store.clone())
            .news()
            .unwrap_or_default()
    }

    pub fn availability(&self) -> Availability {
        match self.config().state_word() {
            "ready" => Availability::Ready,
            "disabled" => Availability::Disabled,
            _ => Availability::Unconfigured,
        }
    }

    /// Fetch one source. The config gate happens before any request.
    async fn fetch_source(
        &self,
        cfg: &NewsConfig,
        source: &NewsSource,
    ) -> Result<Vec<NewsItem>, (NewsError, usize)> {
        let resp = self
            .client
            .get(source.url.trim())
            .timeout(Duration::from_millis(cfg.timeout_ms.clamp(500, 15_000)))
            .header(
                "Accept",
                "application/rss+xml, application/atom+xml, application/json, */*",
            )
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let code = if e.is_timeout() {
                    NEWS_TIMEOUT
                } else {
                    NEWS_OFFLINE
                };
                let detail = if e.is_timeout() {
                    format!("新闻源「{}」超时", source.alias)
                } else {
                    format!("连不上新闻源「{}」", source.alias)
                };
                return Err((NewsError::new(code, detail), 1));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let code = if status.as_u16() == 401 || status.as_u16() == 403 {
                NEWS_HTTP
            } else {
                NEWS_HTTP
            };
            return Err((
                NewsError::new(
                    code,
                    format!("新闻源「{}」返回 HTTP {status}", source.alias),
                ),
                1,
            ));
        }
        let bytes = crate::tool::http::read_bounded(resp, MAX_FEED_BODY)
            .await
            .map_err(|e| {
                let (code, detail) = match e {
                    crate::tool::http::BodyReadError::TooLarge { .. } => (
                        NEWS_RESPONSE_TOO_LARGE,
                        format!(
                            "新闻源「{}」响应超过 {} KiB",
                            source.alias,
                            MAX_FEED_BODY / 1024
                        ),
                    ),
                    // Audit P2: headers arrived, the body stalled → a timeout,
                    // not "offline".
                    crate::tool::http::BodyReadError::Timeout => (
                        NEWS_TIMEOUT,
                        format!("读取新闻源「{}」响应超时", source.alias),
                    ),
                    crate::tool::http::BodyReadError::PrematureClose(_) => (
                        NEWS_OFFLINE,
                        format!("读取新闻源「{}」时连接提前关闭", source.alias),
                    ),
                    crate::tool::http::BodyReadError::Protocol(_) => (
                        NEWS_OFFLINE,
                        format!("读取新闻源「{}」时协议异常", source.alias),
                    ),
                };
                (NewsError::new(code, detail), 1)
            })?;
        let text = String::from_utf8_lossy(&bytes);
        let items = parse_feed(source.kind, &text).map_err(|e| {
            (
                NewsError::new(e.code, format!("新闻源「{}」：{}", source.alias, e.detail)),
                1,
            )
        })?;
        Ok(items)
    }

    /// The tool path: resolve the source(s), fetch, cap, and refuse honestly.
    ///
    /// M7-3 (2026-11-13): with no source named, a configured `default_alias`
    /// means that source, and otherwise EVERY source is merged (interleaved,
    /// each item names its own source). The old behaviour — asking the model
    /// to pick whenever there were several (`NEWS_SOURCE_REQUIRED`) — turned
    /// "今天有什么新闻" into a clarifying question the household never asked.
    pub async fn headlines(
        &self,
        source: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Value, (NewsError, usize)> {
        let cfg = self.config();
        match cfg.state_word() {
            "ready" => {}
            "disabled" => return Err((NewsError::new(NEWS_DISABLED, "新闻服务已关闭"), 0)),
            _ => {
                return Err((
                    NewsError::new(NEWS_UNCONFIGURED, "新闻服务还没配置源；不要编造新闻"),
                    0,
                ))
            }
        }
        let named = source.map(str::trim).filter(|s| !s.is_empty());
        if let Some(alias) = named {
            let Some(src) = cfg.resolve_source(Some(alias)) else {
                return Err((
                    NewsError::new(
                        NEWS_SOURCE_UNKNOWN,
                        format!(
                            "没有名为「{}」的新闻源：只能用 {}",
                            alias,
                            cfg.sources
                                .iter()
                                .map(|s| s.alias.as_str())
                                .collect::<Vec<_>>()
                                .join("、")
                        ),
                    ),
                    0,
                ));
            };
            return self.single_headlines(&cfg, src, limit).await;
        }
        let picks = cfg.default_sources();
        match picks.len() {
            0 => Err((
                NewsError::new(NEWS_UNCONFIGURED, "新闻服务还没配置源；不要编造新闻"),
                0,
            )),
            1 => self.single_headlines(&cfg, picks[0], limit).await,
            _ => self.merged_headlines(&cfg, &picks, limit).await,
        }
    }

    /// One named/default source (the original shape: `source` + `items`).
    async fn single_headlines(
        &self,
        cfg: &NewsConfig,
        src: &NewsSource,
        limit: Option<u64>,
    ) -> Result<Value, (NewsError, usize)> {
        let mut items = self.fetch_source(cfg, src).await?;
        let cap = limit
            .unwrap_or(src.max_items as u64)
            .clamp(1, MAX_NEWS_ITEMS as u64) as usize;
        items.truncate(cap.min(src.max_items as usize));
        if items.is_empty() {
            return Err((
                NewsError::new(NEWS_EMPTY, format!("新闻源「{}」现在没有条目", src.alias)),
                1,
            ));
        }
        self.note(
            "news_fetched",
            json!({ "source": src.alias, "items": items.len(), "kind": src.kind.label() }),
        );
        Ok(json!({
            "source": src.alias,
            "kind": src.kind.label(),
            "fetched_ms": crate::id::now_ms(),
            "items": items.iter().map(NewsItem::to_json).collect::<Vec<_>>(),
        }))
    }

    /// Default MULTI-source answer (M7-3): fetch every source concurrently,
    /// interleave the items so no single feed dominates, and name each item's
    /// source. One broken/empty feed does not poison the answer; only "every
    /// source failed" is an error (and it names them).
    async fn merged_headlines(
        &self,
        cfg: &NewsConfig,
        picks: &[&NewsSource],
        limit: Option<u64>,
    ) -> Result<Value, (NewsError, usize)> {
        let started = Instant::now();
        let cap = limit
            .unwrap_or(MAX_NEWS_ITEMS as u64)
            .clamp(1, MAX_NEWS_ITEMS as u64) as usize;
        let results =
            futures::future::join_all(picks.iter().map(|src| self.fetch_source(cfg, src))).await;
        let mut requests = 0usize;
        let mut reports = Vec::new();
        let mut found: Vec<(&str, Vec<NewsItem>)> = Vec::new();
        let mut failures: Vec<(String, &'static str, String)> = Vec::new();
        for (src, res) in picks.iter().zip(results) {
            match res {
                Ok(items) => {
                    requests += 1;
                    let state = if items.is_empty() { "empty" } else { "ready" };
                    reports.push(json!({
                        "alias": src.alias, "kind": src.kind.label(),
                        "state": state, "items": items.len(),
                    }));
                    if !items.is_empty() {
                        found.push((src.alias.as_str(), items));
                    }
                }
                Err((e, n)) => {
                    requests += n;
                    failures.push((src.alias.clone(), e.code, e.detail.clone()));
                    reports.push(json!({
                        "alias": src.alias, "kind": src.kind.label(),
                        "state": "failed", "code": e.code, "detail": e.detail,
                    }));
                }
            }
        }
        if found.is_empty() {
            if let Some((_alias, code, detail)) = failures.first() {
                return Err((
                    NewsError::new(
                        code,
                        format!(
                            "{detail}（其余 {} 个源也没有可用条目）",
                            reports.len().saturating_sub(1)
                        ),
                    ),
                    requests,
                ));
            }
            return Err((
                NewsError::new(
                    NEWS_EMPTY,
                    format!("{} 个新闻源现在都没有条目", picks.len()),
                ),
                requests,
            ));
        }
        // Interleave: a1, b1, c1, a2, b2, … so a 3-item feed cannot fill the
        // whole answer before the others are seen.
        let mut items: Vec<Value> = Vec::new();
        let mut idx = 0usize;
        'outer: loop {
            let mut added = false;
            for (alias, list) in &found {
                if let Some(item) = list.get(idx) {
                    let mut v = item.to_json();
                    v["source"] = json!(alias);
                    items.push(v);
                    added = true;
                    if items.len() >= cap {
                        break 'outer;
                    }
                }
            }
            if !added {
                break;
            }
            idx += 1;
        }
        let aliases: Vec<&str> = found.iter().map(|(a, _)| *a).collect();
        self.note(
            "news_fetched",
            json!({ "sources": aliases, "items": items.len(), "merged": true,
                    "failed": failures.iter().map(|(a, c, _)| json!({ "alias": a, "code": c }))
                        .collect::<Vec<_>>() }),
        );
        Ok(json!({
            "merged": true,
            "sources": aliases,
            "fetched_ms": crate::id::now_ms(),
            "items": items,
            "reports": reports,
            "failed_sources": failures.iter().map(|(a, c, d)| json!({
                "alias": a, "code": c, "detail": d,
            })).collect::<Vec<_>>(),
            "http_requests": requests,
            "latency_ms": started.elapsed().as_millis(),
        }))
    }

    /// A REAL probe: fetch every configured source (bounded), report each
    /// one. Unconfigured/disabled = 0 requests.
    pub async fn probe(&self) -> Value {
        let started = Instant::now();
        let cfg = self.config();
        let state = cfg.state_word();
        if state != "ready" {
            return json!({
                "ok": false, "state": state, "detail": state_detail(state, &cfg.sources),
                "sources": [], "http_requests": 0, "latency_ms": 0,
            });
        }
        let mut reports = Vec::new();
        let mut requests = 0usize;
        let mut ok = 0usize;
        let mut empty = 0usize;
        for src in &cfg.sources {
            let one = Instant::now();
            match self.fetch_source(&cfg, src).await {
                Ok(items) if items.is_empty() => {
                    // Audit P2: a readable but empty feed is not "ready" on
                    // the probe while the tool path calls it NEWS_EMPTY.
                    requests += 1;
                    empty += 1;
                    reports.push(json!({
                        "alias": src.alias, "kind": src.kind.label(),
                        "state": "empty", "code": NEWS_EMPTY,
                        "detail": format!("新闻源「{}」现在没有条目", src.alias),
                        "items": 0,
                        "latency_ms": one.elapsed().as_millis(),
                    }));
                }
                Ok(items) => {
                    requests += 1;
                    ok += 1;
                    reports.push(json!({
                        "alias": src.alias, "kind": src.kind.label(),
                        "state": "ready", "items": items.len(),
                        "latency_ms": one.elapsed().as_millis(),
                    }));
                }
                Err((e, n)) => {
                    requests += n;
                    reports.push(json!({
                        "alias": src.alias, "kind": src.kind.label(),
                        "state": "failed", "code": e.code, "detail": e.detail,
                        "latency_ms": one.elapsed().as_millis(),
                    }));
                }
            }
        }
        let state = if ok == cfg.sources.len() {
            "ready"
        } else if ok == 0 {
            "failed"
        } else {
            // At least one source answers; the card still names the broken
            // ones instead of hiding them.
            "ready"
        };
        json!({
            "ok": ok > 0, "state": state,
            "detail": if ok == cfg.sources.len() {
                "所有新闻源都可读".to_string()
            } else if ok == 0 {
                format!(
                    "没有可读条目（{} 个为空，{} 个失败）",
                    empty,
                    cfg.sources.len() - empty
                )
            } else {
                format!(
                    "{} 个源可用，{} 个为空/失败",
                    ok,
                    cfg.sources.len() - ok
                )
            },
            "sources": reports,
            "http_requests": requests,
            "latency_ms": started.elapsed().as_millis(),
        })
    }
}

fn state_detail(state: &str, sources: &[NewsSource]) -> String {
    match state {
        "ready" => format!(
            "已配置 {} 个新闻源（{}）；只读标题，不抓正文",
            sources.len(),
            sources
                .iter()
                .map(|s| s.alias.as_str())
                .collect::<Vec<_>>()
                .join("、")
        ),
        "disabled" => "新闻已配置但当前关闭：助手不会去取新闻".into(),
        _ => "新闻服务未配置：请在控制台「新闻」里加一个 RSS/Atom/JSON 源；助手不会编造新闻".into(),
    }
}

/// Read-back shape for `/mode` and the config GET. No secret exists here.
pub fn status_json(rt: &NewsRuntime) -> Value {
    let st = ConfigService::new(rt.store.clone())
        .news_status()
        .unwrap_or(crate::config::NewsStatus {
            enabled: false,
            sources: Vec::new(),
            default_alias: String::new(),
            timeout_ms: crate::config::DEFAULT_NEWS_TIMEOUT_MS,
            max_sources: crate::config::MAX_NEWS_SOURCES,
            state: "unconfigured".into(),
            revision: String::new(),
        });
    json!({
        "enabled": st.enabled,
        "state": st.state,
        "detail": state_detail(&st.state, &st.sources),
        "sources": st.sources,
        "default_alias": st.default_alias,
        "timeout_ms": st.timeout_ms,
        "max_sources": st.max_sources,
        "revision": st.revision,
    })
}

// --- the tool --------------------------------------------------------------

pub struct NewsHeadlinesTool {
    rt: Arc<NewsRuntime>,
}

impl NewsHeadlinesTool {
    pub fn new(rt: Arc<NewsRuntime>) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for NewsHeadlinesTool {
    fn descriptor(&self) -> ToolDescriptor {
        let cfg = self.rt.config();
        let names: Vec<&str> = cfg.sources.iter().map(|s| s.alias.as_str()).collect();
        let description =
            if self.rt.availability() == Availability::Ready {
                format!(
                "读取新闻标题（只读；来自已配置的新闻源：{}）。没有条目就如实说没有，不要编造新闻",
                if names.is_empty() { "（无）".into() } else { names.join("、") }
            )
            } else {
                "读取新闻标题（新闻服务尚未配置：不会发出任何请求，也不会编造新闻）".into()
            };
        ToolDescriptor {
            name: "news.headlines".into(),
            service: "news".into(),
            side_effect: SideEffect::Query,
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "新闻源别名；不填就用默认源（配了多个源且没有默认源时合并全部源）" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_NEWS_ITEMS,
                               "description": "最多几条（默认按源的设置；合并时是总数上限）" }
                }
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
        let source = args.get("source").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64);
        match self.rt.headlines(source, limit).await {
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

    #[test]
    fn rss_titles_links_and_entities_are_clean() {
        let body = r#"<?xml version="1.0"?><rss><channel>
            <item><title><![CDATA[<b>Hello</b> &amp; world]]></title>
              <link>https://example.com/a</link><pubDate>Thu, 13 Nov 2026 10:00:00 GMT</pubDate></item>
            <item><title>Second &lt;story&gt;</title><link>javascript:alert(1)</link></item>
        </channel></rss>"#;
        let items = parse_feed(NewsKind::Rss, body).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Hello & world");
        assert_eq!(items[0].link.as_deref(), Some("https://example.com/a"));
        assert_eq!(items[1].title, "Second <story>");
        assert_eq!(items[1].link, None, "non-http links are dropped");
    }

    #[test]
    fn atom_entry_uses_the_href_attribute() {
        let body = r#"<feed><entry><title>One</title>
            <link rel="alternate" href="https://example.com/one"/></entry></feed>"#;
        let items = parse_feed(NewsKind::Atom, body).unwrap();
        assert_eq!(items[0].title, "One");
        assert_eq!(items[0].link.as_deref(), Some("https://example.com/one"));
    }

    #[test]
    fn json_accepts_both_shapes_and_skips_untitled_items() {
        let a = parse_feed(
            NewsKind::Json,
            r#"{"items":[{"title":"A","link":"https://x/a"},{"link":"https://x/b"}]}"#,
        )
        .unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].title, "A");
        let b = parse_feed(NewsKind::Json, r#"[{"title":"B"}]"#).unwrap();
        assert_eq!(b[0].title, "B");
        assert!(parse_feed(NewsKind::Json, r#"{"nope":1}"#).is_err());
    }

    #[test]
    fn a_long_title_is_bounded() {
        let title = "长".repeat(500);
        let body = format!(r#"<rss><channel><item><title>{title}</title></item></channel></rss>"#);
        let items = parse_feed(NewsKind::Rss, &body).unwrap();
        assert_eq!(items[0].title.chars().count(), MAX_TITLE_CHARS + 1);
        assert!(items[0].title.ends_with('…'));
    }

    #[test]
    fn an_empty_feed_parses_to_no_items() {
        let items = parse_feed(NewsKind::Rss, "<rss><channel></channel></rss>").unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn a_truncated_document_is_a_parse_error() {
        assert!(parse_feed(NewsKind::Rss, "<rss><channel>").is_err());
        assert!(
            parse_feed(NewsKind::Rss, "<rss><channel><item><title>A</title></item>").is_err(),
            "an item closed but the root is not"
        );
        assert!(parse_feed(NewsKind::Atom, "<feed><entry><title>A</title></entry>").is_err());
    }
}
