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
            self.skipTest(
                "the acceptance runner is already inside its isolated namespace"
            )
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
            lambda r: r.update(
                checks=[
                    check for check in r["checks"] if check["name"] != "usage-history"
                ]
            ),
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


@unittest.skipUnless(sys.platform == "linux", "Linux candidate release verification")
class UsageHistoryVerifierTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="usage-verifier-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.scopes = {
            "fixture_shape_version": 2,
            "window_start_ms": 6_000_000_000,
            "window_end_ms": 6_086_400_000,
            "expected_open_operations": 1,
            "expected_wait_spans": 1,
            "expected_thread_count": 64,
            "expected_phase_activity_pairs": 4,
            "expected_token_facts": 49,
            "root_thread_id": "scale-root",
            "include_descendants": True,
            "expected_root_window_operations": 8,
            "expected_root_window_provider_tokens": 8400,
            "expected_all_operations": 8,
            "expected_all_provider_tokens": 8400,
            "repositories": [
                {
                    "name": f"synthetic-{n}",
                    "repository_id": str(n),
                    "expected_operations": 1,
                    "expected_provider_tokens": 1050,
                }
                for n in range(8)
            ],
        }

    def test_usage_fixture_shape_rejects_legacy_and_calculates_lifetime_counts(self):
        import check_usage_history as history

        self.assertEqual(
            history.expected_shape(1300),
            {
                "open_operations": 3,
                "wait_spans": 82,
                "thread_count": 64,
                "phase_activity_pairs": 4,
                "token_facts": 7801,
            },
        )
        (self.directory / "fixture-marker").write_text(history.MARKER)
        (self.directory / "scope.json").write_text(json.dumps(self.scopes))
        self.assertEqual(history.scopes_for(self.directory), self.scopes)
        legacy = dict(self.scopes)
        legacy["fixture_shape_version"] = 1
        (self.directory / "scope.json").write_text(json.dumps(legacy))
        with self.assertRaisesRegex(ValueError, "legacy usage fixture shape"):
            history.scopes_for(self.directory)

    def server_binary(self, *, faults, warming=False):
        binary = self.directory / "fixture-server"
        binary.write_text(
            "#!/usr/bin/env python3\nimport json,sys\n"
            f"faults = {faults!r}\nwarming = {warming!r}\n"
            + """
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    response = {"id": request["id"], "result": {}}
    if request["method"] == "localUsage/summary":
        params = request["params"]
        key = params.get("repositoryKey")
        if key is not None:
            assert params["fromAt"] == 6000000 and params["toAt"] == 6086400
        if warming and key is None:
            response = {"id": request["id"], "error": {"code": -32602,
                "data": {"code": "usage_report_warming", "retryable": True,
                         "retryAfterMs": 1000, "cache": {"schemaVersion": 8,
                         "ready": False, "progress": [{"source": "operations",
                         "cursor": 1, "highWater": 100}]}}}}
            if faults:
                response["error"]["data"]["cache"]["progress"] = faults
        elif faults and key == "2":
            response = {"id": request["id"], "error": {"code": -32602,
                "message": "private fixture sentinel", "data": {"code": "usage_report_timeout"}}}
        else:
            operations = 8 if key is None else 2 if faults and key == "1" else 1
            tokens = 8400 if key is None else "private fixture sentinel" if faults and key == "3" else 1050
            response["result"] = {"report": {"counts": {"operations": operations}},
                                  "aggregate": {"totalTokens": tokens}}
    print(json.dumps(response), flush=True)
"""
        )
        binary.chmod(0o700)
        return binary

    def test_public_probe_accepts_exact_windows_and_unbounded_totals(self):
        import check_usage_history as history

        binary = self.server_binary(faults=False)
        self.assertTrue(
            history.public_probe(binary, self.directory, self.scopes, dict(os.environ))
        )
        self.assertTrue(
            history.public_probe(
                binary,
                self.directory,
                self.scopes,
                dict(os.environ),
                scope="all",
                evidence_name="global.json",
            )
        )

    def test_public_probe_preserves_other_windows_and_rejects_wrong_totals_and_timeout(
        self,
    ):
        import check_usage_history as history

        binary = self.server_binary(faults=True)
        self.assertFalse(
            history.public_probe(binary, self.directory, self.scopes, dict(os.environ))
        )
        text = (self.directory / "public-window-proof.json").read_text()
        rows = json.loads(text)
        self.assertEqual(len(rows), 8)
        self.assertEqual(
            [row["repository"] for row in rows if not row["passed"]],
            ["synthetic-1", "synthetic-2", "synthetic-3"],
        )
        self.assertEqual(rows[2]["failure"], "usage_report_timeout")
        self.assertNotIn("private fixture sentinel", text)

    def test_rebuilding_probe_requires_warming_instead_of_successful_totals(self):
        import check_usage_history as history

        for warming in (False, True):
            with self.subTest(warming=warming):
                binary = self.server_binary(faults=False, warming=warming)
                self.assertEqual(
                    history.public_probe(
                        binary,
                        self.directory,
                        self.scopes,
                        dict(os.environ),
                        scope="warming",
                        evidence_name="warming.json",
                    ),
                    warming,
                )

    def test_native_proof_requires_every_original_window_and_exact_totals(self):
        import check_usage_history as history

        path = self.directory / "native-window-proof.json"
        good = [
            {
                "repository": row["name"],
                "operations": 1,
                "provider_tokens": 1050,
                "elapsed_ms": 1,
            }
            for row in self.scopes["repositories"]
        ]
        path.write_text(json.dumps(good))
        self.assertTrue(history.verify_native(self.directory, self.scopes))
        for bad in (
            None,
            {},
            [None] * 8,
            [dict(good[0], repository=[]), *good[1:]],
            good[:-1],
            [dict(good[0], provider_tokens=1051), *good[1:]],
            [dict(good[0], elapsed_ms=15001), *good[1:]],
            [dict(good[0], elapsed_ms=-1), *good[1:]],
            [dict(good[0], operations=True), *good[1:]],
        ):
            path.write_text(json.dumps(bad))
            self.assertFalse(history.verify_native(self.directory, self.scopes))

    def test_warming_probe_rejects_malformed_source_progress(self):
        import check_usage_history as history

        for progress in (
            [None],
            [{"source": 1, "cursor": "x", "highWater": 100}],
            [{"source": "operations", "cursor": 2, "highWater": 1}],
        ):
            with self.subTest(progress=progress):
                binary = self.server_binary(faults=progress, warming=True)
                self.assertFalse(
                    history.public_probe(
                        binary,
                        self.directory,
                        self.scopes,
                        dict(os.environ),
                        scope="warming",
                    )
                )


if __name__ == "__main__":
    unittest.main()
