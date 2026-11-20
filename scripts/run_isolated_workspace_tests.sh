#!/usr/bin/env bash
# Invoke only through the existing governed check; no host mount is changed.
set -euo pipefail

if (( $# < 2 )); then
  echo 'Usage: run_isolated_workspace_tests.sh EXISTING_BULK_SCRATCH COMMAND [ARGS...]' >&2
  exit 2
fi
cxm_test_scratch=$(realpath -e -- "$1")
shift
test -d "$cxm_test_scratch"
test "$cxm_test_scratch" != /

# A previous invocation may have left repository markers in shared scratch.
# Give this invocation an empty /tmp so ancestry discovery sees only its fixtures.
cxm_test_invocation=$(mktemp -d -- "$cxm_test_scratch/invocation.XXXXXXXX")
cxm_test_cleanup() {
  local cxm_test_exit=$?
  if (( cxm_test_exit == 0 )); then
    # Immutable tool caches may contain owner-owned read-only directories.
    # Only prepare this private invocation for unlinking; never follow links.
    find -P "$cxm_test_invocation" -type d -uid "$(id -u)" -exec chmod u+w -- {} +
    rm -rf -- "$cxm_test_invocation"
  else
    printf 'Retained failed test scratch: %s\n' "$cxm_test_invocation" >&2
  fi
}
trap cxm_test_cleanup EXIT

# Match dirs::home_dir: a nonempty HOME wins over the Unix account home.
# Hide only ambient user skill roots, preserving Cargo/toolchain/runtime data.
cxm_test_host_home=$(python3 -c 'import os, pwd; print(os.environ.get("HOME") or pwd.getpwuid(os.getuid()).pw_dir)')
case "$cxm_test_host_home" in
  /*) ;;
  *) echo 'Test user home must be absolute' >&2; exit 2 ;;
esac
cxm_test_agents_root="${cxm_test_host_home%/}/.agents"

# Preserve the governed PATH, toolchain, library inputs, cache paths, V8 pair,
# network access and product sandbox flags. Tests can still set their own env.
/usr/bin/bwrap \
  --unshare-user \
  --bind / / \
  --bind "$cxm_test_invocation" /tmp \
  --setenv TMPDIR /tmp \
  --tmpfs /etc/codex \
  --remount-ro /etc/codex \
  --tmpfs "$cxm_test_agents_root" \
  --remount-ro "$cxm_test_agents_root" \
  --dev-bind /dev /dev \
  --proc /proc \
  --unsetenv OPENAI_API_KEY \
  --unsetenv CODEX_API_KEY \
  --unsetenv CODEX_ACCESS_TOKEN \
  --unsetenv CODEX_CA_CERTIFICATE \
  --unsetenv SSL_CERT_FILE \
  --unsetenv NO_COLOR \
  --unsetenv INSTA_FORCE_PASS \
  -- "$@"
