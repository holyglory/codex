#!/usr/bin/env python3
"""Stage the verified seven-package set with npm's human approval boundary."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import urlopen

try:
    from verify_npm_release import PACKAGE_NAME, tarball_paths, verify_release
except ModuleNotFoundError:
    from scripts.verify_npm_release import PACKAGE_NAME, tarball_paths, verify_release


def registry_metadata(suffix: str) -> dict | None:
    try:
        with urlopen(f"https://registry.npmjs.org/{suffix}", timeout=30) as response:
            content = response.read(4 * 1024**2 + 1)
    except HTTPError as error:
        if error.code == 404:
            return None
        raise
    if len(content) > 4 * 1024**2:
        raise ValueError("Registry metadata exceeds 4 MiB")
    value = json.loads(content)
    if not isinstance(value, dict):
        raise ValueError("Registry returned invalid package metadata")
    return value


def tarball_integrity(path: Path) -> str:
    digest = hashlib.sha512()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha512-" + base64.b64encode(digest.digest()).decode("ascii")


def published_state(dist_dir: Path, version: str) -> list[tuple[str, Path]]:
    verify_release(dist_dir, version)
    package = quote(PACKAGE_NAME, safe="")
    tags = registry_metadata(f"-/package/{package}/dist-tags")
    pending = []
    for platform, path in tarball_paths(dist_dir, version).items():
        tag = "latest" if platform == "root" else platform
        package_version = version if platform == "root" else f"{version}-{platform}"
        metadata = registry_metadata(f"{package}/{quote(package_version, safe='')}")
        if metadata is None:
            pending.append((tag, path))
            continue
        expected = {"name": PACKAGE_NAME, "version": package_version}
        if {key: metadata.get(key) for key in expected} != expected or metadata.get(
            "dist", {}
        ).get("integrity") != tarball_integrity(path):
            raise ValueError(
                f"Published {PACKAGE_NAME}@{package_version} differs from the immutable candidate"
            )
        if not tags or tags.get(tag) != package_version:
            raise ValueError(
                f"Published tag {tag} is stale or points to another version"
            )
    return pending


def stage_release(dist_dir: Path, version: str, *, public_only: bool = False) -> None:
    # Check every collision before staging the first package; stage platforms
    # before the root so the launcher cannot advertise an incomplete set.
    pending = published_state(dist_dir, version)
    if public_only:
        if pending:
            raise ValueError(
                f"Human-approved public release is incomplete: {[tag for tag, _ in pending]}"
            )
        print(
            "Public root, six tags, versions and tarball integrities match the candidate"
        )
        return
    for tag, path in pending:
        subprocess.run(
            [
                "npm",
                "stage",
                "publish",
                str(path),
                "--access",
                "public",
                "--provenance",
                "--tag",
                tag,
            ],
            check=True,
        )
    print(
        f"Staged {len(pending)} tarballs for npm human approval; publication is still separate"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist-dir", type=Path, required=True)
    parser.add_argument("--verify-public", action="store_true")
    args = parser.parse_args()
    stage_release(args.dist_dir.resolve(), args.version, public_only=args.verify_public)


if __name__ == "__main__":
    main()
