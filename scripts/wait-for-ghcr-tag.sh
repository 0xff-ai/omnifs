#!/usr/bin/env bash
# Poll until a GHCR/OCI tag exists and is readable via buildx imagetools.
set -euo pipefail

ref="${1:?image ref required (e.g. ghcr.io/org/repo:sha-abc123)}"
max_attempts="${2:-120}"
wait_secs="${3:-30}"

for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if docker buildx imagetools inspect "$ref" >/dev/null 2>&1; then
    printf 'found %s\n' "$ref" >&2
    exit 0
  fi
  if ((attempt == max_attempts)); then
    printf 'timed out after %s attempts waiting for %s\n' "$max_attempts" "$ref" >&2
    printf 'CI may still be running, or main CI has not published this commit yet.\n' >&2
    exit 1
  fi
  printf 'waiting for %s (%s/%s)...\n' "$ref" "$attempt" "$max_attempts" >&2
  sleep "$wait_secs"
done
