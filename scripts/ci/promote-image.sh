#!/usr/bin/env bash
# Promote a CI-built image (sha-$commit) to release tags.
# Prints only the promoted manifest digest on stdout; logs to stderr.
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: scripts/ci/promote-image.sh REGISTRY IMAGE_NAME COMMIT_SHA TAG [TAG...]" >&2
  exit 2
fi

registry="$1"
image_name="$2"
commit_sha="$3"
shift 3

source="${registry}/${image_name}:sha-${commit_sha}"
tag_args=()
for tag in "$@"; do
  tag_args+=("-t" "${registry}/${image_name}:${tag}")
done

# Release is triggered by workflow_run after green CI; allow brief registry lag.
max_attempts=12
wait_secs=10
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if docker buildx imagetools inspect "$source" >/dev/null 2>&1; then
    break
  fi
  if ((attempt == max_attempts)); then
    echo "timed out waiting for CI image $source" >&2
    exit 1
  fi
  printf 'waiting for %s (%s/%s)...\n' "$source" "$attempt" "$max_attempts" >&2
  sleep "$wait_secs"
done

docker buildx imagetools inspect "$source" >&2
docker buildx imagetools create "${tag_args[@]}" "$source" >&2

primary="${registry}/${image_name}:$1"
manifest="$(mktemp)"
trap 'rm -f "$manifest"' EXIT
docker buildx imagetools inspect "$primary" >"$manifest"
grep -q 'Platform:.*linux/amd64' "$manifest"
grep -q 'Platform:.*linux/arm64' "$manifest"
awk '/^Digest:/ { print $2; exit }' "$manifest"
