"""Exercise privilege refusal through the real Linux activation entrypoint."""

import json
import os
from pathlib import Path
import select
import signal
import shutil
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

RESTRICT = "import ctypes; assert ctypes.CDLL(None).prctl(38, 1, 0, 0, 0) == 0; "
OWNER = Path(__file__).with_name("local_deploy_owner.py")
QUALIFIER = Path(__file__).with_name("qualify_local_delivery.py")


@unittest.skipUnless(sys.platform == "linux", "Linux /proc and NoNewPrivs contract")
class PrivilegeRefusalTests(unittest.TestCase):
    def test_restricted_launcher_preserves_running_server_and_selection(self):
        self.check_refusal(False, "owner-no-new-privileges-must-be-zero")

    def test_restricted_existing_candidate_is_not_accepted(self):
        self.check_refusal(True, "serving-no-new-privileges-must-be-zero")

    def check_refusal(self, matches_candidate, expected_reason):
        from local_deploy_owner import digest, identity, package_digest

        # The control socket must fit Linux's sockaddr_un path bound even when
        # the test runner supplies a deeply nested TMPDIR.
        with tempfile.TemporaryDirectory(prefix="cxmp-", dir="/tmp") as temporary:
            root = Path(temporary)
            home = root / "home"
            home.mkdir()
            credential = home / "auth.json"
            credential.write_text('{"canary":"preserve"}\n')
            control = home / "app-server-control"
            control.mkdir()
            endpoint = control / "app-server-control.sock"
            standalone = home / "packages/standalone"
            previous = standalone / "releases/previous"
            previous.mkdir(parents=True)
            (standalone / "current").symlink_to(previous)
            candidate = root / "candidate"
            (candidate / "bin").mkdir(parents=True)
            binary = candidate / "bin/codex"
            if matches_candidate:
                shutil.copy2(sys.executable, binary)
            else:
                binary.write_text("#!/bin/sh\nexit 97\n")
                binary.chmod(0o755)
            (candidate / "codex-package.json").write_text(
                json.dumps({"entrypoint": "bin/codex", "target": "test-linux"})
            )
            server = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    RESTRICT
                    + "import socket,sys; s=socket.socket(socket.AF_UNIX); "
                    + "s.bind(sys.argv[1]); s.listen(); print('ready',flush=True); "
                    + "exec('while True: s.accept()[0].close()')",
                    str(endpoint),
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                self.assertTrue(select.select([server.stdout], [], [], 10)[0])
                ready = server.stdout.readline()
                if ready != b"ready\n":
                    self.fail(
                        "fixture server did not start: "
                        + server.stderr.read(4096).decode(errors="replace")
                    )
                before = identity(server.pid)
                self.assertEqual(before["noNewPrivileges"], 1)
                plan = root / "plan.json"
                plan.write_text(
                    json.dumps(
                        {
                            "state": str(root / "state"),
                            "package": str(candidate),
                            "packageSha256": package_digest(candidate),
                            "executableSha256": digest(binary),
                            "ownerUid": os.getuid(),
                            "ownerHome": str(root),
                            "codexHome": str(home),
                            "restartPolicy": "force-matching-unmanaged",
                            "old": before,
                        }
                    )
                )
                result = subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        RESTRICT
                        + "import runpy,sys; sys.argv=sys.argv[1:]; "
                        + "runpy.run_path(sys.argv[0],run_name='__main__')",
                        str(OWNER),
                        str(plan),
                    ],
                    capture_output=True,
                    timeout=20,
                    check=False,
                )
                self.assertEqual(result.returncode, 1, result.stderr.decode())
                receipt = json.loads((root / "state/receipt.json").read_text())
                self.assertEqual(receipt["result"], "failed")
                self.assertEqual(receipt["events"][-1]["reason"], expected_reason)
                self.assertFalse(
                    any(
                        "stop-requested" in event["phase"]
                        for event in receipt["events"]
                    )
                )
                self.assertIsNone(server.poll())
                self.assertEqual(identity(server.pid), before)
                self.assertEqual((standalone / "current").resolve(), previous)
                self.assertEqual(credential.read_text(), '{"canary":"preserve"}\n')
            finally:
                if server.poll() is None:
                    server.terminate()
                server.wait(timeout=10)
                server.stdout.close()
                server.stderr.close()


