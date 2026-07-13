# syntax=docker.io/docker/dockerfile:1.12-labs

FROM rust:1.95.0-bookworm AS toolchain

COPY rust-toolchain.toml .
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libfuse3-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-install-target,target=/usr/local/cargo-install-target,sharing=locked \
    CARGO_TARGET_DIR=/usr/local/cargo-install-target cargo install cargo-chef --locked

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

# --- Build the slim omnifs-thin frontend runner ---
#
# `omnifs-thin` is the dedicated credential-free frontend runner: it attaches
# a wire-backed namespace and serves FUSE or NFS, and needs no
# engine, no Wasmtime, and no provider bundle. This stage builds it alone, so
# the frontend images below need no provider artifacts.

FROM deps AS thin-builder
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=omnifs-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=omnifs-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=omnifs-host-target,target=/src/target,sharing=locked \
    cargo build --release -p omnifs-thin --bin omnifs-thin \
    && cp /src/target/release/omnifs-thin /omnifs-thin

# --- Docker-hosted FUSE frontend ---
#
# `omnifs frontend up` (see `crates/omnifs-cli/src/frontend_container.rs`)
# launches a separate, credential-free container that only ever runs the slim
# `omnifs-thin fuse` binary, attached over TCP to a host-native daemon's shared
# namespace. It never runs a provider, so it gets its own minimal base: no
# `OMNIFS_HOME`, no provider store, no control API, none of an
# interactive-shell toolbox (zsh, gum, git, ripgrep, nfs-common...) — and,
# no provider-store build context at all.
#
# Debian, not Ubuntu: this is the same Debian family the compile `toolchain`
# stage above already uses, and Debian's default coreutils/findutils are GNU
# (uutils is opt-in, not the default `tail`), which is what the frontend
# conformance matrix's `tail -f` case requires.
FROM debian:trixie-slim AS frontend-base

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        coreutils findutils fuse3 jq rsync tar xxd \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir /omnifs

ENTRYPOINT ["/usr/local/bin/omnifs-thin", "fuse", "--mount-point", "/omnifs"]

# Contributor image: the binary compiled in this Dockerfile's `thin-builder`
# stage. `just frontend-image` builds this target.
FROM frontend-base AS frontend-dev
COPY --from=thin-builder /omnifs-thin /usr/local/bin/

# Release image: a prebuilt binary injected as the `omnifs-thin-bin` build
# context. `scripts/ci/build-frontend-image.sh` builds this target.
FROM frontend-base AS frontend-release
COPY --from=omnifs-thin-bin omnifs-thin /usr/local/bin/omnifs-thin
