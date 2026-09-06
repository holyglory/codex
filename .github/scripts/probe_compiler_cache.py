"""Exercise the pinned cache's long-request failure and its CI configuration.

Linux-only preflight: use an isolated socket and local cache, never the job's
compiler cache. Accelerate the default idle timer to one second; the server's
ten-second shutdown grace remains real. A slow wrapper still invokes real cc.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time


SHUTDOWN_WARNING = "server looks like it shut down unexpectedly"


def probe(binary: str, root: Path, idle_timeout: str, expect_shutdown: bool) -> dict:
    compiler = root / "slow-cc"
    compiler.write_text(
        "#!/bin/bash\nset -eu\ncompile=false\npreprocess=false\n"
        'for arg in "$@"; do\n'
        '  [[ "$arg" != -c ]] || compile=true\n'
        '  [[ "$arg" != -E ]] || preprocess=true\n'
        "done\nif $compile && ! $preprocess; then sleep 14; fi\n"
        f'exec {shlex.quote(shutil.which("cc"))} "$@"\n'
    )
    compiler.chmod(0o700)
    source = root / "probe.c"
    source.write_text("int answer(void) { return 42; }\n")
    output = root / "probe.o"
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("SCCACHE_")
    }
    env.update(
        SCCACHE_IDLE_TIMEOUT=idle_timeout,
        SCCACHE_SERVER_UDS=str(root / "server.sock"),
        SCCACHE_DIR=str(root / "cache"),
    )

    def run(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, *map(str, args)],
            cwd=root,
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=True,
        )

    def compile_source() -> tuple[subprocess.CompletedProcess, float, str]:
        start = time.monotonic()
        result = run(compiler, "-c", source, "-o", output)
        return (
            result,
            round(time.monotonic() - start, 2),
            hashlib.sha256(output.read_bytes()).hexdigest(),
        )

    run("--start-server")
    try:
        cold, cold_seconds, cold_hash = compile_source()
        assert (SHUTDOWN_WARNING in cold.stderr) == expect_shutdown, cold.stderr
        result = {
            "idle_timeout": idle_timeout,
            "cold_seconds": cold_seconds,
            "unexpected_shutdown": expect_shutdown,
        }
        if expect_shutdown:
            return result
        warm, warm_seconds, warm_hash = compile_source()
        assert SHUTDOWN_WARNING not in warm.stderr, warm.stderr
        assert cold_hash == warm_hash
        stats = json.loads(run("--show-stats", "--stats-format=json").stdout)["stats"]
        hits = sum(stats["cache_hits"]["counts"].values())
        assert hits >= 1, stats
        source.write_text("int answer(void) { return 43; }\n")
        changed, changed_seconds, changed_hash = compile_source()
        assert SHUTDOWN_WARNING not in changed.stderr, changed.stderr
        assert changed_hash != cold_hash
        stats = json.loads(run("--show-stats", "--stats-format=json").stdout)["stats"]
        misses = sum(stats["cache_misses"]["counts"].values())
        assert misses >= 2, stats
        result.update(
            warm_seconds=warm_seconds,
            changed_seconds=changed_seconds,
            cache_hits=hits,
            cache_misses=misses,
        )
        return result
    finally:
        stopped = subprocess.run(
            [binary, "--stop-server"],
            cwd=root,
            env=env,
            capture_output=True,
            text=True,
            timeout=20,
        )
        if not expect_shutdown:
            stopped.check_returncode()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sccache", default=shutil.which("sccache"))
    args = parser.parse_args()
    if args.sccache is None:
        # Match the composite action's supported ordinary-compiler fallback.
        print(
            "::warning::Cache unavailable; long-compilation cache probe not applicable."
        )
        return
    if shutil.which("cc") is None:
        raise SystemExit(
            "The compiler cache probe requires the preflight's C compiler."
        )
    with tempfile.TemporaryDirectory(prefix="codex-cache-probe-") as directory:
        root = Path(directory)
        cases = []
        for name, idle_timeout, expect_shutdown in [
            ("baseline", "1", True),
            ("protected", "0", False),
        ]:
            case = root / name
            case.mkdir()
            result = probe(args.sccache, case, idle_timeout, expect_shutdown)
            cases.append(result)
            print(json.dumps(result), flush=True)
        print(json.dumps({"result": "passed", "cases": cases}), flush=True)


if __name__ == "__main__":
    main()
