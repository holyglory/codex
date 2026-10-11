import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from run_with_bounded_bazel_cache import cache_root, run, trim_cache


class BoundedBazelCacheTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def entry(self, letter, size, modified, store="cas"):
        path = self.root / store / (letter * 2) / (letter * 64)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"x" * size)
        os.utime(path, ns=(modified, modified))
        return path

    def test_evicts_oldest_entries_and_keeps_recent_hits(self):
        old = self.entry("a", 4, 1)
        recent = self.entry("b", 5, 3)
        metadata = self.entry("c", 2, 2, "ac")
        self.assertEqual(
            trim_cache(self.root, 7), {"retained_bytes": 7, "removed_entries": 1}
        )
        self.assertFalse(old.exists())
        self.assertEqual(
            (recent.read_bytes(), metadata.read_bytes()), (b"xxxxx", b"xx")
        )
        self.assertEqual(
            trim_cache(self.root, 7), {"retained_bytes": 7, "removed_entries": 0}
        )

    def test_does_not_touch_inflight_files_or_unknown_formats(self):
        self.entry("a", 4, 1)
        temporary = self.root / "tmp" / "inflight"
        temporary.parent.mkdir()
        temporary.write_bytes(b"in progress")
        unknown = self.root / "cas" / "aa" / "not-a-digest"
        unknown.write_bytes(b"unknown")
        trim_cache(self.root, 1)
        self.assertEqual(
            (temporary.read_bytes(), unknown.read_bytes()), (b"in progress", b"unknown")
        )

    def test_cache_shrinks_below_its_cap_when_build_storage_is_low(self):
        old = self.entry("a", 4, 1)
        recent = self.entry("b", 5, 3)
        metadata = self.entry("c", 2, 2, "ac")
        with patch(
            "run_with_bounded_bazel_cache.shutil.disk_usage",
            return_value=SimpleNamespace(free=2),
        ):
            self.assertEqual(
                trim_cache(self.root, 100, reserve_bytes=8),
                {"retained_bytes": 5, "removed_entries": 2},
            )
        self.assertFalse(old.exists())
        self.assertFalse(metadata.exists())
        self.assertEqual(recent.read_bytes(), b"xxxxx")

    def test_rejects_persistent_hosts_and_symlink_roots(self):
        environment = {
            "GITHUB_ACTIONS": "true",
            "RUNNER_ENVIRONMENT": "github-hosted",
            "RUNNER_TEMP": str(self.root),
        }
        for override in (
            {"GITHUB_ACTIONS": "false"},
            {"RUNNER_ENVIRONMENT": "self-hosted"},
        ):
            with self.assertRaises(ValueError):
                cache_root(environment | override)
        outside = self.root / "outside"
        outside.mkdir()
        link = self.root / "bazel-action-cache"
        link.symlink_to(outside, target_is_directory=True)
        with self.assertRaises(ValueError):
            cache_root(environment)

    def test_symlinks_inside_the_cache_do_not_delete_external_data(self):
        outside = self.root / "valuable"
        outside.write_bytes(b"preserve")
        entry = self.entry("a", 4, 1)
        entry.unlink()
        entry.symlink_to(outside)
        self.assertEqual(
            trim_cache(self.root, 1), {"retained_bytes": 0, "removed_entries": 0}
        )
        self.assertEqual(outside.read_bytes(), b"preserve")
        self.assertTrue(entry.is_symlink())

    def test_maintenance_runs_while_the_build_is_active_and_after_it_exits(self):
        with (
            patch("run_with_bounded_bazel_cache.subprocess.Popen") as spawn,
            patch(
                "run_with_bounded_bazel_cache.trim_cache",
                return_value={"removed_entries": 0},
            ) as trim,
            patch(
                "run_with_bounded_bazel_cache.time.monotonic", side_effect=[0, 11, 11]
            ),
        ):
            spawn.return_value.wait.side_effect = [
                subprocess.TimeoutExpired("bazel", 0.1),
                0,
            ]
            self.assertEqual(run(["bazel"], self.root, 7), 0)
            self.assertEqual(trim.call_count, 3)
            for call in trim.call_args_list:
                self.assertEqual(call.args, (self.root, 7))

    def test_cache_maintenance_never_changes_the_command_result(self):
        for code in (0, 1, 37):
            with (
                self.subTest(code=code),
                patch("run_with_bounded_bazel_cache.subprocess.Popen") as spawn,
            ):
                spawn.return_value.wait.side_effect = [
                    subprocess.TimeoutExpired("bazel", 0.1),
                    code,
                ]
                with patch(
                    "run_with_bounded_bazel_cache.trim_cache",
                    side_effect=OSError("unavailable"),
                ):
                    self.assertEqual(
                        run(["bazel", "test", "//..."], self.root, 1), code
                    )
                spawn.assert_called_once_with(["bazel", "test", "//..."])


if __name__ == "__main__":
    unittest.main()
