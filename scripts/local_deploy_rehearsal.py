#!/usr/bin/env python3
"""Exercise the reviewed owner through actual Coordinator process deployments."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

from local_deploy_owner import digest, identity, package_digest, peer, save
from local_deploy_rpc import report_results


ROOT = Path(__file__).resolve().parent.parent
EVIDENCE_ROOT = ROOT / "codex-rs/target/local-owner-rehearsal"
RUN_ID = os.environ.get("DEVCOORDINATOR_RUN_ID")
if RUN_ID is None and (EVIDENCE_ROOT / "current").is_symlink():
    EVIDENCE = (EVIDENCE_ROOT / "current").resolve(strict=True)
    RUN_ID = EVIDENCE.name
else:
    RUN_ID = RUN_ID or f"manual-{os.getpid()}"
    EVIDENCE = EVIDENCE_ROOT / "runs" / RUN_ID
CASES = (
    "cold",
    "unmanaged",
    "forced",
    "force-mismatch",
    "force-unpinned",
    "desktop",
    "mismatch",
    "childdeath",
)
CANDIDATE = json.loads(Path(os.environ["CODEX_MULTI_LOCAL_CANDIDATE"]).read_text())
PACKAGE = Path(CANDIDATE["package"])
OLD = Path(CANDIDATE["oldBinary"])
EXE_SHA = CANDIDATE["executableSha256"]
OLD_SHA = CANDIDATE["oldExecutableSha256"]
ARTIFACT_TREE_SHA = CANDIDATE["artifactTreeSha256"]


def coordinator(operation, params):
    result = subprocess.run(
        ["devcoordinator2", "call", operation, "--params", json.dumps(params)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=180,
        check=False,
    )
    response = next(
        json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")
    )
    if not response.get("ok", False):
        save(EVIDENCE / f"coordinator-{operation}.json", response)
        raise RuntimeError(
            f"coordinator-{operation}-{response.get('error', {}).get('code')}"
        )
    return response["data"]


def prepare():
    if EVIDENCE.exists():
        raise RuntimeError("preserve-previous-rehearsal-evidence-use-fresh-worktree")
    if digest(PACKAGE / "bin/codex") != EXE_SHA or digest(OLD) != OLD_SHA:
        raise RuntimeError("qualified-or-reference-binary-changed")
    Path("/mnt/build-storage/codex/ld").mkdir(exist_ok=True, mode=0o700)
    EVIDENCE.mkdir(parents=True, mode=0o700)
    EVIDENCE_ROOT.mkdir(parents=True, exist_ok=True, mode=0o700)
    pointer = EVIDENCE_ROOT / "current.next"
    pointer.symlink_to(EVIDENCE, target_is_directory=True)
    pointer.replace(EVIDENCE_ROOT / "current")
    disposable = Path(tempfile.mkdtemp(prefix="r", dir="/mnt/build-storage/codex/ld"))
    tree = package_digest(PACKAGE)
    for case in CASES:
        state = EVIDENCE / case
        task_home = disposable / case / "u"
        task_home.mkdir(parents=True, mode=0o700)
        codex_home = disposable / case / "c"
        codex_home.mkdir(mode=0o700)
        preserved = {
            "config.toml": "approval_policy = 'never'\n",
            "auth.json": '{"credential":"redacted-canary","retain":true}\n',
            "registry.json": '{"accounts":["holygloryTT","axel"],"retain":true}\n',
            "sessions/session-canary.json": '{"session":"redacted-canary","retain":true}\n',
        }
        for relative, contents in preserved.items():
            target = codex_home / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(contents)
        save(
            codex_home / "preservation.json",
            {relative: digest(codex_home / relative) for relative in preserved},
        )
        plan = {
            "package": str(PACKAGE),
            "packageSha256": tree,
            "executableSha256": EXE_SHA,
            "version": "0.166.0+multi.1",
            "artifactTreeSha256": ARTIFACT_TREE_SHA,
            "state": str(state),
            "codexHome": str(codex_home),
            "ownerHome": str(task_home),
            "ownerUid": os.getuid(),
            "verificationCwd": str(ROOT),
            "verification": "local-rpc",
            "restartPolicy": "single-graceful-matching-unmanaged",
            "drainSeconds": 60,
            "oldBinary": str(OLD.resolve()),
            "oldExecutableSha256": OLD_SHA,
            "preservation": {
                relative: digest(codex_home / relative) for relative in preserved
            },
        }
        if case in ("unmanaged", "forced", "desktop", "mismatch"):
            plan["fixture"] = case
        if case in ("forced", "force-mismatch", "force-unpinned"):
            plan["restartPolicy"] = "force-matching-unmanaged"
        if case == "force-unpinned":
            plan["captureOldIdentity"] = True
        if case == "mismatch":
            plan["mismatchBinary"] = str(OLD.resolve())
        save(state / "plan.json", plan)
    save(
        EVIDENCE / "source.json",
        {
            "executableSha256": EXE_SHA,
            "artifactTreeSha256": ARTIFACT_TREE_SHA,
            "packageSha256": tree,
            "qualifiedRun": CANDIDATE["qualifiedRun"],
            "cases": CASES,
        },
    )


def request():
    save(EVIDENCE / "requester.json", identity(os.getpid()))
    # Refusals deliberately exit immediately. Exercise them as finite commands,
    # keeping the old peer outside the installation owner's process lifecycle.
    for case in ("force-unpinned", "force-mismatch"):
        state = EVIDENCE / case
        plan = json.loads((state / "plan.json").read_text())
        endpoint = (
            Path(plan["codexHome"]) / "app-server-control/app-server-control.sock"
        )
        reference = None
        try:
            with (state / "reference.stderr").open("wb") as log:
                if case == "force-mismatch":
                    reference = subprocess.Popen(
                        [str(OLD), "app-server", "--listen", "unix://"],
                        env={
                            **os.environ,
                            "HOME": plan["ownerHome"],
                            "CODEX_HOME": plan["codexHome"],
                        },
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=log,
                    )
                    deadline = time.monotonic() + 30
                    observed = peer(endpoint)
                    while observed is None and time.monotonic() < deadline:
                        if reference.poll() is not None:
                            raise RuntimeError("refusal-reference-exited")
                        time.sleep(0.1)
                        observed = peer(endpoint)
                    if observed is None or observed["pid"] != reference.pid:
                        raise RuntimeError("refusal-reference-not-ready")
                    plan["old"] = {**observed, "executableSha256": "0" * 64}
                    save(state / "plan.json", plan)
                with (
                    (state / "request.stdout").open("wb") as out,
                    (state / "request.stderr").open("wb") as err,
                ):
                    refused = subprocess.run(
                        [
                            sys.executable,
                            str(ROOT / "scripts/local_deploy_owner.py"),
                            str(state / "plan.json"),
                        ],
                        stdout=out,
                        stderr=err,
                        check=False,
                        timeout=60,
                    )
                assert refused.returncode == 1
                if reference is not None:
                    assert reference.poll() is None and peer(endpoint) == observed
                else:
                    assert peer(endpoint) is None
                save(
                    state / "refusal-observed.json",
                    {
                        "exitCode": refused.returncode,
                        "oldPeerUnchanged": reference is not None,
                    },
                )
        finally:
            if reference is not None:
                reference.terminate()
                reference.wait(timeout=30)
    reference = {"path": str(ROOT), "name": "local-owner-rehearsal"}
    preflight = coordinator("deployment.preflight", reference)
    save(EVIDENCE / "preflight.json", preflight)
    if not preflight["ready"]:
        raise RuntimeError("rehearsal-deployment-preflight-blocked")
    response = coordinator("deployment.apply", reference)
    save(EVIDENCE / "deployment.json", response)
    # Returning ends this finite governed check; the actual owners must survive it.


def wait_receipt(case, terminal):
    deadline = time.monotonic() + 150
    target = EVIDENCE / case / "receipt.json"
    while time.monotonic() < deadline:
        if target.exists():
            value = json.loads(target.read_text())
            if value["phase"] in terminal:
                return value
        time.sleep(0.2)
    raise RuntimeError(f"{case}-receipt-deadline")


def verify():
    requester = json.loads((EVIDENCE / "requester.json").read_text())
    try:
        current = identity(requester["pid"])
    except FileNotFoundError:
        current = None
    if current and current["startTicks"] == requester["startTicks"]:
        raise RuntimeError("requester-has-not-ended")
    proofs = []
    for case in CASES:
        expected_failure = case in {"mismatch", "force-mismatch", "force-unpinned"}
        receipt = wait_receipt(
            case,
            {"failed", "empty-cli-selection-restored"}
            if expected_failure
            else {
                "serving-verified",
                "verification-failed",
                "failed",
                "empty-cli-selection-restored",
            },
        )
        plan = json.loads((EVIDENCE / case / "plan.json").read_text())
        for relative, expected_hash in plan["preservation"].items():
            assert digest(Path(plan["codexHome"]) / relative) == expected_hash
        if expected_failure:
            assert receipt["result"] == "failed"
            reason = {
                "mismatch": "replacement-endpoint-identity-mismatch",
                "force-mismatch": "old-endpoint-identity-mismatch",
                "force-unpinned": "forced-stop-requires-explicit-old-identity",
            }[case]
            assert any(event.get("reason") == reason for event in receipt["events"])
            if case in {"force-mismatch", "force-unpinned"}:
                assert not any(
                    "stop-requested" in event["phase"] for event in receipt["events"]
                )
            if case == "force-unpinned":
                assert (
                    peer(
                        Path(plan["codexHome"])
                        / "app-server-control/app-server-control.sock"
                    )
                    is None
                )
            if case == "force-mismatch":
                assert json.loads(
                    (EVIDENCE / case / "refusal-observed.json").read_text()
                ) == {"exitCode": 1, "oldPeerUnchanged": True}
            assert not any(
                event["phase"].startswith("canonical-") for event in receipt["events"]
            )
            proofs.append({"case": case, "mismatchingPeerRefused": True})
            continue
        assert receipt["result"] == "passed", f"{case}-owner-failed"
        assert identity(receipt["owner"]["pid"]) == receipt["owner"]
        assert receipt["owner"]["cgroup"] != requester["cgroup"]
        assert (
            peer(Path(plan["codexHome"]) / "app-server-control/app-server-control.sock")
            == receipt["serving"]
        )
        assert receipt["ownershipMode"] == (
            "desktop-owned" if case == "desktop" else "managed"
        )
        rpc = json.loads((EVIDENCE / case / "rpc.json").read_text())
        assert rpc["result"] == "passed"
        if case == "forced":
            stops = [
                event
                for event in receipt["events"]
                if "stop-requested" in event["phase"]
            ]
            assert len(stops) == 1 and stops[0]["phase"] == "forced-stop-requested"
            assert stops[0]["old"]["executableSha256"] == OLD_SHA
        save(EVIDENCE / case / "survived-requester.json", receipt)
        if case == "childdeath":
            descriptor = os.pidfd_open(receipt["serving"]["pid"])
            signal.pidfd_send_signal(descriptor, signal.SIGTERM)
            os.close(descriptor)
            failed = wait_receipt(case, {"failed", "empty-cli-selection-restored"})
            assert failed["result"] == "failed"
            assert any(
                event.get("reason") == "serving-child-exited"
                for event in failed["events"]
            )
        proofs.append(
            {
                "case": case,
                "survivedRequester": True,
                "rpcPassed": True,
                "ownershipMode": receipt["ownershipMode"],
                "childDeathDetected": case == "childdeath",
            }
        )
    save(
        EVIDENCE / "verification.json",
        {
            "result": "passed",
            "proofs": proofs,
            "liveActivation": False,
            "realModelTurn": False,
        },
    )


def cleanup():
    saved = EVIDENCE / "deployment.json"
    if saved.exists():
        deployment = json.loads(saved.read_text())
        result = coordinator(
            "deployment.stop", {"deployment_id": deployment["deployment_id"]}
        )
        save(EVIDENCE / "cleanup.json", result)


def verify_reports():
    account = {
        "generation": 4,
        "accounts": [],
        "autoSelectionEnabled": True,
        "priorityOrder": "higherFirst",
    }
    usage = {
        "schemaVersion": 1,
        "kind": "usageSummary",
        "snapshotGeneration": 2,
        "pendingCorrections": 0,
        "freshness": "fresh",
        "watermarkAtMs": 123,
        "counts": {"operations": 1},
    }
    assert report_results(json.dumps(account)) == {"account_management"}
    assert report_results(json.dumps(usage)) == {"usage_stats"}
    wrapped = {
        "account_management": account,
        "usage_stats": {"content": [{"type": "text", "text": json.dumps(usage)}]},
    }
    assert report_results("Script completed\n" + json.dumps(wrapped)) == {
        "account_management",
        "usage_stats",
    }
    assert report_results({"isError": True, "content": [wrapped]}) == set()
    assert report_results("LOCAL_DEPLOY_VERIFIED") == set()
    assert report_results({"account_management": True, "usage_stats": True}) == set()
    assert report_results({**usage, "snapshotGeneration": "2"}) == set()
    print(json.dumps({"result": "passed", "reportShapeChecks": 7}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "step", choices=("prepare", "request", "verify", "cleanup", "reports")
    )
    arguments = parser.parse_args()
    {
        "prepare": prepare,
        "request": request,
        "verify": verify,
        "cleanup": cleanup,
        "reports": verify_reports,
    }[arguments.step]()
