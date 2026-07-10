#!/usr/bin/env bash
# Promote a CI-built guest image (sha-$commit) to release tags.
# Prints only the promoted manifest digest on stdout; logs to stderr.
#
# Mirrors promote-image.sh's wait-for-artifact retry loop and digest output,
# but the guest image is a single-arch, non-container OCI artifact (pushed
# by `oras push` in ci.yml's guest-image-arm64 job, not `docker buildx
# build`), so promotion uses `oras tag` for a registry-side retag instead of
# `docker buildx imagetools create`, which assumes an image manifest.
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: scripts/ci/promote-guest-image.sh REGISTRY IMAGE_NAME COMMIT_SHA TAG [TAG...]" >&2
  exit 2
fi

registry="$1"
image_name="$2"
commit_sha="$3"
shift 3

source="${registry}/${image_name}:sha-${commit_sha}"

# Release is triggered by workflow_run after green CI; allow brief registry lag.
max_attempts=12
wait_secs=10
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if oras manifest fetch "$source" >/dev/null 2>&1; then
    break
  fi
  if ((attempt == max_attempts)); then
    echo "timed out waiting for CI guest image $source" >&2
    exit 1
  fi
  printf 'waiting for %s (%s/%s)...\n' "$source" "$attempt" "$max_attempts" >&2
  sleep "$wait_secs"
done

oras manifest fetch "$source" >&2
oras tag "$source" "$@" >&2

primary="${registry}/${image_name}:$1"
oras resolve "$primary"
