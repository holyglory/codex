#!/usr/bin/env python3

import base64
from contextlib import contextmanager, redirect_stdout
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
    @contextmanager
    def source_v8_fixture(self, *, legacy: bool):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "source with spaces"
            root.mkdir()
            (root / "MODULE.bazel").write_text(
                'native_windows_llvm(name = "v8_native_windows_llvm")\n'
                if legacy
                else 'bazel_dep(name = "llvm", version = "0.8.11")\n'
            )
            output_base = root / "bazel-output-base"
            execution_root = root / "execution-root"
            compiler_repo = (
                "v8_native_windows_llvm"
                if legacy
                else "llvm++llvm_toolchain_minimal+llvm-toolchain-minimal-windows-amd64"
            )
            compiler = output_base / "external" / compiler_repo / "bin"
            compiler.mkdir(parents=True)
            tools = [
                compiler / name
                for name in ("clang-cl.exe", "llvm-lib.exe", "lld-link.exe")
            ]
            if legacy:
                tools.append(compiler / "libclang.dll")
            for path in tools:
                path.write_bytes(b"synthetic pinned tool")
            version = "22.1.8" if legacy else "23.1.2"
            declaration = output_base / "external/llvm+/MODULE.bazel"
            declaration.parent.mkdir(parents=True)
            declaration.write_text(f'LLVM_VERSION = "{version}"\n')
            queried_tools = [
                path.relative_to(output_base).as_posix().replace("/", "\\")
                for path in [*tools, declaration]
            ]
            archive = execution_root / "bazel-out/v8/native.lib"
            binding = archive.with_suffix(".rs")
            archive.parent.mkdir(parents=True)
            archive.write_bytes(b"synthetic V8 archive")
            binding.write_text("synthetic V8 binding")
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.evidence = root / "evidence"
            producer.handoff = root / "handoff"

            def bazel_output(name, *_arguments, **_kwargs):
                return {
                    "v8-x64-source-outputs": "\n".join(
                        path.relative_to(execution_root).as_posix()
                        for path in (archive, binding)
                    ),
                    "v8-execution-root": str(execution_root),
                    "v8-output-base": str(output_base),
                    "v8-pinned-clang": "\n".join(queried_tools),
                }.get(name, "")

            with (
                patch.object(MODULE, "ROOT", root),
                patch.object(producer, "bazel", side_effect=bazel_output) as bazel,
                patch.object(
                    producer, "run", return_value=f"clang version {version}\n"
                ) as run,
                patch.object(
                    producer, "pair_receipt", return_value={"source_built": True}
                ) as receipt,
                patch.object(MODULE, "stage_artifacts") as stage,
            ):
                yield SimpleNamespace(
                    producer=producer,
                    compiler=compiler,
                    queried_tools=queried_tools,
                    declaration=declaration,
                    version=version,
                    bazel=bazel,
                    run=run,
                    receipt=receipt,
                    stage=stage,
                    archive=archive,
                    binding=binding,
                )

    def test_source_v8_resolves_modern_minimal_and_legacy_full_llvm(self) -> None:
        for legacy in (False, True):
            with (
                self.subTest(legacy=legacy),
                self.source_v8_fixture(legacy=legacy) as fixture,
            ):
                fixture.producer.source_v8()
                target = "x86_64-pc-windows-msvc"
                receipt = json.loads(
                    (fixture.producer.handoff / f"v8-{target}.json").read_text()
                )
                self.assertEqual(receipt["compiler_directory"], str(fixture.compiler))
                self.assertEqual(receipt["compiler_version"], fixture.version)
                self.assertEqual(
                    receipt["libclang_directory"],
                    str(fixture.compiler) if legacy else None,
                )
                expected_labels = (
                    ["@v8_native_windows_llvm//:all_files"]
                    if legacy
                    else [
                        "@llvm//tools:clang-cl",
                        "@llvm//tools:llvm-lib",
                        "@llvm//tools:lld-link",
                    ]
                )
                self.assertEqual(receipt["compiler_labels"], expected_labels)
                query = next(
                    item
                    for item in fixture.bazel.call_args_list
                    if item.args[0] == "v8-pinned-clang"
                )
                self.assertEqual(
                    query.args[1:],
                    (
                        "cquery",
                        *MODULE.BAZEL_FLAGS,
                        "--output=files",
                        "set("
                        + " ".join([*expected_labels, "@llvm//:MODULE.bazel"])
                        + ")",
                    ),
                )
                fixture.run.assert_called_once_with(
                    "v8-pinned-clang-version",
                    [str(fixture.compiler / "clang-cl.exe"), "--version"],
                    capture_output=True,
                )
                fixture.stage.assert_called_once_with(
                    target,
                    fixture.archive,
                    fixture.binding,
                    fixture.producer.evidence / "v8" / target,
                    True,
                )
                self.assertEqual(
                    fixture.bazel.call_args_list[0],
                    call(
                        "v8-x64-source-build",
                        "build",
                        *MODULE.BAZEL_FLAGS,
                        *MODULE.NATIVE_V8,
                    ),
                )
                self.assertEqual(
                    fixture.bazel.call_args_list[-1],
                    call("v8-bazel-shutdown", "shutdown"),
                )

    def test_source_v8_rejects_invalid_pinned_compilers_without_host_fallback(
        self,
    ) -> None:
        for failure in (
            "missing",
            "ambiguous",
            "mixed",
            "version",
            "declaration",
            "legacy-dll",
        ):
            with (
                self.subTest(failure=failure),
                self.source_v8_fixture(legacy=failure == "legacy-dll") as fixture,
            ):
                if failure == "missing":
                    (fixture.compiler / "clang-cl.exe").unlink()
                elif failure == "ambiguous":
                    duplicate = fixture.compiler.parent / "duplicate/clang-cl.exe"
                    duplicate.parent.mkdir()
                    duplicate.write_bytes(b"ambiguous tool")
                    fixture.queried_tools.append(str(duplicate))
                elif failure == "mixed":
                    mixed = fixture.compiler.parent / "mixed/llvm-lib.exe"
                    mixed.parent.mkdir()
                    mixed.write_bytes(b"different installation")
                    fixture.queried_tools[:] = [
                        value
                        for value in fixture.queried_tools
                        if not value.endswith("llvm-lib.exe")
                    ] + [str(mixed)]
                elif failure == "version":
                    fixture.run.return_value = "clang version 21.0.0\n"
                elif failure == "declaration":
                    fixture.declaration.write_text('LLVM_VERSION = "unknown"\n')
                else:
                    (fixture.compiler / "libclang.dll").unlink()
                with self.assertRaises(ValueError):
                    fixture.producer.source_v8()
                self.assertFalse(
                    (
                        fixture.producer.handoff / "v8-x86_64-pc-windows-msvc.json"
                    ).exists()
                )
                self.assertEqual(
                    fixture.bazel.call_args_list[-1],
                    call("v8-bazel-shutdown", "shutdown"),
                )

    def test_x64_acceptance_and_package_preserve_flags_and_enable_cpp_unwinding(
        self,
    ) -> None:
        target = "x86_64-pc-windows-msvc"
        for prior_flags, expected_flags, layout in (
            (None, "/EHsc", "minimal"),
            ("/DPRIOR_FLAG=1 /O2", "/DPRIOR_FLAG=1 /O2 /EHsc", "full"),
            (None, "/EHsc", "retained-full"),
        ):
            with (
                self.subTest(prior_flags=prior_flags, layout=layout),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                producer = MODULE.Producer.__new__(MODULE.Producer)
                producer.evidence = root / "evidence"
                producer.handoff = root / "handoff"
                pair = producer.evidence / "v8" / target
                record = {
                    "files": {"native-v8.a.gz": "fixture-digest"},
                    "archive": {"name": "native-v8.a.gz"},
                    "binding": {"name": "native-binding.rs"},
                    "compiler_directory": str(root / "compiler"),
                }
                expected_libclang = str(root / "compiler")
                if layout != "retained-full":
                    expected_libclang = (
                        str(root / "legacy-libclang") if layout == "full" else None
                    )
                    record["libclang_directory"] = expected_libclang
                MODULE.write_json(producer.handoff / f"v8-{target}.json", record)
                environment = {
                    "PATH": "existing-tools",
                    "CXXFLAGS": "/DGLOBAL_FLAG=1",
                    "CODEX_WINDOWS_NEXTEST_CONFIG_FILE": "nextest.toml",
                    "LIBCLANG_PATH": "unverified-host-library",
                    "V8_FROM_SOURCE": "1",
                }
                if prior_flags is not None:
                    environment["CXXFLAGS_x86_64_pc_windows_msvc"] = prior_flags
                original_environment = dict(environment)
                expected_environment = {
                    **environment,
                    "CXXFLAGS_x86_64_pc_windows_msvc": expected_flags,
                    "RUSTY_V8_ARCHIVE": str(pair / record["archive"]["name"]),
                    "RUSTY_V8_SRC_BINDING_PATH": str(pair / record["binding"]["name"]),
                    "PATH": record["compiler_directory"]
                    + os.pathsep
                    + environment["PATH"],
                }
                expected_environment.pop("V8_FROM_SOURCE")
                expected_environment.pop("LIBCLANG_PATH")
                if expected_libclang:
                    expected_environment["LIBCLANG_PATH"] = expected_libclang
                with (
                    patch.object(
                        MODULE,
                        "os",
                        SimpleNamespace(environ=environment, pathsep=os.pathsep),
                    ),
                    patch.object(
                        producer, "pair_receipt", return_value=record
                    ) as pair_receipt,
                    patch.object(producer, "run") as run,
                ):
                    producer.acceptance()
                    self.assertEqual(
                        [
                            (item.args[0], item.kwargs["environment"])
                            for item in run.call_args_list
                        ],
                        [
                            ("v8-x64-poc-cppgc", expected_environment),
                            ("windows-product-regressions", expected_environment),
                        ],
                    )
                    run.reset_mock()
                    run.side_effect = RuntimeError("build intercepted")
                    with self.assertRaisesRegex(RuntimeError, "build intercepted"):
                        producer.package(target)
                    run.assert_called_once()
                    self.assertEqual(run.call_args.args[0], f"{target}-build")
                    self.assertEqual(
                        run.call_args.kwargs["environment"], expected_environment
                    )
                    self.assertEqual(
                        pair_receipt.call_args_list,
                        [call(target, pair, "source-native-windows")] * 2,
                    )
                self.assertEqual(environment, original_environment)

    def test_arm64_package_uses_the_pinned_compiler_and_optional_legacy_libclang(
        self,
    ) -> None:
        for layout in ("minimal", "full", "retained-full"):
            with (
                self.subTest(layout=layout),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                target = "aarch64-pc-windows-msvc"
                producer = MODULE.Producer.__new__(MODULE.Producer)
                producer.evidence = root / "evidence"
                producer.handoff = root / "handoff"
                compiler = root / "compiler"
                x64_pair = {"compiler_directory": str(compiler)}
                expected_libclang = str(compiler)
                if layout != "retained-full":
                    expected_libclang = (
                        str(root / "legacy-libclang") if layout == "full" else None
                    )
                    x64_pair["libclang_directory"] = expected_libclang
                MODULE.write_json(
                    producer.handoff / "v8-x86_64-pc-windows-msvc.json", x64_pair
                )
                pair = {
                    "archive": {"name": "v8.lib.gz"},
                    "binding": {"name": "binding.rs"},
                }
                environment = {
                    "PATH": "existing-tools",
                    "VSCMD_ARG_HOST_ARCH": "x64",
                    "VSCMD_ARG_TGT_ARCH": "arm64",
                    "LIBCLANG_PATH": "unverified-host-library",
                    "V8_FROM_SOURCE": "yes",
                }
                with (
                    patch.dict(os.environ, environment, clear=True),
                    patch.object(producer, "pair_receipt", return_value=pair),
                    patch.object(
                        producer,
                        "run",
                        side_effect=[None, RuntimeError("build intercepted")],
                    ) as run,
                ):
                    with self.assertRaisesRegex(RuntimeError, "build intercepted"):
                        producer.package(target)
                self.assertEqual(
                    [item.args[0] for item in run.call_args_list],
                    ["v8-arm64-compile-link", f"{target}-build"],
                )
                for item in run.call_args_list:
                    actual = item.kwargs["environment"]
                    self.assertEqual(
                        actual["CC_aarch64_pc_windows_msvc"],
                        str(compiler / "clang-cl.exe"),
                    )
                    self.assertEqual(
                        actual["CXX_aarch64_pc_windows_msvc"],
                        str(compiler / "clang-cl.exe"),
                    )
                    self.assertEqual(
                        actual["AR_aarch64_pc_windows_msvc"],
                        str(compiler / "llvm-lib.exe"),
                    )
                    self.assertEqual(actual.get("LIBCLANG_PATH"), expected_libclang)
                    self.assertNotIn("V8_FROM_SOURCE", actual)
                self.assertIn("--locked", run.call_args_list[0].args[1])

    def test_successive_target_builds_retain_distinct_cargo_timing_reports(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cargo_target = root / "cargo-target"
            report = cargo_target / "cargo-timings" / "cargo-timing.html"
            report.parent.mkdir(parents=True)
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.telemetry = root / "telemetry"
            for target, contents in (
                ("x86_64-pc-windows-msvc", "x64 report"),
                ("aarch64-pc-windows-msvc", "ARM64 report"),
            ):
                report.write_text(contents)
                producer.capture_cargo_timings(
                    target, {"CARGO_TARGET_DIR": str(cargo_target)}
                )
            retained = {
                path.parent.name: path.read_text()
                for path in producer.telemetry.glob("cargo-timings/*/cargo-timing.html")
            }
            self.assertEqual(
                retained,
                {
                    "x86_64-pc-windows-msvc": "x64 report",
                    "aarch64-pc-windows-msvc": "ARM64 report",
                },
            )
            self.assertEqual(report.read_text(), "ARM64 report")

    def test_cpu_probe_passes_pid_outside_encoded_script_and_skips_noise(self) -> None:
        environment = {"PATH": "tools", "CODEX_WINDOWS_CPU_PROBE_PID": "old"}
        sample = {
            "cpu_seconds": 12.5,
            "process_count": 3,
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
            {"compiler_process_count": -1},
            {"compiler_process_count": 3},
            {"compiler_process_count": True},
            {"compiler_process_count": 1.5},
            {"compiler_process_count": "1"},
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

    def test_host_cpu_counter_deltas_distinguish_usage_idle_and_unavailable(
        self,
    ) -> None:
        base = 1 << 35
        snapshots = iter(
            [
                (base + 100, base + 300, base + 200),
                (base + 140, base + 380, base + 220),
                (base + 240, base + 480, base + 220),
                (base + 240, base + 480, base + 220),
                (base + 200, base + 400, base + 200),
            ]
        )

        def get_system_times(*pointers):
            for pointer, value in zip(pointers, next(snapshots)):
                pointer._obj.dwLowDateTime = value & 0xFFFFFFFF
                pointer._obj.dwHighDateTime = value >> 32
            return 1

        with (
            patch.object(
                MODULE, "os", SimpleNamespace(name="nt", cpu_count=lambda: 64)
            ),
            patch.object(
                MODULE.ctypes,
                "WinDLL",
                create=True,
                return_value=(
                    SimpleNamespace(
                        GetActiveProcessorCount=Mock(return_value=64),
                        GetActiveProcessorGroupCount=Mock(return_value=1),
                        GetSystemTimes=Mock(side_effect=get_system_times),
                    )
                ),
            ),
        ):
            sample = None
            samples = []
            for expected in (None, 60.0, 0.0, None, None):
                sample = MODULE.host_cpu_sample(sample)
                samples.append(sample)
                with self.subTest(expected=expected):
                    self.assertEqual(sample["utilization_percent"], expected)
                    self.assertEqual(
                        "utilization_unavailable_reason" in sample, expected is None
                    )
            self.assertEqual(samples[0]["source"], "GetSystemTimes")
            self.assertEqual(samples[0]["scope"], "machine")
            self.assertEqual(samples[0]["logical_cpu_count"], 64)
            self.assertEqual(samples[0]["processor_group_count"], 1)
            self.assertEqual(samples[0]["idle_100ns"], base + 100)
            self.assertEqual(samples[0]["kernel_100ns"], base + 300)
            self.assertEqual(samples[0]["user_100ns"], base + 200)

    def test_host_cpu_reports_api_failure_and_unsupported_machine_scope(self) -> None:
        with (
            patch.object(
                MODULE, "os", SimpleNamespace(name="nt", cpu_count=lambda: 64)
            ),
            patch.object(
                MODULE.ctypes,
                "WinDLL",
                create=True,
                return_value=SimpleNamespace(
                    GetActiveProcessorCount=Mock(return_value=64),
                    GetActiveProcessorGroupCount=Mock(return_value=1),
                    GetSystemTimes=Mock(return_value=0),
                ),
            ),
            patch.object(MODULE.ctypes, "get_last_error", create=True, return_value=5),
        ):
            self.assertEqual(
                MODULE.host_cpu_sample(),
                {"error": "GetSystemTimes unavailable: Windows error 5"},
            )
        for native_cpus, groups, expected_error in (
            (128, 2, "more than 64 logical CPUs"),
            (64, 2, "2 active processor groups"),
            (64, 0, "0 active processor groups"),
            (0, 1, "GetActiveProcessorCount failed: Windows error 5"),
        ):
            kernel32 = SimpleNamespace(
                GetActiveProcessorCount=Mock(return_value=native_cpus),
                GetActiveProcessorGroupCount=Mock(return_value=groups),
                GetSystemTimes=Mock(),
            )
            with (
                self.subTest(native_cpus=native_cpus, groups=groups),
                patch.object(
                    MODULE, "os", SimpleNamespace(name="nt", cpu_count=lambda: 64)
                ),
                patch.object(
                    MODULE.ctypes, "WinDLL", create=True, return_value=kernel32
                ),
                patch.object(
                    MODULE.ctypes, "get_last_error", create=True, return_value=5
                ),
            ):
                sample = MODULE.host_cpu_sample()
                self.assertIn(expected_error, sample["error"])
                self.assertNotIn("scope", sample)
                kernel32.GetActiveProcessorCount.assert_called_once_with(0xFFFF)
                kernel32.GetSystemTimes.assert_not_called()

    @unittest.skipUnless(os.name == "nt", "requires Windows host CPU counters")
    def test_host_cpu_samples_real_windows_counter_delta(self) -> None:
        kernel32 = MODULE.ctypes.WinDLL("kernel32", use_last_error=True)
        processor_count = kernel32.GetActiveProcessorCount
        processor_count.argtypes = [MODULE.wintypes.WORD]
        processor_count.restype = MODULE.wintypes.DWORD
        native_cpus = processor_count(0xFFFF)
        group_count = kernel32.GetActiveProcessorGroupCount
        group_count.argtypes = []
        group_count.restype = MODULE.wintypes.WORD
        native_groups = group_count()
        self.assertGreater(native_cpus, 0)
        self.assertGreater(native_groups, 0)
        if native_cpus > 64 or native_groups > 1:
            self.skipTest(
                "GetSystemTimes whole-host counters require one processor group with at most 64 CPUs"
            )
        first = MODULE.host_cpu_sample()
        self.assertNotIn("error", first)
        self.assertEqual(first["logical_cpu_count"], native_cpus)
        self.assertEqual(first["processor_group_count"], native_groups)
        MODULE.time.sleep(0.05)
        second = MODULE.host_cpu_sample(first)
        self.assertNotIn("error", second)
        self.assertEqual(second["scope"], "machine")
        self.assertIsInstance(second.get("utilization_percent"), (int, float))
        self.assertGreaterEqual(second["utilization_percent"], 0)
        self.assertLessEqual(second["utilization_percent"], 100)
        self.assertNotIn("utilization_unavailable_reason", second)
        print(json.dumps({"native_host_cpu_sample": second}))

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
            host_samples = [
                {"error": "host counters unavailable"},
                {
                    "at_ms": 4000,
                    "utilization_percent": None,
                    "utilization_unavailable_reason": "a second counter sample is required",
                },
                {"at_ms": 6000, "utilization_percent": 0.0},
                {"at_ms": 8000, "utilization_percent": 100.0},
            ]
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
                    MODULE, "host_cpu_sample", side_effect=host_samples
                ) as host_probe,
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
            self.assertEqual(
                heartbeats[0]["host_cpu_sample_error"], "host counters unavailable"
            )
            self.assertIsNone(heartbeats[0]["host_cpu_sample"])
            for heartbeat, host_sample in zip(heartbeats[1:], host_samples[1:]):
                self.assertEqual(heartbeat["host_cpu_sample"], host_sample)
                self.assertIsNone(heartbeat["host_cpu_sample_error"])
            self.assertEqual(telemetry["host_cpu_sample"], host_samples[-1])
            self.assertNotIn("host_cpu_sample_error", telemetry)
            self.assertEqual(
                host_probe.call_args_list,
                [call(None), call(None), call(host_samples[1]), call(host_samples[2])],
            )

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
                    "cargo-version": "cargo 1.94.0 (fixture)\n",
                    "git-longpaths": "true\n",
                }.get(name, "")
            )
            environment = {
                "VSCMD_ARG_HOST_ARCH": "x64",
                "VSCMD_ARG_TGT_ARCH": "x64",
                "ComSpec": "cmd.exe",
                "RUSTUP_TOOLCHAIN": "1.94.0",
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
                    ValueError, r"Rust 1\.94\.0 is required for Windows acceptance"
                ),
            ):
                producer.prerequisites()
            run.assert_called_once_with(
                "rust-version", ["rustc", "--version"], capture_output=True
            )
            producer.run = Mock(
                side_effect=lambda name, *args, **kwargs: {
                    "rust-version": "rustc 1.94.0 (fixture)\n",
                    "cargo-version": "cargo 1.95.0 (mismatched)\n",
                }.get(name, "")
            )
            with (
                patch.object(MODULE, "ROOT", root),
                patch.object(MODULE, "os", SimpleNamespace(environ=environment)),
                patch.object(MODULE.shutil, "which", return_value="existing-tool"),
                self.assertRaisesRegex(
                    ValueError, r"Cargo 1\.94\.0 is required for Windows acceptance"
                ),
            ):
                producer.prerequisites()
            self.assertEqual(
                producer.run.call_args_list,
                [
                    call("rust-version", ["rustc", "--version"], capture_output=True),
                    call("cargo-version", ["cargo", "--version"], capture_output=True),
                ],
            )

    def test_prerequisites_prefer_release_workflow_pin_over_stale_source_toolchain(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "codex-rs").mkdir()
            (root / ".github/workflows").mkdir(parents=True)
            (root / "codex-rs/rust-toolchain.toml").write_text(
                '[toolchain]\nchannel = "1.94.0"\n'
            )
            (root / ".github/workflows/rust-release.yml").write_text(
                """
                jobs:
                  build:
                    steps:
                      - uses: dtolnay/rust-toolchain@release-pin # 1.95.0
                """
            )
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.evidence = root
            producer.handoff = root / "windows-handoff"
            producer.identity = {"commit": "fixture-source"}
            producer.run = Mock(
                side_effect=lambda name, *args, **kwargs: {
                    "rust-version": "rustc 1.95.0 (fixture)\n",
                    "cargo-version": "cargo 1.95.0 (fixture)\n",
                    "git-longpaths": "true\n",
                }.get(name, "")
            )
            environment = {
                "VSCMD_ARG_HOST_ARCH": "x64",
                "VSCMD_ARG_TGT_ARCH": "x64",
                "ComSpec": "cmd.exe",
                "RUSTUP_TOOLCHAIN": "1.95.0",
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
                        "1.95.0",
                        *MODULE.TARGETS,
                    ],
                ),
                producer.run.call_args_list,
            )

    def test_conflicting_release_workflow_pins_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workflow = root / ".github/workflows"
            workflow.mkdir(parents=True)
            (workflow / "rust-release.yml").write_text(
                """
                - uses: dtolnay/rust-toolchain@first # 1.95.0
                - uses: dtolnay/rust-toolchain@second # 1.96.0
                """
            )
            with (
                patch.object(MODULE, "ROOT", root),
                self.assertRaisesRegex(ValueError, "conflicting Rust toolchain pins"),
            ):
                MODULE.resolve_rust_channel()

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
