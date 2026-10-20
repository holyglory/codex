import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import runner_handoff as handoff


COMMIT = "a" * 40


def manifest(*, url="https://github.com/holyglory/codex/releases/download/t/bundle.tar.gz"):
    return {
        "schema": 1,
        "commit": COMMIT,
        "producer": "linux-local",
        "status": "success",
        "checks": [
            {"name": "local-acceptance", "exit_code": 0, "log_sha256": "b" * 64}
        ],
        "artifacts": [
            {
                "name": "linux-bundle.tar.gz",
                "url": url,
                "sha256": "c" * 64,
                "size_bytes": 12,
                "targets": ["x86_64-unknown-linux-musl"],
            }
        ],
    }


class RunnerHandoffTests(unittest.TestCase):
    def test_valid_manifest_covers_required_target(self):
        normalized = handoff.validate(
            manifest(), COMMIT, "linux-local", ["x86_64-unknown-linux-musl"]
        )
        self.assertEqual(normalized["commit"], COMMIT)

    def test_rejects_commit_producer_and_target_mismatches(self):
        for mutate, message in (
            (lambda value: value.update(commit="d" * 40), "commit"),
            (lambda value: value.update(producer="windows-private-controller"), "producer"),
            (lambda value: value["artifacts"][0].update(targets=["other"]), "targets"),
        ):
            value = manifest()
            mutate(value)
            with self.subTest(message=message), self.assertRaisesRegex(
                handoff.HandoffError, message
            ):
                handoff.validate(
                    value, COMMIT, "linux-local", ["x86_64-unknown-linux-musl"]
                )

    def test_rejects_failed_checks_unsafe_urls_and_tampered_digests(self):
        for mutate, message in (
            (lambda value: value["checks"][0].update(exit_code=1), "check failed"),
            (
                lambda value: value["artifacts"][0].update(
                    url="http://github.com/unsafe"
                ),
                "HTTPS",
            ),
            (
                lambda value: value["artifacts"][0].update(
                    url="https://example.invalid/bundle"
                ),
                "not allowlisted",
            ),
            (lambda value: value["artifacts"][0].update(sha256="bad"), "digest"),
        ):
            value = manifest()
            mutate(value)
            with self.subTest(message=message), self.assertRaisesRegex(
                handoff.HandoffError, message
            ):
                handoff.validate(
                    value, COMMIT, "linux-local", ["x86_64-unknown-linux-musl"]
                )

    def test_download_checks_digest_and_rejects_archive_traversal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive_path = root / "bundle.tar.gz"
            payload = io.BytesIO(b"safe")
            with tarfile.open(archive_path, "w:gz") as archive:
                entry = tarfile.TarInfo("x86_64-unknown-linux-musl/codex-package")
                entry.size = len(payload.getvalue())
                archive.addfile(entry, payload)
            data = archive_path.read_bytes()
            value = manifest()
            value["artifacts"][0].update(
                sha256=hashlib.sha256(data).hexdigest(),
                size_bytes=len(data),
            )
            destination = root / "out"
            with patch.object(
                handoff,
                "urlopen",
                return_value=io.BytesIO(data),
            ):
                result = handoff.download(
                    value,
                    destination,
                    COMMIT,
                    "linux-local",
                    ["x86_64-unknown-linux-musl"],
                )
            self.assertEqual(result["artifacts"][0]["size_bytes"], len(data))
            self.assertEqual(
                (destination / "x86_64-unknown-linux-musl/codex-package").read_bytes(),
                b"safe",
            )

            traversal = io.BytesIO()
            with tarfile.open(fileobj=traversal, mode="w:gz") as archive:
                entry = tarfile.TarInfo("../escape")
                entry.size = 1
                archive.addfile(entry, io.BytesIO(b"x"))
            bad = traversal.getvalue()
            value["artifacts"][0].update(
                sha256=hashlib.sha256(bad).hexdigest(), size_bytes=len(bad)
            )
            with patch.object(handoff, "urlopen", return_value=io.BytesIO(bad)):
                with self.assertRaisesRegex(handoff.HandoffError, "unsafe path"):
                    handoff.download(
                        value,
                        root / "bad",
                        COMMIT,
                        "linux-local",
                        ["x86_64-unknown-linux-musl"],
                    )


if __name__ == "__main__":
    unittest.main()
