#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/ci/check-darwin-arm64-payload.sh PAYLOAD_DIR" >&2
  exit 2
fi

payload="$1"
for executable in omnifs omnifs-thin omnifs-libkrun; do
  path="$payload/$executable"
  if [[ ! -x "$path" ]]; then
    echo "missing Darwin arm64 payload executable: $path" >&2
    exit 1
  fi
  if [[ "$(lipo -archs "$path")" != "arm64" ]]; then
    echo "$path is not arm64-only: $(lipo -archs "$path")" >&2
    exit 1
  fi
done

"$(dirname "${BASH_SOURCE[0]}")/check-libkrun-runtime.sh" --signed "$payload"

for signed in \
  "$payload/omnifs" \
  "$payload/omnifs-thin" \
  "$payload/omnifs-libkrun" \
  "$payload/libexec/omnifs/libkrun.1.dylib"; do
  codesign --verify --strict --verbose=4 "$signed"
done

entitlements="$(codesign -d --entitlements - "$payload/omnifs-libkrun" 2>/dev/null)"
entitlement_keys="$(
  sed -n \
    -e 's/^[[:space:]]*\[Key\] //p' \
    -e 's#.*<key>\([^<]*\)</key>.*#\1#p' \
    <<<"$entitlements"
)"
if [[ "$entitlement_keys" != "com.apple.security.hypervisor" ]] \
  || ! grep -Eq '(\[Bool\] true|<true/>)' <<<"$entitlements"; then
  echo "omnifs-libkrun must carry only the true Hypervisor entitlement" >&2
  exit 1
fi
for signed in "$payload/omnifs" "$payload/omnifs-thin" "$payload/libexec/omnifs/libkrun.1.dylib"; do
  extra_keys="$(
    codesign -d --entitlements - "$signed" 2>/dev/null \
      | sed -n \
        -e 's/^[[:space:]]*\[Key\] //p' \
        -e 's#.*<key>\([^<]*\)</key>.*#\1#p'
  )"
  if [[ -n "$extra_keys" ]]; then
    echo "$signed must carry no entitlements, found: $extra_keys" >&2
    exit 1
  fi
done

helper_links="$(otool -L "$payload/omnifs-libkrun" | tail -n +2)"
if grep -Eiq '(libkrun|/opt/homebrew|/usr/local)' <<<"$helper_links"; then
  echo "omnifs-libkrun must dynamically load only its configured packaged dylib:" >&2
  printf '%s\n' "$helper_links" >&2
  exit 1
fi

echo "Darwin arm64 payload audit passed"
