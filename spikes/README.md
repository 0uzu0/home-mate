# M3.5 isolated kernel spikes

Isolated experiments for the agent-kernel selection (M3.5). **These crates are NOT
part of the product build**: every manifest carries an empty `[workspace]` table so
the root workspace (and `cargo test` at the repo root) never sees them. Nothing here
adds a dependency to `crates/hm-core`; per the M3.5 rule, no new dependency enters
the product before the selection conclusion is reviewed.

Layout:

- `wire-mock/` — local axum server that serves recorded/synthetic SSE fixtures by
  `model` name and records connection close events; plus a `capture` bin that
  records real GLM (bigmodel v4) SSE transcripts into `fixtures/`.
- `rig-bigmodel/` — Spike A: `rig-core` model-client layer against bigmodel
  (live GLM tool-call streaming + drop-abort) and against the wire-mock replay set.
- `genai-swap/` — Spike B: `genai` 0.7.x behind Home Mate's real `LlmClient` trait
  (swap feasibility, live bigmodel + replay).
- `fixtures/` — recorded GLM wire transcripts and synthetic quirk fixtures
  (SSE response bodies only; no credentials ever appear in responses).
- `results/` — measured evidence committed as text (timings, event sequences,
  adapter line counts).

Credentials are read from the environment at runtime (`GLM_API_KEY`, `GLM_BASE_URL`
via `~/.hermes/.env`); never printed, never committed.

See `docs/kernel-selection.md` (main repo) for how these measurements feed the
selection conclusion.
