# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, REST API, mount delivery, runtime modes, workspace layout, dev home, and generated API shape.

## Read when

Read this before touching `omnifs-cli`, `omnifs-daemon`, `omnifs-api`, lifecycle commands, daemon status, REST routes, mount delivery, reconcile behavior, runtime backend selection, Docker/native launch, provider bundle installation, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes a REST API whose schema lives in `omnifs-api` and whose checked-in OpenAPI document is generated from the daemon implementation. Credential material is never transmitted on the wire.

The daemon authenticates its control port with a per-start bearer token stored at `<config_dir>/control-token`. It writes the token before the listener serves, overwrites stale files on restart, and keeps the file mode at `0600`. The daemon removes the token on graceful exit, and CLI teardown (`omnifs down` and `omnifs reset`) also removes it after reclaiming the backend, so a stale token cannot outlive its daemon. The CLI reads that same host-visible file and attaches `Authorization: Bearer <token>` on every protected control request. In Docker mode, `<config_dir>` is the shared `OMNIFS_HOME` bind mount, so the host CLI reads the token file written by the in-container daemon.

`GET /v1/ready` is the only unauthenticated control route. Every other route, including `/v1/events`, snapshot export routes, and future routes, is authenticated by default through the daemon router middleware. Missing or wrong bearer tokens fail closed with HTTP 401 and an `ApiError` whose code is `Unauthorized`.

The control API may expose operational state that contains no secrets. `GET /v1/credentials` reports registered credential ids, coarse health, expiry, and scopes only; it never reports access tokens, refresh tokens, client secrets, or header material. `POST /v1/credentials/{id}/reload` reloads a registered credential from the host store and returns the same non-secret status shape. `GET /v1/providers` reports the installed provider catalog by provider name, retained artifact content hashes, and the latest artifact pointer.

Mount wire payloads distinguish provider identity from provider naming. `provider_name` is the human/catalog slug used by credentials and UX. `provider_id` is the pinned provider content hash for the exact artifact the mount runs.

### Mount delivery

Current mount delivery is control-API when the daemon is running and disk reconcile when it is not. The CLI probes the daemon and sends mount create/update/delete requests to the REST API when a compatible daemon is ready. With no ready daemon, the CLI writes specs under `mounts/` directly and the next daemon startup converges them through `/v1/reconcile`.

Specs are one file per mount, and a spec file's stem is its mount name: `mounts::Registry` (in `omnifs-workspace`) rejects any file whose stem does not match the spec's `mount`. The Registry is the sole spec owner. The daemon writes through it for running-daemon create/update/delete requests; the CLI writes through it only for offline config changes. Registry writes are atomic (same-dir temp plus rename) and serialized by the mount-registry advisory lock. A spec inherits its provider-manifest defaults (the auth scheme and config defaults) at creation time, so serving reads it as-is, with no read-time resolution step. Materialization still reads the pinned manifest, but only to check the spec's capability grants against the provider's declared needs, never to fill defaults.

`POST /v1/reconcile` converges the daemon to the on-disk specs. With no body it is a full pass. With `{ "mounts": ["name"] }` it is scoped to those mount names for planning, build, and stale removal. HTTP-triggered reconcile is non-queueing: if another reconcile holds the engine lock, the daemon returns `409 ReconcileBusy` with `Retry-After: 2`. Internal daemon calls that intentionally serialize still wait on the engine lock.

Mount specs strict-parse their top-level JSON fields. Unknown top-level keys are invalid in on-disk specs and in daemon mount CRUD requests, while the provider-owned `config` object remains opaque to the host.

Prefer REST API extensions for new non-secret interactions. Keep credential material off the REST API.

### Replica snapshots

`omnifs snapshot <mount> --out <dir>` exports a configured mount's canonical
object store as a plain directory tree plus `index.json`. When a compatible
daemon is running, the CLI reads the snapshot from `GET
/v1/mounts/{name}/export` as an `application/x-tar` stream. When no compatible
daemon answers, the CLI reads `<cache>/object` directly and writes the same
directory layout. Both paths export canonical bytes and metadata only; no
credentials are transmitted.

