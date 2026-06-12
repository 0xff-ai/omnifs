#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -gt 0 ]; then
  exec "$@"
fi

: "${OMNIFS_MOUNT_POINT:=/omnifs}"
: "${OMNIFS_HOME:=/root/.omnifs}"
: "${OMNIFS_LOG_FILE:=/tmp/omnifs.log}"
: "${OMNIFS_LISTEN:=0.0.0.0:7878}"
: "${RUST_LOG:=info}"
export OMNIFS_HOME RUST_LOG
omnifs_cache_dir="$OMNIFS_HOME/cache"

mkdir -p \
  "$OMNIFS_MOUNT_POINT" \
  "$OMNIFS_HOME" \
  "$omnifs_cache_dir" \
  "$(dirname "$OMNIFS_LOG_FILE")"

log_pipe=/tmp/omnifs-entrypoint.log.pipe
rm -f "$log_pipe"
mkfifo "$log_pipe"
tee -a "$OMNIFS_LOG_FILE" < "$log_pipe" &
exec >"$log_pipe" 2>&1
rm -f "$log_pipe"

exec omnifsd \
  --mount-point "$OMNIFS_MOUNT_POINT" \
  --listen "$OMNIFS_LISTEN" \
  --root-symlinks
