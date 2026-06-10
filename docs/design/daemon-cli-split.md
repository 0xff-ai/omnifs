# Daemon / CLI split (omnifsd + omnifs)

Status: implemented (all four phases)
Scope: new `crates/omnifs-daemon`, `crates/omnifs-cli`, `crates/omnifs-host` crate boundary, container entrypoint, control API, mount lifecycle
Related: `docs/design/cli-redesign.md`, `docs/design/mount-lifecycle.md`, `docs/design/host-auth.md`, `docs/design/inspector-emission-architecture.md`

## Context

Today one `omnifs` binary plays two roles. On the host it is the CLI: it talks to Docker directly through bollard, materializes credentials from the host store into a per-session directory, bind-mounts configs and secrets, and launches the runtime container. Inside the container the same binary is the runtime: the entrypoint execs the hidden `omnifs daemon mount` subcommand (`crates/omnifs-cli/src/commands/daemon.rs`), which loads the WASM provider registry and blocks in the FUSE loop.

The host↔runtime channels are ad hoc: the Docker API, `docker exec` (status, logs, shell), bind-mounted files (secrets *and* mount configs), a `runtime_state.json` status file, and the inspector's one-way TCP event stream on port 7878. Mounts are frozen at registry load; `omnifs init` requires a container restart to take effect. The host CLI links wasmtime, fuser, and all of omnifs-host even on macOS, where none of it can run.

This document splits the binary into `omnifsd` (the runtime daemon) and `omnifs` (the CLI), in the docker/dockerd mold, and replaces the ad hoc channels with one HTTP control API plus a single secrets directory.

Two standing constraints shape everything below:

- **The dockerless future is load-bearing.** NFSv4 (and later FSKit) support will let `omnifsd` run directly on the host with no container. Docker is *one launch mechanism*, owned entirely by the CLI; nothing in `omnifsd` or the control protocol may assume a container exists.
- **This is alpha software and breakage is fine.** The split is the opportunity to delete awkward machinery, not to preserve it behind shims. No transition re-exports, no compatibility subcommands, no dual code paths kept "just in case."

## Decisions

| # | Branch | Decision | Why |
|---|---|---|---|
| 1 | Daemon model | `omnifsd` is the runtime as its own binary, with a control API. The CLI owns daemon lifecycle; today that means the Docker container lifecycle (`up`/`down` = create/destroy container). | Docker-like UX without a third host-side process. When native mounts land, the CLI grows a second launch backend that spawns `omnifsd` directly; the daemon and protocol are identical in both modes. |
| 2 | Transport | HTTP + JSON, `/v1/` path prefix. Docker mode: TCP loopback on the published port (7878). Native mode: Unix socket (or loopback TCP). `--listen` accepts both. | Curl-able, versionable, axum is a cheap dependency. Unix sockets cannot cross the Docker Desktop VM boundary (established by the inspector work), so loopback TCP is the container transport; UDS is the natural native transport. |
| 3 | API auth | None; trust the local channel | Single-user dev machine posture, same as the existing inspector port. Made tenable by #4: no secret material ever rides the API. See Security posture. |
| 4 | Mount delivery | Specs over the API, secrets as files. `omnifs up` starts an empty daemon and POSTs each resolved mount spec; secret material is delivered only through the session secrets directory (bind-mounted in docker mode, a plain directory path in native mode). Specs reference secrets by `CredentialKey::storage_key()` filename. | One control path for cold start and hot add; secrets stay off the unauthenticated wire; the daemon keeps the documented invariant of consuming credential *files* and never reading the host store. The mounts-dir bind and daemon-side config scanning are deleted. |
| 5 | Hot mounts | In scope: `POST /v1/mounts` and `DELETE /v1/mounts/{name}` take effect on the running daemon | The headline user-visible win: `omnifs init` while running, no restart. |
| 6 | Image contents | The runtime image ships both `omnifsd` (entrypoint) and the Linux `omnifs` CLI | `omnifs status` inside `omnifs shell` is a documented workflow. The slimmed CLI no longer links wasmtime/fuser, so the image cost is small. |
| 7 | Inspector | Folds into the HTTP server as a streaming `GET /v1/events` endpoint on the same listener | One listener, one server; the raw-TCP custom protocol goes away. `InspectorEvent` schema and the newline-framed JSON wire format are unchanged. |
| 8 | Cleanup license | Delete rather than deprecate: `omnifs daemon` subcommand, `runtime_state.json`, `debug install-dev-mounts`, entrypoint mount/symlink choreography, the mounts-dir bind, exec-based status. Move code and fix call sites; no bridge layers. | Alpha. CLI and image ship as a version-coupled pair, so there is no skew population to protect. |

