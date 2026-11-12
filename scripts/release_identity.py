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
        "scripts/package_native_release.py",
        "scripts/account_cli_compat/captured_credits.py",
        "scripts/local_deploy_owner.py",
        "scripts/local_deploy_rehearsal.py",
        "scripts/local_deploy_rpc.py",
        "scripts/qualify_local_delivery.py",
        "scripts/test_local_deploy_owner.py",
        "scripts/test_local_deploy_rpc.py",
        "scripts/prepare_local_usage_package.py",
        "docs/local-usage-api.md",
        "sdk/python/src/openai_codex/generated/v2_all.py",
        "scripts/test_downstream_windows_contract.py",
        "codex-cli/scripts/verify_candidate_run.py",
        ".github/workflows/downstream-candidate.yml",
        ".github/workflows/downstream-unix-native-target.yml",
        ".github/workflows/downstream-npm-publish.yml",
        ".github/workflows/trusted-windows-native.yml",
        ".github/scripts/downstream-windows-native.ps1",
        ".github/scripts/downstream-unix-native.sh",
        ".github/scripts/bootstrap_bazel_distdir.py",
        ".github/scripts/test_bootstrap_bazel_distdir.py",
        ".devcoordinator.toml",
    }
)

HOSTED_RECIPE_PATHS = (
    ".github/workflows/downstream-unix-native-target.yml",
    ".github/scripts/downstream-unix-native.sh",
    ".github/scripts/bootstrap_bazel_distdir.py",
)

# These are the files that determine the Rusty V8 pair produced by the
# hosted source-v8 phase. Keep this manifest independent of the native package
# recipe: package-only verification changes must not force a V8 rebuild, while
# a change to a locked dependency, toolchain setup, or Bazel target definition
# must invalidate reuse.
V8_INPUT_PATHS = (
    ".bazelversion",
    ".bazelrc",
    "BUILD.bazel",
    "defs.bzl",
    "rbe.bzl",
    "bazel/**",
    "scripts/workspace-status.sh",
    "scripts/workspace-status.cmd",
    "MODULE.bazel",
    "MODULE.bazel.lock",
    "codex-rs/Cargo.toml",
    "codex-rs/Cargo.lock",
    ".github/scripts/rusty_v8_bazel.py",
    ".github/scripts/rusty_v8_module_bazel.py",
    ".github/scripts/run_bazel_with_buildbuddy.py",
    ".github/actions/prepare-bazel-ci/**",
    ".github/actions/setup-ci/**",
    "third_party/v8/**",
)

# Product fixes allowed alongside a reused V8 producer. Keep this explicit so
# a change to any other native or build input still forces a fresh V8 pair.
V8_REUSE_SOURCE_PREFIXES = (
    "scripts/run_isolated_workspace_tests.sh",
    "scripts/test_run_isolated_workspace_tests.py",
    "codex-rs/usage/",
    "codex-rs/app-server/src/request_processors/thread_processor.rs",
    "codex-rs/app-server/src/request_processors/thread_listener_shutdown_tests.rs",
    "codex-rs/ext/image-generation/",
    "codex-rs/ext/web-search/",
    "codex-rs/ext/history-notes/",
    "codex-rs/ext/git-attribution/",
    "codex-rs/model-provider/src/auth.rs",
    "codex-rs/app-server/tests/suite/v2/imagegen_extension.rs",
    "codex-rs/app-server/tests/suite/v2/account_failover_tests.rs",
    "codex-rs/app-server/tests/suite/v2/mod.rs",
    "codex-rs/core/src/session/account_routing",
    "codex-rs/core/src/session/compaction_account_routing.rs",
    "codex-rs/core/src/session/step_context.rs",
    "codex-rs/core/src/session/turn_context_account_routing.rs",
    "codex-rs/core/src/session/turn_context.rs",
    "codex-rs/core/src/session/turn.rs",
    "codex-rs/core/src/session/mod.rs",
    "codex-rs/core/src/session/tests/guardian_tests.rs",
    "codex-rs/core/src/tasks/regular.rs",
    "codex-rs/core/src/compact.rs",
    "codex-rs/core/src/compact_remote_v2.rs",
    "codex-rs/core/src/compact_remote_v2_attempt.rs",
    "codex-rs/core/src/compact_remote_v2_installation.rs",
    "codex-rs/core/src/responses_retry.rs",
    "codex-rs/core/src/usage_capture.rs",
    "codex-rs/core/src/usage_capture/",
    "codex-rs/core/tests/suite/account_profiles.rs",
    "codex-rs/core/tests/suite/account_failover_tests.rs",
    "codex-rs/core/tests/suite/hooks.rs",
    "codex-rs/tui/src/pets/mod.rs",
    "codex-rs/ext/extension-api/src/state.rs",
    "codex-rs/ext/extension-api/tests/state.rs",
    "codex-rs/login/src/auth/profile/router/selection_probe.rs",
    "codex-rs/login/src/auth/profile/router_tests.rs",
    "codex-rs/app-server-protocol/schema/",
    "codex-rs/app-server-protocol/src/protocol/common.rs",
    "codex-rs/app-server-protocol/src/protocol/v2/local_usage",
    "codex-rs/app-server-protocol/src/protocol/v2/mod.rs",
    "codex-rs/app-server/BUILD.bazel",
    "codex-rs/app-server/src/message_processor.rs",
    "codex-rs/app-server/src/request_processors.rs",
    "codex-rs/app-server/src/request_processors/initialize_processor.rs",
    "codex-rs/app-server/src/request_processors/local_usage",
    "codex-rs/app-server/tests/common/test_app_server.rs",
    "codex-rs/app-server/tests/fixtures/local_usage/",
    "codex-rs/app-server/tests/suite/v2/initialize.rs",
    "codex-rs/app-server/tests/suite/v2/local_usage.rs",
    "codex-rs/core/src/lib.rs",
    "codex-rs/core/src/usage_identity.rs",
)


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True).strip()


