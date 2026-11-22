#!/usr/bin/env python3
"""External deterministic HTTP partners for production-main transport tests.

Use ``with TransportPartners() as partner``; configure each Core through its
ordinary management API using partner.llm_config_body/music_config_body, then
POST /api/v0/chat with text=partner.command("music_play", "fixture-0").
Subsequent new chats use music_pause/music_stop. The LLM selects real advertised
wire names and actual search-result IDs. It never forges Core tool results.
Synthetic auth and PCM only; memory extraction stays enabled and receives JSON.
"""

import array
import collections
import hashlib
import http.server
import io
import json
import math
import threading
import urllib.parse
import wave

LIB_USER = "transport-fixture"
LIB_SECRET = "synthetic-transport-library-secret"
LLM_TOKEN = "synthetic-transport-llm-token"
TRACK_ID = "transport-track-1"


def _wav():
    pcm = array.array("h", (int(4000 * math.sin(2 * math.pi * 220 * n / 16000))
                           for n in range(30 * 16000)))
    import sys
    if sys.byteorder != "little":
        pcm.byteswap()
    output = io.BytesIO()
    with wave.open(output, "wb") as writer:
        writer.setnchannels(1)
        writer.setsampwidth(2)
        writer.setframerate(16000)
        writer.writeframes(pcm.tobytes())
    return output.getvalue()


def _actual_tracks(value):
    if isinstance(value, dict):
        if isinstance(value.get("tracks"), list):
            return value["tracks"]
        for child in value.values():
            tracks = _actual_tracks(child)
            if tracks:
                return tracks
    return []


