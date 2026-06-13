#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [[ -n "${OMNIFS_NFS_TEST_ROOT:-}" && -z "${OMNIFS_FRONTEND_TEST_ROOT:-}" ]]; then
  export OMNIFS_FRONTEND_TEST_ROOT="$OMNIFS_NFS_TEST_ROOT"
fi
exec "$script_dir/frontend-test-provider.sh" "$@"
