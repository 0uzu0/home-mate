# Rust agent-kernel landscape scan (as of 2026-11-07)

> M3.5 分智能体调研报告·其四（候选全景扫描），归档于 2026-11-07。来源为公开网络资料；星数为时点快照。调研智能体与实现者分离。

Scope: Rust-native agent kernel / agent-loop / LLM-orchestration candidates other than Rig, rust-genai, ADK-Rust, and Pi (TS, method reference only). Note: two names chased from memory — an "asai" framework attributed to SambaTV and "foldl agentx" — could not be verified anywhere and are treated as nonexistent.

## Shortlist (5 candidates)

### 1. langchain-rust (Abraxas-365)
- **Repo:** https://github.com/Abraxas-365/langchain-rust — **License:** MIT — **Stars:** ~1.4k, 179 forks
- **What it is:** The long-running Rust implementation of LangChain, on crates.io since Feb 2024 with 162k downloads. Beyond chains/vector stores it ships an agent layer: chat agent with tools, OpenAI-compatible tools agent, ReAct (MRKL) agent with parse-error retries, streaming agent events **and cancellation**, persistent memory on SQLite/Postgres, OpenAI/Anthropic/Ollama clients, tracing spans for LLM calls, tools, and agent steps. Crate v8.0.0 released 2026-11-03 — four days before this scan.
- **First-pass fit:** Best feature-match of everything found outside the existing shortlist — OpenAI-compatible tool-loop agent, event streaming, explicit cancellation, SQLite-backed memory all exist today, and it's pure library (embeds in a tokio/axum monolith). Caveats: LangChain-shaped abstractions (Python mental model, not Rust-native kernel design), very fast major-version churn (evidence of v5.0 in Oct 2026 → v8.0.0 on 2026-11-03; the 5.0 date is secondary/unverified, the 8.0 date is crates.io-verified), and the framework brings its own memory/session abstractions Home Mate would need to fence behind interfaces.
- **Maintenance risk:** Moderate-high — active and community-discussed, but effectively one primary maintainer plus contributors; bus factor ~1–2. Three majors in ~12 months signals API churn risk.
- **"The obvious thing the owner meant"?** Plausibly yes — it is the default "LangChain in Rust" answer and the most-downloaded (162k) Rust agent framework found; it routinely appears alongside Rig in comparisons.

### 2. graph-flow / rs-graph-llm (a-agmon)
- **Repo:** https://github.com/a-agmon/rs-graph-llm — **License:** MIT — **Stars:** 386, 43 forks
- **What it is:** A LangGraph-inspired, type-safe framework for **resumable, stateful agent workflow graphs**: `Task` nodes, conditional edges, typed thread-safe Context state with built-in chat history, `NextAction::{Continue, ContinueAndExecute, WaitForInput, End}` for human-in-the-loop, `ExecutionStatus::{Paused, WaitingForInput, Completed}`, per-task timeouts, max-step caps, optimistic-locked sessions (in-memory or PostgreSQL), and `FlowRunner` load→execute→save cycles. Crate `graph-flow` 0.8.1 released 2026-10-21; 19.5k downloads since June 2025. LLM access is an optional feature that bridges to rig-core.
- **First-pass fit:** Strongest match to Home Mate's hard problems — pause/resume/checkpoint semantics, step budgets, WaitForInput — and it deliberately does **not** co-own model access, so it composes with the project's own GLM streaming client or with Rig rather than competing with them. Gaps: no native event-stream abstraction beyond ExecutionStatus (token streaming stays with the LLM layer), session store is Postgres/in-memory (SQLite adapter would be home-grown), and it's a workflow layer, not a full kernel.
- **Maintenance risk:** Moderate — active (release 2.5 weeks before scan), real user base, but small team (single visible author), 87 commits, pre-1.0 API stabilization ongoing.
- **"The obvious thing the owner meant"?** Moderately plausible — "the Rust LangGraph" is a recurring want in 2025-2026 discussions, and graph-flow is the most adopted of the Rust LangGraph ports.

