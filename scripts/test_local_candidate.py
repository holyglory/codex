import copy
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

import local_candidate as candidate
import cargo_native_inputs as native
import check_release_inclusion as inclusion


class LocalCandidateTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="candidate-test-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.commit = "a" * 40
        self.plan = {
            name: (
                [sys.executable, "-c", "print('fixture check passed')"],
                self.directory,
            )
            for name in candidate.CHECKS
        }

    def execute(self, **kwargs):
        with redirect_stdout(io.StringIO()):
            return candidate.execute(
                self.plan,
                dict(os.environ),
                self.directory,
                kwargs.pop("unchanged", lambda: True),
                **kwargs,
            )

    def receipt(self):
        return {
            "schema": 1,
            "commit": self.commit,
            "platform": "linux-x86_64",
            "status": "success",
            "checks": self.execute(),
        }

    def test_real_subprocesses_produce_complete_hash_bound_receipt(self):
        receipt = self.receipt()
        candidate.validate(receipt, self.commit)
        for check in receipt["checks"]:
            log = self.directory / f"{check['name']}.log"
            self.assertEqual(log.read_text(), "fixture check passed\n")
            self.assertEqual(check["log_sha256"], candidate.digest(log))

    def test_isolation_requires_host_configuration_to_be_hidden(self):
        with (
            patch("local_candidate.frozen_commit", return_value=self.commit),
            patch("local_candidate.platform.system", return_value="Linux"),
            patch("local_candidate.platform.machine", return_value="x86_64"),
            patch.dict(os.environ, {"CODEX_RELEASE_ISOLATED": "1"}),
            patch("local_candidate.shutil.which", return_value=None),
        ):
            for present in (True, False):
                with (
                    self.subTest(host_configuration_present=present),
                    patch.object(Path, "exists", return_value=present),
                    self.assertRaisesRegex(
                        ValueError,
                        "host Codex configuration"
                        if present
                        else "Required build tool",
                    ),
                ):
                    candidate.check(
                        self.directory, self.directory, self.directory, self.directory
                    )

    def test_runner_keeps_nonroot_identity_and_private_mounts(self):
        if os.environ.get("CODEX_BAZEL_BATCH"):
            self.skipTest("the acceptance runner is already inside its isolated namespace")
        if sys.platform != "linux" or shutil.which("bwrap") is None:
            self.skipTest("the local Linux runner requires bubblewrap")
        bulk_root = Path("/mnt/build-storage/codex")
        if not bulk_root.is_dir() or not os.access(bulk_root, os.W_OK):
            self.skipTest("the local runner bulk volume is unavailable")
        runner = Path(__file__).with_name("run_local_candidate.sh")
        probe = (
            "import json, os, pathlib, pwd; "
            "home = pathlib.Path(pwd.getpwuid(os.getuid()).pw_dir); "
            "socket = pathlib.Path('/tmp') / f'codex-daemon-{os.getuid()}'; "
            "assert os.getuid() != 0; "
            "assert pathlib.Path(os.environ['HOME']) == home; "
            "assert all(name not in os.environ for name in ('OPENAI_API_KEY', 'CODEX_API_KEY', 'CODEX_ACCESS_TOKEN')); "
            "assert not pathlib.Path('/etc/codex/config.toml').exists(); "
            "assert socket.stat().st_mode & 0o777 == 0o700; "
            "(socket / 'probe').write_text('namespace-only'); "
            "print(json.dumps({'uid': os.getuid(), 'gid': os.getgid(), 'home': str(home)}))"
        )
        with tempfile.TemporaryDirectory(prefix="runner-test-", dir=bulk_root) as state:
            environment = dict(
                os.environ,
                OPENAI_API_KEY="fixture-secret",
                CODEX_API_KEY="fixture-secret",
                CODEX_ACCESS_TOKEN="fixture-secret",
            )
            result = subprocess.run(
                [
                    "bash",
                    str(runner),
                    state,
                    "--command",
                    sys.executable,
                    "-c",
                    probe,
                ],
                cwd=runner.parents[1],
                env=environment,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(result.stdout)
            self.assertEqual(payload["uid"], os.getuid())
            self.assertEqual(payload["gid"], os.getgid())
            self.assertFalse(
                (
                    Path(state)
                    / "test-environment"
                    / "tmp"
                    / f"codex-daemon-{os.getuid()}"
                    / "probe"
                ).exists()
            )

    def test_inclusion_rejects_a_lost_feature_or_changed_migration(self):
        root = self.directory
        manifest_path = root / "codex-rs/Cargo.toml"
        manifest_path.parent.mkdir()
        manifest_path.write_text('[workspace.package]\nversion = "0.162.1+multi.1"\n')
        source = root / "feature.rs"
        source.write_text("fn retained_feature() {}\n")
        migration = root / "migration.sql"
        migration.write_text("CREATE TABLE preserved (id INTEGER);\n")
        for args in (
            ["init", "-q"],
            ["add", "."],
            [
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ):
            subprocess.run(
                ["git", "-C", str(root), *args], check=True, capture_output=True
            )
        commit = subprocess.check_output(
            ["git", "-C", str(root), "rev-parse", "HEAD"], text=True
        ).strip()
        for tag in ("fixture-upstream", "fixture-rollback"):
            subprocess.run(["git", "-C", str(root), "tag", tag], check=True)
        manifest = {
            "schema": 1,
            "upstream": {"tag": "fixture-upstream", "commit": commit},
            "rollback": {"tag": "fixture-rollback", "commit": commit},
            "rust_version": "0.162.1+multi.1",
            "npm_version": "0.162.1-multi.1",
            "capabilities": [
                {
                    "id": "retained",
                    "required_files": ["feature.rs"],
                    "filters": ["test(retained)"],
                }
            ],
            "migrations": {"migration.sql": candidate.digest(migration)},
        }
        self.assertEqual(inclusion.verify(root, manifest)["capabilities"], ["retained"])
        source.unlink()
        with self.assertRaisesRegex(ValueError, "retained: missing feature.rs"):
            inclusion.verify(root, manifest)
        source.write_text("fn retained_feature() {}\n")
        migration.write_text("CREATE TABLE overwritten (id INTEGER);\n")
        with self.assertRaisesRegex(ValueError, "migration missing or changed"):
            inclusion.verify(root, manifest)

    def test_each_gate_failure_prevents_all_expensive_lanes(self):
        for failed in candidate.GATES:
            with self.subTest(failed=failed):
                invoked = []

                def runner(name, *_args):
                    invoked.append(name)
                    return {"name": name, "exit_code": 1 if name == failed else 0}

                self.execute(runner=runner)
                self.assertEqual(
                    invoked, list(candidate.GATES[: candidate.GATES.index(failed) + 1])
                )

    def test_dirty_source_after_gate_stops_even_when_command_succeeds(self):
        checks = self.execute(unchanged=lambda: False)
        self.assertEqual([check["name"] for check in checks], [candidate.GATES[0]])

    def test_failed_rust_lane_preserves_later_bazel_findings(self):
        invoked = []

        def runner(name, *_args):
            invoked.append(name)
            return {"name": name, "exit_code": int(name == "clippy")}

        self.execute(runner=runner)
        self.assertIn("bazel-release", invoked)
        self.assertNotIn("rust-tests", invoked)

    def test_full_engines_do_not_overlap_on_the_same_host(self):
        bazel_entered = threading.Event()

        def runner(name, *_args):
            if name == "clippy":
                self.assertFalse(bazel_entered.wait(timeout=0.1))
            if name == "bazel-tests":
                bazel_entered.set()
            return {"name": name, "exit_code": 0}

        self.execute(runner=runner)
        self.assertTrue(bazel_entered.is_set())

    def test_failed_and_unstartable_commands_are_not_success(self):
        with redirect_stdout(io.StringIO()):
            for name, command, expected in (
                ("failed", [sys.executable, "-c", "raise SystemExit(4)"], 4),
                ("missing", [str(self.directory / "missing")], 127),
            ):
                result = candidate.run_step(
                    name, command, self.directory, os.environ, self.directory
                )
                self.assertEqual(result["exit_code"], expected)

    def test_missing_tool_fails_before_compilation_or_build_storage_writes(self):
        with (
            patch.object(candidate, "frozen_commit", return_value=self.commit),
            patch.object(candidate.Path, "exists", return_value=False),
            patch.object(candidate.shutil, "which", return_value=None),
            patch.object(candidate, "execute") as execute,
            self.assertRaisesRegex(ValueError, "Required build tool is unavailable"),
        ):
            candidate.check(
                self.directory,
                self.directory / "state",
                self.directory / "cargo",
                self.directory / "release",
            )
        execute.assert_not_called()
        self.assertFalse((self.directory / "state").exists())

    def test_native_pkg_config_keeps_voice_probes_separate_from_platform_inputs(self):
        executable = self.directory / "pkg-config"
        executable.write_text(
            f"#!{sys.executable}\n"
            "import json,os,sys\n"
            "print(json.dumps([sys.argv[1:],os.environ.get('PKG_CONFIG_LIBDIR'),os.environ.get('PKG_CONFIG_PATH')]))\n"
        )
        executable.chmod(0o755)
        environment = dict(
            os.environ,
            CODEX_CARGO_SYSTEM_PKG_CONFIG=str(executable),
            CODEX_CARGO_VOICE_SDK=str(self.directory / "sdk"),
            PKG_CONFIG_LIBDIR="platform-metadata",
            PKG_CONFIG_PATH="host-metadata",
        )
        wrapper = Path(native.__file__).with_name("cargo_native_pkg_config.py")
        for module in ("glib-2.0", "gstreamer-1.0", "gstreamer-app-1.0"):
            result = subprocess.check_output(
                [sys.executable, str(wrapper), "--modversion", module],
                env=environment,
                text=True,
            )
            self.assertEqual(
                json.loads(result),
                [
                    ["--define-prefix", "--modversion", module],
                    str(self.directory / "sdk/lib/pkgconfig"),
                    "",
                ],
            )
        for module in ("alsa", "libcap", "glibc"):
            result = subprocess.check_output(
                [sys.executable, str(wrapper), "--modversion", module],
                env=environment,
                text=True,
            )
            self.assertEqual(
                json.loads(result),
                [["--modversion", module], "platform-metadata", "host-metadata"],
            )

    def test_native_cargo_rejects_inputs_from_another_candidate(self):
        (self.directory / "cargo-native-env.json").write_text(
            json.dumps({"STABLE_GIT_COMMIT": "b" * 40})
        )
        with (
            patch.object(native, "commit", return_value=self.commit),
            patch.object(native.os, "execvpe") as execute,
            self.assertRaisesRegex(ValueError, "another candidate"),
        ):
            native.run(self.directory, ["--", "cargo", "clippy"])
        execute.assert_not_called()

    def test_receipt_rejects_missing_duplicate_failed_or_wrong_source_evidence(self):
        receipt = self.receipt()
        mutations = [
            lambda r: r.update(schema=True),
            lambda r: r.update(commit="b" * 40),
            lambda r: r.update(status="failure"),
            lambda r: r.update(platform="darwin-arm64"),
            lambda r: r["checks"].pop(),
            lambda r: r["checks"].append(r["checks"][0]),
            lambda r: r["checks"][1].update(name=r["checks"][0]["name"]),
            lambda r: r["checks"][0].update(exit_code=False),
            lambda r: r["checks"][0].update(exit_code=1),
            lambda r: r["checks"][0].update(log_sha256="not-a-digest"),
            lambda r: r["checks"].__setitem__(0, None),
        ]
        for mutate in mutations:
            changed = copy.deepcopy(receipt)
            mutate(changed)
            with self.subTest(receipt=changed), self.assertRaises(ValueError):
                candidate.validate(changed, self.commit)

    def test_dispatch_requires_matching_logs_remote_and_no_active_run(self):
        receipt = self.receipt()
        path = self.directory / "receipt.json"
        path.write_text(json.dumps(receipt))
        for remote, runs, accepted in (
            ("b" * 40, [], False),
            (self.commit, [{"status": "queued"}], False),
            (self.commit, [{"status": "in_progress"}], False),
            (self.commit, [{"status": "completed"}], True),
        ):
            with (
                self.subTest(remote=remote, runs=runs),
                patch.object(candidate, "frozen_commit", return_value=self.commit),
                patch.object(
                    candidate.subprocess,
                    "check_output",
                    side_effect=[remote, json.dumps(runs)],
                ),
                patch.object(candidate.subprocess, "run") as dispatch,
            ):
                if accepted:
                    candidate.dispatch(self.directory, path, "codex/fixture")
                    payload = json.loads(dispatch.call_args.kwargs["input"])
                    self.assertEqual(json.loads(payload["local_acceptance"]), receipt)
                else:
                    with self.assertRaises(ValueError):
                        candidate.dispatch(self.directory, path, "codex/fixture")
                    dispatch.assert_not_called()
        log = self.directory / "helpers.log"
        for missing in (False, True):
            if missing:
                log.unlink()
            else:
                log.write_text("changed after acceptance")
            with (
                patch.object(candidate, "frozen_commit", return_value=self.commit),
                patch.object(candidate.subprocess, "check_output") as remote,
                self.assertRaises((ValueError, OSError)),
            ):
                candidate.dispatch(self.directory, path, "codex/fixture")
            remote.assert_not_called()

    def test_real_checkout_rejects_dirty_and_untracked_source(self):
        def git(*args):
            return subprocess.check_output(
                ["git", "-C", str(self.directory), *args],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()

        git("init")
        git(
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        )
        self.assertEqual(
            candidate.frozen_commit(self.directory), git("rev-parse", "HEAD")
        )
        (self.directory / "pending").write_text("unfinished work")
        with self.assertRaises(ValueError):
            candidate.frozen_commit(self.directory)
        git("add", "pending")
        with self.assertRaises(ValueError):
            candidate.frozen_commit(self.directory)

    def test_preserve_generated_test_artifacts_keeps_source_clean(self):
        generated = self.directory / "codex-rs/core/tests/suite/example.snap.new"
        generated.parent.mkdir(parents=True)
        generated.write_text("snapshot candidate")
        lock = self.directory / "codex-rs/non-existent/.auth-credential-lock-v1"
        lock.parent.mkdir(parents=True)
        lock.write_text("fixture lock")
        with patch.object(
            candidate,
            "git",
            return_value=(
                "?? codex-rs/core/tests/suite/example.snap.new\0"
                "?? codex-rs/non-existent/.auth-credential-lock-v1\0"
            ),
        ):
            candidate.preserve_generated_test_artifacts(self.directory, self.directory)
        self.assertFalse(generated.exists())
        self.assertFalse(lock.exists())
        self.assertEqual(
            (
                self.directory / "generated/codex-rs/core/tests/suite/example.snap.new"
            ).read_text(),
            "snapshot candidate",
        )

    def test_preserve_generated_test_artifacts_rejects_unexpected_source_change(self):
        with patch.object(candidate, "git", return_value="?? unexpected.txt\0"):
            with self.assertRaisesRegex(ValueError, "unexpected source path"):
                candidate.preserve_generated_test_artifacts(
                    self.directory, self.directory
                )

    def test_package_smokes_require_both_formats_and_real_linux_coverage(self):
        (self.directory / "source-commit.txt").write_text(self.commit)

        def smoke(compression, *, tests=9, skipped=2, failures=0):
            (self.directory / f"smoke-{compression}.xml").write_text(
                f'<testsuites><testsuite tests="{tests}" skipped="{skipped}" failures="{failures}" errors="0"/></testsuites>'
            )

        smoke("gzip")
        with self.assertRaises(OSError):
            candidate.verify_package(self.directory, self.commit)
        smoke("zstd")
        candidate.verify_package(self.directory, self.commit)
        with self.assertRaises(ValueError):
            candidate.verify_package(self.directory, "b" * 40)
        for compression in ("gzip", "zstd"):
            for change in ({"tests": 0, "skipped": 0}, {"skipped": 9}, {"failures": 1}):
                with self.subTest(compression=compression, change=change):
                    smoke(compression, **change)
                    with self.assertRaises(ValueError):
                        candidate.verify_package(self.directory, self.commit)
                    smoke(compression)


if __name__ == "__main__":
    unittest.main()
