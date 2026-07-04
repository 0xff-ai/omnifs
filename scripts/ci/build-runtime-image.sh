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

if [[ ! -x "$binary" ]]; then
  echo "missing native Linux binary: $binary" >&2
  exit 1
fi

# The release image is the top-level Dockerfile's `runtime-release` target: it
# shares the `runtime-base` stage with the contributor image, and the prebuilt
# binary is injected as the `omnifs-bin` named build context rather than copied
# from a compile stage. Targeting `runtime-release` builds only
# `ubuntu -> runtime-base -> runtime-release`, so the toolchain never runs.
bindir="$(mktemp -d)"
cleanup() {
  rm -rf "$bindir"
}
trap cleanup EXIT

cp "$binary" "$bindir/omnifs"

if [[ -z "$metadata_file" ]]; then
  metadata_file="$bindir/build-metadata.json"
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
  --target runtime-release \
  --build-context "omnifs-bin=$bindir" \
  --build-arg "OMNIFS_MIN_LAUNCHER_VERSION=${OMNIFS_MIN_LAUNCHER_VERSION}" \
  -t "$image" \
  -f "$root/Dockerfile" \
  "$root"

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
