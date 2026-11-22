# ADK-Rust deep-research (as of 2026-11-07)

> M3.5 分智能体调研报告·其三，归档于 2026-11-07。来源为公开网络资料（GitHub/crates.io/源码），未做本地构建。调研智能体与实现者分离。

## 0. Repository identity (verified)

The URL is still correct: **https://github.com/zavora-ai/adk-rust** is the live, active repo (743 stars, 116 forks, 1,811 commits, 6 open issues). The org `zavora-ai` (Zavora Technologies, ~200 repos, Rust-centric AI-agent infrastructure) is unchanged. Note an unrelated older crate `adk` (v0.2.1, 2025-04) is a different project — do not confuse.

## 1. Version pinning, cadence, breaking changes, who is behind it

- **Latest published release: v2.3.0, 2026-10-09** (GitHub releases, tags, crates.io API). The umbrella `adk-rust` crate shows 39,072 downloads; README claims "500K+ total downloads" across workspace crates.
- **Stable commit to pin today: tag `v2.3.0`**. `main` is at workspace version **3.0.0, unpublished (release candidate)**; CHANGELOG carries a dated 3.0.0 section (2026-10-10) with breaking changes (ACP → agent-client-protocol 2.2, fallible AP2 amount conversion, Chrome sandbox on by default, terminal streaming events carry the whole response). No `v3.0.0` git tag exists. *Unverified: whether the four 3.0.0 breaking items also appear in v2.3.0 release notes; treat v2.3.0→main as the breaking boundary and pin the tag.*
- **Post-1.0 but young.** v1.0.0 "first stable, semver guarantee" landed 2026-06-07; v2.0.0 (2026-08-10) shipped ~7 breaking changes with migration guides; v2.1.0 (Aug 25), v2.2.0 (Sep 1), v2.3.0 (Oct 9). **Two major versions in 3 months, with a third in RC.**
- **Who:** primary author James Karanja Maina (`jkmaina`, zavora.ai) — workspace Cargo.toml author field and top recent commits confirm single-maintainer dominance. CONTRIBUTORS.md credits ~17 contributors across v1.0.0/v2.0.0. One sponsor.
- **Relation to Python ADK:** independent project, *not* Google's. It deliberately mirrors Google ADK concepts (LlmAgent, Session/State, A2A), defaults to Gemini, benchmarks against the Python ADK. *Unverified: no explicit "not affiliated with Google" statement found.*

## 2. Licenses and dependencies (commercial-use view)

- **All workspace crates: Apache-2.0**, including every crate Home Mate would touch (`adk-core`, `adk-runner`, `adk-session`, `adk-tool`, `adk-model`, umbrella `adk-rust`).
- The project **enforces its own permissive-only dependency policy** via cargo-deny: allow-list MIT/Apache/BSD/ISC/BSL/CC0/Unicode/MPL-2.0; GPL/LGPL/AGPL and unlicensed rejected; exceptions feature-scoped and documented — BUSL-1.1 (surrealdb) only behind optional `adk-rag/surrealdb`; CDDL (inferno) only via optional `adk-rag/lancedb`. **None of those exceptions are reachable from the runner/session/tool path.**
- Minimal-set transitive deps are light and permissive: `adk-runner` = adk-core + adk-session + tokio/tokio-util/futures/async-stream/thiserror/uuid/serde/serde_json/tracing, with artifacts/plugins/skills all optional. `adk-session` default features = none (in-memory only); SQLite/Postgres via sqlx, Redis via fred, all optional. **tokio is the only async runtime** (no actix).
- Heavy stuff strictly opt-in: audio/ONNX (whisper, kokoro) behind `adk-audio` features; mistral.rs local inference behind `adk-mistralrs` ("a heavy build: not in any tier"); LiveKit only under `adk-realtime`. If `adk-model` is used it pulls reqwest 0.12 with rustls unified on **aws-lc-rs** — Apache-2.0/ISC but a C/CMake build on Debian. *Unverified: exact aws-lc-rs build requirements on Debian 12 for this tree.*
- Caveats for the review file: Apache-2.0 requires NOTICE/attribution retention in distribution; v2.3.0 shipped a large security-fix batch (A2A caller binding, sandbox escapes, tenant isolation) meaning **≤2.2.0 should never be pinned**.

## 3. Toolchain, MSRV, trimmability, x86_64 Debian 12 feasibility

