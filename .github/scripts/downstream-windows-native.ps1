#!/usr/bin/env python3
"""Build exact-source Windows packages inside the private owner-only controller."""

import argparse
from contextlib import nullcontext
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib

from rusty_v8_bazel import stage_artifacts


ROOT = Path(__file__).resolve().parents[2]
TARGETS = {"x86_64-pc-windows-msvc": 0x8664, "aarch64-pc-windows-msvc": 0xAA64}
BINARIES = (
    "codex", "codex-code-mode-host", "codex-exec-server",
    "codex-command-runner", "codex-windows-sandbox-setup",
)
NATIVE_V8 = (
    "//third_party/v8:rusty_v8_native_windows_archive_for_target",
    "//third_party/v8:rusty_v8_native_windows_binding_for_target",
)
BAZEL_FLAGS = (
    "--config=ci", "--config=ci-windows-msvc", "--compilation_mode=opt",
    "--host_platform=//:local_windows_msvc", "--platforms=//:local_windows_msvc",
    "--disk_cache=R:/bazel-disk-cache",
    # common:ci selects minimal downloads. The explicit policy must follow it.
    "--remote_download_outputs=all",
)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def process_tree_cpu(pid: int) -> dict | None:
    """Sample CPU time for a Windows process and all descendants."""
    if os.name != "nt":
        return None
    script = r"""
$root = [int]$args[0]
try {
  $processes = @(Get-CimInstance Win32_Process | ForEach-Object {
    [pscustomobject]@{ id = [int]$_.ProcessId; parent = [int]$_.ParentProcessId }
  })
} catch {
  $process = Get-Process -Id $root -ErrorAction Stop
  [pscustomobject]@{ cpu_seconds = $process.TotalProcessorTime.TotalSeconds; process_count = 1 } | ConvertTo-Json -Compress
  exit 0
}
$ids = [System.Collections.Generic.HashSet[int]]::new()
[void]$ids.Add($root)
do {
  $changed = $false
  foreach ($process in $processes) {
    if ($ids.Contains($process.parent) -and $ids.Add($process.id)) { $changed = $true }
  }
} while ($changed)
$cpuSeconds = 0.0
$count = 0
foreach ($id in $ids) {
  try {
    $process = Get-Process -Id $id -ErrorAction Stop
    $cpuSeconds += $process.TotalProcessorTime.TotalSeconds
    $count++
  } catch {}
}
[pscustomobject]@{ cpu_seconds = $cpuSeconds; process_count = $count } | ConvertTo-Json -Compress
"""
    errors = []
    for shell in ("pwsh", "powershell"):
        try:
            result = subprocess.run(
                [shell, "-NoProfile", "-NonInteractive", "-Command", script, str(pid)],
                capture_output=True,
                check=True,
                text=True,
                encoding="utf-8",
                timeout=20,
            )
            for line in reversed(result.stdout.splitlines()):
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                return value if isinstance(value, dict) else {"error": "invalid CPU probe output"}
            return {"error": "CPU probe returned no JSON record"}
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError, ValueError) as error:
            errors.append(f"{shell}: {error}")
    return {"error": "; ".join(errors)[:500]}


def host_cpu_count() -> int:
    value = os.environ.get("CODEX_WINDOWS_CPU_COUNT")
    try:
        return max(1, int(value)) if value is not None else max(1, os.cpu_count() or 1)
    except ValueError:
        return max(1, os.cpu_count() or 1)


def requested_parallelism(command: list[str]) -> int | None:
    for argument in command:
        if argument.startswith("--jobs="):
            try:
                return int(argument.partition("=")[2])
            except ValueError:
                return None
    for name in ("BAZEL_JOBS", "CARGO_BUILD_JOBS"):
        value = os.environ.get(name)
        if value:
            try:
                return int(value)
            except ValueError:
                return None
    return None


def git(*arguments: str) -> str:
    return subprocess.check_output(["git", *arguments], cwd=ROOT, text=True).strip()


def source_identity(expected: str) -> dict:
    if re.fullmatch(r"[0-9a-f]{40}", expected) is None or git("rev-parse", "HEAD") != expected:
        raise ValueError("The checkout must match the requested full source commit.")
    if git("status", "--porcelain", "--untracked-files=no"):
        raise ValueError("Tracked source changed during Windows acceptance.")
    with (ROOT / "codex-rs/Cargo.toml").open("rb") as stream:
        version = tomllib.load(stream)["workspace"]["package"]["version"]
    return {
        "commit": expected, "source_commit": expected,
        "source_tree": git("rev-parse", "HEAD^{tree}"),
        "native_tree": git("rev-parse", "HEAD:codex-rs"),
        "v8_tree": git("rev-parse", "HEAD:third_party/v8"), "version": version,
    }


