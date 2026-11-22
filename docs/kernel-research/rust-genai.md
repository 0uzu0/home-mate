# rust-genai deep-research (as of 2026-11-07)

> M3.5 分智能体调研报告·其二，归档于 2026-11-07。来源为公开网络资料（GitHub/crates.io/源码 tag v0.7.3），未做本地构建。调研智能体与实现者分离。

Candidate: `genai` crate, repo https://github.com/jeremychone/rust-genai — "A Native-Protocol Multi-AI Provider Library for Rust" (README, v0.7.3). Disambiguation checked: the crates.io `genai` crate's repository field points to jeremychone/rust-genai; it is the multi-provider client, not any of the Gemini-only "genai-*" helper crates. Source-level claims verified against the upstream repo at tag **v0.7.3 (commit b449516, 2026-11-03)**.

## 1. Version pinning

- Latest stable release: **0.7.3, published 2026-11-03**, not yanked. Stable commit to pin: tag `v0.7.3` = `b44951653cda95fc80a53e9d68c7eb3a6d43e502`. `main` HEAD (2026-11-04) already carries `version = "0.7.4"` with `publish = false` — a WIP main must not be pinned.
- Pre-1.0 (0.x). Breaking-minor cadence roughly every 4–5 months: 0.2.0 (2025-04-16), 0.3.0 (2025-05-08), 0.4.0 (2025-09-28), 0.5.0 (2026-01-09), 0.6.0 (2026-05-23), 0.7.0 (2026-10-20). Patch flow within a minor is fast (0.7.0→0.7.3 in 14 days).
- Breaking pain between minors is real and API-shape-level: 0.7.0 made `Client::new()` fallible, renamed `ReasoningEffort::None`→`Zero`, omitted `None` fields from JSON, added struct fields forcing literal updates; 0.6.0 restructured `all_model_names()`; 0.5.0 changed `ChatResponse.content` to `MessageContent`; 0.4.0 changed `StreamEnd`/`ChatResponse.tool_calls`. 0.7.0 alone went through ~24 betas + 9 rcs over 4.5 months.
- 155 published versions; 581,617 total / 282,437 recent downloads. Could NOT verify: exact yank history for ~100 pre-0.6 versions (API response truncated) — none of the visible ones were yanked.

## 2. License + dependencies

