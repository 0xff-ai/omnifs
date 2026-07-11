# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, REST API, mount delivery, runtime modes, workspace layout, dev home, and generated API shape.

## Read when

Read this before touching `omnifs-cli`, `omnifs-daemon`, `omnifs-api`, lifecycle commands, daemon status, REST routes, mount delivery, reconcile behavior, the optional Docker-hosted FUSE frontend, provider bundle installation, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes a REST API whose schema lives in `omnifs-api` and whose checked-in OpenAPI document is generated from the daemon implementation. Credential material is never transmitted on the wire.

A host-native daemon serves the control API over a Unix domain socket at `$OMNIFS_HOME/control.sock`. Auth on that socket is filesystem permissions: the config directory is forced to `0700` and the socket to `0600`, so only the owning user can connect, and there is no bearer token on that path. The bearer-token middleware survives but is enforced only on a TCP listener, which the daemon binds only for the `OMNIFS_DAEMON_ADDR` debug/test path (there is no other TCP control listener). There, the token lives in daemon memory only (no token file); its value comes from `OMNIFS_CONTROL_TOKEN` when the caller injects one, else is generated per start.

`$OMNIFS_HOME/daemon.json` is the single daemon-owned runtime record, replacing both `launch.json` and the `control-token` file. It records the endpoint to dial (`{ "kind": "unix", "path": ... }`), the backend identity (the native process id), a per-start `instance_id`, the serving frontends, `started_at`, and an optional `attach` object (`{ "addr": ..., "token": ... }`) for the TCP namespace-attach listener described below; `attach` is absent whenever TCP attach was never requested. It is written mode `0600` because the attach object carries a token. The daemon writes it the moment its socket is bound and routes are installed, and removes it on graceful exit; a crash leaves it stale, and the next CLI command that dials a refused/missing socket removes the stale record and reports the daemon as not running. Teardown (`omnifs down`, `omnifs reset`) removes the record after reclaiming the daemon's mount, liveness-checking the record's pid before trusting it for a stale sweep.

The CLI resolves which endpoint to dial in one order: `OMNIFS_DAEMON_ADDR` when set (TCP, bearer token from `OMNIFS_CONTROL_TOKEN`), else the workspace's `daemon.json` (unix socket), else the daemon is not running (exit 3, no blind port dialing). It asserts the `instance_id` echoed by `/v1/status` against the record's, so a record overwritten by a restart mid-command is caught. Because the CLI only ever dials an endpoint from its own workspace's record, a daemon owned by a different `OMNIFS_HOME` is structurally unaddressable.

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

There is one daemon runtime: host-native. `omnifs up` always spawns a host-native child process; there is no Docker daemon runtime and no `--runtime`/`[system].runtime` choice to make. The daemon is a pure namespace server; it never mounts a frontend in-process.

Docker's only role in the runtime is delivering the *optional* FUSE frontend (`omnifs frontend up|down|status`, below): a separate, credential-free container attached to the host-native daemon's shared namespace over TCP. It is never how the daemon itself runs, and `DaemonBackend`/`RecordedBackend` carry no Docker variant.

The daemon is an attach registry over one shared namespace. Frontends are separate slim runner processes, and the CLI owns their lifecycle. `DaemonStatus.frontends` reports the deterministic live attachment set in driver, kind, and mount-point order.

`omnifs setup` has no runtime stage: the daemon is always host-native. Its mount-point question and Docker reachability row apply only when the effective frontend plan includes a local or docker entry respectively; a guest-only config gets neither and no fabricated host path. Frontend delivery is declarative: `[[frontends]]` config entries (`kind` = `fuse`/`nfs`, `driver` = `local`/`docker`/`krunkit`, plus a `local`-only `mount_point`) name the frontends to run, strictly validated (driver/kind compatibility, at most one docker and one krunkit entry, no two local entries resolving to the same mount point). An absent or empty list falls back to the platform default plan: Linux gets one local FUSE frontend; macOS gets one local NFS frontend plus the Docker-hosted FUSE frontend; other hosts get one local NFS frontend. `omnifs up` starts the host-native daemon, then launches every frontend in the effective plan, local entries before guest ones; a failure launching the implicit macOS Docker default is reported without failing the command, since the native mount above it already succeeded, while every other entry failure is fatal. `--no-frontend` skips launching any of them, on every OS.

`DaemonStatus.backend` is the daemon-reported backend fact, not a config echo: it reports the daemon's own process id. `daemon.json` is only a cache of that daemon-reported fact for stale teardown.

