# Helpers sourced by scripts/ci/*.sh. Source with:
#
#   source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
#
# Sets $root to the repo root and defines version_pin(), which reads a quoted
# string pin from tools/versions.toml (the same sed idiom as just/providers.just).

# shellcheck disable=SC2034
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

version_pin() {
  local value
  value="$(sed -nE "s/^$1[[:space:]]*=[[:space:]]*\"([^\"]+)\".*/\1/p" "$root/tools/versions.toml")"
  if [[ -z "$value" ]]; then
    printf 'missing or non-string key in tools/versions.toml: %s\n' "$1" >&2
    return 1
  fi
  printf '%s\n' "$value"
}
