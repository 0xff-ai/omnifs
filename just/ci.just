# Run the fast CI preflight checks.
[group('ci')]
ci-preflight-fast: just-check fmt-check

# Run policy-backed CI metadata checks.
[group('ci')]
ci-preflight-policy: npm-validate scripts-test

# Run the CI WASM lane.
[group('ci')]
ci-wasm: providers-check providers-build wasm-validate

# Lint, compile, and test native host crates in CI.
[group('ci')]
ci-host: ci-host-build ci-host-test

# Compile host tests into CI artifacts without requiring provider WASM sidecars.
[group('ci')]
ci-host-build:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf dist/host-tests
    mkdir -p dist/host-tests
    just host-clippy
    cargo nextest archive --profile ci --workspace \
      --exclude 'omnifs-provider-*' \
      --exclude 'omnifs-tool-*' \
      --exclude test-provider \
      --archive-file dist/host-tests/host-tests.tar.zst

# Run host tests against already-staged WASM sidecar artifacts.
[group('ci')]
ci-host-test:
    #!/usr/bin/env bash
    set -euo pipefail
    test -f dist/host-tests/host-tests.tar.zst
    cargo nextest run --profile ci --archive-file dist/host-tests/host-tests.tar.zst

# Build a Linux CLI artifact for the requested Rust target.
[group('ci')]
ci-build-linux target:
    scripts/ci/build-linux-zigbuild.sh '{{ target }}'

# Build Darwin CLI artifacts from Linux.
[group('ci')]
ci-build-darwin:
    scripts/ci/build-darwin-zigbuild.sh

# Build Darwin omnifsd artifacts from Linux, mirroring ci-build-darwin.
[group('ci')]
ci-build-darwin-daemon:
    #!/usr/bin/env bash
    set -euo pipefail
    root="$(pwd)"
    target_dir="${CARGO_TARGET_DIR:-$root/target/zigbuild-darwin}"
    cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    rustup_home="${DARWIN_RUSTUP_HOME:-$root/.cache/darwin-rustup}"
    image="$(bun "$root/scripts/toolchain/versions.ts" cargo_zigbuild_container)"

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
          -p omnifs-daemon \
          --target x86_64-apple-darwin \
          --target aarch64-apple-darwin \
          --bin omnifsd
      '

    if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
      {
        echo "x64_artifact=$target_dir/x86_64-apple-darwin/release/omnifsd"
        echo "arm64_artifact=$target_dir/aarch64-apple-darwin/release/omnifsd"
      } >>"$GITHUB_OUTPUT"
    fi

    file "$target_dir/x86_64-apple-darwin/release/omnifsd"
    file "$target_dir/aarch64-apple-darwin/release/omnifsd"

# Package a CLI binary for an npm platform id.
[group('ci')]
ci-package-cli platform binary:
    scripts/ci/package-cli.sh '{{ platform }}' '{{ binary }}'

# Assemble and optionally push the runtime image from staged artifacts.
[group('ci')]
ci-build-runtime:
    scripts/ci/build-runtime-image.sh

# Run the runtime image smoke test.
[group('ci')]
ci-smoke-container:
    scripts/ci/smoke-container.sh
