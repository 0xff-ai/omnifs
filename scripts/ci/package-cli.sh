#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/ci/package-cli.sh PLATFORM_ID BINARY" >&2
  exit 2
fi

platform_id="$1"
binary="$2"

if [[ ! -x "$binary" ]]; then
  echo "missing executable CLI binary: $binary" >&2
  exit 1
fi

dist_dir="$root/dist/cli/$platform_id"
payload_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$payload_dir"
}
trap cleanup EXIT

mkdir -p "$dist_dir"
cp "$binary" "$payload_dir/omnifs"
chmod 0755 "$payload_dir/omnifs"

archive="$dist_dir/omnifs-cli-$platform_id.tar.xz"
tar -C "$payload_dir" -cJf "$archive" omnifs
sha256sum "$archive" >"$archive.sha256"

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "archive=$archive"
    echo "sha256=$(cut -d' ' -f1 "$archive.sha256")"
  } >>"$GITHUB_OUTPUT"
fi

echo "archive=$archive"
cat "$archive.sha256"
