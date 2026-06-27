# Build and validation contracts

Status: current-contract
Owns: local and CI gates, provider build artifacts, generated OpenAPI/schema files, live runtime validation, and documentation checks.

## Read when

Read this before touching CI, `just` recipes, provider artifact generation, wasi-sdk setup, OpenAPI/schema generation, docs checks, runtime smoke paths, or validation guidance.

## Rules

### Provider build artifacts

Provider and tool WASM artifacts are built with the pinned wasi-sdk. `just providers build` compiles providers/tools and injects provider metadata. `just dev` depends on that build and then runs the source CLI's `dev` command, so dev mount pinning and the dev image both consume the same host-built WASM bytes.

Install the pinned wasi-sdk with `just providers wasi-sdk` when needed. Run `just providers build` before host tests that need generated provider/tool artifacts. Use `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1` after prebuilding providers for nextest runs that would otherwise contend (`just host test` sets it for you).

### Generated OpenAPI and schemas

OpenAPI is generated from daemon implementation, and provider manifest schema is generated from provider model types. Keep generated artifacts synchronized with code.

Run `just openapi` after daemon API changes. Run `just schema` after provider manifest schema changes. Keep generated files checked in when their source model changes.

### Live runtime validation

Mount, provider, clone, traversal, frontend, or runtime behavior changes need live runtime validation. Rust checks alone are not enough.

Use `just dev -y` for the supported contributor runtime path. Check status inside the container. Exercise shell traversal and real file tools for path-surface changes.

### CI gates

Use the repo gates instead of ad hoc workspace commands. Host-target gates exclude provider/tool/test-provider WASM crates; WASM crates use provider-specific gates.

Use `just check` before a push or PR handoff. It runs the formatting, policy, OpenAPI, provider WASM, and host gates in one pass. Use `just host clippy` and `just host test` for host-target iteration. Use `just providers check`, `just providers build`, and `just providers validate` for WASM iteration.

### Documentation checks

`just docs-check` verifies doc-to-doc links and the contract file template. It does not validate code symbols or code paths.

## Must not

- Treat missing provider WASM in a fresh worktree as a product regression.
- Use `cargo check --workspace --all-targets` as a host gate.
- Assume bare provider builds contain injected metadata.
- Hand-edit generated OpenAPI or schema files as the primary fix.
- Change API/model code without running the corresponding generation check.
- Validate only the intended leaf path when parent traversal changed.
- Treat compile-time route validity as enough for seal-time behavior.
- Ignore runtime logs when the mount returns `Input/output error`.
- Assemble a pre-push gate from ad hoc Cargo commands when `just check` is available.
- Run host tests that rebuild providers in parallel without prebuilding providers when contention matters.
- Treat `just docs-check` as code-symbol validation.

## Code

- `just/dev.just`
- `just/host.just`
- `just/providers.just`
- `just/npm.just`
- `just/release.just`
- `scripts/ci/check-doc-links.sh`
- `scripts/ci/check-doc-contracts.sh`
- `scripts/openapi.ts`
- `crates/omnifs-api/openapi/daemon.json`
- `crates/omnifs-provider/schema/omnifs.provider.schema.json`
- `crates/omnifs-itest/src/lib.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `Dockerfile`
- `CONTRIBUTING.md`

## Validation

- `just providers wasi-sdk`
- `just providers build`
- `just providers check`
- `just providers validate`
- `just host clippy`
- `just host test`
- `just check`
- `just openapi-check`
- `just schema`
- `just docs-check`

Live runtime path:

```bash
just dev -y
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -n 80 /tmp/omnifs.log'
```
