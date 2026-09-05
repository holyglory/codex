import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("archive-release-symbols-and-strip-binaries.sh")
BINARIES = ("codex", "codex-app-server", "codex-code-mode-host")


class WindowsSymbolsTest(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="codex symbols ")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.release = self.root / "release binaries"
        self.release.mkdir()
        self.environment = dict(os.environ, RUNNER_TEMP=self.root.as_posix())

    def archive(self, target="x86_64-pc-windows-msvc"):
        return subprocess.run(
            [
                os.environ.get("CODEX_SYMBOLS_BASH", "bash"),
                SCRIPT.as_posix(),
                "--target",
                target,
                "--artifact-name",
                "test",
                "--release-dir",
                self.release.as_posix(),
                "--archive-dir",
                (self.root / "archives").as_posix(),
                "--binaries",
                " ".join(BINARIES),
            ],
            env=self.environment,
            capture_output=True,
            text=True,
            check=False,
        )

    def contents(self):
        with tarfile.open(self.root / "archives/codex-symbols-test.tar.gz") as archive:
            return {
                Path(member.name).name: archive.extractfile(member).read()
                for member in archive.getmembers()
                if member.isfile()
            }

    def write_symbols(self, normalized):
        expected = {}
        for binary in BINARIES:
            filename = (binary.replace("-", "_") if normalized else binary) + ".pdb"
            expected[filename] = f"isolated symbol fixture for {binary}".encode()
            (self.release / filename).write_bytes(expected[filename])
        return expected

    def test_preserves_direct_symbol_names_and_bytes(self):
        expected = self.write_symbols(normalized=False)
        result = self.archive()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.contents(), expected)

    def test_preserves_normalized_symbol_names_and_bytes(self):
        expected = self.write_symbols(normalized=True)
        result = self.archive("aarch64-pc-windows-msvc")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.contents(), expected)

    def test_missing_symbol_does_not_produce_an_archive(self):
        self.write_symbols(normalized=True)
        (self.release / "codex_app_server.pdb").unlink()
        result = self.archive()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not found", result.stderr)
        self.assertFalse((self.root / "archives/codex-symbols-test.tar.gz").exists())

    def test_ambiguous_symbol_does_not_select_a_stale_file(self):
        self.write_symbols(normalized=True)
        (self.release / "codex-app-server.pdb").write_bytes(b"stale symbols")
        result = self.archive()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Ambiguous PDBs", result.stderr)
        self.assertFalse((self.root / "archives/codex-symbols-test.tar.gz").exists())

    @unittest.skipUnless(os.name == "nt", "requires a native MSVC Rust toolchain")
    def test_archives_real_cargo_release_pdbs_without_renaming(self):
        project = self.root / "native fixture"
        project.mkdir()
        manifest = (
            '[package]\nname = "symbols-fixture"\nversion = "0.1.0"\nedition = "2024"\n'
        )
        for binary in BINARIES:
            manifest += f'\n[[bin]]\nname = "{binary}"\npath = "{binary}.rs"\n'
            (project / f"{binary}.rs").write_text(
                'fn main() { println!("symbol fixture"); }\n'
            )
        manifest += "\n[profile.release]\ndebug = 2\n"
        (project / "Cargo.toml").write_text(manifest)
        environment = dict(self.environment, CARGO_TARGET_DIR=str(project / "target"))
        for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
            environment.pop(key, None)
        build = subprocess.run(
            ["cargo", "build", "--release", "--offline"],
            cwd=project,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(build.returncode, 0, build.stderr)
        self.release = project / "target/release"
        expected = {path.name: path.read_bytes() for path in self.release.glob("*.pdb")}
        self.assertEqual(
            set(expected), {binary.replace("-", "_") + ".pdb" for binary in BINARIES}
        )
        for binary in BINARIES:
            execution = subprocess.run(
                [str(self.release / f"{binary}.exe")], capture_output=True, check=True
            )
            self.assertEqual(execution.stdout.strip(), b"symbol fixture")
        result = self.archive()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.contents(), expected)


if __name__ == "__main__":
    unittest.main()
