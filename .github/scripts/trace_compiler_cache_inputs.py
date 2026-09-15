#!/usr/bin/env python3
"""Record content-free key inputs for two public crates, then invoke sccache."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest_file(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


compiler, *arguments = sys.argv[1:]
crate = (
    arguments[arguments.index("--crate-name") + 1]
    if "--crate-name" in arguments
    else None
)
if (
    crate in ("cfg_if", "codex_utils_absolute_path")
    and "--emit=dep-info,metadata,link" in arguments
):
    target = (
        arguments[arguments.index("--target") + 1]
        if "--target" in arguments
        else "host"
    )
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", target):
        raise RuntimeError("Unexpected diagnostic target")
    sysroot = Path(
        subprocess.check_output([compiler, "--print=sysroot"], text=True).strip()
    )
    libraries = sorted((sysroot / "lib").glob("*.so"))
    externs = {}
    for index, argument in enumerate(arguments):
        if argument == "--extern":
            name, path = arguments[index + 1].split("=", 1)
            externs[name] = digest_file(Path(path))
    cargo = {
        key: digest_bytes(value.encode())
        for key, value in os.environ.items()
        if key.startswith("CARGO_")
        and not re.search(r"TOKEN|SECRET|PASSWORD|CREDENTIAL", key)
        and not key.startswith("CARGO_REGISTRIES_")
        and key
        not in ("CARGO_MAKEFLAGS", "CARGO_BUILD_JOBS", "CARGO_ENCODED_RUSTFLAGS")
    }
    source = next(
        (Path(arg) for arg in arguments if arg.endswith(".rs") and Path(arg).is_file()),
        None,
    )
    snapshot = {
        "crate": crate,
        "target": target,
        "arguments_sha256": digest_bytes(json.dumps(arguments).encode()),
        "argument_hashes": [digest_bytes(arg.encode()) for arg in arguments],
        "cargo_environment": cargo,
        "cwd_sha256": digest_bytes(os.getcwd().encode()),
        "source_sha256": digest_file(source) if source else None,
        "compiler_libraries": {path.name: digest_file(path) for path in libraries},
        "compiler_version_sha256": digest_bytes(
            subprocess.check_output([compiler, "-vV"])
        ),
        "externs": externs,
    }
    output = Path(os.environ["CACHE_KEY_EVIDENCE"])
    output.mkdir(parents=True, exist_ok=True)
    (output / f"{crate}-{target}.json").write_text(
        json.dumps(snapshot, sort_keys=True) + "\n"
    )
    if (
        crate == "codex_utils_absolute_path"
        and os.environ.get("CACHE_STOP_AFTER_SNAPSHOT") == "1"
    ):
        raise SystemExit(73)
raise SystemExit(
    subprocess.run([os.environ["SCCACHE_PATH"], compiler, *arguments]).returncode
)
