#!/usr/bin/env python3
"""Review schema-only hash evidence and emit a patch without accepting snapshots."""

import argparse
import difflib
import hashlib
import json
from pathlib import Path
import re

from tool_schema_catalog_review import reconstruct_catalog
from tool_schema_catalog_review import review_body

MAX_BYTES = 2 * 1024 * 1024
MAX_FILES = 256


def encoded(value):
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode()


def fingerprint(value):
    result = 0xCBF29CE484222325
    for byte in encoded(value):
        result = ((result ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{result:016x}"


def mappings(current, previous, label="catalog"):
    if current == previous:
        return []
    result = [
        {"label": label, "old": fingerprint(previous), "new": fingerprint(current)}
    ]
    current_tools = current if isinstance(current, list) else current.get("tools", [])
    previous_tools = (
        previous if isinstance(previous, list) else previous.get("tools", [])
    )
    for tool in current_tools:
        old = next(
            (
                row
                for row in previous_tools
                if row.get("type") == tool.get("type")
                and row.get("name") == tool.get("name")
            ),
            None,
        )
        if old is not None:
            result.extend(mappings(tool, old, f"{tool['type']}/{tool['name']}"))
    return result


def load_bounded(path):
    if path.is_symlink() or path.stat().st_size > MAX_BYTES:
        raise ValueError("invalid or oversized private evidence file")
    return json.loads(path.read_text())


def snapshot_parts(text):
    lines = text.splitlines(keepends=True)
    if not lines or lines[0].rstrip("\r\n") != "---":
        raise ValueError("snapshot metadata header is missing")
    end = next(
        (
            index
            for index in range(1, len(lines))
            if lines[index].rstrip("\r\n") == "---"
        ),
        None,
    )
    if end is None:
        raise ValueError("snapshot metadata header is unterminated")
    # Insta may add or move this source location when emitting .snap.new.
    # Preserve the original header; every other metadata field must match.
    metadata = "".join(
        line
        for line in lines[1:end]
        if not re.fullmatch(r"assertion_line: [1-9][0-9]*\r?\n", line)
    )
    return "".join(lines[: end + 1]), metadata, lines[end + 1 :]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--repository-root", required=True, type=Path)
    parser.add_argument("--candidate-root", required=True, type=Path)
    parser.add_argument("--patch-output", required=True, type=Path)
    args = parser.parse_args()
    repository = args.repository_root.resolve()
    patch_output = args.patch_output.resolve()
    if patch_output.is_relative_to(repository):
        raise ValueError("diagnostic patch must be outside the source checkout")
    paths = sorted(args.evidence_dir.glob("catalog-*.json"))
    if not paths or len(paths) > MAX_FILES:
        raise ValueError("no schema evidence or evidence count exceeds limit")
    contract = load_bounded(args.evidence_dir / "contract.json")
    contract_hash = fingerprint(contract)
    transitions = {}
    catalogs = {}
    evidence_receipts = []
    for path in paths:
        evidence = load_bounded(path)
        if (
            evidence.get("schemaVersion") != 1
            or evidence.get("contractHash") != contract_hash
        ):
            raise ValueError("schema evidence contract mismatch")
        if (
            reconstruct_catalog(evidence["current"], contract)
            != evidence["reconstructed"]
        ):
            raise ValueError("reconstruction changes fields outside the approved tools")
        old_hash = fingerprint(evidence["reconstructed"])
        pair = (evidence["reconstructed"], evidence["current"])
        if old_hash in catalogs and catalogs[old_hash] != pair:
            raise ValueError("one old catalog has conflicting exact preimages")
        catalogs[old_hash] = pair
        expected = mappings(evidence["current"], evidence["reconstructed"])
        if expected != evidence["mappings"]:
            raise ValueError(
                "recorded hashes do not match exact normalized schema preimages"
            )
        for row in expected:
            key = row["label"], row["old"]
            if key in transitions and transitions[key] != row["new"]:
                raise ValueError("one old schema has conflicting current fingerprints")
            transitions[key] = row["new"]
        evidence_receipts.append(
            {"file": path.name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        )
    snapshots = repository / "codex-rs/core/tests/suite/snapshots"
    changes = []
    used = {}
    candidates = sorted(args.candidate_root.glob("*.snap.new"))
    if not candidates or len(candidates) > MAX_FILES:
        raise ValueError(
            "no reviewed snapshot candidates or candidate count exceeds limit"
        )
    for candidate in candidates:
        original = snapshots / candidate.name.removesuffix(".new")
        if not original.is_file() or candidate.is_symlink():
            raise ValueError("candidate has no matching existing snapshot")
        if original.stat().st_size > MAX_BYTES or candidate.stat().st_size > MAX_BYTES:
            raise ValueError("snapshot candidate exceeds review limit")
        before = original.read_text()
        after = candidate.read_text()
        original_header, original_metadata, before_lines = snapshot_parts(before)
        _, candidate_metadata, after_lines = snapshot_parts(after)
        if original_metadata != candidate_metadata:
            raise ValueError(f"candidate changes snapshot metadata: {candidate.name}")
        reconstructed, file_used = review_body(
            before_lines, after_lines, catalogs, fingerprint
        )
        if not file_used:
            continue
        relative = original.relative_to(repository).as_posix()
        changes.append(f"diff --git a/{relative} b/{relative}\n")
        changes.extend(
            difflib.unified_diff(
                before.splitlines(True),
                (original_header + reconstructed).splitlines(True),
                fromfile=f"a/{relative}",
                tofile=f"b/{relative}",
            )
        )
        for key, count in file_used.items():
            used[key] = used.get(key, 0) + count
    if not changes:
        raise ValueError("no evidenced fixture changes")
    with patch_output.open("x") as output:
        output.write("".join(changes))
    manifest = {
        "patch": str(patch_output),
        "contractSha256": hashlib.sha256(encoded(contract)).hexdigest(),
        "files": len([line for line in changes if line.startswith("diff --git ")]),
        "fingerprints": [
            {
                "label": label,
                "old": old,
                "new": transitions[label, old],
                "occurrences": count,
            }
            for (label, old), count in sorted(used.items())
        ],
        "evidence": evidence_receipts,
        "toolInventoryRule": "Require complete exact schema-rendered catalogs; reject unproved delta inventories.",
        "candidateMetadataRule": "Preserve original headers; ignore only assertion_line when comparing candidates.",
    }
    manifest_path = patch_output.with_suffix(".evidence.json")
    with manifest_path.open("x") as output:
        json.dump(manifest, output, separators=(",", ":"))
    print(
        json.dumps(
            {
                "patch": str(patch_output),
                "proofManifest": str(manifest_path),
                "files": manifest["files"],
                "fingerprintCount": len(used),
                "evidenceFiles": len(paths),
            },
            separators=(",", ":"),
        )
    )


if __name__ == "__main__":
    main()
