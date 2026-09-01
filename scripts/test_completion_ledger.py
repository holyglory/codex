import contextlib
import io
import json
import os
import sqlite3
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import completion_ledger


class CompletionLedgerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.db = Path(self.tempdir.name) / "ledger.sqlite3"

    def invoke(self, *arguments: str) -> tuple[int, dict]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        argv = ["completion_ledger.py", "--db", str(self.db), *arguments]
        with (
            mock.patch("sys.argv", argv),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            status = completion_ledger.main()
        output = stdout.getvalue() if status == 0 else stderr.getvalue()
        return status, json.loads(output)

    def add_issue(self) -> str:
        status, result = self.invoke(
            "add",
            "--requirement",
            "REQ-1",
            "--title",
            "Complete one outcome",
            "--outcome",
            "Users can complete the requested behavior.",
            "--impact",
            "Readiness is blocked until the behavior works.",
            "--current-state",
            "Implementation has not started.",
            "--unblock-condition",
            "Implement and verify the behavior.",
            "--verification",
            "An end-to-end test passes.",
            "--path",
            "src/example.rs",
        )
        self.assertEqual(0, status)
        return result["id"]

    def test_add_and_list_unresolved_issue(self) -> None:
        issue_id = self.add_issue()
        status, result = self.invoke("list")
        self.assertEqual(0, status)
        self.assertEqual([issue_id], [issue["id"] for issue in result["issues"]])
        self.assertEqual(["src/example.rs"], result["issues"][0]["affected_paths"])

    def test_transition_preserves_event_history(self) -> None:
        issue_id = self.add_issue()
        status, _ = self.invoke(
            "transition",
            issue_id,
            "--status",
            "implemented",
            "--note",
            "Implementation is ready for verification.",
        )
        self.assertEqual(0, status)
        status, result = self.invoke("show", issue_id)
        self.assertEqual(0, status)
        self.assertEqual("implemented", result["issue"]["status"])
        self.assertEqual(
            ["created", "transition"],
            [event["event_type"] for event in result["events"]],
        )

    def test_verified_requires_evidence(self) -> None:
        issue_id = self.add_issue()
        status, result = self.invoke(
            "transition",
            issue_id,
            "--status",
            "verified",
            "--note",
            "Attempted verification.",
        )
        self.assertEqual(2, status)
        self.assertIn("requires --verification-evidence", result["error"])

    def test_database_rejects_event_mutation_and_issue_deletion(self) -> None:
        issue_id = self.add_issue()
        connection = sqlite3.connect(self.db)
        with self.assertRaisesRegex(sqlite3.IntegrityError, "append-only"):
            connection.execute("UPDATE issue_events SET note = 'changed'")
        with self.assertRaisesRegex(sqlite3.IntegrityError, "cannot be deleted"):
            connection.execute("DELETE FROM issues WHERE id = ?", (issue_id,))

    @unittest.skipIf(os.name == "nt", "POSIX mode bits are not available on Windows")
    def test_database_requests_shared_modes_without_granting_other_access(self) -> None:
        with mock.patch("completion_ledger.os.chmod", wraps=os.chmod) as chmod:
            status, _ = self.invoke("init")

        self.assertEqual(0, status)
        chmod.assert_any_call(self.db.parent, 0o2770)
        chmod.assert_any_call(self.db, 0o660)
        directory_mode = stat.S_IMODE(self.db.parent.stat().st_mode)
        database_mode = stat.S_IMODE(self.db.stat().st_mode)
        self.assertIn(directory_mode, {0o2770, 0o700})
        self.assertIn(database_mode, {0o660, 0o600})

    @unittest.skipIf(os.name == "nt", "POSIX mode bits are not available on Windows")
    def test_existing_shared_modes_do_not_require_chmod_ownership(self) -> None:
        self.invoke("init")

        with mock.patch("completion_ledger.os.chmod", side_effect=PermissionError):
            status, result = self.invoke("doctor")

        self.assertEqual(0, status)
        self.assertEqual("ok", result["integrity"])

    @unittest.skipIf(os.name == "nt", "POSIX mode bits are not available on Windows")
    def test_sandboxed_explicit_database_accepts_private_mode_fallback(self) -> None:
        self.invoke("init")
        os.chmod(self.db.parent, 0o700)
        os.chmod(self.db, 0o600)

        with mock.patch("completion_ledger.os.chmod", side_effect=PermissionError):
            status, result = self.invoke("doctor")

        self.assertEqual(0, status)
        self.assertEqual("ok", result["integrity"])


if __name__ == "__main__":
    unittest.main()
