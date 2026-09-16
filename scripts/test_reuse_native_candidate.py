import copy
import hashlib
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import reuse_native_candidate as reuse


class NativeCandidateReuseTests(unittest.TestCase):
    def setUp(self):
        self.commit = "a" * 40
        self.run = {
            "status": "completed",
            "conclusion": "failure",
            "path": reuse.WORKFLOW,
            "event": "workflow_dispatch",
            "head_sha": self.commit,
            "repository": {"full_name": reuse.REPOSITORY},
            "head_repository": {"full_name": reuse.REPOSITORY},
        }
        self.jobs = {
            "jobs": [
                {"name": name, "status": "completed", "conclusion": "success"}
                for name in sorted(reuse.REQUIRED_JOBS)
            ],
            "total_count": len(reuse.REQUIRED_JOBS),
        }
        self.workflow = "env:\n  BUILD_SETTING: fixed\njobs:\n" + "".join(
            f"  {job}:\n    name: {job}\n    steps:\n      - run: build\n"
            for job in reuse.BUILD_JOBS
        )

    def test_accepts_complete_native_and_validation_results_despite_packaging_failure(
        self,
    ):
        self.assertEqual(reuse.validate_run(self.run, self.jobs), self.commit)

    def test_rejects_wrong_identity_and_incomplete_or_failed_component_jobs(self):
        for field, value in (
            ("status", "in_progress"),
            ("event", "pull_request"),
            ("path", ".github/workflows/other.yml"),
            ("head_sha", "invalid"),
            ("conclusion", "cancelled"),
            ("repository", None),
            ("head_repository", {"full_name": "other/codex"}),
        ):
            with self.subTest(field=field), self.assertRaises(ValueError):
                reuse.validate_run({**self.run, field: value}, self.jobs)
        for name in reuse.REQUIRED_JOBS:
            for conclusion in ("failure", "cancelled", "skipped"):
                jobs = copy.deepcopy(self.jobs)
                next(job for job in jobs["jobs"] if job["name"] == name)[
                    "conclusion"
                ] = conclusion
                with (
                    self.subTest(job=name, conclusion=conclusion),
                    self.assertRaises(ValueError),
                ):
                    reuse.validate_run(self.run, jobs)
        for jobs in (
            {**self.jobs, "total_count": 100},
            {
                **self.jobs,
                "jobs": self.jobs["jobs"][:-1],
                "total_count": len(self.jobs["jobs"]) - 1,
            },
        ):
            with self.assertRaises(ValueError):
                reuse.validate_run(self.run, jobs)
        unexpected = {
            "name": "Additional validation",
            "status": "completed",
            "conclusion": "failure",
        }
        with self.assertRaises(ValueError):
            reuse.validate_run(
                self.run,
                {
                    "jobs": self.jobs["jobs"] + [unexpected],
                    "total_count": len(self.jobs["jobs"]) + 1,
                },
            )

    def test_allows_packaging_changes_and_only_exact_build_skip_guards(self):
        after = self.workflow
        for job in reuse.BUILD_JOBS:
            after = after.replace(f"  {job}:\n", f"  {job}:\n{reuse.SKIP_GUARD}")
        reuse.validate_changes(
            {"scripts/stage_npm_packages.py", reuse.WORKFLOW}, self.workflow, after
        )
        for paths, text in (
            ({"codex-rs/core/src/lib.rs"}, after),
            ({".github/actions/setup-ci/action.yml"}, after),
            ({reuse.WORKFLOW}, after.replace("run: build", "run: different", 1)),
            (
                {reuse.WORKFLOW},
                after.replace("BUILD_SETTING: fixed", "BUILD_SETTING: changed"),
            ),
            ({reuse.WORKFLOW}, after.replace("native:", "missing:", 1)),
        ):
            with self.subTest(paths=paths), self.assertRaises(ValueError):
                reuse.validate_changes(paths, self.workflow, text)

    def test_verifies_every_archive_and_rejects_corruption_or_failed_attestation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for target in reuse.BINARY_TARGETS:
                folder = root / target
                folder.mkdir()
                payload = f"isolated native archive {target}".encode()
                name = f"codex-package-{target}.tar.gz"
                (folder / name).write_bytes(payload)
                (folder / "SHA256SUMS").write_text(
                    f"{hashlib.sha256(payload).hexdigest()}  {name}\n"
                )
            with patch.object(reuse.subprocess, "run") as attest:
                reuse.verify_archives(root, self.commit)
                self.assertEqual(attest.call_count, len(reuse.BINARY_TARGETS))
                for call in attest.call_args_list:
                    self.assertIn("--deny-self-hosted-runners", call.args[0])
                    self.assertEqual(
                        call.args[0][call.args[0].index("--source-digest") + 1],
                        self.commit,
                    )
                    self.assertTrue(call.kwargs["check"])
            with patch.object(
                reuse.subprocess,
                "run",
                side_effect=subprocess.CalledProcessError(1, "gh"),
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    reuse.verify_archives(root, self.commit)
            first = reuse.BINARY_TARGETS[0]
            (root / first / f"codex-package-{first}.tar.gz").write_bytes(b"changed")
            with patch.object(reuse.subprocess, "run") as attest:
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    reuse.verify_archives(root, self.commit)
                attest.assert_not_called()


if __name__ == "__main__":
    unittest.main()
