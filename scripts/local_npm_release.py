#!/usr/bin/env python3
"""Assemble npm locally, or verify the exact candidate in GitHub without repacking."""

import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tomllib

import release_identity as release
from check_npm_install import smoke_install
from local_npm_transport import (
    MAX_ARCHIVE,
    BoundedTarInfo,
    REPOSITORY,
    checked_members,
    create_descriptor,
    create_multipart_descriptor,
    download_bundle,
    extract_bundle,
    file_record,
    read_json,
    split_candidate,
    validate_descriptor,
    write_json,
)
from verify_npm_release import PLATFORMS, tarball_paths, verify_release

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = "npm-build-manifest.json"


def verify_identity(source: str, packaging: str, tag: str | None = None) -> str:
    if any(not re.fullmatch(r"[0-9a-f]{40}", value) for value in (source, packaging)):
        raise ValueError("Source and packaging commits must be full lowercase SHA-1s")
    if release.git("rev-parse", "HEAD") != packaging or release.git(
        "status", "--porcelain", "--untracked-files=normal"
    ):
        raise ValueError(
            "Packaging checkout must be clean at the exact packaging commit"
        )
    release.verify_native_source(source, packaging)
    rust_version, npm_version = release.version()
    product_version = tomllib.loads(
        release.git("show", f"{source}:codex-rs/Cargo.toml")
    )["workspace"]["package"]["version"]
    if product_version != rust_version:
        raise ValueError("Product and packaging versions differ")
    release.verify_new_downstream_version(rust_version, source)
    if tag is not None and (
        tag != f"npm-v{npm_version}"
        or release.git("cat-file", "-t", f"refs/tags/{tag}") != "tag"
        or release.git("rev-parse", f"refs/tags/{tag}^{{commit}}") != source
    ):
        raise ValueError(
            "Release tag must annotate the exact product source and version"
        )
    return npm_version


def verify_acceptance_files(directory: Path, source: str) -> None:
    acceptance = read_json(directory / "local-acceptance.json")
    release.verify_acceptance(acceptance, source)
    receipt = read_json(directory / "delivery-receipt.json")
    if (
        not isinstance(receipt, dict)
        or receipt.get("qualified") is not True
        or any(
            receipt.get(key) != acceptance[key]
            for key in ("receipt_id", "run_id", "source_sha256", "manifest_sha256")
        )
    ):
        raise ValueError(
            "Local acceptance needs its retained qualified delivery receipt"
        )


def native_evidence_files(directory: Path) -> set[Path]:
    paths = set()
    for config in PLATFORMS.values():
        folder = directory / config["target_triple"]
        receipt_path = folder / "native-manifest.json"
        receipt = read_json(receipt_path)
        paths.add(receipt_path)
        for name in receipt["archives"]:
            if not isinstance(name, str) or Path(name).name != name or "\\" in name:
                raise ValueError("Invalid native receipt archive name")
            paths.add(folder / name)
        sidecar = folder / "producer-inputs.json"
        if sidecar.exists():
            read_json(sidecar, 256 * 1024)
            paths.add(sidecar)
    if any(not path.is_file() or path.is_symlink() for path in paths):
        raise ValueError("Native evidence must contain original regular files")
    return paths


def archive_records(path: Path) -> dict:
    with tarfile.open(path, "r:gz", tarinfo=BoundedTarInfo) as archive:
        result = {}
        for name, member in checked_members(archive, native=True):
            if not member.isfile():
                continue
            with archive.extractfile(member) as stream:
                if name.endswith("codex-package.json"):
                    if member.size > 65536:
                        raise ValueError("Oversized native package metadata")
                    content = json.load(stream)
                    content.pop("execServer", None)
                    digest = hashlib.sha256(
                        json.dumps(content, sort_keys=True).encode()
                    ).hexdigest()
                else:
                    digest = hashlib.file_digest(stream, "sha256").hexdigest()
            result[name] = {"sha256": digest, "executable": bool(member.mode & 0o111)}
        return result


def verify_payload_bytes(directory: Path, version: str) -> None:
    for tag, config in PLATFORMS.items():
        target = config["target_triple"]
        native = archive_records(
            directory / "native-artifacts" / target / f"codex-package-{target}.tar.gz"
        )
        native = {
            name: record
            for name, record in native.items()
            if name not in ("bin/codex-exec-server", "bin/codex-exec-server.exe")
        }
        prefix = f"package/vendor/{target}/"
        packaged = {
            name.removeprefix(prefix): record
            for name, record in archive_records(
                tarball_paths(directory, version)[tag]
            ).items()
            if name.startswith(prefix)
        }
        if native != packaged:
            raise ValueError(
                f"npm payload differs from its verified native archive: {target}"
            )
    with tarfile.open(tarball_paths(directory, version)["root"], "r:gz") as archive:
        launcher = archive.extractfile("package/bin/codex.js").read()
    if launcher != (ROOT / "codex-cli/bin/codex.js").read_bytes():
        raise ValueError("npm launcher differs from the exact packaging source")


