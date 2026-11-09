#!/usr/bin/env python3
"""Require exact-source native build evidence or a complete npm candidate."""

import argparse
import json
from pathlib import Path
import sys

HOSTED_TARGETS = (
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
HOSTED_V8_JOBS = tuple(
    f"Build and test {target} / V8 source pair" for target in HOSTED_TARGETS
)
HOSTED_PACKAGE_JOBS = tuple(
    f"Build and test {target} / Package native target" for target in HOSTED_TARGETS
)
CANDIDATE_JOBS = (
    "Verify local acceptance and native source identity",
    "Import verified VPS and private Windows outputs",
    "Assemble and verify all seven npm tarballs",
    "Candidate checks passed",
)


def verify_candidate_run(
    run: dict,
    repository: str,
    commit: str,
    jobs: list[dict],
    *,
    hosted_native: bool = False,
) -> None:
    expected = {
        "status": "completed",
        "conclusion": "success",
        "path": ".github/workflows/downstream-candidate.yml",
        "event": "workflow_dispatch",
        "head_sha": commit,
    }
    if repository != "holyglory/codex" or any(
        run.get(key) != value for key, value in expected.items()
    ):
        raise ValueError("Candidate is not a successful exact-source fork release run")
    for field in ("repository", "head_repository"):
        if (
            not isinstance(run.get(field), dict)
            or run[field].get("full_name") != repository
        ):
            raise ValueError(f"Candidate {field} differs from the release repository")
    required = (
        (CANDIDATE_JOBS[0], *HOSTED_V8_JOBS, *HOSTED_PACKAGE_JOBS)
        if hosted_native
        else CANDIDATE_JOBS
    )
    if (
        not isinstance(jobs, list)
        or len(jobs) > 500
        or any(not isinstance(job, dict) for job in jobs)
    ):
        raise ValueError("Candidate jobs must be a bounded complete API result")
    for name in required:
        matches = [job for job in jobs if job.get("name") == name]
        if (
            len(matches) != 1
            or matches[0].get("status") != "completed"
            or matches[0].get("conclusion") != "success"
        ):
            raise ValueError(
                f"Required candidate job did not complete successfully: {name}"
            )
    # A successful native-only workflow has no successful npm or final-result
    # job. Its overall green conclusion is never publication evidence.


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--jobs-file", type=Path, required=True)
    parser.add_argument("--hosted-native", action="store_true")
    args = parser.parse_args()
    pages = json.loads(args.jobs_file.read_text())
    if not isinstance(pages, list) or len(pages) > 5:
        parser.error("jobs file must contain at most five paginated API responses")
    jobs = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get("jobs"), list):
            parser.error("jobs file contains an invalid API response")
        jobs.extend(page["jobs"])
    verify_candidate_run(
        json.load(sys.stdin),
        args.repository,
        args.commit,
        jobs,
        hosted_native=args.hosted_native,
    )


if __name__ == "__main__":
    main()
