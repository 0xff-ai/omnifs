# Contributing

Reference for the contributor workflow, build and validation commands, runtime debugging, and code conventions for working in `omnifs`. The repository-local architecture and design guidance lives in `AGENTS.md`; this file is the operational companion.

## Getting started

The primary contributor workflow is `omnifs dev`, implemented in `crates/omnifs-cli/src/commands/dev.rs`. It builds the dev image, synthesizes mount configs from built-in provider manifests, validates stored credentials, materializes fixtures, and launches the container directly through the Docker API.

```bash
omnifs dev          # build dev image, materialize fixtures, launch container
omnifs shell        # omnifs-aware shell for exploring the tree
omnifs logs -f      # follow container output
omnifs status       # inspect mounts, providers, auth state
omnifs down         # stop and remove the container
```

`omnifs dev` is contributor-only and requires a source checkout. It walks up from cwd looking for the workspace `Cargo.toml`, resolves a dedicated dev home at `~/.omnifs-dev`, provisions each built-in dev mount's credential from your host (`gh auth token`, or a provider env var such as `LINEAR_API_KEY`) into `~/.omnifs-dev/credentials.json`, downloads the Chinook SQLite fixture into `~/.omnifs-dev/db/test.db`, builds an image tagged `omnifs:<short-sha>-dev`, and starts the container with the mounts it could provision under `/omnifs/<mount>`. Nothing is written into the checkout; a mount whose credential is missing is skipped with a warning.

When run from inside the source checkout, the whole contributor command family (`omnifs shell`, `status`, `logs`, `down`) defaults to the same `~/.omnifs-dev` home, so a session started by `omnifs dev` is visible to them with no `OMNIFS_HOME` prefix. An explicit `OMNIFS_HOME` always overrides this; outside a checkout the normal `~/.omnifs` applies.

Do not add alternate local mount recipes unless explicitly requested.

## Build and validate

Package selection uses cargo `-p` / `--exclude` globs (`omnifs-provider-*`, `omnifs-tool-*`, `test-provider`), not a hand-maintained crate list.

Host crates such as `omnifs-cli` and `omnifs-host` build for the native target. Providers build directly as components with the Rust `wasm32-wasip2` target. `cargo build --target wasm32-wasip2` emits provider component artifacts directly; WIT bindings are generated through the SDK.

Provider clippy and test commands must include `--target wasm32-wasip2` and `-p` globs:

```bash
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider --target wasm32-wasip2 -- -D warnings
cargo test -p 'omnifs-provider-*' -p test-provider --target wasm32-wasip2 --no-run
```

WASM tests compile but cannot execute on the host because there is no WASM runtime in the test harness. Use `--no-run` for target-specific provider compilation checks. Tests that need to run should use `#[cfg(test)]` with host-target-compatible code.

### CI and release builds

CI builds Rust artifacts natively and uses Docker only to assemble the runtime image. Linux CLI artifacts use `cargo-zigbuild` with a GNU glibc 2.17 baseline; Darwin CLI artifacts are cross-linked from Linux through the pinned `rust-cross/cargo-zigbuild` container. Provider and tool WASM artifacts are built by `just providers build` with WASI SDK pins from `tools/versions.toml`.

`Dockerfile` remains the contributor image path for `omnifs dev`. Release runtime image assembly uses `scripts/ci/build-runtime-image.sh`, which stages the prebuilt Linux CLI binary into a small Ubuntu runtime context. Release CLI binaries embed the compressed provider/tool WASM bundle and unpack it into the host `OMNIFS_HOME/providers`; do not make the runtime image the owner of `/root/.omnifs/providers`. Keep `omnifs dev` working when changing Docker-related files.

CI orchestration shells live in `scripts/ci/`, with `scripts/ci/common.sh` factoring out repo-root discovery and `version_pin()` (a thin wrapper over `scripts/toolchain/versions.ts`). Toolchain bootstrap (wasi-sdk, version pin lookup) is at `scripts/toolchain/`. The Bun script tree under `scripts/` is layered: thin bins at `scripts/{npm,release}.ts` parse CLI args and dispatch to `scripts/lib/`.

## Validate through the live runtime

For mount, provider, clone, traversal, or runtime behavior changes, do not stop at Rust-only checks. Validate through the supported runtime path:

```bash
omnifs dev -y
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -n 80 /tmp/omnifs.log'
```

For provider path-surface changes, test the whole shell traversal, not only the intended leaf paths. In the live container, run `ll`, `cd`, and `find` from the provider root through every intermediate directory. Verify that parent directories do not synthesize duplicate root entries, route scaffolding names do not bind as dynamic captures, and control directories do not contain paper/item nodes unless the design explicitly says they should.

## Runtime debugging

- Runtime log file is `/tmp/omnifs.log` inside the container.
- `omnifs logs` or `docker logs omnifs` shows stdout/stderr from the entrypoint. Runtime FUSE traces still go to `/tmp/omnifs.log` inside the container.
- Clone failures should surface in the runtime log with `git clone` stderr.
- FUSE `access(...)` warnings are expected noise unless they correlate with a real failure.
- Use `omnifs status` inside the container for fast mount/config/provider/cache triage.
- Do not assume `docker exec` inherits the entrypoint environment. Verify live runtime paths instead of inferring them from defaults.

When a repo path returns `Input/output error`, check:

1. `omnifs logs` or `docker logs omnifs`
2. SSH auth inside the container
3. whether the mount is still present in `/proc/mounts`

When debugging hangs or slow paths, start with user-visible probes before theory:

1. `cd /github/<owner>`
2. `cat /dns/@google/<domain>/MX`
3. `tail -n 80 /tmp/omnifs.log`

The runtime image uses Ubuntu 25.10 and `zsh`. Interactive shells should have `ls` aliased to `ls --color=auto` and `ll` aliased to `ls -lrt`. If changing shell behavior, prefer putting it in the image rather than generating per-session shell config.

## Code conventions

### Rust type conversions

Prefer `From` and `TryFrom` at type boundaries instead of `foo_to_bar` free functions when the conversion is a true one-to-one mapping.

Keep free functions when:

- orphan rules block a cross-crate impl, for example `credential_entry_from_token` from oauth2 token to `omnifs_creds::CredentialEntry`
- extra context is required, for example `io_context_into(context, err)` or `projected_file_from_projection(..., parent, name)`
- the helper is callout-specific extraction for `CalloutFuture`, meaning `fn(CalloutResult) -> Result<T>`. Do not use `TryFrom<CalloutResult>` for single-variant unwraps.
- the mapping is conditional or formatting-only, for example HTTP `status_error` with 429 and `retry-after` handling

When orphan rules block `From<A> for B`, use a local newtype in the owning crate, for example `WitHeaders(&HeaderMap)` in `omnifs-sdk/src/http.rs`, rather than a conversion helper that hides the same logic.

In `omnifs-sdk`, `Result<T>` aliases `core::result::Result<T, ProviderError>`. `TryFrom` impls must return `std::result::Result<_, ProviderError>` explicitly.

Host-only error enums may be supersets of WIT/guest types. For example, host `ExtractError` adds `SandboxTrapped` and setup failures. Do not re-export guest bindgen types as the host public error; map with `From` at the boundary.

Existing conversion hubs: `omnifs-host/src/wit_protocol.rs`, `omnifs-sdk/file_attrs.rs`, `omnifs-sdk/browse.rs`, `omnifs-host/src/{blob,git,archive}.rs`.
