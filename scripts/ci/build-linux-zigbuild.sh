#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

target="${1:-x86_64-unknown-linux-gnu.2.17}"
package="${OMNIFS_PACKAGE:-omnifs-cli}"
bin="${OMNIFS_BIN:-omnifs}"
build_daemon="${OMNIFS_BUILD_DAEMON:-0}"
target_root="${CARGO_TARGET_DIR:-$root/target}"
case "$target_root" in
  /*) ;;
  *) target_root="$root/$target_root" ;;
esac

cargo_zigbuild_version="0.22.3"
zig_version="0.15.2"

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

find_artifact() {
  local artifact_bin="$1"
  local artifact=""
  local candidate
  for candidate in \
    "$target_root/$target/release/$artifact_bin" \
    "$target_root/$base_target/release/$artifact_bin" \
    "$root/target/$target/release/$artifact_bin" \
    "$root/target/$base_target/release/$artifact_bin"
  do
    if [[ -f "$candidate" ]]; then
      artifact="$candidate"
      break
    fi
  done

  if [[ -z "$artifact" ]]; then
    echo "built artifact not found for $target: $artifact_bin" >&2
    exit 1
  fi

  printf '%s\n' "$artifact"
}

emit_output() {
  local name="$1"
  local value="$2"
  echo "$name=$value"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    echo "$name=$value" >>"$GITHUB_OUTPUT"
  fi
}

inspect_linux_artifacts() {
  case "$base_target" in
    *-unknown-linux-gnu) ;;
    *) return 0 ;;
  esac

  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    echo "docker unavailable; skipping Linux readelf/ldd inspection" >&2
    return 0
  fi

  local docker_platform=""
  case "$base_target" in
    x86_64-unknown-linux-gnu) docker_platform="linux/amd64" ;;
    aarch64-unknown-linux-gnu) docker_platform="linux/arm64" ;;
    *)
      echo "no docker platform mapping for $base_target; skipping Linux readelf/ldd inspection" >&2
      return 0
      ;;
  esac

  local inspect_dir
  inspect_dir="$(mktemp -d)"
  local artifact
  for artifact in "$@"; do
    cp "$artifact" "$inspect_dir/$(basename "$artifact")"
  done

  docker run --rm \
    --platform "$docker_platform" \
    -v "$inspect_dir:/work:ro" \
    ubuntu:22.04 \
    /bin/bash -lc '
      set -euo pipefail
      apt-get update >/dev/null
      apt-get install -y --no-install-recommends binutils file >/dev/null
      for binary in /work/*; do
        file "$binary"
        readelf -V "$binary" | grep "GLIBC_" || true
        ldd "$binary"
      done
    '
  rm -rf "$inspect_dir"
}

if [[ "$build_daemon" == "1" ]]; then
  # The single `omnifs` binary contains the daemon (`omnifs daemon`); there is
  # no separate `omnifsd` artifact.
  cargo zigbuild --release --target "$target" \
    -p omnifs-cli --bin omnifs

  artifact="$(find_artifact omnifs)"
  emit_output artifact "$artifact"
  file "$artifact"
  inspect_linux_artifacts "$artifact"
else
  cargo zigbuild --release -p "$package" --target "$target" --bin "$bin"

  artifact="$(find_artifact "$bin")"
  emit_output artifact "$artifact"
  file "$artifact"
  inspect_linux_artifacts "$artifact"
fi
