#!/usr/bin/env bash
# Native target phases: build and hand off one V8 ABI pair, then test and package.
set -euo pipefail
: "${TARGET:?}" "${BAZEL_PLATFORM:?}" "${BAZEL_CONFIG:?}" "${SOURCE_VERSION:?}"
: "${V8_PRODUCER_COMMIT:=}"
: "${V8_PRODUCER_RUN_ID:=}"

phase="${1:-full}"
if [[ ! "$phase" =~ ^(full|source-v8|verify-pair|package)$ ]]; then
  echo "usage: $0 [full|source-v8|verify-pair|package]" >&2
  exit 2
fi

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
handoff="${V8_HANDOFF_DIR:-$RUNNER_TEMP/downstream-v8/$TARGET}"
pair="$handoff/pair"
receipt="$handoff/v8-pair.json"
mkdir -p "$dist/logs" "$handoff/logs" "$pair"

run_check() {
  local name="$1"
  shift
  local log_dir="${LOG_DIR:-$dist/logs}"
  mkdir -p "$log_dir"
  "$@" 2>&1 | tee "$log_dir/$name.log"
}

write_pair_receipt() {
  python3 - "$root" "$pair" "$receipt" <<'PY'
import hashlib
import json
from pathlib import Path
import os
import subprocess
import sys

root, pair, receipt = map(Path, sys.argv[1:])
sys.path.insert(0, str(root / "scripts"))
from release_identity import v8_input_manifest

def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()

def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

target = os.environ["TARGET"]
archive = f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz"
binding = f"src_binding_ptrcomp_sandbox_release_{target}.rs"
checksums = f"rusty_v8_ptrcomp_sandbox_release_{target}.sha256"
expected = {archive, binding, checksums}
actual = {path.name for path in pair.iterdir()}
if actual != expected:
    raise SystemExit(f"V8 pair files differ from the exact expected set: {sorted(actual)}")
for name in expected:
    path = pair / name
    if not path.is_file() or path.is_symlink():
        raise SystemExit(f"V8 pair file is not a regular file: {name}")
log_names = {"v8-source-pair.log", "v8-bazel-shutdown.log"}
actual_logs = {path.name for path in (pair.parent / "logs").iterdir()}
if actual_logs != log_names:
    raise SystemExit(f"V8 logs differ from the exact expected set: {sorted(actual_logs)}")
for name in log_names:
    path = pair.parent / "logs" / name
    if not path.is_file() or path.is_symlink():
        raise SystemExit(f"V8 log is not a regular file: {name}")
(pair / checksums).read_text(encoding="utf-8")
receipt_payload = {
    "schema": 1,
    "status": "success",
    "source_commit": git("rev-parse", "HEAD"),
    "source_tree": git("rev-parse", "HEAD^{tree}"),
    "native_tree": git("rev-parse", "HEAD:codex-rs"),
    "v8_tree": git("rev-parse", "HEAD:third_party/v8"),
    "v8_inputs": v8_input_manifest(git("rev-parse", "HEAD")),
    "github_run_id": os.environ.get("GITHUB_RUN_ID"),
    "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
    "target": target,
    "profile": "sandbox_release",
    "source_built": True,
    "files": {name: digest(pair / name) for name in sorted(expected)},
    "logs": {name: digest(pair.parent / "logs" / name) for name in sorted(log_names)},
}
Path(receipt).write_text(json.dumps(receipt_payload, indent=2) + "\n", encoding="utf-8")
PY
}

