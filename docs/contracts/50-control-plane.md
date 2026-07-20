# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, typed local control protocol, mount desired state, frontend runtimes, workspace layout, and dev home.

`omnifs_workspace::Workspace` is the central broker for one workspace. It does
not expose the home root, generic directory getters, or path-transfer objects.
CLI and daemon code request behavior-owning components, and a concrete path may
leave a component only at the immediate filesystem, process, protocol, engine,
test-fixture, or final-output boundary that consumes it. Relative path names
and home-root env resolution live in `omnifs_workspace::layout`; only
`Workspace` binds those names to components.

## Read when

Read this before touching `omnifs-cli`, `omnifs-api`, lifecycle commands, daemon status, control operations, mount desired state, revision application, frontend runtimes, the embedded provider bundle, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes one current, versioned JSONL control protocol whose wire types live in `omnifs-api`. It has operation-specific Ready, Status, Shutdown, AttachTcp, AttachVsock, and SubscribeInspector requests, with no remote API, route table, or compatibility layer. Desired state and auth bindings cross the daemon boundary only as an immutable startup revision, `FrontendInfo.mount_point` remains authoritative for that frontend, and credential material is never transmitted on the wire.

A host-native daemon serves the local control protocol over `$OMNIFS_HOME/control.sock`. The workspace directory is forced to `0700` and the socket to `0600`, so filesystem permissions authenticate every control request. The control protocol has no remote TCP listener, bearer-token mode, or environment-selected endpoint.

`$OMNIFS_HOME/daemon.json` is the daemon process-identity record, replacing both `launch.json` and the `control-token` file. It records the endpoint to dial (`{ "kind": "unix", "path": ... }`), the daemon pid, a per-start `instance_id`, the exact mount revision loaded by that daemon, and `started_at`; `$OMNIFS_HOME/frontends/targets.json` is the separate workspace-owned store for durable TCP and vsock targets. It uses a strict current schema, atomic `0600` writes, one target per transport, and remains after graceful daemon shutdown so surviving runners can reconnect. The daemon publishes `daemon.json` only after the immutable namespace and every fixed, requested, or restored listener are ready, and removes it on graceful exit. A crash may leave it stale, while a replacement restores targets from `targets.json` before publishing a new instance record. Live frontend rows come from `VfsServer`, not from either file. When the record says a daemon should exist but the control probe fails, inventory reports the daemon as unreachable and status exits 3; a cleanly absent daemon is stopped. Teardown (`omnifs down`) removes the process record after liveness-checking its pid, but preserves `targets.json`.

The CLI resolves the local control socket from its own workspace's `daemon.json` when present, else from the fixed `$OMNIFS_HOME/control.sock`, else the daemon is stopped. It never selects a remote endpoint or accepts a control endpoint override. An unreadable daemon record is stale metadata and permits only the fixed-socket fallback. The CLI asserts the `instance_id` echoed by the typed Status reply against the record's, so a record overwritten by a restart mid-command is caught.

The control protocol is local-only over `$OMNIFS_HOME/control.sock`; its workspace directory and socket permissions authenticate requests. VFS TCP and vsock attachment listeners are separate frontend transports and are not control-protocol transport.

The control protocol exposes only operational state that contains no secrets. Mount health is reported with each `MountInfo`; the daemon has no credential enumeration or reload surface. Credential import and `mount reauth` write the host store and take effect on the next `omnifs up` or `omnifs apply`, while OAuth refresh remains live only for an already-bound mount. Provider state is derived from exact mount pins, never install recency.

Mount wire payloads distinguish provider identity from provider naming. `provider_name` is the human/catalog slug used by credentials and UX. `provider_id` is the pinned provider content hash for the exact artifact the mount runs.

### Mount desired state

Only `$OMNIFS_HOME/mounts` is a local Git repository. Its `HEAD` is desired mount state and `refs/omnifs/applied` is the last revision that reached daemon readiness. Credentials, provider artifacts, cache data, sockets, logs, and runtime records remain outside Git. First use initializes the repository without a remote and commits existing valid specs with a stable Omnifs-local author, so behavior never depends on the operator's Git configuration.