### 3. agent-framework (codehalwell/agent-framework-rs) — Microsoft Agent Framework port
- **Repo:** https://github.com/codehalwell/agent-framework-rs — **License:** MIT (independent port, not Microsoft-affiliated)
- **What it is:** A ~25-crate workspace porting Microsoft's Agent Framework: ChatAgent with an automatic function-invocation loop bounded by max-iterations/call-counts/**wall-clock time**, retry with backoff, AiFunction tools (schemars schemas), full MCP client (stdio/HTTP/WS), token streaming, OpenTelemetry GenAI semantic-convention tracing, AgentThread sessions with serialization + store factories, checkpointed resumable workflows, group-chat/handoff/Magentic orchestrations, A2A, YAML declarative agents. Crate 0.15.0 released 2026-11-03 (first published 2026-07-13); 631 commits, 99 examples, PARITY.md/GAP_ANALYSIS.md tracking.
- **First-pass fit:** On paper the closest single-crate match to the kernel requirements. Reality check: **2 GitHub stars and 241 downloads** — essentially zero adoption; its own gap analysis lists run cancellation as missing; provider matrix is Microsoft-ecosystem-heavy.
- **Maintenance risk:** Severe — one maintainer, 4 months old, near-zero community. Any adoption would be a bet on one person's parity sprint.
- **"The obvious thing the owner meant"?** Only if the owner follows the Microsoft Agent Framework ecosystem; otherwise effectively unknown.

### 4. AutoAgents (liquidos-ai)
- **Repo:** https://github.com/liquidos-ai/AutoAgents — **License:** MIT OR Apache-2.0 — **Stars:** 778
- **What it is:** A modular multi-agent framework: typed pub/sub orchestration, ReAct and basic executors, streaming, structured outputs, derive-macro tools, WASM-sandboxed tool runtime, MCP integration, sliding-window memory with pluggable backends, a dedicated event-protocol crate, guardrails, OTel observability, many providers (OpenAI, Anthropic, Groq, Ollama, mistral.rs...), plus Python bindings. Crate 0.4.0 released 2026-07-08; 24.7k downloads since Feb 2025.
- **First-pass fit:** Moderate — the event-protocol crate and pluggable memory align, and it is genuinely Rust-native; but it is multi-agent-first (Home Mate needs one companion kernel), cancellation is not an explicit feature, and the WASM tool sandbox + Python bindings are weight Home Mate doesn't need.
- **Maintenance risk:** Moderate — real community and CI/coverage, 345 commits; last crate release ~4 months before scan, momentum partially shifted to the Odyssey runtime.
- **"The obvious thing the owner meant"?** Plausible — appears in 2026 Rust-agent-framework surveys alongside Rig (Apr 2026 Zylos.ai survey — unverified secondary source) and had multiple HN launches.

### 5. Juncture (greatwallisme)
- **Repo:** https://github.com/greatwallisme/juncture — **License:** Apache-2.0 — **Stars:** 35
- **What it is:** A faithful Rust port of LangGraph (StateGraph + Pregel) with compile-time type safety: `create_react_agent()`, `Tool`/`ToolNode`, interceptors, structured output, subgraph delegation, 9 streaming modes, `interrupt!` human-in-the-loop with `interrupt_before`, checkpoints with **SQLite/Postgres backends**, per-node timeout/retry, OTel + embedded Langfuse-style telemetry, WASM support. Crate `juncture` 0.4.0 released 2026-10-08.
- **First-pass fit:** Conceptually good — SQLite checkpoints, interrupts, streaming modes, timeouts map neatly onto Home Mate's session/cancellation needs. But it is an early-stage solo project (35 stars, 210 commits) with no releases section; mid-stream cancellation API is not documented. A smaller/younger alternative to graph-flow covering the same niche.
- **Maintenance risk:** High — single author, tiny adoption, 0.x.
- **"The obvious thing the owner meant"?** Unlikely — too small to be famous; included because its SQLite-checkpoint design is unusually relevant to Home Mate.

