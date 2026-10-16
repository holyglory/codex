from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

from run_candidate_preflight import run_preflight


class CandidatePreflightTests(unittest.TestCase):
    def test_each_release_capability_requires_a_nonempty_test_family(self):
        manifest = {
            "capabilities": [
                {
                    "id": "retained-feature",
                    "packages": ["codex-cli"],
                    "filters": ["test(kept_feature::)", "test(recovery::)"],
                }
            ]
        }
        for failure in (None, 4):
            with (
                self.subTest(failure=failure),
                patch("run_candidate_preflight.verify_toolchain"),
                patch("run_candidate_preflight.load_manifest", return_value=manifest),
                patch("run_candidate_preflight.verify"),
                patch("run_candidate_preflight.subprocess.run") as run,
            ):
                if failure is None:
                    run_preflight(Path("/candidate"))
                    self.assertEqual(
                        run.call_args_list[-1].args[0][-1], "test(recovery::)"
                    )
                else:
                    run.side_effect = [None] * 8 + [
                        subprocess.CalledProcessError(failure, "just")
                    ]
                    with self.assertRaises(subprocess.CalledProcessError):
                        run_preflight(Path("/candidate"))
                selections = [call.args[0] for call in run.call_args_list[1:]]
                self.assertTrue(
                    all(command[:-1] == selections[0][:-1] for command in selections)
                )
                self.assertTrue(all("codex-cli" in command for command in selections))

    def test_release_inclusion_refusal_stops_before_build(self):
        with (
            patch("run_candidate_preflight.verify_toolchain"),
            patch("run_candidate_preflight.load_manifest", return_value={}),
            patch(
                "run_candidate_preflight.verify",
                side_effect=ValueError("missing capability"),
            ),
            patch("run_candidate_preflight.subprocess.run") as run,
        ):
            with self.assertRaises(ValueError):
                run_preflight(Path("/candidate"))
            run.assert_not_called()

    def test_all_families_are_required_with_the_same_feature_union(self):
        with (
            patch("run_candidate_preflight.verify_toolchain"),
            patch(
                "run_candidate_preflight.load_manifest",
                return_value={"capabilities": []},
            ),
            patch("run_candidate_preflight.verify"),
            patch("run_candidate_preflight.subprocess.run") as run,
        ):
            run_preflight(Path("/candidate"))
        self.assertEqual(
            run.call_args_list[0].args[0],
            [
                "cargo",
                "build",
                "--locked",
                "-p",
                "codex-code-mode-host",
                "-p",
                "codex-rmcp-client",
                "--bin",
                "codex-code-mode-host",
                "--bin",
                "test_stdio_server",
            ],
        )
        commands = [call.args[0] for call in run.call_args_list[1:]]
        self.assertEqual(len(commands), 6)
        self.assertTrue(all(command[:-1] == commands[0][:-1] for command in commands))
        self.assertEqual(
            [command[-1] for command in commands],
            [
                "test(suite::v2::thread_reconnect::)",
                "test(app::tests::turn_submission::)",
                "test(suite::unified_exec::unified_exec_formats_large_output_summary)",
                "test(suite::v2::account_auto_selection::)",
                "test(suite::models_cache_ttl::)",
                "test(suite::search_tool::)",
            ],
        )
        for call in run.call_args_list[1:]:
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
                    side_effect=[None, subprocess.CalledProcessError(code, "just")],
                ) as run,
                patch("run_candidate_preflight.verify_toolchain"),
                patch(
                    "run_candidate_preflight.load_manifest",
                    return_value={"capabilities": []},
                ),
                patch("run_candidate_preflight.verify"),
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    run_preflight(Path("/candidate"))
                self.assertEqual(run.call_count, 2)

    def test_consumer_failure_is_not_hidden_by_producer_success(self):
        with (
            patch(
                "run_candidate_preflight.subprocess.run",
                side_effect=[None, None, subprocess.CalledProcessError(1, "just")],
            ) as run,
            patch("run_candidate_preflight.verify_toolchain"),
            patch(
                "run_candidate_preflight.load_manifest",
                return_value={"capabilities": []},
            ),
            patch("run_candidate_preflight.verify"),
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                run_preflight(Path("/candidate"))
            self.assertEqual(run.call_count, 3)

    def test_output_capture_failure_stops_the_gate(self):
        for preceding_successes in (3, 4, 5, 6):
            with (
                self.subTest(preceding_successes=preceding_successes),
                patch(
                    "run_candidate_preflight.subprocess.run",
                    side_effect=[None] * preceding_successes
                    + [subprocess.CalledProcessError(1, "just")],
                ) as run,
                patch("run_candidate_preflight.verify_toolchain"),
                patch(
                    "run_candidate_preflight.load_manifest",
                    return_value={"capabilities": []},
                ),
                patch("run_candidate_preflight.verify"),
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    run_preflight(Path("/candidate"))
                self.assertEqual(run.call_count, preceding_successes + 1)

    def test_companion_build_failure_stops_before_test_selection(self):
        with (
            patch(
                "run_candidate_preflight.subprocess.run",
                side_effect=subprocess.CalledProcessError(1, "cargo"),
            ) as run,
            patch("run_candidate_preflight.verify_toolchain"),
            patch(
                "run_candidate_preflight.load_manifest",
                return_value={"capabilities": []},
            ),
            patch("run_candidate_preflight.verify"),
        ):
            with self.assertRaises(subprocess.CalledProcessError):
                run_preflight(Path("/candidate"))
            self.assertEqual(run.call_count, 1)

    def test_toolchain_mismatch_is_rejected_before_build(self):
        with (
            patch(
                "run_candidate_preflight.tomllib.loads",
                return_value={"toolchain": {"channel": "1.95.0"}},
            ),
            patch(
                "run_candidate_preflight.shutil.which", return_value="/usr/bin/rustc"
            ),
            patch(
                "run_candidate_preflight.subprocess.run",
                return_value=type(
                    "Result", (), {"stdout": "rustc 1.94.1\n", "stderr": ""}
                )(),
            ) as run,
            patch(
                "pathlib.Path.read_text",
                return_value="[toolchain]\nchannel = '1.95.0'\n",
            ),
        ):
            with self.assertRaises(RuntimeError):
                from run_candidate_preflight import verify_toolchain

                verify_toolchain(Path("/candidate"))
        self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
