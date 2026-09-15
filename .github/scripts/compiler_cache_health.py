"""Wait for asynchronous compiler-cache writes and report their actual health."""

import json
import os
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
        "errors": sum(stats["cache_errors"]["counts"].values()),
        "pending_writes": max(
            0,
            stats["compilations"] - stats["cache_writes"] - stats["cache_write_errors"],
        ),
    }


def health_failures(summary: dict) -> list[str]:
    failures = []
    if summary["backend"] != "ghac":
        failures.append("The GitHub Actions cache backend was not used")
    if summary["rust_hits"] + summary["rust_misses"] == 0:
        failures.append("No Rust compiler-cache activity was observed")
    for counter in (
        "write_errors",
        "read_errors",
        "timeouts",
        "errors",
        "pending_writes",
    ):
        if summary[counter]:
            failures.append(f"{counter}={summary[counter]}")
    return failures


def wait_for_cache_writes(environment: dict[str, str]) -> dict:
    deadline = time.monotonic() + 360
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
    summary = cache_summary(wait_for_cache_writes(dict(os.environ)))
    failures = health_failures(summary)
    print(json.dumps({**summary, "failures": failures}), flush=True)
    raise SystemExit(int(bool(failures)))