## Binaries and crates

**`crates/omnifs-daemon`** (new, binary `omnifsd`): the daemon main. Absorbs the body of `commands/daemon.rs` — paths, `GitCloner`, `ProviderRegistry`, FUSE `run_blocking` — plus the HTTP server and the mount manager (hot add/remove). Depends on `omnifs-host`, `omnifs-fuse`, axum. Linux is the only supported target today (FUSE); the crate must keep building without container assumptions so the native NFSv4 mode is a frontend addition, not a port.

Flags: `--mount-point`, `--cache-dir`, `--providers-dir`, `--secrets-dir`, `--listen <addr|unix:path>`, `--root-symlinks` (container-image nicety: maintain `/github → /omnifs/github` links as mounts come and go; off by default, passed by the entrypoint, meaningless and absent in native mode).

**`crates/omnifs-api`** (new, library): serde DTOs shared by CLI and daemon — `DaemonStatus`/`VersionInfo`/`ReadyInfo` and (phase 3) the mount-create request body. No business logic. The CLI's `StatusJson` stays in the CLI: it is the *merged* host+daemon presentation for `omnifs status --json`, not a wire type the daemon ever sees.

**`crates/omnifs-cli`**: drops `omnifs-host` and `omnifs-fuse` (and with them wasmtime and fuser). Keeps bollard. The daemon lifecycle sits behind a small backend seam — launch, terminate, control endpoint, secrets-directory path — with Docker as the only implementation now and a native spawn backend later. Keep the seam exactly that small; it is not a plugin system.

**Crate boundary move**: the CLI's only remaining `omnifs-host` imports are `mounts::{Spec, Resolved, Catalog}` + builtins, `config::validate_config`, and `auth::oauth_request_from_config`. The `mounts` module depends only on `omnifs-core` and `omnifs-mount-schema` (verified), so it moves into `omnifs-mount-schema`; the config validator and the oauth-request builder move to the layer of their consumers (mount-schema and `omnifs-auth`). Call sites are fixed in the same change; `omnifs-host` does not re-export the moved items.

## Daemon topology and startup

`omnifsd` starts with an **empty registry**: it brings up the filesystem frontend (FUSE today) on `--mount-point` and the HTTP listener, then waits for mounts. `GET /v1/ready` reports true once both are up. Mounts arrive exclusively via `POST /v1/mounts`; there is no mounts-dir scan.

**Docker mode** (the only shipped mode for now): the entrypoint shrinks to directory creation, the log tee, and `exec omnifsd --root-symlinks --listen 0.0.0.0:7878 ...`. Container binds shrink to exactly:

- the session **secrets directory** (read-write: it carries static token files *and* the OAuth `credentials.json` the daemon refreshes in place) — the only omnifs-owned bind;
- environmental passthroughs that are not omnifs surface: `SSH_AUTH_SOCK`, `/var/run/docker.sock` (optional), user preopened paths, dev-flow extras.

