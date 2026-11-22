#!/usr/bin/env python3
"""Stage-C ASR cloud colloquial check: 10 spoken-style utterances.

Stimuli are synthesized with the configured Volcano TTS (machine speech,
unattended proxy — human spot-check stays with the user), then transcribed
through the SAME provider config hm-core uses (read from /api/v0/status).
Normalized exact-match required for 10/10.
"""
import json
import re
import subprocess
import sys
import urllib.request

SENTENCES = [
    "现在网上大家都在听的歌，来一个呗",
    "帮我把音量调小一点",
    "今天外面下雨了没有啊",
    "明天早上七点叫我起床",
    "这首歌不错，单曲循环吧",
    "你能不能讲个笑话听听",
    "我有点困了，放点轻音乐",
    "刚才那首歌叫什么名字来着",
    "停一下，先别放了",
    "帮我看看明天要下雨吗",
]

VOLC_KEY = None  # from data/env.local


def http_json(url, data=None, headers=None, method=None):
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


DIGIT_MAP = str.maketrans("0123456789", "零一二三四五六七八九")


def norm(s: str) -> str:
    # SenseVoice appends emotion emoji tags and applies digit ITN
    # ("七点" -> "7点"); both are transcript formatting, not content
    # errors, so comparison strips emoji and maps digits to Chinese
    # numerals (single-digit only in this sentence set).
    s = re.sub(r"[\U0001F300-\U0001FAFF\u2600-\u27BF]", "", s)
    s = re.sub(r"[，。？！,.!? ~、]", "", s)
    return s.translate(DIGIT_MAP)


def synth_wav(text: str, path: str) -> bool:
    """Synthesize via Volcano unidirectional TTS, mirroring core's VolcanoTts."""
    import base64
    import struct
    body = json.dumps({
        "user": {"uid": "home-mate-b"},
        "req_params": {
            "text": text,
            "speaker": "zh_female_xiaohe_uranus_bigtts",
            "audio_params": {"format": "pcm", "sample_rate": 16000},
        },
    }).encode()
    req = urllib.request.Request(
        "https://openspeech.bytedance.com/api/v3/tts/unidirectional",
        data=body,
        headers={"X-Api-Key": VOLC_KEY, "X-Api-Resource-Id": "seed-tts-2.0",
                 "Content-Type": "application/json"},
    )
    pcm = bytearray()
    with urllib.request.urlopen(req, timeout=60) as r:
        for line in r:
            line = line.strip()
            if not line:
                continue
            try:
                f = json.loads(line)
            except json.JSONDecodeError:
                continue
            if f.get("code", 0) not in (0, 20000000):
                print("volc frame error:", f)
                return False
            d = f.get("data")
            if isinstance(d, str) and d:
                pcm.extend(base64.b64decode(d))
    if not pcm:
        return False
    hdr = b"RIFF" + struct.pack("<I", 36 + len(pcm)) + b"WAVEfmt " + struct.pack(
        "<IHHIIHH", 16, 1, 1, 16000, 32000, 2, 16) + b"data" + struct.pack("<I", len(pcm))
    with open(path, "wb") as f:
        f.write(hdr + bytes(pcm))
    return True


def transcribe(path: str, cfg: dict, key: str) -> tuple[str, int]:
    import time
    cmd = ["curl", "-s", "-o", "/dev/null", "-w", "%{time_total}",
           f"{cfg['base_url']}/audio/transcriptions",
           "-H", f"Authorization: Bearer {key}",
           "-F", f"model={cfg['model']}",
           "-F", f"file=@{path}"]
    t0 = time.time()
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=90).stdout.strip()
    # need the body too — rerun capturing json
    cmd2 = ["curl", "-s", f"{cfg['base_url']}/audio/transcriptions",
            "-H", f"Authorization: Bearer {key}",
            "-F", f"model={cfg['model']}",
            "-F", f"file=@{path}"]
    body = subprocess.run(cmd2, capture_output=True, text=True, timeout=90).stdout
    try:
        text = json.loads(body).get("text", "")
    except json.JSONDecodeError:
        text = f"<bad body: {body[:120]}>"
    return text, int(float(out) * 1000)


def main():
    global VOLC_KEY
    with open("data/env.local") as f:
        for line in f:
            if line.startswith("VOLC_API_KEY="):
                VOLC_KEY = line.strip().split("=", 1)[1]
    assert VOLC_KEY, "VOLC_API_KEY not found in data/env.local"

    # provider config exactly as hm-core holds it
    st = http_json("http://127.0.0.1:8930/api/v0/status")
    asr = st["services"]["asr"]
    assert asr["provider"] == "openai-asr" and "siliconflow" in asr["base_url"], asr
    key = asr_key = None
    # key lives in secrets db; read it back from env.local-style storage via sqlite
    import sqlite3
    con = sqlite3.connect("data/home-mate.db")
    row = con.execute("SELECT value FROM secrets WHERE name='asr.api_key'").fetchone()
    key = row[0]
    assert key and key.startswith("sk-"), "asr key missing"

    import os
    os.makedirs("/tmp/hm-c/asr10", exist_ok=True)
    ok = 0
    lat = []
    for i, sent in enumerate(SENTENCES, 1):
        wav = f"/tmp/hm-c/asr10/{i:02d}.wav"
        if not (os.path.exists(wav) and os.path.getsize(wav) > 1000):
            if not synth_wav(sent, wav):
                print(f"{i:02d} SYNTH-FAIL: {sent}")
                continue
        text, ms = transcribe(wav, asr, key)
        lat.append(ms)
        good = norm(text) == norm(sent)
        ok += good
        print(f"{i:02d} {'PASS' if good else 'FAIL'} {ms:5d}ms expect={sent!r} got={text!r}")
    print(f"\nresult: {ok}/10, latency ms: {sorted(lat)}")
    return 0 if ok == 10 else 1


if __name__ == "__main__":
    sys.exit(main())
