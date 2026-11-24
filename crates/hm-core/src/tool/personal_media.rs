//! NAS `story-svc` (:18083) and `joke-svc` (:18081) tool integrations.
//!
//! Both services are optional: registered by default, unconfigured until a
//! household sets their LAN/HTTPS URL and enables them. When unconfigured they
//! report `Availability::Unconfigured` without touching the network.

use crate::config::{
    ConfigService, PersonalEndpointConfig, JOKE_KEY, JOKE_TOKEN_SECRET, RADIO_KEY,
    RADIO_TOKEN_SECRET, STORY_KEY, STORY_TOKEN_SECRET,
};
use crate::gateway::GatewayHub;
use crate::store::Store;
use crate::tool::http::{read_bounded, validate_news_url};
use crate::tool::{
    Availability, Evidence, FactBasis, SideEffect, Tool, ToolCtx, ToolDescriptor, ToolFact,
    ToolResult, ToolStatus,
};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_JSON_BODY: usize = 512 * 1024;

fn valid_path_segment(raw: &str) -> bool {
    !raw.is_empty()
        && !raw.contains("..")
        && !raw
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#') || c.is_whitespace() || c.is_control())
}

#[derive(Clone, Debug)]
pub struct StoryCandidate {
    pub id: String,
    pub title: String,
    pub kind: String,
}

#[derive(Clone)]
pub struct PersonalMediaRuntime {
    store: Arc<Store>,
    hub: Arc<GatewayHub>,
    music: Option<Arc<crate::tool::music::MusicRuntime>>,
    client: reqwest::Client,
    titles: Arc<Mutex<HashMap<String, String>>>,
    kinds: Arc<Mutex<HashMap<String, String>>>,
    radio_titles: Arc<Mutex<HashMap<String, String>>>,
    recent_search: Arc<Mutex<Vec<StoryCandidate>>>,
    recent_episodes: Arc<Mutex<Vec<(String, String)>>>,
    unplayable: Arc<Mutex<HashSet<String>>>,
}

impl PersonalMediaRuntime {
    pub fn new(store: Arc<Store>, hub: Arc<GatewayHub>) -> Self {
        let allow_loopback = store.allows_loopback_public();
        let client = crate::tool::http::apply_policy_resolver(
            reqwest::Client::builder(),
            crate::tool::http::OutboundPolicy::PublicOrLan { allow_loopback },
        )
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            store,
            hub,
            music: None,
            client,
            titles: Arc::new(Mutex::new(HashMap::new())),
            kinds: Arc::new(Mutex::new(HashMap::new())),
            radio_titles: Arc::new(Mutex::new(HashMap::new())),
            recent_search: Arc::new(Mutex::new(Vec::new())),
            recent_episodes: Arc::new(Mutex::new(Vec::new())),
            unplayable: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn remember_story_items(&self, payload: &mut Value) {
        let bad = crate::lock_or_recover(&self.unplayable, "story.unplayable").clone();
        let mut titles = crate::lock_or_recover(&self.titles, "story.titles");
        let mut kinds = crate::lock_or_recover(&self.kinds, "story.kinds");
        if titles.len() > 512 {
            titles.clear();
            kinds.clear();
        }
        let mut eps: Vec<(String, String)> = Vec::new();
        let mut search_hits: Vec<StoryCandidate> = Vec::new();
        for key in ["results", "episodes"] {
            if let Some(arr) = payload.get_mut(key).and_then(Value::as_array_mut) {
                arr.retain(|item| item["id"].as_str().is_none_or(|id| !bad.contains(id)));
                for item in arr.iter() {
                    if let (Some(id), Some(title)) = (
                        item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()),
                        item["title"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty()),
                    ) {
                        let kind = item["kind"].as_str().unwrap_or("episode").to_string();
                        titles.insert(id.to_string(), title.to_string());
                        kinds.insert(id.to_string(), kind.clone());
                        if key == "results" {
                            search_hits.push(StoryCandidate {
                                id: id.to_string(),
                                title: title.to_string(),
                                kind: kind.clone(),
                            });
                        }
                        if kind != "album" {
                            eps.push((id.to_string(), title.to_string()));
                        }
                    }
                }
            }
        }
        if !search_hits.is_empty() {
            *crate::lock_or_recover(&self.recent_search, "story.recent_search") = search_hits;
        }
        if !eps.is_empty() {
            *crate::lock_or_recover(&self.recent_episodes, "story.recent_episodes") = eps;
        }
    }

    pub fn lookup_title(&self, track_id: &str) -> Option<String> {
        crate::lock_or_recover(&self.titles, "story.titles")
            .get(track_id)
            .cloned()
    }

    pub fn lookup_kind(&self, id: &str) -> Option<String> {
        crate::lock_or_recover(&self.kinds, "story.kinds")
            .get(id)
            .cloned()
    }

    pub fn match_recent_search(&self, query: &str, title: &str) -> Option<StoryCandidate> {
        let bad = crate::lock_or_recover(&self.unplayable, "story.unplayable").clone();
        let recent = crate::lock_or_recover(&self.recent_search, "story.recent_search");
        let valid: Vec<&StoryCandidate> = recent
            .iter()
            .filter(|c| !bad.contains(&c.id) && valid_path_segment(&c.id))
            .collect();
        if valid.is_empty() {
            return None;
        }
        for needle in [title.trim(), query.trim()] {
            if needle.is_empty() {
                continue;
            }
            if let Some(exact) = valid
                .iter()
                .find(|c| c.title.contains(needle) || needle.contains(&c.title))
            {
                return Some((*exact).clone());
            }
        }
        // When `story.search` already ran in the current turn with a corrected query
        // (e.g. ASR "一千零一页" -> LLM searched "一千零一夜") and then `story.play`
        // omitted `track_id`, prefer the top candidate from `recent_search` if
        // at least 2 characters overlap.
        let probe = if !title.trim().is_empty() {
            title.trim()
        } else {
            query.trim()
        };
        if probe.chars().count() >= 2 {
            if let Some(overlap) = valid.iter().find(|c| {
                let shared = probe.chars().filter(|ch| c.title.contains(*ch)).count();
                shared >= 2
            }) {
                return Some((*overlap).clone());
            }
        }
        None
    }

