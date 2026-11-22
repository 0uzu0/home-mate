#!/usr/bin/env python3
"""External HTTP ASR/TTS partners for the real-main/Firmware voice pipeline.

Only synthetic PCM is accepted. ASR responses can wait at an explicit barrier;
LLM and Subsonic behavior reuse the transport partner unchanged.
"""
import array
import collections
import email.parser
import email.policy
import hashlib
import http.server
import io
import json
import threading
import wave

from luna_transport_partners import PartnerHandler, TransportPartners


class VoicePartners(TransportPartners):
    def __init__(self, node):
        super().__init__()
        self.node = node
        self.scripts = collections.deque()
        self.asr_requests = []
        self.tts_requests = []
        self.llm_requests = []
        self.release_all = []
        # A short nonzero response makes DAC observation specific to this test.
        output = io.BytesIO()
        pcm = array.array("h", [2000, -2000] * 2400)
        import sys
        if sys.byteorder != "little":
            pcm.byteswap()
        with wave.open(output, "wb") as writer:
            writer.setnchannels(1)
            writer.setsampwidth(2)
            writer.setframerate(16000)
            writer.writeframes(pcm.tobytes())
        self.speech_wav = output.getvalue()

    def start(self):
        if self.server is None:
            self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), VoiceHandler)
            self.server.daemon_threads = True
            self.server.block_on_close = False
            self.server.partner = self
            self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
            self.thread.start()
        return self

    def enqueue(self, case, text, blocked=False):
        gate = threading.Event()
        if not blocked:
            gate.set()
        with self.lock:
            self.scripts.append({"case": case, "text": text, "gate": gate})
            self.release_all.append(gate)
        return gate

    def snapshot(self):
        with self.lock:
            return {"asr": [dict(row) for row in self.asr_requests],
                    "tts": [dict(row) for row in self.tts_requests],
                    "llm": [dict(row) for row in self.llm_requests],
                    "queued_asr": len(self.scripts)}

    def close(self):
        for gate in self.release_all:
            gate.set()
        super().close()


class VoiceHandler(PartnerHandler):
    def do_POST(self):
        if self.path == "/v1/chat/completions":
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= 1_000_000:
                return self._reply(413, {"error": "bounded partner request required"})
            raw = self.rfile.read(size)
            body = json.loads(raw)
            with self.server.partner.lock:
                self.server.partner.llm_requests.append(body)
            original = self.rfile
            try:
                self.rfile = io.BytesIO(raw)
                return super().do_POST()
            finally:
                self.rfile = original
        if self.path not in ("/v1/audio/transcriptions", "/v1/audio/speech"):
            return super().do_POST()
        size = int(self.headers.get("Content-Length", "0"))
        if not 0 < size <= 1_000_000:
            return self._reply(413, {"error": "bounded partner request required"})
        raw = self.rfile.read(size)
        partner = self.server.partner
        if self.path.endswith("/speech"):
            body = json.loads(raw)
            with partner.lock:
                partner.tts_requests.append({"input": body.get("input"), "model": body.get("model")})
            partner.record("tts_request")
            return self._reply(200, partner.speech_wav, "audio/wav")
        message = email.parser.BytesParser(policy=email.policy.default).parsebytes(
            b"Content-Type: " + self.headers.get("Content-Type", "").encode() + b"\r\n\r\n" + raw)
        parts = {part.get_param("name", header="content-disposition"): part.get_payload(decode=True)
                 for part in message.iter_parts()}
        wav = parts.get("file", b"")
        try:
            with wave.open(io.BytesIO(wav), "rb") as reader:
                rate, channels, width, count = reader.getframerate(), reader.getnchannels(), reader.getsampwidth(), reader.getnframes()
                pcm = reader.readframes(count)
            assert rate == 16000 and channels == 1 and width == 2 and any(pcm)
            assert parts.get("model") == b"voice-fixture-asr" and parts.get("language") == b"zh"
        except (AssertionError, wave.Error, EOFError) as error:
            partner.record("invalid_asr_pcm", error=str(error))
            return self._reply(400, {"error": "expected real nonzero 16k mono PCM multipart"})
        with partner.lock:
            script = partner.scripts.popleft() if partner.scripts else None
            row = {"case": script["case"] if script else "unexpected", "wav_sha256": hashlib.sha256(wav).hexdigest(),
                   "samples": count, "rate": rate, "nonzero_pcm": True, "released": False}
            partner.asr_requests.append(row)
        partner.record("asr_request")
        if script is None:
            partner.record("unexpected_asr")
            return self._reply(409, {"error": "unscripted ASR request"})
        if not script["gate"].wait(timeout=20):
            partner.record("asr_barrier_timeout")
            return self._reply(504, {"error": "ASR test barrier expired"})
        with partner.lock:
            row["released"] = True
        self._reply(200, {"text": script["text"]})

    def _completion(self, content, call=None):
        if content is not None:
            try:
                parsed = json.loads(content)
                if "scenario" in parsed:
                    content = "系统管线测试成功。"
            except (ValueError, TypeError):
                pass
        return super()._completion(content, call)
