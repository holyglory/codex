"""Prove a real workspace crate is restored from the persistent compiler cache."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess

from compiler_cache_health import cache_summary, health_failures, wait_for_cache_writes

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--mode", choices=("populate", "consume"), required=True)
parser.add_argument("--target", required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
command = [
    "cargo",
    "build",
    "--locked",
    "--release",
    "-p",
    "codex-utils-absolute-path",
    "--target",
    args.target,
]
subprocess.run(command, cwd=root / "codex-rs", check=True)
if os.name == "nt":
    # Proc-macro DLLs are linked locally. Compare their public-file digests and
    # PE timestamps across runners before attributing dependent misses to storage.
    linked_inputs = {}
    directory = Path(os.environ["CARGO_TARGET_DIR"]) / "release" / "deps"
    retained = Path(os.environ["RUNNER_TEMP"]) / "persistent-compiler-linked"
    retained.mkdir()
    for path in sorted(directory.glob("*.dll")):
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
            source.seek(0x3C)
            pe_offset = struct.unpack("<I", source.read(4))[0]
            source.seek(pe_offset + 8)
            timestamp = struct.unpack("<I", source.read(4))[0]
        linked_inputs[path.name] = {"sha256": digest, "pe_timestamp": timestamp}
        shutil.copyfile(path, retained / path.name)
    (
        Path(os.environ["RUNNER_TEMP"]) / "persistent-compiler-link-inputs.json"
    ).write_text(json.dumps(linked_inputs, sort_keys=True) + "\n")
summary = cache_summary(wait_for_cache_writes(dict(os.environ), timeout_seconds=900))
failures = health_failures(summary, "bazel-http")
report = {"mode": args.mode, "fresh_runner": summary}
if args.mode == "consume":
    if summary["rust_hits"] == 0:
        failures.append("The fresh runner did not reuse any Rust dependency results")
    # Linker-produced proc-macro files may legitimately differ between runners.
    # Rebuild the real workspace leaf with its existing dependency files to
    # prove that identical inputs are restored, without ignoring changed bytes.
    subprocess.run(
        [
            "cargo",
            "clean",
            "--release",
            "-p",
            "codex-utils-absolute-path",
            "--target",
            args.target,
        ],
        cwd=root / "codex-rs",
        check=True,
    )
    subprocess.run([os.environ["SCCACHE_PATH"], "--zero-stats"], check=True)
    subprocess.run(command, cwd=root / "codex-rs", check=True)
    stable = cache_summary(wait_for_cache_writes(dict(os.environ), timeout_seconds=900))
    report["identical_inputs"] = stable
    failures.extend(health_failures(stable, "bazel-http"))
    if stable["rust_hits"] == 0 or stable["rust_misses"] != 0:
        failures.append("Identical workspace inputs were not restored from the cache")
report["failures"] = failures
(Path(os.environ["RUNNER_TEMP"]) / "persistent-compiler-reuse.json").write_text(
    json.dumps(report, sort_keys=True) + "\n"
)
print(json.dumps(report), flush=True)
raise SystemExit(int(bool(failures)))
