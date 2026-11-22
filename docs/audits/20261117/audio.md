# Home Mate audio / Gateway independent audit

Date: 2026-11-17. Read-only audit; no physical device, private recording, token, release, or OTA action.

## Scope and evidence

- Gateway: `/Users/bakeyzhang/AI_Workspace/01_Code/projects/home-mate-device-gateway-b`, HEAD `bbdfb3ceba3bf3bd8c8fa8076685bdad9b77844d`, version 0.0.53. Tracked tree unchanged.
- Core: `/Users/bakeyzhang/AI_Workspace/01_Code/projects/home mate b`, supplied audit HEAD `c3ed6c6`, deployed runtime baseline `05da212`. Core-side inspection covers profile API/configuration, Gateway events, wake claim formation, and retreat authorization. Parent owns complete Core tests and broader architecture.
- Read gateway AGENTS/HANDOFF and relevant Core interface/gateway documents. Latest owner scope (five independent Core/Gateway pairs, loopback Gateway, per-unit acoustics) supersedes older single-device handoff passages.
- Reviewed production capture, ALSA adapter, playback/mixing/flush/EOS, output reference ring/dealias, Speex wrapper/fusion, barge detector, NearEndEvaluator, KWS Rust supervisor + C driver, protocol/north writer, per-unit profile startup/application and corresponding tests.
- This is source and isolated-host evidence. It does not replace per-room ERLE, double-talk, wake false-positive/false-negative, drift, latency, CPU/RSS, or field cancellation measurements. The owner's five-device serial69 health report is accepted as baseline, not repeated here.

## Actual audio path

| Stage | Production behavior | Important boundaries |
|---|---|---|
| Capture | Direct ALSA, 9-channel S24 packed input at 16 kHz, 1280 samples / 80 ms block | Capture remains drained across mic-session gaps; privacy mute drops uplink; full buffer currently assumed on successful read |
| Echo reference | Capture ch8 aligns to 48 kHz stereo output history; cleaned reference rebuilt with anti-alias filtering | Per-20 ms match status aggregated across the 80 ms block; silent-reference branch has the integrity defect A2 |
| AEC/fusion | Seven Speex canceller paths, 320-sample frames, configured 2560-sample / 160 ms default tail; array fusion on post-AEC channels | The special Speex adaptation-rate setting is documented as measured tuning; it is not evidence by itself of a sample-rate bug |
| Near-end guard | Separate pre-AEC array fusion + mic/reference correlation, convergence/onset and settling logic in `bargein::Detector` | `NearEndEvaluator` centralizes echo health and emits presence, but playback presence still consumes `barge_fired`; decision ownership is not fully consolidated |
| KWS | Post-AEC PCM queued to separate streaming sidecar, 20 ms model feed blocks; ready/keyword/process-health reporting | Bounded 8-block feeder queue; guard annotates `self_heard`, Core decides semantics; eligible retreat can flush locally before Core |
| Uplink | Post-AEC mono PCM to loopback Core, no second canceller expected | Audio queue bounded to 25 chunks; new presence/health telemetry uses unbounded control queue |
| Playback | Decode worker, voice/music channels, per-unit gain/cap, period-driven mixing, direct stereo DAC writes | Command polling between periods; continuous silent output keeps reference clock running; actual write counts drive progress |
| Stop / EOS | Local/Core stop removes queued channel audio; hardware DROP only when appropriate; receipts distinguish written/consumed/unknown | Preserves reference if another channel continues; reports hardware flush failure instead of claiming success |

## Findings

Priority: P1 = fix before relying on the advertised safety/behavior contract or expanding calibration; P2 = important correctness/resilience gap; P3 = bounded quality issue. No P0 established. Field frequency and device incidence are unknown unless explicitly stated.

### A1 — P1: acoustic profile API reports applied while the main acoustic fields have no runtime consumer

