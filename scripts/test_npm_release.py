#!/usr/bin/env python3
"""Focused checks for release identity and archive naming guards."""

import importlib.util
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
from release_identity import verify_acceptance, verify_hosted_native_source

SPEC = importlib.util.spec_from_file_location(
    "verify_candidate_run",
    Path(__file__).resolve().parents[1] / "codex-cli/scripts/verify_candidate_run.py",
)
candidate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(candidate)


class NpmReleaseContractTest(unittest.TestCase):
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
        harness = """set -euo pipefail
run_check() { shift; "$@"; }
just() { "$NPM_FIXTURE_PYTHON" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@"; }
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
                        ["bash", "-c", harness + command],
                        check=True,
                        capture_output=True,
                        text=True,
                        env={
                            **os.environ,
                            "TARGET": target,
                            "SOURCE_VERSION": "0.166.0+multi.1",
                            "release": str(release),
                            "dist": str(dist),
                            "NPM_FIXTURE_PYTHON": sys.executable,
                        },
                    )
                    expected = [
                        "assemble-codex-package",
                        "--target",
                        target,
                        "--package-version",
                        "0.166.0+multi.1",
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
                    self.assertEqual(json.loads(result.stdout), expected)

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
                *(f"Build and test {target}" for target in candidate.HOSTED_TARGETS),
            )
        ]
        candidate.verify_candidate_run(
            run, repository, commit, jobs, hosted_native=True
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

    def test_release_names_include_root_and_all_six_platforms(self) -> None:
        paths = tarball_paths(Path("dist/npm"), "0.166.0-multi.1")
        self.assertEqual(set(paths), {"root", *PLATFORMS})
        self.assertEqual(len(set(paths.values())), 7)

    def test_build_metadata_is_rejected_before_archive_lookup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "build metadata"):
                verify_release(Path(directory), "0.166.0+multi.1")


if __name__ == "__main__":
    unittest.main()
