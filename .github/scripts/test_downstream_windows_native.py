#!/usr/bin/env python3

import importlib.util
from importlib.machinery import SourceFileLoader
import sys
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_loader(
    "downstream_windows_native",
    SourceFileLoader(
        "downstream_windows_native",
        str(ROOT / ".github/scripts/downstream-windows-native.ps1"),
    ),
)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class WindowsNativeHelperTest(unittest.TestCase):
    def test_capture_output_uses_file_backed_stdout_for_large_child_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            producer = MODULE.Producer.__new__(MODULE.Producer)
            producer.evidence = root
            producer.logs = root / "logs"
            producer.telemetry = root / "telemetry"
            producer.logs.mkdir()
            producer.telemetry.mkdir()
            observed_stdout = []
            popen = MODULE.subprocess.Popen

            def record_stdout(*args, **kwargs):
                observed_stdout.append(kwargs["stdout"])
                return popen(*args, **kwargs)

            with patch.object(MODULE.subprocess, "Popen", side_effect=record_stdout):
                output = producer.run(
                    "large-capture",
                    [
                        sys.executable,
                        "-c",
                        "import sys; sys.stdout.write('x' * 2_000_000)",
                    ],
                    capture_output=True,
                )
            self.assertEqual(len(output), 2_000_000)
            self.assertEqual(len(observed_stdout), 1)
            self.assertIsNot(observed_stdout[0], MODULE.subprocess.PIPE)
            receipt = (root / "checks/large-capture.json").read_text()
            self.assertIn('"exit_code": 0', receipt)


if __name__ == "__main__":
    unittest.main()
