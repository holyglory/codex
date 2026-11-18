"""Local-only compilation for a source-bound Linux release and test archive."""

import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

import local_native_release as driver
from local_native_artifacts import inventory, rust_version


def compile_target(args):
    from prepare_local_musl import prepare

    root, output = (args.source_root.resolve(), args.output.resolve())
    source = driver.source_identity(root, args.commit)
    driver.require(
        args.target in driver.TARGETS and args.cargo_target_dir.is_absolute(),
        "Use a supported target and absolute persistent Cargo cache",
    )
    host = driver.native_host("x86_64-unknown-linux-musl")
    import resource

    soft_limit, hard_limit = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft_limit != resource.RLIM_INFINITY and soft_limit < 16384:
        driver.require(
            hard_limit == resource.RLIM_INFINITY or hard_limit >= 16384,
            "Musl linking requires an open-file hard limit of at least 16384",
        )
        resource.setrlimit(resource.RLIMIT_NOFILE, (16384, hard_limit))
    driver.require(
        not output.exists(),
        "Preserve previous compile output; select an unused directory",
    )
    output.mkdir(parents=True)
    environment = {
        **os.environ,
        "RUSTUP_TOOLCHAIN": rust_version(root),
        "CARGO_TARGET_DIR": str(args.cargo_target_dir),
        "INSTA_UPDATE": "no",
        "CODEX_REPO_ROOT": str(root),
    }
    driver.require(
        subprocess.check_output(
            ["rustc", "--version"], env=environment, text=True
        ).split()[1]
        == environment["RUSTUP_TOOLCHAIN"],
        "Source Rust toolchain is unavailable",
    )
    driver.require(
        subprocess.check_output(
            ["cargo", "--version"], env=environment, text=True
        ).split()[1]
        == environment["RUSTUP_TOOLCHAIN"],
        "Cargo version differs from the source Rust toolchain",
    )
    linker_flags = args.target.upper().replace("-", "_") + "_LDFLAGS"
    environment[linker_flags] = (
        environment.get(linker_flags, "") + " -Wno-unused-command-line-argument"
    ).strip()
    environment.update(
        prepare(root, args.target, args.musl_cache.resolve(), args.zig.resolve())
    )
    environment.update(
        driver.stage_v8(
            root, args.v8_handoff.resolve(), output, args.target, args.commit
        )
    )
    runner = driver.Commands(root / "codex-rs", output, environment)
    runner.run(
        "build-bwrap",
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--target",
            args.target,
            "--bin",
            "bwrap",
        ],
    )
    release = args.cargo_target_dir / args.target / "release"
    # The upstream helper requires CODEX_REPO_ROOT at import time. Execute it
    # with the product checkout's package metadata and an isolated environment.
    runner.run(
        "strip-bwrap",
        [
            sys.executable,
            "-c",
            "import os,sys; from pathlib import Path; "
            "sys.path.insert(0, os.path.join(os.environ['CODEX_REPO_ROOT'], 'scripts')); "
            "from codex_package.symbols import strip_binary; "
            "from codex_package.targets import TARGET_SPECS; "
            "strip_binary(Path(sys.argv[1]), TARGET_SPECS[sys.argv[2]], tool=sys.argv[3] or None)",
            str(release / "bwrap"),
            args.target,
            args.strip_tool or "",
        ],
    )
    bwrap = driver.file_info(release / "bwrap")
    environment["CODEX_BWRAP_SHA256"] = bwrap["sha256"]
    runner.run(
        "build-release",
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--target",
            args.target,
            *(value for binary in driver.BINARIES for value in ("--bin", binary)),
        ],
    )
    runner.run(
        "archive-tests",
        [
            "cargo",
            "nextest",
            "archive",
            "--locked",
            "--target",
            args.target,
            *(
                value
                for package in ["codex-v8-poc", *driver.REGRESSIONS]
                for value in ("-p", package)
            ),
            "--archive-file",
            str(output / f"tests-{args.target}.tar.zst"),
        ],
    )
    with tempfile.TemporaryDirectory(
        prefix="local-native-package-", dir=output.parent
    ) as temporary:
        package = Path(temporary) / "package"
        command = [
            "just",
            "assemble-codex-package",
            "--target",
            args.target,
            "--package-version",
            source["version"],
            "--cargo-profile",
            "release",
            "--strip",
            "all",
            "--package-dir",
            str(package),
            "--entrypoint-bin",
            str(release / "codex"),
            "--code-mode-host-bin",
            str(release / "codex-code-mode-host"),
            "--exec-server-bin",
            str(release / "codex-exec-server"),
            "--bwrap-bin",
            str(release / "bwrap"),
            "--archive-output",
            str(output / f"codex-package-{args.target}.tar.gz"),
        ]
        if args.strip_tool:
            command.extend(["--strip-tool", args.strip_tool])
        runner.run("assemble-package", command)
        driver.require(
            driver.file_info(package / "codex-resources/bwrap") == bwrap,
            "Package changed the final bwrap integrity hash",
        )
        for relative in [
            *(f"bin/{name}" for name in driver.BINARIES),
            "codex-resources/bwrap",
        ]:
            driver.elf_identity(package / relative, args.target)
        driver.elf_identity(
            package / "codex-path/rg",
            args.target,
            policy=driver.ElfPolicy.SOURCE_PINNED_UTILITY,
        )
    config = (root / "codex-rs/.config/nextest.toml").read_text()
    recipe = (root / ".github/scripts/downstream-unix-native.sh").read_text()
    overrides = re.findall(
        "cat >> \\\"\\$nextest_config\\\" <<'EOF'\\n(.*?)^EOF$",
        recipe,
        re.MULTILINE | re.DOTALL,
    )
    driver.require(
        len(overrides) == 1, "Source native timeout overrides are missing or ambiguous"
    )
    (output / "nextest-config.toml").write_text(config + overrides[0])
    driver.require(
        driver.source_identity(root, args.commit) == source,
        "Source changed during compilation",
    )
    manifest = {
        "schema": 1,
        "kind": "codex-prebuilt-target",
        **source,
        "target": args.target,
        "rust_version": environment["RUSTUP_TOOLCHAIN"],
        "nextest_version": driver.nextest_version(environment),
        "status": "compiled",
        "runtime_executed": False,
        "producer": {**driver.helper_identity(), "build_host": host},
        "bwrap": bwrap,
        "checks": runner.checks,
        "files": inventory(output),
    }
    driver.write_json(output / "build-manifest.json", manifest)
    return driver.file_info(output / "build-manifest.json")
