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
        self.previous_daemon = None
        self.rollback_peer = None
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
        daemon = self.task_codex_home / "packages/app-server-daemon/current"
        self.previous_daemon = daemon.resolve(strict=True) if daemon.exists() else None
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
            "managed",
        }:
            raise RuntimeError("fixture-mode-not-authorized")
        if not str(self.task_codex_home).startswith("/mnt/build-storage/codex/ld/"):
            raise RuntimeError("fixture-home-not-disposable")
        if self.plan["fixture"] == "managed":
            self.command(
                "fixture-managed-start",
                [str(binary), "app-server", "daemon", "start"],
            )
            observed = self.managed_identity(peer(self.endpoint))
            return observed
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

    def managed_identity(self, observed):
        record = json.loads(
            (self.task_codex_home / "app-server-daemon/daemon.pid").read_text()
        )
        native = record.get("processIdentity", record.get("linuxProcessIdentity"))
        if (
            not observed
            or not observed["managed"]
            or record["pid"] != observed["pid"]
            or native
            != {
                "bootId": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "startTicks": int(observed["startTicks"]),
            }
            or digest(
                self.task_codex_home / "packages/app-server-daemon/current/bin/codex"
            )
            != observed["executableSha256"]
        ):
            raise RuntimeError("canonical-managed-identity-mismatch")
        return observed

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
        selected = self.selected
        if selected is None:
            raise RuntimeError("staged-release-missing")
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
            if identity(os.getpid())["noNewPrivileges"] != 0:
                raise RuntimeError("owner-no-new-privileges-must-be-zero")
            root = self.task_codex_home / "packages/standalone"
            select_release(root, self.selected)
            self.event("candidate-already-serving", serving=observed)
            self.verify_serving(
                observed, "desktop-owned" if not observed["managed"] else "managed"
            )
        if observed:
            if self.plan["restartPolicy"] == "canonical-matching-managed" and (
                not expected or self.plan.get("captureOldIdentity")
            ):
                raise RuntimeError("canonical-managed-requires-explicit-old-identity")
            if not expected or any(
                observed[key] != expected[key]
                for key in ("pid", "uid", "startTicks", "executableSha256")
            ):
                raise RuntimeError("old-endpoint-identity-mismatch")
            if observed["managed"]:
                if self.plan["restartPolicy"] != "canonical-matching-managed":
                    raise RuntimeError("unsupported-old-owner")
                self.managed_identity(observed)
                self.previous_daemon = (
                    self.task_codex_home / "packages/app-server-daemon/current"
                ).resolve(strict=True)
            elif self.plan["restartPolicy"] not in {
                "single-graceful-matching-unmanaged",
                "force-matching-unmanaged",
            }:
                raise RuntimeError("unsupported-old-owner")
            self.rollback_peer = observed
        elif expected:
            raise RuntimeError("expected-old-endpoint-missing")
        elif self.plan["restartPolicy"] == "canonical-matching-managed":
            raise RuntimeError("expected-managed-endpoint-missing")
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
                if observed["managed"]:
                    self.managed_identity(observed)
                    # This workflow owns activation. The canonical command owns
                    # daemon.lock and its separate daemon install.lock; taking
                    # either here would deadlock it. These checks are not an
                    # atomic compare-and-update against independent CLI actors.
                    self.command(
                        "canonical-managed-update",
                        [
                            str(selected / "bin/codex"),
                            "app-server",
                            "daemon",
                            "update",
                            "--from-cli",
                            "--yes",
                        ],
                    )
                    if not exited(descriptor.fileno(), 0):
                        raise RuntimeError("canonical-update-old-process-still-running")
                else:
                    forced = self.plan["restartPolicy"] == "force-matching-unmanaged"
                    signal.pidfd_send_signal(
                        descriptor.fileno(),
                        signal.SIGKILL if forced else signal.SIGTERM,
                    )
                    self.event(
                        "forced-stop-requested"
                        if forced
                        else "graceful-stop-requested",
                        old=observed,
                    )
                    if not exited(
                        descriptor.fileno(), self.plan.get("drainSeconds", 600)
                    ):
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
            mode = "managed" if observed["managed"] else "desktop-owned"
            if mode == "managed":
                self.managed_identity(observed)
                if observed["cgroup"] != identity(os.getpid())["cgroup"]:
                    raise RuntimeError("canonical-child-outside-owner-unit")
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
            observed = self.managed_identity(peer(self.endpoint))
            mode = "managed"
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
            observed = peer(self.endpoint)
            daemon = self.task_codex_home / "packages/app-server-daemon/current"
            daemon_selected = daemon.resolve(strict=True) if daemon.exists() else None
            # Canonical replacement may already have selected a new daemon even
            # when launch/readiness failed. Keep its matching CLI selected and
            # report recovery, rather than claiming a daemon rollback.
            if (
                self.plan["restartPolicy"] == "canonical-matching-managed"
                and self.selected is not None
                and daemon_selected != self.previous_daemon
            ):
                self.event(
                    "canonical-daemon-selection-retained",
                    selected=str(daemon_selected) if daemon_selected else None,
                    recovery="inspect-canonical-daemon-and-retry-start",
                )
            elif self.previous and (observed is None or observed == self.rollback_peer):
                root = self.task_codex_home / "packages/standalone"
                if (root / "current").resolve() == self.selected:
                    select_release(root, self.previous)
                    self.event("previous-cli-selection-restored")
            elif self.previous is None and (
                observed is None or observed == self.rollback_peer
            ):
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
