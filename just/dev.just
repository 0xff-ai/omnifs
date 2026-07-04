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

# Refresh generated and formatted repository artifacts.
[group('dev')]
refresh:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --all
    just --fmt
    for justfile in just/*.just; do
      just --fmt --justfile "$justfile" --working-directory .
    done
    just npm sync
    just schema
    just openapi

# Regenerate the provider JSON schema.
[group('dev')]
schema:
    cargo run -q -p omnifs-workspace --example write_provider > crates/omnifs-workspace/schema/omnifs.provider.schema.json

# Regenerate the control API OpenAPI spec from the daemon implementation.
[group('dev')]
openapi:
    cargo run -p omnifs-daemon --bin omnifsd-openapi --quiet > crates/omnifs-api/openapi/daemon.json

# Build provider artifacts and launch the supported contributor runtime.
[group('dev')]
dev *args:
    bun scripts/dev.ts {{ args }}
