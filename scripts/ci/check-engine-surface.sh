#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
expected="$repo_root/scripts/ci/engine-surface.txt"
actual="$(mktemp)"
trap 'rm -f "$actual"' EXIT

grep '^pub ' "$repo_root/crates/omnifs-engine/src/lib.rs" > "$actual"
diff -u "$expected" "$actual"
