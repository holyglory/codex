#!/usr/bin/env python3
"""Bind packaging-only revisions and native producer receipts to real Git source."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
# Only npm/release orchestration and its tests may differ when reusing native
# acceptance. Product code, native inputs, shared actions and toolchains may not.
PACKAGING_PATHS = frozenset(
    {
        "codex-cli/package.json",
        "codex-cli/bin/codex.js",
        "codex-cli/scripts/build_npm_package.py",
        "codex-cli/tests/test_build_npm_package.py",
        "scripts/stage_npm_packages.py",
        "scripts/verify_npm_release.py",
        "scripts/check_npm_install.py",
        "scripts/release_identity.py",
        "scripts/runner_handoff.py",
        "scripts/prepare_runner_handoff.py",
        "scripts/stage_verified_npm_release.py",
        "scripts/test_npm_release.py",
        "scripts/test_downstream_windows_contract.py",
        "codex-cli/scripts/verify_candidate_run.py",
        ".github/workflows/downstream-candidate.yml",
        ".github/workflows/downstream-npm-publish.yml",
        ".github/workflows/trusted-windows-native.yml",
        ".github/scripts/downstream-windows-native.ps1",
        ".github/scripts/downstream-unix-native.sh",
        ".devcoordinator.toml",
    }
)


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True).strip()


def identity(commit: str) -> dict:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Source commit must be a full lowercase SHA-1")
    git("cat-file", "-e", f"{commit}^{{commit}}")
    return {
        "source_commit": commit,
        "source_tree": git("rev-parse", f"{commit}^{{tree}}"),
        "native_tree": git("rev-parse", f"{commit}:codex-rs"),
    }


def verify_native_source(producer: str, packaging: str) -> dict:
    native = identity(producer)
    candidate = identity(packaging)
    subprocess.run(
        ["git", "-C", str(ROOT), "merge-base", "--is-ancestor", producer, packaging],
        check=True,
    )
    changed = set(
        git("diff", "--name-only", "--no-renames", producer, packaging).splitlines()
    )
    unexpected = changed - PACKAGING_PATHS
    if unexpected or native["native_tree"] != candidate["native_tree"]:
        raise ValueError(
            f"Native reuse changes product/build inputs: {sorted(unexpected)}"
        )
    return {
        "native_producer": native,
        "packaging": candidate,
        "packaging_only_paths": sorted(changed),
    }


def version() -> tuple[str, str]:
    source = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"][
        "package"
    ]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+\+multi\.\d+", source):
        raise ValueError("Expected a Rust version with a downstream +multi revision")
    return source, source.replace("+multi.", "-multi.", 1)


def verify_acceptance(
    value: dict | None, producer: str, *, mode: str = "complete"
) -> None:
    if mode not in ("complete", "native-only"):
        raise ValueError("Unknown candidate mode")
    if value is None:
        if mode == "native-only":
            return
        raise ValueError(
            "Complete candidates require the local release acceptance receipt"
        )
    producer_identity = identity(producer)
    expected = {
        "schema": 1,
        "status": "success",
        "source_commit": producer,
        "native_tree": producer_identity["native_tree"],
        "proof": "complete",
        "tier": "release",
    }
    if {key: value.get(key) for key in expected} != expected:
        raise ValueError(
            "Local acceptance is not a complete release proof for the native producer"
        )
    if value.get("source_tree") not in (None, producer_identity["source_tree"]):
        raise ValueError(
            "Local acceptance source tree differs from its producer commit"
        )
    if (
        not isinstance(value.get("run_id"), str)
        or not value["run_id"]
        or not isinstance(value.get("receipt_id"), str)
        or not value["receipt_id"]
        or not re.fullmatch(r"[0-9a-f]{64}", value.get("source_sha256", ""))
        or not re.fullmatch(r"[0-9a-f]{64}", value.get("manifest_sha256", ""))
    ):
        raise ValueError(
            "Local acceptance needs its retained run, receipt, source and manifest digests"
        )


def verify_artifacts(directory: Path, packaging: str) -> list[dict]:
    try:
        from verify_npm_release import PLATFORMS
    except ModuleNotFoundError:
        from scripts.verify_npm_release import PLATFORMS
    result = []
    for platform in PLATFORMS.values():
        target = platform["target_triple"]
        folder = directory / target
        receipt = json.loads((folder / "native-manifest.json").read_text())
        producer = receipt.get("source_commit")
        source = verify_native_source(producer, packaging)["native_producer"]
        expected = {
            "native_tree": source["native_tree"],
            "target": target,
            "version": version()[0],
        }
        if {key: receipt.get(key) for key in expected} != expected:
            raise ValueError(f"Native receipt identity differs: {target}")
        if receipt.get("schema") != 1 or receipt.get("status") != "success":
            raise ValueError(f"Native receipt status is not successful: {target}")
        pair = receipt.get("v8_pair", {})
        pair_files = dict(pair.get("files", {}))
        if not pair_files:
            for key in ("archive", "binding", "manifest"):
                item = pair.get(key)
                if (
                    isinstance(item, dict)
                    and isinstance(item.get("name"), str)
                    and isinstance(item.get("sha256"), str)
                ):
                    pair_files[item["name"]] = item["sha256"]
        archive = (
            f"rusty_v8_ptrcomp_sandbox_release_{target}.lib.gz"
            if platform["os"] == "win32"
            else f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz"
        )
        binding = f"src_binding_ptrcomp_sandbox_release_{target}.rs"
        source_built = pair.get("source_built")
        if source_built is None:
            source_built = pair.get("producer") == "source-native-windows"
        if (
            (source_built is not True and target != "aarch64-pc-windows-msvc")
            or pair.get("profile") != "ptrcomp_sandbox_release"
            and pair.get("profile") != "sandbox_release"
            or any(
                not re.fullmatch(r"[0-9a-f]{64}", pair_files.get(name, ""))
                for name in (archive, binding)
            )
        ):
            raise ValueError(
                f"Native receipt has no matching V8 archive/binding pair: {target}"
            )
        if receipt.get("v8_tree") not in (
            None,
            git("rev-parse", f"{packaging}:third_party/v8"),
        ):
            raise ValueError(f"V8 source inputs differ: {target}")
        if target == "aarch64-pc-windows-msvc":
            reason = receipt.get("runtime_not_executed_reason") or receipt.get(
                "runtime_not_executed"
            )
            if receipt.get("runtime_executed") is not False or not reason:
                raise ValueError(
                    "Windows ARM64 receipt must state the cross-compilation boundary"
                )
        elif (
            receipt.get("runtime_executed") is not True
            or pair.get("cppgc_ignored_tests_executed") is not True
        ):
            raise ValueError(f"Native V8/CppGC execution is unverified: {target}")
        checks = receipt.get("checks")
        if (
            not isinstance(checks, list)
            or not checks
            or any(
                check.get("exit_code") != 0
                or not re.fullmatch(r"[0-9a-f]{64}", check.get("log_sha256", ""))
                for check in checks
            )
        ):
            raise ValueError(f"Native checks are not successful and retained: {target}")
        filename = f"codex-package-{target}.tar.gz"
        artifact = folder / filename
        digest = hashlib.sha256()
        with artifact.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
        observed = {"sha256": digest.hexdigest(), "size_bytes": artifact.stat().st_size}
        if receipt.get("archives", {}).get(filename) != observed:
            raise ValueError(
                f"Native package bytes differ from the producer receipt: {target}"
            )
        result.append(
            {
                "target": target,
                "source_commit": producer,
                "native_tree": source["native_tree"],
                "archive": observed,
                "runtime_executed": receipt["runtime_executed"],
            }
        )
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--producer", required=True)
    parser.add_argument("--packaging", required=True)
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("--local-acceptance")
    parser.add_argument("--candidate-mode", choices=("complete", "native-only"))
    parser.add_argument("--artifacts-dir", type=Path)
    args = parser.parse_args()
    result = verify_native_source(args.producer, args.packaging)
    if args.local_acceptance or args.candidate_mode:
        verify_acceptance(
            json.loads(args.local_acceptance) if args.local_acceptance else None,
            args.producer,
            mode=args.candidate_mode or "complete",
        )
    if args.artifacts_dir:
        result["native_artifacts"] = verify_artifacts(
            args.artifacts_dir, args.packaging
        )
    source, npm = version()
    result.update({"source_version": source, "npm_version": npm})
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(
                f"source_version={source}\nnpm_version={npm}\nnative_tree={result['packaging']['native_tree']}\n"
            )
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
