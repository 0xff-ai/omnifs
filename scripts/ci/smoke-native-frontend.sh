#!/usr/bin/env bash
# Smoke the contributor dev flow's CI-relevant halves: a host-native daemon on
# this runner (kernel FUSE on Linux) plus the Docker-hosted FUSE frontend
# container attached to it over TCP. Provisions the github credential from
# $GITHUB_TOKEN, runs the reworked scripts/dev.ts headless, then reads real
# GitHub data through both surfaces it serves: the host mount path the native
# daemon owns directly, and a `docker exec` into the frontend container.
#
# Requires FRONTEND_IMAGE (frontend container image ref), GITHUB_TOKEN, an
# `omnifs` CLI on PATH (the omnifs-install-cli action), bun, jq, and
# target/omnifs-provider-store from the components job.
set -euo pipefail

: "${FRONTEND_IMAGE:?FRONTEND_IMAGE must be set to the frontend image ref}"

if [[ -z "${GITHUB_TOKEN:-}" ]]; then
  echo "GITHUB_TOKEN must be set: scripts/dev.ts provisions the github dev mount from it" >&2
  exit 1
fi

OMNIFS_HOME="$(mktemp -d)"
export OMNIFS_HOME

cleanup() {
  local exit_code=$?
  if [[ "$exit_code" != 0 ]]; then
    echo "== omnifs status ==" >&2
    omnifs status --detail >&2 || true
    echo "== daemon.log (tail) ==" >&2
    tail -n 200 "$OMNIFS_HOME/cache/daemon.log" >&2 || true
  fi
  local frontend
  frontend="$(docker ps --filter "label=ai.0xff.omnifs.home=$OMNIFS_HOME" --format '{{.Names}}' 2>/dev/null || true)"
  [[ -n "$frontend" ]] && docker rm -f "$frontend" >/dev/null 2>&1
  omnifs down --force >/dev/null 2>&1 || true
  rm -rf "$OMNIFS_HOME"
}
trap cleanup EXIT

bun scripts/dev.ts \
  --yes \
  --no-shell \
  --profile smoke \
  --frontend-image "$FRONTEND_IMAGE" \
  --provider-store target/omnifs-provider-store \
  --skip-cli-build

# A live GitHub API list-then-read, not a static synthetic file: proves the
# mount actually talks to GitHub, not just that a frontend booted.
read_first_open_issue_title() {
  local issues_dir="$1/0xff-ai/omnifs/issues/open"
  local issues=("$issues_dir"/*)
  test -f "${issues[0]}/title"
  local title
  title="$(cat "${issues[0]}/title")"
  test -n "$title"
  echo "$title"
}

echo "== host mount read (native daemon) =="
mount_point="$(omnifs status --json | jq -r '.runtime.mount_point')"
test -n "$mount_point" && test "$mount_point" != "null"
read_first_open_issue_title "$mount_point/github"

echo "== frontend container read (docker exec) =="
frontend="$(docker ps --filter "label=ai.0xff.omnifs.home=$OMNIFS_HOME" --format '{{.Names}}')"
test -n "$frontend"
title_in_container="$(docker exec "$frontend" sh -c '
  set -eu
  dir=/omnifs/github/0xff-ai/omnifs/issues/open
  set -- "$dir"/*
  cat "$1/title"
')"
test -n "$title_in_container"

echo "✓ native daemon and frontend container both serve real GitHub data"
