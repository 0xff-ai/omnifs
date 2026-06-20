# Daemon / CLI split (omnifsd + omnifs)

Status: implemented, then substantially superseded by two later changes. (1) The two binaries collapsed into a single `omnifs` binary; the daemon runs as the hidden `omnifs daemon` subcommand (same separate-process model and HTTP control API, no separate `omnifsd` artifact). (2) The push-based mount lifecycle described below (`POST`/`DELETE /v1/mounts`, `up` pushing resolved specs, the daemon booting an empty registry and waiting) was replaced by disk reconcile: the daemon loads `mounts/*.json` on start, `POST /v1/reconcile` converges the running set, and `POST /v1/shutdown` tears it down. Teardown is backend-transparent: `omnifs down`/`reset` identify the running backend from the daemon (`DaemonStatus.launch`) and a launch record, never from `[system].runtime`. The crate boundary holds; the control-protocol and mount-delivery sections below describe the original push model and are superseded. See `PLAN.md` and the code for the current shape.
Scope: new `crates/omnifs-daemon`, `crates/omnifs-cli`, `crates/omnifs-host` crate boundary, container entrypoint, control API, mount lifecycle
Related: `docs/design/cli-redesign.md`, `docs/design/mount-lifecycle.md`, `docs/design/host-auth.md`, `docs/design/architecture.md` §9 (inspector emission)

## Context

Today one `omnifs` binary plays two roles. On the host it is the CLI: it talks to Docker directly through bollard, resolves configuration and credentials, bind-mounts runtime state, and launches the runtime container. Inside the container the same binary is the runtime: the entrypoint execs the hidden `omnifs daemon mount` subcommand (`crates/omnifs-cli/src/commands/daemon.rs`), which loads the WASM provider registry and blocks in the FUSE loop.

The host↔runtime channels are ad hoc: the Docker API, `docker exec` (status, logs, shell), bind-mounted files, a `runtime_state.json` status file, and the inspector's one-way TCP event stream on port 7878. Mounts are frozen at registry load; `omnifs init` requires a container restart to take effect. The host CLI links wasmtime, fuser, and all of omnifs-host even on macOS, where none of it can run.

This document splits the binary into `omnifsd` (the runtime daemon) and `omnifs` (the CLI), in the docker/dockerd mold, and replaces the ad hoc channels with one HTTP control API plus a writable runtime-home bind.

Two standing constraints shape everything below:

- **The dockerless future is load-bearing.** NFSv4 (and later FSKit) support will let `omnifsd` run directly on the host with no container. Docker is *one launch mechanism*, owned entirely by the CLI; nothing in `omnifsd` or the control protocol may assume a container exists.
- **This is alpha software and breakage is fine.** The split is the opportunity to delete awkward machinery, not to preserve it behind shims. No transition re-exports, no compatibility subcommands, no dual code paths kept "just in case."

## Decisions

| # | Branch | Decision | Why |
|---|---|---|---|
| 1 | Daemon model | `omnifsd` is the runtime as its own binary, with a control API. The CLI owns daemon lifecycle; today that means the Docker container lifecycle (`up`/`down` = create/destroy container). | Docker-like UX without a third host-side process. When native mounts land, the CLI grows a second launch backend that spawns `omnifsd` directly; the daemon and protocol are identical in both modes. |
| 2 | Transport | HTTP + JSON, `/v1/` path prefix. Docker mode: TCP loopback on the published port (7878). `--listen` takes a `SocketAddr` (TCP loopback only today). Native mode (future) prefers a Unix socket; the flag would grow `unix:path` parsing then. | Curl-able, versionable, axum is a cheap dependency. Unix sockets cannot cross the Docker Desktop VM boundary (established by the inspector work), so loopback TCP is the container transport; UDS is the natural native transport. |
| 3 | API auth | None; trust the local channel | Single-user dev machine posture, same as the existing inspector port. Made tenable by #4: no secret material ever rides the API. See Security posture. |
| 4 | Mount delivery | Specs over the API, credentials through `OMNIFS_HOME`. `omnifs up` starts an empty daemon and POSTs each resolved mount spec; the host omnifs home is bind-mounted writable in docker mode and used directly in native mode. Host-managed auth stays as `scheme` plus optional `account`; explicit `token_file` / `token_env` remain external user-managed sources. | One control path for cold start and hot add; secret values stay off the unauthenticated wire; the trusted daemon reads and refreshes the same file-backed `credentials.json` as the host CLI. The mounts-dir bind, daemon-side config scanning, and credential-copy bridge are deleted. |
| 5 | Hot mounts | In scope: `POST /v1/mounts` and `DELETE /v1/mounts/{name}` take effect on the running daemon | The headline user-visible win: `omnifs init` while running, no restart. |
| 6 | Image contents | The runtime image ships both `omnifsd` (entrypoint) and the Linux `omnifs` CLI | `omnifs status` inside `omnifs shell` is a documented workflow. The slimmed CLI no longer links wasmtime/fuser, so the image cost is small. |
| 7 | Inspector | Folds into the HTTP server as a streaming `GET /v1/events` endpoint on the same listener | One listener, one server; the raw-TCP custom protocol goes away. `InspectorEvent` schema and the newline-framed JSON wire format are unchanged. |
| 8 | Cleanup license | Delete rather than deprecate: `omnifs daemon` subcommand, `runtime_state.json`, `debug install-dev-mounts`, entrypoint mount/symlink choreography, the mounts-dir bind, exec-based status. Move code and fix call sites; no bridge layers. | Alpha. CLI and image ship as a version-coupled pair, so there is no skew population to protect. |

