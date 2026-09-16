"""Reuse attested native builds only when their source and build inputs are unchanged."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

from stage_npm_packages import BINARY_TARGETS

REPOSITORY = "holyglory/codex"
WORKFLOW = ".github/workflows/downstream-candidate.yml"
SKIP_GUARD = "    if: inputs.native_source_run == ''\n"
RECOVERY_INPUT = (
    "      native_source_run:\n"
    "        description: Reuse a verified native candidate for packaging-only changes\n"
    "        type: string\n"
    '        default: ""\n'
)
PACKAGING_FILES = {
    WORKFLOW,
    "scripts/local_candidate.py",
    "scripts/stage_npm_packages.py",
    "scripts/test_stage_npm_packages.py",
    "scripts/reuse_native_candidate.py",
    "scripts/test_reuse_native_candidate.py",
}
BUILD_JOBS = ("preflight", "rust-validation", "bazel-validation", "native")
REQUIRED_JOBS = {
    "Validate candidate identity",
    "Critical producer and consumer regressions",
    "Complete Rust validation",
    "Complete Bazel validation",
    *(f"Build {target}" for target in BINARY_TARGETS),
}


def validate_run(run: dict, jobs: dict) -> str:
    if any(
        run.get(key) != value
        for key, value in {
            "status": "completed",
            "path": WORKFLOW,
            "event": "workflow_dispatch",
        }.items()
    ) or run.get("conclusion") not in ("success", "failure"):
        raise ValueError("Native producer is not a completed candidate workflow")
    for key in ("repository", "head_repository"):
        if (
            not isinstance(run.get(key), dict)
            or run[key].get("full_name") != REPOSITORY
        ):
            raise ValueError("Native producer belongs to a different repository")
    commit = run.get("head_sha", "")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Native producer has no valid source commit")
    if jobs.get("total_count") != len(jobs.get("jobs", [])):
        raise ValueError("Native producer job evidence is incomplete")
    for job in jobs["jobs"]:
        if job.get("name") not in (
            "Build and verify seven npm tarballs",
            "Candidate checks passed",
        ) and (job.get("status"), job.get("conclusion")) != ("completed", "success"):
            raise ValueError("Native producer has an unresolved non-packaging job")
    for name in REQUIRED_JOBS:
        matching = [job for job in jobs["jobs"] if job.get("name") == name]
        if len(matching) != 1 or any(
            matching[0].get(key) != value
            for key, value in {"status": "completed", "conclusion": "success"}.items()
        ):
            raise ValueError(f"Native producer did not pass {name}")
    return commit


def validate_changes(paths: set[str], before: str, after: str) -> None:
    if paths - PACKAGING_FILES:
        raise ValueError("Native reuse requires changes confined to packaging tooling")
    if before.partition("jobs:\n")[0] != after.partition("jobs:\n")[0].replace(
        RECOVERY_INPUT, ""
    ):
        raise ValueError("Global workflow inputs or build environment changed")
    patterns = [
        rf"(?ms)^  {job}:\n.*?(?=^  [a-z][a-z0-9-]*:\n|\Z)" for job in BUILD_JOBS
    ]
    for pattern in patterns:
        old = re.search(pattern, before)
        new = re.search(pattern, after)
        if old is None or new is None or old[0] != new[0].replace(SKIP_GUARD, ""):
            raise ValueError("Native build or validation definitions changed")


def source_receipt(root: Path, run_id: int) -> dict:
    subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=root, check=True)

    def api(suffix):
        return json.loads(
            subprocess.check_output(
                ["gh", "api", f"repos/{REPOSITORY}/actions/runs/{run_id}{suffix}"]
            )
        )

    run = api("")
    if run.get("id") != run_id:
        raise ValueError("Native producer run identity does not match")
    commit = validate_run(run, api("/jobs?filter=latest&per_page=100"))
    subprocess.run(
        ["git", "merge-base", "--is-ancestor", commit, "HEAD"], cwd=root, check=True
    )
    changed = (
        subprocess.check_output(
            ["git", "diff", "--name-only", "-z", f"{commit}..HEAD"], cwd=root
        )
        .decode()
        .split("\0")
    )
    before = subprocess.check_output(
        ["git", "show", f"{commit}:{WORKFLOW}"], cwd=root, text=True
    )
    after = subprocess.check_output(
        ["git", "show", f"HEAD:{WORKFLOW}"], cwd=root, text=True
    )
    validate_changes(set(changed) - {""}, before, after)
    return {
        "schema": 1,
        "nativeSourceCommit": commit,
        "nativeSourceRunId": run_id,
        "packagingCommit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=root, text=True
        ).strip(),
        "targets": list(BINARY_TARGETS),
    }


def verify_archives(directory: Path, commit: str) -> None:
    for target in BINARY_TARGETS:
        root = directory / target
        archive = root / f"codex-package-{target}.tar.gz"
        checksums = {}
        for line in (root / "SHA256SUMS").read_text().splitlines():
            digest, name = line.split(maxsplit=1)
            name = name.removeprefix("./")
            if name in checksums or not re.fullmatch(r"[0-9a-f]{64}", digest):
                raise ValueError("Invalid native archive checksum manifest")
            checksums[name] = digest
        with archive.open("rb") as source:
            actual = hashlib.file_digest(source, "sha256").hexdigest()
        if checksums.get(archive.name) != actual:
            raise ValueError(f"Native archive checksum mismatch: {target}")
        subprocess.run(
            [
                "gh",
                "attestation",
                "verify",
                str(archive),
                "--repo",
                REPOSITORY,
                "--signer-workflow",
                f"{REPOSITORY}/{WORKFLOW}",
                "--source-digest",
                commit,
                "--deny-self-hosted-runners",
            ],
            check=True,
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-run", type=int, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--artifacts-dir", type=Path)
    args = parser.parse_args()
    if args.source_run <= 0:
        parser.error("A positive native producer run ID is required")
    root = Path(__file__).resolve().parents[1]
    receipt = source_receipt(root, args.source_run)
    if args.artifacts_dir:
        if json.loads(args.receipt.read_text()) != receipt:
            raise ValueError("Native provenance receipt does not match the candidate")
        verify_archives(args.artifacts_dir, receipt["nativeSourceCommit"])
    else:
        with args.receipt.open("x") as output:
            json.dump(receipt, output, sort_keys=True)
            output.write("\n")
    print(json.dumps(receipt), flush=True)


if __name__ == "__main__":
    main()