**Native mode** (future, with NFSv4/FSKit): the CLI spawns `omnifsd` directly, pointing `--secrets-dir` at the same session directory it would otherwise have bind-mounted, and `--listen` at a Unix socket. Nothing else changes: same API, same secrets contract, same lifecycle verbs. The FUSE frontend is swapped for an NFSv4 server behind the same registry; that frontend is a separate design when it lands.

A bare `docker run` of the image yields an empty filesystem until a CLI pushes mounts. This is deliberate; `omnifs dev` pushes the built-in dev mount specs through the API, and the entrypoint's `install-dev-mounts` step is deleted.

## Control API

All endpoints under `/v1`, JSON bodies. The API carries **no secret material in either direction** — specs reference secrets by storage-key filename; status redacts as today.

| Endpoint | Purpose |
|---|---|
| `GET /v1/ready` | Readiness gate for `omnifs up` (replaces exec-probing `/omnifs`). |
| `GET /v1/version` | Daemon semver + API version; CLI compatibility check. |
| `GET /v1/status` | Typed status payload (replaces `docker exec omnifs omnifs status` and `runtime_state.json`). |
| `GET /v1/mounts` | List active mounts. |
| `POST /v1/mounts` | Create a mount from a resolved spec. Credential references are storage-key filenames resolved against `--secrets-dir`. |
| `DELETE /v1/mounts/{name}` | Remove a mount live. |
| `GET /v1/events` | Inspector stream: chunked newline-framed JSON records, the existing `omnifs-inspector` wire format. Replaces the raw TCP listener. |

`runtime_state.json` is deleted once `status` is API-backed. `omnifs status` on the host queries the daemon endpoint when one is running and falls back to its config-only offline view otherwise; inside the container the shipped CLI hits the local listener.

## Mount lifecycle after the split

`omnifs up`:

1. Resolve enabled mounts and credentials from the host store (unchanged).
2. Materialize the session secrets directory: static token files named by `CredentialKey::storage_key()`, plus the session `credentials.json` for OAuth entries.
3. Launch the daemon via the backend (docker mode: create/start the container with the secrets bind).
4. Poll `GET /v1/ready`.
5. `POST /v1/mounts` for each resolved spec.
6. Gate on `GET /v1/status`.

Failure at any step tears the daemon down and removes the session secrets directory, preserving the cleanup contract from `mount-lifecycle.md`.

`omnifs down`:

1. Sync refreshed OAuth entries from the session `credentials.json` back into the host store (unchanged mechanism, now the *only* credential return path).
2. Terminate the daemon via the backend; remove the session secrets directory.

`omnifs init` / `omnifs mounts rm` while the daemon is running: do the host-side work (config write, OAuth flow, store update) as today, then apply live — write any new secret files into the live session secrets directory (file creation propagates through the directory bind), then `POST /v1/mounts` / `DELETE /v1/mounts/{name}`. When no daemon is running they degrade to config-only, exactly as today.

**Restart-required detection (docker mode only).** Docker cannot add bind mounts to a running container. A hot-added mount that needs a new host bind (a `db` provider pointing at a host SQLite file, the docker provider when the socket was not bound at `up`) cannot be applied live; the CLI detects required-but-absent binds by inspecting the running container and reports "restart required: run `omnifs up`". Native mode has no such limitation — this check lives in the docker backend, not in the protocol.

## Registry dynamics (hot mounts)

`ProviderRegistry` becomes a concurrent map of `Arc<Runtime>` behind interior mutability (the FUSE `Frontend` already holds the registry behind an `Arc` and resolves per-path by mount name):

- **Add**: resolve the spec against provider WASM metadata (daemon-side, providers dir), instantiate the `Runtime`, insert, spawn that mount's timer task. Timer tasks move from one bulk `start_timers` to per-mount spawn/abort so add and remove are symmetric.
- **Remove**: abort the timer task, call `runtime.shutdown()`, drop the entry. FUSE inodes belonging to the removed mount answer `ENOENT`; the kernel root listing is invalidated through the existing notifier. View-cache entries for the mount are dropped; durable object-cache entries are left to capacity eviction (mount-prefixed, harmless, and a re-added mount benefits from them).
- The frontend's root listing derives from the live registry, so mounts appear and disappear in `ls /omnifs` without remount. This registry contract is frontend-agnostic and carries over to the future NFSv4 frontend unchanged.

