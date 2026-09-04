#!/usr/bin/env python3
"""Reject npm publication unless the complete candidate workflow succeeded."""

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
    for field, value in expected.items():
        if run.get(field) != value:
            raise ValueError(f"candidate run has an unexpected {field}")
    for field in ("repository", "head_repository"):
        if (
            not isinstance(run.get(field), dict)
            or run[field].get("full_name") != repository
        ):
            raise ValueError(f"candidate run has an unexpected {field}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    try:
        verify_candidate_run(json.load(sys.stdin), args.repository, args.commit)
    except (ValueError, TypeError, AttributeError) as error:
        parser.exit(1, f"Publication refused: {error}\n")


if __name__ == "__main__":
    main()
