from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

from run_candidate_preflight import run_preflight


class CandidatePreflightTests(unittest.TestCase):
    def test_all_families_are_required_with_the_same_feature_union(self):
        with patch("run_candidate_preflight.subprocess.run") as run:
            run_preflight(Path("/candidate"))
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(len(commands), 4)
        self.assertTrue(all(command[:-1] == commands[0][:-1] for command in commands))
        self.assertEqual(
            [command[-1] for command in commands],
            [
                "test(suite::v2::thread_reconnect::)",
                "test(app::tests::turn_submission::)",
                "test(unified_exec::async_watcher::tests::streaming_output_preserves_summary_when_delta_consumer_lags)",
                "test(suite::unified_exec::unified_exec_formats_large_output_summary)",
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

    def test_output_capture_failure_stops_the_gate(self):
        for preceding_successes in (2, 3):
            with (
                self.subTest(preceding_successes=preceding_successes),
                patch(
                    "run_candidate_preflight.subprocess.run",
                    side_effect=[None] * preceding_successes
                    + [subprocess.CalledProcessError(1, "just")],
                ) as run,
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    run_preflight(Path("/candidate"))
                self.assertEqual(run.call_count, preceding_successes + 1)


if __name__ == "__main__":
    unittest.main()
