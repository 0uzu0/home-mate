# Rig deep-research (as of 2026-11-07)

> M3.5 分智能体调研报告·其一，归档于 2026-11-07。来源为公开网络资料（GitHub/crates.io/docs.rs/源码），未做本地构建。调研智能体与实现者分离。

Scope: web-only research on [0xPlaygrounds/rig](https://github.com/0xPlaygrounds/rig) (GitHub, crates.io API, docs.rs, raw source on `main`, release notes, docs site rig.rs), evaluated against Home Mate B's constraints (own loop + task registry with cancel/evidence levels, OpenAI-compatible GLM client, tokio/axum/rusqlite monolith, x86_64 Debian 12 NAS, commercial licensing review, no framework task-state co-ownership). No repository was modified; no build was attempted.

Important orientation fact: since v0.41 (2026-07-28) Rig is a multi-crate workspace under `crates/` — `rig-core` (wires/protocol), `rig-agent` ("classic agent runtime"), `rig-memory` (sessions), plus coding-agent crates (`rig-tools`, `rig-harness`/`rig-ecs` Bevy ECS kernel, `rig-tui`, MCP/ACP clients). The umbrella crate `rig` 0.46.0 re-exports the library surface; `cargo install rig` installs the terminal coding agent. Home Mate would only ever touch `rig-core` (and possibly `rig-agent`/`rig-memory`), never the harness/tools/TUI crates. [Source: repo README; crates.io API; releases page; source tree]

## 1. Version pinning

- Latest release: **rig-core / rig 0.46.0, published 2026-10-31** (crates.io API), tag `rig-v0.46.0`, release commit `6e039cd`.
- Stable-commit recommendation for 2026-11: **pin `=0.46.0` exactly (commit `6e039cd`)**. `main` is extremely active (last push 2026-11-07, multiple commits that day), so floating `0.46` would drift.
- Cadence 2026: 15 releases between 0.35.0 (2026-04-13) and 0.46.0 (2026-10-31) — roughly two per month, sometimes weekly.
- Breaking-change history (from release notes and rig-agent CHANGELOG):
  - **0.40.0 (2026-07-11)**: `max_turns` semantics changed to an *exact* bound on model calls; hook system v2; PromptResponse/FinalResponse unified.
  - **0.41.0 (2026-07-28)**: rig-core/rig-agent split behind the `rig` facade (#2197); core errors non-exhaustive.
  - **0.44.0 (2026-10-07)**: **rig-reqwest split (#2397) — `rig-core` now has no default HTTP transport and no reqwest/tokio dependency**; "one meaning for Usage"; tool-argument streaming rework (#2721); typed tool errors (#2687); one agent-run error type (#2644).
  - **0.45.0 (2026-10-19)**: heavy breaking release — API-review sweeps, durable/restartable runs (#2762), streamed tool items as projection of committed history (#2763), removal of the rig-ecs crate (#2761).
  - **0.46.0 (2026-10-31)**: only one library breaking change ("stop forcing serde_json features" #2934); adds rig-mcp-client, rig-traffic, SqliteSessionStore/PostgresSessionStore, `ProviderError::Stalled` idle timeout.
- The project itself warns it is pre-1.0 with breaking changes between releases, and ships a `rig-migrate` tool for migration. "Every release contains breaking changes" is the project's own warning; 0.46 was in fact mild, so treat as directional.

**Pinning conclusion:** any adoption must pin an exact version (`=0.46.0`) and budget for a migration at every upgrade (roughly monthly).

## 2. Licenses

- **All relevant crates are MIT**: `rig-core`, `rig` (facade), `rig-agent`, `rig-memory`, `rig-reqwest`, `rig-sqlite`, `rig-derive` — per crates.io metadata and each crate's Cargo.toml.
- Spot-checked transitive licenses: `schemars` MIT, `fastrand` Apache-2.0 OR MIT, `web-time` MIT OR Apache-2.0, `ordered-float` MIT. The rest of the default graph (serde, serde_json, thiserror 2, tokio, reqwest 0.13, uuid, indexmap, async-stream, futures, tracing, url, sha2 0.11, mime_guess, glob) are conventional permissive crates **but were not individually re-verified in this session** — final call belongs to the project's licensing review.
- One flag for that review: the default `rustls` path of `rig-reqwest` pulls **`webpki-root-certs`, licensed CDLA-Permissive-2.0** (earlier versions MPL-2.0) — a permissive *data* license for the Mozilla CA bundle, unusual in a license scan. Home Mate's own reqwest+rustls stack very likely already ships an equivalent CA bundle — not verified here.
- Avoidable heavy/optional deps behind non-default features: `lopdf` (pdf), `epub` 2 + `quick-xml` (epub) — all off with `default-features = false`. `rig-sqlite` additionally pulls `rusqlite` (bundled), `tokio-rusqlite`, and `sqlite-vec` (**sqlite-vec's license was not checked**).
- No CLA/patent-grant analysis was done (CONTRIBUTING not examined) — unverified.

## 3. Toolchain / MSRV, minimal feature set, build feasibility

- **MSRV: Rust 1.97.1, edition 2024** (workspace `rust-version = "1.97.1"` in root Cargo.toml; crates.io metadata for rig-core 0.46.0 confirms).
- Debian 12 ships **rustc 1.63** — the distro toolchain cannot build Rig; Home Mate must build with a rustup toolchain ≥1.97.1 on the NAS (or cross-compile). Packaging constraint, not a blocker.
- Minimal "model-client only" configuration (no agent loop, no coding tools): `rig-core = { version = "=0.46.0", default-features = false, features = ["derive", "reqwest"] }` (add `rustls` or `native-tls` for TLS). Since 0.44, **rig-core itself has no reqwest/tokio dependency unless you enable the `reqwest` feature**; the transport is `rig-reqwest` (reqwest 0.13, json/stream/multipart).
- The model client is usable standalone: `Model<W, T>` (a struct, not a trait) with `call()` / `stream()` / `with_idle_timeout()`; nothing is sent until the returned stream is first polled; a custom `Transport` trait impl can replace HTTP entirely ("a local runtime, a proxy or a mock is a transport").
- If the loop is also adopted: `rig-agent` depends on `rig-core` (default-features=false) + `rig-memory` + futures/async-stream, with **tokio optional (`rt`,`sync` only)** — caller supplies the runtime.
- Build on x86_64 Linux: no Linux-specific obstacles apparent. **Not verified by an actual `cargo build`.**

## 4. Code-level API truth (read from source on `main`, ≈0.46.0)

**Streaming tool-call argument assembly** (`crates/rig-core/src/streaming/event.rs`): the stream yields `StreamEvent::{Start, Text, Reasoning, Arguments, End}` per *Part* (a position in the response). A tool call opens with `Start{kind: PartKind::ToolCall, name}` — the tool name is fixed up front — argument JSON arrives as `Arguments` fragments whose concatenation "is the argument JSON the provider sent"; `parse_partial_arguments` decodes incomplete prefixes; `End` carries the finalized `AssistantContent`. Raw argument streaming was added in 0.44 (#2721). Essentially the same delta-assembly problem Home Mate solved by hand, but with a formally validated sequence model.

**Event ordering** (same file): parts are numbered in start order; each part must Start → grow → End exactly once; violations are typed errors (`UnknownPart`, `WrongKind`, `EndedTwice`, `Unclosed`); `parse_prefix` tolerates open parts for early-stopped streams; unmodeled payloads pass through as `Unknown`.

**Serial vs parallel tool execution** (`crates/rig-agent/src/agent/engine.rs`, `runner.rs`): configurable via `tool_concurrency(n)` (default 1, zero clamped to 1). The driver runs a tool batch with `buffer_unordered(concurrency.max(1))` — ≤1 means strictly sequential fail-fast; results are collected per index and committed to history **in call order regardless of completion order**. Batches are "atomic per batch".

**Loop termination / budgets** (`run/mod.rs`, `run/spec.rs`, `runner.rs`): budgets live on the serializable `RunSpec`: `max_turns` (default `None` → effective 1; exceeding yields `PromptError::MaxTurns`; since 0.40 it bounds the exact number of model calls), `max_invalid_tool_call_retries` (default 0), `max_consecutive_malformed_tool_calls`, `max_output_retries`. Exit conditions: no pending tool calls → Done; any hook Stop → `run.cancel_error(reason)`. **There is no detection/dedup of repeated identical tool calls** (confirmed absent in the run state machine).

**Error/timeout semantics** (`crates/rig-core/src/error.rs`, `error/retry.rs`, `driver.rs`): `ProviderError` non-exhaustive — `Http`, `Json`, `Url`, `Request`, `Response`, `Provider`, `ProviderResponse` (raw reply preserved with status/headers/request-id), `InvalidAuthentication` (401/403, **not retryable**), `CacheExpired`, `MismatchedDimensions`, `Relayed`, `Truncated` (**retryable**), `Stalled(Duration)` (**retryable**; 5-minute default with `Model::with_idle_timeout`), `UnsupportedOption`. `is_retryable()` = transient Http, status-based ProviderResponse, Relayed flag, Truncated, Stalled. `retry_after()` parses `retry-after`/`retry-after-ms`/HTTP-date plus provider body hints. **No automatic retry loop at the driver level** — retries are the caller's (or a hook's: `ModelTurnAction::Retry` re-issues a model call without consuming a turn).

**Cancellation propagation** — the most important finding:
- **There is no external cancel token or abort API.** Cancellation is *hook-driven*: any `AgentHook` returning a Stop action causes the driver to call `run.cancel_error(reason)` and settle with an error.
- In-flight tool calls are **not aborted**: a shared `terminating` flag makes *not-yet-started* siblings skip, "while in-flight siblings are drained" to completion; already-collected answers are still committed; the lowest call-index terminator wins. In streaming, a delta-hook stop drops the provider stream mid-turn.
- The cancel error carries `canonical_history()` — full history closed for cancellation, with remaining/unanswered calls closed — a resumable canonical transcript.
- Dropping the agent's output stream mid-run: the blocking path deliberately drains "rather than being dropped at the error yield"; what a plain drop does to teardown is **inference only, not documented** (marked unverified). The docs site has **no cancellation page at all**.

**Context injection / history / session resume** (`agent/builder.rs`, `run/mod.rs`): `preamble()` / static `context(Document)` / `dynamic_context(samples, index)` (retrieval hook patching requests via `RequestPatch`). Prior history: `with_history(iter)` (unchecked) and `with_validated_history(vec)` which **rejects non-canonical transcripts**. `AgentRun` is `Serialize + Deserialize` with a **versioned envelope (`RUN_FORMAT = 5`, unknown formats refused)**; resume re-emits unanswered tool calls only. Memory (`rig_memory::AsyncStore`) is loaded before the first model call and saved at Done; **failed or cancelled runs save nothing**; concurrent saves to the same conversation fail with `SessionError::Changed`.

## 5. Seam analysis for Home Mate

- **State ownership:** `AgentRun` (inside rig-agent) owns the authoritative per-run state — history, budgets, usage, tool advertisements — deliberately sans-I/O and serializable. This directly collides with Home Mate's constraint that framework task state must not co-own "can this task still act": in Rig, the run's own state machine decides the next step, and external code can only steer it through hooks.
- **The interception seam is the hook system**: `on_run_start`, `on_completion_call` (request patching), `on_model_select`, `on_model_turn_finished` (Continue/Retry/Stop), `on_invalid_tool_call` (Fail/Retry/Repair/Skip/Stop), `on_text_delta` / `on_reasoning_delta` / `on_tool_call_delta` (Stop cancels before execution), `on_dispatch` (Proceed/Patch/Deny/Skip), `on_outcome` (Keep/Replace output/Stop, including `rewrite_tool_result`), plus a typed run-scoped `scratchpad`. An external task registry would be bridged as an `AgentHook` returning Stop when Home Mate's registry says the task is cancelled; evidence-level gates would sit in `on_dispatch`/`on_outcome` — these fire at the dispatch boundary *before* results commit to history, a good fit, with the explicit caveat that "an `Ok` completion outcome doesn't prove the turn was accepted."
- **Where the seams don't fit Home Mate:**
  1. *Cancel semantics*: hook Stop = unstarted tools skipped, **in-flight tools drained to completion**. Home Mate's voice stop-epoch design wants prompt termination; "drain, then cancel" is a behavioral mismatch an adapter cannot remove.
  2. *Late-result recording*: Rig's resume **re-executes unanswered tool calls**; Home Mate records late results and discards. Different durability models.
  3. *Sessions*: `with_validated_history` only accepts Rig's canonical `Message` transcript; Home Mate's SQLite session schema would need an `AsyncStore` adapter or dual-write. `rig-sqlite` exists but brings its own schema and `sqlite-vec`.
- **Adapter size estimate**: small if adopting only the model-client layer (`Model::stream()` yourself); medium-to-large if adopting `rig-agent` whole.
- **Partial adoption cleanliness:** the model client is genuinely clean post-0.44. Adopting *just* tool-call delta parsing is less clean: `StreamEvent` argument assembly is coupled to rig-core's `Message`/`AssistantContent` types.

## 6. GLM / bigmodel support

- Rig ships a first-class **`zai` provider: Z.ai / Zhipu AI** — base URLs `https://api.z.ai/api/paas/v4` (OpenAI-compatible), coding endpoint, Anthropic-compatible endpoint; auth `ZAI_API_KEY`; model constants glm-4.6/4.5 family.
- **Home Mate's endpoint is `open.bigmodel.cn/api/paas/v4` (bigmodel), not `api.z.ai`.** Whether a bigmodel key/endpoint works through Rig's `zai` provider is **not verifiable from public materials** (separate platforms; `glm-4-flash` is not among the constants — pass the model id as a string).
- Arbitrary OpenAI-compatible base URLs are supported: `OpenAIConfig::with_key(&Dialect::gateway(name, base_url, key_env), key).with_base_url(url).with_route(Route::Chat)`; `OpenAIConfig::from_env()` honors `OPENAI_BASE_URL`.
- **Tool-call streaming quirks for GLM through Rig's OpenAI wire: no public issue, test, or doc found.** Rig has per-vendor `Quirks`/`StrictSchema` mechanisms (e.g. DeepSeek special-casing), but GLM-specific streaming behavior through Rig is **unverified — needs a spike against bigmodel v4**.

## 7. Maintenance risk

- **Stewardship**: Playgrounds org; 9.1k stars, 1.0k forks, 1,718 commits; `main` pushed 2026-11-07.
- **Funding is the headline risk**: development is closely associated with the **AI Rig Complex (ARC) token on Solana** — a crypto-token-funded model rather than disclosed VC; community support via GitHub Sponsors/Open Source Collective. Sustainability + reputational factor for a commercial product (secondary sources: Blockworks token transparency filing, Solana Compass — directionally reliable, not audited).
- **Bus factor**: 100+ contributors; top three `gold-silver-copper` (603 commits), `joshua-mo-143` (249), `cvauclair` (160). All recent crates.io releases published by a single account (`cvauclair`) — single-publisher risk on the registry side.
- **Responsiveness**: 35 open issues / 19 open PRs against the activity level is very low; sampled issues closed within 1–4 days.
- **Docs**: complete docs.rs per crate; docs site rig.rs — but **no cancellation documentation and no GLM/Z.ai/bigmodel mention**.
- **Churn**: pre-1.0 with breaking changes warned per release; harness/ECS area restructured repeatedly in 2026.

## 8. Verdict for Home Mate

**Recommendation: adapt-methods-only by default; optionally adopt the model-client layer (`rig-core`, `derive`+`reqwest` features) behind Home Mate's own `LlmClient` interface after a one-day spike; do not adopt the `rig-agent` loop for the task-facing runtime at this stage. Do not adopt the harness/tools/TUI crates at all.**

- The loop's semantics are genuinely good (validated streaming part model, exact turn budgets, canonical cancellation histories, versioned resumable run envelope, rigorous retryability taxonomy, stall detection) — these are the *methods* worth copying into Home Mate's ~600-line loop; several map to known-hard problems (stop-epoch race protection ↔ terminating-flag + canonical-closed-history; late-result recording ↔ resumable in-flight steps; loop termination ↔ exact `max_turns`).
- But adopting the loop strains three hard constraints: (a) framework state co-ownership — `AgentRun` owns "what happens next", hooks are the only steering surface; (b) cancellation — no external cancel token, in-flight tools drained not aborted — wrong default for a voice assistant's hard stop; (c) late-result policy — Rig re-executes, Home Mate records-and-discards. Each bridgeable with hooks, but that's a medium adapter hostage to ~monthly breaking releases — a permanent maintenance tax for capabilities already working in ~600 dependency-free lines.
- The model-client layer alone is defensible (post-0.44 transport-free, MIT, streaming-first, custom-base-URL APIs fit bigmodel v4) **provided** the spike confirms glm-4-flash tool-call streaming behaves through Rig's OpenAI wire (unverified) and dropping a stream aborts promptly (unverified).
- Licensing broadly clean (MIT) with two review items: `webpki-root-certs` (CDLA-Permissive-2.0), `sqlite-vec` unchecked. ARC-token funding documented in the commercial review regardless of technical merit.

### Unverified / could not confirm from public materials

- Actual `cargo build` of rig-core 0.46.0 on x86_64 Debian 12.
- bigmodel.cn compatibility with Rig's `zai` provider or generic OpenAI wire, incl. GLM tool-call streaming quirks — needs a live spike.
- Provider-HTTP-request abort semantics when the caller drops `Model::stream`'s stream mid-run.
- Individual licenses of common transitive crates (assumed permissive; not re-verified), CLA/patent terms.
- Playgrounds/ARC funding structure (secondary sources only).
- Whether "every release contains breaking changes" is literally true for every future release.
