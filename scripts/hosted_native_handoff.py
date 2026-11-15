#!/usr/bin/env python3
"""Import three successful native targets from their original Actions artifacts."""

import argparse
from datetime import datetime
import importlib.util
import json
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import tempfile
import zipfile

from release_identity import (
    ROOT,
    verify_artifacts,
    verify_hosted_native_source,
    verify_hosted_v8_source,
    verify_native_source,
)
from release_producer_inputs import bindings
from runner_handoff import download_artifact

SPEC = importlib.util.spec_from_file_location(
    "verify_candidate_run", ROOT / "codex-cli/scripts/verify_candidate_run.py"
)
candidate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(candidate)
REPOSITORY = "holyglory/codex"
MAX_BYTES = 8 * 1024**3


def parse_runs(value: str) -> dict[str, str]:
    if len(value) > 2048:
        raise ValueError("Hosted native run mapping exceeds 2 KiB")
    pairs = json.loads(value, object_pairs_hook=list)
    if (
        not isinstance(pairs, list)
        or len(pairs) != len(candidate.HOSTED_TARGETS)
        or any(not isinstance(pair, tuple) or len(pair) != 2 for pair in pairs)
        or {pair[0] for pair in pairs} != set(candidate.HOSTED_TARGETS)
        or any(
            type(value) not in (str, int)
            or not re.fullmatch(r"[1-9][0-9]{0,19}", str(value))
            for _, value in pairs
        )
    ):
        raise ValueError(
            "Hosted native runs must map exactly three targets to numeric run IDs"
        )
    return {target: str(run_id) for target, run_id in pairs}


def api(path: str) -> dict:
    data = subprocess.check_output(["gh", "api", f"repos/{REPOSITORY}/{path}"])
    if len(data) > 4 * 1024**2:
        raise ValueError("Hosted producer API response exceeds 4 MiB")
    return json.loads(data)


def pages(path: str, key: str) -> list[dict]:
    result = []
    for page in range(1, 6):
        separator = "&" if "?" in path else "?"
        response = api(f"{path}{separator}per_page=100&page={page}")
        items = response.get(key)
        total = response.get("total_count")
        if (
            type(total) is not int
            or not 0 <= total <= 500
            or not isinstance(items, list)
            or len(items) > 100
            or any(not isinstance(item, dict) for item in items)
        ):
            raise ValueError("Hosted producer API pagination is invalid or incomplete")
        result.extend(items)
        if len(result) == total:
            return result
        if not items or len(result) > total:
            break
    raise ValueError("Hosted producer API pagination is incomplete")


def load_run(run_id: str, attempt: str | None = None) -> tuple[dict, list[dict]]:
    path = f"actions/runs/{run_id}" + (f"/attempts/{attempt}" if attempt else "")
    run = api(path)
    if str(run.get("id")) != run_id or (
        attempt is not None and str(run.get("run_attempt")) != attempt
    ):
        raise ValueError("Hosted producer API returned another run or attempt")
    return run, pages(f"{path}/jobs?filter=latest", "jobs")


def extract(archive: Path, directory: Path, target: str) -> None:
    with zipfile.ZipFile(archive) as source:
        entries = source.infolist()
        names = [entry.filename for entry in entries]
        required = {"native-manifest.json", f"codex-package-{target}.tar.gz"}
        if (
            not 0 < len(entries) <= 10000
            or len(names) != len(set(names))
            or not required <= set(names)
            or sum(entry.file_size for entry in entries) > MAX_BYTES
        ):
            raise ValueError(
                "Hosted artifact has missing, duplicate or oversized ZIP entries"
            )
        for entry in entries:
            path = PurePosixPath(entry.filename)
            mode = stat.S_IFMT(entry.external_attr >> 16)
            if (
                path.is_absolute()
                or ".." in path.parts
                or "\\" in entry.filename
                or not path.parts
                or ":" in path.parts[0]
                or str(path) != entry.filename.rstrip("/")
                or mode not in (0, stat.S_IFREG, stat.S_IFDIR)
                or entry.flag_bits & 1
                or (
                    entry.filename == "native-manifest.json"
                    and entry.file_size > 1024**2
                )
            ):
                raise ValueError("Hosted artifact contains an unsafe ZIP entry")
        source.extractall(directory)


