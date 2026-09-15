"""Prove a real workspace crate is restored from the persistent compiler cache."""

import argparse
import json
import os
from pathlib import Path
import subprocess

from compiler_cache_health import cache_summary, health_failures, wait_for_cache_writes

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--mode", choices=("populate", "consume"), required=True)
parser.add_argument("--target", required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
subprocess.run(
    [
        "cargo",
        "build",
        "--locked",
        "--release",
        "-p",
        "codex-utils-absolute-path",
        "--target",
        args.target,
    ],
    cwd=root / "codex-rs",
    check=True,
)
summary = cache_summary(wait_for_cache_writes(dict(os.environ), timeout_seconds=900))
failures = health_failures(summary, "bazel-http")
if args.mode == "consume" and (
    summary["rust_hits"] == 0 or summary["rust_misses"] != 0
):
    failures.append("The fresh runner did not restore every cacheable Rust compilation")
print(json.dumps({"mode": args.mode, **summary, "failures": failures}), flush=True)
raise SystemExit(int(bool(failures)))
