# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, REST API, mount delivery, runtime modes, workspace layout, dev home, and generated API shape.

## Read when

Read this before touching `omnifs-cli`, `omnifs-daemon`, `omnifs-api`, lifecycle commands, daemon status, REST routes, mount delivery, desired-state application, the optional Docker-hosted FUSE frontend, provider bundle installation, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes REST API 7.0, whose schema lives in `omnifs-api` and whose checked-in OpenAPI document is generated from the daemon implementation. API 7 removes mount mutation, explicit reconcile, hot-reconcile reporting, and failed-mount projections because desired state now crosses the daemon boundary only as an immutable startup revision. `FrontendInfo.mount_point` remains authoritative for that frontend, and credential material is never transmitted on the wire.

A host-native daemon serves the control API over a Unix domain socket at `$OMNIFS_HOME/control.sock`. Auth on that socket is filesystem permissions: the config directory is forced to `0700` and the socket to `0600`, so only the owning user can connect, and there is no bearer token on that path. The bearer-token middleware survives but is enforced only on a TCP listener, which the daemon binds only for the `OMNIFS_DAEMON_ADDR` debug/test path (there is no other TCP control listener). There, the token lives in daemon memory only (no token file); its value comes from `OMNIFS_CONTROL_TOKEN` when the caller injects one, else is generated per start.

`$OMNIFS_HOME/daemon.json` is the single daemon-owned runtime record, replacing both `launch.json` and the `control-token` file. It records the endpoint to dial (`{ "kind": "unix", "path": ... }`), the backend identity (the native process id), a per-start `instance_id`, the exact mount revision loaded by that daemon, the serving frontends, `started_at`, and any token-authenticated attach listeners. It is written mode `0600` because attach records carry tokens. The daemon publishes it only after the immutable namespace and every fixed, requested, or restored listener are ready, and removes it on graceful exit. A crash may leave it stale; a replacement validates and restores current persisted attach authority from that stale record, including its token, before publishing a new instance record. When the record says a daemon should exist but the control probe fails, inventory reports the daemon as unreachable and status exits 3; a cleanly absent daemon is stopped. Teardown (`omnifs down`) removes the record after liveness-checking its pid before trusting it for a stale sweep.

The CLI resolves which endpoint to dial in one order: `OMNIFS_DAEMON_ADDR` when set (TCP, bearer token from `OMNIFS_CONTROL_TOKEN`), else the workspace's `daemon.json` (unix socket), else the fixed `$OMNIFS_HOME/control.sock` when present, else the daemon is stopped. It never dials a default control port blind. An unreadable runtime record is stale metadata and permits only the fixed-socket fallback. The CLI asserts the `instance_id` echoed by `/v1/status` against the record's, so a record overwritten by a restart mid-command is caught. Because it only dials an endpoint from its own workspace's record, fixed socket, or explicit debug override, a daemon owned by a different `OMNIFS_HOME` is structurally unaddressable.

`GET /v1/ready` is the only unauthenticated control route on the TCP listener. Every other TCP route, including `/v1/events`, snapshot export routes, and future routes, is authenticated by default through the daemon router middleware. Missing or wrong bearer tokens fail closed with HTTP 401 and an `ApiError` whose code is `Unauthorized`. The Unix-socket listener omits the middleware entirely.

The control API may expose operational state that contains no secrets. `GET /v1/credentials` reports registered credential ids, coarse health, expiry, and scopes only; it never reports access tokens, refresh tokens, client secrets, or header material. `POST /v1/credentials/{id}/reload` reloads a registered credential from the host store and returns the same non-secret status shape. `GET /v1/providers` reports installed artifacts grouped by provider name. Provider state is derived from exact mount pins, never install recency.

Mount wire payloads distinguish provider identity from provider naming. `provider_name` is the human/catalog slug used by credentials and UX. `provider_id` is the pinned provider content hash for the exact artifact the mount runs.

### Mount delivery

