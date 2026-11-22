# Home Mate Milestone Ledger

> Canonical status ledger across `yes999zc/home-mate-b` (Core & Console), `yes999zc/home-mate-device-gateway-b` (Gateway Firmware), and `yes999zc/rui-personal-services` (NAS `memory-svc`). Updated 2026-11-17. Local Core HEAD remains `c3ed6c6`; remote source PR/CI checkpoints are listed in [Luna handoff](luna/HANDOFF-NEXT.md). Deployed baseline remains Core `05da212872da`, Gateway `bbdfb3c` / `0.0.53`, serial `69`; new source merges do not change that baseline. Detailed implementation plan and per-wave verification checklists live in [docs/ROADMAP.md](ROADMAP.md). Historical issues index: [#35](https://github.com/yes999zc/home-mate-b/issues/35).

---

## Next Version — Luna (Approved, L0 In Progress)

[Luna plan](luna/PLAN.md) · [L0–L7 milestones](luna/MILESTONES.md) · [execution checklist](luna/CHECKLIST.md). The owner selected remediation plus mobile-assisted vocabulary study, multi-learner data with one-person field validation, manual/CSV words, FSRS and three-color self-assessment. Configuration delivery, dot2-d defaults and production pipeline/dead-code verification are required scope.

On 2026-11-17 the owner explicitly approved implementation, independent audit, release and five-unit deployment. Human field tests await their return. No Luna device rollout has occurred. Historical W1–W4b completion below remains evidence of those releases; the 37 new audit findings in [PR #92](https://github.com/yes999zc/home-mate-b/pull/92) are Luna remediation, not repeated old OTA.

| Range | Status | Exit reference |
|---|---|---|
| L0 contracts/inventory | IN PROGRESS | [Milestones](luna/MILESTONES.md) |
| L1/L2/L3/L5 implementation | Narrow foundations implemented/reviewed; full tracks IN PROGRESS; L4 runtime NOT STARTED | [Milestones](luna/MILESTONES.md) |
| L6 system validation | Initial production smoke and mounted UI checks; full P01–P13/browser validation pending | [System validation](luna/SYSTEM-VALIDATION.md) |
| L7 field acceptance/release | NOT STARTED; deployment authorized, gates pending | [Field gates](luna/MILESTONES.md#l7-现场门与小瑞决策) |

---

## Epoch 1 — Historical Completed Foundation (Phase 0–7, M1–M6a)

> Preserved as the verified baseline record. The original Phase 0–7 roadmap narrative is archived verbatim at [`docs/archive/ROADMAP-phase0-7-20261117.md`](archive/ROADMAP-phase0-7-20261117.md).

| Phase | Milestone / Focus | Status | Core PRs (`home-mate-b`) | Gateway PRs (`home-mate-device-gateway-b`) | Closed Issues | Verification Evidence |
|---|---|---|---|---|---|---|
| **Phase 0** | Toolchain, CI Gates, Loopback Bind & First Splits | **Done** | #1–#6, #36–#39 | #2–#6, #30–#34 | Core #7, #27, #29, #31; Gw #7, #9, #10, #27, #28 | CI green; loopback-only bind & 16 MiB gate verified; `HANDOFF.md` pre-flight check |
| **Phase 1** | Protocol v1 Handshake, Remote Mute & 4 Link Lamps | **Done** | #46 (`55aeed7` / `bf93fb2`) | #36 (`537fbbf`) | Core #8 (proto), #47; Gw #11–#14, #37 | Verified on `dot2-d` (`G090L91072732EXG`): v1 handshake, mute sync, 4 link LEDs; A/B rollback intact |
| **Phase 2** | 32 MiB Streaming OTA Upload & Docs Cleanup | **Done** | #48 (`5c56164` / `604b402`) | — | Core #41, #42 | Streaming `/api/v0/firmware/{audio,core}` to staging without RAM spike; `size_gate.py` 32 MiB / 24 MB |
| **Phase 3** | Event Pump Decoupling, Gateway Hardening & AEC Resync | **Done** | #49 (`f55ede7` / `fab0a04`) | #38 (`e3000ec`), #39 (`e25b4fa`) | Core #32; Gw #15–#21 | Non-blocking control pump, Silero VAD offload, AEC resync across `Flush`/gaps, RSS telemetry on `dot2-d` |
| **Phase 4** | Large-File Splits & Dead-Code Removal | **Done** | #50 (`d07c000`), #51 (`9c7724b`) | #40 (`e7fd4cc`) | Core #25, #26, #28, #30, #43; Gw #26 | `voice/`, `api/`, `gateway/`, `firmware/` split; dead `CaptureGrant` & full-WAV `Play` removed; tests green |
| **Phase 5** | Interaction Overhaul (`live_turn`, `enum Barge`, 4-Block UI) | **Done** | #52 (`74a8ed2` / `c79bc09`) | — | Core #9, #10, #44, #45 | Single `live_turn`, unified KWS/ASR admission, lag/drop observability, 4-section `InteractionPage` on `dot2-d` |
| **Phase 6** | Family UX, Explicit `device-id` & 3-Layer Dot2 Profiles | **Done** | #53 (`ca1bb94` / `9aa7669`) | #41 (`e70091b` / `6966365`) | Core #13–#19; Gw #22–#25, #35 | 3-step setup, Advanced gate, core-only reset, mute-hold Wi-Fi, `device-id`, per-unit profiles & night LEDs on `dot2-d` (loudness arbitration logic ready; 5-unit `mic_gain_offset_db` calibration pending field measurement) |
| **Phase 7** | M6 Half-Duplex Jarvis & Remaining Health | **Done (Core)**; Gw #8 manual pending | #60 (`262084f` / `16b223c`) | — | Core #20, #21, #22, #23, #33, #34 | Core: cross-turn tasks, remote/open sockets, long-term working memory, SQLite 3.54.1, outbound http on `dot2-d`; full-duplex (`#11`, `#24`, `#40`) still deferred |
| **Post-M6 Polish** | Wake Arbitration Overhaul, UX/IA/i18n, Guard `ref_locked` & Auth 429 | **Done** | #66 (`0087c2c`), #67 (`5750f54`), #68 (`70f03e3`) | #48 (`5aba4f1`), #49 (`66bd2e4`) | Core #61–#65; Gw #47 | Release `2026.11.17-68` (serial `68`) Ed25519-signed & verified across `dot2-a`–`dot2-e`; `guard_corr` unlocked suppression, `COALESCE_MS=2400`, `-50 dBFS` floor, `louder_uncalibrated` honesty, 429 auth throttling |

---

## Epoch 2 — Structural Remediation & Feature Evolution (`Wave W0 – W4`)

> Replaces the Phase 0–7 forward tracker. Sequenced strictly as **Precondition Measurements (W0) → Structural Refactoring (W1–W3) → Tool/Brain Capabilities & Tuning (W4)** so shared invariants are unified before parameter tuning or new features land.

| Wave | Focus | Status | Issues Covered / Absorbed | Preconditions / Dependencies | Primary Exit Gate (Summary; see [ROADMAP](ROADMAP.md)) |
|---|---|---|---|---|---|
| **W0** | **Field Baseline Measurements & Unit Calibration** | **Not Started (Owner Field Run)** | Core [#76](https://github.com/yes999zc/home-mate-b/issues/76), 5-unit `mic_gain_offset_db` calibration; optional ops [gw#8](https://github.com/yes999zc/home-mate-device-gateway-b/issues/8) | Owner at home with all 5 units online (`serial 69` already deployed) | Committed 5-unit clock-skew & `lag_ms` P50/P95/max dataset + calibrated per-unit `mic_gain_offset_db` (`mic_calibrated=true` on 5 units) + explicit time-resolution decision (`10ms` vs `100ms` tier) |
| **W1** | **Single Near-End Truth, Fail-Closed Echo Health & Cross-Repo Tail Contract** | **Done (Code/OTA; Field Check Pending W0)** | Core [#72](https://github.com/yes999zc/home-mate-b/issues/72), [#73](https://github.com/yes999zc/home-mate-b/issues/73) (§1–§2), [#71](https://github.com/yes999zc/home-mate-b/issues/71), [#74](https://github.com/yes999zc/home-mate-b/issues/74) (item 2); Gw PR #51 (`bbdfb3c`), Core PR #89 (`1cc0051`) | Deployed to `dot2-a`–`dot2-e` in `serial 69` | Gateway `NearEndAssessment` + `EchoPathHealth`; `Hello.aec_tail_ms=160` (`tail_mismatch=false` verified on all 5 units); ASR acoustic level/SNR; incomplete claims rejected |
| **W2** | **Per-Room Sovereignty, Session Ownership & Lifecycle Alignment** | **Done (Code/OTA; Field Check Pending W0)** | Core [#75](https://github.com/yes999zc/home-mate-b/issues/75), [#73](https://github.com/yes999zc/home-mate-b/issues/73) (§3), [#70](https://github.com/yes999zc/home-mate-b/issues/70), [#74](https://github.com/yes999zc/home-mate-b/issues/74) (items 1,3,4), [#69](https://github.com/yes999zc/home-mate-b/issues/69), [#77](https://github.com/yes999zc/home-mate-b/issues/77), [#88](https://github.com/yes999zc/home-mate-b/issues/88); Core PR #90 (`05da212`) | Deployed to `dot2-a`–`dot2-e` in `serial 69` | Unified `RoomSessionDomain` + concurrent rooms; passive `NearEndPresence` contention; owner-only retreat; two-level ASR fallback; `VoiceSession` (9s, silent close, `voice_live`/`media_live` guarded) vs `ContextSession` (240s); `FleetPlaybackNotice` producer & `sovereignty_decision` UI |
| **W3** | **Scoped & Tiered Household Memory Architecture** | **Done (Code/NAS/OTA)** | Core [#78](https://github.com/yes999zc/home-mate-b/issues/78), [#79](https://github.com/yes999zc/home-mate-b/issues/79), [#80](https://github.com/yes999zc/home-mate-b/issues/80); Core PR #89 & #90 (`X-Internal-Token`); NAS `rui-personal-services` PR #1 (`72ab5dc`) | NAS `192.168.1.10:18084` (`memory-svc`) & `:18080` (`admin-svc`) live | Explicit `ScopeContext`; `workmem.set` `SideEffect::Mutate`; bounded LRU/pinned eviction; NAS `memory-svc` + `admin-svc` live & wired with `X-Internal-Token`; `assistant_name` configurable (hardcoded assistant identity removed) |
| **W4** | **Write-Capable Tools, Governed Jarvis Persona & Speaker Identity** | **W4a & W4b Done (PR #90); W4c (#82) Next** | **W4a**: [#84](https://github.com/yes999zc/home-mate-b/issues/84) (PR #90) · **W4b**: [#83](https://github.com/yes999zc/home-mate-b/issues/83) (PR #90) · **W4c**: [#82](https://github.com/yes999zc/home-mate-b/issues/82) (Not Started) | **W4c** requires **W3** | `W4a`: OpenAPI multi-method `EffectLevel`, readback/assert receipts, idempotency-aware POST retry control, destructive confirm; `W4b`: versioned `Agent.md` hot-reload & auditable `USER.md` Console edit/revert; `W4c` (`#82`): post-utterance speaker embedding spike & safe `person_id` binding |

---

## Deferred / Frozen / Won't-Do Registry

| Issue(s) | Topic | Status | Rationale & Re-Entry Condition |
|---|---|---|---|
| Core [#85](https://github.com/yes999zc/home-mate-b/issues/85) | Idle always-on transcription retention & hallucination filter | **Indefinitely Deferred** (2026-11-17) | Owner decision: explicitly excluded from scheduling until W1–W3 stabilization completes |
| Gateway [gw#50](https://github.com/yes999zc/home-mate-device-gateway-b/issues/50) | Cross-device audio fingerprint broadcast | **Deferred / Spike-Gated** | Rejected as W1 prerequisite; Core metadata `FleetPlaybackNotice` + W0/W1/W2 evaluated first; revisit only if unattributable cross-room media bleed remains significant in measured multi-unit tests |
| Core [#11](https://github.com/yes999zc/home-mate-b/issues/11), [#24](https://github.com/yes999zc/home-mate-b/issues/24) | Full-duplex barge-in attribution, rate limiting & `200ms` delayed barge check | **Deferred (Post-W4)** | Project operates half-duplex by default; `#11` blocks `#24`; `#73` §4 200ms barge delay belongs here, never in half-duplex reflex |
| Core [#40](https://github.com/yes999zc/home-mate-b/issues/40) | Full-duplex S2S model evaluation (`Haru-4B` / `MiniCPM-o 4.5`) | **Deferred (Post-W4)** | Requires GPU cluster for 5 concurrent cores; decoupled behind existing brain/tool boundary later |
| Core [#81](https://github.com/yes999zc/home-mate-b/issues/81), [#17](https://github.com/yes999zc/home-mate-b/issues/17), [#18](https://github.com/yes999zc/home-mate-b/issues/18) | MCP client, bilingual TTS/wake phrases | **Closed (`Won't Do`)** | Evaluated and explicitly dropped by owner |
