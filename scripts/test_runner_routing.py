from pathlib import Path
import json
import os
import re
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]


class RunnerRoutingTests(unittest.TestCase):
    def test_candidate_requires_external_handoffs_and_keeps_only_public_packaging_targets(
        self,
    ):
        workflow = (ROOT / ".github/workflows/downstream-candidate.yml").read_text()
        self.assertIn("linux_handoff:", workflow)
        self.assertIn("windows_handoff:", workflow)
        self.assertIn("runner-handoff-${{ github.sha }}", workflow)
        self.assertNotIn("runner: windows-2025", workflow)
        self.assertNotIn("runner: windows-11-arm", workflow)
        self.assertNotIn(
            "runner: ubuntu-24.04\n            target: x86_64-unknown-linux-musl",
            workflow,
        )
        self.assertNotIn("--workflow-url", workflow)
        self.assertIn("x86_64-pc-windows-msvc", workflow)
        self.assertIn("aarch64-pc-windows-msvc", workflow)

    def test_fork_routes_compilers_outside_github_and_enforces_policy_failures(self):
        workflow = (ROOT / ".github/workflows/blocking-ci.yml").read_text()
        guard = "if: ${{ github.repository != 'holyglory/codex' }}"
        self.assertEqual(workflow.count(guard), 3)
        for file, jobs in {
            "downstream-ci.yml": ("quality", "downstream-tests", "linux-compile"),
            "postmerge-ci.yml": ("rust-ci-full", "v8-canary"),
            "v8-canary.yml": ("metadata",),
            "downstream-windows-storage.yml": ("private-storage",),
            "downstream-windows-symbols.yml": ("symbols",),
        }.items():
            content = (ROOT / ".github/workflows" / file).read_text()
            for job in jobs:
                with self.subTest(workflow=file, job=job):
                    block = re.split(
                        r"\n  (?=\S)", content.split(f"\n  {job}:\n", 1)[1], maxsplit=1
                    )[0]
                    self.assertIn(guard, block)
        policy = workflow.split("  fork-policy:")[1]
        self.assertIn("run: python3 .github/scripts/check_ci_results.py", policy)
        for result, expected in [("success", 0), ("failure", 1), ("skipped", 1)]:
            with self.subTest(result=result):
                needs = {
                    name: {"result": "success"}
                    for name in (
                        "blob-size-policy",
                        "cargo-deny",
                        "codespell",
                        "repo-checks",
                    )
                }
                needs["cargo-deny"]["result"] = result
                completed = subprocess.run(
                    [sys.executable, str(ROOT / ".github/scripts/check_ci_results.py")],
                    env={**os.environ, "NEEDS": json.dumps(needs)},
                    capture_output=True,
                )
                self.assertEqual(completed.returncode, expected)


if __name__ == "__main__":
    unittest.main()
