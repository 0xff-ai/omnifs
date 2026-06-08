#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS probe smoke must run on Darwin" >&2
  exit 77
fi

repo_root=${1:-${OMNIFS_NFS_SMOKE_REPO_ROOT:-/github/0xff-ai/omnifs}}
sample_file=${OMNIFS_NFS_SMOKE_FILE:-}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

discover_sample_file() {
  local candidate

  if [[ -n "$sample_file" ]]; then
    printf '%s\n' "$sample_file"
    return 0
  fi

  for candidate in \
    "${repo_root}/repo/README.md" \
    "${repo_root}/repo/README" \
    "${repo_root}/repo/AGENTS.md"
  do
    if [[ -r "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  find "${repo_root}/repo" \
    -path '*/.git/*' -prune -o \
    -type f -readable -print -quit 2>/dev/null || true
}

for _ in $(seq 1 60); do
  sample_file=$(discover_sample_file)
  if [[ -r "$sample_file" ]]; then
    break
  fi
  sleep 1
done

[[ -d "$repo_root" ]]
[[ -r "$sample_file" ]]

sample_dir=$(dirname "$sample_file")
sample_base=$(basename "$sample_file")

if [[ -e "${sample_dir}/.DS_Store" ]]; then
  echo "unexpected .DS_Store probe materialized under $sample_dir" >&2
  exit 1
fi

if [[ -e "${sample_dir}/._${sample_base}" ]]; then
  echo "unexpected AppleDouble probe materialized under $sample_dir" >&2
  exit 1
fi

before=$(stat -f '%z:%m:%c' "$sample_file")
for _ in $(seq 1 20); do
  stat "$sample_file" >/dev/null
  ls -la "$sample_dir" >/dev/null
done
after=$(stat -f '%z:%m:%c' "$sample_file")
[[ "$before" == "$after" ]]

cp "$sample_file" "$workdir/sample.copy"
cmp "$sample_file" "$workdir/sample.copy"

echo "nfs macOS probe smoke passed for $sample_file"
