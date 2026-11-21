#!/usr/bin/env python3
"""Stage exact locked GitHub BCR archives into Bazel's local distdir."""

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
from urllib.error import HTTPError, URLError
from urllib.parse import quote, unquote, urlparse
from urllib.request import HTTPRedirectHandler, Request, build_opener, urlopen

LOCK_MAX_BYTES = 8 * 1024 * 1024
SOURCE_MAX_BYTES = 256 * 1024
ARCHIVE_MAX_BYTES = 128 * 1024 * 1024
OVERRIDE_MAX_BYTES = 64 * 1024
FETCH_TIMEOUT_SECONDS = 30
DOWNLOAD_ATTEMPTS = 3
OVERRIDE_MARKER = "# Codex hosted native Bazel distdir cache hint."


class BootstrapError(RuntimeError):
    """A locked dependency could not be staged safely."""


class FetchError(BootstrapError):
    """A bounded network fetch failed without exposing a URL or token."""

    def __init__(self, status: int | None = None):
        self.status = status
        super().__init__(f"fetch failed{f' (HTTP {status})' if status else ''}")


class _NoRedirect(HTTPRedirectHandler):
    def http_error_302(self, request, fp, code, msg, headers):
        return fp

    http_error_301 = http_error_303 = http_error_307 = http_error_308 = http_error_302


NO_REDIRECT_OPENER = build_opener(_NoRedirect)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_bounded(path: Path, limit: int) -> bytes:
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except OSError as exc:
        raise BootstrapError(f"cannot inspect {path.name}") from exc
    try:
        before = os.fstat(fd)
        if before.st_mode & 0o170000 != 0o100000:
            raise BootstrapError(f"{path.name} must be a regular file")
        data = os.read(fd, limit + 1)
        after = os.fstat(fd)
    except OSError as exc:
        raise BootstrapError(f"cannot read {path.name}") from exc
    finally:
        os.close(fd)
    if len(data) > limit or after.st_size > limit:
        raise BootstrapError(f"{path.name} is too large")
    if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino):
        raise BootstrapError(f"{path.name} changed while reading")
    return data


def fetch_url(url: str, *, headers: dict[str, str] | None = None, limit: int) -> bytes:
    request = Request(url, headers=headers or {}, method="GET")
    try:
        with urlopen(request, timeout=FETCH_TIMEOUT_SECONDS) as response:
            status = getattr(response, "status", 200)
            if status != 200:
                raise FetchError(status)
            content = response.read(limit + 1)
    except HTTPError as exc:
        raise FetchError(exc.code) from exc
    except (URLError, TimeoutError, OSError) as exc:
        raise FetchError from exc
    if len(content) > limit:
        raise BootstrapError("download exceeded its bounded size")
    return content


def validate_api_redirect(location: str) -> None:
    parsed = urlparse(location)
    if parsed.scheme != "https" or parsed.netloc != "api.github.com":
        raise BootstrapError("GitHub API redirect host mismatch")


def fetch_api_metadata(url: str, headers: dict[str, str]) -> tuple[bytes, str]:
    current = url
    for _ in range(2):
        request = Request(current, headers=headers, method="GET")
        try:
            response = NO_REDIRECT_OPENER.open(request, timeout=FETCH_TIMEOUT_SECONDS)
        except HTTPError as exc:
            if exc.code not in {301, 302, 303, 307, 308}:
                raise FetchError(exc.code) from exc
            response = exc
        status = getattr(response, "status", getattr(response, "code", 200))
        if status == 200:
            return read_response(response, 2 * 1024 * 1024), current
        if status not in {301, 302, 303, 307, 308}:
            raise FetchError(status)
        location = response.headers.get("Location")
        if not isinstance(location, str):
            raise BootstrapError("GitHub API redirect missing")
        validate_api_redirect(location)
        current = location
    raise BootstrapError("GitHub API redirect limit exceeded")


def fetch_with_retry(url: str, *, limit: int) -> bytes:
    last: FetchError | None = None
    for _ in range(DOWNLOAD_ATTEMPTS):
        try:
            return fetch_url(url, limit=limit)
        except FetchError as exc:
            last = exc
    raise last or FetchError


