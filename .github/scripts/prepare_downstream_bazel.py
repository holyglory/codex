"""Prepare the disposable Linux runner used by downstream Bazel validation."""

import os
from pathlib import Path
import subprocess
import tempfile


def prepare(environ: dict[str, str]) -> None:
    if (
        environ.get("GITHUB_ACTIONS") != "true"
        or environ.get("RUNNER_ENVIRONMENT") != "github-hosted"
        or environ.get("RUNNER_OS") != "Linux"
    ):
        raise RuntimeError("Bazel runner cleanup requires a GitHub-hosted Linux job")

    workspace = environ["GITHUB_WORKSPACE"]
    environment_file = Path(environ["GITHUB_ENV"])
    subprocess.run(["df", "-h", "--", workspace], check=True)
    # These preinstalled SDKs are not used by this Rust/Bazel job. Never run
    # this cleanup on the development host or a persistent self-hosted runner.
    subprocess.run(
        [
            "sudo",
            "rm",
            "-rf",
            "--",
            "/usr/local/lib/android",
            "/usr/share/dotnet",
            "/opt/ghc",
            "/opt/hostedtoolcache/CodeQL",
        ],
        check=True,
    )
    subprocess.run(["df", "-h", "--", workspace], check=True)
    # Bazel appends a per-test hash. Its default sandbox path makes Unix
    # sockets exceed SUN_LEN and causes otherwise short fixture paths to wrap.
    test_tmpdir = tempfile.mkdtemp(prefix="b.", dir="/tmp")
    with environment_file.open("a") as output:
        output.write(f"CODEX_BAZEL_TEST_TMPDIR={test_tmpdir}\n")


if __name__ == "__main__":
    prepare(dict(os.environ))