## Binaries and crates

**`crates/omnifs-daemon`** (new, binary `omnifsd`): the daemon main. Absorbs the body of `commands/daemon.rs` — paths, `GitCloner`, `ProviderRegistry`, FUSE `run_blocking` — plus the HTTP server and the mount manager (hot add/remove). Depends on `omnifs-host`, `omnifs-fuse`, axum. Linux is the only supported target today (FUSE); the crate must keep building without container assumptions so the native NFSv4 mode is a frontend addition, not a port.

Flags: `--mount-point`, `--config-dir`, `--cache-dir`, `--listen <addr>` (a `SocketAddr`, TCP loopback only today), `--root-symlinks` (container-image nicety: maintain `/github → /omnifs/github` links as mounts come and go; off by default, passed by the entrypoint, meaningless and absent in native mode).

**`crates/omnifs-api`** (new, library): serde DTOs shared by CLI and daemon — `DaemonStatus`/`VersionInfo`/`ReadyInfo` and (phase 3) the mount-create request body. No business logic. The CLI's `StatusJson` stays in the CLI: it is the *merged* host+daemon presentation for `omnifs status --json`, not a wire type the daemon ever sees.

**`crates/omnifs-cli`**: drops `omnifs-host` and `omnifs-fuse` (and with them wasmtime and fuser). Keeps bollard. The daemon lifecycle sits behind a small backend seam — launch, terminate, control endpoint, runtime-home path — with Docker as the only implementation now and a native spawn backend later. Keep the seam exactly that small; it is not a plugin system.

**Crate boundary move**: the CLI's only remaining `omnifs-host` imports are `mounts::{Spec, Resolved, Catalog}` + builtins, the config validator, and the oauth-request builder. The `mounts` module depends only on `omnifs-core` and the provider contract (verified), so it moves into `omnifs-mount`; the config validator (`validate_config`) lands in `omnifs-provider` and the oauth-request builder in `omnifs-auth`, each at the layer of its consumers. Call sites are fixed in the same change; `omnifs-host` does not re-export the moved items.

## Daemon topology and startup

`omnifsd` starts with an **empty registry**: it brings up the filesystem frontend (FUSE today) on `--mount-point` and the HTTP listener, then waits for mounts. `GET /v1/ready` reports true once both are up. Mounts arrive exclusively via `POST /v1/mounts`; there is no mounts-dir scan.

**Docker mode** (the only shipped mode for now): the entrypoint shrinks to directory creation, the log tee, and `exec omnifsd --root-symlinks --listen 0.0.0.0:7878 ...`. Container binds shrink to exactly:

- the host `OMNIFS_HOME` directory as the daemon's writable runtime home, including `credentials.json`, `mounts/`, `providers/`, `cache/`, and `config.toml`;
- environmental passthroughs that are not omnifs surface: `SSH_AUTH_SOCK`, `/var/run/docker.sock` (optional), user preopened paths, dev-flow extras.

