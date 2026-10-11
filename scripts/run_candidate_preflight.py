#!/usr/bin/env python3
"""Run critical producer/consumer regressions before expensive candidate jobs."""

from pathlib import Path
import argparse
import re
import shutil
import subprocess
import tomllib


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
        )
        version_match = re.search(r"\b\d+\.\d+\.\d+\b", result.stdout)
        if version_match is None or version_match.group() != expected:
            actual = result.stdout.strip() or result.stderr.strip()
            raise RuntimeError(
                f"{tool} selected {executable} ({actual}); expected Rust {expected}"
            )
        selected[f"{tool}_path"] = executable
        selected[f"{tool}_version"] = version_match.group()
    print(
        "toolchain preflight: "
        + " ".join(f"{key}={value}" for key, value in selected.items()),
        flush=True,
    )
    return selected


def run_preflight(root: Path, *, check_toolchain: bool = True) -> None:
    if check_toolchain:
        verify_toolchain(root)
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
    # Separate invocations intentionally require each family to select tests.
    # A combined OR filter can silently omit one family after a module rename.
    for family in (
        "suite::v2::thread_reconnect::",
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
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Candidate preflight refused: {error}\n")
