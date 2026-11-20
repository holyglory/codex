#!/usr/bin/env python3
"""Build source-pinned musl dependencies in an explicit user-owned target cache.

Requires Zig 0.14.0, Perl, make and a host C compiler already installed. Never
installs system packages or falls back to a host musl-gcc for another target.
"""

import argparse
import os
from pathlib import Path
import re
import shlex
import shutil
import struct
import subprocess
import sys
from urllib.request import urlopen

from local_native_artifacts import (
    TARGETS,
    file_info,
    load_json,
    require,
    safe_extract,
    write_json,
)


def pins(root):
    result = {}
    for name, helper in [
        ("openssl", "install-musl-openssl.sh"),
        ("libcap", "install-musl-build-tools.sh"),
    ]:
        text = (root / ".github/scripts" / helper).read_text()
        version = re.search(rf'^{name}_version="([0-9.]+)"$', text, re.MULTILINE)
        digest = re.search(rf'^{name}_sha256="([0-9a-f]{{64}})"$', text, re.MULTILINE)
        require(version and digest, f"Missing source-pinned {name} inputs")
        result[name] = {
            "version": version[1],
            "sha256": digest[1],
            "helper": file_info(root / ".github/scripts" / helper),
        }
    return result


def verify_archive_machine(path, target):
    """Check every ELF member, including archives assembled on another host."""
    objects = 0
    with path.open("rb") as stream:
        require(stream.read(8) == b"!<arch>\n", "Expected a static archive")
        while header := stream.read(60):
            require(
                len(header) == 60 and header[-2:] == b"`\n",
                "Invalid static archive header",
            )
            size = int(header[48:58].decode().strip())
            start = stream.tell()
            prefix = stream.read(min(size, 20))
            if prefix.startswith(b"\x7fELF"):
                require(
                    len(prefix) == 20
                    and prefix[4:6] == b"\x02\x01"
                    and struct.unpack_from("<H", prefix, 18)[0] == TARGETS[target],
                    "Dependency archive contains the wrong architecture",
                )
                objects += 1
            stream.seek(start + size + size % 2)
    require(objects > 0, "Dependency archive contains no target objects")


