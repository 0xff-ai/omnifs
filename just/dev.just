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

# Check that docs do not reference nonexistent docs paths.
[group('dev')]
docs-check:
    bash scripts/ci/check-doc-links.sh
    bash scripts/ci/check-doc-contracts.sh

# Run the CI-shaped local validation path for broad changes.
[group('dev')]
check: fmt-check just-check docs-check npm::validate openapi-check providers::check providers::build providers::validate host::clippy host::test

# Regenerate the provider JSON schema.
[group('dev')]
schema:
    cargo run -q -p omnifs-provider --example write_provider > crates/omnifs-provider/schema/omnifs.provider.schema.json

# Regenerate the control API OpenAPI spec from the daemon implementation.
[group('dev')]
openapi:
    cargo xtask openapi generate

# Check that the checked-in OpenAPI spec matches the implementation.
[group('dev')]
openapi-check:
    cargo xtask openapi check

# Build provider artifacts and launch the supported contributor runtime.
[group('dev')]
dev *args: providers::build
    cargo run -p omnifs-cli -- dev {{ args }}
