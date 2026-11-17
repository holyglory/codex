"""Bounded, source-bound files shared by local compilation and native verification."""

import hashlib
import json
import platform
from pathlib import Path, PurePosixPath
import re
import struct
import subprocess
import tarfile
import tomllib

from local_release_tar import BoundedTarInfo

TARGETS = {"x86_64-unknown-linux-musl": 62, "aarch64-unknown-linux-musl": 183}
MAX_BYTES = 32 * 1024**3
MAX_FILES = 4096
SHA256 = re.compile(r"[0-9a-f]{64}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def file_info(path):
    require(
        path.is_file() and not path.is_symlink(), f"Expected a regular file: {path}"
    )
    with path.open("rb") as stream:
        value = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"sha256": value, "size_bytes": path.stat().st_size}


def load_json(path):
    require(path.stat().st_size <= 1024**2, "Receipt exceeds 1 MiB")
    return json.loads(path.read_text())


def write_json(path, value):
    with path.open("x") as stream:
        stream.write(json.dumps(value, indent=2, sort_keys=True) + "\n")


def relative_path(name):
    require(isinstance(name, str), "Artifact paths must be strings")
    path = PurePosixPath(name)
    require(
        isinstance(name, str)
        and 0 < len(name) <= 512
        and not path.is_absolute()
        and ".." not in path.parts
        and "\\" not in name
        and ":" not in name
        and str(path) == name
        and path.parts,
        "Unsafe artifact path",
    )
    return Path(*path.parts)


def source_identity(root, commit):
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "Expected an exact source commit")

    def git(*args):
        return subprocess.check_output(
            ["git", "-C", str(root), *args], text=True
        ).strip()

    require(
        git("rev-parse", "HEAD") == commit, "Checkout differs from requested source"
    )
    status = subprocess.check_output(
        ["git", "-C", str(root), "status", "--porcelain", "-z"]
    )
    require(
        all(row[3:] == b".devcoordinator.toml" for row in status.split(b"\0") if row),
        "Source checkout has unsealed changes",
    )
    return {
        "source_commit": commit,
        "source_tree": git("rev-parse", "HEAD^{tree}"),
        "native_tree": git("rev-parse", "HEAD:codex-rs"),
        "version": tomllib.loads((root / "codex-rs/Cargo.toml").read_text())[
            "workspace"
        ]["package"]["version"],
    }


def rust_version(root):
    versions = set(
        re.findall(
            r"dtolnay/rust-toolchain@[^\s#]+\s+#\s*(\d+\.\d+\.\d+)\s*$",
            (root / ".github/workflows/rust-release.yml").read_text(),
            re.MULTILINE,
        )
    )
    require(len(versions) == 1, "Release source must declare one exact Rust version")
    return versions.pop()


def native_host(target):
    require(target in TARGETS, "Unsupported local release target")
    machine = {"amd64": "x86_64", "arm64": "aarch64"}.get(
        platform.machine().lower(), platform.machine().lower()
    )
    require(
        platform.system() == "Linux" and machine == target.split("-", 1)[0],
        "Tests require the exact native Linux architecture",
    )
    return {"os": "Linux", "architecture": machine}


def elf_identity(path, target):
    with path.open("rb") as stream:
        header = stream.read(64)
        require(
            len(header) == 64
            and header[:6] == b"\x7fELF\x02\x01"
            and struct.unpack_from("<H", header, 18)[0] == TARGETS[target],
            "Wrong executable architecture",
        )
        offset = struct.unpack_from("<Q", header, 32)[0]
        size, count = struct.unpack_from("<HH", header, 54)
        require(
            size >= 4
            and 0 < count < 4096
            and offset + size * count <= path.stat().st_size,
            "Invalid ELF program headers",
        )
        for index in range(count):
            stream.seek(offset + index * size)
            require(
                struct.unpack("<I", stream.read(4))[0] != 3,
                "Expected a static musl executable",
            )


def safe_extract(archive, destination, *, max_files=MAX_FILES):
    """Extract regular files only, without replacing a previous extraction."""
    require(not destination.exists(), "Preserve the existing extracted artifact")
    with tarfile.open(archive, "r:*", tarinfo=BoundedTarInfo) as source:
        members, total, names = [], 0, set()
        for member in source:
            require(len(members) < max_files, "Archive has too many entries")
            relative_path(member.name.rstrip("/"))
            require(
                member.isdir() or member.isfile(),
                "Archive links and special files are forbidden",
            )
            name = member.name.rstrip("/")
            require(
                name not in names and 0 <= member.size <= MAX_BYTES,
                "Duplicate or oversized archive entry",
            )
            total += member.size
            require(total <= MAX_BYTES, "Archive is too large")
            names.add(name)
            members.append(member)
        destination.mkdir(parents=True)
        source.extractall(destination, members=members, filter="data")


def inventory(directory):
    result, total = {}, 0
    for index, path in enumerate(sorted(directory.rglob("*"))):
        require(
            index < MAX_FILES and not path.is_symlink(),
            "Artifact contains too many entries or links",
        )
        require(path.is_file() or path.is_dir(), "Artifact contains a special file")
        if path.is_file() and path != directory / "build-manifest.json":
            result[path.relative_to(directory).as_posix()] = file_info(path)
            total += result[path.relative_to(directory).as_posix()]["size_bytes"]
    require(total <= MAX_BYTES, "Artifact exceeds its byte bound")
    return result


def verify_build(directory, expected_digest, root, commit, target):
    require(
        target in TARGETS and SHA256.fullmatch(expected_digest),
        "Invalid target or manifest digest",
    )
    path = directory / "build-manifest.json"
    require(
        file_info(path)["sha256"] == expected_digest,
        "Build manifest digest differs from its transport receipt",
    )
    value = load_json(path)
    expected = {
        "schema": 1,
        "kind": "codex-prebuilt-target",
        **source_identity(root, commit),
        "target": target,
        "rust_version": rust_version(root),
        "status": "compiled",
        "runtime_executed": False,
    }
    require(
        {key: value.get(key) for key in expected} == expected,
        "Compiled source identity differs",
    )
    require(
        re.fullmatch(r"\d+\.\d+\.\d+", value.get("nextest_version", "")),
        "Invalid archived Nextest version",
    )
    files = value.get("files")
    require(
        isinstance(files, dict) and 0 < len(files) <= MAX_FILES,
        "Invalid build file inventory",
    )
    for name, recorded in files.items():
        relative_path(name)
        require(
            isinstance(recorded, dict)
            and SHA256.fullmatch(str(recorded.get("sha256", "")))
            and type(recorded.get("size_bytes")) is int
            and 0 <= recorded["size_bytes"] <= MAX_BYTES,
            "Invalid file receipt",
        )
    require(
        files == inventory(directory),
        "A compiled artifact is missing, extra or changed",
    )
    required = {
        f"codex-package-{target}.tar.gz",
        f"tests-{target}.tar.zst",
        "v8-pair.json",
        "nextest-config.toml",
    }
    require(required <= files.keys(), "Compiled handoff is incomplete")
    require(
        value.get("producer", {}).get("helper_files"), "Build omitted helper provenance"
    )
    return value


def check_logs(directory, checks):
    require(isinstance(checks, list) and checks, "Native verification has no checks")
    names = set()
    for check in checks:
        name = check.get("name", "")
        require(
            re.fullmatch(r"[a-z0-9-]+", name)
            and name not in names
            and check.get("exit_code") == 0,
            "Native check did not succeed",
        )
        require(
            file_info(directory / "logs" / f"{name}.log")["sha256"]
            == check["log_sha256"],
            "Native verification log changed",
        )
        names.add(name)
    return names
