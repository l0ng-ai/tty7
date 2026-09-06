#!/usr/bin/env python3
"""Private daemon for the opt-in Windows/native-SSH reconnect regression.

Keep stdin open while using the printed configuration. EOF stops only the
process group created here. Never uses the account's normal daemon endpoint.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--server", required=True, type=Path)
parser.add_argument("--artifacts", required=True, type=Path)
args = parser.parse_args()
server = args.server.resolve(strict=True)
root = args.artifacts.resolve(strict=True)
assert str(root).startswith("/tmp/tty7-774-"), "private test artifacts only"
assert server.is_relative_to(root), "candidate must be inside the private test artifacts"
directory = Path(tempfile.mkdtemp(prefix="windows-", dir=root))
env = dict(os.environ, TTY7_CONFIG_DIR=str(directory), TTY7_DATA_DIR=str(directory / "data"))
with (directory / "server.log").open("wb") as log:
    proc = subprocess.Popen([str(server), "--daemon", "--config-dir", str(directory)],
                            env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                            start_new_session=True)
    try:
        deadline = time.monotonic() + 15
        while not (directory / "control.sock").exists():
            assert proc.poll() is None, "private daemon exited; inspect server.log"
            assert time.monotonic() < deadline, "private daemon did not start"
            time.sleep(0.02)
        print(json.dumps({"directory": str(directory), "pid": proc.pid}), flush=True)
        sys.stdin.buffer.read()
    finally:
        if proc.poll() is None:
            try:
                with socket.socket(socket.AF_UNIX) as sock:
                    sock.settimeout(3)
                    sock.connect(str(directory / "daemon.sock"))
                    body = b'"manage"'
                    sock.sendall(struct.pack("<IB", len(body), 57) + body)
                    sock.sendall(struct.pack("<IB", 0, 8))
                proc.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                os.killpg(proc.pid, signal.SIGTERM)
                proc.wait(timeout=5)
