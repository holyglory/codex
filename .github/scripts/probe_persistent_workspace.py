"""Prove a real workspace crate is restored from the persistent compiler cache."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
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
if os.name == "nt":
    # Proc-macro DLLs are linked locally. Compare their public-file digests and
    # PE timestamps across runners before attributing dependent misses to storage.
    linked_inputs = {}
    directory = Path(os.environ["CARGO_TARGET_DIR"]) / "release" / "deps"
    for path in sorted(directory.glob("*.dll")):
        with path.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
            source.seek(0x3C)
            pe_offset = struct.unpack("<I", source.read(4))[0]
            source.seek(pe_offset + 8)
            timestamp = struct.unpack("<I", source.read(4))[0]
        linked_inputs[path.name] = {"sha256": digest, "pe_timestamp": timestamp}
    (Path(os.environ["RUNNER_TEMP"]) / "persistent-compiler-link-inputs.json").write_text(
        json.dumps(linked_inputs, sort_keys=True) + "\n"
    )
summary = cache_summary(wait_for_cache_writes(dict(os.environ), timeout_seconds=900))
failures = health_failures(summary, "bazel-http")
if args.mode == "consume" and (
    summary["rust_hits"] == 0 or summary["rust_misses"] != 0
):
    failures.append("The fresh runner did not restore every cacheable Rust compilation")
print(json.dumps({"mode": args.mode, **summary, "failures": failures}), flush=True)
raise SystemExit(int(bool(failures)))
