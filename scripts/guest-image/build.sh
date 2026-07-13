#!/usr/bin/env bash
# Build the arm64 krunkit guest with containerized mkosi. By default the script
# extracts `omnifs-thin` from Dockerfile's shared `thin-builder`; CI supplies
# the already-built binary through OMNIFS_THIN_BIN. The dev profile is local
# and autologins on the console, while the published release profile does not.
set -euo pipefail

profile="${GUEST_IMAGE_PROFILE:-dev}"
if (($#)); then
  if [[ "$1" != "--profile" || $# -ne 2 ]]; then
    echo "usage: scripts/guest-image/build.sh [--profile dev|release]" >&2
    exit 2
  fi
  profile="$2"
fi
if [[ "$profile" != "dev" && "$profile" != "release" ]]; then
  echo "guest image profile must be dev or release, got: $profile" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${OUT_DIR:-$root/target/guest-image}"
builder_image="omnifs-guest-image-builder:local"
thin_builder_image="omnifs-guest-thin-builder:local"

mkdir -p "$out_dir"

bin_dir="$(mktemp -d)"
trap 'rm -rf "$bin_dir"' EXIT

if [[ -n "${OMNIFS_THIN_BIN:-}" ]]; then
  echo "== using prebuilt omnifs-thin binary: $OMNIFS_THIN_BIN =="
  cp "$OMNIFS_THIN_BIN" "$bin_dir/omnifs-thin"
  chmod 0755 "$bin_dir/omnifs-thin"
else
  echo "== extracting the linux/arm64 omnifs-thin binary (top-level Dockerfile thin-builder stage) =="
  docker build \
    --target thin-builder \
    --platform linux/arm64 \
    -t "$thin_builder_image" \
    "$root"

  cid="$(docker create "$thin_builder_image")"
  docker cp "$cid:/omnifs-thin" "$bin_dir/omnifs-thin"
  docker rm -v "$cid" >/dev/null
  chmod 0755 "$bin_dir/omnifs-thin"
fi

echo "== building the mkosi container =="
docker build -t "$builder_image" -f "$root/scripts/guest-image/builder.Dockerfile" "$root/scripts/guest-image"

echo "== assembling the guest disk image with mkosi (profile: $profile) =="
docker run --rm --privileged \
  -v "$root/scripts/guest-image/mkosi:/work:ro" \
  -v "$bin_dir:/mnt/bin:ro" \
  -v "$out_dir:/out" \
  "$builder_image" \
  --output-directory /out \
  --extra-tree "/mnt/bin:/usr/local/bin" \
  --profile "$profile" \
  build

echo "== done: $out_dir/omnifs-guest.raw =="
ls -lh "$out_dir/omnifs-guest.raw"
