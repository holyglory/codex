#!/usr/bin/env bash
# Use a private mount namespace, then run validation as the invoking non-root user.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test "$(id -u)" = 0
test_user=${SUDO_USER:?Run through sudo from the intended build account}
test "$(id -u "$test_user")" != 0
if [[ "${1:-}" != --inside ]]; then
  exec unshare --mount --propagation private bash "$root/scripts/run_local_candidate.sh" --inside "$@"
fi
shift
test "$(readlink /proc/self/ns/mnt)" != "$(readlink /proc/1/ns/mnt)"
state=$(realpath -e "${1:?Provide an existing build-state directory outside the checkout}")
shift
case "$state/" in "$root/"*|/tmp/*) echo 'Build state must be outside the checkout and /tmp' >&2; exit 1 ;; esac
exec 9>"$state/environment.lock"
flock -n 9
# Stable mounts let a warm Bazel server see the same backing files on later runs.
environment_root="$state/test-environment"
runuser -u "$test_user" -- mkdir -p "$environment_root/tmp" "$environment_root/system-config"
test -z "$(ls -A "$environment_root/system-config")"
mount --bind "$environment_root/tmp" /tmp
mount --bind "$environment_root/system-config" /etc/codex
mount -o remount,bind,ro /etc/codex
test ! -e /etc/codex/config.toml
test_home=$(getent passwd "$test_user" | cut -d: -f6)
cd "$root"
exec runuser -u "$test_user" -- env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN \
  PATH="$test_home/.cargo/bin:$test_home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
  TMPDIR=/tmp PYTHONDONTWRITEBYTECODE=1 \
  python3 scripts/local_candidate.py check --state-dir "$state" "$@"
