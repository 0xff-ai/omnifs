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
#
# The mkosi profile (mkosi/mkosi.profiles/{dev,release}/mkosi.conf) selects
# whether root gets the dev-only unlocked autologin console or the locked,
# no-autologin shape CI publishes. Defaults to `dev`; override with
# `--profile release` or `GUEST_IMAGE_PROFILE=release`.
#
# `OMNIFS_FUSE_BIN`, if set, points at an already-built linux/arm64
# `omnifs-fuse` binary and skips the `fuse-builder` docker build entirely.
# CI's guest-image-arm64 job sets this to the `fuse-linux-arm64` job's
# downloaded artifact, so the guest image is assembled from the one binary
# CI already built and will attest, rather than recompiling it a second time
# inside this script.
set -euo pipefail

profile="${GUEST_IMAGE_PROFILE:-dev}"
if [[ "${1:-}" == "--profile" ]]; then
  profile="${2:?--profile requires a value}"
  shift 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="${OUT_DIR:-$root/target/guest-image}"
builder_image="omnifs-guest-image-builder:local"
fuse_builder_image="omnifs-guest-fuse-builder:local"

mkdir -p "$out_dir"

bin_dir="$(mktemp -d)"
trap 'rm -rf "$bin_dir"' EXIT

if [[ -n "${OMNIFS_FUSE_BIN:-}" ]]; then
  echo "== using prebuilt omnifs-fuse binary: $OMNIFS_FUSE_BIN =="
  cp "$OMNIFS_FUSE_BIN" "$bin_dir/omnifs-fuse"
  chmod 0755 "$bin_dir/omnifs-fuse"
else
  echo "== extracting the linux/arm64 omnifs-fuse binary (top-level Dockerfile fuse-builder stage) =="
  docker build \
    --target fuse-builder \
    --platform linux/arm64 \
    -t "$fuse_builder_image" \
    "$root"

  cid="$(docker create "$fuse_builder_image")"
  docker cp "$cid:/omnifs-fuse" "$bin_dir/omnifs-fuse"
  docker rm -v "$cid" >/dev/null
  chmod 0755 "$bin_dir/omnifs-fuse"
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
