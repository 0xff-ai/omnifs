# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, REST API, mount delivery, runtime modes, workspace layout, dev home, and generated API shape.

## Read when

Read this before touching `omnifs-cli`, `omnifs-daemon`, `omnifs-api`, lifecycle commands, daemon status, REST routes, mount delivery, reconcile behavior, runtime backend selection, Docker/native launch, provider bundle installation, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes a REST API whose schema lives in `omnifs-api` and whose checked-in OpenAPI document is generated from the daemon implementation. Credential material is never transmitted on the wire.

A host-native daemon serves the control API over a Unix domain socket at `$OMNIFS_HOME/control.sock`. Auth on that socket is filesystem permissions: the config directory is forced to `0700` and the socket to `0600`, so only the owning user can connect, and there is no bearer token on that path. The bearer-token middleware survives but is enforced only on TCP listeners: the Docker bridge (the container publishes `7878` on the host loopback) and the `OMNIFS_DAEMON_ADDR` debug path. On those, the token lives in daemon memory only (no token file); its value comes from `OMNIFS_CONTROL_TOKEN` when a launcher injects one, else is generated per start.

`$OMNIFS_HOME/daemon.json` is the single daemon-owned runtime record, replacing both `launch.json` and the `control-token` file. It records the endpoint to dial (`{ "kind": "unix", "path": ... }` or `{ "kind": "tcp", "addr": ..., "token": ... }`), the backend identity (native pid, or Docker container name plus image), a per-start `instance_id`, the serving frontends, `started_at`, and an optional `attach` object (`{ "addr": ..., "token": ... }`) for the TCP namespace-attach listener described below; `attach` is absent whenever TCP attach was never requested. It is written mode `0600` because the tcp endpoint and the attach object both carry a token. A host-native daemon writes it the moment its socket is bound and routes are installed, and removes it on graceful exit; a crash leaves it stale, and the next CLI command that dials a refused/missing socket removes the stale record and reports the daemon as not running. The Docker launcher writes the record host-side once the in-container daemon is serving (a Unix socket on a macOS Docker bind mount is unreliable, so the container speaks TCP). Teardown (`omnifs down`, `omnifs reset`) removes the record after reclaiming the backend, liveness-checking a native record's pid before trusting it for a stale sweep.

The CLI resolves which endpoint to dial in one order: `OMNIFS_DAEMON_ADDR` when set (TCP, bearer token from `OMNIFS_CONTROL_TOKEN`), else the workspace's `daemon.json` (unix socket, or tcp with the record's token), else the daemon is not running (exit 3, no blind port dialing). It asserts the `instance_id` echoed by `/v1/status` against the record's, so a record overwritten by a restart mid-command is caught. Because the CLI only ever dials an endpoint from its own workspace's record, a daemon owned by a different `OMNIFS_HOME` is structurally unaddressable.

`GET /v1/ready` is the only unauthenticated control route on the TCP listener. Every other TCP route, including `/v1/events`, snapshot export routes, and future routes, is authenticated by default through the daemon router middleware. Missing or wrong bearer tokens fail closed with HTTP 401 and an `ApiError` whose code is `Unauthorized`. The Unix-socket listener omits the middleware entirely.

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

The daemon is a multi-frontend registry: it builds one renderer per requested frontend over a single shared namespace. The `--frontend <kind>=<mount_point>` daemon flag (repeatable, `fuse` or `nfs`) serves an explicit set; absent, the daemon serves the single platform default at the resolved mount point. This is a daemon surface only, not yet CLI-exposed; the container entrypoint and CLI launcher pass no `--frontend` flag, so their behavior is unchanged. `DaemonStatus.frontends` reports the served set; the singular `DaemonStatus.frontend` remains as the first served entry. Linux can serve FUSE and NFS concurrently; macOS is NFS-only.

`omnifs setup` chooses the default runtime and records it at `[system].runtime`; the picker default is per-OS. On Linux and WSL native kernel FUSE is the default, with Docker offered as the opt-in second choice. On macOS Docker remains the default, with native loopback NFS (experimental, read-only) as the opt-in second choice. The mount-point question is asked only when the chosen runtime is native; under Docker files always appear at the in-container mount point. `omnifs up --runtime <docker|native>` overrides that default for a single launch without persisting it. The override flows into the runtime record, so `down`, `status`, and `shell` read the actually-running backend from the daemon and `daemon.json`, never from `[system].runtime`.

`DaemonStatus.backend` is the daemon-reported backend fact, not a config echo: native reports its process id, and Docker reports the launcher-provided container name plus image. `daemon.json` is only a cache of those daemon-reported facts for stale teardown. If neither daemon status nor a valid runtime record identifies the backend, teardown reports an unknown backend and stops without guessing a container name.

The default Docker image is chosen by build channel. A release binary (built by the packaging lane, which sets `OMNIFS_RELEASE` at compile time) defaults to the pinned registry tag `ghcr.io/0xff-ai/omnifs:<version>`. A locally built binary defaults to `omnifs:dev`, the floating tag `just dev` moves onto the newest local image, and never pulls. The flag > `OMNIFS_IMAGE` > config precedence is unchanged; the channel only sets the fallback default. Pulls are gated on the reference: only an image whose first path segment names a registry host (contains `.` or `:`, or is `localhost`) is ever pulled. A registry-less reference (`omnifs:dev`) that is absent locally fails with a build-it hint rather than reaching for a Docker Hub image.

