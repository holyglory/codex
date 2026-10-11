import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

import run_bazel_with_buildbuddy as bazel
import run_with_bazel_remote_cache as cache
from probe_bazel_remote_cache import remote_hits


class RemoteCacheTests(unittest.TestCase):
    def test_probe_counts_real_remote_hits_not_action_cache_or_progress_text(self):
        self.assertEqual(
            remote_hits("INFO: 3 processes: 2 remote cache hit, 1 internal.\n"), 2
        )
        self.assertEqual(
            remote_hits(
                "[1/2] remote cache hit\nINFO: 3 processes: 2 action cache hit, 1 internal.\n"
            ),
            0,
        )
        self.assertEqual(
            remote_hits("INFO: 2 processes: 1 internal, 1 linux-sandbox.\n"), 0
        )

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.env = {
            "RUNNER_TEMP": str(self.root),
            cache.KEY_ENV: "private-fixture",
            cache.HOST_KEY_ENV: "public-fixture",
            cache.HOST_ENV: "cache.example",
        }

    def test_private_files_are_private_and_cannot_overwrite(self):
        target = self.root / "key"
        cache.private_file(target, "fixture")
        self.assertEqual(
            (target.stat().st_mode & 0o777, target.read_text()), (0o600, "fixture\n")
        )
        with self.assertRaises(FileExistsError):
            cache.private_file(target, "replacement")
        self.assertEqual(target.read_text(), "fixture\n")

    def test_transport_pins_host_key_and_only_forwards_to_the_cache(self):
        command = cache.ssh_command(self.root, "cache.example", 12345)
        for option in (
            "StrictHostKeyChecking=yes",
            "IdentitiesOnly=yes",
            "ExitOnForwardFailure=yes",
            "HostKeyAlias=codex-bazel-cache",
            "127.0.0.1:12345:127.0.0.1:9095",
        ):
            self.assertIn(option, command)
        self.assertEqual(command[-1], "codex-bazel-cache@cache.example")
        for hostile in (
            "-oProxyCommand=anything",
            "host\nProxyCommand anything",
            "user@host",
            "host:22",
        ):
            with self.subTest(hostile=hostile), self.assertRaises(ValueError):
                cache.ssh_command(self.root, hostile, 12345)

    def test_connected_child_has_url_but_no_credential_and_tunnel_is_cleaned(self):
        process = MagicMock()
        process.poll.return_value = None
        with (
            patch.object(cache.subprocess, "Popen", return_value=process) as spawn,
            patch.object(cache, "ready", return_value=True),
        ):
            with cache.remote_cache(self.env) as environment:
                self.assertTrue(
                    environment[cache.URL_ENV].endswith("/holyglory-codex-v1")
                )
                self.assertNotIn(cache.KEY_ENV, environment)
                self.assertNotIn(cache.HOST_KEY_ENV, environment)
                self.assertNotIn(cache.KEY_ENV, spawn.call_args.kwargs["env"])
            process.terminate.assert_called_once()
            process.wait.assert_called_once_with(timeout=5)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_connection_failure_runs_the_child_once_and_preserves_failure(self):
        process = MagicMock()
        process.poll.return_value = None
        with (
            patch.object(cache.subprocess, "Popen", return_value=process),
            patch.object(cache, "ready", return_value=False),
            patch.object(cache.subprocess, "run") as child,
        ):
            child.return_value.returncode = 37
            self.assertEqual(cache.run(["build"], self.env), 37)
            child.assert_called_once()
            self.assertNotIn(cache.URL_ENV, child.call_args.kwargs["env"])
            process.terminate.assert_called_once()

    def test_setup_failure_does_not_leak_secret_or_skip_build(self):
        output = io.StringIO()
        with (
            patch.object(cache, "private_file", side_effect=OSError("secret-fixture")),
            patch.object(cache.subprocess, "run") as child,
            contextlib.redirect_stdout(output),
        ):
            child.return_value.returncode = 0
            self.assertEqual(cache.run(["build"], self.env), 0)
            child.assert_called_once()
            self.assertNotIn(cache.KEY_ENV, child.call_args.kwargs["env"])
        self.assertIn("warning", output.getvalue())
        self.assertNotIn("secret-fixture", output.getvalue())
        self.assertEqual(list(self.root.iterdir()), [])

    def test_unconfigured_real_child_preserves_failure_and_does_not_inherit_secret(
        self,
    ):
        environment = dict(os.environ) | {
            cache.KEY_ENV: "fixture",
            cache.URL_ENV: "untrusted",
        }
        environment.pop(cache.HOST_ENV, None)
        program = f"import os; assert {cache.KEY_ENV!r} not in os.environ; assert {cache.URL_ENV!r} not in os.environ; raise SystemExit(37)"
        self.assertEqual(cache.run([sys.executable, "-c", program], environment), 37)

    def test_required_probe_never_silently_passes_without_a_cache(self):
        with patch.object(cache.subprocess, "run") as child:
            self.assertEqual(cache.run(["build"], {}, require_cache=True), 1)
            child.assert_not_called()

    def test_cleanup_when_build_raises(self):
        process = MagicMock()
        process.poll.return_value = None
        with (
            patch.object(cache.subprocess, "Popen", return_value=process),
            patch.object(cache, "ready", return_value=True),
            patch.object(
                cache.subprocess, "run", side_effect=FileNotFoundError("build")
            ),
        ):
            with self.assertRaises(FileNotFoundError):
                cache.run(["missing-build"], self.env)
            process.terminate.assert_called_once()

    def test_bazel_adds_cache_before_targets_without_enabling_remote_execution(self):
        command = bazel.bazel_command(
            "test",
            "--",
            "//fixture",
            env={cache.URL_ENV: "http://127.0.0.1:12345/cache"},
        )
        self.assertEqual(command[-2:], ["--", "//fixture"])
        self.assertIn("--remote_cache=http://127.0.0.1:12345/cache", command)
        self.assertIn("--remote_verify_downloads=true", command)
        self.assertIn("--remote_upload_local_results=true", command)
        self.assertFalse(any(arg.startswith("--remote_executor=") for arg in command))

    def test_bazel_preserves_explicit_cache_and_does_not_add_build_flags_to_info(self):
        environment = {cache.URL_ENV: "http://127.0.0.1:12345/cache"}
        self.assertEqual(
            bazel.bazel_command("info", env=environment), ["bazel", "info"]
        )
        self.assertEqual(
            bazel.bazel_command(
                "build", "--remote_cache=explicit", "//fixture", env=environment
            ),
            ["bazel", "build", "--remote_cache=explicit", "//fixture"],
        )


if __name__ == "__main__":
    unittest.main()
