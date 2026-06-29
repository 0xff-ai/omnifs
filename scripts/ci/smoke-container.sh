#!/usr/bin/env bash
# Smoke the runtime image through the contributor dev launch path: provision
# the github credential from $GITHUB_TOKEN into the dev-home store, bring up the
# pre-built image as-is, render the built-in dev mounts, then run the baked demo
# against the live mount.
#
# Requires IMAGE (container image ref), GITHUB_TOKEN, Bun on PATH, and
# target/omnifs-provider-store from the components job. Optional: CONTAINER
# (default omnifs), RUNNER_TEMP. CI passes --image and --skip-cli-build so the
# script consumes the prebuilt provider-store bundle and does not rebuild Rust.
set -euo pipefail

: "${IMAGE:?IMAGE must be set to the container image ref}"
CONTAINER="${CONTAINER:-omnifs}"
RUNNER_TEMP="${RUNNER_TEMP:-/tmp}"

if [[ -z "${GITHUB_TOKEN:-}" ]]; then
  echo "GITHUB_TOKEN must be set: scripts/dev.ts provisions the github dev mount from it" >&2
  exit 1
fi

export OMNIFS_CONTAINER_NAME="$CONTAINER"

cleanup() {
  if [[ "${SMOKE_FAILED:-0}" == "1" ]]; then
    docker logs "$CONTAINER" >&2 || true
    docker exec "$CONTAINER" sh -lc 'cat /tmp/omnifs.log' >&2 || true
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [[ -z "${SSH_AUTH_SOCK:-}" ]]; then
  eval "$(ssh-agent -a "$RUNNER_TEMP/ssh-agent.sock")"
  export SSH_AUTH_SOCK="$RUNNER_TEMP/ssh-agent.sock"
fi

bun scripts/dev.ts \
  --yes \
  --no-shell \
  --profile smoke \
  --image "$IMAGE" \
  --provider-store target/omnifs-provider-store \
  --skip-cli-build || { SMOKE_FAILED=1; exit 1; }

docker exec "$CONTAINER" env \
  OMNIFS_DEMO_MODE=smoke \
  OMNIFS_DEMO_OWNER=0xff-ai \
  OMNIFS_DEMO_REPO=omnifs \
  /tmp/demo.sh || { SMOKE_FAILED=1; exit 1; }
