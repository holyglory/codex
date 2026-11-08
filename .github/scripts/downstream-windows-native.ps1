#!/usr/bin/env python3
"""Build exact-source Windows packages inside the private owner-only controller."""

import argparse
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
)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


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
        self.logs.mkdir(parents=True, exist_ok=True)

    def run(self, name: str, command: list[str], *, environment: dict | None = None, capture_output: bool = False) -> str:
        log = self.logs / f"{name}.log"
        if log.exists():
            raise ValueError(f"Refusing to replace retained check evidence: {name}")
        print(json.dumps({"check": name, "state": "started"}), flush=True)
        with log.open("w", encoding="utf-8") as stream:
            result = subprocess.run(command, cwd=ROOT, env=environment, stdout=subprocess.PIPE if capture_output else stream, stderr=stream, text=True, encoding="utf-8", errors="replace", check=False)
            if capture_output:
                stream.write(result.stdout)
        receipt = {"name": name, "exit_code": result.returncode, "log_sha256": digest(log), "log_path": f"logs/{log.name}", "command": command}
        write_json(self.evidence / "checks" / f"{name}.json", receipt)
        print(json.dumps({"check": name, "exit_code": result.returncode}), flush=True)
        result.check_returncode()
        return result.stdout if capture_output else ""

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

    def bazel(self, name: str, *arguments: str, capture_output: bool = False) -> str:
        return self.run(name, [sys.executable, str(ROOT / ".github/scripts/run_bazel_with_buildbuddy.py"), *arguments], capture_output=capture_output)

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
        probe = self.evidence / "sdk-probe.c"
        probe.write_text('#include <windows.h>\nint main(void) { return sizeof(void*) == 8 ? 0 : 1; }\n', encoding="ascii")
        binary = self.evidence / "sdk-probe.exe"
        self.run("sdk-x64-build", ["cl", "/nologo", "/W4", "/WX", str(probe), f"/Fo{self.evidence / 'sdk-probe.obj'}", f"/Fe{binary}"])
        verify_pe(binary, "x86_64-pc-windows-msvc")
        self.run("sdk-x64-run", [str(binary)])
        write_json(self.handoff / "source.json", self.identity)

    def source_v8(self) -> None:
        self.bazel("v8-x64-source-build", "build", "--remote_download_all", *BAZEL_FLAGS, *NATIVE_V8)
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
        return {**os.environ, "RUSTY_V8_ARCHIVE": str(pair / record["archive"]["name"]),
                "RUSTY_V8_SRC_BINDING_PATH": str(pair / record["binding"]["name"]),
                "LIBCLANG_PATH": record["compiler_directory"], "PATH": record["compiler_directory"] + os.pathsep + os.environ["PATH"]}

    def acceptance(self) -> None:
        environment = self.native_environment()
        checks = {
            "v8-x64-poc-cppgc": ["--target", "x86_64-pc-windows-msvc", "-p", "codex-v8-poc", "--run-ignored", "all"],
            "windows-product-regressions": ["--target", "x86_64-pc-windows-msvc", "-p", "codex-code-mode-runtime", "-p", "codex-account-registry", "-p", "codex-account-selection", "-p", "codex-private-storage", "-p", "codex-usage"],
        }
        for name, arguments in checks.items():
            self.run(name, ["just", "test", "--locked", "--test-threads", "4", *arguments], environment=environment)

    def package(self, target: str) -> None:
        environment = self.native_environment() if target.startswith("x86_64") else dict(os.environ)
        if target.startswith("aarch64"):
            if environment.get("VSCMD_ARG_HOST_ARCH") != "x64" or environment.get("VSCMD_ARG_TGT_ARCH") != "arm64":
                raise ValueError("The x64-host/ARM64-target MSVC SDK environment is required.")
            directory = self.copy_published_v8_pair(target, environment)
            pair = self.pair_receipt(target, directory, "published-source-pinned")
            x64_pair = json.loads((self.handoff / "v8-x86_64-pc-windows-msvc.json").read_text())
            compiler = Path(x64_pair["compiler_directory"])
            for name, tool in (("CC", "clang-cl.exe"), ("CXX", "clang-cl.exe"), ("AR", "llvm-lib.exe")):
                environment[f"{name}_aarch64_pc_windows_msvc"] = str(compiler / tool)
            environment["LIBCLANG_PATH"] = str(compiler)
            environment["PATH"] = str(compiler) + os.pathsep + environment["PATH"]
            self.run("v8-arm64-compile-link", ["cargo", "nextest", "list", "--manifest-path", "codex-rs/Cargo.toml", "--config", "codex-rs/.cargo/config.toml", "--locked", "--target", target, "--list-type", "binaries-only", "--message-format", "json", "-p", "codex-v8-poc"], environment=environment)
            pair["runtime_not_executed"] = "The owner-controlled runner is x64; ARM64 receives compile/link and PE architecture proof only."
            write_json(self.handoff / f"v8-{target}.json", pair)
        arguments = ["cargo", "build", "--manifest-path", "codex-rs/Cargo.toml", "--config", "codex-rs/.cargo/config.toml", "--locked", "--release", "--target", target]
        self.run(f"{target}-build", [*arguments, *(value for binary in BINARIES for value in ("--bin", binary))], environment=environment)
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
        required = {"v8-x64-poc-cppgc", "windows-product-regressions", "v8-arm64-compile-link", "package-x64-version", "package-x64-accounts"}
        required.update(f"{target}-{phase}" for target in TARGETS for phase in ("build", "assemble"))
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("prerequisites", "source-v8", "acceptance", "package", "finalize"))
    parser.add_argument("--target", choices=TARGETS)
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("Native execution belongs to the existing private Windows controller.")
    producer = Producer()
    if args.phase == "package":
        if args.target is None:
            parser.error("package requires --target")
        producer.package(args.target)
    else:
        {"prerequisites": producer.prerequisites, "source-v8": producer.source_v8, "acceptance": producer.acceptance, "finalize": producer.finalize}[args.phase]()


if __name__ == "__main__":
    main()
