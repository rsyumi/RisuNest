"""Synthetic Linux process, user systemd, and local installer integration.

Run with explicit daemon and manager paths. Never opens installed application data.
"""

import json
import hashlib
import os
from pathlib import Path
import pty
import select
import shutil
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
        run(args + ["start"])
        assert json.loads(run(args + ["status"]))["devices"] == []
        screen = run(args, input="1\n\n0\n")
        assert "서버 파일 용량" in screen and "번호 선택" in screen
        keyboard_menu(args)
        run(args + ["autostart", "remove"])
        assert not json.loads(run(args + ["autostart", "status"]))["registered"]
        assert json.loads(run(args + ["status"]))["devices"] == []
        run(args + ["stop"])
        assert not (data / "management-session").exists()
        print("PASS: Linux user systemd, numeric TUI, removal preserves daemon, graceful stop")
    finally:
        run(args + ["prepare-update"])
        run(args + ["autostart", "remove"])

    home = root / "isolated-home"
    home.mkdir()
    env = dict(os.environ, HOME=str(home), XDG_DATA_HOME=str(home / "data"),
               XDG_CONFIG_HOME=str(home / "config"))
    archive = root / "archive"
    archive.mkdir()
    shutil.copy2(server, archive / "risunest-sync-server")
    shutil.copy2(manager, archive / "risunest-sync-manager")
    # This test does not start a Tunnel. Only verify the installer's local file contract.
    (archive / "cloudflared").write_text("#!/bin/sh\nexit 99\n")
    (archive / "cloudflared").chmod(0o700)
    (archive / "CLOUDFLARED-LICENSE").write_text("Synthetic installer fixture\n")
    installer = Path(__file__).resolve().parents[1] / "install/install.sh"
    shutil.copy2(installer, archive / "install.sh")
    cloudflared_hash = hashlib.sha256((archive / "cloudflared").read_bytes()).hexdigest()
    (archive / "risunest-sync-bundle.json").write_text(json.dumps({
        "schema": "risunest-sync-bundle/v1",
        "product": "sync",
        "variant": "managed",
        "version": "0.1.0",
        "protocolId": "risunest-sync/v1",
        "storeFormatId": "risunest-sync-store/v8",
        "files": [
            "CLOUDFLARED-LICENSE",
            "cloudflared",
            "install.sh",
            "risunest-sync-manager",
            "risunest-sync-server",
        ],
        "vendor": [{
            "name": "cloudflared",
            "version": "synthetic",
            "os": "linux",
            "arch": "x86_64" if os.uname().machine == "x86_64" else "aarch64",
            "sha256": cloudflared_hash,
            "path": "cloudflared",
        }],
    }, indent=2) + "\n")
    installed = home / ".local/bin/risunest-sync-manager"
    try:
        run(["sh", str(archive / "install.sh"), "--non-interactive"], env=env, input="")
        assert installed.is_file()
        assert json.loads(run([str(installed), "status"], env=env))["devices"] == []
        run([str(installed), "uninstall"], env=env)
        assert (home / "data/risunest-sync/metadata.sqlite").is_file()
        assert not (home / "data/risunest-sync/management-session").exists()
        print("PASS: local sh installation, user bin wrapper, uninstall preserves synthetic data")
    finally:
        if installed.exists():
            run([str(installed), "prepare-update"], env=env)
