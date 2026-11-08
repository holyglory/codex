#!/usr/bin/env bash
# Native target only: generate one V8 ABI pair, test it, then package that source.
set -euo pipefail
: "${TARGET:?}" "${BAZEL_PLATFORM:?}" "${BAZEL_CONFIG:?}" "${SOURCE_VERSION:?}"
if [[ "$TARGET" == *-linux-musl ]]; then
  # Keep Zig's unused CRT option from failing jemalloc's -Werror probes.
  jemalloc_ldflags="$(printf '%s_LDFLAGS' "$TARGET" | tr '[:lower:]-' '[:upper:]_')"
  export "$jemalloc_ldflags=${!jemalloc_ldflags:-} -Wno-unused-command-line-argument"
  # Zig opens more than 1024 Rust objects when linking the release binaries.
  musl_open_files="$(ulimit -S -n)"
  if [[ "$musl_open_files" != unlimited ]] && (( musl_open_files < 16384 )); then
    ulimit -S -n 16384
  fi
fi
root="$(git rev-parse --show-toplevel)"
dist="$root/native-dist/$TARGET"
pair="$RUNNER_TEMP/downstream-v8/$TARGET"
mkdir -p "$dist/logs" "$pair"
run_check() {
  local name="$1"
  shift
  "$@" 2>&1 | tee "$dist/logs/$name.log"
}
run_check v8-source-pair python3 "$root/.github/scripts/rusty_v8_bazel.py" stage-release-pair \
  --platform "$BAZEL_PLATFORM" --target "$TARGET" --sandbox \
  --compilation-mode opt --bazel-config ci --bazel-config "$BAZEL_CONFIG" --output-dir "$pair"
export RUSTY_V8_ARCHIVE="$pair/librusty_v8_ptrcomp_sandbox_release_${TARGET}.a.gz"
export RUSTY_V8_SRC_BINDING_PATH="$pair/src_binding_ptrcomp_sandbox_release_${TARGET}.rs"
test -s "$RUSTY_V8_ARCHIVE"
test -s "$RUSTY_V8_SRC_BINDING_PATH"
# The copied ABI pair is independent of Bazel; release its resident server before Cargo.
run_check v8-bazel-shutdown env -u BUILDBUDDY_API_KEY python3 "$root/.github/scripts/run_bazel_with_buildbuddy.py" shutdown
cd "$root/codex-rs"
run_check v8-poc just test --locked -p codex-v8-poc --target "$TARGET" --run-ignored all
run_check product-regressions just test --locked --target "$TARGET" \
  -p codex-code-mode-runtime -p codex-account-registry -p codex-account-selection \
  -p codex-private-storage -p codex-usage
if [[ "$TARGET" == *-linux-musl ]]; then
  run_check bwrap cargo build --locked --target "$TARGET" --release --bin bwrap
  strip --strip-debug --strip-unneeded "$CARGO_TARGET_DIR/$TARGET/release/bwrap"
  CODEX_BWRAP_SHA256="$(python3 -c 'import hashlib, pathlib, sys; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())' "$CARGO_TARGET_DIR/$TARGET/release/bwrap")"
  export CODEX_BWRAP_SHA256
fi
run_check product-build cargo build --locked --target "$TARGET" --release \
  --bin codex --bin codex-code-mode-host --bin codex-exec-server
release="$CARGO_TARGET_DIR/$TARGET/release"
package_args=(
  --target "$TARGET"
  --package-version "$SOURCE_VERSION" --cargo-profile release --strip all
  --entrypoint-bin "$release/codex" --code-mode-host-bin "$release/codex-code-mode-host"
  --exec-server-bin "$release/codex-exec-server"
)
if [[ "$TARGET" == *-linux-musl ]]; then
  package_args+=(--bwrap-bin "$release/bwrap")
fi
package_args+=(
  --package-dir "$dist/package"
  --archive-output "$dist/codex-package-$TARGET.tar.gz"
  --archive-output "$dist/codex-package-$TARGET.tar.zst"
)
run_check canonical-package just assemble-codex-package "${package_args[@]}"
run_check native-version "$dist/package/bin/codex" --version
if [[ "$(cat "$dist/logs/native-version.log")" != "codex-cli $SOURCE_VERSION" ]]; then
  echo "Packaged CLI does not report the release version" >&2
  exit 1
fi
run_check native-help "$dist/package/bin/codex" --help
cd "$root"
python3 - "$dist" "$pair" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

dist, pair = map(Path, sys.argv[1:])
def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()
def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()
receipt = {
    "schema": 1, "commit": git("rev-parse", "HEAD"), "source_commit": git("rev-parse", "HEAD"),
    "source_tree": git("rev-parse", "HEAD^{tree}"), "native_tree": git("rev-parse", "HEAD:codex-rs"),
    "v8_tree": git("rev-parse", "HEAD:third_party/v8"),
    "producer": "github-native", "status": "success", "target": os.environ["TARGET"],
    "version": os.environ["SOURCE_VERSION"], "runtime_executed": True,
    "github_run_id": os.environ["GITHUB_RUN_ID"], "github_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
    "checks": [{"name": path.stem, "exit_code": 0, "log_sha256": digest(path)} for path in sorted((dist / "logs").glob("*.log"))],
    "v8_pair": {"profile": "sandbox_release", "source_built": True, "cppgc_ignored_tests_executed": True,
                "files": {path.name: digest(path) for path in sorted(pair.iterdir()) if path.is_file()}},
    "archives": {path.name: {"sha256": digest(path), "size_bytes": path.stat().st_size} for path in sorted(dist.glob("codex-package-*.tar.*"))},
}
(dist / "native-manifest.json").write_text(json.dumps(receipt, indent=2) + "\n")
(dist / "SHA256SUMS").write_text("".join(f"{digest(path)}  {path.name}\n" for path in sorted(dist.iterdir()) if path.is_file() and path.name != "SHA256SUMS"))
PY
