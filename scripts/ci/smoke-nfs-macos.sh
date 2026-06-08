#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS NFS smoke must run on Darwin" >&2
  exit 2
fi

if ! sudo -n true >/dev/null 2>&1; then
  echo "passwordless sudo is required for mount_nfs in CI" >&2
  exit 1
fi

root=${GITHUB_WORKSPACE:-$(git rev-parse --show-toplevel)}
workdir=$(mktemp -d)
mount_point="$workdir/mnt"
config_dir="$workdir/config"
cache_dir="$workdir/cache"
providers_dir="$workdir/providers"
state_dir="$workdir/state"
log="$workdir/nfs.log"
daemon_pid=

cleanup() {
  set +e
  "$root/target/debug/omnifs" daemon nfs-unmount --mount-point "$mount_point" >/dev/null 2>&1
  if [[ -n "${daemon_pid:-}" ]]; then
    kill "$daemon_pid" >/dev/null 2>&1
    wait "$daemon_pid" >/dev/null 2>&1
  fi
  rm -rf "$workdir"
}
trap cleanup EXIT

mkdir -p "$mount_point" "$config_dir/mounts" "$cache_dir" "$providers_dir" "$state_dir"
cp "$root/target/wasm32-wasip2/release/test_provider.wasm" "$providers_dir/test_provider.wasm"
cp "$root/target/wasm32-wasip2/release/omnifs_tool_archive.wasm" \
  "$providers_dir/omnifs_tool_archive.wasm"
cat >"$config_dir/mounts/test.json" <<'JSON'
{
  "provider": "test_provider.wasm",
  "mount": "test"
}
JSON

"$root/target/debug/omnifs" daemon nfs-mount \
  --mount-point "$mount_point" \
  --config-dir "$config_dir" \
  --providers-dir "$providers_dir" \
  --cache-dir "$cache_dir" \
  --state-dir "$state_dir" \
  >"$log" 2>&1 &
daemon_pid=$!

for _ in $(seq 1 80); do
  if [[ -r "$mount_point/test/hello/message" ]]; then
    break
  fi
  if ! kill -0 "$daemon_pid" >/dev/null 2>&1; then
    echo "NFS daemon exited before smoke became ready" >&2
    cat "$log" >&2 || true
    exit 1
  fi
  sleep 1
done

if [[ ! -r "$mount_point/test/hello/message" ]]; then
  echo "NFS mount did not become readable" >&2
  cat "$log" >&2 || true
  exit 1
fi

"$root/tests/smoke/frontend-test-provider.sh" "$mount_point/test"
