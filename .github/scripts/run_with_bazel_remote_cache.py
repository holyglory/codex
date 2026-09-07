"""Run a build through a cache-only SSH tunnel; never retry the build itself."""

import argparse
from contextlib import contextmanager, ExitStack
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
from urllib.request import urlopen


KEY_ENV = "CODEX_BAZEL_CACHE_SSH_KEY"
HOST_KEY_ENV = "CODEX_BAZEL_CACHE_HOST_KEY"
HOST_ENV = "CODEX_BAZEL_CACHE_HOST"
URL_ENV = "CODEX_BAZEL_REMOTE_CACHE_URL"


def private_file(path: Path, contents: str) -> None:
    with open(
        path, "x", opener=lambda name, flags: os.open(name, flags, 0o600)
    ) as file:
        file.write(contents.rstrip() + "\n")


def ssh_command(root: Path, host: str, port: int) -> list[str]:
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9.-]{0,252}", host):
        raise ValueError("Invalid cache SSH hostname")
    return [
        "ssh",
        "-F",
        "/dev/null",
        "-NT",
        "-i",
        str(root / "key"),
        "-o",
        "BatchMode=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        f"UserKnownHostsFile={root / 'known_hosts'}",
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "HostKeyAlias=codex-bazel-cache",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-L",
        f"127.0.0.1:{port}:127.0.0.1:9095",
        f"codex-bazel-cache@{host}",
    ]


def ready(process: subprocess.Popen, url: str, deadline: float) -> bool:
    while process.poll() is None and time.monotonic() < deadline:
        try:
            with urlopen(url + "/status", timeout=0.5) as response:
                if response.status == 200:
                    return True
        except OSError:
            pass
        time.sleep(0.1)
    return False


@contextmanager
def remote_cache(environment: dict[str, str]):
    # Neither build tools nor their tests inherit the cache credential.
    child_env = {
        key: value
        for key, value in environment.items()
        if key not in (KEY_ENV, HOST_KEY_ENV, URL_ENV)
    }
    configured = all(environment.get(key) for key in (KEY_ENV, HOST_KEY_ENV, HOST_ENV))
    if not configured:
        print(
            "::warning::Persistent Bazel cache is not configured; compiling locally.",
            flush=True,
        )
        yield child_env
        return
    with tempfile.TemporaryDirectory(
        prefix="bazel-tunnel-", dir=environment.get("RUNNER_TEMP")
    ) as directory:
        root = Path(directory)
        private_file(root / "key", environment[KEY_ENV])
        private_file(root / "known_hosts", environment[HOST_KEY_ENV])
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        process = None
        try:
            with (root / "ssh.log").open("wb") as log:
                try:
                    process = subprocess.Popen(
                        ssh_command(root, environment[HOST_ENV], port),
                        stdin=subprocess.DEVNULL,
                        stdout=log,
                        stderr=log,
                        env=child_env,
                    )
                    base_url = f"http://127.0.0.1:{port}"
                    if ready(process, base_url, time.monotonic() + 15):
                        child_env[URL_ENV] = base_url + "/holyglory-codex-v1"
                        print(
                            "Persistent Bazel cache connected over restricted SSH.",
                            flush=True,
                        )
                    else:
                        print(
                            "::warning::Persistent Bazel cache unavailable; compiling locally.",
                            flush=True,
                        )
                except (OSError, ValueError):
                    # Never print credentials, SSH output, or exceptions containing inputs.
                    print(
                        "::warning::Persistent Bazel cache setup failed; compiling locally.",
                        flush=True,
                    )
                yield child_env
        finally:
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def run(
    command: list[str], environment: dict[str, str], *, require_cache: bool = False
) -> int:
    with ExitStack() as stack:
        try:
            child_env = stack.enter_context(remote_cache(environment))
        except (OSError, ValueError):
            print(
                "::warning::Persistent Bazel cache setup failed; compiling locally.",
                flush=True,
            )
            child_env = {
                key: value
                for key, value in environment.items()
                if key not in (KEY_ENV, HOST_KEY_ENV, URL_ENV)
            }
        if require_cache and URL_ENV not in child_env:
            return 1
        return subprocess.run(command, env=child_env, check=False).returncode


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--require-cache", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("A build command is required")
    raise SystemExit(run(command, dict(os.environ), require_cache=args.require_cache))
