"""Exact-output golden capture. Binaries run only in newly seeded temporary homes."""

import argparse
import datetime
import fnmatch
import hashlib
import json
import os
import re
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

from .contract import validate
from .fixtures import Backend, fingerprints, seed
from .matrix import cases


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def source_contract(root: Path) -> dict:
    prefix = root / "codex-rs/cli/src"
    names = [
        "account_cmd.rs",
        *[
            f"account_cmd/{name}.rs"
            for name in ("add", "doctor", "error", "limits", "reset", "view")
        ],
    ]
    return {"root": str(root), "files": {name: digest(prefix / name) for name in names}}


def normalize(case, style, stdout, registry, requests, started, ended):
    """Replace only validated, case-declared generated scalar values, retaining raw JSON layout."""
    changes = []
    if case.observed_at and style == "json" and stdout:
        observed = json.loads(stdout)["observedAt"]
        if not isinstance(observed, int) or not started - 2 <= observed <= ended + 2:
            raise ValueError(
                "observedAt is not a generated timestamp from this command"
            )
        stdout, count = re.subn(
            r'("observedAt"\s*:\s*)-?\d+',
            lambda match: match[1] + '"<OBSERVED_AT>"',
            stdout,
        )
        if count != 1:
            raise ValueError("observedAt must appear exactly once")
        changes.append("stdout.observedAt")
    if case.last_used_at:
        account = next(
            account
            for account in json.loads(registry)["accounts"]
            if account["id"] == json.loads(registry)["defaultAccountId"]
        )
        value = account["lastUsedAt"]
        timestamp = datetime.datetime.fromisoformat(
            value.replace("Z", "+00:00")
        ).timestamp()
        if not started - 2 <= timestamp <= ended + 2:
            raise ValueError(
                "lastUsedAt is not a generated timestamp from this command"
            )
        if not value.endswith("Z"):
            raise ValueError("persisted lastUsedAt must retain UTC Z serialization")
        registry = registry.replace(json.dumps(value), '"<LAST_USED_AT>"')
        if style == "json":
            reported = json.loads(stdout)["account"]["lastUsedAt"]
            if reported != value.removesuffix("Z") + "+00:00":
                raise ValueError(
                    "reported lastUsedAt changed its timestamp or UTC offset format"
                )
            stdout = stdout.replace(json.dumps(reported), '"<LAST_USED_AT>"')
        changes.append("registry/default-account.lastUsedAt (+ stdout if JSON)")
    if case.request_id:
        value = (
            json.loads(stdout)["requestId"]
            if style == "json"
            else re.search(r"^Request: ([^\n]+)$", stdout, re.MULTILINE)[1]
        )
        if uuid.UUID(value).version != 7 or str(uuid.UUID(value)) != value:
            raise ValueError(
                "generated reset requestId must retain its UUIDv7 contract"
            )
        stdout = stdout.replace(value, "<REQUEST_ID>")
        for request in requests:
            request["body"] = request["body"].replace(value, "<REQUEST_ID>")
        changes.append("stdout.requestId + matching backend redeem_request_id")
    return stdout, registry, changes


