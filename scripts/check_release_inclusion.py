#!/usr/bin/env python3
"""Reject lost release prerequisites before compilation; tests still prove behavior."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib


def load_manifest(root):
    return json.loads((root / "scripts/release_inclusion.json").read_text())


def patch_identity(root, commit):
    patch = subprocess.check_output(
        [
            "git",
            "show",
            "--format=",
            "--no-ext-diff",
            "--binary",
            "--diff-merges=first-parent",
            commit,
        ],
        cwd=root,
        stderr=subprocess.DEVNULL,
    )
    identity = (
        subprocess.check_output(
            ["git", "patch-id", "--stable"], input=patch, cwd=root, text=False
        )
        .decode()
        .split()
    )
    if not identity:
        raise ValueError(f"included commit has no patch: {commit}")
    return identity[0]


def pull_request_inventory(root, repository):
    output = subprocess.check_output(
        [
            "gh",
            "api",
            "--paginate",
            f"repos/{repository}/pulls?state=all&per_page=100",
            "--jq",
            ".[] | {number, state, merged_at, merge_commit_sha, head_ref: .head.ref, head_sha: .head.sha}",
        ],
        cwd=root,
        text=True,
        stderr=subprocess.DEVNULL,
        timeout=30,
    )
    return [json.loads(line) for line in output.splitlines() if line]


def verify(root, manifest):
    if manifest.get("schema") != 2:
        raise ValueError("Unsupported release inclusion manifest")
    errors = []

    def git(*args):
        return subprocess.check_output(
            ["git", "-C", str(root), *args], text=True, stderr=subprocess.DEVNULL
        ).strip()

    for boundary in ("upstream", "rollback"):
        identity = manifest[boundary]
        try:
            actual = git("rev-parse", "--verify", identity["tag"] + "^{commit}")
        except subprocess.CalledProcessError:
            errors.append(f"{boundary}: tag unavailable; fetch the declared tag")
            continue
        if actual != identity["commit"]:
            errors.append(f"{boundary}: tag no longer matches the approved commit")
    try:
        git("merge-base", "--is-ancestor", manifest["upstream"]["commit"], "HEAD")
    except subprocess.CalledProcessError:
        errors.append("candidate is not based on the approved upstream commit")

    version = tomllib.loads((root / "codex-rs/Cargo.toml").read_text())["workspace"][
        "package"
    ]["version"]
    if (
        version != manifest["rust_version"]
        or version.replace("+multi.", "-multi.") != manifest["npm_version"]
    ):
        errors.append("release versions differ from the inclusion manifest")
    sources = set()
    required_pulls = set(manifest["required_pull_requests"])
    for capability in manifest["capabilities"]:
        for path in capability["required_files"]:
            if not (root / path).is_file():
                errors.append(f"{capability['id']}: missing {path}")
        if not capability["filters"]:
            errors.append(f"{capability['id']}: missing mandatory test selection")
        if "pull_request" in capability:
            required_pulls.add(capability["pull_request"])
        patches = capability.get("included_patches", [])
        if "source_commit" in capability and not any(
            row["source_commit"] == capability["source_commit"] for row in patches
        ):
            errors.append(f"{capability['id']}: missing source patch mapping")
        for row in patches:
            sources.add(row["source_commit"])
            try:
                git("merge-base", "--is-ancestor", row["applied_commit"], "HEAD")
                original = patch_identity(root, row["source_commit"])
                applied = patch_identity(root, row["applied_commit"])
                if (
                    original != row["source_patch_id"]
                    or applied != row["applied_patch_id"]
                ):
                    errors.append(f"{capability['id']}: patch identity changed")
                if original != applied and not row.get("reviewed_adaptation"):
                    errors.append(
                        f"{capability['id']}: replay needs reviewed adaptation"
                    )
            except (ValueError, subprocess.CalledProcessError):
                errors.append(
                    f"{capability['id']}: missing or unapplied recorded patch"
                )
    for stack in manifest["retained_stacks"]:
        try:
            tip = git("rev-parse", "--verify", stack["tag"] + "^{commit}")
            commits = git(
                "rev-list", "--reverse", f"{stack['base_commit']}..{tip}"
            ).splitlines()
            if tip != stack["tip_commit"] or commits != stack["source_commits"]:
                errors.append(f"{stack['branch']}: retained stack changed")
            if not set(commits).issubset(sources):
                errors.append(
                    f"{stack['branch']}: retained commits lack inclusion mappings"
                )
        except subprocess.CalledProcessError:
            errors.append(f"{stack['branch']}: retained source unavailable; fetch tags")
    prefixes = tuple(manifest["release_branch_prefixes"])
    checked_pulls = []
    if required_pulls or prefixes:
        inventory = pull_request_inventory(root, manifest["github_repository"])
        pulls = {row["number"]: row for row in inventory}
        candidate_commit = git("rev-parse", "HEAD")
        for number in sorted(required_pulls):
            row = pulls.get(number)
            if not row or not row["merged_at"] or not row["merge_commit_sha"]:
                errors.append(f"PR #{number}: not merged")
                continue
            try:
                git("merge-base", "--is-ancestor", row["merge_commit_sha"], "HEAD")
                checked_pulls.append(
                    {"number": number, "merge_commit": row["merge_commit_sha"]}
                )
            except subprocess.CalledProcessError:
                errors.append(f"PR #{number}: merged commit absent from candidate")
        for row in inventory:
            if (
                row["state"] == "open"
                and row["head_ref"].startswith(prefixes)
                and row.get("head_sha") != candidate_commit
            ):
                errors.append(f"PR #{row['number']}: release branch still open")
    for path, expected in manifest["migrations"].items():
        source = root / path
        if (
            not source.is_file()
            or hashlib.sha256(source.read_bytes()).hexdigest() != expected
        ):
            errors.append(f"migration missing or changed: {path}")
    if errors:
        raise ValueError("\n".join(errors))
    return {
        "commit": git("rev-parse", "HEAD"),
        "upstream": manifest["upstream"],
        "capabilities": [row["id"] for row in manifest["capabilities"]],
        "merged_pull_requests": checked_pulls,
        "recorded_patch_sources": len(sources),
        "scope": "source prerequisites only; no runtime or release pass is claimed",
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    try:
        print(json.dumps(verify(root, load_manifest(root))))
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Release inclusion refused: {error}\n")
