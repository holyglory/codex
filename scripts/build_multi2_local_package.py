"""Build one exact local Linux package for the current immutable release."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
PACKAGE_ROOT = ROOT / "codex-rs/target/multi2-local"
CARGO_TARGET = Path(
    os.environ.get("CARGO_TARGET_DIR", "/mnt/build-storage/codex/multi2-release-target")
)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def package_digest(root: Path) -> str:
    entries = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise RuntimeError("package contains symlink")
        if path.is_file():
            entries.append(
                [
                    path.relative_to(root).as_posix(),
                    path.stat().st_mode & 0o777,
                    digest(path),
                ]
            )
    return hashlib.sha256(
        json.dumps(entries, separators=(",", ":")).encode()
    ).hexdigest()


def source_version() -> str:
    return tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"][
        "package"
    ]["version"]


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True).strip()


def verify_v8_pair(environment: dict[str, str]) -> dict:
    handoff = Path(environment["V8_HANDOFF_DIR"])
    receipt_path = handoff / "v8-pair.json"
    encoded = receipt_path.read_bytes()
    receipt = json.loads(encoded)
    # The canonical verifier checks both this producer's identity and whether
    # its locked V8 inputs still match the current package source.
    if not environment.get("V8_PRODUCER_COMMIT"):
        environment["V8_PRODUCER_COMMIT"] = receipt["source_commit"]
    subprocess.run(
        [
            "bash",
            str(ROOT / ".github/scripts/downstream-unix-native.sh"),
            "verify-pair",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
        timeout=120,
    )
    if receipt_path.read_bytes() != encoded:
        raise RuntimeError("V8 pair receipt changed during verification")
    return {
        "handoff_dir": str(handoff),
        "receipt_sha256": hashlib.sha256(encoded).hexdigest(),
        "receipt": receipt,
    }


def prepare_v8(environment: dict[str, str], version: str) -> dict:
    handoff = Path(environment.get("V8_HANDOFF_DIR", PACKAGE_ROOT / "v8-handoff"))
    if not handoff.is_absolute():
        raise RuntimeError("V8_HANDOFF_DIR must be an absolute release handoff path")
    handoff = handoff.resolve()
    environment.update(
        {"TARGET": TARGET, "SOURCE_VERSION": version, "V8_HANDOFF_DIR": str(handoff)}
    )
    environment["BAZEL_PLATFORM"] = "linux_x86_64"
    environment.setdefault("BAZEL_CONFIG", "ci")
    reused = handoff.exists()
    if not reused:
        subprocess.run(
            [
                "bash",
                str(ROOT / ".github/scripts/downstream-unix-native.sh"),
                "source-v8",
            ],
            cwd=ROOT,
            env={
                key: value
                for key, value in environment.items()
                if key != "BUILDBUDDY_API_KEY"
            },
            check=True,
            timeout=7200,
        )
    proof = verify_v8_pair(environment)
    pair = handoff / "pair"
    environment["RUSTY_V8_ARCHIVE"] = str(
        pair / f"librusty_v8_ptrcomp_sandbox_release_{TARGET}.a.gz"
    )
    environment["RUSTY_V8_SRC_BINDING_PATH"] = str(
        pair / f"src_binding_ptrcomp_sandbox_release_{TARGET}.rs"
    )
    # Package Cargo consumes this verified pair rather than starting a separate
    # GN build from an inherited source-build override.
    environment.pop("V8_FROM_SOURCE", None)
    return {**proof, "reused": reused}


def main() -> None:
    version = source_version()
    if version != "0.170.0+multi.2":
        raise RuntimeError(f"unexpected product version: {version}")
    commit = git("rev-parse", "HEAD")
    if re.fullmatch(r"[0-9a-f]{40}", commit) is None:
        raise RuntimeError("source checkout does not have an immutable commit")
    if git(
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--",
        ".",
        ":(top,exclude).devcoordinator.toml",
        ":(top,exclude).devcoordinator",
    ):
        raise RuntimeError("source must be clean except for governed coordinator files")
    if PACKAGE_ROOT.exists():
        raise RuntimeError("preserve existing output; choose a fresh release source")

    package = PACKAGE_ROOT / "package"
    archive = PACKAGE_ROOT / f"codex-package-{TARGET}.tar.gz"
    PACKAGE_ROOT.mkdir(parents=True)
    environment = {
        **os.environ,
        "CARGO_TARGET_DIR": str(CARGO_TARGET),
        "CODEX_REPO_ROOT": str(ROOT),
        "INSTA_UPDATE": "no",
        "RUSTUP_TOOLCHAIN": tomllib.loads(
            (ROOT / "codex-rs/rust-toolchain.toml").read_text()
        )["toolchain"]["channel"],
    }
    v8_pair = prepare_v8(environment, version)
    subprocess.run(
        [
            "just",
            "assemble-codex-package",
            "--target",
            TARGET,
            "--variant",
            "codex",
            "--package-version",
            version,
            "--cargo-profile",
            "release",
            "--strip",
            "auto",
            "--package-dir",
            str(package),
            "--archive-output",
            str(archive),
        ],
        cwd=ROOT,
        env=environment,
        check=True,
        timeout=7200,
    )
    if verify_v8_pair(environment)["receipt_sha256"] != v8_pair["receipt_sha256"]:
        raise RuntimeError("V8 pair receipt changed during the package build")
    metadata = json.loads((package / "codex-package.json").read_text())
    if metadata.get("version") != version or metadata.get("target") != TARGET:
        raise RuntimeError("package metadata does not match source")
    for relative in ["bin/codex", "codex-resources/bwrap", "codex-path/rg"]:
        path = package / relative
        if not path.is_file() or path.is_symlink():
            raise RuntimeError(f"missing package file: {relative}")
    proof = {
        "schema": 1,
        "kind": "multi2-local-package",
        "source_commit": commit,
        "version": version,
        "target": TARGET,
        "package_sha256": package_digest(package),
        "executable_sha256": digest(package / "bin/codex"),
        "archive_sha256": digest(archive),
        "archive_size_bytes": archive.stat().st_size,
        "package_dir": str(package),
        "archive": str(archive),
        "v8_pair": v8_pair,
        "status": "built",
        "runtime_verified": False,
    }
    (PACKAGE_ROOT / "package-proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps(proof, sort_keys=True), flush=True)


if __name__ == "__main__":
    main()
