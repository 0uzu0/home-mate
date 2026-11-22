"""Tooling checks only; these do not claim actual Core/Firmware pair coverage."""
import concurrent.futures
import copy
import io
import json
import urllib.error
import urllib.request
import unittest
import wave

from check_luna_voice_evidence import validate, validate_inline_capture_transfers
from luna_transport_partners import LLM_TOKEN
from luna_voice_partners import VoicePartners


def post(partner, path, data, content_type="application/json", token=""):
    request = urllib.request.Request(partner.base + path, data=data,
        headers={"Content-Type": content_type, "Authorization": "Bearer " + token})
    try:
        response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=3)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, response.read()


def multipart(wav):
    boundary = "voice-fixture-boundary"
    pieces = []
    for name, data, suffix in [("model", b"voice-fixture-asr", ""), ("language", b"zh", ""),
                                ("file", wav, '; filename="utterance.wav"\r\nContent-Type: audio/wav')]:
        pieces.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"{suffix}\r\n\r\n'.encode() + data + b"\r\n")
    return b"".join(pieces) + f"--{boundary}--\r\n".encode(), "multipart/form-data; boundary=" + boundary


class PartnerTests(unittest.TestCase):
    def test_actual_http_asr_waits_at_barrier_and_records_nonzero_multipart(self):
        with VoicePartners("test") as partner, concurrent.futures.ThreadPoolExecutor() as pool:
            gate = partner.enqueue("barrier", "synthetic utterance", blocked=True)
            data, kind = multipart(partner.speech_wav)
            future = pool.submit(post, partner, "/v1/audio/transcriptions", data, kind)
            # A request that has not entered its HTTP consumer cannot prove
            # a held response. Poll the externally observed arrival barrier.
            import time
            until = time.monotonic() + 2
            while not partner.snapshot()["asr"] and time.monotonic() < until:
                time.sleep(.01)
            self.assertEqual(partner.snapshot()["asr"][0]["case"], "barrier")
            self.assertFalse(future.done())
            self.assertFalse(partner.snapshot()["asr"][0]["released"])
            gate.set()
            status, body = future.result(timeout=2)
            self.assertEqual((status, json.loads(body)), (200, {"text": "synthetic utterance"}))
            self.assertTrue(partner.snapshot()["asr"][0]["nonzero_pcm"])

    def test_asr_refuses_silent_pcm_and_unscripted_requests(self):
        with VoicePartners("test") as partner:
            silent = io.BytesIO()
            with wave.open(silent, "wb") as writer:
                writer.setnchannels(1)
                writer.setsampwidth(2)
                writer.setframerate(16000)
                writer.writeframes(bytes(3200))
            for wav, expected in [(silent.getvalue(), 400), (partner.speech_wav, 409)]:
                data, kind = multipart(wav)
                self.assertEqual(post(partner, "/v1/audio/transcriptions", data, kind)[0], expected)
            self.assertEqual(partner.metrics()["counts"]["invalid_asr_pcm"], 1)
            self.assertEqual(partner.metrics()["counts"]["unexpected_asr"], 1)

    def test_real_http_llm_and_tts_consumers_retain_input_and_nonzero_wav(self):
        with VoicePartners("test") as partner:
            request = {"model": "fixture", "messages": [{"role": "user", "content": "case-identity"}],
                       "tools": [{"type": "function", "function": {"name": "probe", "description": "probe"}}]}
            status, body = post(partner, "/v1/chat/completions", json.dumps(request).encode(), token=LLM_TOKEN)
            self.assertEqual(status, 200)
            self.assertIn("data: [DONE]", body.decode())
            self.assertEqual(partner.snapshot()["llm"], [request])
            status, body = post(partner, "/v1/audio/speech", json.dumps({"model": "fixture", "input": "response"}).encode())
            self.assertEqual(status, 200)
            with wave.open(io.BytesIO(body), "rb") as reader:
                self.assertEqual(reader.getframerate(), 16000)
                self.assertTrue(any(reader.readframes(reader.getnframes())))


class EvidenceTests(unittest.TestCase):
    def test_inline_identity_confirmation_and_absolute_deadline_corruption_are_rejected(self):
        original = {"input_id": 7, "start_ms": 1000, "capture_deadline_ms": 13000,
                    "lifetime_deadline_ms": 61000, "origin": {"spans": ["current-source"]},
                    "confirmed": False, "eligibility": "pending"}
        admitted = dict(original, eligibility="session")
        confirmed = dict(admitted, confirmed=True)
        report = {"normal_voice": {"wake_mode": "inline"}, "reconnected_voice": {"wake_mode": "separate"},
                  "inline_capture_transfers": [{"before_keyword": {"input": original},
                      "after_keyword": {"input": admitted}, "after_speech_started": {"input": confirmed}}]}
        validate_inline_capture_transfers(report)
        for key, replacement in [("input_id", 8), ("start_ms", 1001), ("capture_deadline_ms", 13001),
                                 ("lifetime_deadline_ms", 61001), ("origin", {"spans": ["different-source"]}),
                                 ("confirmed", True)]:
            bad = copy.deepcopy(report)
            bad["inline_capture_transfers"][0]["after_keyword"]["input"][key] = replacement
            with self.subTest(key=key), self.assertRaises(AssertionError):
                validate_inline_capture_transfers(bad)

    def test_closed_reconnect_rejects_control_only_onset_and_speech_started(self):
        original = {"input_id": 11, "start_ms": 2000, "capture_deadline_ms": 14000,
                    "lifetime_deadline_ms": 62000, "origin": {"spans": [{"control_only": False}]},
                    "confirmed": False, "eligibility": "pending"}
        admitted = dict(original, eligibility="session")
        confirmed = dict(admitted, confirmed=True)
        report = {"case": "closed-reconnect",
                  "normal_voice": {"wake_mode": "separate"},
                  "reconnected_voice": {"wake_mode": "inline"},
                  "inline_capture_transfers": [{"before_keyword": {"input": original},
                      "after_keyword": {"input": admitted},
                      "after_speech_started": {"input": confirmed},
                      "speech_started_event": {"provenance": {"spans": [{"control_only": False}]}}}]}
        validate_inline_capture_transfers(report)
        bad_onset = copy.deepcopy(report)
        bad_onset["inline_capture_transfers"][0]["before_keyword"]["input"]["origin"]["spans"][0]["control_only"] = True
        with self.assertRaisesRegex(AssertionError, "control_only"):
            validate_inline_capture_transfers(bad_onset)
        bad_started = copy.deepcopy(report)
        bad_started["inline_capture_transfers"][0]["speech_started_event"]["provenance"]["spans"][0]["control_only"] = True
        with self.assertRaisesRegex(AssertionError, "control_only"):
            validate_inline_capture_transfers(bad_started)

    def test_checker_rejects_failed_or_incomplete_artifacts(self):
        for report in ({}, {"status": "failed", "failures": []}, {"status": "passed", "failures": [], "protocol": 0}):
            with self.assertRaises((AssertionError, KeyError)):
                validate(report)

    def test_checker_rejects_false_full_coverage_before_actor_checks(self):
        report = {"status": "passed", "failures": [], "protocol": 0, "sources_unchanged": True,
                  "coverage": {f"P{i:02}": {"status": "unexercised"} for i in range(1, 14)}}
        report["coverage"]["P01"]["status"] = "partial"
        report["coverage"]["P02"]["status"] = "covered"
        with self.assertRaisesRegex(AssertionError, "unrelated coverage overstated"):
            validate(report)


if __name__ == "__main__":
    unittest.main()