    pub fn remember_radio_items(&self, payload: &Value) {
        let mut titles = crate::lock_or_recover(&self.radio_titles, "radio.titles");
        if titles.len() > 512 {
            titles.clear();
        }
        for key in ["results", "stations"] {
            if let Some(arr) = payload.get(key).and_then(Value::as_array) {
                for item in arr {
                    if let (Some(id), Some(title)) = (
                        item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()),
                        item["title"]
                            .as_str()
                            .or_else(|| item["name"].as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty()),
                    ) {
                        titles.insert(id.to_string(), title.to_string());
                    }
                }
            }
        }
    }

    pub fn lookup_radio_title(&self, station_id: &str) -> Option<String> {
        crate::lock_or_recover(&self.radio_titles, "radio.titles")
            .get(station_id)
            .cloned()
    }

    pub fn mark_unplayable_and_next(
        &self,
        failed_id: &str,
        allow_fallback: bool,
    ) -> Option<(String, String)> {
        {
            let mut bad = crate::lock_or_recover(&self.unplayable, "story.unplayable");
            if bad.len() >= 256 {
                bad.clear();
            }
            bad.insert(failed_id.to_string());
        }
        if !allow_fallback {
            return None;
        }
        let bad = crate::lock_or_recover(&self.unplayable, "story.unplayable").clone();
        let recent = crate::lock_or_recover(&self.recent_episodes, "story.recent_episodes");
        if !recent.iter().any(|(id, _)| id == failed_id) {
            return None;
        }
        recent
            .iter()
            .find(|(id, _)| id != failed_id && !bad.contains(id) && valid_path_segment(id))
            .cloned()
    }

    pub fn with_music(mut self, music: Arc<crate::tool::music::MusicRuntime>) -> Self {
        self.music = Some(music);
        self
    }

    pub fn story_config(&self) -> (PersonalEndpointConfig, Option<String>) {
        let svc = ConfigService::new(self.store.clone());
        let cfg = svc.personal_endpoint(STORY_KEY).unwrap_or_default();
        let tok = svc.personal_token(STORY_TOKEN_SECRET).ok().flatten();
        (cfg, tok)
    }

    pub fn joke_config(&self) -> (PersonalEndpointConfig, Option<String>) {
        let svc = ConfigService::new(self.store.clone());
        let cfg = svc.personal_endpoint(JOKE_KEY).unwrap_or_default();
        let tok = svc.personal_token(JOKE_TOKEN_SECRET).ok().flatten();
        (cfg, tok)
    }

    pub fn radio_config(&self) -> (PersonalEndpointConfig, Option<String>) {
        let svc = ConfigService::new(self.store.clone());
        let cfg = svc.personal_endpoint(RADIO_KEY).unwrap_or_default();
        let tok = svc.personal_token(RADIO_TOKEN_SECRET).ok().flatten();
        (cfg, tok)
    }

    async fn get_json(
        &self,
        cfg: &PersonalEndpointConfig,
        token: Option<&str>,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, String> {
        let url = format!("{}{}", cfg.url.trim_end_matches('/'), path);
        validate_news_url(&url, self.store.allows_loopback_public())
            .map_err(|e| format!("invalid url: {e}"))?;
        let mut req = self
            .client
            .get(&url)
            .query(query)
            .timeout(Duration::from_millis(cfg.timeout_ms));
        if let Some(tok) = token.filter(|t| !t.trim().is_empty()) {
            req = req.bearer_auth(tok.trim());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        let bytes = read_bounded(resp, MAX_JSON_BODY)
            .await
            .map_err(|e| format!("body read failed: {e}"))?;
        if !status.is_success() {
            let preview = String::from_utf8_lossy(&bytes);
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                preview.chars().take(200).collect::<String>()
            ));
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid json: {e}"))
    }

    pub async fn test_story(&self) -> Value {
        let (cfg, tok) = self.story_config();
        if !cfg.is_ready() {
            return json!({ "ok": false, "state": "unconfigured", "detail": "story-svc not enabled or url missing" });
        }
        match self.get_json(&cfg, tok.as_deref(), "/health", &[]).await {
            Ok(health) => match self
                .get_json(
                    &cfg,
                    tok.as_deref(),
                    "/v1/search",
                    &[("q", "故事"), ("limit", "1")],
                )
                .await
            {
                Ok(search) => {
                    json!({ "ok": true, "state": "ready", "health": health, "sample_count": search["results"].as_array().map(|a| a.len()).unwrap_or(0) })
                }
                Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
            },
            Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
        }
    }

    pub async fn test_joke(&self) -> Value {
        let (cfg, tok) = self.joke_config();
        if !cfg.is_ready() {
            return json!({ "ok": false, "state": "unconfigured", "detail": "joke-svc not enabled or url missing" });
        }
        match self.get_json(&cfg, tok.as_deref(), "/health", &[]).await {
            Ok(health) => match self.get_json(&cfg, tok.as_deref(), "/v1/joke", &[]).await {
                Ok(joke) => {
                    json!({ "ok": true, "state": "ready", "health": health, "sample_id": joke["id"] })
                }
                Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
            },
            Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
        }
    }

    pub async fn test_radio(&self) -> Value {
        let (cfg, tok) = self.radio_config();
        if !cfg.is_ready() {
            return json!({ "ok": false, "state": "unconfigured", "detail": "radio-svc not enabled or url missing" });
        }
        match self.get_json(&cfg, tok.as_deref(), "/health", &[]).await {
            Ok(health) => match self
                .get_json(
                    &cfg,
                    tok.as_deref(),
                    "/v1/search",
                    &[("q", "933"), ("limit", "1")],
                )
                .await
            {
                Ok(search) => json!({
                    "ok": true,
                    "state": "ready",
                    "health": health,
                    "sample_count": search["results"].as_array().map(|a| a.len()).unwrap_or(0)
                }),
                Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
            },
            Err(e) => json!({ "ok": false, "state": "error", "detail": e }),
        }
    }
}

