#!/usr/bin/env bash
# Fail if current contract docs drift away from the theme-file template.
set -euo pipefail
cd "$(dirname "$0")/../.."

report=""

index="docs/contracts/00-index.md"
expected=(
  docs/contracts/00-index.md
  docs/contracts/10-system.md
  docs/contracts/20-provider-sdk.md
  docs/contracts/30-projection-tree.md
  docs/contracts/40-frontends.md
  docs/contracts/50-control-plane.md
  docs/contracts/60-build-validation.md
)

for file in "${expected[@]}"; do
  [[ -f "$file" ]] || report+="  MISSING  $file"$'\n'
done

while IFS= read -r -d '' file; do
  grep -q '^# ' "$file" || report+="  MISSING HEADING  $file"$'\n'
  grep -q '^Status: current-contract$' "$file" || report+="  BAD STATUS  $file"$'\n'

  if [[ "$file" == "$index" ]]; then
    grep -q '^## Rules$' "$file" || report+="  MISSING RULES  $file"$'\n'
    continue
  fi

  for marker in '^Owns: ' '^## Read when$' '^## Rules$' '^## Must not$' '^## Code$' '^## Validation$'; do
    if ! grep -q "$marker" "$file"; then
      report+="  MISSING ${marker//^/}  $file"$'\n'
    fi
  done
done < <(find docs/contracts -name '*.md' -print0)

if [[ -n "$report" ]]; then
  echo "Contract doc check FAILED: docs/contracts files must keep the theme-file template."
  printf '%s' "$report"
  exit 1
fi

echo "Contract doc check passed."