## Credentials

Custody is unchanged: the host owns the durable store (file or keychain); interactive OAuth (browser, device flow) stays in the CLI; the daemon never reads the host store and consumes only the session secrets directory. The in-daemon 401-triggered refresh path (`omnifs-host/src/auth.rs`) keeps operating on the session `credentials.json`, refreshing in place under the existing file lock; `omnifs down` syncs it back. The only change from today is that mount *specs* no longer travel as files next to the secrets — the secrets directory is the entire file surface.

## Security posture

The control API is unauthenticated local HTTP. Because of decision #4 it is **control-plane only**: a local process that reaches it can mutate mounts, read status, and subscribe to events, but cannot read or inject credential material through it (secrets live in the session directory with restrictive permissions, same as today). This matches the single-user-dev-machine posture of the existing inspector port.

Guardrails:

- Docker mode publishes the port on `127.0.0.1` only, never `0.0.0.0`, on the host side. Native mode prefers a Unix socket, which restores same-user enforcement for free.
- API request/response logging must not include bodies; the existing `log_redaction` discipline extends to the HTTP layer.
- If a stronger posture is ever needed (shared hosts), the pre-designed mitigation is a per-session bearer token delivered as a file in the secrets directory and enforced by one axum middleware.

## Versioning and compatibility

The image label handshake (`ai.0xff.omnifs.min-launcher-version`) stays as the pre-start gate against old CLIs launching new images. Post-start, `GET /v1/version` returns `{ version, apiVersion }`; the CLI refuses to manage a daemon with an incompatible `apiVersion`. CLI and image ship as a version-coupled pair per release (same unprefixed semver), so these checks guard accidental skew, not supported divergence — which is also why no compatibility shims are kept for the deleted surfaces.

## Packaging and release ramifications

- `omnifsd` is an image-only artifact for now: built for Linux x64/arm64 in the existing native CI lanes, staged into the runtime image by `scripts/ci/build-runtime-image.sh` alongside the Linux CLI and the provider WASM components. The contributor `Dockerfile` builds both binaries. When native mode ships, `omnifsd` joins the platform artifact set; nothing in this design needs to change for that beyond distribution.
- npm is untouched: the published packages ship the (now much smaller) CLI binary only. `npm/platforms.json` does not change.
- `cargo nextest run` and `just check` package globs pick up the new crates; no hand-maintained lists.

## Phasing

Each phase lands independently and keeps `omnifs dev` plus the smoke harness green. Deletions happen in the phase that obsoletes them, not in a cleanup epilogue.

1. **Binary split.** Create `crates/omnifs-daemon` from `commands/daemon.rs`; move the `mounts` module and friends down to `omnifs-mount-schema`; entrypoint execs `omnifsd`; Dockerfile ships both binaries; CLI drops `omnifs-host`/`omnifs-fuse`; the `omnifs daemon` subcommand is deleted. No behavior change.
2. **Control plane.** Axum server in `omnifsd`; `ready`/`version`/`status`/`events`; inspector TCP listener replaced by `GET /v1/events`; CLI switches the `up` readiness gate, `status`, and `inspect` from exec/state-file/raw-TCP to the API; `runtime_state.json` deleted.
3. **API mount delivery.** `up` pushes specs via `POST /v1/mounts`; the mounts-dir bind and daemon-side config scanning are deleted; the secrets directory becomes the only omnifs-owned bind; entrypoint dev-mount install replaced by `omnifs dev` pushing specs; `debug install-dev-mounts` deleted.
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