- **MSRV: Rust 1.95** (`rust-version = "1.95"`, edition 2024, resolver 3; `rust-toolchain.toml` pins 1.95.0; published `adk-runner 2.3.0` reports rust_version 1.95). Debian 12's system rustc (~1.63) is far too old: **rustup toolchain mandatory** on the NAS. Current stable (Nov 2026) is ~1.99 — a modern-but-reasonable pin.
- **Umbrella tiers**: `minimal` (default) = agents+models+gemini+runner+sessions — **tools are NOT in minimal**; `standard` adds tools/MCP/auth/server/graph/eval; `enterprise` adds realtime/browser/rag/payments; `full` adds audio/code/sandbox/harness. All voice/audio/ML opt-in.
- **You can bypass the umbrella entirely.** The crates are independently consumable: `adk-runner` with `default-features = false` has no model dependency at all; the `Agent` trait is fully model-agnostic — `async fn run(&self, ctx: Arc<dyn InvocationContext>) -> Result<EventStream>` with no Llm client in the trait. "Runner/session/tools WITHOUT its model and voice parts" is genuinely achievable: implement your own `Agent` wrapping Home Mate's GLM client.
- If desired, their model layer: `OpenAICompatible` supports custom base URLs and has a **Zhipu GLM preset** (`zhipu`) — directly applicable to GLM via OpenAI-compatible HTTP.
- x86_64 Linux build: nothing platform-specific in the minimal path. Benchmarks report ~15 MB peak RSS for the loop.

## 4. Code-level API truth

- **Runner semantics**: typestate builder requiring `app_name`, `agent`, `session_service` (compile-time enforced); the session **must already exist** — the runner loads it. `run(user_id, session_id, content) -> EventStream`; `run_with_config` / `start_with_config`. Events stream incrementally: partial streaming chunks share one event id; tool-progress events (`partial = true`) push live stdout/stderr; `is_final_response()` = skip_summarization OR long_running_tool_ids OR "no function calls/responses, not partial"; **multiple finals possible** in multi-agent invocations. No explicit ordering contract beyond per-call correlation.
- **Session/state ownership — the big one:** the Runner is the automatic owner of the session lifecycle: "it loads the session, appends every event, and applies state deltas as they arrive". `SessionService` trait (create/get/list/delete/append/rewind) with InMemory default, SQLite/Postgres/Redis/Mongo/Neo4j/Firestore/Vertex backends, `EncryptedSession` (AES-256-GCM) wrapper; state is tiered `app:`/`user:`/`temp:` with events carrying `state_delta`; rewind rebuilds session-scoped state from deltas. **SQLite uses sqlx, not rusqlite** — a second SQLite stack next to Home Mate's rusqlite if persisting ADK sessions. Durable runs (leases, epochs, claim/finish) exist via `DurableRunStore` (adk-graph/adk-kernel).
- **Tool trait**: `#[async_trait] Tool { name, description, parameters_schema, response_schema, async fn execute(&self, ctx, args) -> Result<Value> }` plus a rich optional surface: `timeout_override` (per-call timeout, or explicit none), `effect()`/`is_read_only()`/`is_concurrency_safe()` (drives auto-parallelism and retry policy), `is_long_running()` (task-id pattern), `origin()`/`annotations()` (approval identity), `egress(args)` (network declaration enforced by run egress policy), and a derived **idempotency key** `{app}/{user}/{session}/{invocation}/{function_call_id}`. Parallel tools: `ToolExecutionStrategy::Auto` overlaps only read-only+concurrency-safe calls. Cancellation-awareness inside `execute` is cooperative (token exposed on context).
- **Error taxonomy**: structured `AdkError` with `ErrorCategory` (InvalidInput, Unauthorized, Forbidden, NotFound, RateLimited, Timeout, Unavailable, **Cancelled**, Internal, Unsupported, **ResourceExhausted**, **Conflict**), retryability via `RetryHint`, `is_rejection()` distinguishing "refused" from "upstream failed". Notably clean and HTTP-mappable.
- **Budgets/loop termination**: `RunBudget` caps model calls, total tokens, cost (micro-USD, `allow_unpriced_models` for self-hosted), wall time, tool calls; checked after every event; exhaustion **persists an explanatory event** and fails with `ResourceExhausted`; optional spend ledger does reserve/commit around model calls.
- **Context injection & resume**: async `{var}`/`{var?}`/`{artifact.name}` template substitution from tiered session state; history resume across restarts = persisted session events reloaded by the runner; durable resume with checkpoints is adk-graph. Context overflow handled by optional compaction features.

## 5. Cancellation — first-class, not bolted on

The most mature part relative to Home Mate's needs:

