"""Run GNU release tests with a verified source-built V8 pair after host setup."""

from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

import build_multi2_local_package as package
from release_identity import v8_input_manifest


@contextmanager
def preparation_lock(path: Path):
    import fcntl

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def prepare_test_v8(environment: dict[str, str], version: str) -> dict:
    inputs = v8_input_manifest(package.git("rev-parse", "HEAD"))
    if package.git("status", "--porcelain", "--", *inputs["paths"]):
        raise RuntimeError("V8 source inputs must match the committed producer inputs")
    environment.pop("V8_FROM_SOURCE", None)
    selected = environment.get("V8_HANDOFF_DIR")
    if selected:
        handoff = Path(selected)
        if not handoff.is_absolute():
            raise RuntimeError(
                "V8_HANDOFF_DIR must be an absolute release handoff path"
            )
        with preparation_lock(handoff.parent / f".{handoff.name}.prepare.lock"):
            return package.prepare_v8(environment, version)

    target = Path(environment.get("CARGO_TARGET_DIR", package.ROOT / "codex-rs/target"))
    cache = target.resolve() / "multi2-test-v8" / package.TARGET / inputs["sha256"]
    current = cache / "current"
    with preparation_lock(cache / ".prepare.lock"):
        if current.is_symlink() and current.resolve().is_dir():
            candidate = {**environment, "V8_HANDOFF_DIR": str(current.resolve())}
            try:
                proof = package.prepare_v8(candidate, version)
            except (
                OSError,
                ValueError,
                KeyError,
                RuntimeError,
                subprocess.CalledProcessError,
            ):
                print(
                    f"Retained invalid V8 test handoff: {current.resolve()}",
                    file=sys.stderr,
                )
            else:
                environment.update(candidate)
                return proof
        elif current.exists() and not current.is_symlink():
            raise RuntimeError("preserve unexpected V8 cache marker")

        # Published generations keep their absolute paths while other tests use
        # them. Failed or invalid generations remain available for diagnosis.
        handoff = cache / f"handoff-{uuid.uuid4().hex}"
        print(f"Preparing source-built GNU V8 test handoff: {handoff}", flush=True)
        candidate = {**environment, "V8_HANDOFF_DIR": str(handoff)}
        proof = package.prepare_v8(candidate, version)
        link = cache / f".current-{uuid.uuid4().hex}"
        link.symlink_to(handoff.name, target_is_directory=True)
        link.replace(current)
        environment.update(candidate)
        return proof


def main(arguments: list[str] | None = None) -> None:
    environment = dict(os.environ)
    proof = prepare_test_v8(environment, package.source_version())
    print(
        json.dumps(
            {
                "v8_handoff": proof["handoff_dir"],
                "v8_receipt_sha256": proof["receipt_sha256"],
            }
        ),
        flush=True,
    )
    subprocess.run(
        ["just", "test", *(sys.argv[1:] if arguments is None else arguments)],
        cwd=package.ROOT,
        env=environment,
        check=True,
    )
    if package.verify_v8_pair(environment)["receipt_sha256"] != proof["receipt_sha256"]:
        raise RuntimeError("V8 pair receipt changed during the test run")


if __name__ == "__main__":
    main()
