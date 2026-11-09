#!/usr/bin/env python3
"""Assemble and qualify a native package through existing release workflows.

Run only as declared Coordinator checks. This stages a preliminary executable
artifact; it never installs a runtime, publishes externally, or deletes old data.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
VERSION = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"][
    "package"
]["version"]
RELEASE_TARGET = Path("/mnt/build-storage/codex/rebase166-release-target")
BULK_ROOT = Path("/mnt/build-storage/codex/rebase166-package-work")
WORKTREE_ID = "wdcb50e621f3f9f8c"
ARTIFACT = "native-linux-gnu"


def run(arguments, *, cwd=ROOT, env=None, capture=False):
    return subprocess.run(
        [str(argument) for argument in arguments],
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def source_identity():
    client = Path(shutil.which("devcoordinator2")).resolve()
    executor = client.with_name("devcoordinator2-executor")
    response = json.loads(
        run([executor, "source-digest", "--worktree", ROOT], capture=True).stdout
    )
    if response.get("schema") != 2 or not re.fullmatch(
        "[0-9a-f]{64}", response.get("sha256", "")
    ):
        raise RuntimeError(
            "Installed source-digest helper returned no authoritative digest"
        )
    return {
        "source_sha256": response["sha256"],
        "config_file_sha256": digest(ROOT / ".devcoordinator.toml"),
    }


def context():
    run_id = os.environ["DEVCOORDINATOR_RUN_ID"]
    if not re.fullmatch(r"t[0-9]{8}T[0-9]{6}Z-[0-9a-f]+", run_id):
        raise RuntimeError("Expected the actual governed run identity")
    return run_id, BULK_ROOT / run_id


def verify_source(work):
    expected = json.loads((work / "source.json").read_text())
    if source_identity() != expected:
        raise RuntimeError(
            "Source or Coordinator declaration changed during release qualification"
        )
    return expected


def preflight(work):
    workspace = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"][
        "package"
    ]
    if workspace["version"] != VERSION:
        raise RuntimeError(
            "Package version differs from the compiled workspace version"
        )
    for command in [
        "cargo",
        "rustc",
        "just",
        "uv",
        "objcopy",
        "strip",
        "nm",
        "addr2line",
        "pkg-config",
    ]:
        if shutil.which(command) is None:
            raise RuntimeError(f"Missing release prerequisite: {command}")
    run(["pkg-config", "--exists", "libcap", "openssl"])
    if (ROOT / "codex-rs/target").is_symlink():
        raise RuntimeError(
            "Retained package staging must use a regular repository directory"
        )
    work.mkdir(parents=True, exist_ok=False)
    write_json(work / "source.json", source_identity())


def build(work):
    verify_source(work)
    if Path(os.environ["CARGO_TARGET_DIR"]).resolve() != RELEASE_TARGET:
        raise RuntimeError(
            "Native release Cargo outputs must use the approved bulk target directory"
        )
    for variant, directory in [("codex", "cli"), ("codex-app-server", "app-server")]:
        run(
            [
                "just",
                "assemble-codex-package",
                "--target",
                TARGET,
                "--variant",
                variant,
                "--package-version",
                VERSION,
                "--cargo-profile",
                "release",
                "--strip",
                "auto",
                "--package-dir",
                work / directory,
                "--archive-output",
                work / f"{variant}-{TARGET}.tar.gz",
            ]
        )
    # The canonical symbols helper strips its inputs. Give it private copies;
    # preserve Cargo outputs and the already hashed bwrap resource unchanged.
    symbols_input = work / "symbols-input"
    symbols_input.mkdir()
    binaries = [
        "codex",
        "codex-app-server",
        "codex-code-mode-host",
        "codex-exec-server",
    ]
    for binary in binaries:
        shutil.copy2(
            RELEASE_TARGET / TARGET / "release" / binary, symbols_input / binary
        )
    run(
        [
            "bash",
            ROOT / ".github/scripts/archive-release-symbols-and-strip-binaries.sh",
            "--target",
            TARGET,
            "--artifact-name",
            TARGET,
            "--release-dir",
            symbols_input,
            "--archive-dir",
            work / "symbols",
            "--binaries",
            " ".join(binaries),
        ],
        env={**os.environ, "RUNNER_TEMP": str(work / "symbols-temp")},
    )
    verify_source(work)


def smoke(work):
    verify_source(work)
    diagnostic = (
        Path(os.environ["DEVCOORDINATOR_DIAGNOSTICS_DIR"]) / "package-smoke.xml"
    )
    archives = [
        work / f"codex-{TARGET}.tar.gz",
        work / f"codex-app-server-{TARGET}.tar.gz",
        work / "symbols" / f"codex-symbols-{TARGET}.tar.gz",
    ]
    archive_hashes = {str(path.relative_to(work)): digest(path) for path in archives}
    environment = {
        **os.environ,
        "PYTHONPATH": f"{ROOT}/sdk/python/src:{ROOT}/sdk/python/tests",
        "UV_PROJECT_ENVIRONMENT": str(work / "smoke-venv"),
    }
    run(
        [
            "uv",
            "run",
            "--frozen",
            "pytest",
            "-v",
            "--compression",
            "gzip",
            "--package-target",
            TARGET,
            "--cli-archive",
            archives[0],
            "--app-server-archive",
            archives[1],
            "--symbols-archive",
            archives[2],
            "--basetemp",
            work / "smoke-tmp",
            "--junitxml",
            diagnostic,
            "test_codex_package.py",
        ],
        cwd=ROOT / "scripts/codex_package/smoke_tests",
        env=environment,
    )
    if archive_hashes != {
        str(path.relative_to(work)): digest(path) for path in archives
    }:
        raise RuntimeError("An archive changed during the real package smoke suite")
    shutil.copy2(diagnostic, work / "package-smoke.xml")
    write_json(
        work / "smoke.json",
        {
            "source": verify_source(work),
            "archives": archive_hashes,
            "suite": "scripts/codex_package/smoke_tests/test_codex_package.py",
            "checked_at_ms": time.time_ns() // 1_000_000,
        },
    )


def golden(work):
    verify_source(work)
    reference = Path(
        "/mnt/build-storage/codex/account-golden/reference-installed-be7b167.json"
    )
    if (
        digest(reference)
        != "fae81eb5297a5230ebcfd79dd2c3f5c47f6d8e9756ff70e42061026455d94bc5"
    ):
        raise RuntimeError("The independently captured installed CLI reference changed")
    run(
        [
            "python3",
            "-m",
            "scripts.account_cli_compat.runner",
            "compare",
            "--binary",
            work / "cli/bin/codex",
            "--source-root",
            ROOT,
            "--reference",
            reference,
            "--output",
            work / "account-golden.json",
        ]
    )
    run(
        [
            "python3",
            "-m",
            "scripts.account_cli_compat.credits",
            "--binary",
            work / "cli/bin/codex",
            "--reference-binary",
            "/home/holyglory/.codex/packages/standalone/releases/0.162.1+multi.4-x86_64-unknown-linux-gnu-local-usage-recovery-571ba85a/bin/codex",
            "--source-root",
            ROOT,
            "--output",
            work / "account-credit-golden.json",
        ]
    )
    verify_source(work)


def qualify(run_id, work):
    identity = verify_source(work)
    base_run = os.environ.get("CODEX_MULTI_BASE_VALIDATION_RUN", run_id)
    auth_run = os.environ.get("CODEX_MULTI_AUTH_VALIDATION_RUN", run_id)
    smoke_proof = json.loads((work / "smoke.json").read_text())
    for name, sha256 in smoke_proof["archives"].items():
        if digest(work / name) != sha256:
            raise RuntimeError(
                "The qualified package no longer matches the smoke-tested archive"
            )
    directory = os.environ.get(
        "CODEX_MULTI_NATIVE_ARTIFACT_DIRECTORY", "native-release-delivery"
    )
    if not re.fullmatch(r"native-release-[a-z0-9-]{1,64}", directory):
        raise RuntimeError("Expected a bounded native release artifact directory name")
    bundle = ROOT / "codex-rs/target" / directory
    bundle.mkdir(exist_ok=False)
    shutil.copytree(work / "cli", bundle / "package")
    archive_name = f"codex-{VERSION}-{TARGET}.tar.gz"
    shutil.copy2(work / f"codex-{TARGET}.tar.gz", bundle / archive_name)
    for name in [
        "source.json",
        "smoke.json",
        "package-smoke.xml",
        "account-golden.json",
        "account-credit-golden.json",
    ]:
        shutil.copy2(work / name, bundle / name)
    executable = bundle / "package/bin/codex"
    if run([executable, "--version"], capture=True).stdout != f"codex-cli {VERSION}\n":
        raise RuntimeError("Retained executable version does not match the release")
    if digest(executable) != digest(work / "cli/bin/codex"):
        raise RuntimeError(
            "Retained executable differs from the canonical package input"
        )
    capabilities = [
        {
            "id": "native-linux-package",
            "scope": "product",
            "state": "real_e2e",
            "expected_result": "Run the native GNU CLI package with its packaged executor, host, ripgrep and sandbox resource.",
            "evidence_refs": [
                f"run/{run_id}/package-smoke",
                f"run/{run_id}/account-golden",
            ],
        },
        {
            "id": "account-routing-and-cli",
            "task_id": "pa4e2ae746541807c",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Preserve account command output and model-request selection; packaged CLI compatibility is tested, live replacement is pending.",
            "evidence_refs": [
                f"run/{base_run}/full-workspace",
                f"run/{run_id}/account-golden",
            ],
        },
        {
            "id": "usage-worker-and-retention",
            "task_id": "pfe4506d4f0e6ff44",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Keep capture, reporting and seven-day retention correct on growing histories; final installation and live observation are pending.",
            "evidence_refs": [
                f"run/{base_run}/full-workspace",
                f"run/{base_run}/usage-scale",
            ],
        },
        {
            "id": "account-credit-protocol",
            "task_id": "p9ba8b8805e54269b",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Preserve installed credit capability and profile read/update fields through the public account RPCs; live activation is pending.",
            "evidence_refs": [f"run/{base_run}/full-workspace"],
        },
        {
            "id": "accepted-account-cli-output",
            "task_id": "p1adf58fb06e9d8c0",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Match the accepted installed account credit, help and aligned table output across the unchanged golden matrix and named credit-balance cases; live activation is pending.",
            "evidence_refs": [f"run/{run_id}/account-golden"],
        },
        {
            "id": "local-usage-capability",
            "task_id": "pb55141372eaf9f45",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Preserve null negotiation for the unported legacy usage RPC extension while keeping the implemented usage_stats tool available; live activation is pending.",
            "evidence_refs": [f"run/{base_run}/full-workspace"],
        },
        {
            "id": "remaining-observed-detail-families",
            "task_id": "p45bf207f3dd2f866",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Return captured process, agent, approval, repository, taxonomy and activity-span facts with filters and pagination; installed observation is pending.",
            "evidence_refs": [f"run/{base_run}/full-workspace"],
        },
        {
            "id": "performance-outcome-continuation",
            "task_id": "p71691992a596b0dd",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Keep continued usage pages on their original snapshot and resolved filters until expiry. Nonempty outcome pagination is fixture-tested; authoritative outcome capture remains unavailable.",
            "evidence_refs": [f"run/{base_run}/full-workspace"],
        },
        {
            "id": "observed-retries-and-waits",
            "task_id": "p310ae9370e9a2ffa",
            "scope": "product",
            "state": "fixture_only",
            "expected_result": "Capture actual Responses HTTP/WebSocket attempts, retry predecessors and measured retry waits, including cancellation, with explicit coverage limits.",
            "evidence_refs": [f"run/{base_run}/full-workspace"],
        },
        {
            "id": "retry-wait-and-outcome-bindings",
            "task_id": "p310ae9370e9a2ffa",
            "scope": "product",
            "state": "external_blocked",
            "expected_result": "Attribute effort only after explicit authoritative host selection or assignment. No such receipt contract currently exists; reports preserve unattributed effort.",
            "evidence_refs": [],
        },
    ]
    if base_run != run_id:
        capabilities.append(
            {
                "id": "desktop-profile-authentication",
                "task_id": "pe5642aede831f5b1",
                "scope": "product",
                "state": "fixture_only",
                "expected_result": "Report the preserved managed profile consistently to Desktop and reconnect the original chat; installed verification is pending.",
                "evidence_refs": [f"run/{auth_run}/app-server-suite"],
            }
        )
    if live_proof_path := os.environ.get("CODEX_MULTI_LIVE_PROOF"):
        live_proof = Path(live_proof_path)
        if (
            digest(live_proof)
            != "89f46f5797e25b76b0fdfba3c8c6876458db3a4e37b47abf090b5ede57d37d1c"
        ):
            raise RuntimeError("The qualified live observation changed")
        live = json.loads(live_proof.read_text())
        if live["result"] != "passed" or live["serving"]["executableSha256"] != digest(
            executable
        ):
            raise RuntimeError(
                "This package differs from the independently verified live executable"
            )
        completed = {
            "usage-worker-and-retention": "The installed store serves real usage reports; retention, correction and restart behavior passed the complete storage and scale checks.",
            "account-credit-protocol": "The installed account credit capability and profile fields match the tested protocol contract.",
            "accepted-account-cli-output": "The selected local executable matches the accepted account output and credit fixtures.",
            "local-usage-capability": "Installed initialization truthfully reports the legacy extension as unavailable and the real usage_stats tool succeeds.",
            "desktop-profile-authentication": "The installed login APIs recognize preserved profiles consistently and both original chats resume.",
        }
        for capability in capabilities:
            if capability["id"] in completed:
                capability["state"] = "real_e2e"
                capability["expected_result"] = completed[capability["id"]]
                capability["evidence_refs"].append(
                    "delivery-7935351864b86bec784b5ede398e6dd0a0598f404e6c31029fa7e24559940595"
                )
    write_json(
        bundle / "completion.json",
        {
            "schema_version": 1,
            "claim": "preliminary",
            "source_sha256": identity["source_sha256"],
            "capabilities": capabilities,
        },
    )
    check = os.environ["DEVCOORDINATOR_CHECK_NAME"]
    write_json(
        bundle / "delivery.json",
        {
            "version": 1,
            "kind": "local-executable",
            "target": TARGET,
            "source_sha256": identity["source_sha256"],
            "file": "package/bin/codex",
            "observed_sha256": digest(executable),
            "checked_at_ms": time.time_ns() // 1_000_000,
            "access": f"artifact://{WORKTREE_ID}/{run_id}/{check}/{ARTIFACT}/package/bin/codex",
            "observation": "executable_smoke_passed",
        },
    )
    verify_source(work)
    paths = list(bundle.rglob("*"))
    if any(path.is_symlink() for path in paths):
        raise RuntimeError("Delivery artifact cannot contain symbolic links")
    if sum(path.stat().st_size for path in paths if path.is_file()) > 1024**3:
        raise RuntimeError(
            "Retained delivery exceeds the Coordinator's one-GiB artifact limit"
        )


if __name__ == "__main__":
    run_id, work = context()
    stages = {
        "preflight": lambda: preflight(work),
        "build": lambda: build(work),
        "smoke": lambda: smoke(work),
        "golden": lambda: golden(work),
        "qualify": lambda: qualify(run_id, work),
    }
    if len(sys.argv) != 2 or sys.argv[1] not in stages:
        raise SystemExit("Expected preflight, build, smoke, golden, or qualify")
    stages[sys.argv[1]]()
