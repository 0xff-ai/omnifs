# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, REST API, mount delivery, runtime modes, workspace layout, dev home, and generated API shape.

## Read when

Read this before touching `omnifs-cli`, `omnifs-daemon`, `omnifs-api`, lifecycle commands, daemon status, REST routes, mount delivery, reconcile behavior, runtime backend selection, Docker/native launch, provider bundle installation, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes a REST API whose schema lives in `omnifs-api` and whose checked-in OpenAPI document is generated from the daemon implementation. Credentials are never transmitted on the wire.

### Mount delivery

Current mount delivery is disk reconcile. The CLI writes specs under `mounts/`; the daemon loads them from disk on startup and converges the running set through `/v1/reconcile`.

Specs are one file per mount, and a spec file's stem is its mount name: `mount::Registry` (in `omnifs-mount`) rejects any file whose stem does not match the spec's `mount`. The Registry is the sole spec owner. The CLI is the only author and writes through it atomically (same-dir temp plus rename); the daemon reads through the same Registry on reconcile. A spec inherits its provider-manifest defaults (the auth scheme and config defaults) at creation time, so serving reads it as-is, with no read-time resolution step. Materialization still reads the pinned manifest, but only to check the spec's capability grants against the provider's declared needs, never to fill defaults.

Prefer REST API extensions for new non-secret interactions. Keep credentials off the REST API.

### Runtime modes

The daemon runs host-native or in Docker. Docker is one launch mechanism, not the architecture. Host-native frontend defaults are FUSE on Linux and NFSv4 loopback on non-Linux.

Keep Docker-specific bind/materialization policy in Docker launch paths. Keep native and Docker daemon argument generation aligned where behavior is shared.

### Dev home and workspace layout

Inside a source checkout, contributor commands use `~/.omnifs-dev` unless `OMNIFS_HOME` is explicit. Outside a checkout, normal `~/.omnifs` applies.

Keep `omnifs dev`, `shell`, `status`, `logs`, and `down` aligned on the same dev home when run from a checkout. Keep explicit `OMNIFS_HOME` as the override.

### Provider bundles

`Dockerfile` is the contributor image path for `omnifs dev`. Dev image builds consume host-built provider WASM from `target/wasm32-wasip2/release` as a named Docker build context; the normal runtime image stage must not compile providers again. `omnifs dev` installs those same host-built provider artifacts into the dev provider store for runtime mount pinning. Release runtime image assembly uses `scripts/ci/build-runtime-image.sh`. Release CLI binaries embed the provider bundle and unpack it into `OMNIFS_HOME/providers`.

## Must not

- Claim mount specs are POSTed over the control API today.
- Add a second spec read or write path that bypasses `mount::Registry`, or write a spec to a file whose stem is not its mount name.
- Add more direct workspace coupling when a REST API extension fits.
- Infer launch backend only from config when daemon status or launch records identify the running backend.
- Hand-edit `crates/omnifs-api/openapi/daemon.json`.
- Add API routes without keeping client/status behavior and schema generation in step.
- Reintroduce a separate public `omnifsd` binary name in docs or UX.
- Deepen Docker assumptions in daemon architecture.
- Present macOS host-native integration as macFUSE.
- Make the runtime image own release provider bundles.
- Assume a fresh worktree already has provider artifacts or wasi-sdk.
- Move generated or cache state into source directories.

## Code

- `crates/omnifs-api/src/lib.rs`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/server.rs`
- `crates/omnifs-cli/src/live.rs`
- `crates/omnifs-mount/src/mounts/mod.rs`
- `crates/omnifs-cli/src/launch.rs`
- `crates/omnifs-cli/src/runtime.rs`
- `crates/omnifs-cli/src/dev_support.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `crates/omnifs-home/src/lib.rs`
- `Dockerfile`
- `crates/omnifs-daemon/src/bin/openapi.rs`
- `scripts/ci/build-runtime-image.sh`
- `CONTRIBUTING.md`

## Validation

- Control API changes need daemon API tests after `just openapi` regenerates the checked-in spec.
- API shape changes run `just openapi` and keep generated OpenAPI synchronized.
- Runtime-mode changes need targeted CLI/daemon tests and live runtime validation for the affected launch path.
- Contributor workflow changes need CLI tests and, when touching runtime behavior, `just dev -y` plus the smoke path in `CONTRIBUTING.md`.
