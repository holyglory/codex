"""Exercise the source/artifact/native-verification boundary without a compiler."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import local_native_artifacts as artifacts
import local_native_release as release
import local_native_transport as transport


class NativeReleaseTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "source"
        (self.root / "codex-rs").mkdir(parents=True)
        (self.root / ".github/workflows").mkdir(parents=True)
        (self.root / "codex-rs/Cargo.toml").write_text(
            '[workspace.package]\nversion="1.2.3+multi.4"\n'
        )
        (self.root / ".github/workflows/rust-release.yml").write_text(
            "- uses: dtolnay/rust-toolchain@pinned # 1.96.0\n"
        )
        for command in [
            ["git", "init", "--quiet"],
            ["git", "add", "."],
            [
                "git",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ]:
            subprocess.run(command, cwd=self.root, check=True, capture_output=True)
        self.commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=self.root, text=True
        ).strip()
        self.target = "aarch64-unknown-linux-musl"
        self.incoming = self.base / "compiled"
        (self.incoming / "v8/logs").mkdir(parents=True)
        (self.incoming / "v8/logs/v8-source-pair.log").write_text(
            "compiled V8 fixture\n"
        )
        (self.incoming / "nextest-config.toml").write_text(
            "[profile.default]\nretries=1\n"
        )
        (self.incoming / f"tests-{self.target}.tar.zst").write_bytes(b"archive fixture")
        self.producer = {
            "helper_commit": "b" * 40,
            "helper_files": {
                "scripts/local_native_release.py": {
                    "sha256": "c" * 64,
                    "size_bytes": 100,
                }
            },
        }
        self.bwrap = None
        with tarfile.open(
            self.incoming / f"codex-package-{self.target}.tar.gz", "w:gz"
        ) as archive:
            for name in [
                "bin/codex",
                "bin/codex-code-mode-host",
                "bin/codex-exec-server",
                "codex-resources/bwrap",
                "codex-path/rg",
            ]:
                interpreter = (
                    b"/lib/ld-linux-aarch64.so.1\0" if name == "codex-path/rg" else b""
                )
                data = bytearray(120 + len(interpreter))
                data[:6] = b"\x7fELF\x02\x01"
                struct.pack_into("<H", data, 18, 183)
                struct.pack_into("<Q", data, 32, 64)
                struct.pack_into("<HH", data, 54, 56, 1)
                struct.pack_into("<I", data, 64, 3 if interpreter else 1)
                if interpreter:
                    struct.pack_into("<Q", data, 72, 120)
                    struct.pack_into("<Q", data, 96, len(interpreter))
                    data[120:] = interpreter
                info = tarfile.TarInfo(name)
                info.mode, info.size = 0o755, len(data)
                archive.addfile(info, io.BytesIO(data))
                if name == "codex-resources/bwrap":
                    self.bwrap = {
                        "sha256": hashlib.sha256(data).hexdigest(),
                        "size_bytes": len(data),
                    }
        (self.incoming / "v8-pair.json").write_text(
            json.dumps(
                {"v8_tree": "d" * 40, "github_run_id": "123", "github_run_attempt": "1"}
            )
        )
        self.manifest = {
            "schema": 1,
            "kind": "codex-prebuilt-target",
            **artifacts.source_identity(self.root, self.commit),
            "target": self.target,
            "rust_version": "1.96.0",
            "nextest_version": "0.9.151",
            "status": "compiled",
            "runtime_executed": False,
            "producer": self.producer,
            "bwrap": self.bwrap,
            "files": artifacts.inventory(self.incoming),
        }
        artifacts.write_json(self.incoming / "build-manifest.json", self.manifest)
        self.manifest_digest = artifacts.file_info(
            self.incoming / "build-manifest.json"
        )["sha256"]
        self.args = argparse.Namespace(
            source_root=self.root,
            commit=self.commit,
            target=self.target,
            input=self.incoming,
            manifest_sha256=self.manifest_digest,
            output=self.base / "verified",
        )

    def native_environment(self):
        return patch.multiple(
            artifacts.platform, system=lambda: "Linux", machine=lambda: "aarch64"
        )

    def fake_native_check(self, runner, name, command, *, timeout=36000):
        self.assertNotIn("build", command)
        self.assertNotEqual(command[0], "cargo")
        if command[0] == "just":
            self.assertEqual(command[:2], ["just", "test"])
            self.assertIn("--archive-file", command)
            self.assertIn("--workspace-remap", command)
            self.assertIn("platform(target)", command[command.index("-E") + 1])
        text = (
            "codex-cli 1.2.3+multi.4\n"
            if name == "native-version"
            else "native fixture passed\n"
        )
        log = runner.directory / "logs" / f"{name}.log"
        log.write_text(text)
        runner.checks.append(
            {
                "name": name,
                "exit_code": 0,
                "command": command,
                "log_sha256": artifacts.file_info(log)["sha256"],
            }
        )
        return text

    def verify(self):
        with (
            self.native_environment(),
            patch.object(release, "helper_identity", return_value=self.producer),
            patch.object(release, "nextest_version", return_value="0.9.151"),
            patch.dict(os.environ, {}, clear=True),
            patch.object(
                release.Commands,
                "run",
                autospec=True,
                side_effect=self.fake_native_check,
            ) as run,
        ):
            result = release.verify_target(self.args)
        return result, run

    def test_verify_and_seal_same_release_artifact_without_compilation(self):
        result, run = self.verify()
        self.assertEqual(run.call_count, 5)
        poc, regressions = [item.args[2] for item in run.call_args_list[:2]]
        self.assertEqual(
            poc[-4:],
            ["-E", "platform(target) & package(codex-v8-poc)", "--run-ignored", "all"],
        )
        self.assertNotIn("--run-ignored", regressions)
        rg = next(
            item for item in run.call_args_list if item.args[1] == "native-rg-version"
        )
        self.assertEqual(
            rg.args[2],
            [str(self.args.output / "package/codex-path/rg"), "--version"],
        )
        self.assertEqual(rg.kwargs, {"timeout": 60})
        receipt = artifacts.load_json(self.args.output / "verification.json")
        self.assertEqual(
            {
                key: receipt[key]
                for key in [
                    "target",
                    "build_manifest_sha256",
                    "runtime_host",
                    "runtime_executed",
                ]
            },
            {
                "target": self.target,
                "build_manifest_sha256": self.manifest_digest,
                "runtime_host": {"os": "Linux", "architecture": "aarch64"},
                "runtime_executed": True,
            },
        )
        args = argparse.Namespace(
            **{
                **vars(self.args),
                "verification": self.args.output,
                "verification_sha256": result["sha256"],
                "output": self.base / "sealed",
            }
        )
        with patch.object(release, "helper_identity", return_value=self.producer):
            release.seal_target(args)
        native = artifacts.load_json(args.output / "native-manifest.json")
        self.assertEqual(
            native["archives"],
            {
                f"codex-package-{self.target}.tar.gz": artifacts.file_info(
                    self.incoming / f"codex-package-{self.target}.tar.gz"
                )
            },
        )
        self.assertTrue(native["runtime_executed"])
        self.assertEqual(native["status"], "success")
        self.assertEqual(
            artifacts.file_info(self.incoming / f"codex-package-{self.target}.tar.gz"),
            artifacts.file_info(args.output / f"codex-package-{self.target}.tar.gz"),
        )

    @unittest.skipUnless(os.name == "posix", "Unix producer command streaming")
    def test_command_streams_large_output_and_preserves_failed_exit(self):
        payload = "x" * (1024 * 1024) + "\n"
        command = [
            sys.executable,
            "-c",
            "import sys; print('x' * (1024 * 1024)); sys.exit(7)",
        ]
        output = self.base / "streamed"
        code = (
            "import json,os,sys; from pathlib import Path; "
            "sys.path.insert(0,sys.argv[1]); from local_native_release import Commands; "
            "runner=Commands(Path.cwd(),Path(sys.argv[2]),dict(os.environ))\n"
            "try:\n runner.run('fixture',json.loads(sys.argv[3]),timeout=10)\n"
            "except ValueError:\n print(json.dumps(runner.checks))\n"
        )
        result = subprocess.run(
            [
                sys.executable,
                "-c",
                code,
                str(Path(release.__file__).parent),
                str(output),
                json.dumps(command),
            ],
            capture_output=True,
            text=True,
            timeout=20,
            check=True,
        )
        self.assertIn(payload, result.stdout)
        self.assertEqual((output / "logs/fixture.log").read_text(), payload)
        self.assertEqual(
            json.loads(result.stdout.splitlines()[-1]),
            [
                {
                    "name": "fixture",
                    "exit_code": 7,
                    "command": command,
                    "log_sha256": hashlib.sha256(payload.encode()).hexdigest(),
                }
            ],
        )

    def test_tamper_source_and_wrong_native_host_are_refused_before_execution(self):
        for mutation, message in [
            ("host", "native Linux"),
            ("source", "source"),
            ("archive", "changed"),
            ("manifest", "digest"),
        ]:
            with self.subTest(mutation=mutation):
                args = argparse.Namespace(**vars(self.args))
                if mutation == "source":
                    args.commit = "f" * 40
                if mutation == "manifest":
                    args.manifest_sha256 = "e" * 64
                archive = self.incoming / f"tests-{self.target}.tar.zst"
                original = archive.read_bytes()
                if mutation == "archive":
                    archive.write_bytes(b"tampered")
                try:
                    with (
                        patch.object(
                            artifacts.platform, "system", return_value="Linux"
                        ),
                        patch.object(
                            artifacts.platform,
                            "machine",
                            return_value="x86_64" if mutation == "host" else "aarch64",
                        ),
                        patch.object(release.Commands, "run") as execute,
                        self.assertRaisesRegex(ValueError, message),
                    ):
                        release.verify_target(args)
                    execute.assert_not_called()
                finally:
                    archive.write_bytes(original)

    def test_failed_native_tests_never_emit_runtime_receipt(self):
        def fail(runner, name, command, **kwargs):
            (runner.directory / "logs" / f"{name}.log").write_text(
                "native boundary failure\n"
            )
            raise ValueError(f"{name} failed")

        with (
            self.native_environment(),
            patch.object(release, "helper_identity", return_value=self.producer),
            patch.object(release, "nextest_version", return_value="0.9.151"),
            patch.dict(os.environ, {}, clear=True),
            patch.object(release.Commands, "run", autospec=True, side_effect=fail),
            self.assertRaisesRegex(ValueError, "v8-poc failed"),
        ):
            release.verify_target(self.args)
        self.assertFalse((self.args.output / "verification.json").exists())
        self.assertTrue((self.args.output / "logs/v8-poc.log").exists())

    def test_changed_bwrap_binding_is_refused_before_native_tests(self):
        self.manifest["bwrap"] = {**self.bwrap, "sha256": "e" * 64}
        manifest = self.incoming / "build-manifest.json"
        manifest.write_text(json.dumps(self.manifest))
        self.args.manifest_sha256 = artifacts.file_info(manifest)["sha256"]
        with (
            self.native_environment(),
            patch.object(release, "helper_identity", return_value=self.producer),
            patch.object(release, "nextest_version", return_value="0.9.151"),
            patch.dict(os.environ, {}, clear=True),
            patch.object(release.Commands, "run") as execute,
            self.assertRaisesRegex(ValueError, "bwrap differs"),
        ):
            release.verify_target(self.args)
        execute.assert_not_called()

    def test_dynamic_first_party_and_wrong_architecture_are_refused_before_execution(
        self,
    ):
        package = self.incoming / f"codex-package-{self.target}.tar.gz"
        original = package.read_bytes()
        for index, (name, machine, program_type, message) in enumerate(
            [
                ("bin/codex", 183, 3, "static musl"),
                ("bin/codex-code-mode-host", 183, 3, "static musl"),
                ("bin/codex-exec-server", 183, 3, "static musl"),
                ("codex-resources/bwrap", 183, 3, "static musl"),
                ("bin/codex", 62, 1, "architecture"),
                ("codex-path/rg", 62, 3, "architecture"),
            ]
        ):
            with self.subTest(name=name, machine=machine, program_type=program_type):
                with tarfile.open(fileobj=io.BytesIO(original), mode="r:gz") as source:
                    members = [
                        (item, source.extractfile(item).read()) for item in source
                    ]
                with tarfile.open(package, "w:gz") as output:
                    for member, content in members:
                        if member.name == name:
                            content = bytearray(content)
                            struct.pack_into("<H", content, 18, machine)
                            struct.pack_into("<I", content, 64, program_type)
                        output.addfile(member, io.BytesIO(content))
                self.manifest["files"] = artifacts.inventory(self.incoming)
                manifest_path = self.incoming / "build-manifest.json"
                manifest_path.write_text(json.dumps(self.manifest))
                args = argparse.Namespace(
                    **{
                        **vars(self.args),
                        "manifest_sha256": artifacts.file_info(manifest_path)["sha256"],
                        "output": self.base / f"rejected-elf-{index}",
                    }
                )
                with (
                    self.native_environment(),
                    patch.object(
                        release, "helper_identity", return_value=self.producer
                    ),
                    patch.object(release, "nextest_version", return_value="0.9.151"),
                    patch.dict(os.environ, {}, clear=True),
                    patch.object(release.Commands, "run") as execute,
                    self.assertRaisesRegex(ValueError, message),
                ):
                    release.verify_target(args)
                execute.assert_not_called()

    def test_failed_ripgrep_smoke_never_emits_native_verification(self):
        def run_check(runner, name, command, **kwargs):
            if name == "native-rg-version":
                (runner.directory / "logs" / f"{name}.log").write_text(
                    "dynamic loader unavailable\n"
                )
                raise ValueError(f"{name} failed")
            return self.fake_native_check(runner, name, command, **kwargs)

        with (
            self.native_environment(),
            patch.object(release, "helper_identity", return_value=self.producer),
            patch.object(release, "nextest_version", return_value="0.9.151"),
            patch.dict(os.environ, {}, clear=True),
            patch.object(release.Commands, "run", autospec=True, side_effect=run_check),
            self.assertRaisesRegex(ValueError, "native-rg-version failed"),
        ):
            release.verify_target(self.args)
        self.assertFalse((self.args.output / "verification.json").exists())
        self.assertTrue((self.args.output / "logs/native-rg-version.log").exists())

    @unittest.skipUnless(os.name == "posix", "Linux resource-limit preflight")
    def test_compile_preserves_musl_linker_flags_and_uses_source_rust_and_cargo(self):
        import resource

        linker_flags = self.target.upper().replace("-", "_") + "_LDFLAGS"
        original_check_output = subprocess.check_output
        for cargo_version in ["1.96.0", "1.95.0"]:
            with self.subTest(cargo_version=cargo_version):
                output = self.base / ("compile-" + cargo_version)
                args = argparse.Namespace(
                    **{
                        **vars(self.args),
                        "output": output,
                        "cargo_target_dir": self.base / "cargo-cache",
                        "musl_cache": self.base / "musl-cache",
                        "zig": self.base / "zig",
                        "v8_handoff": self.base / "v8",
                        "strip_tool": None,
                    }
                )
                observed = []

                def version(command, **kwargs):
                    if command[0] in {"rustc", "cargo"}:
                        observed.append((command, kwargs["env"]["RUSTUP_TOOLCHAIN"]))
                        value = "1.96.0" if command[0] == "rustc" else cargo_version
                        return f"{command[0]} {value} (fixture)\n"
                    return original_check_output(command, **kwargs)

                def intercept(runner, name, command, **kwargs):
                    self.assertEqual(name, "build-bwrap")
                    self.assertEqual(
                        runner.environment[linker_flags],
                        "-Wl,--as-needed -Wno-unused-command-line-argument",
                    )
                    self.assertEqual(runner.environment["RUSTUP_TOOLCHAIN"], "1.96.0")
                    raise RuntimeError("compile intercepted")

                with (
                    patch.object(
                        release,
                        "native_host",
                        return_value={"os": "Linux", "architecture": "x86_64"},
                    ),
                    patch.object(resource, "getrlimit", return_value=(1024, 65536)),
                    patch.object(resource, "setrlimit") as set_limit,
                    patch.dict(os.environ, {linker_flags: "-Wl,--as-needed"}),
                    patch.object(
                        release.subprocess, "check_output", side_effect=version
                    ),
                    patch("prepare_local_musl.prepare", return_value={}) as prepare,
                    patch.object(release, "stage_v8", return_value={}),
                    patch.object(
                        release.Commands, "run", autospec=True, side_effect=intercept
                    ) as execute,
                ):
                    if cargo_version == "1.96.0":
                        with self.assertRaisesRegex(
                            RuntimeError, "compile intercepted"
                        ):
                            release.compile_target(args)
                        execute.assert_called_once()
                    else:
                        with self.assertRaisesRegex(
                            ValueError, "Cargo version differs"
                        ):
                            release.compile_target(args)
                        prepare.assert_not_called()
                        execute.assert_not_called()
                    set_limit.assert_called_once_with(
                        resource.RLIMIT_NOFILE, (16384, 65536)
                    )
                self.assertEqual(
                    observed,
                    [
                        (["rustc", "--version"], "1.96.0"),
                        (["cargo", "--version"], "1.96.0"),
                    ],
                )

    def test_seal_rejects_replaced_verification_log(self):
        result, _ = self.verify()
        (self.args.output / "logs/product-regressions.log").write_text("changed")
        args = argparse.Namespace(
            **{
                **vars(self.args),
                "verification": self.args.output,
                "verification_sha256": result["sha256"],
                "output": self.base / "sealed",
            }
        )
        with (
            patch.object(release, "helper_identity", return_value=self.producer),
            self.assertRaisesRegex(ValueError, "log changed"),
        ):
            release.seal_target(args)
        self.assertFalse(args.output.exists())

    def test_safe_extraction_rejects_unsafe_members_and_extension_headers(self):
        for name, kind, size in [
            ("../outside", tarfile.REGTYPE, 0),
            ("link", tarfile.SYMTYPE, 0),
            ("header", tarfile.XHDTYPE, 65537),
            ("long-name", tarfile.GNUTYPE_LONGNAME, 65537),
        ]:
            with self.subTest(name=name):
                archive = self.base / "unsafe.tar.gz"
                with tarfile.open(archive, "w:gz") as output:
                    info = tarfile.TarInfo(name)
                    info.type, info.linkname = kind, "/outside"
                    info.size = size
                    output.addfile(info, io.BytesIO(b"x" * size))
                expected = "extension header" if size else "."
                with self.assertRaisesRegex(ValueError, expected):
                    artifacts.safe_extract(archive, self.base / "unpacked")
                self.assertFalse((self.base / "unpacked").exists())
                if size:
                    metadata = {
                        "bundle": artifacts.file_info(archive),
                        "files": {
                            "build-manifest.json": {"sha256": "0" * 64, "size_bytes": 1}
                        },
                        "manifest_sha256": "0" * 64,
                    }
                    with self.assertRaisesRegex(ValueError, "extension header"):
                        transport.verify_archive(archive, metadata)

    def create_bundle(self):
        args = argparse.Namespace(
            **{
                **vars(self.args),
                "output": self.base / "native-input.tar.gz",
                "metadata": self.base / "transport.json",
            }
        )
        with patch.object(transport, "helper_identity", return_value=self.producer):
            result = transport.bundle(args)
        return args, result

    def test_bundle_and_descriptor_preserve_original_canonical_members(self):
        args, result = self.create_bundle()
        with tarfile.open(args.output, "r:gz") as archive:
            members = archive.getmembers()
        self.assertEqual(
            [item.name for item in members],
            sorted([*self.manifest["files"], "build-manifest.json"]),
        )
        self.assertTrue(
            all(item.isfile() and not item.name.startswith("./") for item in members)
        )
        extracted = self.base / "downloaded"
        artifacts.safe_extract(args.output, extracted)
        self.assertEqual(
            artifacts.verify_build(
                extracted, self.manifest_digest, self.root, self.commit, self.target
            ),
            self.manifest,
        )
        descriptor_args = argparse.Namespace(
            bundle=args.output,
            metadata=args.metadata,
            metadata_sha256=result["metadata_sha256"],
            asset_url="https://api.github.com/repos/holyglory/codex/releases/assets/123",
            output=self.base / "descriptor.json",
        )
        with patch.object(transport, "helper_identity", return_value=self.producer):
            value = transport.descriptor(descriptor_args)
        self.assertEqual(
            value,
            {
                "target": self.target,
                "url": descriptor_args.asset_url,
                **artifacts.file_info(args.output),
                "manifest_sha256": self.manifest_digest,
            },
        )
        self.assertEqual(artifacts.load_json(descriptor_args.output), value)
        with (
            patch.object(transport, "helper_identity", return_value=self.producer),
            self.assertRaisesRegex(ValueError, "Preserve"),
        ):
            transport.bundle(args)

    def test_descriptor_refuses_changed_bytes_metadata_and_unowned_asset_urls(self):
        args, result = self.create_bundle()
        original_bundle, original_metadata = (
            args.output.read_bytes(),
            args.metadata.read_bytes(),
        )
        for mutation, message in [
            ("bundle", "transport bytes changed"),
            ("metadata", "metadata changed"),
            ("url", "owned GitHub"),
        ]:
            with self.subTest(mutation=mutation):
                descriptor_args = argparse.Namespace(
                    bundle=args.output,
                    metadata=args.metadata,
                    metadata_sha256=result["metadata_sha256"],
                    asset_url="https://api.github.com/repos/holyglory/codex/releases/assets/123",
                    output=self.base / "bad-descriptor.json",
                )
                if mutation == "bundle":
                    args.output.write_bytes(original_bundle + b"changed")
                elif mutation == "metadata":
                    args.metadata.write_bytes(original_metadata + b"changed")
                else:
                    descriptor_args.asset_url = (
                        "https://api.github.com/repos/another/owner/releases/assets/123"
                    )
                try:
                    with (
                        patch.object(
                            transport, "helper_identity", return_value=self.producer
                        ),
                        self.assertRaisesRegex(ValueError, message),
                    ):
                        transport.descriptor(descriptor_args)
                    self.assertFalse(descriptor_args.output.exists())
                finally:
                    args.output.write_bytes(original_bundle)
                    args.metadata.write_bytes(original_metadata)

    def test_bundle_refuses_changed_helper_identity_before_creating_transport(self):
        args = argparse.Namespace(
            **{
                **vars(self.args),
                "output": self.base / "native-input.tar.gz",
                "metadata": self.base / "transport.json",
            }
        )
        with (
            patch.object(
                transport, "helper_identity", return_value={"helper_commit": "e" * 40}
            ),
            self.assertRaisesRegex(ValueError, "helpers differ"),
        ):
            transport.bundle(args)
        self.assertFalse(args.output.exists())
        self.assertFalse(args.metadata.exists())


if __name__ == "__main__":
    unittest.main()
