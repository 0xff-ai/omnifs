#!/usr/bin/env bash
set -euo pipefail

provider_root=${1:-${OMNIFS_FRONTEND_TEST_ROOT:-}}
if [[ -z "$provider_root" ]]; then
  echo "usage: $0 /path/to/mounted/test-provider-root" >&2
  exit 2
fi

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

wait_for_readable() {
  local path=$1
  for _ in $(seq 1 60); do
    if [[ -r "$path" ]]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

file_size() {
  local path=$1
  if [[ "$(uname -s)" == "Darwin" ]]; then
    stat -f '%z' "$path"
  else
    stat -c '%s' "$path"
  fi
}

sha256_files() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

message="${provider_root}/hello/message"
ranged="${provider_root}/hello/ranged"
unknown_ranged="${provider_root}/hello/unknown-ranged"
large_ranged="${provider_root}/hello/large-ranged"
dynamic_value="${provider_root}/dynamic/alpha/value"
bundle_dir="${provider_root}/hello/bundle"

wait_for_readable "$message"

[[ -d "$provider_root" ]]
[[ -d "${provider_root}/hello" ]]
[[ -d "$bundle_dir" ]]

ls -la "$provider_root" >"$workdir/root.ls"
ls -la "${provider_root}/hello" >"$workdir/hello.ls"
find "$provider_root" -maxdepth 3 \
  \( -path "${provider_root}/checkout" -o -path "${provider_root}/hello/throttled" \) -prune -o \
  -type f -print | sort >"$workdir/find.files"

grep -q '^Hello, world!$' "$message"
[[ "$(cat "$message")" == "Hello, world!" ]]
[[ "$(cat "$dynamic_value")" == "alpha" ]]
[[ "$(cat "${bundle_dir}/title")" == "title" ]]
[[ "$(cat "${bundle_dir}/body")" == "body" ]]

[[ "$(file_size "$message")" == "13" ]]
[[ "$(file_size "$ranged")" == "26" ]]
[[ "$(dd if="$ranged" bs=1 skip=2 count=4 2>/dev/null)" == "cdef" ]]
[[ "$(cat "$unknown_ranged")" == "unknown-size" ]]
[[ "$(dd if="$large_ranged" bs=1048576 skip=64 count=1 2>/dev/null)" == "L" ]]

head -c 5 "$ranged" >"$workdir/ranged.head"
tail -c 5 "$ranged" >"$workdir/ranged.tail"
[[ "$(cat "$workdir/ranged.head")" == "abcde" ]]
[[ "$(cat "$workdir/ranged.tail")" == "vwxyz" ]]

wc -c "$message" "$ranged" >"$workdir/wc.txt"
cp "$message" "$workdir/message.copy"
cmp "$message" "$workdir/message.copy"
sha256_files "$message" "$workdir/message.copy" >"$workdir/sha256.txt"

tar -cf "$workdir/bundle.tar" -C "$bundle_dir" title body
tar -tf "$workdir/bundle.tar" | sort >"$workdir/bundle.tar.list"
grep -q '^title$' "$workdir/bundle.tar.list"
grep -q '^body$' "$workdir/bundle.tar.list"

if command -v rg >/dev/null 2>&1; then
  rg -n 'Hello|world' "$message" >"$workdir/search.txt"
else
  grep -nE 'Hello|world' "$message" >"$workdir/search.txt"
fi

echo "frontend test-provider smoke passed for $provider_root"
