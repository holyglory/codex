#!/usr/bin/env python3
"""Stage verified npm tarballs, allowing an identical published bootstrap."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import urlopen

from verify_npm_release import PACKAGE_NAME, tarball_paths, verify_release


def registry_metadata(path: str) -> dict | None:
    # Read public metadata without transmitting the operator's npm credentials.
    try:
        with urlopen(f"https://registry.npmjs.org/{path}", timeout=30) as response:
            data = response.read(4 * 1024 * 1024 + 1)
    except HTTPError as error:
        if error.code == 404:
            return None
        raise RuntimeError(f"npm registry lookup failed: HTTP {error.code}") from error
    except (URLError, TimeoutError) as error:
        raise RuntimeError(
            "npm registry lookup unavailable; staging was not started"
        ) from error
    if len(data) > 4 * 1024 * 1024:
        raise RuntimeError("npm registry metadata exceeds the bounded response size")
    metadata = json.loads(data)
    if not isinstance(metadata, dict):
        raise RuntimeError("npm registry returned invalid metadata")
    return metadata


def tarball_integrity(path: Path) -> str:
    digest = hashlib.sha512()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha512-" + base64.b64encode(digest.digest()).decode("ascii")


def pending_tarballs(dist_dir: Path, version: str) -> list[tuple[str, Path]]:
    verify_release(dist_dir, version)
    package = quote(PACKAGE_NAME, safe="")
    tags = None
    pending = []
    # Preflight the entire set before staging anything. In particular, a late
    # root-version collision must not leave newly staged platform packages.
    for platform, path in tarball_paths(dist_dir, version).items():
        tag = "latest" if platform == "root" else platform
        package_version = version if platform == "root" else f"{version}-{platform}"
        published = registry_metadata(f"{package}/{quote(package_version, safe='')}")
        if published is None:
            pending.append((tag, path))
            continue
        dist = published.get("dist")
        if (
            published.get("name") != PACKAGE_NAME
            or published.get("version") != package_version
            or not isinstance(dist, dict)
            or dist.get("integrity") != tarball_integrity(path)
        ):
            raise RuntimeError(
                f"Published {PACKAGE_NAME}@{package_version} differs from candidate"
            )
        if tags is None:
            tags = registry_metadata(f"-/package/{package}/dist-tags")
        if tags is None or tags.get(tag) != package_version:
            raise RuntimeError(
                f"Published {PACKAGE_NAME}@{package_version} has an unexpected tag"
            )
    return pending


def stage_release(dist_dir: Path, version: str) -> int:
    pending = pending_tarballs(dist_dir, version)
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
        f"Staged {len(pending)} tarballs; existing versions were checksum- and tag-verified."
    )
    return len(pending)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist-dir", required=True, type=Path)
    args = parser.parse_args()
    stage_release(args.dist_dir.resolve(), args.version)


if __name__ == "__main__":
    main()
