"""Verify cache reuse for a real workspace crate under the release environment."""

import argparse
import json
import os
from pathlib import Path
import subprocess

from compiler_cache_health import cache_summary, health_failures, wait_for_cache_writes
from compiler_cache_write_diagnostics import cache_write_diagnostics

parser = argparse.ArgumentParser()
parser.add_argument("--mode", choices=("populate", "consume"), required=True)
parser.add_argument("--evidence", type=Path, required=True)
args = parser.parse_args()
args.evidence.mkdir(parents=True, exist_ok=False)
root = Path(__file__).resolve().parents[2]
wrapper = root / ".github/scripts/trace_compiler_cache_inputs.py"
subprocess.run([os.environ["SCCACHE_PATH"], "--stop-server"], check=True)
with cache_write_diagnostics(args.evidence, dict(os.environ)) as (
    environment,
    diagnostics,
):
    environment.update(
        RUSTC_WRAPPER=str(wrapper), CACHE_KEY_EVIDENCE=str(args.evidence / "inputs")
    )
    subprocess.run(
        [os.environ["SCCACHE_PATH"], "--zero-stats"], env=environment, check=True
    )
    result = subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "-p",
            "codex-utils-absolute-path",
            "--target",
            "x86_64-unknown-linux-musl",
        ],
        cwd=root / "codex-rs",
        env=environment,
        check=False,
    )
    summary = cache_summary(wait_for_cache_writes(environment))
failures = health_failures(summary)
if result.returncode:
    failures.append(f"Compilation exited with {result.returncode}")
if args.mode == "consume" and (summary["rust_hits"] == 0 or summary["rust_misses"]):
    failures.append("The fresh runner recompiled cacheable Rust crates")
report = {
    "mode": args.mode,
    **summary,
    "write_diagnostics": dict(diagnostics),
    "failures": failures,
}
(args.evidence / "workspace-cache.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report), flush=True)
raise SystemExit(int(bool(failures)))
