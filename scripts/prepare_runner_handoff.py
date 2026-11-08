#!/usr/bin/env python3
"""Create a digest-bound handoff manifest and immutable native bundle archives.

The owner uploads the emitted archives to a draft GitHub release through the
existing authenticated handoff path. This helper performs no network mutation.
"""

import argparse
import hashlib
import json
from pathlib import Path
import tarfile

try:
    from runner_handoff import SCHEMA, validate
except ModuleNotFoundError:
    from scripts.runner_handoff import SCHEMA, validate


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def prepare(
    source_dir: Path,
    output_dir: Path,
    commit: str,
    producer: str,
    targets: list[str],
    repository: str,
    tag: str,
) -> Path:
    output_dir.mkdir(parents=True, exist_ok=True)
    artifacts = []
    receipts = []
    producer_commits = []
    native_tree = None
    for target in targets:
        source = source_dir / target
        receipt_path = source / "native-manifest.json"
        if not source.is_dir() or not receipt_path.is_file():
            raise ValueError(f"Missing native handoff target or receipt: {target}")
        receipt = json.loads(receipt_path.read_text())
        if receipt.get("status") != "success" or receipt.get("target") != target:
            raise ValueError(f"Native handoff receipt is not successful: {target}")
        producer_commit = receipt.get("source_commit") or receipt.get("commit")
        if not isinstance(producer_commit, str):
            raise ValueError(f"Native handoff has no source commit: {target}")
        producer_commits.append(producer_commit)
        if native_tree is None:
            native_tree = receipt.get("native_tree")
        if receipt.get("native_tree") != native_tree:
            raise ValueError("Native handoffs do not share a codex-rs tree")
        receipt_checks = receipt.get("checks")
        if not isinstance(receipt_checks, list) or not receipt_checks:
            raise ValueError(f"Native handoff has no retained checks: {target}")
        bundle_name = f"native-handoff-{target}-{commit}.tar.gz"
        bundle = output_dir / bundle_name
        with tarfile.open(bundle, "w:gz") as archive:
            for path in sorted(source.rglob("*")):
                if path.is_file():
                    archive.add(path, arcname=path.relative_to(source_dir).as_posix())
        artifacts.append(
            {
                "name": bundle_name,
                "url": f"https://github.com/{repository}/releases/download/{tag}/{bundle_name}",
                "sha256": digest(bundle),
                "size_bytes": bundle.stat().st_size,
                "targets": [target],
            }
        )
        receipts.extend(
            {**check, "name": f"{target}:{check['name']}"} for check in receipt_checks
        )
    manifest = {
        "schema": SCHEMA,
        "commit": commit,
        "source_commit": producer_commits[0],
        "producer_commits": sorted(set(producer_commits)),
        "native_tree": native_tree,
        "producer": producer,
        "status": "success",
        "checks": receipts,
        "artifacts": artifacts,
    }
    validate(manifest, commit, producer, targets)
    path = output_dir / f"{producer}-handoff-{commit}.json"
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--producer", required=True)
    parser.add_argument("--targets", required=True)
    parser.add_argument("--repository", default="holyglory/codex")
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()
    print(
        prepare(
            args.source_dir.resolve(),
            args.output_dir.resolve(),
            args.commit,
            args.producer,
            args.targets.split(","),
            args.repository,
            args.tag,
        )
    )


if __name__ == "__main__":
    main()
