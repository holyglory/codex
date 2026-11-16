#!/usr/bin/env python3
"""Focused checks for release identity and archive naming guards."""

import importlib.util
import io
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
import unittest
from unittest.mock import patch

from verify_npm_release import PLATFORMS, tarball_paths, verify_release
from release_identity import (
    V8_INPUT_PATHS,
    verify_acceptance,
    verify_hosted_native_source,
    verify_hosted_producer_source,
    verify_hosted_v8_source,
    verify_native_source,
    verify_new_downstream_version,
)
import release_identity
import package_native_release

SPEC = importlib.util.spec_from_file_location(
    "verify_candidate_run",
    Path(__file__).resolve().parents[1] / "codex-cli/scripts/verify_candidate_run.py",
)
candidate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(candidate)


class NpmReleaseContractTest(unittest.TestCase):
    def test_windows_long_build_keeps_target_checkpoints_and_heartbeat(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/trusted-windows-native.yml").read_text()
        helper = (root / ".github/scripts/downstream-windows-native.ps1").read_text()
        self.assertIn("timeout-minutes: 900", workflow)
        self.assertIn(
            "downstream-windows-native.ps1 finalize-target --target x86_64-pc-windows-msvc",
            workflow,
        )
        self.assertIn(
            "downstream-windows-native.ps1 finalize-target --target aarch64-pc-windows-msvc",
            workflow,
        )
        self.assertIn('"state": "running"', helper)
        self.assertIn('"elapsed_seconds"', helper)
        self.assertIn("last_heartbeat", helper)
        self.assertIn("time.sleep(1)", helper)
        self.assertIn('"log_bytes"', helper)
        self.assertIn("self.telemetry", helper)
        self.assertIn('"requested_jobs"', helper)
        self.assertIn("utilization_percent_of_machine", helper)
        self.assertIn("BAZEL_JOBS", workflow)
        self.assertIn('"--timings"', helper)

    def test_exact_reserved_tag_is_allowed_only_for_its_producer(self) -> None:
        for prefix in ("v", "npm-v", "local-v"):
            tag = f"{prefix}0.166.0-multi.7"
            with (
                self.subTest(tag=tag),
                patch(
                    "release_identity.git",
                    side_effect=lambda *args: {
                        (
                            "tag",
                            "--list",
                            "v0.166.0-multi.*",
                            "npm-v0.166.0-multi.*",
                            "local-v0.166.0-multi.*",
                        ): tag,
                        ("rev-parse", f"{tag}^{{commit}}"): "a" * 40,
                    }[args],
                ),
            ):
                verify_new_downstream_version("0.166.0+multi.7", "a" * 40)
                with self.assertRaisesRegex(ValueError, "already reserved"):
                    verify_new_downstream_version("0.166.0+multi.7", "b" * 40)

    def test_native_preflight_checks_local_tags_before_staging_or_building(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "source"
            root.mkdir()

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args], text=True, stderr=subprocess.PIPE
                ).strip()

            def commit() -> None:
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

            git("init", "-q")
            (root / "codex-rs").mkdir()
            (root / "codex-rs/Cargo.toml").write_text(
                '[workspace.package]\nversion = "0.166.0+multi.7"\n'
            )
            product = root / "codex-rs/product.rs"
            product.write_text("original product\n")
            commit()
            git("tag", "local-v0.166.0-multi.7")
            identity = {"source_sha256": "a" * 64, "config_file_sha256": "b" * 64}
            with (
                patch.object(package_native_release, "ROOT", root),
                patch.object(package_native_release, "VERSION", "0.166.0+multi.7"),
                patch.object(release_identity, "ROOT", root),
                patch.object(
                    package_native_release.shutil, "which", return_value="tool"
                ),
                patch.object(package_native_release, "run") as command,
                patch.object(
                    package_native_release, "source_identity", return_value=identity
                ) as source_identity,
            ):
                retry = Path(directory) / "retry"
                package_native_release.preflight(retry)
                self.assertEqual(
                    json.loads((retry / "source.json").read_text()), identity
                )
                command.assert_called_once_with(
                    ["pkg-config", "--exists", "libcap", "openssl"]
                )
                command.reset_mock()
                source_identity.reset_mock()

                for path in (product, root / "untracked.rs"):
                    with self.subTest(dirty_path=path.name):
                        previous = path.read_text() if path.exists() else None
                        path.write_text("changed source\n")
                        rejected = Path(directory) / "dirty"
                        with self.assertRaisesRegex(
                            RuntimeError, "clean and committed"
                        ):
                            package_native_release.preflight(rejected)
                        self.assertFalse(rejected.exists())
                        command.assert_not_called()
                        source_identity.assert_not_called()
                        if previous is None:
                            path.unlink()
                        else:
                            path.write_text(previous)

                product.write_text("new committed product\n")
                commit()
                git("tag", "npm-v0.166.0-multi.7")
                rejected = Path(directory) / "reserved"
                with self.assertRaisesRegex(ValueError, "local-v0.166.0-multi.7"):
                    package_native_release.preflight(rejected)
                self.assertFalse(rejected.exists())
                command.assert_not_called()
                source_identity.assert_not_called()

    def test_downstream_version_must_advance_existing_tag(self) -> None:
        with patch("release_identity.git", return_value="npm-v0.166.0-multi.4\n"):
            with self.assertRaisesRegex(ValueError, r"use \+multi\.5"):
                verify_new_downstream_version("0.166.0+multi.4")

    def test_hosted_cli_reserves_tag_for_product_source(self) -> None:
        product = "a" * 40
        hosted = "b" * 40
        packaging = "c" * 40
        calls = {}

        def relationship(
            product_source: str,
            hosted_producer: str,
            *,
            allow_v8_reuse: bool = False,
        ) -> dict:
            calls["relationship"] = (
                product_source,
                hosted_producer,
                allow_v8_reuse,
            )
            return {"product_source": {"source_commit": product_source}}

        def version_guard(source: str, producer: str) -> None:
            calls["tag"] = (source, producer)

        with (
            patch.object(
                release_identity,
                "verify_hosted_producer_source",
                side_effect=relationship,
            ),
            patch.object(
                release_identity,
                "verify_hosted_native_source",
                return_value={},
            ),
            patch.object(
                release_identity,
                "version",
                return_value=("0.166.0+multi.8", "0.166.0-multi.8"),
            ),
            patch.object(
                release_identity,
                "verify_new_downstream_version",
                side_effect=version_guard,
            ),
            patch.object(
                sys,
                "argv",
                [
                    "release_identity.py",
                    "--producer",
                    hosted,
                    "--product-source",
                    product,
                    "--packaging",
                    packaging,
                    "--hosted-native",
                ],
            ),
            patch.object(sys, "stdout", io.StringIO()),
        ):
            release_identity.main()

        self.assertEqual(calls["relationship"], (product, hosted, False))
        self.assertEqual(calls["tag"], ("0.166.0+multi.8", product))
        calls.clear()
        with (
            patch.object(
                release_identity,
                "verify_hosted_producer_source",
                side_effect=relationship,
            ),
            patch.object(
                release_identity,
                "verify_hosted_v8_source",
                return_value={},
            ),
            patch.object(
                release_identity,
                "version",
                return_value=("0.166.0+multi.8", "0.166.0-multi.8"),
            ),
            patch.object(
                release_identity,
                "verify_new_downstream_version",
                side_effect=version_guard,
            ),
            patch.object(
                sys,
                "argv",
                [
                    "release_identity.py",
                    "--producer",
                    hosted,
                    "--product-source",
                    product,
                    "--packaging",
                    packaging,
                    "--hosted-v8",
                ],
            ),
            patch.object(sys, "stdout", io.StringIO()),
        ):
            release_identity.main()
        self.assertEqual(calls["relationship"], (product, hosted, True))
        self.assertEqual(calls["tag"], ("0.166.0+multi.8", product))

    def test_hosted_cli_rejects_missing_product_source(self) -> None:
        with (
            patch.object(
                sys,
                "argv",
                [
                    "release_identity.py",
                    "--producer",
                    "a" * 40,
                    "--packaging",
                    "b" * 40,
                    "--hosted-v8",
                ],
            ),
            patch.object(sys, "stderr", io.StringIO()),
        ):
            with self.assertRaises(SystemExit):
                release_identity.main()

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

    @unittest.skipUnless(os.name == "posix", "requires a POSIX command fixture")
    def test_windows_acceptance_records_one_external_config_and_command_failure(
        self,
    ) -> None:
        helper = (
            Path(__file__).resolve().parents[1]
            / ".github/scripts/downstream-windows-native.ps1"
        )
        harness = """import os, runpy, sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]).parent))
module = runpy.run_path(sys.argv[1])
producer = module['Producer'].__new__(module['Producer'])
producer.evidence = Path(sys.argv[2])
producer.handoff = producer.evidence / 'windows-handoff'
producer.logs = producer.handoff / 'logs'
producer.logs.mkdir(parents=True)
producer.native_environment = lambda: dict(os.environ)
producer.acceptance()
"""
        with tempfile.TemporaryDirectory(
            prefix="Windows release fixture "
        ) as directory:
            root = Path(directory)
            config = root / "nextest external.toml"
            config.write_text("[profile.default]\nretries = 0\n")
            binary = root / "just"
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import json,os,sys\n"
                "print(json.dumps(sys.argv[1:]))\n"
                "if '--config-file' in sys.argv:\n"
                "    assert sys.argv.count('--config-file') == 1\n"
                "    assert sys.argv[sys.argv.index('--config-file')+1] == os.environ['CODEX_WINDOWS_NEXTEST_CONFIG_FILE']\n"
                "    sys.exit(int(os.environ['FIXTURE_EXIT']))\n"
            )
            binary.chmod(0o755)
            before = helper.read_bytes()
            for status in (0, 7):
                with self.subTest(exit_status=status):
                    evidence = root / str(status)
                    result = subprocess.run(
                        [sys.executable, "-c", harness, str(helper), str(evidence)],
                        env={
                            **os.environ,
                            "PATH": str(root) + os.pathsep + os.environ["PATH"],
                            "CODEX_WINDOWS_NEXTEST_CONFIG_FILE": str(config),
                            "FIXTURE_EXIT": str(status),
                        },
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(result.returncode == 0, status == 0, result.stderr)
                    receipt = json.loads(
                        (
                            evidence / "checks/windows-product-regressions.json"
                        ).read_text()
                    )
                    self.assertEqual(receipt["exit_code"], status)
                    self.assertEqual(receipt["command"].count("--config-file"), 1)
                    self.assertEqual(
                        receipt["test_config_sha256"],
                        hashlib.sha256(config.read_bytes()).hexdigest(),
                    )
                    observed = json.loads(
                        (
                            evidence
                            / "windows-handoff/logs/windows-product-regressions.log"
                        ).read_text()
                    )
                    self.assertEqual(observed, receipt["command"][1:])
                    self.assertEqual(helper.read_bytes(), before)

    @unittest.skipUnless(
        os.name == "posix" and shutil.which("bash"), "requires Unix Bash"
    )
    def test_unix_acceptance_preserves_source_and_passes_external_config(self) -> None:
        root = Path(__file__).resolve().parents[1]
        source = (root / ".github/scripts/downstream-unix-native.sh").read_text()
        start = source.index("  nextest_config=")
        recipe = source[
            start : source.index('  if [[ "$TARGET" == *-linux-musl ]]', start)
        ]
        run_check = source[
            source.index("run_check() {") : source.index("write_pair_receipt()")
        ]
        harness = """set -euo pipefail
just() { "$FIXTURE_PYTHON" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@"; }
"""
        with tempfile.TemporaryDirectory(prefix="Unix release fixture ") as directory:
            scratch = Path(directory)
            (scratch / ".config").mkdir()
            original = (root / "codex-rs/.config/nextest.toml").read_bytes()
            tracked = scratch / ".config/nextest.toml"
            tracked.write_bytes(original)
            result = subprocess.run(
                ["bash", "-c", harness + run_check + recipe],
                cwd=scratch,
                env={
                    **os.environ,
                    "RUNNER_TEMP": str(scratch),
                    "TARGET": "x86_64-apple-darwin",
                    "dist": str(scratch / "dist"),
                    "FIXTURE_PYTHON": sys.executable,
                },
                capture_output=True,
                text=True,
                check=True,
            )
            args = json.loads(result.stdout)
            self.assertEqual(args.count("--config-file"), 1)
            config = Path(args[args.index("--config-file") + 1])
            self.assertNotEqual(config, tracked)
            self.assertEqual(tracked.read_bytes(), original)
            self.assertEqual(
                tomllib.loads(config.read_text())["profile"]["default"]["overrides"][
                    -3:
                ],
                [
                    {
                        "filter": "package(codex-usage) & test(=repeated_switches_aggregate_by_job_before_report_limits)",
                        "slow-timeout": {"period": "2m", "terminate-after": 2},
                    },
                    {
                        "filter": "package(codex-usage) & test(=review_retention::tests::work_tests::job_totals_survive_hourly_lifetime_restart_and_resumed_work)",
                        "slow-timeout": {"period": "3m", "terminate-after": 2},
                    },
                    {
                        "filter": "package(codex-usage) & test(=prepared_window_matches_raw_summary_across_corrections_and_scopes)",
                        "slow-timeout": {"period": "3m", "terminate-after": 2},
                    },
                ],
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
            native_script = ".github/scripts/downstream-unix-native.sh"
            native_recipe = (
                "cat >> \"$nextest_config\" <<'EOF'\n\n"
                "[[profile.default.overrides]]\n"
                "filter = 'package(codex-usage) & test(=repeated_switches_aggregate_by_job_before_report_limits)'\n"
                'slow-timeout = { period = "2m", terminate-after = 2 }\n'
                "EOF\n"
                "run_check product-regressions just test --locked -p codex-usage\n"
                "run_check product-build cargo build --locked --release\n"
            )
            inputs = {
                "codex-rs/product.rs": "product source\n",
                "third_party/v8/source.bzl": "V8 source\n",
                "MODULE.bazel": "toolchain source\n",
                ".github/scripts/rusty_v8_bazel.py": "V8 helper\n",
                ".github/scripts/prepare_downstream_musl.py": "musl helper\n",
                native_script: native_recipe,
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
                relationship = verify_hosted_producer_source(producer, packaging)
                self.assertEqual(
                    relationship["product_source"]["source_commit"], producer
                )
                self.assertEqual(
                    relationship["hosted_producer"]["source_commit"], packaging
                )
                invalid_product = commit(
                    "codex-rs/product.rs", inputs["codex-rs/product.rs"] + "changed\n"
                )
                with self.assertRaisesRegex(ValueError, "Native reuse changes"):
                    verify_hosted_producer_source(producer, invalid_product)
                packaging = commit("codex-rs/product.rs", inputs["codex-rs/product.rs"])
                timeout_additions = (
                    "\n[[profile.default.overrides]]\n"
                    "filter = 'package(codex-usage) & test(=review_retention::tests::work_tests::job_totals_survive_hourly_lifetime_restart_and_resumed_work)'\n"
                    'slow-timeout = { period = "3m", terminate-after = 2 }\n'
                    "\n[[profile.default.overrides]]\n"
                    "filter = 'package(codex-usage) & test(=prepared_window_matches_raw_summary_across_corrections_and_scopes)'\n"
                    'slow-timeout = { period = "3m", terminate-after = 2 }\n'
                )
                extended_recipe = native_recipe.replace(
                    "EOF\n", timeout_additions + "EOF\n"
                )
                extended = commit(native_script, extended_recipe)
                verify_hosted_native_source(producer, extended)
                for before, after in (
                    ('period = "3m"', 'period = "4m"'),
                    ("terminate-after = 2", "terminate-after = 3"),
                    (
                        "test(=prepared_window_matches_raw_summary_across_corrections_and_scopes)",
                        "test(=another_test)",
                    ),
                    ("--locked", "--offline"),
                    ("--release", ""),
                    (
                        "run_check product-regressions just test --locked -p codex-usage\n",
                        "",
                    ),
                    ("run_check product-build", "echo skipped-product-build"),
                ):
                    with self.subTest(recipe_change=(before, after)):
                        changed = commit(
                            native_script, extended_recipe.replace(before, after)
                        )
                        with self.assertRaisesRegex(
                            ValueError, "Hosted native recipe changed"
                        ):
                            verify_hosted_native_source(producer, changed)
                for invalid_recipe in (
                    extended_recipe + "\n",
                    "\n" + extended_recipe,
                    extended_recipe.replace(
                        timeout_additions,
                        timeout_additions.rsplit("\n[[profile.default.overrides]]", 1)[
                            0
                        ],
                    ),
                    native_recipe + timeout_additions,
                    extended_recipe.replace("EOF\n", timeout_additions + "EOF\n"),
                ):
                    invalid = commit(native_script, invalid_recipe)
                    with self.assertRaisesRegex(
                        ValueError, "Hosted native recipe changed"
                    ):
                        verify_hosted_native_source(producer, invalid)
                removed = commit(native_script, native_recipe)
                with self.assertRaisesRegex(ValueError, "Hosted native recipe changed"):
                    verify_hosted_native_source(extended, removed)
                git("update-index", "--chmod=+x", native_script)
                git(
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@localhost",
                    "commit",
                    "-qm",
                    "mode changed",
                )
                with self.assertRaisesRegex(ValueError, "recipe file identity changed"):
                    verify_hosted_native_source(producer, git("rev-parse", "HEAD"))
                git("update-index", "--chmod=-x", native_script)
                git(
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@localhost",
                    "commit",
                    "-qm",
                    "restore mode",
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
        workflow = (
            Path(__file__).resolve().parents[1]
            / ".github/workflows/downstream-candidate.yml"
        ).read_text()
        native_job = (
            release_identity._extract_yaml_job(workflow, "native")
            .replace(" && !inputs.reuse_linux_arm_handoff", "")
            .replace(" && inputs.hosted_native_runs == ''", "")
        )
        mac_job = release_identity._extract_yaml_job(workflow, "native-mac")
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
            (root / ".github/scripts/bootstrap_bazel_distdir.py").write_text(
                "bootstrap fixture\n"
            )
            (root / ".github/workflows").mkdir(parents=True, exist_ok=True)
            (root / ".github/workflows/downstream-unix-native-target.yml").write_text(
                "name: target\njobs:\n  v8:\n    name: V8 source pair\n    runs-on: ubuntu\n    steps:\n      - run: echo source\n  package:\n    name: Package native target\n"
            )
            (root / ".github/workflows/downstream-candidate.yml").write_text(
                "env:\n  BUILD_MODE: release\njobs:\n"
                + native_job
                + "  npm:\n    name: npm\n"
            )
            producer = commit("producer")
            qualification = root / "scripts/qualify_local_delivery.py"
            qualification.write_text("post-activation evidence verification\n")
            qualification_commit = commit("local delivery evidence fix")
            with patch("release_identity.ROOT", root):
                result = verify_native_source(producer, qualification_commit)
                self.assertEqual(
                    result["packaging_only_paths"],
                    ["scripts/qualify_local_delivery.py"],
                )
                verify_hosted_v8_source(producer, qualification_commit)
            candidate_workflow = root / ".github/workflows/downstream-candidate.yml"
            candidate_workflow.write_text(
                candidate_workflow.read_text()
                .replace(
                    "if: inputs.hosted_native_run_id == ''",
                    "if: inputs.hosted_native_run_id == '' && !inputs.reuse_linux_arm_handoff",
                )
                .replace("  npm:\n", mac_job + "  npm:\n")
            )
            handoff_matrix = commit("allow verified Linux ARM handoff")
            with patch("release_identity.ROOT", root):
                verify_hosted_v8_source(producer, handoff_matrix)
                verify_hosted_native_source(
                    producer, handoff_matrix, target="aarch64-unknown-linux-musl"
                )
                with self.assertRaisesRegex(ValueError, "Hosted native job"):
                    verify_hosted_native_source(producer, handoff_matrix)
            handoff_workflow = candidate_workflow.read_text()
            candidate_workflow.write_text(
                handoff_workflow.replace(
                    "if: inputs.hosted_native_run_id == ''",
                    "if: inputs.hosted_native_run_id == '' && inputs.hosted_native_runs == ''",
                )
            )
            mapped_reuse = commit("allow exact per-target native reuse routing")
            with patch("release_identity.ROOT", root):
                verify_hosted_native_source(
                    producer, mapped_reuse, target="x86_64-apple-darwin"
                )
                verify_hosted_native_source(handoff_matrix, mapped_reuse)
            for original, changed in (
                ("macos-15-intel", "macos-14"),
                ("platform: macos_arm64", "platform: linux_arm64_musl"),
                ("bazel_config: ci-macos", "bazel_config: ci"),
                (
                    "source_version: ${{ needs.identity.outputs.source_version }}",
                    "source_version: changed",
                ),
                (
                    "./.github/workflows/downstream-unix-native-target.yml",
                    "./.github/workflows/other.yml",
                ),
                ("if: inputs.reuse_linux_arm_handoff", "if: true"),
            ):
                with self.subTest(mac_input=original):
                    candidate_workflow.write_text(
                        handoff_workflow.replace(
                            mac_job, mac_job.replace(original, changed)
                        )
                    )
                    invalid_mac = commit("reject changed Mac producer input")
                    with patch("release_identity.ROOT", root):
                        with self.assertRaisesRegex(
                            ValueError, "Hosted native Mac matrix"
                        ):
                            verify_hosted_v8_source(producer, invalid_mac)
            candidate_workflow.write_text(handoff_workflow.replace(mac_job, ""))
            missing_mac = commit("reject missing Mac producer alternative")
            with patch("release_identity.ROOT", root):
                with self.assertRaisesRegex(ValueError, "Hosted native matrix"):
                    verify_hosted_v8_source(producer, missing_mac)
            candidate_workflow.write_text(
                handoff_workflow.replace(
                    "if: inputs.hosted_native_run_id == '' && !inputs.reuse_linux_arm_handoff",
                    "if: inputs.hosted_native_run_id == '' && inputs.reuse_linux_arm_handoff",
                )
            )
            invalid_handoff_matrix = commit("reject broad native matrix change")
            with patch("release_identity.ROOT", root):
                with self.assertRaisesRegex(ValueError, "Hosted native matrix"):
                    verify_hosted_v8_source(producer, invalid_handoff_matrix)
            candidate_workflow.write_text(
                candidate_workflow.read_text()
                .replace(
                    "if: inputs.hosted_native_run_id == '' && inputs.reuse_linux_arm_handoff",
                    "if: inputs.hosted_native_run_id == ''",
                )
                .replace(mac_job, "")
            )
            usage = root / "codex-rs/usage/src/runtime.rs"
            usage.parent.mkdir(parents=True, exist_ok=True)
            usage.write_text("usage fix\n")
            packaging = commit("usage fix")

            with patch("release_identity.ROOT", root):
                relationship = verify_hosted_producer_source(
                    packaging, producer, allow_v8_reuse=True
                )
                self.assertEqual(
                    relationship["product_source"]["source_commit"], packaging
                )
                self.assertEqual(
                    relationship["hosted_producer"]["source_commit"], producer
                )
                with self.assertRaisesRegex(ValueError, "Native reuse changes"):
                    verify_hosted_producer_source(producer, packaging)
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
                (root / "codex-rs/core/src/compact.rs").write_text(
                    "required compaction account continuation\n"
                )
                usage_api = (
                    root / "codex-rs/app-server-protocol/src/protocol/v2/local_usage.rs"
                )
                usage_api.parent.mkdir(parents=True, exist_ok=True)
                usage_api.write_text("bounded usage RPC contract\n")
                packaging = commit("managed account failover fix")
                result = verify_hosted_v8_source(producer, packaging)
                self.assertEqual(result["v8_inputs"]["schema"], 1)
                self.assertIn(
                    str(account_routing.relative_to(root)),
                    result["packaging_only_paths"],
                )

                for path in (
                    "scripts/run_isolated_workspace_tests.sh",
                    "scripts/test_run_isolated_workspace_tests.py",
                    "codex-rs/core/tests/suite/hooks.rs",
                    "codex-rs/tui/src/pets/mod.rs",
                ):
                    before_harness = packaging
                    harness = root / path
                    harness.parent.mkdir(parents=True, exist_ok=True)
                    harness.write_text("owned test scratch cleanup\n")
                    packaging = commit("test fixture changes")
                    result = verify_hosted_v8_source(producer, packaging)
                    self.assertIn(path, result["packaging_only_paths"])
                    with self.assertRaisesRegex(ValueError, "Native reuse changes"):
                        verify_native_source(before_harness, packaging)

                unrelated_app_server = root / "codex-rs/app-server/src/lib.rs"
                unrelated_app_server.write_text("unrelated app-server change\n")
                changed_app_server = commit("unapproved app-server change")
                with self.assertRaisesRegex(
                    ValueError, "unapproved product/build inputs"
                ):
                    verify_hosted_v8_source(producer, changed_app_server)
                with self.assertRaisesRegex(ValueError, "V8 reuse changes"):
                    verify_hosted_producer_source(
                        changed_app_server, producer, allow_v8_reuse=True
                    )
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
                with self.assertRaisesRegex(
                    ValueError, "(?:Hosted V8|V8 reuse) (?:product source|changes)"
                ):
                    verify_hosted_producer_source(
                        changed_inputs, producer, allow_v8_reuse=True
                    )
                git("checkout", "-q", "-b", "divergent-product", producer)
                divergent_usage = root / "codex-rs/usage/src/runtime.rs"
                divergent_usage.parent.mkdir(parents=True, exist_ok=True)
                divergent_usage.write_text("divergent product fix\n")
                divergent_product = commit("divergent product")
                git("checkout", "-q", "-b", "divergent-producer", producer)
                divergent_packaging = root / "scripts/qualify_local_delivery.py"
                divergent_packaging.parent.mkdir(parents=True, exist_ok=True)
                divergent_packaging.write_text("divergent producer helper\n")
                divergent_producer = commit("divergent producer")
                with self.assertRaisesRegex(ValueError, "no valid ancestry"):
                    verify_hosted_producer_source(
                        divergent_product,
                        divergent_producer,
                        allow_v8_reuse=True,
                    )

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

    @unittest.skipUnless(
        os.name == "posix" and shutil.which("bash"), "requires Unix Bash"
    )
    def test_candidate_arm_handoff_modes_and_target_selection(self) -> None:
        source = (
            Path(__file__).resolve().parents[1]
            / ".github/workflows/downstream-candidate.yml"
        ).read_text()
        mode_step = source.split("      - name: Check native producer modes\n", 1)[1]
        mode_command = "\n".join(
            line[10:]
            for line in mode_step.split("        run: |\n", 1)[1]
            .split("      - name: Check the completed local proof", 1)[0]
            .splitlines()
        )
        for native_only, hosted_native, reuse_arm, mapped_runs, v8_run, accepted in (
            ("false", "", "false", "", "", True),
            ("true", "", "false", "", "", True),
            ("false", "123", "false", "", "", True),
            ("false", "", "true", "", "", True),
            ("true", "", "true", "", "", False),
            ("false", "123", "true", "", "", False),
            ("false", "", "false", "{}", "", True),
            ("true", "", "false", "{}", "", False),
            ("false", "123", "false", "{}", "", False),
            ("false", "", "false", "{}", "456", False),
            ("false", "", "true", "{}", "", False),
        ):
            with self.subTest(
                native_only=native_only,
                hosted_native=hosted_native,
                reuse_arm=reuse_arm,
            ):
                result = subprocess.run(
                    ["bash", "-c", mode_command],
                    capture_output=True,
                    text=True,
                    env={
                        **os.environ,
                        "NATIVE_ONLY": native_only,
                        "HOSTED_NATIVE_RUN_ID": hosted_native,
                        "HOSTED_NATIVE_RUNS": mapped_runs,
                        "HOSTED_V8_RUN_ID": v8_run,
                        "REUSE_LINUX_ARM": reuse_arm,
                    },
                )
                self.assertEqual(result.returncode == 0, accepted, result.stderr)

        final_command = "\n".join(
            line[10:]
            for line in source.split("\n  candidate-result:", 1)[1]
            .split("        run: |\n", 1)[1]
            .splitlines()
        )
        for failed in (None, "identity", "handoff", "npm", "native"):
            results = {
                name: {
                    "result": "success"
                    if name in ("identity", "handoff", "npm")
                    else "skipped"
                }
                for name in ("identity", "handoff", "npm", "native", "native-mac")
            }
            if failed:
                results[failed]["result"] = "failure"
            result = subprocess.run(
                ["bash", "-c", final_command],
                capture_output=True,
                text=True,
                env={
                    **os.environ,
                    "NEEDS_JSON": json.dumps(results),
                    "HOSTED_RUN_ID": "",
                    "HOSTED_NATIVE_RUNS": "{}",
                    "REUSE_LINUX_ARM": "false",
                },
            )
            self.assertEqual(result.returncode == 0, failed is None, result.stderr)

        handoff_step = source.split(
            "      - name: Import authenticated and digested native bundles\n", 1
        )[1]
        handoff_command = "\n".join(
            line[10:]
            for line in handoff_step.split("        run: |\n", 1)[1]
            .split("      - uses:", 1)[0]
            .splitlines()
        )
        harness = """python3() {
  "$FIXTURE_PYTHON" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@"
}
"""
        commit = "a" * 40
        for reuse_arm, targets in (
            ("false", "x86_64-unknown-linux-musl"),
            ("true", "x86_64-unknown-linux-musl,aarch64-unknown-linux-musl"),
        ):
            with self.subTest(reuse_arm=reuse_arm):
                result = subprocess.run(
                    ["bash", "-c", harness + handoff_command],
                    check=True,
                    capture_output=True,
                    text=True,
                    env={
                        **os.environ,
                        "FIXTURE_PYTHON": sys.executable,
                        "LINUX_HANDOFF": "linux-fixture",
                        "WINDOWS_HANDOFF": "windows-fixture",
                        "GITHUB_SHA": commit,
                        "REUSE_LINUX_ARM": reuse_arm,
                    },
                )
                self.assertEqual(
                    [json.loads(line) for line in result.stdout.splitlines()],
                    [
                        [
                            "scripts/runner_handoff.py",
                            "download",
                            "--manifest",
                            manifest,
                            "--commit",
                            commit,
                            "--producer",
                            producer,
                            "--targets",
                            required,
                            "--destination",
                            "native-artifacts",
                        ]
                        for manifest, producer, required in (
                            ("linux-fixture", "linux-local", targets),
                            (
                                "windows-fixture",
                                "windows-private-controller",
                                "x86_64-pc-windows-msvc,aarch64-pc-windows-msvc",
                            ),
                        )
                    ],
                )

    @unittest.skipUnless(
        os.name == "posix" and shutil.which("bash"), "requires Unix Bash"
    )
    def test_native_only_v8_reuse_runs_both_producer_checks(self) -> None:
        source = (
            Path(__file__).resolve().parents[1]
            / ".github/workflows/downstream-candidate.yml"
        ).read_text()
        self.assertEqual(source.count('--product-source "$NATIVE_SOURCE_COMMIT"'), 2)
        self.assertEqual(
            source.count("NATIVE_SOURCE_COMMIT: ${{ inputs.native_source_commit }}"),
            4,
        )
        step = source.split(
            "      - name: Verify the optional hosted V8 producer\n", 1
        )[1]
        command = "\n".join(
            line[10:]
            for line in step.split("        run: |\n", 1)[1]
            .split("\n  handoff:", 1)[0]
            .splitlines()
        )
        harness = """gh() { printf '{"head_sha":"%s"}\\n' "$FIXTURE_PRODUCER"; }
jq() { printf '%s\\n' "$FIXTURE_PRODUCER"; }
python3() {
  printf '%s\\n' "$*" >> "$FIXTURE_CHECKS"
  [[ "$1" != "$FIXTURE_REJECT" ]]
}
"""
        producer = "a" * 40
        product_source = "c" * 40
        packaging = "b" * 40
        for mode, run_id, native_id, reject, accepted in (
            ("true", "123", "", "", True),
            ("false", "123", "", "", True),
            ("true", "invalid", "", "", False),
            ("true", "123", "456", "", False),
            ("true", "123", "", "scripts/release_identity.py", False),
            ("true", "123", "", "codex-cli/scripts/verify_candidate_run.py", False),
        ):
            with self.subTest(
                mode=mode, run_id=run_id, native_id=native_id, reject=reject
            ):
                with tempfile.TemporaryDirectory(prefix="npm-v8-reuse-") as directory:
                    root = Path(directory)
                    checks = root / "checks"
                    output = root / "output"
                    result = subprocess.run(
                        ["bash", "-c", harness + command],
                        capture_output=True,
                        text=True,
                        env={
                            **os.environ,
                            "NATIVE_ONLY": mode,
                            "HOSTED_RUN_ID": run_id,
                            "HOSTED_NATIVE_RUN_ID": native_id,
                            "GITHUB_REPOSITORY": "holyglory/codex",
                            "GITHUB_SHA": packaging,
                            "RUNNER_TEMP": directory,
                            "GITHUB_OUTPUT": str(output),
                            "FIXTURE_PRODUCER": producer,
                            "NATIVE_SOURCE_COMMIT": product_source,
                            "FIXTURE_CHECKS": str(checks),
                            "FIXTURE_REJECT": reject,
                        },
                    )
                    self.assertEqual(result.returncode == 0, accepted, result.stderr)
                    self.assertEqual(output.exists(), accepted)
                    if accepted:
                        self.assertEqual(
                            output.read_text(), f"source_commit={producer}\n"
                        )
                        self.assertEqual(
                            checks.read_text().splitlines(),
                            [
                                f"scripts/release_identity.py --producer {producer} --product-source {product_source} --packaging {packaging} --hosted-v8",
                                f"codex-cli/scripts/verify_candidate_run.py --repository holyglory/codex --commit {producer} --jobs-file {root / 'hosted-v8-jobs.json'} --hosted-v8",
                            ],
                        )

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
        reused_jobs = [dict(job) for job in jobs]
        reuse_check = {
            "name": "Verify the optional hosted V8 producer",
            "status": "completed",
            "conclusion": "success",
        }
        reused_jobs[0]["steps"] = [reuse_check]
        for job in reused_jobs:
            if job["name"] in candidate.HOSTED_V8_JOBS:
                job["conclusion"] = "skipped"
        candidate.verify_candidate_run(
            run, repository, commit, reused_jobs, hosted_native=True
        )
        with self.assertRaisesRegex(ValueError, "Required candidate job"):
            candidate.verify_candidate_run(run, repository, commit, reused_jobs)
        for conclusion in ("skipped", "failure"):
            with self.subTest(reuse_check=conclusion):
                reused_jobs[0]["steps"] = [{**reuse_check, "conclusion": conclusion}]
                with self.assertRaisesRegex(ValueError, "Required candidate job"):
                    candidate.verify_candidate_run(
                        run, repository, commit, reused_jobs, hosted_native=True
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
