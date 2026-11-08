"""Compare synthetic credit display exactly with the pinned installed binary."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

from .fixtures import CREDIT_USAGE_PAYLOADS
from .matrix import Case
from .runner import digest, run_case, source_contract

REFERENCE_SHA256 = "be7b16717abc136361ee269c3252d0ad05de1bf724203d016ec164629a71edc5"
MAX_RESULT_BYTES = 64 * 1024
MAX_ARTIFACT_BYTES = 4 * 1024 * 1024


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--reference-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-root", type=Path, default=Path(__file__).resolve().parents[2])
    options = parser.parse_args()
    if options.output.exists():
        parser.error("output already exists; evidence is immutable")
    binaries = {
        "installed": options.reference_binary.resolve(strict=True),
        "candidate": options.binary.resolve(strict=True),
    }
    identities = {
        role: {"path": str(binary), "sha256": digest(binary)}
        for role, binary in binaries.items()
    }
    if identities["installed"]["sha256"] != REFERENCE_SHA256:
        parser.error("reference binary does not match the pinned installed SHA-256")
    source_root = options.source_root.resolve(strict=True)
    source = source_contract(source_root)
    harness_paths = [Path(__file__).with_name(name) for name in (
        "credits.py", "fixtures.py", "matrix.py", "runner.py", "contract.py",
    )]
    harness = {path.name: digest(path) for path in harness_paths}
    definitions = [
        Case(name, ("list",), fixture="single", backend=name)
        for name in CREDIT_USAGE_PAYLOADS
    ]
    descriptions = [case.description() for case in definitions]
    matrix_digest = hashlib.sha256(json.dumps(
        {"cases": descriptions, "inputs": CREDIT_USAGE_PAYLOADS}, sort_keys=True,
    ).encode()).hexdigest()
    rows = []
    for case in definitions:
        for style in ("human", "json"):
            results, failures = {}, []
            for role, binary in binaries.items():
                try:
                    result = run_case(binary, case, style, validate_contract=False)
                    if result["exitCode"] != 0 or result["stderr"]:
                        failures.append(f"{role}: fixture command did not succeed cleanly")
                    if result["registryBefore"] != result["registryAfter"]:
                        failures.append(f"{role}: reading credits changed the registry")
                    if result["unexpectedRequests"] or result["sourceFailures"]:
                        failures.append(f"{role}: fixture or protected-file check failed")
                    if result["normalizations"]:
                        failures.append(f"{role}: deterministic credit rows were normalized")
                    if not any(
                        request["method"] == "GET"
                        and request["path"] == "/backend-api/wham/usage"
                        for request in result["backendRequests"]
                    ):
                        failures.append(f"{role}: synthetic usage response was not requested")
                    if style == "json" and result["exitCode"] == 0:
                        accounts = json.loads(result["stdout"])["accounts"]
                        if len(accounts) != 1 or accounts[0]["limits"]["state"] != "observed":
                            failures.append(f"{role}: synthetic credit fixture was not observed")
                    encoded = json.dumps(result, ensure_ascii=False).encode()
                    if len(encoded) > MAX_RESULT_BYTES:
                        failures.append(f"{role}: captured row exceeded the evidence bound")
                        result = {
                            "captureError": "row exceeded the evidence bound",
                            "bytes": len(encoded),
                            "sha256": hashlib.sha256(encoded).hexdigest(),
                        }
                except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
                    failures.append(f"{role}: capture failed ({type(error).__name__})")
                    result = {"captureError": type(error).__name__, "message": str(error)[:512]}
                results[role] = result
            differences = [
                key
                for key in sorted(results["installed"].keys() | results["candidate"].keys())
                if results["installed"].get(key) != results["candidate"].get(key)
            ]
            if differences:
                failures.append("installed comparison differs in: " + ", ".join(differences))
            rows.append({
                "case": case.name + "." + style,
                "differences": differences,
                "failures": failures,
                **results,
            })
    identity_failures = []
    for role, binary in binaries.items():
        if digest(binary) != identities[role]["sha256"]:
            identity_failures.append(f"{role} binary changed while running")
    if source_contract(source_root) != source:
        identity_failures.append("candidate source changed while running")
    if {path.name: digest(path) for path in harness_paths} != harness:
        identity_failures.append("credit fixture harness changed while running")
    failed = [row["case"] for row in rows if row["failures"]]
    artifact = {
        "schemaVersion": 1,
        "kind": "account-credit-installed-comparison",
        "binaries": identities,
        "sourceContract": source,
        "harnessContract": harness,
        "matrixDigest": matrix_digest,
        "cases": descriptions,
        "fixtureInputs": CREDIT_USAGE_PAYLOADS,
        "auxiliaryCreditProbe": "Unmodeled auxiliary wire credits; installed behavior is authoritative.",
        "matchesInstalled": not failed and not identity_failures,
        "identityFailures": identity_failures,
        "rows": rows,
    }
    encoded = json.dumps(artifact, ensure_ascii=False, separators=(",", ":")).encode() + b"\n"
    if len(encoded) > MAX_ARTIFACT_BYTES:
        raise ValueError("credit comparison artifact exceeded its evidence bound")
    with options.output.open("xb") as output:
        output.write(encoded)
    print(json.dumps({
        "rows": len(rows),
        "passed": len(rows) - len(failed),
        "failed": failed,
        "identityFailures": identity_failures,
        "artifact": str(options.output),
    }))
    return int(bool(failed or identity_failures))


if __name__ == "__main__":
    raise SystemExit(main())
