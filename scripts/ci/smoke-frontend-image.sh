#!/usr/bin/env bash
# Smoke the frontend image directly: the three structural guarantees it must
# uphold on its own, with no daemon and no attach target. The full
# attach/mount path (a live daemon, a real TCP attach, an actual FUSE mount)
# is exercised by the fuse-docker itest gate, a later slice, not here.
#
# Requires IMAGE (container image ref).
set -euo pipefail

: "${IMAGE:?IMAGE must be set to the frontend image ref}"

echo "== version =="
docker run --rm --entrypoint /usr/local/bin/omnifs-fuse "$IMAGE" --version

echo "== GNU tail, not uutils (tail -f fidelity) =="
docker run --rm --entrypoint tail "$IMAGE" --version | head -1 | grep -q 'GNU coreutils'

echo "== fails loudly without an attach target =="
set +e
output="$(docker run --rm "$IMAGE" 2>&1)"
run_status=$?
set -e
if [[ "$run_status" -eq 0 ]]; then
  echo "expected a nonzero exit when OMNIFS_ATTACH_ADDR is unset, got 0" >&2
  echo "$output" >&2
  exit 1
fi
echo "$output" | grep -qi "OMNIFS_ATTACH_ADDR"
echo "$output"