## Considered and excluded (famous-but-ill-fitting, one-line reasons)

- **goose** (ex-Block, now Agentic AI Foundation at the Linux Foundation): 56.3k-star Rust agent, Apache-2.0 — but a desktop/CLI/API **application** with its own provider stack and session ownership, not an embeddable kernel. Famous enough that it may well be "what the owner meant," yet structurally unusable as a kernel.
- **OpenFang**: 18.2k-star Rust "Agent OS" — a standalone platform (daemon, dashboard, Tauri app, 140+ REST endpoints), not an embeddable library.
- **Swiftide**: active and well-made, but RAG/indexing-first — the agent harness is secondary to its pipeline identity.
- **kalosm** (~2.2k stars): local-model inference stack on candle/GGUF — wrong layer for HTTP-API LLM consumption; single-maintainer.
- **llm-chain**: the 2023-era standard answer, now dormant (content stops ~2024).
- **smartgpt**: abandoned AutoGPT-style 2023 project.
- **swarms-rs**: OpenAI-Swarm-style multi-agent port — swarm/handoff shape, no kernel semantics.
- **Chidori** (~1.4k stars, Apache-2.0): durable-execution agent runtime where **agents are authored in TypeScript** inside an embedded JS engine — Rust binary, but cross-language authoring violates the Rust-native constraint.
- **a2a-rs** (92 stars, 0.10): excellent A2A-1.0 protocol plumbing + framework, but Home Mate is a single companion; no orchestration/planning logic shipped.
- **Coding-agent harnesses** — OpenAI codex CLI (Rust), `pi_agent_rust`, Helix, forge-class tools: agent *applications* for code, excluded by mission definition.
- **Rust LLM gateways** — TensorZero, Bifrost-class: model-serving/routing layer, not agent loops.
- **Symbiont** (57 stars, Apache-2.0 core + commercial Enterprise Edition): security/zero-trust agent **execution governor** (Cedar policies, sandboxing) — interesting reference for Home Mate's tool-gating, but a governance runtime, not a kernel, and the open-core split complicates commercial review.
- **Too-early / too-small, examined and passed:** neuron→Skelegent (12 stars — composable-by-construction design worth reading; no documented events/cancellation), RustyFlow (2 stars), rust-agent-framework (0 stars — though its cooperative AtomicBool cancellation + approval-pause design is a useful pattern reference), rust-langgraph.

## Recommendation

The current candidate list (Rig / rust-genai / ADK-Rust) already covers the serious mainstream options — Rig in particular remains what 2025-2026 discussions default to, and nothing found here displaces it. Two entries nonetheless deserve deep research alongside them:

1. **langchain-rust** — the only genuinely notable, actively-shipped Rust framework (v8.0.0, 2026-11-03; 162k downloads) missing from the owner's list; it already has the two hardest kernel features — streaming agent events and cancellation — plus SQLite memory; deep research is cheap because the risks (API churn, bus factor ~1, LangChain-shaped state ownership) are exactly the kind of thing a focused evaluation will confirm or kill quickly.
2. **graph-flow** (0.8.1, 2026-10-21) — it attacks the part Rig/genai/ADK-Rust handle least well: resumable, bounded, human-interruptible execution with explicit session semantics; because it delegates model access (optional rig bridge), it composes with the existing shortlist rather than duplicating it — the lowest-risk way to get Home Mate's cancellation/late-results semantics without letting a framework co-own task validity.

Treat **agent-framework (codehalwell)** as a watch-list item only: best paper-fit, unacceptable adoption risk today; revisit if it gains community.

**Honest bottom line:** no hidden heavyweight was missed — the ecosystem's serious answers are the ones already under evaluation, plus langchain-rust and, for orchestration semantics, graph-flow.
