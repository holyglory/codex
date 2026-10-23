#!/usr/bin/env bash
# Use an unprivileged bubblewrap mount namespace, then run validation as the
# invoking non-root user.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_uid=$(id -u)
test_gid=$(id -g)
test_user=$(id -un)
test "$test_uid" != 0

if [[ "${1:-}" != --inside ]]; then
  state=$(realpath -e "${1:?Provide an existing build-state directory outside the checkout}")
  shift
  case "$state/" in
    "$root/"*|/tmp/*)
      echo 'Build state must be outside the checkout and /tmp' >&2
      exit 1
      ;;
  esac

  # The lock is held by the invoking account and inherited by bubblewrap. It
  # protects warm Cargo/Bazel stores without requiring a privileged helper.
  exec 9>"$state/environment.lock"
  if ! flock -n 9; then
    echo 'Candidate environment is in use; inspect its active run or retained Bazel server before retrying.' >&2
    exit 1
  fi

  # Stable mounts let a warm Bazel server see the same backing files on later
  # runs. The directories are created before entering the namespace because
  # the coordinator executor does not grant mount privileges to the command.
  environment_root="$state/test-environment"
  mkdir -p "$environment_root/tmp" "$environment_root/system-config"
  test -z "$(ls -A "$environment_root/system-config")"

  test_home=$(getent passwd "$test_uid" | cut -d: -f6)
  test -n "$test_home"
  socket_root="/tmp/codex-daemon-$test_uid"
  exec bwrap \
    --die-with-parent \
    --unshare-user \
    --uid "$test_uid" \
    --gid "$test_gid" \
    --bind / / \
    --bind "$environment_root/tmp" /tmp \
    --tmpfs "$socket_root" \
    --ro-bind "$environment_root/system-config" /etc/codex \
    --dev /dev \
    --proc /proc \
    --chdir "$root" \
    --unsetenv OPENAI_API_KEY \
    --unsetenv CODEX_API_KEY \
    --unsetenv CODEX_ACCESS_TOKEN \
    --unsetenv CODEX_HOME \
    --unsetenv SUDO_USER \
    --unsetenv SUDO_UID \
    --unsetenv SUDO_GID \
    --unsetenv SUDO_COMMAND \
    -- bash "$root/scripts/run_local_candidate.sh" --inside "$state" "$test_uid" "$test_gid" "$@"
fi

shift
state=$(realpath -e "${1:?Provide an existing build-state directory outside the checkout}")
shift
expected_uid=${1:?Missing invoking UID}
expected_gid=${2:?Missing invoking GID}
shift 2
test "$(id -u)" = "$expected_uid"
test "$(id -g)" = "$expected_gid"
test "$expected_uid" != 0
test_uid=$expected_uid
test_gid=$expected_gid
test_user=$(id -un)
socket_root="/tmp/codex-daemon-$test_uid"

# Bubblewrap has already created the mounts before dropping to the invoking
# UID. Keep these checks close to the command so a changed sandbox invocation
# fails before it can start a build.
chmod 0700 "$socket_root"
test ! -e /etc/codex/config.toml
test "$(findmnt -T "$socket_root" -n -o FSTYPE)" = tmpfs
test "$(stat -c %a "$socket_root")" = 700
test -w /tmp
test_home=$(getent passwd "$test_uid" | cut -d: -f6)
test -n "$test_home"
cd "$root"

if [[ "${1:-}" == --command ]]; then
  shift
  test "$#" -gt 0
  exec env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN \
    HOME="$test_home" USER="$test_user" LOGNAME="$test_user" \
    PATH="$test_home/.cargo/bin:$test_home/.local/bin:$PATH" \
    TMPDIR=/tmp PYTHONDONTWRITEBYTECODE=1 \
    python3 scripts/local_candidate.py run --state-dir "$state" -- "$@"
fi
exec env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN \
  HOME="$test_home" USER="$test_user" LOGNAME="$test_user" \
  PATH="$test_home/.cargo/bin:$test_home/.local/bin:$PATH" \
  TMPDIR=/tmp PYTHONDONTWRITEBYTECODE=1 \
  python3 scripts/local_candidate.py check --state-dir "$state" "$@"
