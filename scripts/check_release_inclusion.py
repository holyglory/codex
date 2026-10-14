#!/usr/bin/env python3
"""Reject lost release prerequisites before compilation; tests still prove behavior."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib


PACKAGE_CHECKS = {"fork-update", "root-usage"}


def load_manifest(root):
    return json.loads((root / "scripts/release_inclusion.json").read_text())


def verify(root, manifest):
    if manifest.get("schema") != 1:
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
    for capability in manifest["capabilities"]:
        for path in capability["required_files"]:
            if not (root / path).is_file():
                errors.append(f"{capability['id']}: missing {path}")
        if not capability["filters"]:
            errors.append(f"{capability['id']}: missing mandatory test selection")
        for check in capability.get("package_checks", []):
            if check not in PACKAGE_CHECKS:
                errors.append(f"{capability['id']}: unknown package check {check}")
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
        "scope": "source prerequisites only; no runtime or release pass is claimed",
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    try:
        print(json.dumps(verify(root, load_manifest(root))))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Release inclusion refused: {error}\n")