@unittest.skipUnless(sys.platform == "linux", "Linux /proc and Unix peer contract")
class PostDeploymentQualificationTests(unittest.TestCase):
    def start_server(self, endpoint):
        server = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import signal,socket,sys,time; s=socket.socket(socket.AF_UNIX); "
                "s.bind(sys.argv[1]); s.listen(); "
                "signal.signal(signal.SIGUSR1, lambda *_: s.close()); "
                "print('ready',flush=True); time.sleep(120)",
                str(endpoint),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.assertTrue(select.select([server.stdout], [], [], 10)[0])
        self.assertEqual(server.stdout.readline(), b"ready\n")
        return server

    def make_fixture(self, root, server, checked_at_ms=None):
        from local_deploy_owner import digest, identity, package_digest

        package_root = root / "package-root"
        package = package_root / "package"
        (package / "bin").mkdir(parents=True)
        shutil.copy2(sys.executable, package / "bin/codex")
        (package / "resources").mkdir()
        (package / "resources/rg").write_bytes(b"companion-resource")
        (package / "codex-package.json").write_text(
            json.dumps({"entrypoint": "bin/codex", "target": "x86_64-unknown-linux-gnu", "version": "fixture-version"})
        )
        source_sha = "a" * 64
        (package_root / "source.json").write_text(json.dumps({"source_sha256": source_sha}))
        (package_root / "completion.json").write_text(
            json.dumps({
                "schema_version": 1,
                "claim": "preliminary",
                "source_sha256": source_sha,
                "capabilities": [
                    {"id": "live-sparse-job-attribution", "state": "fixture_only", "evidence_refs": []},
                    {"id": "account-routing-and-cli", "state": "fixture_only", "evidence_refs": []},
                    {"id": "desktop-profile-authentication", "state": "fixture_only", "evidence_refs": []},
                    {"id": "unknown-capability", "state": "fixture_only", "evidence_refs": []},
                ],
            })
        )
        serving = identity(server.pid)
        now = int(time.time() * 1000)
        owner = {
            "result": "passed",
            "packageSha256": package_digest(package),
            "ownershipMode": "managed",
            "serving": serving,
            "events": [
                {"phase": "verify-rpc", "atMs": now - 1000},
                {"phase": "serving-verified", "atMs": now + 60_000},
            ],
        }
        owner_path = root / "owner-receipt.json"
        owner_path.write_text(json.dumps(owner))
        rpc_path = root / "rpc-proof.json"
        rpc_path.write_text(
            json.dumps(
                {
                    "result": "passed",
                    "checkedAtMs": checked_at_ms or now,
                    "realModelTurn": True,
                    "usageSparseControlCapability": True,
                    "sparseJourney": True,
                    "sparseReportVerified": True,
                    "sparseDeclarationsObserved": 3,
                    "sparseThreadId": "fixture-thread",
                    "sparseBoundOperations": 2,
                    "sparseUnattributedOperations": 1,
                    "sparseOutcomeRows": ["fixture-outcome-a", "fixture-outcome-b"],
                    "accountAuthStatusAgrees": True,
                    "accountToolRequested": True,
                    "originalChatResumed": True,
                    "secondClientReattached": True,
                }
            )
        )
        return package_root, owner_path, rpc_path, source_sha, serving

    def qualify_args(self, package_root, owner, rpc, source_root, output, home):
        return SimpleNamespace(
            package_root=str(package_root),
            owner_receipt=str(owner),
            rpc_proof=str(rpc),
            source_root=str(source_root),
            output=str(output),
            artifact="native-linux-gnu",
            target="x86_64-unknown-linux-gnu",
            worktree_id="w0123456789abcdef",
            run_id="t20261111T150000Z-abc123",
            check="qualify-local",
            codex_home=str(home),
        )

    def run_qualifier(self, arguments):
        import qualify_local_delivery

        with (
            patch.object(qualify_local_delivery, "authoritative_source_sha256", return_value="a" * 64),
            patch.object(
                qualify_local_delivery.subprocess,
                "run",
                return_value=SimpleNamespace(stdout="codex-cli fixture-version\n"),
            ),
        ):
            qualify_local_delivery.qualify(arguments)

    def test_success_retains_actual_package_and_exact_delivery_contract(self):
        from local_deploy_owner import digest

        with tempfile.TemporaryDirectory(prefix="qualify-", dir="/tmp") as temporary:
            root = Path(temporary)
            home = root / "home"
            endpoint = home / "app-server-control/app-server-control.sock"
            endpoint.parent.mkdir(parents=True)
            server = self.start_server(endpoint)
            try:
                package_root, owner, rpc, _, _ = self.make_fixture(root, server)
                output = root / "qualified"
                self.run_qualifier(
                    self.qualify_args(package_root, owner, rpc, root, output, home)
                )
                delivery = json.loads((output / "delivery.json").read_text())
                self.assertEqual(delivery["kind"], "local-executable")
                self.assertEqual(delivery["file"], "package/bin/codex")
                self.assertEqual(delivery["observation"], "executable_smoke_passed")
                self.assertEqual(
                    delivery["access"],
                    "artifact://w0123456789abcdef/t20261111T150000Z-abc123/qualify-local/native-linux-gnu/package/bin/codex",
                )
                self.assertEqual(
                    delivery["observed_sha256"], digest(output / "package/bin/codex")
                )
                self.assertEqual(
                    (output / "package/resources/rg").read_bytes(), b"companion-resource"
                )
                completion = json.loads((output / "completion.json").read_text())
                self.assertEqual(completion["claim"], "preliminary")
                states = {item["id"]: item["state"] for item in completion["capabilities"]}
                self.assertEqual(states["live-sparse-job-attribution"], "real_e2e")
                self.assertEqual(states["unknown-capability"], "fixture_only")
            finally:
                server.terminate()
                server.wait(timeout=10)
                server.stdout.close()
                server.stderr.close()

    def test_stale_rpc_proof_rejected_while_old_serving_process_lives(self):
        with tempfile.TemporaryDirectory(prefix="qualify-stale-", dir="/tmp") as temporary:
            root = Path(temporary)
            home = root / "home"
            endpoint = home / "app-server-control/app-server-control.sock"
            endpoint.parent.mkdir(parents=True)
            server = self.start_server(endpoint)
            try:
                package_root, owner, rpc, _, _ = self.make_fixture(
                    root, server, checked_at_ms=int(time.time() * 1000) - 16 * 60 * 1000
                )
                with self.assertRaisesRegex(RuntimeError, "sparse-proof-stale"):
                    self.run_qualifier(
                        self.qualify_args(package_root, owner, rpc, root, root / "out", home)
                    )
                self.assertIsNone(server.poll())
            finally:
                server.terminate()
                server.wait(timeout=10)
                server.stdout.close()
                server.stderr.close()

    def test_changed_current_peer_rejected_even_when_old_pid_remains_alive(self):
        with tempfile.TemporaryDirectory(prefix="qualify-peer-", dir="/tmp") as temporary:
            root = Path(temporary)
            home = root / "home"
            endpoint = home / "app-server-control/app-server-control.sock"
            endpoint.parent.mkdir(parents=True)
            old = self.start_server(endpoint)
            replacement = None
            try:
                package_root, owner, rpc, _, old_identity = self.make_fixture(root, old)
                old.send_signal(signal.SIGUSR1)
                time.sleep(0.2)  # Keep the old PID alive while releasing the peer socket.
                self.assertIsNone(old.poll())
                replacement = self.start_server(endpoint)
                self.assertNotEqual(old_identity["startTicks"], identity(replacement.pid)["startTicks"])
                with self.assertRaisesRegex(RuntimeError, "serving-control-peer-identity-mismatch"):
                    self.run_qualifier(
                        self.qualify_args(package_root, owner, rpc, root, root / "out", home)
                    )
            finally:
                if replacement is not None:
                    replacement.terminate()
                    replacement.wait(timeout=10)
                    replacement.stdout.close()
                    replacement.stderr.close()
                if old.poll() is None:
                    old.terminate()
                    old.wait(timeout=10)
                old.stdout.close()
                old.stderr.close()


if __name__ == "__main__":
    unittest.main()
