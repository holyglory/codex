#!/usr/bin/env python3
"""Focused checks for release identity and archive naming guards."""

import tempfile
from pathlib import Path
import unittest

from verify_npm_release import PLATFORMS, tarball_paths, verify_release


class NpmReleaseContractTest(unittest.TestCase):
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
