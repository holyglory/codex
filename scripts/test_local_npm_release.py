"""Fixture end-to-end checks; real native acceptance remains a release gate."""

from contextlib import ExitStack
import io
import json
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import local_npm_release as candidate
import local_npm_transport as transport
from check_npm_install import smoke_install
from verify_npm_release import PLATFORMS, tarball_paths

SOURCE, PACKAGING = "a" * 40, "b" * 40
VERSION = "1.2.3-multi.99"
IDENTITY = {
    "source_commit": SOURCE,
    "source_tree": "c" * 40,
    "native_tree": "d" * 40,
    "v8_tree": "e" * 40,
}


def identities():
    stack = ExitStack()
    for obj, name, result in (
        (candidate, "verify_identity", VERSION),
        (candidate.release, "identity", IDENTITY),
        (candidate.release, "verify_native_source", {"native_producer": IDENTITY}),
        (
            candidate.release,
            "version",
            (VERSION.replace("-multi.", "+multi."), VERSION),
        ),
        (candidate.release, "git", IDENTITY["v8_tree"]),
        (candidate.release, "v8_input_manifest", {"sha256": "f" * 64}),
    ):
        stack.enter_context(patch.object(obj, name, return_value=result))
    return stack


def tar_bytes(path, files):
    with tarfile.open(path, "w:gz") as archive:
        for name, content, mode in files:
            member = tarfile.TarInfo(name)
            member.size, member.mode = len(content), mode
            archive.addfile(member, io.BytesIO(content))


