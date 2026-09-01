import dataclasses
import os
from pathlib import Path
import pwd
from typing import Any
import uuid

from .atomic_install import ensure_install_directories
from .atomic_install import extract_verified_archive
from .atomic_install import fsync_directory
from .atomic_install import fsync_tree
from .atomic_install import nearest_existing_device
from .atomic_install import rename_no_replace
from .atomic_install import replace_symlink
from .atomic_install import set_owner
from .atomic_install import set_tree_owner
from .atomic_install import verify_installed_release
from .evidence import append_evidence_event
from .evidence import delivery_lock
from .model import ArtifactInfo
from .model import DeliveryConfig
from .model import DeliveryError
from .model import DeliveryPlan
from .model import EVENTS_FILE
from .model import PRODUCTION_TARGETS
from .model import RELEASE_NAME
from .model import ROOT
from .model import TARGET
from .model import UserState
from .model import UserTarget
from .model import VERSION
from .model import path_exists
from .rollback import prepared_event
from .rollback import rollback_user
from .rollback import verify_rollback_user
from .validation import executable_version
from .validation import read_bounded_symlink
from .validation import resolve_release_target
from .validation import resolved_code_mode_binary
from .validation import resolved_launcher_binary
from .validation import scan_codex_processes
from .validation import sha256_file
from .validation import verify_artifact
from .validation import verify_private_codex_home


