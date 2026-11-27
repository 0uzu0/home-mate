# Dot2 3-Layer Hardware Profile

This document defines the 3-layer split for **Dot2** units running Home Mate (one core per unit, loopback-only gateway, multi-room via `stable_id`).

## Identity & Per-Unit Rule

- **Identity**: Every unit is identified by its `stable_id` (e.g. `dot2-d`, `dot2-e`) and optional household `device_meta` (`name`, `room`). IP addresses are transport details, never device identity.
- **No cross-unit acoustic copy**: Hardware shares one model and firmware family, but acoustic parameters (volume cap, mix levels, VAD thresholds) are measured per unit and are **never** copied across `stable_id`s.

## 3-Layer Split

### 1. Session (Core — `hm-core`)
- Owns conversation state, turn lifecycle, LLM/ASR/TTS orchestration, and tool execution.
- Owns household-level configuration (`services`, `audio_eq`, `reflex` wake words, `voice` mode) and per-`stable_id` runtime state (`muted`, `volume`, `device_meta` room/name).
- Preserves the `echomuse` EQ preset (`AudioEqConfig::default().curve == "echomuse"`, in `AUDIO_CURVES` alongside `official`, `rui`, `custom`) as the wire-format voicing selector.
- Records physical button facts (`DeviceEvent::Button`) in `voice/events`:
  - `action_press` stops active playback.
  - `mute_hold_wifi_setup` / `113_hold` records `wifi_provisioning_requested` (`trigger: "mute_hold"`) and surfaces `wifi_provisioning` in `GET /api/v0/status` so the Web Console displays a Wi-Fi setup notice banner.
  - **Owner rule (`#16` / `#23`)**: Mute-hold is a local Wi-Fi provisioning trigger/notice only and **NEVER** triggers factory reset; factory reset is a Core/Console command only (`POST /api/v0/system/factory-reset` requiring `{"confirm":"FACTORY_RESET"}`).

### 2. Hardware Profile (Per-Unit Calibrated Parameters)
- Keyed by `stable_id` and exposed via `GET /api/v0/devices/:id/profile` and `PUT /api/v0/devices/:id/profile` (never copied across units in fleet push or backup/restore).
- Holds unit-specific acoustic calibration (`UnitAcousticProfile`) and LED/orientation configuration (`DeviceLedConfig`):
  - **Volume cap (`volume_cap`)**: maximum safe speaker output level for that physical enclosure.
  - **Mix levels (`mix_gain`)**: playback/TTS/media balance and ducking/attenuation curves.
  - **VAD thresholds (`vad_threshold`)**: speech onset/hangover thresholds tuned to the unit's mic array and room noise floor.
  - **Night LED & Ring Orientation (`DeviceLedConfig`)**: per-unit night dimming window (`night_enabled`, `night_start_minute`, `night_end_minute`, `night_brightness_pct`) and physical LED ring orientation (`orientation_rotation`, `orientation_reverse`).
- Two `stable_id`s (e.g. `dot2-d` in `客厅` and `dot2-e` in `儿童房`) maintain independent `UnitAcousticProfile`, `DeviceLedConfig`, volume, mute state, and `device_meta` without overwriting each other.

### 3. Port (Device Gateway — `home-mate-gateway-linux`)
- Runs on loopback only and adapts the physical Dot2 hardware to the versioned Core–Gateway protocol (`HelloDevice`, `DeviceStatus`, `VolumeReceipt`, `MuteState`, `Button`, `PlayStream`, `Stop`, `Flush`).
- Streams audio via sentence-chunked `PlayStream` (`Start` -> `Chunk` -> `End` with `PlayStage` receipts); legacy monolithic WAV `Play` is retired.
- Owns ALSA/audio I/O, AEC/KWS front-end, LED ring rendering, DSP/loudness execution (including the `echomuse` biscuit voicing chain), and Wi-Fi provisioning execution.
