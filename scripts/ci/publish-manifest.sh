#!/usr/bin/env bash
# Merge per-arch image digests into one multi-platform manifest tag and
# verify both platforms are present. Shared by the runtime-image and
# frontend-image publish steps in ci.yml: same imagetools mechanics, a
# different image repo and digest set per caller.
#
# Usage: publish-manifest.sh <image-repo> <tag> <digests-dir> <out-prefix>
#
# <image-repo> is the registry-qualified repo with no tag (e.g.
# ghcr.io/0xff-ai/omnifs or ghcr.io/0xff-ai/omnifs-frontend). <digests-dir>
# holds one file per platform, named by its sha256 digest (hex, no prefix),
# as `actions/download-artifact` lays out `docker-digest-*`/`frontend-digest-*`
# artifacts with merge-multiple. Writes <out-prefix>-manifest.txt and
# <out-prefix>-manifest-digest.txt in the current directory.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: scripts/ci/publish-manifest.sh IMAGE_REPO TAG DIGESTS_DIR OUT_PREFIX" >&2
  exit 2
fi

image_repo="$1"
tag="$2"
digests_dir="$3"
out_prefix="$4"

refs=()
for digest_file in "$digests_dir"/*; do
  digest="${digest_file##*/}"
  refs+=("${image_repo}@sha256:${digest}")
done
test "${#refs[@]}" -ge 2

full_tag="${image_repo}:${tag}"
manifest_out="${out_prefix}-manifest.txt"
digest_out="${out_prefix}-manifest-digest.txt"

docker buildx imagetools create -t "$full_tag" "${refs[@]}"
docker buildx imagetools inspect "$full_tag" >"$manifest_out"
grep -q 'Platform:.*linux/amd64' "$manifest_out"
grep -q 'Platform:.*linux/arm64' "$manifest_out"
awk '/^Digest:/ { print $2; exit }' "$manifest_out" >"$digest_out"
