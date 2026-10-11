"""Prove compiled-output reuse from fresh Bazel state, plus invalidation/outage."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def remote_hits(output: str) -> int:
    matches = re.findall(
        r"^INFO: \d+ processes:.*?\b(\d+) remote cache hit", output, re.MULTILINE
    )
    return max(map(int, matches), default=0)


def probe(mode: str, namespace: str, bazel: str, evidence: Path) -> dict:
    remote = os.environ.get("CODEX_BAZEL_REMOTE_CACHE_URL")
    if not remote or not re.fullmatch(r"[a-zA-Z0-9_-]{1,80}", namespace):
        raise ValueError("A connected cache and bounded probe namespace are required")
    executable = shutil.which(bazel)
    if not executable:
        raise ValueError("Bazel executable is unavailable")
    evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="cache-proof-", dir=os.environ.get("RUNNER_TEMP")
    ) as directory:
        root = Path(directory) / "workspace"
        root.mkdir()
        (root / "MODULE.bazel").write_text('module(name = "cache_proof")\n')
        compiler = Path("/usr/bin/cc").resolve()
        compiler_digest = hashlib.sha256(compiler.read_bytes()).hexdigest()
        (root / "compiler.identity").write_text(compiler_digest + "\n")
        (root / "BUILD.bazel").write_text(
            'genrule(name = "probe", srcs = ["probe.c", "compiler.identity"], '
            'outs = ["probe.o"], cmd = "/usr/bin/cc -c $(location probe.c) -o $@")\n'
        )

        def build(phase: str, marker: str, url: str, upload: bool) -> tuple[str, int]:
            (root / "probe.c").write_text(f'const char cache_probe[] = "{marker}";\n')
            command = [
                executable,
                "--batch",
                "--nosystem_rc",
                "--nohome_rc",
                "--noworkspace_rc",
                f"--output_user_root={root.parent / phase}",
                "build",
                "--disk_cache=",
                f"--remote_cache={url}",
                f"--remote_upload_local_results={str(upload).lower()}",
                "--remote_verify_downloads=true",
                "--remote_download_outputs=all",
                "--remote_timeout=1" if phase == "outage" else "--remote_timeout=30",
                "--remote_retries=0",
                "--color=no",
                "--curses=no",
                "//:probe",
            ]
            result = subprocess.run(
                command,
                cwd=root,
                capture_output=True,
                text=True,
                timeout=180,
                check=False,
            )
            output = result.stdout + result.stderr
            (evidence / f"{mode}-{phase}.log").write_text(output)
            if result.returncode:
                raise RuntimeError(
                    f"Cache proof failed during {phase}; see {evidence / f'{mode}-{phase}.log'}"
                )
            content = (root / "bazel-bin/probe.o").read_bytes()
            if not content.startswith(b"\x7fELF") or marker.encode() not in content:
                raise RuntimeError("Bazel did not return the requested compiled object")
            return hashlib.sha256(content).hexdigest(), remote_hits(output)

        url = remote + "/proof-" + namespace
        original, hits = build("baseline", "CACHE_PROBE_V1", url, mode == "populate")
        report = {
            "mode": mode,
            "compiler_sha256": compiler_digest,
            "object_sha256": original,
            "remote_hits": hits,
        }
        if mode == "consume":
            if hits < 1:
                raise RuntimeError(
                    "Fresh consumer compiled locally instead of reusing the remote object"
                )
            changed, changed_hits = build("changed", "CACHE_PROBE_V2", url, False)
            if changed == original or changed_hits:
                raise RuntimeError(
                    "Changed input incorrectly reused the previous compiled object"
                )
            recovered, outage_hits = build(
                "outage", "CACHE_PROBE_V1", "http://127.0.0.1:1", False
            )
            if recovered != original or outage_hits:
                raise RuntimeError(
                    "Unavailable cache did not fall back to the correct local build"
                )
            report.update({"invalidation": "passed", "outage_fallback": "passed"})
        (evidence / f"{mode}.json").write_text(json.dumps(report, indent=2) + "\n")
        return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", required=True, choices=("populate", "consume"))
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--bazel", default="bazel")
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(probe(args.mode, args.namespace, args.bazel, args.evidence)))
