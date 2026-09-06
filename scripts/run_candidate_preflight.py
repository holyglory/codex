#!/usr/bin/env python3
"""Run critical producer/consumer regressions before expensive candidate jobs."""

from pathlib import Path
import subprocess


def run_preflight(root: Path) -> None:
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
        "--no-tests=fail",
    ]
    # Separate invocations intentionally require each family to select tests.
    # A combined OR filter can silently omit one family after a module rename.
    for family in (
        "suite::v2::thread_reconnect::",
        "app::tests::turn_submission::",
    ):
        subprocess.run(
            [*command, "-E", f"test({family})"],
            cwd=root / "codex-rs",
            check=True,
        )


if __name__ == "__main__":
    run_preflight(Path(__file__).resolve().parents[1])