Specs are one file per mount, and a spec file's stem is its mount name. `mounts::Registry` remains the sole owner of parsing, naming, and atomic file writes, while `mounts::Repository` owns Git revisions, snapshots, and the shared advisory lock around an official write or apply operation. Explicit mount-writing commands record the resulting desired-state commit. `mount add` is create-only, and `mount rm` changes only Git-backed desired state because rollback can restore its credential reference; credential revocation is explicit through `omnifs mount revoke <name>`.

`omnifs mount revoke <name>` resolves the exact credential selected by the mount and names every configured mount sharing it before confirmation. A present OAuth credential is revoked upstream first when the pinned scheme supports revocation, and any upstream failure leaves the local credential intact for retry. Static credentials and OAuth schemes without a revocation endpoint delete locally. An already-absent credential is a successful no-op that needs no confirmation. The running daemon is never changed; successful deletion applies on the next `omnifs up` or `omnifs apply`.

`omnifs up` is the sole apply implementation, and visible `omnifs apply` is an alias of that exact clap command, args type, handler, usage-metrics label, and receipt. Online `up` may commit valid manual `*.json` edits before application, then rejects malformed specs, unexpected tracked paths, missing provider artifacts, insufficient grants, and unusable credentials before stopping a healthy daemon. `omnifs up --offline` observes the existing committed `HEAD` and snapshots that exact revision without committing dirty specs; it skips provider, credential, network, and runtime startup checks, serves only validated durable projection facts, never advances `refs/omnifs/applied`, and restarts a daemon when its online/offline mode differs. When replacing a responding daemon, it first asks that daemon to validate the exact snapshot against its open cache; a failed validation leaves the current daemon, sockets, and applied ref untouched.

The CLI materializes `HEAD` under cache storage and starts the daemon with that immutable snapshot plus its exact revision. The daemon never invokes Git or chooses desired state. If a healthy daemon already records `HEAD`, `up` leaves it running. Otherwise it stops only the daemon, starts the new revision, waits for readiness, and then advances `refs/omnifs/applied`; a failed start never advances the ref. `up` and its exact `apply` alias never launch, stop, or reconcile a frontend. Existing frontend runners survive daemon replacement and reconnect through the fixed local socket or the restored TCP/vsock listener authority. `down` also stops only the daemon; runner teardown belongs to `omnifs frontend disable`.

`omnifs setup [--providers NAME] [--no-up] [--no-browser]` is the thin first-run composition over the existing mount configuration, daemon start, frontend enable, and Inventory operations. Provider names are exact embedded names; `--yes` selects only providers whose existing resolver policy proves safe without required config or a fresh credential flow, while `--no-input` requires explicit providers or `--yes`. With `--no-up`, setup configures mounts only. It emits no setup receipt or frontend desired state, and structured output ends with the single terminal Inventory result after all selected operations.

Host-owned mount objects, including `auth` and `limits`, strict-parse their fields. Unknown top-level or nested host-owned keys are invalid, while the provider-owned `config` object remains opaque to the host. The control protocol has no mount mutation or reconcile operation; it retains typed frontend, status, event, readiness, attach, and shutdown surfaces.

Add an operation only when it has one owning domain fact and a focused typed reply. Keep credential material off the control protocol.

### Runtime modes

There is one daemon runtime: host-native. `omnifs up` always spawns a host-native child process; there is no Docker daemon runtime and no `--runtime`/`[system].runtime` choice to make. The daemon is a pure namespace server; it never mounts a frontend in-process.

Docker's only role in the runtime is serving an optional FUSE frontend (`omnifs frontend enable fuse --runtime docker`): a separate, credential-free container attached to the host-native daemon's shared namespace over TCP. It is never how the daemon itself runs.

