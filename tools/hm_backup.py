#!/usr/bin/env python3
"""Home Mate config backup / restore (R1f).

One file that survives a device wipe: services (LLM/ASR/TTS), the music
service, and the voice/mode settings. `pull` includes secret VALUES by
default (that is the point of a recovery file); it is written outside the
repository with mode 0600 and never printed. Use `--no-secrets` for a
shareable copy.

Usage:
  hm_backup.py pull [--no-secrets] [--host URL] [--dir DIR]
  hm_backup.py restore <file> [--yes] [--host URL]
  hm_backup.py show <file>
  hm_backup.py list [--dir DIR]

Environment: HM_CORE (default http://192.168.1.73:8930).
"""
import json
import os
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
DEFAULT_DIR = os.path.join(os.path.expanduser("~"), ".home-mate", "backups")
SECRET_NAMES = (
    "llm.api_key", "llm_fallback.api_key", "asr.api_key", "tts.api_key",
    "music.library.secret", "music.online.secret",
)


def die(msg, code=2):
    print(f"hm_backup: {msg}", file=sys.stderr)
    sys.exit(code)


def _auth_headers(extra=None):
    headers = dict(extra or {})
    token = os.environ.get("HM_TOKEN", "").strip()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def get(path, timeout=10):
    req = urllib.request.Request(CORE + path, headers=_auth_headers(), method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def post(path, body, timeout=30, extra_headers=None):
    data = b"" if body is None else json.dumps(body).encode()
    headers = _auth_headers(extra_headers)
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(CORE + path, data=data, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def summary(doc):
    services = doc.get("services", {})
    for name in ("llm", "llm_fallback", "asr", "tts"):
        s = services.get(name, {})
        print(f"  {name:12s} {s.get('provider', '-'):12s} {s.get('model', '-'):24s} "
              f"key={'set' if s.get('api_key_set') else 'env/absent'}")
    music = doc.get("music", {})
    def conn(label, c):
        if not c:
            return f"{label}=off"
        return f"{label}={c.get('endpoint','-')} secret={'yes' if c.get('secret_set') else 'no'}"
    print(f"  music        enabled={music.get('enabled')} {conn('library', music.get('library'))} "
          f"{conn('online', music.get('online'))}")
    mode = doc.get("mode", {})
    print(f"  mode         wake_words={mode.get('wake_words')} "
          f"tail_blind_ms={mode.get('tail_blind_ms')} barge_in={mode.get('barge_in')} "
          f"keyword_interrupt={mode.get('keyword_interrupt')}")
    secrets = doc.get("secrets") or {}
    print(f"  secrets      {len(secrets)} value(s) " + (f"({', '.join(sorted(secrets))})" if secrets else "(not included)"))


def pull(args):
    with_secrets = "--no-secrets" not in args
    out_dir = DEFAULT_DIR
    if "--dir" in args:
        out_dir = args[args.index("--dir") + 1]
    if with_secrets:
        doc = post(
            "/api/v0/config/export?secrets=1",
            None,
            extra_headers={"X-HM-Confirm-Secrets": "EXPORT_SECRETS"},
        )
    else:
        doc = get("/api/v0/config/export")
    os.makedirs(out_dir, mode=0o700, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    out = os.path.join(out_dir, f"hm-config-{stamp}.json")
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(doc, f, ensure_ascii=False, indent=1)
    latest = os.path.join(out_dir, "latest.json")
    tmp = latest + ".tmp"
    with open(tmp, "w") as f:
        json.dump(doc, f, ensure_ascii=False, indent=1)
    os.chmod(tmp, 0o600)
    os.replace(tmp, latest)
    print(f"saved {out} (0600)")
    summary(doc)
    if with_secrets:
        print("  NOTE: this file contains API keys / music secrets; keep it off Git.")


def restore(args):
    if not args:
        die("restore needs a file")
    src = args[0]
    with open(src, encoding="utf-8") as f:
        doc = json.load(f)
    print(f"about to restore {src}:")
    summary(doc)
    if "--yes" not in args:
        if not sys.stdin.isatty():
            die("refusing to restore without --yes (non-interactive)")
        if input("apply this config to the device? [y/N] ").strip().lower() not in ("y", "yes"):
            die("cancelled", 1)
    res = post("/api/v0/config/import", doc)
    print(f"state={res.get('state')} applied={res.get('applied')}")
    for w in res.get("warnings") or []:
        print(f"  warning: {w}")
    print("verify: services/music/mode read back through /api/v0/config/services and /api/v0/mode")


def show(args):
    if not args:
        die("show needs a file")
    with open(args[0], encoding="utf-8") as f:
        summary(json.load(f))


def list_backups(args):
    out_dir = DEFAULT_DIR
    if "--dir" in args:
        out_dir = args[args.index("--dir") + 1]
    if not os.path.isdir(out_dir):
        print("no backups yet")
        return
    for name in sorted(os.listdir(out_dir)):
        if name.endswith(".json"):
            p = os.path.join(out_dir, name)
            st = os.stat(p)
            print(f"{time.strftime('%Y-%m-%d %H:%M', time.localtime(st.st_mtime))} "
                  f"{st.st_size:8d} B  {p}")


def main(argv):
    global CORE
    if not argv or argv[0] in ("-h", "--help", "help"):
        print(__doc__)
        return 0
    cmd = argv[0]
    args = argv[1:]
    # --host URL applies to every command; strip it before dispatching.
    if "--host" in args:
        i = args.index("--host")
        if i + 1 >= len(args):
            die("--host needs a URL")
        CORE = args[i + 1]
        args = args[:i] + args[i + 2:]
    if cmd == "pull":
        pull(args)
    elif cmd == "restore":
        restore(args)
    elif cmd == "show":
        show(args)
    elif cmd == "list":
        list_backups(args)
    else:
        die(f"unknown command {cmd}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
