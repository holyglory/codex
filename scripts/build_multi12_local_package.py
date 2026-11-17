"""Build one exact .12 Linux package for the governed local deployment."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
VERSION = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())["workspace"]["package"]["version"]
OUTPUT = ROOT / "codex-rs/target/multi12-local"
CARGO_TARGET = Path(os.environ.get("CARGO_TARGET_DIR", "/mnt/build-storage/codex/multi12-release-target"))


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def package_digest(root: Path) -> str:
    rows = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise RuntimeError("package contains symlink")
        if path.is_file():
            rows.append([path.relative_to(root).as_posix(), path.stat().st_mode & 0o777, digest(path)])
    return hashlib.sha256(json.dumps(rows, separators=(",", ":")).encode()).hexdigest()


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True).strip()


COMMIT = git("rev-parse", "HEAD")
if not re.fullmatch(r"[0-9a-f]{40}", COMMIT):
    raise RuntimeError("source checkout does not have an immutable commit")
status = subprocess.check_output(["git", "-C", str(ROOT), "status", "--porcelain", "--untracked-files=all"], text=True)
if any(line and line[3:] != ".devcoordinator.toml" for line in status.splitlines()):
    raise RuntimeError("source must be clean except for the governed declaration")
if VERSION != "0.166.0+multi.12":
    raise RuntimeError(f"unexpected product version: {VERSION}")
if OUTPUT.exists():
    raise RuntimeError("preserve existing .12 package output; choose a fresh target")
OUTPUT.mkdir(parents=True)
package_dir = OUTPUT / "package"
archive = OUTPUT / f"codex-package-{TARGET}.tar.gz"
environment = {**os.environ, "CARGO_TARGET_DIR": str(CARGO_TARGET), "CODEX_REPO_ROOT": str(ROOT), "INSTA_UPDATE": "no"}
command = [
    "just", "assemble-codex-package", "--target", TARGET, "--variant", "codex",
    "--package-version", VERSION, "--cargo-profile", "release", "--strip", "auto",
    "--package-dir", str(package_dir), "--archive-output", str(archive),
]
subprocess.run(command, cwd=ROOT, env=environment, check=True, timeout=7200)
metadata = json.loads((package_dir / "codex-package.json").read_text())
if metadata.get("version") != VERSION or metadata.get("target") != TARGET:
    raise RuntimeError("package metadata does not match .12")
for relative in ["bin/codex", "codex-resources/bwrap", "codex-path/rg"]:
    path = package_dir / relative
    if not path.is_file() or path.is_symlink():
        raise RuntimeError(f"missing package file: {relative}")
proof = {
    "schema": 1, "kind": "multi12-local-package", "source_commit": COMMIT,
    "version": VERSION, "target": TARGET, "package_sha256": package_digest(package_dir),
    "executable_sha256": digest(package_dir / "bin/codex"),
    "archive_sha256": digest(archive), "archive_size_bytes": archive.stat().st_size,
    "package_dir": str(package_dir), "archive": str(archive), "status": "built",
    "runtime_verified": False,
}
(OUTPUT / "package-proof.json").write_text(json.dumps(proof, indent=2) + "\n")
print(json.dumps(proof, sort_keys=True), flush=True)