The daemon serves one shared namespace through `omnifs_vfs_wire::VfsServer`. `VfsServer` owns listeners, listener and connection tasks, readiness, and the deduplicated live attachment set; the daemon owns namespace construction, typed local control, durable TCP/vsock target records, and process lifetime. Frontends are separate slim runner processes, each exposing every mount, and the CLI owns their lifecycle. `DaemonStatus.frontends` reports the deterministic live attachment set. Public presentation orders by runtime, filesystem, and location; low-level fields retain runtime, filesystem type, transport, and per-frontend mount point.

Frontend defaults are never persisted. After configuring mounts and starting the daemon, `omnifs setup` offers every filesystem/runtime pair supported on the current OS and enables whichever the user selects (or, under `--yes`/`--no-input`/a non-interactive run, whichever are pre-checked) as imperative actions; there is no separate hardcoded setup default list. The pre-checked set is the same deterministic runtime `omnifs frontend enable FILESYSTEM` resolves when `--runtime` is absent: FUSE uses libkrun on macOS and host on Linux, while NFS uses host. Docker remains explicit, never pre-checked. Separate operations remain available through `omnifs up` and `omnifs frontend enable`; Inventory reports only observed frontends.

Frontend identity uses `filesystem` (`fuse` or `nfs`), `runtime` (`host`, `docker`, or `libkrun`), and an optional host-only absolute `location`. Docker and libkrun deliver FUSE only, NFS is host-only, and host FUSE is Linux-only. A missing host location resolves from `OMNIFS_MOUNT_POINT` or the existing platform location helper. Guest runtimes own their mount location. There is no persistent frontend desired state and no default/effective-plan normalization.

`daemon.json` records process identity for control-plane readiness and stale teardown. `frontends/targets.json` owns the durable attach authority needed for runner reconnect; live frontend runtime and transport facts come from listener ownership and the `VfsServer` attachment observation.

Keep frontend-specific Docker policy (image resolution, container naming, the no-credentials contract) in the frontend command paths (`crates/omnifs-cli/src/commands/frontend/`, `crates/omnifs-cli/src/frontend_container.rs`); the daemon launch path has no Docker policy to keep aligned with it.

### Namespace attach sockets and out-of-process frontends

The daemon always serves its shared namespace over `$OMNIFS_HOME/frontends/local.sock` for local frontend runners. The `frontends/` directory is forced to `0700`, the socket to `0600`, and `targets.json` to `0600`; filesystem permissions authenticate the local socket. A refused stale socket is removed before binding, while any ambiguous probe error fails closed. The socket is removed on graceful exit, while durable TCP/vsock targets remain. There is no named attach-socket flag. The Omnifs VFS wire protocol uses length-delimited postcard framing from `omnifs-vfs-wire`, not an RPC framework. It transports the engine-owned `Namespace` surface and does not own projection semantics.

The `Ready` operation succeeds only after the immutable mount revision loads completely and every fixed, requested, or restored namespace listener has bound and remains supervised. Listener readiness does not require a frontend to be attached. `DaemonStatus.frontends` is the authoritative location set; status has no singular daemon mount-point projection or failed-mount collection.

The separate `omnifs-thin` binary runs either `fuse` or `nfs`, attaches a VFS-wire-backed namespace, and serves one frontend location until teardown. It runs no provider and its normal dependency graph excludes Wasmtime, the provider bundle, and the daemon control plane. `omnifs-thin fuse --attach <socket> --mount-point <path>` is the Docker image and libkrun guest entrypoint; with `--attach` absent, `OMNIFS_ATTACH_ADDR` selects TCP or vsock. `omnifs-thin nfs --attach <socket> --mount-point <path>` additionally persists filehandle identity and mount discovery state so a restarted runner can resume an active kernel mount. Both modes consume attach resolution, reconnect, and readiness signaling from `omnifs-vfs-wire`.

