import contextlib
import fcntl
import json
import os
from pathlib import Path
import stat
import time
from collections.abc import Iterator
from typing import Any

from .atomic_install import fsync_directory
from .model import DeliveryError
from .model import LOCK_FILE
from .model import MAX_EVIDENCE_BYTES
from .model import ROOT
from .model import canonical_json
from .model import path_exists


@contextlib.contextmanager
def delivery_lock(root: Path) -> Iterator[None]:
    ensure_evidence_root(root)
    path = root / LOCK_FILE
    try:
        descriptor = os.open(
            path,
            os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW,
            0o600,
        )
    except OSError as error:
        raise DeliveryError(
            "evidence_unavailable",
            "the delivery lock is unavailable",
        ) from error
    try:
        info = os.fstat(descriptor)
        if (
            not stat.S_ISREG(info.st_mode)
            or stat.S_IMODE(info.st_mode) & 0o077
            or info.st_uid != os.geteuid()
        ):
            raise DeliveryError(
                "evidence_path_invalid",
                "the delivery lock is not private",
            )
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise DeliveryError(
                "delivery_busy",
                "another delivery operation is active",
            ) from error
        yield
    finally:
        os.close(descriptor)


def ensure_evidence_root(root: Path) -> None:
    if (
        not root.is_absolute()
        or root == ROOT
        or ROOT in root.resolve(strict=False).parents
    ):
        raise DeliveryError(
            "evidence_path_invalid",
            "deployment evidence must remain outside source",
        )
    try:
        if not path_exists(root):
            root.mkdir(mode=0o700)
        info = root.lstat()
    except OSError as error:
        raise DeliveryError(
            "evidence_unavailable",
            "deployment evidence storage is unavailable",
        ) from error
    if (
        not stat.S_ISDIR(info.st_mode)
        or root.is_symlink()
        or stat.S_IMODE(info.st_mode) & 0o077
        or info.st_uid != os.geteuid()
    ):
        raise DeliveryError(
            "evidence_path_invalid",
            "deployment evidence storage is not private",
        )


def append_evidence_event(path: Path, event: dict[str, Any]) -> None:
    existing = read_evidence_events(path)
    previous = existing[-1]["eventDigest"] if existing else None
    record = {
        "schemaVersion": 1,
        "sequence": len(existing) + 1,
        "createdAt": int(time.time()),
        "previousDigest": previous,
        **event,
    }
    record["eventDigest"] = (
        __import__("hashlib").sha256(canonical_json(record)).hexdigest()
    )
    try:
        descriptor = os.open(
            path,
            os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW,
            0o600,
        )
    except OSError as error:
        raise DeliveryError(
            "evidence_unavailable",
            "deployment evidence is unavailable",
        ) from error
    try:
        info = os.fstat(descriptor)
        if (
            not stat.S_ISREG(info.st_mode)
            or stat.S_IMODE(info.st_mode) & 0o077
            or info.st_uid != os.geteuid()
        ):
            raise DeliveryError(
                "evidence_path_invalid",
                "deployment evidence file is not private",
            )
        data = canonical_json(record) + b"\n"
        offset = 0
        while offset < len(data):
            offset += os.write(descriptor, data[offset:])
        os.fsync(descriptor)
    except OSError as error:
        raise DeliveryError(
            "evidence_write_failed",
            "deployment evidence could not be committed",
        ) from error
    finally:
        os.close(descriptor)
    fsync_directory(path.parent)


def read_evidence_events(path: Path) -> list[dict[str, Any]]:
    if not path_exists(path):
        return []
    try:
        info = path.stat()
        if (
            path.is_symlink()
            or not stat.S_ISREG(info.st_mode)
            or stat.S_IMODE(info.st_mode) & 0o077
            or info.st_uid != os.geteuid()
            or info.st_size > MAX_EVIDENCE_BYTES
        ):
            raise DeliveryError(
                "evidence_invalid",
                "deployment evidence is invalid",
            )
        lines = path.read_bytes().splitlines()
    except OSError as error:
        raise DeliveryError(
            "evidence_unavailable",
            "deployment evidence is unavailable",
        ) from error
    events = []
    previous = None
    for index, line in enumerate(lines, start=1):
        try:
            event = json.loads(line)
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise DeliveryError(
                "evidence_invalid",
                "deployment evidence is invalid",
            ) from error
        digest = event.pop("eventDigest", None)
        expected = __import__("hashlib").sha256(canonical_json(event)).hexdigest()
        event["eventDigest"] = digest
        if (
            digest != expected
            or event.get("sequence") != index
            or event.get("previousDigest") != previous
        ):
            raise DeliveryError(
                "evidence_invalid",
                "deployment evidence integrity failed",
            )
        previous = digest
        events.append(event)
    return events