def check_candidate(directory: Path, source: str, packaging: str, version: str) -> list:
    verify_acceptance_files(directory, source)
    receipts = release.verify_artifacts(directory / "native-artifacts", packaging)
    if len(receipts) != len(PLATFORMS) or any(
        item["source_commit"] != source for item in receipts
    ):
        raise ValueError("Candidate requires all six exact-product native receipts")
    for path in tarball_paths(directory, version).values():
        with tarfile.open(path, "r:gz", tarinfo=BoundedTarInfo) as archive:
            for name, member in checked_members(archive, native=True):
                if not name.startswith("package/vendor/") and member.size > 1024**2:
                    raise ValueError("Oversized npm metadata or launcher")
    verify_release(directory, version)
    verify_payload_bytes(directory, version)
    smoke_install(directory, version)
    return receipts


def assemble_candidate(
    source: str,
    packaging: str,
    artifacts: Path,
    acceptance: Path,
    delivery: Path,
    output: Path,
) -> dict:
    version = verify_identity(source, packaging)
    # All six native receipts and bytes must pass before npm pack is permitted.
    receipts = release.verify_artifacts(artifacts, packaging)
    if len(receipts) != len(PLATFORMS) or any(
        item["source_commit"] != source for item in receipts
    ):
        raise ValueError("Assembly requires all six exact-product native receipts")
    evidence = native_evidence_files(artifacts)
    output.mkdir(parents=True, exist_ok=False)
    candidate = output / "candidate"
    candidate.mkdir()
    shutil.copyfile(acceptance, candidate / "local-acceptance.json")
    shutil.copyfile(delivery, candidate / "delivery-receipt.json")
    verify_acceptance_files(candidate, source)
    for path in sorted(evidence):
        target = candidate / "native-artifacts" / path.relative_to(artifacts)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts/stage_npm_packages.py"),
            "--release-version",
            version,
            "--artifacts-dir",
            str(artifacts),
            "--output-dir",
            str(candidate),
            "--package",
            "codex",
        ],
        check=True,
        cwd=ROOT,
    )
    checked = check_candidate(candidate, source, packaging, version)
    if checked != receipts:
        raise ValueError("Native inputs changed during local assembly")
    manifest = {
        "schema": 1,
        "kind": "codex-local-npm-candidate",
        "source_commit": source,
        "packaging_commit": packaging,
        "version": version,
        "files": {
            path.name: file_record(path)
            for path in tarball_paths(candidate, version).values()
        },
        "native_receipts": receipts,
        "status": "assembled",
        "npm_published": False,
    }
    write_json(candidate / MANIFEST, manifest)
    write_json(candidate / "native-receipts.json", receipts)
    write_json(
        candidate / "local-checks.json",
        {
            "schema": 1,
            "status": "passed",
            "source_commit": source,
            "packaging_commit": packaging,
            "version": version,
            "host": {"system": platform.system(), "machine": platform.machine()},
            "checks": [
                "native-receipts",
                "seven-tarballs",
                "native-payload-bytes",
                "npm-install-version-help",
            ],
        },
    )
    (candidate / "SHA256SUMS").write_text(
        "".join(
            f"{record['sha256']}  {name}\n"
            for name, record in sorted(manifest["files"].items())
        )
    )
    archive = output / "candidate.tar.gz"
    if (
        sum(path.stat().st_size for path in candidate.rglob("*") if path.is_file())
        > MAX_ARCHIVE - 1024**2
    ):
        raise ValueError("Candidate inputs exceed the 8 GiB transport budget")
    # Most members are already compressed native/npm archives.
    with tarfile.open(archive, "x:gz", compresslevel=1) as stream:
        for path in sorted(candidate.rglob("*")):
            if path.is_file():
                stream.add(
                    path,
                    arcname=path.relative_to(candidate).as_posix(),
                    recursive=False,
                )
    if archive.stat().st_size > MAX_ARCHIVE:
        raise ValueError("Candidate archive exceeds the 8 GiB transport limit")
    value = {
        "schema": 1,
        "kind": "codex-local-npm-bundle",
        "archive_path": str(archive.resolve()),
        "archive": file_record(archive),
        "manifest_sha256": file_record(candidate / MANIFEST)["sha256"],
        "source_commit": source,
        "packaging_commit": packaging,
        "version": version,
        "npm_published": False,
    }
    write_json(output / "local-candidate.json", value)
    return value


