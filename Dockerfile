# syntax=docker.io/docker/dockerfile:1.12-labs

FROM rust:1.91.0-bookworm AS toolchain

COPY rust-toolchain.toml .
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang fuse3 libfuse3-dev mold pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-install-target,target=/usr/local/cargo-install-target,sharing=locked \
    CARGO_TARGET_DIR=/usr/local/cargo-install-target cargo install cargo-chef --locked \
    && rustup target add wasm32-wasip2

# wasi-sdk supplies the wasi-sysroot headers + clang that the
# `libsqlite3-sys` build script needs to compile bundled SQLite for
# `wasm32-wasip2`. The Rust toolchain ships precompiled
# `wasi-libc.a` but no headers, so any C dependency reaches for a
# sysroot via `--sysroot=`. We expose the sysroot through
# `WASI_SYSROOT` and target-specific `CC_*` / `CFLAGS_*` env vars so
# `cc-rs` invokes the right clang with the right `--sysroot`.
ARG WASI_SDK_VERSION=33
ARG WASI_SDK_RELEASE=33.0
ENV WASI_SDK_HOME=/opt/wasi-sdk
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        amd64) tarball="wasi-sdk-${WASI_SDK_RELEASE}-x86_64-linux.tar.gz" ;; \
        arm64) tarball="wasi-sdk-${WASI_SDK_RELEASE}-arm64-linux.tar.gz" ;; \
        *) echo "unsupported arch $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/wasi-sdk.tar.gz \
        "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/${tarball}"; \
    mkdir -p "${WASI_SDK_HOME}"; \
    tar -xzf /tmp/wasi-sdk.tar.gz -C "${WASI_SDK_HOME}" --strip-components=1; \
    rm -f /tmp/wasi-sdk.tar.gz
ENV WASI_SYSROOT=${WASI_SDK_HOME}/share/wasi-sysroot \
    CC_wasm32_wasip2=${WASI_SDK_HOME}/bin/clang \
    CFLAGS_wasm32_wasip2="--sysroot=${WASI_SDK_HOME}/share/wasi-sysroot"

# --- Dependency cache (host crates) ---

FROM toolchain AS planner
WORKDIR /src
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM toolchain AS deps
WORKDIR /src
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# --- Build providers ---
#
# Discovers every crate under `providers/` whose package name starts
# with `omnifs-provider-` and builds them in a single cargo invocation.
# Adding a new provider is therefore just `providers/<name>/...` with
# its provider manifest.

