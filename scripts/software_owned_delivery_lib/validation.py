import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import subprocess
import tarfile

from .model import ArtifactInfo
from .model import DeliveryError
from .model import MAX_ARCHIVE_BYTES
from .model import MAX_ARCHIVE_MEMBERS
from .model import MAX_EXPANDED_BYTES
from .model import REQUIRED_COMPONENTS
from .model import TARGET
from .model import UserTarget
from .model import VERSION
from .model import path_exists


def sha256_file(path: Path) -> str:
    try:
        info = path.stat()
        if not stat.S_ISREG(info.st_mode):
            raise DeliveryError(
                "component_invalid",
                "a package component is not a regular file",
            )
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            while chunk := stream.read(1024 * 1024):
                digest.update(chunk)
        return digest.hexdigest()
    except OSError as error:
        raise DeliveryError(
            "component_unavailable",
            "a package component is unavailable",
        ) from error


def verify_artifact(
    artifact: Path,
    manifest: Path,
    artifact_root: Path,
) -> ArtifactInfo:
    artifact = validate_artifact_path(artifact, artifact_root)
    manifest = validate_artifact_path(manifest, artifact_root)
    if artifact.suffixes[-2:] != [".tar", ".gz"]:
        raise DeliveryError(
            "artifact_format_invalid",
            "the delivery artifact must be a tar.gz package",
        )
    if artifact.stat().st_size > MAX_ARCHIVE_BYTES:
        raise DeliveryError(
            "artifact_too_large",
            "the delivery artifact exceeds its size bound",
        )
    actual_sha = sha256_file(artifact)
    if manifest_digest(manifest, artifact.name) != actual_sha:
        raise DeliveryError(
            "artifact_checksum_mismatch",
            "the artifact checksum does not match its manifest",
        )
    component_sha: dict[str, str] = {}
    try:
        with tarfile.open(artifact, "r:gz") as archive:
            members = validated_members(archive)
            metadata_member = members.get("codex-package.json")
            if metadata_member is None or metadata_member.size > 8192:
                raise DeliveryError(
                    "package_metadata_invalid",
                    "package metadata is missing or invalid",
                )
            metadata_stream = archive.extractfile(metadata_member)
            if metadata_stream is None:
                raise DeliveryError(
                    "package_metadata_invalid",
                    "package metadata is unavailable",
                )
            validate_package_metadata(json.loads(metadata_stream.read(8193)))
            for component in REQUIRED_COMPONENTS:
                member = members.get(component)
                if member is None or not member.isfile():
                    raise DeliveryError(
                        "package_component_missing",
                        "a required package component is missing",
                    )
                stream = archive.extractfile(member)
                if stream is None:
                    raise DeliveryError(
                        "package_component_missing",
                        "a required package component is unavailable",
                    )
                digest = hashlib.sha256()
                while chunk := stream.read(1024 * 1024):
                    digest.update(chunk)
                component_sha[component] = digest.hexdigest()
    except (tarfile.TarError, json.JSONDecodeError, UnicodeDecodeError) as error:
        raise DeliveryError(
            "artifact_invalid",
            "the delivery artifact is invalid",
        ) from error
    return ArtifactInfo(actual_sha, component_sha)


def validate_artifact_path(path: Path, root: Path) -> Path:
    if not path.is_absolute():
        raise DeliveryError(
            "artifact_path_invalid",
            "artifact paths must be absolute",
        )
    try:
        resolved = path.resolve(strict=True)
        root_resolved = root.resolve(strict=True)
        info = path.lstat()
    except OSError as error:
        raise DeliveryError(
            "artifact_unavailable",
            "a delivery artifact input is unavailable",
        ) from error
    if root_resolved != resolved.parent and root_resolved not in resolved.parents:
        raise DeliveryError(
            "artifact_path_invalid",
            "artifact inputs must stay in the release staging root",
        )
    if not stat.S_ISREG(info.st_mode) or path.is_symlink():
        raise DeliveryError(
            "artifact_path_invalid",
            "artifact inputs must be regular files",
        )
    return resolved


