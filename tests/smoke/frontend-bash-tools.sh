#!/usr/bin/env bash
set -euo pipefail

repo_root=${1:-${OMNIFS_FRONTEND_SMOKE_REPO_ROOT:-/github/0xff-ai/omnifs}}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

discover_sample_file() {
  local candidate

  if [[ -n "${OMNIFS_FRONTEND_SMOKE_FILE:-}" ]]; then
    printf '%s\n' "$OMNIFS_FRONTEND_SMOKE_FILE"
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

sample_file=
for _ in $(seq 1 60); do
  sample_file=$(discover_sample_file)
  if [[ -r "$sample_file" ]]; then
    break
  fi
  sleep 1
done

[[ -d "$repo_root" ]]
[[ -r "$sample_file" ]]

ls "$repo_root" >"$workdir/repo.ls"
stat "$sample_file" >"$workdir/sample.stat"
cat "$sample_file" >"$workdir/sample.cat"
wc -c "$sample_file" >"$workdir/sample.wc"
find "$repo_root" -maxdepth 2 -type d | head -20 >"$workdir/repo.find"

if command -v rg >/dev/null 2>&1; then
  rg -n "omnifs|OmnIFS" "$sample_file" >"$workdir/sample.search" || true
else
  grep -nE "omnifs|OmnIFS" "$sample_file" >"$workdir/sample.search" || true
fi

cp "$sample_file" "$workdir/sample.copy"
cmp "$sample_file" "$workdir/sample.copy"
sha256sum "$sample_file" "$workdir/sample.copy" >"$workdir/sample.sha256"
tar -cf "$workdir/sample.tar" -C "$(dirname "$sample_file")" "$(basename "$sample_file")"
tar -tf "$workdir/sample.tar" >"$workdir/sample.tar.list"

echo "frontend bash-tool smoke passed for $sample_file"
