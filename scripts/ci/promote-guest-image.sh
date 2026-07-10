#!/usr/bin/env bash
# Promote a CI-built guest image (sha-$commit) to release tags.
# Prints only the promoted manifest digest on stdout; logs to stderr.
#
# The guest image is a non-container OCI artifact, so promotion uses `oras tag`
# rather than Docker image-manifest tooling.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

if [[ $# -lt 3 ]]; then
  echo "usage: scripts/ci/promote-guest-image.sh REGISTRY IMAGE_NAME COMMIT_SHA TAG [TAG...]" >&2
  exit 2
fi

registry="$1"
image_name="$2"
commit_sha="$3"
shift 3

source="${registry}/${image_name}:sha-${commit_sha}"

wait_for_registry_artifact "$source" oras manifest fetch "$source"

oras manifest fetch "$source" >&2
oras tag "$source" "$@" >&2

primary="${registry}/${image_name}:$1"
oras resolve "$primary"
