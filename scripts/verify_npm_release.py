#!/usr/bin/env python3
"""Verify the complete @holyglory/codex npm release tarball set."""

import argparse
import json
from pathlib import Path
import tarfile


PACKAGE_NAME = "@holyglory/codex"
PLATFORMS = {
    "linux-x64": ("x86_64-unknown-linux-musl", "linux", "x64"),
    "linux-arm64": ("aarch64-unknown-linux-musl", "linux", "arm64"),
    "darwin-x64": ("x86_64-apple-darwin", "darwin", "x64"),
    "darwin-arm64": ("aarch64-apple-darwin", "darwin", "arm64"),
    "win32-x64": ("x86_64-pc-windows-msvc", "win32", "x64"),
    "win32-arm64": ("aarch64-pc-windows-msvc", "win32", "arm64"),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist-dir", required=True, type=Path)
    return parser.parse_args()


def tarball_paths(dist_dir: Path, version: str) -> dict[str, Path]:
    paths = {
        platform: dist_dir / f"codex-npm-{platform}-{version}.tgz"
        for platform in PLATFORMS
    }
    paths["root"] = dist_dir / f"codex-npm-{version}.tgz"
    return paths


def read_tarball(path: Path) -> tuple[dict, set[str]]:
    if not path.is_file():
        raise RuntimeError(f"Missing npm release tarball: {path}")

    with tarfile.open(path, "r:gz") as archive:
        members = {member.name for member in archive.getmembers()}
        package_json = archive.extractfile("package/package.json")
        if package_json is None:
            raise RuntimeError(f"Tarball has no package/package.json: {path}")
        metadata = json.load(package_json)
    return metadata, members


def verify_release(dist_dir: Path, version: str) -> None:
    if "+" in version:
        raise RuntimeError(
            "npm release version contains build metadata that npm would strip: "
            f"{version}"
        )

    paths = tarball_paths(dist_dir, version)
    root, root_members = read_tarball(paths["root"])
    expected_dependencies = {
        f"@holyglory/codex-{platform}": (f"npm:{PACKAGE_NAME}@{version}-{platform}")
        for platform in PLATFORMS
    }
    expected_root = {
        "name": PACKAGE_NAME,
        "version": version,
        "bin": {"codex": "bin/codex.js"},
        "repository": {
            "type": "git",
            "url": "git+https://github.com/holyglory/codex.git",
            "directory": "codex-cli",
        },
        "publishConfig": {
            "access": "public",
            "registry": "https://registry.npmjs.org/",
        },
        "optionalDependencies": expected_dependencies,
    }
    actual_root = {key: root.get(key) for key in expected_root}
    if actual_root != expected_root:
        raise RuntimeError(
            "Root npm metadata does not match the downstream release contract:\n"
            f"expected={expected_root!r}\nactual={actual_root!r}"
        )
    require_redistribution_files(paths["root"], root_members)
    if "package/bin/codex.js" not in root_members:
        raise RuntimeError(f"Root npm tarball has no launcher: {paths['root']}")

    for platform, (target, operating_system, cpu) in PLATFORMS.items():
        metadata, members = read_tarball(paths[platform])
        expected = {
            "name": PACKAGE_NAME,
            "version": f"{version}-{platform}",
            "os": [operating_system],
            "cpu": [cpu],
            "publishConfig": {
                "access": "public",
                "registry": "https://registry.npmjs.org/",
            },
        }
        actual = {key: metadata.get(key) for key in expected}
        if actual != expected:
            raise RuntimeError(
                f"{platform} npm metadata does not match the release contract:\n"
                f"expected={expected!r}\nactual={actual!r}"
            )
        require_redistribution_files(paths[platform], members)
        executable = "codex.exe" if operating_system == "win32" else "codex"
        expected_binary = f"package/vendor/{target}/bin/{executable}"
        if expected_binary not in members:
            raise RuntimeError(
                f"{platform} npm tarball has no expected binary {expected_binary}: "
                f"{paths[platform]}"
            )


def require_redistribution_files(path: Path, members: set[str]) -> None:
    required = {"package/LICENSE", "package/NOTICE"}
    missing = required - members
    if missing:
        raise RuntimeError(f"Tarball {path} is missing {sorted(missing)}")


def main() -> int:
    args = parse_args()
    verify_release(args.dist_dir.resolve(), args.version)
    print(f"Verified complete {PACKAGE_NAME}@{args.version} npm release")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
