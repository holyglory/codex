"""Digest-bound transport for local npm candidates; no packaging or execution."""

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import tarfile

from runner_handoff import download_artifact
from local_release_tar import BoundedTarInfo

MAX_ARCHIVE = 8 * 1024**3
MAX_ASSET = 2 * 1024**3  # GitHub requires each release asset to be strictly smaller.
PART_SIZE = 1024**3
MAX_PARTS = 8
MAX_EXPANDED = 16 * 1024**3
MAX_JSON = 1024**2
REPOSITORY = "holyglory/codex"


def file_record(path: Path) -> dict:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024**2), b""):
            digest.update(chunk)
    return {"sha256": digest.hexdigest(), "size_bytes": path.stat().st_size}


def read_json(path: Path, limit: int = MAX_JSON):
    with path.open("rb") as stream:
        content = stream.read(limit + 1)
    if len(content) > limit:
        raise ValueError(f"JSON evidence exceeds its limit: {path.name}")
    return json.loads(content)


def write_json(path: Path, value) -> None:
    content = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
    if len(content) > MAX_JSON:
        raise ValueError("JSON evidence exceeds 1 MiB")
    with path.open("xb") as stream:
        stream.write(content)


def checked_members(archive: tarfile.TarFile, *, native: bool = False):
    """Bound nested archives too, before existing release helpers inspect them."""
    names = set()
    total = 0
    result = []
    for index, member in enumerate(archive):
        if index >= (1024 if native else 64):
            raise ValueError("Candidate archive has too many members")
        name = member.name.removeprefix("./") if native else member.name
        if native and member.isdir():
            name = name.rstrip("/")
        parts = PurePosixPath(name).parts
        if native and member.isdir() and name in ("", "."):
            continue
        total += member.size + 512
        if (
            total > MAX_EXPANDED
            or member.size < 0
            or member.size > MAX_ARCHIVE
            or (name.endswith(".json") or name == "SHA256SUMS")
            and member.size > MAX_JSON
            or not parts
            or name != str(PurePosixPath(name))
            or PurePosixPath(name).is_absolute()
            or ".." in parts
            or "\\" in name
            or any(":" in part for part in parts)
            or name in names
            or not (member.isfile() or native and member.isdir())
        ):
            raise ValueError("Unsafe, duplicate, or oversized candidate archive member")
        names.add(name)
        result.append((name, member))
    return result


def extract_bundle(path: Path, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=False)
    with tarfile.open(path, "r:gz", tarinfo=BoundedTarInfo) as archive:
        for name, member in checked_members(archive):
            output = destination / name
            output.parent.mkdir(parents=True, exist_ok=True)
            with archive.extractfile(member) as source, output.open("xb") as target:
                shutil.copyfileobj(source, target, length=1024**2)


def validate_descriptor(value: dict, repository: str) -> dict:
    if repository != REPOSITORY or not isinstance(value, dict):
        raise ValueError("Candidate must belong to the configured owned repository")
    if (
        value.get("schema") != 1
        or value.get("kind") != "codex-local-npm-transport"
        or not valid_record(value, MAX_ARCHIVE)
        or not re.fullmatch(r"[0-9a-f]{64}", str(value.get("manifest_sha256", "")))
    ):
        raise ValueError("Invalid owned GitHub release-asset transport descriptor")
    if "parts" in value:
        if "url" in value:
            raise ValueError("Multipart transport cannot also name a single asset")
        validate_parts(value["parts"], value["size_bytes"])
        if any(not owned_asset_url(part.get("url")) for part in value["parts"]):
            raise ValueError("Every part needs an owned GitHub release-asset URL")
    elif not owned_asset_url(value.get("url")) or value["size_bytes"] >= MAX_ASSET:
        raise ValueError("A single owned GitHub release asset must be below 2 GiB")
    return value


def owned_asset_url(value) -> bool:
    return isinstance(value, str) and bool(
        re.fullmatch(
            rf"https://api\.github\.com/repos/{re.escape(REPOSITORY)}/releases/assets/[1-9][0-9]*",
            value,
        )
    )


def valid_record(value, limit: int) -> bool:
    return (
        isinstance(value, dict)
        and type(value.get("size_bytes")) is int
        and 0 < value["size_bytes"] <= limit
        and bool(re.fullmatch(r"[0-9a-f]{64}", str(value.get("sha256", ""))))
    )


def validate_parts(parts, archive_size: int) -> None:
    if not isinstance(parts, list) or not 1 <= len(parts) <= MAX_PARTS:
        raise ValueError("Multipart candidate requires one to eight ordered parts")
    if len(parts) != (archive_size + PART_SIZE - 1) // PART_SIZE:
        raise ValueError("Multipart candidate has missing or extra parts")
    for index, part in enumerate(parts, start=1):
        expected_size = min(PART_SIZE, archive_size - (index - 1) * PART_SIZE)
        if (
            not valid_record(part, PART_SIZE)
            or part.get("name") != f"candidate.tar.gz.part-{index:04d}"
            or part["size_bytes"] != expected_size
        ):
            raise ValueError("Invalid, oversized, missing, or reordered candidate part")


