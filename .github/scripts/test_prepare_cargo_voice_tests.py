import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from prepare_cargo_voice_tests import cargo_environment


@unittest.skipUnless(sys.platform == "linux", "GNU/Linux native test setup")
class CargoVoiceEnvironmentTest(unittest.TestCase):
    def test_native_metadata_is_private_and_system_dependencies_keep_their_search_path(
        self,
    ):
        with tempfile.TemporaryDirectory(prefix="voice test ") as temporary:
            root = Path(temporary)
            prefix = root / "native/prefix"
            prefix.mkdir(parents=True)
            pkg_config = root / "pkg-config"
            pkg_config.write_text(
                "#!/usr/bin/python3\n"
                "import json, os, sys\n"
                "print(json.dumps([sys.argv[1:], os.environ.get('PKG_CONFIG_LIBDIR'), "
                "os.environ.get('PKG_CONFIG_PATH')]))\n"
            )
            pkg_config.chmod(0o755)
            environment = cargo_environment(prefix, pkg_config)
            inherited = dict(
                os.environ,
                PKG_CONFIG_LIBDIR="system metadata",
                PKG_CONFIG_PATH="extra metadata",
            )
            for package in ("gstreamer-1.0", "gstreamer-app-1.0", "gio-2.0"):
                with self.subTest(package=package):
                    actual = subprocess.check_output(
                        [environment["PKG_CONFIG"], "--libs", package],
                        env=inherited,
                        text=True,
                    )
                    self.assertEqual(
                        json.loads(actual),
                        [
                            ["--define-prefix", "--libs", package],
                            os.pathsep.join(
                                str(prefix / p)
                                for p in ("lib/pkgconfig", "share/pkgconfig")
                            ),
                            "",
                        ],
                    )
            for package in ("openssl", "alsa", "libcap"):
                with self.subTest(package=package):
                    actual = subprocess.check_output(
                        [environment["PKG_CONFIG"], "--libs", package],
                        env=inherited,
                        text=True,
                    )
                    self.assertEqual(
                        json.loads(actual),
                        [["--libs", package], "system metadata", "extra metadata"],
                    )


if __name__ == "__main__":
    unittest.main()