class TransportPartners:
    def __init__(self):
        self.lock = threading.Lock()
        self.counts = collections.Counter()
        self.recent = collections.deque(maxlen=128)
        self.wav = _wav()
        self.server = None
        self.thread = None

    def start(self):
        if self.server is None:
            self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), PartnerHandler)
            self.server.daemon_threads = True
            self.server.block_on_close = False
            self.server.partner = self
            self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
            self.thread.start()
        return self

    @property
    def base(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    @property
    def llm_base(self):
        return self.base + "/v1"

    @property
    def llm_config_body(self):
        return {"provider": "deepseek", "base_url": self.llm_base,
                "model": "transport-fixture", "api_key_env": "", "api_key": LLM_TOKEN}

    @property
    def music_config_body(self):
        return {"enabled": True, "prefer_local": True,
                "library": {"endpoint": self.base, "user": LIB_USER, "secret": LIB_SECRET},
                "online": None}

    @staticmethod
    def command(scenario, device):
        if scenario not in ("music_play", "music_pause", "music_stop", "chat"):
            raise ValueError("unsupported transport scenario")
        return json.dumps({"scenario": scenario, "device": device}, separators=(",", ":"))

    def record(self, kind, **fields):
        with self.lock:
            self.counts[kind] += 1
            self.recent.append({"kind": kind, **fields})
            return self.counts[kind]

    def metrics(self):
        with self.lock:
            return {"counts": dict(self.counts), "recent": list(self.recent),
                    "wav_bytes": len(self.wav), "wav_duration_ms": 30000}

    def close(self):
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
            self.thread.join(timeout=2)
            self.server = None

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()


class PartnerHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def _reply(self, status, value, kind="application/json"):
        data = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.connection.settimeout(2)
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, TimeoutError):
            self.server.partner.record("partner_client_closed")

    def do_GET(self):
        parsed = urllib.parse.urlsplit(self.path)
        query = urllib.parse.parse_qs(parsed.query)
        get = lambda key: query.get(key, [""])[0]
        endpoint = parsed.path.removeprefix("/rest/").removesuffix(".view")
        if not parsed.path.startswith("/rest/") or endpoint not in ("search3", "getRandomSongs", "getSong", "stream", "ping"):
            self._reply(404, {"error": "unsupported fixture endpoint"})
            return
        expected = hashlib.md5((LIB_SECRET + get("s")).encode()).hexdigest()
        if get("u") != LIB_USER or not get("s") or get("t") != expected:
            self.server.partner.record("subsonic_auth_refused")
            self._reply(401, {"subsonic-response": {"status": "failed", "error": {"code": 40}}})
            return
        self.server.partner.record("subsonic_" + endpoint)
        song = {"id": TRACK_ID, "title": "Transport fixture", "artist": "Synthetic partner", "duration": 30}
        if endpoint in ("stream", "getSong") and get("id") != TRACK_ID:
            self._reply(404, {"subsonic-response": {"status": "failed", "error": {"code": 70}}})
        elif endpoint == "stream":
            self._reply(200, self.server.partner.wav, "audio/wav")
        elif endpoint == "getSong":
            self._reply(200, {"subsonic-response": {"status": "ok", "song": song}})
        elif endpoint == "ping":
            self._reply(200, {"subsonic-response": {"status": "ok"}})
        else:
            matches = not get("query") or get("query") == "*" or "transport" in get("query").lower()
            key = "searchResult3" if endpoint == "search3" else "randomSongs"
            self._reply(200, {"subsonic-response": {"status": "ok", key: {"song": [song] if matches else []}}})

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self._reply(404, {"error": "unsupported fixture endpoint"})
            return
        if self.headers.get("Authorization") != "Bearer " + LLM_TOKEN:
            self._reply(401, {"error": "synthetic LLM auth required"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 1_000_000:
                raise ValueError("request size")
            body = json.loads(self.rfile.read(length))
        except (ValueError, json.JSONDecodeError):
            self._reply(400, {"error": "invalid fixture request"})
            return
        call_no = self.server.partner.record("llm_request")
        tools = body.get("tools") or []
        messages = body.get("messages") or []
        wire = {}
        for item in tools:
            function = item.get("function", {})
            canonical = function.get("description", "").split(":", 1)[0]
            if canonical in ("music.search", "music.play", "music.control"):
                wire[canonical] = function.get("name")
        if not tools:
            self.server.partner.record("memory_json_request")
            # Actual extraction/confirmation calls remain enabled. No facts
            # exist in these synthetic transport commands, so propose none.
            self._completion(json.dumps({"candidates": [], "decisions": [], "entries": []}))
            return
        last_user = max((i for i, message in enumerate(messages) if message.get("role") == "user"), default=-1)
        try:
            command = json.loads(messages[last_user].get("content", "")) if last_user >= 0 else {}
        except (TypeError, ValueError):
            command = {}
        scenario = command.get("scenario", "chat")
        ids = {}
        results = {}
        inverse = {value: key for key, value in wire.items()}
        for message in messages[last_user + 1:]:
            for call in message.get("tool_calls") or []:
                canonical = inverse.get(call.get("function", {}).get("name"))
                if canonical:
                    ids[call.get("id")] = canonical
            if message.get("role") == "tool" and message.get("tool_call_id") in ids:
                try:
                    results[ids[message["tool_call_id"]]] = json.loads(message.get("content", "{}"))
                except (TypeError, ValueError):
                    results[ids[message["tool_call_id"]]] = {"status": "unparseable actual response"}
        plan = None
        if scenario == "music_play":
            if "music.search" not in results:
                plan = ("music.search", {"query": "transport", "source": "library", "limit": 1})
            elif "music.play" not in results:
                tracks = _actual_tracks(results["music.search"])
                track = next((t for t in tracks if t.get("source") == "library" and t.get("id")), None)
                if track:
                    plan = ("music.play", {"source": "library", "id": track["id"]})
        elif scenario in ("music_pause", "music_stop") and "music.control" not in results:
            plan = ("music.control", {"action": scenario.removeprefix("music_"), "target": "media"})
        if plan and wire.get(plan[0]):
            self.server.partner.record("llm_tool_" + plan[0])
            call = {"index": 0, "id": f"transport-call-{call_no}", "type": "function",
                    "function": {"name": wire[plan[0]], "arguments": json.dumps(plan[1])}}
            self._completion(None, call)
        else:
            statuses = {name: {"status": result.get("status"), "evidence": result.get("evidence")}
                        for name, result in results.items() if isinstance(result, dict)}
            self._completion(json.dumps({"scenario": scenario, "actual_tool_statuses": statuses}))

    def _completion(self, content, call=None):
        delta = {"tool_calls": [call]} if call else {"content": content}
        reason = "tool_calls" if call else "stop"
        chunks = [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                  {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}]
        data = "".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks) + "data: [DONE]\n\n"
        self._reply(200, data.encode(), "text/event-stream")
