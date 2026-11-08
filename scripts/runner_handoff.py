#!/usr/bin/env python3
"""Verify commit-bound handoffs from the VPS and private Windows controller."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import tarfile
from urllib.parse import urlparse
from urllib.request import HTTPRedirectHandler, Request, build_opener

try:
    from release_identity import identity, verify_native_source
except ModuleNotFoundError:
    from scripts.release_identity import identity, verify_native_source

SCHEMA = 1
HOSTS = frozenset(
    {
        "github.com",
        "api.github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
    }
)
HEX64 = re.compile(r"[0-9a-f]{64}")


def validate_checks(checks: object) -> list[dict]:
    if not isinstance(checks, list) or not checks:
        raise ValueError("Handoff needs retained successful check receipts")
    names = set()
    for check in checks:
        if (
            not isinstance(check, dict)
            or not isinstance(check.get("name"), str)
            or not check["name"]
            or check["name"] in names
        ):
            raise ValueError("Handoff checks need unique names")
        names.add(check["name"])
        if (
            type(check.get("exit_code")) is not int
            or check["exit_code"] != 0
            or not HEX64.fullmatch(str(check.get("log_sha256", "")))
        ):
            raise ValueError(
                f"Handoff check is not successful and hash-bound: {check['name']}"
            )
    return checks


def validate(
    manifest: dict, commit: str, producer: str, required_targets: list[str]
) -> dict:
    native_commit = manifest.get("source_commit", manifest.get("commit"))
    producer_commits = manifest.get("producer_commits", [native_commit])
    if (
        not isinstance(producer_commits, list)
        or not producer_commits
        or any(not isinstance(value, str) for value in producer_commits)
    ):
        raise ValueError("Handoff must identify every native producer commit")
    native_identities = [identity(value) for value in dict.fromkeys(producer_commits)]
    for value in producer_commits:
        verify_native_source(value, commit)
    if (
        manifest.get("schema") != SCHEMA
        or manifest.get("producer") != producer
        or manifest.get("status") != "success"
    ):
        raise ValueError("Handoff producer identity or status differs")
    if (
        manifest.get("commit") != commit
        or manifest.get("native_tree") != native_identities[0]["native_tree"]
    ):
        raise ValueError("Handoff native source identity differs")
    if any(
        value["native_tree"] != manifest["native_tree"] for value in native_identities
    ):
        raise ValueError("Native producers do not share the same codex-rs tree")
    validate_checks(manifest.get("checks"))
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        raise ValueError("Handoff has no native artifacts")
    covered = set()
    names = set()
    for artifact in artifacts:
        name = artifact.get("name", "")
        parsed = urlparse(artifact.get("url", ""))
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name) or name in names:
            raise ValueError("Invalid or duplicate artifact name")
        names.add(name)
        if (
            parsed.scheme != "https"
            or parsed.hostname not in HOSTS
            or parsed.username
            or parsed.password
            or parsed.port not in (None, 443)
        ):
            raise ValueError(
                "Handoff must use the existing GitHub HTTPS artifact boundary"
            )
        size = artifact.get("size_bytes")
        if (
            type(size) is not int
            or not 0 < size <= 8 * 1024**3
            or not HEX64.fullmatch(str(artifact.get("sha256", "")))
        ):
            raise ValueError("Handoff artifact has invalid digest or size")
        targets = artifact.get("targets")
        if (
            not isinstance(targets, list)
            or not targets
            or any(target not in required_targets for target in targets)
        ):
            raise ValueError("Handoff artifact declares unexpected targets")
        covered.update(targets)
    if covered != set(required_targets):
        raise ValueError("Handoff does not cover the required native targets")
    return manifest


def download_artifact(artifact: dict, path: Path) -> None:
    headers = {
        "User-Agent": "codex-release-handoff/1",
        "Accept": "application/octet-stream",
    }
    token = os.environ.get("RUNNER_HANDOFF_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"

    class GithubRedirects(HTTPRedirectHandler):
        def redirect_request(self, request, fp, code, msg, headers, newurl):
            if (
                urlparse(newurl).scheme != "https"
                or urlparse(newurl).hostname not in HOSTS
            ):
                raise ValueError("Artifact redirected outside the GitHub boundary")
            redirected = super().redirect_request(
                request, fp, code, msg, headers, newurl
            )
            if (
                redirected
                and urlparse(newurl).hostname != urlparse(request.full_url).hostname
            ):
                redirected.remove_header("Authorization")
            return redirected

    digest = hashlib.sha256()
    size = 0
    try:
        with (
            build_opener(GithubRedirects()).open(
                Request(artifact["url"], headers=headers), timeout=60
            ) as response,
            path.open("xb") as output,
        ):
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > artifact["size_bytes"]:
                    raise ValueError("Artifact is larger than its receipt")
                digest.update(chunk)
                output.write(chunk)
        if size != artifact["size_bytes"] or digest.hexdigest() != artifact["sha256"]:
            raise ValueError("Artifact does not match its retained digest and size")
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def download(
    manifest: dict,
    destination: Path,
    commit: str,
    producer: str,
    required_targets: list[str],
) -> dict:
    validate(manifest, commit, producer, required_targets)
    destination.mkdir(parents=True, exist_ok=True)
    for artifact in manifest["artifacts"]:
        temporary = destination / (artifact["name"] + ".download")
        download_artifact(artifact, temporary)
        try:
            with tarfile.open(temporary) as archive:
                members = archive.getmembers()
                for member in members:
                    parts = Path(member.name).parts
                    if (
                        not parts
                        or parts[0] not in required_targets
                        or Path(member.name).is_absolute()
                        or ".." in parts
                        or not (member.isfile() or member.isdir())
                    ):
                        raise ValueError(
                            "Handoff archive contains unexpected or unsafe content"
                        )
                    if (destination / member.name).exists():
                        raise ValueError("Handoff would replace an existing artifact")
                archive.extractall(destination, members=members, filter="data")
        finally:
            temporary.unlink(missing_ok=True)
    for target in required_targets:
        archive = destination / target / f"codex-package-{target}.tar.gz"
        if not archive.is_file():
            raise ValueError(f"Handoff is missing the canonical {target} archive")
    return {
        "source_commit": manifest.get("source_commit", manifest["commit"]),
        "producer_commits": manifest.get(
            "producer_commits", [manifest.get("source_commit", manifest["commit"])]
        ),
        "packaging_commit": commit,
        "native_tree": manifest["native_tree"],
        "producer": producer,
        "artifacts": manifest["artifacts"],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("verify", "download"))
    parser.add_argument("--manifest", required=True, help="JSON or @manifest.json")
    parser.add_argument("--commit", required=True)
    parser.add_argument("--producer", required=True)
    parser.add_argument("--targets", required=True)
    parser.add_argument("--destination", type=Path)
    args = parser.parse_args()
    value = (
        Path(args.manifest[1:]).read_text()
        if args.manifest.startswith("@")
        else args.manifest
    )
    if len(value.encode()) > 65536:
        raise ValueError("Handoff metadata exceeds 64 KiB")
    manifest = json.loads(value)
    targets = args.targets.split(",")
    if args.action == "verify":
        validate(manifest, args.commit, args.producer, targets)
        print(
            json.dumps(
                {"status": "success", "producer": args.producer, "targets": targets}
            )
        )
    else:
        if args.destination is None:
            parser.error("download requires --destination")
        print(
            json.dumps(
                download(
                    manifest, args.destination, args.commit, args.producer, targets
                )
            )
        )


if __name__ == "__main__":
    main()
