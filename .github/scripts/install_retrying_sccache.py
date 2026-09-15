"""Build the pinned cache with temporary-storage retries, preserving cache keys."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


SOURCE_COMMIT = "b799af2eea02bba9e0ef2550775fe10296b62981"
VERSION = "0.16.0"


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def cached_binary(root: Path, identity: dict) -> Path | None:
    binary = root / ("sccache.exe" if os.name == "nt" else "sccache")
    try:
        receipt = json.loads((root / "receipt.json").read_text())
        if receipt == {**identity, "binary_sha256": digest(binary)}:
            return binary
    except (OSError, ValueError):
        pass
    return None


def host_build_environment(
    inherited: dict[str, str], target_dir: Path
) -> dict[str, str]:
    compiler_variables = (
        "CC",
        "CXX",
        "AR",
        "RANLIB",
        "CFLAGS",
        "CXXFLAGS",
        "CPPFLAGS",
        "LDFLAGS",
        "PKG_CONFIG",
        "CMAKE",
        "BORING_BSSL_SYSROOT",
        "OPENSSL",
    )
    environment = {
        key: value
        for key, value in inherited.items()
        if key
        not in (
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "TARGET",
            "HOST",
        )
        and not key.startswith("CARGO_TARGET_")
        and not any(
            key == prefix or key.startswith(prefix + "_")
            for base in compiler_variables
            for prefix in (base, "TARGET_" + base, "HOST_" + base)
        )
    }
    environment.update(
        CARGO_TARGET_DIR=str(target_dir),
        CARGO_INCREMENTAL="0",
        CARGO_PROFILE_RELEASE_DEBUG="0",
        CARGO_PROFILE_RELEASE_LTO="false",
        CARGO_PROFILE_RELEASE_STRIP="symbols",
        CARGO_PROFILE_RELEASE_CODEGEN_UNITS="16",
    )
    return environment


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--install-dir", type=Path, required=True)
    parser.add_argument("--build-root", type=Path, required=True)
    args = parser.parse_args()
    patch = (
        Path(__file__).resolve().parents[1] / "patches/sccache-0.16.0-gha-retry.patch"
    )
    rustc = subprocess.check_output(["rustc", "-vV"], text=True)
    host = next(
        line.removeprefix("host: ")
        for line in rustc.splitlines()
        if line.startswith("host: ")
    )
    identity = {
        "source_commit": SOURCE_COMMIT,
        "version": VERSION,
        "patch_sha256": digest(patch),
        "host": host,
    }
    binary = cached_binary(args.install_dir, identity)
    if binary is None:
        args.build_root.mkdir(parents=True, exist_ok=False)
        source = args.build_root / "source"
        subprocess.run(["git", "init", "--quiet", str(source)], check=True)
        subprocess.run(
            [
                "git",
                "fetch",
                "--quiet",
                "--depth=1",
                "https://github.com/mozilla/sccache.git",
                SOURCE_COMMIT,
            ],
            cwd=source,
            check=True,
        )
        subprocess.run(
            ["git", "checkout", "--quiet", "--detach", "FETCH_HEAD"],
            cwd=source,
            check=True,
        )
        subprocess.run(["git", "apply", str(patch)], cwd=source, check=True)
        environment = host_build_environment(
            dict(os.environ), args.build_root / "target"
        )
        subprocess.run(
            [
                "cargo",
                "build",
                "--locked",
                "--release",
                "--no-default-features",
                "--features",
                "gha",
                "--bin",
                "sccache",
                "--target",
                host,
            ],
            cwd=source,
            env=environment,
            check=True,
        )
        built = (
            args.build_root
            / "target"
            / host
            / "release"
            / ("sccache.exe" if os.name == "nt" else "sccache")
        )
        if (
            subprocess.check_output([str(built), "--version"], text=True).strip()
            != f"sccache {VERSION}"
        ):
            raise RuntimeError("Unexpected compiler-cache build version")
        args.install_dir.mkdir(parents=True, exist_ok=True)
        binary = args.install_dir / built.name
        staging = args.install_dir / (built.name + ".new")
        shutil.copy2(built, staging)
        staging.replace(binary)
        (args.install_dir / "receipt.json").write_text(
            json.dumps({**identity, "binary_sha256": digest(binary)}) + "\n"
        )
        # Retire the large generated output. Keep the small pinned source for
        # runner diagnostics; Git pack files can be read-only on Windows.
        shutil.rmtree(args.build_root / "target")
    print(f"Using retrying sccache {VERSION} from source {SOURCE_COMMIT}")
    for variable, value in (
        ("GITHUB_PATH", str(args.install_dir.resolve())),
        ("GITHUB_ENV", f"SCCACHE_PATH={binary.resolve()}"),
    ):
        if path := os.environ.get(variable):
            with open(path, "a") as output:
                output.write(value + "\n")


if __name__ == "__main__":
    main()
