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

# Build the omnifs-frontend:dev image from the working tree. Compiles inside
# Docker via the `fuse-builder` stage `just dev` never touches: the slim
# `omnifs-fuse` binary needs no engine or Wasmtime, so unlike the full `omnifs`
# binary's `builder` stage, no provider-store build context is needed here.
[group('dev')]
frontend-image:
    docker build -t omnifs-frontend:dev --target frontend-dev .

# Build the krunkit guest disk image: an EFI-bootable Debian trixie arm64 raw
# disk with fuse3 and the linux/arm64 `omnifs-fuse` binary baked in, whose init
# starts the FUSE frontend runner once a per-launch seed volume delivers the
# attach parameters. Mirrors `frontend-image`'s reuse of the shared
# `fuse-builder` Dockerfile stage; see scripts/guest-image/build.sh.
[group('dev')]
guest-image:
    bash scripts/guest-image/build.sh

# Boot the guest image once with a throwaway seed ISO and check the serial
# console for a successful boot and the frontend runner starting. See
# scripts/guest-image/smoke.sh for the intentionally boot-only success criteria.
[group('dev')]
guest-image-smoke:
    bash scripts/guest-image/smoke.sh
