#!/usr/bin/env python3
"""Focused checks for release identity and archive naming guards."""

import importlib.util
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from verify_npm_release import PLATFORMS, tarball_paths, verify_release
from release_identity import (
    V8_INPUT_PATHS,
    verify_acceptance,
    verify_hosted_native_source,
    verify_hosted_v8_source,
    verify_new_downstream_version,
)

SPEC = importlib.util.spec_from_file_location(
    "verify_candidate_run",
    Path(__file__).resolve().parents[1] / "codex-cli/scripts/verify_candidate_run.py",
)
candidate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(candidate)


class NpmReleaseContractTest(unittest.TestCase):
    def test_downstream_version_must_advance_existing_tag(self) -> None:
        with patch("release_identity.git", return_value="npm-v0.166.0-multi.4\n"):
            with self.assertRaisesRegex(ValueError, r"use \+multi\.5"):
                verify_new_downstream_version("0.166.0+multi.4")

    @unittest.skipUnless(
        os.name == "posix" and shutil.which("bash"), "requires Unix Bash"
    )
    def test_unix_package_command_preserves_common_and_optional_arguments(self) -> None:
        source = (
            Path(__file__).resolve().parents[1]
            / ".github/scripts/downstream-unix-native.sh"
        ).read_text()
        command = source[
            source.index("package_args=(") : source.index("run_check native-version")
        ]
        preparation = source[
            source.index('if [[ "$TARGET" == *-linux-musl ]]') : source.index('root="')
        ]
        harness = """set -euo pipefail
ulimit -S -n 1024
run_check() { shift; "$@"; }
just() { "$NPM_FIXTURE_PYTHON" -c 'import json,os,resource,sys; print(json.dumps({"argv":sys.argv[1:], "ldflags":os.environ[os.environ["TARGET"].upper().replace("-", "_")+"_LDFLAGS"], "fileLimit":resource.getrlimit(resource.RLIMIT_NOFILE)[0]}))' "$@"; }
"""
        with tempfile.TemporaryDirectory(prefix="npm recipe ") as directory:
            for platform in PLATFORMS.values():
                if platform["os"] == "win32":
                    continue
                target = platform["target_triple"]
                with self.subTest(target=target):
                    release = Path(directory) / target / "release"
                    dist = Path(directory) / "dist" / target
                    result = subprocess.run(
                        ["bash", "-c", harness + preparation + command],
                        check=True,
                        capture_output=True,
                        text=True,
                        env={
                            **os.environ,
                            "TARGET": target,
                            "SOURCE_VERSION": "0.166.0+multi.5",
                            "release": str(release),
                            "dist": str(dist),
                            "NPM_FIXTURE_PYTHON": sys.executable,
                            target.upper().replace("-", "_")
                            + "_LDFLAGS": "-Wl,--as-needed",
                        },
                    )
                    expected = [
                        "assemble-codex-package",
                        "--target",
                        target,
                        "--package-version",
                        "0.166.0+multi.5",
                        "--cargo-profile",
                        "release",
                        "--strip",
                        "all",
                        "--entrypoint-bin",
                        str(release / "codex"),
                        "--code-mode-host-bin",
                        str(release / "codex-code-mode-host"),
                        "--exec-server-bin",
                        str(release / "codex-exec-server"),
                    ]
                    if platform["os"] == "linux":
                        expected.extend(["--bwrap-bin", str(release / "bwrap")])
                    expected.extend(
                        [
                            "--package-dir",
                            str(dist / "package"),
                            "--archive-output",
                            str(dist / f"codex-package-{target}.tar.gz"),
                            "--archive-output",
                            str(dist / f"codex-package-{target}.tar.zst"),
                        ]
                    )
                    self.assertEqual(
                        json.loads(result.stdout),
                        {
                            "argv": expected,
                            "ldflags": "-Wl,--as-needed"
                            + (
                                " -Wno-unused-command-line-argument"
                                if platform["os"] == "linux"
                                else ""
                            ),
                            "fileLimit": 16384 if platform["os"] == "linux" else 1024,
                        },
                    )

    def test_hosted_reuse_keeps_the_real_producer_and_unchanged_native_inputs(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args], text=True, stderr=subprocess.PIPE
                ).strip()

            def commit(path: str, content: str) -> str:
                destination = root / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(content)
                git("add", ".")
                git(
                    "-c",
                    "user.name=release-fixture",
                    "-c",
                    "user.email=fixture@localhost",
                    "commit",
                    "-qm",
                    "fixture",
                )
                return git("rev-parse", "HEAD")

            git("init", "-q")
            inputs = {
                "codex-rs/product.rs": "product source\n",
                "third_party/v8/source.bzl": "V8 source\n",
                "MODULE.bazel": "toolchain source\n",
                ".github/scripts/rusty_v8_bazel.py": "V8 helper\n",
                ".github/scripts/prepare_downstream_musl.py": "musl helper\n",
                ".github/scripts/downstream-unix-native.sh": "hosted helper\n",
                ".github/scripts/bootstrap_bazel_distdir.py": "distdir helper\n",
                ".github/workflows/downstream-unix-native-target.yml": "name: target\non:\n  workflow_call:\njobs:\n  v8:\n    name: V8 source pair\n  package:\n    name: Package native target\n",
                ".github/workflows/downstream-candidate.yml": "env:\n  BUILD_MODE: release\njobs:\n  native:\n    runs-on: ubuntu-24.04-arm\n    steps:\n      - run: echo hosted\n  npm:\n    steps:\n      - run: echo assembly\n",
            }
            for path, content in inputs.items():
                producer = commit(path, content)
            packaging = commit(
                ".github/workflows/trusted-windows-native.yml", "Windows checkout fix\n"
            )
            with patch("release_identity.ROOT", root):
                result = verify_hosted_native_source(producer, packaging)
                self.assertEqual(result["native_producer"]["source_commit"], producer)
                self.assertEqual(result["packaging"]["source_commit"], packaging)
                self.assertEqual(
                    result["native_producer"]["native_tree"],
                    result["packaging"]["native_tree"],
                )
                workflow = ".github/workflows/downstream-candidate.yml"
                orchestration = commit(
                    workflow,
                    inputs[workflow].replace("echo assembly", "echo verified-assembly"),
                )
                verify_hosted_native_source(producer, orchestration)
                commit(workflow, inputs[workflow])
                for path, original in inputs.items():
                    with self.subTest(changed_input=path):
                        changed = (
                            original.replace("echo hosted", "echo changed")
                            if path.endswith("downstream-candidate.yml")
                            else original + "changed\n"
                        )
                        invalid = commit(path, changed)
                        with self.assertRaises(ValueError):
                            verify_hosted_native_source(producer, invalid)
                        packaging = commit(path, original)
                        verify_hosted_native_source(producer, packaging)
                invalid = commit(
                    workflow,
                    inputs[workflow].replace(
                        "BUILD_MODE: release", "BUILD_MODE: changed"
                    ),
                )
                with self.assertRaisesRegex(ValueError, "global build settings"):
                    verify_hosted_native_source(producer, invalid)
                invalid = commit(
                    workflow,
                    "defaults: {run: {shell: bash}}\n" + inputs[workflow],
                )
                with self.assertRaisesRegex(ValueError, "global build settings"):
                    verify_hosted_native_source(producer, invalid)

    def test_hosted_v8_reuse_allows_usage_fix_and_rejects_v8_input_changes(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args],
                    text=True,
                    stderr=subprocess.PIPE,
                ).strip()

            def commit(message: str) -> str:
                git("add", "-f", ".")
                git(
                    "-c",
                    "user.name=release-fixture",
                    "-c",
                    "user.email=fixture@localhost",
                    "commit",
                    "-qm",
                    message,
                )
                return git("rev-parse", "HEAD")

            git("init", "-q")
            for pattern in V8_INPUT_PATHS:
                if pattern.endswith("/**"):
                    path = Path(pattern[:-3]) / "fixture.bzl"
                else:
                    path = Path(pattern)
                path = root / path
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(f"{path}\n")
            (root / ".github/scripts").mkdir(parents=True, exist_ok=True)
            native_script = (
                "phase=full\nroot=fixture\nrun_check() {\n  echo check\n}\n"
                "write_pair_receipt() {\n  echo receipt\n}\n"
                "source_v8() {\n  echo source\n}\n\npackage_native() {\n  echo package\n}\n"
            )
            script_path = root / ".github/scripts/downstream-unix-native.sh"
            script_path.write_text(native_script)
            (root / ".github/workflows").mkdir(parents=True, exist_ok=True)
            (root / ".github/workflows/downstream-unix-native-target.yml").write_text(
                "name: target\njobs:\n  v8:\n    name: V8 source pair\n    runs-on: ubuntu\n    steps:\n      - run: echo source\n  package:\n    name: Package native target\n"
            )
            (root / ".github/workflows/downstream-candidate.yml").write_text(
                "env:\n  BUILD_MODE: release\njobs:\n  native:\n    if: inputs.hosted_native_run_id == ''\n    uses: target\n    with:\n      target: test\n  npm:\n    name: npm\n"
            )
            producer = commit("producer")
            usage = root / "codex-rs/usage/src/runtime.rs"
            usage.parent.mkdir(parents=True, exist_ok=True)
            usage.write_text("usage fix\n")
            packaging = commit("usage fix")

            with patch("release_identity.ROOT", root):
                result = verify_hosted_v8_source(producer, packaging)
                self.assertEqual(result["native_producer"]["source_commit"], producer)
                self.assertEqual(result["packaging"]["source_commit"], packaging)
                self.assertEqual(result["v8_inputs"]["schema"], 1)

                app_server_fix = (
                    root
                    / "codex-rs/app-server/src/request_processors/thread_processor.rs"
                )
                app_server_fix.parent.mkdir(parents=True, exist_ok=True)
                app_server_fix.write_text("stale writer fix\n")
                app_server_test = (
                    root
                    / "codex-rs/app-server/src/request_processors/thread_listener_shutdown_tests.rs"
                )
                app_server_test.write_text("stale writer regression\n")
                packaging = commit("app-server reconnect fix")
                result = verify_hosted_v8_source(producer, packaging)
                self.assertIn(
                    str(app_server_fix.relative_to(root)),
                    result["packaging_only_paths"],
                )

                account_routing = root / "codex-rs/core/src/session/account_routing.rs"
                account_routing.parent.mkdir(parents=True, exist_ok=True)
                account_routing.write_text("account continuation fix\n")
                packaging = commit("managed account failover fix")
                result = verify_hosted_v8_source(producer, packaging)
                self.assertEqual(result["v8_inputs"]["schema"], 1)
                self.assertIn(
                    str(account_routing.relative_to(root)),
                    result["packaging_only_paths"],
                )

                unrelated_app_server = root / "codex-rs/app-server/src/lib.rs"
                unrelated_app_server.write_text("unrelated app-server change\n")
                changed_app_server = commit("unapproved app-server change")
                with self.assertRaisesRegex(
                    ValueError, "unapproved product/build inputs"
                ):
                    verify_hosted_v8_source(producer, changed_app_server)
                unrelated_app_server.unlink()

                script_path.write_text(
                    native_script.replace("phase=full", "phase=debug")
                )
                changed_setup = commit("changed V8 setup")
                with self.assertRaisesRegex(ValueError, "V8 setup"):
                    verify_hosted_v8_source(producer, changed_setup)
                script_path.write_text(
                    native_script.replace("echo check", "echo changed")
                )
                changed_helper = commit("changed V8 helper")
                with self.assertRaisesRegex(ValueError, "source recipe"):
                    verify_hosted_v8_source(producer, changed_helper)
                script_path.write_text(native_script)
                commit("restore V8 recipe")
                (root / "third_party/v8/fixture.bzl").write_text("changed\n")
                changed_inputs = commit("changed V8 input")
                with self.assertRaisesRegex(
                    ValueError, "V8 reuse changes|locked inputs"
                ):
                    verify_hosted_v8_source(producer, changed_inputs)

    @unittest.skipUnless(
        os.name == "posix" and shutil.which("bash"), "requires Unix Bash"
    )
    def test_v8_handoff_rejects_corruption_and_identity_mismatch(self) -> None:
        script = (
            Path(__file__).resolve().parents[1]
            / ".github/scripts/downstream-unix-native.sh"
        )
        target = "x86_64-apple-darwin"
        with tempfile.TemporaryDirectory() as directory:
            handoff = Path(directory) / "handoff"
            pair = handoff / "pair"
            logs = handoff / "logs"
            pair.mkdir(parents=True)
            logs.mkdir()
            (logs / "v8-source-pair.log").write_text("source\n")
            (logs / "v8-bazel-shutdown.log").write_text("shutdown\n")
            archive = f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz"
            binding = f"src_binding_ptrcomp_sandbox_release_{target}.rs"
            checksums = f"rusty_v8_ptrcomp_sandbox_release_{target}.sha256"
            (pair / archive).write_bytes(b"archive")
            (pair / binding).write_text("binding\n")

            def digest(path: Path) -> str:
                return hashlib.sha256(path.read_bytes()).hexdigest()

            (pair / checksums).write_text(
                f"{digest(pair / archive)}  {archive}\n"
                f"{digest(pair / binding)}  {binding}\n"
            )
            root = Path(__file__).resolve().parents[1]

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args], text=True
                ).strip()

            (handoff / "v8-pair.json").write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "status": "success",
                        "source_commit": git("rev-parse", "HEAD"),
                        "source_tree": git("rev-parse", "HEAD^{tree}"),
                        "native_tree": git("rev-parse", "HEAD:codex-rs"),
                        "v8_tree": git("rev-parse", "HEAD:third_party/v8"),
                        "target": target,
                        "profile": "sandbox_release",
                        "source_built": True,
                        "files": {
                            name: digest(pair / name)
                            for name in (archive, binding, checksums)
                        },
                        "logs": {
                            name: digest(logs / name)
                            for name in ("v8-source-pair.log", "v8-bazel-shutdown.log")
                        },
                    }
                )
            )
            env = {
                **os.environ,
                "TARGET": target,
                "BAZEL_PLATFORM": "macos_amd64",
                "BAZEL_CONFIG": "ci-macos",
                "SOURCE_VERSION": "0.166.0+multi.5",
                "RUNNER_TEMP": directory,
                "V8_HANDOFF_DIR": str(handoff),
            }
            valid = subprocess.run(
                ["bash", str(script), "verify-pair"],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(valid.returncode, 0, valid.stderr)
            (logs / "v8-source-pair.log").unlink()
            missing_log = subprocess.run(
                ["bash", str(script), "verify-pair"], cwd=root, env=env
            )
            self.assertNotEqual(missing_log.returncode, 0)
            (logs / "v8-source-pair.log").write_text("source\n")
            (pair / archive).write_bytes(b"changed")
            corrupted = subprocess.run(
                ["bash", str(script), "verify-pair"], cwd=root, env=env
            )
            self.assertNotEqual(corrupted.returncode, 0)
            (pair / archive).write_bytes(b"archive")
            receipt = json.loads((handoff / "v8-pair.json").read_text())
            receipt["target"] = "aarch64-apple-darwin"
            (handoff / "v8-pair.json").write_text(json.dumps(receipt))
            mismatched = subprocess.run(
                ["bash", str(script), "verify-pair"], cwd=root, env=env
            )
            self.assertNotEqual(mismatched.returncode, 0)

    def test_only_native_builds_may_start_without_complete_local_proof(self) -> None:
        producer = "a" * 40
        verify_acceptance(None, producer, mode="native-only")
        with self.assertRaisesRegex(ValueError, "require the local release"):
            verify_acceptance(None, producer, mode="complete")

    def test_native_only_success_cannot_qualify_npm_publication(self) -> None:
        commit = "a" * 40
        repository = "holyglory/codex"
        run = {
            "status": "completed",
            "conclusion": "success",
            "path": ".github/workflows/downstream-candidate.yml",
            "event": "workflow_dispatch",
            "head_sha": commit,
            "repository": {"full_name": repository},
            "head_repository": {"full_name": repository},
        }
        jobs = [
            {"name": name, "status": "completed", "conclusion": "success"}
            for name in (
                candidate.CANDIDATE_JOBS[0],
                *candidate.HOSTED_V8_JOBS,
                *candidate.HOSTED_PACKAGE_JOBS,
            )
        ]
        candidate.verify_candidate_run(
            run, repository, commit, jobs, hosted_native=True
        )
        for required in (*candidate.HOSTED_V8_JOBS, *candidate.HOSTED_PACKAGE_JOBS):
            with self.subTest(missing_hosted_job=required):
                missing = [job for job in jobs if job["name"] != required]
                with self.assertRaisesRegex(ValueError, "Required candidate job"):
                    candidate.verify_candidate_run(
                        run, repository, commit, missing, hosted_native=True
                    )
        with self.assertRaisesRegex(ValueError, "Required candidate job"):
            candidate.verify_candidate_run(run, repository, commit, jobs)
        complete_jobs = [
            {"name": name, "status": "completed", "conclusion": "success"}
            for name in candidate.CANDIDATE_JOBS
        ]
        candidate.verify_candidate_run(run, repository, commit, complete_jobs)
        complete_jobs[2]["conclusion"] = "skipped"
        with self.assertRaisesRegex(ValueError, "Required candidate job"):
            candidate.verify_candidate_run(run, repository, commit, complete_jobs)
        with self.assertRaisesRegex(ValueError, "exact-source"):
            candidate.verify_candidate_run(
                run, repository, "b" * 40, jobs, hosted_native=True
            )

    def test_partial_hosted_run_can_prove_only_successful_v8_jobs(self) -> None:
        commit = "a" * 40
        repository = "holyglory/codex"
        run = {
            "status": "completed",
            "conclusion": "failure",
            "path": ".github/workflows/downstream-candidate.yml",
            "event": "workflow_dispatch",
            "head_sha": commit,
            "repository": {"full_name": repository},
            "head_repository": {"full_name": repository},
        }
        jobs = [
            {"name": name, "status": "completed", "conclusion": "success"}
            for name in candidate.HOSTED_V8_JOBS
        ]
        jobs.append(
            {
                "name": candidate.HOSTED_PACKAGE_JOBS[0],
                "status": "completed",
                "conclusion": "failure",
            }
        )
        candidate.verify_candidate_run(run, repository, commit, jobs, hosted_v8=True)
        with self.assertRaisesRegex(ValueError, "Required candidate job"):
            candidate.verify_candidate_run(
                run,
                repository,
                commit,
                jobs[1:],
                hosted_v8=True,
            )
        with self.assertRaisesRegex(ValueError, "terminal state"):
            candidate.verify_candidate_run(
                {**run, "status": "in_progress"},
                repository,
                commit,
                jobs,
                hosted_v8=True,
            )
        with self.assertRaisesRegex(ValueError, "mutually exclusive"):
            candidate.verify_candidate_run(
                run,
                repository,
                commit,
                jobs,
                hosted_native=True,
                hosted_v8=True,
            )

    def test_release_names_include_root_and_all_six_platforms(self) -> None:
        paths = tarball_paths(Path("dist/npm"), "0.166.0-multi.5")
        self.assertEqual(set(paths), {"root", *PLATFORMS})
        self.assertEqual(len(set(paths.values())), 7)

    def test_build_metadata_is_rejected_before_archive_lookup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "build metadata"):
                verify_release(Path(directory), "0.166.0+multi.5")


if __name__ == "__main__":
    unittest.main()