**Evidence (Core):** `crates/hm-core/src/api/devices.rs:521` saves the requested acoustic profile at lines 528–550; only LED configuration is sent to the Gateway at lines 598–611; line 615 responds `state: applied`. Repository-wide production call-site search shows the only non-API/non-storage consumer of `unit_acoustic_profile` is `voice/reflex.rs:35`, reading mic gain calibration. `volume_cap`, `mix_gain`, and `vad_threshold` have no playback/VAD application path from this store.

**Evidence (Gateway):** `crates/gateway/src/main.rs:575` loads `<state_dir>/profile.json` once at startup, then `firmware/mod.rs:351` applies that independent object. `firmware/profile.rs:76` defines VAD threshold as **dBFS**, with defaults such as -42.0. Core `config/profile.rs:88` instead validates the same named field in **0.05..=0.99**; Core mix supports up to4, Gateway documents/clamps up to2. There is no acoustic-profile apply/receipt wire operation bridging these representations.

**Trigger / impact:** Save a different volume ceiling, mix gain, or VAD threshold through the management profile API. The response/readback/history changes but physical output/detection settings retain the Gateway profile/CLI values. A user can reasonably believe a volume ceiling was applied when it was only persisted. Per-unit management is therefore not complete merely because CRUD and fleet exclusion tests pass.

**Verification:** Exhaustive references and control-flow inspection. Existing Core API/profile/fleet tests assert roundtrip/preservation, not behavior at the Gateway. No audio output generated.

**Fix:** Choose one authoritative measured schema with explicit units and provenance; implement versioned Gateway apply + applied-state receipt/readback (or honestly expose stored-only/restart-required until implemented). Add a paired test proving effective cap, mix, and onset change, plus rejected/failed-apply behavior. Coordinate with architecture finding on fleet copying legacy runtime mode/VAD settings: protecting an unused profile map does not protect all actual acoustic controls.

### A2 — P1: a zeroed capture reference remains falsely “Locked / Healthy” after an earlier lock

**Evidence:** Gateway `firmware/aec/dealias.rs:244` handles an all-zero ch8 block. When previously locked, it checks whether the output ring also contains zeros at `pos`; however, **even when that match fails**, line259 returns `LockState::Locked` and line264 returns zero samples. This violates the method's lines233–239 contract that every sample came from a verified ring match. Pipeline block health accepts that state; `nearend.rs:113` calls it Healthy during playback; `bargein.rs:491` and `nearend.rs:135` fail closed only if lock/health is bad. Correlation against a missing reference can therefore become low while marked trusted.

**Trigger / impact:** Obtain a valid reference lock, then ch8 is zeroed/dropped while nonzero DAC history continues. AEC receives zeros and cannot cancel that echo, while health telemetry continues to say Healthy. KWS can emit unguarded self-heard words; the local retreat path may then act before Core filtering. This does not prove every zero frame causes an erroneous semantic barge-in (additional reference/energy gates exist).

**Verification:** Isolated actual-module repro `zeroed_reference_during_nonzero_playback_is_reported_healthy` passes, proving current false-health behavior. Existing `dealias.rs:632` test `all_zero_gap_does_not_drift_pos_and_resyncs_promptly` also explicitly expects Locked for zeroed gaps; preserving relock position is being conflated with verifying the current block.

**Fix:** Preserve internal position/relock state if useful, but return a per-block degraded/unverified state when silent ch8 fails the ring match. Only verified DAC silence (or explicit idle policy) may count as healthy. Add playback-onset, drop/zero gap, relock and KWS guard regressions with nonzero DAC history.

### A3 — P1 before nonzero W0 calibration: KWS RMS correction is applied twice; other claim routes differ

