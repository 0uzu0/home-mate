#!/usr/bin/env python3
"""Exercise actual Core mains and Gateway Firmware/fake Hardware over bounded TCP.

The proxy forwards bytes without generating protocol frames. A fault pauses only
an existing uplink; reconnects use a fresh tunnel. Inputs are synthetic, children
use isolated data, and no inherited HM_* settings or household credentials enter
the fixture. A short development run is never a ten-minute acceptance result.
"""
import argparse
import asyncio
import concurrent.futures
import collections
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import socket
import struct
import subprocess
import tempfile
import time
import wave

from system_smoke import Core, require, reserve_ports
from luna_transport_partners import TransportPartners


class Tunnel:
    def __init__(self, number):
        self.number = number
        self.gate = asyncio.Event()
        self.gate.set()
        self.writers = []
        self.forwarded_up = 0
        self.forwarded_down = 0
        self.paused_at = None
        self.closed = False


class FaultProxy:
    """One bounded stream buffer per direction, no protocol reconstruction."""
    def __init__(self, target_port, sustained=False):
        self.target_port = target_port
        self.server = None
        self.tunnels = {}
        self.total_connections = 0
        self.closed_connections = 0
        self.tasks = set()
        self.events = []
        self.sustained = sustained

    async def start(self):
        sock = socket.socket()
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        sock.bind(("127.0.0.1", 0))
        sock.listen(16)
        sock.setblocking(False)
        self.port = sock.getsockname()[1]
        self.server = await asyncio.start_server(self.accept, sock=sock, limit=8192)

    async def accept(self, gateway_reader, gateway_writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        self.total_connections += 1
        tunnel = Tunnel(self.total_connections)
        self.tunnels[tunnel.number] = tunnel
        pumps = []
        try:
            core_reader, core_writer = await asyncio.open_connection(
                "127.0.0.1", self.target_port, limit=8192)
            tunnel.writers = [gateway_writer, core_writer]
            for writer in tunnel.writers:
                writer.transport.set_write_buffer_limits(high=8192, low=4096)

            async def pump(reader, writer, uplink):
                while True:
                    if uplink:
                        await tunnel.gate.wait()
                    data = await reader.read(4096)
                    if not data:
                        return
                    if uplink:
                        await tunnel.gate.wait()
                    writer.write(data)
                    await writer.drain()
                    if uplink:
                        tunnel.forwarded_up += len(data)
                        if self.sustained and tunnel.forwarded_up >= 262144 and tunnel.paused_at is None:
                            tunnel.gate.clear()
                            tunnel.paused_at = time.monotonic()
                            self.events.append({"connection": tunnel.number, "pause_seconds": None,
                                "monotonic": tunnel.paused_at, "forwarded_up": tunnel.forwarded_up})
                    else:
                        tunnel.forwarded_down += len(data)

            pumps = [asyncio.create_task(pump(gateway_reader, core_writer, True)),
                     asyncio.create_task(pump(core_reader, gateway_writer, False))]
            done, _ = await asyncio.wait(pumps, return_when=asyncio.FIRST_COMPLETED)
            for item in done:
                item.result()
        except (ConnectionError, OSError, asyncio.CancelledError):
            pass
        finally:
            for item in pumps:
                item.cancel()
            if pumps:
                await asyncio.gather(*pumps, return_exceptions=True)
            for writer in tunnel.writers or [gateway_writer]:
                writer.close()
            tunnel.closed = True
            self.closed_connections += 1
            self.tunnels.pop(tunnel.number, None)
            self.tasks.discard(task)

    def pause_current(self, seconds):
        candidates = [t for t in self.tunnels.values() if t.gate.is_set()]
        if not candidates:
            return None
        tunnel = max(candidates, key=lambda t: t.number)
        tunnel.gate.clear()
        tunnel.paused_at = time.monotonic()
        asyncio.get_running_loop().call_later(seconds, tunnel.gate.set)
        event = {"connection": tunnel.number, "pause_seconds": seconds,
                 "monotonic": tunnel.paused_at, "forwarded_up": tunnel.forwarded_up}
        self.events.append(event)
        return event

    async def close(self):
        if self.server:
            self.server.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*list(self.tasks), return_exceptions=True)
        if self.server:
            await self.server.wait_closed()


