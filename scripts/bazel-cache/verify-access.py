"""Prove that the dedicated SSH identity can reach only the loopback cache."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.request import urlopen

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github/scripts"))
from run_with_bazel_remote_cache import (
    HOST_ENV,
    HOST_KEY_ENV,
    KEY_ENV,
    URL_ENV,
    private_file,
    remote_cache,
    ssh_command,
)


def verify(host: str, key: Path, host_key: Path) -> dict:
    public_key = " ".join(host_key.read_text().split()[:2])
    environment = dict(os.environ) | {
        HOST_ENV: host,
        KEY_ENV: key.read_text(),
        HOST_KEY_ENV: f"codex-bazel-cache {public_key}",
    }
    with remote_cache(environment) as child_env:
        if URL_ENV not in child_env:
            raise RuntimeError("Positive cache access failed")
        status_url = child_env[URL_ENV].rsplit("/", 1)[0] + "/status"
        with urlopen(status_url, timeout=5) as response:
            status = json.load(response)
        if status["MaxSize"] != 900 * 1024**3:
            raise RuntimeError("Cache retention is not the approved value")
    with tempfile.TemporaryDirectory(prefix="cache-access-") as directory:
        root = Path(directory)
        private_file(root / "key", environment[KEY_ENV])
        private_file(root / "known_hosts", environment[HOST_KEY_ENV])
        command = ssh_command(root, host, 1)
        # Remove the allowed local-forward arguments; retain all identity checks.
        base, destination = command[:-3], command[-1]
        command_base = ["-T" if item == "-NT" else item for item in base]
        attempts = {
            "shell_denied": (command_base + [destination, "id"], 1, None),
            "other_destination_denied": (
                base + ["-W", "127.0.0.1:22", destination],
                255,
                "administratively prohibited",
            ),
            "remote_listener_denied": (
                base + ["-R", "127.0.0.1:0:127.0.0.1:9095", destination],
                255,
                "remote port forwarding failed",
            ),
        }
        for name, (arguments, expected, diagnostic) in attempts.items():
            result = subprocess.run(
                arguments, capture_output=True, text=True, timeout=15, check=False
            )
            if result.returncode != expected or (
                diagnostic and diagnostic not in result.stderr
            ):
                raise RuntimeError(
                    f"SSH restriction did not pass: {name} (exit {result.returncode})"
                )
    return {
        "cache_access": "passed",
        "shell_denied": True,
        "other_destination_denied": True,
        "remote_listener_denied": True,
        "retained_bytes": status["MaxSize"],
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--host-key", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(verify(args.host, args.key, args.host_key)))
