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
        or not re.fullmatch(
            rf"https://api\.github\.com/repos/{re.escape(repository)}/releases/assets/[1-9][0-9]*",
            str(value.get("url", "")),
        )
        or type(value.get("size_bytes")) is not int
        or not 0 < value["size_bytes"] <= MAX_ARCHIVE
        or any(
            not re.fullmatch(r"[0-9a-f]{64}", str(value.get(key, "")))
            for key in ("sha256", "manifest_sha256")
        )
    ):
        raise ValueError("Invalid owned GitHub release-asset transport descriptor")
    return value


def download_bundle(value: dict, destination: Path, repository: str) -> Path:
    validate_descriptor(value, repository)
    if not os.environ.get("GH_TOKEN"):
        raise ValueError("Authenticated GitHub download requires GH_TOKEN")
    destination.mkdir(parents=True, exist_ok=False)
    archive = destination / "candidate.tar.gz"
    download_artifact(value, archive)
    return archive


def create_descriptor(candidate: Path, url: str, output: Path) -> dict:
    local = read_json(candidate)
    archive = candidate.parent / "candidate.tar.gz"
    manifest = candidate.parent / "candidate/npm-build-manifest.json"
    if (
        local.get("schema") != 1
        or local.get("kind") != "codex-local-npm-bundle"
        or local.get("npm_published") is not False
        or file_record(archive) != local.get("archive")
        or file_record(manifest)["sha256"] != local.get("manifest_sha256")
    ):
        raise ValueError("Local candidate changed before transport registration")
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