def append_part(path: Path, part: dict, digest, output=None) -> int:
    """Check a part while streaming its original bytes into the aggregate digest."""
    if not path.is_file() or path.is_symlink():
        raise ValueError("Candidate part must be an original regular file")
    observed, size = hashlib.sha256(), 0
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024**2), b""):
            size += len(chunk)
            if size > part["size_bytes"]:
                raise ValueError("Candidate part is larger than its receipt")
            observed.update(chunk)
            digest.update(chunk)
            if output is not None:
                output.write(chunk)
    if {"sha256": observed.hexdigest(), "size_bytes": size} != {
        key: part[key] for key in ("sha256", "size_bytes")
    }:
        raise ValueError("Candidate part SHA-256 or size differs")
    return size


def download_bundle(value: dict, destination: Path, repository: str) -> Path:
    validate_descriptor(value, repository)
    if not os.environ.get("GH_TOKEN"):
        raise ValueError("Authenticated GitHub download requires GH_TOKEN")
    destination.mkdir(parents=True, exist_ok=False)
    archive = destination / "candidate.tar.gz"
    if "parts" not in value:
        download_artifact(value, archive)
        return archive
    digest, total = hashlib.sha256(), 0
    try:
        with archive.open("xb") as output:
            for part in value["parts"]:
                path = destination / part["name"]
                try:
                    download_artifact(part, path)
                    total += append_part(path, part, digest, output)
                finally:
                    path.unlink(missing_ok=True)
        if {"sha256": digest.hexdigest(), "size_bytes": total} != {
            key: value[key] for key in ("sha256", "size_bytes")
        }:
            raise ValueError("Reassembled candidate archive SHA-256 or size differs")
    except BaseException:
        archive.unlink(missing_ok=True)
        raise
    return archive


def local_candidate(candidate: Path) -> tuple[dict, Path]:
    local = read_json(candidate)
    archive = candidate.parent / "candidate.tar.gz"
    manifest = candidate.parent / "candidate/npm-build-manifest.json"
    if (
        local.get("schema") != 1
        or local.get("kind") != "codex-local-npm-bundle"
        or local.get("npm_published") is not False
        or not valid_record(local.get("archive"), MAX_ARCHIVE)
        or file_record(archive) != local.get("archive")
        or file_record(manifest)["sha256"] != local.get("manifest_sha256")
    ):
        raise ValueError("Local candidate changed before transport registration")
    return local, archive


def create_descriptor(candidate: Path, url: str, output: Path) -> dict:
    local, _archive = local_candidate(candidate)
    value = {
        "schema": 1,
        "kind": "codex-local-npm-transport",
        "url": url,
        **local["archive"],
        "manifest_sha256": local["manifest_sha256"],
    }
    validate_descriptor(value, REPOSITORY)
    write_json(output, value)
    return value


def split_candidate(candidate: Path, output: Path) -> dict:
    local, archive = local_candidate(candidate)
    output.mkdir(parents=True, exist_ok=False)
    parts, digest, total = [], hashlib.sha256(), 0
    count = (local["archive"]["size_bytes"] + PART_SIZE - 1) // PART_SIZE
    with archive.open("rb") as source:
        for index in range(1, count + 1):
            name = f"candidate.tar.gz.part-{index:04d}"
            part_digest, size = hashlib.sha256(), 0
            with (output / name).open("xb") as destination:
                while size < PART_SIZE and (
                    chunk := source.read(min(1024**2, PART_SIZE - size))
                ):
                    size += len(chunk)
                    part_digest.update(chunk)
                    digest.update(chunk)
                    destination.write(chunk)
            total += size
            parts.append(
                {"name": name, "sha256": part_digest.hexdigest(), "size_bytes": size}
            )
        if source.read(1):
            raise ValueError("Candidate archive grew during splitting")
    if {"sha256": digest.hexdigest(), "size_bytes": total} != local["archive"]:
        raise ValueError("Candidate archive changed during splitting")
    validate_parts(parts, total)
    value = {
        "schema": 1,
        "kind": "codex-local-npm-parts",
        "archive": local["archive"],
        "manifest_sha256": local["manifest_sha256"],
        "parts": parts,
        "npm_published": False,
        **{key: local[key] for key in ("source_commit", "packaging_commit", "version")},
    }
    write_json(output / "parts-manifest.json", value)
    return value


def create_multipart_descriptor(manifest: Path, assets: Path, output: Path) -> dict:
    local, urls = read_json(manifest, 65536), read_json(assets, 65536)
    if (
        not isinstance(local, dict)
        or local.get("schema") != 1
        or local.get("kind") != "codex-local-npm-parts"
        or local.get("npm_published") is not False
        or not valid_record(local.get("archive"), MAX_ARCHIVE)
    ):
        raise ValueError("Invalid original candidate parts manifest")
    validate_parts(local.get("parts"), local["archive"]["size_bytes"])
    if not isinstance(urls, dict) or set(urls) != {
        part["name"] for part in local["parts"]
    }:
        raise ValueError("Asset URLs must map every original part name exactly once")
    value = {
        "schema": 1,
        "kind": "codex-local-npm-transport",
        **local["archive"],
        "manifest_sha256": local.get("manifest_sha256"),
        "parts": [{**part, "url": urls[part["name"]]} for part in local["parts"]],
    }
    validate_descriptor(value, REPOSITORY)
    digest, total = hashlib.sha256(), 0
    for part in value["parts"]:
        total += append_part(manifest.parent / part["name"], part, digest)
    if {"sha256": digest.hexdigest(), "size_bytes": total} != local["archive"]:
        raise ValueError("Original archive differs from its ordered parts")
    write_json(output, value)
    return value
