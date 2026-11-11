#!/usr/bin/env python3
"""Build and qualify an isolated local package; never select or start it live."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

from codex_package.targets import TARGET_SPECS
from codex_package.v8 import fetch_codex_v8_artifacts
from local_deploy_owner import digest, package_digest, save

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
CARGO = "/home/holyglory/.rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/bin/cargo"


def context():
    run_id = os.environ["DEVCOORDINATOR_RUN_ID"]
    if not re.fullmatch(r"t[0-9]{8}T[0-9]{6}Z-[0-9a-f]+", run_id):
        raise RuntimeError("actual-governed-run-required")
    return ROOT / "codex-rs/target/local-usage-package" / run_id


def run(arguments, **kwargs):
    return subprocess.run(
        [str(arg) for arg in arguments], cwd=ROOT, check=True, **kwargs
    )


def build():
    work = context()
    work.mkdir(parents=True, exist_ok=False)
    version = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"][
        "package"
    ]["version"]
    commit = run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True
    ).stdout.strip()
    if run(
        ["git", "status", "--porcelain"], capture_output=True, text=True
    ).stdout.strip():
        raise RuntimeError("package-requires-committed-source")
    # Verify and reuse the existing pinned native V8 pair. Cargo retains its
    # normal host target/cache; no target-triple cold rebuild or old binary reuse.
    v8 = fetch_codex_v8_artifacts(
        TARGET_SPECS[TARGET], cache_root=Path("/mnt/build-storage/codex/rebase166-v8")
    )
    environment = {
        **os.environ,
        "RUSTY_V8_ARCHIVE": str(v8.archive),
        "RUSTY_V8_SRC_BINDING_PATH": str(v8.binding),
    }
    metadata = json.loads(
        run(
            [
                CARGO,
                "metadata",
                "--locked",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
                ROOT / "codex-rs/Cargo.toml",
            ],
            env=environment,
            capture_output=True,
            text=True,
        ).stdout
    )
    target_directory = Path(metadata["target_directory"])
    if not target_directory.is_absolute():
        raise RuntimeError("cargo-target-directory-not-absolute")
    run(
        [
            CARGO,
            "build",
            "--locked",
            "--manifest-path",
            ROOT / "codex-rs/Cargo.toml",
            "--bin",
            "codex",
            "--bin",
            "codex-code-mode-host",
            "--bin",
            "codex-exec-server",
            "--bin",
            "bwrap",
        ],
        env=environment,
    )
    binaries = target_directory / "debug"
    package = work / "package"
    run(
        [
            sys.executable,
            ROOT / "scripts/build_codex_package.py",
            "--target",
            TARGET,
            "--package-version",
            version,
            "--cargo-profile",
            "dev",
            "--strip",
            "all",
            "--package-dir",
            package,
            "--entrypoint-bin",
            binaries / "codex",
            "--code-mode-host-bin",
            binaries / "codex-code-mode-host",
            "--exec-server-bin",
            binaries / "codex-exec-server",
            "--bwrap-bin",
            binaries / "bwrap",
        ]
    )
    observed = run(
        [package / "bin/codex", "--version"], capture_output=True, text=True
    ).stdout.strip()
    if observed != f"codex-cli {version}":
        raise RuntimeError("packaged-version-mismatch")
    manifest = {
        "schemaVersion": 1,
        "sourceCommit": commit,
        "version": version,
        "profile": "dev",
        "target": TARGET,
        "package": str(package),
        "packageSha256": package_digest(package),
        "executableSha256": digest(package / "bin/codex"),
        "governedRun": os.environ["DEVCOORDINATOR_RUN_ID"],
        "sourceBinaries": {
            name: digest(binaries / name)
            for name in ("codex", "codex-code-mode-host", "codex-exec-server", "bwrap")
        },
        "files": {
            str(path.relative_to(package)): digest(path)
            for path in sorted(package.rglob("*"))
            if path.is_file()
        },
    }
    save(work / "candidate.json", manifest)
    print(
        json.dumps(
            {
                "result": "assembled",
                "version": version,
                "sourceCommit": commit,
                "candidate": str(work / "candidate.json"),
            }
        )
    )


def verify():
    work = context()
    manifest = json.loads((work / "candidate.json").read_text())
    package = Path(manifest["package"])
    if package_digest(package) != manifest["packageSha256"]:
        raise RuntimeError("assembled-package-changed")
    run(
        [
            sys.executable,
            "-m",
            "unittest",
            "discover",
            "-s",
            "scripts",
            "-p",
            "test_local_deploy_rpc.py",
            "-v",
        ],
        env={
            **os.environ,
            "CODEX_MULTI_VERIFY_BINARY": str(package / "bin/codex"),
            "CODEX_MULTI_VERIFY_VERSION": manifest["version"],
        },
    )
    save(
        work / "verified.json",
        {
            "result": "passed",
            "version": manifest["version"],
            "sourceCommit": manifest["sourceCommit"],
            "packageSha256": manifest["packageSha256"],
            "usageContractVersion": 1,
            "usageStorageFormatVersion": 3,
            "liveInstallationChanged": False,
        },
    )


if __name__ == "__main__":
    {"build": build, "verify": verify}[sys.argv[1]]()
