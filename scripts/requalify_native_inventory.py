#!/usr/bin/env python3
"""Correct release inventory metadata while retaining exact tested native bytes."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import time

from package_native_release import (
    ROOT,
    VERSION,
    digest,
    run,
    source_identity,
    write_json,
)

BASE = "91330bc7511d152b8df9513d089c20d47a461e71"
PRIOR_RUN = "t20261108T001658Z-4773de"
ARTIFACT = "native-linux-gnu"
MANIFEST = "dc3c3f725a957982feaee9df25240640452df5cd52d03be803fa1bcdca87a939"
SOURCE = "9e0b690f3eef6af6c5f6fffd09d9e34d9f5c2e5ae8e632148eaae24934f5529b"
PRIOR_CONFIG = "d6b4a8b84cc3768635c8a361e452b5906e33a294999f418a4ce5c6da17191e37"
CONFIG = "098fa778de0174edfddbf39bce0aa5324a48126c0ef85f2fd7f3857c37e968be"


def main():
    # This is a metadata repair, not permission to reuse binaries after code changes.
    allowed = {
        "scripts/package_native_release.py",
        "scripts/requalify_native_inventory.py",
    }
    changed = set(
        run(
            ["git", "diff", "--name-only", BASE, "HEAD"], capture=True
        ).stdout.splitlines()
    )
    if changed != allowed or run(["git", "status", "--porcelain"], capture=True).stdout:
        raise RuntimeError("Expected only the two committed release inventory files")
    generator = "scripts/package_native_release.py"
    before = run(["git", "show", f"{BASE}:{generator}"], capture=True).stdout
    marker = '            "id": "observed-retries-and-waits",\n'
    expected = before.replace(
        marker, marker + '            "task_id": "p310ae9370e9a2ffa",\n', 1
    )
    if before.count(marker) != 1 or (ROOT / generator).read_text() != expected:
        raise RuntimeError("Expected only the missing outcome link in the generator")
    config = (ROOT / ".devcoordinator.toml").read_bytes()
    boundary = b"\n[test.multi-native-inventory]\n"
    if (
        config.count(boundary) != 1
        or hashlib.sha256(config).hexdigest() != CONFIG
        or hashlib.sha256(config.split(boundary)[0]).hexdigest() != PRIOR_CONFIG
    ):
        raise RuntimeError(
            "Expected only the reviewed metadata check appended to prior config"
        )
    prior = json.loads(
        run(
            [
                "devcoordinator2",
                "--client",
                "codex",
                "call",
                "test.artifact.catalog",
                "--params",
                json.dumps(
                    {
                        "path": str(ROOT),
                        "run_id": PRIOR_RUN,
                        "check": "qualify-native",
                        "artifact": ARTIFACT,
                        "manifest_sha256": MANIFEST,
                        "limit": 100,
                    }
                ),
            ],
            capture=True,
        ).stdout
    )
    proof = prior["data"]
    if (
        not prior["ok"]
        or not proof["run_complete"]
        or proof["run_status"] != "passed"
        or proof["proof"] != "complete"
        or not proof["readiness_eligible"]
        or proof["source_sha256"] != SOURCE
        or proof["manifest_sha256"] != MANIFEST
        or proof["config_sha256"] != PRIOR_CONFIG
        or proof["next_offset"] is not None
    ):
        raise RuntimeError("The original complete release proof does not match")
    original = ROOT / "codex-rs/target/native-release-delivery"
    output = ROOT / "codex-rs/target/native-release-requalified"
    output.mkdir(exist_ok=False)
    for entry in proof["entries"]:
        relative = Path(entry["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise RuntimeError("Invalid retained artifact path")
        source = original / relative
        if (
            any(path.is_symlink() for path in [source, *source.parents])
            or source.stat().st_size != entry["size"]
            or digest(source) != entry["sha256"]
        ):
            raise RuntimeError("Original package bytes changed")
        target = output / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        if digest(target) != entry["sha256"]:
            raise RuntimeError("Copied package bytes changed")
    identity = source_identity()
    completion = json.loads((output / "completion.json").read_text())
    capability = next(
        c for c in completion["capabilities"] if c["id"] == "observed-retries-and-waits"
    )
    capability["task_id"] = "p310ae9370e9a2ffa"
    completion["source_sha256"] = identity["source_sha256"]
    validation = json.loads(
        run(
            [
                "devcoordinator2",
                "--client",
                "codex",
                "call",
                "completion.check",
                "--params",
                json.dumps({"path": str(ROOT), "manifest": completion}),
            ],
            capture=True,
        ).stdout
    )
    if not validation["ok"] or not validation["data"]["valid"]:
        raise RuntimeError("Corrected capability inventory is still incomplete")
    write_json(output / "completion.json", completion)
    shutil.copy2(output / "source.json", output / "validation-source.json")
    write_json(output / "source.json", identity)
    write_json(
        output / "validation-provenance.json",
        {
            "sourceCommit": BASE,
            "sourceSha256": SOURCE,
            "runId": PRIOR_RUN,
            "configSha256": PRIOR_CONFIG,
            "metadataConfigSha256": CONFIG,
            "manifestSha256": MANIFEST,
            "artifactSha256": proof["artifact"]["sha256"],
            "metadataOnlyFiles": sorted(changed),
            "originalFiles": proof["entries"],
            "currentObservation": "Exact retained CLI --version only; full package smoke and account contracts belong to the original run.",
        },
    )
    executable = output / "package/bin/codex"
    if run([executable, "--version"], capture=True).stdout != f"codex-cli {VERSION}\n":
        raise RuntimeError("Retained package version changed")
    delivery = json.loads((output / "delivery.json").read_text())
    delivery["source_sha256"] = identity["source_sha256"]
    delivery["observed_sha256"] = digest(executable)
    delivery["checked_at_ms"] = time.time_ns() // 1_000_000
    delivery["access"] = (
        f"artifact://wdcb50e621f3f9f8c/{os.environ['DEVCOORDINATOR_RUN_ID']}/"
        f"{os.environ['DEVCOORDINATOR_CHECK_NAME']}/{ARTIFACT}/package/bin/codex"
    )
    write_json(output / "delivery.json", delivery)
    if source_identity() != identity:
        raise RuntimeError("Source changed during metadata qualification")


if __name__ == "__main__":
    main()
