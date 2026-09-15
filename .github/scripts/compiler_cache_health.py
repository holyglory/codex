"""Wait for asynchronous compiler-cache writes and report their actual health."""

import json
import os
import argparse
from pathlib import Path
import subprocess
import time


def cache_summary(document: dict) -> dict:
    stats = document["stats"]
    return {
        "backend": document["cache_location"].split(",", 1)[0],
        "version": document["version"],
        "rust_hits": stats["cache_hits"]["counts"].get("Rust", 0),
        "rust_misses": stats["cache_misses"]["counts"].get("Rust", 0),
        "writes": stats["cache_writes"],
        "write_errors": stats["cache_write_errors"],
        "read_errors": stats["cache_read_errors"],
        "timeouts": stats["cache_timeouts"],
        "compiler_errors": sum(stats["cache_errors"]["counts"].values()),
        "pending_writes": max(
            0,
            sum(stats["cache_misses"]["counts"].values())
            - stats["cache_writes"]
            - stats["cache_write_errors"],
        ),
    }


def health_failures(summary: dict, expected_backend: str = "ghac") -> list[str]:
    failures = []
    if summary["backend"] != expected_backend:
        failures.append(f"The expected {expected_backend} cache backend was not used")
    if summary["rust_hits"] + summary["rust_misses"] == 0:
        failures.append("No Rust compiler-cache activity was observed")
    for counter in (
        "write_errors",
        "read_errors",
        "timeouts",
        "pending_writes",
    ):
        if summary[counter]:
            failures.append(f"{counter}={summary[counter]}")
    return failures


def wait_for_cache_writes(
    environment: dict[str, str], timeout_seconds: int = 360
) -> dict:
    deadline = time.monotonic() + timeout_seconds
    delay = 0.5
    while True:
        document = json.loads(
            subprocess.check_output(
                [
                    environment.get("SCCACHE_PATH", "sccache"),
                    "--show-stats",
                    "--stats-format=json",
                ],
                env=environment,
                timeout=30,
            )
        )
        if (
            cache_summary(document)["pending_writes"] == 0
            or time.monotonic() >= deadline
        ):
            return document
        time.sleep(min(delay, max(0, deadline - time.monotonic())))
        delay = min(delay * 2, 5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", type=Path)
    parser.add_argument("--backend", choices=("ghac", "bazel-http"), default="ghac")
    args = parser.parse_args()
    summary = (
        json.loads(args.file.read_text())["summary"]
        if args.file
        else cache_summary(wait_for_cache_writes(dict(os.environ)))
    )
    failures = health_failures(summary, args.backend)
    print(json.dumps({**summary, "failures": failures}), flush=True)
    raise SystemExit(int(bool(failures)))
