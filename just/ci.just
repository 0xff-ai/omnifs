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
    OMNIFS_PACKAGE=omnifs-daemon OMNIFS_BIN=omnifsd scripts/ci/build-darwin-zigbuild.sh

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
