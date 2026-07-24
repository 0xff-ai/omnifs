#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: scripts/ci/sign-darwin-arm64-payload.sh PAYLOAD_DIR IDENTITY TEAM_ID" >&2
  exit 2
fi

payload="$1"
identity="$2"
team_id="$3"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
dylib="$payload/libexec/omnifs/libkrun.1.dylib"
helper="$payload/omnifs-libkrun"

# Sign nested code first. The helper is a separate executable, but this order
# also prevents a later dylib mutation from escaping the final payload audit.
codesign --force --timestamp --options runtime --sign "$identity" "$dylib"
codesign --force --timestamp --options runtime --sign "$identity" \
  --entitlements "$root/scripts/ci/omnifs-libkrun.entitlements.plist" \
  "$helper"
codesign --force --timestamp --options runtime --sign "$identity" "$payload/omnifs"
codesign --force --timestamp --options runtime --sign "$identity" "$payload/omnifs-thin"

"$root/scripts/ci/check-darwin-arm64-payload.sh" "$payload"

for signed in "$dylib" "$helper" "$payload/omnifs" "$payload/omnifs-thin"; do
  signature="$(codesign -d --verbose=4 "$signed" 2>&1)"
  actual_team="$(sed -n 's/^TeamIdentifier=//p' <<<"$signature")"
  if [[ "$actual_team" != "$team_id" ]]; then
    echo "unexpected signing team for $signed: expected $team_id, got ${actual_team:-<none>}" >&2
    exit 1
  fi
  if ! grep -Eq '^flags=.*\(.*runtime.*\)' <<<"$signature"; then
    echo "$signed lacks the hardened runtime signature flag" >&2
    exit 1
  fi
done

echo "Darwin arm64 payload signed by team $team_id"
