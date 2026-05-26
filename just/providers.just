# Check provider and tool WASM crates.
[group('providers')]
providers-check:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(bun scripts/toolchain/wasi-env.ts)"
    cargo check --target wasm32-wasip2 \
      -p 'omnifs-provider-*' \
      -p 'omnifs-tool-*' \
      -p test-provider
    cargo clippy --target wasm32-wasip2 \
      -p 'omnifs-provider-*' \
      -p 'omnifs-tool-*' \
      -p test-provider \
      -- -D warnings
    cargo test --target wasm32-wasip2 \
      -p 'omnifs-provider-*' \
      -p test-provider \
      --no-run

# Build provider and tool WASM components.
[group('providers')]
providers-build:
    #!/usr/bin/env bash
    set -euo pipefail
    eval "$(bun scripts/toolchain/wasi-env.ts)"
    cargo build --target wasm32-wasip2 --release -p 'omnifs-tool-*'
    cargo build --target wasm32-wasip2 --release \
      -p 'omnifs-provider-*' \
      -p test-provider

# Validate built WASM components.
[group('providers')]
wasm-validate:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    wasms=(target/wasm32-wasip2/release/*.wasm)
    if (( ${#wasms[@]} == 0 )); then
      printf 'no WASM components found in target/wasm32-wasip2/release\n' >&2
      exit 1
    fi
    for wasm in "${wasms[@]}"; do
      wasm-tools validate "$wasm"
    done
