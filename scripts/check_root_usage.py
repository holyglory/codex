#!/usr/bin/env python3
"""Exercise installed usage storage in a disposable UID-0 environment."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import queue
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time


def run(command, environment=None):
    result = subprocess.run(
        command, env=environment, capture_output=True, text=True, timeout=45
    )
    if result.returncode:
        raise RuntimeError(f"fixture command failed with exit {result.returncode}")
    return result.stdout


class Server:
    def __init__(self, binary, environment, workspace, *, stderr=None):
        self.process = subprocess.Popen(
            [str(binary), "app-server"],
            cwd=workspace,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL if stderr is None else stderr,
            text=True,
        )
        self.messages = queue.Queue()
        self.counter = 0
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        try:
            for line in self.process.stdout:
                self.messages.put(json.loads(line))
        finally:
            self.messages.put(None)

    def receive(self, *, timeout=30.0):
        message = self.messages.get(timeout=timeout)
        if message is None:
            raise RuntimeError("app-server exited before its response")
        return message

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def rpc(self, method, params):
        self.counter += 1
        self.send({"id": self.counter, "method": method, "params": params})
        while True:
            message = self.receive()
            if message.get("id") == self.counter:
                if "error" in message:
                    raise RuntimeError(
                        f"{method}: error code {message['error']['code']}"
                    )
                return message["result"]

    def __enter__(self):
        try:
            self.rpc(
                "initialize",
                {
                    "clientInfo": {"name": "root_usage_fixture", "version": "1"},
                    "capabilities": {"experimentalApi": True},
                },
            )
            self.send({"method": "initialized"})
        except BaseException:
            self.__exit__()
            raise
        return self

    def __exit__(self, *_args):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=15)
        self.reader.join(timeout=5)


def private_storage(home):
    usage = home / "usage"
    assert usage.stat().st_uid == 0 and stat.S_IMODE(usage.stat().st_mode) == 0o700
    for suffix in ("", "-wal", "-shm"):
        path = usage / ("usage.sqlite3" + suffix)
        assert path.is_file(), "database and live WAL sidecars must exist"
        info = path.stat()
        assert info.st_uid == 0 and stat.S_IMODE(info.st_mode) == 0o600


def inside(binary, scratch):
    assert os.geteuid() == 0
    assert Path(pwd.getpwuid(0).pw_dir) == Path("/root")
    repository = Path(__file__).resolve().parents[1]
    sys.path[:0] = [
        str(repository / "sdk/python/tests"),
        str(repository / "sdk/python/src"),
    ]
    from app_server_harness import AppServerHarness

    for name, explicit in (("passwd", False), ("override", True)):
        case = scratch / name
        case.mkdir()
        with AppServerHarness(case) as fixture:
            home = fixture.codex_home if explicit else Path("/root/.codex")
            if not explicit:
                home.mkdir(mode=0o700)
                shutil.copy2(fixture.codex_home / "config.toml", home / "config.toml")
            inherited = case / "inherited"
            inherited.mkdir()
            environment = dict(
                os.environ,
                HOME=str(inherited),
                CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG="1",
                RUST_LOG="error",
            )
            environment.pop("CODEX_HOME", None)
            if explicit:
                environment["CODEX_HOME"] = str(home)
            doctor = json.loads(
                run([str(binary), "usage", "doctor", "--json"], environment)
            )
            assert doctor["integrity"] == "ok" and doctor["migrationCount"] == 9
            assert (home / "usage/usage.sqlite3").exists(), (
                "CLI selected inherited HOME instead of passwd home"
            )
            assert not (inherited / ".codex").exists(), "CLI touched inherited HOME"
            fixture.responses.enqueue_assistant_message("ROOT_USAGE_OK")
            with Server(binary, environment, fixture.workspace) as server:
                thread = server.rpc(
                    "thread/start",
                    {
                        "cwd": str(fixture.workspace),
                        "approvalPolicy": "never",
                        "sandbox": "read-only",
                    },
                )["thread"]["id"]
                server.rpc(
                    "turn/start",
                    {
                        "threadId": thread,
                        "input": [{"type": "text", "text": "root usage fixture"}],
                    },
                )
                while True:
                    message = server.receive()
                    if message.get("method") == "turn/completed":
                        assert message["params"]["turn"]["status"] == "completed"
                        break
                params = {
                    "threadId": thread,
                    "fromAt": 0,
                    "toAt": int(time.time()) + 60,
                }
                first = server.rpc("localUsage/summary", params)
                assert first["report"]["counts"]["modelRequests"] >= 1
                assert first["aggregate"]["totalTokens"] == 2
                private_storage(home)
            with Server(binary, environment, fixture.workspace) as reopened:
                second = reopened.rpc("localUsage/summary", params)
                assert second["aggregate"]["totalTokens"] == 2
                assert second["report"]["counts"] == first["report"]["counts"]
                private_storage(home)
            with sqlite3.connect(
                f"file:{home}/usage/usage.sqlite3?mode=ro", uri=True
            ) as connection:
                assert connection.execute("PRAGMA integrity_check").fetchone() == (
                    "ok",
                )
                assert (
                    connection.execute("SELECT count(*) FROM operations").fetchone()[0]
                    > 0
                )
            assert not (inherited / ".codex").exists()
            print(
                json.dumps(
                    {
                        "case": name,
                        "uid": 0,
                        "capture": True,
                        "reopen_read": True,
                        "private_sidecars": True,
                    }
                ),
                flush=True,
            )
    with binary.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    print(json.dumps({"result": "passed", "binary_sha256": digest}))


def verify(binary):
    if sys.platform != "linux":
        raise RuntimeError("UID-0 installed-package check requires Linux")
    bwrap = shutil.which("bwrap")
    if bwrap is None:
        raise RuntimeError("bubblewrap is required")
    with tempfile.TemporaryDirectory(prefix="root-passwd-") as temporary:
        passwd = Path(temporary) / "passwd"
        passwd.write_text("root:x:0:0:fixture:/root:/bin/sh\n")
        # Keep the interpreter used by the outer fixture available after
        # bubblewrap clears the environment. Hosted runners do not have the
        # local candidate virtualenv path, so use the active interpreter as
        # the portable default.
        python_runner = os.environ.get("LOCAL_PACKAGE_PYTHON") or sys.executable
        command = [
            bwrap,
            "--die-with-parent",
            "--unshare-user",
            "--uid",
            "0",
            "--gid",
            "0",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/root",
            "--tmpfs",
            "/etc/codex",
            "--ro-bind",
            str(passwd),
            "/etc/passwd",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--chdir",
            "/tmp",
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/tmp/inherited",
            "--setenv",
            "LOCAL_PACKAGE_PYTHON",
            python_runner,
            "--setenv",
            "PYTHONDONTWRITEBYTECODE",
            "1",
            "--",
            python_runner,
            str(Path(__file__).resolve()),
            "--binary",
            str(binary.resolve()),
            "--inside",
        ]
        result = subprocess.run(command, capture_output=True, text=True, timeout=240)
        if result.returncode == 0:
            return
        if (
            "uid map" not in result.stderr
            and "Operation not permitted" not in result.stderr
        ):
            raise RuntimeError(f"root journey failed with exit {result.returncode}")
        sudo = shutil.which("sudo")
        if sudo is None:
            raise RuntimeError(
                "bubblewrap user namespace unavailable and sudo is missing"
            )
        inherited = Path(temporary) / "direct-inherited"
        inherited.mkdir()
        environment = dict(
            os.environ,
            HOME=str(inherited),
            LOCAL_PACKAGE_PYTHON=python_runner,
            PYTHONDONTWRITEBYTECODE="1",
        )
        environment.pop("CODEX_HOME", None)
        direct = subprocess.run(
            [
                sudo,
                "--preserve-env=HOME,LOCAL_PACKAGE_PYTHON,PYTHONDONTWRITEBYTECODE",
                python_runner,
                str(Path(__file__).resolve()),
                "--binary",
                str(binary.resolve()),
                "--direct",
            ],
            env=environment,
            timeout=240,
        )
        if direct.returncode:
            raise RuntimeError(
                f"direct UID-0 journey failed with exit {direct.returncode}"
            )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--direct", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--expect-failure", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        with tempfile.TemporaryDirectory(prefix="root-usage-") as temporary:
            inside(args.binary, Path(temporary))
    elif args.direct:
        if os.geteuid() != 0:
            raise RuntimeError("direct UID-0 fixture must run as root")
        with tempfile.TemporaryDirectory(prefix="root-direct-") as temporary:
            scratch = Path(temporary) / "scratch"
            scratch.mkdir()
            root_codex = Path("/root/.codex")
            backup = Path(temporary) / "existing-codex"
            had_existing = root_codex.exists() or root_codex.is_symlink()
            if had_existing:
                shutil.move(root_codex, backup)
            try:
                inside(args.binary, scratch)
            finally:
                if root_codex.is_symlink() or root_codex.is_file():
                    root_codex.unlink()
                elif root_codex.exists():
                    shutil.rmtree(root_codex)
                if had_existing:
                    shutil.move(backup, root_codex)
    else:
        if args.expect_failure:
            try:
                verify(args.binary)
            except (AssertionError, RuntimeError, subprocess.CalledProcessError):
                print(
                    json.dumps(
                        {"result": "expected_failure", "binary": str(args.binary)}
                    )
                )
                return
            raise RuntimeError("regression candidate unexpectedly passed")
        verify(args.binary)


if __name__ == "__main__":
    main()
