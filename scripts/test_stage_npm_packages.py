import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import stage_npm_packages as stage


class DownloadedNativeArtifactsTests(unittest.TestCase):
    def test_installs_all_downloaded_platforms_without_network(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = root / "artifacts"
            expected = {}
            for target in stage.BINARY_TARGETS:
                target_dir = artifacts / target
                target_dir.mkdir(parents=True)
                payload = f"isolated archive fixture for {target}".encode()
                expected[target] = payload
                with tarfile.open(
                    target_dir / f"codex-package-{target}.tar.gz", "w:gz"
                ) as archive:
                    entry = tarfile.TarInfo("bin/fixture")
                    entry.size = len(payload)
                    archive.addfile(entry, io.BytesIO(payload))
            with patch.object(stage, "download_artifacts") as download:
                stage.install_native_components(
                    None, {stage.CODEX_PACKAGE_COMPONENT}, root / "staging", artifacts
                )
                download.assert_not_called()
            self.assertEqual(
                {
                    target: (
                        root / "staging/vendor" / target / "bin/fixture"
                    ).read_bytes()
                    for target in stage.BINARY_TARGETS
                },
                expected,
            )
            (
                artifacts
                / stage.BINARY_TARGETS[-1]
                / f"codex-package-{stage.BINARY_TARGETS[-1]}.tar.gz"
            ).unlink()
            with (
                patch.object(stage, "download_artifacts") as download,
                self.assertRaisesRegex(FileNotFoundError, "incomplete"),
            ):
                stage.install_native_components(
                    None,
                    {stage.CODEX_PACKAGE_COMPONENT},
                    root / "incomplete",
                    artifacts,
                )
            download.assert_not_called()
            self.assertEqual(list((root / "incomplete/vendor").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