**Native mode** (future, with NFSv4/FSKit): the CLI spawns `omnifsd` directly with the same resolved `OMNIFS_HOME` and, once the flag grows `unix:path` parsing, `--listen` at a Unix socket. Nothing else changes: same API, same credential contract, same lifecycle verbs. The FUSE frontend is swapped for an NFSv4 server behind the same registry; that frontend is a separate design when it lands.

A bare `docker run` of the image yields an empty filesystem until a CLI pushes mounts. This is deliberate; `omnifs dev` pushes the built-in dev mount specs through the API, and the entrypoint's `install-dev-mounts` step is deleted.

## Control API

All endpoints under `/v1`, JSON bodies. The API carries **no secret values in either direction**; status redacts as today.

| Endpoint | Purpose |
|---|---|
| `GET /v1/ready` | Readiness gate for `omnifs up` (replaces exec-probing `/omnifs`). |
| `GET /v1/version` | Daemon semver + API version; CLI compatibility check. |
| `GET /v1/status` | Typed status payload (replaces `docker exec omnifs omnifs status` and `runtime_state.json`); active mounts are surfaced here via `DaemonStatus.mounts`. |
| `POST /v1/mounts` | Create a mount from a resolved spec. Host-managed credential references are `scheme` plus optional `account`, resolved against `credentials.json`. |
| `DELETE /v1/mounts/{name}` | Remove a mount live. |
| `GET /v1/events` | Inspector stream: chunked newline-framed JSON records, the existing `omnifs-inspector` wire format. Replaces the raw TCP listener. |

`runtime_state.json` is deleted once `status` is API-backed. `omnifs status` on the host queries the daemon endpoint when one is running and falls back to its config-only offline view otherwise; inside the container the shipped CLI hits the local listener.

## Mount lifecycle after the split

`omnifs up`:

1. Resolve enabled mounts and validate required host-managed credentials in the file store.
2. Launch the daemon via the backend (docker mode: create/start the container with the writable `OMNIFS_HOME` bind).
3. Poll `GET /v1/ready`.
4. `POST /v1/mounts` for each resolved spec.
5. Gate on `GET /v1/status`.

Failure after container launch tears the daemon down.

`omnifs down`:

1. Terminate the daemon via the backend.

`omnifs init` / `omnifs mounts rm` while the daemon is running: do the host-side work (config write, OAuth flow, store update) as today, then apply live with `POST /v1/mounts` / `DELETE /v1/mounts/{name}`. The running daemon sees credential updates through the writable `OMNIFS_HOME` bind. When no daemon is running these commands degrade to config-only, exactly as today.

**Restart-required detection (docker mode only).** Docker cannot add bind mounts to a running container. A hot-added mount that needs a new host bind (a `db` provider pointing at a host SQLite file, the docker provider when the socket was not bound at `up`) cannot be applied live; the CLI detects required-but-absent binds by inspecting the running container and reports "restart required: run `omnifs up`". Native mode has no such limitation — this check lives in the docker backend, not in the protocol.

## Registry dynamics (hot mounts)

`ProviderRegistry` becomes a concurrent map of `Arc<Runtime>` behind interior mutability (the FUSE `Frontend` already holds the registry behind an `Arc` and resolves per-path by mount name):

- **Add**: resolve the spec against provider WASM metadata (daemon-side, providers dir), instantiate the `Runtime`, insert, spawn that mount's timer task. Timer tasks move from one bulk `start_timers` to per-mount spawn/abort so add and remove are symmetric.
- **Remove**: abort the timer task, call `runtime.shutdown()`, drop the entry. FUSE inodes belonging to the removed mount answer `ENOENT`; the kernel root listing is invalidated through the existing notifier. View-cache entries for the mount are dropped; durable object-cache entries are left to capacity eviction (mount-prefixed, harmless, and a re-added mount benefits from them).
- The frontend's root listing derives from the live registry, so mounts appear and disappear in `ls /omnifs` without remount. This registry contract is frontend-agnostic and carries over to the future NFSv4 frontend unchanged.

## Credentials

Custody is direct: the host owns the file-backed durable store at `credentials.json`, and the trusted daemon reads and refreshes that same file through `OMNIFS_HOME`. Interactive OAuth (browser, device flow) stays in the CLI. The in-daemon 401-triggered refresh path (`omnifs-host/src/auth.rs`) refreshes in place under the existing file lock. Static-token mounts with `scheme` plus optional `account` read from the same store; explicit `token_file` and `token_env` remain external user-managed sources.

