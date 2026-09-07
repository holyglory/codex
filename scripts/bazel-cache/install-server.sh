#!/usr/bin/env bash
# Install only this dedicated cache service; never touch installed Codex releases.
set -euo pipefail
[[ $EUID -eq 0 && $# -eq 2 ]] || { echo 'Run as root: install-server.sh VERIFIED_BINARY PUBLIC_KEY' >&2; exit 1; }
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binary="$(realpath -- "$1")"
public_key="$(realpath -- "$2")"
expected=62e236bf8396e69396928e0d0c32062fbd5575f20fe55dc10a82eb791297e1a0
[[ "$(sha256sum -- "$binary" | cut -d' ' -f1)" == "$expected" ]] || { echo 'Cache binary checksum mismatch' >&2; exit 1; }
ssh-keygen -lf "$public_key" >/dev/null
read -r key_type key_value key_comment < "$public_key"
[[ "$key_type" == ssh-ed25519 ]] || { echo 'Expected dedicated Ed25519 key' >&2; exit 1; }
/usr/sbin/sshd -t
/usr/sbin/sshd -t -f "$script_dir/sshd.conf"
mountpoint -q /mnt/build-storage

# Refuse replacing unrelated or manually edited configuration.
for pair in 'server.yml:/etc/codex-bazel-cache/server.yml' 'codex-bazel-cache.service:/etc/systemd/system/codex-bazel-cache.service' 'sshd.conf:/etc/ssh/sshd_config.d/60-codex-bazel-cache.conf'; do
  source_file="${pair%%:*}"
  destination="${pair#*:}"
  [[ ! -L "$destination" ]]
  if [[ -e "$destination" ]]; then cmp -- "$script_dir/$source_file" "$destination"; fi
done
if getent passwd codex-bazel-cache >/dev/null; then
  [[ "$(getent passwd codex-bazel-cache | cut -d: -f6-7)" == '/nonexistent:/usr/sbin/nologin' ]]
else
  useradd --system --user-group --home-dir /nonexistent --shell /usr/sbin/nologin codex-bazel-cache
fi
install -d -m 755 /opt/codex-bazel-cache/2.6.2 /etc/codex-bazel-cache
install -d -m 700 -o codex-bazel-cache -g codex-bazel-cache /mnt/build-storage/codex/state/persistent-bazel-cache/data
install -m 755 "$binary" /opt/codex-bazel-cache/2.6.2/bazel-remote
install -m 644 "$script_dir/server.yml" /etc/codex-bazel-cache/server.yml
install -m 644 "$script_dir/codex-bazel-cache.service" /etc/systemd/system/codex-bazel-cache.service
authorized_key="restrict,port-forwarding,permitopen=\"127.0.0.1:9095\" $key_type $key_value"
[[ ! -L /etc/codex-bazel-cache/authorized_keys ]]
if [[ -e /etc/codex-bazel-cache/authorized_keys ]]; then
  read -r existing_key < /etc/codex-bazel-cache/authorized_keys
  [[ "$existing_key" == "$authorized_key" ]] || { echo 'Existing cache key differs; rotation requires review' >&2; exit 1; }
else
  printf '%s\n' "$authorized_key" > /etc/codex-bazel-cache/authorized_keys
  chmod 644 /etc/codex-bazel-cache/authorized_keys
fi
install -m 644 "$script_dir/sshd.conf" /etc/ssh/sshd_config.d/60-codex-bazel-cache.conf
/usr/sbin/sshd -t
policy="$(/usr/sbin/sshd -T -C user=codex-bazel-cache,host=localhost,addr=127.0.0.1)"
for required in 'allowtcpforwarding local' 'allowstreamlocalforwarding no' 'permitopen 127.0.0.1:9095' 'permitlisten none' 'forcecommand /usr/bin/false' 'passwordauthentication no'; do
  grep -Fxq "$required" <<< "$policy"
done
systemd-analyze verify /etc/systemd/system/codex-bazel-cache.service
systemctl daemon-reload
systemctl enable --now codex-bazel-cache.service
systemctl reload ssh.service
for attempt in {1..100}; do
  if curl --fail --silent --max-time 1 http://127.0.0.1:9095/status >/dev/null; then break; fi
  sleep 0.1
done
curl --fail --silent --max-time 1 http://127.0.0.1:9095/status >/dev/null
systemctl is-active --quiet codex-bazel-cache.service
echo 'Dedicated cache installed; SSH destination and command restrictions verified.'
