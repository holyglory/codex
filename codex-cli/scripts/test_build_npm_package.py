#!/usr/bin/env python3

import json
from pathlib import Path
import sys
import tempfile
import unittest


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[1]
sys.path.insert(0, str(SCRIPT_DIR))

import build_npm_package


class BuildNpmPackageTest(unittest.TestCase):
    def test_stages_downstream_root_package_with_platform_aliases(self) -> None:
        version = "1.2.3-multi.4"
        with tempfile.TemporaryDirectory() as temp_dir:
            staging_dir = Path(temp_dir)
            build_npm_package.stage_sources(staging_dir, version, "codex")

            source_package = json.loads(
                (REPO_ROOT / "codex-cli" / "package.json").read_text()
            )
            source_package["version"] = version
            source_package["files"] = ["bin/codex.js", "NOTICE"]
            source_package["optionalDependencies"] = {
                "@holyglory/codex-linux-x64": (
                    "npm:@holyglory/codex@1.2.3-multi.4-linux-x64"
                ),
                "@holyglory/codex-linux-arm64": (
                    "npm:@holyglory/codex@1.2.3-multi.4-linux-arm64"
                ),
                "@holyglory/codex-darwin-x64": (
                    "npm:@holyglory/codex@1.2.3-multi.4-darwin-x64"
                ),
                "@holyglory/codex-darwin-arm64": (
                    "npm:@holyglory/codex@1.2.3-multi.4-darwin-arm64"
                ),
                "@holyglory/codex-win32-x64": (
                    "npm:@holyglory/codex@1.2.3-multi.4-win32-x64"
                ),
                "@holyglory/codex-win32-arm64": (
                    "npm:@holyglory/codex@1.2.3-multi.4-win32-arm64"
                ),
            }

            self.assertEqual(
                json.loads((staging_dir / "package.json").read_text()),
                source_package,
            )
            self.assertEqual(
                {
                    filename: (staging_dir / filename).read_bytes()
                    for filename in ("LICENSE", "NOTICE")
                },
                {
                    filename: (REPO_ROOT / filename).read_bytes()
                    for filename in ("LICENSE", "NOTICE")
                },
            )

    def test_stages_platform_payload_under_downstream_package(self) -> None:
        version = "1.2.3-multi.4"
        with tempfile.TemporaryDirectory() as temp_dir:
            staging_dir = Path(temp_dir)
            build_npm_package.stage_sources(
                staging_dir,
                version,
                "codex-linux-x64",
            )

            source_package = json.loads(
                (REPO_ROOT / "codex-cli" / "package.json").read_text()
            )
            self.assertEqual(
                json.loads((staging_dir / "package.json").read_text()),
                {
                    "name": "@holyglory/codex",
                    "version": "1.2.3-multi.4-linux-x64",
                    "description": source_package["description"],
                    "license": "Apache-2.0",
                    "os": ["linux"],
                    "cpu": ["x64"],
                    "files": ["vendor", "NOTICE"],
                    "repository": source_package["repository"],
                    "homepage": source_package["homepage"],
                    "bugs": source_package["bugs"],
                    "publishConfig": source_package["publishConfig"],
                    "engines": source_package["engines"],
                    "packageManager": source_package["packageManager"],
                },
            )

    def test_rejects_semver_build_metadata_that_npm_would_strip(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with self.assertRaisesRegex(RuntimeError, "npm strips SemVer"):
                build_npm_package.stage_sources(
                    Path(temp_dir),
                    "1.2.3+multi.4",
                    "codex",
                )


if __name__ == "__main__":
    unittest.main()
