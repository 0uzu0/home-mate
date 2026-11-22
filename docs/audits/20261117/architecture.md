# Home Mate architecture and systemic health audit

Audit date: 2026-11-17. Read-only source review of Core c3ed6c6c8b5d367a44f10ee103dd0c22865f3b4d; user-supplied deployed baseline is Core 05da212 / Gateway bbdfb3c / serial 69 on five units. No devices, credentials, services, deployment, or repository source were changed. Paths below are relative to `/Users/bakeyzhang/AI_Workspace/01_Code/projects/home mate b`.

## Product and architecture understanding

Home Mate is a household voice companion with one Core per Dot2 and an independently built hardware Gateway. The Rust Core is a modular monolith; the Gateway owns hardware/audio transport and the Console provides management. NAS memory/admin services support optional shared state. This is not a NAS-hosted central brain architecture. Multi-room behavior is a current product requirement; full-duplex/S2S remains deferred as the owner specified.

Useful foundations include explicit task cancellation and session persistence, the common tool registry, transport delivery receipts/generations, configurable external speech/model services, authenticated management routes, local gateway constraints, and signed/A-B release machinery. Retain these boundaries and improve state contracts rather than rewrite the system.

## Scope

Read project instructions and HANDOFF, README, plan, architecture principles, ROADMAP and MILESTONES; inspected composition/router/auth, main, lib, tasks, sessions, store migrations/reset, config import/export and reset APIs, voice settings restoration, fleet filtering, prompt construction, Agent.md, USER.md, shared memory and release boot guard. Other agents own deep audio, arbitration, brain/tools and UI reviews. This review does not claim all repository lines or runtime paths were exercised.

## Findings

### ARCH-01 — P1: Factory reset leaves newer persona and private-memory stores active

**Evidence:** `crates/hm-core/src/store.rs:336` resets legacy config/memory/session/task/trace tables but omits `agent_md_revisions`, `user_md_entries`, `shared_profile_cache`, and `shared_memory_outbox`. `crates/hm-core/src/api/services/mod.rs:259` returns the reset result after restoring default voice settings, without clearing these stores. This applies even to reset with secrets included.

**Execution path and impact:** `crates/hm-core/src/config/mod.rs:165` reads the latest Agent.md into prompt preferences; `crates/hm-core/src/agent_md.rs:43` selects retained content; `crates/hm-core/src/agent/mod.rs:540` renders those preferences and `:573` adds scoped shared-profile and USER.md context. `crates/hm-core/src/user_md.rs:223` renders retained active entries; `crates/hm-core/src/shared_memory.rs:228` reads retained profile cache independently of the sync enable flag. After reset and reconfiguration, old persona and matching household/person context can therefore enter new prompts. Retained outbox entries remain eligible for upload after shared memory is enabled again. Configuration-history retention is intentionally not classified here: its retention policy may differ from private memory.

**Verification:** Executed the exact migration and reset SQL extracted from source in isolated Python SQLite databases, with synthetic records. All four tables retain one record after all reset deletes including the secrets branch. Source tracing establishes that these are still prompt inputs, not only dormant records. No speaker data was accessed.

**Remediation:** Define one explicit reset inventory and transaction, including the W3/W4 stores; coordinate cancellation of in-flight writers and sync workers. Add behavioral checks of the effective prompt after reset and of reenabled sync. Existing `crates/hm-core/tests/api.rs:985` reset coverage checks confirmation, metadata removal and secret retention but does not seed these newer stores. **Confidence: high.**

### ARCH-02 — P1: Interrupted schema migration can prevent the same build from starting

**Evidence:** `crates/hm-core/src/store.rs:223` applies each migration using `execute_batch`, then separately inserts `schema_version`, without a wrapping transaction. Migration 8 contains multiple ALTER statements, starting with `ALTER TABLE working_memory ADD COLUMN pinned ...`.

**Trigger and impact:** Power/process failure after that ALTER commits but before version 8 is stamped leaves version 7 and the new column. On the next open, migration 8 reruns and fails with `duplicate column name: pinned`; Store::open fails and startup exits. `crates/hm-core/src/release.rs:182` has useful boot rollback protection, but its same-target-build branch at `:231` does not restore the DB on an ordinary restart of that same build. Other release rollback paths may mitigate this; this finding does not assert every OTA bricks a unit.

**Verification:** Exact source migration SQL reproduced this failure with a close/reopen between the first ALTER and retry. This is a SQLite recovery reproduction, not a hardware power-cut test.

**Remediation:** Apply each schema migration and its version insert in the same SQLite transaction. Add a migration interruption/reopen test and verify restoration behavior under OTA boot failure. **Confidence: high.**

### ARCH-03 — P1: Fleet configuration push copies live per-unit VAD and media trim

