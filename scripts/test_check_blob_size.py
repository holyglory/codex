"""Exercise blob-policy boundaries with real, isolated Git histories."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


CHECKER = Path(__file__).with_name("check_blob_size.py").resolve()


class BlobSizePolicyTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="codex-blob-policy-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "--quiet")
        self.git("config", "user.name", "Blob policy fixture")
        self.git("config", "user.email", "blob-policy@example.invalid")
        self.git("config", "commit.gpgsign", "false")

    def git(self, *arguments):
        return subprocess.run(
            ["git", *arguments],
            cwd=self.repo,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

    def commit(self, files):
        for name, content in files.items():
            (self.repo / name).write_bytes(content)
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "Fixture files")
        return self.git("rev-parse", "HEAD")

    def check(self, base, head, allowlist=""):
        allowlist_path = self.root / "allowlist.txt"
        allowlist_path.write_text(allowlist, encoding="utf-8")
        return subprocess.run(
            [
                sys.executable,
                str(CHECKER),
                "--base",
                base,
                "--head",
                head,
                "--max-bytes",
                "32",
                "--allowlist",
                str(allowlist_path),
            ],
            cwd=self.repo,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_new_branch_checks_inherited_files_not_only_latest_commit(self):
        previous = self.commit({"inherited.bin": b"\0" * 33})
        head = self.commit({"small.txt": b"ok"})
        for sentinel in ("0" * 40, "0" * 64):
            with self.subTest(sentinel_length=len(sentinel)):
                result = self.check(sentinel, head)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("Checked 2 changed file(s)", result.stdout)
                self.assertIn("inherited.bin: 33 bytes > 32 bytes", result.stdout)
        ordinary = self.check(previous, head)
        self.assertEqual(ordinary.returncode, 0, ordinary.stderr)
        self.assertIn("Checked 1 changed file(s)", ordinary.stdout)

    def test_new_branch_preserves_exact_allowlist_matching(self):
        head = self.commit({"asset.bin": b"\0" * 33})
        permitted = self.check("0" * 40, head, "asset.bin\n")
        self.assertEqual(permitted.returncode, 0, permitted.stderr)
        self.assertIn("[binary, allowlisted]", permitted.stdout)
        wrong_path = self.check("0" * 40, head, "other/asset.bin\n")
        self.assertEqual(wrong_path.returncode, 1, wrong_path.stderr)
        self.assertIn("[binary, blocked]", wrong_path.stdout)

    def test_binary_and_text_blobs_obey_the_same_exact_size_boundary(self):
        head = self.commit(
            {
                "at-limit.txt": b"a" * 32,
                "over-limit.txt": b"a" * 33,
                "at-limit.bin": b"\0" * 32,
                "over-limit.bin": b"\0" * 33,
            }
        )
        result = self.check("0" * 40, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        statuses = [line for line in result.stdout.splitlines() if "[" in line]
        self.assertEqual(
            statuses,
            [
                "- at-limit.bin: 32 bytes (0.0 KiB) [binary, ok]",
                "- at-limit.txt: 32 bytes (0.0 KiB) [non-binary, ok]",
                "- over-limit.bin: 33 bytes (0.0 KiB) [binary, blocked]",
                "- over-limit.txt: 33 bytes (0.0 KiB) [non-binary, blocked]",
            ],
        )

    def test_ordinary_push_still_rejects_new_oversized_blobs(self):
        previous = self.commit({"small.txt": b"ok"})
        head = self.commit({"large.txt": b"a" * 33})
        result = self.check(previous, head)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("large.txt: 33 bytes > 32 bytes", result.stdout)

    def test_invalid_revisions_still_fail_closed(self):
        head = self.commit({"small.txt": b"ok"})
        for invalid in ("f" * 40, "0" * 39, ""):
            with self.subTest(base=invalid):
                result = self.check(invalid, head)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("No changed files were detected.", result.stdout)


if __name__ == "__main__":
    unittest.main()
