#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

image="${IMAGE:-omnifs:native-runtime}"
platform="${PLATFORM:-}"
push="${PUSH:-false}"
metadata_file="${METADATA_FILE:-}"

if [[ -z "$platform" ]]; then
  arch="$(docker version --format '{{.Server.Arch}}')"
  case "$arch" in
    amd64 | x86_64) platform="linux/amd64" ;;
    arm64 | aarch64) platform="linux/arm64" ;;
    *) echo "unsupported docker server arch: $arch" >&2; exit 1 ;;
  esac
fi

case "$platform" in
  linux/amd64) target="${OMNIFS_LINUX_TARGET:-x86_64-unknown-linux-gnu}" ;;
  linux/arm64) target="${OMNIFS_LINUX_TARGET:-aarch64-unknown-linux-gnu}" ;;
  *) echo "unsupported runtime platform: $platform" >&2; exit 1 ;;
esac

binary="${OMNIFS_BINARY:-$root/target/$target/release/omnifs}"
daemon_binary="${OMNIFSD_BINARY:-$root/target/$target/release/omnifsd}"

if [[ ! -x "$binary" ]]; then
  echo "missing native Linux binary: $binary" >&2
  exit 1
fi

if [[ ! -x "$daemon_binary" ]]; then
  echo "missing native Linux daemon binary: $daemon_binary" >&2
  exit 1
fi

context="$(mktemp -d)"
cleanup() {
  rm -rf "$context"
}
trap cleanup EXIT

mkdir -p "$context/scripts"
cp "$binary" "$context/omnifs"
cp "$daemon_binary" "$context/omnifsd"
cp "$root/scripts/demo.sh" "$context/scripts/demo.sh"
cp "$root/scripts/container-entrypoint.sh" "$context/scripts/container-entrypoint.sh"
cp "$root/scripts/container-zshrc.zsh" "$context/scripts/container-zshrc.zsh"

if [[ -z "$metadata_file" ]]; then
  metadata_file="$context/build-metadata.json"
fi

output_arg=(--load)
if [[ "$push" == "true" ]]; then
  output_arg=(--push)
fi

# Bake the launcher's crate version into the image so the launcher's
# pre-`docker create` handshake (see `crates/omnifs-cli/src/runtime.rs`) can
# refuse mismatched pairings. Read the workspace version directly so
# this works whether or not the build binary is native-runnable.
if [[ -z "${OMNIFS_MIN_LAUNCHER_VERSION:-}" ]]; then
  OMNIFS_MIN_LAUNCHER_VERSION="$(awk -F'"' '/^version = / {print $2; exit}' "$root/Cargo.toml")"
fi

docker buildx build "${output_arg[@]}" \
  --metadata-file "$metadata_file" \
  --platform "$platform" \
  --build-arg "OMNIFS_MIN_LAUNCHER_VERSION=${OMNIFS_MIN_LAUNCHER_VERSION}" \
  -t "$image" \
  -f "$(dirname "${BASH_SOURCE[0]}")/Dockerfile.runtime" \
  "$context"

digest=""
if [[ -s "$metadata_file" ]]; then
  digest="$(jq -r '."containerimage.digest" // empty' "$metadata_file")"
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "image=$image"
    echo "platform=$platform"
    echo "digest=$digest"
  } >>"$GITHUB_OUTPUT"
fi

echo "image=$image"
echo "platform=$platform"
echo "digest=$digest"
echo "binary=$binary"
echo "daemon_binary=$daemon_binary"
