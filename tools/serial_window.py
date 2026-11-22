#!/usr/bin/env python3
"""Fixed-window serial helper.

`tools/serial_cmd.py` (gateway repo) keeps extending its read window while the
firmware log keeps flowing, so it can hang forever on a busy device. This
helper has a HARD deadline: it sends the commands, reads until the window
closes, prints what it saw and exits.

Usage:
  python3 tools/serial_window.py /dev/cu.usbmodem31101 4 "echo hello"
  python3 tools/serial_window.py PORT 8 --cmd "cmd1" --cmd "cmd2"
  python3 tools/serial_window.py PORT 3            # just read the log

Exit code: 0 if the port opened, 2 otherwise.
"""
import argparse
import sys
import time

import serial


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("port")
    ap.add_argument("seconds", type=float, nargs="?", default=3.0)
    ap.add_argument("command", nargs="?", default="")
    ap.add_argument("--cmd", action="append", default=[], help="extra command (repeatable)")
    ap.add_argument("--baud", type=int, default=115200)
    args = ap.parse_args()

    commands = []
    if args.command:
        commands.append(args.command)
    commands.extend(args.cmd)

    try:
        ser = serial.Serial(args.port, args.baud, timeout=0.1)
    except Exception as e:  # noqa: BLE001 - the caller needs the reason
        print(f"serial open failed: {e}", file=sys.stderr)
        return 2

    out = []
    try:
        time.sleep(0.2)
        ser.reset_input_buffer()
        for cmd in commands:
            ser.write((cmd + "\n").encode())
            ser.flush()
            time.sleep(0.3)
        deadline = time.time() + max(0.1, args.seconds)
        while time.time() < deadline:
            chunk = ser.read(4096)
            if chunk:
                out.append(chunk.decode("utf-8", errors="replace"))
    finally:
        try:
            ser.close()
        except Exception:  # noqa: BLE001
            pass
    sys.stdout.write("".join(out))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
