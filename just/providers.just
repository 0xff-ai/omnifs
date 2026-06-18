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

# Build provider and tool WASM components.
[group('providers')]
providers-build: wasi-sdk
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --target wasm32-wasip2 --release -p 'omnifs-tool-*'
    cargo build --target wasm32-wasip2 --release \
      -p 'omnifs-provider-*' \
      -p test-provider
    just providers-introspect

# Introspect each built provider's route table into providers/<name>/omnifs.routes.json.
#
# Instantiates each component through the host runtime (the omnifs-introspect-routes
# binary), calls `initialize`, and writes the route table. Requires the wasm components
# to be built first (providers-build runs this automatically). The provider WASM filename
# comes from the `provider` field of each omnifs.provider.json.
[group('providers')]
providers-introspect:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -p omnifs-host --bin omnifs-introspect-routes
    bin="target/release/omnifs-introspect-routes"
    wasm_dir="target/wasm32-wasip2/release"
    shopt -s nullglob
    for manifest in providers/*/omnifs.provider.json; do
      dir="$(dirname "$manifest")"
      wasm_name="$(sed -nE 's/.*"provider"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$manifest")"
      if [[ -z "$wasm_name" ]]; then
        printf 'no provider wasm filename in %s\n' "$manifest" >&2
        exit 1
      fi
      wasm="$wasm_dir/$wasm_name"
      if [[ ! -f "$wasm" ]]; then
        printf 'missing built component %s (run just providers-build)\n' "$wasm" >&2
        exit 1
      fi
      "$bin" "$wasm" "$dir/omnifs.routes.json"
    done

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
