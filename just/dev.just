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

# Check that docs do not reference nonexistent docs paths.
[group('dev')]
docs-check:
    bash scripts/ci/check-doc-links.sh

# Run the full local validation path for broad changes.
[group('dev')]
check: fmt-check openapi-check host-clippy test providers-check npm-validate scripts-test docs-check

# Regenerate the provider JSON schema.
[group('dev')]
schema:
    cargo run -q -p omnifs-provider --example write_provider > crates/omnifs-provider/schema/omnifs.provider.schema.json

# Regenerate the control API OpenAPI spec from the daemon implementation.
[group('dev')]
openapi:
    bun scripts/openapi.ts generate

# Check that the checked-in OpenAPI spec matches the implementation.
[group('dev')]
openapi-check:
    bun scripts/openapi.ts check

# Smoke a mounted test-provider tree's bash-tool and read semantics, FUSE or NFS (pass the mount root).
[group('dev')]
smoke-frontend-test-provider root:
    bash tests/smoke/frontend-test-provider.sh {{root}}

# Launch the supported contributor runtime.
[group('dev')]
dev:
    omnifs dev

# Launch the supported contributor runtime without prompting.
[group('dev')]
dev-y:
    omnifs dev -y
