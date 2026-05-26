#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

target_dir="${CARGO_TARGET_DIR:-$root/target/zigbuild-darwin}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

# The container ships rustc 1.93.0 but rust-toolchain.toml pins 1.91.0, so
# cargo auto-installs 1.91.0 + apple-darwin std libs into RUSTUP_HOME on first
# run. Persist that across CI runs via $root/.cache/darwin-rustup (cached by
# the cli-cross actions/cache step), distinct from the container's
# pre-installed /usr/local/rustup so we never shadow the image's toolchain.
rustup_home="${DARWIN_RUSTUP_HOME:-$root/.cache/darwin-rustup}"

image="$(version_pin cargo_zigbuild_container)"

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
  "$image" \
  bash -lc '
    set -euo pipefail
    trap '\''chown -R "$HOST_UID:$HOST_GID" /cargo-home /cargo-target /cargo-rustup'\'' EXIT
    export PATH="/usr/local/cargo/bin:/cargo-home/bin:$PATH"
    if [[ -f /usr/local/cargo/config.toml && ! -f /cargo-home/config.toml ]]; then
      cp /usr/local/cargo/config.toml /cargo-home/config.toml
    fi
    rustup target add x86_64-apple-darwin aarch64-apple-darwin

    export CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS=
    export CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS=
    ulimit -n 4096 2>/dev/null || true

    cargo zigbuild --release \
      -p omnifs-cli \
      --target x86_64-apple-darwin \
      --target aarch64-apple-darwin \
      --bin omnifs
  '

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "x64_artifact=$target_dir/x86_64-apple-darwin/release/omnifs"
    echo "arm64_artifact=$target_dir/aarch64-apple-darwin/release/omnifs"
  } >>"$GITHUB_OUTPUT"
fi

file "$target_dir/x86_64-apple-darwin/release/omnifs"
file "$target_dir/aarch64-apple-darwin/release/omnifs"
