#!/usr/bin/env bash
# Promote a CI-built image (sha-$commit) to release tags.
# Prints only the promoted manifest digest on stdout; logs to stderr.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

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

wait_for_registry_artifact "$source" docker buildx imagetools inspect "$source"

docker buildx imagetools inspect "$source" >&2
docker buildx imagetools create "${tag_args[@]}" "$source" >&2

primary="${registry}/${image_name}:$1"
manifest="$(mktemp)"
trap 'rm -f "$manifest"' EXIT
docker buildx imagetools inspect "$primary" >"$manifest"
grep -q 'Platform:.*linux/amd64' "$manifest"
grep -q 'Platform:.*linux/arm64' "$manifest"
awk '/^Digest:/ { print $2; exit }' "$manifest"
