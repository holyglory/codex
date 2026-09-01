import dataclasses
import hashlib
import json
import os
from pathlib import Path
from types import MappingProxyType
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
VERSION = "0.153.0-alpha.2+multi.4"
TARGET = "x86_64-unknown-linux-musl"
RELEASE_NAME = f"{VERSION}-{TARGET}"
ARTIFACT_ROOT = ROOT / "dist" / "releases" / VERSION
EVIDENCE_ROOT = Path("/var/lib/codex-multi-delivery")
EVENTS_FILE = "events.jsonl"
LOCK_FILE = "delivery.lock"
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024 * 1024
MAX_EXPANDED_BYTES = 3 * 1024 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 256
MAX_EVIDENCE_BYTES = 16 * 1024 * 1024
REQUIRED_COMPONENTS = (
    "codex-package.json",
    "bin/codex",
    "bin/codex-code-mode-host",
    "codex-path/rg",
    "codex-resources/bwrap",
)
EXECUTABLE_COMPONENTS = frozenset(REQUIRED_COMPONENTS[1:])


class DeliveryError(Exception):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


@dataclasses.dataclass(frozen=True)
class UserTarget:
    name: str
    uid: int
    gid: int
    home: Path
    allow_legacy_npm: bool = False

    @property
    def codex_home(self) -> Path:
        return self.home / ".codex"

    @property
    def standalone(self) -> Path:
        return self.codex_home / "packages" / "standalone"

    @property
    def releases(self) -> Path:
        return self.standalone / "releases"

    @property
    def current(self) -> Path:
        return self.standalone / "current"

    @property
    def launcher(self) -> Path:
        return self.home / ".local" / "bin" / "codex"

    @property
    def standalone_launcher_target(self) -> str:
        return str(self.current / "bin" / "codex")

    @property
    def npm_launcher_target(self) -> str:
        return "../lib/node_modules/@openai/codex/bin/codex.js"

    @property
    def npm_root(self) -> Path:
        return self.home / ".local" / "lib" / "node_modules" / "@openai" / "codex"


PRODUCTION_TARGETS = MappingProxyType(
    {
        "holyglory": UserTarget("holyglory", 1000, 1003, Path("/home/holyglory")),
        "slawa": UserTarget("slawa", 1003, 1006, Path("/home/slawa")),
    }
)
PRODUCTION_TARGET_NAMES = tuple(PRODUCTION_TARGETS)


@dataclasses.dataclass(frozen=True)
class DeliveryConfig:
    targets: tuple[UserTarget, ...]
    artifact_root: Path
    evidence_root: Path
    require_root: bool = True
    verify_system_users: bool = True
    manage_ownership: bool = True


@dataclasses.dataclass(frozen=True)
class ArtifactInfo:
    sha256: str
    component_sha256: dict[str, str]


@dataclasses.dataclass(frozen=True)
class UserState:
    name: str
    prior_launcher_target: str
    prior_current_target: str | None
    prior_version: str
    prior_binary_sha256: str
    prior_code_mode_sha256: str
    launcher_mode: str


@dataclasses.dataclass(frozen=True)
class DeliveryPlan:
    artifact: ArtifactInfo
    users: tuple[UserState, ...]
    process_counts: dict[str, dict[str, int]]
    blockers: tuple[str, ...]

    def private_payload(self) -> dict[str, Any]:
        return {
            "schemaVersion": 1,
            "version": VERSION,
            "target": TARGET,
            "artifactSha256": self.artifact.sha256,
            "componentSha256": self.artifact.component_sha256,
            "users": [dataclasses.asdict(user) for user in self.users],
            "processCounts": self.process_counts,
            "blockers": list(self.blockers),
        }

    def fingerprint(self) -> str:
        return hashlib.sha256(canonical_json(self.private_payload())).hexdigest()

    def public_payload(self) -> dict[str, Any]:
        fingerprint = self.fingerprint()
        return {
            "ok": True,
            "mode": "plan",
            "ready": not self.blockers,
            "version": VERSION,
            "target": TARGET,
            "artifactSha256": self.artifact.sha256,
            "componentSha256": self.artifact.component_sha256,
            "users": [
                {
                    "user": user.name,
                    "launcherMode": user.launcher_mode,
                    "rollbackVersion": user.prior_version,
                    "rollbackBinarySha256": user.prior_binary_sha256,
                    "rollbackCodeModeSha256": user.prior_code_mode_sha256,
                }
                for user in self.users
            ],
            "processCounts": self.process_counts,
            "blockers": list(self.blockers),
            "planFingerprint": fingerprint,
            "confirmation": f"deploy:{VERSION}:{fingerprint}",
        }


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=True,
    ).encode()


def path_exists(path: Path) -> bool:
    return path.exists() or path.is_symlink()


def production_config(user: str) -> DeliveryConfig:
    target = PRODUCTION_TARGETS.get(user)
    if target is None:
        raise DeliveryError(
            "target_not_allowed",
            "production delivery is restricted to an approved local account",
        )
    return DeliveryConfig((target,), ARTIFACT_ROOT, EVIDENCE_ROOT)
