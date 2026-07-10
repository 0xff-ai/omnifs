#!/usr/bin/env bash
# Build the krunkit guest disk image end to end: extract the linux/arm64
# `omnifs-fuse` binary from the top-level Dockerfile's `fuse-builder` stage
# (the same stage `just frontend-image`/`just dev` use, so this never compiles
# a second way), then hand it to a containerized mkosi build that assembles a
# Debian trixie arm64 raw disk image with fuse3 and the binary baked in.
#
# mkosi needs Linux (loop devices, systemd-repart) that macOS does not have,
# so it runs inside a privileged container built from
# scripts/guest-image/builder.Dockerfile; the image mkosi produces is a plain
# EFI-bootable raw disk, not a container artifact.
#
# `omnifs-fuse` needs no engine or Wasmtime, so unlike the full `omnifs`
# binary's `builder` stage, this needs no provider-store build context.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${OUT_DIR:-$root/target/guest-image}"
builder_image="omnifs-guest-image-builder:local"
fuse_builder_image="omnifs-guest-fuse-builder:local"

mkdir -p "$out_dir"

echo "== extracting the linux/arm64 omnifs-fuse binary (top-level Dockerfile fuse-builder stage) =="
docker build \
  --target fuse-builder \
  --platform linux/arm64 \
  -t "$fuse_builder_image" \
  "$root"

bin_dir="$(mktemp -d)"
trap 'rm -rf "$bin_dir"' EXIT
cid="$(docker create "$fuse_builder_image")"
docker cp "$cid:/omnifs-fuse" "$bin_dir/omnifs-fuse"
docker rm -v "$cid" >/dev/null
chmod 0755 "$bin_dir/omnifs-fuse"

echo "== building the mkosi container =="
docker build -t "$builder_image" -f "$root/scripts/guest-image/builder.Dockerfile" "$root/scripts/guest-image"

echo "== assembling the guest disk image with mkosi =="
docker run --rm --privileged \
  -v "$root/scripts/guest-image/mkosi:/work:ro" \
  -v "$bin_dir:/mnt/bin:ro" \
  -v "$out_dir:/out" \
  "$builder_image" \
  --output-directory /out \
  --extra-tree "/mnt/bin:/usr/local/bin" \
  build

echo "== done: $out_dir/omnifs-guest.raw =="
ls -lh "$out_dir/omnifs-guest.raw"
