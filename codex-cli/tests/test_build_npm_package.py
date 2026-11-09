"""Check the contents of npm archives staged from canonical release packages."""

import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "codex-cli" / "scripts"))

from build_npm_package import CODEX_PLATFORM_PACKAGES, stage_sources  # noqa: E402

sys.path.insert(0, str(REPO_ROOT / "scripts"))
from check_npm_install import smoke_install  # noqa: E402
from verify_npm_release import tarball_paths, verify_release  # noqa: E402


class NpmPackageTest(unittest.TestCase):
    def test_downstream_readme_uses_fork_install_command(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            staging = Path(temp)
            stage_sources(staging, "0.166.0-multi.2", "codex")
            readme = (staging / "README.md").read_text(encoding="utf-8")
            self.assertIn("npm install -g @holyglory/codex", readme)
            self.assertNotIn("@openai/codex", readme)

    def test_downloaded_archives_build_seven_packages_and_launch_in_isolation(
        self,
    ) -> None:
        version = "0.166.0-multi.2"
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            artifacts = root / "artifacts"
            fixture = b"#!/usr/bin/env node\nconsole.log(JSON.stringify({args:process.argv.slice(2),manager:process.env.CODEX_MANAGED_BY_NPM,root:process.env.CODEX_MANAGED_PACKAGE_ROOT}));\n"
            for platform in CODEX_PLATFORM_PACKAGES.values():
                target = platform["target_triple"]
                suffix = ".exe" if platform["os"] == "win32" else ""
                metadata = {
                    "layoutVersion": 1,
                    "variant": "codex",
                    "version": "0.166.0+multi.2",
                    "target": target,
                    "entrypoint": f"bin/codex{suffix}",
                    "execServer": f"bin/codex-exec-server{suffix}",
                    "resourcesDir": "codex-resources",
                    "pathDir": "codex-path",
                }
                files = [
                    metadata["entrypoint"],
                    metadata["execServer"],
                    f"bin/codex-code-mode-host{suffix}",
                    f"codex-path/rg{suffix}",
                ]
                if platform["os"] == "linux":
                    files.append("codex-resources/bwrap")
                elif platform["os"] == "win32":
                    files.extend(
                        [
                            "codex-resources/codex-command-runner.exe",
                            "codex-resources/codex-windows-sandbox-setup.exe",
                        ]
                    )
                artifact = artifacts / target
                artifact.mkdir(parents=True)
                with tarfile.open(
                    artifact / f"codex-package-{target}.tar.gz", "w:gz"
                ) as archive:
                    for name, content in [
                        ("codex-package.json", json.dumps(metadata).encode()),
                        *((name, fixture) for name in files),
                    ]:
                        member = tarfile.TarInfo(name)
                        member.mode = 0o755 if name in files else 0o644
                        member.size = len(content)
                        archive.addfile(member, io.BytesIO(content))
            dist = root / "npm"
            subprocess.run(
                [
                    sys.executable,
                    str(REPO_ROOT / "scripts/stage_npm_packages.py"),
                    "--release-version",
                    version,
                    "--artifacts-dir",
                    str(artifacts),
                    "--output-dir",
                    str(dist),
                    "--package",
                    "codex",
                ],
                check=True,
                capture_output=True,
            )
            verify_release(dist, version)
            smoke_install(dist, version, fixture=True)
            self.assertEqual(len(list(dist.glob("*.tgz"))), 7)
            # A package can have a plausible version and still lose a helper.
            # Exercise the verifier at the actual archive boundary.
            payload = tarball_paths(dist, version)["win32-arm64"]
            with tarfile.open(payload) as archive:
                entries = [
                    (member, archive.extractfile(member).read())
                    for member in archive.getmembers()
                ]
            with tarfile.open(payload, "w:gz") as archive:
                for member, content in entries:
                    if not member.name.endswith("codex-command-runner.exe"):
                        archive.addfile(member, io.BytesIO(content))
            with self.assertRaises(KeyError):
                verify_release(dist, version)

    def test_npm_build_metadata_is_rejected_before_packing(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(ValueError, "build metadata"):
                stage_sources(Path(temp), "0.166.0+multi.2", "codex")

    def test_npm_archives_preserve_cli_runtime_without_standalone_executor(
        self,
    ) -> None:
        # Use an installed executable as prebuilt input: packaging must preserve its
        # bytes even when staging a package for another operating system.
        executable = shutil.which("whoami") or sys.executable
        for package, platform in CODEX_PLATFORM_PACKAGES.items():
            with self.subTest(package=package), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                target = platform["target_triple"]
                source = root / "vendor" / target
                suffix = ".exe" if platform["os"] == "win32" else ""
                metadata = {
                    "layoutVersion": 1,
                    "variant": "codex",
                    "version": "1.2.3",
                    "target": target,
                    "entrypoint": f"bin/codex{suffix}",
                    "execServer": f"bin/codex-exec-server{suffix}",
                    "resourcesDir": "codex-resources",
                    "pathDir": "codex-path",
                }
                files = [
                    metadata["entrypoint"],
                    metadata["execServer"],
                    f"bin/codex-code-mode-host{suffix}",
                    f"codex-path/rg{suffix}",
                ]
                if platform["os"] == "linux":
                    files.append("codex-resources/bwrap")
                elif platform["os"] == "win32":
                    files.extend(
                        [
                            "codex-resources/codex-command-runner.exe",
                            "codex-resources/codex-windows-sandbox-setup.exe",
                        ]
                    )
                for name in files:
                    destination = source / name
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(executable, destination)
                (source / "codex-package.json").write_text(json.dumps(metadata))
                original = {
                    path.relative_to(source).as_posix(): path.read_bytes()
                    for path in source.rglob("*")
                    if path.is_file()
                }
                archive_path = root / "package.tgz"
                subprocess.run(
                    [
                        sys.executable,
                        str(
                            REPO_ROOT / "codex-cli" / "scripts" / "build_npm_package.py"
                        ),
                        "--package",
                        package,
                        "--version",
                        "1.2.3",
                        "--vendor-src",
                        str(source.parent),
                        "--staging-dir",
                        str(root / "staged"),
                        "--pack-output",
                        str(archive_path),
                    ],
                    check=True,
                    capture_output=True,
                )
                with tarfile.open(archive_path, "r:gz") as archive:
                    prefix = f"package/vendor/{target}/"
                    actual = {
                        member.name.removeprefix(prefix): archive.extractfile(
                            member
                        ).read()
                        for member in archive.getmembers()
                        if member.isfile() and member.name.startswith(prefix)
                    }
                expected = original.copy()
                metadata = json.loads(expected.pop("codex-package.json"))
                expected.pop(metadata.pop("execServer"))
                self.assertEqual(json.loads(actual.pop("codex-package.json")), metadata)
                self.assertEqual(actual, expected)
                self.assertEqual(
                    {
                        path.relative_to(source).as_posix(): path.read_bytes()
                        for path in source.rglob("*")
                        if path.is_file()
                    },
                    original,
                )


if __name__ == "__main__":
    unittest.main()
