"""Run Bazel with periodic eviction of old entries in its dedicated CI cache."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import time


def trim_cache(
    root: Path, limit: int, reserve_bytes: int = 4 * 1024**3
) -> dict[str, int]:
    if limit <= 0 or root.is_symlink():
        raise ValueError("Invalid cache root or size limit")
    entries = []
    # Bazel 9 stores SHA-256 entries at {ac,cas}/<two hex>/<64 hex>.
    # Never touch tmp (in-flight writes), symlinks, or unknown cache formats.
    for store in (root / "ac", root / "cas"):
        if store.is_symlink() or not store.is_dir():
            continue
        for shard in store.iterdir():
            if (
                shard.is_symlink()
                or not shard.is_dir()
                or not re.fullmatch(r"[0-9a-f]{2}", shard.name)
            ):
                continue
            for entry in shard.iterdir():
                if not re.fullmatch(
                    r"[0-9a-f]{64}", entry.name
                ) or not entry.name.startswith(shard.name):
                    continue
                try:
                    metadata = entry.lstat()
                except FileNotFoundError:
                    continue
                if stat.S_ISREG(metadata.st_mode):
                    entries.append((metadata.st_mtime_ns, metadata.st_size, entry))
    retained = sum(size for _, size, _ in entries)
    # Compilation has priority over the optimization. Evict cache entries if
    # necessary to keep four GiB available for the active build's next outputs.
    target = min(limit, max(0, retained + shutil.disk_usage(root).free - reserve_bytes))
    removed = 0
    for modified, size, entry in sorted(entries):
        if retained <= target:
            break
        try:
            current = entry.lstat()
            # A reader may have refreshed the entry since discovery. Keep it.
            if not stat.S_ISREG(current.st_mode) or current.st_mtime_ns != modified:
                continue
            entry.unlink()
        except FileNotFoundError:
            pass
        retained -= size
        removed += 1
    return {"retained_bytes": retained, "removed_entries": removed}


def cache_root(environ: dict[str, str]) -> Path:
    if (
        environ.get("GITHUB_ACTIONS") != "true"
        or environ.get("RUNNER_ENVIRONMENT") != "github-hosted"
    ):
        raise ValueError("Cache eviction is restricted to disposable GitHub runners")
    root = Path(environ["RUNNER_TEMP"]) / "bazel-action-cache"
    if root.is_symlink():
        raise ValueError("Cache directory must not be a symlink")
    root.mkdir(parents=True, exist_ok=True)
    return root


def run(command: list[str], root: Path, limit: int) -> int:
    def maintain():
        try:
            result = trim_cache(root, limit)
            if result["removed_entries"]:
                print(json.dumps({"bazel_cache": result}), flush=True)
        except OSError as error:
            # Cache maintenance is an optimization, not a substitute test gate.
            print(
                f"::warning::Bazel cache maintenance failed: {type(error).__name__}",
                flush=True,
            )

    maintain()
    process = subprocess.Popen(command)
    next_maintenance = time.monotonic() + 10
    while True:
        try:
            result = process.wait(timeout=0.1)
            maintain()
            return result
        except subprocess.TimeoutExpired:
            if time.monotonic() >= next_maintenance:
                maintain()
                next_maintenance = time.monotonic() + 10


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--max-bytes", type=int, default=4 * 1024**3)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or args.max_bytes <= 0:
        parser.error("A command and positive cache limit are required")
    raise SystemExit(run(command, cache_root(dict(os.environ)), args.max_bytes))
