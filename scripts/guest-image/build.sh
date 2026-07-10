#!/usr/bin/env bash
# Build the krunkit guest disk image end to end: extract the linux/arm64
# `omnifs` binary from the top-level Dockerfile's `builder` stage (the same
# stage `just frontend-image`/`just dev` use, so this never compiles a second
# way), then hand it to a containerized mkosi build that assembles a Debian
# trixie arm64 raw disk image with fuse3 and the binary baked in.
#
# mkosi needs Linux (loop devices, systemd-repart) that macOS does not have,
# so it runs inside a privileged container built from
# scripts/guest-image/builder.Dockerfile; the image mkosi produces is a plain
# EFI-bootable raw disk, not a container artifact.
#
# Requires PROVIDER_STORE to point at a built provider-store bundle (default
# target/omnifs-provider-store, produced by `just providers build`): the CLI
# build embeds it even though the guest frontend runner never executes a
# provider.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
provider_store="${PROVIDER_STORE:-$root/target/omnifs-provider-store}"
out_dir="${OUT_DIR:-$root/target/guest-image}"
builder_image="omnifs-guest-image-builder:local"
cli_builder_image="omnifs-guest-cli-builder:local"

if [[ ! -d "$provider_store" ]]; then
  echo "missing provider store: $provider_store (run \`just providers build\` first)" >&2
  exit 1
fi

mkdir -p "$out_dir"

echo "== extracting the linux/arm64 omnifs binary (top-level Dockerfile builder stage) =="
docker build \
  --target builder \
  --platform linux/arm64 \
  --build-context "provider-wasm=$provider_store" \
  -t "$cli_builder_image" \
  "$root"

bin_dir="$(mktemp -d)"
trap 'rm -rf "$bin_dir"' EXIT
cid="$(docker create "$cli_builder_image")"
docker cp "$cid:/omnifs" "$bin_dir/omnifs"
docker rm -v "$cid" >/dev/null
chmod 0755 "$bin_dir/omnifs"

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
