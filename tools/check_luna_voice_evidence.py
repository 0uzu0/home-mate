#!/usr/bin/env python3
"""Fail closed on incomplete actual-pair voice evidence; never synthesizes receipts."""
import argparse
import json
from pathlib import Path


def need(condition, description):
    if not condition:
        raise AssertionError(description)


def validate_inline_capture_transfers(report):
    transfers = report["inline_capture_transfers"]
    need(len(transfers) == 1, "inline capture was not observed")
    case = report.get("case", "inline-wake")
    if case == "closed-reconnect":
        need(report["normal_voice"]["wake_mode"] == "separate" and report["reconnected_voice"]["wake_mode"] == "inline", "closed-reconnect and separate scope mislabeled")
    else:
        need(report["normal_voice"]["wake_mode"] == "inline" and report["reconnected_voice"]["wake_mode"] == "separate", "inline and split reconnect scope mislabeled")
    for transfer in transfers:
        before = transfer["before_keyword"]["input"]
        admitted = transfer["after_keyword"]["input"]
        confirmed = transfer["after_speech_started"]["input"]
        for key in ("input_id", "start_ms", "capture_deadline_ms", "lifetime_deadline_ms", "origin"):
            need(before[key] == admitted[key] == confirmed[key], f"original physical input {key} changed")
        need(not before["confirmed"] and not admitted["confirmed"] and confirmed["confirmed"], "KWS bypassed VAD confirmation")
        need(before["eligibility"] == "pending" and admitted["eligibility"] == "session", "no pending-to-admitted physical capture")
        need(not any(isinstance(span, dict) and span.get("control_only") for span in before.get("origin", {}).get("spans", [])), "pending onset marked control_only")
        need(not any(isinstance(span, dict) and span.get("control_only") for span in confirmed.get("origin", {}).get("spans", [])), "SpeechStarted marked control_only")
        if "speech_started_event" in transfer:
            need(not any(isinstance(span, dict) and span.get("control_only") for span in transfer["speech_started_event"].get("provenance", {}).get("spans", [])), "mic_speech_started provenance marked control_only")

