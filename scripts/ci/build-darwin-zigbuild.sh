#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

if [[ $# -ne 1 ]]; then
  echo "usage: scripts/ci/build-darwin-zigbuild.sh TARGET" >&2
  exit 2
fi

target="$1"
case "$target" in
  x86_64-apple-darwin | aarch64-apple-darwin) ;;
  *)
    echo "unsupported Darwin target: $target" >&2
    exit 2
    ;;
esac

target_dir="${CARGO_TARGET_DIR:-$root/target/zigbuild-darwin}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

# rust-toolchain.toml pins the exact Rust release, so cargo auto-installs that
# toolchain plus apple-darwin std libs into RUSTUP_HOME on first run. Persist
# that across CI runs via $root/.cache/darwin-rustup (cached by the cli-cross
# actions/cache step), distinct from the container's pre-installed
# /usr/local/rustup so we never shadow the image's toolchain.
rustup_home="${DARWIN_RUSTUP_HOME:-$root/.cache/darwin-rustup}"

image="ghcr.io/rust-cross/cargo-zigbuild:0.22.3@sha256:b66e2a5063921aca74fc53248d75d187b7499fe1e076d78eb7d87ab1dbc52f6a"

mkdir -p "$target_dir" "$cargo_home" "$rustup_home"
docker pull "$image"
docker run --rm \
  --platform linux/amd64 \
  -v "$root:/work:ro" \
  -v "$target_dir:/cargo-target" \
  -v "$cargo_home:/cargo-home" \
  -v "$rustup_home:/cargo-rustup" \
  -w /work \
  -e CARGO_HOME=/cargo-home \
  -e CARGO_TARGET_DIR=/cargo-target \
  -e RUSTUP_HOME=/cargo-rustup \
  -e HOST_UID="$(id -u)" \
  -e HOST_GID="$(id -g)" \
  -e OMNIFS_RELEASE \
  -e TARGET="$target" \
  "$image" \
  bash -lc '
    set -euo pipefail
    trap '\''chown -R "$HOST_UID:$HOST_GID" /cargo-home /cargo-target /cargo-rustup'\'' EXIT
    export PATH="/usr/local/cargo/bin:/cargo-home/bin:$PATH"
    if [[ -f /usr/local/cargo/config.toml && ! -f /cargo-home/config.toml ]]; then
      cp /usr/local/cargo/config.toml /cargo-home/config.toml
    fi
    rustup target add "$TARGET"

    export CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS=
    export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS=
    ulimit -n 4096 2>/dev/null || true

    cargo zigbuild --release \
      -p omnifs-cli \
      -p omnifs-thin \
      --target "$TARGET" \
      --bin omnifs \
      --bin omnifs-thin
  '

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "artifact=$target_dir/$target/release/omnifs"
    echo "thin_artifact=$target_dir/$target/release/omnifs-thin"
  } >>"$GITHUB_OUTPUT"
fi

file "$target_dir/$target/release/omnifs"
file "$target_dir/$target/release/omnifs-thin"
