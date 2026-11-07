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

# Preserve the governed PATH, toolchain, library inputs, cache paths, V8 pair,
# network access and product sandbox flags. Tests can still set their own env.
exec /usr/bin/bwrap \
  --unshare-user \
  --bind / / \
  --bind "$cxm_test_scratch" /tmp \
  --setenv TMPDIR /tmp \
  --tmpfs /etc/codex \
  --remount-ro /etc/codex \
  --dev-bind /dev /dev \
  --proc /proc \
  --unsetenv OPENAI_API_KEY \
  --unsetenv CODEX_API_KEY \
  --unsetenv CODEX_ACCESS_TOKEN \
  --unsetenv NO_COLOR \
  -- "$@"
