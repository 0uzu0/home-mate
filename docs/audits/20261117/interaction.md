# Home Mate interaction and follow-up audit (read-only, 2026-11-17)

Scope: Core source at `c3ed6c6` (runtime base `05da212`, serial 69 per owner), focusing on `interaction.rs`, `voice/{participation,mod,pump,pump_turn,turn,playback,reflex,mode,settings,v2_tests}.rs`, `session.rs`, `sovereignty.rs`, `tool/music/{session,tools,mod}.rs` and relevant Gateway hub receipt handling. This review did not connect to devices, touch credentials, alter tracked sources, or validate acoustic performance. The five deployed units and completed W1–W4 baseline are accepted as owner-provided evidence. No repeat OTA is indicated merely by a newer documentation commit.

Summary: four concrete implementation gaps; no P0 found within this scope. Two P1 interaction/truthfulness issues and two P2 contract gaps. Priorities are suggested, not release authorization.

## I-1 — P1 — Follow-up admission expires while the user is speaking or ASR is pending

**Trigger:** Follow window enabled, deadline 9 seconds after the previous answer. User starts a new utterance at +8 seconds; utterance ends after +9 seconds, or ends at +8.5 seconds but cloud ASR returns at +9.5 seconds. No explicit new wake word is spoken, because this is an advertised follow-up.

**Execution path and proof:**

- `crates/hm-core/src/voice/participation.rs:475` (`session_open`) only considers `SessionState::Listening`, active output, active **request**, and wall-clock `now - max(last_activity_ms, tail_end_ms) < follow_window.ms`.
- `voice/pump_turn.rs:245` (`SpeechStarted`) increments a counter, logs provenance and updates assistant state. It does not reserve admission, extend a speech lease, or start a request.
- `voice/pump_turn.rs:367` increments input sequence; `:385` increments `asr_inflight`; the request is not started until after transcription.
- `voice/pump.rs:283` (`follow_window_tone_sweep`), especially `:309–329`, calls timeout solely through session/media activity, independent of VAD speech state and ASR-inflight.
- `voice/mod.rs:779` closes the session if neither media nor `session_open` holds it. `participation.rs:426–445` increments generation, cancels requests, blocks follow-up, and revokes capture lease.
- A leased result is then rejected by `voice/pump_turn.rs:513–525`. Even before a timer sweep, the ordinary transcript path computes admission at **current ASR completion time**, not capture time (`:639–657`), so the utterance loses without a new wake.
- `participation.rs:486` has a capture-interval-aware `session_covers`, but its only production dispatch use (`pump_turn.rs:328–343`) is a pre-ASR privacy decision. Final request admission ignores it.

**Impact:** Normal follow-up can be silently dropped because of utterance length or ASR latency. This is separate from intended stale-result rejection after explicit stop/retreat. The deadline needs to revoke idle admission while preserving a bounded, already-admitted utterance.

**Confidence:** High, complete static execution path. No new end-to-end VAD/ASR test run by this agent. Default configuration sets `follow_window_enabled=false` (`settings.rs:639`); actual deployed per-device values were not queried. This also affects a long first utterance after a bare wake if no request is registered before deadline.

**Fix direction:** Freeze admission provenance at speech start within a valid window, reserve that capture until bounded segmentation/ASR completion, and let explicit stop/re-wake/connection generation still revoke it. Do not globally prolong idle listening whenever any unrelated ASR is in flight.

**Regression:** Fake clock, leased PCM starting at +8s, finish +10s, fake delayed ASR; assert exactly one admitted request. Separate finish +8s/ASR +10s. Also start after +9s must deny; explicit retreat during ASR must still deny; a never-ending segment must expire by a bounded cap.

## I-2 — P1 — Media pause/stop reports device confirmation on disconnect, send failure, or failed flush

**Trigger:** `music.control pause/stop` while a play exists; Gateway disconnects after Stop was sent, Stop send fails, or Gateway returns a terminal stop receipt with an unconfirmed/failed flush stage.

**Execution path and proof:**

- `crates/hm-core/src/tool/music/session.rs:517–522` determines pause confirmation using `timeout(..., rx).await.is_ok()`; `:532–536` does the same for stop.
- `oneshot::Receiver` returns an inner `Result`; disconnect drops senders via `gateway/mod.rs:699–702`. Timeout returns `Ok(Err(RecvError))`, and `.is_ok()` incorrectly evaluates to **true**.
- Stop send failure is converted into `None` by `session.rs:474–477` for pause and `:532` for stop; `None => true` conflates failure with “nothing on the wire.” `gateway/outbound.rs:428–430` explicitly returns `Err` when sending fails.
- Even successful receiver completion cannot prove flush: `DeviceHub::stop_and_wait` is `Receiver<()>` (`gateway/outbound.rs:418`), and `gateway/mod.rs:878–883` resolves it for **all** StopReceipt stages, including a documented `unknown`/`flush_failed` case. `outbound.rs:348–356` also resolves it when a play is forgotten without a receipt.
- Those booleans become `Evidence::DeviceConfirmed` and rendered details “设备已停稳” / “设备已停稳，会话已丢弃” (`session.rs:692–699`, `:773–784`). `pause_now` repeats the nested-result issue in its `flushed` telemetry (`:493`).

