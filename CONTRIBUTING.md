# Contributing

Reference for the contributor workflow, build and validation commands, runtime debugging, and code conventions for working in `omnifs`. The repository-local architecture and design guidance lives in `AGENTS.md`; this file is the operational companion.

## Getting started

The primary contributor workflow is `just dev`, which runs `scripts/dev.ts`. The script checks prerequisites, builds provider WASM into a content-addressed provider-store bundle, builds the omnifs CLI natively, starts a host-native daemon (`omnifs up --no-frontend`) with dev mounts and credentials rendered into `~/.omnifs-dev`, launches the Docker FUSE frontend (via driver), and opens a shell inside that frontend container at `/omnifs`.

```bash
just dev            # build providers and the CLI, start the native daemon, attach the frontend, open /omnifs
omnifs status                                  # host-native: runs directly, no docker exec needed
FRONTEND=$(docker ps --filter label=ai.0xff.omnifs.home="$HOME/.omnifs-dev" --format '{{.Names}}')
docker exec -it -w /omnifs "$FRONTEND" /bin/sh # reattach to the browsing shell
omnifs frontend down && omnifs down            # tear both down
```

`scripts/dev.ts` is contributor-only and requires a source checkout. It resolves a dedicated dev home at `~/.omnifs-dev`, copies the provider-store bundle into `~/.omnifs-dev/providers`, writes pinned mount specs under `~/.omnifs-dev/mounts`, interpolates host tokens into the `contrib/dev-credentials.json` template to produce `~/.omnifs-dev/credentials.json`, and pins `[system].frontend_image` in `~/.omnifs-dev/config.toml` at an image tagged `omnifs-frontend:<short-sha>-dev`. The daemon itself is host-native, not containerized: no home bind mount, no daemon container. Dev auth uses host tokens: set `GITHUB_TOKEN` or `LINEAR_API_KEY`, or allow the script to read `gh auth token` for GitHub when prompted. Authenticated mounts without a token are skipped rather than started broken.

A locally built `omnifs` binary targets the `omnifs-frontend:dev` image by default and never pulls; produce that image with `just dev --build-only`, which builds providers, the CLI, and the frontend image, tags it `omnifs-frontend:dev`, and exits without starting a session.

Do not add alternate local mount recipes unless explicitly requested.

## Build and validate

Package selection uses cargo `-p` / `--exclude` globs (`omnifs-provider-*`, `test-provider`), not a hand-maintained crate list.

Host crates such as `omnifs-cli` and `omnifs-engine` build for the native target. Providers build directly as components with the Rust `wasm32-wasip2` target. `cargo build --target wasm32-wasip2` emits provider component artifacts directly; WIT bindings are generated through the SDK.

Provider clippy and test commands must include `--target wasm32-wasip2` and `-p` globs:

```bash
cargo clippy -p 'omnifs-provider-*' -p test-provider --target wasm32-wasip2 -- -D warnings
cargo test -p 'omnifs-provider-*' -p test-provider --target wasm32-wasip2 --no-run
```

WASM tests compile but cannot execute on the host because there is no WASM runtime in the test harness. Use `--no-run` for target-specific provider compilation checks. Tests that need to run should use `#[cfg(test)]` with host-target-compatible code.

### CI and release builds

CI builds Rust artifacts natively and uses Docker only to assemble the optional frontend image. Linux CLI artifacts use `cargo-zigbuild` with a GNU glibc 2.17 baseline; Darwin CLI artifacts are cross-linked from Linux through the pinned `rust-cross/cargo-zigbuild` container. Provider and tool WASM artifacts are built by `just build providers`; its WASI SDK pin in `justfile` is shared by local and CI builds.

`Dockerfile`'s `frontend-dev` stage is the contributor image path for `just dev` (built by `scripts/dev.ts`, same as `just frontend-image`). It runs the slim `omnifs-fuse` binary (built by the `fuse-builder` stage), which needs no engine, no Wasmtime, and no provider bundle, so unlike the full `omnifs` CLI/daemon binary (`OMNIFS_PROVIDER_BUNDLE_DIR` embeds `just build providers`'s `target/omnifs-provider-store` output into that binary at compile time), the frontend image needs no provider-wasm build context at all. Release frontend image assembly uses `scripts/ci/build-frontend-image.sh`, which stages a prebuilt Linux `omnifs-fuse` binary rather than compiling in Docker. Release CLI binaries embed the compressed provider bundle and unpack it into the host `OMNIFS_HOME/providers`; do not make the image the owner of `/root/.omnifs/providers`. Keep `just dev` working when changing Docker-related files.

CI orchestration shells live in `scripts/ci/`, with `scripts/ci/common.sh` factoring out repo-root discovery. Npm version sync and OpenAPI generation are just recipes. The release flow is git-cliff plus the `release-pr.yml` coordinator (see `RELEASING.md`); the repo carries no Bun.