def import_targets(
    runs: dict[str, str],
    repository: str,
    product: str,
    packaging: str,
    destination: Path,
) -> dict:
    if repository != REPOSITORY or set(runs) != set(candidate.HOSTED_TARGETS):
        raise ValueError("Hosted native handoff repository or targets differ")
    verify_native_source(product, packaging)
    destination.mkdir(parents=True, exist_ok=True)
    records = {}
    v8_records = {}
    producers = []
    with tempfile.TemporaryDirectory(
        prefix="hosted-native-", dir=destination
    ) as temporary:
        staging = Path(temporary)
        for target in candidate.HOSTED_TARGETS:
            run_id = runs[target]
            if run_id not in records:
                run, jobs = load_run(run_id)
                artifacts = pages(f"actions/runs/{run_id}/artifacts", "artifacts")
                records[run_id] = run, jobs, artifacts
            run, jobs, artifacts = records[run_id]
            source = run.get("head_sha", "")
            if not re.fullmatch(r"[0-9a-f]{40}", source):
                raise ValueError("Hosted native source commit is invalid")
            verify_hosted_native_source(source, packaging, target=target)
            selected = candidate.verify_hosted_target_run(
                run, repository, source, jobs, target
            )
            matches = [
                artifact for artifact in artifacts if artifact.get("name") == target
            ]
            if len(matches) != 1:
                raise ValueError("Hosted target artifact is missing or duplicated")
            artifact = matches[0]
            size = artifact.get("size_in_bytes")
            digest = artifact.get("digest", "")
            if (
                type(artifact.get("id")) is not int
                or artifact["id"] <= 0
                or artifact.get("expired") is not False
                or artifact.get("workflow_run", {}).get("id") != run["id"]
                or artifact["workflow_run"].get("head_sha") != source
                or type(size) is not int
                or not 0 < size <= MAX_BYTES
                or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest)
            ):
                raise ValueError(
                    "Hosted artifact source, expiry, size or digest differs"
                )
            upload = next(
                step
                for step in selected["package"]["steps"]
                if step["name"] == candidate.UPLOAD_STEP
            )
            created, started, completed = (
                int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
                for value in (
                    artifact["created_at"],
                    upload["started_at"],
                    upload["completed_at"],
                )
            )
            if not started <= created <= completed:
                raise ValueError(
                    "Hosted artifact does not belong to the successful upload"
                )
            archive = staging / f"{target}.zip"
            download_artifact(
                {
                    "url": f"https://api.github.com/repos/{repository}/actions/artifacts/{artifact['id']}/zip",
                    "sha256": digest.removeprefix("sha256:"),
                    "size_bytes": size,
                },
                archive,
            )
            folder = staging / target
            extract(archive, folder, target)
            archive.unlink()
            receipt = json.loads((folder / "native-manifest.json").read_bytes())
            if (
                receipt.get("source_commit") != source
                or receipt.get("target") != target
            ):
                raise ValueError("Hosted native receipt target or source differs")
            bindings(folder)
            pair = receipt["v8_pair"]
            v8_run, attempt = (
                str(pair.get("producer_run_id", "")),
                str(pair.get("producer_run_attempt", "")),
            )
            if any(
                not re.fullmatch(r"[1-9][0-9]{0,19}", value)
                for value in (v8_run, attempt)
            ):
                raise ValueError("Hosted V8 producer run or attempt is invalid")
            if (v8_run, attempt) not in v8_records:
                v8_records[v8_run, attempt] = load_run(v8_run, attempt)
            origin, origin_jobs = v8_records[v8_run, attempt]
            candidate.verify_candidate_run(
                origin, repository, pair["source_commit"], origin_jobs, hosted_v8=True
            )
            verify_hosted_v8_source(pair["source_commit"], source)
            producers.append(
                {
                    "target": target,
                    "run_id": run_id,
                    "run_conclusion": run["conclusion"],
                    "source_commit": source,
                    "job_id": selected["package"]["id"],
                    "job_attempt": selected["package"]["run_attempt"],
                    "artifact": artifact,
                    "v8_run_id": v8_run,
                    "v8_run_attempt": attempt,
                }
            )
        verified = verify_artifacts(
            staging, packaging, targets=candidate.HOSTED_TARGETS
        )
        if any((destination / target).exists() for target in candidate.HOSTED_TARGETS):
            raise ValueError("Hosted handoff would replace existing native artifacts")
        for target in candidate.HOSTED_TARGETS:
            (staging / target).rename(destination / target)
    result = {
        "schema": 1,
        "packaging_commit": packaging,
        "producers": producers,
        "native_receipts": verified,
    }
    (destination / "hosted-native-producers.json").write_text(
        json.dumps(result, indent=2) + "\n"
    )
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--product-source", required=True)
    parser.add_argument("--packaging", required=True)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    result = import_targets(
        parse_runs(args.runs),
        args.repository,
        args.product_source,
        args.packaging,
        args.destination,
    )
    print(json.dumps(result))


if __name__ == "__main__":
    main()
