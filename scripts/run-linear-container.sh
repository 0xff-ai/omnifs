#!/usr/bin/env bash
# Standalone runner for the Linear provider, used during development to
# avoid colliding with the default `omnifs` container managed by
# `just dev` / `just start`.
#
# Usage:
#   LINEAR_TOKEN=lin_api_... ./scripts/run-linear-container.sh \
#       [<image> [<container>]]
#
# Defaults to image `omnifs-linear:dev` and container `omnifs-linear`.
set -euo pipefail

image="${1:-omnifs-linear:dev}"
container="${2:-omnifs-linear}"

if [[ -z "${LINEAR_TOKEN:-}" ]]; then
  echo "LINEAR_TOKEN env var is required" >&2
  exit 1
fi

docker rm -f "$container" >/dev/null 2>&1 || true
docker run -d \
  --name "$container" \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor:unconfined \
  -e LINEAR_TOKEN="$LINEAR_TOKEN" \
  "$image"

for _ in $(seq 1 60); do
  if docker exec "$container" sh -lc "grep -qs ' /omnifs ' /proc/mounts"; then
    exit 0
  fi
  if ! docker ps --format '{{.Names}}' | grep -qx "$container"; then
    docker logs "$container" >&2 || true
    docker exec "$container" sh -lc 'cat /tmp/omnifs.log' >&2 || true
    exit 1
  fi
  sleep 1
done
docker exec "$container" sh -lc 'cat /tmp/omnifs.log' >&2 || true
exit 1
