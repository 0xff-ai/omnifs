#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -gt 0 ]; then
  exec "$@"
fi

# OMNIFS_HOME and OMNIFS_MOUNT_POINT are image ENV (see Dockerfile).
: "${OMNIFS_LOG_FILE:=/tmp/omnifs.log}"
: "${OMNIFS_LISTEN:=0.0.0.0:7878}"
: "${RUST_LOG:=info}"
export OMNIFS_HOME RUST_LOG OMNIFS_MOUNT_POINT
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

# The daemon resolves its own mount point; it reads OMNIFS_MOUNT_POINT (exported
# above) and falls back to $HOME/omnifs host-native.
exec omnifs daemon \
  --listen "$OMNIFS_LISTEN" \
  --root-symlinks
