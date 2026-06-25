# Install the pinned WASI SDK into .cache/wasi-sdk (version from tools/versions.toml).
# The build env (WASI_SYSROOT, CC) lives in .cargo/config.toml; this only fetches
# the toolchain the config points at. Idempotent: a matching .version stamp skips.
[group('providers')]
wasi-sdk:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(sed -nE 's/^wasi_sdk[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' tools/versions.toml)"
    release="$(sed -nE 's/^wasi_sdk_release[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' tools/versions.toml)"
    if [[ -z "$version" || -z "$release" ]]; then
      printf 'tools/versions.toml must define wasi_sdk and wasi_sdk_release\n' >&2
      exit 1
    fi
    home=".cache/wasi-sdk"
    if [[ "$(cat "$home/.version" 2>/dev/null)" == "$release" ]]; then
      exit 0
    fi
    case "$(uname -sm)" in
      "Darwin arm64")  suffix="arm64-macos" ;;
      "Darwin x86_64") suffix="x86_64-macos" ;;
      "Linux aarch64") suffix="arm64-linux" ;;
      "Linux x86_64")  suffix="x86_64-linux" ;;
      *) printf 'unsupported host for wasi-sdk install: %s\n' "$(uname -sm)" >&2; exit 1 ;;
    esac
    tarball="wasi-sdk-${release}-${suffix}.tar.gz"
    url="https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${version}/${tarball}"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL -o "$tmp/wasi-sdk.tar.gz" "$url"
    rm -rf "$home"
    mkdir -p "$home"
    tar -xzf "$tmp/wasi-sdk.tar.gz" -C "$home" --strip-components=1
    printf '%s\n' "$release" > "$home/.version"

# Check provider and tool WASM crates.
[group('providers')]
providers-check: wasi-sdk
    #!/usr/bin/env bash
    set -euo pipefail
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

# Build provider and tool WASM components, then embed each provider's
# self-reported manifest as the `omnifs.provider-metadata.v1` custom section.
[group('providers')]
providers-build: wasi-sdk
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --target wasm32-wasip2 --release -p 'omnifs-tool-*'
    cargo build --target wasm32-wasip2 --release \
      -p 'omnifs-provider-*' \
      -p test-provider
    # The manifest section is authored from `#[provider]` annotations and
    # assembled here by running each component's `manifest_json()` export (the
    # only place `config_schema` can be evaluated), then injected into the wasm.
    cargo build --release -p omnifs-host --bin omnifs-embed-manifest
    shopt -s nullglob
    wasms=(target/wasm32-wasip2/release/omnifs_provider_*.wasm target/wasm32-wasip2/release/test_provider.wasm)
    if (( ${#wasms[@]} == 0 )); then
      printf 'no provider WASM components found to embed\n' >&2
      exit 1
    fi
    target/release/omnifs-embed-manifest "${wasms[@]}"

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
