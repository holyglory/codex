#!/usr/bin/env python3
"""Require the successful fork candidate at the annotated release tag's commit."""

import argparse
import json
import sys


def verify_candidate_run(run: dict, repository: str, commit: str) -> None:
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    verify_candidate_run(json.load(sys.stdin), args.repository, args.commit)


if __name__ == "__main__":
    main()
