#!/bin/sh
set -eu
mkdir -p /data
if [ ! -s /data/test.db ]; then
  cp /seed/test.db /data/test.db
fi
exec sleep infinity