- License: **"MIT OR Apache-2.0"** on every listed version — dual-permissive, commercial-friendly pending Home Mate's own review pass (notices/attribution only).
- It **does pull reqwest — hard, non-optional**: `reqwest 0.13` with `default-features = false, features = ["json","stream","gzip","charset","http2"]`. TLS: default feature `rustls-tls` → rustls + **aws-lc-rs** + OS trust store via `rustls-platform-verifier`; alternatives: `native-tls`, or `default-features = false` + bring-your-own reqwest (incl. `ring` provider for no `aws-lc-sys` builds), or inject a self-built `reqwest::Client` via `with_reqwest(...)`. Note for Debian 12 NAS: default build compiles `aws-lc-sys` (C/asm, needs C toolchain/CMake); the `ring` path avoids it.
- Other direct deps (all mandatory unless noted): tokio 1, futures 0.3, tokio-stream 0.1, serde/serde_json (`preserve_order`), serde_with 3, bytes, base64, mime_guess, pastey, macro_rules_attribute, tracing, derive_more, value-ext (maintainer's own crate), strum, uuid, regex; optional aws crates only behind `bedrock-sigv4`. Nothing copyleft. **Unresolved:** whether Home Mate's reqwest major (0.12) can coexist with genai's reqwest 0.13 without a duplicated hyper/TLS stack — `with_reqwest` requires the caller to build a reqwest 0.13 client, so adopting genai cleanly implies moving Home Mate to reqwest 0.13 or accepting two stacks.

## 3. Toolchain/MSRV, features, Linux build

- **No `rust-version`/MSRV declared.** `edition = "2024"` implies Rust ≥ 1.85. CI installs stable and runs only on ubuntu-latest — effective policy "recent stable", x86_64 Linux CI-verified. No MSRV CI job; no author-stated MSRV found.
- Feature flags for minimal use: `default = ["rustls-tls", "system-proxy"]` suffices for one OpenAI-compatible endpoint; turn off `system-proxy` if proxy autodetection is unwanted.

## 4. Code-level API truth (v0.7.3 sources)

- **Abstraction shape:** `Client` → `exec_chat(model, ChatRequest, Option<&ChatOptions>) -> ChatResponse` and `exec_chat_stream(...) -> ChatStreamResponse`; model arg `impl Into<ModelSpec>`; dispatch through an `Adapter` trait with ~40 per-provider adapters.
- **bigmodel v4 specifically:** first-class `BigModel` adapter — pass-through to `OpenAIAdapter` with endpoint `https://open.bigmodel.cn/api/paas/v4/`, env key `BIGMODEL_API_KEY`, forced via `bigmodel::glm-4-flash` namespace. Caveat: bare `glm-*` names route to the **Zai** adapter (api.z.ai international) — Home Mate must use the `bigmodel::` namespace. Generic OpenAI-compatible endpoints: `genai_{n}::` env-configured adapters, or a `ServiceTargetResolver` for arbitrary endpoint/auth/model.
- **Streaming tool-call assembly** (`src/adapter/adapters/openai/streamer.rs`, 791 lines) is materially more battle-hardened than a hand-rolled version: a `ToolCallSlots` router for servers that **omit `index`** (routes by `id`, last-slot continuation, new-slot on function name; documented limitation: repeated fn-name with neither index nor id starts a new call); argument accumulation per slot in a `BTreeMap<u32, ToolCall>`; per-delta `ToolCallChunk` events plus complete parsed `ToolCall`s in `StreamEnd.captured_tool_calls`; `reasoning_details` merge-back (OpenRouter); recorded regression fixtures for **"ZhiPu/BigModel GLM-4 series … return the full tool call and finish_reason: 'tool_calls' in the first content chunk"** and for `stream_tool_calls_without_index`. That is precisely Home Mate's wire quirk set.
- **Error taxonomy:** one `Error` enum grouped by stage. **No transient/fatal classification method exists**; instead `status()` and `headers()` deliberately expose `retry-after`/`retry-after-ms`/`x-should-retry`, with the doc comment: *"Retry policy stays with the caller — this only reports what the provider said."* This preserves Home Mate's boundary: retries, evidence levels, and task validity stay home.
- **Timeouts:** `ChatOptions::stream_header_timeout` (max wait for streaming HTTP response headers) as `tokio::time::timeout` around `request.send()`; `stream_read_timeout` (max wait per raw body chunk; heartbeats and partial frames reset; not an overall deadline) enforced per-chunk. The header timeout fires on **headers**, not first SSE event — the read timeout covers a headers-then-stall; together they reproduce Home Mate's first-event-timeout protection. Tested semantics include "header_timeout_releases_request", "idle_body_timeout_releases_response", "stalled_error_body_preserves_http_status_and_retry_headers", "heartbeats_reset_read_timeout_without_model_events".
- **What it deliberately does NOT provide:** no agent loop, no retry logic, no sessions/storage (only a Responses-API `response_id` passthrough), no cancellation tokens, no task registry. Boundary confirmed.

## 5. Cancellation

- **Drop-based only.** `ChatStream` is a `Pin<Box<dyn Stream>>` over the reqwest byte stream; dropping it drops the response and closes the connection. **No `CancellationToken`, no `abort()`.** For Home Mate this is fine — its tokio `select!`/drop on task cancellation composes naturally, no framework-side co-ownership of task validity.

## 6. Gains vs Home Mate's 419-line client

**Gains:** maintained `bigmodel::` v4 adapter plus ~40 others (DeepSeek, Zai, and notably **Ollama** — relevant to the possible on-speaker study); a streamer with hard-won handling of OpenAI-compatible wire quirks (missing `index`, GLM-4 single-chunk tool calls, interleaved `reasoning_content`, usage tail capture), each locked by HTTP record/replay ("yakbak") fixtures — 58 test files, 32 examples; tested stream-timeout semantics; normalized usage/stop reasons; `raw_frame_sink` escape hatch. **Pays:** pre-1.0 churn (breaking minor every ~4–5 months); ~20-crate direct dependency surface incl. reqwest 0.13 and maintainer-owned utility crates; undeclared MSRV; behavior shifts under us; no free transient/fatal classification — Home Mate's retry policy code stays regardless.

## 7. Maintenance risk

- **Bus factor ~1 on design:** Jeremy Chone has 980 of 1,207 commits (~81%), but 80 total contributors with an active cohort (missing-index streaming fix landed 2026-11-02). Commit cadence strong: 22–92 commits/month throughout 2025-10→2026-11.
- Issues: 37 open / ~101 total; recent issues get engagement but some sit unanswered. Docs: docs.rs 86.8% documented, 32 runnable examples, detailed CHANGELOG. Repo: ~936 stars / 197 forks.

## 8. Verdict

**Recommendation: keep the own 419-line client for the current NAS one-room slice; revisit genai at the first multi-provider trigger (on-speaker study with Ollama, or any second provider/model family) — the decision is cheap precisely because the client sits behind the LlmClient trait.** Today Home Mate needs exactly one OpenAI-compatible endpoint and its own client already encodes its quirk set, retry policy, and first-event timeout with zero churn exposure; genai's strongest arguments become decisive only when a second wire format appears, at which point genai is the clear adopt-over-rewrite candidate.

**Swap cost estimate:** small and bounded — a genai-backed `LlmClient` impl of roughly 300-400 lines (request/tool-schema mapping, event mapping, error taxonomy via `status()`/`headers()`, `bigmodel::glm-4-flash` + `stream_header_timeout`/`stream_read_timeout` config) plus a rerun of the existing behavior-verification slice — on the order of 1-2 focused days, since the trait contract, task registry, sessions, and cancellation design all stay untouched.

**Bottom line:** rust-genai (genai 0.7.3, MIT/Apache-2.0) is an unusually well-matched "mature model client" for Home Mate's exact stack — first-class bigmodel v4 adapter, replay-tested handling of the very GLM-4 SSE quirks Home Mate hand-coded, and stream timeouts whose tested semantics mirror Home Mate's first-event-timeout policy, while explicitly refusing to own retries, loops, sessions, or task state; its costs are pre-1.0 breaking minors, a ~20-crate dependency surface pinned to reqwest 0.13 with an undeclared MSRV, and single-maintainer dependence — adopt only when multi-provider need materializes (the on-speaker Ollama study being the natural trigger), behind the existing LlmClient trait.
