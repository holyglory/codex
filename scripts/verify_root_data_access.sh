#!/usr/bin/env bash
set -euo pipefail

codex_binary=${1:?usage: verify_root_data_access.sh CODEX_BINARY}
if [[ ! -x "$codex_binary" ]]; then
  echo "candidate binary is not executable" >&2
  exit 2
fi

inherited_home=${HOME:?HOME must be set for the inherited-HOME check}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/codex-root-access.XXXXXX")
cleanup() {
  sudo -n rm -rf "$scratch"
}
trap cleanup EXIT

# Run the installed candidate as UID 0 with a non-root HOME while binding a
# disposable directory over /root. This exercises the default passwd-home
# resolution without touching the operator's real root data.
sudo -n mkdir -p "$scratch/root"
sudo -n unshare --mount --propagation private /bin/bash -s -- \
  "$scratch/root" "$inherited_home" "$codex_binary" <<'EOF'
set -euo pipefail
mount --bind "$1" /root
chmod 700 /root
export HOME="$2"
unset CODEX_HOME
"$3" usage --json doctor >/dev/null
"$3" usage --json summary >/dev/null

[[ "$(stat -c '%u' /root/.codex)" == "0" ]]
[[ "$(stat -c '%u %a' /root/.codex/usage)" == "0 700" ]]
[[ "$(stat -c '%u %a' /root/.codex/usage/usage.sqlite3)" == "0 600" ]]
for sidecar in /root/.codex/usage/usage.sqlite3-wal /root/.codex/usage/usage.sqlite3-shm; do
  if [[ -e "$sidecar" ]]; then
    [[ "$(stat -c '%u %a' "$sidecar")" == "0 600" ]]
  fi
done
EOF

echo "root-owned Codex home and usage database passed"