pub struct JokeGetTool {
    rt: PersonalMediaRuntime,
}

impl JokeGetTool {
    pub fn new(rt: PersonalMediaRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for JokeGetTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("joke.get"),
            service: Cow::Borrowed("joke"),
            side_effect: SideEffect::Query,
            description: "Fetch a joke, riddle, or pun from the household NAS joke-svc library."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["joke", "riddle", "pun"] },
                    "topic": { "type": "string", "description": "Optional topic or keyword" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        if self.rt.joke_config().0.is_ready() {
            Availability::Ready
        } else {
            Availability::Unconfigured
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.joke_config();
        if !cfg.is_ready() {
            return ToolResult::failed("JOKE_UNCONFIGURED", "joke-svc is not configured", false);
        }
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(k) = args["kind"].as_str().filter(|s| !s.is_empty()) {
            query.push(("kind", k.to_string()));
        }
        if let Some(t) = args["topic"].as_str().filter(|s| !s.is_empty()) {
            query.push(("topic", t.to_string()));
        }
        if let Some(dev) = ctx.device_id.as_deref().filter(|s| !s.is_empty()) {
            query.push(("pid", dev.to_string()));
        }
        let refs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
        match self
            .rt
            .get_json(&cfg, tok.as_deref(), "/v1/joke", &refs)
            .await
        {
            Ok(v) => ToolResult::observed_ok(v),
            Err(e) => ToolResult::failed("JOKE_ERROR", e, true),
        }
    }
}

pub struct StorySearchTool {
    rt: PersonalMediaRuntime,
}

impl StorySearchTool {
    pub fn new(rt: PersonalMediaRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for StorySearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("story.search"),
            service: Cow::Borrowed("story"),
            side_effect: SideEffect::Query,
            description: "Search stories or audio albums in the household NAS story-svc library."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        if self.rt.story_config().0.is_ready() {
            Availability::Ready
        } else {
            Availability::Unconfigured
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.story_config();
        if !cfg.is_ready() {
            return ToolResult::failed("STORY_UNCONFIGURED", "story-svc is not configured", false);
        }
        let Some(q) = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::failed("INVALID_ARGS", "query is required", false);
        };
        let limit = args["limit"]
            .as_u64()
            .unwrap_or(10)
            .clamp(1, 20)
            .to_string();
        match self
            .rt
            .get_json(
                &cfg,
                tok.as_deref(),
                "/v1/search",
                &[("q", q), ("limit", &limit)],
            )
            .await
        {
            Ok(mut v) => {
                self.rt.remember_story_items(&mut v);
                ToolResult::observed_ok(v)
            }
            Err(e) => ToolResult::failed("STORY_ERROR", e, true),
        }
    }
}

pub struct StoryAlbumTool {
    rt: PersonalMediaRuntime,
}

impl StoryAlbumTool {
    pub fn new(rt: PersonalMediaRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for StoryAlbumTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("story.album"),
            service: Cow::Borrowed("story"),
            side_effect: SideEffect::Query,
            description: "List episodes of a story album from story-svc.".into(),
            parameters: json!({
                "type": "object",
                "required": ["album_id"],
                "properties": { "album_id": { "type": "string" } }
            }),
        }
    }

