#!/usr/bin/env python3
"""Build the pinned GNU/Linux voice libraries for complete Cargo validation."""

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys


def cargo_environment(prefix: Path, pkg_config: Path) -> dict[str, str]:
    wrapper = prefix.parent / "cargo-pkg-config"
    metadata = os.pathsep.join(
        str(prefix / path) for path in ("lib/pkgconfig", "share/pkgconfig")
    )
    wrapper.write_text(
        "#!/bin/sh\n"
        "for argument do\n"
        '  case "$argument" in\n'
        "    gstreamer-*|glib-2.0|gobject-2.0|gio-2.0|gmodule-2.0|gthread-2.0)\n"
        f"      export PKG_CONFIG_LIBDIR={shlex.quote(metadata)} PKG_CONFIG_PATH=\n"
        f'      exec {shlex.quote(str(pkg_config))} --define-prefix "$@" ;;\n'
        "  esac\n"
        "done\n"
        f'exec {shlex.quote(str(pkg_config))} "$@"\n'
    )
    wrapper.chmod(0o755)
    libraries = [str(prefix / "lib")]
    if inherited := os.environ.get("LD_LIBRARY_PATH"):
        libraries.append(inherited)
    return {
        "PKG_CONFIG": str(wrapper),
        "LD_LIBRARY_PATH": os.pathsep.join(libraries),
        "CODEX_TEST_VOICE_RUNTIME": str(prefix),
    }


def prepare(root: Path, output: Path, archives: Path | None) -> dict[str, str]:
    output.mkdir(parents=True, exist_ok=False)
    archives = archives or output / "archives"
    archives.mkdir(parents=True, exist_ok=True)
    sys.path.insert(0, str(root / "third_party/voice"))
    from prepare_sources import MAX_ARCHIVE_BYTES, load_sources

    for source in load_sources((root / "third_party/voice/sources.json").read_bytes()):
        archive = archives / source.archive
        if not archive.exists():
            subprocess.run(
                [
                    "curl",
                    "--fail",
                    "--location",
                    "--retry",
                    "2",
                    "--connect-timeout",
                    "15",
                    "--max-time",
                    "300",
                    "--max-filesize",
                    str(MAX_ARCHIVE_BYTES),
                    source.url,
                    "--output",
                    str(archive),
                ],
                check=True,
            )
    # The existing recipe verifies every source digest and extraction bound before building.
    command = [
        sys.executable,
        str(root / "third_party/voice/build_native.py"),
        "--archives",
        str(archives),
        "--output",
        str(output / "native"),
        "--target",
        "x86_64-unknown-linux-gnu",
    ]
    for argument, tool in (
        ("cc", "gcc"),
        ("cxx", "g++"),
        ("cmake", "cmake"),
        ("make", "make"),
        ("pkg-config", "pkg-config"),
        ("shell", "bash"),
    ):
        if (executable := shutil.which(tool)) is None:
            raise ValueError(f"Missing native build prerequisite: {tool}")
        command.extend([f"--{argument}", executable])
    subprocess.run(command, check=True)
    return cargo_environment(output / "native/prefix", Path(shutil.which("pkg-config")))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--archives", type=Path)
    parser.add_argument("--environment-file", required=True, type=Path)
    args = parser.parse_args()
    environment = prepare(
        Path(__file__).resolve().parents[2], args.output.resolve(), args.archives
    )
    with args.environment_file.open("a") as stream:
        for key, value in environment.items():
            stream.write(f"{key}={value}\n")


if __name__ == "__main__":
    main()
