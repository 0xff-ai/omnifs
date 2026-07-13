#!/usr/bin/env bash
set -euo pipefail

cargo build --target wasm32-wasip2 --release \
  -p 'omnifs-provider-*' \
  -p test-provider

shopt -s nullglob
wasms=(target/wasm32-wasip2/release/omnifs_provider_*.wasm target/wasm32-wasip2/release/test_provider.wasm)
if (( ${#wasms[@]} == 0 )); then
  printf 'no provider WASM components found\n' >&2
  exit 1
fi

cargo run -p omnifs-embed-metadata -- target/wasm32-wasip2/release
bundle_wasms=(target/wasm32-wasip2/release/omnifs_provider_*.wasm)
cargo run --release -p omnifs-workspace --bin omnifs-provider-store-bundle -- \
  --out target/omnifs-provider-store \
  "${bundle_wasms[@]}"
