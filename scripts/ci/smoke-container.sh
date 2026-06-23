#!/usr/bin/env bash
# Smoke the runtime image through the real `omnifs dev` launch path: provision
# the github credential from $GITHUB_TOKEN into the dev-home store, bring up the
# pre-built image as-is, push the built-in dev mounts, then run the baked demo
# against the live mount. This is the same code path contributors use locally,
# so the smoke exercises credential provisioning, mount push, and fixtures end
# to end rather than reimplementing them.
#
# Requires IMAGE (container image ref), GITHUB_TOKEN, and the `omnifs` CLI on
# PATH. Optional: CONTAINER (default omnifs), RUNNER_TEMP. The provider WASM
# bundle is embedded in the CLI and unpacked into the dev home before launch.
set -euo pipefail

: "${IMAGE:?IMAGE must be set to the container image ref}"
CONTAINER="${CONTAINER:-omnifs}"
RUNNER_TEMP="${RUNNER_TEMP:-/tmp}"

if [[ -z "${GITHUB_TOKEN:-}" ]]; then
  echo "GITHUB_TOKEN must be set: omnifs dev provisions the github dev mount from it" >&2
  exit 1
fi

# `omnifs dev` detects the github credential from $GITHUB_TOKEN and names the
# container from $OMNIFS_CONTAINER_NAME so cleanup/exec can find it.
export OMNIFS_CONTAINER_NAME="$CONTAINER"

cleanup() {
  if [[ "${SMOKE_FAILED:-0}" == "1" ]]; then
    docker logs "$CONTAINER" >&2 || true
    docker exec "$CONTAINER" sh -lc 'cat /tmp/omnifs.log' >&2 || true
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# git callouts (clone provider) need an agent; spin up an ephemeral one if the
# runner has none.
if [[ -z "${SSH_AUTH_SOCK:-}" ]]; then
  eval "$(ssh-agent -a "$RUNNER_TEMP/ssh-agent.sock")"
  export SSH_AUTH_SOCK="$RUNNER_TEMP/ssh-agent.sock"
fi

# Bring up the pre-built image through the contributor sandbox path. This
# provisions credentials, unpacks the embedded provider WASM, launches the
# container, and pushes the built-in dev mounts to the daemon.
omnifs dev --yes --image "$IMAGE" || { SMOKE_FAILED=1; exit 1; }

# Exercise the live mount with the demo baked into the image.
docker exec "$CONTAINER" env \
  OMNIFS_DEMO_MODE=smoke \
  OMNIFS_DEMO_OWNER=0xff-ai \
  OMNIFS_DEMO_REPO=omnifs \
  /tmp/demo.sh || { SMOKE_FAILED=1; exit 1; }