The snapshot tree is the audit surface for replicas. Compare rendered canonical
files with `diff -r --exclude=index.json <before> <after>`; `index.json` records
logical id, path, blake3, and size for each file and therefore changes whenever
file bytes change. Use `scripts/demo/snapshot-diff.sh` for the supported demo
flow.

### Runtime modes

The daemon runs host-native or in Docker. Docker is one launch mechanism, not the architecture. Host-native frontend defaults are FUSE on Linux and NFSv4 loopback on non-Linux.

`omnifs setup` chooses the default runtime and records it at `[system].runtime`; the picker defaults to Docker on every OS, with host-native (loopback NFS on macOS, experimental; kernel FUSE on Linux/WSL) offered as the opt-in second choice. The mount-point question is asked only when the chosen runtime is native; under Docker files always appear at the in-container mount point. `omnifs up --runtime <docker|native>` overrides that default for a single launch without persisting it. The override flows into the launch record, so `down`, `status`, and `shell` read the actually-running backend from the daemon and launch record, never from `[system].runtime`.

`DaemonStatus.backend` is the daemon-reported backend fact, not a config echo: native reports its process id, and Docker reports the launcher-provided container name plus image. `launch.json` is only a cache of those daemon-reported facts for stale teardown. If neither daemon status nor a valid launch record identifies the backend, teardown reports an unknown backend and stops without guessing a container name.

The default Docker image is chosen by build channel. A release binary (built by the packaging lane, which sets `OMNIFS_RELEASE` at compile time) defaults to the pinned registry tag `ghcr.io/0xff-ai/omnifs:<version>`. A locally built binary defaults to `omnifs:dev`, the floating tag `just dev` moves onto the newest local image, and never pulls. The flag > `OMNIFS_IMAGE` > config precedence is unchanged; the channel only sets the fallback default. Pulls are gated on the reference: only an image whose first path segment names a registry host (contains `.` or `:`, or is `localhost`) is ever pulled. A registry-less reference (`omnifs:dev`) that is absent locally fails with a build-it hint rather than reaching for a Docker Hub image.

Keep Docker-specific bind/materialization policy in Docker launch paths. Keep native and Docker daemon argument generation aligned where behavior is shared.

### Dev home

`scripts/dev.ts` owns contributor dev state. It renders a dedicated `~/.omnifs-dev` home, bind-mounts it into the container as `OMNIFS_HOME`, and opens the developer inside that container. Host CLI commands use the normal workspace resolution unless `OMNIFS_HOME` is explicit; do not reintroduce a Rust-side dev command or dev-session owner.

### Provider bundles

`Dockerfile` is the contributor image path for `just dev`. Dev image builds consume the content-addressed provider-store bundle at `target/omnifs-provider-store` as a named Docker build context; the normal runtime image stage must not compile providers again. `scripts/dev.ts` copies that same bundle into the dev provider store for runtime mount pinning. Release runtime image assembly uses `scripts/ci/build-runtime-image.sh`. Release CLI binaries embed the provider bundle and unpack it into `OMNIFS_HOME/providers`.

Provider-store indexes strict-parse both the top-level index object and retained provider entries. Unknown keys make the store unreadable instead of being silently accepted.

## Must not

- Bypass the daemon mount CRUD API for config changes while a compatible daemon is ready.
- Add a second spec read or write path that bypasses `mount::Registry`, or write a spec to a file whose stem is not its mount name.
- Add more direct workspace coupling when a REST API extension fits.
- Put credential material or provider secrets in snapshot export routes or snapshot
  indexes.
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
- `crates/omnifs-workspace/src/mounts/mod.rs`
- `crates/omnifs-cli/src/launch.rs`
- `crates/omnifs-cli/src/runtime.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `crates/omnifs-workspace/src/layout.rs`
- `scripts/dev.ts`
- `Dockerfile`
- `crates/omnifs-daemon/src/bin/openapi.rs`
- `scripts/ci/build-runtime-image.sh`
- `CONTRIBUTING.md`

## Validation

- Control API changes need daemon API tests after `just openapi` regenerates the checked-in spec.
- API shape changes run `just openapi` and keep generated OpenAPI synchronized.
- Runtime-mode changes need targeted CLI/daemon tests and live runtime validation for the affected launch path.
- Contributor workflow changes need CLI tests and, when touching runtime behavior, `just dev -y` plus the smoke path in `CONTRIBUTING.md`.
