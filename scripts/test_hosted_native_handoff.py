"""Exercise per-target Actions imports through download, extraction and receipt checks."""

from contextlib import ExitStack
import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import hosted_native_handoff as handoff
import release_identity as release


class HostedNativeHandoffTest(unittest.TestCase):
    def setUp(self):
        self.sources = {"123": "a" * 40, "456": "b" * 40}
        self.runs = dict(zip(handoff.candidate.HOSTED_TARGETS, ("123", "456", "456")))
        self.records, self.payloads, self.receipts = {}, {}, {}
        self.time = "2026-11-14T12:00:00Z"
        for run_id, source in self.sources.items():
            run = {
                "id": int(run_id),
                "run_attempt": 1,
                "head_sha": source,
                "status": "completed",
                "conclusion": "failure",
                "path": ".github/workflows/downstream-candidate.yml",
                "event": "workflow_dispatch",
                "repository": {"full_name": handoff.REPOSITORY},
                "head_repository": {"full_name": handoff.REPOSITORY},
            }
            identity = self.job(
                handoff.candidate.CANDIDATE_JOBS[0],
                (
                    "Check the completed local proof and permitted packaging-only diff",
                    "Verify the seven-package fixture and release receipt regressions",
                    "Verify the optional hosted V8 producer",
                ),
            )
            jobs = [identity]
            for target in handoff.candidate.HOSTED_TARGETS:
                v8 = self.job(
                    f"Build and test {target} / V8 source pair",
                    (
                        "Build and seal the matching V8 pair",
                        "Require a complete V8 handoff before upload",
                        handoff.candidate.UPLOAD_STEP,
                    ),
                )
                if run_id == "456":
                    v8.update(conclusion="skipped", steps=[])
                package = self.job(
                    f"Build and test {target} / Package native target",
                    (
                        "Verify the V8 handoff before package setup",
                        "Verify the V8 handoff, run native regressions, and assemble packages",
                        "Download V8 handoff from this run"
                        if run_id == "123"
                        else "Download V8 handoff from the producer run",
                        handoff.candidate.UPLOAD_STEP,
                    ),
                )
                if self.runs[target] != run_id:
                    package["conclusion"] = "failure"
                jobs.extend((v8, package))
            self.records[f"actions/runs/{run_id}"] = run
            self.records[f"actions/runs/{run_id}/jobs"] = {
                "total_count": len(jobs),
                "jobs": jobs,
            }
            self.records[f"actions/runs/{run_id}/artifacts"] = {
                "total_count": 0,
                "artifacts": [],
            }
        self.records["actions/runs/123/attempts/1"] = self.records["actions/runs/123"]
        self.records["actions/runs/123/attempts/1/jobs"] = self.records[
            "actions/runs/123/jobs"
        ]
        for number, target in enumerate(handoff.candidate.HOSTED_TARGETS, 1):
            source = self.sources[self.runs[target]]
            archive = f"codex-package-{target}.tar.gz"
            receipt = {
                "schema": 1,
                "status": "success",
                "source_commit": source,
                "native_tree": "native",
                "v8_tree": "v8",
                "target": target,
                "version": "0.166.0+multi.8",
                "runtime_executed": True,
                "checks": [
                    {
                        "name": "native-regressions",
                        "exit_code": 0,
                        "log_sha256": "1" * 64,
                    }
                ],
                "archives": {
                    archive: {
                        "sha256": hashlib.sha256(b"native bytes").hexdigest(),
                        "size_bytes": 12,
                    }
                },
                "v8_pair": {
                    "source_commit": self.sources["123"],
                    "producer_run_id": "123",
                    "producer_run_attempt": "1",
                    "source_built": True,
                    "profile": "ptrcomp_sandbox_release",
                    "cppgc_ignored_tests_executed": True,
                    "files": {
                        f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz": "1" * 64,
                        f"src_binding_ptrcomp_sandbox_release_{target}.rs": "2" * 64,
                    },
                },
            }
            self.receipts[target] = json.dumps(receipt).encode()
            artifact = {
                "id": number,
                "name": target,
                "expired": False,
                "created_at": self.time,
                "workflow_run": {"id": int(self.runs[target]), "head_sha": source},
            }
            record = self.records[f"actions/runs/{self.runs[target]}/artifacts"]
            record["artifacts"].append(artifact)
            record["total_count"] += 1
            self.zip(target)
        stack = ExitStack()
        self.addCleanup(stack.close)
        stack.enter_context(
            patch.object(
                handoff,
                "api",
                side_effect=lambda path: copy.deepcopy(
                    self.records[path.split("?")[0]]
                ),
            )
        )
        opener = stack.enter_context(patch("runner_handoff.build_opener"))
        opener.return_value.open.side_effect = lambda request, **_: io.BytesIO(
            self.payloads[int(request.full_url.split("/")[-2])]
        )
        stack.enter_context(patch.object(handoff, "verify_native_source"))
        stack.enter_context(patch.object(handoff, "verify_hosted_native_source"))
        stack.enter_context(patch.object(handoff, "verify_hosted_v8_source"))
        stack.enter_context(
            patch.object(
                release,
                "identity",
                return_value={"native_tree": "native", "v8_tree": "v8"},
            )
        )
        stack.enter_context(
            patch.object(
                release,
                "verify_native_source",
                return_value={"native_producer": {"native_tree": "native"}},
            )
        )
        stack.enter_context(patch.object(release, "git", return_value="v8"))
        stack.enter_context(
            patch.object(
                release, "v8_input_manifest", return_value={"sha256": "3" * 64}
            )
        )
        stack.enter_context(
            patch.object(
                release, "version", return_value=("0.166.0+multi.8", "0.166.0-multi.8")
            )
        )

    def job(self, name, steps):
        return {
            "id": 999,
            "run_attempt": 1,
            "name": name,
            "status": "completed",
            "conclusion": "success",
            "steps": [
                {
                    "name": step,
                    "status": "completed",
                    "conclusion": "success",
                    "started_at": self.time,
                    "completed_at": self.time,
                }
                for step in steps
            ],
        }

    def zip(self, target, *, archive_bytes=b"native bytes", extras=()):
        artifact = next(
            a
            for a in self.records[f"actions/runs/{self.runs[target]}/artifacts"][
                "artifacts"
            ]
            if a["name"] == target
        )
        output = io.BytesIO()
        with zipfile.ZipFile(output, "w") as archive:
            archive.writestr("native-manifest.json", self.receipts[target])
            archive.writestr(f"codex-package-{target}.tar.gz", archive_bytes)
            for name, content in extras:
                archive.writestr(name, content)
        data = output.getvalue()
        self.payloads[artifact["id"]] = data
        artifact.update(
            size_in_bytes=len(data), digest="sha256:" + hashlib.sha256(data).hexdigest()
        )

    def import_targets(self, destination):
        return handoff.import_targets(
            self.runs, handoff.REPOSITORY, "d" * 40, "c" * 40, destination
        )

    def test_partial_runs_preserve_three_original_receipts_and_require_final_green(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "imported"
            result = self.import_targets(destination)
            self.assertEqual(
                [p["target"] for p in result["native_receipts"]],
                list(handoff.candidate.HOSTED_TARGETS),
            )
            for target in handoff.candidate.HOSTED_TARGETS:
                self.assertEqual(
                    (destination / target / "native-manifest.json").read_bytes(),
                    self.receipts[target],
                )
            self.assertEqual(
                json.loads((destination / "hosted-native-producers.json").read_text()),
                result,
            )
        jobs = [self.job(name, ("check",)) for name in handoff.candidate.CANDIDATE_JOBS]
        with self.assertRaisesRegex(ValueError, "exact-source"):
            handoff.candidate.verify_candidate_run(
                self.records["actions/runs/456"],
                handoff.REPOSITORY,
                self.sources["456"],
                jobs,
            )

    def test_failed_or_incomplete_selected_target_is_rejected(self):
        jobs = self.records["actions/runs/123/jobs"]["jobs"]
        selected = next(
            job
            for job in jobs
            if job["name"] == handoff.candidate.HOSTED_PACKAGE_JOBS[0]
        )
        original = copy.deepcopy(selected)
        for failure in (
            "job",
            "missing-step",
            "failed-step",
            "incomplete-step",
            "missing-upload",
        ):
            with (
                self.subTest(failure=failure),
                tempfile.TemporaryDirectory() as directory,
            ):
                selected.clear()
                selected.update(copy.deepcopy(original))
                if failure == "job":
                    selected["conclusion"] = "failure"
                elif failure == "missing-step":
                    selected["steps"].pop(0)
                elif failure == "missing-upload":
                    selected["steps"].pop()
                else:
                    selected["steps"][0][
                        "conclusion" if failure == "failed-step" else "status"
                    ] = "failure" if failure == "failed-step" else "in_progress"
                with self.assertRaisesRegex(ValueError, "Hosted target|hosted target"):
                    self.import_targets(Path(directory))
                self.assertFalse(
                    (Path(directory) / handoff.candidate.HOSTED_TARGETS[0]).exists()
                )

    def test_artifact_source_digest_expiry_and_upload_window_are_verified(self):
        artifact = self.records["actions/runs/123/artifacts"]["artifacts"][0]
        original = copy.deepcopy(artifact)
        for field, value in (
            ("workflow_run", {"id": 123, "head_sha": "e" * 40}),
            ("digest", "sha256:" + "0" * 64),
            ("expired", True),
            ("created_at", "2026-11-14T13:00:00Z"),
            ("name", "other-target"),
        ):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                artifact.clear()
                artifact.update(copy.deepcopy(original))
                artifact[field] = value
                with self.assertRaises(ValueError):
                    self.import_targets(Path(directory))

    def test_original_zip_cannot_change_archive_receipt_or_paths(self):
        target = handoff.candidate.HOSTED_TARGETS[0]
        for failure in ("archive", "receipt", "path", "duplicate"):
            with (
                self.subTest(failure=failure),
                tempfile.TemporaryDirectory() as directory,
            ):
                original = self.receipts[target]
                if failure == "receipt":
                    receipt = json.loads(original)
                    receipt["source_commit"] = "e" * 40
                    self.receipts[target] = json.dumps(receipt).encode()
                extras = (
                    (("../escaped", b"bad"),)
                    if failure == "path"
                    else (("native-manifest.json", b"duplicate"),)
                    if failure == "duplicate"
                    else ()
                )
                self.zip(
                    target,
                    archive_bytes=b"wrong bytes!"
                    if failure == "archive"
                    else b"native bytes",
                    extras=extras,
                )
                with self.assertRaises(ValueError):
                    self.import_targets(Path(directory))
                self.receipts[target] = original

    def test_expanded_zip_size_is_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "expanded.zip"
            target = handoff.candidate.HOSTED_TARGETS[0]
            with zipfile.ZipFile(
                archive, "w", compression=zipfile.ZIP_DEFLATED
            ) as output:
                output.writestr("native-manifest.json", "{}")
                output.writestr(f"codex-package-{target}.tar.gz", b"0" * 1024)
            with (
                patch.object(handoff, "MAX_BYTES", 512),
                self.assertRaisesRegex(ValueError, "oversized"),
            ):
                handoff.extract(archive, root / "target", target)

    def test_mapping_requires_exact_targets_and_numeric_runs(self):
        self.assertEqual(handoff.parse_runs(json.dumps(self.runs)), self.runs)
        for invalid in (
            {},
            {**self.runs, "other": "123"},
            {**self.runs, next(iter(self.runs)): True},
            {**self.runs, next(iter(self.runs)): "invalid"},
            list(self.runs.items()),
        ):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                handoff.parse_runs(json.dumps(invalid))
        with self.assertRaises(ValueError):
            handoff.parse_runs(
                '{"aarch64-unknown-linux-musl":123,"aarch64-unknown-linux-musl":123,"x86_64-apple-darwin":456}'
            )


if __name__ == "__main__":
    unittest.main()