@unittest.skipUnless(
    os.name == "posix" and shutil.which("npm") and shutil.which("node"),
    "requires POSIX npm and Node executable fixtures",
)
class LocalNpmCandidateTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.temp.cleanup)
        cls.root = Path(cls.temp.name)
        artifacts = cls.root / "native"
        fixture = b"#!/usr/bin/env node\nconsole.log(JSON.stringify({args:process.argv.slice(2),manager:process.env.CODEX_MANAGED_BY_NPM,root:process.env.CODEX_MANAGED_PACKAGE_ROOT}));\n"
        for config in PLATFORMS.values():
            target, suffix = (
                config["target_triple"],
                ".exe" if config["os"] == "win32" else "",
            )
            folder = artifacts / target
            folder.mkdir(parents=True)
            metadata = {
                "layoutVersion": 1,
                "variant": "codex",
                "version": VERSION.replace("-multi.", "+multi."),
                "target": target,
                "entrypoint": f"bin/codex{suffix}",
                "execServer": f"bin/codex-exec-server{suffix}",
                "resourcesDir": "codex-resources",
                "pathDir": "codex-path",
            }
            names = [
                metadata["entrypoint"],
                metadata["execServer"],
                f"bin/codex-code-mode-host{suffix}",
                f"codex-path/rg{suffix}",
            ]
            if config["os"] == "linux":
                names.append("codex-resources/bwrap")
            elif config["os"] == "win32":
                names.extend(
                    [
                        "codex-resources/codex-command-runner.exe",
                        "codex-resources/codex-windows-sandbox-setup.exe",
                    ]
                )
            archive = folder / f"codex-package-{target}.tar.gz"
            tar_bytes(
                archive,
                [
                    ("codex-package.json", json.dumps(metadata).encode(), 0o644),
                    *((name, fixture, 0o755) for name in names),
                ],
            )
            pair_name = (
                f"rusty_v8_ptrcomp_sandbox_release_{target}.lib.gz"
                if suffix
                else f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz"
            )
            receipt = {
                "schema": 1,
                "status": "success",
                **IDENTITY,
                "target": target,
                "version": metadata["version"],
                "runtime_executed": target != "aarch64-pc-windows-msvc",
                "runtime_not_executed_reason": "cross-compilation fixture",
                "checks": [{"exit_code": 0, "log_sha256": "1" * 64}],
                "archives": {archive.name: transport.file_record(archive)},
                "v8_pair": {
                    "source_commit": SOURCE,
                    "source_built": True,
                    "profile": "ptrcomp_sandbox_release",
                    "cppgc_ignored_tests_executed": True,
                    "files": {
                        pair_name: "2" * 64,
                        f"src_binding_ptrcomp_sandbox_release_{target}.rs": "3" * 64,
                    },
                },
            }
            transport.write_json(folder / "native-manifest.json", receipt)
        acceptance = {
            "schema": 1,
            "status": "success",
            **IDENTITY,
            "proof": "complete",
            "tier": "release",
            "receipt_id": "retained-fixture",
            "run_id": "fixture-run",
            "source_sha256": "4" * 64,
            "manifest_sha256": "5" * 64,
        }
        transport.write_json(artifacts / "local-acceptance.json", acceptance)
        delivery = {
            key: acceptance[key]
            for key in ("receipt_id", "run_id", "source_sha256", "manifest_sha256")
        }
        delivery["qualified"] = True
        transport.write_json(cls.root / "delivery.json", delivery)
        cls.bundle = cls.root / "assembled"
        # These executables are explicit Node fixtures, never real native proof.
        with (
            identities(),
            patch.object(
                candidate,
                "smoke_install",
                side_effect=lambda directory, version: smoke_install(
                    directory, version, fixture=True
                ),
            ),
        ):
            candidate.assemble_candidate(
                SOURCE,
                PACKAGING,
                artifacts,
                artifacts / "local-acceptance.json",
                cls.root / "delivery.json",
                cls.bundle,
            )
        cls.descriptor = transport.create_descriptor(
            cls.bundle / "local-candidate.json",
            "https://api.github.com/repos/holyglory/codex/releases/assets/123",
            cls.root / "transport.json",
        )

    def setUp(self):
        self.work = tempfile.TemporaryDirectory()
        self.addCleanup(self.work.cleanup)
        self.output = Path(self.work.name)

    def verify(self, archive=None, descriptor=None):
        with (
            identities(),
            patch.object(candidate, "smoke_install") as smoke,
            patch.object(candidate.subprocess, "run") as process,
        ):
            result = candidate.verify_bundle(
                archive or self.bundle / "candidate.tar.gz",
                descriptor or self.descriptor,
                SOURCE,
                PACKAGING,
                f"npm-v{VERSION}",
                self.output / "verified",
            )
            smoke.assert_called_once()
            process.assert_not_called()  # GitHub verification cannot compile or npm pack.
        return result

    def altered_bundle(self, change):
        directory = self.output / "altered"
        shutil.copytree(self.bundle / "candidate", directory)
        change(directory)
        archive = self.output / "altered.tar.gz"
        with tarfile.open(archive, "w:gz") as stream:
            for path in sorted(directory.rglob("*")):
                if path.is_file():
                    stream.add(
                        path,
                        arcname=path.relative_to(directory).as_posix(),
                        recursive=False,
                    )
        return archive, {
            **self.descriptor,
            **transport.file_record(archive),
            "manifest_sha256": transport.file_record(directory / candidate.MANIFEST)[
                "sha256"
            ],
        }

    def test_assembly_and_github_verification_preserve_seven_original_tarballs(self):
        manifest = self.verify()
        original = transport.read_json(self.bundle / "candidate" / candidate.MANIFEST)
        self.assertEqual(manifest, original)
        self.assertEqual(len(manifest["files"]), 7)
        self.assertEqual(len(manifest["native_receipts"]), 6)
        self.assertEqual(manifest["npm_published"], False)
        self.assertEqual(
            {
                path.name: transport.file_record(path)
                for path in tarball_paths(self.output / "verified", VERSION).values()
            },
            manifest["files"],
        )

    def test_two_part_transport_retains_original_archive_and_seven_tarballs(self):
        archive = self.bundle / "candidate.tar.gz"
        parts_dir = self.output / "parts"
        with patch.object(transport, "PART_SIZE", (archive.stat().st_size + 1) // 2):
            parts = transport.split_candidate(
                self.bundle / "local-candidate.json", parts_dir
            )
            self.assertEqual(len(parts["parts"]), 2)
            urls = {
                part[
                    "name"
                ]: f"https://api.github.com/repos/holyglory/codex/releases/assets/{number}"
                for number, part in enumerate(parts["parts"], start=1)
            }
            transport.write_json(self.output / "assets.json", urls)
            descriptor = transport.create_multipart_descriptor(
                parts_dir / "parts-manifest.json",
                self.output / "assets.json",
                self.output / "transport.json",
            )
            files = {
                urls[part["name"]]: parts_dir / part["name"] for part in parts["parts"]
            }
            with (
                patch.dict(os.environ, {"GH_TOKEN": "fixture"}),
                patch.object(
                    transport,
                    "download_artifact",
                    side_effect=lambda part, path: shutil.copyfile(
                        files[part["url"]], path
                    ),
                ),
            ):
                downloaded = transport.download_bundle(
                    descriptor, self.output / "download", transport.REPOSITORY
                )
            self.assertEqual(
                transport.file_record(downloaded), transport.file_record(archive)
            )
            verified = self.verify(downloaded, descriptor)
        self.assertEqual(
            verified,
            transport.read_json(self.bundle / "candidate" / candidate.MANIFEST),
        )

    def test_archive_or_manifest_digest_change_is_rejected(self):
        for key in ("sha256", "manifest_sha256"):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "SHA-256"):
                self.verify(descriptor={**self.descriptor, key: "0" * 64})

    def test_resealed_wrong_product_source_is_rejected(self):
        def change(directory):
            path = directory / candidate.MANIFEST
            value = transport.read_json(path)
            value["source_commit"] = "9" * 40
            path.write_text(json.dumps(value))

        with self.assertRaisesRegex(ValueError, "identity"):
            self.verify(*self.altered_bundle(change))

    def test_resealed_modified_native_payload_is_rejected(self):
        def change(directory):
            path = tarball_paths(directory, VERSION)["linux-x64"]
            with tarfile.open(path) as archive:
                files = [
                    (member.name, archive.extractfile(member).read(), member.mode)
                    for member in archive
                ]
            tar_bytes(
                path,
                [
                    (name, b"tampered" if name.endswith("/bin/codex") else data, mode)
                    for name, data, mode in files
                ],
            )
            manifest = directory / candidate.MANIFEST
            value = transport.read_json(manifest)
            value["files"][path.name] = transport.file_record(path)
            manifest.write_text(json.dumps(value))

        with self.assertRaisesRegex(ValueError, "payload differs"):
            self.verify(*self.altered_bundle(change))

    def test_missing_platform_and_changed_native_archive_fail_before_packing(self):
        for mutation in ("missing", "changed"):
            artifacts = self.output / mutation
            shutil.copytree(self.root / "native", artifacts)
            archive = next(artifacts.glob("*/codex-package-*.tar.gz"))
            archive.unlink() if mutation == "missing" else archive.write_bytes(
                b"changed native bytes"
            )
            with identities(), patch.object(candidate.subprocess, "run") as process:
                with self.assertRaises((FileNotFoundError, ValueError)):
                    candidate.assemble_candidate(
                        SOURCE,
                        PACKAGING,
                        artifacts,
                        artifacts / "local-acceptance.json",
                        self.root / "delivery.json",
                        self.output / (mutation + "-output"),
                    )
                process.assert_not_called()

    def test_delivery_receipt_must_be_qualified_and_match_all_retained_links(self):
        for field in (
            "qualified",
            "receipt_id",
            "run_id",
            "source_sha256",
            "manifest_sha256",
        ):
            receipt = transport.read_json(
                self.bundle / "candidate/delivery-receipt.json"
            )
            receipt[field] = False if field == "qualified" else "different"
            directory = self.output / field
            directory.mkdir()
            shutil.copyfile(
                self.root / "native/local-acceptance.json",
                directory / "local-acceptance.json",
            )
            transport.write_json(directory / "delivery-receipt.json", receipt)
            with (
                identities(),
                self.assertRaisesRegex(ValueError, "qualified delivery receipt"),
            ):
                candidate.verify_acceptance_files(directory, SOURCE)


