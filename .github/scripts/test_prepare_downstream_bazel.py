from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from prepare_downstream_bazel import prepare


class PrepareDownstreamBazelTest(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.environment_file = self.root / "environment"
        self.environment_file.write_text("EXISTING=value\n")
        self.environ = {
            "GITHUB_ACTIONS": "true",
            "RUNNER_ENVIRONMENT": "github-hosted",
            "RUNNER_OS": "Linux",
            "GITHUB_WORKSPACE": str(self.root),
            "GITHUB_ENV": str(self.environment_file),
        }

    def test_rejects_non_disposable_environments_without_side_effects(self) -> None:
        for key, value in (
            ("GITHUB_ACTIONS", "false"),
            ("RUNNER_ENVIRONMENT", "self-hosted"),
            ("RUNNER_ENVIRONMENT", ""),
            ("RUNNER_OS", "Windows"),
            ("RUNNER_OS", "macOS"),
        ):
            with (
                self.subTest(key=key, value=value),
                patch("prepare_downstream_bazel.subprocess.run") as run,
                patch("prepare_downstream_bazel.tempfile.mkdtemp") as mkdtemp,
            ):
                with self.assertRaises(RuntimeError):
                    prepare(self.environ | {key: value})
                run.assert_not_called()
                mkdtemp.assert_not_called()
                self.assertEqual(self.environment_file.read_text(), "EXISTING=value\n")

    def test_failed_cleanup_does_not_report_preparation_success(self) -> None:
        with (
            patch(
                "prepare_downstream_bazel.subprocess.run",
                side_effect=[None, subprocess.CalledProcessError(1, "sudo")],
            ),
            patch("prepare_downstream_bazel.tempfile.mkdtemp") as mkdtemp,
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                prepare(self.environ)
            mkdtemp.assert_not_called()
        self.assertEqual(self.environment_file.read_text(), "EXISTING=value\n")

    @unittest.skipUnless(sys.platform == "linux", "Linux runner Unix socket layout")
    def test_hosted_preparation_supports_bazel_nested_unix_socket(self) -> None:
        # Keep cleanup mocked: a unit test must never delete installed SDKs.
        with patch("prepare_downstream_bazel.subprocess.run") as run:
            prepare(self.environ)
        lines = self.environment_file.read_text().splitlines()
        self.assertEqual(lines[0], "EXISTING=value")
        key, temporary_path = lines[1].split("=", 1)
        self.assertEqual(key, "CODEX_BAZEL_TEST_TMPDIR")
        # Model Bazel's hash, tempfile's directory, and a daemon socket name.
        root = Path(temporary_path)
        self.addCleanup(root.rmdir)
        hashed = root / ("a" * 32)
        hashed.mkdir()
        self.addCleanup(hashed.rmdir)
        fixture = hashed / ".tmpabcdef"
        fixture.mkdir()
        self.addCleanup(fixture.rmdir)
        endpoint = fixture / ("control-" + "b" * 32 + ".sock")
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(endpoint))
            self.addCleanup(endpoint.unlink)
        self.assertEqual(
            run.call_args_list[1].args[0],
            [
                "sudo",
                "rm",
                "-rf",
                "--",
                "/usr/local/lib/android",
                "/usr/share/dotnet",
                "/opt/ghc",
                "/opt/hostedtoolcache/CodeQL",
            ],
        )


if __name__ == "__main__":
    unittest.main()
