# Build and validation contracts

Status: current-contract
Owns: local and CI gates, provider build artifacts, generated OpenAPI/schema files, live runtime validation, and documentation checks.

## Read when

Read this before touching CI, `just` recipes, provider artifact generation, wasi-sdk setup, OpenAPI/schema generation, docs checks, runtime smoke paths, or validation guidance.

## Rules

### Provider build artifacts

Provider WASM artifacts are built with the pinned wasi-sdk. `just providers build` compiles providers, then runs the native `omnifs-embed-metadata` harvester, which converts each provider's `Provider::METADATA` const into the host `ProviderManifest` and injects it as the `omnifs.provider-metadata.v1` custom section, and emits `target/omnifs-provider-store` with content-addressed WASM files plus `index.json`. `just dev` runs `scripts/dev.ts`, so dev mount pinning and the dev image both consume the same provider-store bundle.

Install the pinned wasi-sdk with `just providers wasi-sdk` when needed. Run `just providers build` before host tests that need generated provider artifacts. Use `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1` after prebuilding providers for nextest runs that would otherwise contend (`just host test` sets it for you).

Provider runtime changes must validate both binding surfaces separately: `omnifs-wit` host bindings with `--features host-bindings`, and SDK/provider guest bindings without that feature. Do not combine those into one Cargo invocation that enables host bindings while compiling the SDK.

Provider component validation must enable the component-model async validation features used by provider exports.

Provider behavior is verified host-driven through the engine as scenario replay tests. Provider-specific scenarios live beside each provider under `providers/<p>/tests/`, not as guest-side `#[cfg(test)]` unit modules. Provider crates must stay clippy-clean under host `--all-targets`, and their scenario replay tests run as part of the host test lane.

### Generated OpenAPI and schemas

OpenAPI is generated from daemon implementation, and provider manifest schema is generated from provider model types. Keep generated artifacts synchronized with code.

Run `just openapi` after daemon API changes. Run `just schema` after provider manifest schema changes. Keep generated files checked in when their source model changes.

### Live runtime validation

Mount, provider, clone, traversal, frontend, or runtime behavior changes need live runtime validation. Rust checks alone are not enough.

Use `just dev -y` for the supported contributor runtime path. Check status with `omnifs status` directly (host-native, no `docker exec` needed). Exercise shell traversal and real file tools for path-surface changes.

### CI gates

Use the repo gates instead of ad hoc workspace commands. Host-target gates exclude provider/test-provider WASM crates; WASM crates use provider-specific gates.

Run the relevant CI-shaped lanes before a push or PR handoff. Use `just fmt-check` and `just just-check` for preflight parity. Use `just host clippy` and `just host test` for host-target iteration. Use `just providers check`, `just providers build`, and `just providers validate` for WASM iteration.

### Cross-language facts on the container boundary

The daemon always runs host-native, so `OMNIFS_HOME` and `OMNIFS_MOUNT_POINT` resolve directly from the host environment on every platform (name consts `OMNIFS_HOME_ENV` / `OMNIFS_MOUNT_POINT_ENV` in `omnifs-home`), and the layout under the home has one owner (`omnifs-home::under_root`). The only remaining guest-container path is the optional Docker-hosted FUSE frontend's fixed mount point, `/omnifs`: it is not env-var-driven (the frontend container is credential-free and gets no `OMNIFS_HOME`), so the literal is hardcoded at its owners instead — the frontend image's `ENTRYPOINT` (`Dockerfile`) and each host launcher that targets it (`crates/omnifs-cli/src/launch_backend.rs`'s `GUEST_MOUNT` for production, `scripts/dev.ts`'s `GUEST_MOUNT` for dev). The value is frozen; a change breaks `just dev` and the integration tests loudly.

### Frontend image artifact

