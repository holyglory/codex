"""Exercise privilege refusal through the real Linux activation entrypoint."""

import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import unittest

RESTRICT = "import ctypes; assert ctypes.CDLL(None).prctl(38, 1, 0, 0, 0) == 0; "
OWNER = Path(__file__).with_name("local_deploy_owner.py")


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


if __name__ == "__main__":
    unittest.main()
