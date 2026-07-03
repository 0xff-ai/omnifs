#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <mount> <before-dir> <after-dir>" >&2
  exit 2
fi

mount="$1"
before="$2"
after="$3"

omnifs snapshot "$mount" --out "$before"

cat >&2 <<'MSG'
Apply an upstream change for this mount, then press Enter to take the second snapshot.
MSG
read -r _

omnifs snapshot "$mount" --out "$after"

diff -r --exclude=index.json "$before" "$after"