    fn availability(&self) -> Availability {
        if self.rt.story_config().0.is_ready() {
            Availability::Ready
        } else {
            Availability::Unconfigured
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.story_config();
        if !cfg.is_ready() {
            return ToolResult::failed("STORY_UNCONFIGURED", "story-svc is not configured", false);
        }
        let Some(aid) = args["album_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| valid_path_segment(s))
        else {
            return ToolResult::failed("INVALID_ARGS", "valid album_id is required", false);
        };
        match self
            .rt
            .get_json(&cfg, tok.as_deref(), &format!("/v1/album/{aid}"), &[])
            .await
        {
            Ok(mut v) => {
                self.rt.remember_story_items(&mut v);
                ToolResult::observed_ok(v)
            }
            Err(e) => ToolResult::failed("STORY_ERROR", e, true),
        }
    }
}

pub struct StoryPlayTool {
    rt: PersonalMediaRuntime,
}

impl StoryPlayTool {
    pub fn new(rt: PersonalMediaRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for StoryPlayTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("story.play"),
            service: Cow::Borrowed("story"),
            side_effect: SideEffect::Mutate,
            description:
                "Play a story episode or album on the speaker from story-svc. Always pass track_id (or album_id) returned by story.search / story.album when available."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "track_id": { "type": "string", "description": "Exact episode or album id from story.search / story.album (preferred)." },
                    "album_id": { "type": "string", "description": "Optional album id to queue and play all episodes of an album." },
                    "query": { "type": "string", "description": "Fallback search query if track_id was not searched yet." },
                    "title": { "type": "string", "description": "Story or album title." }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        if self.rt.story_config().0.is_ready() {
            Availability::Ready
        } else {
            Availability::Unconfigured
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.story_config();
        if !cfg.is_ready() {
            return ToolResult::failed("STORY_UNCONFIGURED", "story-svc is not configured", false);
        }
        let mut track_id = args["track_id"]
            .as_str()
            .or_else(|| args["id"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut album_id = args["album_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut title = args["title"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let query_arg = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let mut episode_queue: Vec<(String, String)> = Vec::new();

        // 1. If neither track_id nor album_id was passed, first reuse any matching
        // candidate from the turn's `story.search` before issuing a new search with
        // a potentially raw/homophone query.
        if track_id.is_none() && album_id.is_none() {
            if let Some(cand) = self.rt.match_recent_search(query_arg, &title) {
                if cand.kind == "album" {
                    album_id = Some(cand.id);
                } else {
                    track_id = Some(cand.id);
                }
                if title.is_empty() {
                    title = cand.title;
                }
            }
        }

        // 2. If track_id was passed and is known to be an album hit from story.search,
        // treat it as an album so we load the full album episode queue.
        if album_id.is_none() {
            if let Some(tid) = track_id.as_deref() {
                if self.rt.lookup_kind(tid).as_deref() == Some("album") {
                    album_id = Some(tid.to_string());
                }
            }
        }

        // 3. Still no track_id or album_id: run `/v1/search` and respect #1 ranking
        // (do NOT skip a top-ranked album in favor of a random lower-ranked web clip).
        if track_id.is_none() && album_id.is_none() {
            let q = if !query_arg.is_empty() {
                query_arg
            } else if !title.is_empty() {
                title.as_str()
            } else {
                return ToolResult::failed("INVALID_ARGS", "track_id or query is required", false);
            };
            let mut res = match self
                .rt
                .get_json(
                    &cfg,
                    tok.as_deref(),
                    "/v1/search",
                    &[("q", q), ("limit", "5")],
                )
                .await
            {
                Ok(v) => v,
                Err(e) => return ToolResult::failed("STORY_ERROR", e, true),
            };
            self.rt.remember_story_items(&mut res);
            let Some(hits) = res["results"].as_array() else {
                return ToolResult::failed("STORY_NOT_FOUND", "no matching story found", false);
            };
            if let Some(first) = hits.first() {
                let first_id = first["id"].as_str().map(str::to_string);
                let first_title = first["title"].as_str().unwrap_or("").to_string();
                if first["kind"] == "album" {
                    album_id = first_id;
                    if title.is_empty() {
                        title = first_title;
                    }
                } else {
                    track_id = first_id;
                    if title.is_empty() {
                        title = first_title;
                    }
                    for h in hits.iter().filter(|h| h["kind"] != "album") {
                        if let Some(hid) = h["id"].as_str().filter(|s| valid_path_segment(s)) {
                            let htitle = h["title"].as_str().unwrap_or(hid).to_string();
                            episode_queue.push((hid.to_string(), htitle));
                        }
                    }
                }
            }
        }

        // 4. If an album was selected (by album_id, album track_id, or top search hit),
        // load its episodes from `/v1/album/{base_album_id}` into `episode_queue`.
        if let Some(aid_raw) = album_id.as_deref() {
            let base_aid = aid_raw.rsplit_once(':').map(|(a, _)| a).unwrap_or(aid_raw);
            if valid_path_segment(base_aid) {
                if let Ok(mut a) = self
                    .rt
                    .get_json(&cfg, tok.as_deref(), &format!("/v1/album/{base_aid}"), &[])
                    .await
                {
                    self.rt.remember_story_items(&mut a);
                    if let Some(eps) = a["episodes"].as_array() {
                        for ep in eps {
                            if let Some(eid) = ep["id"].as_str().filter(|s| valid_path_segment(s)) {
                                let etitle = ep["title"].as_str().unwrap_or(eid).to_string();
                                episode_queue.push((eid.to_string(), etitle));
                            }
                        }
                    }
                }
            }
            if let Some((first_id, first_title)) = episode_queue.first() {
                if track_id
                    .as_deref()
                    .is_none_or(|tid| !episode_queue.iter().any(|(id, _)| id == tid))
                {
                    track_id = Some(first_id.clone());
                    title = first_title.clone();
                }
            }
        }

        let Some(mut tid) = track_id.filter(|s| valid_path_segment(s)) else {
            return ToolResult::failed("STORY_NOT_FOUND", "no valid story episode found", false);
        };
        let device = ctx
            .device_id
            .clone()
            .or_else(|| self.rt.hub.primary_audio_out().map(|d| d.stable_id));
        let Some(device) = device else {
            return ToolResult::failed("NO_DEVICE", "no audio output device online", false);
        };
        let mut label = self
            .rt
            .lookup_title(&tid)
            .or_else(|| (!title.is_empty()).then(|| title.clone()))
            .unwrap_or_else(|| tid.clone());
        if let Some(music) = &self.rt.music {
            let binding = format!("story|{}", cfg.url.trim_end_matches('/'));
            for attempt in 0..3 {
                let mut tracks: Vec<crate::tool::music::Track> = episode_queue
                    .iter()
                    .filter(|(id, _)| valid_path_segment(id))
                    .map(|(id, t)| crate::tool::music::Track {
                        source: crate::tool::music::Source::Story,
                        id: id.clone(),
                        title: if t.is_empty() { id.clone() } else { t.clone() },
                        artist: "story".into(),
                        binding: binding.clone(),
                    })
                    .collect();
                let start_index = if let Some(pos) = tracks.iter().position(|t| t.id == tid) {
                    pos
                } else {
                    tracks.insert(
                        0,
                        crate::tool::music::Track {
                            source: crate::tool::music::Source::Story,
                            id: tid.clone(),
                            title: label.clone(),
                            artist: "story".into(),
                            binding: binding.clone(),
                        },
                    );
                    0
                };
                match music
                    .start_story_queue(device.clone(), tracks, start_index, &ctx)
                    .await
                {
                    Ok((play_id, evidence, detail)) => {
                        let confirmed =
                            matches!(evidence, Evidence::DeviceConfirmed | Evidence::Sent);
                        return ToolResult {
                            status: if confirmed {
                                ToolStatus::Completed
                            } else {
                                ToolStatus::Unknown
                            },
                            evidence,
                            result: Some(
                                json!({ "status": if confirmed { "playing" } else { "unknown" }, "device": device, "play_id": play_id, "track_id": tid, "title": label, "detail": detail }),
                            ),
                            error: None,
                            fact: Some(ToolFact {
                                subject: format!("《{label}》"),
                                action: "play".into(),
                                basis: FactBasis::DeviceStage,
                                confirmed,
                                detail: Some(detail),
                                value: None,
                            }),
                        };
                    }
                    Err((code, msg)) => {
                        if code == "CANCELLED" {
                            return ToolResult::cancelled();
                        }
                        if code == "MUSIC_DECODE" {
                            if let Some((next_id, next_title)) =
                                self.rt.mark_unplayable_and_next(&tid, attempt < 2)
                            {
                                tid = next_id;
                                label = if next_title.is_empty() {
                                    tid.clone()
                                } else {
                                    next_title
                                };
                                continue;
                            }
                        }
                        return ToolResult::failed(&code, msg, true);
                    }
                }
            }
        }
        ToolResult::failed("MUSIC_UNAVAILABLE", "media runtime not attached", false)
    }
}

pub struct RadioSearchTool {
    rt: PersonalMediaRuntime,
}

impl RadioSearchTool {
    pub fn new(rt: PersonalMediaRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for RadioSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("radio.search"),
            service: Cow::Borrowed("radio"),
            side_effect: SideEffect::Query,
            description: "List live radio stations from the household NAS radio-svc by frequency, station name, language, or tag. Only for answering which stations exist; to listen, call radio.play directly instead.".into(),
            parameters: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Frequency (e.g. 933, 97.2), station name, or genre tag" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        if self.rt.radio_config().0.is_ready() {
            Availability::Ready
        } else {
            Availability::Unconfigured
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.radio_config();
        if !cfg.is_ready() {
            return ToolResult::failed("RADIO_UNCONFIGURED", "radio-svc is not configured", false);
        }
        let Some(q) = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::failed("INVALID_ARGS", "query is required", false);
        };
        let limit = args["limit"]
            .as_u64()
            .unwrap_or(10)
            .clamp(1, 20)
            .to_string();
        match self
            .rt
            .get_json(
                &cfg,
                tok.as_deref(),
                "/v1/search",
                &[("q", q), ("limit", &limit)],
            )
            .await
        {
            Ok(v) => {
                self.rt.remember_radio_items(&v);
                ToolResult::observed_ok(v)
            }
            Err(e) => ToolResult::failed("RADIO_ERROR", e, true),
        }
    }
}

pub struct RadioPlayTool {
    rt: PersonalMediaRuntime,
}

impl RadioPlayTool {
    pub fn new(rt: PersonalMediaRuntime) -> Self {
        Self { rt }
    }
}

#[async_trait::async_trait]
impl Tool for RadioPlayTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: Cow::Borrowed("radio.play"),
            service: Cow::Borrowed("radio"),
            side_effect: SideEffect::Mutate,
            description: "Play a live radio station on the speaker. Call this directly whenever the user wants to listen to radio or a frequency: pass station_id, or query (frequency, name, genre; use \"电台\" if none given). Do not search first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "station_id": { "type": "string", "description": "Station id returned by radio.search (e.g. sg-yes933)" },
                    "query": { "type": "string", "description": "Frequency, station name, or genre (e.g. 933, 华语电台)" },
                    "title": { "type": "string" }
                }
            }),
        }
    }

    fn availability(&self) -> Availability {
        if self.rt.radio_config().0.is_ready() {
            Availability::Ready
        } else {
            Availability::Unconfigured
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let (cfg, tok) = self.rt.radio_config();
        if !cfg.is_ready() {
            return ToolResult::failed("RADIO_UNCONFIGURED", "radio-svc is not configured", false);
        }
        let binding = format!("radio|{}", cfg.url.trim_end_matches('/'));
        let explicit_id = args["station_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let explicit_title = args["title"]
            .as_str()
            .map(str::trim)
            .unwrap_or("")
            .to_string();

        let mut tracks: Vec<crate::tool::music::Track> = Vec::new();
        if let Some(sid) = explicit_id {
            if !valid_path_segment(&sid) {
                return ToolResult::failed("RADIO_NOT_FOUND", "invalid station_id", false);
            }
            let title = self
                .rt
                .lookup_radio_title(&sid)
                .or_else(|| (!explicit_title.is_empty()).then(|| explicit_title.clone()))
                .unwrap_or_else(|| sid.clone());
            tracks.push(crate::tool::music::Track {
                source: crate::tool::music::Source::Radio,
                id: sid,
                title,
                artist: "radio".into(),
                binding: binding.clone(),
            });
        } else {
            let q = args["query"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("电台");
            match self
                .rt
                .get_json(
                    &cfg,
                    tok.as_deref(),
                    "/v1/search",
                    &[("q", q), ("limit", "5")],
                )
                .await
            {
                Ok(v) => {
                    self.rt.remember_radio_items(&v);
                    if let Some(arr) = v["results"].as_array() {
                        for item in arr {
                            let Some(id) = item["id"].as_str().filter(|s| valid_path_segment(s))
                            else {
                                continue;
                            };
                            let title = item["title"]
                                .as_str()
                                .or_else(|| item["name"].as_str())
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or(id)
                                .to_string();
                            let artist = item["artist"]
                                .as_str()
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or("radio")
                                .to_string();
                            tracks.push(crate::tool::music::Track {
                                source: crate::tool::music::Source::Radio,
                                id: id.to_string(),
                                title,
                                artist,
                                binding: binding.clone(),
                            });
                        }
                    }
                }
                Err(e) => return ToolResult::failed("RADIO_ERROR", e, true),
            }
        }
        if tracks.is_empty() {
            return ToolResult::failed("RADIO_NOT_FOUND", "no matching radio station found", false);
        }
        // "下一首" / "换个台" walks this queue and never re-searches, so a
        // single chosen station must not be the whole queue: append the other
        // stations (best effort) behind it.
        if let Ok(v) = self
            .rt
            .get_json(&cfg, tok.as_deref(), "/v1/stations", &[])
            .await
        {
            if let Some(arr) = v["stations"].as_array() {
                let mut rest: Vec<&Value> = arr
                    .iter()
                    .filter(|i| i["enabled"].as_bool().unwrap_or(true))
                    .collect();
                rest.sort_by_key(|i| !i["favorite"].as_bool().unwrap_or(false));
                for item in rest {
                    let Some(id) = item["id"].as_str().filter(|s| valid_path_segment(s)) else {
                        continue;
                    };
                    if tracks.iter().any(|t| t.id == id) {
                        continue;
                    }
                    let title = item["title"]
                        .as_str()
                        .or_else(|| item["name"].as_str())
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(id)
                        .to_string();
                    tracks.push(crate::tool::music::Track {
                        source: crate::tool::music::Source::Radio,
                        id: id.to_string(),
                        title,
                        artist: "radio".into(),
                        binding: binding.clone(),
                    });
                }
            }
        }
        let device = ctx
            .device_id
            .clone()
            .or_else(|| self.rt.hub.primary_audio_out().map(|d| d.stable_id));
        let Some(device) = device else {
            return ToolResult::failed("NO_DEVICE", "no audio output device online", false);
        };
        let Some(music) = &self.rt.music else {
            return ToolResult::failed("MUSIC_UNAVAILABLE", "media runtime not attached", false);
        };

        let first = tracks[0].clone();
        match music.start_radio(device.clone(), tracks, 0, ctx).await {
            Ok((play_id, evidence, detail)) => {
                let confirmed = matches!(evidence, Evidence::DeviceConfirmed | Evidence::Sent);
                ToolResult {
                    status: if confirmed {
                        ToolStatus::Completed
                    } else {
                        ToolStatus::Unknown
                    },
                    evidence,
                    result: Some(json!({
                        "status": if confirmed { "playing" } else { "unknown" },
                        "device": device,
                        "play_id": play_id,
                        "station_id": first.id,
                        "title": first.title,
                        "detail": detail,
                    })),
                    error: None,
                    fact: Some(ToolFact {
                        subject: format!("《{}》", first.title),
                        action: "play".into(),
                        basis: FactBasis::DeviceStage,
                        confirmed,
                        detail: Some(detail),
                        value: None,
                    }),
                }
            }
            Err((code, msg)) => {
                if code == "CANCELLED" {
                    ToolResult::cancelled()
                } else {
                    ToolResult::failed(&code, msg, true)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolStatus;
    use axum::{routing::get, Json, Router};
    use std::sync::atomic::AtomicBool;

    fn tiny_wav_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        let samples = 1600u32;
        let data_len = samples * 2;
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&16000u32.to_le_bytes());
        out.extend_from_slice(&(16000u32 * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(44 + data_len as usize, 0);
        out
    }

    #[tokio::test]
    async fn joke_and_story_tools_refuse_when_unconfigured_and_work_when_configured() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let hub = Arc::new(GatewayHub::new());
        let rt = PersonalMediaRuntime::new(store.clone(), Arc::clone(&hub));

        let joke_tool = JokeGetTool::new(rt.clone());
        let story_tool = StorySearchTool::new(rt.clone());
        let album_tool = StoryAlbumTool::new(rt.clone());
        assert_eq!(joke_tool.availability(), Availability::Unconfigured);
        assert_eq!(story_tool.availability(), Availability::Unconfigured);

        let app = Router::new()
            .route("/health", get(|| async { Json(json!({ "ok": true })) }))
            .route("/v1/joke", get(|| async { Json(json!({ "id": "j1", "kind": "joke", "text": "哈哈", "source": "library" })) }))
            .route("/v1/search", get(|| async { Json(json!({ "query": "睡前", "results": [{ "id": "cat:123:1", "kind": "album", "title": "童话", "source": "catalog" }] })) }))
            .route("/v1/album/cat:123", get(|| async { Json(json!({ "id": "cat:123", "name": "童话", "episodes": [{ "id": "cat:123:1", "title": "小星星" }] })) }))
            .route("/v1/stream/cat:123:1", get(|| async { tiny_wav_bytes() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");

        let svc = ConfigService::new(store);
        svc.save_personal_endpoint(
            JOKE_KEY,
            JOKE_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: base.clone(),
                timeout_ms: 3000,
            },
            Some("tok"),
            "test",
        )
        .unwrap();
        svc.save_personal_endpoint(
            STORY_KEY,
            STORY_TOKEN_SECRET,
            PersonalEndpointConfig {
                enabled: true,
                url: base,
                timeout_ms: 3000,
            },
            Some("tok"),
            "test",
        )
        .unwrap();

        assert_eq!(joke_tool.availability(), Availability::Ready);
        assert_eq!(story_tool.availability(), Availability::Ready);

        let ctx = ToolCtx::new("T1", "C1", Arc::new(AtomicBool::new(false)));
        let jr = joke_tool.execute(json!({}), &ctx).await;
        assert_eq!(jr.status, ToolStatus::Completed);
        assert_eq!(jr.result.unwrap()["id"], "j1");

        let sr = story_tool.execute(json!({ "query": "睡前" }), &ctx).await;
        assert_eq!(sr.status, ToolStatus::Completed);
        assert_eq!(sr.result.unwrap()["results"][0]["id"], "cat:123:1");

        let ar = album_tool
            .execute(json!({ "album_id": "cat:123" }), &ctx)
            .await;
        assert_eq!(ar.status, ToolStatus::Completed);
        let bad = album_tool
            .execute(json!({ "album_id": "../admin/config" }), &ctx)
            .await;
        assert_eq!(bad.status, ToolStatus::Failed);
        assert_eq!(bad.error.unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn radio_tools_test_search_and_play() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let store = Arc::new(Store::open_in_memory().unwrap());
        store.allow_loopback_public_for_test();
        let hub = GatewayHub::new();
        let music = crate::tool::music::MusicRuntime::new(store.clone(), hub.clone());
        let rt =
            PersonalMediaRuntime::new(store.clone(), Arc::clone(&hub)).with_music(music.clone());

        let search_tool = RadioSearchTool::new(rt.clone());
        let play_tool = RadioPlayTool::new(rt.clone());
        let control_tool = crate::tool::music::MusicControlTool::new(music.clone());
        assert_eq!(search_tool.availability(), Availability::Unconfigured);
        assert_eq!(play_tool.availability(), Availability::Unconfigured);
        assert_eq!(rt.test_radio().await["ok"], false);

        // Seed story.recent_episodes to verify radio never overwrites story state.
        {
            let mut story_payload = json!({
                "results": [{ "id": "story-ep-1", "kind": "episode", "title": "睡前故事" }]
            });
            rt.remember_story_items(&mut story_payload);
        }

        let seen_format = Arc::new(Mutex::new(String::new()));
        let seen_format_ref = seen_format.clone();
        let http_hits = Arc::new(AtomicUsize::new(0));
        let hits_933 = http_hits.clone();
        let hits_972 = http_hits.clone();
        let stream_972_hits = Arc::new(AtomicUsize::new(0));
        let stream_972_ref = stream_972_hits.clone();
        let app = Router::new()
            .route("/health", get(|| async { Json(json!({ "status": "ok", "stations_total": 33 })) }))
            .route(
                "/v1/search",
                get(|| async {
                    Json(json!({
                        "query": "933",
                        "results": [
                            { "id": "sg-yes933", "kind": "station", "title": "Yes933", "artist": "FM 93.3 · SG" },
                            { "id": "sg-love972", "kind": "station", "title": "Love972", "artist": "FM 97.2 · SG" }
                        ]
                    }))
                }),
            )
            .route(
                "/v1/stations",
                get(|| async {
                    Json(json!({
                        "count": 2,
                        "stations": [
                            { "id": "sg-yes933", "title": "Yes933", "enabled": true, "favorite": true },
                            { "id": "sg-love972", "title": "Love972", "enabled": true, "favorite": false }
                        ]
                    }))
                }),
            )
            .route(
                "/v1/stream/sg-yes933",
                get(move |axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| {
                    let seen = seen_format_ref.clone();
                    let hits = hits_933.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        *seen.lock().unwrap() = q.get("format").cloned().unwrap_or_default();
                        ([("content-type", "audio/wav")], tiny_wav_bytes())
                    }
                }),
            )
            .route(
                "/v1/stream/sg-love972",
                get(move || {
                    let hits = hits_972.clone();
                    let s972 = stream_972_ref.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        s972.fetch_add(1, Ordering::SeqCst);
                        ([("content-type", "audio/wav")], tiny_wav_bytes())
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");

        ConfigService::new(store)
            .save_personal_endpoint(
                RADIO_KEY,
                RADIO_TOKEN_SECRET,
                PersonalEndpointConfig {
                    enabled: true,
                    url: base,
                    timeout_ms: 3000,
                },
                Some("radio-tok"),
                "test",
            )
            .unwrap();

        assert_eq!(search_tool.availability(), Availability::Ready);
        assert_eq!(play_tool.availability(), Availability::Ready);

        // 1. test_radio probes /health and /v1/search?q=933&limit=1
        let probe = rt.test_radio().await;
        assert_eq!(probe["ok"], true, "{probe}");
        assert_eq!(probe["sample_count"], 2);

        // 2. radio.search returns candidate stations without touching story.recent_episodes
        let ctx = ToolCtx::new("T-radio", "C1", Arc::new(AtomicBool::new(false)));
        let sr = search_tool.execute(json!({ "query": "933" }), &ctx).await;
        assert_eq!(sr.status, ToolStatus::Completed);
        assert_eq!(sr.result.unwrap()["results"][0]["id"], "sg-yes933");

        // 3. Invalid station_id is rejected without hitting HTTP
        let hits_before = http_hits.load(Ordering::SeqCst);
        let bad = play_tool
            .execute(json!({ "station_id": "../admin" }), &ctx)
            .await;
        assert_eq!(bad.status, ToolStatus::Failed);
        assert_eq!(bad.error.unwrap().code, "RADIO_NOT_FOUND");
        assert_eq!(http_hits.load(Ordering::SeqCst), hits_before);

        // 4. Connect a mock gateway device that confirms PlayBegin & Stop frames
        let gw_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw_addr = gw_listener.local_addr().unwrap();
        let srv_hub = hub.clone();
        tokio::spawn(async move {
            if let Ok((stream, _)) = gw_listener.accept().await {
                let _ = srv_hub.serve_connection(stream).await;
            }
        });
        let gw = tokio::net::TcpStream::connect(gw_addr).await.unwrap();
        let (rd, mut wr) = gw.into_split();
        let hello = crate::gateway::GatewayFrame::Hello {
            protocol: 2,
            min_protocol: 0,
            supported_versions: vec![0, 2],
            gateway: "mock-gw".into(),
            gateway_version: "0.2.0".into(),
            devices: vec![crate::gateway::HelloDevice {
                stable_id: "dot2-radio".into(),
                model: "AEOBC".into(),
                firmware: None,
                capabilities: vec!["audio_out".into(), "play_stream".into()],
                audio_profile: String::new(),
                wifi_ssid: None,
            }],
            aec_tail_ms: None,
            profile_version: None,
            auth_token: None,
        };
        let mut line = serde_json::to_string(&hello).unwrap();
        line.push('\n');
        wr.write_all(line.as_bytes()).await.unwrap();
        let _gw_responder = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let Ok(frame) = serde_json::from_str::<crate::gateway::CoreFrame>(&l) else {
                    continue;
                };
                match frame {
                    crate::gateway::CoreFrame::PlayBegin { task_id, .. } => {
                        let ack = crate::gateway::GatewayFrame::PlayReceipt {
                            task_id,
                            stage: "device_confirmed".into(),
                            detail: Some("ok".into()),
                            ts: 1,
                        };
                        let mut out = serde_json::to_string(&ack).unwrap();
                        out.push('\n');
                        let _ = wr.write_all(out.as_bytes()).await;
                    }
                    crate::gateway::CoreFrame::Stop { task_id, .. } => {
                        let ack = crate::gateway::GatewayFrame::StopReceipt {
                            task_id,
                            stage: "device_confirmed".into(),
                            played_ms: Some(1200),
                            consumed_ms: Some(1200),
                            progress_basis: Some("written".into()),
                            detail: None,
                        };
                        let mut out = serde_json::to_string(&ack).unwrap();
                        out.push('\n');
                        let _ = wr.write_all(out.as_bytes()).await;
                    }
                    _ => {}
                }
            }
        });
        for _ in 0..50 {
            if hub.primary_audio_out().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 5. radio.play starts sg-yes933 with ?format=mp3
        let pr = play_tool.execute(json!({ "query": "933" }), &ctx).await;
        assert_eq!(
            pr.status,
            ToolStatus::Completed,
            "radio.play failed: {pr:?}"
        );
        assert_eq!(seen_format.lock().unwrap().as_str(), "mp3");
        let val = pr.result.unwrap();
        assert_eq!(val["station_id"], "sg-yes933");
        assert_eq!(val["title"], "Yes933");

        // 6. story.recent_episodes must remain untouched by radio.search / radio.play
        {
            let recent = crate::lock_or_recover(&rt.recent_episodes, "story.recent_episodes");
            assert_eq!(recent.len(), 1);
            assert_eq!(recent[0].0, "story-ep-1");
        }

        // 7. music.control `next` switches sg-yes933 -> sg-love972
        let next_res = control_tool
            .execute(json!({ "action": "next", "target": "media" }), &ctx)
            .await;
        assert_eq!(
            next_res.status,
            ToolStatus::Completed,
            "next failed: {next_res:?}"
        );
        assert!(
            next_res.result.as_ref().unwrap()["subject"]
                .as_str()
                .unwrap_or("")
                .contains("Love972"),
            "expected switch to Love972: {:?}",
            next_res.result
        );
        assert_eq!(stream_972_hits.load(Ordering::SeqCst), 1);

        // 8. music.control `pause` -> `resume` re-opens sg-love972 live (start_ms == 0)
        let pause_res = control_tool
            .execute(json!({ "action": "pause", "target": "media" }), &ctx)
            .await;
        assert_eq!(
            pause_res.status,
            ToolStatus::Completed,
            "pause failed: {pause_res:?}"
        );

        let resume_res = control_tool
            .execute(json!({ "action": "resume", "target": "media" }), &ctx)
            .await;
        assert_eq!(
            resume_res.status,
            ToolStatus::Completed,
            "resume failed: {resume_res:?}"
        );
        assert_eq!(
            stream_972_hits.load(Ordering::SeqCst),
            2,
            "resume must re-open live radio stream"
        );

        // 9. Playing one chosen station by id (no search queue) must still allow
        // "换个台": the other stations are queued behind it.
        let by_id = play_tool
            .execute(
                json!({ "station_id": "sg-yes933", "title": "Yes933" }),
                &ctx,
            )
            .await;
        assert_eq!(
            by_id.status,
            ToolStatus::Completed,
            "play by id failed: {by_id:?}"
        );
        let before = stream_972_hits.load(Ordering::SeqCst);
        let switch = control_tool
            .execute(json!({ "action": "next", "target": "media" }), &ctx)
            .await;
        assert_eq!(
            switch.status,
            ToolStatus::Completed,
            "next after play-by-id failed: {switch:?}"
        );
        assert!(
            switch.result.as_ref().unwrap()["subject"]
                .as_str()
                .unwrap_or("")
                .contains("Love972"),
            "expected switch to Love972: {:?}",
            switch.result
        );
        assert_eq!(stream_972_hits.load(Ordering::SeqCst), before + 1);
    }
}
