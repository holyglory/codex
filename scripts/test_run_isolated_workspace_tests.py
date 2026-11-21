"""Exercise cleanup through the real Linux namespace wrapper."""

from contextlib import contextmanager
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest


WRAPPER = Path(__file__).resolve().with_name("run_isolated_workspace_tests.sh")


@contextmanager
def fixture_root():
    root = Path(tempfile.mkdtemp(prefix="isolation-cleanup-"))
    try:
        yield root
    except BaseException:
        print(f"Retained wrapper regression scratch: {root}", file=sys.stderr)
        raise
    else:
        shutil.rmtree(root)


def run_wrapper(scratch, body, *arguments, private_tmp_root=False):
    return subprocess.run(
        [
            "bash",
            str(WRAPPER),
            *(["--private-tmp-root"] if private_tmp_root else []),
            str(scratch),
            sys.executable,
            "-c",
            body,
            *map(str, arguments),
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )


@unittest.skipUnless(sys.platform == "linux", "Linux namespace isolation only")
class IsolatedWorkspaceCleanupTests(unittest.TestCase):
    def test_success_removes_read_only_cache_without_touching_link_targets(self):
        with fixture_root() as root:
            scratch = root / "scratch"
            scratch.mkdir()
            outside = root / "outside"
            outside.mkdir()
            payload = outside / "payload"
            payload.write_text("unchanged fixture")
            payload.chmod(0o400)
            outside.chmod(0o500)
            result = run_wrapper(
                scratch,
                "from pathlib import Path\nimport os,sys\n"
                "cache=Path('/tmp/cache');cache.mkdir()\n"
                "backing=next(Path(sys.argv[3]).glob('invocation.*'))/'cache'\n"
                "os.link(sys.argv[1],backing/'hard-link')\n"
                "assert (cache/'hard-link').read_bytes()==Path(sys.argv[1]).read_bytes()\n"
                "cache.chmod(0o500)\n"
                "Path('/tmp/symlink').symlink_to(sys.argv[2],target_is_directory=True)\n",
                payload,
                outside,
                scratch,
            )
            self.assertEqual(
                {
                    "exit": result.returncode,
                    "scratch": list(scratch.iterdir()),
                    "directoryMode": stat.S_IMODE(outside.stat().st_mode),
                    "fileMode": stat.S_IMODE(payload.stat().st_mode),
                    "contents": payload.read_text(),
                },
                {
                    "exit": 0,
                    "scratch": [],
                    "directoryMode": 0o500,
                    "fileMode": 0o400,
                    "contents": "unchanged fixture",
                },
                result.stderr,
            )
            outside.chmod(0o700)

    def test_failure_retains_read_only_scratch_across_later_success(self):
        with fixture_root() as root:
            scratch = root / "scratch"
            scratch.mkdir()
            failed = run_wrapper(
                scratch,
                "from pathlib import Path\nimport sys\n"
                "cache=Path('/tmp/cache');cache.mkdir()\n"
                "(cache/'marker').write_text('retained')\ncache.chmod(0o500)\nsys.exit(7)\n",
            )
            retained = list(scratch.iterdir())
            self.assertEqual(failed.returncode, 7, failed.stderr)
            self.assertEqual(len(retained), 1)
            cache = retained[0] / "cache"
            later = run_wrapper(scratch, "pass")
            self.assertEqual(
                {
                    "exit": later.returncode,
                    "scratch": list(scratch.iterdir()),
                    "directoryMode": stat.S_IMODE(cache.stat().st_mode),
                    "marker": (cache / "marker").read_text(),
                },
                {
                    "exit": 0,
                    "scratch": retained,
                    "directoryMode": 0o500,
                    "marker": "retained",
                },
                later.stderr,
            )
            self.assertIn("Retained failed test scratch:", failed.stderr)
            cache.chmod(0o700)

    def test_private_tmp_root_uses_tmpfs_and_bulk_default_temp_directory(self):
        with fixture_root() as root:
            scratch = root / "scratch"
            scratch.mkdir()
            bulk_sibling = root / "bulk-sibling"
            bulk_sibling.mkdir()
            result = run_wrapper(
                scratch,
                "from pathlib import Path\nimport os,sys,tempfile\n"
                "tmp=Path('/tmp');tmpdir=Path(tempfile.gettempdir())\n"
                "backing=next(Path(sys.argv[1]).glob('invocation.*'))\n"
                "assert tmp.resolve()==tmp\n"
                "assert tmpdir==Path('/tmp/codextests')\n"
                "assert os.environ['TMPDIR']==str(tmpdir)\n"
                "assert tmp.stat().st_dev!=Path(sys.argv[2]).stat().st_dev\n"
                "assert tmpdir.samefile(backing)\n"
                "mounts=[line.split() for line in Path('/proc/self/mounts').read_text().splitlines()]\n"
                "assert any(mount[1:3]==['/tmp','tmpfs'] for mount in mounts)\n"
                "with tempfile.NamedTemporaryFile() as temporary:\n"
                " path=Path(temporary.name)\n"
                " assert path.parent==tmpdir\n"
                " assert path.samefile(backing/path.name)\n",
                scratch,
                bulk_sibling,
                private_tmp_root=True,
            )
            self.assertEqual(
                {"exit": result.returncode, "scratch": list(scratch.iterdir())},
                {"exit": 0, "scratch": []},
                result.stderr,
            )

    def test_private_tmp_root_retains_only_bulk_scratch_and_isolates_later_run(self):
        with fixture_root() as root:
            scratch = root / "scratch"
            scratch.mkdir()
            failed = run_wrapper(
                scratch,
                "from pathlib import Path\nimport sys,tempfile\n"
                "cache=Path(tempfile.gettempdir())/'cache';cache.mkdir()\n"
                "(cache/'marker').write_text('retained')\n"
                "cache.chmod(0o500)\n"
                "Path('/tmp/ephemeral').write_text('private tmpfs')\n"
                "sys.exit(7)\n",
                private_tmp_root=True,
            )
            retained = list(scratch.iterdir())
            self.assertEqual(failed.returncode, 7, failed.stderr)
            self.assertEqual(len(retained), 1)
            cache = retained[0] / "cache"
            later = run_wrapper(
                scratch,
                "from pathlib import Path\nimport tempfile\n"
                "tmpdir=Path(tempfile.gettempdir())\n"
                "assert list(Path('/tmp').iterdir())==[Path('/tmp/codextests')]\n"
                "assert list(tmpdir.iterdir())==[]\n"
                "(tmpdir/'later-marker').write_text('remove on success')\n",
                private_tmp_root=True,
            )
            self.assertEqual(
                {
                    "exit": later.returncode,
                    "scratch": list(scratch.iterdir()),
                    "retainedFiles": list(retained[0].iterdir()),
                    "directoryMode": stat.S_IMODE(cache.stat().st_mode),
                    "marker": (cache / "marker").read_text(),
                },
                {
                    "exit": 0,
                    "scratch": retained,
                    "retainedFiles": [cache],
                    "directoryMode": 0o500,
                    "marker": "retained",
                },
                later.stderr,
            )
            self.assertIn(f"Retained failed test scratch: {retained[0]}", failed.stderr)
            cache.chmod(0o700)


if __name__ == "__main__":
    unittest.main()
