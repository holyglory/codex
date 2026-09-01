import ctypes
import errno
import os
from pathlib import Path, PurePosixPath
import stat
import tarfile

from .model import ArtifactInfo
from .model import DeliveryError
from .model import EXECUTABLE_COMPONENTS
from .model import UserTarget
from .model import VERSION
from .model import path_exists
from .validation import executable_version
from .validation import sha256_file
from .validation import validated_members


def extract_verified_archive(artifact: Path, destination: Path) -> None:
    try:
        with tarfile.open(artifact, "r:gz") as archive:
            members = validated_members(archive)
            ordered = sorted(
                members.items(),
                key=lambda item: (len(PurePosixPath(item[0]).parts), item[0]),
            )
            for name, member in ordered:
                target = destination.joinpath(*PurePosixPath(name).parts)
                if member.isdir():
                    target.mkdir(mode=0o755, parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
                source = archive.extractfile(member)
                if source is None:
                    raise DeliveryError(
                        "archive_member_invalid",
                        "an archive member is unavailable",
                    )
                descriptor = os.open(
                    target,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                    0o600,
                )
                with os.fdopen(descriptor, "wb") as output:
                    while chunk := source.read(1024 * 1024):
                        output.write(chunk)
                    os.fchmod(
                        output.fileno(),
                        0o755 if name in EXECUTABLE_COMPONENTS else 0o644,
                    )
                    output.flush()
                    os.fsync(output.fileno())
    except tarfile.TarError as error:
        raise DeliveryError(
            "artifact_invalid",
            "the delivery artifact cannot be extracted",
        ) from error
    except OSError as error:
        raise DeliveryError(
            "archive_extract_failed",
            "the delivery artifact could not be staged",
        ) from error


def verify_installed_release(release: Path, artifact: ArtifactInfo) -> None:
    for component, expected_sha in artifact.component_sha256.items():
        path = release.joinpath(*PurePosixPath(component).parts)
        if sha256_file(path) != expected_sha:
            raise DeliveryError(
                "installed_hash_mismatch",
                "an installed component hash does not match",
            )
        if component in EXECUTABLE_COMPONENTS and not os.access(path, os.X_OK):
            raise DeliveryError(
                "component_not_executable",
                "an installed component is not executable",
            )
    if executable_version(release / "bin" / "codex") != VERSION:
        raise DeliveryError(
            "installed_version_mismatch",
            "the installed version does not match",
        )


def ensure_install_directories(target: UserTarget, manage_owner: bool) -> None:
    path = target.codex_home
    for name in ("packages", "standalone", "releases"):
        path = path / name
        if not path_exists(path):
            try:
                path.mkdir(mode=0o750)
            except OSError as error:
                raise DeliveryError(
                    "target_layout_invalid",
                    "an install directory could not be created",
                ) from error
            set_owner(path, target, manage_owner)
        try:
            info = path.lstat()
        except OSError as error:
            raise DeliveryError(
                "target_layout_invalid",
                "an install directory is unavailable",
            ) from error
        if not stat.S_ISDIR(info.st_mode) or path.is_symlink():
            raise DeliveryError(
                "target_layout_invalid",
                "an install directory is invalid",
            )
        if manage_owner and info.st_uid != target.uid:
            raise DeliveryError(
                "target_owner_invalid",
                "an install directory owner is invalid",
            )


def set_owner(path: Path, target: UserTarget, enabled: bool) -> None:
    if enabled:
        try:
            os.chown(path, target.uid, target.gid, follow_symlinks=False)
        except OSError as error:
            raise DeliveryError(
                "target_owner_update_failed",
                "an install path owner could not be set",
            ) from error


def set_tree_owner(root: Path, target: UserTarget, enabled: bool) -> None:
    if not enabled:
        return
    try:
        for path in sorted(root.rglob("*")):
            os.chown(path, target.uid, target.gid, follow_symlinks=False)
        os.chown(root, target.uid, target.gid, follow_symlinks=False)
    except OSError as error:
        raise DeliveryError(
            "target_owner_update_failed",
            "staged package ownership could not be set",
        ) from error


def replace_symlink(
    path: Path,
    target_value: str,
    target: UserTarget,
    manage_owner: bool,
    suffix: str,
) -> None:
    temporary = path.parent / f".{path.name}.{suffix}"
    if path_exists(temporary):
        raise DeliveryError(
            "temporary_link_exists",
            "an atomic-link temporary already exists",
        )
    try:
        os.symlink(target_value, temporary)
        if manage_owner:
            os.lchown(temporary, target.uid, target.gid)
        os.replace(temporary, path)
    except OSError as error:
        raise DeliveryError(
            "atomic_link_failed",
            "an atomic launcher update failed",
        ) from error
    try:
        fsync_directory(path.parent)
    except DeliveryError as error:
        raise DeliveryError(
            "link_committed_durability_uncertain",
            "the launcher changed but parent durability is uncertain",
        ) from error


def nearest_existing_device(path: Path) -> int:
    candidate = path
    while not path_exists(candidate):
        if candidate == candidate.parent:
            raise DeliveryError(
                "filesystem_unavailable",
                "a staging filesystem is unavailable",
            )
        candidate = candidate.parent
    try:
        return candidate.stat().st_dev
    except OSError as error:
        raise DeliveryError(
            "filesystem_unavailable",
            "a staging filesystem is unavailable",
        ) from error


def rename_no_replace(source: Path, destination: Path) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    renameat2 = getattr(libc, "renameat2", None)
    if renameat2 is None:
        raise DeliveryError(
            "atomic_noreplace_unsupported",
            "atomic no-clobber rename is unsupported",
        )
    renameat2.argtypes = [
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    ]
    renameat2.restype = ctypes.c_int
    result = renameat2(
        -100,
        os.fsencode(source),
        -100,
        os.fsencode(destination),
        1,
    )
    if result == 0:
        return
    error_number = ctypes.get_errno()
    if error_number == errno.EEXIST:
        raise DeliveryError(
            "release_exists",
            "the final release directory already exists",
        )
    raise DeliveryError(
        "atomic_noreplace_failed",
        "the release directory could not be activated",
    )


def fsync_directory(path: Path) -> None:
    try:
        descriptor = os.open(
            path,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
        )
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    except OSError as error:
        raise DeliveryError(
            "durability_sync_failed",
            "directory durability synchronization failed",
        ) from error


def fsync_tree(root: Path) -> None:
    directories = [
        root,
        *(path for path in root.rglob("*") if path.is_dir()),
    ]
    for path in sorted(
        directories,
        key=lambda item: len(item.parts),
        reverse=True,
    ):
        fsync_directory(path)