Only `$OMNIFS_HOME/mounts` is a local Git repository. Its `HEAD` is desired mount state and `refs/omnifs/applied` is the last revision that reached daemon readiness. Credentials, provider artifacts, cache data, sockets, logs, and runtime records remain outside Git. First use initializes the repository without a remote and commits existing valid specs with a stable Omnifs-local author, so behavior never depends on the operator's Git configuration.

Specs are one file per mount, and a spec file's stem is its mount name. `mounts::Registry` remains the sole owner of parsing, naming, and atomic file writes, while `mounts::Repository` owns Git revisions, snapshots, and the shared advisory lock around an official write or apply operation. Mount-writing commands record the resulting desired-state commit. `mount add` is create-only, and `mount rm` changes only Git-backed desired state because rollback can restore its credential reference; credential revocation is explicit through `omnifs mount revoke <name>`. Setup may write several specs before recording one coherent commit at its launch boundary.

`omnifs up` is the sole apply implementation, and visible `omnifs apply` is an alias of that exact clap command, args type, handler, telemetry label, and receipt. It may commit valid manual `*.json` edits before application. It rejects malformed specs, unexpected tracked paths, missing provider artifacts, insufficient grants, and unusable credentials before stopping a healthy daemon.

The CLI materializes `HEAD` under cache storage and starts the daemon with that immutable snapshot plus its exact revision. The daemon never invokes Git or chooses desired state. If a healthy daemon already records `HEAD`, `up` leaves it running. Otherwise it stops only the daemon, starts the new revision, waits for readiness, and then advances `refs/omnifs/applied`; a failed start never advances the ref. `up` and its exact `apply` alias never launch, stop, or reconcile a frontend. Existing frontend runners survive daemon replacement and reconnect through the fixed local socket or the restored TCP/vsock listener authority and token. `down` also stops only the daemon; runner teardown belongs to `omnifs frontend disable`.

Mount specs strict-parse their top-level JSON fields. Unknown top-level keys are invalid, while the provider-owned `config` object remains opaque to the host. The control API has no mount mutation or reconcile routes; it retains read-only inspection/export plus credential, frontend, status, event, and shutdown surfaces.

Prefer REST API extensions for new non-secret interactions. Keep credential material off the REST API.

### Replica snapshots

`omnifs mount snapshot <mount> --out <dir>` exports a configured mount's canonical
object store as a plain directory tree plus `index.json`. When a compatible
daemon is running, the CLI reads the snapshot from `GET
/v1/mounts/{name}/export` as an `application/x-tar` stream. When no compatible
daemon answers, the CLI reads `<cache>/object` directly and writes the same
directory layout. Both paths export canonical bytes and metadata only; no
credentials are transmitted.

The top-level `index.json` is generated snapshot metadata, so the entire
canonical path namespace rooted at `/index.json` is reserved and rejected
before directory or tar export.

The snapshot tree is the audit surface for replicas. Compare rendered canonical
files with `diff -r --exclude=index.json <before> <after>`; `index.json` records
logical id, path, blake3, and size for each file and therefore changes whenever
file bytes change. Use `scripts/demo/snapshot-diff.sh` for the supported demo
flow.

### Runtime modes

There is one daemon runtime: host-native. `omnifs up` always spawns a host-native child process; there is no Docker daemon runtime and no `--runtime`/`[system].runtime` choice to make. The daemon is a pure namespace server; it never mounts a frontend in-process.

Docker's only role in the runtime is delivering an optional FUSE frontend (`omnifs frontend enable fuse --environment docker`): a separate, credential-free container attached to the host-native daemon's shared namespace over TCP. It is never how the daemon itself runs, and `DaemonBackend`/`RecordedBackend` carry no Docker variant.

The daemon is an attach registry over one shared namespace. Frontends are separate slim runner processes, each exposing every mount, and the CLI owns their lifecycle. `DaemonStatus.frontends` reports the deterministic live attachment set. Public presentation orders by environment, filesystem, and location; low-level API fields retain delivery, filesystem type, and per-frontend mount point.