**Impact:** The assistant/tool result claims a physical result that was never confirmed; UI/agent evidence is stronger than reality. A user hears “stopped” while the system may have lost its connection or not flushed. This is a concrete correctness violation of the project's device-confirmed evidence contract, not a hypothetical attack.

**Verification:** A no-network standalone Rust harness linked against the **actual built hm_core/tokio libraries** reproduced `Ok(Err(RecvError)).is_ok() == true` for a dropped stop sender. Source `/tmp/hm-audit-interaction-repro.rs`, binary `/tmp/hm-audit-interaction-repro`, log `/tmp/hm-audit-interaction-repro.log`. Both assertions passed. This harness verifies the erroneous expression, not a complete live Gateway disconnect scenario.

**Fix direction:** Return a typed stop terminal (`Flushed`, `Drained`, `Failed`, `Unknown`, `Disconnected`) rather than `()`. Require `Ok(Ok(confirmed_terminal))`, retain send errors, distinguish genuinely absent play-id from failed send. A bounded wait can release the caller without upgrading evidence.

**Regression:** Unit/integration scenarios for successful flush, timeout, dropped sender, send failure, unknown/flush_failed receipt, and stale generation. Assert only the first produces DeviceConfirmed / confirmed=true.

## I-3 — P2 — 240-second ContextSession expiry does not expire the brain's conversation context

**Trigger:** Talk to one speaker, remain idle longer than configured `context_window_ms` (default 240 seconds), then re-wake it. The new turn still receives earlier device conversation messages.

**Execution path and proof:**

- `sovereignty.rs:14–18` defines 9s VoiceSession vs 240s ContextSession; `:116–149` creates a new `rsess` identity after context expiry.
- `voice/turn.rs:729–732` always uses `SessionStore.ensure("dev:<device>")`; it does not use `SessionOwner.session_id` or a context epoch/cutoff.
- `session.rs:28–38` explicitly persists that stable session across restarts. `session.rs:86–115` selects latest N messages solely by session id, with no time cutoff or semantic session boundary.
- `agent/mod.rs:587` consumes this history for the next model turn. `context_until_ms` is not consulted in the agent context path.
- Timeout closes participation/VoiceSession but preserves context (`voice/pump.rs:327–329`), and nothing later expires the actual message context. Explicit retreat does clear it (`voice/turn.rs:130–138`), which is a different event.
- `sovereignty.renew_voice_window` has no production caller, so even its displayed context freshness is not renewed at ordinary answer/follow-up completion.

**Impact:** The setting and domain snapshot imply context expires after 240s, while the model continues resolving pronouns, tasks and previous topics across later conversations on that device. Another household member using the same speaker can inherit that active prompt context. Permanent transcript retention is legitimate but should be separate from active conversational reference.

**Verification:** The same no-network harness calls actual `RoomSessionDomain`, `SessionStore`, `Store`. It creates a room session and a stable device transcript, advances domain time past 240s, verifies owner expiry and a newly generated `rsess`, then verifies the same device history is still returned. Assertions pass. Source and log paths above. This demonstrates lack of linkage; no LLM call or physical device involved.

**Fix direction:** Add an explicit active-context id/cutoff that the voice turn and history builder share; expire only prompt context, retaining audit transcripts/memory according to their separate policies. Renew it on real permitted turns/answers; apply changes through current management API.

**Regression:** Prior topic remains referable at +239s, is absent from active prompt at +241s, archived transcript remains accessible, re-wake after expiry generates independent context, timeout preserves context before TTL, explicit retreat clears active context immediately, per-room/device isolation remains intact.

## I-4 — P2 — The 2-second post-media follow-up guard is absent from the real media/admission path

**Trigger:** Music/media ends or is stopped while a follow-up window is still otherwise open (e.g. short play or quick stop); near-tail sound/new utterance occurs in the next two seconds.

**Execution path and proof:**

- `voice/mod.rs:702–705` checks `last_media_end_ms`; `:721–734` applies it only in `session_covers_with_live`.
- The only production assignment to `last_media_end_ms` is `voice/pump.rs:1390–1391`, requiring a matching media `OutputRecord` in the **voice** outputs map. `:1327–1330` rejects PlayFinished for plays absent from that map.
- Normal music creates media plays through `hub.play_begin_ref(...,"media",...)` (`tool/music/session.rs:60–80`) and tracks them in MusicRuntime's media map/hub. It does not insert them in the voice output map. Voice output constructors inspected (`voice/turn.rs:610–630`, `playback.rs:201–216`, `:1049–1064`, `:1394–1408`) are voice channel.
- StopStage (`voice/pump.rs:1251–1323`) never sets this timestamp either. Therefore a real media end/stop does not arm the declared guard.
- Independently, `session_covers_with_live` is only used under `!proactive_asr_wake` in `pump_turn.rs:328–343`; default is true (`settings.rs:665`). Final transcript admission (`pump_turn.rs:639–657`) checks current media and `session_open`, with no post-media guard. Even a populated timestamp is bypassed there.
- Existing `voice/v2_tests.rs:280–345` manually fabricates a media OutputRecord, sets `last_media_end_ms`, and calls helper `session_covers`. It does not exercise real music completion or final transcript dispatch, so it passes while the feature is not wired end-to-end.

