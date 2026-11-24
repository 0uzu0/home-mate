# Home Mate Master Brain Async Architecture (#267)

## Overview

Home Mate uses a **two-tier architecture** separating fast edge voice interaction from deep multi-step household intelligence:

1. **Edge Fast Brain (`hm-core` on each speaker or NAS Docker)**
   - Locked to ultra-fast offline wake/retreat/arbitration, deterministic local controls (clock, volume, music, Home Assistant, open sockets), and everyday voice chat (< 1.5 s). The legacy synchronous `remote_socket` (computer socket) was retired so remote computer/NAS/file/multi-step work goes cleanly through the async Master Brain instead of overlapping with edge tools.
   - Exposes a provider-agnostic **downward announcement API** (`POST/GET /api/v0/voice/announce`) with bounded queuing, non-interrupting busy deferral, hardware-mute/offline honesty, and optional bounded follow-up listening windows (`open_followup: true`).
   - Exposes four **upward delegation tools** (`brain.delegate`, `brain.status`, `brain.steer`, `brain.cancel`), configured live via `GET/PUT /api/v0/config/services/master_brain` (alias `/api/v0/config/master-brain`) + `POST /api/v0/config/services/master_brain/test` and the bilingual Console UI card (`MasterBrainCard.vue`).
   - Supports **Zero-Bridge Direct Hermes Mode (`protocol: "hermes"`, default)**: `hm-core` connects directly to any reachable Hermes Agent HTTP/SSE API (`/v1/runs`, `/v1/runs/{id}/events`, `/steer`, `/approval`, `/stop`), consumes SSE progress/approval/completion events in-process, cleans Markdown for natural TTS speech, and enqueues spoken callbacks directly into `VoicePipeline::enqueue_announce` — **no external bridge daemon and no `callback_url` required**.
   - Also retains **`protocol: "generic"`** for households or future backends that implement the generic Home Mate async contract (`/v1/tasks*` + downward `POST /api/v0/voice/announce`).
2. **Hermes `home-mate-fleet` Skill (`skills/home-mate-fleet/`)**
   - Standalone L1 read-only diagnostics & controlled-announce skill (`SKILL.md` + `fleet_cli.py`) installed on the Master Brain host so Hermes can inspect every speaker's status, mode, voice events, and diagnostics across the household without mutating device state.

## Contracts

### 1. Speaker Downward Announcement (`POST /api/v0/voice/announce`)

Protected by the household Bearer token middleware.

```json
{
  "text": "主脑巡检完毕，全部音箱在线。",
  "device": "dot2-d",
  "priority": "normal",
  "open_followup": false,
  "task_id": "run_123",
  "source": "master_brain",
  "ttl_ms": 300000
}
```

- Unsolicited calls (`task_id` absent) are gated by `proactive_speech` (`409 PROACTIVE_SPEECH_DISABLED` when off).
- Task callbacks (`task_id` present) are allowed even when `proactive_speech` is off (matching user reminders).
- Bounded queue (`MAX_ANNOUNCE_QUEUE_PER_DEVICE = 8`); high-priority entries evict the oldest normal entry; full high-priority queues return `429 ANNOUNCE_QUEUE_FULL`.
- Never interrupts active speech, ASR-in-flight, active turns, open listening sessions, or live voice playback; drains automatically on the 1 Hz sweep once idle.

### 2. Master Brain Configuration (`GET/PUT /api/v0/config/master-brain` or `/api/v0/config/services/master_brain`)

#### Direct Hermes Mode (default, zero bridge)

```json
{
  "enabled": true,
  "protocol": "hermes",
  "url": "http://<hermes-host>:8642",
  "timeout_ms": 8000,
  "bearer_token": "<hermes-API_SERVER_KEY>"
}
```

- Ensure Hermes Agent is configured with `API_SERVER_HOST=0.0.0.0` (or the LAN/Tailscale interface reachable from `hm-core`) and `API_SERVER_KEY`.
- `bearer_token` is stored only in the Vault (`secrets` table) and never returned in plaintext (`bearer_token_set: true`).
- Live-applied without restarting `hm-core`; `POST /api/v0/config/services/master_brain/test` probes `GET {url}/health` and `GET {url}/v1/models` with Bearer auth so an invalid key fails honestly with HTTP 401.

#### Generic Async Bridge Mode (optional)

```json
{
  "enabled": true,
  "protocol": "generic",
  "url": "http://<bridge-host>:18940",
  "callback_url": "http://<speaker-ip>:8930",
  "timeout_ms": 8000,
  "bearer_token": "<write-only-secret>"
}
```

### 3. Hermes Fleet Skill Setup (Optional)

Copy `skills/home-mate-fleet/` into your Hermes skills directory (`~/.hermes/skills/home-mate-fleet/`) and point `HOME_MATE_FLEET_CONFIG` to your local (non-Git) `fleet.json`.