class DeliveryWorkflow:
    def __init__(self, config: DeliveryConfig, process_scanner=None) -> None:
        self.config = config
        self.process_scanner = process_scanner or scan_codex_processes

    def plan(self, artifact: Path, checksum_manifest: Path) -> DeliveryPlan:
        self._validate_invocation_identity()
        artifact_info = verify_artifact(
            artifact,
            checksum_manifest,
            self.config.artifact_root,
        )
        users = tuple(self._inspect_user(target) for target in self.config.targets)
        process_counts = self.process_scanner(self.config.targets)
        blockers = []
        if any(sum(counts.values()) for counts in process_counts.values()):
            blockers.append("codex_processes_active")
        if any(
            path_exists(target.releases / RELEASE_NAME)
            for target in self.config.targets
        ):
            blockers.append("release_directory_exists")
        return DeliveryPlan(
            artifact_info,
            users,
            process_counts,
            tuple(blockers),
        )

    def apply(
        self,
        artifact: Path,
        checksum_manifest: Path,
        expected_fingerprint: str,
        confirmation: str,
    ) -> dict[str, Any]:
        if self.config.require_root and os.geteuid() != 0:
            raise DeliveryError(
                "root_required",
                "deployment requires the privileged host boundary",
            )
        plan = self.plan(artifact, checksum_manifest)
        self._validate_apply_authorization(
            plan,
            expected_fingerprint,
            confirmation,
        )
        deployment_id = str(uuid.uuid4())
        activated: list[str] = []
        with delivery_lock(self.config.evidence_root):
            plan = self.plan(artifact, checksum_manifest)
            self._validate_apply_authorization(
                plan,
                expected_fingerprint,
                confirmation,
            )
            self._append_event(
                {
                    "eventType": "prepared",
                    "deploymentId": deployment_id,
                    "version": VERSION,
                    "target": TARGET,
                    "artifactSha256": plan.artifact.sha256,
                    "componentSha256": plan.artifact.component_sha256,
                    "users": [dataclasses.asdict(user) for user in plan.users],
                }
            )
            try:
                states = {state.name: state for state in plan.users}
                for target in self.config.targets:
                    if self._processes_active():
                        raise DeliveryError(
                            "process_gate_blocked",
                            "Codex processes must remain drained",
                        )
                    self._install_user(
                        target,
                        states[target.name],
                        artifact,
                        deployment_id,
                        plan.artifact,
                    )
                    activated.append(target.name)
                    self._append_event(
                        {
                            "eventType": "activated",
                            "deploymentId": deployment_id,
                            "user": target.name,
                        }
                    )
            except DeliveryError as error:
                self._append_failed(deployment_id, activated, error.code)
                raise
            except Exception as error:
                self._append_failed(
                    deployment_id,
                    activated,
                    "deployment_failed",
                )
                raise DeliveryError(
                    "deployment_failed",
                    "deployment failed safely",
                ) from error
            self._append_event(
                {
                    "eventType": "completed",
                    "deploymentId": deployment_id,
                    "activatedUsers": activated,
                }
            )
        return {
            "ok": True,
            "mode": "apply",
            "deploymentId": deployment_id,
            "version": VERSION,
            "users": activated,
            "artifactSha256": plan.artifact.sha256,
        }

    def rollback(self, deployment_id: str, confirmation: str) -> dict[str, Any]:
        if self.config.require_root and os.geteuid() != 0:
            raise DeliveryError(
                "root_required",
                "rollback requires the privileged host boundary",
            )
        self._validate_invocation_identity()
        if confirmation != f"rollback:{deployment_id}":
            raise DeliveryError(
                "confirmation_mismatch",
                "rollback confirmation does not match",
            )
        with delivery_lock(self.config.evidence_root):
            prepared = prepared_event(
                self.config.evidence_root / EVENTS_FILE,
                deployment_id,
            )
            if self._processes_active():
                raise DeliveryError(
                    "process_gate_blocked",
                    "Codex processes must be drained first",
                )
            raw_states = prepared.get("users")
            if not isinstance(raw_states, list) or len(raw_states) != len(
                self.config.targets
            ):
                raise DeliveryError(
                    "evidence_invalid",
                    "deployment evidence is incomplete",
                )
            try:
                states = {
                    item["name"]: item for item in raw_states if isinstance(item, dict)
                }
            except (KeyError, TypeError) as error:
                raise DeliveryError(
                    "evidence_invalid",
                    "deployment evidence is incomplete",
                ) from error
            if set(states) != {target.name for target in self.config.targets}:
                raise DeliveryError(
                    "evidence_invalid",
                    "deployment evidence targets do not match",
                )
            for target in self.config.targets:
                state = states.get(target.name)
                if not isinstance(state, dict):
                    raise DeliveryError(
                        "evidence_invalid",
                        "deployment evidence is incomplete",
                    )
                verify_rollback_user(target, state)
            restored = []
            for target in self.config.targets:
                rollback_user(
                    target,
                    states[target.name],
                    self.config.manage_ownership,
                )
                restored.append(target.name)
                self._append_event(
                    {
                        "eventType": "rollbackUser",
                        "deploymentId": deployment_id,
                        "user": target.name,
                    }
                )
            self._append_event(
                {
                    "eventType": "rolledBack",
                    "deploymentId": deployment_id,
                    "users": restored,
                }
            )
        return {
            "ok": True,
            "mode": "rollback",
            "deploymentId": deployment_id,
            "users": restored,
            "failedReleasePreserved": True,
        }

    def _validate_invocation_identity(self) -> None:
        if len(self.config.targets) != 1:
            raise DeliveryError(
                "target_allowlist_invalid",
                "delivery requires exactly one approved target",
            )
        target = self.config.targets[0]
        production_target = PRODUCTION_TARGETS.get(target.name)
        if production_target is None:
            raise DeliveryError(
                "target_allowlist_invalid",
                "the delivery target is not approved",
            )
        if (self.config.require_root or self.config.verify_system_users) and (
            target != production_target
        ):
            raise DeliveryError(
                "target_identity_mismatch",
                "the target configuration differs from fixed production metadata",
            )
        if self.config.verify_system_users:
            try:
                account = pwd.getpwnam(target.name)
            except KeyError as error:
                raise DeliveryError(
                    "target_identity_mismatch",
                    "the target user is unavailable",
                ) from error
            if (
                account.pw_uid != target.uid
                or account.pw_gid != target.gid
                or Path(account.pw_dir) != target.home
            ):
                raise DeliveryError(
                    "target_identity_mismatch",
                    "the target user identity changed",
                )
        evidence = self.config.evidence_root.resolve(strict=False)
        if evidence == ROOT or ROOT in evidence.parents:
            raise DeliveryError(
                "evidence_path_invalid",
                "deployment evidence must remain outside source",
            )

    def _inspect_user(self, target: UserTarget) -> UserState:
        verify_private_codex_home(target, self.config.manage_ownership)
        launcher_target = read_bounded_symlink(target.launcher)
        if launcher_target == target.standalone_launcher_target:
            launcher_mode = "standalone"
        elif target.allow_legacy_npm and launcher_target == target.npm_launcher_target:
            launcher_mode = "npm"
        else:
            raise DeliveryError(
                "launcher_target_unexpected",
                "a user launcher target is unexpected",
            )
        prior_current = None
        if target.current.is_symlink():
            prior_current = read_bounded_symlink(target.current)
            release = resolve_release_target(target, prior_current)
            codex = release / "bin" / "codex"
            code_mode = release / "bin" / "codex-code-mode-host"
        elif launcher_mode == "npm" and not target.current.exists():
            codex = resolved_launcher_binary(target, launcher_target, None)
            code_mode = resolved_code_mode_binary(target, launcher_target, None)
        else:
            raise DeliveryError(
                "current_link_invalid",
                "a current release link is invalid",
            )
        return UserState(
            name=target.name,
            prior_launcher_target=launcher_target,
            prior_current_target=prior_current,
            prior_version=executable_version(codex),
            prior_binary_sha256=sha256_file(codex),
            prior_code_mode_sha256=sha256_file(code_mode),
            launcher_mode=launcher_mode,
        )

    def _validate_apply_authorization(
        self,
        plan: DeliveryPlan,
        expected_fingerprint: str,
        confirmation: str,
    ) -> None:
        if plan.blockers:
            raise DeliveryError(
                "preflight_blocked",
                "deployment preflight is blocked",
            )
        fingerprint = plan.fingerprint()
        if expected_fingerprint != fingerprint:
            raise DeliveryError(
                "plan_changed",
                "the reviewed deployment plan changed",
            )
        if confirmation != f"deploy:{VERSION}:{fingerprint}":
            raise DeliveryError(
                "confirmation_mismatch",
                "deployment confirmation does not match",
            )

    def _install_user(
        self,
        target: UserTarget,
        prior: UserState,
        artifact: Path,
        deployment_id: str,
        artifact_info: ArtifactInfo,
    ) -> None:
        if read_bounded_symlink(target.launcher) != prior.prior_launcher_target:
            raise DeliveryError(
                "target_state_changed",
                "a user launcher changed after planning",
            )
        if path_exists(target.current) and not target.current.is_symlink():
            raise DeliveryError(
                "target_state_changed",
                "a current release changed after planning",
            )
        current = (
            read_bounded_symlink(target.current)
            if target.current.is_symlink()
            else None
        )
        if current != prior.prior_current_target:
            raise DeliveryError(
                "target_state_changed",
                "a current release changed after planning",
            )
        ensure_install_directories(target, self.config.manage_ownership)
        final = target.releases / RELEASE_NAME
        stage = target.releases / f".staging.{RELEASE_NAME}.{deployment_id}"
        if path_exists(stage) or path_exists(final):
            raise DeliveryError(
                "release_exists",
                "a staged or final release already exists",
            )
        if nearest_existing_device(stage.parent) != nearest_existing_device(
            final.parent
        ):
            raise DeliveryError(
                "cross_device_stage",
                "release staging must use one filesystem",
            )
        try:
            stage.mkdir(mode=0o750)
        except OSError as error:
            raise DeliveryError(
                "stage_create_failed",
                "the release staging directory could not be created",
            ) from error
        set_owner(stage, target, self.config.manage_ownership)
        extract_verified_archive(artifact, stage)
        set_tree_owner(stage, target, self.config.manage_ownership)
        verify_installed_release(stage, artifact_info)
        fsync_tree(stage)
        rename_no_replace(stage, final)
        try:
            fsync_directory(target.releases)
        except DeliveryError as error:
            raise DeliveryError(
                "release_committed_durability_uncertain",
                "the release was activated but parent durability is uncertain",
            ) from error
        replace_symlink(
            target.current,
            str(final),
            target,
            self.config.manage_ownership,
            deployment_id,
        )
        if read_bounded_symlink(target.launcher) != target.standalone_launcher_target:
            replace_symlink(
                target.launcher,
                target.standalone_launcher_target,
                target,
                self.config.manage_ownership,
                deployment_id,
            )
        self._verify_activated_user(target, final, artifact_info)

    def _verify_activated_user(
        self,
        target: UserTarget,
        final: Path,
        artifact: ArtifactInfo,
    ) -> None:
        if (
            read_bounded_symlink(target.current) != str(final)
            or read_bounded_symlink(target.launcher)
            != target.standalone_launcher_target
        ):
            raise DeliveryError(
                "activation_verification_failed",
                "a user launcher did not activate the release",
            )
        verify_installed_release(final, artifact)

    def _processes_active(self) -> bool:
        return any(
            sum(counts.values())
            for counts in self.process_scanner(self.config.targets).values()
        )

    def _append_event(self, event: dict[str, Any]) -> None:
        append_evidence_event(
            self.config.evidence_root / EVENTS_FILE,
            event,
        )

    def _append_failed(
        self,
        deployment_id: str,
        activated: list[str],
        failure_code: str,
    ) -> None:
        self._append_event(
            {
                "eventType": "failed",
                "deploymentId": deployment_id,
                "failureCode": failure_code,
                "activatedUsers": activated,
            }
        )
