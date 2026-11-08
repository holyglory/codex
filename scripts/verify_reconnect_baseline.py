#!/usr/bin/env python3
"""Retain the passed product checks when repairing only a release reference path."""

import datetime
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
BASE = "79dd5d441b39347b5784e7e8e96e987a12c63618"
PRIOR_RUN = "t20261108T200058Z-e66d25"


def main():
    def git(*args):
        return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True)

    allowed = {
        "scripts/package_native_release.py",
        "scripts/verify_reconnect_baseline.py",
    }
    changed = set(git("diff", "--name-only", BASE, "HEAD").splitlines())
    if changed != allowed or git("status", "--porcelain").strip():
        raise RuntimeError("Expected only the committed release-reference repair")
    generator = "scripts/package_native_release.py"
    expected = git("show", f"{BASE}:{generator}")
    replacements = {
        '"/home/holyglory/.codex/packages/standalone/current/bin/codex"': '"/home/holyglory/.codex/packages/standalone/releases/0.162.1+multi.4-x86_64-unknown-linux-gnu-local-usage-recovery-571ba85a/bin/codex"',
        '    base_run = os.environ.get("CODEX_MULTI_BASE_VALIDATION_RUN", run_id)\n': '    base_run = os.environ.get("CODEX_MULTI_BASE_VALIDATION_RUN", run_id)\n    auth_run = os.environ.get("CODEX_MULTI_AUTH_VALIDATION_RUN", run_id)\n',
        'f"run/{run_id}/app-server-suite"': 'f"run/{auth_run}/app-server-suite"',
    }
    for before, after in replacements.items():
        if expected.count(before) != 1:
            raise RuntimeError("Release reference repair does not match its base")
        expected = expected.replace(before, after, 1)
    if (ROOT / generator).read_text() != expected:
        raise RuntimeError(
            "Native build or smoke inputs changed with the reference repair"
        )
    deadline = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(
        seconds=30
    )
    response = subprocess.run(
        [
            "devcoordinator2",
            "call",
            "test.wait",
            "--params",
            json.dumps(
                {
                    "path": str(ROOT),
                    "run_id": PRIOR_RUN,
                    "deadline_at": deadline.isoformat(),
                }
            ),
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=45,
    )
    result = json.loads(response.stdout)
    status = result["data"]["status"]
    required = {
        "source-preflight",
        "rust-lint",
        "native-test-inputs",
        "v8-test-inputs",
        "app-server-suite",
        "native-packages",
        "package-smoke",
    }
    checks = {check["name"]: check["status"] for check in status["checks"]}
    if (
        not result["ok"]
        or status["run_id"] != PRIOR_RUN
        or status["status"] != "failed"
        or status["source_changed"]
        or status["proof"] != "complete"
        or status["requested_tier"] != "release"
        or any(checks.get(name) != "passed" for name in required)
        or {name for name, state in checks.items() if state == "failed"}
        != {"account-golden"}
    ):
        raise RuntimeError(
            "Prior product proof contains a failure beyond the pinned-reference check"
        )
    source = json.loads(
        (
            Path("/mnt/build-storage/codex/rebase166-package-work")
            / PRIOR_RUN
            / "source.json"
        ).read_text()
    )
    if source != {
        "source_sha256": "9adbdda8b4e6aea7b7ffa941ed03b2d29c2726f3d373b986d67c577057f70795",
        "config_file_sha256": "1055c248a2566ccfc9ab2c22cde1a7d86f9c98a021dd4858d142fb049c62b126",
    }:
        raise RuntimeError("Prior source identity changed")
    print(
        json.dumps(
            {
                "priorRun": PRIOR_RUN,
                "productInputsUnchanged": True,
                "passedChecks": sorted(required),
                "remainingCheck": "fresh package acceptance with the immutable reference",
            }
        )
    )


if __name__ == "__main__":
    main()