**Evidence:** `crates/hm-core/src/api/services/system.rs:211` exports the full voice settings as `mode`. `crates/hm-core/src/fleet.rs:588` removes only the acoustic-profile object, wake words and wake reflex entries. The receiving import at `crates/hm-core/src/api/services/system.rs:413` restores only local wake words and then imports the rest. Actual controls are `crates/hm-core/src/voice/settings.rs:149` (`vad`) and `:167` (`media_trim_db`). `crates/hm-core/src/voice/mode.rs:771` replaces/persists settings and at `:788` applies restored VAD thresholds directly to live microphone processors. Persisted media trim is projected at startup in `crates/hm-core/src/voice/mod.rs:1287`; interactive edits project it in `voice/mode.rs:631`.

**Trigger and impact:** Push a valid configuration from unit A with different measured VAD/trim to unit B. B receives A's VAD immediately and retains A's media trim for startup, contrary to the explicit per-unit acoustic ownership rule. This can alter speech triggering or media balance on individually calibrated hardware. Fleet tests currently preserve the trim value rather than protect it.

**Important distinction:** The audio reviewer independently confirmed that UnitAcousticProfile volume_cap/mix_gain/vad_threshold have no runtime consumers; the profile-applied reporting defect belongs to their report. This finding concerns the separate live `mode.vad` and persisted `mode.media_trim_db` controls and must not be merged away as a duplicate.

**Remediation:** Specify an explicit cross-node configuration allowlist; retain all actual per-unit acoustic controls at both sender and receiver. Verify two units with distinct VAD and trim keep those values after push. No physical calibration or configuration push was performed. **Confidence: high.**

### ARCH-04 — P2: Configuration import partially mutates state before returning an ordinary failure

**Evidence:** `crates/hm-core/src/api/services/system.rs:373` implements sequential validation and mutation. Mode is restored at `:435`, device metadata is persisted at `:444`, and an unknown/invalid service can subsequently return BAD_REQUEST at `:466`. The handler at `:346` returns `applied` only on success; intermediate errors return only `error`. Further blocks have the same ordering pattern.

**Trigger and impact:** Import a document with valid changed device metadata followed by an unknown service name. The metadata has already changed when the API returns 400. A valid mode block can likewise already update runtime VAD and emit requests before a later failure. A user or retrying fleet client sees a failure without knowing which changes took effect. The source comment promising that invalid mode is validated first does not establish atomicity for the complete document.

**Remediation:** Validate the entire document before writes; transactionally commit stored state and change history, then explicitly report runtime application outcomes. If partial semantics are intentional, return a structured partial result with accurate applied items and recovery instructions. Add a late-invalid-block behavioral test. **Confidence: high source proof; this API path was not executed during this audit.**

### ARCH-05 — P2: Entry-point documentation contradicts the current runtime and milestone ledger

**Evidence:** `docs/HANDOFF.md:20` retains old phase/commit baselines and display-only room descriptions, and its next-up section at `:66` still presents older phases as pending. `docs/ROADMAP.md:3` retains serial 68 framing and unchecked items that overlap completed W1-W4 work. `docs/MILESTONES.md:3` and `:31` provide the newer serial 69 completion baseline. The owner explicitly supplied the current five-device baseline to prevent repeating completed deployment work.

**Impact:** An incoming agent following the mandatory entry-point documents can propose or repeat already-completed work or misread current room ownership. This is documentation/operational-health debt, not proof the deployed runtime is obsolete.

**Remediation:** Use one authoritative current-state summary linked by HANDOFF/README/ROADMAP; mark historical plans as superseded without erasing evidence. Preserve outstanding field measurement, W4c and explicitly frozen/deferred work rather than treating code completion as every acceptance criterion having passed. **Confidence: high.**

## Exact-SQL reproduction output

Artifact: `/tmp/hm-architecture-sql-repro.json`.

```json
{
  "migration_version_after_interrupt": 7,
  "migration_retry_error": "duplicate column name: pinned",
  "rows_after_reset_including_secrets_true": {
    "agent_md_revisions": 1,
    "user_md_entries": 1,
    "shared_profile_cache": 1,
    "shared_memory_outbox": 1
  }
}
```

The migration reproduction applied source migrations 1–7, stamped them, executed only the first ALTER of migration 8, closed/reopened the database, and retried source migration 8. The reset reproduction migrated an isolated database fully, seeded synthetic records in the four stores, and ran the DELETE statements extracted from factory_reset. These checks isolate SQL behavior; they do not replace Rust API integration or physical device acceptance.

## Other health observations and limits

The composition root carries many concrete services and voice management has a large positional-argument surface. Synchronous SQLite behind a mutex is called from asynchronous request paths. These are maintainability and potential latency concerns, not measured production outages; this review does not elevate them to confirmed defects without load evidence.

Root is independently running the repository checks and will report exact results. This agent avoided parallel Cargo work. No P0 defect was established in this scope. No recommendation to repeat serial 69 rollout follows from these findings: source fixes need scoped tests, independent review and owner-authorized deployment. Audio/network backlog concerns and deep UI behavior belong to their dedicated audit owners.


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