Keep Docker-specific bind/materialization policy in Docker launch paths. Keep native and Docker daemon argument generation aligned where behavior is shared.

### Namespace attach sockets and out-of-process frontends

The daemon can serve its shared namespace over a Unix socket so a renderer runs in a different process than the projection owner. The `--attach-socket <name>` daemon flag (repeatable, a bare `[a-z0-9-]+` label) binds `$OMNIFS_HOME/frontends/<name>.sock` and serves the same `TreeNamespace` the in-process frontends use, with the daemon's instance id. The `frontends/` dir is `0700` and each socket `0600` (auth is filesystem permissions, like the control socket); a stale socket is unlinked after a refused connect probe, and sockets are removed on graceful exit. The wire is the length-delimited postcard framing in `omnifs-namespace-wire`, not an RPC framework.

A daemon with at least one attach socket and no `--frontend` serves the namespace only, with no in-process mount. `/v1/ready` reports ready once mounts are reconciled and every requested surface (in-process frontends plus attach sockets) is up; the Frontend health subsystem counts surfaces, so a namespace-only daemon is `Healthy` once its sockets serve. A namespace-only daemon reports an empty `DaemonStatus.mount_point` (nothing is mounted in-process). Zero frontends and zero attach sockets is unchanged: the platform default is still injected.

The hidden `omnifs frontend run --attach <socket> --kind <fuse|nfs> --mount-point <path> [--nfs-state-dir <dir>]` runner attaches a wire-backed namespace and runs the same renderer entry the daemon uses, blocking until unmount; SIGTERM/SIGINT unmount cleanly. `--kind fuse` is Linux-only. With `--attach` absent, it attaches over TCP instead, reading the target from `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` (the Docker-hosted frontend's only option, since it cannot share a host Unix socket into its container). The wire client reconnects with backoff and exposes `AttachEvent::Reattached` when a reconnect lands on a different daemon instance (stale node ids); acting on it in the renderers is a later-phase concern.

`omnifs frontend {up,down,status}` is the visible lifecycle for the optional Docker-hosted FUSE frontend, a separate credential-free container attached to a host-native daemon's TCP namespace listener; it is not a daemon runtime mode and `[system].runtime` never references it. `up` ensures a host-native daemon (refusing a running Docker-backend daemon, since that already renders FUSE in-process), binds the TCP attach listener via `POST /v1/attach-listeners`, resolves the frontend image by the same build-channel provenance as the daemon runtime image (`ghcr.io/0xff-ai/omnifs-frontend:<version>` for a release binary, `omnifs-frontend:dev` for a dev binary, `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override), and runs the container with no binds, no `OMNIFS_HOME`, no docker.sock, no SSH agent, and no published ports: only `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` cross into it. A fail-closed check right after start asserts an empty `Mounts` array and an env set of exactly those two vars plus the image's own defaults, killing the container on violation. One frontend container runs per workspace: `omnifs-frontend` for the default home, `omnifs-frontend-<hash8(home)>` otherwise. `down` removes the container; the attach listener itself has no close route (`POST /v1/attach-listeners` only ever binds, idempotently), so it stays bound until the daemon restarts. `omnifs down` tears down a running frontend container first. The runtime record's `frontends` entries gain an optional `via` field (`"docker"` for this frontend, absent for a host-native one).

The namespace wire also serves over token-authenticated TCP because a containerized frontend cannot share the host Unix socket. Docker Desktop forwards `host.docker.internal` to host loopback, so `--attach-tcp <port>` (`0` = ephemeral) and the default `POST /v1/attach-listeners` request bind `127.0.0.1:<port>`. Native Linux maps that name to the default Docker bridge gateway instead, so `omnifs frontend up` discovers that gateway from Docker and asks the daemon to bind only that address; the daemon verifies that the address is assigned to `docker0`. The route always selects an ephemeral port, must not bind `0.0.0.0`, and must not use host networking. The per-instance attach token is 32 hex chars, generated once per daemon start the first time TCP attach is requested. The route is idempotent, returning the already-bound address and token on a repeat call rather than rebinding; `frontend up` rejects an existing listener on the wrong address and asks the operator to restart the daemon. The handshake protocol bumped 1 to 2 to carry the token: a v1 client (or a wrong/missing token on a TCP listener) is rejected outright with a named reason, no negotiation fallback. A Unix-socket listener still never checks the token.

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
- Infer launch backend only from config when daemon status or the runtime record identify the running backend.
- Dial a default control port blind. The CLI only ever dials an endpoint read from its own workspace's `daemon.json` or from `OMNIFS_DAEMON_ADDR`.
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
- `crates/omnifs-daemon/src/frontend.rs`
- `crates/omnifs-cli/src/commands/frontend/`
- `crates/omnifs-cli/src/frontend_container.rs`
- `crates/omnifs-namespace-wire/src/lib.rs`
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
