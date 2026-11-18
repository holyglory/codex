"""Check owned bundle transport boundaries without running package assembly."""

import copy
import hashlib
import io
import os
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
            {"size_bytes": transport.MAX_ASSET},
            {"size_bytes": True},
        ):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                transport.validate_descriptor(
                    {**valid, **changes}, transport.REPOSITORY
                )
        self.assertEqual(
            transport.validate_descriptor(
                {**valid, "size_bytes": transport.MAX_ASSET - 1}, transport.REPOSITORY
            )["size_bytes"],
            transport.MAX_ASSET - 1,
        )

    def parts_fixture(self, root):
        original = root / "original"
        (original / "candidate").mkdir(parents=True)
        (original / "candidate.tar.gz").write_bytes(b"first001second02")
        manifest = original / "candidate/npm-build-manifest.json"
        transport.write_json(manifest, {"schema": 1, "fixture": True})
        transport.write_json(
            original / "local-candidate.json",
            {
                "schema": 1,
                "kind": "codex-local-npm-bundle",
                "npm_published": False,
                "archive": transport.file_record(original / "candidate.tar.gz"),
                "manifest_sha256": transport.file_record(manifest)["sha256"],
                "source_commit": "a" * 40,
                "packaging_commit": "b" * 40,
                "version": "1.2.3-multi.99",
            },
        )
        parts = root / "parts"
        value = transport.split_candidate(original / "local-candidate.json", parts)
        urls = {
            part[
                "name"
            ]: f"https://api.github.com/repos/holyglory/codex/releases/assets/{index}"
            for index, part in enumerate(value["parts"], start=1)
        }
        transport.write_json(root / "assets.json", dict(reversed(list(urls.items()))))
        descriptor = transport.create_multipart_descriptor(
            parts / "parts-manifest.json",
            root / "assets.json",
            root / "descriptor.json",
        )
        return original, parts, descriptor

    def test_split_manifest_and_download_preserve_the_complete_original_bytes(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(transport, "PART_SIZE", 8),
        ):
            root = Path(directory)
            original, parts, descriptor = self.parts_fixture(root)
            self.assertEqual(len(descriptor["parts"]), 2)
            self.assertEqual(
                [part["size_bytes"] for part in descriptor["parts"]], [8, 8]
            )
            self.assertEqual(
                descriptor["sha256"], hashlib.sha256(b"first001second02").hexdigest()
            )
            self.assertEqual(
                transport.read_json(parts / "parts-manifest.json")["manifest_sha256"],
                descriptor["manifest_sha256"],
            )

            def download(part, output):
                output.write_bytes((parts / part["name"]).read_bytes())

            with (
                patch.dict(os.environ, {"GH_TOKEN": "fixture"}),
                patch.object(
                    transport, "download_artifact", side_effect=download
                ) as requests,
            ):
                archive = transport.download_bundle(
                    descriptor, root / "received", transport.REPOSITORY
                )
            self.assertEqual(
                archive.read_bytes(), (original / "candidate.tar.gz").read_bytes()
            )
            self.assertEqual(
                [call.args[0]["name"] for call in requests.call_args_list],
                [part["name"] for part in descriptor["parts"]],
            )
            self.assertEqual(list(archive.parent.iterdir()), [archive])
            with self.assertRaises(FileExistsError):
                transport.split_candidate(original / "local-candidate.json", parts)

    def test_descriptor_refuses_missing_reordered_oversized_and_unowned_parts(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(transport, "PART_SIZE", 8),
        ):
            _, _, valid = self.parts_fixture(Path(directory))
            cases = []
            missing = copy.deepcopy(valid)
            missing["parts"].pop()
            cases.append(missing)
            reordered = copy.deepcopy(valid)
            reordered["parts"].reverse()
            cases.append(reordered)
            oversized = copy.deepcopy(valid)
            oversized["parts"][0]["size_bytes"] = 9
            cases.append(oversized)
            for key, value in (
                ("sha256", "bad"),
                ("name", "../part"),
                ("url", "https://api.github.com/repos/other/codex/releases/assets/1"),
            ):
                changed = copy.deepcopy(valid)
                changed["parts"][0][key] = value
                cases.append(changed)
            cases.extend(
                [
                    {**valid, "size_bytes": transport.MAX_ARCHIVE + 1},
                    {**valid, "url": valid["parts"][0]["url"]},
                    {**valid, "parts": valid["parts"] * 5},
                ]
            )
            for index, value in enumerate(cases):
                with self.subTest(case=index), self.assertRaises(ValueError):
                    transport.validate_descriptor(value, transport.REPOSITORY)

    def test_asset_mapping_requires_every_actual_owned_url_and_original_part_bytes(
        self,
    ):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(transport, "PART_SIZE", 8),
        ):
            root = Path(directory)
            original, parts, descriptor = self.parts_fixture(root)
            urls = {part["name"]: part["url"] for part in descriptor["parts"]}
            for index, mapping in enumerate(
                (
                    {},
                    {**urls, "extra": next(iter(urls.values()))},
                    {**urls, next(iter(urls)): "https://example.com/part"},
                )
            ):
                assets = root / f"bad-assets-{index}.json"
                transport.write_json(assets, mapping)
                with self.assertRaises(ValueError):
                    transport.create_multipart_descriptor(
                        parts / "parts-manifest.json",
                        assets,
                        root / f"bad-{index}.json",
                    )
            changed_manifest = transport.read_json(parts / "parts-manifest.json")
            changed_manifest["archive"]["sha256"] = "0" * 64
            transport.write_json(parts / "changed-manifest.json", changed_manifest)
            with self.assertRaisesRegex(ValueError, "Original archive differs"):
                transport.create_multipart_descriptor(
                    parts / "changed-manifest.json",
                    root / "assets.json",
                    root / "bad-original.json",
                )
            (parts / descriptor["parts"][0]["name"]).write_bytes(b"changed!")
            with self.assertRaisesRegex(ValueError, "part SHA-256"):
                transport.create_multipart_descriptor(
                    parts / "parts-manifest.json",
                    root / "assets.json",
                    root / "changed.json",
                )
            (original / "candidate.tar.gz").write_bytes(b"changed original")
            with self.assertRaisesRegex(ValueError, "Local candidate changed"):
                transport.split_candidate(
                    original / "local-candidate.json", root / "new-parts"
                )

    def test_download_rejects_wrong_missing_reordered_oversized_parts_and_aggregate(
        self,
    ):
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(transport, "PART_SIZE", 8),
        ):
            root = Path(directory)
            _, parts, valid = self.parts_fixture(root)
            bodies = {
                part["url"]: (parts / part["name"]).read_bytes()
                for part in valid["parts"]
            }
            for failure in ("wrong", "missing", "reordered", "oversized", "aggregate"):
                descriptor = copy.deepcopy(valid)
                if failure == "aggregate":
                    descriptor["sha256"] = "0" * 64

                def download(part, output):
                    if failure == "missing":
                        raise FileNotFoundError("missing uploaded part")
                    body = bodies[part["url"]]
                    if failure == "wrong":
                        body = b"changed!"
                    elif failure == "reordered":
                        body = bodies[valid["parts"][1]["url"]]
                    elif failure == "oversized":
                        body += b"!"
                    output.write_bytes(body)

                received = root / failure
                with (
                    self.subTest(failure=failure),
                    patch.dict(os.environ, {"GH_TOKEN": "fixture"}),
                    patch.object(transport, "download_artifact", side_effect=download),
                    patch.object(transport, "extract_bundle") as extract,
                ):
                    with self.assertRaises((FileNotFoundError, ValueError)):
                        transport.download_bundle(
                            descriptor, received, transport.REPOSITORY
                        )
                    extract.assert_not_called()
                    self.assertFalse((received / "candidate.tar.gz").exists())

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
