#!/usr/bin/env python3
"""Check all seven downstream npm tarballs before staging or installation."""

import argparse
import json
from pathlib import Path
import sys
import tarfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "codex-cli" / "scripts"))
from build_npm_package import CODEX_NPM_NAME, CODEX_PLATFORM_PACKAGES

PACKAGE_NAME = CODEX_NPM_NAME
PLATFORMS = {value["npm_tag"]: value for value in CODEX_PLATFORM_PACKAGES.values()}
PUBLISH_CONFIG = {"access": "public", "registry": "https://registry.npmjs.org/"}


def tarball_paths(dist_dir: Path, version: str) -> dict[str, Path]:
    return {
        **{tag: dist_dir / f"codex-npm-{tag}-{version}.tgz" for tag in PLATFORMS},
        "root": dist_dir / f"codex-npm-{version}.tgz",
    }


def read_json(archive: tarfile.TarFile, name: str) -> dict:
    member = archive.getmember(name)
    if not member.isfile() or member.size > 65536:
        raise ValueError(f"Invalid package metadata: {name}")
    with archive.extractfile(member) as source:
        value = json.load(source)
    if not isinstance(value, dict):
        raise ValueError(f"Package metadata is not an object: {name}")
    return value


def verify_release(dist_dir: Path, version: str) -> None:
    if "+" in version:
        raise ValueError("npm versions must not contain + build metadata")
    paths = tarball_paths(dist_dir, version)
    if set(dist_dir.glob("*.tgz")) != set(paths.values()):
        raise ValueError("Expected exactly the root launcher and six platform tarballs")
    for tag, path in paths.items():
        with tarfile.open(path, "r:gz") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            if len(set(names)) != len(names) or any(
                not member.isfile()
                or not member.name.startswith("package/")
                or ".." in Path(member.name).parts
                for member in members
            ):
                raise ValueError(f"Unexpected archive members in {path.name}")
            metadata = read_json(archive, "package/package.json")
            expected = {
                "name": PACKAGE_NAME,
                "version": version if tag == "root" else f"{version}-{tag}",
                "publishConfig": PUBLISH_CONFIG,
            }
            if {key: metadata.get(key) for key in expected} != expected:
                raise ValueError(f"Incorrect downstream identity in {path.name}")
            for name in ("LICENSE", "NOTICE"):
                member = archive.getmember(f"package/{name}")
                if (
                    member.size > 65536
                    or archive.extractfile(member).read()
                    != (Path(__file__).resolve().parents[1] / name).read_bytes()
                ):
                    raise ValueError(f"Redistribution file differs: {path.name}/{name}")
            if tag == "root":
                expected_dependencies = {
                    platform["npm_name"]: f"npm:{PACKAGE_NAME}@{version}-{platform_tag}"
                    for platform_tag, platform in PLATFORMS.items()
                }
                if metadata.get("optionalDependencies") != expected_dependencies:
                    raise ValueError(
                        "Root optional dependencies must pin all six fork payloads"
                    )
                if metadata.get("bin") != {"codex": "bin/codex.js"}:
                    raise ValueError("Root package must expose the codex launcher")
                launcher = archive.extractfile("package/bin/codex.js").read()
                if b"@openai/codex" in launcher:
                    raise ValueError(
                        "Launcher still points to the upstream npm package"
                    )
                continue
            platform = PLATFORMS[tag]
            if metadata.get("os") != [platform["os"]] or metadata.get("cpu") != [
                platform["cpu"]
            ]:
                raise ValueError(f"Incorrect platform constraints in {path.name}")
            target = platform["target_triple"]
            prefix = f"package/vendor/{target}/"
            suffix = ".exe" if platform["os"] == "win32" else ""
            required = [
                f"bin/codex{suffix}",
                f"bin/codex-code-mode-host{suffix}",
                f"codex-path/rg{suffix}",
            ]
            if platform["os"] == "linux":
                required.append("codex-resources/bwrap")
            elif platform["os"] == "win32":
                required.extend(
                    [
                        "codex-resources/codex-command-runner.exe",
                        "codex-resources/codex-windows-sandbox-setup.exe",
                    ]
                )
            for name in required:
                member = archive.getmember(prefix + name)
                if not member.size or (not suffix and not member.mode & 0o111):
                    raise ValueError(f"Missing executable content: {path.name}/{name}")
            native = read_json(archive, prefix + "codex-package.json")
            native_expected = {
                "layoutVersion": 1,
                "variant": "codex",
                "target": target,
                "version": version.replace("-multi.", "+multi.", 1),
                "entrypoint": f"bin/codex{suffix}",
                "resourcesDir": "codex-resources",
                "pathDir": "codex-path",
            }
            if {
                key: native.get(key) for key in native_expected
            } != native_expected or "execServer" in native:
                raise ValueError(f"Native package metadata differs in {path.name}")
            if any("codex-exec-server" in name for name in names):
                raise ValueError("npm packages must omit the standalone exec server")
            if any(
                name.startswith("package/vendor/") and not name.startswith(prefix)
                for name in names
            ):
                raise ValueError(f"Unexpected platform files in {path.name}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist-dir", type=Path, required=True)
    args = parser.parse_args()
    verify_release(args.dist_dir.resolve(), args.version)
    print(f"Verified seven {PACKAGE_NAME}@{args.version} tarballs")


if __name__ == "__main__":
    main()
