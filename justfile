set shell := ["bash", "-euo", "pipefail", "-c"]

wasi_sdk_version := "33"
wasi_sdk_release := "33.0"
wasi_sdk_home := ".cache/wasi-sdk"

# Show the maintainer command menu.
[default]
default:
    @just --justfile '{{ justfile() }}' --list --unsorted

[private]
_install-wasi-sdk action scope:
    #!/usr/bin/env bash
    set -euo pipefail

    case "{{ action }}:{{ scope }}" in
      build:host) exit 0 ;;
      check:host)
        wasm_dir="target/wasm32-wasip2/release"
        if [[ -n "$(find "$wasm_dir" -maxdepth 1 -name 'omnifs_provider_*.wasm' -print -quit 2>/dev/null)" ]]; then
          exit 0
        fi
        ;;
      build:|build:providers|check:|check:providers) ;;
      *) exit 0 ;;
    esac

    version="{{ wasi_sdk_version }}"
    release="{{ wasi_sdk_release }}"
    home="{{ wasi_sdk_home }}"
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

# Build the whole codebase, or one build scope.
[group('dev')]
build scope='': (_install-wasi-sdk "build" scope)
    #!/usr/bin/env bash
    set -euo pipefail

    build_providers() {
      bash scripts/ci/build-providers.sh
    }

    build_host() {
      cargo build --workspace \
        --exclude 'omnifs-provider-*' \
        --exclude test-provider \
        --exclude omnifs-embed-metadata
    }

    case "{{ scope }}" in
      '') build_providers; build_host ;;
      providers) build_providers ;;
      host) build_host ;;
      *) printf 'unknown build scope: %s (expected providers or host)\n' "{{ scope }}" >&2; exit 1 ;;
    esac

# Check the whole codebase, or one validation scope.
[group('dev')]
check scope='': (_install-wasi-sdk "check" scope)
    #!/usr/bin/env bash
    set -euo pipefail

    check_providers() {
      cargo check --target wasm32-wasip2 \
        -p 'omnifs-provider-*' \
        -p test-provider
      cargo clippy --target wasm32-wasip2 \
        -p 'omnifs-provider-*' \
        -p test-provider \
        -- -D warnings
      cargo test --target wasm32-wasip2 \
        -p 'omnifs-provider-*' \
        -p test-provider \
        --no-run
    }

    check_host() {
      wasm_dir="target/wasm32-wasip2/release"
      if [[ -z "$(find "$wasm_dir" -maxdepth 1 -name 'omnifs_provider_*.wasm' -print -quit 2>/dev/null)" ]]; then
        bash scripts/ci/build-providers.sh
      fi
      cargo clippy --workspace \
        --exclude 'omnifs-provider-*' \
        --exclude test-provider \
        --exclude omnifs-embed-metadata \
        --all-targets \
        -- -D warnings
    }

    case "{{ scope }}" in
      '') check_providers; check_host ;;
      providers) check_providers ;;
      host) check_host ;;
      *) printf 'unknown check scope: %s (expected providers or host)\n' "{{ scope }}" >&2; exit 1 ;;
    esac

# Validate built artifacts for one scope.
[group('dev')]
validate scope='providers':
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "{{ scope }}" != providers ]]; then
      printf 'unknown validation scope: %s (expected providers)\n' "{{ scope }}" >&2
      exit 1
    fi
    shopt -s nullglob
    wasms=(target/wasm32-wasip2/release/*.wasm)
    if (( ${#wasms[@]} == 0 )); then
      printf 'no WASM components found in target/wasm32-wasip2/release\n' >&2
      exit 1
    fi
    for wasm in "${wasms[@]}"; do
      wasm-tools validate --features cm-async,cm-async-stackful,cm-async-builtins "$wasm"
    done

# Run tests for one test scope.
[group('dev')]
test scope='host':
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "{{ scope }}" != host ]]; then
      printf 'unknown test scope: %s (expected host)\n' "{{ scope }}" >&2
      exit 1
    fi
    OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run --profile ci --workspace \
      --exclude 'omnifs-provider-*' \
      --exclude test-provider \
      --exclude omnifs-embed-metadata

import 'just/dev.just'
mod npm 'just/npm.just'