def load_locked_sources(lock_path: Path) -> list[dict[str, str]]:
    raw = read_bounded(lock_path, LOCK_MAX_BYTES)
    try:
        lock = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise BootstrapError("MODULE.bazel.lock is invalid JSON") from exc
    hashes = lock.get("registryFileHashes")
    if not isinstance(hashes, dict):
        raise BootstrapError("MODULE.bazel.lock has no registry file hashes")
    manifest_entries = sorted(
        (url, digest)
        for url, digest in hashes.items()
        if isinstance(url, str)
        and url.startswith("https://bcr.bazel.build/modules/")
        and url.endswith("/source.json")
    )
    if not manifest_entries or len(manifest_entries) > 64:
        raise BootstrapError(
            "MODULE.bazel.lock has an invalid bounded source manifest set"
        )

    def load_manifest(entry: tuple[str, object]) -> dict[str, object] | None:
        manifest_url, manifest_sha = entry
        if not isinstance(manifest_sha, str) or not re.fullmatch(
            r"[0-9a-f]{64}", manifest_sha
        ):
            raise BootstrapError(
                "MODULE.bazel.lock has an invalid source manifest hash"
            )
        manifest_bytes = fetch_with_retry(manifest_url, limit=SOURCE_MAX_BYTES)
        if sha256(manifest_bytes) != manifest_sha:
            raise BootstrapError(
                f"source manifest hash mismatch for {manifest_url.rsplit('/modules/', 1)[-1]}"
            )
        try:
            manifest = json.loads(manifest_bytes)
        except json.JSONDecodeError as exc:
            raise BootstrapError(
                f"source manifest is invalid JSON: {manifest_url.rsplit('/modules/', 1)[-1]}"
            ) from exc
        if not isinstance(manifest, dict):
            raise BootstrapError("source manifest is not an object")
        manifest["_url"] = manifest_url
        manifest["_sha256"] = manifest_sha
        return manifest

    with ThreadPoolExecutor(max_workers=8) as executor:
        manifests = list(executor.map(load_manifest, manifest_entries))

    selected: list[dict[str, str]] = []
    seen_filenames: set[str] = set()
    for manifest in manifests:
        assert manifest is not None
        try:
            archive_url = manifest["url"]
            integrity = manifest["integrity"]
        except (KeyError, TypeError) as exc:
            raise BootstrapError("source manifest is incomplete") from exc
        if not isinstance(archive_url, str) or not isinstance(integrity, str):
            raise BootstrapError("source manifest has invalid fields")
        canonical = canonical_release_parts(archive_url)
        if canonical is None:
            continue
        expected_hex = parse_integrity(integrity)
        owner, repo, tag, filename = canonical
        if filename in seen_filenames:
            raise BootstrapError(f"GitHub archive basename collision: {filename}")
        seen_filenames.add(filename)
        selected.append(
            {
                "manifest_url": str(manifest["_url"]),
                "owner": owner,
                "repo": repo,
                "tag": tag,
                "filename": filename,
                "url": archive_url,
                "sha256": expected_hex,
                "manifest_sha256": str(manifest["_sha256"]),
            }
        )
    if not selected:
        raise BootstrapError(
            "MODULE.bazel.lock has no supported GitHub release archives"
        )
    return selected


def parse_integrity(value: str) -> str:
    if not value.startswith("sha256-"):
        raise BootstrapError("only sha256 source integrity is supported")
    try:
        decoded = base64.b64decode(value[7:], validate=True)
    except (ValueError, base64.binascii.Error) as exc:
        raise BootstrapError("source integrity is not valid base64") from exc
    if len(decoded) != 32:
        raise BootstrapError("source integrity has the wrong length")
    return decoded.hex()


def canonical_release_parts(url: str) -> tuple[str, str, str, str] | None:
    parsed = urlparse(url)
    if parsed.hostname != "github.com":
        return None
    if parsed.scheme != "https" or parsed.netloc != "github.com":
        raise BootstrapError("unsupported GitHub release archive URL")
    if "/releases/download/" not in parsed.path:
        return None
    match = re.fullmatch(
        r"/([^/]+)/([^/]+)/releases/download/([^/]+)/([^/]+)", parsed.path
    )
    if not match or parsed.query or parsed.fragment:
        raise BootstrapError("unsupported GitHub release archive URL")
    owner, repo, tag, filename = match.groups()
    if not all(unquote(value) == value for value in (owner, repo, tag, filename)):
        raise BootstrapError("encoded GitHub release archive URL is unsupported")
    return owner, repo, tag, filename


