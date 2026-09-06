from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

from run_candidate_preflight import run_preflight


class CandidatePreflightTests(unittest.TestCase):
    def test_both_families_are_required_with_the_same_feature_union(self):
        with patch("run_candidate_preflight.subprocess.run") as run:
            run_preflight(Path("/candidate"))
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(len(commands), 2)
        self.assertEqual(commands[0][:-1], commands[1][:-1])
        self.assertEqual(
            [command[-1] for command in commands],
            [
                "test(suite::v2::thread_reconnect::)",
                "test(app::tests::turn_submission::)",
            ],
        )
        for call in run.call_args_list:
            self.assertIn("--no-tests=fail", call.args[0])
            self.assertEqual(
                call.kwargs, {"cwd": Path("/candidate/codex-rs"), "check": True}
            )

    def test_test_failure_or_empty_selection_stops_the_gate(self):
        for code in (1, 4):
            with (
                self.subTest(code=code),
                patch(
                    "run_candidate_preflight.subprocess.run",
                    side_effect=subprocess.CalledProcessError(code, "just"),
                ) as run,
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    run_preflight(Path("/candidate"))
                self.assertEqual(run.call_count, 1)

    def test_consumer_failure_is_not_hidden_by_producer_success(self):
        with patch(
            "run_candidate_preflight.subprocess.run",
            side_effect=[None, subprocess.CalledProcessError(1, "just")],
        ) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                run_preflight(Path("/candidate"))
            self.assertEqual(run.call_count, 2)


if __name__ == "__main__":
    unittest.main()
