# Helpers sourced by scripts/ci/*.sh. Source with:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
#
# Sets $root to the repo root and defines version_pin(), which reads a scalar
# from tools/versions.toml via scripts/toolchain/versions.ts.

# shellcheck disable=SC2034
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

version_pin() {
  bun "$root/scripts/toolchain/versions.ts" "$1"
}
