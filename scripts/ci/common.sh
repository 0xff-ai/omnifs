#!/usr/bin/env bash
# Helpers sourced by scripts/ci/*.sh. Source with:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
#
# Sets $root to the repo root.

# shellcheck disable=SC2034
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Wait for a CI artifact to become visible in the registry. Release workflows
# start immediately after CI, so registry propagation can briefly lag.
wait_for_registry_artifact() {
  local source="$1"
  shift

  local attempt max_attempts=12
  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    if "$@" >/dev/null 2>&1; then
      return
    fi
    if ((attempt == max_attempts)); then
      echo "timed out waiting for CI artifact $source" >&2
      return 1
    fi
    printf 'waiting for %s (%s/%s)...\n' "$source" "$attempt" "$max_attempts" >&2
    sleep 10
  done
}

# Build the top-level Dockerfile's `frontend-release` stage for one platform:
# a minimal image with a prebuilt Linux `omnifs-fuse` binary injected as the
# `omnifs-fuse-bin` build context, never compiling inside Docker.
#
# $1: the Dockerfile stage to build (`frontend-release`).
#
# Reads IMAGE (required), PLATFORM, PUSH, METADATA_FILE, OMNIFS_BINARY,
# OMNIFS_LINUX_TARGET from the environment (all optional beyond IMAGE). Writes
# image/platform/digest to $GITHUB_OUTPUT when set, and to stdout
# unconditionally.
build_release_stage_image() {
  local dockerfile_target="$1"

  local image="${IMAGE:?IMAGE must be set}"
  local platform="${PLATFORM:-}"
  local push="${PUSH:-false}"
  local metadata_file="${METADATA_FILE:-}"

  if [[ -z "$platform" ]]; then
    local arch
    arch="$(docker version --format '{{.Server.Arch}}')"
    case "$arch" in
      amd64 | x86_64) platform="linux/amd64" ;;
      arm64 | aarch64) platform="linux/arm64" ;;
      *) echo "unsupported docker server arch: $arch" >&2; return 1 ;;
    esac
  fi

  local target
  case "$platform" in
    linux/amd64) target="${OMNIFS_LINUX_TARGET:-x86_64-unknown-linux-gnu}" ;;
    linux/arm64) target="${OMNIFS_LINUX_TARGET:-aarch64-unknown-linux-gnu}" ;;
    *) echo "unsupported runtime platform: $platform" >&2; return 1 ;;
  esac

  local binary="${OMNIFS_BINARY:-$root/target/$target/release/omnifs-fuse}"
  if [[ ! -x "$binary" ]]; then
    echo "missing native Linux binary: $binary" >&2
    return 1
  fi

  local bindir
  bindir="$(mktemp -d)"
  trap 'rm -rf "$bindir"' RETURN

  cp "$binary" "$bindir/omnifs-fuse"

  if [[ -z "$metadata_file" ]]; then
    metadata_file="$bindir/build-metadata.json"
  fi

  local output_arg=(--load)
  if [[ "$push" == "true" ]]; then
    output_arg=(--push)
  fi

  docker buildx build "${output_arg[@]}" \
    --metadata-file "$metadata_file" \
    --platform "$platform" \
    --target "$dockerfile_target" \
    --build-context "omnifs-fuse-bin=$bindir" \
    -t "$image" \
    -f "$root/Dockerfile" \
    "$root"

  local digest=""
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
}
