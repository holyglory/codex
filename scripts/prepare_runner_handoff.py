#!/usr/bin/env python3
"""Stage Linux and private-Windows artifacts for a candidate workflow.

The local Linux VPS and the private Windows controller cannot upload into a
public workflow run directly. This owner-operated command creates one draft
GitHub release keyed by the exact source commit, uploads immutable bundle
assets, writes commit/hash-bound manifests, and optionally dispatches the
macOS/npm candidate workflow with those manifests as inputs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
from itertools import chain
from typing import Iterable

import local_candidate
import runner_handoff


DEFAULT_REPOSITORY = "holyglory/codex"
DEFAULT_WINDOWS_REPOSITORY = "holyglory/codex-win-msvc-ci"


def _run(command: list[str], *, input_text: str | None = None) -> str:
    return subprocess.check_output(command, text=True, input=input_text).strip()


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _archive_tree(source: Path, output: Path) -> None:
    source = source.resolve()
    if not source.is_dir():
        raise ValueError(f"handoff source is not a directory: {source}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(output, "w:gz", compresslevel=9) as archive:
        files = sorted(path for path in source.rglob("*") if path.is_file())
        if not files:
            raise ValueError(f"handoff source is empty: {source}")
        for path in files:
            relative = path.relative_to(source)
            if path.is_symlink() or relative.is_absolute() or ".." in relative.parts:
                raise ValueError(f"unsafe handoff source entry: {relative}")
            info = archive.gettarinfo(str(path), arcname=str(relative))
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            info.mtime = 0
            with path.open("rb") as stream:
                archive.addfile(info, stream)


def _copy_target_tree(
    source: Path, targets: Iterable[str], destination: Path
) -> list[str]:
    destination.mkdir(parents=True, exist_ok=True)
    copied: list[str] = []
    for target in targets:
        target_source = source / target
        if not target_source.is_dir():
            raise ValueError(f"handoff target directory is missing: {target}")
        target_destination = destination / target
        shutil.copytree(target_source, target_destination)
        copied.append(target)
    return copied


def _linux_bundle_source(package_directory: Path, destination: Path) -> list[str]:
    native_dist = package_directory / "native-dist"
    if not native_dist.is_dir():
        raise ValueError("Linux package handoff lacks native-dist")
    target = "x86_64-unknown-linux-musl"
    target_destination = destination / target
    target_destination.mkdir(parents=True, exist_ok=True)
    files = list(native_dist.glob("*"))
    if not files:
        raise ValueError("Linux package handoff native-dist is empty")
    for path in files:
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"invalid Linux package handoff entry: {path.name}")
        shutil.copy2(path, target_destination / path.name)
    required = target_destination / f"codex-package-{target}.tar.gz"
    if not required.is_file():
        raise ValueError(f"Linux package handoff lacks {required.name}")
    return [target]


def _find_windows_targets(source: Path, destination: Path) -> list[str]:
    targets = ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]
    candidates = {path.name: path for path in source.rglob("codex-package-*.tar.gz")}
    for target in targets:
        package = candidates.get(f"codex-package-{target}.tar.gz")
        if package is None:
            raise ValueError(f"Windows controller handoff lacks {target} package")
        target_source = package.parent
        target_destination = destination / target
        target_destination.mkdir(parents=True, exist_ok=True)
        for path in target_source.iterdir():
            if (
                path.is_file()
                and not path.is_symlink()
                and (path.name.startswith("codex-package-") or path.name == "SHA256SUMS")
            ):
                shutil.copy2(path, target_destination / path.name)
    return targets


def _manifest(
    *,
    commit: str,
    producer: str,
    checks: list[dict],
    artifact: Path,
    targets: list[str],
    repository: str,
    tag: str,
) -> dict:
    url = f"https://github.com/{repository}/releases/download/{tag}/{artifact.name}"
    value = {
        "schema": runner_handoff.SCHEMA,
        "commit": commit,
        "producer": producer,
        "status": "success",
        "checks": checks,
        "artifacts": [
            {
                "name": artifact.name,
                "url": url,
                "sha256": _sha256(artifact),
                "size_bytes": artifact.stat().st_size,
                "targets": targets,
            }
        ],
    }
    runner_handoff.validate(value, commit, producer, targets)
    return value


def _release_exists(repository: str, tag: str) -> bool:
    result = subprocess.run(
        ["gh", "release", "view", tag, "--repo", repository],
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return result.returncode == 0


def _ensure_release(repository: str, tag: str) -> None:
    if _release_exists(repository, tag):
        return
    subprocess.run(
        [
            "gh",
            "release",
            "create",
            tag,
            "--repo",
            repository,
            "--draft",
            "--title",
            f"Runner handoff {tag.removeprefix('codex-runner-handoff-')}",
            "--notes",
            "Owner-operated release handoff. Candidate artifacts are commit-bound and are not a product release.",
        ],
        check=True,
    )


def _upload(repository: str, tag: str, paths: Iterable[Path]) -> None:
    subprocess.run(
        [
            "gh",
            "release",
            "upload",
            tag,
            "--repo",
            repository,
            "--clobber",
            *map(str, paths),
        ],
        check=True,
    )


def _download_windows_run(
    repository: str, run_id: str, source_sha: str, destination: Path
) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    artifact_name = f"windows-handoff-{source_sha}"
    subprocess.run(
        [
            "gh",
            "run",
            "download",
            run_id,
            "--repo",
            repository,
            "--name",
            artifact_name,
            "--dir",
            str(destination),
        ],
        check=True,
    )
    return destination


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--linux-receipt", type=Path, required=True)
    parser.add_argument("--linux-package", type=Path, required=True)
    parser.add_argument("--windows-artifacts", type=Path)
    parser.add_argument("--windows-run-id")
    parser.add_argument("--windows-repository", default=DEFAULT_WINDOWS_REPOSITORY)
    parser.add_argument("--repository", default=DEFAULT_REPOSITORY)
    parser.add_argument("--tag")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--branch")
    parser.add_argument("--dispatch", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    commit = args.commit
    repository_root = Path(__file__).resolve().parents[1]
    if local_candidate.git(repository_root, "rev-parse", "HEAD") != commit:
        raise SystemExit("handoff source checkout does not match --commit")
    receipt = json.loads(args.linux_receipt.read_text())
    local_candidate.validate(receipt, commit)
    if bool(args.windows_artifacts) == bool(args.windows_run_id):
        raise SystemExit(
            "provide exactly one of --windows-artifacts or --windows-run-id"
        )

    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    tag = args.tag or f"codex-runner-handoff-{commit}"
    with tempfile.TemporaryDirectory(prefix="runner-handoff-") as temporary:
        temporary_root = Path(temporary)
        linux_root = temporary_root / "linux"
        windows_root = temporary_root / "windows"
        linux_targets = _linux_bundle_source(args.linux_package, linux_root)
        windows_source = args.windows_artifacts
        if windows_source is None:
            windows_source = _download_windows_run(
                args.windows_repository, args.windows_run_id, commit, windows_root
            )
        manifest_candidates = list(windows_source.rglob("windows-manifest.json"))
        if len(manifest_candidates) != 1:
            raise SystemExit("Windows controller handoff lacks windows-manifest.json")
        windows_manifest_path = manifest_candidates[0]
        windows_manifest = json.loads(windows_manifest_path.read_text())
        if (
            windows_manifest.get("schema") != runner_handoff.SCHEMA
            or windows_manifest.get("commit") != commit
            or windows_manifest.get("producer") != "windows-private-controller"
            or windows_manifest.get("status") != "success"
        ):
            raise SystemExit(
                "Windows controller manifest identity or status is invalid"
            )
        windows_checks = runner_handoff.validate_checks(windows_manifest.get("checks"))
        windows_targets = _find_windows_targets(windows_source, windows_root)
        linux_bundle = output / f"linux-handoff-{commit}.tar.gz"
        windows_bundle = output / f"windows-handoff-{commit}.tar.gz"
        _archive_tree(linux_root, linux_bundle)
        _archive_tree(windows_root, windows_bundle)

        linux_manifest = _manifest(
            commit=commit,
            producer="linux-local",
            checks=receipt["checks"],
            artifact=linux_bundle,
            targets=linux_targets,
            repository=args.repository,
            tag=tag,
        )
        windows_manifest = _manifest(
            commit=commit,
            producer="windows-private-controller",
            checks=windows_checks,
            artifact=windows_bundle,
            targets=windows_targets,
            repository=args.repository,
            tag=tag,
        )
        linux_manifest_path = output / f"linux-handoff-{commit}.json"
        windows_manifest_path = output / f"windows-handoff-{commit}.json"
        linux_manifest_path.write_text(json.dumps(linux_manifest, indent=2) + "\n")
        windows_manifest_path.write_text(json.dumps(windows_manifest, indent=2) + "\n")

    _ensure_release(args.repository, tag)
    _upload(
        args.repository,
        tag,
        (linux_bundle, windows_bundle, linux_manifest_path, windows_manifest_path),
    )

    if args.dispatch:
        if not args.branch:
            raise SystemExit("--branch is required with --dispatch")
        payload = {
            "local_acceptance": args.linux_receipt.read_text(),
            "linux_handoff": linux_manifest_path.read_text(),
            "windows_handoff": windows_manifest_path.read_text(),
        }
        subprocess.run(
            [
                "gh",
                "workflow",
                "run",
                "downstream-candidate.yml",
                "--repo",
                args.repository,
                "--ref",
                args.branch,
                *chain.from_iterable(
                    ("--field", f"{key}={value}") for key, value in payload.items()
                ),
            ],
            check=True,
        )
    print(
        json.dumps(
            {
                "tag": tag,
                "linux": str(linux_manifest_path),
                "windows": str(windows_manifest_path),
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
