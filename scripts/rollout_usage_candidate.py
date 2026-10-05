#!/usr/bin/env python3
"""Stage a verified local candidate and activate it through cooperative handover."""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import tarfile
import tempfile


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def save(path, value):
    temporary = path.with_suffix(".tmp")
    with temporary.open("w") as stream:
        json.dump(value, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def identities(package):
    return {
        str(item.relative_to(package)): digest(item)
        for item in sorted(package.rglob("*"))
        if item.is_file()
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["backup", "stage", "activate", "verify", "rollback"])
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--runtime-home", required=True, type=Path)
    parser.add_argument("--receipt", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    home = args.runtime_home.resolve(strict=True)
    if home.stat().st_uid != os.getuid():
        raise SystemExit("Run as the runtime owner")
    args.evidence.mkdir(mode=0o700, parents=True, exist_ok=True)
    receipt_path = args.evidence / "rollout.json"
    package_root = home / "packages/standalone"
    current = package_root / "current"
    if (home / "packages/app-server-daemon/current").exists():
        raise SystemExit("This rollout requires the recorded standalone installation")
    if args.action == "backup":
        if receipt_path.exists():
            raise SystemExit("Refusing to overwrite rollout evidence")
        previous = current.resolve(strict=True)
        backup = args.evidence / "usage.sqlite3"
        if backup.exists():
            raise SystemExit("Refusing to overwrite backup")
        with sqlite3.connect("file:" + str(home / "usage/usage.sqlite3") + "?mode=ro", uri=True) as source:
            with sqlite3.connect(backup) as destination:
                source.backup(destination, pages=1024, sleep=0.1)
                if destination.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                    raise SystemExit("Backup integrity failed")
        save(receipt_path, {"previous": str(previous), "previous_files": identities(previous),
                            "backup_sha256": digest(backup), "status": "backed_up"})
    else:
        record = json.loads(receipt_path.read_text())
        previous = Path(record["previous"])
        if args.action == "stage":
            from local_candidate import validate
            if args.receipt is None:
                raise SystemExit("Provide exact-commit local acceptance")
            accepted = json.loads(args.receipt.read_text())
            validate(accepted, accepted["commit"])
            for check in accepted["checks"]:
                if digest(args.receipt.parent / (check["name"] + ".log")) != check["log_sha256"]:
                    raise SystemExit("Acceptance evidence changed")
            package = args.receipt.parent / "linux-package"
            if (package / "source-commit.txt").read_text().strip() != accepted["commit"]:
                raise SystemExit("Package source mismatch")
            archive = package / "native-dist/codex-package-x86_64-unknown-linux-musl.tar.gz"
            checksums = (archive.parent / "SHA256SUMS").read_text().splitlines()
            expected = next(line.split()[0] for line in checksums if line.split()[-1].removeprefix("./") == archive.name)
            if digest(archive) != expected:
                raise SystemExit("Package checksum mismatch")
            releases = package_root / "releases"
            with tempfile.TemporaryDirectory(prefix=".usage-stage-", dir=releases) as temporary:
                stage = Path(temporary)
                with tarfile.open(archive) as bundle:
                    bundle.extractall(stage, filter="data")
                metadata = json.loads((stage / "codex-package.json").read_text())
                if metadata["version"] != "0.159.1+multi.2" or metadata["target"] != "x86_64-unknown-linux-musl":
                    raise SystemExit("Unexpected package identity")
                version = subprocess.check_output([str(stage / "bin/codex"), "--version"], text=True).strip()
                if version != "codex-cli " + metadata["version"]:
                    raise SystemExit("Executable version mismatch")
                release = releases / (metadata["version"] + "-x86_64-unknown-linux-musl-local-" + accepted["commit"][:12])
                files = identities(stage)
                if release.exists():
                    if identities(release) != files:
                        raise SystemExit("Existing package differs")
                else:
                    os.rename(stage, release)
            record.update(target=str(release), source_commit=accepted["commit"], files=files, status="staged")
            save(receipt_path, record)
        elif args.action in ("activate", "rollback"):
            target = previous if args.action == "rollback" else Path(record["target"])
            expected = record["previous_files"] if args.action == "rollback" else record["files"]
            if identities(target) != expected or digest(args.evidence / "usage.sqlite3") != record["backup_sha256"]:
                raise SystemExit("Package or backup changed")
            with (package_root / "install.lock").open("a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                if current.resolve() not in (previous, Path(record["target"])):
                    raise SystemExit("Another installation owns selection")
                temporary = package_root / (".usage-current-" + str(os.getpid()))
                temporary.symlink_to(target)
                os.replace(temporary, current)
            # The supported worker retains live recovery state and the previous
            # serving executable. No direct process stop or database restore.
            response = subprocess.check_output([str(target / "bin/codex"), "app-server", "daemon", "handover"], text=True)
            record["handover"] = json.loads(response)
            record["status"] = args.action + "_requested"
            save(receipt_path, record)
        else:
            target = Path(record["target"])
            if identities(target) != record["files"] or current.resolve() != target:
                raise SystemExit("Installed package mismatch")
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(5)
                connection.connect(str(home / "app-server-control/app-server-control.sock"))
                pid, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            serving = Path("/proc") / str(pid) / "exe"
            if uid != os.getuid() or serving.resolve() not in (target / "bin/codex", target / "bin/codex-app-server"):
                raise SystemExit("Serving executable mismatch")
            if digest(serving) != record["files"][str(serving.resolve().relative_to(target))]:
                raise SystemExit("Serving executable digest mismatch")
            record["status"] = "serving_verified"
            save(receipt_path, record)
    print(json.dumps({"action": args.action, "verified": True}), flush=True)


if __name__ == "__main__":
    main()
