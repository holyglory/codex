#!/usr/bin/env python3
"""Qualify the exact executable served by a completed local activation.

This is a post-activation, read-only qualification step. It consumes the
owner's retained process receipt and the already completed local RPC proof;
it never starts a daemon, opens the control socket, publishes a package, or
reruns model turns.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

from local_deploy_owner import digest
from local_deploy_owner import identity
from local_deploy_owner import package_digest


TARGET = "x86_64-unknown-linux-gnu"
RUN_PATTERN = re.compile(r"t[0-9]{8}T[0-9]{6}Z-[0-9a-f]+")
WORKTREE_PATTERN = re.compile(r"w[0-9a-f]{16}")


def load_json(path, label):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"{label}-invalid") from error


def authoritative_source_sha256(root):
    executable = shutil.which("devcoordinator2-executor")
    if executable is None:
        raise RuntimeError("source-digest-helper-missing")
    result = subprocess.run(
        [executable, "source-digest", "--worktree", str(Path(root).resolve())],
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
    )
    response = json.loads(result.stdout)
    source_sha256 = response.get("sha256")
    if response.get("schema") != 2 or not re.fullmatch(r"[0-9a-f]{64}", source_sha256 or ""):
        raise RuntimeError("source-digest-invalid")
    return source_sha256


def event_window(receipt):
    events = receipt.get("events")
    if not isinstance(events, list):
        raise RuntimeError("owner-receipt-events-missing")
    verify_started = [
        event.get("atMs")
        for event in events
        if event.get("phase") == "verify-rpc" and type(event.get("atMs")) is int
    ]
    serving_verified = [
        event.get("atMs")
        for event in events
        if event.get("phase") == "serving-verified" and type(event.get("atMs")) is int
    ]
    if not verify_started or not serving_verified:
        raise RuntimeError("owner-verification-window-missing")
    start = max(verify_started)
    end = min(value for value in serving_verified if value >= start)
    return start, end


def require_sparse_proof(proof):
    if (
        proof.get("result") != "passed"
        or proof.get("realModelTurn") is not True
        or proof.get("usageSparseControlCapability") is not True
        or proof.get("sparseJourney") is not True
        or proof.get("sparseReportVerified") is not True
        or type(proof.get("sparseDeclarationsObserved")) is not int
        or proof["sparseDeclarationsObserved"] < 3
        or type(proof.get("sparseBoundOperations")) is not int
        or proof["sparseBoundOperations"] < 1
        or type(proof.get("sparseUnattributedOperations")) is not int
        or proof["sparseUnattributedOperations"] < 1
        or not isinstance(proof.get("sparseThreadId"), str)
        or not proof["sparseThreadId"]
        or not isinstance(proof.get("sparseOutcomeRows"), list)
        or len(proof["sparseOutcomeRows"]) < 2
    ):
        raise RuntimeError("sparse-rpc-proof-incomplete")
    if type(proof.get("checkedAtMs")) is not int:
        raise RuntimeError("sparse-rpc-proof-timestamp-missing")


def update_capability_inventory(completion, proof, evidence_ref):
    if not isinstance(completion, dict) or not isinstance(completion.get("capabilities"), list):
        raise RuntimeError("completion-inventory-invalid")
    sparse_ids = {
        "live-sparse-job-attribution",
        "local-usage-capability",
    }
    account_ids = {"account-routing-and-cli", "accepted-account-cli-output"}
    resume_ids = {"desktop-profile-authentication"}
    account_proof = proof.get("accountAuthStatusAgrees") is True and proof.get("accountToolRequested") is True
    resume_proof = proof.get("originalChatResumed") is True and proof.get("secondClientReattached") is True
    for capability in completion["capabilities"]:
        if not isinstance(capability, dict) or not isinstance(capability.get("id"), str):
            raise RuntimeError("completion-capability-invalid")
        capability_id = capability["id"]
        if capability_id in sparse_ids:
            capability["state"] = "real_e2e"
            capability.setdefault("evidence_refs", []).append(evidence_ref)
        elif capability_id in account_ids and account_proof:
            capability["state"] = "real_e2e"
            capability.setdefault("evidence_refs", []).append(evidence_ref)
        elif capability_id in resume_ids and resume_proof:
            capability["state"] = "real_e2e"
            capability.setdefault("evidence_refs", []).append(evidence_ref)
    return completion


def owner_evidence(receipt):
    serving = receipt["serving"]
    return {
        "result": receipt["result"],
        "packageSha256": receipt.get("packageSha256"),
        "ownershipMode": receipt.get("ownershipMode"),
        "serving": {
            key: serving.get(key)
            for key in ("pid", "uid", "startTicks", "executableSha256", "noNewPrivileges")
        },
        "events": [
            {"phase": event.get("phase"), "atMs": event.get("atMs")}
            for event in receipt.get("events", [])
            if event.get("phase") in {"verify-rpc", "serving-verified"}
        ],
    }


def rpc_evidence(proof):
    return {
        key: proof.get(key)
        for key in (
            "result",
            "checkedAtMs",
            "realModelTurn",
            "usageSparseControlCapability",
            "sparseJourney",
            "sparseReportVerified",
            "sparseDeclarationsObserved",
            "sparseThreadId",
            "sparseBoundOperations",
            "sparseUnattributedOperations",
            "sparseOutcomeRows",
            "accountAuthStatusAgrees",
            "accountToolRequested",
            "originalChatResumed",
            "secondClientReattached",
        )
    }


def qualify(args):
    package_root = Path(args.package_root).resolve(strict=True)
    package = package_root / "package"
    executable = package / "bin/codex"
    if not executable.is_file() or executable.is_symlink() or not os.access(executable, os.X_OK):
        raise RuntimeError("actual-executable-missing")
    metadata = load_json(package / "codex-package.json", "package-metadata")
    source_record = load_json(package_root / "source.json", "package-source")
    completion = load_json(package_root / "completion.json", "completion-inventory")
    owner = load_json(args.owner_receipt, "owner-receipt")
    proof = load_json(args.rpc_proof, "rpc-proof")
    if owner.get("result") != "passed" or not isinstance(owner.get("serving"), dict):
        raise RuntimeError("owner-activation-proof-incomplete")
    require_sparse_proof(proof)
    source_sha256 = authoritative_source_sha256(args.source_root)
    if source_record.get("source_sha256") != source_sha256:
        raise RuntimeError("package-source-mismatch")
    if completion.get("source_sha256") != source_sha256:
        raise RuntimeError("completion-source-mismatch")
    package_sha256 = package_digest(package)
    if owner.get("packageSha256") != package_sha256:
        raise RuntimeError("owner-package-generation-mismatch")
    executable_sha256 = digest(executable)
    serving = owner["serving"]
    process = identity(int(serving.get("pid", 0)))
    for field in ("pid", "uid", "startTicks", "executableSha256", "noNewPrivileges"):
        if process.get(field) != serving.get(field):
            raise RuntimeError("serving-process-identity-changed")
    if process["noNewPrivileges"] != 0 or process["executableSha256"] != executable_sha256:
        raise RuntimeError("serving-process-security-or-hash-mismatch")
    version = str(metadata.get("version", ""))
    if not version:
        raise RuntimeError("package-version-missing")
    version_result = subprocess.run(
        [f"/proc/{process['pid']}/exe", "--version"],
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    )
    if version not in version_result.stdout:
        raise RuntimeError("serving-process-version-mismatch")
    verify_started, serving_verified = event_window(owner)
    checked_at_ms = proof["checkedAtMs"]
    if not verify_started <= checked_at_ms <= serving_verified:
        raise RuntimeError("sparse-proof-freshness-mismatch")
    worktree_id = args.worktree_id or os.environ.get("DEVCOORDINATOR_WORKTREE_ID") or os.environ.get("CODEX_MULTI_NATIVE_WORKTREE_ID")
    run_id = args.run_id or os.environ.get("DEVCOORDINATOR_RUN_ID")
    check = args.check or os.environ.get("DEVCOORDINATOR_CHECK_NAME")
    if not worktree_id or not WORKTREE_PATTERN.fullmatch(worktree_id):
        raise RuntimeError("worktree-identity-missing")
    if not run_id or not RUN_PATTERN.fullmatch(run_id) or not check:
        raise RuntimeError("governed-check-identity-missing")
    artifact = args.artifact
    output = Path(args.output).resolve()
    if output.exists():
        raise RuntimeError("qualification-artifact-already-exists")
    (output / "package/bin").mkdir(parents=True, mode=0o700)
    shutil.copy2(executable, output / "package/bin/codex")
    if digest(output / "package/bin/codex") != executable_sha256:
        raise RuntimeError("retained-executable-changed")
    (output / "owner-receipt.json").write_text(
        json.dumps(owner_evidence(owner), indent=2) + "\n"
    )
    (output / "rpc-proof.json").write_text(
        json.dumps(rpc_evidence(proof), indent=2) + "\n"
    )
    evidence_ref = f"artifact/{artifact}/{run_id}/{check}/rpc-proof.json"
    completion = update_capability_inventory(completion, proof, evidence_ref)
    completion["claim"] = "preliminary"
    completion["source_sha256"] = source_sha256
    (output / "completion.json").write_text(json.dumps(completion, indent=2) + "\n")
    qualification = {
        "version": 1,
        "source_sha256": source_sha256,
        "package_sha256": package_sha256,
        "executable_sha256": executable_sha256,
        "process": {
            "pid": process["pid"],
            "start_ticks": process["startTicks"],
            "no_new_privileges": process["noNewPrivileges"],
            "version": version,
        },
        "rpc_proof": {
            "checked_at_ms": checked_at_ms,
            "sparse_thread_id": proof["sparseThreadId"],
            "outcome_rows": proof["sparseOutcomeRows"],
        },
        "capability_states": {
            capability["id"]: capability.get("state")
            for capability in completion["capabilities"]
            if isinstance(capability, dict) and isinstance(capability.get("id"), str)
        },
    }
    (output / "qualification.json").write_text(json.dumps(qualification, indent=2) + "\n")
    delivery = {
        "version": 1,
        "kind": "local-executable",
        "target": args.target,
        "source_sha256": source_sha256,
        "file": "package/bin/codex",
        "observed_sha256": executable_sha256,
        "checked_at_ms": time.time_ns() // 1_000_000,
        "access": f"artifact://{worktree_id}/{run_id}/{check}/{artifact}/package/bin/codex",
        "observation": "executable_smoke_passed",
    }
    (output / "delivery.json").write_text(json.dumps(delivery, indent=2) + "\n")
    if digest(output / "package/bin/codex") != delivery["observed_sha256"]:
        raise RuntimeError("delivery-executable-hash-mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-root", required=True)
    parser.add_argument("--owner-receipt", required=True)
    parser.add_argument("--rpc-proof", required=True)
    parser.add_argument("--source-root", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--artifact", default="native-linux-gnu")
    parser.add_argument("--target", default=TARGET)
    parser.add_argument("--worktree-id")
    parser.add_argument("--run-id")
    parser.add_argument("--check")
    args = parser.parse_args()
    if args.target != TARGET:
        raise SystemExit("local qualification currently supports only x86_64-unknown-linux-gnu")
    qualify(args)


if __name__ == "__main__":
    main()
