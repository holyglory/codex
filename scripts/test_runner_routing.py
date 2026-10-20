from pathlib import Path
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

    def test_candidate_branch_skips_duplicate_blocking_compiler_workflows(self):
        workflow = (ROOT / ".github/workflows/blocking-ci.yml").read_text()
        guard = "github.head_ref != 'codex/release1621-candidate-v3'"
        self.assertEqual(workflow.count(guard), 3)
        self.assertIn("local Linux and private Windows handoff", workflow)


if __name__ == "__main__":
    unittest.main()