**Evidence:** Gateway `firmware/role.rs:671` reads `cfg.profile.mic_gain_offset_db` and applies `calibrate_rms_dbfs` at679 before sending Keyword RMS (`kws.rs:779`). Core `voice/pump.rs:970` stores this already corrected RMS, then `voice/reflex.rs:31` accepts it as `raw_rms` and adds Core profile `mic_gain_offset_db` again at39. By contrast, Gateway `nearend.rs:235` sends raw post-AEC RMS in NearEndPresence; Core applies its own correction once.

**Trigger / impact:** Following the calibration plan and placing a +6 dB correction on both Gateway and Core yields a +12 dB KWS correction versus +6 dB for passive evidence. A 6 dB artificial advantage exceeds the 3 dB arbitration margin and can choose the wrong room or make KWS/passive decisions disagree. Zero offsets mask this defect; no claim that current five devices have nonzero offsets is made.

**Verification:** Both producers and consuming claim path inspected; arbitration reviewer independently confirmed. Source arithmetic is deterministic. No field calibration data read.

**Fix:** Transport raw RMS with explicit units/source/version and apply exactly one correction, or mark already corrected evidence and honor that flag. Add an end-to-end contract test with distinct nonzero device offsets across KWS, passive presence, and ASR fallback. Fix before performing the postponed physical W0 calibration.

### A4 — P2: disabling semantic barge-in also silently bypasses the KWS fail-closed echo guard

**Evidence:** Gateway `firmware/role.rs:596` creates the shared detector only if `cfg.barge_in && cfg.aec`. `firmware/nearend.rs:284` returns false (not suspected) immediately if detector is absent, before consulting the evaluator's echo health. This contradicts the advertised “Unknown/Degraded during playback fails closed” behavior.

**Trigger / impact:** With barge-in disabled (or without AEC), active playback and degraded/unknown echo health still give KWS `self_heard=false`. That can admit self-wake/local retreat even though disabling semantic interruption should not disable echo integrity protection. Default/current field configuration is not asserted to exercise this branch.

**Verification:** Actual-module repro `missing_barge_detector_bypasses_degraded_kws_guard` passes for a Degraded evaluator and absent detector.

**Fix:** Evaluate the health gate independently of the optional semantic-barge feature. Keep a common echo-correlation source alive for KWS, or conservatively mark playback-time hits as suspected when no trustworthy source exists. Test barge-in on/off and AEC on/off combinations.

### A5 — P2: Hello advertises the compile-time AEC tail, not the effective instantiated tail

**Evidence:** Gateway `protocol.rs:28` derives tail milliseconds from constant `aec::TAIL`; `firmware/north.rs:1070` always emits it. CLI `main.rs:227` accepts any positive multiple-of320 tail, sets `cfg.aec_tail`, and pipeline construction uses the configured value. The detailed mic report uses configured tail, so the two observables can disagree. Hello also emits160ms when AEC is absent.

**Trigger / impact:** Run an alternative valid tail (e.g.10240 samples /640ms): AEC uses640ms but Hello reports160ms, defeating startup mismatch diagnostics. Current standard160ms deployed baseline is unaffected by this particular discrepancy.

**Verification:** Configuration/protocol/pipeline control-flow inspection.

**Fix:** Derive advertised facts from effective pipeline/configuration, represent disabled/unknown explicitly, and test multiple tails plus AEC disabled through handshake construction.

### A6 — P2: unbounded control telemetry can grow indefinitely while a connected Core stops reading

**Evidence:** Gateway `firmware/north.rs:1053` creates an unbounded control channel. `role.rs:16` enqueues without a limit. `capture.rs:470` sends health and presence via this channel; `nearend.rs:230` can emit presence every80ms (12.5/s). Writer at `north.rs:1229` prioritizes control, and `:1254` waits on socket `write_all` without a deadline. The connection monitor at1261 detects completion/failure, not lack of writer progress.

**Trigger / impact:** A live TCP connection whose peer is stalled or no longer drains input accumulates control telemetry indefinitely once kernel buffers fill. Audio's bounded25-chunk queue does not bound this memory. This creates prolonged RSS growth and stale control-event delivery on an embedded host. No OOM or field outage was reproduced or asserted.

