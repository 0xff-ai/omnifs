set shell := ["bash", "-euo", "pipefail", "-c"]
set working-directory := '..'

# Build native workspace crates.
build:
    #!/usr/bin/env bash
    set -euo pipefail
    excludes=()
    while IFS= read -r package; do
      excludes+=(--exclude "$package")
    done < <(
      cargo metadata --no-deps --format-version 1 \
        | jq -r '.packages[] | select(.manifest_path | contains("/providers/")) | .name'
    )
    excludes+=(--exclude omnifs-embed-metadata)
    cargo build --workspace "${excludes[@]}"

# Lint native host crates with warnings denied.
clippy:
    #!/usr/bin/env bash
    set -euo pipefail
    wasm_dir="target/wasm32-wasip2/release"
    if [[ -z "$(find "$wasm_dir" -maxdepth 1 -name 'omnifs_provider_*.wasm' -print -quit 2>/dev/null)" ]]; then
      just providers build
    fi
    cargo clippy --workspace \
      --exclude test-provider \
      --exclude omnifs-embed-metadata \
      --all-targets \
      -- -D warnings

# Run host tests directly. CI archives across jobs via scripts/ci/host-*.sh.
# Provider crates are included: each owns its host-driven scenario tests.
test:
    OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run --profile ci --workspace \
      --exclude test-provider \
      --exclude omnifs-embed-metadata

# Record (or re-record) provider tapes against live upstreams. Pass a scenario
# name to record just one; omit it to record every scenario in the provider's
# crate. Authenticated providers need their OMNIFS_RECORD_* env var set.
itest-record provider scenario="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "{{scenario}}" ]]; then
      filter='test(scenarios::{{scenario}})'
    else
      filter='test(scenarios::)'
    fi
    OMNIFS_TAPE=record INSTA_UPDATE=always cargo nextest run -p omnifs-provider-{{provider}} \
      -E "$filter" --no-capture
