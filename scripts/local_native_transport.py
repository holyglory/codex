#!/usr/bin/env python3
"""Bundle compiled native inputs, then bind their bytes to an uploaded asset URL.

These local commands never upload, dispatch, compile or execute target binaries.
Keep the emitted metadata digest outside the bundle for the descriptor phase.
"""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import tarfile

from local_native_artifacts import (
    MAX_BYTES,
    MAX_FILES,
    SHA256,
    TARGETS,
    file_info,
    load_json,
    relative_path,
    require,
    verify_build,
    write_json,
)
from local_native_release import helper_identity
from local_release_tar import BoundedTarInfo

MAX_TRANSPORT_BYTES = 8 * 1024**3


def bundle(args):
    incoming, output, metadata = (
        args.input.resolve(),
        args.output.resolve(),
        args.metadata.resolve(),
    )
    require(output.suffixes[-2:] == [".tar", ".gz"], "Use a .tar.gz transport filename")
    require(
        not output.exists() and not metadata.exists(),
        "Preserve the existing transport and metadata",
    )
    require(
        not output.is_relative_to(incoming) and not metadata.is_relative_to(incoming),
        "Transport outputs must be outside the compiled inputs",
    )
    build = verify_build(
        incoming,
        args.manifest_sha256,
        args.source_root.resolve(),
        args.commit,
        args.target,
    )
    helpers = helper_identity()
    require(
        {key: build["producer"].get(key) for key in helpers} == helpers,
        "Transport helpers differ from the compiler's helpers",
    )
    files = {
        **build["files"],
        "build-manifest.json": file_info(incoming / "build-manifest.json"),
    }
    require(len(files) <= MAX_FILES, "Transport has too many files")
    output.parent.mkdir(parents=True, exist_ok=True)
    metadata.parent.mkdir(parents=True, exist_ok=True)
    with (
        output.open("xb") as destination,
        gzip.GzipFile(
            fileobj=destination, mode="wb", filename="", mtime=0, compresslevel=1
        ) as compressed,
    ):
        with tarfile.open(fileobj=compressed, mode="w|") as archive:
            for name, recorded in sorted(files.items()):
                source = incoming / relative_path(name)
                require(
                    file_info(source) == recorded,
                    "Compiled input changed before bundling",
                )
                member = tarfile.TarInfo(name)
                member.size, member.mode, member.mtime = (
                    recorded["size_bytes"],
                    source.stat().st_mode & 0o777,
                    0,
                )
                with source.open("rb") as stream:
                    archive.addfile(member, stream)
    require(
        output.stat().st_size <= MAX_TRANSPORT_BYTES,
        "Transport exceeds the GitHub artifact byte bound",
    )
    verify_build(
        incoming,
        args.manifest_sha256,
        args.source_root.resolve(),
        args.commit,
        args.target,
    )
    value = {
        "schema": 1,
        "kind": "codex-prebuilt-transport",
        "target": args.target,
        **{
            key: build[key]
            for key in ["source_commit", "source_tree", "native_tree", "version"]
        },
        "producer": helpers,
        "manifest_sha256": args.manifest_sha256,
        "bundle": file_info(output),
        "files": files,
        "runtime_executed": False,
    }
    verify_archive(output, value)
    write_json(metadata, value)
    return {
        "bundle": str(output),
        **value["bundle"],
        "manifest_sha256": args.manifest_sha256,
        "metadata": str(metadata),
        "metadata_sha256": file_info(metadata)["sha256"],
    }


def verify_archive(path, metadata):
    require(
        file_info(path) == metadata["bundle"]
        and 0 < path.stat().st_size <= MAX_TRANSPORT_BYTES,
        "Original transport bytes changed",
    )
    files, seen, total = metadata["files"], set(), 0
    require(
        isinstance(files, dict) and 0 < len(files) <= MAX_FILES,
        "Invalid transport inventory",
    )
    with tarfile.open(path, "r|gz", tarinfo=BoundedTarInfo) as archive:
        for member in archive:
            require(
                len(seen) < MAX_FILES and member.isfile(),
                "Transport contains excessive entries, links or special files",
            )
            relative_path(member.name)
            require(
                member.name in files
                and member.name not in seen
                and 0 <= member.size <= MAX_BYTES,
                "Unexpected transport member",
            )
            total += member.size
            require(total <= MAX_BYTES, "Transport expansion exceeds its byte bound")
            digest = hashlib.sha256()
            with archive.extractfile(member) as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
            require(
                files[member.name]
                == {"sha256": digest.hexdigest(), "size_bytes": member.size},
                "A bundled input differs from the original receipt",
            )
            seen.add(member.name)
    require(
        seen == set(files) and "build-manifest.json" in seen,
        "Transport omitted compiled inputs",
    )
    require(
        files["build-manifest.json"]["sha256"] == metadata["manifest_sha256"],
        "Transport manifest digest differs",
    )


def descriptor(args):
    require(
        SHA256.fullmatch(args.metadata_sha256)
        and file_info(args.metadata)["sha256"] == args.metadata_sha256,
        "Original transport metadata changed",
    )
    value = load_json(args.metadata)
    require(
        value.get("schema") == 1
        and value.get("kind") == "codex-prebuilt-transport"
        and value.get("target") in TARGETS
        and value.get("runtime_executed") is False,
        "Invalid local transport metadata",
    )
    require(
        value["producer"] == helper_identity(),
        "Descriptor helpers differ from the original bundle",
    )
    require(
        re.fullmatch(
            r"https://api\.github\.com/repos/holyglory/codex/releases/assets/[1-9][0-9]*",
            args.asset_url,
        ),
        "Use the owned GitHub release-asset API URL",
    )
    verify_archive(args.bundle, value)
    result = {
        "target": value["target"],
        "helper_sha": value["producer"]["helper_commit"],
        "url": args.asset_url,
        **value["bundle"],
        "manifest_sha256": value["manifest_sha256"],
    }
    write_json(args.output, result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    phases = parser.add_subparsers(dest="phase", required=True)
    create = phases.add_parser(
        "bundle", help="Create canonical immutable transport bytes and local metadata"
    )
    for name in ["source-root", "input", "output", "metadata"]:
        create.add_argument("--" + name, type=Path, required=True)
    create.add_argument("--commit", required=True)
    create.add_argument("--target", choices=TARGETS, required=True)
    create.add_argument("--manifest-sha256", required=True)
    describe = phases.add_parser(
        "descriptor", help="Bind original local bytes to the asset URL after upload"
    )
    for name in ["bundle", "metadata", "output"]:
        describe.add_argument("--" + name, type=Path, required=True)
    describe.add_argument("--metadata-sha256", required=True)
    describe.add_argument("--asset-url", required=True)
    args = parser.parse_args()
    print(
        json.dumps(
            {"bundle": bundle, "descriptor": descriptor}[args.phase](args),
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
