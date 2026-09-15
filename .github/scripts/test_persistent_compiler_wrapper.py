from contextlib import contextmanager
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import run_with_persistent_compiler_cache as wrapper


@contextmanager
def connected_cache(_environment):
    yield {
        wrapper.URL_ENV: "http://127.0.0.1:12345/approved-cache",
        "SCCACHE_PATH": "cache-tool",
        "TMPDIR": "product-temporary-directory",
    }


def statistics(write_errors=0):
    return {
        "cache_location": "bazel-http",
        "version": "0.16.0",
        "stats": {
            "cache_hits": {"counts": {"Rust": 0}},
            "cache_misses": {"counts": {"Rust": 1}},
            "cache_writes": 1 - write_errors,
            "cache_write_errors": write_errors,
            "cache_read_errors": 0,
            "cache_timeouts": 0,
            "cache_errors": {"counts": {}},
        },
    }


class PersistentCompilerWrapperTests(unittest.TestCase):
    def test_build_receives_scrubbed_environment_and_preserves_its_temporary_paths(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / "health.json"
            with (
                patch.object(wrapper, "remote_cache", connected_cache),
                patch.object(
                    wrapper, "wait_for_cache_writes", return_value=statistics()
                ),
                patch.object(
                    wrapper.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0),
                ) as execute,
            ):
                code = wrapper.run(
                    ["build"],
                    receipt,
                    {"CODEX_BAZEL_CACHE_SSH_KEY": "private-fixture"},
                    defer_health=False,
                )
                self.assertEqual(code, 0)
                build = execute.call_args_list[1]
                self.assertEqual(build.args, (["build"],))
                self.assertEqual(
                    build.kwargs["env"]["TMPDIR"], "product-temporary-directory"
                )
                self.assertNotIn("CODEX_BAZEL_CACHE_SSH_KEY", build.kwargs["env"])
                self.assertTrue(
                    build.kwargs["env"]["SCCACHE_BAZEL_REMOTE_CACHE_URL"].endswith(
                        "-compiler-v1"
                    )
                )
                self.assertEqual(
                    execute.call_args_list[-1].args, (["cache-tool", "--stop-server"],)
                )
            self.assertEqual(json.loads(receipt.read_text())["failures"], [])
            self.assertNotIn("private-fixture", receipt.read_text())

    def test_deferred_cache_failure_remains_a_failing_final_check(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / "health.json"
            with (
                patch.object(wrapper, "remote_cache", connected_cache),
                patch.object(
                    wrapper, "wait_for_cache_writes", return_value=statistics(1)
                ),
                patch.object(
                    wrapper.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0),
                ),
            ):
                self.assertEqual(
                    wrapper.run(["build"], receipt, {}, defer_health=True), 0
                )
            result = subprocess.run(
                [
                    sys.executable,
                    str(Path(wrapper.__file__).with_name("compiler_cache_health.py")),
                    "--file",
                    str(receipt),
                    "--backend",
                    "bazel-http",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(result.stdout)["failures"], ["write_errors=1"])

    def test_build_failure_and_daemon_cleanup_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            with (
                patch.object(wrapper, "remote_cache", connected_cache),
                patch.object(
                    wrapper, "wait_for_cache_writes", return_value=statistics()
                ),
                patch.object(
                    wrapper.subprocess,
                    "run",
                    side_effect=[
                        subprocess.CompletedProcess([], 0),
                        subprocess.CompletedProcess([], 42),
                        subprocess.CompletedProcess([], 0),
                    ],
                ) as execute,
            ):
                self.assertEqual(
                    wrapper.run(
                        ["build"],
                        Path(directory) / "health.json",
                        {},
                        defer_health=False,
                    ),
                    42,
                )
                self.assertEqual(
                    execute.call_args_list[-1].args, (["cache-tool", "--stop-server"],)
                )

    def test_failed_startup_still_cleans_up_only_its_own_daemon(self):
        with tempfile.TemporaryDirectory() as directory:
            with (
                patch.object(wrapper, "remote_cache", connected_cache),
                patch.object(
                    wrapper.subprocess,
                    "run",
                    side_effect=[
                        subprocess.CalledProcessError(1, "start"),
                        subprocess.CompletedProcess([], 0),
                    ],
                ) as execute,
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    wrapper.run(
                        ["build"],
                        Path(directory) / "health.json",
                        {},
                        defer_health=False,
                    )
                self.assertEqual(len(execute.call_args_list), 2)
                self.assertEqual(
                    execute.call_args_list[-1].args, (["cache-tool", "--stop-server"],)
                )


if __name__ == "__main__":
    unittest.main()