`omnifs setup` has no runtime stage: the daemon is always host-native. It may offer the platform defaults once, start the daemon through the same operation as `up`, and then invoke the same imperative enable operation as `omnifs frontend enable`. Linux offers host FUSE; macOS offers host NFS plus Docker FUSE. Setup never persists those choices, and its Docker reachability row appears only when the selected one-time actions include Docker.

Frontend identity uses `filesystem` (`fuse` or `nfs`), `environment` (`host`, `docker`, or `krunkit`), and an optional host-only absolute `location`. Docker and krunkit deliver FUSE only, NFS is host-only, and host FUSE is Linux-only. A missing host location resolves from `OMNIFS_MOUNT_POINT` or the existing platform location helper. Guest environments own their mount location. There is no persistent frontend desired state and no default/effective-plan normalization.

`DaemonStatus.backend` is the daemon-reported backend fact, not a config echo: it reports the daemon's own process id. `daemon.json` is only a cache of that daemon-reported fact for stale teardown.

Keep frontend-specific Docker policy (image resolution, container naming, the no-credentials contract) in the frontend command paths (`crates/omnifs-cli/src/commands/frontend/`, `crates/omnifs-cli/src/frontend_container.rs`); the daemon launch path has no Docker policy to keep aligned with it.

### Namespace attach sockets and out-of-process frontends

The daemon always serves its shared namespace over `$OMNIFS_HOME/frontends/local.sock` for local frontend runners. The `frontends/` directory is forced to `0700` and the socket to `0600`; filesystem permissions are its authentication. A refused stale socket is removed before binding, while any ambiguous probe error fails closed. The socket is removed on graceful exit. There is no named attach-socket flag. The Omnifs VFS wire protocol uses length-delimited postcard framing from `omnifs-vfs-wire`, not an RPC framework. It transports the engine-owned `Namespace` surface and does not own projection semantics.

`/v1/ready` reports ready only after the immutable mount revision loads completely and every fixed, requested, or restored namespace listener has bound and remains supervised. Listener readiness does not require a frontend to be attached. `DaemonStatus.frontends` is the authoritative location set; API 7 has no singular daemon mount-point projection or failed-mount collection.

The separate `omnifs-thin` binary runs either `fuse` or `nfs`, attaches a VFS-wire-backed namespace, and serves one frontend location until teardown. It runs no provider and its normal dependency graph excludes Wasmtime, the provider bundle, and the daemon control plane. `omnifs-thin fuse --attach <socket> --mount-point <path>` is the Docker image and krunkit guest entrypoint; with `--attach` absent, `OMNIFS_ATTACH_ADDR` and `OMNIFS_ATTACH_TOKEN` select TCP or vsock. `omnifs-thin nfs --attach <socket> --mount-point <path>` additionally persists filehandle identity and mount discovery state so a restarted runner can resume an active kernel mount. Both modes consume attach resolution, reconnect, and readiness signaling from `omnifs-vfs-wire`.