verify_pair() {
  python3 - "$root" "$pair" "$receipt" "$TARGET" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

root, pair, receipt_path, target = map(Path, sys.argv[1:])

def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()

def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
archive = f"librusty_v8_ptrcomp_sandbox_release_{target}.a.gz"
binding = f"src_binding_ptrcomp_sandbox_release_{target}.rs"
checksums = f"rusty_v8_ptrcomp_sandbox_release_{target}.sha256"
expected = {archive, binding, checksums}
expected_identity = {
    "schema": 1,
    "status": "success",
    "target": str(target),
    "profile": "sandbox_release",
    "source_built": True,
}
if {key: receipt.get(key) for key in expected_identity} != expected_identity:
    raise SystemExit("V8 pair receipt identity is invalid")
producer_commit = os.environ.get("V8_PRODUCER_COMMIT", "")
if producer_commit:
    sys.path.insert(0, str(root / "scripts"))
    from release_identity import v8_input_manifest

    producer = {
        "source_commit": producer_commit,
        "source_tree": git("rev-parse", f"{producer_commit}^{{tree}}"),
        "native_tree": git("rev-parse", f"{producer_commit}:codex-rs"),
        "v8_tree": git("rev-parse", f"{producer_commit}:third_party/v8"),
    }
    if {key: receipt.get(key) for key in producer} != producer:
        raise SystemExit("V8 pair receipt producer identity differs from its run")
    producer_run_id = os.environ.get("V8_PRODUCER_RUN_ID", "")
    if producer_run_id and receipt.get("github_run_id") not in (None, producer_run_id):
        raise SystemExit("V8 pair receipt run identity differs from its producer")
    if git("rev-parse", "HEAD:third_party/v8") != receipt.get("v8_tree"):
        raise SystemExit("V8 pair source inputs differ from the checked out candidate")
    expected_inputs = v8_input_manifest(producer_commit)
    if v8_input_manifest(git("rev-parse", "HEAD")) != expected_inputs:
        raise SystemExit("V8 pair locked inputs differ from the checked out candidate")
    if receipt.get("v8_inputs") not in (None, expected_inputs):
        raise SystemExit("V8 pair receipt input manifest differs from its producer")
else:
    current = {
        "source_commit": git("rev-parse", "HEAD"),
        "source_tree": git("rev-parse", "HEAD^{tree}"),
        "native_tree": git("rev-parse", "HEAD:codex-rs"),
        "v8_tree": git("rev-parse", "HEAD:third_party/v8"),
    }
    if {key: receipt.get(key) for key in current} != current:
        raise SystemExit(
            "V8 pair receipt source identity differs from the checked out source"
        )
files = receipt.get("files")
if not isinstance(files, dict) or set(files) != expected:
    raise SystemExit("V8 pair receipt does not name the exact archive, binding and checksum files")
logs = receipt.get("logs")
log_names = {"v8-source-pair.log", "v8-bazel-shutdown.log"}
if not isinstance(logs, dict) or set(logs) != log_names:
    raise SystemExit("V8 pair receipt does not name the exact build and shutdown logs")
actual_pair_entries = {path.name for path in pair.iterdir()}
if actual_pair_entries != expected:
    raise SystemExit("V8 pair handoff contains unexpected entries")
for name in expected:
    path = pair / name
    if not path.is_file() or path.is_symlink() or digest(path) != files.get(name):
        raise SystemExit(f"V8 pair handoff file changed: {name}")
actual_log_entries = {path.name for path in receipt_path.parent.joinpath("logs").iterdir()}
if actual_log_entries != log_names:
    raise SystemExit("V8 pair handoff contains unexpected log entries")
for name in log_names:
    path = receipt_path.parent / "logs" / name
    if not path.is_file() or path.is_symlink() or digest(path) != logs.get(name):
        raise SystemExit(f"V8 build log changed: {name}")
checksum_lines = (pair / checksums).read_text(encoding="utf-8").splitlines()
expected_lines = [f"{files[name]}  {name}" for name in (archive, binding)]
if checksum_lines != expected_lines:
    raise SystemExit("V8 pair checksum manifest differs from its receipt")
PY
  export RUSTY_V8_ARCHIVE="$pair/librusty_v8_ptrcomp_sandbox_release_${TARGET}.a.gz"
  export RUSTY_V8_SRC_BINDING_PATH="$pair/src_binding_ptrcomp_sandbox_release_${TARGET}.rs"
}

