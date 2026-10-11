#!/usr/bin/env python3
"""Prepare the pinned Linux voice inputs used by complete Cargo validation."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
VOICE_MODULE_PREFIXES = (
    "glib-",
    "gobject-",
    "gio-",
    "gmodule-",
    "gthread-",
    "gstreamer-",
    "libffi",
    "libpcre2-",
    "zlib",
)


def commit():
    return subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True
    ).strip()


def pkg_config_command(arguments, environment):
    environment = dict(environment)
    executable = environment["CODEX_CARGO_SYSTEM_PKG_CONFIG"]
    if any(argument.startswith(VOICE_MODULE_PREFIXES) for argument in arguments):
        sdk = Path(environment["CODEX_CARGO_VOICE_SDK"])
        environment["PKG_CONFIG_LIBDIR"] = str(sdk / "lib/pkgconfig")
        environment["PKG_CONFIG_PATH"] = ""
        arguments = ["--define-prefix", *arguments]
    elif "alsa" in arguments and (prefix := environment.get("CODEX_CARGO_ALSA_PREFIX")):
        environment["PKG_CONFIG_LIBDIR"] = str(
            Path(prefix) / "lib/x86_64-linux-gnu/pkgconfig"
        )
        environment["PKG_CONFIG_PATH"] = ""
        arguments = [
            f"--define-variable=prefix={prefix}",
            f"--define-variable=libdir={prefix}/lib/x86_64-linux-gnu",
            f"--define-variable=includedir={prefix}/include",
            *arguments,
        ]
    return [executable, *arguments], environment


def alsa_inputs(state, pkg_config):
    if subprocess.run([pkg_config, "--exists", "alsa"]).returncode == 0:
        return {}
    # Stage matching development inputs without changing the host installation.
    version = subprocess.check_output(
        ["dpkg-query", "-W", "-f=${Version}", "libasound2t64:amd64"], text=True
    ).strip()
    root = state / ("alsa-" + hashlib.sha256(version.encode()).hexdigest()[:12])
    prefix = root / "usr"
    if not (prefix / "lib/x86_64-linux-gnu/pkgconfig/alsa.pc").is_file():
        with tempfile.TemporaryDirectory(
            prefix="alsa-download-", dir=state
        ) as directory:
            subprocess.run(
                ["apt-get", "download", f"libasound2-dev:amd64={version}"],
                cwd=directory,
                check=True,
            )
            archives = list(Path(directory).glob("*.deb"))
            if len(archives) != 1:
                raise ValueError("Expected one matching ALSA development package")
            subprocess.run(
                ["dpkg-deb", "--extract", str(archives[0]), str(root)], check=True
            )
    library_dir = prefix / "lib/x86_64-linux-gnu"
    shutil.copy2(
        Path("/usr/lib/x86_64-linux-gnu/libasound.so.2").resolve(strict=True),
        library_dir / "libasound.so.2",
    )
    alias = library_dir / "libasound.so"
    alias.unlink(missing_ok=True)
    alias.symlink_to("libasound.so.2")
    environment = {
        "CODEX_CARGO_SYSTEM_PKG_CONFIG": pkg_config,
        "CODEX_CARGO_ALSA_PREFIX": str(prefix),
    }
    command, probe_environment = pkg_config_command(
        ["--exists", "alsa"], {**os.environ, **environment}
    )
    subprocess.run(command, env=probe_environment, check=True)
    return {"CODEX_CARGO_ALSA_PREFIX": str(prefix)}


def prepare(state):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("This acceptance environment requires Linux x86_64")
    system_pkg_config = shutil.which("pkg-config")
    if not system_pkg_config:
        raise ValueError("pkg-config is required for native Cargo inputs")
    state.mkdir(parents=True, exist_ok=True)
    platform_environment = alsa_inputs(state, system_pkg_config)
    command = ["bash", ".github/scripts/run-bazel-ci.sh", "--", "build"]
    if cache := os.environ.get("LOCAL_BAZEL_CACHE"):
        command.append(f"--disk_cache={cache}")
    subprocess.run(
        [
            *command,
            "--",
            "//third_party/voice:native_sdk",
            "//third_party/voice:native_link",
        ],
        cwd=ROOT,
        check=True,
    )
    output = ROOT / "bazel-bin/third_party/voice"
    sdk = (output / "native_runtime_linux_x86_64_sdk").resolve(strict=True)
    runtime = (output / "native_link_linux_x86_64").resolve(strict=True)
    identity = json.loads((sdk / "sdk.json").read_text())
    source_commit = commit()
    source_hash = hashlib.sha256(
        (ROOT / "third_party/voice/sources.json").read_bytes()
    ).hexdigest()
    if (
        identity["sourceCommit"],
        identity["target"],
        identity["sourceManifestSha256"],
    ) != (
        source_commit,
        "x86_64-unknown-linux-gnu",
        source_hash,
    ):
        raise ValueError("Native SDK does not match the candidate's pinned sources")
    environment = {
        **platform_environment,
        "CODEX_CARGO_SYSTEM_PKG_CONFIG": system_pkg_config,
        "CODEX_CARGO_VOICE_SDK": str(sdk),
        "CODEX_TEST_VOICE_RUNTIME": str(runtime),
        "PKG_CONFIG": str(ROOT / "scripts/cargo_native_pkg_config.py"),
        "STABLE_GIT_COMMIT": source_commit,
        "LD_LIBRARY_PATH": os.pathsep.join(
            filter(None, [str(runtime / "lib"), os.environ.get("LD_LIBRARY_PATH")])
        ),
    }
    # Match MODULE.bazel: probe SDK versions and headers, then link the exact
    # libraries used by the runtime and its plugins. No host fallback for voice.
    for key in (
        "GLIB_2_0",
        "GOBJECT_2_0",
        "GIO_2_0",
        "GSTREAMER_1_0",
        "GSTREAMER_BASE_1_0",
        "GSTREAMER_APP_1_0",
        "GSTREAMER_AUDIO_1_0",
    ):
        environment[f"SYSTEM_DEPS_{key}_SEARCH_NATIVE"] = str(runtime / "lib")
        if key.startswith("GSTREAMER_"):
            environment[f"SYSTEM_DEPS_{key}_LDFLAGS"] = ""
    for module in ("gstreamer-1.0", "gstreamer-app-1.0", "gstreamer-audio-1.0"):
        arguments, probe_environment = pkg_config_command(
            ["--atleast-version=1.28", module], {**os.environ, **environment}
        )
        subprocess.run(arguments, env=probe_environment, check=True)
    state.mkdir(parents=True, exist_ok=True)
    (state / "cargo-native-env.json").write_text(
        json.dumps(environment, indent=2) + "\n"
    )


def run(state, arguments):
    environment = json.loads((state / "cargo-native-env.json").read_text())
    if environment["STABLE_GIT_COMMIT"] != commit():
        raise ValueError("Native Cargo inputs belong to another candidate")
    if arguments[:1] == ["--"]:
        arguments = arguments[1:]
    if not arguments:
        raise ValueError("A Cargo validation command is required")
    os.execvpe(arguments[0], arguments, {**os.environ, **environment})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    setup = commands.add_parser("prepare")
    setup.add_argument("--state", type=Path, required=True)
    execute = commands.add_parser("run")
    execute.add_argument("--state", type=Path, required=True)
    execute.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.action == "prepare":
        prepare(args.state)
    else:
        run(args.state, args.command)


if __name__ == "__main__":
    main()
