set shell := ["bash", "-euo", "pipefail", "-c"]
set working-directory := '..'

# Lint native host crates with warnings denied.
clippy:
    #!/usr/bin/env bash
    set -euo pipefail
    wasm_dir="target/wasm32-wasip2/release"
    if [[ -z "$(find "$wasm_dir" -maxdepth 1 -name 'omnifs_provider_*.wasm' -print -quit 2>/dev/null)" ]]; then
      just providers build
    fi
    cargo clippy --workspace \
      --exclude 'omnifs-provider-*' \
      --exclude test-provider \
      --exclude omnifs-embed-metadata \
      --all-targets \
      -- -D warnings

# Run host tests directly. CI archives across jobs via scripts/ci/host-*.sh.
test:
    OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run --profile ci --workspace \
      --exclude 'omnifs-provider-*' \
      --exclude test-provider \
      --exclude omnifs-embed-metadata
