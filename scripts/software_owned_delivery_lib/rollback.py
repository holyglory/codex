from pathlib import Path
from typing import Any

from .atomic_install import replace_symlink
from .evidence import read_evidence_events
from .model import DeliveryError
from .model import RELEASE_NAME
from .model import UserTarget
from .model import path_exists
from .validation import executable_version
from .validation import read_bounded_symlink
from .validation import resolve_release_target
from .validation import resolved_code_mode_binary
from .validation import resolved_launcher_binary
from .validation import sha256_file


def prepared_event(path: Path, deployment_id: str) -> dict[str, Any]:
    matches = [
        event
        for event in read_evidence_events(path)
        if event.get("eventType") == "prepared"
        and event.get("deploymentId") == deployment_id
    ]
    if len(matches) != 1:
        raise DeliveryError(
            "deployment_unknown",
            "deployment evidence was not found",
        )
    return matches[0]


def verify_rollback_user(
    target: UserTarget,
    state: dict[str, Any],
) -> None:
    prior_current = state.get("prior_current_target")
    prior_launcher = state.get("prior_launcher_target")
    if prior_current is not None and not isinstance(prior_current, str):
        raise DeliveryError(
            "evidence_invalid",
            "rollback current target is invalid",
        )
    if not isinstance(prior_launcher, str):
        raise DeliveryError(
            "evidence_invalid",
            "rollback launcher target is invalid",
        )
    rollback_binary = resolved_launcher_binary(
        target,
        prior_launcher,
        prior_current,
    )
    rollback_code_mode = resolved_code_mode_binary(
        target,
        prior_launcher,
        prior_current,
    )
    if (
        executable_version(rollback_binary) != state.get("prior_version")
        or sha256_file(rollback_binary) != state.get("prior_binary_sha256")
        or sha256_file(rollback_code_mode) != state.get("prior_code_mode_sha256")
    ):
        raise DeliveryError(
            "rollback_verification_failed",
            "rollback component verification failed",
        )
    if path_exists(target.current) and not target.current.is_symlink():
        raise DeliveryError(
            "target_state_changed",
            "a current release is not rollback-safe",
        )
    current = (
        read_bounded_symlink(target.current) if target.current.is_symlink() else None
    )
    if current not in {str(target.releases / RELEASE_NAME), prior_current}:
        raise DeliveryError(
            "target_state_changed",
            "a current release is not rollback-safe",
        )
    launcher = read_bounded_symlink(target.launcher)
    if launcher not in {
        target.standalone_launcher_target,
        prior_launcher,
    }:
        raise DeliveryError(
            "target_state_changed",
            "a user launcher is not rollback-safe",
        )


def rollback_user(
    target: UserTarget,
    state: dict[str, Any],
    manage_ownership: bool,
) -> None:
    prior_current = state.get("prior_current_target")
    if prior_current is not None:
        resolve_release_target(target, prior_current)
        replace_symlink(
            target.current,
            prior_current,
            target,
            manage_ownership,
            "rollback",
        )
    prior_launcher = state.get("prior_launcher_target")
    allowed = {target.standalone_launcher_target}
    if target.allow_legacy_npm:
        allowed.add(target.npm_launcher_target)
    if prior_launcher not in allowed:
        raise DeliveryError(
            "evidence_invalid",
            "rollback launcher target is not allowlisted",
        )
    replace_symlink(
        target.launcher,
        prior_launcher,
        target,
        manage_ownership,
        "rollback",
    )
    verify_rollback_user(target, state)
