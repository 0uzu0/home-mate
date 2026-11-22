# 5-Unit Microphone Gain Calibration SOP (`mic_gain_offset_db`)

Per `AGENTS.md`, the 5 Dot2 units share one hardware model and firmware family, not interchangeable acoustics. Until every participating unit has a measured `mic_gain_offset_db` and `mic_calibrated: true` in its `UnitAcousticProfile`, multi-room loudness wins are recorded honestly as `louder_uncalibrated`.

## Why Calibration Is Needed

- Wake arbitration compares `rms_dbfs + mic_gain_offset_db` across rooms when `rms_dbfs >= -50.0` (`MIN_LOUDNESS_EVIDENCE_DBFS`) and the loudest unit leads by `>= 3.0 dB` (`LOUDER_DB`).
- Per-unit mic sensitivity and enclosure variance can bias raw `rms_dbfs` by several dB.
- `Claim.room` / `winner_room` is a display label for the winning device only and never weights the arbitration decision.

## Procedure (Portable Phone @ 50 cm Reference Audio)

1. **Prepare the reference source**: Use a portable phone playing a consistent recorded wake-phrase clip at a fixed media volume (recommended: 60–70%, loud enough that all units report `rms_dbfs >= -50.0` at 50 cm).
2. **Measure each unit one at a time** (or in the same quiet room at the same 50 cm distance and on-axis angle from the Dot2 top ring):
   - Place the phone 50 cm from the unit under test.
   - Play the reference wake phrase **5–10 times**, leaving > 3 s between plays (`COALESCE_MS = 2400`).
   - Confirm each trigger appears in `GET /api/v0/voice/events` (`wake_claim` or `arbitration_*`) with `rms_dbfs`.
3. **Compute suggested offsets** relative to a chosen baseline unit (e.g. `dot2-d`):
   ```sh
   python3 tools/calibrate_mic_offset.py \
     --baseline dot2-d \
     --unit dot2-d=192.168.1.73 \
     --unit dot2-b=192.168.1.71 \
     --unit dot2-c=192.168.1.72 \
     --unit dot2-e=192.168.1.69 \
     --unit dot2a=192.168.1.78 \
     --token "$HM_TOKEN"
   ```
   (Use `--token-file` or per-unit `id=ip:port@token` if units use different bearer tokens.)
4. **Apply per-unit profile** via `PUT /api/v0/devices/:id/profile` (or the Console device profile form), setting `mic_gain_offset_db` to the suggested value and `mic_calibrated: true` for each measured unit. Never copy `UnitAcousticProfile` across units.
5. **Verify**: Re-run a quick 2-unit desk test and confirm `why` reports `louder` (not `louder_uncalibrated`) when all participating units have `mic_calibrated: true`.