source_v8() {
  LOG_DIR="$handoff/logs" run_check v8-source-pair python3 "$root/.github/scripts/rusty_v8_bazel.py" stage-release-pair \
    --platform "$BAZEL_PLATFORM" --target "$TARGET" --sandbox \
    --compilation-mode opt --bazel-config ci --bazel-config "$BAZEL_CONFIG" --output-dir "$pair"
  test -s "$pair/librusty_v8_ptrcomp_sandbox_release_${TARGET}.a.gz"
  test -s "$pair/src_binding_ptrcomp_sandbox_release_${TARGET}.rs"
  test -s "$pair/rusty_v8_ptrcomp_sandbox_release_${TARGET}.sha256"
  # The copied ABI pair is independent of Bazel; release its resident server before Cargo.
  LOG_DIR="$handoff/logs" run_check v8-bazel-shutdown env -u BUILDBUDDY_API_KEY python3 "$root/.github/scripts/run_bazel_with_buildbuddy.py" shutdown
  write_pair_receipt
  verify_pair
}

package_native() {
  verify_pair
  run_check v8-bazel-shutdown-package env -u BUILDBUDDY_API_KEY python3 "$root/.github/scripts/run_bazel_with_buildbuddy.py" shutdown
  shopt -s nullglob
  for log in "$handoff/logs"/*.log; do
    cp "$log" "$dist/logs/"
  done
  shopt -u nullglob
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
  python3 - "$dist" "$pair" "$receipt" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

dist, pair, receipt_path = map(Path, sys.argv[1:])

def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()

def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()

pair_receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
receipt = {
    "schema": 1, "commit": git("rev-parse", "HEAD"), "source_commit": git("rev-parse", "HEAD"),
    "source_tree": git("rev-parse", "HEAD^{tree}"), "native_tree": git("rev-parse", "HEAD:codex-rs"),
    "v8_tree": git("rev-parse", "HEAD:third_party/v8"),
    "producer": "github-native", "status": "success", "target": os.environ["TARGET"],
    "version": os.environ["SOURCE_VERSION"], "runtime_executed": True,
    "github_run_id": os.environ["GITHUB_RUN_ID"], "github_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
    "checks": [{"name": path.stem, "exit_code": 0, "log_sha256": digest(path)} for path in sorted((dist / "logs").glob("*.log"))],
    "v8_pair": {"profile": pair_receipt["profile"], "source_built": True, "cppgc_ignored_tests_executed": True,
                "source_commit": pair_receipt["source_commit"], "source_tree": pair_receipt["source_tree"],
                "native_tree": pair_receipt["native_tree"], "v8_tree": pair_receipt["v8_tree"],
                "v8_inputs": pair_receipt.get("v8_inputs"), "reused": bool(os.environ.get("V8_PRODUCER_COMMIT")),
                "producer_run_id": os.environ.get("V8_PRODUCER_RUN_ID") or pair_receipt.get("github_run_id"),
                "producer_run_attempt": pair_receipt.get("github_run_attempt"),
                "files": pair_receipt["files"]},
    "archives": {path.name: {"sha256": digest(path), "size_bytes": path.stat().st_size} for path in sorted(dist.glob("codex-package-*.tar.*"))},
}
(dist / "native-manifest.json").write_text(json.dumps(receipt, indent=2) + "\n")
(dist / "SHA256SUMS").write_text("".join(f"{digest(path)}  {path.name}\n" for path in sorted(dist.iterdir()) if path.is_file() and path.name != "SHA256SUMS"))
PY
}

case "$phase" in
  source-v8)
    source_v8
    ;;
  verify-pair)
    verify_pair
    ;;
  package)
    package_native
    ;;
  full)
    source_v8
    package_native
    ;;
 esac