## Security posture

The control API is unauthenticated local HTTP. Because of decision #4 it is **control-plane only**: a local process that reaches it can mutate mounts, read status, and subscribe to events, but cannot read or inject credential values through it. Credentials live in the file-backed `OMNIFS_HOME` store with restrictive permissions and are read only by the trusted daemon. This matches the single-user-dev-machine posture of the existing inspector port.

Guardrails:

- Docker mode publishes the port on `127.0.0.1` only, never `0.0.0.0`, on the host side. Native mode prefers a Unix socket, which restores same-user enforcement for free.
- API request/response logging must not include bodies; the existing `log_redaction` discipline extends to the HTTP layer.
- If a stronger posture is ever needed (shared hosts), the mitigation is a same-user transport in native mode or a launch-time control token enforced by one axum middleware. That token must not become another credential-transfer path.

## Versioning and compatibility

The image label handshake (`ai.0xff.omnifs.min-launcher-version`) stays as the pre-start gate against old CLIs launching new images. Post-start, `GET /v1/version` returns `{ version, apiVersion }`; the CLI refuses to manage a daemon with an incompatible `apiVersion`. CLI and image ship as a version-coupled pair per release (same unprefixed semver), so these checks guard accidental skew, not supported divergence — which is also why no compatibility shims are kept for the deleted surfaces.

## Packaging and release ramifications

- `omnifsd` is an image-only artifact for now: built for Linux x64/arm64 in the existing native CI lanes, staged into the runtime image by `scripts/ci/build-runtime-image.sh` alongside the Linux CLI and the provider WASM components. The contributor `Dockerfile` builds both binaries. When native mode ships, `omnifsd` joins the platform artifact set; nothing in this design needs to change for that beyond distribution.
- npm is untouched: the published packages ship the (now much smaller) CLI binary only. `npm/platforms.json` does not change.
- `cargo nextest run` and `just check` package globs pick up the new crates; no hand-maintained lists.

## Phasing

Each phase lands independently and keeps `omnifs dev` plus the smoke harness green. Deletions happen in the phase that obsoletes them, not in a cleanup epilogue.

1. **Binary split.** Create `crates/omnifs-daemon` from `commands/daemon.rs`; move the `mounts` module down to `omnifs-mount`, the config validator to `omnifs-provider`, and the oauth-request builder to `omnifs-auth`; entrypoint execs `omnifsd`; Dockerfile ships both binaries; CLI drops `omnifs-host`/`omnifs-fuse`; the `omnifs daemon` subcommand is deleted. No behavior change.
2. **Control plane.** Axum server in `omnifsd`; `ready`/`version`/`status`/`events`; inspector TCP listener replaced by `GET /v1/events`; CLI switches the `up` readiness gate, `status`, and `inspect` from exec/state-file/raw-TCP to the API; `runtime_state.json` deleted.
3. **API mount delivery.** `up` pushes specs via `POST /v1/mounts`; the mounts-dir bind and daemon-side config scanning are deleted; the writable `OMNIFS_HOME` bind becomes the only omnifs-owned runtime-state bind; entrypoint dev-mount install replaced by `omnifs dev` pushing specs; `debug install-dev-mounts` deleted.
4. **Hot mounts.** Registry add/remove dynamics, live `POST`/`DELETE`, `init`/`mounts rm` apply live when running, docker-backend restart-required detection, daemon-managed root symlinks behind `--root-symlinks`.

## Non-goals

- Shipping the native (dockerless) mode, the NFSv4/FSKit frontend, or a host-side supervisor daemon now. This design only guarantees they slot in without protocol or daemon changes.
- API authentication (recorded mitigation only), multi-session, or remote (non-loopback) access.
- Mutations, mount config editing via the API, or any write surface beyond mount add/remove.
- Changing the clone transport, auth model, or provider protocol.

## Open questions

- **Events framing.** Chunked newline-framed JSON is the default (zero format change for `parse_record_line`); SSE would buy auto-reconnect semantics in exchange for a framing layer. Decide in phase 2.
- **`omnifs logs` over the API.** A `GET /v1/logs?follow=1` endpoint could replace the `docker exec tail -F` bridge later; out of scope for the split.
- **Object-cache hygiene on mount remove.** Leaving durable entries to capacity eviction is the simple call; an explicit purge can be added if stale-prefix buildup shows up in practice.
