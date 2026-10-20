import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import prepare_runner_handoff as prepare


class PrepareRunnerHandoffTests(unittest.TestCase):
    def test_archive_and_manifest_are_commit_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            (source / "x86_64-unknown-linux-musl").mkdir()
            package = (
                source
                / "x86_64-unknown-linux-musl/codex-package-x86_64-unknown-linux-musl.tar.gz"
            )
            package.write_bytes(b"fixture")
            archive = root / "linux.tar.gz"
            prepare._archive_tree(source, archive)
            manifest = prepare._manifest(
                commit="a" * 40,
                producer="linux-local",
                checks=[{"name": "local", "exit_code": 0, "log_sha256": "b" * 64}],
                artifact=archive,
                targets=["x86_64-unknown-linux-musl"],
                repository="holyglory/codex",
                tag="codex-runner-handoff-" + "a" * 40,
            )
            self.assertEqual(
                manifest["artifacts"][0]["sha256"],
                hashlib.sha256(archive.read_bytes()).hexdigest(),
            )
            self.assertEqual(
                manifest["artifacts"][0]["size_bytes"], archive.stat().st_size
            )
            self.assertEqual(manifest["commit"], "a" * 40)

    def test_linux_source_requires_the_canonical_package(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "package"
            source.mkdir()
            (source / "source-commit.txt").write_text("a" * 40)
            with self.assertRaisesRegex(ValueError, "native-dist"):
                prepare._linux_bundle_source(
                    source, Path(directory) / "bundle", "a" * 40
                )

    def test_windows_bundle_excludes_private_build_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "controller"
            for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
                target_dir = source / target
                target_dir.mkdir(parents=True)
                (target_dir / f"codex-package-{target}.tar.gz").write_bytes(b"package")
                (target_dir / "build.log").write_text("private workspace path\n")
            destination = root / "bundle"
            prepare._find_windows_targets(source, destination)
            self.assertFalse(
                any(destination.glob("**/build.log")),
            )


if __name__ == "__main__":
    unittest.main()
