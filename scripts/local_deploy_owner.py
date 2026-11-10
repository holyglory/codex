#!/usr/bin/env python3
"""One reviewed local activation attempt, owned by a persistent Coordinator unit.

This Linux installation tool preserves canonical daemon lifecycle controls. It
uses only the explicit stop policy and never escalates after sending a signal,
manufactures daemon state, or restores agent work.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def package_digest(root):
    entries = []
    for path in sorted(Path(root).rglob("*")):
        if path.is_symlink():
            raise RuntimeError("package-symlink-not-supported")
        if path.is_file():
            entries.append(
                [str(path.relative_to(root)), path.stat().st_mode & 0o777, digest(path)]
            )
    return hashlib.sha256(
        json.dumps(entries, separators=(",", ":")).encode()
    ).hexdigest()


def save(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.chmod(0o600)
    temporary.replace(path)


def identity(pid):
    root = Path(f"/proc/{pid}")
    fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
    status = dict(
        line.split(":", 1) for line in (root / "status").read_text().splitlines()
    )
    return {
        "pid": pid,
        "uid": root.stat().st_uid,
        "startTicks": fields[19],
        "executableSha256": digest(root / "exe"),
        "managed": b"--managed-daemon" in (root / "cmdline").read_bytes().split(b"\0"),
        "cgroup": (root / "cgroup").read_text().strip(),
        "noNewPrivileges": int(status["NoNewPrivs"]),
    }


def peer(path):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        try:
            connection.connect(str(path))
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        pid, uid, _ = struct.unpack(
            "3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        )
    result = identity(pid)
    if result["uid"] != uid:
        raise RuntimeError("peer-identity-changed")
    return result


def exited(pidfd, seconds=None):
    poller = select.poll()
    poller.register(pidfd, select.POLLIN)
    return bool(poller.poll() if seconds is None else poller.poll(int(seconds * 1000)))


def select_release(root, release):
    temporary = root / f".local-selection-{os.getpid()}"
    temporary.symlink_to(release)
    temporary.replace(root / "current")


class Owner:
    def __init__(self, plan_path):
        self.plan_path = Path(plan_path).resolve(strict=True)
        self.plan = json.loads(self.plan_path.read_text())
        self.state = Path(self.plan["state"])
        self.state.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.receipt = {
            "result": "incomplete",
            "phase": "preflight",
            "events": [],
            "packageSha256": self.plan["packageSha256"],
            "owner": identity(os.getpid()),
        }
        self.task_codex_home = Path(self.plan["codexHome"])
        self.endpoint = (
            self.task_codex_home / "app-server-control/app-server-control.sock"
        )
        self.env = {
            **os.environ,
            "HOME": self.plan["ownerHome"],
            "CODEX_HOME": str(self.task_codex_home),
        }
        self.candidate = Path(self.plan["package"]).resolve(strict=True)
        self.previous = None
        self.selected = None
        self.fixture_children = []

    def event(self, phase, **fields):
        self.receipt["phase"] = phase
        self.receipt["events"].append(
            {"phase": phase, "atMs": int(time.time() * 1000), **fields}
        )
        save(self.state / "receipt.json", self.receipt)
        print(json.dumps({"phase": phase}), flush=True)

    def command(self, name, argv):
        self.event(name)
        with (
            (self.state / f"{name}.stdout").open("wb") as out,
            (self.state / f"{name}.stderr").open("wb") as err,
        ):
            result = subprocess.run(
                argv,
                env=self.env,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=err,
                check=False,
            )
        if result.returncode:
            raise RuntimeError(f"{name}-exit-{result.returncode}")

    def stage(self):
        if (
            os.getuid() != self.plan["ownerUid"]
            or self.task_codex_home == self.candidate
        ):
            raise RuntimeError("wrong-owner-or-home")
        if package_digest(self.candidate) != self.plan["packageSha256"]:
            raise RuntimeError("package-changed")
        metadata = json.loads((self.candidate / "codex-package.json").read_text())
        if (
            metadata["entrypoint"] != "bin/codex"
            or digest(self.candidate / "bin/codex") != self.plan["executableSha256"]
        ):
            raise RuntimeError("executable-changed")
        root = self.task_codex_home / "packages/standalone"
        releases = root / "releases"
        releases.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.install_lock = (root / "install.lock").open("a+")
        fcntl.flock(self.install_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        current = root / "current"
        self.previous = current.resolve(strict=True) if current.is_symlink() else None
        if current.exists() and not current.is_symlink():
            raise RuntimeError("non-symlink-standalone-selection")
        release = releases / f"local-{self.plan['packageSha256']}-{metadata['target']}"
        if not release.exists():
            with tempfile.TemporaryDirectory(
                prefix=".local-stage-", dir=releases
            ) as temporary:
                stage = Path(temporary) / "package"
                shutil.copytree(self.candidate, stage)
                if package_digest(stage) != self.plan["packageSha256"]:
                    raise RuntimeError("staged-package-changed")
                stage.replace(release)
        if package_digest(release) != self.plan["packageSha256"]:
            raise RuntimeError("existing-release-changed")
        self.selected = release
        self.event(
            "staged",
            previous=str(self.previous) if self.previous else None,
            selected=str(release),
        )

    def fixture_server(self, binary):
        if self.plan.get("fixture") not in {
            "unmanaged",
            "forced",
            "desktop",
            "mismatch",
        }:
            raise RuntimeError("fixture-mode-not-authorized")
        if not str(self.task_codex_home).startswith("/mnt/build-storage/codex/ld/"):
            raise RuntimeError("fixture-home-not-disposable")
        log = (self.state / f"fixture-{len(self.fixture_children)}.stderr").open("wb")
        child = subprocess.Popen(
            [str(binary), "app-server", "--listen", "unix://"],
            env=self.env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
        self.fixture_children.append(child)
        limit = time.monotonic() + 30
        while time.monotonic() < limit:
            observed = peer(self.endpoint)
            if observed:
                return observed
            if child.poll() is not None:
                raise RuntimeError("fixture-server-exited")
            time.sleep(0.1)
        raise RuntimeError("fixture-server-not-ready")

    def verify_serving(self, observed, mode):
        if (
            observed["uid"] != self.plan["ownerUid"]
            or observed["executableSha256"] != self.plan["executableSha256"]
        ):
            raise RuntimeError("candidate-identity-mismatch")
        if observed["noNewPrivileges"] != 0:
            raise RuntimeError("serving-no-new-privileges-must-be-zero")
        self.receipt["serving"] = observed
        self.receipt["ownershipMode"] = mode
        try:
            self.command(
                "verify-rpc",
                [
                    sys.executable,
                    str(Path(__file__).with_name("local_deploy_rpc.py")),
                    str(self.plan_path),
                ],
            )
        except Exception as error:
            # A verification failure is not authority to terminate a serving child.
            self.receipt["result"] = "failed"
            self.event("verification-failed", reason=str(error)[:200])
        else:
            self.receipt["result"] = "passed"
            self.event("serving-verified")
        self.install_lock.close()
        with os.fdopen(os.pidfd_open(observed["pid"]), "rb") as descriptor:
            if identity(observed["pid"]) != observed:
                raise RuntimeError("candidate-changed-before-supervision")
            exited(descriptor.fileno())
        raise RuntimeError("serving-child-exited")

    def activate(self):
        if self.plan["restartPolicy"] == "force-matching-unmanaged" and (
            self.plan.get("captureOldIdentity")
            or (not self.plan.get("old") and not self.plan.get("fixture"))
        ):
            raise RuntimeError("forced-stop-requires-explicit-old-identity")
        self.stage()
        observed = peer(self.endpoint)
        expected = self.plan.get("old")
        if observed and expected is None and self.plan.get("captureOldIdentity"):
            expected = observed
            self.receipt["capturedOld"] = observed
            self.event("old-identity-captured", old=observed)
        if self.plan.get("fixture"):
            if observed:
                raise RuntimeError("fixture-endpoint-already-present")
            reference = Path(self.plan["oldBinary"])
            if digest(reference) != self.plan["oldExecutableSha256"]:
                raise RuntimeError("fixture-old-binary-changed")
            observed = expected = self.fixture_server(reference)
        if observed and (
            observed["uid"] == self.plan["ownerUid"]
            and observed["executableSha256"] == self.plan["executableSha256"]
        ):
            if observed["noNewPrivileges"] != 0:
                raise RuntimeError("serving-no-new-privileges-must-be-zero")
            root = self.task_codex_home / "packages/standalone"
            select_release(root, self.selected)
            self.event("candidate-already-serving", serving=observed)
            self.verify_serving(
                observed, "desktop-owned" if not observed["managed"] else "managed"
            )
        if observed:
            if not expected or any(
                observed[key] != expected[key]
                for key in ("pid", "uid", "startTicks", "executableSha256")
            ):
                raise RuntimeError("old-endpoint-identity-mismatch")
            if observed["managed"] or self.plan["restartPolicy"] not in {
                "single-graceful-matching-unmanaged",
                "force-matching-unmanaged",
            }:
                raise RuntimeError("unsupported-old-owner")
        elif expected:
            raise RuntimeError("expected-old-endpoint-missing")
        # Children inherit this irreversible bit. Refuse before changing the
        # selected release or signalling the existing server.
        if identity(os.getpid())["noNewPrivileges"] != 0:
            raise RuntimeError("owner-no-new-privileges-must-be-zero")
        # Select before drain: Desktop can only respawn the reviewed package.
        root = self.task_codex_home / "packages/standalone"
        select_release(root, self.selected)
        self.event("standalone-selected")
        if observed:
            with os.fdopen(os.pidfd_open(observed["pid"]), "rb") as descriptor:
                if (
                    peer(self.endpoint) != observed
                    or identity(observed["pid"]) != observed
                ):
                    raise RuntimeError("old-owner-changed-before-stop")
                forced = self.plan["restartPolicy"] == "force-matching-unmanaged"
                signal.pidfd_send_signal(
                    descriptor.fileno(), signal.SIGKILL if forced else signal.SIGTERM
                )
                self.event(
                    "forced-stop-requested" if forced else "graceful-stop-requested",
                    old=observed,
                )
                if not exited(descriptor.fileno(), self.plan.get("drainSeconds", 600)):
                    raise RuntimeError(
                        "old-server-did-not-exit-after-forced-stop"
                        if forced
                        else "old-work-still-draining-no-escalation"
                    )
            self.event("old-exited")
        if self.plan.get("fixture") in {"desktop", "mismatch"}:
            fixture_binary = self.candidate / "bin/codex"
            if self.plan["fixture"] == "mismatch":
                fixture_binary = Path(self.plan["mismatchBinary"])
            self.fixture_server(fixture_binary)
        observed = peer(self.endpoint)
        if observed:
            if (
                observed["uid"] != self.plan["ownerUid"]
                or observed["executableSha256"] != self.plan["executableSha256"]
            ):
                raise RuntimeError("replacement-endpoint-identity-mismatch")
            mode = "desktop-owned"
        else:
            binary = str(self.candidate / "bin/codex")
            prior_state = self.task_codex_home / "app-server-daemon"
            prior_selection = (
                self.task_codex_home / "packages/app-server-daemon/current"
            )
            # Canonical Replace requires a prior daemon selection. Cold Start
            # performs canonical Missing installation from this complete package.
            prior_daemon = prior_selection.exists() or any(
                (prior_state / name).exists()
                for name in (
                    "daemon.pid",
                    "daemon.stderr.log",
                    "daemon-updater.pid",
                    "daemon-updater.stderr.log",
                    "app-server.pid",
                    "app-server.stderr.log",
                    "app-server-updater.pid",
                    "app-server-updater.stderr.log",
                )
            )
            if prior_daemon:
                self.command(
                    "canonical-update",
                    [binary, "app-server", "daemon", "update", "--from-cli", "--yes"],
                )
            self.command("canonical-start", [binary, "app-server", "daemon", "start"])
            observed = peer(self.endpoint)
            mode = "managed"
            record = json.loads(
                (self.task_codex_home / "app-server-daemon/daemon.pid").read_text()
            )
            if (
                not observed
                or record["pid"] != observed["pid"]
                or not observed["managed"]
            ):
                raise RuntimeError("canonical-child-ownership-missing")
            if observed["cgroup"] != identity(os.getpid())["cgroup"]:
                raise RuntimeError("canonical-child-outside-owner-unit")
        self.verify_serving(observed, mode)

    def run(self):
        try:
            with (self.state / "attempt.once").open("x"):
                pass
        except FileExistsError:
            return 78  # systemd may restart this owner, never its installation attempt.
        try:
            self.activate()
        except Exception as error:
            self.receipt["result"] = "failed"
            self.event("failed", reason=str(error)[:200])
            # Keep a responsive exact candidate selected. Never touch a new peer.
            if self.previous and peer(self.endpoint) is None:
                root = self.task_codex_home / "packages/standalone"
                if (root / "current").resolve() == self.selected:
                    select_release(root, self.previous)
                    self.event("previous-cli-selection-restored")
            elif self.previous is None and peer(self.endpoint) is None:
                root = self.task_codex_home / "packages/standalone"
                if (root / "current").resolve() == self.selected:
                    (root / "current").unlink()
                    self.event("empty-cli-selection-restored")
            return 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("plan", type=Path)
    arguments = parser.parse_args()
    sys.exit(Owner(arguments.plan).run())