def run_case(binary: Path, case, style: str, *, validate_contract: bool = True) -> dict:
    with (
        tempfile.TemporaryDirectory(prefix="codex-account-compat-") as temporary,
        Backend(case.backend) as backend,
    ):
        codex_home = Path(temporary) / "fixture-home"
        initial = seed(codex_home, case.fixture, backend.url)
        registry_path = codex_home / "accounts/index.json"
        before = (
            registry_path.read_bytes().decode("utf-8")
            if registry_path.exists()
            else None
        )
        protected = fingerprints(codex_home)
        environment = {
            key: os.environ[key]
            for key in (
                "PATH",
                "HOME",
                "SYSTEMROOT",
                "WINDIR",
                "LD_LIBRARY_PATH",
                "DYLD_LIBRARY_PATH",
            )
            if key in os.environ
        }
        environment.update(
            CODEX_HOME=str(codex_home),
            NO_COLOR="1",
            TERM="dumb",
            RUST_LOG="error",
            LANG="C.UTF-8",
            LC_ALL="C.UTF-8",
        )
        arguments = [
            str(binary),
            "account",
            *case.arguments,
            *(["--json"] if style == "json" else []),
        ]
        started = time.time()
        completed = subprocess.run(
            arguments,
            cwd=temporary,
            env=environment,
            input=b"",
            capture_output=True,
            timeout=60,
            check=False,
        )
        ended = time.time()
        after = (
            registry_path.read_bytes().decode("utf-8")
            if registry_path.exists()
            else None
        )
        stdout = completed.stdout.decode("utf-8")
        normalizations, failures = [], []
        try:
            stdout, after, normalizations = normalize(
                case, style, stdout, after, backend.requests, started, ended
            )
        except (ValueError, KeyError, TypeError, AttributeError, IndexError) as error:
            failures.append(
                f"normalization precondition failed: {type(error).__name__}: {error}"
            )
        result = {
            "case": case.name + "." + style,
            "arguments": arguments[1:],
            "fixture": case.fixture,
            "exitCode": completed.returncode,
            "stdout": stdout,
            "stderr": completed.stderr.decode("utf-8"),
            "registryBefore": before,
            "registryAfter": after,
            "fingerprintsAfter": fingerprints(codex_home),
            "normalizations": normalizations,
            # Backend calls are concurrent for --all; request bytes and multiplicity remain exact.
            "backendRequests": sorted(
                backend.requests,
                key=lambda request: json.dumps(request, sort_keys=True),
            ),
            "unexpectedRequests": backend.unexpected,
        }
        if validate_contract:
            try:
                failures.extend(validate(case, style, result, initial, protected))
            except (ValueError, KeyError, TypeError, IndexError) as error:
                failures.append(
                    f"source shape changed: {type(error).__name__}: {error}"
                )
        result["sourceFailures"] = failures
        for suffix in ("-wal", "-shm", "-journal"):
            if (codex_home / ("usage/usage.sqlite3" + suffix)).exists():
                result["sourceFailures"].append("a retired SQLite sidecar was created")
        return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("matrix", "capture", "compare"))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--source-root", type=Path, default=Path("/home/CodexMulti"))
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--group", action="append", default=[])
    parser.add_argument("--case", action="append", default=[])
    options = parser.parse_args()
    definitions = cases()
    signature = hashlib.sha256(
        json.dumps(
            [case.description() for case in definitions], sort_keys=True
        ).encode()
    ).hexdigest()
    selected = [
        (case, style)
        for case in definitions
        for style in ("human", "json")
        if (not options.group or case.group in options.group)
        and (
            not options.case
            or any(
                fnmatch.fnmatchcase(case.name + "." + style, pattern)
                for pattern in options.case
            )
        )
    ]
    if not selected:
        parser.error("selection matched no matrix rows")
    if options.mode == "matrix":
        print(
            json.dumps(
                {
                    "matrixDigest": signature,
                    "rows": [
                        {**case.description(), "style": style}
                        for case, style in selected
                    ],
                },
                indent=2,
            )
        )
        return 0
    if options.binary is None or options.output is None:
        parser.error("capture/compare require --binary and --output")
    binary = options.binary.resolve(strict=True)
    binary_digest = digest(binary)
    source = source_contract(options.source_root.resolve(strict=True))
    reference = None
    reference_digest = None
    if options.mode == "compare":
        if options.reference is None:
            parser.error("compare requires --reference")
        reference_digest = digest(options.reference)
        reference = json.loads(options.reference.read_text())
        if not reference["sourceCompatible"] or reference["matrixDigest"] != signature:
            parser.error("reference is unapproved or uses a different matrix")
        # A reimplementation is expected to change source files. Compare its
        # exact behavior with the frozen reference; retain both identities and
        # still reject either input changing while this comparison runs.
        retained = {row["case"] for row in reference["rows"]}
        if not options.group and not options.case:
            selected = [
                (case, style)
                for case, style in selected
                if case.name + "." + style in retained
            ]
        elif any(case.name + "." + style not in retained for case, style in selected):
            parser.error("selected comparison rows are missing from the reference")
    rows, mismatches = [], []
    for case, style in selected:
        case_id = case.name + "." + style
        try:
            result = run_case(binary, case, style, validate_contract=False)
            if reference is not None:
                baseline = next(
                    row for row in reference["rows"] if row["case"] == case_id
                )
                changed = [
                    key
                    for key in result
                    if key != "sourceFailures" and result[key] != baseline[key]
                ]
                if changed:
                    result["sourceFailures"].append(
                        "reference differs in: " + ", ".join(changed)
                    )
        except (
            OSError,
            ValueError,
            KeyError,
            StopIteration,
            subprocess.TimeoutExpired,
        ) as error:
            result = {
                "case": case_id,
                "sourceFailures": [
                    f"fixture/capture failure: {type(error).__name__}: {error}"
                ],
            }
        rows.append(result)
        if result["sourceFailures"]:
            mismatches.append(case_id)
    if digest(binary) != binary_digest:
        mismatches.append("binary identity changed while running")
    if source_contract(options.source_root.resolve(strict=True)) != source:
        mismatches.append("named source contract changed while running")
    if reference_digest is not None and digest(options.reference) != reference_digest:
        mismatches.append("reference identity changed while running")
    artifact = {
        "schemaVersion": 1,
        "mode": options.mode,
        "sourceContract": source,
        "referenceSha256": reference_digest,
        "referenceSourceContract": reference["sourceContract"] if reference else None,
        "binary": {"path": str(binary), "sha256": binary_digest},
        "matrixDigest": signature,
        "sourceCompatible": not mismatches,
        "rows": rows,
    }
    with options.output.open("x", encoding="utf-8") as output:
        json.dump(artifact, output, ensure_ascii=False, indent=2)
        output.write("\n")
    print(
        json.dumps(
            {
                "rows": len(rows),
                "passed": sum(not row["sourceFailures"] for row in rows),
                "failed": mismatches[:20],
                "artifact": str(options.output),
            }
        )
    )
    return int(bool(mismatches))


if __name__ == "__main__":
    raise SystemExit(main())
