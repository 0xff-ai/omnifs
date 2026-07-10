# syntax=docker.io/docker/dockerfile:1.12-labs

FROM rust:1.95.0-bookworm AS toolchain

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

# --- Build providers for explicit artifact export ---
#
# Discovers every crate under `providers/` whose package name starts
# with `omnifs-provider-` and builds them in a single cargo invocation.
# Adding a new provider is therefore just `providers/<name>/...` with
# its provider manifest. The normal contributor runtime image no longer depends
# on this stage; `scripts/dev.ts` passes the host-built provider-store bundle
# as the `provider-wasm` named build context so the Docker image does not
# compile providers again.

FROM toolchain AS providers
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-provider-target,target=/src/target,sharing=locked \
    set -eux; \
    workspace_pkgs=$(awk -F'"' '/^name = "omnifs-/ || /^name = "test-provider"/ { printf " -p %s", $2 }' crates/*/Cargo.toml providers/*/Cargo.toml); \
    cargo clean $workspace_pkgs --target wasm32-wasip2 --release --target-dir /src/target; \
    rm -f /src/target/wasm32-wasip2/release/*.wasm; \
    pkgs=$(awk -F'"' '/^name = "omnifs-provider-/ { printf " -p %s", $2 }' providers/*/Cargo.toml); \
    cargo build $pkgs -p test-provider --target wasm32-wasip2 --release --target-dir /src/target; \
    mkdir -p /out/wasm; \
    cp /src/target/wasm32-wasip2/release/*.wasm /out/wasm/

# Provider WASM for contributor dev homes and CI-internal host tests.
FROM scratch AS wasm-artifacts
COPY --from=providers /out/wasm/*.wasm /

# --- Build host binary ---

FROM deps AS builder
WORKDIR /src
COPY . .
COPY --from=provider-wasm / /provider-wasm/
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    OMNIFS_PROVIDER_BUNDLE_DIR=/provider-wasm cargo build --release -p omnifs-cli \
    && cp /src/target/release/omnifs /omnifs

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
    # The CLI embeds the built-in provider bundle, so host clippy needs the
    # provider WASM artifacts in the target dir first.
    cargo build --release --target wasm32-wasip2 \
        -p 'omnifs-provider-*'; \
    cargo clippy -p omnifs-cli -p omnifs-daemon -p omnifs-host -p omnifs-sdk \
        -p omnifs-sdk-macros -p omnifs-workspace -- -D warnings; \
    cargo clippy -p 'omnifs-provider-*' -p test-provider \
        --target wasm32-wasip2 -- -D warnings

FROM deps AS test
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    set -eux; \
    # The host's runtime tests load `test_provider.wasm` from the target
    # dir, and the CLI test build embeds the built-in provider bundle
    # from those same wasm artifacts.
    cargo build --release --target wasm32-wasip2 \
        -p 'omnifs-provider-*' -p test-provider; \
    cargo test --release -p omnifs-cli -p omnifs-daemon -p omnifs-host -p omnifs-sdk \
        -p omnifs-sdk-macros -p omnifs-workspace; \
    cargo test -p 'omnifs-provider-*' -p test-provider \
        --target wasm32-wasip2 --no-run

# --- Runtime ---

# --- Runtime base ---
#
# The single runtime setup for both images. `runtime-dev` (contributor, built
# by `just dev`) copies the binary compiled in this Dockerfile; `runtime-release`
# (built by `scripts/ci/build-runtime-image.sh`) injects a prebuilt binary as a
# named build context. Because both descend from `runtime-base`, the apt/setup
# block below has one owner — targeting `runtime-release` never builds the
# compile toolchain, so no base image needs publishing.

FROM ubuntu:25.10 AS runtime-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash ca-certificates curl fuse3 gnupg jq \
        zsh git openssh-client procps nfs-common netbase \
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
# The container owns its guest paths. Declaring them as image ENV means the
# entrypoint, the daemon (which resolves OMNIFS_HOME / OMNIFS_MOUNT_POINT from
# the environment), interactive `docker exec` shells, and the welcome banner all
# read one value, with no in-image file to source.
ENV SHELL=/bin/zsh \
    OMNIFS_HOME=/root/.omnifs \
    OMNIFS_MOUNT_POINT=/omnifs
RUN chmod 0755 /tmp/demo.sh /usr/local/bin/omnifs-container-entrypoint \
    && mkdir -p "$OMNIFS_HOME/cache" /tmp/omnifs-provider-manifests

SHELL ["/bin/zsh", "-c"]
WORKDIR /
ENTRYPOINT ["/usr/local/bin/omnifs-container-entrypoint"]

# Launcher↔image version handshake. The launcher inspects these labels
# before `docker create` and refuses to start the container if it is
# older than the value here — catches the footgun where a
# contributor's `omnifs` on PATH (e.g. an old npm-installed release)
# is used to launch an image built from a newer source tree that
# wires new capabilities (ports, env vars, mounts) the old launcher
# doesn't know about. `scripts/dev.ts`, `scripts/ci/build-runtime-image.sh`,
# and CI all pass the workspace version as the build arg. Set on `runtime-base`
# so both final stages inherit the labels.
#
# OMNIFS_LAUNCH_PROTOCOL is set to `daemon-control-v<API_MAJOR>` and must
# match the `EXPECTED_LAUNCH_PROTOCOL` constant in `crates/omnifs-cli/src/runtime.rs`.
# Both are derived from the same API major version; when API_MAJOR bumps, update
# this arg default and the constant together.
ARG OMNIFS_MIN_LAUNCHER_VERSION=unknown
ARG OMNIFS_LAUNCH_PROTOCOL=daemon-control-v3
LABEL ai.0xff.omnifs.min-launcher-version=${OMNIFS_MIN_LAUNCHER_VERSION}
LABEL ai.0xff.omnifs.launch-protocol=${OMNIFS_LAUNCH_PROTOCOL}

# Contributor image: the binary compiled in this Dockerfile's `builder` stage.
FROM runtime-base AS runtime-dev
COPY --from=builder /omnifs /usr/local/bin/
RUN chmod 0755 /usr/local/bin/omnifs

# Release image: a prebuilt binary injected as the `omnifs-bin` build context by
# `scripts/ci/build-runtime-image.sh`, so no compile toolchain is built.
FROM runtime-base AS runtime-release
COPY --from=omnifs-bin omnifs /usr/local/bin/omnifs

# --- Docker-hosted FUSE frontend ---
#
# `omnifs frontend up` (see `crates/omnifs-cli/src/frontend_container.rs`,
# `crates/omnifs-daemon/src/frontend.rs`) launches a separate, credential-free
# container that only ever runs `omnifs frontend run --kind fuse`, attached over
# TCP to a host-native daemon's shared namespace. It never runs a provider, so
# it gets its own minimal base rather than extending `runtime-base`: no
# `OMNIFS_HOME`, no provider store, no control API, none of the runtime image's
# interactive-shell toolbox (zsh, gum, git, ripgrep, nfs-common...).
#
# Debian, not the Ubuntu `runtime-base` family: this is the same Debian family
# the compile `toolchain` stage above already uses, and Debian's default
# coreutils/findutils are GNU (uutils is opt-in, not the default `tail`), which
# is what the frontend conformance matrix's `tail -f` case requires.
FROM debian:trixie-slim AS frontend-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        coreutils findutils fuse3 jq rsync tar xxd \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir /omnifs

ENTRYPOINT ["/usr/local/bin/omnifs", "frontend", "run", "--kind", "fuse", "--mount-point", "/omnifs"]

# Contributor image: the binary compiled in this Dockerfile's `builder` stage,
# same source as `runtime-dev`. `just frontend-image` builds this target.
FROM frontend-base AS frontend-dev
COPY --from=builder /omnifs /usr/local/bin/
RUN chmod 0755 /usr/local/bin/omnifs

# Release image: a prebuilt binary injected as the `omnifs-bin` build context,
# same mechanism as `runtime-release`. `scripts/ci/build-frontend-image.sh`
# builds this target.
FROM frontend-base AS frontend-release
COPY --from=omnifs-bin omnifs /usr/local/bin/omnifs