**Verification:** Queue ownership, producer cadence and writer/monitor inspected. Slow-socket tests cannot run in this sandbox because socket binds are forbidden.

**Fix:** Coalesce replaceable health/presence into latest-state slots, bound events with explicit critical-event policy, add writer progress deadlines/reconnect, and test a peer that keeps its socket open while not reading.

### A7 — P2: local retreat flushes before Core's room-owner authorization and cannot undo a denial

**Evidence:** Gateway `firmware/kws.rs:791` calls local retreat for an eligible non-self-heard word; `role.rs:688` computes active voice/music channels and sends Flush at723/726, explicitly before consulting Core. Core `voice/turn.rs:68` later checks `sovereignty.can_retreat` and at84 can return `closed:false, ignored:not_owner` with no state mutation.

**Trigger / impact:** A retreat phrase heard by a non-owner device playing unrelated media can cut that device's audio before the Core decides the request does not belong to it. The “not owner” response cannot restore samples already dropped. This is a contract-ordering gap between the earlier low-latency local reflex and newer multi-room ownership rules. The separate cross-Core sovereignty replication issue is owned by the arbitration auditor; this local ordering remains a problem even after that issue is fixed.

**Verification:** Both Gateway flush and Core rejection paths inspected; interaction reviewer independently confirmed the Core ordering. Existing `tests/firmware_contract/retreat_word.rs:22` intentionally validates pre-Core flushing but does not include a non-owner device.

**Fix:** Propagate bounded owner/session eligibility to the reflex layer and match task/session identities, or scope immediate local retreat to owned voice while routing unrelated media stop through Core authorization. Preserve intended low latency with an explicit contract; add a two-room owner/non-owner regression.

### A8 — P2: capture ignores actual read counts and can feed stale/invalid samples while renewing its healthy heartbeat

**Evidence:** Gateway `firmware/alsa.rs:273` initializes transfer `frames` to requested size. On successful ioctl return it returns `x.frames` at290, ignoring transfer `x.result`; the neighboring corrected playback implementation explicitly documents and uses `x.result` at310/356. `DeviceHardware::capture_read` forwards this return (`firmware/mod.rs:557`). More generally `capture.rs:113` checks only Err, discards **any** returned sample count, renews `ok_ms` at122, and decodes the entire reused buffer at163.

**Trigger / impact:** A capture backend/kernel short result leaves a previous-block suffix in the reusable buffer, but the code treats it as1280 new samples. A negative result carried by the transfer structure would likewise escape wrapper validation if ioctl itself returned0. This distorts sample timing, echo alignment and KWS input, while watchdog liveness says a real block arrived. The frequency/semantics of these exceptional results on the exact vendor driver require field or driver-level fault injection; no claim of current field incidence.

**Verification:** Source-level exact transfer-result and caller-count loss. Existing write pump tests cover short writes/recovery, with no corresponding capture-count coverage identified.

**Fix:** Complete capture reads with checked offsets/counts and bounded recovery, surface fatal/zero-progress results, and only publish heartbeat/process a full valid block. Add a fake capture sequence (short read, zero progress, overrun, recovery) proving no stale samples and correct health behavior.

### A9 — P3: first quiet near-end block can be learned as the noise floor and suppress the whole initial segment

**Evidence:** `firmware/nearend.rs:163` initializes the floor from the first idle block below-40dBFS **before** speech evaluation. At168 its SNR therefore becomes0; the adaptation continues to regard equal-energy input as quiet.

**Trigger / impact:** If the evaluator starts while a sustained qualifying near-end signal is present around-45dBFS, presence is lost until a quieter interval lets the floor recover. Passive-nearby arbitration can miss the first utterance. This is startup/calibration bias, not evidence that all ordinary speech is lost.