def validate_signed_redirect(location: str) -> None:
    parsed = urlparse(location)
    if parsed.scheme != "https" or parsed.netloc not in {
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
    }:
        raise BootstrapError("GitHub release asset redirect host mismatch")


def api_asset_bytes(spec: dict[str, str], token: str | None) -> bytes:
    api_url = (
        f"https://api.github.com/repos/{quote(spec['owner'])}/{quote(spec['repo'])}"
        f"/releases/tags/{quote(spec['tag'])}"
    )
    headers = {"Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        metadata_bytes, _metadata_url = fetch_api_metadata(api_url, headers)
        metadata = json.loads(metadata_bytes)
    except json.JSONDecodeError as exc:
        raise BootstrapError(
            f"GitHub release metadata for {spec['repo']} is invalid"
        ) from exc
    assets = metadata.get("assets") if isinstance(metadata, dict) else None
    if not isinstance(assets, list):
        raise BootstrapError(
            f"GitHub release metadata for {spec['repo']} has no assets"
        )
    matches = [
        asset
        for asset in assets
        if isinstance(asset, dict) and asset.get("name") == spec["filename"]
    ]
    if len(matches) != 1 or not isinstance(matches[0].get("id"), int):
        raise BootstrapError(
            f"GitHub release asset identity mismatch for {spec['repo']}"
        )
    asset = matches[0]
    release_url = metadata.get("url") if isinstance(metadata, dict) else None
    release_parts = (
        re.fullmatch(
            r"https://api\.github\.com/repos/([^/]+)/([^/]+)/releases/[^/]+",
            release_url,
        )
        if isinstance(release_url, str)
        else None
    )
    if release_parts is None:
        raise BootstrapError("GitHub release metadata repository identity missing")
    resolved_owner, resolved_repo = release_parts.groups()
    if metadata.get("tag_name") != spec["tag"]:
        raise BootstrapError(f"GitHub release tag mismatch for {spec['repo']}")
    browser_url = asset.get("browser_download_url")
    expected_browser_url = (
        f"https://github.com/{resolved_owner}/{resolved_repo}/releases/download/"
        f"{spec['tag']}/{spec['filename']}"
    )
    if browser_url != expected_browser_url:
        raise BootstrapError(f"GitHub release asset URL mismatch for {spec['repo']}")
    asset_url = f"https://api.github.com/repos/{quote(resolved_owner)}/{quote(resolved_repo)}/releases/assets/{asset['id']}"
    asset_headers = {"Accept": "application/octet-stream"}
    if token:
        asset_headers["Authorization"] = f"Bearer {token}"
    request = Request(asset_url, headers=asset_headers, method="GET")
    try:
        response = NO_REDIRECT_OPENER.open(request, timeout=FETCH_TIMEOUT_SECONDS)
    except HTTPError as exc:
        if exc.code not in {301, 302, 303, 307, 308}:
            raise FetchError(exc.code) from exc
        response = exc
    status = getattr(response, "status", getattr(response, "code", 200))
    if status == 200:
        return read_response(response, ARCHIVE_MAX_BYTES)
    if status not in {301, 302, 303, 307, 308}:
        raise FetchError(status)
    location = response.headers.get("Location")
    if not isinstance(location, str):
        raise BootstrapError(
            f"GitHub release asset redirect missing for {spec['repo']}"
        )
    try:
        validate_signed_redirect(location)
    except BootstrapError as exc:
        raise BootstrapError(
            f"GitHub release asset redirect host mismatch for {spec['repo']}"
        ) from exc
    # Signed release URLs never receive the API bearer token.
    return fetch_url(location, limit=ARCHIVE_MAX_BYTES)


def read_response(response, limit: int) -> bytes:
    content = response.read(limit + 1)
    if len(content) > limit:
        raise BootstrapError("download exceeded its bounded size")
    return content


def stage_archive(spec: dict[str, str], distdir: Path, token: str | None) -> None:
    destination = distdir / spec["filename"]
    if destination.exists() or destination.is_symlink():
        if destination.is_symlink() or not destination.is_file():
            raise BootstrapError(
                f"existing distdir entry is not a regular file: {spec['filename']}"
            )
        if sha256(read_bounded(destination, ARCHIVE_MAX_BYTES)) != spec["sha256"]:
            raise BootstrapError(
                f"existing distdir archive hash mismatch: {spec['filename']}"
            )
        return
    try:
        payload = fetch_with_retry(spec["url"], limit=ARCHIVE_MAX_BYTES)
    except FetchError:
        payload = api_asset_bytes(spec, token)
    if sha256(payload) != spec["sha256"]:
        raise BootstrapError(f"archive hash mismatch: {spec['filename']}")
    with tempfile.NamedTemporaryFile(
        mode="wb",
        dir=distdir,
        prefix=f".{spec['filename']}.",
        suffix=".tmp",
        delete=False,
    ) as temporary:
        temporary.write(payload)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    try:
        if destination.exists() or destination.is_symlink():
            if destination.is_symlink() or not destination.is_file():
                raise BootstrapError(
                    f"distdir race produced a non-file: {spec['filename']}"
                )
            if sha256(read_bounded(destination, ARCHIVE_MAX_BYTES)) != spec["sha256"]:
                raise BootstrapError(
                    f"distdir race changed archive: {spec['filename']}"
                )
        else:
            os.replace(temporary_path, destination)
            temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def validate_override(workspace: Path, distdir: Path) -> str | None:
    override = workspace / "user.bazelrc"
    if override.exists() or override.is_symlink():
        if override.is_symlink() or not override.is_file():
            raise BootstrapError("pre-existing user.bazelrc is not a regular file")
        try:
            contents = read_bounded(override, OVERRIDE_MAX_BYTES).decode("utf-8")
        except UnicodeDecodeError as exc:
            raise BootstrapError("pre-existing user.bazelrc is not UTF-8") from exc
    else:
        return None
    line = f"common --distdir={distdir}"
    meaningful = [
        entry.strip()
        for entry in contents.splitlines()
        if entry.strip() and not entry.lstrip().startswith("#")
    ]
    distdir_lines = [entry for entry in meaningful if "--distdir=" in entry]
    if any(entry != line for entry in distdir_lines) or any(
        "--distdir=" not in entry for entry in meaningful
    ):
        raise BootstrapError(
            "refusing to overwrite unrelated pre-existing user.bazelrc overrides"
        )
    return contents


def update_override(workspace: Path, distdir: Path) -> None:
    override = workspace / "user.bazelrc"
    line = f"common --distdir={distdir}\n"
    contents = validate_override(workspace, distdir)
    if contents is not None:
        if line.strip() in [entry.strip() for entry in contents.splitlines()]:
            return
        updated = contents
        if updated and not updated.endswith("\n"):
            updated += "\n"
        updated += f"{OVERRIDE_MARKER}\n{line}"
    else:
        updated = f"{OVERRIDE_MARKER}\n{line}"
    with tempfile.NamedTemporaryFile(
        mode="w",
        dir=workspace,
        prefix=".user.bazelrc.",
        suffix=".tmp",
        delete=False,
        encoding="utf-8",
    ) as temporary:
        temporary.write(updated)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    try:
        os.replace(temporary_path, override)
    finally:
        temporary_path.unlink(missing_ok=True)


def bootstrap(workspace: Path, distdir: Path, token: str | None = None) -> None:
    workspace = workspace.resolve()
    if any(char.isspace() or ord(char) < 32 for char in str(distdir)):
        raise BootstrapError("distdir path contains whitespace or control characters")
    lock_path = workspace / "MODULE.bazel.lock"
    try:
        distdir.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise BootstrapError("cannot create distdir") from exc
    if distdir.is_symlink() or not distdir.is_dir():
        raise BootstrapError("distdir must be a real directory")
    validate_override(workspace, distdir)
    specs = load_locked_sources(lock_path)
    for spec in specs:
        stage_archive(spec, distdir, token)
    update_override(workspace, distdir)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--distdir", type=Path, required=True)
    args = parser.parse_args()
    try:
        bootstrap(
            args.workspace,
            args.distdir,
            os.environ.get("GITHUB_TOKEN") or os.environ.get("BAZELISK_GITHUB_TOKEN"),
        )
    except BootstrapError as exc:
        print(f"Bazel distdir bootstrap failed: {exc}", file=os.sys.stderr)
        return 1
    print(f"Verified locked Bazel distdir archives in {args.distdir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