Keep frontend-specific Docker policy (image resolution, container naming, the no-credentials contract) in the frontend command paths (`crates/omnifs-cli/src/commands/frontend/`, `crates/omnifs-cli/src/frontend_container.rs`); the daemon launch path has no Docker policy to keep aligned with it.

### Namespace attach sockets and out-of-process frontends

The daemon always serves its shared namespace over `$OMNIFS_HOME/frontends/local.sock` for local frontend runners. The `frontends/` directory is forced to `0700` and the socket to `0600`; filesystem permissions are its authentication. A refused stale socket is removed before binding, while any ambiguous probe error fails closed. The socket is removed on graceful exit. There is no named attach-socket flag. The Omnifs VFS wire protocol uses length-delimited postcard framing from `omnifs-vfs-wire`, not an RPC framework. It transports the engine-owned `Namespace` surface and does not own projection semantics.

`/v1/ready` reports ready only after startup reconcile completes and every requested namespace listener has bound. Listener readiness does not require a frontend to be attached. `DaemonStatus.mount_point` is derived from the first local live attachment and is empty when none is connected; `DaemonStatus.frontends` is authoritative.

The separate `omnifs-fuse` and `omnifs-nfs` binaries attach a VFS-wire-backed namespace and serve one mount until unmount. They run no provider and their normal dependency graphs exclude Wasmtime, the provider bundle, and the daemon control plane. `omnifs-fuse --attach <socket> --mount-point <path>` is the Docker image and krunkit guest entrypoint; with `--attach` absent, `OMNIFS_ATTACH_ADDR` and `OMNIFS_ATTACH_TOKEN` select TCP or vsock. `omnifs-nfs --attach <socket> --mount-point <path>` (the runner receives its per-mount state leaf under `cache/frontends/nfs/<hash>`) additionally persists filehandle identity and mount discovery state so a restarted runner can resume an active kernel mount. Corrupt leaves affect only their mount. Both runners consume attach resolution, reconnect, and readiness signaling from `omnifs-vfs-wire`. A FUSE reattach is observational because its inode table lives with the mount process; an NFS reattach invalidates cached `NodeId` values and lazily resolves its persisted protocol identities against the new daemon instance.

