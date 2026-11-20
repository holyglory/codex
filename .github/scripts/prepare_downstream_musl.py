#!/usr/bin/env python3
"""Use one target-scoped Zig toolchain for musl C++, Rust linking and CRTs."""

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess

TARGETS = ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl")


def prepare(target: str, output_dir: Path, github_env: Path) -> None:
    if target not in TARGETS:
        raise ValueError(f"Unsupported musl target: {target}")
    zig = shutil.which("zig")
    if (
        zig is None
        or subprocess.check_output([zig, "version"], text=True).strip() != "0.14.0"
    ):
        raise ValueError("Install the pinned Zig 0.14.0 before preparing musl tools")
    output_dir.mkdir(parents=True, exist_ok=True)
    zig_target = target.replace("-unknown", "")
    wrappers = {}
    for name, subcommand in (("zigcc", "cc"), ("zigcxx", "c++")):
        wrapper = output_dir / name
        wrapper.write_text(
            f"""#!/usr/bin/env bash
set -euo pipefail
args=()
skip=0
compile=0
pending_include=0
for arg in "$@"; do
  if [[ "$skip" -eq 1 ]]; then skip=0; continue; fi
  if [[ "$pending_include" -eq 1 ]]; then
    pending_include=0
    if [[ "$arg" == /usr/include || "$arg" == /usr/include/* ]]; then
      args+=("-idirafter" "$arg")
    else
      args+=("-I" "$arg")
    fi
    continue
  fi
  case "$arg" in
    --target|-target) skip=1; continue ;;
    --target=*|-target=*) continue ;;
    -I) pending_include=1; continue ;;
    -I/usr/include|-I/usr/include/*) args+=("-idirafter" "${{arg#-I}}"); continue ;;
    -Wp,-U_FORTIFY_SOURCE) args+=("-U_FORTIFY_SOURCE"); continue ;;
    -c) compile=1 ;;
  esac
  # Zig owns the target CRT. Mixing Rust's self-contained startup objects
  # with Zig's copies gives duplicate _start, _init and _fini definitions.
  if [[ "$arg" == */rustlib/*/self-contained/*.o ]]; then continue; fi
  args+=("$arg")
done
extra=()
if [[ "$compile" -eq 0 ]]; then extra+=("-nostartfiles"); fi
exec {shlex.quote(zig)} {subcommand} -target {shlex.quote(zig_target)} "${{args[@]}}" -fno-sanitize=undefined "${{extra[@]}}"
""",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        wrappers[name] = str(wrapper.resolve())
    suffix = target.replace("-", "_")
    # Preserve the upstream musl helper's BoringSSL warning policy on ARM64.
    flags = (
        "-pthread -Wno-error=frame-larger-than" if target.startswith("aarch64") else ""
    )
    values = {
        f"CC_{suffix}": wrappers["zigcc"],
        f"CXX_{suffix}": wrappers["zigcxx"],
        f"CARGO_TARGET_{suffix.upper()}_LINKER": wrappers["zigcc"],
        # DuckDB is separate from V8's bundled Chromium runtime. Select Zig's
        # target libc++ instead of resolving a host glibc libstdc++ archive.
        f"CXXSTDLIB_{suffix}": "c++",
        f"CFLAGS_{suffix}": flags,
        f"CXXFLAGS_{suffix}": flags,
    }
    with github_env.open("a", encoding="utf-8") as output:
        for name, value in values.items():
            output.write(f"{name}={value}\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--github-env", type=Path, default=os.environ.get("GITHUB_ENV"))
    args = parser.parse_args()
    if args.github_env is None:
        parser.error("--github-env or GITHUB_ENV is required")
    prepare(args.target, args.output_dir, args.github_env)


if __name__ == "__main__":
    main()
