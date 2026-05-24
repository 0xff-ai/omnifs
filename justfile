image := "ghcr.io/raulk/omnifs:latest"

check: build-providers
    cargo fmt --all --check
    cargo clippy -p omnifs-cli -p omnifs-host -p omnifs-sdk -p omnifs-sdk-macros -p omnifs-mount-schema -p omnifs-auth -p omnifs-creds -p omnifs-model -- -D warnings
    cargo test -p omnifs-cli -p omnifs-host -p omnifs-sdk -p omnifs-sdk-macros
    just check-providers
    just check-npm-platforms

check-providers:
    cargo check -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider -p 'omnifs-tool-*' --target wasm32-wasip2
    cargo clippy -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider -p 'omnifs-tool-*' --target wasm32-wasip2 -- -D warnings
    cargo test -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider --target wasm32-wasip2 --no-run

build-providers:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --target wasm32-wasip2 --release \
        -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider \
        -p 'omnifs-tool-*'

test: build-providers
    cargo test --workspace

test-integration: build-providers
    cargo test -p omnifs-host --test runtime_test

build:
    docker build -t {{image}} .

smoke-init:
    bash tests/smoke/init_no_source.sh

# Regenerate the checked-in provider manifest JSON schema. Run after
# changing types in `crates/omnifs-mount-schema/src/lib.rs` that affect
# the manifest shape; the result is asserted against by
# `checked_in_provider_manifest_matches_generated`.
regen-schema:
    cargo run -q -p omnifs-mount-schema --example write_provider \
        > crates/omnifs-mount-schema/schema/omnifs.provider.schema.json

check-npm-platforms:
    node npm/scripts/validate-platforms.mjs

check-release:
    cargo run -q -p omnifs-release -- check release-pr

release-prompt:
    cargo run -q -p omnifs-release -- prompt

prepare-release *args:
    cargo run -p omnifs-release -- prepare {{args}}