**Impact:** The published 2s post-media barrier is not effective for normal music flow; residual audio or unaddressed speech can be treated as follow-up when a window is otherwise active. Actual audible false-trigger rate requires device recordings and was not measured; this report claims the missing guard, not measured acoustic failures.

**Fix direction:** Record media terminal facts at the shared hub/music lifecycle (finish, confirmed stop/pause, failure/disconnect according to a defined policy) and apply the guard in the single final capture admission path, independent of proactive ASR preference. Bind the check to capture timestamps rather than late ASR return time.

**Regression:** Real hub media play lifecycle into PlayFinished/StopStage; no synthetic voice media record. Feed a post-stop leased transcript at +500ms and verify no follow-up, at +2001ms verify admission if otherwise valid. Cover proactive ASR on/off; define whether explicit wake bypasses guard.

## Rechecked behavior that should be retained

- Generation/revision/request identity and cancellation scopes protect asynchronous model/TTS paths. Output permits include generation, stop epoch and optional active request. Submission rechecks under states lock exist for local acks and streamed/non-streamed speech. These are material improvements over scattered boolean window state.
- `participation.rs:407–422` prevents an old request's completion from closing/moving a newer request's window. Only answer output termination advances the answer tail (`voice/mod.rs:737–755`); ack/filler/prompt classes cannot extend it.
- `SpokenTerminal` distinguishes written progress from hardware-consumed progress (`voice/mod.rs:88–122`). The I-2 music path loses this discipline through its separate `()` waiter.
- Media-live prevents timeout/farewell in both timer and helper (`pump.rs:304–307`, `mod.rs:779–783`). Silent timeout is the default (`settings.rs:647`), and memory proposal extraction is not incorrectly tied to the audible farewell toggle (`pump.rs:339–343`). The issues above concern in-flight input and media end, not the already-fixed active-media timeout.
- Explicit retreat revokes input/requests, clears conversational messages, discards program/media, and closes sovereignty (`voice/turn.rs:116–138`). Silent arbitration loser close intentionally does not clear history. Its failure to update sovereignty is being reported by the arbitration agent, not duplicated here.
- Physical action press stops playback; mute hold records Wi-Fi provisioning and speaks its management prompt rather than factory-resetting (`voice/pump.rs:1145–1185`). No contradictory reset path found in this Core handler.
- TTS is streamed with output identity checks, bounded device lead, sentence segmentation and preserved resume bookkeeping. A duplicate unreachable `return false` after `return true` at `voice/turn.rs:557–558` is cleanup debt, not promoted into a user-facing defect.
- Half-duplex remains the product default; future full-duplex/S2S is explicitly deferred. No finding treats deferred full-duplex delivery as a current regression.

## Cross-team boundaries and old audit document

`docs/audit-voice-interaction-20261117.md` describes an older core/gateway baseline and mixes measured evidence with pending proposals. Its ★1 KWS guard, ★2–★4 arbitration, ★5 AEC budget claims are being independently rechecked by audio/arbitration agents. Do not reopen those issues or change acoustic thresholds from this report alone; the owner states serial69 AEC tail=160 and mismatch=false for all five units.

Audio agent identified Gateway local retreat flush before Core owner authorization. Core's identity retreat guard is confirmed at `voice/turn.rs:68–90`; audio/arbitration reports own its distributed/device consequences. Brain agent owns tool discovery, long-term memory, person/room scope and remote integration; this report owns the active 240s context boundary.

## Verification limits and suggested next slice

This was a source/contract audit with two isolated deterministic demonstrations, not a device acceptance run. Root owns full CI results and independent acceptance checks. Runtime settings, latency distribution, sound conditions and five-device simultaneous behavior remain unmeasured in this subtask.

Smallest useful correction slice: (1) preserve admitted speech across idle deadline without weakening explicit cancellation; (2) typed media stop receipts and truthful evidence; (3) connect ContextSession and actual prompt history; (4) unify media terminal guard with final input admission. Each needs behavior-based tests through the real owner boundary. Any deployment should be a new reviewed PR/build followed by one-device canary and the specific interaction scenarios, not a blind repeat of already deployed serial69.


本次统一基线、优先级与验证边界见 [审计总报告](README.md)；隔离复现源码/输出见 [证据附录](evidence.md)。
