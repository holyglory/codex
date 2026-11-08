#!/usr/bin/env python3
"""Smoke local tarball installation with a manually installed platform alias.

This offline check exercises launcher resolution; public-registry installation
remains a separate post-approval release gate.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile

from verify_npm_release import PLATFORMS, tarball_paths, verify_release


def smoke_install(dist_dir: Path, version: str, *, fixture: bool = False) -> None:
    verify_release(dist_dir, version)
    system = {"Linux": "linux", "Darwin": "darwin", "Windows": "win32"}[
        platform.system()
    ]
    arch = {"x86_64": "x64", "AMD64": "x64", "aarch64": "arm64", "arm64": "arm64"}[
        platform.machine()
    ]
    tag = f"{system}-{arch}"
    with tempfile.TemporaryDirectory(prefix="codex-npm-install-") as temp:
        root = Path(temp)
        prefix = root / "prefix"
        env = {
            **os.environ,
            "NPM_CONFIG_CACHE": str(root / "cache"),
            "NPM_CONFIG_USERCONFIG": str(root / "npmrc"),
        }
        subprocess.run(
            [
                "npm",
                "install",
                "--global",
                "--prefix",
                str(prefix),
                "--offline",
                "--ignore-scripts",
                "--omit=optional",
                "--no-audit",
                "--no-fund",
                str(tarball_paths(dist_dir, version)["root"]),
            ],
            check=True,
            capture_output=True,
            env=env,
        )
        modules = prefix / ("node_modules" if system == "win32" else "lib/node_modules")
        native = modules / PLATFORMS[tag]["npm_name"]
        native.mkdir(parents=True)
        with tarfile.open(tarball_paths(dist_dir, version)[tag]) as archive:
            for member in archive.getmembers():
                member.name = member.name.removeprefix("package/")
                archive.extract(member, native, filter="data")
        launcher = modules / "@holyglory/codex/bin/codex.js"
        for arg in ("--version", "--help"):
            result = subprocess.run(
                ["node", str(launcher), arg],
                check=True,
                capture_output=True,
                text=True,
                env=env,
            )
            if fixture:
                observed = json.loads(result.stdout)
                if observed != {
                    "args": [arg],
                    "manager": "1",
                    "root": str(launcher.parents[1]),
                }:
                    raise ValueError(
                        f"Launcher fixture did not preserve arguments and npm ownership: {observed}"
                    )
            elif (
                arg == "--version"
                and result.stdout.strip()
                != f"codex-cli {version.replace('-multi.', '+multi.', 1)}"
            ):
                raise ValueError(
                    f"Installed CLI reported the wrong version: {result.stdout!r}"
                )
        missing = native.with_name(native.name + "-missing")
        native.rename(missing)
        result = subprocess.run(
            ["node", str(launcher), "--version"],
            capture_output=True,
            text=True,
            env=env,
        )
        if (
            result.returncode == 0
            or "npm install -g @holyglory/codex@latest" not in result.stderr
        ):
            raise ValueError("Missing payload did not give the fork reinstall command")
    print("Isolated npm launcher installation passed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist-dir", type=Path, required=True)
    args = parser.parse_args()
    smoke_install(args.dist_dir.resolve(), args.version)


if __name__ == "__main__":
    main()
