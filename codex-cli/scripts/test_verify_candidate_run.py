import copy
import unittest

from verify_candidate_run import verify_candidate_run


class VerifyCandidateRunTests(unittest.TestCase):
    def setUp(self):
        self.repository = "holyglory/codex"
        self.commit = "a" * 40
        self.run = {
            "status": "completed",
            "conclusion": "success",
            "path": ".github/workflows/downstream-candidate.yml",
            "event": "workflow_dispatch",
            "head_sha": self.commit,
            "repository": {"full_name": self.repository},
            "head_repository": {"full_name": self.repository},
        }

    def test_accepts_complete_successful_candidate(self):
        verify_candidate_run(self.run, self.repository, self.commit)

    def test_rejects_incomplete_failed_cancelled_or_skipped_candidate(self):
        for status, conclusion in (
            ("in_progress", None),
            ("queued", None),
            ("completed", "failure"),
            ("completed", "cancelled"),
            ("completed", "skipped"),
        ):
            with self.subTest(status=status, conclusion=conclusion):
                run = {**self.run, "status": status, "conclusion": conclusion}
                with self.assertRaises(ValueError):
                    verify_candidate_run(run, self.repository, self.commit)

    def test_rejects_wrong_source_workflow_trigger_or_missing_metadata(self):
        for field, value in (
            ("path", ".github/workflows/downstream-ci.yml"),
            ("event", "pull_request"),
            ("head_sha", "b" * 40),
            ("repository", {"full_name": "someone/codex"}),
            ("head_repository", {"full_name": "someone/codex"}),
            ("head_repository", None),
        ):
            with self.subTest(field=field, value=value):
                run = copy.deepcopy(self.run)
                run[field] = value
                with self.assertRaises(ValueError):
                    verify_candidate_run(run, self.repository, self.commit)
        for field in self.run:
            with self.subTest(missing=field):
                run = {key: value for key, value in self.run.items() if key != field}
                with self.assertRaises(ValueError):
                    verify_candidate_run(run, self.repository, self.commit)


if __name__ == "__main__":
    unittest.main()
