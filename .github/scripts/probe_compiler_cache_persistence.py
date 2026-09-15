"""Prove hosted compiler-cache writes and reuse with independent runner state."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time

from compiler_cache_write_diagnostics import cache_write_diagnostics


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
    }


def verify_summary(summary: dict, mode: str, count: int) -> list[str]:
    failures = []
    if summary["backend"] != "ghac":
        failures.append("The GitHub Actions cache backend was not used")
    for counter in ("write_errors", "read_errors", "timeouts", "errors"):
        if summary[counter]:
            failures.append(f"{counter}={summary[counter]}")
    if mode == "populate" and summary["writes"] < count:
        failures.append(f"Expected {count} successful cache writes")
    if mode == "consume" and (
        summary["rust_hits"] < count or summary["rust_misses"] != 0
    ):
        failures.append(f"Expected {count} Rust cache hits without recompilation")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("populate", "consume"), required=True)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--workdir", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--count", type=int, default=512)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9]+-[0-9]+", args.namespace):
        parser.error("Namespace must identify the workflow run and attempt")
    if not 1 <= args.count <= 512:
        parser.error("Count must be between 1 and 512")
    if os.environ.get("CARGO_INCREMENTAL") != "0":
        parser.error("Incremental compilation must be disabled")
    if Path(os.environ.get("RUSTC_WRAPPER", "")).name != "sccache":
        parser.error("Cargo must use the sccache compiler wrapper")

    args.workdir.mkdir(parents=True, exist_ok=False)
    members = []
    seed = int(args.namespace.split("-", 1)[0])
    for index in range(args.count):
        name = f"cache_probe_{index:04}"
        members.append(name)
        crate = args.workdir / name
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "2024"\n'
        )
        (crate / "src/lib.rs").write_text(
            f'pub const NAMESPACE: &str = "{args.namespace}";\n'
            f"pub fn value(input: u64) -> u64 {{ input.wrapping_add({seed})"
            f".rotate_left({index % 64}) }}\n"
        )
    (args.workdir / "Cargo.toml").write_text(
        f'[workspace]\nresolver = "3"\nmembers = {json.dumps(members)}\n'
        '[profile.release]\nlto = "thin"\ndebug = "line-tables-only"\n'
        'split-debuginfo = "packed"\nstrip = false\ncodegen-units = 4\n'
    )
    with cache_write_diagnostics(args.workdir, dict(os.environ)) as (
        environment,
        write_diagnostics,
    ):
        subprocess.run(["sccache", "--zero-stats"], env=environment, check=True)
        result = subprocess.run(
            [
                "cargo",
                "build",
                "--offline",
                "--release",
                "--workspace",
                "--manifest-path",
                str(args.workdir / "Cargo.toml"),
                "--target",
                args.target,
            ],
            env=environment,
            check=False,
        )
        deadline = time.monotonic() + 360
        delay = 0.5
        while True:
            document = json.loads(
                subprocess.check_output(
                    ["sccache", "--show-stats", "--stats-format=json"], env=environment
                )
            )
            stats = document["stats"]
            # The compiler response can precede its asynchronous cache write.
            if (
                stats["cache_writes"] + stats["cache_write_errors"]
                >= stats["compilations"]
                or time.monotonic() >= deadline
            ):
                break
            time.sleep(min(delay, max(0, deadline - time.monotonic())))
            delay = min(delay * 2, 5)
    summary = cache_summary(document)
    failures = verify_summary(summary, args.mode, args.count)
    if result.returncode:
        failures.append(f"Compilation exited with status {result.returncode}")
    evidence = {
        "mode": args.mode,
        "count": args.count,
        **summary,
        "write_diagnostics": dict(write_diagnostics),
        "failures": failures,
    }
    args.evidence.mkdir(parents=True, exist_ok=True)
    (args.evidence / "cache-proof.json").write_text(
        json.dumps(evidence, indent=2) + "\n"
    )
    print(json.dumps(evidence), flush=True)
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