`omnifs frontend enable FILESYSTEM [--runtime RUNTIME] [--location PATH]` starts or confirms one whole-namespace runner. `disable` stops one exact observed runner, `restart` restarts one observed selector or every observed runner, and `ls` reports platform support and prerequisite readiness separately from the Inventory observation join. Host selectors may need `--location` to disambiguate; distinct locations permit multiple host instances of one filesystem/runtime pair. Docker and libkrun reject location because their guest location is runtime-owned, which gives each one instance per workspace. These commands persist no desired frontend configuration and are not daemon runtime modes. The Docker path requests an `AttachTcp` target, resolves the frontend image by build-channel provenance (`ghcr.io/0xff-ai/omnifs-frontend:<version>` for a release binary, `omnifs-frontend:dev` for a dev binary, `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override), and runs the container with no binds, no `OMNIFS_HOME`, no docker.sock, no SSH agent, and no published ports: only `OMNIFS_ATTACH_ADDR` crosses into it. A fail-closed check right after start asserts an empty `Mounts` array and an env set of exactly that var plus the image's own defaults, killing the container on violation. One frontend container runs per workspace: `omnifs-frontend` for the default home, `omnifs-frontend-<hash8(home)>` otherwise. Durable attach targets live in `$OMNIFS_HOME/frontends/targets.json`; live attachment rows come from `VfsServer`. A listener that exits unexpectedly removes only its exact target, while graceful `VfsServer` shutdown preserves targets for the next daemon.

`omnifs frontend shell FILESYSTEM --runtime RUNTIME` enters one exact observed Docker or libkrun frontend and accepts an optional `--shell` plus a trailing command. It never ranks or guesses among frontends. Host frontends are already mounted in the caller's filesystem namespace and are browsed through the caller's ordinary shell.

The libkrun runtime (macOS only) mirrors that build-channel provenance for its guest disk image instead of a container image: a release binary resolves `ghcr.io/0xff-ai/omnifs-guest:<version>` and pulls it into the workspace cache on first use; a dev binary resolves the local `target/guest-image/omnifs-guest.raw` and never downloads. `[frontend] guest_image`/`OMNIFS_GUEST_IMAGE` override either default the same way `[system].frontend_image`/`OMNIFS_FRONTEND_IMAGE` override the Docker image.

The resolved libkrun image is an immutable base. Before each launch, the CLI materializes a workspace-owned `libkrun/root.raw` with mode `0600` and passes that root, never the cached or configured base path, as the first virtio-blk device. Atomic temporary roots and `root.raw` are launch-owned state and are removed on rollback, stale replacement, restart, and disable; the base image is preserved unchanged.

The Omnifs VFS wire protocol also serves over local TCP because a containerized frontend cannot share the host Unix socket. Docker Desktop forwards `host.docker.internal` to host loopback, so the `AttachTcp` operation normally requests `127.0.0.1` on an ephemeral port. Native Linux maps that name to the default Docker bridge gateway instead, so Docker frontend enablement discovers that gateway and asks the daemon to bind only that address; the daemon verifies that the address is assigned to `docker0`. The operation must not bind `0.0.0.0` or use host networking. A replacement daemon restores the same validated TCP address from `frontends/targets.json`, while generating a new daemon-control `instance_id`; that control-plane identifier is separate from the VFS protocol handshake and is never a namespace identity. A restored listener failure aborts readiness and publication while leaving the target for retry. The operation is idempotent while its supervised listener is alive, and removes a dead target from `targets.json` so a later request can bind again. Wire protocol v7 carries the connecting frontend's identity (kind plus guest-side mount point, display-only) in the handshake, engine-issued cache lifetimes, cacheable negative lookup answers, and the terminal `OfflineMiss` namespace error; older clients are rejected outright with a named reason. The daemon labels attachments by listener ownership (`docker` for TCP, `libkrun` for the vsock-proxy UDS listener, and `local` for `frontends/local.sock`), never by a guest claim. `Status` reports these live attachments. The fixed local listener uses filesystem permissions; the `AttachVsock` operation returns the UDS listener for the libkrun vsock-proxy path.

### Dev home

`scripts/dev.ts` owns contributor dev state. It renders a dedicated `~/.omnifs-dev` home, builds the CLI with its provider bundle, starts the host-native daemon through `omnifs up`, then imperatively enables the credential-free host and Docker frontends and opens the developer at `/omnifs` inside the Docker frontend. Host CLI commands use the normal workspace resolution unless `OMNIFS_HOME` is explicit; do not reintroduce a Rust-side dev command or dev-session owner.

### Provider bundles

`just build providers` emits the content-addressed provider-store bundle at `target/omnifs-provider-store`. `scripts/dev.ts` embeds it into the natively-built `omnifs` CLI/daemon binary via `OMNIFS_PROVIDER_BUNDLE_DIR`, validates its v2 entries by exact name and id, and invokes `mount add` so each selected embedded artifact is retained lazily; it must not copy the whole store into the dev home or rebuild provider Wasm. Retention may warm Wasmtime's host compilation cache as described below. The frontend image (`Dockerfile`'s `frontend-dev`/`frontend-release` stages, built from or injected into `thin-builder`) needs no provider-store build context at all: it runs `omnifs-thin fuse`, which contains no engine runtime and no provider bundle. Release CLI binaries embed the provider bundle and retain selected artifacts through mount creation.

Provider-store indexes strict-parse both the top-level index object and retained provider entries. Unknown keys make the store unreadable instead of being silently accepted.

Retaining a new provider starts the hidden `warm-providers` child as detached best-effort work for that exact provider ID. It loads the retained component through the same `ComponentEngine` used by the daemon, warming the workspace-owned Wasmtime cache, and atomically records aggregate progress in `cache/provider-warmup.json`. Progress is historical status only, never cache authority.

Online `omnifs up` joins warmup through one workspace advisory lock before replacing a serving daemon and retains that lease until the replacement reports readiness. It loads every unique provider ID selected by the immutable mount revision through `ComponentEngine` while holding the lease, so detached warmup cannot overlap the daemon's component loading. The launcher owns this coordination; the daemon has no warmup state or control operation. Successful background work becomes cache hits, while failed, interrupted, evicted, or incompatible entries are retried synchronously and leave the current daemon serving when warmup fails. Offline startup skips provider warmup.

### Local metrics

Dogfood usage metrics are private workspace files and are never transmitted. The CLI and daemon append best-effort JSONL records only under `$OMNIFS_HOME/metrics/`; the writer has no network path or networking dependency. `[metrics] enabled = false` and `OMNIFS_METRICS=0` disable recording. The dogfood reporter reads those local files directly. Metric failures never fail a product operation, and the files remain mode `0600` inside a mode `0700` directory.

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

One global `--output human|json|jsonl` owns the invocation. Human mode renders a compact workspace context strip followed by responsive resource tables on stdout while narration and progress use stderr. Tables use sentence-case headers, soft alignment without borders or rules, explicit state text, and contextual recovery lines; below 72 columns the same typed fields stack beneath each resource identity. JSON emits exactly one result or error envelope on stdout and suppresses progress. Finite JSONL commands emit the same single terminal result or error envelope with a `type` discriminator; streaming passthrough commands such as logs and Inspector records remain line streams. Structured modes never prompt unless required answers were supplied by flags. `--no-input` forbids prompts and browser handoffs, `--yes` approves confirmation-only decisions, and `--quiet` suppresses optional narration without hiding tables, receipts, or errors.

Every JSON result uses one envelope:

```json
{
  "schema_version": 1,
  "command": "status",
  "verdict": "ok",
  "result": {
    "workspace": {},
    "frontends": [],
    "mounts": []
  }
}
```

Inventory and receipt models are typed and sorted before both rendering and serialization. Status owns plural `frontends` and `mounts`; each mount reports the health of its exact provider pin, and focused mutation receipts add plural `access_paths` where relevant. Frontends always carry `scope: "all"` and a mount count because they expose the complete namespace. `up` applies the committed pin exactly and never installs or chooses a replacement artifact.

Observation commands exit 0 when collection succeeds and every resource is positive or neutral, including a deliberately stopped daemon, observed runners waiting to reconnect, offline mounts while stopped, and unnecessary auth. Inventory never adds a row for an unlaunched default. A complete inventory with an actionable or failed row exits 5. When a runtime record says the daemon should be live but its control probe is unavailable, status emits the trustworthy degraded inventory and exits 3. Human, JSON, and JSONL derive the same resource verdict; the exit mapper applies the unreachable override.

`down` stops the daemon only and leaves frontend runners alive. It treats shutdown as complete only
after the acknowledged shutdown request's control surface becomes unavailable and
the recorded daemon process exits; the CLI polls both facts with a bounded timeout
and reports a failed teardown row if either remains live, so a successful
`DaemonStopped` outcome always means a subsequent status probe can observe
`not_running` and replacement cannot overlap its predecessor. `down` never
deletes mount desired state, credentials, provider artifacts, cache or workspace
files, or `$OMNIFS_HOME`; users and uninstallers remove `$OMNIFS_HOME` through
ordinary filesystem operations.

`crates/omnifs-cli/src/ui` owns terminal rendering and stream selection. CLI
modules emit reports, JSON values, narration, or already-rendered raw records
through that surface; clippy rejects direct print macros elsewhere.
The only non-UI passthroughs are daemon logs and generated shell completions,
whose destination streams are owned by the invoked tools.

## Must not

- Make any directory above `$OMNIFS_HOME/mounts` a Git repository, or place credentials, provider artifacts, cache data, sockets, logs, or daemon records under mount-version control.
- Add a second spec read or write path that bypasses `mounts::Registry`, or write a spec to a file whose stem is not its mount name.
- Add a second apply command path, args type, receipt, lifecycle branch, or usage-metrics label for the `apply` spelling.
- Reintroduce a persisted daemon runtime choice (a `[system].runtime`-shaped config field or a daemon-level `--runtime` flag); the daemon has exactly one host runtime.
- Select a remote control endpoint or bypass the workspace's local control socket; the CLI only dials the Unix socket recorded in its own `daemon.json` or the fixed `$OMNIFS_HOME/control.sock`.
- Add ad hoc control operations without keeping the typed client and daemon behavior in step.
- Reintroduce a separate public `omnifsd` binary name in docs or UX.
- Deepen Docker assumptions in daemon architecture; Docker policy belongs in the frontend command paths only.
- Present macOS host-native integration as macFUSE.
- Make the frontend (or any other) Docker image own release provider bundles; the CLI binary is the sole owner.
- Assume a fresh worktree already has provider artifacts or wasi-sdk.
- Move generated or cache state into source directories.

## Code

- `crates/omnifs-api/src/lib.rs`
- `crates/omnifs-cli/src/daemon/app.rs`
- `crates/omnifs-cli/src/daemon/server.rs`
- `crates/omnifs-thin/src/fuse.rs`
- `crates/omnifs-vfs-wire/src/beacon.rs`
- `crates/omnifs-cli/src/commands/frontend/`
- `crates/omnifs-cli/src/frontend_container.rs`
- `crates/omnifs-vfs-wire/src/lib.rs`
- `crates/omnifs-itest/src/live.rs`
- `crates/omnifs-thin/src/nfs.rs`
- `crates/omnifs-workspace/src/mounts/mod.rs`
- `crates/omnifs-cli/src/launch.rs`
- `crates/omnifs-cli/src/image.rs`, `docker.rs`, and `process.rs`
- `crates/omnifs-cli/src/docker.rs`
- `crates/omnifs-cli/src/daemon_teardown.rs`
- `crates/omnifs-cli/src/provider_bundle.rs`
- `crates/omnifs-workspace/src/layout.rs`
- `scripts/dev.ts`
- `Dockerfile`
- `crates/omnifs-api/src/control.rs`
- `CONTRIBUTING.md`

## Validation

- Control protocol changes need focused daemon, CLI, and existing lifecycle tests for request/reply and streaming behavior.
- Protocol shape changes keep `omnifs-api` wire types, daemon dispatch, and CLI decoding synchronized.
- Daemon-launch or frontend-attach changes need targeted CLI/daemon tests and live runtime validation for the affected path.
- Contributor workflow changes need CLI tests and, when touching launch behavior, `just dev -y` plus the smoke path in `CONTRIBUTING.md`.
