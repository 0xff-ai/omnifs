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
# Docker via the `thin-builder` stage `just dev` never touches: the slim
# `omnifs-thin` binary needs no engine runtime or Wasmtime, so unlike the full `omnifs`
# binary's `builder` stage, no provider-store build context is needed here.
[group('dev')]
frontend-image:
    docker build -t omnifs-frontend:dev --target frontend-dev .

# Build the krunkit guest disk image: an EFI-bootable Debian trixie arm64 raw
# disk with fuse3 and the linux/arm64 `omnifs-thin` binary baked in, whose init
# starts the FUSE frontend runner once a per-launch seed volume delivers the
# attach parameters. Mirrors `frontend-image`'s reuse of the shared
# `thin-builder` Dockerfile stage; see scripts/guest-image/build.sh.
[group('dev')]
guest-image:
    bash scripts/guest-image/build.sh

# Boot the guest image once with a throwaway seed ISO and check the serial
# console for a successful boot and the frontend runner starting. See
# scripts/guest-image/smoke.sh for the intentionally boot-only success criteria.
[group('dev')]
guest-image-smoke:
    bash scripts/guest-image/smoke.sh

# Run the krunkit driver's conformance lane against a live guest: builds the
# guest image if missing, then runs `crates/omnifs-itest/tests/frontend_krunkit`
# with the live-acceptance opt-in set. macOS (Apple Silicon) only, and
# LOCAL-ONLY: GitHub-hosted macOS runners cannot nest virtualization, so this
# never runs in CI (see docs/contracts/60-build-validation.md). Requires
# `krunkit` and `socat` on PATH.
[group('dev')]
krunkit-conformance:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! -f target/guest-image/omnifs-guest.raw ]]; then
      echo "guest image missing; building it now (run \`just guest-image\` yourself to rebuild after guest-image source changes)"
      just guest-image
    fi
    OMNIFS_ACCEPTANCE_LIVE=1 cargo nextest run -p omnifs-itest --test frontend_krunkit
