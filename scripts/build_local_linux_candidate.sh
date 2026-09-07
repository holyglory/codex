#!/usr/bin/env bash
# Build and smoke-test local artifacts only; never install or publish them.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target=x86_64-unknown-linux-musl
: "${LOCAL_MUSL_V8_ARCHIVE:?Provide the verified musl V8 archive}"
: "${LOCAL_MUSL_V8_BINDING:?Provide the matching musl V8 bindings}"
: "${LOCAL_MUSL_PKG_CONFIG:?Provide the musl libcap pkg-config directory}"
: "${LOCAL_PACKAGE_PYTHON:?Provide the package smoke-test virtualenv Python}"
: "${LOCAL_RELEASE_TARGET:?Provide persistent release build storage}"
for tool in cargo python3 x86_64-linux-musl-gcc strip sha256sum zstd; do
  command -v "$tool" >/dev/null
done
test -f "$LOCAL_MUSL_V8_ARCHIVE"
test -f "$LOCAL_MUSL_V8_BINDING"
test -d "$LOCAL_MUSL_PKG_CONFIG"
"$LOCAL_PACKAGE_PYTHON" -c 'import pytest, zstandard'
if [[ "${1:-}" == --check ]]; then
  exit 0
fi
artifact_root=${1:?Provide an unused absolute artifact directory}
[[ "$artifact_root" = /* ]]
cd "$root"
commit=$(git rev-parse HEAD)
test -z "$(git status --porcelain --untracked-files=all)"
mkdir "$artifact_root"
mkdir "$artifact_root/bin" "$artifact_root/native-dist"
git rev-parse HEAD > "$artifact_root/source-commit.txt"
export CARGO_TARGET_DIR="$LOCAL_RELEASE_TARGET" CARGO_INCREMENTAL=0
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
export CFLAGS='-pthread -idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu'
export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH="$LOCAL_MUSL_PKG_CONFIG"
export PKG_CONFIG_PATH_x86_64_unknown_linux_musl="$LOCAL_MUSL_PKG_CONFIG"
export PKG_CONFIG_LIBDIR_x86_64_unknown_linux_musl="$LOCAL_MUSL_PKG_CONFIG"
export RUSTY_V8_ARCHIVE="$LOCAL_MUSL_V8_ARCHIVE" RUSTY_V8_SRC_BINDING_PATH="$LOCAL_MUSL_V8_BINDING"
export PYTHONDONTWRITEBYTECODE=1
export PYTHONPATH="$root/sdk/python/src:$root/sdk/python/tests"
cd codex-rs
version=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "codex-cli"))')
cargo build --locked --release --target "$target" --bin bwrap
cp --reflink=auto "$CARGO_TARGET_DIR/$target/release/bwrap" "$artifact_root/bin/bwrap"
strip --strip-debug --strip-unneeded "$artifact_root/bin/bwrap"
CODEX_BWRAP_SHA256=$(sha256sum "$artifact_root/bin/bwrap" | cut -d ' ' -f 1)
export CODEX_BWRAP_SHA256
cargo build --locked --release --target "$target" --bin codex --bin codex-app-server --bin codex-code-mode-host
for binary in codex codex-app-server codex-code-mode-host; do
  cp --reflink=auto "$CARGO_TARGET_DIR/$target/release/$binary" "$artifact_root/bin/$binary"
done
test "$("$artifact_root/bin/codex" --version)" = "codex-cli $version"
python3 "$root/scripts/check_fork_update.py" --codex-binary "$artifact_root/bin/codex"
bash "$root/.github/scripts/archive-release-symbols-and-strip-binaries.sh" \
  --target "$target" --artifact-name "$target" --release-dir "$artifact_root/bin" \
  --archive-dir "$artifact_root/native-dist" --binaries 'codex codex-app-server codex-code-mode-host'
for variant in codex codex-app-server; do
  python3 "$root/scripts/build_codex_package.py" --target "$target" --variant "$variant" \
    --package-version "$version" --entrypoint-bin "$artifact_root/bin/$variant" \
    --code-mode-host-bin "$artifact_root/bin/codex-code-mode-host" --bwrap-bin "$artifact_root/bin/bwrap" \
    --archive-output "$artifact_root/native-dist/$variant-package-$target.tar.gz" \
    --archive-output "$artifact_root/native-dist/$variant-package-$target.tar.zst"
done
(
  cd "$artifact_root/native-dist"
  sha256sum ./*.tar.gz ./*.tar.zst > SHA256SUMS
  sha256sum --check SHA256SUMS
)
cd "$root"
result=0
for compression in gzip zstd; do
  extension=gz
  [[ "$compression" != zstd ]] || extension=zst
  "$LOCAL_PACKAGE_PYTHON" -m pytest -q -p no:cacheprovider scripts/codex_package/smoke_tests \
    --compression "$compression" --package-target "$target" \
    --cli-archive "$artifact_root/native-dist/codex-package-$target.tar.$extension" \
    --app-server-archive "$artifact_root/native-dist/codex-app-server-package-$target.tar.$extension" \
    --symbols-archive "$artifact_root/native-dist/codex-symbols-$target.tar.gz" \
    --junitxml "$artifact_root/smoke-$compression.xml" || result=1
done
python3 scripts/local_candidate.py verify-package --directory "$artifact_root" || result=1
test "$(git rev-parse HEAD)" = "$commit"
test -z "$(git status --porcelain --untracked-files=all)"
exit "$result"
