#!/usr/bin/env python3
"""Validate and import artifacts produced outside GitHub-hosted runners.

The release candidate workflow runs only a small verifier and the macOS lane on
GitHub. Linux and Windows producers publish a hash-bound manifest plus one or
more tar archives. This module keeps the handoff contract deliberately small:
the consumer verifies the frozen commit, producer identity, checks, URLs,
sizes, and digests before extracting anything into the npm artifact layout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import tarfile
from typing import Iterable
from urllib.parse import urlparse
from urllib.request import Request, urlopen


SCHEMA = 1
HEX64 = re.compile(r"^[0-9a-f]{64}$")
SHA40 = re.compile(r"^[0-9a-f]{40}$")
SAFE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
DEFAULT_ALLOWED_HOSTS = frozenset(
    {
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
        "raw.githubusercontent.com",
    }
)


class HandoffError(ValueError):
    """Raised when an external runner handoff is incomplete or unsafe."""


def _json_value(value: str | Path) -> dict:
    if isinstance(value, Path) or (isinstance(value, str) and value.startswith("@")):
        path = value if isinstance(value, Path) else Path(value[1:])
        try:
            value = path.read_text()
        except OSError as error:
            raise HandoffError(f"cannot read handoff manifest: {error}") from error
    try:
        manifest = json.loads(value)
    except (TypeError, json.JSONDecodeError) as error:
        raise HandoffError("handoff manifest is not valid JSON") from error
    if not isinstance(manifest, dict):
        raise HandoffError("handoff manifest must be a JSON object")
    return manifest


def _require_string(value: object, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise HandoffError(f"handoff field {field!r} must be a non-empty string")
    return value


def _allowed_hosts(extra: Iterable[str] = ()) -> frozenset[str]:
    hosts = set(DEFAULT_ALLOWED_HOSTS)
    hosts.update(host.strip().lower() for host in extra if host.strip())
    return frozenset(hosts)


def _validate_url(url: str, allowed_hosts: frozenset[str]) -> None:
    parsed = urlparse(url)
    if parsed.scheme != "https" or not parsed.hostname:
        raise HandoffError("handoff artifact URLs must use HTTPS")
    if parsed.hostname.lower() not in allowed_hosts:
        raise HandoffError(
            f"handoff artifact host is not allowlisted: {parsed.hostname.lower()}"
        )
    if parsed.username or parsed.password:
        raise HandoffError("handoff artifact URLs cannot contain credentials")


def _validate_artifact(artifact: object, allowed_hosts: frozenset[str]) -> tuple[str, set[str]]:
    if not isinstance(artifact, dict):
        raise HandoffError("each handoff artifact must be an object")
    name = _require_string(artifact.get("name"), "artifacts[].name")
    if not SAFE_NAME.fullmatch(name):
        raise HandoffError(f"unsafe handoff artifact name: {name!r}")
    url = _require_string(artifact.get("url"), "artifacts[].url")
    _validate_url(url, allowed_hosts)
    digest = _require_string(artifact.get("sha256"), "artifacts[].sha256")
    if not HEX64.fullmatch(digest):
        raise HandoffError(f"invalid handoff artifact digest for {name!r}")
    size = artifact.get("size_bytes")
    if type(size) is not int or size <= 0:
        raise HandoffError(f"handoff artifact {name!r} must declare a positive size")
    targets = artifact.get("targets")
    if not isinstance(targets, list) or not targets or any(
        not isinstance(target, str) or not target for target in targets
    ):
        raise HandoffError(f"handoff artifact {name!r} must declare targets")
    return name, set(targets)


def validate_checks(checks: object) -> list[dict]:
    """Validate producer check evidence without requiring artifact URLs."""

    if not isinstance(checks, list) or not checks:
        raise HandoffError("handoff must include at least one check result")
    check_names: set[str] = set()
    for check in checks:
        if not isinstance(check, dict):
            raise HandoffError("handoff check entries must be objects")
        name = _require_string(check.get("name"), "checks[].name")
        if name in check_names:
            raise HandoffError(f"duplicate handoff check: {name}")
        check_names.add(name)
        if type(check.get("exit_code")) is not int or check["exit_code"] != 0:
            raise HandoffError(f"handoff check failed: {name}")
        digest = _require_string(check.get("log_sha256"), "checks[].log_sha256")
        if not HEX64.fullmatch(digest):
            raise HandoffError(f"invalid handoff check digest: {name}")
    return checks


def validate(
    manifest: dict,
    commit: str,
    producer: str,
    required_targets: Iterable[str],
    *,
    allowed_hosts: Iterable[str] = (),
) -> dict:
    """Validate a complete producer manifest and return a normalized copy."""

    if not SHA40.fullmatch(commit):
        raise HandoffError("expected commit must be a full lowercase SHA-1")
    if manifest.get("schema") != SCHEMA:
        raise HandoffError(f"unsupported handoff schema: {manifest.get('schema')!r}")
    if manifest.get("commit") != commit:
        raise HandoffError("handoff commit does not match the candidate")
    if manifest.get("producer") != producer:
        raise HandoffError(f"handoff producer must be {producer!r}")
    if manifest.get("status") != "success":
        raise HandoffError("handoff producer did not report success")

    checks = validate_checks(manifest.get("checks"))

    hosts = _allowed_hosts(allowed_hosts)
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        raise HandoffError("handoff must include at least one artifact")
    artifact_names: set[str] = set()
    covered_targets: set[str] = set()
    for artifact in artifacts:
        name, targets = _validate_artifact(artifact, hosts)
        if name in artifact_names:
            raise HandoffError(f"duplicate handoff artifact: {name}")
        artifact_names.add(name)
        covered_targets.update(targets)

    required = set(required_targets)
    missing = sorted(required - covered_targets)
    if missing:
        raise HandoffError("handoff is missing targets: " + ", ".join(missing))

    return {
        "schema": SCHEMA,
        "commit": commit,
        "producer": producer,
        "status": "success",
        "checks": checks,
        "artifacts": artifacts,
    }


def _safe_members(archive: tarfile.TarFile) -> list[tarfile.TarInfo]:
    members = archive.getmembers()
    for member in members:
        path = Path(member.name)
        if path.is_absolute() or ".." in path.parts:
            raise HandoffError(f"handoff archive contains an unsafe path: {member.name}")
    return members


def _download_artifact(artifact: dict, destination: Path) -> None:
    expected_size = artifact["size_bytes"]
    expected_digest = artifact["sha256"]
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(destination.name + ".part")
    digest = hashlib.sha256()
    size = 0
    headers = {"User-Agent": "codex-runner-handoff/1"}
    token = os.environ.get("RUNNER_HANDOFF_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = Request(artifact["url"], headers=headers)
    try:
        with urlopen(request, timeout=120) as response, temporary.open("wb") as output:
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > expected_size:
                    raise HandoffError(f"handoff artifact is larger than declared: {artifact['name']}")
                digest.update(chunk)
                output.write(chunk)
        if size != expected_size or digest.hexdigest() != expected_digest:
            raise HandoffError(f"handoff artifact digest or size mismatch: {artifact['name']}")
        temporary.replace(destination)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise


def download(
    manifest: dict,
    destination: Path,
    commit: str,
    producer: str,
    required_targets: Iterable[str],
    *,
    allowed_hosts: Iterable[str] = (),
) -> dict:
    normalized = validate(
        manifest,
        commit,
        producer,
        required_targets,
        allowed_hosts=allowed_hosts,
    )
    destination.mkdir(parents=True, exist_ok=True)
    downloaded = []
    for artifact in normalized["artifacts"]:
        archive_path = destination / (artifact["name"] + ".download")
        _download_artifact(artifact, archive_path)
        try:
            with tarfile.open(archive_path, "r:*") as archive:
                members = _safe_members(archive)
                archive.extractall(destination, members=members, filter="data")
        finally:
            archive_path.unlink(missing_ok=True)
        downloaded.append(
            {
                "name": artifact["name"],
                "sha256": artifact["sha256"],
                "size_bytes": artifact["size_bytes"],
            }
        )
    return {"commit": commit, "producer": producer, "artifacts": downloaded}


def _parse_targets(value: str) -> list[str]:
    targets = [target for target in value.split(",") if target]
    if not targets:
        raise argparse.ArgumentTypeError("at least one target is required")
    return targets


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="action", required=True)
    for action in ("verify", "download"):
        subparser = subparsers.add_parser(action)
        subparser.add_argument("--manifest", required=True, help="JSON or @manifest.json")
        subparser.add_argument("--commit", required=True)
        subparser.add_argument("--producer", required=True)
        subparser.add_argument("--targets", required=True, type=_parse_targets)
        subparser.add_argument("--allowed-host", action="append", default=[])
        if action == "download":
            subparser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    try:
        manifest = _json_value(args.manifest)
        if args.action == "verify":
            normalized = validate(
                manifest,
                args.commit,
                args.producer,
                args.targets,
                allowed_hosts=args.allowed_host,
            )
            print(json.dumps({"status": "success", "targets": sorted({target for artifact in normalized["artifacts"] for target in artifact["targets"]})}))
        else:
            print(json.dumps(download(
                manifest,
                args.destination,
                args.commit,
                args.producer,
                args.targets,
                allowed_hosts=args.allowed_host,
            )))
    except (HandoffError, OSError, tarfile.TarError) as error:
        parser.exit(1, f"Runner handoff refused: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