def prepare(root, target, cache, zig, *, jobs=None, run=subprocess.run):
    require(sys.platform == "linux", "Local musl dependency preparation requires Linux")
    import fcntl

    require(
        target in TARGETS and cache.is_absolute() and zig.is_absolute(),
        "Use an explicit target and absolute cache/tool paths",
    )
    require(
        subprocess.check_output([str(zig), "version"], text=True).strip() == "0.14.0",
        "Zig 0.14.0 is required",
    )
    host_cc = shutil.which("cc")
    require(
        host_cc and all(shutil.which(tool) for tool in ["make", "perl", "bash"]),
        "Install host cc, make, Perl and bash before preparing dependencies",
    )
    selected = pins(root)
    directory = cache / target
    directory.mkdir(parents=True, exist_ok=True)
    require(
        directory.stat().st_uid == os.getuid(),
        "The dependency cache must be owned by this user",
    )
    with (directory / ".prepare.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        receipt = directory / "dependencies.json"
        identity = {
            "target": target,
            "zig": str(zig),
            "zig_sha256": file_info(zig)["sha256"],
            "pins": selected,
            "preparer": file_info(Path(__file__)),
            "wrapper_source": file_info(
                root / ".github/scripts/prepare_downstream_musl.py"
            ),
        }
        if receipt.exists():
            value = load_json(receipt)
            require(
                value["identity"] == identity,
                "Cache inputs changed; select a new cache directory",
            )
            require(
                all(
                    file_info(directory / path) == info
                    for path, info in value["files"].items()
                ),
                "Cached dependency changed",
            )
            for name in ["openssl", "libcap"]:
                verify_archive_machine(Path(value["archives"][name]), target)
            return value["environment"]
        require(
            not (directory / "environment.env").exists(),
            "Preserve incomplete dependency work; choose a new cache directory",
        )
        env = {**os.environ, "PATH": str(zig.parent) + os.pathsep + os.environ["PATH"]}
        output_env = directory / "environment.env"
        run(
            [
                sys.executable,
                str(root / ".github/scripts/prepare_downstream_musl.py"),
                "--target",
                target,
                "--output-dir",
                str(directory / "bin"),
                "--github-env",
                str(output_env),
            ],
            env=env,
            check=True,
        )
        values = dict(
            line.split("=", 1) for line in output_env.read_text().splitlines()
        )
        for name in ["ar", "ranlib"]:
            path = directory / "bin" / name
            path.write_text(f'#!/bin/sh\nexec {shlex.quote(str(zig))} {name} "$@"\n')
            path.chmod(0o755)
        suffix = target.replace("-", "_")
        compiler = values[f"CC_{suffix}"]
        env.update(
            CC=compiler,
            AR=str(directory / "bin/ar"),
            RANLIB=str(directory / "bin/ranlib"),
        )
        prefixes = {}
        for name, pin in selected.items():
            version = pin["version"]
            extension = "tar.gz" if name == "openssl" else "tar.xz"
            archive = directory / f"{name}-{version}.{extension}"
            url = (
                f"https://github.com/openssl/openssl/releases/download/openssl-{version}/{archive.name}"
                if name == "openssl"
                else f"https://mirrors.edge.kernel.org/pub/linux/libs/security/linux-privs/libcap2/{archive.name}"
            )
            with urlopen(url, timeout=60) as response, archive.open("xb") as output:
                size = 0
                while chunk := response.read(1024 * 1024):
                    size += len(chunk)
                    require(
                        size <= 128 * 1024**2, "Dependency download exceeds its bound"
                    )
                    output.write(chunk)
            require(
                file_info(archive)["sha256"] == pin["sha256"],
                "Pinned dependency download differs",
            )
            extracted = directory / f"source-{name}"
            safe_extract(archive, extracted, max_files=10000)
            source = extracted / f"{name}-{version}"
            prefix = directory / f"{name}-{version}" / "prefix"
            prefixes[name] = prefix
            parallel = f"-j{jobs or os.cpu_count() or 1}"
            if name == "openssl":
                # Reuse the checked-source options rather than keeping another ABI policy.
                helper = (root / ".github/scripts/install-musl-openssl.sh").read_text()
                options = re.search(
                    r"\n\s+(no-shared.*?-DOPENSSL_NO_SECURE_MEMORY)", helper, re.DOTALL
                )
                require(
                    options, "OpenSSL configuration is missing from its source helper"
                )
                flags = shlex.split(options[1].replace("\\\n", " "))
                configure_target = (
                    "linux-aarch64" if target.startswith("aarch64") else "linux-x86_64"
                )
                run(
                    [
                        "perl",
                        "./Configure",
                        configure_target,
                        f"--prefix={prefix}",
                        "--openssldir=/usr/local/ssl",
                        "--libdir=lib",
                        *flags,
                    ],
                    cwd=source,
                    env=env,
                    check=True,
                )
                run(["make", parallel, "build_libs"], cwd=source, env=env, check=True)
                run(["make", "install_dev"], cwd=source, env=env, check=True)
            else:
                run(
                    [
                        "make",
                        "-C",
                        str(source / "libcap"),
                        parallel,
                        "libcap.a",
                        f"CC={compiler}",
                        f"BUILD_CC={host_cc}",
                        f"AR={env['AR']}",
                        f"RANLIB={env['RANLIB']}",
                    ],
                    env=env,
                    check=True,
                )
                for folder in ["lib/pkgconfig", "include/sys", "include/linux"]:
                    (prefix / folder).mkdir(parents=True, exist_ok=True)
                for source_path, relative in [
                    (source / "libcap/libcap.a", "lib/libcap.a"),
                    (
                        source / "libcap/include/sys/capability.h",
                        "include/sys/capability.h",
                    ),
                    (
                        source / "libcap/include/uapi/linux/capability.h",
                        "include/linux/capability.h",
                    ),
                ]:
                    shutil.copy2(source_path, prefix / relative)
                (prefix / "lib/pkgconfig/libcap.pc").write_text(
                    f"prefix={prefix}\nlibdir=${{prefix}}/lib\nincludedir=${{prefix}}/include\nName: libcap\nDescription: Linux capabilities\nVersion: {version}\nLibs: -L${{libdir}} -lcap\nCflags: -I${{includedir}}\n"
                )
        archives = {
            "openssl": str(prefixes["openssl"] / "lib/libcrypto.a"),
            "libcap": str(prefixes["libcap"] / "lib/libcap.a"),
        }
        for path in [
            *map(Path, archives.values()),
            prefixes["openssl"] / "lib/libssl.a",
        ]:
            verify_archive_machine(path, target)
        values.update(
            {
                f"{suffix.upper()}_OPENSSL_DIR": str(prefixes["openssl"]),
                f"{suffix.upper()}_OPENSSL_NO_VENDOR": "1",
                f"{suffix.upper()}_OPENSSL_STATIC": "1",
                "PKG_CONFIG_ALLOW_CROSS": "1",
                f"PKG_CONFIG_PATH_{suffix}": str(prefixes["libcap"] / "lib/pkgconfig"),
                f"PKG_CONFIG_LIBDIR_{suffix}": str(
                    prefixes["libcap"] / "lib/pkgconfig"
                ),
                "AWS_LC_SYS_NO_JITTER_ENTROPY": "1",
                f"AWS_LC_SYS_NO_JITTER_ENTROPY_{suffix}": "1",
            }
        )
        values.update(
            {
                f"AR_{suffix}": str(directory / "bin/ar"),
                f"RANLIB_{suffix}": str(directory / "bin/ranlib"),
            }
        )
        retained = [
            path
            for prefix in prefixes.values()
            for path in prefix.rglob("*")
            if path.is_file()
        ]
        retained.extend((directory / "bin").iterdir())
        write_json(
            receipt,
            {
                "schema": 1,
                "identity": identity,
                "environment": values,
                "archives": archives,
                "files": {
                    path.relative_to(directory).as_posix(): file_info(path)
                    for path in retained
                },
            },
        )
        return values


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--zig", type=Path, required=True)
    args = parser.parse_args()
    print(
        __import__("json").dumps(
            prepare(
                args.source_root.resolve(),
                args.target,
                args.cache.resolve(),
                args.zig.resolve(),
            ),
            indent=2,
        )
    )
