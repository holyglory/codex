"""Compare current credit output with the qualified, immutable installed capture."""

import argparse
import hashlib
import json
from pathlib import Path

from .credits import MAX_ARTIFACT_BYTES, MAX_RESULT_BYTES, REFERENCE_SHA256
from .fixtures import CREDIT_USAGE_PAYLOADS
from .matrix import Case
from .runner import digest, run_case, source_contract


CAPTURE_SHA256 = "87b9c2061f909e553efaa201b09f478d5324037d54ffc68c14f009d110fae19b"
CAPTURE_RECEIPT = (
    "delivery-b4470eb12856a963e21a8a516f333f4cba02c0b328bcc441634b97a27f8105b2"
)
CAPTURE_MANIFEST = "7605d27a393d38c6d57f4b633debc62cf5f0851dd090c7dd6eb97bc5100cc87d"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.source_root.resolve(strict=True)
    reference = args.reference.resolve(strict=True)
    require(
        reference.stat().st_size <= MAX_ARTIFACT_BYTES, "capture exceeds size bound"
    )
    require(digest(reference) == CAPTURE_SHA256, "qualified credit capture changed")
    captured = json.loads(reference.read_text())
    require(
        captured["schemaVersion"] == 1
        and captured["kind"] == "account-credit-installed-comparison"
        and captured["matchesInstalled"] is True
        and captured["identityFailures"] == []
        and captured["binaries"]["installed"]["sha256"] == REFERENCE_SHA256,
        "capture does not establish the installed reference",
    )
    harness = {
        name: digest(root / "scripts/account_cli_compat" / name)
        for name in captured["harnessContract"]
    }
    require(harness == captured["harnessContract"], "captured fixture harness changed")
    cases = [
        Case(name, ("list",), fixture="single", backend=name)
        for name in CREDIT_USAGE_PAYLOADS
    ]
    descriptions = [case.description() for case in cases]
    # Python tuple arguments become arrays in the captured JSON declarations.
    # Actual command outputs and observation rows are compared unchanged below.
    require(
        captured["cases"] == json.loads(json.dumps(descriptions))
        and captured["fixtureInputs"] == json.loads(json.dumps(CREDIT_USAGE_PAYLOADS)),
        "captured credit case definitions changed",
    )
    matrix = hashlib.sha256(
        json.dumps(
            {"cases": descriptions, "inputs": CREDIT_USAGE_PAYLOADS}, sort_keys=True
        ).encode()
    ).hexdigest()
    require(matrix == captured["matrixDigest"], "captured credit matrix changed")
    originals = {row["case"]: row for row in captured["rows"]}
    require(
        len(originals) == len(captured["rows"]) == len(cases) * 2
        and all(
            not row["failures"] and not row["differences"] for row in originals.values()
        ),
        "captured credit rows are incomplete or failed",
    )
    binary = args.binary.resolve(strict=True)
    binary_sha = digest(binary)
    source = source_contract(root)
    require(not args.output.exists(), "preserve existing comparison evidence")
    rows = []
    for case in cases:
        for style in ("human", "json"):
            name = case.name + "." + style
            expected = originals[name]["installed"]
            require(
                expected["exitCode"] == 0
                and not expected["stderr"]
                and not expected["normalizations"],
                "captured credit row was not a clean unnormalized result",
            )
            observed = run_case(binary, case, style, validate_contract=False)
            require(
                len(json.dumps(observed, ensure_ascii=False).encode())
                <= MAX_RESULT_BYTES,
                "candidate row exceeds the evidence bound",
            )
            differences = [
                key
                for key in sorted(expected.keys() | observed.keys())
                if expected.get(key) != observed.get(key)
            ]
            rows.append(
                {"case": name, "differences": differences, "candidate": observed}
            )
    require(digest(binary) == binary_sha, "candidate changed during comparison")
    require(digest(reference) == CAPTURE_SHA256, "reference changed during comparison")
    require(
        source_contract(root) == source, "candidate source changed during comparison"
    )
    require(
        {name: digest(root / "scripts/account_cli_compat" / name) for name in harness}
        == harness,
        "fixture harness changed during comparison",
    )
    failed = [row["case"] for row in rows if row["differences"]]
    result = {
        "schemaVersion": 1,
        "kind": "account-credit-captured-comparison",
        "reference": {
            "mode": "historicalCapture",
            "executedThisRun": False,
            "path": str(reference),
            "sha256": CAPTURE_SHA256,
            "binarySha256": REFERENCE_SHA256,
            "qualifiedReceipt": CAPTURE_RECEIPT,
            "retainedRun": "t20261108T224855Z-959592",
            "retainedCheck": "final-source-installed-acceptance",
            "retainedArtifact": "native-linux-gnu",
            "manifestSha256": CAPTURE_MANIFEST,
        },
        "candidate": {
            "path": str(binary),
            "sha256": binary_sha,
            "executedThisRun": True,
        },
        "sourceContract": source,
        "harnessContract": harness,
        "matrixDigest": matrix,
        "matchesCapturedReference": not failed,
        "rows": rows,
    }
    encoded = (
        json.dumps(result, ensure_ascii=False, separators=(",", ":")) + "\n"
    ).encode()
    require(len(encoded) <= MAX_ARTIFACT_BYTES, "comparison exceeds the evidence bound")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("xb") as output:
        output.write(encoded)
    args.output.chmod(0o600)
    print(
        json.dumps(
            {"rows": len(rows), "passed": len(rows) - len(failed), "failed": failed}
        )
    )
    return int(bool(failed))


if __name__ == "__main__":
    raise SystemExit(main())