**Verification:** Actual-module sine signal repro at approximately-45dBFS remains `present=false` and SNR≈0 for19 consecutive80ms blocks. Synthetic signal establishes estimator behavior; speech-corpus effect requires measurement.

**Fix:** Use a quiet acquisition interval/minimum-statistics estimator or explicit uncertain/calibrating state; avoid accepting one arbitrary input block as a definitive noise floor. Test speech-at-startup and noise-level step changes, measure recovery time.

## Strengths and quality assessment

- Clear Core/Gateway ownership and loopback pairing keep hardware-dependent DSP outside the brain.
- Playback cancellation is unusually well covered for this project stage: period-level command polling, partial-write offsets, hardware-aware played position, flush-failure honesty, single-/dual-channel stop, stream sequence gaps, bounded decode work, and watchdog child-process tests.
- Continuous DAC silence clock and output-history rebuilding address actual Dot2 reference behavior rather than treating ch8 as a clean analog reference.
- Seven-channel AEC and common pre/post array fusion avoid the old average-microphone correlation problem. Block-level lock aggregation is a good integrity design; A2 is a localized exception defeating it.
- KWS process readiness, loaded-vocabulary hash, stop responsiveness, bounded feeder queue and process supervision are substantial operational improvements. C driver `lag_ms` is explicitly **local decode freshness**, not end-to-end wake latency; capture/pipe/model backlog and algorithmic delay require separate timing evidence.
- Privacy mute is checked at capture gating and again before uplink. No raw capture/recording was generated during this audit.
- “Single acoustic truth” is partly implemented: health is shared, but barge-in still owns separate onset/correlation/convergence and playback presence is derived from an event (`barge_fired`). Tests should treat the full shared-contract surface, not only helper outputs.

## Validation actually run

1. `CARGO_TARGET_DIR=/tmp/hm-audit-gw-target cargo test --workspace --all-targets --locked --offline` in Gateway: **157 library tests passed**, main0. **8/8 contract tests could not bind a local TCP listener: PermissionDenied** (most at `tests/contract.rs:60`; negotiation at397/436/474). These are environment-blocked checks, not product regressions. The all-target run stops there; firmware contract tests were inspected but not reported as passed.
2. Separate scratch crate `/tmp/hm-audit-audio-repro`, path dependency on actual Gateway source; `CARGO_TARGET_DIR=/tmp/hm-audit-gw-target cargo test --manifest-path /tmp/hm-audit-audio-repro/Cargo.toml --offline -- --nocapture`: **3 tests passed demonstrating A2/A4/A9's present behavior**. These are defect reproductions, not successful acceptance tests. Scratch lock resolution used cached dependencies; baseline library suite above used the repository lock.
3. Read-only reference searches and manual control-flow review for all other findings. No hardware audio, replay corpus, network install, release action or private recording access.

## What remains to measure before calling the audio experience accepted

- Per-unit calibration provenance and effective profile readback (not simply a stored profile); identity normalization including any legacy `dot2a` alias.
- ERLE and reference-lock integrity across boot, mute, microphone session gaps, output DROP, partial writes/reads, TTS starts/ends and long silence.
- Double-talk / near-end preservation and wake precision/recall during voice, music and room noise, including zeroed-reference faults.
- End-to-end keyword-to-Core decision and audible stop latency, separating queue, model, Core and DAC backlog.
- 5-device passive/KWS arbitration with nonzero microphone corrections and identical simultaneous phrases.
- ARM device CPU/RSS/thermal and sidecar lag distributions under simultaneous KWS + AEC + TTS decode/music + management traffic. Host tests cannot certify the hardware budget.

Do not reflash the accepted serial69 baseline merely because a newer docs commit exists. Address the paired contract defects in isolated PRs, add behavior regressions, then determine the smallest paired Core/Gateway rollout with the owner after acceptance. Frozen full-duplex/S2S and other deferred issues are not reopened by this audit.


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
