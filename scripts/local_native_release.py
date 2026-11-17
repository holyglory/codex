#!/usr/bin/env python3
"""Compile locally, verify archived tests on native hardware, then seal a handoff.

Compilation needs an immutable source checkout, a matching source-built V8
handoff, preinstalled tools and explicit persistent Cargo/musl cache paths.
Verification never invokes Cargo compilation and never uses an emulator.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

from local_native_artifacts import (
    TARGETS,
    check_logs,
    elf_identity,
    file_info,
    load_json,
    native_host,
    relative_path,
    require,
    safe_extract,
    source_identity,
    verify_build,
    write_json,
)

HELPERS = Path(__file__).resolve().parents[1]
REGRESSIONS = [
    "codex-code-mode-runtime",
    "codex-account-registry",
    "codex-account-selection",
    "codex-private-storage",
    "codex-usage",
]
BINARIES = ["codex", "codex-code-mode-host", "codex-exec-server"]
REQUIRED_CHECKS = {"v8-poc", "product-regressions", "native-version", "native-help"}


def helper_identity():
    commit = subprocess.check_output(
        ["git", "-C", str(HELPERS), "rev-parse", "HEAD"], text=True
    ).strip()
    source_identity(HELPERS, commit)
    paths = [
        Path(__file__),
        HELPERS / "scripts/local_native_artifacts.py",
        HELPERS / "scripts/prepare_local_musl.py",
        HELPERS / "scripts/local_native_compile.py",
        HELPERS / "scripts/local_release_tar.py",
    ]
    return {
        "helper_commit": commit,
        "helper_files": {
            path.relative_to(HELPERS).as_posix(): file_info(path) for path in paths
        },
    }


class Commands:
    """Keep command exit codes and original output bound to each finite stage."""

    def __init__(self, root, directory, environment):
        self.root, self.directory, self.environment = root, directory, environment
        (directory / "logs").mkdir(parents=True, exist_ok=True)
        self.checks = []

    def run(self, name, command, *, timeout=36000):
        log = self.directory / "logs" / f"{name}.log"
        log.touch(exist_ok=False)
        started = time.monotonic()
        print(json.dumps({"stage": name, "status": "started"}), flush=True)
        # Match the existing Unix producer's pipefail/tee path. Output reaches
        # Coordinator while the compiler runs and is retained without a pipe
        # that Python waits to drain only after process exit.
        result = subprocess.run(
            [
                "bash",
                "-c",
                'set -o pipefail; "$@" 2>&1 | tee -a "$CODEX_LOCAL_BUILD_LOG"',
                "codex-native-stage",
                *map(str, command),
            ],
            cwd=self.root,
            env={**self.environment, "CODEX_LOCAL_BUILD_LOG": str(log.resolve())},
            stderr=subprocess.STDOUT,
            timeout=timeout,
            check=False,
        )
        print(
            json.dumps(
                {
                    "stage": name,
                    "status": "finished",
                    "exitCode": result.returncode,
                    "elapsedSeconds": time.monotonic() - started,
                    "logBytes": log.stat().st_size,
                }
            ),
            flush=True,
        )
        self.checks.append(
            {
                "name": name,
                "exit_code": result.returncode,
                "log_sha256": file_info(log)["sha256"],
                "command": list(map(str, command)),
            }
        )
        require(result.returncode == 0, f"{name} failed; preserve {log}")
        return log.read_text() if name in {"native-version", "native-help"} else None


def nextest_version(environment):
    output = subprocess.check_output(
        ["cargo-nextest", "--version"], env=environment, text=True
    )
    match = re.match(r"cargo-nextest (\d+\.\d+\.\d+)\b", output)
    require(match, "Cannot identify the installed Nextest version")
    return match[1]


def stage_v8(root, handoff, output, target, commit):
    from release_identity import identity, v8_input_manifest

    pair = load_json(handoff / "v8-pair.json")
    producer = pair.get("source_commit", "")
    expected = {
        "schema": 1,
        "status": "success",
        "target": target,
        "profile": "sandbox_release",
        "source_built": True,
    }
    require(
        {key: pair.get(key) for key in expected} == expected,
        "V8 handoff is not a successful source-built pair",
    )
    require(
        all(pair.get(key) == value for key, value in identity(producer).items()),
        "V8 producer source identity differs",
    )
    require(
        pair.get("v8_inputs")
        == v8_input_manifest(producer)
        == v8_input_manifest(commit),
        "V8 locked inputs differ from source",
    )
    archive = f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz"
    binding = f"src_binding_ptrcomp_sandbox_release_{target}.rs"
    checksum = f"rusty_v8_ptrcomp_sandbox_release_{target}.sha256"
    require(set(pair["files"]) == {archive, binding, checksum}, "Incomplete V8 pair")
    for kind, folder in [("files", "pair"), ("logs", "logs")]:
        require(
            {path.name for path in (handoff / folder).iterdir()} == set(pair[kind]),
            "Unexpected V8 handoff entries",
        )
        for name, digest in pair[kind].items():
            require(
                relative_path(name).name == name
                and file_info(handoff / folder / name)["sha256"] == digest,
                "Changed V8 file or build log",
            )
            destination = output / "v8" / folder / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(handoff / folder / name, destination)
    require(
        (handoff / "pair" / checksum).read_text().splitlines()
        == [f"{pair['files'][name]}  {name}" for name in (archive, binding)],
        "V8 checksum manifest differs",
    )
    shutil.copy2(handoff / "v8-pair.json", output / "v8-pair.json")
    return {
        "RUSTY_V8_ARCHIVE": str(output / "v8/pair" / archive),
        "RUSTY_V8_SRC_BINDING_PATH": str(output / "v8/pair" / binding),
    }


def compile_target(args):
    from local_native_compile import compile_target as compile_local_target

    return compile_local_target(args)


def verify_target(args):
    root, incoming, output = (
        args.source_root.resolve(),
        args.input.resolve(),
        args.output.resolve(),
    )
    host = native_host(args.target)
    build = verify_build(incoming, args.manifest_sha256, root, args.commit, args.target)
    helpers = helper_identity()
    require(
        {key: build["producer"].get(key) for key in helpers} == helpers,
        "Native verifier helpers differ from the compiler's helpers",
    )
    environment = dict(os.environ)
    require(
        not any(
            "RUNNER" in key and key.startswith(("CARGO_TARGET_", "NEXTEST_"))
            for key in environment
        ),
        "Custom test runners are forbidden for native proof",
    )
    require(
        nextest_version(environment) == build["nextest_version"],
        "Install the exact archived Nextest version",
    )
    require(not output.exists(), "Preserve earlier verification evidence")
    output.mkdir(parents=True)
    safe_extract(incoming / f"codex-package-{args.target}.tar.gz", output / "package")
    package = output / "package"
    for relative in [
        *(f"bin/{name}" for name in BINARIES),
        "codex-resources/bwrap",
        "codex-path/rg",
    ]:
        elf_identity(package / relative, args.target)
    require(
        file_info(package / "codex-resources/bwrap") == build["bwrap"],
        "Packaged bwrap differs from its embedded integrity hash",
    )
    home = output / "smoke-home"
    home.mkdir(mode=0o700)
    environment.update(CODEX_HOME=str(home), INSTA_UPDATE="no")
    runner = Commands(root / "codex-rs", output, environment)
    base = [
        "just",
        "test",
        "--config-file",
        str(incoming / "nextest-config.toml"),
        "--archive-file",
        str(incoming / f"tests-{args.target}.tar.zst"),
        "--workspace-remap",
        str(root / "codex-rs"),
    ]
    runner.run(
        "v8-poc",
        [
            *base,
            "-E",
            "platform(target) & package(codex-v8-poc)",
            "--run-ignored",
            "all",
        ],
    )
    expression = (
        "platform(target) & ("
        + " | ".join(f"package({name})" for name in REGRESSIONS)
        + ")"
    )
    runner.run("product-regressions", [*base, "-E", expression])
    require(
        runner.run(
            "native-version", [str(package / "bin/codex"), "--version"], timeout=60
        ).strip()
        == f"codex-cli {build['version']}",
        "Release executable version differs",
    )
    runner.run("native-help", [str(package / "bin/codex"), "--help"], timeout=60)
    verify_build(incoming, args.manifest_sha256, root, args.commit, args.target)
    receipt = {
        "schema": 1,
        "kind": "codex-native-verification",
        "status": "success",
        "runtime_executed": True,
        **{
            key: build[key]
            for key in [
                "source_commit",
                "source_tree",
                "native_tree",
                "version",
                "target",
            ]
        },
        "build_manifest_sha256": args.manifest_sha256,
        "runtime_host": host,
        "checks": runner.checks,
        "producer": helpers,
        "coordinator_run_id": os.environ.get("DEVCOORDINATOR_RUN_ID"),
        "github_run_id": os.environ.get("GITHUB_RUN_ID"),
        "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
    }
    write_json(output / "verification.json", receipt)
    return file_info(output / "verification.json")


def seal_target(args):
    root, incoming, verified, output = (
        args.source_root.resolve(),
        args.input.resolve(),
        args.verification.resolve(),
        args.output.resolve(),
    )
    build = verify_build(incoming, args.manifest_sha256, root, args.commit, args.target)
    require(
        file_info(verified / "verification.json")["sha256"] == args.verification_sha256,
        "Native verification digest changed",
    )
    receipt = load_json(verified / "verification.json")
    expected = {
        "schema": 1,
        "kind": "codex-native-verification",
        "status": "success",
        "runtime_executed": True,
        "build_manifest_sha256": args.manifest_sha256,
        **{
            key: build[key]
            for key in [
                "source_commit",
                "source_tree",
                "native_tree",
                "version",
                "target",
            ]
        },
    }
    require(
        {key: receipt.get(key) for key in expected} == expected,
        "Native verification does not match this compiled target",
    )
    helpers = helper_identity()
    require(
        receipt["producer"] == helpers
        and {key: build["producer"].get(key) for key in helpers} == helpers,
        "Sealing helpers differ from the checked build and verification",
    )
    require(
        receipt["runtime_host"]
        == {"os": "Linux", "architecture": args.target.split("-", 1)[0]},
        "Verification host was not native",
    )
    require(
        check_logs(verified, receipt["checks"]) == REQUIRED_CHECKS,
        "Native acceptance checks are incomplete",
    )
    require(not output.exists(), "Preserve the existing sealed handoff")
    output.mkdir(parents=True)
    shutil.copytree(verified / "logs", output / "logs")
    shutil.copytree(incoming / "v8/logs", output / "v8-source-logs")
    for name in [
        f"codex-package-{args.target}.tar.gz",
        "build-manifest.json",
        "v8-pair.json",
    ]:
        shutil.copy2(incoming / name, output / name)
    shutil.copy2(verified / "verification.json", output / "verification.json")
    pair = load_json(incoming / "v8-pair.json")
    native = {
        "schema": 1,
        "status": "success",
        "commit": args.commit,
        **{
            key: build[key]
            for key in [
                "source_commit",
                "source_tree",
                "native_tree",
                "version",
                "target",
            ]
        },
        "v8_tree": pair["v8_tree"],
        "producer": "linux-local-prebuilt",
        "runtime_executed": True,
        "github_run_id": receipt["github_run_id"],
        "github_run_attempt": receipt["github_run_attempt"],
        "coordinator_run_id": receipt["coordinator_run_id"],
        "checks": receipt["checks"],
        "build_manifest_sha256": args.manifest_sha256,
        "verification_sha256": args.verification_sha256,
        "v8_pair": {
            **pair,
            "cppgc_ignored_tests_executed": True,
            "reused": True,
            "producer_run_id": pair.get("github_run_id"),
            "producer_run_attempt": pair.get("github_run_attempt"),
        },
        "archives": {
            f"codex-package-{args.target}.tar.gz": file_info(
                output / f"codex-package-{args.target}.tar.gz"
            )
        },
    }
    write_json(output / "native-manifest.json", native)
    return file_info(output / "native-manifest.json")


def fetch(args):
    from runner_handoff import download_artifact

    require(
        re.fullmatch(
            r"https://api\.github\.com/repos/holyglory/codex/releases/assets/[1-9][0-9]*",
            args.asset_url,
        ),
        "Use an authenticated GitHub release asset URL",
    )
    archive = args.output.with_suffix(".download.tar.gz")
    require(0 < args.asset_size <= 8 * 1024**3, "Transport asset exceeds its bound")
    download_artifact(
        {
            "url": args.asset_url,
            "size_bytes": args.asset_size,
            "sha256": args.asset_sha256,
        },
        archive,
    )
    safe_extract(archive, args.output)
    build = verify_build(
        args.output, args.manifest_sha256, args.source_root, args.commit, args.target
    )
    helpers = helper_identity()
    require(
        {key: build["producer"].get(key) for key in helpers} == helpers,
        "Downloaded input uses a different helper revision",
    )
    return build["nextest_version"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="phase", required=True)
    for phase in ["compile", "verify", "seal", "fetch"]:
        command = subparsers.add_parser(phase)
        command.add_argument("--source-root", type=Path, required=True)
        command.add_argument("--commit", required=True)
        command.add_argument("--target", choices=TARGETS, required=True)
        command.add_argument("--output", type=Path, required=True)
        if phase == "compile":
            for name in ["cargo-target-dir", "musl-cache", "zig", "v8-handoff"]:
                command.add_argument("--" + name, type=Path, required=True)
            command.add_argument("--strip-tool")
        else:
            command.add_argument("--manifest-sha256", required=True)
        if phase in {"verify", "seal"}:
            command.add_argument("--input", type=Path, required=True)
        if phase == "seal":
            command.add_argument("--verification", type=Path, required=True)
            command.add_argument("--verification-sha256", required=True)
        if phase == "fetch":
            command.add_argument("--asset-url", required=True)
            command.add_argument("--asset-sha256", required=True)
            command.add_argument("--asset-size", type=int, required=True)
    args = parser.parse_args()
    action = {
        "compile": compile_target,
        "verify": verify_target,
        "seal": seal_target,
        "fetch": fetch,
    }[args.phase]
    print(json.dumps(action(args), sort_keys=True))


if __name__ == "__main__":
    main()
