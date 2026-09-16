from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from run_with_bazel_remote_cache import private_file, ssh_failure_category


class CacheSshKeyFormatTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("ssh-keygen"), "OpenSSH key parser is required")
    def test_private_key_keeps_lf_bytes_and_remains_parseable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / "generated"
            subprocess.run(
                ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(original)],
                capture_output=True,
                check=True,
                timeout=30,
            )
            copy = root / "copied"
            private_file(copy, original.read_text())
            self.assertFalse(
                b"\r" in copy.read_bytes(),
                "SSH key files require preserved LF line endings",
            )
            parsed = subprocess.run(
                ["ssh-keygen", "-y", "-f", str(copy)],
                capture_output=True,
                check=False,
                timeout=30,
            )
            self.assertEqual(
                parsed.returncode, 0, "The copied test key must remain parseable"
            )

    def test_failure_categories_never_return_private_diagnostic_text(self):
        for message, expected in (
            ('Load key "secret-path": invalid format private-fixture', "key_format"),
            ("Permission denied (publickey). private-fixture", "authentication"),
            ("Can't open user config file /dev/null private-fixture", "config_path"),
            ("UNPROTECTED PRIVATE KEY FILE private-fixture", "key_permissions"),
            ("Host key verification failed private-fixture", "host_key"),
            ("Could not resolve hostname private-fixture", "name_resolution"),
            ("Connection refused private-fixture", "connection_refused"),
            ("Connection timed out private-fixture", "connection_timeout"),
            ("Authenticated to private-fixture", "connection_not_ready"),
            ("unrecognized private-fixture", "connection_not_ready"),
        ):
            with self.subTest(category=expected):
                self.assertEqual(ssh_failure_category(message), expected)


if __name__ == "__main__":
    unittest.main()