`omnifs frontend enable FILESYSTEM --environment ENVIRONMENT [--location PATH]` starts or confirms one whole-namespace runner. `disable` stops one exact observed runner, `restart` restarts one observed selector or every observed runner, and `ls` reports the Inventory observation join. Host selectors may need `--location` to disambiguate; Docker and krunkit reject it because their guest location is environment-owned. These commands persist no desired frontend configuration and are not daemon runtime modes. The Docker path binds the TCP attach listener via `POST /v1/frontend/attach-target`, resolves the frontend image by build-channel provenance (`ghcr.io/0xff-ai/omnifs-frontend:<version>` for a release binary, `omnifs-frontend:dev` for a dev binary, `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override), and runs the container with no binds, no `OMNIFS_HOME`, no docker.sock, no SSH agent, and no published ports: only `OMNIFS_ATTACH_ADDR`/`OMNIFS_ATTACH_TOKEN` cross into it. A fail-closed check right after start asserts an empty `Mounts` array and an env set of exactly those two vars plus the image's own defaults, killing the container on violation. One frontend container runs per workspace: `omnifs-frontend` for the default home, `omnifs-frontend-<hash8(home)>` otherwise. Runtime-record frontend entries are live attach-registry snapshots with a required low-level `via` delivery value.

The krunkit environment (macOS only) mirrors that build-channel provenance for its guest disk image instead of a container image: a release binary resolves `ghcr.io/0xff-ai/omnifs-guest:<version>` and pulls it into the workspace cache on first use; a dev binary resolves the local `target/guest-image/omnifs-guest.raw` and never downloads. `[frontend] guest_image`/`OMNIFS_GUEST_IMAGE` override either default the same way `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override the Docker image.

The Omnifs VFS wire protocol also serves over token-authenticated TCP because a containerized frontend cannot share the host Unix socket. Docker Desktop forwards `host.docker.internal` to host loopback, so the default `POST /v1/frontend/attach-target` request binds `127.0.0.1` on an ephemeral port. Native Linux maps that name to the default Docker bridge gateway instead, so Docker frontend enablement discovers that gateway and asks the daemon to bind only that address; the daemon verifies that the address is assigned to `docker0`. The route must not bind `0.0.0.0` or use host networking. The current per-instance attach token is 32 lowercase hex chars. A replacement daemon restores the same validated TCP address and token, while generating a new `instance_id`; a restored listener failure aborts readiness and publication. New targets generate a token when first requested. The route is idempotent while its supervised listener is alive, and removes a dead target from the runtime record so a later request can bind again. Wire protocol v3 carries the connecting frontend's identity (kind plus guest-side mount point, display-only) in the handshake alongside the token; a current client or wrong/missing token is rejected with a named reason. The daemon labels attachments by listener ownership (`docker` for TCP, `krunkit` for the vsock-proxy UDS listener, and `local` for `frontends/local.sock`), never by a guest claim. `GET /v1/status` reports these live attachments. The fixed local listener uses filesystem permissions instead of a token; the UDS listener bound by `POST /v1/frontend/attach-target/vsock` requires the token, like TCP.

### Dev home

`scripts/dev.ts` owns contributor dev state. It renders a dedicated `~/.omnifs-dev` home, builds the CLI with its provider bundle, starts the host-native daemon through `omnifs up`, then imperatively enables the credential-free host and Docker frontends and opens the developer at `/omnifs` inside the Docker frontend. Host CLI commands use the normal workspace resolution unless `OMNIFS_HOME` is explicit; do not reintroduce a Rust-side dev command or dev-session owner.

### Provider bundles

`just build providers` emits the content-addressed provider-store bundle at `target/omnifs-provider-store`. `scripts/dev.ts` embeds it into the natively-built `omnifs` CLI/daemon binary via `OMNIFS_PROVIDER_BUNDLE_DIR` and copies it into the dev provider store for runtime mount pinning; it must not compile providers again. The frontend image (`Dockerfile`'s `frontend-dev`/`frontend-release` stages, built from or injected into `thin-builder`) needs no provider-store build context at all: it runs `omnifs-thin fuse`, which contains no engine runtime and no provider bundle. Release CLI binaries embed the provider bundle and unpack it into `OMNIFS_HOME/providers`.

Provider-store indexes strict-parse both the top-level index object and retained provider entries. Unknown keys make the store unreadable instead of being silently accepted.

### Agent contract: output, inventory, and exits

The CLI is a machine surface, not only a human one. `crates/omnifs-cli/src/error.rs` owns the exit-code enum and the stable error identities; the receipts live in `crates/omnifs-cli/src/commands/receipt.rs`.

Exit codes are the API. Every code is modeled in `error::ExitCode`, and clap parse/usage errors are mapped once at the `main` parse boundary (never per command):

| code | meaning |
|---|---|
| 0 | success |
| 1 | generic failure |
| 2 | usage error (clap) |
| 3 | daemon unreachable |
| 4 | auth or consent required |
| 5 | degraded health |
| 130 | canceled (declined prompt or Ctrl-C, 128 + SIGINT) |

Every top-level error carries a stable slug derived from its exit class, not its wording (`generic-failure`, `usage`, `daemon-unavailable`, `auth-required`, `degraded`, `canceled`). The human error block shows it dim as a trailing `(id: <slug>)`; structured modes emit the same slug in the terminal error envelope.

One global `--output human|json|jsonl` owns the invocation. Human mode renders a compact workspace context strip followed by responsive resource tables on stdout while narration and progress use stderr. Tables use sentence-case headers, soft alignment without borders or rules, explicit state text, and contextual recovery lines; below 72 columns the same typed fields stack beneath each resource identity. JSON emits exactly one result or error envelope on stdout and suppresses progress. JSONL emits zero or more event lines followed by exactly one terminal result or error line. Structured modes never prompt unless required answers were supplied by flags. `--no-input` forbids prompts and browser handoffs, `--yes` approves confirmation-only decisions, and `--quiet` suppresses optional narration without hiding tables, receipts, or errors.

Every JSON result uses one envelope:

```json
{
  "schema_version": 1,
  "command": "status",
  "verdict": "ok",
  "result": {
    "workspace": {},
    "frontends": [],
    "mounts": [],
    "providers": []
  }
}
```

Inventory and receipt models are typed and sorted before both rendering and serialization. Status owns plural `frontends`, `mounts`, and `providers`; focused mutation receipts add plural `access_paths` where relevant. Frontends always carry `scope: "all"` and a mount count because they expose the complete namespace. Provider state is derived from exact reverse pins as `pinned`, `installed`, or `missing`; `up` applies the committed pin exactly and never chooses a replacement artifact.

Observation commands exit 0 when collection succeeds and every resource is positive or neutral, including a deliberately stopped daemon, observed runners waiting to reconnect, offline mounts while stopped, unpinned installed artifacts, and unnecessary auth. Inventory never adds a row for an unlaunched default. A complete inventory with an actionable or failed row exits 5. When a runtime record says the daemon should be live but its control probe is unavailable, status emits the trustworthy degraded inventory and exits 3. Human, JSON, and JSONL derive the same resource verdict; the exit mapper applies the unreachable override.

`down` stops the daemon only and leaves frontend runners alive. It treats shutdown as complete only
after the acknowledged shutdown request's control surface becomes unavailable;
the CLI polls that surface with a bounded timeout and reports a failed teardown
row if the daemon remains reachable, so a successful `DaemonStopped` outcome
always means a subsequent status probe can observe `not_running`. `down` never
deletes mount desired state, credentials, provider artifacts, cache or workspace
files, or `$OMNIFS_HOME`; users and uninstallers remove `$OMNIFS_HOME` through
ordinary filesystem operations.

`crates/omnifs-cli/src/ui` owns terminal rendering and stream selection. CLI
modules emit reports, events, JSON values, narration, or already-rendered raw
records through that surface; clippy rejects direct print macros elsewhere.
The only non-UI passthroughs are daemon logs and generated shell completions,
whose destination streams are owned by the invoked tools.

## Must not

- Make any directory above `$OMNIFS_HOME/mounts` a Git repository, or place credentials, provider artifacts, cache data, sockets, logs, or daemon records under mount-version control.
- Add a second spec read or write path that bypasses `mounts::Registry`, or write a spec to a file whose stem is not its mount name.
- Add a second apply command path, args type, receipt, lifecycle branch, or telemetry label for the `apply` spelling.
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
- `crates/omnifs-thin/src/fuse.rs`
- `crates/omnifs-vfs-wire/src/beacon.rs`
- `crates/omnifs-cli/src/commands/frontend/`
- `crates/omnifs-cli/src/frontend_container.rs`
- `crates/omnifs-vfs-wire/src/lib.rs`
- `crates/omnifs-itest/src/live.rs`
- `crates/omnifs-thin/src/nfs.rs`
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
