#!/usr/bin/env python3

import base64
from contextlib import redirect_stdout
import importlib.util
from importlib.machinery import SourceFileLoader
import io
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_loader(
    "downstream_windows_native",
    SourceFileLoader(
        "downstream_windows_native",
        str(ROOT / ".github/scripts/downstream-windows-native.ps1"),
    ),
)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class WindowsNativeHelperTest(unittest.TestCase):
    def test_cpu_probe_passes_pid_outside_encoded_script_and_skips_noise(self) -> None:
        environment = {"PATH": "tools", "CODEX_WINDOWS_CPU_PROBE_PID": "old"}
        sample = {
            "cpu_seconds": 12.5,
            "process_count": 3,
            "host_cpu_percent": 54.0,
            "compiler_process_count": 2,
        }
        result = subprocess.CompletedProcess(
            [],
            0,
            stdout="banner\n\ufeff" + json.dumps(sample) + '\n1234\n"noise"\n{}\n',
        )
        with (
            patch.object(MODULE, "os", SimpleNamespace(name="nt", environ=environment)),
            patch.object(MODULE.subprocess, "run", return_value=result) as run,
        ):
            self.assertEqual(MODULE.process_tree_cpu(4321), sample)

        run.assert_called_once()
        command = run.call_args.args[0]
        self.assertEqual(
            command[:-1], ["pwsh", "-NoProfile", "-NonInteractive", "-EncodedCommand"]
        )
        script = base64.b64decode(command[-1]).decode("utf-16-le")
        self.assertIn("$env:CODEX_WINDOWS_CPU_PROBE_PID", script)
        self.assertEqual(
            run.call_args.kwargs["env"],
            {**environment, "CODEX_WINDOWS_CPU_PROBE_PID": "4321"},
        )
        self.assertEqual(
            environment, {"PATH": "tools", "CODEX_WINDOWS_CPU_PROBE_PID": "old"}
        )

    def test_cpu_probe_ignores_invalid_optional_metrics(self) -> None:
        sample = {"cpu_seconds": 1.5, "process_count": 2}
        for optional in (
            {"host_cpu_percent": -1, "compiler_process_count": -1},
            {"host_cpu_percent": 101, "compiler_process_count": 3},
            {"host_cpu_percent": True, "compiler_process_count": True},
            {"host_cpu_percent": float("nan"), "compiler_process_count": 1.5},
            {"host_cpu_percent": 10**400, "compiler_process_count": "1"},
        ):
            with (
                self.subTest(optional=optional),
                patch.object(MODULE, "os", SimpleNamespace(name="nt", environ={})),
                patch.object(
                    MODULE.subprocess,
                    "run",
                    return_value=(
                        subprocess.CompletedProcess(
                            [], 0, stdout=json.dumps({**sample, **optional})
                        )
                    ),
                ),
            ):
                self.assertEqual(MODULE.process_tree_cpu(4321), sample)

    def test_cpu_probe_retries_fallback_for_invalid_sample_records(self) -> None:
        invalid_samples = (
            4321,
            {},
            {"cpu_seconds": 1},
            {"cpu_seconds": "1", "process_count": 1},
            {"cpu_seconds": True, "process_count": 1},
            {"cpu_seconds": -1, "process_count": 1},
            {"cpu_seconds": float("nan"), "process_count": 1},
            {"cpu_seconds": float("inf"), "process_count": 1},
            {"cpu_seconds": 10**400, "process_count": 1},
            {"cpu_seconds": 1, "process_count": True},
            {"cpu_seconds": 1, "process_count": 1.5},
            {"cpu_seconds": 1, "process_count": 0},
        )
        sample = {"cpu_seconds": 0, "process_count": 1}
        for invalid in invalid_samples:
            with (
                self.subTest(invalid=invalid),
                patch.object(MODULE, "os", SimpleNamespace(name="nt", environ={})),
                patch.object(
                    MODULE.subprocess,
                    "run",
                    side_effect=[
                        subprocess.CompletedProcess([], 0, stdout=json.dumps(invalid)),
                        subprocess.CompletedProcess([], 0, stdout=json.dumps(sample)),
                    ],
                ) as run,
            ):
                self.assertEqual(MODULE.process_tree_cpu(4321), sample)
                self.assertEqual(
                    [item.args[0][0] for item in run.call_args_list],
                    ["pwsh", "powershell"],
                )

    def test_cpu_probe_retries_unavailable_shell_and_retains_both_failures(
        self,
    ) -> None:
        sample = {"cpu_seconds": 1.5, "process_count": 2}
        with patch.object(MODULE, "os", SimpleNamespace(name="nt", environ={})):
            with patch.object(
                MODULE.subprocess,
                "run",
                side_effect=[
                    FileNotFoundError("pwsh unavailable"),
                    subprocess.CompletedProcess([], 0, stdout=json.dumps(sample)),
                ],
            ):
                self.assertEqual(MODULE.process_tree_cpu(4321), sample)
            with patch.object(
                MODULE.subprocess,
                "run",
                side_effect=[
                    FileNotFoundError("pwsh unavailable"),
                    subprocess.CompletedProcess([], 0, stdout="1234\n"),
                ],
            ):
                self.assertEqual(
                    MODULE.process_tree_cpu(4321),
                    {
                        "error": "pwsh: pwsh unavailable; powershell: CPU probe returned no valid CPU sample"
                    },
                )

    def test_cpu_probe_rejects_invalid_pid_before_starting_a_shell(self) -> None:
        for pid in (0, -1, True, "4321", 2_147_483_648):
            with (
                self.subTest(pid=pid),
                patch.object(MODULE, "os", SimpleNamespace(name="nt", environ={})),
                patch.object(MODULE.subprocess, "run") as run,
            ):
                self.assertEqual(
                    MODULE.process_tree_cpu(pid),
                    {"error": "CPU probe requires a positive 32-bit process ID"},
                )
                run.assert_not_called()

    @unittest.skipUnless(os.name == "nt", "requires Windows process telemetry")
    def test_cpu_probe_samples_current_windows_process(self) -> None:
        sample = MODULE.process_tree_cpu(os.getpid())
        self.assertNotIn("error", sample)
        self.assertGreaterEqual(sample["cpu_seconds"], 0)
        self.assertGreaterEqual(sample["process_count"], 1)

    def test_run_reports_cpu_errors_and_recovery_in_live_heartbeats(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.evidence = root
            producer.logs = root / "logs"
            producer.logs.mkdir()
            process = Mock(pid=4321, returncode=0)
            process.poll.side_effect = [None, None, None, None, 0]
            sample = {"cpu_seconds": 1.5, "process_count": 2}
            stdout = io.StringIO()
            with (
                redirect_stdout(stdout),
                patch.object(MODULE.subprocess, "Popen", return_value=process),
                patch.object(
                    MODULE.time, "monotonic", side_effect=[0, 0, 60, 120, 180, 180]
                ),
                patch.object(
                    MODULE.time,
                    "time_ns",
                    side_effect=[index * 1_000_000_000 for index in range(1, 11)],
                ),
                patch.object(MODULE.time, "sleep"),
                patch.object(MODULE, "host_cpu_count", return_value=4),
                patch.object(
                    MODULE,
                    "process_tree_cpu",
                    side_effect=[
                        {"error": "probe unavailable"},
                        sample,
                        {"cpu_seconds": 0.5, "process_count": 1},
                        {"cpu_seconds": 2, "process_count": 2},
                    ],
                ) as cpu_probe,
            ):
                producer.run("cpu-heartbeat", ["build-tool"])
            heartbeats = [
                value
                for line in stdout.getvalue().splitlines()
                if (value := json.loads(line)).get("state") == "running"
            ]
            self.assertEqual(len(heartbeats), 4)
            self.assertEqual(heartbeats[0]["cpu_sample_error"], "probe unavailable")
            self.assertIsNone(heartbeats[0]["cpu_sample"])
            recovered = heartbeats[1]["cpu_sample"]
            self.assertEqual(recovered, {"at_ms": recovered["at_ms"], **sample})
            self.assertIsNone(heartbeats[1]["cpu_sample_error"])
            self.assertEqual(
                heartbeats[2]["cpu_sample"],
                {
                    "at_ms": 6000,
                    "cpu_seconds": 0.5,
                    "process_count": 1,
                    "utilization_percent_of_machine": None,
                    "utilization_unavailable_reason": "process tree CPU counter decreased as processes exited",
                },
            )
            self.assertEqual(
                heartbeats[3]["cpu_sample"],
                {
                    "at_ms": 8000,
                    "cpu_seconds": 2,
                    "process_count": 2,
                    "utilization_percent_of_machine": 18.75,
                },
            )
            telemetry = json.loads((root / "telemetry/cpu-heartbeat.json").read_text())
            self.assertEqual(telemetry["cpu_sample"], heartbeats[3]["cpu_sample"])
            self.assertNotIn("cpu_sample_error", telemetry)
            self.assertEqual(cpu_probe.call_args_list, [call(4321)] * 4)

    def test_prerequisites_use_source_toolchain_channel_for_version_and_targets(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "codex-rs").mkdir()
            (root / "codex-rs/rust-toolchain.toml").write_text(
                '[toolchain]\nchannel = "1.94.0"\n'
            )
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.evidence = root
            producer.handoff = root / "windows-handoff"
            producer.identity = {"commit": "fixture-source"}
            producer.run = Mock(
                side_effect=lambda name, *args, **kwargs: {
                    "rust-version": "rustc 1.94.0 (fixture)\n",
                    "git-longpaths": "true\n",
                }.get(name, "")
            )
            environment = {
                "VSCMD_ARG_HOST_ARCH": "x64",
                "VSCMD_ARG_TGT_ARCH": "x64",
                "ComSpec": "cmd.exe",
            }
            with (
                patch.object(MODULE, "ROOT", root),
                patch.object(MODULE, "os", SimpleNamespace(environ=environment)),
                patch.object(MODULE.shutil, "which", return_value="existing-tool"),
                patch.object(MODULE, "verify_pe"),
                patch.object(
                    MODULE.subprocess,
                    "check_output",
                    return_value=(
                        "PATH=tools\nVSCMD_ARG_HOST_ARCH=x64\nVSCMD_ARG_TGT_ARCH=arm64\n"
                    ),
                ),
            ):
                producer.prerequisites()
            self.assertIn(
                call(
                    "rust-targets",
                    [
                        "rustup",
                        "target",
                        "add",
                        "--toolchain",
                        "1.94.0",
                        *MODULE.TARGETS,
                    ],
                ),
                producer.run.call_args_list,
            )
            self.assertEqual(
                json.loads((producer.handoff / "source.json").read_text()),
                producer.identity,
            )
            with (
                patch.object(MODULE, "ROOT", root),
                patch.object(MODULE, "os", SimpleNamespace(environ=environment)),
                patch.object(MODULE.shutil, "which", return_value="existing-tool"),
                patch.object(
                    producer, "run", return_value="rustc 1.95.0 (mismatched)\n"
                ) as run,
                self.assertRaisesRegex(
                    ValueError, r"Rust 1\.94\.0 from rust-toolchain\.toml is required"
                ),
            ):
                producer.prerequisites()
            run.assert_called_once_with(
                "rust-version", ["rustc", "--version"], capture_output=True
            )

    def test_capture_output_uses_file_backed_stdout_for_large_child_output(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.evidence = root
            producer.logs = root / "logs"
            producer.telemetry = root / "telemetry"
            producer.logs.mkdir()
            producer.telemetry.mkdir()
            observed_stdout = []
            popen = MODULE.subprocess.Popen
            command = [
                sys.executable,
                "-c",
                "import os,sys,threading; "
                "watchdog=threading.Timer(10, lambda: os._exit(124)); "
                "watchdog.daemon=True; watchdog.start(); "
                "sys.stdout.write('x' * 2_000_000); sys.stdout.flush(); watchdog.cancel()",
            ]

            def record_stdout(*args, **kwargs):
                # The live Windows CPU probe also launches subprocesses.
                if args[0] == command:
                    observed_stdout.append(kwargs["stdout"])
                return popen(*args, **kwargs)

            with patch.object(MODULE.subprocess, "Popen", side_effect=record_stdout):
                output = producer.run(
                    "large-capture",
                    command,
                    capture_output=True,
                )
            self.assertEqual(len(output), 2_000_000)
            self.assertEqual(len(observed_stdout), 1)
            self.assertIsNot(observed_stdout[0], MODULE.subprocess.PIPE)
            receipt = (root / "checks/large-capture.json").read_text()
            self.assertIn('"exit_code": 0', receipt)


if __name__ == "__main__":
    unittest.main()
