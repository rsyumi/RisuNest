"""Synthetic Linux process and user systemd integration.

Run with explicit daemon and manager paths. Never opens installed application data.
"""

import json
import os
from pathlib import Path
import pty
import select
import socket
import subprocess
import sys
import tempfile
import time


server, manager = map(lambda value: str(Path(value).resolve()), sys.argv[1:3])
assert os.getuid() != 0, "Run as an ordinary user"


def run(args, *, env=None, input=None):
    result = subprocess.run(
        args, env=env, input=input, text=True, capture_output=True, timeout=45
    )
    assert result.returncode == 0, f"Synthetic command failed: {result.stderr}"
    return result.stdout


def keyboard_menu(args):
    master, slave = pty.openpty()
    process = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                               env=dict(os.environ, TERM="xterm-256color"))
    os.close(slave)

    def receive(needle):
        output = b""
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.1)[0]:
                output += os.read(master, 65536)
                if needle in output:
                    return output
        raise AssertionError("Interactive TUI did not reach the expected screen")

    try:
        first = receive("Esc 뒤로".encode())
        assert b"\x1b[?1049h" in first
        os.write(master, b"\x1b[B\r")  # Down, Enter: devices
        receive("새 기기 등록".encode())
        os.write(master, b"\x1b")  # Esc: main menu
        receive("RisuNest 동기화 서버".encode())
        os.write(master, b"0")
        receive(b"\x1b[?1049l")
        assert process.wait(timeout=5) == 0
        print("PASS: TUI arrow/Enter/Esc navigation and alternate-screen restoration")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)


# Both adapters currently use the default sync port. Refuse to disturb a listener.
with socket.socket() as probe:
    probe.bind(("127.0.0.1", 4319))

with tempfile.TemporaryDirectory(prefix="risunest-manager-test-") as temporary:
    root = Path(temporary)
    data = root / "scheduled-data"
    args = [manager, "--data-dir", str(data), "--server", server]
    try:
        run(args + ["autostart", "install"])
        assert json.loads(run(args + ["autostart", "status"])) == {
            "registered": True, "enabled": True, "actionMatches": True
        }
        update_status = json.loads(run(args + ["update", "status"]))
        assert update_status["settings"]["policy"] == "automatic"
        assert not update_status["schedule"]["registered"]
        assert not update_status["schedule"]["enabled"]
        run(args + ["update", "schedule", "reconcile"])
        update_status = json.loads(run(args + ["update", "status"]))
        assert update_status["schedule"] == {
            "registered": True, "enabled": True, "actionMatches": True
        }
        run(args + ["start"])
        assert json.loads(run(args + ["status"]))["devices"] == []
        screen = run(args, input="1\n\n0\n")
        assert "서버 파일 용량" in screen and "번호 선택" in screen
        keyboard_menu(args)
        run(args + ["autostart", "remove"])
        assert not json.loads(run(args + ["autostart", "status"]))["registered"]
        update_status = json.loads(run(args + ["update", "status"]))
        assert update_status["schedule"] == {
            "registered": True, "enabled": True, "actionMatches": True
        }
        assert json.loads(run(args + ["status"]))["devices"] == []

        run(args + ["update", "policy", "off"])
        update_status = json.loads(run(args + ["update", "status"]))
        assert update_status["settings"]["policy"] == "off"
        assert not update_status["schedule"]["registered"]
        assert not update_status["schedule"]["enabled"]
        assert not json.loads(run(args + ["autostart", "status"]))["registered"]
        assert json.loads(run(args + ["status"]))["devices"] == []
        run(args + ["stop"])
        assert not (data / "management-session").exists()
        print("PASS: Linux independent user systemd scheduling, numeric TUI, removal preserves daemon, graceful stop")
    finally:
        run(args + ["update", "policy", "off"])
        run(args + ["prepare-update"])
        run(args + ["autostart", "remove"])
