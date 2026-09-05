import base64
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

import stage_verified_npm_release as stage


class StageVerifiedNpmReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.version = "0.153.0-multi.1"
        self.paths = stage.tarball_paths(self.root, self.version)
        for platform, path in self.paths.items():
            # These isolated orchestration fixtures are not product packages.
            path.write_bytes(f"tarball fixture: {platform}".encode())
        verifier = patch.object(stage, "verify_release")
        self.verify = verifier.start()
        self.addCleanup(verifier.stop)

    def published(self, platform):
        return {
            "name": stage.PACKAGE_NAME,
            "version": self.version
            if platform == "root"
            else f"{self.version}-{platform}",
            "dist": {
                "integrity": "sha512-"
                + base64.b64encode(
                    hashlib.sha512(self.paths[platform].read_bytes()).digest()
                ).decode("ascii"),
            },
        }

    def test_new_release_stages_six_platforms_then_root(self):
        with (
            patch.object(stage, "registry_metadata", return_value=None),
            patch.object(stage.subprocess, "run") as run,
        ):
            self.assertEqual(stage.stage_release(self.root, self.version), 7)
        self.verify.assert_called_once_with(self.root, self.version)
        self.assertEqual(
            [call.args[0][3] for call in run.call_args_list],
            [str(path) for path in self.paths.values()],
        )
        self.assertEqual(run.call_args.args[0][-2:], ["--tag", "latest"])
        for call in run.call_args_list:
            self.assertTrue(call.kwargs["check"])
            self.assertIn("--provenance", call.args[0])

    def test_identical_bootstrap_is_skipped_without_republishing(self):
        responses = [
            self.published("linux-x64"),
            {"linux-x64": f"{self.version}-linux-x64"},
        ]
        with (
            patch.object(
                stage, "registry_metadata", side_effect=responses + [None] * 6
            ),
            patch.object(stage.subprocess, "run") as run,
        ):
            self.assertEqual(stage.stage_release(self.root, self.version), 6)
        self.assertEqual(
            [call.args[0][3] for call in run.call_args_list],
            [
                str(path)
                for platform, path in self.paths.items()
                if platform != "linux-x64"
            ],
        )

    def test_root_collision_fails_before_any_stage_write(self):
        for change in (
            {"name": "@someone/else"},
            {"version": "0.0.0"},
            {"dist": {"integrity": "sha512-different"}},
            {"dist": None},
        ):
            with (
                self.subTest(change=change),
                patch.object(
                    stage,
                    "registry_metadata",
                    side_effect=[None] * 6 + [self.published("root") | change],
                ),
                patch.object(stage.subprocess, "run") as run,
            ):
                with self.assertRaises(RuntimeError):
                    stage.stage_release(self.root, self.version)
                run.assert_not_called()

    def test_existing_version_with_wrong_tag_is_not_silently_accepted(self):
        with (
            patch.object(
                stage,
                "registry_metadata",
                side_effect=[self.published("linux-x64"), {"linux-x64": "0.150.0"}],
            ),
            patch.object(stage.subprocess, "run") as run,
        ):
            with self.assertRaises(RuntimeError):
                stage.stage_release(self.root, self.version)
            run.assert_not_called()

    def test_invalid_candidate_and_registry_outage_prevent_staging(self):
        self.verify.side_effect = RuntimeError("invalid candidate")
        with (
            patch.object(stage, "registry_metadata") as metadata,
            patch.object(stage.subprocess, "run") as run,
        ):
            with self.assertRaises(RuntimeError):
                stage.stage_release(self.root, self.version)
            metadata.assert_not_called()
            run.assert_not_called()
        self.verify.side_effect = None
        with (
            patch.object(
                stage, "registry_metadata", side_effect=RuntimeError("unavailable")
            ),
            patch.object(stage.subprocess, "run") as run,
        ):
            with self.assertRaises(RuntimeError):
                stage.stage_release(self.root, self.version)
            run.assert_not_called()

    def test_npm_failure_stops_without_reporting_success(self):
        with (
            patch.object(stage, "registry_metadata", return_value=None),
            patch.object(
                stage.subprocess,
                "run",
                side_effect=subprocess.CalledProcessError(1, "npm"),
            ) as run,
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                stage.stage_release(self.root, self.version)
            self.assertEqual(run.call_count, 1)


class RegistryMetadataTests(unittest.TestCase):
    def test_only_not_found_means_unpublished(self):
        with patch.object(
            stage, "urlopen", side_effect=HTTPError("url", 404, "missing", None, None)
        ):
            self.assertIsNone(stage.registry_metadata("package/version"))
        for error in (
            HTTPError("url", 401, "unauthorized", None, None),
            HTTPError("url", 500, "unavailable", None, None),
            URLError("network failure"),
            TimeoutError(),
        ):
            with (
                self.subTest(error=error),
                patch.object(stage, "urlopen", side_effect=error),
            ):
                with self.assertRaises(RuntimeError):
                    stage.registry_metadata("package/version")

    def test_metadata_is_bounded_and_must_be_an_object(self):
        for content in (b"[1]", b"x" * (4 * 1024 * 1024 + 1), b"invalid JSON"):
            with (
                self.subTest(size=len(content)),
                patch.object(stage, "urlopen", return_value=io.BytesIO(content)),
            ):
                with self.assertRaises((RuntimeError, ValueError)):
                    stage.registry_metadata("package/version")
        content = {"name": "@holyglory/codex", "version": "0.153.0-multi.1"}
        with patch.object(
            stage, "urlopen", return_value=io.BytesIO(json.dumps(content).encode())
        ):
            self.assertEqual(stage.registry_metadata("package/version"), content)


if __name__ == "__main__":
    unittest.main()