def verify_bundle(
    archive: Path,
    descriptor: dict,
    source: str,
    packaging: str,
    tag: str,
    output: Path,
    *,
    repository: str = REPOSITORY,
) -> dict:
    validate_descriptor(descriptor, repository)
    version = verify_identity(source, packaging, tag)
    if file_record(archive) != {
        key: descriptor[key] for key in ("sha256", "size_bytes")
    }:
        raise ValueError("Candidate archive SHA-256 or size differs")
    extract_bundle(archive, output)
    if file_record(output / MANIFEST)["sha256"] != descriptor["manifest_sha256"]:
        raise ValueError("Candidate manifest SHA-256 differs")
    manifest = read_json(output / MANIFEST)
    expected = {
        "schema": 1,
        "kind": "codex-local-npm-candidate",
        "source_commit": source,
        "packaging_commit": packaging,
        "version": version,
        "status": "assembled",
        "npm_published": False,
    }
    files = {
        path.name: file_record(path) for path in tarball_paths(output, version).values()
    }
    if {key: manifest.get(key) for key in expected} != expected or manifest.get(
        "files"
    ) != files:
        raise ValueError("Candidate identity or original tarball bytes differ")
    retained = native_evidence_files(output / "native-artifacts")
    expected_files = (
        retained
        | set(tarball_paths(output, version).values())
        | {
            output / name
            for name in (
                MANIFEST,
                "native-receipts.json",
                "local-acceptance.json",
                "delivery-receipt.json",
                "local-checks.json",
                "SHA256SUMS",
            )
        }
    )
    if {path for path in output.rglob("*") if path.is_file()} != expected_files:
        raise ValueError("Candidate contains missing or unexpected evidence files")
    receipts = check_candidate(output, source, packaging, version)
    if {
        path.name: file_record(path) for path in tarball_paths(output, version).values()
    } != files:
        raise ValueError("Candidate tarballs changed during verification")
    if (
        manifest.get("native_receipts") != receipts
        or read_json(output / "native-receipts.json") != receipts
    ):
        raise ValueError("Retained native receipts differ from freshly verified inputs")
    expected_sums = "".join(
        f"{record['sha256']}  {name}\n" for name, record in sorted(files.items())
    )
    if (output / "SHA256SUMS").read_text() != expected_sums:
        raise ValueError("Candidate checksum list differs from its original tarballs")
    checks = read_json(output / "local-checks.json")
    if checks.get("status") != "passed" or any(
        checks.get(key) != expected[key]
        for key in ("source_commit", "packaging_commit", "version")
    ):
        raise ValueError("Local npm acceptance belongs to another candidate")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    assemble = commands.add_parser("assemble")
    verify = commands.add_parser("verify")
    for command in (assemble, verify):
        command.add_argument("--source-commit", required=True)
        command.add_argument("--packaging-commit", required=True)
        command.add_argument("--output-dir", type=Path, required=True)
    assemble.add_argument("--artifacts-dir", type=Path, required=True)
    assemble.add_argument("--local-acceptance", type=Path)
    assemble.add_argument("--delivery-receipt", type=Path, required=True)
    verify.add_argument("--descriptor", type=Path, required=True)
    verify.add_argument("--tag", required=True)
    verify.add_argument("--repository", default=REPOSITORY)
    split = commands.add_parser("split")
    split.add_argument("--candidate", type=Path, required=True)
    split.add_argument("--output-dir", type=Path, required=True)
    descriptor = commands.add_parser("descriptor")
    original = descriptor.add_mutually_exclusive_group(required=True)
    original.add_argument("--candidate", type=Path)
    original.add_argument("--parts-manifest", type=Path)
    descriptor.add_argument("--asset-url")
    descriptor.add_argument("--assets-json", type=Path)
    descriptor.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "assemble":
        artifacts = args.artifacts_dir.resolve()
        value = assemble_candidate(
            args.source_commit,
            args.packaging_commit,
            artifacts,
            args.local_acceptance or artifacts / "local-acceptance.json",
            args.delivery_receipt,
            args.output_dir.resolve(),
        )
    elif args.command == "split":
        value = split_candidate(args.candidate.resolve(), args.output_dir.resolve())
    elif args.command == "descriptor":
        if args.parts_manifest and args.assets_json and not args.asset_url:
            value = create_multipart_descriptor(
                args.parts_manifest.resolve(),
                args.assets_json.resolve(),
                args.output.resolve(),
            )
        elif args.candidate and args.asset_url and not args.assets_json:
            value = create_descriptor(
                args.candidate.resolve(), args.asset_url, args.output.resolve()
            )
        else:
            parser.error(
                "Use --candidate with --asset-url, or --parts-manifest with --assets-json"
            )
    else:
        value = read_json(args.descriptor, 65536)
        archive = download_bundle(value, args.output_dir.resolve(), args.repository)
        value = verify_bundle(
            archive,
            value,
            args.source_commit,
            args.packaging_commit,
            args.tag,
            args.output_dir.resolve() / "candidate",
            repository=args.repository,
        )
    print(json.dumps(value, sort_keys=True))


if __name__ == "__main__":
    main()
