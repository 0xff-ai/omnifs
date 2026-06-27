# Helpers sourced by scripts/ci/*.sh. Source with:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
#
# Sets $root to the repo root.

# shellcheck disable=SC2034
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