def resource_sample(pid):
    result = subprocess.run(["ps", "-p", str(pid), "-o", "rss=,%cpu=,time="],
                            capture_output=True, text=True, check=False)
    fields = result.stdout.split()
    if len(fields) != 3:
        return {"pid": pid, "alive": False}
    threads = subprocess.run(["ps", "-M" if platform.system() == "Darwin" else "-L",
                              "-p", str(pid), "-o", "pid="], capture_output=True, text=True)
    return {"pid": pid, "alive": True, "rss_kib": int(fields[0]),
            "cpu_percent": float(fields[1]), "cpu_time": fields[2],
            "os_threads": len(threads.stdout.splitlines()) if threads.returncode == 0 else None}


def source_manifest(root):
    """Hash actual tracked/unignored build inputs, including new dirty source."""
    names = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=root)
    hashes = {}
    for name in sorted(set(names.decode().split("\0"))):
        if not name or not (name.startswith(("crates/", "web/src/", "tools/", "docs/fixtures/", ".cargo/"))
                            or name in ("Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain.toml")):
            continue
        path = root / name
        hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else "deleted"
    return {"root": str(root.resolve()), "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
            "files": hashes, "manifest_sha256": hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest()}


def make_asset(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    rate = 16000
    pcm = b"".join(struct.pack("<h", int(1800 * math.sin(i * 2 * math.pi * 440 / rate)))
                   for i in range(rate * 4))
    with wave.open(str(path), "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(rate)
        out.writeframes(pcm)


def latest_probe(file, previous, on_record=None):
    while True:
        position = file.tell()
        line = file.readline(16385)
        if not line:
            break
        require(len(line) <= 16384, "oversized fixture metrics record")
        if not line.endswith("\n"):
            file.seek(position)
            break
        if line.strip():
            previous = json.loads(line)
            if on_record:
                on_record(previous)
    return previous


class ProbeClock:
    def __init__(self):
        self.elapsed_ms = None
        self.changed_at = None
        self.unique = 0

    def accept(self, probe, now):
        elapsed = probe.get("elapsed_ms")
        require(isinstance(elapsed, int) and elapsed >= 0, "invalid probe clock")
        require(self.elapsed_ms is None or elapsed >= self.elapsed_ms, "probe clock moved backwards")
        if elapsed != self.elapsed_ms:
            self.elapsed_ms, self.changed_at = elapsed, now
            self.unique += 1

    def check_fresh(self, now):
        require(self.changed_at is not None and now - self.changed_at <= 3,
                "fixture observer clock did not advance within3s")


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def cpu_seconds(value):
    days, value = value.split("-", 1) if "-" in value else ("0", value)
    total = 0.
    for part in value.split(":"):
        total = total * 60 + float(part)
    return int(days) * 86400 + total


def summarize_samples(path):
    values = {}
    with path.open() as source:
        for line in source:
            row = json.loads(line)
            for kind in ("core", "gateway"):
                sample = row[kind]
                key = f"{kind}-{row['node']}"
                entry = values.setdefault(key, {"rss": [], "cpu": [], "threads": [], "elapsed": [],
                    "cpu_seconds": [], "cpu_time_first": sample.get("cpu_time")})
                entry["rss"].append(sample.get("rss_kib", 0))
                entry["cpu"].append(sample.get("cpu_percent", 0))
                entry["threads"].append(sample.get("os_threads"))
                entry["elapsed"].append(row["elapsed_s"])
                entry["cpu_seconds"].append(cpu_seconds(sample["cpu_time"]))
                entry["cpu_time_last"] = sample.get("cpu_time")
    result = {}
    for key, entry in values.items():
        n = len(entry["rss"])
        intervals = [(entry["cpu_seconds"][i] - entry["cpu_seconds"][i-1]) * 100 /
                     (entry["elapsed"][i] - entry["elapsed"][i-1]) for i in range(1, n)
                     if entry["elapsed"][i] > entry["elapsed"][i-1]]
        early = [rss for rss, elapsed in zip(entry["rss"], entry["elapsed"]) if 30 <= elapsed <= 180]
        late = [rss for rss, elapsed in zip(entry["rss"], entry["elapsed"]) if elapsed >= entry["elapsed"][-1] - 150]
        early_median, late_median = percentile(early, .5), percentile(late, .5)
        result[key] = {"sample_count": n, "peak_rss_kib": max(entry["rss"], default=0),
            "first_rss_kib": entry["rss"][0], "last_rss_kib": entry["rss"][-1],
            "last_quarter_rss_p50_kib": percentile(entry["rss"][n*3//4:], .5),
            "last_quarter_rss_p95_kib": percentile(entry["rss"][n*3//4:], .95),
            "cpu_percent_p50": percentile(entry["cpu"], .5),
            "cpu_percent_p95": percentile(entry["cpu"], .95),
            "interval_cpu_percent_p50": percentile(intervals, .5),
            "interval_cpu_percent_p95": percentile(intervals, .95),
            "os_threads_first": entry["threads"][0], "os_threads_last": entry["threads"][-1],
            "os_threads_peak": max((x for x in entry["threads"] if x is not None), default=None),
            "early_rss_p50_kib": early_median, "last150s_rss_p50_kib": late_median,
            "rss_growth_requires_investigation": bool(early_median and late_median - early_median > max(2048, early_median * .05)),
            "cpu_time_first": entry["cpu_time_first"], "cpu_time_last": entry["cpu_time_last"]}
    return result


def check_bounds(probe):
    sample = probe.get("northbound", {})
    if not sample:
        return False
    for key, cap in [("critical_current_count", 64), ("critical_high_water_count", 64),
                     ("critical_current_bytes", 262144), ("critical_high_water_bytes", 262144),
                     ("audio_current_count", 25), ("audio_high_water_count", 25)]:
        value = sample.get(key)
        require(isinstance(value, int) and 0 <= value <= cap,
                f"northbound bound failed: {key}={value}, cap={cap}")
    for key, cap in [("pending_health_count", 1), ("pending_presence_count", 1),
                     ("pending_health_bytes", 4096), ("pending_presence_bytes", 4096),
                     ("inflight_bytes", 262144)]:
        value = probe.get(key)
        require(isinstance(value, int) and 0 <= value <= cap,
                f"observer bound failed: {key}={value}, cap={cap}")
    return True


def check_progress(probe, history, enforce_timing):
    if not probe or not enforce_timing:
        return
    for key, cap in [("capture_max_gap_us", 250000), ("playback_max_gap_us", 150000)]:
        require(probe[key] <= cap, f"whole-process cadence tolerance failed: {key}={probe[key]}")
    if "producer_max_us" in probe:
        require(probe["producer_max_us"] <= 20000, f"producer call exceeded20ms: {probe['producer_max_us']}")
        for key, cap in [("reader_tasks", 1), ("writer_tasks", 1), ("decode_active", 1), ("decode_pending", 64)]:
            require(0 <= probe[key] <= cap, f"live task bound failed: {key}={probe[key]}")
        if probe.get("last_fault_generation", 0) and probe["last_cleanup_generation"] == probe["last_fault_generation"]:
            delay = probe["last_cleanup_us"] - probe["last_fault_us"]
            require(0 <= delay <= 1000000, f"fatal-to-cleanup exceeded1000ms: {delay}us")
        elif probe.get("last_fault_generation", 0):
            require(probe["elapsed_ms"] * 1000 - probe["last_fault_us"] <= 1000000,
                    "fatal generation still lacks cleanup after1000ms")
        fault = probe.get("northbound", {}).get("last_fault")
        if fault and fault["code"] == "writer_timeout":
            delay = probe["last_fault_us"] - probe["last_fault_write_started_us"]
            require(1000000 <= delay <= 1100000, f"write timeout outside decision+jitter budget: {delay}us")
    if history and probe["elapsed_ms"] == history[-1]["elapsed_ms"]:
        return
    history.append(probe)
    while len(history) > 1 and probe["elapsed_ms"] - history[1]["elapsed_ms"] >= 10000:
        history.popleft()
    elapsed = (probe["elapsed_ms"] - history[0]["elapsed_ms"]) / 1000
    if elapsed >= 10:
        for key, rate in [("capture_calls", 11.2), ("playback_calls", 12.5)]:
            delta = probe[key] - history[0][key]
            require(delta >= elapsed * rate, f"10s cadence failed {key}: {delta}/{elapsed:.3f}s")


def observe_consumers(node, diagnostics, device_id):
    """Retain real consumer evidence even if a later Hello replaces its slot."""
    gateway = diagnostics["gateway"]
    device = next((d for d in gateway["devices"] if d["stable_id"] == device_id), {})
    if (device.get("echo_health") == "unknown"
            and device.get("echo_health_reason") == "aec_disabled"
            and device.get("echo_degraded_ms") == 0):
        node["health_consumer_seen"] = True
        node.setdefault("health_consumer_snapshot", {
            "generation": gateway["generation"], "device": device.copy(),
            "echo_health_events": diagnostics["gateway_counts"].get("echo_health", 0)})
    if device.get("northbound", {}).get("state") == "available":
        node["northbound_consumer_seen"] = True


async def run(args):
    root = Path(tempfile.mkdtemp(prefix="transport-", dir=args.output_dir.resolve()))
    report = {"status": "failed", "scope": "paired-production-transport",
              "duration_requested_s": args.duration, "host": platform.platform(),
              "machine": platform.machine(), "cpu_count": os.cpu_count(),
              "mode": "baseline" if args.baseline else "candidate",
              "workload": "sustained-nonreader" if args.sustained_nonreader else "mixed",
              "binary_sha256": {}, "nodes": [], "failures": [], "checks": []}
    for name, binary in [("core", args.core), ("gateway_fixture", args.gateway_fixture)]:
        report["binary_sha256"][name] = hashlib.sha256(binary.read_bytes()).hexdigest()
    report["sources"] = {name: source_manifest(path) for name, path in
                         [("core", args.core_source), ("gateway", args.gateway_source)]}
    harness_paths = [Path(__file__).resolve(), Path(__file__).with_name("system_smoke.py"),
                     Path(__file__).with_name("luna_transport_partners.py")]
    report["harness_files"] = {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in harness_paths}
    report["rustc"] = subprocess.check_output(["rustc", "-Vv"], text=True).strip()
    report["tolerances"] = {"warmup_s": args.warmup, "capture_calls_per_10s": 112,
        "mixed_playback_calls_per_10s": 125, "whole_process_capture_max_gap_ms": 250,
        "whole_process_playback_max_gap_ms": 150, "producer_max_ms": 20,
        "write_decision_ms": 1000, "host_detection_jitter_ms": 100, "fatal_cleanup_ms": 1000,
        "sampling_interval_s": 1, "timing_enforced": args.duration >= 600}
    deadline = time.monotonic() + args.duration + args.warmup + 90
    cores, proxies, gateways, logs, probes = [], [], [], [], []
    active_work = set()
    last_probe = [{}, {}, {}]
    probe_clocks = [ProbeClock() for _ in range(3)]
    progress = [collections.deque(maxlen=16) for _ in range(3)]
    measuring = False
    provider = TransportPartners().start()
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=12)
    loop = asyncio.get_running_loop()

    async def request(core, method, path, body=None, **kwargs):
        return await loop.run_in_executor(pool, lambda: core.request(
            method, path, body, request_timeout=25, **kwargs))

    def observe(index):
        def validate(probe):
            probe_clocks[index].accept(probe, time.monotonic())
            if check_bounds(probe) and measuring:
                report["nodes"][index]["bounds_samples"] += 1
            check_progress(probe, progress[index], measuring and args.duration >= 600)
        last_probe[index] = latest_probe(probes[index], last_probe[index], validate)
        return last_probe[index]

    async def chat(core, binding, scenario, index):
        events = await request(core, "POST", "/api/v0/chat", {
            "scope_binding_id": binding, "text": provider.command(scenario, f"fixture-{index}")}, sse=True)
        require(not any(e["type"] == "error" for e in events), f"chat error: {events}")
        require(any(e["type"] == "turn_end" and e["status"] == "completed" for e in events),
                f"chat did not complete: {events}")
        return events

    async def action(core, index, induce_fault, cycle):
        before_audio = observe(index).get("nonzero_playback_calls", 0)
        # Fresh server-issued binding prevents context growth from becoming an
        # accidental workload change; physical-device authority is not forged.
        binding = await request(core, "POST", "/api/v1/chat/scopes", {"device_id": f"fixture-{index}"})
        playing = asyncio.create_task(chat(core, binding["binding_id"], "music_play", index))
        try:
            play_events = await playing
            play_result = [e for e in play_events if e["type"] == "tool_result" and e["tool"] == "music.play"]
            require(len(play_result) == 1 and play_result[0]["status"] == "completed", f"music play failed: {play_events}")
            end = time.monotonic() + 3
            while time.monotonic() < end:
                if observe(index).get("nonzero_playback_calls", 0) > before_audio:
                    break
                await asyncio.sleep(0.05)
            else:
                raise RuntimeError("real asset never reached nonzero Hardware playback")
            if induce_fault:
                require(proxies[index].pause_current(120) is not None,
                        "no existing connection available for nonreader fault")
            started = time.monotonic()
            stopped = await chat(core, binding["binding_id"], "music_pause" if cycle % 2 else "music_stop", index)
            http_elapsed = (time.monotonic() - started) * 1000
            results = [e for e in stopped if e["type"] == "tool_result" and e["tool"] == "music.control"]
            require(len(results) == 1, f"missing actual music control: {stopped}")
            receipt = results[0]
            if not args.baseline and induce_fault:
                require(receipt["evidence"] == "unknown", f"lost receipt falsely confirmed: {receipt}")
            elif not induce_fault:
                require(receipt["status"] == "completed" and receipt["evidence"] == "deviceconfirmed", f"normal stop failed: {receipt}")
            report["nodes"][index]["cancellations"].append({
                "stop_http_ms": http_elapsed,
                "stop_to_terminal_ms": (time.monotonic() - started) * 1000,
                "nonreader_fault": induce_fault,
                "stop_result": receipt, "play_result": play_result[0]})
        finally:
            if not playing.done():
                playing.cancel()

    try:
        ports = reserve_ports(6)
        for i in range(3):
            core = Core(args.core.resolve(), root, f"core-{i}", ports[2*i], ports[2*i+1], deadline)
            cores.append(core)
            make_asset(core.data / "assets/fixture.wav")
            await loop.run_in_executor(pool, core.start)
            core.token = (await request(core, "POST", "/api/v0/auth/provision"))["token"]
            await request(core, "PUT", "/api/v0/config/services/llm", provider.llm_config_body)
            await request(core, "PUT", "/api/v0/config/services/music", provider.music_config_body)
            proxy = FaultProxy(core.gateway_port, args.sustained_nonreader)
            await proxy.start()
            proxies.append(proxy)
            state = root / f"gateway-{i}"
            state.mkdir()
            metric_path = state / "metrics.jsonl"
            log = (root / f"gateway-{i}.log").open("wb")
            logs.append(log)
            child = subprocess.Popen([str(args.gateway_fixture.resolve()),
                "--core", f"127.0.0.1:{proxy.port}", "--state-dir", str(state),
                "--device", f"fixture-{i}", "--probe-file", str(metric_path)],
                stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT,
                env={"PATH": os.defpath}, cwd=state, text=True)
            gateways.append(child)
            metric_deadline = time.monotonic() + 5
            while not metric_path.exists():
                require(child.poll() is None and time.monotonic() < metric_deadline,
                        "Gateway fixture did not create its probe file")
                await asyncio.sleep(0.05)
            probes.append(metric_path.open())
            report["nodes"].append({"index": i, "core_pid": core.proc.pid,
                "gateway_pid": child.pid, "data_dir": str(core.data),
                "cancellations": [], "bounds_samples": 0, "initial_generation": None})
        ready_until = time.monotonic() + 20
        while time.monotonic() < ready_until:
            statuses = await asyncio.gather(*(request(c, "GET", "/api/v0/status") for c in cores))
            if all(s["gateway"]["connected"] for s in statuses):
                break
            require(all(g.poll() is None for g in gateways), "Gateway fixture exited at startup")
            await asyncio.sleep(0.1)
        else:
            raise RuntimeError("three actual Gateway handshakes did not complete")
        for i, status in enumerate(statuses):
            report["nodes"][i]["initial_generation"] = status["gateway"]["generation"]
        report["checks"].append("three actual Core mains and Firmware fixture handshakes")
        presence_until = time.monotonic() + 15
        while time.monotonic() < presence_until:
            for i, core in enumerate(cores):
                # Stable health can be emitted once before the first fault.
                # Observe the real consumer before warmup/reconnect replaces it.
                diag = await request(core, "GET", "/api/v0/diagnostics")
                observe_consumers(report["nodes"][i], diag, f"fixture-{i}")
                if "presence_consumer" in report["nodes"][i]:
                    continue
                observed = await request(core, "POST", "/api/v0/fleet/wake", {
                    "device": "transport-probe", "heard_ms": int(time.time() * 1000),
                    "rms_dbfs": -90, "calibrated": False, "room": "", "word": ""})
                if observed.get("state") == "passive_presence" and observed.get("claim", {}).get("device") == f"fixture-{i}":
                    report["nodes"][i]["presence_consumer"] = observed
            if all("presence_consumer" in node and node.get("health_consumer_seen")
                   for node in report["nodes"]):
                break
            await asyncio.sleep(.2)
        require(all("presence_consumer" in node for node in report["nodes"]),
                "real NearEndPresence did not reach production passive candidate consumer")
        require(all(node.get("health_consumer_seen") for node in report["nodes"]),
                "real EchoHealth did not reach Core before warmup")
        if args.sustained_nonreader:
            await asyncio.gather(*(action(core, i, False, 0) for i, core in enumerate(cores)))
        await asyncio.sleep(args.warmup)
        for i in range(3):
            observe(i)
            probe_clocks[i].unique = 0
        measuring = True
        start = time.monotonic()
        next_sample = start
        last_action = -1
        blocked_until = [0., 0., 0.]
        with (root / "samples.jsonl").open("w") as samples:
            while time.monotonic() - start < args.duration:
                elapsed = time.monotonic() - start
                cycle = int(elapsed // 20)
                if cycle != last_action:
                    last_action = cycle
                    for i, core in enumerate(cores):
                        if args.sustained_nonreader:
                            continue
                        if elapsed < blocked_until[i]:
                            continue
                        fault = not args.sustained_nonreader and cycle % 9 == 1 and i == (cycle // 9) % 3
                        if fault:
                            # Fixed schedule on both versions: do not issue a
                            # new play while its receipts are deliberately held.
                            blocked_until[i] = elapsed + 140
                        work = asyncio.create_task(action(core, i, fault, cycle))
                        active_work.add(work)
                for work in list(active_work):
                    if work.done():
                        work.result()
                        active_work.remove(work)
                for i, (core, gateway, probe_file) in enumerate(zip(cores, gateways, probes)):
                    require(core.proc.poll() is None and gateway.poll() is None, "fixture process exited")
                    observe(i)
                    probe_clocks[i].check_fresh(time.monotonic())
                    diag = await request(core, "GET", "/api/v0/diagnostics")
                    observe_consumers(report["nodes"][i], diag, f"fixture-{i}")
                    tasks = await request(core, "GET", "/api/v0/tasks")
                    task_counts = dict(collections.Counter(t["status"] for t in tasks["tasks"]))
                    require(task_counts.get("running", 0) <= 1, "sequential node workload accumulated running application tasks")
                    if not args.baseline and args.duration >= 600:
                        transport = diag["gateway"].get("transport_tasks", {})
                        for field in ("connection_tasks", "reader_futures", "writer_tasks"):
                            require(isinstance(transport.get(field), int) and 0 <= transport[field] <= 2,
                                    f"actual Core transport lifetime count outside current+retiring bound: {transport}")
                    require(len(proxies[i].tunnels) <= 2 and len(proxies[i].tasks) <= 2,
                            "fault proxy retained obsolete connections/tasks")
                    record = {"elapsed_s": elapsed, "node": i,
                        "core": resource_sample(core.proc.pid), "gateway": resource_sample(gateway.pid),
                        "probe": last_probe[i], "diagnostics": diag["gateway"],
                        "voice_devices": diag["devices"], "gateway_counts": diag["gateway_counts"],
                        "application_tasks": task_counts, "cycle_phase_s": elapsed % 20,
                        "proxy_connections": proxies[i].total_connections,
                        "proxy_live_tunnels": len(proxies[i].tunnels)}
                    samples.write(json.dumps(record, ensure_ascii=False) + "\n")
                    samples.flush()
                next_sample += 1
                await asyncio.sleep(max(0, next_sample - time.monotonic()))
        await asyncio.gather(*active_work)
        active_work.clear()
        report["elapsed_s"] = time.monotonic() - start
        for i, core in enumerate(cores):
            final = await request(core, "GET", "/api/v0/diagnostics")
            node = report["nodes"][i]
            node["final_gateway"] = final["gateway"]
            node["final_probe"] = last_probe[i]
            node["unique_probe_samples"] = probe_clocks[i].unique
            if args.duration >= 600:
                require(probe_clocks[i].unique >= args.duration / 1.2, "insufficient unique observer samples")
            node["proxy_faults"] = proxies[i].events
            node["proxy_connections"] = proxies[i].total_connections
            require(node["cancellations"], "no actual play/stop activity")
            require(last_probe[i].get("capture_calls", 0) > 0, "no paced capture Hardware calls")
            require(last_probe[i].get("playback_calls", 0) > 0, "no playback Hardware calls")
            require(last_probe[i].get("nonzero_playback_calls", 0) > 0, "no actual nonzero music playback")
            device = next((d for d in final["gateway"]["devices"] if d["stable_id"] == f"fixture-{i}"), {})
            require(node.get("health_consumer_seen"), "real EchoHealth never reached Core snapshot")
            if not args.baseline:
                require(node.get("northbound_consumer_seen"), "real DeviceStatus diagnostics never reached Core")
            timings = [item["stop_to_terminal_ms"] for item in node["cancellations"]]
            node["stop_to_terminal_ms"] = {"p50": percentile(timings, .5),
                "p95": percentile(timings, .95), "max": max(timings)}
            if not args.baseline:
                require(node["bounds_samples"] > 0, "no production northbound observer data")
                if node["proxy_faults"]:
                    require(final["gateway"]["generation"] > node["initial_generation"],
                            "fault did not reach actual Core reconnect generation")
                    require(last_probe[i]["northbound"]["writer_timeouts"] > 0,
                            "proxy pause never produced observed socket write timeout")
        report["ten_minute_evidence"] = args.duration >= 600 and report["elapsed_s"] >= 600
        report["resources"] = summarize_samples(root / "samples.jsonl")
        report["status"] = "passed"
    except Exception as error:
        report["failures"].append(str(error))
    finally:
        for work in active_work:
            work.cancel()
        await asyncio.gather(*active_work, return_exceptions=True)
        for gateway in gateways:
            if gateway.poll() is None:
                try:
                    gateway.stdin.write("shutdown\n")
                    gateway.stdin.flush()
                    await loop.run_in_executor(pool, lambda g=gateway: g.wait(timeout=3))
                except (OSError, subprocess.TimeoutExpired):
                    gateway.terminate()
                    try:
                        await loop.run_in_executor(pool, lambda g=gateway: g.wait(timeout=3))
                    except subprocess.TimeoutExpired:
                        gateway.kill()
                        gateway.wait()
        await asyncio.gather(*(p.close() for p in proxies))
        report["core_clean_exit"] = [await loop.run_in_executor(pool, c.stop) for c in cores]
        for file in probes + logs:
            file.close()
        report["partners"] = provider.metrics()
        await loop.run_in_executor(pool, provider.close)
        pool.shutdown(wait=True, cancel_futures=True)
        report["gateway_exit_codes"] = [g.returncode for g in gateways]
        report["sources_unchanged"] = all(source_manifest(Path(value["root"]))["manifest_sha256"] == value["manifest_sha256"]
                                          for value in report["sources"].values())
        report["sources_unchanged"] &= all(hashlib.sha256(Path(path).read_bytes()).hexdigest() == sha
                                           for path, sha in report["harness_files"].items())
        if args.duration >= 600 and not report["sources_unchanged"]:
            report["status"] = "failed"
            report["failures"].append("source inputs changed during controlled run")
        if not all(report["core_clean_exit"]):
            report["status"] = "failed"
            report["failures"].append("Core failed clean shutdown")
        if any(code != 0 for code in report["gateway_exit_codes"]):
            report["status"] = "failed"
            report["failures"].append("Gateway fixture failed bounded child shutdown")
        (root / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({"status": report["status"], "report": str(root / "report.json"),
                      "failures": report["failures"]}, ensure_ascii=False), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, required=True)
    parser.add_argument("--gateway-fixture", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--duration", type=float, default=600)
    parser.add_argument("--warmup", type=float, default=30)
    parser.add_argument("--core-source", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--gateway-source", type=Path, required=True)
    parser.add_argument("--baseline", action="store_true")
    parser.add_argument("--sustained-nonreader", action="store_true")
    options = parser.parse_args()
    require(1 <= options.duration <= 1800, "duration must be1..1800 seconds")
    require(not (options.sustained_nonreader and options.baseline), "baseline comparison uses mixed workload")
    require(options.core.is_file() and options.gateway_fixture.is_file(), "build both real binaries first")
    options.output_dir.mkdir(parents=True, exist_ok=True)
    raise SystemExit(asyncio.run(run(options)))
