#!/usr/bin/env python3
"""Bounded real three-Core/three-Firmware synthetic voice and cancellation run.

Reuse the A6 process, bounded proxy and DAC probes. No real hardware, credentials,
recordings or inherited HM_* settings. Fake KWS is a PCM-fed external sidecar
partner; actual keyword-model recognition and physical AEC remain unexercised.
"""
import argparse
import asyncio
import concurrent.futures
import hashlib
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from system_smoke import Core, require, reserve_ports
from luna_transport_system import FaultProxy, latest_probe, source_manifest
from luna_voice_partners import VoicePartners
from check_luna_voice_evidence import validate


class VoiceProxy(FaultProxy):
    def __init__(self, port):
        super().__init__(port)
        self.reject_new = False

    async def accept(self, reader, writer):
        if self.reject_new:
            writer.close()
            await writer.wait_closed()
            return
        await super().accept(reader, writer)

    def disconnect(self):
        self.reject_new = True
        require(bool(self.tunnels), "fault requires an actual connected tunnel")
        for tunnel in list(self.tunnels.values()):
            for writer in tunnel.writers:
                writer.close()
        self.events.append({"fault": "disconnect", "connections": self.total_connections})


async def run(args):
    root = Path(tempfile.mkdtemp(prefix="voice-system-", dir=args.output_dir.resolve()))
    deadline = time.monotonic() + args.timeout
    report = {"status": "failed", "scope": "real-main-firmware-voice", "checks": [], "failures": [],
              "case": args.case, "closed_reconnect_same_utterance": {"status": "unexercised", "issue": "https://github.com/yes999zc/home-mate/issues/141"}, "same_utterance_wake": {"status": "unexercised", "scope": "first_normal_voice_only", "issue": "https://github.com/yes999zc/home-mate/issues/138"}, "coverage": {f"P{i:02}": {"status": "unexercised"} for i in range(1, 14)},
              "limitations": ["No actual KWS model recognition, acoustic/AEC measurements or physical hardware",
                  "No distributed owner/partition acceptance, NAS memory or learning"],
              "sources": {"core": source_manifest(args.core_source), "gateway": source_manifest(args.gateway_source)},
              "binaries": {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in
                  [("core", args.core), ("gateway_fixture", args.gateway_fixture)]}, "protocol": 0, "nodes": []}
    cores, gateways, proxies, providers, logs, probes = [], [], [], [], [], []
    last = [{}, {}, {}]
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=12)
    loop = asyncio.get_running_loop()

    async def request(core, method, path, body=None):
        return await loop.run_in_executor(pool, lambda: core.request(method, path, body))

    async def wait(label, predicate, seconds=10):
        until = min(deadline, time.monotonic() + seconds)
        while time.monotonic() < until:
            value = await predicate()
            if value:
                return value
            require(all(child.poll() is None for child in gateways), "Firmware child exited")
            await asyncio.sleep(.01)
        raise RuntimeError(f"bounded condition failed: {label}")

    async def mode(index):
        value = await request(cores[index], "GET", "/api/v0/mode")
        return next((row for row in value["devices"] if row["device"] == f"fixture-{index}"), {})

    async def events(index):
        return (await request(cores[index], "GET", "/api/v0/voice/log"))["events"]

    def probe(index):
        last[index] = latest_probe(probes[index], last[index])
        return last[index]

    def command(index, text):
        gateways[index].stdin.write(text + "\n")
        gateways[index].stdin.flush()

    async def spoken_segment(index, wake):
        before = (await mode(index))["mic"]["blocks_seen"]
        command(index, "capture speech")
        if wake:
            async def pending_onset():
                value = await mode(index)
                return (value["input"].get("state") == "unadmitted"
                        and not value["input"]["confirmed"] and not value["mic"]["in_speech"] and value)
            original = await wait("original pending physical onset before KWS", pending_onset)
            require(original["input"].get("eligibility") == "pending", f"expected pending eligibility, got {original['input'].get('eligibility')}")
            require(not any(span.get("control_only") for span in original["input"].get("origin", {}).get("spans", [])), "pending onset marked control_only")
            path = root / f"gateway-{index}/kws/keyword.json"
            path.write_text(json.dumps({"keyword": "你好高达"}))
            async def transferred():
                value = await mode(index)
                return value["input"].get("eligibility") == "session" and value
            admitted = await wait("same pending physical input admitted by actual KWS", transferred)
            for key in ("input_id", "start_ms", "capture_deadline_ms", "lifetime_deadline_ms", "origin"):
                require(original["input"][key] == admitted["input"][key], f"KWS changed original {key}")
            require(not admitted["input"]["confirmed"], "KWS fabricated VAD confirmation")
            report.setdefault("inline_capture_transfers", []).append({"before_keyword": original, "after_keyword": admitted})
        async def enough():
            value = await mode(index)
            return value["mic"]["blocks_seen"] >= before + 10 and value["mic"]["in_speech"]
        await wait(f"node{index} actual VAD speech", enough)
        if wake:
            snapshot = await mode(index)
            require(snapshot["input"]["confirmed"], "actual SpeechStarted did not confirm original input")
            require(snapshot["input"]["input_id"] == original["input"]["input_id"], "SpeechStarted replaced original input")
            require(not any(span.get("control_only") for span in snapshot["input"].get("origin", {}).get("spans", [])), "SpeechStarted marked control_only")
            started_events = [row for row in await events(index) if row.get("kind") == "mic_speech_started"]
            require(bool(started_events), "missing mic_speech_started event")
            latest_started = started_events[-1]
            require(not any(span.get("control_only") for span in latest_started.get("provenance", {}).get("spans", [])), "mic_speech_started provenance marked control_only")
            report["inline_capture_transfers"][-1]["after_speech_started"] = snapshot
            report["inline_capture_transfers"][-1]["speech_started_event"] = latest_started
        command(index, "capture silence")

    async def prepare_wake(index):
        before = (await mode(index))["kws"]["wakes"]
        before_asr = len(providers[index].snapshot()["asr"])
        path = root / f"gateway-{index}/kws/keyword.json"
        path.write_text(json.dumps({"keyword": "你好高达"}))
        command(index, "capture speech")
        async def woke():
            snapshot = await mode(index)
            return snapshot["kws"]["wakes"] > before and snapshot["input_window_open"] and snapshot
        snapshot = await wait("actual Firmware KWS opens input window", woke)
        command(index, "capture silence")
        last_block = snapshot["mic"]["blocks_seen"]
        async def quiet():
            snapshot = await mode(index)
            return snapshot["mic"]["blocks_seen"] >= last_block + 10 and not snapshot["mic"]["in_speech"]
        await wait("distinct wake utterance ends before question", quiet)
        require(len(providers[index].snapshot()["asr"]) == before_asr, "wake preamble unexpectedly reached ASR")
        return snapshot

    async def success(case):
        provider = providers[0]
        inline = (
            (args.case == "inline-wake" and case == "normal_voice")
            or (args.case == "closed-reconnect" and case == "reconnected_voice")
        )
        if not inline:
            report.setdefault("wake_preambles", []).append(await prepare_wake(0))
        provider.enqueue(case, f"请告诉我系统测试结果，编号{case}")
        before_audio = probe(0).get("nonzero_playback_calls", 0)
        before_tasks = {row["id"] for row in (await request(cores[0], "GET", "/api/v0/tasks"))["tasks"]}
        await spoken_segment(0, inline)
        async def completed():
            rows = (await request(cores[0], "GET", "/api/v0/tasks"))["tasks"]
            fresh = [row for row in rows if row["id"] not in before_tasks]
            if len(fresh) == 1 and fresh[0]["status"] == "completed" and probe(0).get("nonzero_playback_calls", 0) > before_audio:
                snapshot = await mode(0)
                if not snapshot["outputs"] and snapshot.get("current_task") is None:
                    return {"task": fresh[0], "mode": snapshot, "probe": probe(0), "events": await events(0)}
        observed = await wait(f"{case} actual task, terminal and nonzero DAC", completed, 20)
        report["checks"].append(case)
        observed["wake_mode"] = "inline" if inline else "separate"
        observed["conversation"] = await request(cores[0], "GET", "/api/v0/devices/fixture-0/conversation")
        report[case] = observed
        return observed

    try:
        ports = reserve_ports(6)
        for index in range(3):
            provider = VoicePartners(f"fixture-{index}").start()
            providers.append(provider)
            core = Core(args.core.resolve(), root, f"core-{index}", ports[index*2], ports[index*2+1], deadline)
            cores.append(core)
            await loop.run_in_executor(pool, core.start)
            core.token = (await request(core, "POST", "/api/v0/auth/provision"))["token"]
            for service, body in [("llm", provider.llm_config_body),
                ("asr", {"provider": "openai-asr", "base_url": provider.llm_base, "model": "voice-fixture-asr", "api_key_env": ""}),
                ("tts", {"provider": "omni", "base_url": provider.llm_base, "model": "voice-fixture-tts", "voice": "synthetic", "api_key_env": ""})]:
                await request(core, "PUT", f"/api/v0/config/services/{service}", body)
            proxy = VoiceProxy(core.gateway_port)
            await proxy.start()
            proxies.append(proxy)
            state = root / f"gateway-{index}"
            kws = state / "kws"
            kws.mkdir(parents=True)
            sidecar = args.gateway_source / "tools/luna_voice_sidecar.py"
            (kws / "run.sh").write_text(f"#!{sys.executable}\nimport os\nos.execv({sys.executable!r}, [{sys.executable!r}, {str(sidecar.resolve())!r}])\n")
            (kws / "run.sh").chmod(0o700)
            metric = state / "metrics.jsonl"
            log = (state / "gateway.log").open("wb")
            logs.append(log)
            child = subprocess.Popen([str(args.gateway_fixture.resolve()), "--core", f"127.0.0.1:{proxy.port}",
                "--device", f"fixture-{index}", "--state-dir", str(state), "--probe-file", str(metric), "--kws-dir", str(kws)],
                stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT, text=True, env={"PATH": os.defpath}, cwd=state, start_new_session=True)
            gateways.append(child)
            command(index, "capture silence")
            async def ready():
                status = await request(core, "GET", "/api/v0/status")
                return status["gateway"]["connected"] and metric.is_file() and bool((await mode(index)).get("mic"))
            await wait(f"node{index} actual Firmware handshake", ready)
            probes.append(metric.open())
            await request(core, "PUT", f"/api/v0/mode/fixture-{index}", {
                "asr_wake_fallback": False, "wake_ack": {"enabled": False, "phrases": ["我在"]},
                "filler": {"thinking": [], "search": [], "device": [], "story": []},
                "follow_window_tone": False})
            report["nodes"].append({"core_pid": core.proc.pid, "gateway_pid": child.pid, "data": str(core.data),
                                    "initial_generation": (await request(core, "GET", "/api/v0/status"))["gateway"]["generation"]})
        report["checks"].append("three actual mains and Firmware.run/fake Hardware, isolated DBs and loopback")
        # All nodes receive real nonzero PCM, but none has been invited.
        await asyncio.gather(*(spoken_segment(index, False) for index in range(3)))
        for index in range(3):
            async def skipped(i=index):
                snapshot = await mode(i)
                return snapshot["mic"]["counters"].get("asr_skipped", 0) >= 1 and snapshot
            snapshot = await wait(f"node{index} unwoken ASR rejected at consumer", skipped)
            require(not providers[index].snapshot()["asr"], f"node{index}: unwoken PCM reached actual ASR HTTP")
            require(probe(index).get("nonzero_playback_calls", 0) == 0, "unwoken input caused new DAC audio")
            report["nodes"][index]["unwoken_mode"] = snapshot
        report["checks"].append("three real nonzero PCM segments; zero external ASR and zero nonzero DAC while unwoken")
        await success("normal_voice")
        provider = providers[0]
        await prepare_wake(0)
        release = provider.enqueue("late_cancelled_asr", "这段迟到结果不能开启新回合", blocked=True)
        before_requests = len(provider.snapshot()["asr"])
        await spoken_segment(0, False)
        async def entered():
            return len(provider.snapshot()["asr"]) == before_requests + 1
        await wait("actual HTTP ASR held at barrier", entered)
        before_llm = provider.metrics()["counts"].get("llm_request", 0)
        before_tts = len(provider.snapshot()["tts"])
        before_dac = probe(0).get("nonzero_playback_calls", 0)
        before_task_ids = {row["id"] for row in (await request(cores[0], "GET", "/api/v0/tasks"))["tasks"]}
        proxies[0].disconnect()
        async def offline():
            status = await request(cores[0], "GET", "/api/v0/status")
            return not status["gateway"]["connected"] and status
        report["disconnected_status"] = await wait("actual Core sees disconnected Firmware producer", offline)
        async def cancelled_while_held():
            snapshot = await mode(0)
            held = provider.snapshot()["asr"][-1]
            require(not held["released"], "ASR response released before cancellation was observed")
            return (snapshot["input"]["state"] == "cancelled"
                    and snapshot["mic"]["counters"].get("asr_inflight", 0) == 0
                    and {"mode": snapshot, "asr": held})
        report["cancelled_before_asr_release"] = await wait("disconnected input cancelled while HTTP response remains held", cancelled_while_held)
        release.set()
        async def rejected():
            snapshot = await mode(0)
            requests = provider.snapshot()["asr"]
            return (requests[-1]["released"] and snapshot["input"]["state"] == "cancelled"
                    and snapshot["mic"]["counters"].get("asr_inflight", 0) == 0 and snapshot)
        report["late_asr_cancelled_mode"] = await wait("actual consumer cancels ASR worker and rejects released response", rejected)
        report["late_asr_events"] = await events(0)
        require(provider.metrics()["counts"].get("llm_request", 0) == before_llm, "late ASR created LLM request")
        require(len(provider.snapshot()["tts"]) == before_tts, "late ASR created TTS request")
        require(probe(0).get("nonzero_playback_calls", 0) == before_dac, "late ASR created new nonzero DAC audio")
        require({row["id"] for row in (await request(cores[0], "GET", "/api/v0/tasks"))["tasks"]} == before_task_ids,
                "late ASR created a task")
        report["late_asr_zero_deltas"] = {"llm": provider.metrics()["counts"].get("llm_request", 0) - before_llm,
            "tts": len(provider.snapshot()["tts"]) - before_tts,
            "nonzero_dac": probe(0).get("nonzero_playback_calls", 0) - before_dac,
            "new_tasks": len({row["id"] for row in (await request(cores[0], "GET", "/api/v0/tasks"))["tasks"]} - before_task_ids)}
        report["checks"].append("real TCP disconnect without explicit close; late HTTP ASR produces zero new tasks/LLM/TTS/DAC")
        negative = {key: report[key] for key in ("disconnected_status", "cancelled_before_asr_release", "late_asr_cancelled_mode", "late_asr_zero_deltas")}
        (root / "disconnect-negative.json").write_text(json.dumps(negative, ensure_ascii=False, indent=2))
        proxies[0].reject_new = False
        async def reconnected():
            status = await request(cores[0], "GET", "/api/v0/status")
            return (status["gateway"]["connected"] and status["gateway"]["generation"] > report["nodes"][0]["initial_generation"] and status)
        report["reconnected_status"] = await wait("fresh Firmware connection generation", reconnected, 15)
        if args.case == "closed-reconnect":
            before_events = await events(0)
            before_play_finished = len([row for row in before_events if row.get("kind") == "play_finished"])
            before_session_closed = len([row for row in before_events if row.get("kind") == "session_closed"])
            before_ack_audio = probe(0).get("nonzero_playback_calls", 0)
            close_resp = await request(cores[0], "POST", "/api/v0/session/close", {"device": "fixture-0"})
            require(close_resp.get("closed") is True, f"explicit session close did not return closed: {close_resp}")
            async def ack_finished():
                ev = await events(0)
                closed_ev = [row for row in ev if row.get("kind") == "session_closed"]
                finished_ev = [row for row in ev if row.get("kind") == "play_finished"]
                snapshot = await mode(0)
                if (len(closed_ev) > before_session_closed and len(finished_ev) > before_play_finished
                        and probe(0).get("nonzero_playback_calls", 0) > before_ack_audio
                        and not snapshot["outputs"] and not snapshot["input_window_open"]):
                    return {"play_finished": finished_ev[-1], "mode": snapshot}
            ack_done = await wait("explicit session close retreat ack playback completion", ack_finished, 15)
            top_mode = await request(cores[0], "GET", "/api/v0/mode")
            tail_blind_ms = int(top_mode["tail_blind_ms"])
            await asyncio.sleep((tail_blind_ms + 50) / 1000.0)
            after_tail = await mode(0)
            require(not after_tail["outputs"] and not after_tail["mic"]["in_speech"] and not after_tail["input_window_open"],
                    "device not idle after retreat ack echo-tail wait")
            report["closed_reconnect_ack"] = {
                "close_response": close_resp,
                "play_finished": ack_done["play_finished"],
                "tail_blind_ms": tail_blind_ms,
                "mode_after_tail": after_tail,
            }
            report["checks"].append("explicit session close, retreat ack playback completion, and echo-tail wait")
        await success("reconnected_voice")
        for index in (1, 2):
            require(not providers[index].snapshot()["asr"], "other unwoken node transcribed another node's wake")
            require(probe(index).get("nonzero_playback_calls", 0) == 0, "other unwoken node produced audio")
            report["nodes"][index]["final_mode"] = await mode(index)
        for index in range(3):
            report["nodes"][index]["final_probe"] = probe(index)
        if args.case == "inline-wake":
            report["same_utterance_wake"]["status"] = "covered"
        if args.case == "closed-reconnect":
            report["closed_reconnect_same_utterance"]["status"] = "covered"
        report["coverage"]["P01"] = {"status": "partial", "same_utterance_wake": report["same_utterance_wake"]["status"], "covered": "PCM/VAD/HTTP ASR/Agent LLM/HTTP TTS/real Firmware nonzero DAC and terminal"}
        report["status"] = "passed"
    except Exception as error:
        if args.case == "inline-wake":
            report["same_utterance_wake"]["status"] = "failed"
        if args.case == "closed-reconnect":
            report["closed_reconnect_same_utterance"]["status"] = "failed"
        report["failures"].append(str(error))
        report["failure_observations"] = []
        for index, core in enumerate(cores):
            try:
                report["failure_observations"].append({"index": index, "mode": await mode(index),
                    "events": await events(index), "tasks": await request(core, "GET", "/api/v0/tasks")})
            except Exception as observation_error:
                report["failure_observations"].append({"index": index, "error": str(observation_error)})
    finally:
        for provider in providers:
            for gate in provider.release_all:
                gate.set()
        for child in gateways:
            if child.poll() is None:
                try:
                    child.stdin.write("shutdown\n")
                    child.stdin.flush()
                    await loop.run_in_executor(pool, lambda c=child: c.wait(timeout=3))
                except (OSError, subprocess.TimeoutExpired):
                    child.terminate()
                    try:
                        await loop.run_in_executor(pool, lambda c=child: c.wait(timeout=3))
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
        await asyncio.gather(*(proxy.close() for proxy in proxies))
        report["core_clean_exit"] = [await loop.run_in_executor(pool, core.stop) for core in cores]
        report["gateway_exit_codes"] = [child.returncode for child in gateways]
        report["sidecar_cleanup"] = []
        for child in gateways:
            def group_alive():
                try:
                    os.killpg(child.pid, 0)
                    return True
                except ProcessLookupError:
                    return False
            until = time.monotonic() + 3
            while group_alive() and time.monotonic() < until:
                await asyncio.sleep(.02)
            natural = not group_alive()
            if not natural:
                os.killpg(child.pid, signal.SIGKILL)
                until = time.monotonic() + 3
                while group_alive() and time.monotonic() < until:
                    await asyncio.sleep(.02)
            report["sidecar_cleanup"].append({"group_exited_after_eof": natural,
                "forced_group_kill": not natural, "group_empty": not group_alive()})
        report["partners"] = [{"http": provider.metrics(), "voice": provider.snapshot()} for provider in providers]
        for provider in providers:
            await loop.run_in_executor(pool, provider.close)
        for handle in probes + logs:
            handle.close()
        pool.shutdown(wait=True, cancel_futures=True)
        if (not all(report["core_clean_exit"]) or any(code != 0 for code in report["gateway_exit_codes"])
                or not all(row["group_empty"] for row in report["sidecar_cleanup"])):
            report["status"] = "failed"
            report["failures"].append("child process failed bounded cleanup")
        report["sources_unchanged"] = all(source_manifest(Path(value["root"]))["manifest_sha256"] == value["manifest_sha256"]
                                          for value in report["sources"].values())
        if report["status"] == "passed":
            try:
                validate(report)
            except (AssertionError, KeyError, TypeError) as error:
                report["status"] = "failed"
                report["failures"].append(f"evidence validation: {error}")
        (root / "result.json").write_text(json.dumps(report, indent=2, ensure_ascii=False))
    print(json.dumps({"status": report["status"], "result": str(root / "result.json"), "failures": report["failures"]}))
    return int(report["status"] != "passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, required=True)
    parser.add_argument("--gateway-fixture", type=Path, required=True)
    parser.add_argument("--gateway-source", type=Path, required=True)
    parser.add_argument("--core-source", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--case", choices=("separate-wake", "inline-wake", "closed-reconnect"), default="separate-wake")
    args = parser.parse_args()
    require(30 <= args.timeout <= 180, "timeout must be30..180 seconds")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    raise SystemExit(asyncio.run(run(args)))