`omnifs frontend {up,down,status}` owns frontend process lifecycle; it is not a daemon runtime mode. The Docker path binds the TCP attach listener via `POST /v1/frontend/attach-target`, resolves the frontend image by build-channel provenance (`ghcr.io/0xff-ai/omnifs-frontend:<version>` for a release binary, `omnifs-frontend:dev` for a dev binary, `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override), and runs the container with no binds, no `OMNIFS_HOME`, no docker.sock, no SSH agent, and no published ports: only `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` cross into it. A fail-closed check right after start asserts an empty `Mounts` array and an env set of exactly those two vars plus the image's own defaults, killing the container on violation. One frontend container runs per workspace: `omnifs-frontend` for the default home, `omnifs-frontend-<hash8(home)>` otherwise. The attach listener has no close route, so it stays bound until the daemon restarts. Runtime-record frontend entries are live attach-registry snapshots with a required `via` driver.

`--driver krunkit` (macOS only) mirrors that same build-channel provenance for its guest disk image instead of a container image: a release binary resolves `ghcr.io/0xff-ai/omnifs-guest:<version>` and pulls it into the workspace cache on first use (never re-downloading once cached); a dev binary resolves the local `target/guest-image/omnifs-guest.raw` and never downloads, regardless of override. `[frontend] guest_image`/`OMNIFS_GUEST_IMAGE` override either default the same way `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override the Docker image.

The Omnifs VFS wire protocol also serves over token-authenticated TCP because a containerized frontend cannot share the host Unix socket. Docker Desktop forwards `host.docker.internal` to host loopback, so `--attach-tcp <port>` (`0` = ephemeral) and the default `POST /v1/frontend/attach-target` request bind `127.0.0.1:<port>`. Native Linux maps that name to the default Docker bridge gateway instead, so `omnifs frontend up` discovers that gateway from Docker and asks the daemon to bind only that address; the daemon verifies that the address is assigned to `docker0`. The route always selects an ephemeral port, must not bind `0.0.0.0`, and must not use host networking. The per-instance attach token is 32 hex chars, generated once per daemon start the first time TCP attach is requested. The route is idempotent, returning the already-bound address and token on a repeat call rather than rebinding; `frontend up` rejects an existing listener on the wrong address and asks the operator to restart the daemon. The wire protocol v3 carries the connecting frontend's identity (kind + guest-side mount point, display-only) in the handshake alongside the token; a v2 (or lower) client (or a wrong/missing token on a TCP listener) is rejected outright with a named reason. The daemon's frontend registry tracks every attached client live from an `AttachObserver` the handshake fires into, labeled with the delivery mechanism (`docker` for TCP, `krunkit` for the vsock-proxy UDS listener, and `local` for `frontends/local.sock`) assigned at bind time, never from anything the guest claims about itself. `GET /v1/status` reports these live attachments. The fixed local listener never checks a token because filesystem permissions are its whole auth; the token-checking UDS listener bound by `POST /v1/frontend/attach-target/vsock` requires it, same as TCP.

### Dev home

`scripts/dev.ts` owns contributor dev state. It renders a dedicated `~/.omnifs-dev` home, builds the CLI with its provider bundle, starts the host-native daemon through `omnifs up --no-frontend`, then attaches the credential-free frontend and opens the developer at `/omnifs` inside it. Host CLI commands use the normal workspace resolution unless `OMNIFS_HOME` is explicit; do not reintroduce a Rust-side dev command or dev-session owner.

### Provider bundles

`just providers build` emits the content-addressed provider-store bundle at `target/omnifs-provider-store`. `scripts/dev.ts` embeds it into the natively-built `omnifs` CLI/daemon binary via `OMNIFS_PROVIDER_BUNDLE_DIR` and copies it into the dev provider store for runtime mount pinning; it must not compile providers again. The frontend image (`Dockerfile`'s `frontend-dev`/`frontend-release` stages, built from `fuse-builder`) needs no provider-store build context at all: it runs the slim `omnifs-fuse` binary, which links no engine and no provider bundle. Release CLI binaries embed the provider bundle and unpack it into `OMNIFS_HOME/providers`.

Provider-store indexes strict-parse both the top-level index object and retained provider entries. Unknown keys make the store unreadable instead of being silently accepted.

## Must not

- Bypass the daemon mount CRUD API for config changes while a compatible daemon is ready.
- Add a second spec read or write path that bypasses `mount::Registry`, or write a spec to a file whose stem is not its mount name.
- Add more direct workspace coupling when a REST API extension fits.
- Put credential material or provider secrets in snapshot export routes or snapshot
  indexes.
- Reintroduce a persisted daemon runtime-backend choice (a `[system].runtime`-shaped config field or a `--runtime` flag); the daemon has exactly one runtime.
- Dial a default control port blind. The CLI only ever dials an endpoint read from its own workspace's `daemon.json` or from `OMNIFS_DAEMON_ADDR`.
- Hand-edit `crates/omnifs-api/openapi/daemon.json`.
- Add API routes without keeping client/status behavior and schema generation in step.
- Reintroduce a separate public `omnifsd` binary name in docs or UX.
- Deepen Docker assumptions in daemon architecture; Docker policy belongs in the frontend command paths only.
- Present macOS host-native integration as macFUSE.
- Make the frontend (or any other) Docker image own release provider bundles; the CLI binary is the sole owner.
- Assume a fresh worktree already has provider artifacts or wasi-sdk.
- Move generated or cache state into source directories.

## Code

- `crates/omnifs-api/src/lib.rs`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/server.rs`
- `crates/omnifs-fuse/src/bin/omnifs_fuse.rs`
- `crates/omnifs-vfs-wire/src/beacon.rs`
- `crates/omnifs-cli/src/commands/frontend/`
- `crates/omnifs-cli/src/frontend_container.rs`
- `crates/omnifs-vfs-wire/src/lib.rs`
- `crates/omnifs-itest/src/live.rs`
- `crates/omnifs-nfs/src/bin/omnifs_nfs.rs`
- `crates/omnifs-workspace/src/mounts/mod.rs`
- `crates/omnifs-cli/src/launch.rs`
- `crates/omnifs-cli/src/launch_backend.rs`
- `crates/omnifs-cli/src/runtime.rs`
- `crates/omnifs-cli/src/daemon_teardown.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `crates/omnifs-workspace/src/layout.rs`
- `scripts/dev.ts`
- `Dockerfile`
- `crates/omnifs-daemon/src/bin/openapi.rs`
- `CONTRIBUTING.md`

## Validation

- Control API changes need daemon API tests after `just openapi` regenerates the checked-in spec.
- API shape changes run `just openapi` and keep generated OpenAPI synchronized.
- Daemon-launch or frontend-attach changes need targeted CLI/daemon tests and live runtime validation for the affected path.
- Contributor workflow changes need CLI tests and, when touching launch behavior, `just dev -y` plus the smoke path in `CONTRIBUTING.md`.
