#!/usr/bin/env python3

import base64
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from urllib.parse import urlparse
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
import bootstrap_bazel_distdir as bootstrap


class BootstrapBazelDistdirTest(unittest.TestCase):
    def setUp(self) -> None:
        self.specs = [
            (
                "rules_cc",
                "0.2.20",
                "bazelbuild",
                "rules_cc",
                "0.2.20",
                "rules_cc-0.2.20.tar.gz",
            ),
            (
                "bazel_features",
                "1.50.0",
                "bazel-contrib",
                "bazel_features",
                "v1.50.0",
                "bazel_features-v1.50.0.tar.gz",
            ),
            (
                "bazel_skylib",
                "1.9.0",
                "bazelbuild",
                "bazel-skylib",
                "1.9.0",
                "bazel-skylib-1.9.0.tar.gz",
            ),
        ]
        self.payloads = {
            item[5]: f"verified bytes for {item[5]}".encode() for item in self.specs
        }
        self.manifests = {}
        for module, version, owner, repo, tag, filename in self.specs:
            url = (
                f"https://github.com/{owner}/{repo}/releases/download/{tag}/{filename}"
            )
            integrity = (
                "sha256-"
                + base64.b64encode(
                    hashlib.sha256(self.payloads[filename]).digest()
                ).decode()
            )
            manifest_url = (
                f"https://bcr.bazel.build/modules/{module}/{version}/source.json"
            )
            self.manifests[manifest_url] = json.dumps(
                {"url": url, "integrity": integrity}
            ).encode()

    def write_lock(self, workspace: Path) -> None:
        workspace.joinpath("MODULE.bazel.lock").write_text(
            json.dumps(
                {
                    "registryFileHashes": {
                        url: hashlib.sha256(manifest).hexdigest()
                        for url, manifest in self.manifests.items()
                    }
                }
            ),
            encoding="utf-8",
        )

    def fake_source_fetch(self, url: str, *, limit: int) -> bytes:
        return self.manifests[url]

    def test_fallback_stages_exact_bytes_and_common_distdir_hint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            distdir = Path(directory) / "distdir"
            workspace.mkdir()
            self.write_lock(workspace)

            def fetch(url: str, *, limit: int) -> bytes:
                if url in self.manifests:
                    return self.manifests[url]
                raise bootstrap.FetchError(504)

            def api_asset(spec: dict[str, str], token: str | None) -> bytes:
                self.assertEqual(token, "read-only-token")
                return self.payloads[spec["filename"]]

            with (
                patch.object(bootstrap, "fetch_with_retry", side_effect=fetch),
                patch.object(bootstrap, "api_asset_bytes", side_effect=api_asset),
            ):
                bootstrap.bootstrap(workspace, distdir, "read-only-token")

            for filename, payload in self.payloads.items():
                self.assertEqual(
                    workspace.joinpath("user.bazelrc").read_text().count("--distdir="),
                    1,
                )
                self.assertEqual(distdir.joinpath(filename).read_bytes(), payload)
            self.assertIn(
                f"common --distdir={distdir}",
                workspace.joinpath("user.bazelrc").read_text(),
            )

    def test_source_manifest_hash_mismatch_fails_before_archive_fetch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            self.write_lock(workspace)
            corrupt = dict(self.manifests)
            first = next(iter(corrupt))
            corrupt[first] = b"{}"
            with (
                patch.object(
                    bootstrap,
                    "fetch_with_retry",
                    side_effect=lambda url, *, limit: corrupt[url],
                ),
                patch.object(bootstrap, "api_asset_bytes") as api_asset,
            ):
                with self.assertRaisesRegex(
                    bootstrap.BootstrapError, "manifest hash mismatch"
                ):
                    bootstrap.bootstrap(workspace, Path(directory) / "distdir")
            api_asset.assert_not_called()

    def test_unsupported_source_url_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            self.write_lock(workspace)
            bad = json.loads(next(iter(self.manifests.values())))
            bad["url"] = (
                "https://github.com/example/project/releases/download/v1/archive.tar.gz?download=1"
            )
            bad_bytes = json.dumps(bad).encode()
            first = next(iter(self.manifests))
            lock = json.loads(workspace.joinpath("MODULE.bazel.lock").read_text())
            lock["registryFileHashes"][first] = hashlib.sha256(bad_bytes).hexdigest()
            workspace.joinpath("MODULE.bazel.lock").write_text(json.dumps(lock))
            manifests = dict(self.manifests)
            manifests[first] = bad_bytes
            with patch.object(
                bootstrap,
                "fetch_with_retry",
                side_effect=lambda url, *, limit: manifests[url],
            ):
                with self.assertRaisesRegex(
                    bootstrap.BootstrapError, "unsupported GitHub release archive URL"
                ):
                    bootstrap.bootstrap(workspace, Path(directory) / "distdir")

    def test_github_archive_basename_collision_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            manifests = dict(self.manifests)
            urls = sorted(manifests)
            first = json.loads(manifests[urls[0]])
            second = json.loads(manifests[urls[1]])
            second["url"] = first["url"]
            manifests[urls[1]] = json.dumps(second).encode()
            lock = {
                "registryFileHashes": {
                    url: hashlib.sha256(value).hexdigest()
                    for url, value in manifests.items()
                }
            }
            workspace.joinpath("MODULE.bazel.lock").write_text(json.dumps(lock))
            with patch.object(
                bootstrap,
                "fetch_with_retry",
                side_effect=lambda url, *, limit: manifests[url],
            ):
                with self.assertRaisesRegex(
                    bootstrap.BootstrapError, "basename collision"
                ):
                    bootstrap.bootstrap(workspace, Path(directory) / "distdir")

    def test_non_github_source_is_left_for_bazel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            distdir = Path(directory) / "distdir"
            workspace.mkdir()
            manifests = dict(self.manifests)
            first = next(iter(manifests))
            non_github = json.loads(manifests[first])
            non_github["url"] = "https://example.invalid/archive.tar.gz"
            manifests[first] = json.dumps(non_github).encode()
            workspace.joinpath("MODULE.bazel.lock").write_text(
                json.dumps(
                    {
                        "registryFileHashes": {
                            url: hashlib.sha256(value).hexdigest()
                            for url, value in manifests.items()
                        }
                    }
                )
            )

            def fetch(url: str, *, limit: int) -> bytes:
                if url in manifests:
                    return manifests[url]
                return self.payloads[Path(urlparse(url).path).name]

            with (
                patch.object(bootstrap, "fetch_with_retry", side_effect=fetch),
                patch.object(
                    bootstrap,
                    "api_asset_bytes",
                    side_effect=lambda spec, token: self.payloads[spec["filename"]],
                ),
            ):
                bootstrap.bootstrap(workspace, distdir)
            self.assertFalse(
                any("archive.tar.gz" in path.name for path in distdir.iterdir())
            )

    def test_modified_existing_archive_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            distdir = Path(directory) / "distdir"
            workspace.mkdir()
            self.write_lock(workspace)
            distdir.mkdir()
            filename = self.specs[0][5]
            distdir.joinpath(filename).write_bytes(b"modified")

            def fetch(url: str, *, limit: int) -> bytes:
                if url in self.manifests:
                    return self.manifests[url]
                return self.payloads[Path(urlparse(url).path).name]

            with patch.object(bootstrap, "fetch_with_retry", side_effect=fetch):
                with self.assertRaisesRegex(
                    bootstrap.BootstrapError, "existing distdir archive hash mismatch"
                ):
                    bootstrap.bootstrap(workspace, distdir)

    def test_api_asset_identity_mismatch_is_rejected(self) -> None:
        spec = {
            "owner": "bazelbuild",
            "repo": "rules_cc",
            "tag": "0.2.20",
            "filename": "rules_cc-0.2.20.tar.gz",
            "url": "https://github.com/bazelbuild/rules_cc/releases/download/0.2.20/rules_cc-0.2.20.tar.gz",
        }
        metadata = json.dumps(
            {
                "assets": [
                    {
                        "id": 1,
                        "name": "other.tar.gz",
                        "browser_download_url": spec["url"],
                    }
                ]
            }
        ).encode()
        with patch.object(
            bootstrap,
            "fetch_api_metadata",
            return_value=(
                metadata,
                "https://api.github.com/repos/bazelbuild/rules_cc/releases/tags/0.2.20",
            ),
        ):
            with self.assertRaisesRegex(
                bootstrap.BootstrapError, "asset identity mismatch"
            ):
                bootstrap.api_asset_bytes(spec, None)

    def test_api_metadata_follows_one_same_host_redirect_only(self) -> None:
        class Response:
            def __init__(self, status: int, headers: dict[str, str], body: bytes = b""):
                self.status = status
                self.headers = headers
                self._body = body

            def read(self, limit: int) -> bytes:
                return self._body

        class Opener:
            def __init__(self, location: str):
                self.location = location
                self.requests = []

            def open(self, request, timeout: int):
                self.requests.append(request)
                if len(self.requests) == 1:
                    return Response(301, {"Location": self.location})
                return Response(
                    200,
                    {},
                    b'{"url":"https://api.github.com/repos/new/repo/releases/1"}',
                )

        opener = Opener("https://api.github.com/repositories/42/releases/tags/v1")
        with patch.object(bootstrap, "NO_REDIRECT_OPENER", opener):
            body, final_url = bootstrap.fetch_api_metadata(
                "https://api.github.com/repos/old/repo/releases/tags/v1",
                {"Authorization": "Bearer secret"},
            )
        self.assertIn(b"new/repo", body)
        self.assertEqual(
            final_url, "https://api.github.com/repositories/42/releases/tags/v1"
        )
        self.assertTrue(
            all(request.headers.get("Authorization") for request in opener.requests)
        )

        cross_host = Opener("https://github.com/new/repo/releases/tags/v1")
        with patch.object(bootstrap, "NO_REDIRECT_OPENER", cross_host):
            with self.assertRaisesRegex(
                bootstrap.BootstrapError, "API redirect host mismatch"
            ):
                bootstrap.fetch_api_metadata(
                    "https://api.github.com/repos/old/repo/releases/tags/v1",
                    {"Authorization": "Bearer secret"},
                )

    def test_api_metadata_move_keeps_exact_tag_name_and_unauthed_cdn_fetch(
        self,
    ) -> None:
        spec = {
            "owner": "aspect-build",
            "repo": "tools_telemetry",
            "tag": "v0.3.3",
            "filename": "tools_telemetry-v0.3.3.tar.gz",
            "url": "https://github.com/aspect-build/tools_telemetry/releases/download/v0.3.3/tools_telemetry-v0.3.3.tar.gz",
        }
        moved_url = "https://github.com/aspect-archives/tools_telemetry/releases/download/v0.3.3/tools_telemetry-v0.3.3.tar.gz"
        metadata = json.dumps(
            {
                "url": "https://api.github.com/repos/aspect-archives/tools_telemetry/releases/1",
                "tag_name": "v0.3.3",
                "assets": [
                    {
                        "id": 7,
                        "name": spec["filename"],
                        "browser_download_url": moved_url,
                    }
                ],
            }
        ).encode()

        class Response:
            status = 302
            headers = {
                "Location": "https://release-assets.githubusercontent.com/signed"
            }

        class Opener:
            def open(self, request, timeout: int):
                self.request = request
                return Response()

        opener = Opener()
        with (
            patch.object(
                bootstrap,
                "fetch_api_metadata",
                return_value=(
                    metadata,
                    "https://api.github.com/repositories/1009790369/releases/tags/v0.3.3",
                ),
            ),
            patch.object(bootstrap, "NO_REDIRECT_OPENER", opener),
            patch.object(bootstrap, "fetch_url", return_value=b"archive") as fetch,
        ):
            self.assertEqual(bootstrap.api_asset_bytes(spec, "secret"), b"archive")
        self.assertIn("Authorization", opener.request.headers)
        fetch.assert_called_once_with(
            "https://release-assets.githubusercontent.com/signed",
            limit=bootstrap.ARCHIVE_MAX_BYTES,
        )

    def test_url_guards_reject_insecure_or_credentialed_release_sources(self) -> None:
        for url in (
            "http://github.com/bazelbuild/rules_cc/releases/download/0.2.20/rules_cc-0.2.20.tar.gz",
            "https://token@github.com/bazelbuild/rules_cc/releases/download/0.2.20/rules_cc-0.2.20.tar.gz",
            "https://github.com:443/bazelbuild/rules_cc/releases/download/0.2.20/rules_cc-0.2.20.tar.gz",
        ):
            with (
                self.subTest(url=url),
                self.assertRaisesRegex(
                    bootstrap.BootstrapError, "unsupported GitHub release archive URL"
                ),
            ):
                bootstrap.canonical_release_parts(url)
        for location in (
            "http://release-assets.githubusercontent.com/signed",
            "https://token@release-assets.githubusercontent.com/signed",
            "https://release-assets.githubusercontent.com:443/signed",
        ):
            with (
                self.subTest(location=location),
                self.assertRaisesRegex(
                    bootstrap.BootstrapError, "redirect host mismatch"
                ),
            ):
                bootstrap.validate_signed_redirect(location)

    def test_distdir_path_with_whitespace_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            self.write_lock(workspace)
            with self.assertRaisesRegex(bootstrap.BootstrapError, "whitespace"):
                bootstrap.bootstrap(workspace, Path(directory) / "dist dir")

    def test_preexisting_unrelated_override_is_preserved_and_refused(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            self.write_lock(workspace)
            override = workspace / "user.bazelrc"
            original = "common --jobs=1\n"
            override.write_text(original)
            with patch.object(
                bootstrap, "fetch_with_retry", side_effect=self.fake_source_fetch
            ):
                with self.assertRaisesRegex(
                    bootstrap.BootstrapError, "unrelated pre-existing"
                ):
                    bootstrap.bootstrap(workspace, Path(directory) / "distdir")
            self.assertEqual(override.read_text(), original)


if __name__ == "__main__":
    unittest.main()
