"""Check owned bundle transport boundaries without running package assembly."""

import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import local_npm_transport as transport


class LocalNpmTransportTest(unittest.TestCase):
    def test_only_owned_github_api_assets_and_bounded_sizes_are_accepted(self):
        valid = {
            "schema": 1,
            "kind": "codex-local-npm-transport",
            "url": "https://api.github.com/repos/holyglory/codex/releases/assets/123",
            "sha256": "1" * 64,
            "manifest_sha256": "2" * 64,
            "size_bytes": 1,
        }
        self.assertEqual(
            transport.validate_descriptor(valid, transport.REPOSITORY), valid
        )
        for changes in (
            {"url": "https://example.com/candidate.tar.gz"},
            {"url": "https://api.github.com/repos/other/codex/releases/assets/123"},
            {"size_bytes": transport.MAX_ARCHIVE + 1},
            {"size_bytes": True},
        ):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                transport.validate_descriptor(
                    {**valid, **changes}, transport.REPOSITORY
                )

    def test_archive_rejects_traversal_symlinks_duplicates_and_expansion(self):
        for kind in (
            "traversal",
            "absolute",
            "symlink",
            "hardlink",
            "duplicate",
            "expansion",
            "member-count",
            "extension",
        ):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                path = root / "candidate.tar.gz"
                with tarfile.open(path, "w:gz") as archive:
                    member = tarfile.TarInfo(
                        {"traversal": "../outside", "absolute": "/outside"}.get(
                            kind, "file"
                        )
                    )
                    if kind in ("symlink", "hardlink"):
                        member.type, member.linkname = (
                            (tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE),
                            "/tmp/target",
                        )
                    member.size = 4 if kind == "expansion" else 0
                    if kind == "extension":
                        member.type, member.size = tarfile.XHDTYPE, 65537
                    archive.addfile(member, io.BytesIO(b"x" * member.size))
                    if kind == "duplicate":
                        archive.addfile(member)
                    if kind == "member-count":
                        for number in range(64):
                            archive.addfile(tarfile.TarInfo(f"file-{number}"))
                with (
                    patch.object(
                        transport,
                        "MAX_EXPANDED",
                        2 if kind == "expansion" else 16 * 1024**3,
                    ),
                    self.assertRaises(ValueError),
                ):
                    transport.extract_bundle(path, root / "unpacked")
                self.assertFalse((root / "outside").exists())


if __name__ == "__main__":
    unittest.main()