def verify_pe(path: Path, target: str) -> None:
    with path.open("rb") as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:2] != b"MZ":
            raise ValueError(f"Missing DOS executable header: {path.name}")
        stream.seek(struct.unpack_from("<I", header, 60)[0])
        signature = stream.read(6)
    if len(signature) != 6 or signature[:4] != b"PE\0\0" or struct.unpack_from("<H", signature, 4)[0] != TARGETS[target]:
        raise ValueError(f"Wrong PE architecture for {target}: {path.name}")


class Producer:
    def __init__(self) -> None:
        self.identity = source_identity(os.environ["SOURCE_SHA"])
        self.evidence = Path(os.environ["CODEX_WINDOWS_EVIDENCE"])
        self.handoff = self.evidence / "windows-handoff"
        self.logs = self.handoff / "logs"
        self.telemetry = self.evidence / "telemetry"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.telemetry.mkdir(parents=True, exist_ok=True)

    def run(self, name: str, command: list[str], *, environment: dict | None = None, capture_output: bool = False) -> str:
        log = self.logs / f"{name}.log"
        if log.exists():
            raise ValueError(f"Refusing to replace retained check evidence: {name}")
        print(json.dumps({"check": name, "state": "started"}), flush=True)
        effective_environment = dict(os.environ if environment is None else environment)
        telemetry_root = getattr(self, "telemetry", self.evidence / "telemetry")
        telemetry_root.mkdir(parents=True, exist_ok=True)
        telemetry_path = telemetry_root / f"{name}.json"
        started_at_ms = time.time_ns() // 1_000_000
        telemetry = {
            "name": name,
            "command": command,
            "started_at_ms": started_at_ms,
            "pid": None,
            "heartbeat_interval_seconds": 60,
            "parallelism": {
                "host_cpu_count": host_cpu_count(),
                "requested_jobs": requested_parallelism(command),
            },
            "state": "running",
        }
        write_json(telemetry_path, telemetry)
        with (
            log.open("w", encoding="utf-8") as stream,
            (
                tempfile.TemporaryFile(
                    mode="w+", encoding="utf-8", errors="replace"
                )
                if capture_output
                else nullcontext(stream)
            ) as stdout_stream,
        ):
            result = subprocess.Popen(
                command,
                cwd=ROOT,
                env=effective_environment,
                stdout=stdout_stream,
                stderr=stream,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            telemetry["pid"] = result.pid
            write_json(telemetry_path, telemetry)
            started = time.monotonic()
            last_heartbeat = started - 60
            previous_cpu = None
            while result.poll() is None:
                now = time.monotonic()
                if now - last_heartbeat >= 60:
                    log_bytes = log.stat().st_size if log.exists() else 0
                    cpu = process_tree_cpu(result.pid)
                    cpu_sample = {"at_ms": time.time_ns() // 1_000_000, **cpu} if cpu else None
                    if cpu_sample and "cpu_seconds" in cpu_sample and previous_cpu:
                        elapsed = max(0.001, cpu_sample["at_ms"] - previous_cpu["at_ms"])
                        cpu_sample["utilization_percent_of_machine"] = round(
                            100
                            * (cpu_sample["cpu_seconds"] - previous_cpu["cpu_seconds"])
                            / (elapsed / 1000 * telemetry["parallelism"]["host_cpu_count"]),
                            2,
                        )
                    if cpu_sample and "cpu_seconds" in cpu_sample:
                        telemetry["cpu_sample"] = cpu_sample
                        previous_cpu = cpu_sample
                    elif cpu_sample:
                        telemetry["cpu_sample_error"] = cpu_sample.get("error")
                    telemetry.update({
                        "state": "running",
                        "elapsed_seconds": int(now - started),
                        "log_bytes": log_bytes,
                        "heartbeat_at_ms": time.time_ns() // 1_000_000,
                    })
                    write_json(telemetry_path, telemetry)
                    print(json.dumps({
                        "check": name,
                        "state": "running",
                        "pid": result.pid,
                        "elapsed_seconds": int(now - started),
                        "log_bytes": log_bytes,
                    }), flush=True)
                    last_heartbeat = now
                time.sleep(1)
            result.wait()
            if capture_output:
                stdout_stream.flush()
                stdout_stream.seek(0)
                output = stdout_stream.read()
            else:
                output = ""
        finished_at_ms = time.time_ns() // 1_000_000
        telemetry.update({
            "state": "completed",
            "finished_at_ms": finished_at_ms,
            "duration_ms": finished_at_ms - started_at_ms,
            "exit_code": result.returncode,
            "log_bytes": log.stat().st_size if log.exists() else 0,
        })
        final_cpu = process_tree_cpu(result.pid)
        if final_cpu and "cpu_seconds" in final_cpu:
            telemetry["cpu_sample"] = {"at_ms": finished_at_ms, **final_cpu}
        elif final_cpu:
            telemetry["cpu_sample_error"] = final_cpu.get("error")
        write_json(telemetry_path, telemetry)
        receipt = {"name": name, "exit_code": result.returncode, "log_sha256": digest(log), "log_path": f"logs/{log.name}", "command": command, "telemetry_path": f"telemetry/{telemetry_path.name}"}
        if "--config-file" in command:
            config = Path(command[command.index("--config-file") + 1])
            receipt["test_config_sha256"] = digest(config)
        write_json(self.evidence / "checks" / f"{name}.json", receipt)
        print(json.dumps({"check": name, "exit_code": result.returncode, "elapsed_seconds": int(time.monotonic() - started), "log_bytes": log.stat().st_size if log.exists() else 0}), flush=True)
        if result.returncode != 0:
            raise subprocess.CalledProcessError(result.returncode, command)
        return output if capture_output else ""

    def record_check(self, name: str, output: str) -> None:
        """Retain a successful in-process verification as a normal check receipt."""
        log = self.logs / f"{name}.log"
        if log.exists():
            raise ValueError(f"Refusing to replace retained check evidence: {name}")
        log.write_text(output.rstrip() + "\n", encoding="utf-8")
        write_json(self.evidence / "checks" / f"{name}.json", {
            "name": name, "exit_code": 0, "log_sha256": digest(log),
            "log_path": f"logs/{log.name}", "command": ["in-process-verification"],
        })

    def capture_cargo_timings(self, target: str, environment: dict) -> None:
        """Retain Cargo's compiler timing report for the target build."""
        target_dir = environment.get("CARGO_TARGET_DIR")
        if not target_dir:
            return
        source = Path(target_dir) / target / "cargo-timings"
        if source.is_dir():
            shutil.copytree(source, self.telemetry / "cargo-timings" / target, dirs_exist_ok=True)

    def bazel(self, name: str, *arguments: str, capture_output: bool = False) -> str:
        wrapper = Path(__file__).with_name("run_bazel_with_buildbuddy.py")
        return self.run(name, [sys.executable, str(wrapper), *arguments], capture_output=capture_output)

    def prerequisites(self) -> None:
        if os.environ.get("VSCMD_ARG_HOST_ARCH") != "x64" or os.environ.get("VSCMD_ARG_TGT_ARCH") != "x64":
            raise ValueError("The x64 host and target MSVC SDK environment is required.")
        for name in ("just", "cargo", "rustc", "rustup", "bazel", "cl", "git", "bash", "node", "dotslash"):
            if shutil.which(name) is None:
                raise ValueError(f"Provision the existing non-admin runner prerequisite: {name}")
        if not self.run("rust-version", ["rustc", "--version"], capture_output=True).startswith("rustc 1.95.0 "):
            raise ValueError("Rust 1.95.0 is required.")
        self.run("nextest-version", ["cargo", "nextest", "--version"])
        self.run("rust-targets", ["rustup", "target", "add", "--toolchain", "1.95.0-x86_64-pc-windows-msvc", *TARGETS])
        if self.run("git-longpaths", ["git", "config", "--global", "--get", "core.longpaths"], capture_output=True).strip() != "true":
            raise ValueError("Git long-path support is required for the pinned dependency checkouts.")
        # Match nextest's dependency discovery before the expensive native V8
        # build. This checks out locked Git submodules without compiling them.
        self.run("cargo-source-metadata", ["cargo", "metadata", "--manifest-path", str(ROOT / "codex-rs/Cargo.toml"), "--config", str(ROOT / "codex-rs/.cargo/config.toml"), "--format-version", "1", "--all-features", "--filter-platform", "x86_64-pc-windows-msvc", "--locked"])
        probe = self.evidence / "sdk-probe.c"
        probe.write_text('#include <windows.h>\nint main(void) { return sizeof(void*) == 8 ? 0 : 1; }\n', encoding="ascii")
        binary = self.evidence / "sdk-probe.exe"
        self.run("sdk-x64-build", ["cl", "/nologo", "/W4", "/WX", str(probe), f"/Fo{self.evidence / 'sdk-probe.obj'}", f"/Fe{binary}"])
        verify_pe(binary, "x86_64-pc-windows-msvc")
        self.run("sdk-x64-run", [str(binary)])
        cross = subprocess.check_output([
            os.environ["ComSpec"], "/d", "/c",
            'C:\\BuildTools\\Common7\\Tools\\VsDevCmd.bat -no_logo -arch=arm64 -host_arch=x64 >nul && set',
        ], cwd=ROOT, text=True)
        environment = dict(os.environ)
        for line in cross.splitlines():
            name, separator, value = line.partition("=")
            if separator and name.upper() in {"PATH", "INCLUDE", "LIB", "LIBPATH", "VSCMD_ARG_HOST_ARCH", "VSCMD_ARG_TGT_ARCH"}:
                # Preserve Windows' case-insensitive environment behavior in the dictionary.
                environment = {key: item for key, item in environment.items() if key.upper() != name.upper()}
                environment[name.upper()] = value
        if environment.get("VSCMD_ARG_HOST_ARCH") != "x64" or environment.get("VSCMD_ARG_TGT_ARCH") != "arm64":
            raise ValueError("The existing ARM64 cross SDK is unavailable.")
        compiler = shutil.which("cl.exe", path=environment["PATH"])
        if compiler is None:
            raise ValueError("The existing ARM64 cross compiler is unavailable.")
        binary = self.evidence / "sdk-probe-arm64.exe"
        self.run("sdk-arm64-build", [compiler, "/nologo", "/W4", "/WX", str(probe), f"/Fo{self.evidence / 'sdk-probe-arm64.obj'}", f"/Fe{binary}"], environment=environment)
        verify_pe(binary, "aarch64-pc-windows-msvc")
        write_json(self.handoff / "source.json", self.identity)

    def source_v8(self) -> None:
        try:
            self.bazel("v8-x64-source-build", "build", *BAZEL_FLAGS, *NATIVE_V8)
            outputs = self.bazel("v8-x64-source-outputs", "cquery", *BAZEL_FLAGS, "--output=files", "set(" + " ".join(NATIVE_V8) + ")", capture_output=True)
            execution_root = Path(self.bazel("v8-execution-root", "info", "execution_root", capture_output=True).strip())
            output_base = Path(self.bazel("v8-output-base", "info", "output_base", capture_output=True).strip())

            def resolve(value: str) -> Path:
                path = Path(value.strip())
                return path if path.is_absolute() else (output_base if value.startswith("external/") else execution_root) / path

            paths = [resolve(line) for line in outputs.splitlines() if line.strip()]
            archives = [path for path in paths if path.suffix == ".lib"]
            bindings = [path for path in paths if path.suffix == ".rs"]
            if len(archives) != 1 or len(bindings) != 1:
                raise ValueError("The native producer must return one archive and one matching binding.")
            if any(not path.is_file() or path.stat().st_size == 0 for path in paths):
                raise ValueError("The native source archive and binding were not materialized by Bazel.")
            target = "x86_64-pc-windows-msvc"
            directory = self.evidence / "v8" / target
            stage_artifacts(target, archives[0], bindings[0], directory, True)
            compilers = self.bazel("v8-pinned-clang", "cquery", *BAZEL_FLAGS, "--output=files", "@v8_native_windows_llvm//:compiler_files", capture_output=True)
            clang_paths = [resolve(line) for line in compilers.splitlines() if line.strip().replace("\\", "/").endswith("/clang-cl.exe")]
            if len(clang_paths) != 1 or not clang_paths[0].is_file():
                raise ValueError("The source-pinned Windows LLVM compiler is unavailable.")
            pair = self.pair_receipt(target, directory, "source-native-windows")
            pair["compiler_directory"] = str(clang_paths[0].parent)
            write_json(self.handoff / f"v8-{target}.json", pair)
        finally:
            # Let the runner's normal cleanup remove its workspace without a live
            # Bazel process holding entries open. Cargo retains its own cache.
            self.bazel("v8-bazel-shutdown", "shutdown")

    def pair_receipt(self, target: str, directory: Path, producer: str) -> dict:
        profile = "ptrcomp_sandbox_release"
        archive = directory / f"rusty_v8_{profile}_{target}.lib.gz"
        binding = directory / f"src_binding_{profile}_{target}.rs"
        manifest = directory / f"rusty_v8_{profile}_{target}.sha256"
        entries = {line.split()[1]: line.split()[0] for line in manifest.read_text().splitlines()}
        if set(entries) != {archive.name, binding.name} or any(entries[path.name] != digest(path) for path in (archive, binding)):
            raise ValueError("The V8 archive and binding checksum pair differs.")
        if producer == "published-source-pinned":
            with (ROOT / "codex-rs/Cargo.lock").open("rb") as stream:
                versions = {item["version"] for item in tomllib.load(stream)["package"] if item["name"] == "v8"}
            if len(versions) != 1:
                raise ValueError("Expected one locked V8 crate version.")
            trusted = ROOT / "third_party/v8" / f"rusty_v8_{next(iter(versions)).replace('.', '_')}_release_manifests.sha256"
            pins = {line.split()[1]: line.split()[0] for line in trusted.read_text().splitlines()}
            if pins.get(manifest.name) != digest(manifest):
                raise ValueError("The published V8 manifest does not match the committed pin.")
        if re.search(r"v8__TYPED_ARRAY_MAX_SIZE_IN_HEAP\s*:\s*usize\s*=\s*0\s*;", binding.read_text(encoding="utf-8")) is None:
            raise ValueError("The Windows archive must use its documented zero inline-heap binding policy.")
        return {"target": target, "profile": "sandbox_release", "producer": producer, "source_commit": self.identity["source_commit"],
                "source_built": producer == "source-native-windows", "files": {path.name: digest(path) for path in (archive, binding, manifest)},
                "archive": {"name": archive.name, "sha256": digest(archive)}, "binding": {"name": binding.name, "sha256": digest(binding)},
                "manifest_sha256": digest(manifest)}

    def copy_published_v8_pair(self, target: str, environment: dict) -> Path:
        """Copy the authenticated published pair into the immutable handoff tree."""
        source_paths = [Path(environment[name]) for name in ("RUSTY_V8_ARCHIVE", "RUSTY_V8_SRC_BINDING_PATH")]
        checksums = source_paths[0].with_name(f"rusty_v8_ptrcomp_sandbox_release_{target}.sha256")
        source_paths.append(checksums)
        if any(not path.is_file() for path in source_paths):
            raise ValueError("The checksum-pinned Windows V8 pair is incomplete.")
        directory = self.evidence / "v8" / target
        directory.mkdir(parents=True, exist_ok=True)
        for source in source_paths:
            shutil.copy2(source, directory / source.name)
        return directory

    def native_environment(self) -> dict:
        target = "x86_64-pc-windows-msvc"
        pair = self.evidence / "v8" / target
        record = json.loads((self.handoff / f"v8-{target}.json").read_text())
        if self.pair_receipt(target, pair, "source-native-windows")["files"] != record["files"]:
            raise ValueError("The staged native V8 pair changed after its source build.")
        return {**os.environ, "RUSTY_V8_ARCHIVE": str(pair / record["archive"]["name"]),
                "RUSTY_V8_SRC_BINDING_PATH": str(pair / record["binding"]["name"]),
                "LIBCLANG_PATH": record["compiler_directory"], "PATH": record["compiler_directory"] + os.pathsep + os.environ["PATH"]}

    def published_v8(self) -> None:
        # Reuse the canonical checksum verifier without the composite action's
        # python3 alias requirement on a native Windows Python installation.
        sys.path.insert(0, str(ROOT / "scripts"))
        from codex_package.targets import TARGET_SPECS
        from codex_package.v8 import fetch_codex_v8_artifacts

        target = "aarch64-pc-windows-msvc"
        pair = fetch_codex_v8_artifacts(TARGET_SPECS[target], cache_root=self.evidence / "published-v8")
        environment = {"RUSTY_V8_ARCHIVE": str(pair.archive), "RUSTY_V8_SRC_BINDING_PATH": str(pair.binding)}
        directory = self.copy_published_v8_pair(target, environment)
        receipt = self.pair_receipt(target, directory, "published-source-pinned")
        self.record_check("v8-arm64-published-pair", json.dumps(receipt, sort_keys=True))
        write_json(self.handoff / f"v8-{target}.json", receipt)

    def acceptance(self) -> None:
        environment = self.native_environment()
        checks = {
            "v8-x64-poc-cppgc": ["--target", "x86_64-pc-windows-msvc", "-p", "codex-v8-poc", "--run-ignored", "all"],
            "windows-product-regressions": ["--target", "x86_64-pc-windows-msvc", "-p", "codex-code-mode-runtime", "-p", "codex-account-registry", "-p", "codex-account-selection", "-p", "codex-private-storage", "-p", "codex-usage", "-p", "codex-login"],
        }
        for name, arguments in checks.items():
            command = ["just", "test", "--locked", "--test-threads", "4", *arguments]
            if name == "windows-product-regressions":
                command[2:2] = ["--config-file", os.environ["CODEX_WINDOWS_NEXTEST_CONFIG_FILE"]]
            self.run(name, command, environment=environment)

    def package(self, target: str) -> None:
        environment = self.native_environment() if target.startswith("x86_64") else dict(os.environ)
        if target.startswith("aarch64"):
            if environment.get("VSCMD_ARG_HOST_ARCH") != "x64" or environment.get("VSCMD_ARG_TGT_ARCH") != "arm64":
                raise ValueError("The x64-host/ARM64-target MSVC SDK environment is required.")
            directory = self.evidence / "v8" / target
            pair = self.pair_receipt(target, directory, "published-source-pinned")
            environment["RUSTY_V8_ARCHIVE"] = str(directory / pair["archive"]["name"])
            environment["RUSTY_V8_SRC_BINDING_PATH"] = str(directory / pair["binding"]["name"])
            x64_pair = json.loads((self.handoff / "v8-x86_64-pc-windows-msvc.json").read_text())
            compiler = Path(x64_pair["compiler_directory"])
            for name, tool in (("CC", "clang-cl.exe"), ("CXX", "clang-cl.exe"), ("AR", "llvm-lib.exe")):
                environment[f"{name}_aarch64_pc_windows_msvc"] = str(compiler / tool)
            environment["LIBCLANG_PATH"] = str(compiler)
            environment["PATH"] = str(compiler) + os.pathsep + environment["PATH"]
            self.run("v8-arm64-compile-link", ["cargo", "nextest", "list", "--manifest-path", "codex-rs/Cargo.toml", "--config", "codex-rs/.cargo/config.toml", "--locked", "--target", target, "--list-type", "binaries-only", "--message-format", "json", "-p", "codex-v8-poc"], environment=environment)
            pair["runtime_not_executed"] = "The owner-controlled runner is x64; ARM64 receives compile/link and PE architecture proof only."
            write_json(self.handoff / f"v8-{target}.json", pair)
            # clang-cl needs explicit C++ exceptions for bundled DuckDB.
            cxx_flags = "CXXFLAGS_aarch64_pc_windows_msvc"
            environment[cxx_flags] = (environment.get(cxx_flags, "") + " /EHsc").strip()
        arguments = ["cargo", "build", "--timings", "--manifest-path", "codex-rs/Cargo.toml", "--config", "codex-rs/.cargo/config.toml", "--locked", "--release", "--target", target]
        self.run(f"{target}-build", [*arguments, *(value for binary in BINARIES for value in ("--bin", binary))], environment=environment)
        self.capture_cargo_timings(target, environment)
        release = Path(environment["CARGO_TARGET_DIR"]) / target / "release"
        output = self.handoff / target
        output.mkdir()
        package = self.evidence / "packages" / target
        self.run(f"{target}-assemble", ["just", "assemble-codex-package", "--target", target, "--package-version", self.identity["version"], "--package-dir", str(package),
                 "--entrypoint-bin", str(release / "codex.exe"), "--code-mode-host-bin", str(release / "codex-code-mode-host.exe"),
                 "--exec-server-bin", str(release / "codex-exec-server.exe"), "--codex-command-runner-bin", str(release / "codex-command-runner.exe"),
                 "--codex-windows-sandbox-setup-bin", str(release / "codex-windows-sandbox-setup.exe"), "--archive-output", str(output / f"codex-package-{target}.tar.gz")], environment=environment)
        pe_entries = []
        for executable in package.rglob("*.exe"):
            verify_pe(executable, target)
            pe_entries.append(f"{executable.relative_to(package)}: machine=0x{TARGETS[target]:04x}")
        self.record_check(f"{target}-pe-architecture", "\n".join(sorted(pe_entries)))
        with tarfile.open(output / f"codex-package-{target}.symbols.tar.gz", "w:gz") as archive:
            for binary in BINARIES:
                candidates = [release / f"{binary.replace('-', '_')}.pdb", release / f"{binary}.pdb"]
                pdb = next((path for path in candidates if path.is_file()), None)
                if pdb is None or pdb.stat().st_size == 0:
                    raise ValueError(f"Debug symbols are required for {binary}.")
                archive.add(pdb, arcname=f"{binary}.pdb")
        if target.startswith("x86_64"):
            isolated = self.evidence / "package-smoke-home"
            isolated.mkdir()
            environment = {**environment, "CODEX_HOME": str(isolated)}
            for name, command in {
                "package-x64-version": [str(package / "bin/codex.exe"), "--version"],
                "package-x64-help": [str(package / "bin/codex.exe"), "--help"],
                "package-x64-features": [str(package / "bin/codex.exe"), "features", "list"],
                "package-x64-accounts": [str(package / "bin/codex.exe"), "account", "--json", "list"],
                "package-x64-executor": [str(package / "bin/codex-exec-server.exe"), "--version"],
                "package-x64-code-mode-host": [str(package / "bin/codex-code-mode-host.exe"), "--help"],
            }.items():
                result = self.run(name, command, environment=environment, capture_output=True)
                if name == "package-x64-version" and result.strip() != f"codex-cli {self.identity['version']}":
                    raise ValueError("Packaged executable version differs from the source version.")
                if name == "package-x64-accounts":
                    json.loads(result)
        artifacts = [{"name": path.name, "sha256": digest(path), "size_bytes": path.stat().st_size} for path in sorted(output.glob("*.tar.gz"))]
        (output / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['name']}\n" for item in artifacts), encoding="utf-8")
        write_json(self.handoff / f"package-{target}.json", {**self.identity, "target": target, "artifacts": artifacts,
                   "runtime_executed": target.startswith("x86_64"), "pe_machine": TARGETS[target]})

    def finalize(self) -> None:
        if source_identity(os.environ["SOURCE_SHA"]) != self.identity:
            raise ValueError("Source identity changed before sealing the handoff.")
        checks = [json.loads(path.read_text()) for path in sorted((self.evidence / "checks").glob("*.json"))]
        required = {"git-longpaths", "cargo-source-metadata", "v8-x64-poc-cppgc", "windows-product-regressions", "v8-arm64-published-pair", "v8-arm64-compile-link", "sdk-arm64-build", "package-x64-version", "package-x64-accounts"}
        required.update(f"{target}-{phase}" for target in TARGETS for phase in ("build", "assemble", "pe-architecture"))
        if not required.issubset({check["name"] for check in checks}) or any(check["exit_code"] != 0 or digest(self.handoff / check["log_path"]) != check["log_sha256"] for check in checks):
            raise ValueError("Successful, unchanged Windows acceptance evidence is required.")
        pairs = [json.loads((self.handoff / f"v8-{target}.json").read_text()) for target in TARGETS]
        pairs[0]["poc_cppgc_result"] = "passed"
        pairs[0]["cppgc_ignored_tests_executed"] = True
        pairs[1]["poc_cppgc_result"] = "compiled_and_linked_not_executed"
        pairs[1]["cppgc_ignored_tests_executed"] = False
        packages = [json.loads((self.handoff / f"package-{target}.json").read_text()) for target in TARGETS]
        for package, pair in zip(packages, pairs, strict=True):
            for item in package["artifacts"]:
                path = self.handoff / package["target"] / item["name"]
                if path.stat().st_size != item["size_bytes"] or digest(path) != item["sha256"]:
                    raise ValueError("A native archive changed before the handoff was sealed.")
            # The importer bundles target directories independently. Keep the
            # evidence named by each receipt within that same target subtree.
            shutil.copytree(self.logs, self.handoff / package["target"] / "logs", dirs_exist_ok=True)
            write_json(self.handoff / package["target"] / "native-manifest.json", {
                "schema": 1,
                "status": "success",
                "source_commit": package["source_commit"],
                "native_tree": self.identity["native_tree"],
                "v8_tree": self.identity["v8_tree"],
                "target": package["target"],
                "version": self.identity["version"],
                "runtime_executed": package["runtime_executed"],
                "runtime_not_executed_reason": pair.get("runtime_not_executed"),
                "checks": checks,
                "v8_pair": pair,
                "archives": {item["name"]: {"sha256": item["sha256"], "size_bytes": item["size_bytes"]} for item in package["artifacts"]},
            })
        write_json(self.handoff / "windows-manifest.json", {"schema": 1, **self.identity, "producer": "windows-private-controller", "status": "success",
                   "checks": checks, "packages": packages, "v8_pairs": pairs, "controller": json.loads((self.evidence / "runner.json").read_text(encoding="utf-8-sig"))})
        (self.handoff / "source-commit.txt").write_text(self.identity["source_commit"] + "\n", encoding="ascii")
        (self.handoff / "source-version.txt").write_text(self.identity["version"] + "\n", encoding="ascii")

    def finalize_target(self, target: str) -> None:
        """Seal one target so a later target failure cannot erase its proof."""
        if target not in TARGETS:
            raise ValueError(f"Unknown target: {target}")
        if source_identity(os.environ["SOURCE_SHA"]) != self.identity:
            raise ValueError("Source identity changed before the target checkpoint.")
        checks = [json.loads(path.read_text()) for path in sorted((self.evidence / "checks").glob("*.json"))]
        required = {"git-longpaths", "cargo-source-metadata", "windows-product-regressions"}
        if target.startswith("x86_64"):
            required.update({"v8-x64-poc-cppgc", "package-x64-version", "package-x64-accounts"})
        else:
            required.update({"v8-arm64-published-pair", "v8-arm64-compile-link", "sdk-arm64-build"})
        required.update({f"{target}-{phase}" for phase in ("build", "assemble", "pe-architecture")})
        names = {check["name"] for check in checks}
        if not required.issubset(names):
            raise ValueError(f"Target checkpoint is missing successful checks: {sorted(required - names)}")
        selected = [check for check in checks if check["name"] in required]
        if any(check["exit_code"] != 0 or digest(self.handoff / check["log_path"]) != check["log_sha256"] for check in selected):
            raise ValueError("Target checkpoint contains failed or changed evidence.")
        pair = json.loads((self.handoff / f"v8-{target}.json").read_text())
        pair["poc_cppgc_result"] = "passed" if target.startswith("x86_64") else "compiled_and_linked_not_executed"
        pair["cppgc_ignored_tests_executed"] = target.startswith("x86_64")
        package = json.loads((self.handoff / f"package-{target}.json").read_text())
        target_root = self.handoff / target
        target_root.mkdir(parents=True, exist_ok=True)
        for item in package["artifacts"]:
            source = self.handoff / target / item["name"]
            if source.stat().st_size != item["size_bytes"] or digest(source) != item["sha256"]:
                raise ValueError("A target archive changed before checkpoint sealing.")
        shutil.copytree(self.logs, target_root / "logs", dirs_exist_ok=True)
        write_json(target_root / "native-manifest.json", {
            "schema": 1, "status": "success", "source_commit": package["source_commit"],
            "native_tree": self.identity["native_tree"], "v8_tree": self.identity["v8_tree"],
            "target": target, "version": self.identity["version"],
            "runtime_executed": package["runtime_executed"],
            "runtime_not_executed_reason": pair.get("runtime_not_executed"),
            "checks": selected, "v8_pair": pair,
            "archives": {item["name"]: {"sha256": item["sha256"], "size_bytes": item["size_bytes"]} for item in package["artifacts"]},
        })
        write_json(self.handoff / f"windows-manifest-{target}.json", {
            "schema": 1, **self.identity, "producer": "windows-private-controller",
            "status": "success", "checks": selected, "packages": [package], "v8_pairs": [pair],
        })


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("prerequisites", "source-v8", "acceptance", "published-v8", "package", "finalize-target", "finalize"))
    parser.add_argument("--target", choices=TARGETS)
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("Native execution belongs to the existing private Windows controller.")
    producer = Producer()
    if args.phase == "package":
        if args.target is None:
            parser.error("package requires --target")
        producer.package(args.target)
    elif args.phase == "finalize-target":
        if args.target is None:
            parser.error("finalize-target requires --target")
        producer.finalize_target(args.target)
    else:
        {"prerequisites": producer.prerequisites, "source-v8": producer.source_v8, "acceptance": producer.acceptance, "published-v8": producer.published_v8, "finalize": producer.finalize}[args.phase]()


if __name__ == "__main__":
    main()
