"""Prove hosted compiler-cache writes and reuse with independent runner state."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess

from compiler_cache_health import cache_summary
from compiler_cache_health import health_failures
from compiler_cache_health import wait_for_cache_writes
from compiler_cache_write_diagnostics import cache_write_diagnostics


def verify_summary(summary: dict, mode: str, count: int) -> list[str]:
    failures = health_failures(summary)
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
    if not 1 <= args.count <= 4096:
        parser.error("Count must be between 1 and 4096")
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
    # The action proves startup before compilation. Restart only this job's
    # cache daemon to enable the credential-safe diagnostic stream below.
    subprocess.run([os.environ["SCCACHE_PATH"], "--stop-server"], check=True)
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
        document = wait_for_cache_writes(environment)
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
