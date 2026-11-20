"""Prepare an identity-bound plan for the governed local 0.170.0+multi.2 activation."""

import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
PACKAGE_ROOT = ROOT / "codex-rs/target/multi2-local"
SOCKET = Path("/home/holyglory/.codex/app-server-control/app-server-control.sock")
STATE = Path("/mnt/build-storage/codex/state/local-multi2-live")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


sys.path.insert(0, str(ROOT / "scripts"))
from local_deploy_owner import peer


proof = json.loads((PACKAGE_ROOT / "package-proof.json").read_text())
if proof["version"] != "0.170.0+multi.2":
    raise RuntimeError("package proof is not 0.170.0+multi.2")
commit = subprocess.check_output(
    ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
).strip()
if commit != proof["source_commit"] or re.fullmatch(r"[0-9a-f]{40}", commit) is None:
    raise RuntimeError("package proof source changed")
if STATE.exists():
    raise RuntimeError("preserve prior activation evidence")
old = peer(SOCKET)
if old["uid"] != 1000 or not old["managed"] or old["noNewPrivileges"] != 0:
    raise RuntimeError("serving peer is not the verified managed owner")
plan = {
    "package": proof["package_dir"],
    "packageSha256": proof["package_sha256"],
    "executableSha256": proof["executable_sha256"],
    "sourceCommit": commit,
    "version": proof["version"],
    "state": str(STATE),
    "codexHome": "/home/holyglory/.codex",
    "ownerHome": "/home/holyglory",
    "ownerUid": 1000,
    "verificationCwd": str(ROOT),
    "verification": "rpc-only",
    "requireSparseBindings": False,
    "restartPolicy": "canonical-matching-managed",
    "drainSeconds": 30,
    "old": old,
}
output = ROOT / "codex-rs/target/multi2-local-plan.json"
if output.exists():
    raise RuntimeError("preserve prior activation plan")
output.write_text(json.dumps(plan, indent=2) + "\n")
output.chmod(0o600)
print(json.dumps({"commit": commit, "old": old, "plan": str(output)}))