def validate(report):
    need(report["status"] == "passed" and not report["failures"], "run failed")
    need(report["protocol"] == 0 and report["sources_unchanged"], "protocol or source drift")
    need(set(report["coverage"]) == {f"P{i:02}" for i in range(1, 14)}, "P01–P13 matrix incomplete")
    need(report["coverage"]["P01"]["status"] == "partial", "P01 scope overstated")
    need(all(report["coverage"][f"P{i:02}"]["status"] == "unexercised" for i in range(2, 14)), "unrelated coverage overstated")
    case = report["case"]
    inline = case == "inline-wake"
    closed_reconnect = case == "closed-reconnect"
    need(report["same_utterance_wake"]["status"] == ("covered" if inline else "unexercised"), "inline-wake scope mislabeled")
    need(report["same_utterance_wake"]["scope"] == "first_normal_voice_only", "inline scope overstated")
    need(report["closed_reconnect_same_utterance"]["status"] == ("covered" if closed_reconnect else "unexercised"), "closed ACK scene overstated")
    if inline or closed_reconnect:
        validate_inline_capture_transfers(report)
    if closed_reconnect:
        ack = report["closed_reconnect_ack"]
        need(ack["close_response"].get("closed") is True, "explicit session close did not return closed")
        need(ack["play_finished"].get("kind") == "play_finished", "retreat ack play_finished missing")
        need(not ack["mode_after_tail"]["outputs"] and not ack["mode_after_tail"]["input_window_open"], "mode not idle after retreat ack tail")
    for kind in ("core", "gateway"):
        need(len(report["sources"][kind]["head"]) == 40 and len(report["sources"][kind]["manifest_sha256"]) == 64, "missing actor source")
    need(all(len(sha) == 64 for sha in report["binaries"].values()), "missing actual binary hash")
    nodes = report["nodes"]
    need(len(nodes) == 3 and len({row["data"] for row in nodes}) == 3, "three independent DB roots required")
    need(len({row[key] for row in nodes for key in ("core_pid", "gateway_pid")}) == 6, "six independent actors required")
    need(all(report["core_clean_exit"]) and report["gateway_exit_codes"] == [0, 0, 0], "child cleanup failed")
    need(len(report["sidecar_cleanup"]) == 3 and all(row["group_empty"] for row in report["sidecar_cleanup"]), "sidecar group leaked")
    for index in (1, 2):
        need(not report["partners"][index]["voice"]["asr"], "unwoken external ASR was called")
        need(nodes[index]["final_probe"]["nonzero_playback_calls"] == 0, "unwoken DAC produced audio")
        need(nodes[index]["unwoken_mode"]["mic"]["counters"]["asr_skipped"] >= 1, "no unwoken real PCM consumer")
    need(not report["disconnected_status"]["gateway"]["connected"], "disconnect not consumed by Core")
    held = report["cancelled_before_asr_release"]
    need(not held["asr"]["released"] and held["mode"]["input"]["state"] == "cancelled"
         and held["mode"]["mic"]["counters"]["asr_inflight"] == 0, "ASR released before cancellation completed")
    need(report["late_asr_cancelled_mode"]["input"]["state"] == "cancelled", "old ASR input remained admitted")
    need(report["late_asr_zero_deltas"] == {"llm": 0, "tts": 0, "nonzero_dac": 0, "new_tasks": 0}, "late ASR created activity")
    fresh = report["reconnected_status"]["gateway"]
    need(fresh["connected"] and fresh["generation"] > nodes[0]["initial_generation"], "no fresh reconnect generation")
    need(all(not any(partner["http"]["counts"].get(key, 0) for key in
                     ("invalid_asr_pcm", "unexpected_asr", "asr_barrier_timeout")) for partner in report["partners"]),
         "partner rejected or timed out an actual request")
    asr = report["partners"][0]["voice"]["asr"]
    need([row["case"] for row in asr] == ["normal_voice", "late_cancelled_asr", "reconnected_voice"], "unexpected/missing actual ASR requests")
    need(all(row["released"] and row["nonzero_pcm"] and row["rate"] == 16000 and row["samples"] > 0 for row in asr), "missing actual ASR PCM")
    previous_task = None
    previous_play = None
    for case in ("normal_voice", "reconnected_voice"):
        value = report[case]
        task = value["task"]
        need(task["status"] == "completed" and task["id"] != previous_task, "task did not independently complete")
        need(value["probe"]["nonzero_playback_calls"] > 0 and not value["mode"]["outputs"], "no DAC or playback remained live")
        events = value["events"]
        streamed = [row for row in events if row["kind"] == "tts_streamed" and row.get("task") == task["id"]]
        need(len(streamed) == 1, "task-to-play identity missing")
        play = streamed[0]["play"]
        need(play != previous_play, "reconnected turn reused old playback")
        finished = [row for row in events if row["kind"] == "play_finished" and row.get("play_id") == play]
        need(len(finished) == 1 and finished[0]["consumed_ms"] > 0 and finished[0]["progress_basis"] == "hw_delay", "no actual DAC terminal")
        need(any(row["kind"] == "play_stage" and row.get("play_id") == play and row.get("stage") == "device_confirmed" for row in events), "no Firmware DAC receipt")
        need(any(row["kind"] == "turn_audio_end" and row.get("task") == task["id"] and row["played_ms"] > 0 for row in events), "task audio terminal missing")
        need(any(row["kind"] == "mic_utterance" and case in row.get("text", "") for row in events), "ASR case not consumed")
        conversation = value["conversation"]
        need(task["session_id"] == conversation["session_id"] == conversation["current_context"]["session_id"] == "dev:fixture-0",
             "task and management conversation identity mismatch")
        need(case in json.dumps(conversation, ensure_ascii=False), "management conversation readback missing")
        llm = report["partners"][0]["voice"]["llm"]
        need(any(any(message.get("role") == "user" and case in str(message.get("content", "")) for message in body.get("messages", [])) for body in llm), "actual LLM did not receive ASR input")
        previous_task, previous_play = task["id"], play


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("result", type=Path)
    args = parser.parse_args()
    validate(json.loads(args.result.read_text()))
    print("PASS actual-pair voice evidence")
