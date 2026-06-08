#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [[ -n "${OMNIFS_NFS_SMOKE_REPO_ROOT:-}" && -z "${OMNIFS_FRONTEND_SMOKE_REPO_ROOT:-}" ]]; then
  export OMNIFS_FRONTEND_SMOKE_REPO_ROOT="$OMNIFS_NFS_SMOKE_REPO_ROOT"
fi
if [[ -n "${OMNIFS_NFS_SMOKE_FILE:-}" && -z "${OMNIFS_FRONTEND_SMOKE_FILE:-}" ]]; then
  export OMNIFS_FRONTEND_SMOKE_FILE="$OMNIFS_NFS_SMOKE_FILE"
fi
exec "$script_dir/frontend-bash-tools.sh" "$@"
