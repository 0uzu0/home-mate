#!/usr/bin/env python3
"""Start three real Core mains with isolated data and an external HTTP LLM.

This is the first Luna system-test slice: startup, management, headless text,
storage isolation and restart. It does not exercise voice, arbitration, Gateway
DSP or LAN authorization enforcement (loopback is trusted by Core).
Only Python's standard library and a previously built hm-core are required.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request


class SmokeFailure(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise SmokeFailure(message)


class FakeLlm(http.server.BaseHTTPRequestHandler):
    """A protocol partner, reached through production DeepSeekClient HTTP."""

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        size = int(self.headers.get("Content-Length", "0"))
        if size > 1_000_000:
            self.send_error(413)
            return
        body = json.loads(self.rfile.read(size))
        with self.server.request_lock:
            self.server.requests.append(body)
        answer = f"Luna smoke response for {body['model']}."
        chunks = [
            {"choices": [{"index": 0, "delta": {"content": answer}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        data = "".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks)
        payload = (data + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


def reserve_ports(count):
    reservations = []
    try:
        for _ in range(count):
            sock = socket.socket()
            reservations.append(sock)
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in reservations]
    finally:
        for sock in reservations:
            sock.close()


class Core:
    def __init__(self, binary, root, name, api_port, gateway_port, deadline):
        self.name = name
        self.data = root / name
        self.data.mkdir()
        self.log_path = root / f"{name}.log"
        self.binary = binary
        self.api_port = api_port
        self.gateway_port = gateway_port
        self.deadline = deadline
        self.token = None
        self.session_id = None
        self.proc = None
        self.log = None

    def request(self, method, path, body=None, expected=200, sse=False, request_timeout=5):
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, "system smoke deadline expired")
        headers = {"Content-Type": "application/json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(
            f"http://127.0.0.1:{self.api_port}{path}",
            data=json.dumps(body).encode() if body is not None else None,
            headers=headers, method=method,
        )
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            response = opener.open(request, timeout=min(request_timeout, remaining))
        except urllib.error.HTTPError as exc:
            response = exc
        with response:
            raw = response.read(2_000_001)
            require(len(raw) <= 2_000_000, f"{self.name}: oversized response on {path}")
            require(response.status == expected,
                    f"{self.name} {method} {path}: expected {expected}, got {response.status}")
        if sse:
            return [json.loads(line[5:].strip()) for line in raw.decode().splitlines()
                    if line.startswith("data:")]
        return json.loads(raw)

    def start(self):
        # No inherited cloud credentials, production HM_* settings or env.local.
        env = {
            "PATH": os.defpath,
            "HM_DATA_DIR": str(self.data),
            "HM_NODE_ID": self.name,
            "HM_BIND": f"127.0.0.1:{self.api_port}",
            "HM_GATEWAY_BIND": f"127.0.0.1:{self.gateway_port}",
            "HM_WEB_DIR": str(self.data / "no-console"),
            "RUST_LOG": "hm_core=info",
        }
        self.log = self.log_path.open("ab")
        self.proc = subprocess.Popen(
            [str(self.binary)], cwd=self.data, env=env,
            stdout=self.log, stderr=subprocess.STDOUT,
        )
        startup_deadline = min(self.deadline, time.monotonic() + 15)
        last_error = None
        while time.monotonic() < startup_deadline:
            require(self.proc.poll() is None, f"{self.name} exited during startup")
            try:
                return self.request("GET", "/api/v0/status")
            except (OSError, SmokeFailure) as exc:
                last_error = str(exc)
                time.sleep(0.05)
        raise SmokeFailure(f"{self.name} startup timeout: {last_error}")

    def stop(self):
        graceful = True
        if self.proc is not None and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                graceful = False
                self.proc.kill()
                self.proc.wait(timeout=3)
        if self.log is not None:
            self.log.close()
            self.log = None
        return graceful and self.proc is not None and self.proc.returncode == 0


def chat(core, text):
    events = core.request("POST", "/api/v0/chat", {
        "session_id": core.session_id, "text": text, "source": "text",
    }, sse=True)
    require(not any(e["type"] == "error" for e in events), f"{core.name}: chat error")
    sessions = [e["session_id"] for e in events if e["type"] == "session"]
    require(len(sessions) == 1, f"{core.name}: missing authoritative session identity")
    require(core.session_id is None or sessions[0] == core.session_id,
            f"{core.name}: request silently switched sessions")
    core.session_id = sessions[0]
    answers = [e["text"] for e in events if e["type"] == "answer"]
    require(answers == [f"Luna smoke response for {core.name}."],
            f"{core.name}: missing external-provider answer: {events}")
    require(any(e["type"] == "turn_end" and e["status"] == "completed" for e in events),
            f"{core.name}: no completed turn")
    require(not any(e["type"] in ("tool_start", "tool_result") for e in events),
            f"{core.name}: unexpected device/tool execution")
    return events


def run(args):
    binary = args.core.resolve()
    require(binary.is_file(), f"build hm-core first: {binary}")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="run-", dir=args.output_dir.resolve()))
    deadline = time.monotonic() + args.timeout
    cores = []
    server = None
    thread = None
    report = {"status": "failed", "scope": "real-main-headless-foundation", "checks": []}
    def expired(_signum, _frame):
        raise SmokeFailure("system smoke deadline expired")
    previous_alarm = signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, args.timeout)
    try:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeLlm)
        server.requests = []
        server.request_lock = threading.Lock()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        ports = reserve_ports(6)
        fake_url = f"http://127.0.0.1:{server.server_port}/v1"
        for i, name in enumerate(("luna-a", "luna-b", "luna-c")):
            core = Core(binary, root, name, ports[i * 2], ports[i * 2 + 1], deadline)
            cores.append(core)
            status = core.start()
            require(status["product"] == "Home Mate", f"{name}: wrong startup product")
            require(Path(status["data_dir"]) == core.data, f"{name}: wrong data directory")
            require(status["gateway"]["devices"] == [] and not status["gateway"]["connected"],
                    f"{name}: unexpected Gateway")
            require(core.data.joinpath("home-mate.db").is_file(), f"{name}: missing DB")
            require(core.data.joinpath("home-mate.schema").is_file(), f"{name}: main schema sidecar missing")
            core.token = core.request("POST", "/api/v0/auth/provision")["token"]
            require(core.request("GET", "/api/v0/status")["auth"]["configured"],
                    f"{name}: pairing not persisted")
            core.request("POST", "/api/v0/auth/provision", expected=409)
            core.request("POST", "/api/v0/chat", {"text": " "}, expected=400)
            core.request("PUT", "/api/v0/config/services/llm", {
                "provider": "unconfigured", "base_url": "", "model": "", "api_key_env": "",
            })
            core.request("POST", "/api/v0/chat", {"text": "no provider"}, expected=409)
            core.request("PUT", "/api/v0/config/services/llm", {
                "provider": "deepseek", "base_url": "", "model": name,
            }, expected=400)
            require(core.request("GET", "/api/v0/config/services")["llm"]["provider"] == "unconfigured",
                    f"{name}: rejected configuration changed stored provider")
            core.request("PUT", "/api/v0/config/services/llm", {
                "provider": "deepseek", "base_url": fake_url, "model": name, "api_key_env": "",
            })
            core.request("PUT", "/api/v0/fleet", {"self_id": name})
            require(core.request("GET", "/api/v0/fleet")["self_id"] == name,
                    f"{name}: management identity readback mismatch")
            events = chat(core, f"unique-input-{name}")
            (root / f"{name}-chat.json").write_text(json.dumps(events, indent=2))
            report["checks"].append(f"{name}: startup, pairing, refusal, config, headless chat")

        for core in cores:
            session = urllib.parse.quote(core.session_id, safe="")
            messages = core.request("GET", f"/api/v0/session/{session}/messages")["messages"]
            serialized = json.dumps(messages)
            require(f"unique-input-{core.name}" in serialized, f"{core.name}: missing own history")
            for other in cores:
                if other is not core:
                    require(f"unique-input-{other.name}" not in serialized,
                            f"{core.name}: cross-DB history leak")
            require(core.request("GET", "/api/v0/devices")["gateway"]["devices"] == [],
                    f"{core.name}: headless chat invented a device")
        report["checks"].append("three independent DBs have isolated API session histories")
        first = cores[0]
        require(first.stop(), "Core did not shut down gracefully before restart")
        first.start()
        require(first.request("GET", "/api/v0/fleet")["self_id"] == first.name,
                "restart lost managed identity")
        require(first.request("GET", "/api/v0/auth")["configured"], "restart lost pairing")
        chat(first, "restart-check-luna-a")
        with server.request_lock:
            requests = list(server.requests)
        require(len(requests) == 4, f"unexpected provider calls: {len(requests)}")
        require("unique-input-luna-a" in json.dumps(requests[-1]["messages"]),
                "real provider prompt lost persisted history after restart")
        for body in requests:
            require(body.get("stream") is True and body.get("tools"),
                    "production streaming/tool-directory HTTP request not observed")
            require(body["model"] in [core.name for core in cores], "unknown model fixture identity")
            for core in cores:
                if core.name != body["model"]:
                    require(f"unique-input-{core.name}" not in json.dumps(body["messages"]),
                            "foreign node history reached real provider prompt")
        (root / "provider-requests.json").write_text(json.dumps(requests, indent=2))
        report["checks"].append("real-main restart preserves config, pairing and next-turn prompt history")
        report["status"] = "passed"
    except Exception as exc:
        report["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_alarm)
        for core in reversed(cores):
            if not core.stop():
                report["status"] = "failed"
                report.setdefault("cleanup_errors", []).append(f"{core.name}: non-graceful exit")
        if server is not None:
            if thread is not None:
                server.shutdown()
                thread.join(timeout=2)
            server.server_close()
        with binary.open("rb") as artifact:
            report["core_binary_sha256"] = hashlib.file_digest(artifact, "sha256").hexdigest()
        report["nodes"] = [{"id": c.name, "api_port": c.api_port, "gateway_port": c.gateway_port,
                            "data_dir": str(c.data), "session_id": c.session_id,
                            "exit_code": c.proc.returncode if c.proc else None}
                           for c in cores]
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps({"report": str(root / "report.json"), **report}, indent=2))
        if report["status"] != "passed":
            for core in cores:
                print(f"--- {core.name} log tail ---", file=sys.stderr)
                print(core.log_path.read_text(errors="replace")[-8000:], file=sys.stderr)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, default=Path("target/debug/hm-core"))
    parser.add_argument("--output-dir", type=Path, default=Path(tempfile.gettempdir()) / "luna-system-smoke")
    parser.add_argument("--timeout", type=float, default=60)
    options = parser.parse_args()
    require(0 < options.timeout <= 300, "timeout must be greater than 0 and at most 300 seconds")
    sys.exit(run(options))
