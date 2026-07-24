#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

if [[ $# -lt 2 ]]; then
  echo "usage: scripts/ci/package-cli.sh PLATFORM_ID BINARY... | PLATFORM_ID --payload-dir DIR" >&2
  exit 2
fi

platform_id="$1"
shift
inputs=("$@")

dist_dir="$root/dist/cli/$platform_id"
payload_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$payload_dir"
}
trap cleanup EXIT

mkdir -p "$dist_dir"
payload=()
if [[ "${inputs[0]}" == "--payload-dir" ]]; then
  if [[ ${#inputs[@]} -ne 2 || ! -d "${inputs[1]}" ]]; then
    echo "--payload-dir requires one existing directory" >&2
    exit 2
  fi
  cp -R "${inputs[1]}/." "$payload_dir/"
  while IFS= read -r entry; do
    payload+=("${entry#"$payload_dir/"}")
  done < <(find "$payload_dir" -mindepth 1 -maxdepth 1 -print | sort)
else
  for binary in "${inputs[@]}"; do
    if [[ ! -x "$binary" ]]; then
      echo "missing executable binary: $binary" >&2
      exit 1
    fi
    name="$(basename "$binary")"
    cp "$binary" "$payload_dir/$name"
    chmod 0755 "$payload_dir/$name"
    payload+=("$name")
  done
fi

archive="$dist_dir/omnifs-cli-$platform_id.tar.xz"
tar -C "$payload_dir" -cJf "$archive" "${payload[@]}"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$archive" >"$archive.sha256"
else
  shasum -a 256 "$archive" >"$archive.sha256"
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "archive=$archive"
    echo "sha256=$(cut -d' ' -f1 "$archive.sha256")"
  } >>"$GITHUB_OUTPUT"
fi

echo "archive=$archive"
cat "$archive.sha256"