- `tokio_util::sync::CancellationToken` plumbed at config level; each run derives a child token; **`Runner::interrupt(session_id)`** cancels exactly one run.
- The event loop **races the stream against the token**; on cancellation it drops the agent stream (stopping the interrupted tool's future), then `answer_interrupted()` **synthesizes a function response for every in-flight call** with `ToolResultStatus::OutcomeUnknown` — literal message: *"the run was cancelled while 'X' was in flight, so the call may have taken effect; check its result before calling it again"* — persisted to the session and yielded to the caller.
- Client disconnect gets the same treatment via `RunFinish`'s `Drop` (cause "disconnected"); `Runner::recover_unanswered_calls` cleans calls abandoned by a stopped process; durable runs are answered from the action ledger by the recovering worker.
- Errors carry `ErrorCategory::Cancelled`.
- In-tool cancellation is cooperative: tools are futures the runner can drop, plus per-call timeouts; no forced kill inside `execute` — same model as Home Mate's design space.

## 6. Seam analysis vs Home Mate B

- **Alignment:** outcome-unknown late-result discipline, budget taxonomy, structured error categories, tiered state keys map remarkably well onto Home Mate's TaskRegistry cancellation/late-results and evidence thinking. The `Tool` trait (egress declaration, effect classes, idempotency keys) is a superset of what a device-gateway tool needs — the LDJSON gateway would be one `Tool` with `egress()` declaring the NAS→gateway address, and gateway receipts become `FunctionResponse` payloads. GLM is natively reachable (`zhipu` preset / custom base URL) — or skippable via a custom `Agent`.
- **Conflict:** ADK's Runner **owns the session/event/state lifecycle and the in-flight-call ledger**. Home Mate's constraint is single task-state ownership in its TaskRegistry. Adopting adk-runner as the loop engine means two systems each maintaining "which calls are unanswered / whether a run is valid" — **exactly the dual ownership that is disallowed**, unless Home Mate demotes its registry to a pure observer of ADK state.
- **Adapter complexity if adopted anyway:** moderate — implement `Agent` around Home Mate's OpenAI-compatible client; wrap Home Mate tools into `Tool` (thin); either implement a custom `SessionService` over Home Mate's rusqlite schema or accept adk-session's sqlx SQLite as a second store. Evidence levels have no ADK counterpart and would live entirely in Home Mate's layer — feasible but then a large part of what the runner tracks is duplicated state needing reconciliation.
- **Churn risk:** three majors in five months, MSRV 1.95/edition 2024, 3.0.0 breaking changes already queued on main — a fast-moving single-maintainer dependency.

## 7. Maintenance risk

- **Bus factor ≈ 1.** jkmaina authored essentially all recent commits; ~17 historical contributors; one sponsor. Very active (commits within the last week of the scan; 6 open issues on 1,811 commits).
- **Docs:** unusually good for a solo project — per-crate READMEs with examples, in-repo docs, GitHub Wiki, docs.rs with tiered stability contract (STABILITY.md — core crates "Stable"/semver; ACP/AWP/devtools/harness/kernel "Beta"), deprecation lifecycle (N+2 minors), reproducible benchmarks, SECURITY.md + CodeQL. "104 runnable examples" claimed at v2.0.0 (exact current count unverified).
- Community: GitHub Discussions only. No named production deployments or case studies — "production-ready" is self-described.

## 8. Verdict

**Adapt-methods-only; do not adopt as the runtime loop.** Home Mate keeps its own minimal loop, GLM client, and TaskRegistry. What to copy (as design patterns, not code, so no framework types leak past Home Mate interfaces): (1) the **outcome-unknown late-result protocol** — synthesize a persisted "cancelled while in flight, may have taken effect, check before re-calling" answer for every interrupted tool call, with a run-level in-flight tracker; (2) `RunBudget`-style multi-axis termination (model calls / tokens / cost / wall time / tool calls) with a persisted exhaustion event rather than a bare error; (3) the `ErrorCategory` taxonomy incl. `Cancelled`/`ResourceExhausted`/`Conflict` with retry hints; (4) tiered state keys (`app:`/`user:`/`temp:`) and event-carried `state_delta`; (5) tool effect classes (read-only/idempotent/non-idempotent) gating parallelism and retry, plus stable idempotency keys per call.

Conditional fallback: if Home Mate later needs graph orchestration or crash-durable runs, `adk-runner`+`adk-graph` at a pinned tag (Apache-2.0, tokio-only, trimmable to no model/audio) is the strongest Rust candidate — but adopting it means ceding run/session validity to the framework, which today's constraints forbid.

**Bottom line:** ADK-Rust (zavora-ai, v2.3.0, 2026-10-09; Apache-2.0; MSRV 1.95) is a real, unusually well-engineered, permissively licensed, tokio-only framework whose cancellation/budget/error/session designs are strikingly aligned with Home Mate's hardest problems — but it is a fast-moving single-maintainer project whose Runner necessarily owns session, event, run, and in-flight-call state, colliding directly with Home Mate's no-dual-task-state-ownership constraint; mine its first-class cancellation semantics as proven methods for Home Mate's own minimal loop, keeping pinned-tag adoption documented as a fallback if durable multi-step orchestration is ever needed.