The Docker-hosted FUSE frontend (`omnifs frontend up`) ships a minimal image from `Dockerfile`: `frontend-base` (`debian:trixie-slim`, chosen because Debian's default coreutils/findutils are GNU, which `tail -f` fidelity requires), `frontend-dev` (contributor, built by `just frontend-image`, copies the binary from the shared `builder` stage), and `frontend-release` (built by `scripts/ci/build-frontend-image.sh`, injects a prebuilt Linux binary as the `omnifs-bin` build context). The frontend image carries no launch-protocol/min-launcher-version label: `launch_frontend_container` (`crates/omnifs-cli/src/frontend_container.rs`) never checks one.

CI builds and pushes the frontend image per architecture in the PR lane (`frontend-amd64`/`frontend-arm64`), smokes it directly with `scripts/ci/smoke-frontend-image.sh` (version, GNU `tail`, fails loudly with no `OMNIFS_ATTACH_ADDR`), and on a `main` push merges the per-arch digests into one multi-platform manifest via `scripts/ci/publish-manifest.sh`. Release promotes that manifest to `ghcr.io/0xff-ai/omnifs-frontend:<version>` through `scripts/ci/promote-image.sh`. The `fuse-docker` job (needs `frontend-amd64`'s image digest and the packaged Linux CLI, mirroring `conformance-fuse`'s input shape) runs `crates/omnifs-itest/tests/frontend_docker` against a live host-native daemon and the real amd64 image: the `fuse-docker` conformance column, `omnifs frontend {up,down,status}` lifecycle, `omnifs down` teardown ordering, a cold-start budget, cross-mount byte identity, kill/reattach behavior, and the no-credentials contract. Its scorecards upload as `conformance-scorecards-fuse-docker`, next to `conformance-fuse`'s own artifact.

### Documentation checks

`just docs-check` verifies doc-to-doc links and the contract file template. It does not validate code symbols or code paths. It is a local convenience recipe only; CI does not run it, so it never blocks a merge.

## Must not

- Treat missing provider WASM in a fresh worktree as a product regression.
- Use `cargo check --workspace --all-targets` as a host gate.
- Treat host-target provider checks as proof the metadata section was injected; only `just providers build` runs the harvester that injects it.
- Hand-edit generated OpenAPI or schema files as the primary fix.
- Change API/model code without regenerating the corresponding checked-in artifact and running its focused parity test.
- Validate only the intended leaf path when parent traversal changed.
- Treat compile-time route validity as enough for seal-time behavior.
- Ignore runtime logs when the mount returns `Input/output error`.
- Treat a local aggregate command as the source of truth when CI runs the lanes directly.
- Run host tests that rebuild providers in parallel without prebuilding providers when contention matters.
- Treat `just docs-check` as code-symbol validation.
- Reintroduce a second copy of the frontend apt block; edit `frontend-base` instead.
- Add a fourth literal for the frontend's fixed `/omnifs` guest mount point instead of updating its three existing owners together.
- Give the frontend image an `OMNIFS_HOME` or a provider store. It only ever runs `omnifs frontend run`.

## Code

- `just/dev.just`
- `just/host.just`
- `just/providers.just`
- `just/npm.just`
- `npm/package.json`
- `scripts/ci/check-doc-links.sh`
- `scripts/ci/check-doc-contracts.sh`
- `crates/omnifs-daemon/src/bin/openapi.rs`
- `crates/omnifs-api/openapi/daemon.json`
- `crates/omnifs-workspace/schema/omnifs.provider.schema.json`
- `crates/omnifs-itest/src/lib.rs`
- `providers/<p>/tests/`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `Dockerfile`
- `scripts/ci/common.sh`
- `scripts/ci/build-frontend-image.sh`
- `scripts/ci/smoke-frontend-image.sh`
- `scripts/ci/publish-manifest.sh`
- `scripts/ci/promote-image.sh`
- `CONTRIBUTING.md`

## Validation

- `just providers wasi-sdk`
- `just providers build`
- `just providers check`
- `just providers validate`
- `just host clippy`
- `just host test`
- `just refresh`
- `just schema`
- `just openapi`
- `just docs-check`

Live runtime path (the daemon runs host-native; only the frontend needs `docker exec`):

```bash
just dev -y
omnifs status
FRONTEND=$(docker ps --filter label=ai.0xff.omnifs.home="$HOME/.omnifs-dev" --format '{{.Names}}')
docker exec -it -w /omnifs "$FRONTEND" /bin/sh
tail -n 80 ~/.omnifs-dev/cache/daemon.log
```

Frontend image, built standalone (no daemon, no attach):

```bash
just frontend-image
docker run --rm --entrypoint /usr/local/bin/omnifs omnifs-frontend:dev --version
docker run --rm --entrypoint tail omnifs-frontend:dev --version | head -1
docker run --rm omnifs-frontend:dev # fails loudly: OMNIFS_ATTACH_ADDR is unset
```