## Validate through the live runtime

For mount, provider, clone, traversal, or runtime behavior changes, do not stop at Rust-only checks. Validate through the supported runtime path:

```bash
just dev -y
omnifs status
FRONTEND=$(docker ps --filter label=ai.0xff.omnifs.home="$HOME/.omnifs-dev" --format '{{.Names}}')
docker exec "$FRONTEND" sh -c 'ls /omnifs/github/0xff-ai/omnifs/issues/open'
tail -n 80 ~/.omnifs-dev/cache/daemon.log
```

`scripts/ci/smoke-native-frontend.sh` is the standalone equivalent CI's `smoke-amd64`/`smoke-arm64` jobs run: it starts its own throwaway dev home (not `~/.omnifs-dev`), reads real GitHub data through both the host mount path and a `docker exec` into the frontend container, and tears everything down again. Run it directly with `FRONTEND_IMAGE=omnifs-frontend:dev GITHUB_TOKEN=$(gh auth token) scripts/ci/smoke-native-frontend.sh` instead of the block above when you want a scripted pass/fail rather than a session to poke at.

For provider path-surface changes, test the whole shell traversal, not only the intended leaf paths. In the live container, run `ll`, `cd`, and `find` from the provider root through every intermediate directory. Verify that parent directories do not synthesize duplicate root entries, route scaffolding names do not bind as dynamic captures, and control directories do not contain paper/item nodes unless the design explicitly says they should.

## Runtime debugging

- The daemon is host-native: its log is `~/.omnifs-dev/cache/daemon.log` on the host, not inside any container.
- `docker logs "$FRONTEND"` shows the frontend container's own stdout/stderr; it only ever runs `omnifs-fuse`, so a mount-serving failure is almost always in the daemon log instead.
- Clone failures should surface in the daemon log with `git clone` stderr.
- FUSE `access(...)` warnings are expected noise unless they correlate with a real failure.
- Use `omnifs status` directly (host-native, no `docker exec` needed) for fast mount/config/provider/cache triage.
- The frontend container is credential-free and carries no `OMNIFS_HOME`: `docker exec` into it only ever sees `/omnifs`, never host paths or credentials.

When a repo path returns `Input/output error`, check:

1. `tail -n 80 ~/.omnifs-dev/cache/daemon.log`
2. SSH auth on the host (the daemon runs host-native, not in a container)
3. whether the mount is still present (`mount | grep omnifs` on Linux; the daemon's own runtime record on macOS)

When debugging hangs or slow paths, start with user-visible probes before theory:

1. `cd /github/<owner>`
2. `cat /dns/@google/<domain>/MX`
3. `tail -n 80 ~/.omnifs-dev/cache/daemon.log`

The frontend image is a minimal `debian:trixie-slim` (GNU coreutils/findutils, fuse3, jq, rsync, tar, xxd; `Dockerfile`'s `frontend-base`), deliberately without zsh, gum, or any shell rc customization: it only ever runs `omnifs-fuse`, and an interactive session there is `/bin/sh`. Do not add interactive-shell tooling to it; that belongs to the host shell the contributor already has.

## Code conventions

### Rust type conversions

Prefer `From` and `TryFrom` at type boundaries instead of `foo_to_bar` free functions when the conversion is a true one-to-one mapping.

Keep free functions when:

- orphan rules block a cross-crate impl, for example `credential_entry_from_token` from an oauth2 token to `omnifs_workspace::creds::CredentialEntry`
- extra context is required, for example `io_context_into(context, error)` or `push_projected_file_content(records, path, file)`
- the helper is callout-specific extraction for `CalloutFuture`, meaning `fn(CalloutResult) -> Result<T>`. Do not use `TryFrom<CalloutResult>` for single-variant unwraps.
- the mapping is conditional or formatting-only, for example HTTP `status_error` with 429 and `retry-after` handling

When orphan rules block `From<A> for B`, use a local newtype in the owning crate, for example `WitHeaders(&HeaderMap)` in `omnifs-sdk/src/http.rs`, rather than a conversion helper that hides the same logic.

In `omnifs-sdk`, `Result<T>` aliases `core::result::Result<T, ProviderError>`. `TryFrom` impls must return `std::result::Result<_, ProviderError>` explicitly.

Host-only error enums may be supersets of WIT/guest types. For example, host `ExtractError` adds `SandboxTrapped` and setup failures. Do not re-export guest bindgen types as the host public error; map with `From` at the boundary.

Existing conversion hubs: `crates/omnifs-engine/src/callouts/wit_convert.rs`, `crates/omnifs-sdk/src/file_attrs.rs`, `crates/omnifs-sdk/src/browse.rs`, and `crates/omnifs-engine/src/callouts/{blob,git,archive}.rs`.
