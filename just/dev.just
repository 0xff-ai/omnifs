# Format the Rust workspace.
[group('dev')]
fmt:
    cargo fmt --all

# Check Rust formatting without writing changes.
[group('dev')]
fmt-check:
    cargo fmt --all --check

# Check that the just command surface is formatted.
[group('dev')]
just-check:
    #!/usr/bin/env bash
    set -euo pipefail
    just --fmt --check
    for justfile in just/*.just; do
      just --fmt --check --justfile "$justfile" --working-directory .
    done

# Lint native host crates with warnings denied.
[group('dev')]
host-clippy:
    cargo clippy --workspace \
      --exclude 'omnifs-provider-*' \
      --exclude 'omnifs-tool-*' \
      --exclude test-provider \
      --all-targets \
      -- -D warnings

# Build provider WASM, then run the host test suite.
[group('dev')]
test: providers-build ci-host

# Run the bun script tests.
[group('dev')]
scripts-test:
    bun test scripts

# Run the full local validation path for broad changes.
[group('dev')]
check: fmt-check host-clippy test providers-check npm-validate scripts-test

# Regenerate the provider JSON schema.
[group('dev')]
schema:
    cargo run -q -p omnifs-mount-schema --example write_provider > crates/omnifs-mount-schema/schema/omnifs.provider.schema.json

# Prepare smoke-test fixtures without launching the runtime.
[group('dev')]
smoke-init:
    bash tests/smoke/init_no_source.sh

# Launch the supported contributor runtime.
[group('dev')]
dev:
    omnifs dev

# Launch the supported contributor runtime without prompting.
[group('dev')]
dev-y:
    omnifs dev -y
