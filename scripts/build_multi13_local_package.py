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
PACKAGE_ROOT = ROOT / "codex-rs/target/multi13-local"
CARGO_TARGET = Path(
    os.environ.get(
        "CARGO_TARGET_DIR", "/mnt/build-storage/codex/multi13-release-target"
    )
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


version = source_version()
if version != "0.166.0+multi.13":
    raise RuntimeError(f"unexpected product version: {version}")
commit = git("rev-parse", "HEAD")
if re.fullmatch(r"[0-9a-f]{40}", commit) is None:
    raise RuntimeError("source checkout does not have an immutable commit")
status = git("status", "--porcelain", "--untracked-files=all")
if status:
    raise RuntimeError("source must be clean before package assembly")
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
metadata = json.loads((package / "codex-package.json").read_text())
if metadata.get("version") != version or metadata.get("target") != TARGET:
    raise RuntimeError("package metadata does not match source")
for relative in ["bin/codex", "codex-resources/bwrap", "codex-path/rg"]:
    path = package / relative
    if not path.is_file() or path.is_symlink():
        raise RuntimeError(f"missing package file: {relative}")
proof = {
    "schema": 1,
    "kind": "multi13-local-package",
    "source_commit": commit,
    "version": version,
    "target": TARGET,
    "package_sha256": package_digest(package),
    "executable_sha256": digest(package / "bin/codex"),
    "archive_sha256": digest(archive),
    "archive_size_bytes": archive.stat().st_size,
    "package_dir": str(package),
    "archive": str(archive),
    "status": "built",
    "runtime_verified": False,
}
(PACKAGE_ROOT / "package-proof.json").write_text(json.dumps(proof, indent=2) + "\n")
print(json.dumps(proof, sort_keys=True), flush=True)
