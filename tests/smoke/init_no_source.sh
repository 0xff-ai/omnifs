#!/usr/bin/env bash
set -euo pipefail

# Locate worktree root from BASH_SOURCE, regardless of cwd.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$script_dir/../.." && pwd)"

# Locate omnifs binary; build if missing.
omnifs_bin="$root/target/release/omnifs"
if [[ ! -x "$omnifs_bin" ]]; then
  echo "Building omnifs (release)..."
  cargo build --release -p omnifs-cli --manifest-path "$root/Cargo.toml"
fi

# Scrub env vars that would taint the test, and point the daemon address at
# a dead port so init never live-pushes the mount into a running dev daemon.
unset OMNIFS_MOUNTS_DIR OMNIFS_CONFIG_DIR OMNIFS_CACHE_DIR
export OMNIFS_DAEMON_ADDR="127.0.0.1:1"

# Isolated tmp dir; cleaned on exit.
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

export OMNIFS_HOME="$tmpdir"

# Plant a sentinel stale wasm in the cwd-relative target/ tree.
sentinel_dir="$tmpdir/target/wasm32-wasip2/release"
mkdir -p "$sentinel_dir"
sentinel="$sentinel_dir/omnifs_provider_arxiv.wasm"
printf 'STALE-DO-NOT-CONSULT' > "$sentinel"

# Save a reference copy to verify it is untouched after init.
sentinel_ref="$tmpdir/sentinel.ref"
cp "$sentinel" "$sentinel_ref"

# Run init from tmpdir so any cwd-relative target/ probe finds the sentinel.
mounts_out="$tmpdir/mounts"
output="$(cd "$tmpdir" && "$omnifs_bin" init arxiv --no-input --mounts-dir "$mounts_out" 2>&1)" || {
  echo "FAIL: omnifs init exited nonzero"
  echo "$output"
  exit 1
}

# Assert 1: mount file was created.
mount_file="$mounts_out/arxiv.json"
if [[ ! -f "$mount_file" ]]; then
  echo "FAIL: $mount_file does not exist"
  echo "$output"
  exit 1
fi

# Assert 2: provider and mount fields are correct.
if ! jq -e '.provider == "omnifs_provider_arxiv.wasm" and .mount == "arxiv"' "$mount_file" >/dev/null; then
  echo "FAIL: provider or mount field missing or wrong in $mount_file"
  jq . "$mount_file" >&2 || cat "$mount_file" >&2
  exit 1
fi

# Assert 3: sentinel is byte-identical (init never touched target/).
if ! cmp -s "$sentinel" "$sentinel_ref"; then
  echo "FAIL: sentinel wasm was modified — init consulted target/wasm32-wasip2/release/"
  exit 1
fi

echo "OK: omnifs init works source-free for arxiv"