def manifest_digest(path: Path, artifact_name: str) -> str:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise DeliveryError(
            "manifest_unavailable",
            "the checksum manifest is unavailable",
        ) from error
    if len(data) > 64 * 1024:
        raise DeliveryError(
            "manifest_invalid",
            "the checksum manifest exceeds its size bound",
        )
    matches = []
    try:
        for raw_line in data.splitlines():
            fields = raw_line.decode("ascii", errors="strict").split()
            if len(fields) == 2 and fields[1].lstrip("*") == artifact_name:
                matches.append(fields[0].lower())
    except UnicodeDecodeError as error:
        raise DeliveryError(
            "manifest_invalid",
            "the checksum manifest is not ASCII",
        ) from error
    if (
        len(matches) != 1
        or len(matches[0]) != 64
        or any(char not in "0123456789abcdef" for char in matches[0])
    ):
        raise DeliveryError(
            "manifest_invalid",
            "the checksum manifest has no unique artifact digest",
        )
    return matches[0]


def validated_members(archive: tarfile.TarFile) -> dict[str, tarfile.TarInfo]:
    members = archive.getmembers()
    if len(members) > MAX_ARCHIVE_MEMBERS:
        raise DeliveryError(
            "archive_bounds_exceeded",
            "the package has too many archive members",
        )
    total = 0
    result: dict[str, tarfile.TarInfo] = {}
    for member in members:
        path = PurePosixPath(member.name)
        normalized_name = member.name.rstrip("/")
        if (
            not member.name
            or normalized_name in {"", "."}
            or path.is_absolute()
            or ".." in path.parts
            or any(ord(char) < 32 for char in member.name)
            or len(member.name.encode()) > 512
            or not (member.isdir() or member.isfile())
            or normalized_name in result
        ):
            raise DeliveryError(
                "archive_member_invalid",
                "the package contains an unsafe archive member",
            )
        total += member.size
        if total > MAX_EXPANDED_BYTES:
            raise DeliveryError(
                "archive_bounds_exceeded",
                "the expanded package exceeds its size bound",
            )
        result[normalized_name] = member
    return result


def validate_package_metadata(metadata: object) -> None:
    expected = {
        "layoutVersion": 1,
        "version": VERSION,
        "target": TARGET,
        "variant": "codex",
        "entrypoint": "bin/codex",
        "resourcesDir": "codex-resources",
        "pathDir": "codex-path",
    }
    if not isinstance(metadata, dict) or any(
        metadata.get(key) != value for key, value in expected.items()
    ):
        raise DeliveryError(
            "package_metadata_invalid",
            "package metadata does not match the release",
        )


