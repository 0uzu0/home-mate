# Synthetic production-entry voice pipeline

This bounded test reuses the A6 `Core`, bounded TCP proxy, Gateway probes and cleanup. It starts three independent actual Core mains and three actual `Firmware.run` processes with FakeHardware. The external KWS sidecar is a deterministic protocol partner fed actual capture PCM; it is not keyword-model recognition. External HTTP ASR verifies nonzero 16 kHz mono WAV multipart; real production ASR/LLM/TTS clients and the Agent run normally. No physical device, household data, credentials or recordings are used.

Build Core and the host-only `northbound_fixture` example from a verified Gateway checkout containing its optional `--kws-dir` and capture controls. Then run:

```sh
python3 tools/luna_voice_system.py \
  --core /absolute/core-target/debug/hm-core \
  --gateway-fixture /absolute/gateway-target/debug/examples/northbound_fixture \
  --gateway-source /absolute/gateway-checkout \
  --output-dir /absolute/synthetic-evidence
python3 tools/check_luna_voice_evidence.py /absolute/synthetic-evidence/voice-system-RUN/result.json
```

The supported default case uses a distinct KWS wake utterance followed by a question. All three nodes first receive unwoken nonzero PCM with ASR fallback disabled; actual ASR HTTP and nonzero DAC counts must remain zero. Only node 0 is then invited. Its real VAD/ASR/Agent/LLM/TTS output must reach actual FakeDAC and a consumed terminal. API conversation readback, task/play identities, source/binary hashes and all P01–P13 statuses are retained.

The fault case holds an actual ASR HTTP response, cuts only node 0's real Gateway TCP connection, observes Core's offline/cancelled input state, then releases the old ASR response. There is no explicit session-close operation. The old input must create zero new LLM/TTS/task/nonzero-DAC activity. A new connection generation must complete another fresh wake/question.

The first normal same-utterance regression is separately executable:

```sh
python3 tools/luna_voice_system.py --case inline-wake \
  --core /absolute/core-target/debug/hm-core \
  --gateway-fixture /absolute/gateway-target/debug/examples/northbound_fixture \
  --gateway-source /absolute/gateway-checkout \
  --output-dir /absolute/synthetic-evidence
```

The original baseline failure for [issue 138](https://github.com/yes999zc/home-mate-b/issues/138) is retained in historical artifacts. The current inline case captures the original pending input through the real management API before asking the PCM-fed sidecar to emit KWS, requires unchanged id/start/absolute capture and lifetime deadlines/origin, and verifies false VAD confirmation stays false until real SpeechStarted. It then requires the complete actual voice pipeline. Reconnection success in this run uses the existing separate wake/question flow.

The closed-session inline wake after reconnect, explicit `POST /api/v0/session/close`, retreat acknowledgment playback completion, and `tail_blind_ms` echo-tail wait is separately executable via `--case closed-reconnect` ([issue 141](https://github.com/yes999zc/home-mate-b/issues/141)):

```sh
python3 tools/luna_voice_system.py --case closed-reconnect \
  --core /absolute/core-target/debug/hm-core \
  --gateway-fixture /absolute/gateway-target/debug/examples/northbound_fixture \
  --gateway-source /absolute/gateway-checkout \
  --output-dir /absolute/synthetic-evidence
```

The independent disconnect negative is frozen before later scenarios. Failed runs retain actual mode/input, task and event observations.

Core CI runs the external HTTP partner/evidence-parser checks **and** the mandatory fail-closed paired real Core + pinned private Gateway (`yes999zc/home-mate-device-gateway-b`) voice pipeline across `separate-wake`, `inline-wake`, and `closed-reconnect` using `secrets.GATEWAY_REPO_READ_TOKEN` (PR `#192`, closing [issue 139](https://github.com/yes999zc/home-mate-b/issues/139)), archiving `real-core-gateway-voice-pipeline` artifacts (`result.json` + actor logs). Missing credentials or fixtures fail the CI gate closed. Physical KWS/AEC, distributed ownership, NAS memory, learning, and P02–P13 remain unexercised in this synthetic harness.
