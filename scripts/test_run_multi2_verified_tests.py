"""Exercise verified V8 selection without compiling V8 or running Rust tests."""

from concurrent.futures import ThreadPoolExecutor
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

import run_multi2_verified_tests as runner


@unittest.skipUnless(sys.platform == "linux", "GNU local release checks require Linux")
class VerifiedMulti2Tests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.environment = {"CARGO_TARGET_DIR": str(self.root / "target")}
        self.builds = []
        for name, value in (
            ("ROOT", self.root),
            ("PACKAGE_ROOT", self.root / "target/multi2-local"),
            ("source_version", lambda: "0.170.0+multi.2"),
            ("git", lambda *args: "a" * 40 if args[0] == "rev-parse" else ""),
            ("prepare_v8", self.prepare_pair),
        ):
            context = patch.object(runner.package, name, value)
            context.start()
            self.addCleanup(context.stop)
        context = patch.object(
            runner,
            "v8_input_manifest",
            return_value={
                "paths": {"third_party/v8/fixture.bzl": "blob"},
                "sha256": "b" * 64,
            },
        )
        context.start()
        self.addCleanup(context.stop)
        context = patch("sys.stdout", new_callable=io.StringIO)
        context.start()
        self.addCleanup(context.stop)

    def prepare_pair(self, environment, version):
        handoff = Path(environment["V8_HANDOFF_DIR"])
        receipt = handoff / "v8-pair.json"
        if not handoff.exists():
            self.builds.append(handoff)
            handoff.mkdir(parents=True)
            receipt.write_text(json.dumps({"version": version}))
        json.loads(receipt.read_bytes())
        environment["RUSTY_V8_ARCHIVE"] = str(handoff / "pair/archive.a.gz")
        environment["RUSTY_V8_SRC_BINDING_PATH"] = str(handoff / "pair/binding.rs")
        environment.pop("V8_FROM_SOURCE", None)
        return {
            "handoff_dir": str(handoff),
            "receipt_sha256": hashlib.sha256(receipt.read_bytes()).hexdigest(),
        }

    def test_default_cache_reuses_verified_pairs_and_preserves_invalid_generations(
        self,
    ):
        fresh_env = {**self.environment, "V8_FROM_SOURCE": "1"}
        first = runner.prepare_test_v8(fresh_env, "0.170.0+multi.2")
        cached_env = {**self.environment, "V8_FROM_SOURCE": "1"}
        second = runner.prepare_test_v8(cached_env, "0.170.0+multi.2")
        self.assertEqual(first, second)
        self.assertNotIn("V8_FROM_SOURCE", fresh_env)
        self.assertNotIn("V8_FROM_SOURCE", cached_env)
        self.assertEqual(len(self.builds), 1)
        self.assertFalse(runner.package.PACKAGE_ROOT.exists())
        for contents in ("invalid retained fixture", None):
            retained = Path(first["handoff_dir"])
            receipt = retained / "v8-pair.json"
            if contents is None:
                receipt.unlink()
            else:
                receipt.write_text(contents)
            with patch("sys.stderr", new_callable=io.StringIO):
                replacement = runner.prepare_test_v8(
                    dict(self.environment), "0.170.0+multi.2"
                )
            self.assertNotEqual(first["handoff_dir"], replacement["handoff_dir"])
            self.assertTrue(retained.is_dir())
            if contents is None:
                self.assertFalse(receipt.exists())
            else:
                self.assertEqual(receipt.read_text(), contents)
            first = replacement
        self.assertEqual(len(self.builds), 3)
        self.assertFalse(runner.package.PACKAGE_ROOT.exists())

    def test_concurrent_preparation_builds_only_one_generation(self):
        import fcntl

        real_flock = fcntl.flock
        attempts = threading.Barrier(2)

        def flock(file, operation):
            attempts.wait(timeout=5)
            real_flock(file, operation)

        with patch.object(fcntl, "flock", side_effect=flock) as locks:
            with ThreadPoolExecutor(max_workers=2) as workers:
                futures = [
                    workers.submit(
                        runner.prepare_test_v8,
                        dict(self.environment),
                        "0.170.0+multi.2",
                    )
                    for _ in range(2)
                ]
                results = [future.result(timeout=10) for future in futures]
        self.assertEqual(results[0], results[1])
        self.assertEqual(len(self.builds), 1)
        self.assertEqual(locks.call_count, 2)

    def test_selected_pair_preserves_host_setup_and_rejects_invalid_receipts(self):
        environment = {
            **self.environment,
            "V8_HANDOFF_DIR": str(self.root / "selected"),
            "RUSTUP_TOOLCHAIN": "1.95.0",
            "TMPDIR": "/tmp/codextests",
            "PKG_CONFIG": "/fixture/native/cargo-pkg-config",
            "BAZEL_JOBS": "3",
            "RUSTY_V8_ARCHIVE": "incompatible-published-archive",
            "RUSTY_V8_SRC_BINDING_PATH": "incompatible-published-binding",
            "V8_FROM_SOURCE": "1",
        }
        proof = self.prepare_pair(dict(environment), "0.170.0+multi.2")
        with (
            patch.dict(os.environ, environment, clear=True),
            patch.object(runner.subprocess, "run") as run,
            patch.object(
                runner.package, "verify_v8_pair", return_value=proof
            ) as verify,
        ):
            runner.main(["--locked", "-p", "codex-v8-poc"])
            self.assertEqual(
                run.call_args.args[0],
                [
                    "just",
                    "test",
                    "--locked",
                    "-p",
                    "codex-v8-poc",
                ],
            )
            actual = run.call_args.kwargs["env"]
            for key in ("RUSTUP_TOOLCHAIN", "TMPDIR", "PKG_CONFIG", "BAZEL_JOBS"):
                self.assertEqual(actual[key], environment[key])
            self.assertEqual(
                actual["RUSTY_V8_ARCHIVE"],
                str(self.root / "selected/pair/archive.a.gz"),
            )
            self.assertEqual(
                actual["RUSTY_V8_SRC_BINDING_PATH"],
                str(self.root / "selected/pair/binding.rs"),
            )
            self.assertNotIn("V8_FROM_SOURCE", actual)
            verify.assert_called_once_with(actual)
            (self.root / "selected/v8-pair.json").write_text("invalid selected fixture")
            run.reset_mock()
            with self.assertRaises(ValueError):
                runner.main([])
            run.assert_not_called()
        self.assertEqual(len(self.builds), 1)


if __name__ == "__main__":
    unittest.main()
