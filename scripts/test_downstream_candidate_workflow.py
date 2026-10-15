#!/usr/bin/env python3
"""Guard the cost-sensitive ordering of the hosted release candidate graph."""

from pathlib import Path
import unittest


WORKFLOW = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "workflows"
    / "downstream-candidate.yml"
).read_text()


def job_body(name: str, next_name: str) -> str:
    start = WORKFLOW.index(f"\n  {name}:\n")
    end = WORKFLOW.index(f"\n  {next_name}:\n", start)
    return WORKFLOW[start:end]


class DownstreamCandidateWorkflowTests(unittest.TestCase):
    def test_native_waits_for_shared_validation(self) -> None:
        native = job_body("native", "root-usage")
        self.assertIn(
            "needs: [identity, preflight, rust-validation, bazel-validation]", native
        )

    def test_schema_generation_is_before_expensive_rust_tests(self) -> None:
        preflight = job_body("preflight", "rust-validation")
        rust = job_body("rust-validation", "bazel-validation")
        self.assertIn("Regenerate schemas before expensive tests", preflight)
        self.assertNotIn("Regenerate schemas", rust)

    def test_clippy_output_is_reclaimed_only_under_storage_pressure(self) -> None:
        rust = job_body("rust-validation", "bazel-validation")
        self.assertNotIn("        run: cargo clean\n", rust)
        self.assertIn("target_bytes > 8 * 1024 * 1024 * 1024", rust)
        self.assertIn("retaining it for the full suite", rust)

    def test_branch_candidates_cancel_when_superseded(self) -> None:
        self.assertIn(
            "cancel-in-progress: ${{ github.ref != 'refs/heads/main' && !startsWith(github.ref, 'refs/tags/') }}",
            WORKFLOW,
        )


if __name__ == "__main__":
    unittest.main()
