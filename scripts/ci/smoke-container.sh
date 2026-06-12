#!/usr/bin/env bash
# Run the CI smoke container: fixtures, start, demo, cleanup.
# Requires IMAGE (container image ref). Optional: CONTAINER (default omnifs),
# GITHUB_WORKSPACE, GITHUB_TOKEN, RUNNER_TEMP.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

: "${IMAGE:?IMAGE must be set to the container image ref}"
CONTAINER="${CONTAINER:-omnifs}"
WORKSPACE="${GITHUB_WORKSPACE:-$root}"
RUNNER_TEMP="${RUNNER_TEMP:-/tmp}"
WASM_DIR="${OMNIFS_WASM_DIR:-$root/target/wasm32-wasip2/release}"
OMNIFS_HOME_DIR="$RUNNER_TEMP/omnifs-home"
OMNIFS_HOME="/root/.omnifs"
providers_dir="$OMNIFS_HOME_DIR/providers"

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

mkdir -p "$WORKSPACE/.secrets/db"
mkdir -p "$providers_dir"
cp "$WASM_DIR"/omnifs_provider_*.wasm "$providers_dir/"
cp "$WASM_DIR"/omnifs_tool_archive.wasm "$providers_dir/"
if [[ -z "${GITHUB_TOKEN:-}" ]]; then
  echo "GITHUB_TOKEN must be set for the github provider fixture" >&2
  exit 1
fi
printf '%s' "$GITHUB_TOKEN" > "$WORKSPACE/.secrets/github_token"
chmod 600 "$WORKSPACE/.secrets/github_token"
curl -fsSL -o "$WORKSPACE/.secrets/db/test.db" \
  https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite

docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d \
  --name "$CONTAINER" \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor:unconfined \
  -e SSH_AUTH_SOCK=/ssh-agent \
  -e GIT_SSH_COMMAND='ssh -F /dev/null -o StrictHostKeyChecking=accept-new' \
  -e "OMNIFS_HOME=$OMNIFS_HOME" \
  -v "$OMNIFS_HOME_DIR:$OMNIFS_HOME" \
  -v "$WORKSPACE/.secrets/github_token:/run/secrets/github_token:ro" \
  -v "$WORKSPACE/.secrets/db:/data:ro" \
  -v "$WORKSPACE/scripts/demo.sh:/tmp/demo.sh:ro" \
  -v "/var/run/docker.sock:/var/run/docker.sock:ro" \
  -v "$SSH_AUTH_SOCK:/ssh-agent" \
  "$IMAGE"

for _ in $(seq 1 60); do
  if docker exec "$CONTAINER" sh -lc "grep -qs ' /omnifs ' /proc/mounts"; then
    break
  fi
  if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    SMOKE_FAILED=1
    exit 1
  fi
  sleep 1
done

if ! docker exec "$CONTAINER" sh -lc "grep -qs ' /omnifs ' /proc/mounts"; then
  SMOKE_FAILED=1
  exit 1
fi

# The daemon starts with an empty registry; seed the built-in dev mounts
# through the control API (mounts only otherwise arrive from a host-side
# `omnifs up`/`omnifs dev`).
docker exec "$CONTAINER" omnifs debug push-dev-mounts || { SMOKE_FAILED=1; exit 1; }

docker exec "$CONTAINER" env \
  OMNIFS_DEMO_MODE=smoke \
  OMNIFS_DEMO_OWNER=ollama \
  OMNIFS_DEMO_REPO=ollama \
  /tmp/demo.sh || { SMOKE_FAILED=1; exit 1; }