FROM toolchain AS providers
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-provider-target,target=/src/target,sharing=locked \
    set -eux; \
    pkgs=$(awk -F'"' '/^name = "omnifs-provider-/ { printf " -p %s", $2 }' providers/*/Cargo.toml); \
    cargo build $pkgs -p test-provider --target wasm32-wasip2 --release --target-dir /src/target; \
    cargo build --release -p 'omnifs-tool-*' --target wasm32-wasip2 --target-dir /src/target; \
    mkdir -p /out/wasm; \
    cp /src/target/wasm32-wasip2/release/*.wasm /out/wasm/

# Provider + tool WASM for host builds and macOS dist (CI artifact omnifs-wasm).
FROM scratch AS wasm-artifacts
COPY --from=providers /out/wasm/*.wasm /

# --- Build extractor and host binary ---

FROM deps AS builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    cargo build --release -p 'omnifs-tool-*' --target wasm32-wasip2 \
    && cargo build --release -p omnifs-cli \
    && cp /src/target/release/omnifs /omnifs

FROM builder AS cli-export
RUN mkdir -p /out && cp /omnifs /out/omnifs

# --- Lint and test (CI targets) ---
#
# `lint` and `test` are CI-only stages that share the cooked deps from
# `deps`. CI invokes them via `docker/build-push-action` with
# `target: lint` / `target: test`; the BuildKit cache mounts on the
# cargo registry and target dir mean repeated runs are incremental
# inside the stage, and the registry-backed buildx cache shares the
# `toolchain` and `deps` layers with the runtime build so nothing
# duplicates work across jobs.

FROM deps AS lint
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    set -eux; \
    cargo fmt --all --check; \
    # The host links pre-built `omnifs-tool-*` wasm components via
    # `include_bytes!`; clippy on the host crate cannot compile until
    # those artifacts exist in the target dir.
    cargo build --release --target wasm32-wasip2 -p 'omnifs-tool-*'; \
    cargo clippy -p omnifs-cli -p omnifs-host -p omnifs-sdk \
        -p omnifs-sdk-macros -p omnifs-mount-schema -- -D warnings; \
    cargo clippy -p 'omnifs-provider-*' -p test-provider \
        -p 'omnifs-tool-*' --target wasm32-wasip2 -- -D warnings

FROM deps AS test
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    set -eux; \
    # The host's runtime tests load `test_provider.wasm` (and other
    # provider components) from the target dir at run time; the same
    # build step also satisfies the `include_bytes!` constraint on
    # `omnifs-tool-*` that the host crate has at compile time.
    cargo build --release --target wasm32-wasip2 \
        -p 'omnifs-provider-*' -p test-provider -p 'omnifs-tool-*'; \
    cargo test --release -p omnifs-cli -p omnifs-host -p omnifs-sdk \
        -p omnifs-sdk-macros -p omnifs-mount-schema; \
    cargo test -p 'omnifs-provider-*' -p test-provider \
        --target wasm32-wasip2 --no-run

# --- Runtime ---

FROM ubuntu:25.10 AS runtime-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash ca-certificates curl fuse3 gnupg jq \
        zsh git openssh-client procps \
        bat git-delta ripgrep util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/apt/keyrings \
    && curl -fsSL https://repo.charm.sh/apt/gpg.key \
        | gpg --dearmor -o /etc/apt/keyrings/charm.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/charm.gpg] https://repo.charm.sh/apt/ * *" \
        > /etc/apt/sources.list.d/charm.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends gum \
    && rm -rf /var/lib/apt/lists/*

COPY scripts/container-zshrc.zsh /etc/zsh/zshrc

COPY scripts/demo.sh /tmp/demo.sh
COPY scripts/container-entrypoint.sh /usr/local/bin/omnifs-container-entrypoint
RUN chmod 0755 /tmp/demo.sh /usr/local/bin/omnifs-container-entrypoint \
    && mkdir -p /root/.omnifs/config/mounts /root/.omnifs/data /root/.omnifs/cache /root/.omnifs/providers /tmp/omnifs-provider-manifests

SHELL ["/bin/zsh", "-c"]
# `omnifs daemon mount` resolves `providers_dir` from `OMNIFS_PROVIDERS_DIR`
# before falling back to `data_dir/providers`. The image bakes WASMs under
# `/root/.omnifs/providers/`, so declare that location for the daemon and for
# `docker exec omnifs omnifs status`.
ENV SHELL=/bin/zsh \
    OMNIFS_PROVIDERS_DIR=/root/.omnifs/providers
WORKDIR /
ENTRYPOINT ["/usr/local/bin/omnifs-container-entrypoint"]

FROM runtime-base AS runtime

# Launcher↔image version handshake. The launcher inspects this label
# before `docker create` and refuses to start the container if it is
# older than the value here — catches the footgun where a
# contributor's `omnifs` on PATH (e.g. an old npm-installed release)
# is used to launch an image built from a newer source tree that
# wires new capabilities (ports, env vars, mounts) the old launcher
# doesn't know about. `omnifs dev` and CI both pass the workspace
# `CARGO_PKG_VERSION` as the build arg.
ARG OMNIFS_MIN_LAUNCHER_VERSION=unknown
LABEL ai.0xff.omnifs.min-launcher-version=${OMNIFS_MIN_LAUNCHER_VERSION}

COPY --from=builder /omnifs /usr/local/bin/
COPY --from=providers /out/wasm/omnifs_provider_*.wasm \
     /root/.omnifs/providers/
COPY --from=providers /out/wasm/omnifs_tool_archive.wasm \
     /root/.omnifs/providers/
RUN chmod 0755 /usr/local/bin/omnifs
