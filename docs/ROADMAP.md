# Home Mate Engineering Roadmap

> **Current planning entry — Luna (2026-11-17):** [Development plan](luna/PLAN.md) · [Milestones](luna/MILESTONES.md) · [Checklist](luna/CHECKLIST.md). Status: **APPROVED / L0 IN PROGRESS**; implementation and deployment after verification are owner-authorized. Human field checks remain pending. Scope: all 37 audit findings, complete configuration delivery and system wiring, dot2-d profile, mobile-assisted English study/review.
>
> **Live baseline:** serial 69, Core runtime `05da212872da`, Gateway `bbdfb3c` / `0.0.53` on five units. Local HEAD `c3ed6c6` is stale; remote source has advanced through Luna PRs, listed in [current handoff](luna/HANDOFF-NEXT.md). Current completion evidence is in [MILESTONES](MILESTONES.md). The Wave v2 material below is the historical design/checklist, not a new instruction to repeat already deployed W1–W4b work. New audit gaps are tracked by Luna; frozen/deferred scope is unchanged.

## Historical Roadmap v2 — Structural Evolution & Wave Checklists

> **Historical baseline**: Core `yes999zc/home-mate-b` @ `d8be474` (`70f03e3` runtime), Gateway `yes999zc/home-mate-device-gateway-b` @ `66bd2e4` (`2026.11.17-68` / serial `68` across `dot2-a`–`dot2-e`).
> **Companion Ledger**: [docs/MILESTONES.md](MILESTONES.md) · **Issue Index**: [#35](https://github.com/yes999zc/home-mate-b/issues/35) · **Archived Phase 0–7 Roadmap**: [docs/archive/ROADMAP-phase0-7-20261117.md](archive/ROADMAP-phase0-7-20261117.md).

---

## 1. Ground-Truth Code Reconciliation (What Already Landed vs Structural Debt)

Completed Phases 0–7 (M1–M6a, including `Phase 7 = Done (Core)`) and Post-M6 Stabilization (**Core PRs #66, #67, #68** and **Gateway PRs #48, #49**) closed operational bugs and UX debt, **yet left three structural root causes intact**:

| Issue | What PR #67 / #68 / gw#49 Already Changed on `main` | What Structural Gap Still Remains |
|---|---|---|
| **#69** | Raised `COALESCE_MS` from `1600` to `2400` (`arbitration.rs:62`) and replaced the 30 ms wall-clock race in `slow_peer_response_within_window_still_participates` (`wake_arbitration.rs:648`) with deterministic `start_gated_peer`. | Window (`2400 ms`) is still an unmeasured constant (`#76` clock/lag telemetry missing), and KWS-only utterance clustering still merges two concurrent rooms speaking 1 s apart or drops slow speakers (`#75`). |
| **#71** | Added `MIN_LOUDNESS_EVIDENCE_DBFS = -50.0`, floored `decide_inner`, added `Why::LouderUncalibrated`, and added negative test `below_loudness_floor_falls_back_to_arrival_time` (`arbitration.rs:71,278,858`). | `runner_db.map_or(true, ...)` (`arbitration.rs:279`) STILL lets a single measured `-45 dBFS` claim unconditionally beat peers whose claims carry `None` (e.g., ASR fallback `#74`). Missing comparable evidence pool validation. |
| **#72** | Updated `voice/mod.rs:421` comment from `80 ms` to `160 ms` (`TAIL = 2560`). | `voice/pump.rs:1297` comment still reads `80 ms`, and Gateway `aec::TAIL` (`160 ms`) vs Core `default_tail_blind_ms` (`400 ms`, `voice/settings.rs:556`) have zero wire contract or startup check linking them. |
| **#70** | Explicitly demoted `Claim.room` / `winner_room` to display-only in docs and `ArbitrationCard.vue`. | `decide_inner` still ignores room continuity and active room ownership (`#75`). In real household use, an open hallway still lets an idle room steal turns. |
| **#74** | `admit_asr_wake` checks `Arbiter` sliding window (`reflex.rs:192`); late loss triggers silent retreat (`turn.rs:34`). | All 4 core bugs remain: ① `retreat_session_at` ignores session owner (`states` has no owner); ② `reflex.rs:215` ASR fallback sends `Claim::new(device, now, None)`; ③ zero passive near-end voice evidence when KWS misses; ④ `asr_wake_fallback` conflates idle wake vs post-sovereignty transcription. |
| **#78/#79** | Working memory shipped (500-item SQLite store, API & Console card). | `agent/mod.rs:514` hardcodes `scope_person = None`; `scope_device` parses `"voice:"` string prefixes; `workmem.set` declares `SideEffect::Query` (`tool/memory.rs:88`); hit at `MAX_ITEMS = 500` fails writes instead of aging/evicting. |

---

## 2. Structural Synthesis & Engineering Judgments (Where Issues Merge & Where We Push Back)

Auditing `#35` and `#69`–`#85` + `gw#50` together confirms they collapse into **three architectural axes** plus explicit precondition measurements (`#76`). Implementing issues one-by-one would produce redundant patches; instead we resolve each family once.

### Family Synthesis

1. **Family 1 — Single Acoustic Truth & Room Sovereignty** (`#76`, `#72`, `#73`, `#75`, `#74`, `#70`, `#71`, `#69`)
   - **#73 and #75 are two layers of ONE pipeline, not rival architectures**:
     - `#73` (§1–§2) + `#72` govern **Device-Side Truth**: collapse 4 separate near-end heuristics (`guard_corr`, `bargein::Detector`, `MicProcessor`, `decide_inner` levels) into ONE `NearEndAssessment` gated by first-class `EchoPathHealth` (`Healthy | Degraded | Unknown`) with `aec_tail_ms` carried in `GatewayFrame::Hello`.
     - `#75` + `#73` (§3) + `#70` + `#74` + `#77` govern **Core Sovereignty & Session Lifecycle**: decouple *instantaneous acoustic claim* (available even without a KWS hit) from *room-owned session sovereignty* (`SessionOwner`) and separate *mic listening window (`VoiceSession`)* from *conversation context (`ContextSession`)*.
2. **Family 2 — Memory Scope, Eviction & Shared Layer** (`#78`, `#79`, `#80`, `#82`)
   - Unify `ScopeContext { device_id, room, person_id }`, `VisibilityBoundary`, `RetentionPolicy`, and a non-blocking NAS sync outbox/cache ONCE so local privacy filtering (`#78`), bounded LRU/pin eviction (`#79`), NAS consolidation (`#80`), and future speaker identification (`#82`) share a single data contract.
3. **Family 3 — Typed Effects, Receipts & Governed Persona** (`#77`, `#84`, `#83`; `#85` frozen)
   - `#77` merges cleanly with `#75` session lifecycle in **W2**.
   - `#84` (multi-method OpenAPI writes with explicit receipt contracts) and `#83` (versioned `Agent.md` + provenance-audited `USER.md`) share Core's existing `Evidence` (`Observed | Accepted | Submitted | Sent`) + `ConfigHistory` invariants in **W4**.

### Engineering Objections (Issue Proposals Modified or Rejected)

> Per issue contract: each issue states desired system properties, not binding implementation orders. Where an RFC mechanism conflicts with hardware physics, latency, or network budgets, we adopt a cleaner design:

1. **Reject `gw#50` (Cross-device PCM/fingerprint broadcast) as a W1/W2 prerequisite → Defer behind Core `FleetPlaybackNotice`**:
   - Reverberation, clock skew, and sample-rate drift degrade cross-room audio fingerprint correlation while burning Cortex-A7 CPU. Core *already* knows every peer's live TTS/media state and text. Broadcasting compact **`FleetPlaybackNotice`** state metadata across fleet units solves "Speaker A plays wake/speech through doorway → Speaker B shouldn't self-wake" at zero DSP cost. Revisit `gw#50` *only* as an isolated experiment if hallway media playback still triggers false wakes after W0+W1+W2.
2. **Reject physical fault-LED alarm on every `EchoPathHealth::Degraded` block (`#73` §2)**:
   - Dealias unlocks legitimately happen briefly at TTS/music onset or period boundaries. Flashing a red/amber ring for 40–160 ms transients creates false "broken hardware" alarms. Keep hard ring LEDs reserved for persistent link faults (`protocol_error`, `core_offline`, `wifi_down`), and surface `EchoPathHealth` + sustained `degraded_ms` in `/api/v0/status` + Diagnostic Console (only raising a console warning after sustained unlock, e.g. `>= 5 s` continuous active play).
3. **Reject 200 ms delayed barge-in buffering (`#73` §4) in Half-Duplex**:
   - Defer `#73` §4 strictly to Post-W4 Full-Duplex (`#24` / `#11`). Half-duplex local retreat/reflex must remain zero-added-delay.
4. **Reject continuous 12.5 Hz (60 msg/s × peers) HTTP broadcast (`#75` §5)**:
   - Run 80 ms `NearEndAssessment` over loopback (`Gateway -> Core`, zero LAN traffic). Cross-room LAN peer exchange triggers **only on state transitions** (`SpeechOnset`, `Keyword`, `SessionClaim`, `SessionEnd`), bounding LAN coordination to ~0–2 RPCs per utterance instead of ~250 HTTP calls/s.
5. **Modify `#73` 附A ("SNR-only replaces fixed mic offsets") → Calibrated Offset + SNR Admission Gate**:
   - SNR alone over-weights dead-quiet rooms (a quiet whisper in a `-65 dBFS` bedroom scores higher SNR than normal speech next to a `-40 dBFS` TV in the living room). Keep calibrated `mic_gain_offset_db` (from **W0**) for comparable cross-room loudness, and add `snr_db >= MIN_SNR_DB` + `!clipped` as **admission prerequisites** before any claim competes acoustically.

---

## 3. Dependency Graph & Wave Sequence

```
  [W0] Baseline Measurements & 5-Unit Calibration (#76, mic offsets)
     │
     ├──► [W1] Single Near-End Truth, EchoPathHealth & Tail Contract (#73 §1-2, #72, #71/#74 level hygiene)
     │       │
     │       └──► [W2] Room Sovereignty, Session Ownership & Lifecycle (#75, #73 §3, #70, #74, #69, #77)
     │
     └──► [W3] Scoped & Tiered Memory (#78, #79, #80 Core + NAS memory-svc)
             │
             ├──► [W4a] OpenAPI Writes & Receipt Contracts (#84)  ──┐
             │                                                      ▼
             └──────────────────────────────────────────────► [W4b] Jarvis Agent.md & USER.md (#83)
                                                              [W4c] Speaker ID Spike (#82)

  Frozen / Deferred:  #85 (Idle Transcription) · gw#50 (PCM Broadcast) · #11/#24/#40 (Full-Duplex / S2S)
```

---

## 4. Detailed Wave Specifications & Acceptance Checklists

> **Execution Rule**: No Wave is marked `Done` without running every verification checklist item below AND passing an independent read-only `acceptance-audit` (`research` subagent) + hardware verification on live Dot2 units. Keep all new/modified `.rs`/`.ts`/`.vue` files within `tools/line_gate.py` limits (`<= 1500` lines).

---

### Wave 0 (`W0`): Field Measurements & Hardware Baseline (Preconditions)

- **Scope & Issues**: `#76` (5-unit clock skew & `lag_ms` telemetry), 5-unit `mic_gain_offset_db` acoustic calibration (`tools/calibrate_mic_offset.py`), operational check of `gw#8`.
- **No Product Code Behavior Changes Before Data**: Only add measurement instrumentation/scripts if needed; freeze threshold conclusions until numbers are recorded.

#### W0 Implementation Checklist
- [ ] **W0-1 Time Synchronization Check (`#76`)**: Confirm `ntpd` peer source across `dot2-a`–`dot2-e` (pin NAS or Mac Studio LAN NTP if drifting); verify `/api/v0/status` or small probe records simultaneous `now_ms()` deltas across all 5 online units over ≥ 10 minutes.
- [ ] **W0-2 `lag_ms` Distribution Telemetry (`#76`)**: Collect ≥ 30 wake triggers per device under both idle and busy playback/TTS load across `dot2-a`–`dot2-e` (reusing existing `lag_ms` in `/api/v0/voice/events`). Record per-device `P50`, `P95`, `max`, and jitter spread.
- [ ] **W0-3 Multi-Unit Simultaneous `heard_ms` Experiment (`#76`)**: Play a timestamped wake-word recording audibly to ≥ 2 co-located/adjacent units (`dot2-b`, `dot2-d`, `dot2-e`) across ≥ 10 trials; compute actual inter-unit `|heard_ms_A - heard_ms_B|` spread and record whether end-to-end residual error is `10 ms`-class or `100 ms`-class (explicitly gating W2's envelope grouping resolution).
- [ ] **W0-4 Five-Unit Acoustic Gain Calibration**: Follow [`docs/hardware-profiles/mic-calibration.md`](hardware-profiles/mic-calibration.md) with portable fixed-volume source @ 50 cm across all 5 units (`>= 5` samples/unit). Run `python3 tools/calibrate_mic_offset.py` to compute relative `mic_gain_offset_db`, persist per-unit `mic_gain_offset_db` + `mic_calibrated: true` into unit configs/profiles (both Core & Gateway).
- [ ] **W0-5 Measurement Ledger Doc**: Commit measured numbers (table of NTP skew, `lag_ms` percentiles, `heard_ms` spread, 5 `mic_gain_offset_db` values, and resulting W1/W2 time-budget decision) to `docs/hardware-profiles/measurements-w0-202611.md`.
- [ ] **W0-6 *(Optional Ops)* `gw#8` Cleanup**: On each speaker manually inspect `/data/local/bin/` and confirm active init uses `start_server.sh` + `slot_decision.sh` (no EchoMuse fallback), backing up and removing stale `server_a` / `server_b` / `start_server.echomuse.sh` only after verification.

#### W0 Exit / Verification Checklist
- [ ] All 5 units report `mic_calibrated: true` and show `Why::Louder` (not `Why::LouderUncalibrated`) during desk/cross-room pair wakes when ≥ 3 dB apart.
- [ ] `measurements-w0-202611.md` states explicit W1/W2 parameter bounds backed by data (no unmeasured constants).
- [ ] Independent `acceptance-audit` confirms measurements & calibration configs match live unit readbacks (`0 PARTIAL / 0 MISSING`).

---

### Wave 1 (`W1`): Single Near-End Truth, Fail-Closed Echo Health & Cross-Repo Tail Contract

- **Scope & Issues**: `#73` (§1–§2), `#72` (cross-repo echo tail alignment), `#71` (structural incomplete-evidence rejection), `#74` (item 2: ASR claim acoustic level).
- **Goal**: Eliminate scattered near-end heuristics and silent defaults across Gateway (`home-mate-device-gateway-b`) and Core (`home-mate-b`).

#### W1 Implementation Checklist
- [ ] **W1-1 Cross-Repo Protocol Frame Extensions (`home-mate-device-gateway-b` & `home-mate-b`)**:
  - [ ] Add `aec_tail_ms: u32` (derived from `TAIL * 1000 / 16_000 = 160`, never duplicate hardcoded magic) and `profile_version: String` to `GatewayFrame::Hello`.
  - [ ] Add periodic/change-triggered `GatewayFrame::EchoHealth { healthy, degraded_ms, reason }` (serializes `Healthy | Degraded | Unknown`) so Core sees persistent AEC unlocks.
  - [ ] Add lightweight `GatewayFrame::NearEndPresence` (or piggyback on existing telemetry/endpoint frame) carrying `{ ts_ms, duration_ms, rms_dbfs, noise_floor_dbfs, snr_db, clipped, echo_health, profile_version }`, strictly sampled from the same post-AEC mono ring (`<= 1` control frame per 80 ms block only when speech/state active, loopback only — zero LAN flood).
- [ ] **W1-2 Gateway Single Acoustic Evaluator (`home-mate-device-gateway-b`)**:
  - [ ] Create a single `NearEndEvaluator` module owning `EchoPathHealth`, per-block `noise_floor_dbfs`, `snr_db`, `rms_dbfs`, `clipped`, dual-threshold + min-duration hysteresis, and `guard_corr` / barge-in residual logic.
  - [ ] Wire `capture.rs`, `role.rs` (KWS guard), and `bargein.rs` to consume ONLY `NearEndEvaluator` output; remove duplicate correlation/threshold calculations outside it.
  - [ ] Enforce fail-closed health gate: whenever `EchoPathHealth != Healthy` during active playback, suppress BOTH `bargein` fire AND playback-time KWS guard (`1.0`) in that single gate (keeping unit-level `dealias` lock behavior intact and preserving measured `dealias`/`SPEEX_RATE=8000`/`TAIL=2560`/convergence metrics).
- [ ] **W1-3 Core Tail Budget Alignment (`#72`)**:
  - [ ] Fix stale `80 ms` comment in `crates/hm-core/src/voice/pump.rs:1297`.
  - [ ] Store Gateway's announced `aec_tail_ms` in `DeviceSlot` (`gateway/mod.rs`); validate/enforce `effective_tail_blind_ms >= aec_tail_ms + SAFETY_MARGIN_MS` (logging mismatch telemetry cleanly in `/api/v0/status` + startup log if user configured `tail_blind_ms < aec_tail_ms` without explicit override).
- [ ] **W1-4 Core Acoustic Evidence Integrity (`#71` structural tail + `#74` item 2)**:
  - [ ] Maintain rolling `MicLevelSnapshot` (last `N` blocks of calibrated `rms_dbfs`, `snr_db`, `clipped`, `echo_health`, `profile_version`) per device in `VoicePipeline` from `NearEndPresence` / audio metrics.
  - [ ] Eliminate `Claim::new(device, ..., None)` in `admit_asr_wake` (`voice/reflex.rs:215`): attach the device's real window `rms_dbfs`, `snr_db`, `clipped`, `echo_health`, and `profile_version`.
  - [ ] Update `arbitration::decide_inner`: reject incomplete or non-comparable claims (`!evidence_valid`, `profile_version` mismatch across peers, `snr_db < MIN_SNR_DB`, `clipped` warning, or `echo_health != Healthy`) from winning on loudness (`Why::IncompleteFallback` or downgrade to non-loudness tie-break); remove `runner_db.map_or(true)` blind pass when peer explicitly lacks valid acoustic telemetry.
  - [ ] Add Diagnostic Console surfacing in `DiagnosticPage.vue` + `/api/v0/status` for `aec_tail_ms` and sustained `EchoPathHealth::Degraded` (do NOT flash alarming hardware LED on short transient unlocks).

#### W1 Exit / Verification Checklist
- [ ] **Gateway unit & contract tests**: verify `NearEndEvaluator` single path, fail-closed `EchoPathHealth::Degraded` blocks both `guard` and `bargein` identically, `clipped` flags saturation, `Hello` emits `aec_tail_ms == 160` and `profile_version`.
- [ ] **Core unit & integration tests**: verify `admit_asr_wake` claims carry real non-`None` acoustic evidence; verify mismatched `profile_version` or low `snr_db` / missing evidence does NOT beat valid peers via `map_or(true)`; verify `tail_blind_ms` warns/clamps cleanly against Gateway `aec_tail_ms`.
- [ ] **CI & Line Gates**: `cargo fmt --check`, `cargo clippy -- -W clippy::all` (Core) / `-D warnings` (Gateway), `python3 tools/line_gate.py`, `(cd web && npm run build)` all exit 0.
- [ ] **Live Canary (`dot2-d` + `dot2-b`)**: `/api/v0/status` exposes Gateway `aec_tail_ms = 160`, `EchoPathHealth`, and ASR wake log includes `snr_db` + calibrated `rms_dbfs`.
- [ ] Independent `acceptance-audit` (`0 PARTIAL / 0 MISSING`).

---

### Wave 2 (`W2`): Per-Room Sovereignty, Session Ownership & Lifecycle Alignment

- **Scope & Issues**: `#75` (Sovereignty–Recognition decoupling + concurrent rooms), `#73` (§3 three-stage arbitration), `#70` (`room` ownership), `#74` (items 1, 3, 4: owner retreat, passive near-end evidence, two-level ASR fallback), `#69` (data-backed adaptive/envelope clustering), `#77` (8–10s window, silent timeout, drop opaque credits, `VoiceSession` vs `ContextSession`).
- **Preconditions**: Requires **W0** measurements (defines clustering tolerance & envelope resolution) and **W1** (`NearEndPresence` + `EchoPathHealth`).

#### W2 Implementation Checklist
- [ ] **W2-1 Core Room Sovereignty & Session Registry (`crates/hm-core/src/sovereignty.rs` or `voice/sovereignty.rs`)**:
  - [ ] Introduce `RoomSessionDomain` / `SessionOwner { session_id, room, owner_device, lease_until_ms, active_voice_window, context_id }`, decoupled from per-device raw socket state (`DeviceVoiceState`).
  - [ ] Enforce **Concurrent Independent Rooms**: two rooms with distinct speakers (`|t_diff| > W0_COALESCE_MS` OR low cross-room 10 ms energy-envelope correlation if W0 proved `< 100 ms` clock precision) hold separate independent `RoomSessionDomain` entries so a living room session never blocks or eats a bedroom request.
  - [ ] Implement sticky active-room preference (`#70`, `#75`): when a `RoomSessionDomain` already has an active voice window in `room`, follow-up turns/retreats from that same `room` get explicit continuity preference rather than leaking to adjacent hallway rooms.
- [ ] **W2-2 Acoustic-Driven Contention & Passive Near-End Evidence (`#73` §3 + `#74` item 3)**:
  - [ ] Allow devices that detect sustained `NearEndPresence` (`present` without a local KWS hit) to enter contention cleanly via W1 metrics when queried by an awakening peer, OR open passive arbitration when ASR hears wake text in an adjacent room (`#74` living room accent missed KWS vs study KWS case).
  - [ ] Preserve event-driven LAN cost (only exchange peer states on speech-onset / KWS trigger / state change — NO 12.5 Hz continuous polling).
  - [ ] Add `FleetPlaybackNotice` (lightweight active-play metadata synced across peers) so idle neighbors know when a peer room is broadcasting TTS/media and suppress pure bleed self-wakes without needing PCM fingerprints (`gw#50`).
  - [ ] Explicitly record abstention / loss reason (`sovereignty_decision` event: `abstained_bleed`, `owner_room_sticky`, `acoustic_winner`, `incomplete_fallback`, `room_label_display` vs `room_owner`) for Console UI (`ArbitrationCard.vue`).
- [ ] **W2-3 Owner-Scoped Retreat & Two-Level ASR Fallback (`#74` items 1 & 4)**:
  - [ ] Update `retreat_session_at` / `on_keyword_at(Retreat)`: inspect `RoomSessionDomain` ownership. Only the current `owner_device` (or an explicitly room-addressed command) executes full farewell (`retreat_ack` + session close); non-owner devices hearing trailing "退下吧" remain silent and log `retreat_ignored_not_owner` (matching `arbitration_late_silent` safety).
  - [ ] Split `asr_wake_fallback` into two settings/paths:
    1. `proactive_asr_wake: bool` (user-configurable: whether idle speech without KWS triggers wake lookup);
    2. **Passive ASR resolution** (always enabled when `RoomSessionDomain` has already awarded turn sovereignty to this device and KWS missed the phrase).
- [ ] **W2-4 Session Lifecycle Alignment (`#77`)** *(staged step-by-step behind existing `ca575ca`/`97f8247`/`7a4f1be` regression tests)*:
  - [ ] Split **two lifecycle timers**:
    - `VoiceSession` (microphone follow window): default **`8000–10000 ms`** (aligned with Alexa/Google benchmark), muted/disabled during media playback and within a `2000 ms` post-media tail guard (`#77` #5).
    - `ContextSession` (semantic reference stack for "它/继续/上一首"): decays independently over **`180–300 s`** idle while preserving permanent history.
  - [ ] Default **silent mic-window timeout** (`follow_window_tone` / voice farewell suppressed on silence timeout; spoken short ack only on explicit user exit "没事了/谢谢/退下吧").
  - [ ] Simplify `followup_credits`: either remove hidden fixed turn cap (`Unlimited` inside valid 8–10s window) OR surface remaining turns cleanly via ring cue/Console if retained as optional setting (choose clean default = no hidden quota surprise).
  - [ ] Update Console panels (`FollowUpPanel.vue`, `ArbitrationCard.vue`) & bilingual i18n (`zh` / `en`) to match new `VoiceSession` / `ContextSession` / two-tier ASR settings and read back persisted state.

#### W2 Exit / Verification Checklist
- [ ] **Test suite**:
  - [ ] `wake_arbitration`: test non-owner unit hearing `"退下吧"` does NOT interrupt active owner unit; test living-room passive `NearEndPresence` beats quieter study KWS hit; test concurrent rooms speaking together each keep independent sessions.
  - [ ] `gateway_voice`: test `VoiceSession` timeout closes mic silently (`0` farewell audio); test `ContextSession` survives beyond `8s` `VoiceSession` closure within `3 min`; test media playback + `2s` post-media suppresses follow window; test `ca575ca` / `97f8247` / `7a4f1be` invariants stay green.
- [ ] **CI & Line Gates**: `line_gate.py`, `npm run build`, `cargo test --workspace --locked`, `cargo clippy` all green.
- [ ] **Hardware Multi-Unit Verification (`dot2-a`–`dot2-e`)**: live spoken test across adjacent/distant rooms verifying single winner, no double-answer, owner-only `"退下吧"`, passive timeout silence, and Console read/write persistence.
- [ ] Independent `acceptance-audit` (`0 PARTIAL / 0 MISSING`).

---

### Wave 3 (`W3`): Unified Memory Scope, Lifecycle & Shared NAS Tier

- **Scope & Issues**: `#78` (real `ScopeContext` + `workmem.set` `Mutate`), `#79` (eviction & lifecycle instead of 500-item write failure), `#80` (2-tier local SQLite + NAS `personal-service/memory-svc` + Mac Studio `oMLX` consolidation).
- **Goal**: Answer "whose memory, visible to whom, evicted when, summarized where" in ONE unified contract.

#### W3 Implementation Checklist
- [ ] **W3-1 Core `ScopeContext` & Privacy Boundary (`#78`)**:
  - [ ] Define `pub struct ScopeContext { pub device_id: Option<String>, pub room: Option<String>, pub person_id: Option<String> }` constructed explicitly at turn entry points (`voice`, `api`, `cron`), retiring string-prefix parsing (`source.strip_prefix("voice:")`) in `agent/mod.rs:511`.
  - [ ] Enforce strict `WorkingMemoryStore::render_for_prompt(&ScopeContext)`:
    - Global items (`scope_person == "" && scope_device == ""`): visible to all.
    - Device/room items: visible ONLY when `ScopeContext.device_id` matches (`None` device context never injects all 500 device items!).
    - Person items: visible ONLY when `ScopeContext.person_id == Some(id)` matches (`None` speaker MUST NEVER leak private `person`-scoped entries).
  - [ ] Fix `workmem.set` (`tool/memory.rs:88`) from `SideEffect::Query` to `SideEffect::Mutate` + explicit attribution metadata (`written_by_device`, `source_trace_id`).
- [ ] **W3-2 Bounded Memory Lifecycle & Eviction (`#79`)**:
  - [ ] Extend `working_memory` schema (v8 migration) with `pinned: bool`, `access_count: u64`, `last_accessed_ms: u64`, and `archived_at_ms: Option<u64>`.
  - [ ] Replace hard `MAX_ITEMS = 500` error (`working_memory.rs:69`) with deterministic eviction: when approaching/reaching capacity, archive oldest un-pinned low-utility items (by `(pinned, access_count, last_accessed_ms, updated_ms)`) into the sync outbox / archive log (never silently dropping user-pinned entries; fail cleanly ONLY if 100% of 500 slots are `pinned`, with Console warning).
  - [ ] Surface capacity, pinned status, and eviction notices in `WorkingMemoryCard.vue`.
- [ ] **W3-3 Core Shared-Memory Outbox & Degraded Cache (`#80` Core Half)**:
  - [ ] Add durable SQLite sync outbox + local `shared_profile_cache` table in `hm-core` for NAS `memory-svc` (`/api/v1/observations` observation push, `/api/v1/profiles` cached profile pull).
  - [ ] **Strict Offline / Failure Invariant**: If NAS or Mac Studio `oMLX` is offline/slow (bounded circuit breaker, e.g., `<= 250 ms` connect/budget on async background sync, NEVER blocking the voice turn path!), Core injects only local SQLite cache/working memory with `0 ms` turn penalty and queues pending observations safely.
  - [ ] Keep Console controls on the Core minimal (`upload_shared_memory: bool`, `clear_local_observations`), delegating consolidation schedule & prompts to NAS `admin-svc`.
- [ ] **W3-4 NAS `memory-svc` & Mac Studio `oMLX` Integration (`#80` Infra Half)**:
  - [ ] Deploy `memory-svc` inside `personal-service` compose on NAS (SQLite store: 30-day rolling TTL on raw observations; permanent versioned summaries/profiles preserving conflicting observations by timestamp).
  - [ ] Wire configurable nightly consolidation schedule (default `03:00` local, configurable via `admin-svc`) calling `http://192.168.1.20:9981/v1` (`DeepSeek-V4-Pro-Qwen3.5-9B-4bit`) with busy-check / backoff so daytime real-time ASR/VL traffic is never starved.
  - [ ] Add Console / `admin-svc` visibility for backlog count (`N pending batches`), summary provenance (linked back to `trace_id` / `device_id`), edit, and delete/revert.

#### W3 Exit / Verification Checklist
- [ ] **Unit & integration tests**:
  - [ ] `ScopeContext`: `(person="alice", "")` is absent when `person_id == None` or `Some("bob")`; `device_id == None` does not dump device-scoped entries.
  - [ ] `WorkingMemoryStore`: inserting `> 500` unpinned entries succeeds via deterministic eviction while preserving `pinned` entries.
  - [ ] Shared-memory client circuit breaker: dead/unreachable NAS returns local fallback in `< 5 ms` without failing or delaying voice turn tests; queued observations flush when NAS returns.
- [ ] **Live multi-unit verification**: write observation on `dot2-b`, consolidate/mock summary via `memory-svc`, verify `dot2-d` reads updated shared context while disconnected NAS gracefully falls back to local-only.
- [ ] Independent `acceptance-audit` (`0 PARTIAL / 0 MISSING`).

---

### Wave 4 (`W4`): Write-Capable Tools, Governed Jarvis Persona & Speaker Identity

> Sub-staged so safe real-world execution (**W4a**) lands before autonomous persona/profile updates (**W4b**), and speaker classification (**W4c**) runs a real accuracy spike before gating private memory.

#### W4a — OpenAPI Write Operations & Verification Contracts (`#84`)
- [ ] **W4a-1 Multi-Method & Effect Classification**: Extend `OpenApiOperation` (`tool/openapi.rs`) to parse/whitelist `POST`, `PUT`, `PATCH`, `DELETE` alongside `GET`, with explicit `EffectLevel::Query | Mutate | Destructive`.
- [ ] **W4a-2 Per-Operation Verification Contract**: Require every non-GET whitelisted op to declare ONE verification strategy:
  - `ReadbackOp { op_id, json_path, expected }` (post-write verification GET); OR
  - `ResponseAssert { json_path, expected }` (structured receipt inside write HTTP response); OR
  - `TransportOnly` (explicitly produces `Evidence::Sent`, never `Evidence::Observed` / `is_confirmed`, forcing facts/answers to state "已发出指令，未验证最终状态").
- [ ] **W4a-3 Safety Guards**: Disable automatic HTTP transport retries on non-idempotent writes (`POST` without idempotency header); enforce confirmation gate on `EffectLevel::Destructive` (e.g., locks, purchases, outbound external messages); preserve strict token non-echo, SSRF IP checks, and body caps; update `OpenApiCard.vue` (`zh`/`en`) for configuring effect level, verification rule, and high-risk flag.

#### W4b — Governed `Agent.md` & Auditable `USER.md` (`#83`, requires W3 + W4a)
- [ ] **W4b-1 Versioned `Agent.md` Config**: Add editable `Agent.md` persona file/revision store in `data_dir` (separate from emergency `system_prompt_override`), hot-reloaded on the next turn without Core restart, backed by revision history + 1-click rollback.
- [ ] **W4b-2 Guarded `USER.md` Profile Proposals**: Allow model/consolidation layer to propose structured `USER.md` updates ONLY with explicit provenance `(source_device, trace_id, turn_ts, rationale)`, surfaced in Console for user review, edit, or revert; **forbid** autonomous model self-modification of safety policies or tool permissions inside `Agent.md`.

#### W4c — Post-Utterance Speaker Classification Spike & Integration (`#82`, requires W3)
- [ ] **W4c-1 Offline Enrollment & Accuracy Spike**: Benchmark lightweight post-utterance speaker embedding (`sherpa-onnx` / Cam++ / ECAPA-TDNN on finished audio segment — NOT real-time KWS!) on actual family recordings from Dot2 mics; measure False Accept Rate (FAR) & False Reject Rate (FRR) across 2–4 speakers + stranger audio on Mac Studio / NAS or Core.
- [ ] **W4c-2 Safe Identity Binding**: Only wire `ScopeContext.person_id` if measured FAR stays below household safety bar; below confidence threshold MUST fall back conservatively to `person_id = None` (household shared context only, never guessing another member's private memory). Provide Console enrollment/re-label/forget UI.

#### W4 Exit / Verification Checklist
- [ ] **W4a**: Mock OpenAPI server integration test covering ① `POST` + `ReadbackOp` passing (`is_confirmed == true`), ② `POST` + failed readback returning truthful failure, ③ `TransportOnly` downgraded to `Evidence::Sent`, ④ `POST` retry disabled on 500/timeout, ⑤ `Destructive` blocked without confirmation.
- [ ] **W4b**: Edit `Agent.md` live -> next conversation turn reflects persona without restart -> corrupt/revert restores prior revision cleanly; autonomous `USER.md` entry displays provenance and supports Console rollback.
- [ ] **W4c**: Speaker classification report committed (`FAR/FRR` on real Dot2 clips); low-confidence segment test proves `person_id = None` blocks private `person`-scoped memory leak.
- [ ] `line_gate.py`, `npm run build`, `cargo test --workspace --locked`, `cargo clippy` green; independent `acceptance-audit` (`0 PARTIAL / 0 MISSING`).

---

## 5. Complete Open Issue Disposition Matrix

| Issue | Title / Topic | Main State @ `70f03e3` / `66bd2e4` | Wave | Engineering Disposition |
|---|---|---|---|---|
| **#76** | Clock skew & `lag_ms` telemetry | Unmeasured | **W0** | Mandatory precondition measurement; gates W1/W2 time resolution |
| **#69** | Coalesce margin (`1600 -> 2400`) & deterministic peer test | Symptom patched in PR #67; unmeasured & fixed window | **W0 + W2** | Validate `2400` in W0; replace fixed KWS-only window with envelope/domain grouping in W2 |
| **#71** | Solo `rms_dbfs` win & `-50 dBFS` floor | Floor & test landed in PR #67; `map_or(true)` asymmetry stays | **W1** | Eliminate `map_or(true)` incomplete-pool bias via W1 acoustic evidence contract |
| **#72** | Echo tail alignment (`160 ms` vs `400 ms`) | `voice/mod.rs` comment fixed in PR #68; `pump.rs` & runtime check missing | **W1** | `Hello` reports `aec_tail_ms`; Core checks/links `tail_blind_ms`; fix `pump.rs:1297` |
| **#73** | RFC: single near-end truth + fail-closed health + 3-stage | Only `guard_corr` unlocked fix landed (`gw#49`) | **W1 + W2** | Adopt §1–§2 (`NearEndEvaluator` + `EchoPathHealth`) in W1, §3 in W2; reject transient fault LED & 200 ms half-duplex delay |
| **#74** | Retreat without owner, ASR `Claim(..., None)`, presence evidence | Silent retreat & sliding window landed in PR #61; 4 structural gaps stay | **W1 + W2** | ASR level (`W1`), owner-only retreat + passive presence + two-level ASR (`W2`) |
| **#70** | `Claim.room` display-only vs room participation | Explicitly display-only in PR #67 | **W2** | Elevated to active room sovereignty & sticky ownership inside W2 (`RoomSessionDomain`) |
| **#75** | RFC: sovereignty-recognition decoupling & concurrent rooms | Architectural RFC | **W2** | Core W2 design (event-driven loopback + edge transitions; reject 12.5 Hz HTTP flood) |
| **#77** | Session lifecycle (8–10s window, silent timeout, split session) | `20 s`, `credits=2`, default off | **W2** | Landed inside W2 `VoiceSession` vs `ContextSession` split; staged parameter changes |
| **#78** | `scope_person = None`, `"voice:"` hack, `workmem.set` `Query` | Unfixed on `main` | **W3** | Unified `ScopeContext` + strict visibility rules + `SideEffect::Mutate` |
| **#79** | Working memory 500-item write failure | Unfixed on `main` | **W3** | Deterministic `pinned`/LRU eviction + soft warning + archive to W3 outbox |
| **#80** | Cross-device shared memory (Core cache + NAS `memory-svc` + `oMLX`) | Unimplemented; architecture decided | **W3** | Core offline-resilient outbox/cache + NAS `personal-service/memory-svc` |
| **#84** | OpenAPI write operations & readback receipts | GET-only on `main` | **W4a** | Multi-method `EffectLevel` + verified receipt strategies (`Readback`/`Assert`/`Sent`) |
| **#83** | Jarvis persona (`Agent.md`), auditable `USER.md`, intent dispatch | String prompt + OpenAPI/socket discovery exist; file/history/audit missing | **W4b** | Versioned `Agent.md` + provenance-audited `USER.md` (requires W3 + W4a; no extra "consciousness" engine) |
| **#82** | Post-utterance speaker classification & profiles | Missing | **W4c** | Offline embedding accuracy spike first; safe `person_id` fallback on low confidence |
| **#85** | Idle always-on transcription & hallucination filter | Flag exists, unstored | **Frozen** | Indefinitely deferred per owner decision (2026-11-17) |
| **gw#50** | Cross-device audio fingerprint broadcast | Unimplemented | **Deferred** | Replaced first by Core metadata `FleetPlaybackNotice`; revisit only if bleed persists |
| **gw#8** | Manual EchoMuse cleanup on speakers | Pending manual execution | **W0 Ops** | Operational cleanup check per speaker |
| **#11/#24/#40** | Full-duplex barge-in attribution/throttling & S2S models | Deferred | **Post-W4** | Strictly deferred until Half-Duplex W1–W4 stabilize (`#11` blocks `#24`) |
