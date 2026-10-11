#!/usr/bin/env python3
"""Run critical producer/consumer regressions before expensive candidate jobs."""

from pathlib import Path
import argparse
import json
import re
import shutil
import subprocess
import tomllib

from check_release_inclusion import load_manifest
from check_release_inclusion import verify


def verify_toolchain(root: Path) -> dict[str, str]:
    """Verify the binaries selected by this runner match rust-toolchain.toml."""
    manifest = root / "codex-rs" / "rust-toolchain.toml"
    try:
        expected = tomllib.loads(manifest.read_text())["toolchain"]["channel"]
    except (KeyError, OSError, tomllib.TOMLDecodeError) as error:
        raise RuntimeError(f"Cannot read pinned Rust toolchain: {manifest}") from error
    if not re.fullmatch(r"\d+\.\d+\.\d+", expected):
        raise RuntimeError(f"Unsupported non-version Rust channel: {expected}")

    selected: dict[str, str] = {}
    for tool in ("rustc", "cargo"):
        executable = shutil.which(tool)
        if executable is None:
            raise RuntimeError(f"{tool} is not available on PATH")
        result = subprocess.run(
            [executable, "--version"],
            cwd=root / "codex-rs",
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
        version_match = re.match(rf"^{tool} (\d+\.\d+\.\d+)(?:\s|$)", result.stdout)
        if version_match is None or version_match.group(1) != expected:
            actual = (result.stdout.strip() or result.stderr.strip())[:256]
            raise RuntimeError(
                f"{tool} selected {executable} ({actual}); expected Rust {expected}"
            )
        selected[f"{tool}_path"] = executable
        selected[f"{tool}_version"] = version_match.group(1)
    print(json.dumps({"expected": expected, **selected}), flush=True)
    return selected


def run_preflight(root: Path) -> None:
    verify_toolchain(root)
    manifest = load_manifest(root)
    verify(root, manifest)
    subprocess.run(
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
        cwd=root / "codex-rs",
        check=True,
    )
    # Keep the package selection identical between filters: changing Cargo's
    # feature union can rebuild shared dependencies even with a warm target dir.
    command = [
        "just",
        "test",
        "--locked",
        "-p",
        "codex-app-server",
        "-p",
        "codex-tui",
        "-p",
        "codex-core",
        "--no-tests=fail",
    ]
    for package in sorted(
        {
            package
            for capability in manifest["capabilities"]
            for package in capability["packages"]
        }
        - {"codex-app-server", "codex-tui", "codex-core"}
    ):
        command.extend(["-p", package])
    # Separate invocations intentionally require each family to select tests.
    # A combined OR filter can silently omit one family after a module rename.
    for family in (
        "suite::v2::local_usage::",
        "app::tests::turn_submission::",
        "unified_exec::async_watcher::tests::streaming_output_preserves_summary_when_delta_consumer_lags",
        "suite::unified_exec::unified_exec_formats_large_output_summary",
        "suite::v2::account_auto_selection::",
        "suite::models_cache_ttl::",
        "suite::search_tool::",
    ):
        subprocess.run(
            [*command, "-E", f"test({family})"],
            cwd=root / "codex-rs",
            check=True,
        )
    for capability in manifest["capabilities"]:
        for selection in capability["filters"]:
            print(
                json.dumps({"capability": capability["id"], "selection": selection}),
                flush=True,
            )
            subprocess.run(
                [*command, "-E", selection], cwd=root / "codex-rs", check=True
            )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--toolchain-only", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    try:
        if args.toolchain_only:
            verify_toolchain(root)
        else:
            run_preflight(root)
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Candidate preflight refused: {error}\n")
