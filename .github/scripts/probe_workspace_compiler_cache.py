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
parser.add_argument("--full-command", action="store_true")
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
    if args.full_command:
        environment["CACHE_STOP_AFTER_SNAPSHOT"] = "1"
    subprocess.run(
        [os.environ["SCCACHE_PATH"], "--zero-stats"], env=environment, check=True
    )
    command = [
        "cargo",
        "build",
        "--locked",
        "--release",
        "--target",
        "x86_64-unknown-linux-musl",
    ]
    if args.full_command:
        command += [
            "--bin",
            "codex",
            "--bin",
            "codex-app-server",
            "--bin",
            "codex-code-mode-host",
        ]
    else:
        command += ["-p", "codex-utils-absolute-path"]
    result = subprocess.run(
        command,
        cwd=root / "codex-rs",
        env=environment,
        check=False,
    )
    summary = cache_summary(wait_for_cache_writes(environment))
failures = health_failures(summary)
if args.full_command:
    if (
        result.returncode == 0
        or not (
            args.evidence
            / "inputs/codex_utils_absolute_path-x86_64-unknown-linux-musl.json"
        ).is_file()
    ):
        failures.append(
            "Expected the controlled diagnostic stop after recording the full-build inputs"
        )
elif result.returncode:
    failures.append(f"Compilation exited with {result.returncode}")
if (
    not args.full_command
    and args.mode == "consume"
    and (summary["rust_hits"] == 0 or summary["rust_misses"])
):
    failures.append("The fresh runner recompiled cacheable Rust crates")
report = {
    "full_command_snapshot_only": args.full_command,
    "mode": args.mode,
    **summary,
    "write_diagnostics": dict(diagnostics),
    "failures": failures,
}
(args.evidence / "workspace-cache.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report), flush=True)
raise SystemExit(int(bool(failures)))