def _v8_input_paths(commit: str) -> list[str]:
    paths: set[str] = set()
    for pattern in V8_INPUT_PATHS:
        if pattern.endswith("/**"):
            prefix = pattern[:-3]
            paths.update(
                git("ls-tree", "-r", "--name-only", commit, "--", prefix).splitlines()
            )
        else:
            git("cat-file", "-e", f"{commit}:{pattern}")
            paths.add(pattern)
    if not paths:
        raise ValueError("V8 input manifest is empty")
    return sorted(paths)


def v8_input_manifest(commit: str) -> dict:
    entries = {
        path: git("rev-parse", f"{commit}:{path}") for path in _v8_input_paths(commit)
    }
    encoded = json.dumps(
        entries, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("utf-8")
    return {
        "schema": 1,
        "paths": entries,
        "sha256": hashlib.sha256(encoded).hexdigest(),
    }


def _extract_shell_function(script: str, name: str) -> str:
    match = re.search(
        rf"^{re.escape(name)}\(\) \{{\n.*?^\}}\n",
        script,
        flags=re.MULTILINE | re.DOTALL,
    )
    if not match:
        raise ValueError(f"Hosted native recipe is missing {name}()")
    return match.group(0)


def _extract_v8_global_setup(script: str) -> str:
    try:
        start = script.index("phase=")
        end = script.index("write_pair_receipt()")
    except ValueError as exc:
        raise ValueError("Hosted native recipe is missing its V8 setup") from exc
    setup = script[start:end]
    return re.sub(r'^: "\$\{V8_PRODUCER_COMMIT:=\}"\n', "", setup, flags=re.MULTILINE)


def _extract_yaml_job(workflow: str, name: str) -> str:
    match = re.search(
        rf"^  {re.escape(name)}:\n.*?(?=^  [A-Za-z0-9_-]+:\n|\Z)",
        workflow,
        flags=re.MULTILINE | re.DOTALL,
    )
    if not match:
        raise ValueError(f"Hosted native workflow is missing the {name} job")
    return match.group(0)


def _verify_v8_recipe(producer: str, packaging: str) -> None:
    scripts = [
        git("show", f"{commit}:.github/scripts/downstream-unix-native.sh")
        for commit in (producer, packaging)
    ]
    if _extract_shell_function(scripts[0], "source_v8") != _extract_shell_function(
        scripts[1], "source_v8"
    ):
        raise ValueError("Hosted V8 source recipe changed after its producer run")
    for name in ("run_check",):
        if _extract_shell_function(scripts[0], name) != _extract_shell_function(
            scripts[1], name
        ):
            raise ValueError("Hosted V8 source recipe changed after its producer run")
    if _extract_v8_global_setup(scripts[0]) != _extract_v8_global_setup(scripts[1]):
        raise ValueError("Hosted V8 setup changed after its producer run")

    workflows = [
        git("show", f"{commit}:.github/workflows/downstream-unix-native-target.yml")
        for commit in (producer, packaging)
    ]
    source_jobs = []
    for workflow in workflows:
        job = _extract_yaml_job(workflow, "v8")
        job = re.sub(r"^    if: inputs\.v8_run_id == ''\n", "", job, flags=re.MULTILINE)
        source_jobs.append(job)
    if source_jobs[0] != source_jobs[1]:
        raise ValueError(
            "Hosted V8 toolchain or source job changed after its producer run"
        )

    candidate_workflows = [
        git("show", f"{commit}:.github/workflows/downstream-candidate.yml")
        for commit in (producer, packaging)
    ]
    native_jobs = []
    for workflow in candidate_workflows:
        job = _extract_yaml_job(workflow, "native")
        job = job.replace(" && inputs.hosted_v8_run_id == ''", "")
        job = re.sub(r"^      v8_run_id:.*\n", "", job, flags=re.MULTILINE)
        job = re.sub(r"^      v8_source_commit:.*\n", "", job, flags=re.MULTILINE)
        native_jobs.append(job)
    if native_jobs[0] != native_jobs[1]:
        raise ValueError(
            "Hosted native matrix or package inputs changed after its producer run"
        )
    for pattern in (r"^(?:env|defaults|permissions):\n.*?(?=^\S|\Z)",):
        before, after = [
            re.findall(pattern, workflow, flags=re.MULTILINE | re.DOTALL)
            for workflow in candidate_workflows
        ]
        if not before or before != after:
            raise ValueError("Hosted V8 global workflow settings changed")


def identity(commit: str) -> dict:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Source commit must be a full lowercase SHA-1")
    git("cat-file", "-e", f"{commit}^{{commit}}")
    return {
        "source_commit": commit,
        "source_tree": git("rev-parse", f"{commit}^{{tree}}"),
        "native_tree": git("rev-parse", f"{commit}:codex-rs"),
        "v8_tree": git("rev-parse", f"{commit}:third_party/v8"),
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
    if (
        unexpected
        or native["native_tree"] != candidate["native_tree"]
        or native["v8_tree"] != candidate["v8_tree"]
    ):
        raise ValueError(
            f"Native reuse changes product/build inputs: {sorted(unexpected)}"
        )
    return {
        "native_producer": native,
        "packaging": candidate,
        "packaging_only_paths": sorted(changed),
    }


def verify_hosted_native_source(producer: str, packaging: str) -> dict:
    result = verify_native_source(producer, packaging)
    if any(path in result["packaging_only_paths"] for path in HOSTED_RECIPE_PATHS):
        raise ValueError("Hosted native recipe changed after its producer run")
    for path in HOSTED_RECIPE_PATHS:
        try:
            recipes = [
                git("show", f"{commit}:{path}") for commit in (producer, packaging)
            ]
        except subprocess.CalledProcessError as exc:
            raise ValueError(f"Hosted native recipe is missing from {path}") from exc
        if recipes[0] != recipes[1]:
            raise ValueError(
                f"Hosted native recipe changed after its producer run: {path}"
            )
    workflows = [
        git("show", f"{commit}:.github/workflows/downstream-candidate.yml")
        for commit in (producer, packaging)
    ]
    # Keep the hosted native wrapper and workflow-wide execution settings
    # byte-identical. Identity, handoff and npm orchestration may evolve.
    for pattern in (
        r"^  native:\n.*?(?=^  [\w-]+:\n|\Z)",
        r"^(?:env|defaults|permissions):[^\n]*(?:\n|\Z).*?(?=^\S|\Z)",
    ):
        before, after = [
            re.findall(pattern, workflow, flags=re.MULTILINE | re.DOTALL)
            for workflow in workflows
        ]
        if not before or before != after:
            raise ValueError("Hosted native job or global build settings changed")
    return result


def verify_hosted_v8_source(producer: str, packaging: str) -> dict:
    native = identity(producer)
    candidate = identity(packaging)
    subprocess.run(
        ["git", "-C", str(ROOT), "merge-base", "--is-ancestor", producer, packaging],
        check=True,
    )
    changed = set(
        git("diff", "--name-only", "--no-renames", producer, packaging).splitlines()
    )
    unexpected = {
        path
        for path in changed
        if path not in PACKAGING_PATHS
        and not any(path.startswith(prefix) for prefix in V8_REUSE_SOURCE_PREFIXES)
    }
    if unexpected:
        raise ValueError(
            f"V8 reuse changes unapproved product/build inputs: {sorted(unexpected)}"
        )
    producer_inputs = v8_input_manifest(producer)
    candidate_inputs = v8_input_manifest(packaging)
    if producer_inputs != candidate_inputs:
        raise ValueError("Hosted V8 locked inputs or build recipe changed")
    _verify_v8_recipe(producer, packaging)
    return {
        "native_producer": native,
        "packaging": candidate,
        "packaging_only_paths": sorted(changed),
        "v8_inputs": producer_inputs,
        "reuse_source_prefixes": list(V8_REUSE_SOURCE_PREFIXES),
    }


def version() -> tuple[str, str]:
    source = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"][
        "package"
    ]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+\+multi\.\d+", source):
        raise ValueError("Expected a Rust version with a downstream +multi revision")
    return source, source.replace("+multi.", "-multi.", 1)


def verify_new_downstream_version(source: str) -> None:
    """Reject a downstream version that has already been tagged for this upstream release."""
    match = re.fullmatch(r"(?P<base>\d+\.\d+\.\d+)\+multi\.(?P<revision>\d+)", source)
    if match is None:
        raise ValueError("Expected a Rust version with a downstream +multi revision")
    revision = int(match.group("revision"))
    base = match.group("base")
    tags = git("tag", "--list", f"v{base}-multi.*", f"npm-v{base}-multi.*").splitlines()
    prior = []
    for tag in tags:
        tag_match = re.fullmatch(
            r"(?:npm-)?v" + re.escape(base) + r"-multi\.(\d+)", tag
        )
        if tag_match is not None:
            prior.append((int(tag_match.group(1)), tag))
    if prior and revision <= max(number for number, _ in prior):
        highest, tag = max(prior)
        raise ValueError(
            f"Downstream version {source} is not newer than existing tag {tag}; "
            f"use +multi.{highest + 1} or later"
        )


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
        pair_source_commit = pair.get("source_commit") or producer
        if not re.fullmatch(r"[0-9a-f]{40}", pair_source_commit):
            raise ValueError(f"V8 pair source commit is invalid: {target}")
        pair_source = identity(pair_source_commit)
        for field in ("source_tree", "native_tree", "v8_tree"):
            if field in pair and pair[field] != pair_source[field]:
                raise ValueError(f"V8 pair {field} differs from its source: {target}")
        expected_v8_inputs = v8_input_manifest(pair_source_commit)
        if v8_input_manifest(packaging) != expected_v8_inputs:
            raise ValueError(f"V8 locked inputs differ: {target}")
        if pair.get("v8_inputs") not in (None, expected_v8_inputs):
            raise ValueError(f"V8 receipt input manifest differs: {target}")
        for key in ("producer_run_id", "producer_run_attempt"):
            if pair.get(key) is not None and not re.fullmatch(
                r"[0-9]+", str(pair[key])
            ):
                raise ValueError(f"V8 producer run identity is invalid: {target}")
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
                "v8_source_commit": pair_source_commit,
                "v8_inputs_sha256": expected_v8_inputs["sha256"],
                "v8_producer_run_id": pair.get("producer_run_id"),
                "v8_producer_run_attempt": pair.get("producer_run_attempt"),
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
    parser.add_argument("--hosted-native", action="store_true")
    parser.add_argument("--hosted-v8", action="store_true")
    args = parser.parse_args()
    if args.hosted_native and args.hosted_v8:
        parser.error("--hosted-native and --hosted-v8 are mutually exclusive")
    if args.hosted_v8:
        result = verify_hosted_v8_source(args.producer, args.packaging)
    elif args.hosted_native:
        result = verify_hosted_native_source(args.producer, args.packaging)
    else:
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
    verify_new_downstream_version(source)
    result.update({"source_version": source, "npm_version": npm})
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(
                f"source_version={source}\nnpm_version={npm}\nnative_tree={result['packaging']['native_tree']}\n"
            )
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
