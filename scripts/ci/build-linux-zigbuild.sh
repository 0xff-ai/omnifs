#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

target="${1:-x86_64-unknown-linux-gnu.2.17}"
package="${OMNIFS_PACKAGE:-omnifs-cli}"
bin="${OMNIFS_BIN:-omnifs}"
target_root="${CARGO_TARGET_DIR:-$root/target}"
case "$target_root" in
  /*) ;;
  *) target_root="$root/$target_root" ;;
esac

cargo_zigbuild_version="$(version_pin cargo_zigbuild)"
zig_version="$(version_pin zig)"

if ! command -v zig >/dev/null 2>&1; then
  echo "zig is not installed; install Zig $zig_version before running this script" >&2
  exit 1
fi

actual_zig_version="$(zig version)"
if [[ "$actual_zig_version" != "$zig_version" ]]; then
  echo "zig version mismatch: expected $zig_version, got $actual_zig_version" >&2
  exit 1
fi

if ! command -v cargo-zigbuild >/dev/null 2>&1 ||
  ! cargo-zigbuild --version 2>/dev/null | grep -q " $cargo_zigbuild_version$"; then
  cargo install --locked --version "$cargo_zigbuild_version" cargo-zigbuild
fi

base_target="$(printf '%s\n' "$target" | sed -E 's/\.[0-9]+(\.[0-9]+)?$//')"
rustup target add "$base_target"

cd "$root"

# The repo currently configures Linux GNU targets with `-fuse-ld=mold`.
# cargo-zigbuild replaces the linker with Zig, so carrying the mold flag into
# this build is both unnecessary and can exceed the low macOS fd limit.
target_env="$(printf '%s\n' "$base_target" | tr '[:lower:].-' '[:upper:]__')"
export "CARGO_TARGET_${target_env}_RUSTFLAGS=${CARGO_ZIGBUILD_RUSTFLAGS:-}"
ulimit -n 4096 2>/dev/null || true

cargo zigbuild --release -p "$package" --target "$target" --bin "$bin"

artifact=""
for candidate in \
  "$target_root/$target/release/$bin" \
  "$target_root/$base_target/release/$bin" \
  "$root/target/$target/release/$bin" \
  "$root/target/$base_target/release/$bin"
do
  if [[ -f "$candidate" ]]; then
    artifact="$candidate"
    break
  fi
done

if [[ -z "$artifact" ]]; then
  echo "built artifact not found for $target" >&2
  exit 1
fi

echo "artifact=$artifact"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "artifact=$artifact" >>"$GITHUB_OUTPUT"
fi
file "$artifact"

case "$base_target" in
  *-unknown-linux-gnu)
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
      case "$base_target" in
        x86_64-unknown-linux-gnu) docker_platform="linux/amd64" ;;
        aarch64-unknown-linux-gnu) docker_platform="linux/arm64" ;;
        *)
          echo "no docker platform mapping for $base_target; skipping Linux readelf/ldd inspection" >&2
          exit 0
          ;;
      esac

      docker run --rm \
        --platform "$docker_platform" \
        -v "$artifact:/work/omnifs:ro" \
        ubuntu:22.04 \
        /bin/bash -lc '
          set -euo pipefail
          apt-get update >/dev/null
          apt-get install -y --no-install-recommends binutils file >/dev/null
          file /work/omnifs
          readelf -V /work/omnifs | grep "GLIBC_" || true
          ldd /work/omnifs
        '
    else
      echo "docker unavailable; skipping Linux readelf/ldd inspection" >&2
    fi
    ;;
esac
