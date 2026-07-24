#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/ci/wait-for-notarization.sh SUBMISSION_ID" >&2
  exit 2
fi

submission_id="$1"
max_attempts="${OMNIFS_NOTARY_MAX_ATTEMPTS:-180}"
interval="${OMNIFS_NOTARY_POLL_SECONDS:-20}"

for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  error_file="$(mktemp)"
  if ! response="$(
    xcrun notarytool info "$submission_id" \
      --apple-id "${APPLE_NOTARY_APPLE_ID:?APPLE_NOTARY_APPLE_ID is required}" \
      --password "${APPLE_NOTARY_PASSWORD:?APPLE_NOTARY_PASSWORD is required}" \
      --team-id "${APPLE_TEAM_ID:?APPLE_TEAM_ID is required}" \
      --output-format json 2>"$error_file"
  )"; then
    printf 'notary lookup failed for %s (%d/%d): ' \
      "$submission_id" "$attempt" "$max_attempts" >&2
    cat "$error_file" >&2
    rm -f "$error_file"
    if ((attempt == max_attempts)); then
      exit 1
    fi
    sleep "$interval"
    continue
  fi
  rm -f "$error_file"
  status="$(jq -er '.status' <<<"$response")"
  case "$status" in
    Accepted)
      printf '%s\n' "$response"
      exit 0
      ;;
    "In Progress")
      printf 'notarization %s is still in progress (%d/%d)\n' \
        "$submission_id" "$attempt" "$max_attempts" >&2
      ;;
    Invalid | Rejected)
      printf '%s\n' "$response" >&2
      xcrun notarytool log "$submission_id" \
        --apple-id "$APPLE_NOTARY_APPLE_ID" \
        --password "$APPLE_NOTARY_PASSWORD" \
        --team-id "$APPLE_TEAM_ID" >&2 || true
      exit 1
      ;;
    *)
      echo "unexpected notarization status for $submission_id: $status" >&2
      printf '%s\n' "$response" >&2
      exit 1
      ;;
  esac
  sleep "$interval"
done

echo "notarization $submission_id did not finish after $max_attempts polls" >&2
exit 1