class LocalNpmIdentityTest(unittest.TestCase):
    def test_identity_requires_clean_exact_checkout_and_annotated_product_tag(self):
        tag = f"npm-v{VERSION}"
        responses = {
            ("rev-parse", "HEAD"): PACKAGING,
            ("status", "--porcelain", "--untracked-files=normal"): "",
            (
                "show",
                f"{SOURCE}:codex-rs/Cargo.toml",
            ): '[workspace.package]\nversion="1.2.3+multi.99"',
            ("cat-file", "-t", f"refs/tags/{tag}"): "tag",
            ("rev-parse", f"refs/tags/{tag}^{{commit}}"): SOURCE,
        }
        with (
            patch.object(
                candidate.release, "git", side_effect=lambda *args: responses[args]
            ),
            patch.object(candidate.release, "verify_native_source"),
            patch.object(
                candidate.release, "version", return_value=("1.2.3+multi.99", VERSION)
            ),
            patch.object(candidate.release, "verify_new_downstream_version"),
        ):
            self.assertEqual(candidate.verify_identity(SOURCE, PACKAGING, tag), VERSION)
            for key, changed in (
                (("rev-parse", "HEAD"), SOURCE),
                (
                    ("status", "--porcelain", "--untracked-files=normal"),
                    " M product.rs",
                ),
                (("cat-file", "-t", f"refs/tags/{tag}"), "commit"),
                (("rev-parse", f"refs/tags/{tag}^{{commit}}"), PACKAGING),
            ):
                original = responses[key]
                responses[key] = changed
                with self.subTest(key=key), self.assertRaises(ValueError):
                    candidate.verify_identity(SOURCE, PACKAGING, tag)
                responses[key] = original


if __name__ == "__main__":
    unittest.main()
