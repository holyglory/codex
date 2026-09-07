#!/usr/bin/env python3
"""Local-first Linux acceptance; retain build state and dispatch exact commits only."""

import argparse
from contextlib import ExitStack
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET


GATES = (
    "helpers",
    "format",
    "package-prerequisites",
    "focused",
    "schemas",
    "source-drift",
    "bazel-lock",
    "bazel-layout",
)
LANES = {
    "rust": ("clippy", "rust-tests", "linux-package"),
    "bazel": ("bazel-tests", "bazel-release"),
}
CHECKS = (*GATES, *LANES["rust"], *LANES["bazel"])


def verify_package(directory, commit):
    if (directory / "source-commit.txt").read_text().strip() != commit:
        raise ValueError("Linux packages do not belong to this commit")
    for compression in ("gzip", "zstd"):
        suites = ET.parse(directory / f"smoke-{compression}.xml").getroot()
        counts = {
            key: sum(int(suite.get(key, "0")) for suite in suites.iter("testsuite"))
            for key in ("tests", "errors", "failures", "skipped")
        }
        if (
            counts["tests"] - counts["skipped"] < 7
            or counts["errors"]
            or counts["failures"]
        ):
            raise ValueError(
                f"Linux {compression} smoke coverage failed or is incomplete"
            )


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def frozen_commit(root):
    if git(root, "status", "--porcelain", "--untracked-files=all"):
        raise ValueError(
            "Candidate must be committed and clean; preserve dirty work separately"
        )
    return git(root, "rev-parse", "HEAD")


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def commands(root, state, directory):
    python = shlex.quote(sys.executable)
    bazel = ["bash", ".github/scripts/run-bazel-ci.sh", "--"]
    cache = f"--disk_cache={state / 'bazel-actions'}"
    return {
        "helpers": (
            [
                "bash",
                "-euc",
                f"{python} .github/scripts/test_archive_release_symbols.py && {python} -m unittest discover -s codex-cli/scripts -p 'test_*.py' && {python} -m unittest discover -s scripts -p 'test_*candidate*.py' && {python} -m unittest discover -s scripts -p 'test_stage_verified_npm_release.py' && {python} -m unittest discover -s .github/scripts -p 'test_bounded_bazel_cache.py' && {python} -m unittest discover -s .github/scripts -p 'test_bazel_remote_cache.py'",
            ],
            root,
        ),
        "format": (["just", "fmt-check"], root),
        "package-prerequisites": (
            ["bash", "scripts/build_local_linux_candidate.sh", "--check"],
            root,
        ),
        "focused": ([sys.executable, "scripts/run_candidate_preflight.py"], root),
        "schemas": (
            [
                "bash",
                "-euc",
                "just write-config-schema && just write-app-server-schema && just write-app-server-schema --experimental",
            ],
            root,
        ),
        "source-drift": (
            [
                "bash",
                "-euc",
                "cargo metadata --manifest-path codex-rs/Cargo.toml --locked --format-version 1 >/dev/null; git diff --exit-code",
            ],
            root,
        ),
        "bazel-lock": (
            [
                "bash",
                "-euc",
                f'{python} .github/scripts/rusty_v8_bazel.py check-module-bazel && .github/scripts/run_bazel_with_buildbuddy.py --output_user_root="$BAZEL_OUTPUT_USER_ROOT" mod deps --lockfile_mode=error',
            ],
            root,
        ),
        "bazel-layout": (
            [
                *bazel,
                "test",
                cache,
                "--nocache_test_results",
                "--test_tmpdir=/tmp/b",
                "--test_env=RUST_TEST_THREADS=1",
                "--test_env=RUST_MIN_STACK=16777216",
                "--",
                "//codex-rs/app-server-transport:app-server-transport-unit-tests",
                "//codex-rs/external-agent-migration:external-agent-migration-unit-tests",
                "//codex-rs/utils/pty:pty-unit-tests",
            ],
            root,
        ),
        "clippy": (
            [
                "cargo",
                "clippy",
                "--locked",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
            root / "codex-rs",
        ),
        "rust-tests": (["just", "test", "--locked", "--no-tests=fail"], root),
        "linux-package": (
            [
                "bash",
                "scripts/build_local_linux_candidate.sh",
                str(directory / "linux-package"),
            ],
            root,
        ),
        "bazel-tests": (
            [
                *bazel,
                "test",
                cache,
                "--test_tmpdir=/tmp/b",
                "--keep_going",
                "--test_env=RUST_TEST_THREADS=1",
                "--test_env=RUST_MIN_STACK=16777216",
                "--test_timeout=300,600,1200,3600",
                "--flaky_test_attempts=3",
                "--test_tag_filters=-argument-comment-lint",
                "--",
                "//...",
                "-//third_party/v8:all",
                "-//codex-rs/v8-poc:v8-poc-unit-tests",
            ],
            root,
        ),
        "bazel-release": (
            [
                "bash",
                "-euc",
                'target_list=$(bash scripts/list-bazel-release-targets.sh); test -n "$target_list"; mapfile -t targets <<< "$target_list"; bash .github/scripts/run-bazel-ci.sh -- build --disk_cache="$LOCAL_BAZEL_CACHE" --compilation_mode=fastbuild --@rules_rust//rust/settings:extra_rustc_flag=-Cdebug-assertions=no --@rules_rust//rust/settings:extra_exec_rustc_flag=-Cdebug-assertions=no -- "${targets[@]}"',
            ],
            root,
        ),
    }


def run_step(name, command, cwd, environ, directory):
    started = time.monotonic()
    print(json.dumps({"check": name, "status": "running"}), flush=True)
    log = directory / f"{name}.log"
    with log.open("xb") as output:
        try:
            code = subprocess.run(
                command, cwd=cwd, env=environ, stdout=output, stderr=subprocess.STDOUT
            ).returncode
        except OSError as error:
            output.write(f"Unable to start check: {type(error).__name__}\n".encode())
            code = 127
    result = {
        "name": name,
        "exit_code": code,
        "seconds": round(time.monotonic() - started, 3),
        "log_sha256": digest(log),
    }
    print(json.dumps(result), flush=True)
    return result


def execute(plan, environ, directory, unchanged, runner=run_step):
    results = []

    def lane(names):
        values = []
        for name in names:
            command, cwd = plan[name]
            result = runner(name, command, cwd, environ, directory)
            values.append(result)
            if result["exit_code"] != 0:
                break
        return values

    for name in GATES:
        result = lane((name,))[0]
        results.append(result)
        if result["exit_code"] != 0 or not unchanged():
            return results
    # Both native engines assume they can use the host CPU. Running them together
    # caused short-deadline integration failures even when isolated cases passed.
    # Sequence full engines without imposing a second worker/capacity controller.
    # An ordinary failure still leaves the other engine available to collect findings.
    for names in LANES.values():
        results.extend(lane(names))
    return results


def validate(receipt, commit):
    if (
        not isinstance(receipt, dict)
        or type(receipt.get("schema")) is not int
        or receipt["schema"] != 1
    ):
        raise ValueError("Unsupported local acceptance receipt")
    if receipt.get("commit") != commit or receipt.get("status") != "success":
        raise ValueError(
            "Local acceptance does not cover this successful frozen commit"
        )
    if receipt.get("platform") != "linux-x86_64":
        raise ValueError("Local acceptance must cover Linux x86_64")
    checks = receipt.get("checks")
    if not isinstance(checks, list) or len(checks) != len(CHECKS):
        raise ValueError("Local acceptance is missing required checks")
    if [check.get("name") for check in checks if isinstance(check, dict)] != list(
        CHECKS
    ):
        raise ValueError("Local acceptance has missing, duplicate or unknown checks")
    for check in checks:
        checksum = check.get("log_sha256")
        if type(check.get("exit_code")) is not int or check["exit_code"] != 0:
            raise ValueError("Local acceptance includes a failed check")
        if (
            not isinstance(checksum, str)
            or len(checksum) != 64
            or any(c not in "0123456789abcdef" for c in checksum)
        ):
            raise ValueError("Local acceptance lacks a log digest")


def check(root, state, cargo_target, release_target):
    commit = frozen_commit(root)
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("Local acceptance requires Linux x86_64")
    if Path("/etc/codex/config.toml").exists():
        raise ValueError(
            "Use the isolated local test environment; host Codex configuration is present"
        )
    for tool in (
        "just",
        "cargo",
        "cargo-nextest",
        "uv",
        "dotslash",
        os.environ.get("CODEX_BAZEL_BIN", "bazel"),
    ):
        if shutil.which(tool) is None:
            raise ValueError(f"Required build tool is unavailable: {tool}")
    for variable in ("RUSTY_V8_ARCHIVE", "RUSTY_V8_SRC_BINDING_PATH"):
        if not Path(os.environ.get(variable, "")).is_file():
            raise ValueError(f"Provide the verified GNU/Linux prerequisite: {variable}")
    if any(
        path == root or path.is_relative_to(root)
        for path in (state, cargo_target, release_target)
    ):
        raise ValueError("Build storage must be outside the checkout")
    state.mkdir(parents=True, exist_ok=True)
    cargo_target.mkdir(parents=True, exist_ok=True)
    release_target.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="accept-", dir=state))
    environ = dict(os.environ)
    environ.update(
        CARGO_TARGET_DIR=str(cargo_target),
        CARGO_INCREMENTAL="0",
        CARGO_PROFILE_DEV_DEBUG="0",
        CARGO_PROFILE_TEST_DEBUG="0",
        CARGO_PROFILE_TEST_DEBUG_ASSERTIONS="true",
        BAZEL_OUTPUT_USER_ROOT=str(state / "bazel-output"),
        BAZEL_REPOSITORY_CACHE=str(state / "bazel-downloads"),
        LOCAL_BAZEL_CACHE=str(state / "bazel-actions"),
        CODEX_REPO_ROOT=str(root),
        CODEX_HOME=tempfile.mkdtemp(prefix="ch.", dir="/tmp"),
        TMPDIR="/tmp",
        PYTHONDONTWRITEBYTECODE="1",
    )
    environ["LOCAL_RELEASE_TARGET"] = str(release_target)
    print(json.dumps({"commit": commit, "logs": str(directory)}), flush=True)

    def unchanged():
        try:
            return frozen_commit(root) == commit
        except ValueError:
            return False

    # Lock the actual reusable stores: two callers cannot race on the same state.
    with ExitStack() as stack:
        for path in sorted(
            {path / "acceptance.lock" for path in (state, cargo_target, release_target)}
        ):
            lock = stack.enter_context(path.open("a"))
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        results = execute(
            commands(root, state, directory), environ, directory, unchanged
        )
        receipt = {
            "schema": 1,
            "commit": commit,
            "platform": "linux-x86_64",
            "status": "success" if unchanged() else "source-changed",
            "checks": results,
        }
        try:
            validate(receipt, commit)
        except ValueError:
            receipt["status"] = "failure"
        (directory / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(
        json.dumps(
            {"receipt": str(directory / "receipt.json"), "status": receipt["status"]}
        ),
        flush=True,
    )
    return 0 if receipt["status"] == "success" else 1


def dispatch(root, receipt_path, branch):
    commit = frozen_commit(root)
    receipt = json.loads(receipt_path.read_text())
    validate(receipt, commit)
    for entry in receipt["checks"]:
        if digest(receipt_path.parent / f"{entry['name']}.log") != entry["log_sha256"]:
            raise ValueError("Local check log changed or is unavailable")
    remote = subprocess.check_output(
        [
            "gh",
            "api",
            f"repos/holyglory/codex/git/ref/heads/{branch}",
            "--jq",
            ".object.sha",
        ],
        text=True,
    ).strip()
    if remote != commit:
        raise ValueError("Remote branch does not point to the locally accepted commit")
    running = json.loads(
        subprocess.check_output(
            [
                "gh",
                "run",
                "list",
                "--repo",
                "holyglory/codex",
                "--workflow",
                "downstream-candidate.yml",
                "--branch",
                branch,
                "--limit",
                "100",
                "--json",
                "status",
            ],
            text=True,
        )
    )
    if any(run["status"] != "completed" for run in running):
        raise ValueError(
            "A candidate is still active on this branch; preserve its evidence"
        )
    subprocess.run(
        [
            "gh",
            "workflow",
            "run",
            "downstream-candidate.yml",
            "--repo",
            "holyglory/codex",
            "--ref",
            branch,
            "--json",
        ],
        input=json.dumps({"local_acceptance": json.dumps(receipt)}),
        text=True,
        check=True,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    for action in ("plan", "check"):
        item = sub.add_parser(action)
        item.add_argument("--state-dir", type=Path, required=True)
        item.add_argument("--cargo-target-dir", type=Path)
        item.add_argument("--release-target-dir", type=Path)
    item = sub.add_parser("dispatch")
    item.add_argument("--receipt", type=Path, required=True)
    item.add_argument("--branch", required=True)
    item = sub.add_parser("verify")
    item.add_argument("--commit", required=True)
    item = sub.add_parser("verify-package")
    item.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    try:
        if args.action == "verify":
            validate(json.loads(os.environ["LOCAL_ACCEPTANCE"]), args.commit)
        elif args.action == "verify-package":
            verify_package(args.directory, frozen_commit(root))
        elif args.action == "dispatch":
            dispatch(root, args.receipt, args.branch)
        elif args.action == "plan":
            print(
                json.dumps(
                    {
                        name: {"command": command, "cwd": str(cwd)}
                        for name, (command, cwd) in commands(
                            root,
                            args.state_dir.resolve(),
                            args.state_dir.resolve() / "accept-EXAMPLE",
                        ).items()
                    },
                    indent=2,
                )
            )
        else:
            state = args.state_dir.resolve()
            return check(
                root,
                state,
                (args.cargo_target_dir or state / "cargo-target").resolve(),
                (args.release_target_dir or state / "release-target").resolve(),
            )
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Local candidate refused: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
