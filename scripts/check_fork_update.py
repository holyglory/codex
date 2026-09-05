#!/usr/bin/env python3
"""Exercise a built CLI updater without invoking any real package manager."""

import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-binary", required=True, type=Path)
    binary = parser.parse_args().codex_binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="codex-fork-update-") as scratch:
        root = Path(scratch)
        codex_home = root / "home"
        shims = root / "bin"
        codex_home.mkdir()
        shims.mkdir()
        if os.name == "nt":
            shim = shims / "npm.cmd"
            shim.write_text(
                '@echo off\n(echo %1& echo %2& echo %3)>"%CODEX_UPDATE_ARGS%"\n'
                "exit /b %CODEX_UPDATE_EXIT%\n"
            )
        else:
            shim = shims / "npm"
            shim.write_text(
                '#!/bin/sh\nprintf \'%s\\n\' "$@" > "$CODEX_UPDATE_ARGS"\n'
                'exit "$CODEX_UPDATE_EXIT"\n'
            )
            shim.chmod(0o700)
        environment = os.environ.copy()
        for manager in ("VITE_PLUS", "PNPM", "BUN"):
            environment.pop(f"CODEX_MANAGED_BY_{manager}", None)
        environment.update(
            CODEX_HOME=str(codex_home),
            CODEX_MANAGED_BY_NPM="1",
            PATH=str(shims),
        )
        for installer_exit in (0, 42):
            receipt = root / f"arguments-{installer_exit}"
            result = subprocess.run(
                [str(binary), "update"],
                cwd=root,
                env={
                    **environment,
                    "CODEX_UPDATE_ARGS": str(receipt),
                    "CODEX_UPDATE_EXIT": str(installer_exit),
                },
                capture_output=True,
                text=True,
                encoding="utf-8",
                timeout=30,
                check=False,
            )
            arguments = receipt.read_text().splitlines() if receipt.exists() else []
            if arguments != ["install", "-g", "@holyglory/codex@latest"]:
                raise SystemExit("Updater did not invoke the exact fork npm command")
            if installer_exit == 0:
                if (
                    result.returncode != 0
                    or "Update ran successfully" not in result.stdout
                ):
                    raise SystemExit(
                        "Updater did not report successful fork installation"
                    )
            elif result.returncode == 0 or "failed with status" not in result.stderr:
                raise SystemExit("Updater did not report the failed npm installation")
        print("Fork npm update target and installer failure reporting verified")


if __name__ == "__main__":
    main()
