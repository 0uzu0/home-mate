---
name: home-mate-fleet
description: Read-only L1 health inspection, voice event diagnostics, and controlled voice announcements across all household Home Mate speakers.
---

# Home Mate Fleet Skill (L1 Self-Diagnosis & Controlled Announce)

Use this skill when the user asks about the health, online status, wake/ASR/TTS logs, or diagnostics of any Home Mate speaker in the household, or asks you to announce a message on a specific room's speaker.

## Configuration

The helper script reads speaker URLs and household Bearer tokens from `$HOME_MATE_FLEET_CONFIG` (a local JSON file outside Git). Never echo or log bearer tokens in your output.

## Commands

Run `fleet_cli.py` (located alongside this `SKILL.md`):

- **Check all speakers at once**:
  ```bash
  python3 fleet_cli.py fleet
  ```
- **Inspect one speaker's status / voice mode / diagnostics**:
  ```bash
  python3 fleet_cli.py status <node>
  python3 fleet_cli.py mode <node>
  python3 fleet_cli.py diagnostics <node>
  ```
- **Read recent voice journal events** (to diagnose missed wakes, ASR errors, TTS failures, or barge-in):
  ```bash
  python3 fleet_cli.py events <node> --limit 40
  ```
- **Send a controlled spoken announcement to a speaker**:
  ```bash
  python3 fleet_cli.py announce <node> --text "客厅音箱已恢复在线。" --task-id "<run_id>"
  ```

## Safety Rules (Mandatory — L1 Boundary)

1. **Read-only diagnosis by default**: Only use `fleet`, `status`, `mode`, `events`, and `diagnostics` to gather facts.
2. **Never mutate speaker configuration, restart services, SSH into speakers, or modify firmware** unless the owner explicitly requested and approved the exact action.
3. **Never fabricate device status**: If a node is unreachable (`reachable: false`), report honestly which node could not be reached and why.
4. **Spoken response style**: When replying to a speaker-delegated task, summarize your diagnosis in 1–3 concise Chinese sentences without Markdown tables or raw JSON.
