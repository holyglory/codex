import dataclasses
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from contextlib import redirect_stderr
from unittest.mock import patch

from software_owned_delivery import build_parser
import software_owned_delivery_lib as delivery


OLD_VERSION = "0.147.0"


def executable(version: str) -> bytes:
    return f"#!/bin/sh\nprintf 'codex-cli {version}\\n'\n".encode()


class DeliveryWorkflowTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.artifact_root = self.root / "artifacts"
        self.artifact_root.mkdir()
        self.evidence_root = self.root / "evidence"
        uid = os.getuid()
        gid = os.getgid()
        self.targets = (
            delivery.UserTarget(
                "holyglory",
                uid,
                gid,
                self.root / "homes" / "holyglory",
            ),
        )
        self.non_selected_targets = tuple(
            delivery.UserTarget(
                name,
                uid,
                gid,
                self.root / "homes" / name,
            )
            for name in ("slawa", "holygloryTT", "axel")
        )
        self.non_selected_homes = tuple(
            target.home for target in self.non_selected_targets
        )
        for target in self.non_selected_targets:
            target.home.mkdir(parents=True)
            (target.home / "sentinel").write_bytes(f"unchanged-{target.name}".encode())
            self.seed_user(target)
        for target in self.targets:
            self.seed_user(target)
        self.target_by_name = {
            target.name: target
            for target in (*self.targets, *self.non_selected_targets)
        }
        self.artifact, self.manifest = self.make_artifact()
        self.config = delivery.DeliveryConfig(
            self.targets,
            self.artifact_root,
            self.evidence_root,
            require_root=False,
            verify_system_users=False,
            manage_ownership=False,
        )
        self.workflow = delivery.DeliveryWorkflow(
            self.config,
            process_scanner=lambda targets: self.empty_process_counts(targets),
        )

    def seed_user(self, target: delivery.UserTarget) -> None:
        target.codex_home.mkdir(parents=True, mode=0o770)
        os.chmod(target.codex_home, 0o770)
        target.launcher.parent.mkdir(parents=True)
        credential = target.codex_home / "auth.json"
        credential.write_bytes(f"private-{target.name}".encode())
        os.chmod(credential, 0o600)
        release = target.releases / f"{OLD_VERSION}-{delivery.TARGET}"
        (release / "bin").mkdir(parents=True)
        self.write_executable(release / "bin" / "codex", OLD_VERSION)
        self.write_executable(release / "bin" / "codex-code-mode-host", OLD_VERSION)
        target.current.symlink_to(str(release))
        target.launcher.symlink_to(target.standalone_launcher_target)

    def write_executable(self, path: Path, version: str) -> None:
        path.write_bytes(executable(version))
        path.chmod(0o755)

    def make_artifact(
        self,
        *,
        unsafe_member: str | None = None,
        missing_component: str | None = None,
    ) -> tuple[Path, Path]:
        package = self.root / f"package-{len(list(self.artifact_root.iterdir()))}"
        package.mkdir()
        metadata = {
            "layoutVersion": 1,
            "version": delivery.VERSION,
            "target": delivery.TARGET,
            "variant": "codex",
            "entrypoint": "bin/codex",
            "resourcesDir": "codex-resources",
            "pathDir": "codex-path",
        }
        files = {
            "codex-package.json": json.dumps(metadata).encode(),
            "bin/codex": executable(delivery.VERSION),
            "bin/codex-code-mode-host": executable(delivery.VERSION),
            "codex-path/rg": b"#!/bin/sh\nexit 0\n",
            "codex-resources/bwrap": b"#!/bin/sh\nexit 0\n",
        }
        for name, contents in files.items():
            if name == missing_component:
                continue
            path = package / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)
            path.chmod(0o755 if name in delivery.EXECUTABLE_COMPONENTS else 0o644)
        artifact = self.artifact_root / f"package-{package.name}.tar.gz"
        with tarfile.open(artifact, "w:gz") as archive:
            for path in sorted(package.rglob("*")):
                archive.add(path, arcname=path.relative_to(package), recursive=False)
            if unsafe_member is not None:
                member = tarfile.TarInfo(unsafe_member)
                member.size = 1
                archive.addfile(member, io.BytesIO(b"x"))
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        manifest = self.artifact_root / f"{artifact.stem}.sha256"
        manifest.write_text(f"{digest}  {artifact.name}\n", encoding="ascii")
        return artifact, manifest

    @staticmethod
    def empty_process_counts(
        targets: tuple[delivery.UserTarget, ...],
    ) -> dict[str, dict[str, int]]:
        return {
            target.name: {"appServer": 0, "codeModeHost": 0, "otherCodex": 0}
            for target in targets
        }

    def credential_snapshot(self) -> dict[str, tuple[bytes, int]]:
        return {
            target.name: (
                (target.codex_home / "auth.json").read_bytes(),
                stat.S_IMODE((target.codex_home / "auth.json").stat().st_mode),
            )
            for target in self.targets
        }

    def non_selected_snapshot(self) -> dict[str, tuple[tuple, ...]]:
        return {home.name: self.tree_snapshot(home) for home in self.non_selected_homes}

    def tree_snapshot(self, home: Path) -> tuple[tuple, ...]:
        return tuple(
            self.snapshot_entry(home, path) for path in (home, *sorted(home.rglob("*")))
        )

    @staticmethod
    def snapshot_entry(home: Path, path: Path) -> tuple:
        relative = "." if path == home else str(path.relative_to(home))
        mode = stat.S_IMODE(path.lstat().st_mode)
        if path.is_symlink():
            return (relative, "symlink", mode, os.readlink(path))
        if path.is_dir():
            return (relative, "directory", mode)
        return (relative, "file", mode, path.read_bytes())

    def apply(self) -> dict:
        return self.apply_with(self.workflow)

    def workflow_for(self, target: delivery.UserTarget) -> delivery.DeliveryWorkflow:
        return delivery.DeliveryWorkflow(
            dataclasses.replace(self.config, targets=(target,)),
            process_scanner=lambda targets: self.empty_process_counts(targets),
        )

    def apply_with(self, workflow: delivery.DeliveryWorkflow) -> dict:
        plan = workflow.plan(self.artifact, self.manifest)
        fingerprint = plan.fingerprint()
        return workflow.apply(
            self.artifact,
            self.manifest,
            fingerprint,
            f"deploy:{delivery.VERSION}:{fingerprint}",
        )

    def test_plan_is_content_free_and_does_not_mutate_targets(self) -> None:
        credentials = self.credential_snapshot()
        plan = self.workflow.plan(self.artifact, self.manifest)

        public = plan.public_payload()
        self.assertTrue(public["ready"])
        self.assertEqual(
            ["holyglory"],
            [user["user"] for user in public["users"]],
        )
        self.assertNotIn(str(self.root), json.dumps(public))
        self.assertFalse(self.evidence_root.exists())
        self.assertEqual(credentials, self.credential_snapshot())

    def test_apply_installs_only_holyglory(self) -> None:
        credentials = self.credential_snapshot()
        non_selected = self.non_selected_snapshot()
        result = self.apply()

        self.assertEqual(["holyglory"], result["users"])
        for target in self.targets:
            final = target.releases / delivery.RELEASE_NAME
            self.assertEqual(str(final), os.readlink(target.current))
            self.assertEqual(
                target.standalone_launcher_target, os.readlink(target.launcher)
            )
            self.assertEqual(
                delivery.VERSION, delivery.executable_version(final / "bin" / "codex")
            )
        events = delivery.read_evidence_events(
            self.evidence_root / delivery.EVENTS_FILE
        )
        self.assertEqual(
            [
                "prepared",
                "activated",
                "completed",
            ],
            [event["eventType"] for event in events],
        )
        self.assertEqual(["holyglory"], [user["name"] for user in events[0]["users"]])
        self.assertEqual(credentials, self.credential_snapshot())
        self.assertEqual(non_selected, self.non_selected_snapshot())

    def test_rollback_restores_exact_launchers_and_preserves_failed_release(
        self,
    ) -> None:
        credentials = self.credential_snapshot()
        result = self.apply()
        rollback = self.workflow.rollback(
            result["deploymentId"], f"rollback:{result['deploymentId']}"
        )

        self.assertTrue(rollback["failedReleasePreserved"])
        for target in self.targets:
            final = target.releases / delivery.RELEASE_NAME
            self.assertTrue(final.is_dir())
            self.assertIn(OLD_VERSION, os.readlink(target.current))
            self.assertEqual(
                target.standalone_launcher_target, os.readlink(target.launcher)
            )
        self.assertEqual(credentials, self.credential_snapshot())

    def test_process_gate_refuses_apply_without_creating_evidence(self) -> None:
        def active(
            targets: tuple[delivery.UserTarget, ...],
        ) -> dict[str, dict[str, int]]:
            counts = self.empty_process_counts(targets)
            counts["holyglory"]["appServer"] = 1
            return counts

        workflow = delivery.DeliveryWorkflow(self.config, process_scanner=active)
        plan = workflow.plan(self.artifact, self.manifest)
        self.assertEqual(("codex_processes_active",), plan.blockers)
        with self.assertRaisesRegex(delivery.DeliveryError, "preflight"):
            workflow.apply(
                self.artifact,
                self.manifest,
                plan.fingerprint(),
                f"deploy:{delivery.VERSION}:{plan.fingerprint()}",
            )
        self.assertFalse(self.evidence_root.exists())

    def test_existing_release_is_never_clobbered(self) -> None:
        final = self.targets[0].releases / delivery.RELEASE_NAME
        final.mkdir()
        marker = final / "marker"
        marker.write_text("preserve", encoding="ascii")
        plan = self.workflow.plan(self.artifact, self.manifest)

        self.assertIn("release_directory_exists", plan.blockers)
        with self.assertRaises(delivery.DeliveryError):
            self.workflow.apply(
                self.artifact,
                self.manifest,
                plan.fingerprint(),
                f"deploy:{delivery.VERSION}:{plan.fingerprint()}",
            )
        self.assertEqual("preserve", marker.read_text(encoding="ascii"))

    def test_checksum_and_component_validation_fail_closed(self) -> None:
        self.manifest.write_text(
            f"{'0' * 64}  {self.artifact.name}\n", encoding="ascii"
        )
        with self.assertRaisesRegex(delivery.DeliveryError, "checksum"):
            self.workflow.plan(self.artifact, self.manifest)

        artifact, manifest = self.make_artifact(
            missing_component="codex-resources/bwrap"
        )
        with self.assertRaisesRegex(delivery.DeliveryError, "component"):
            self.workflow.plan(artifact, manifest)

    def test_unsafe_archive_member_is_rejected(self) -> None:
        artifact, manifest = self.make_artifact(unsafe_member="../outside")
        with self.assertRaisesRegex(delivery.DeliveryError, "unsafe"):
            self.workflow.plan(artifact, manifest)
        self.assertFalse((self.root / "outside").exists())

    def test_unexpected_launcher_target_is_rejected(self) -> None:
        target = self.targets[0]
        target.launcher.unlink()
        target.launcher.symlink_to("/unexpected/codex")
        with self.assertRaisesRegex(delivery.DeliveryError, "unexpected"):
            self.workflow.plan(self.artifact, self.manifest)

    def test_changed_plan_confirmation_is_rejected(self) -> None:
        plan = self.workflow.plan(self.artifact, self.manifest)
        with self.assertRaisesRegex(delivery.DeliveryError, "changed"):
            self.workflow.apply(
                self.artifact,
                self.manifest,
                "0" * 64,
                f"deploy:{delivery.VERSION}:{'0' * 64}",
            )
        self.assertFalse(self.evidence_root.exists())

    def test_tampered_evidence_blocks_rollback_without_changing_links(self) -> None:
        result = self.apply()
        links_before = {
            target.name: (os.readlink(target.launcher), os.readlink(target.current))
            for target in self.targets
        }
        evidence = self.evidence_root / delivery.EVENTS_FILE
        lines = evidence.read_text(encoding="ascii").splitlines()
        first = json.loads(lines[0])
        first["version"] = "tampered"
        lines[0] = json.dumps(first, sort_keys=True, separators=(",", ":"))
        evidence.write_text("\n".join(lines) + "\n", encoding="ascii")
        evidence.chmod(0o600)

        with self.assertRaisesRegex(delivery.DeliveryError, "integrity"):
            self.workflow.rollback(
                result["deploymentId"], f"rollback:{result['deploymentId']}"
            )
        self.assertEqual(
            links_before,
            {
                target.name: (os.readlink(target.launcher), os.readlink(target.current))
                for target in self.targets
            },
        )

    def test_target_allowlist_is_exact(self) -> None:
        invalid_targets = (
            (),
            (
                self.targets[0],
                delivery.UserTarget(
                    "slawa",
                    os.getuid(),
                    os.getgid(),
                    self.root / "homes" / "slawa",
                ),
            ),
            (
                delivery.UserTarget(
                    "unknown",
                    os.getuid(),
                    os.getgid(),
                    self.root / "homes" / "unknown",
                ),
            ),
        )
        for targets in invalid_targets:
            with self.subTest(targets=tuple(target.name for target in targets)):
                workflow = delivery.DeliveryWorkflow(
                    dataclasses.replace(self.config, targets=targets),
                    process_scanner=lambda selected: self.empty_process_counts(
                        selected
                    ),
                )
                with self.assertRaisesRegex(delivery.DeliveryError, "target"):
                    workflow.plan(self.artifact, self.manifest)
                with self.assertRaisesRegex(delivery.DeliveryError, "target"):
                    workflow.rollback("deployment", "rollback:deployment")

    def test_production_config_has_exact_approved_targets(self) -> None:
        expected = {
            "holyglory": (1000, 1003, Path("/home/holyglory")),
            "slawa": (1003, 1006, Path("/home/slawa")),
        }
        actual = {}
        for name in expected:
            targets = delivery.production_config(name).targets
            self.assertEqual(1, len(targets))
            target = targets[0]
            actual[name] = (target.uid, target.gid, target.home)
        self.assertEqual(expected, actual)
        for name in ("holygloryTT", "axel", "unknown"):
            with self.subTest(name=name):
                with self.assertRaisesRegex(delivery.DeliveryError, "restricted"):
                    delivery.production_config(name)

    def test_cli_requires_one_explicit_approved_target(self) -> None:
        parser = build_parser()
        run = [
            "run",
            "--artifact",
            str(self.artifact),
            "--checksum-manifest",
            str(self.manifest),
        ]
        rollback = ["rollback", "--deployment-id", "deployment", "--confirm", "x"]
        with redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                parser.parse_args(run)
            with self.assertRaises(SystemExit):
                parser.parse_args(rollback)
            for name in ("holygloryTT", "axel"):
                with self.subTest(name=name):
                    with self.assertRaises(SystemExit):
                        parser.parse_args([*run, "--user", name])
                    with self.assertRaises(SystemExit):
                        parser.parse_args([*rollback, "--user", name])
            with self.assertRaises(SystemExit):
                parser.parse_args([*run, "--user", "holyglory", "--user", "slawa"])
            with self.assertRaises(SystemExit):
                parser.parse_args([*rollback, "--user", "holyglory", "--user", "slawa"])
        for name in ("holyglory", "slawa"):
            with self.subTest(name=name):
                self.assertEqual(name, parser.parse_args([*run, "--user", name]).user)
                self.assertEqual(
                    name,
                    parser.parse_args([*rollback, "--user", name]).user,
                )

    def test_verified_config_rejects_changed_production_metadata(self) -> None:
        changed_target = delivery.UserTarget(
            "holyglory",
            9999,
            9999,
            Path("/unexpected/home"),
        )
        for require_root, verify_system_users in ((True, False), (False, True)):
            with self.subTest(
                require_root=require_root,
                verify_system_users=verify_system_users,
            ):
                config = dataclasses.replace(
                    self.config,
                    targets=(changed_target,),
                    require_root=require_root,
                    verify_system_users=verify_system_users,
                )
                workflow = delivery.DeliveryWorkflow(
                    config,
                    process_scanner=lambda targets: self.empty_process_counts(targets),
                )
                with self.assertRaisesRegex(delivery.DeliveryError, "fixed production"):
                    workflow.plan(self.artifact, self.manifest)

    def test_verified_config_checks_runtime_passwd_identity(self) -> None:
        for name in ("holyglory", "slawa"):
            target = delivery.production_config(name).targets[0]
            config = delivery.DeliveryConfig(
                (target,),
                self.artifact_root,
                self.evidence_root,
                require_root=False,
                verify_system_users=True,
                manage_ownership=False,
            )
            workflow = delivery.DeliveryWorkflow(
                config,
                process_scanner=lambda targets: self.empty_process_counts(targets),
            )
            correct = SimpleNamespace(
                pw_uid=target.uid,
                pw_gid=target.gid,
                pw_dir=str(target.home),
            )
            with self.subTest(name=name, state="correct"):
                with patch(
                    "software_owned_delivery_lib.workflow.pwd.getpwnam",
                    return_value=correct,
                ):
                    workflow._validate_invocation_identity()
            with self.subTest(name=name, state="missing"):
                with patch(
                    "software_owned_delivery_lib.workflow.pwd.getpwnam",
                    side_effect=KeyError(name),
                ):
                    with self.assertRaisesRegex(delivery.DeliveryError, "unavailable"):
                        workflow._validate_invocation_identity()
            mismatches = (
                SimpleNamespace(
                    pw_uid=target.uid + 1,
                    pw_gid=target.gid,
                    pw_dir=str(target.home),
                ),
                SimpleNamespace(
                    pw_uid=target.uid,
                    pw_gid=target.gid + 1,
                    pw_dir=str(target.home),
                ),
                SimpleNamespace(
                    pw_uid=target.uid,
                    pw_gid=target.gid,
                    pw_dir="/unexpected/home",
                ),
            )
            for account in mismatches:
                with self.subTest(name=name, account=account):
                    with patch(
                        "software_owned_delivery_lib.workflow.pwd.getpwnam",
                        return_value=account,
                    ):
                        with self.assertRaisesRegex(
                            delivery.DeliveryError, "identity changed"
                        ):
                            workflow._validate_invocation_identity()

    def test_rollback_rejects_evidence_for_another_approved_target(self) -> None:
        slawa = self.target_by_name["slawa"]
        workflow = self.workflow_for(slawa)
        result = self.apply_with(workflow)
        evidence = self.evidence_root / delivery.EVENTS_FILE
        before = {
            "holyglory": self.tree_snapshot(self.targets[0].home),
            "slawa": self.tree_snapshot(slawa.home),
            "evidence": (
                evidence.read_bytes(),
                stat.S_IMODE(evidence.stat().st_mode),
            ),
        }

        with self.assertRaisesRegex(delivery.DeliveryError, "targets do not match"):
            self.workflow.rollback(
                result["deploymentId"], f"rollback:{result['deploymentId']}"
            )
        self.assertEqual(
            before,
            {
                "holyglory": self.tree_snapshot(self.targets[0].home),
                "slawa": self.tree_snapshot(slawa.home),
                "evidence": (
                    evidence.read_bytes(),
                    stat.S_IMODE(evidence.stat().st_mode),
                ),
            },
        )

    def test_slawa_rollback_rejects_holyglory_evidence_without_mutation(
        self,
    ) -> None:
        result = self.apply()
        slawa = self.target_by_name["slawa"]
        workflow = self.workflow_for(slawa)
        evidence = self.evidence_root / delivery.EVENTS_FILE
        before = {
            "holyglory": self.tree_snapshot(self.targets[0].home),
            "slawa": self.tree_snapshot(slawa.home),
            "evidence": (
                evidence.read_bytes(),
                stat.S_IMODE(evidence.stat().st_mode),
            ),
        }

        with self.assertRaisesRegex(delivery.DeliveryError, "targets do not match"):
            workflow.rollback(
                result["deploymentId"], f"rollback:{result['deploymentId']}"
            )
        self.assertEqual(
            before,
            {
                "holyglory": self.tree_snapshot(self.targets[0].home),
                "slawa": self.tree_snapshot(slawa.home),
                "evidence": (
                    evidence.read_bytes(),
                    stat.S_IMODE(evidence.stat().st_mode),
                ),
            },
        )

    def test_slawa_rollback_remains_valid_after_holyglory_deployment(self) -> None:
        slawa = self.target_by_name["slawa"]
        workflow = self.workflow_for(slawa)
        credentials = (slawa.codex_home / "auth.json").read_bytes()
        slawa_result = self.apply_with(workflow)
        slawa_after_apply = self.tree_snapshot(slawa.home)

        self.apply()
        self.assertEqual(slawa_after_apply, self.tree_snapshot(slawa.home))
        holyglory_after_apply = self.tree_snapshot(self.targets[0].home)
        rollback = workflow.rollback(
            slawa_result["deploymentId"],
            f"rollback:{slawa_result['deploymentId']}",
        )

        self.assertEqual(["slawa"], rollback["users"])
        self.assertTrue(rollback["failedReleasePreserved"])
        self.assertIn(OLD_VERSION, os.readlink(slawa.current))
        self.assertTrue((slawa.releases / delivery.RELEASE_NAME).is_dir())
        self.assertEqual(credentials, (slawa.codex_home / "auth.json").read_bytes())
        self.assertEqual(
            holyglory_after_apply,
            self.tree_snapshot(self.targets[0].home),
        )


if __name__ == "__main__":
    unittest.main()