def executable_version(executable: Path) -> str:
    try:
        result = subprocess.run(
            [str(executable), "--version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env={
                "PATH": "/usr/bin:/bin",
                "HOME": "/nonexistent",
                "CODEX_HOME": "/nonexistent",
            },
            timeout=10,
            check=False,
            text=True,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise DeliveryError(
            "version_check_failed",
            "a Codex version check failed",
        ) from error
    output = result.stdout.strip().split()
    if (
        result.returncode != 0
        or len(output) != 2
        or output[0] != "codex-cli"
        or len(output[1]) > 64
    ):
        raise DeliveryError(
            "version_check_failed",
            "a Codex version check returned an invalid result",
        )
    return output[1]


def verify_private_codex_home(target: UserTarget, verify_owner: bool) -> None:
    for path in (
        target.home,
        target.codex_home,
        target.home / ".local",
        target.launcher.parent,
    ):
        try:
            info = path.lstat()
        except OSError as error:
            raise DeliveryError(
                "target_layout_invalid",
                "a target directory is unavailable",
            ) from error
        if not stat.S_ISDIR(info.st_mode) or path.is_symlink():
            raise DeliveryError(
                "target_layout_invalid",
                "a target directory is invalid",
            )
        if verify_owner and info.st_uid != target.uid:
            raise DeliveryError(
                "target_owner_invalid",
                "a target directory owner is invalid",
            )
    if stat.S_IMODE(target.codex_home.stat().st_mode) & 0o007:
        raise DeliveryError(
            "codex_home_not_private",
            "a target CODEX_HOME allows unrelated-user access",
        )


def read_bounded_symlink(path: Path) -> str:
    try:
        info = path.lstat()
        if not stat.S_ISLNK(info.st_mode):
            raise DeliveryError(
                "symlink_invalid",
                "an expected launcher is not a symlink",
            )
        target = os.readlink(path)
    except OSError as error:
        raise DeliveryError(
            "symlink_unavailable",
            "an expected launcher is unavailable",
        ) from error
    if (
        not target
        or len(target.encode()) > 1024
        or any(ord(char) < 32 for char in target)
    ):
        raise DeliveryError(
            "symlink_invalid",
            "an expected launcher target is invalid",
        )
    return target


def resolve_release_target(target: UserTarget, link_target: str) -> Path:
    raw = Path(link_target)
    candidate = raw if raw.is_absolute() else target.current.parent / raw
    try:
        resolved = candidate.resolve(strict=True)
        releases = target.releases.resolve(strict=True)
    except OSError as error:
        raise DeliveryError(
            "rollback_release_unavailable",
            "a rollback release is unavailable",
        ) from error
    if resolved.parent != releases or not resolved.is_dir():
        raise DeliveryError(
            "rollback_release_invalid",
            "a rollback release target is invalid",
        )
    return resolved


def resolved_launcher_binary(
    target: UserTarget,
    launcher_target: str,
    current_target: str | None,
) -> Path:
    if launcher_target == target.npm_launcher_target:
        return npm_native_bin(target) / "codex"
    if current_target is None:
        raise DeliveryError(
            "evidence_invalid",
            "rollback release evidence is incomplete",
        )
    return resolve_release_target(target, current_target) / "bin" / "codex"


def resolved_code_mode_binary(
    target: UserTarget,
    launcher_target: str,
    current_target: str | None,
) -> Path:
    if launcher_target == target.npm_launcher_target:
        return npm_native_bin(target) / "codex-code-mode-host"
    if current_target is None:
        raise DeliveryError(
            "evidence_invalid",
            "rollback release evidence is incomplete",
        )
    return (
        resolve_release_target(target, current_target) / "bin" / "codex-code-mode-host"
    )


def npm_native_bin(target: UserTarget) -> Path:
    return (
        target.npm_root
        / "node_modules"
        / "@openai"
        / "codex-linux-x64"
        / "vendor"
        / TARGET
        / "bin"
    )


def scan_codex_processes(
    targets: tuple[UserTarget, ...],
) -> dict[str, dict[str, int]]:
    by_uid = {target.uid: target.name for target in targets}
    counts = {
        target.name: {"appServer": 0, "codeModeHost": 0, "otherCodex": 0}
        for target in targets
    }
    try:
        entries = list(Path("/proc").iterdir())
    except OSError as error:
        raise DeliveryError(
            "process_inventory_unavailable",
            "Codex process inventory is unavailable",
        ) from error
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            user = by_uid.get(entry.stat().st_uid)
            if user is None:
                continue
            basename = Path(os.readlink(entry / "exe")).name
            if "codex" not in basename.lower():
                continue
            with (entry / "cmdline").open("rb") as stream:
                arguments = stream.read(65536).split(b"\0")
            if basename == "codex-code-mode-host":
                counts[user]["codeModeHost"] += 1
            elif b"app-server" in arguments:
                counts[user]["appServer"] += 1
            else:
                counts[user]["otherCodex"] += 1
        except FileNotFoundError:
            continue
        except OSError as error:
            raise DeliveryError(
                "process_inventory_unavailable",
                "Codex process inventory is incomplete",
            ) from error
    return counts
