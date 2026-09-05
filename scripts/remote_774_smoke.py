#!/usr/bin/env python3
"""Run against a supplied test binary, never an existing user's daemon.

Creates a private configuration and data directory under --artifacts, starts
its own process group, and only shuts down that isolated daemon. Artifacts are
retained. No root, packages, live configuration, or existing sessions required.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import select
import socket
import struct
import subprocess
import tempfile
import time
import uuid


def frame(sock, kind, payload=b""):
    sock.sendall(struct.pack("<IB", len(payload), kind) + payload)


def exact(sock, length):
    data = bytearray()
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            raise EOFError("test peer closed")
        data.extend(chunk)
    return bytes(data)


def receive(sock):
    length, kind = struct.unpack("<IB", exact(sock, 5))
    assert length <= 64 * 1024 * 1024, "oversize test frame"
    return kind, exact(sock, length)


def connect(path):
    sock = socket.socket(socket.AF_UNIX)
    sock.settimeout(10)
    sock.connect(str(path))
    return sock


class Control:
    def __init__(self, directory, version, name):
        self.sock = connect(directory / "control.sock")
        self.sequence = 0
        frame(self.sock, 60, json.dumps({
            "control_version": version,
            "workspace": None,
            "client_token": str(uuid.uuid4()),
            "client_hostname": name,
            "gui": False,
        }).encode())
        kind, payload = receive(self.sock)
        assert kind == 60
        self.hello = json.loads(payload)

    def call(self, request):
        self.sequence += 1
        body = json.dumps(request).encode()
        frame(self.sock, 61, struct.pack("<QI", self.sequence, len(body)) + body)
        while True:
            kind, payload = receive(self.sock)
            if kind == 63:  # layout / preemption events
                continue
            assert kind == 61
            sequence, length = struct.unpack("<QI", payload[:12])
            assert sequence == self.sequence
            return json.loads(payload[12:12 + length])

    def close(self):
        self.sock.close()


def lease_smoke(directory, version):
    a = Control(directory, version, "test-laptop")
    b = Control(directory, version, "test-desktop")
    workspace = a.call({"workspace_create": {"name": "isolated-774", "workspace": None}})["ok"]["workspace_tree"]["id"]
    first = a.call({"workspace_resume": {"id": workspace, "proof": None}})["ok"]["workspace_lease"]["proof"]
    assert "workspace_busy" in b.call({"workspace_resume": {"id": workspace, "proof": None}})["ok"]
    current = b.call({"workspace_take_over": {"id": workspace}})["ok"]["workspace_lease"]["proof"]
    assert first != current
    denied = a.call({"workspace_rename": {"workspace": workspace, "name": "stale-write"}})
    assert denied["err"]["kind"] == "permission_denied", denied
    assert b.call({"workspace_rename": {"workspace": workspace, "name": "current-write"}}) == {"ok": "unit"}
    b.close()
    assert "workspace_busy" in a.call({"workspace_resume": {"id": workspace, "proof": first}})["ok"]
    b = Control(directory, version, "test-desktop-reconnected")
    resumed = b.call({"workspace_resume": {"id": workspace, "proof": current}})
    assert resumed["ok"]["workspace_lease"]["proof"] == current
    assert b.call({"workspace_tree": {"workspace": workspace}})["ok"]["workspace_tree"]["name"] == "current-write"
    a.close()
    b.close()
    return {"resume_takeover_and_stale_layout": "passed"}


SIZE = {"cols": 120, "rows": 40, "cell_w": 8, "cell_h": 16}


def spawn_pane(directory, program, args):
    pane = connect(directory / "daemon.sock")
    frame(pane, 9, json.dumps([str(directory), SIZE, {"program": program, "args": args}]).encode())
    kind, payload = receive(pane)
    assert kind == 1, (kind, payload[:200])
    return pane, json.loads(payload)


def read_for(pane, duration):
    deadline = time.monotonic() + duration
    frames = []
    while time.monotonic() < deadline:
        readable, _, _ = select.select([pane], [], [], min(0.1, max(0, deadline - time.monotonic())))
        if readable:
            kind, payload = receive(pane)
            frames.append((kind, payload))
            if kind == 6:
                break
    return frames


def tui_smoke(directory):
    path = directory / "vim-test.txt"
    vim, _ = spawn_pane(directory, "/usr/bin/vim", ["-Nu", "NONE", "-i", "NONE", "-n", str(path)])
    read_for(vim, 0.5)
    frame(vim, 3, b"itty7 isolated vim test\x1b:wq\r")
    read_for(vim, 3)
    vim.close()
    assert path.read_text().strip() == "tty7 isolated vim test"

    btop, pane_id = spawn_pane(directory, "/usr/bin/btop", ["--utf-force", "-u", "100"])
    initial = b"".join(payload for kind, payload in read_for(btop, 1) if kind in (2, 3))
    assert b"\x1b[?1049h" in initial, "btop did not enter its alternate screen"
    produced = len(initial)
    started = time.monotonic()
    counter = 0
    while produced < 8 * 1024 * 1024 + 65536 and time.monotonic() - started < 45:
        size = dict(SIZE, cols=120 + counter % 2)
        frame(btop, 4, json.dumps(size).encode())
        frames = read_for(btop, 0.025)
        assert all(kind != 6 for kind, _ in frames), "btop exited during repaint"
        produced += sum(len(payload) for kind, payload in frames if kind == 3)
        counter += 1
    frame(btop, 5)
    btop.close()
    btop = connect(directory / "daemon.sock")
    frame(btop, 2, json.dumps([pane_id, SIZE, False]).encode())
    replay = b"".join(payload for kind, payload in read_for(btop, 1) if kind == 2)
    frame(btop, 3, b"q")
    read_for(btop, 2)
    btop.close()
    return {"vim_save_exit": "passed", "btop_output_bytes": produced,
            "btop_overflow_reached": produced > 8 * 1024 * 1024,
            "btop_replay_bytes": len(replay), "btop_replay_has_alt_screen_entry": b"\x1b[?1049h" in replay}


def replay_overflow_smoke(directory):
    # A deterministic PTY producer, not a claim that btop itself reproduced.
    # Keep alternate-screen/mouse initialization outside the final 8 MiB.
    marker = b"TTY7-774-OVERFLOW-READY"
    program = (
        "import os; "
        "os.write(1, b'\\x1b[?1049h\\x1b[?1000h'); "
        "chunk = b'x' * 65536; "
        "[(os.write(1, chunk)) for _ in range(144)]; "
        "os.write(1, b'TTY7-774-OVERFLOW-READY'); os.read(0, 16)"
    )
    pane, pane_id = spawn_pane(directory, "/usr/bin/python3", ["-c", program])
    produced = 0
    tail = b""
    deadline = time.monotonic() + 20
    while marker not in tail:
        assert time.monotonic() < deadline, "synthetic PTY output did not finish"
        kind, payload = receive(pane)
        assert kind != 6, "synthetic producer exited early"
        if kind in (2, 3):
            produced += len(payload)
            tail = (tail + payload)[-len(marker) * 2:]
    assert produced > 8 * 1024 * 1024
    frame(pane, 5)
    pane.close()
    pane = connect(directory / "daemon.sock")
    frame(pane, 2, json.dumps([pane_id, SIZE, False]).encode())
    replay = b"".join(payload for kind, payload in read_for(pane, 1) if kind == 2)
    frame(pane, 3, b"q\n")
    read_for(pane, 1)
    pane.close()
    assert marker in replay, "replay did not reach the live position"
    return {"synthetic_output_bytes": produced, "synthetic_replay_bytes": len(replay),
            "synthetic_replay_lost_alt_screen": b"\x1b[?1049h" not in replay,
            "synthetic_replay_lost_mouse_mode": b"\x1b[?1000h" not in replay}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--tui", action="store_true", help="exercise isolated Vim and btop (up to 50 seconds)")
    parser.add_argument("--replay", action="store_true", help="diagnose replay truncation with a synthetic PTY producer")
    args = parser.parse_args()
    server = args.server.resolve(strict=True)
    artifacts = args.artifacts.resolve(strict=True)
    directory = Path(tempfile.mkdtemp(prefix="run-", dir=artifacts))
    # Deliberately below the Unix socket length limit, with no user's HOME override.
    assert len(str(directory / "control.sock")) < 104
    env = dict(os.environ, TTY7_CONFIG_DIR=str(directory), TTY7_DATA_DIR=str(directory / "data"), XDG_CONFIG_HOME=str(directory / "xdg"))
    protocol = json.loads(subprocess.check_output([str(server), "--protocol"], env=env))
    with (directory / "server.log").open("wb") as log:
        proc = subprocess.Popen([str(server), "--daemon", "--config-dir", str(directory)],
                                env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True)
        try:
            deadline = time.monotonic() + 10
            while not (directory / "control.sock").exists() or not (directory / "daemon.sock").exists():
                assert proc.poll() is None, "isolated daemon exited; inspect server.log"
                assert time.monotonic() < deadline, "isolated sockets did not appear"
                time.sleep(0.025)
            results = lease_smoke(directory, protocol["control"])
            if args.tui:
                results.update(tui_smoke(directory))
            if args.replay:
                results.update(replay_overflow_smoke(directory))
            print(json.dumps({"directory": str(directory), "pid": proc.pid, "protocol": protocol, **results}))
        finally:
            if proc.poll() is None:
                try:
                    with connect(directory / "daemon.sock") as pane:
                        frame(pane, 8)  # Shutdown, only on our private endpoint.
                    proc.wait(timeout=5)
                except (OSError, subprocess.TimeoutExpired):
                    # This process group was created above; never name or scan
                    # another tty7 process or use a process-name kill command.
                    os.killpg(proc.pid, signal.SIGTERM)
                    proc.wait(timeout=5)
            assert proc.returncode == 0, f"isolated server exit: {proc.returncode}"


if __name__ == "__main__":
    main()
