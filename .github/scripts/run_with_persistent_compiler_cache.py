"""Run a build through the approved cache tunnel and retain its health receipt."""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile

from compiler_cache_health import cache_summary, health_failures, wait_for_cache_writes
from run_with_bazel_remote_cache import URL_ENV, remote_cache


def run(
    command: list[str],
    stats_file: Path,
    environment: dict[str, str],
    *,
    defer_health: bool,
) -> int:
    with remote_cache(environment) as child:
        if URL_ENV not in child:
            raise RuntimeError("The approved persistent cache connection is required")
        child["SCCACHE_BAZEL_REMOTE_CACHE_URL"] = child[URL_ENV] + "-compiler-v1"
        child["RUSTC_WRAPPER"] = child["SCCACHE_PATH"]
        with tempfile.TemporaryDirectory(
            prefix="sc-", dir="/tmp" if os.name == "posix" else None
        ) as temporary:
            if os.name == "posix":
                child["SCCACHE_SERVER_UDS"] = str(Path(temporary) / "server.sock")
            else:
                with socket.socket() as listener:
                    listener.bind(("127.0.0.1", 0))
                    child["SCCACHE_SERVER_PORT"] = str(listener.getsockname()[1])
            # Only the cache daemon's startup sockets need short temporary paths.
            # Product compilers retain their existing environment and build paths.
            cache_environment = dict(
                child, TMPDIR=temporary, TEMP=temporary, TMP=temporary
            )
            binary = child["SCCACHE_PATH"]
            server_started = False
            try:
                subprocess.run(
                    [binary, "--start-server"],
                    env=cache_environment,
                    check=True,
                    timeout=360,
                )
                server_started = True
                result = subprocess.run(command, env=child, check=False).returncode
                summary = cache_summary(
                    wait_for_cache_writes(cache_environment, timeout_seconds=900)
                )
                failures = health_failures(summary, "bazel-http")
                stats_file.parent.mkdir(parents=True, exist_ok=True)
                with stats_file.open("x") as output:
                    json.dump({"summary": summary, "failures": failures}, output)
                    output.write("\n")
                print(
                    json.dumps({"persistent_cache": summary, "failures": failures}),
                    flush=True,
                )
                if failures:
                    print(
                        "::warning::Compiler-cache health failed; retained for final verification.",
                        flush=True,
                    )
                return result or (int(bool(failures)) if not defer_health else 0)
            finally:
                stopped = subprocess.run(
                    [binary, "--stop-server"],
                    env=cache_environment,
                    check=False,
                    timeout=30,
                )
                if server_started and stopped.returncode:
                    raise RuntimeError("The owned compiler-cache daemon did not stop")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stats-file", type=Path, required=True)
    parser.add_argument("--defer-health", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("A build command is required")
    raise SystemExit(
        run(command, args.stats_file, dict(os.environ), defer_health=args.defer_health)
    )
