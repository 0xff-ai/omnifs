# Control plane contracts

Status: current-contract
Owns: CLI/daemon split, typed local control protocol, mount desired state, filesystem runtimes, workspace layout, and dev home.

`omnifs_workspace::Workspace` is the central broker for one workspace. It does
not expose the home root, generic directory getters, or path-transfer objects.
CLI and daemon code request behavior-owning components, and a concrete path may
leave a component only at the immediate filesystem, process, protocol, engine,
test-fixture, or final-output boundary that consumes it. Relative path names and home-root env resolution live on `Workspace`
itself; only that type binds those names to behavior-owning components.

## Read when

Read this before touching `omnifs-cli`, `omnifs-api`, lifecycle commands, daemon status, control operations, mount desired state, revision application, filesystem runtimes, the embedded provider bundle, or dev workspace behavior.

## Rules

### CLI and daemon

A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`; there is no separate public `omnifsd` binary.

The daemon exposes one current, versioned JSONL control protocol whose wire types live in `omnifs-api`. It has Ready, Status, Shutdown, ValidateOffline, and SubscribeInspector requests, with no remote API or compatibility layer. Shutdown carries whether this is an explicit filesystem-stopping `down` or a daemon-only replacement.

A host-native daemon serves the local control protocol over `$OMNIFS_HOME/control.sock`. The workspace directory is forced to `0700` and the socket to `0600`, so filesystem permissions authenticate every control request. The control protocol has no remote TCP listener, bearer-token mode, or environment-selected endpoint.

`$OMNIFS_HOME/daemon.json` is the daemon process-identity record. It records the local control socket, daemon pid, per-start instance, exact mount revision, and start time. Attach endpoints have no durable target store: the Unix path and TCP port are pure workspace configuration, and Status reports the bound TCP address. The daemon publishes its record only after both namespace endpoints are ready and removes it on graceful exit.

The CLI resolves the local control socket from its own workspace's `daemon.json` when present, else from the fixed `$OMNIFS_HOME/control.sock`, else the daemon is stopped. It never selects a remote endpoint or accepts a control endpoint override. An unreadable daemon record is stale metadata and permits only the fixed-socket fallback. The CLI asserts the `instance_id` echoed by the typed Status reply against the record's, so a record overwritten by a restart mid-command is caught.

The control protocol is local-only over `$OMNIFS_HOME/control.sock`; its workspace directory and socket permissions authenticate requests. The VFS Unix and TCP endpoints are separate namespace transports and are not control-protocol transport.

The control protocol exposes only operational state that contains no secrets. Mount health is reported with each `MountInfo`; the daemon has no credential enumeration or reload surface. Credential import and `mount reauth` write the host store and take effect on the next `omnifs up` or `omnifs apply`, while OAuth refresh remains live only for an already-bound mount. Provider state is derived from exact mount pins, never install recency.

Mount wire payloads distinguish provider identity from provider naming. `provider_name` is the human/catalog slug used by credentials and UX. `provider_id` is the pinned provider content hash for the exact artifact the mount runs.

### Mount desired state

Only `$OMNIFS_HOME/mounts` is a local Git repository. Its `HEAD` is desired mount state and `refs/omnifs/applied` is the last revision that reached daemon readiness. Credentials, provider artifacts, cache data, sockets, logs, and runtime records remain outside Git. First use initializes the repository without a remote and commits existing valid specs with a stable Omnifs-local author, so behavior never depends on the operator's Git configuration.

Specs are one file per mount, and a spec file's stem is its mount name. `mounts::Registry` remains the sole owner of parsing, naming, and atomic file writes, while `mounts::Repository` owns Git revisions, snapshots, and the shared advisory lock around an official write or apply operation. Explicit mount-writing commands record the resulting desired-state commit. `mount add` is create-only, and `mount rm` changes only Git-backed desired state because rollback can restore its credential reference; credential revocation is explicit through `omnifs mount revoke <name>`.

`omnifs mount revoke <name>` resolves the exact credential selected by the mount and names every configured mount sharing it before confirmation. A present OAuth credential is revoked upstream first when the pinned scheme supports revocation, and any upstream failure leaves the local credential intact for retry. Static credentials and OAuth schemes without a revocation endpoint delete locally. An already-absent credential is a successful no-op that needs no confirmation. The running daemon is never changed; successful deletion applies on the next `omnifs up` or `omnifs apply`.

`omnifs up` is the sole apply implementation, and visible `omnifs apply` is an alias of that exact clap command, args type, handler, usage-metrics label, and receipt. Online `up` may commit valid manual `*.json` edits before application, then rejects malformed specs, unexpected tracked paths, missing provider artifacts, insufficient grants, and unusable credentials before stopping a healthy daemon. `omnifs up --offline` observes the existing committed `HEAD` and snapshots that exact revision without committing dirty specs; it skips provider, credential, network, and runtime startup checks, serves only validated durable projection facts, never advances `refs/omnifs/applied`, and restarts a daemon when its online/offline mode differs. When replacing a responding daemon, it first asks that daemon to validate the exact snapshot against its open cache; a failed validation leaves the current daemon, sockets, and applied ref untouched.

The CLI materializes `HEAD` under cache storage and starts the daemon with that immutable snapshot plus its exact revision. The daemon never invokes Git or chooses desired state. If a healthy daemon already records `HEAD`, `up` leaves it running. Otherwise it stops only the daemon, starts the new revision, waits for readiness, and then advances `refs/omnifs/applied`; a failed start never advances the ref. `up` and its exact `apply` alias never launch, stop, or reconcile a filesystem. Existing filesystem runners survive daemon replacement and reconnect through the fixed endpoints. Explicit `down` asks the daemon to stop attached filesystems, waits for a bounded drain, reports any busy stragglers, then stops the daemon.

`omnifs setup [--providers NAME] [--no-up] [--no-browser]` is the thin first-run composition over mount configuration, daemon start, named filesystem creation and attachment, and Inventory operations. Provider names are exact embedded names; `--yes` selects only providers whose existing resolver policy proves safe without required config or a fresh credential flow, while `--no-input` requires explicit providers or `--yes`. With `--no-up`, setup configures mounts only. Structured output ends with the single terminal Inventory result after all selected operations.

Host-owned mount objects, including `auth` and `limits`, strict-parse their fields. Unknown top-level or nested host-owned keys are invalid, while the provider-owned `config` object remains opaque to the host. The control protocol has no mount mutation or reconcile operation; it retains typed filesystem, status, event, readiness, attach, and shutdown surfaces.

Add an operation only when it has one owning domain fact and a focused typed reply. Keep credential material off the control protocol.

### Runtime modes

There is one daemon runtime: host-native. `omnifs up` always spawns a host-native child process; there is no Docker daemon runtime and no `--runtime`/`[system].runtime` choice to make. The daemon is a pure namespace server; it never mounts a filesystem in-process.

Docker's only runtime role is serving a named FUSE filesystem in a separate, credential-free container attached to the host-native daemon over TCP. It never runs the daemon.

The daemon serves one shared namespace through `omnifs_vfs::VfsServer`. `VfsServer` owns the fixed Unix and TCP listeners, listener and connection tasks, readiness, server-pushed stop, drain state, and live attachments. The daemon owns namespace construction, typed local control, and process lifetime. Host filesystems run through hidden `omnifs run-fs`; Docker and libkrun guests use the slim `omnifs-thin` binary. `DaemonStatus.filesystems` reports exact live attachment specs and `DaemonStatus.attach_tcp` reports the bound TCP address.

`omnifs_core::fs` owns the validated `Id`, `Protocol`, `Runtime`, and fully resolved `Spec` types. `omnifs-workspace` stores one strict atomic spec per ID under `$OMNIFS_HOME/filesystems/specs` and supplies a per-ID claim that serializes create, attach, detach, restart, and remove. Filesystem specs are configuration, not desired running state.

`omnifs fs create --name <id>` resolves defaults once and persists every field without launching a runtime. Linux defaults to FUSE/host, Apple Silicon macOS to FUSE/libkrun, and Intel macOS to NFS/host. NFS without a runtime means host; Docker or libkrun without a protocol means FUSE. Host locations must be absolute and default to an ID-bearing workspace path. Docker and libkrun reject `--location` and persist `/omnifs`.

`omnifs fs attach|detach|restart|rm --name <id>` and `fs shell --name <id>` select only by stable ID. Attach rejects a confirmed existing instance. Detach proves both mount absence and runtime exit. Restart preserves the stored spec. Remove never detaches and refuses running, attached, or uncertain state. `fs ls` joins specs with exact daemon attachment state and reports `unknown` when that join cannot be proved.

`daemon.json` records process identity for control-plane readiness and stale teardown. Attach endpoints have no durable target record: the Unix path is fixed, and `[filesystem].attach_port` or a stable workspace-derived nonzero port selects TCP. Runtime comes from the launcher-supplied wire identity, not listener ownership.

Keep filesystem-specific Docker policy in `commands/fs.rs` and `fs_container.rs`; the daemon launch path has no Docker policy.

### Namespace attach sockets and out-of-process filesystems

The daemon always serves its shared namespace over `$OMNIFS_HOME/filesystems/runtime/local.sock` and one fixed-port TCP endpoint. The runtime directory is forced to `0700` and the Unix socket to `0600`; filesystem permissions authenticate the Unix socket. TCP has no auth and binds only loopback or Linux's detected Docker bridge address. A refused stale Unix socket is removed before binding, while any ambiguous probe error fails closed. Both endpoints bind before daemon readiness and either listener's unexpected exit is fatal. There is no named attach-socket flag or listener-creation control operation. The Omnifs VFS wire protocol uses length-delimited postcard framing from `omnifs-vfs`. It transports the shared `Namespace` surface and does not own projection semantics.

The `Ready` operation succeeds only after the immutable mount revision loads completely and both fixed namespace listeners have bound and remain supervised. Listener readiness does not require a filesystem to be attached. `DaemonStatus.filesystems` is the authoritative location set; status has no singular daemon mount-point projection or failed-mount collection.

The host filesystem runs through the full binary's hidden `omnifs run-fs`; Docker and libkrun guests use `omnifs-thin`. Both accept required flat `--name`, `--protocol`, `--runtime`, and `--location` flags, attach a VFS-wire-backed namespace, and serve until teardown. `OMNIFS_ATTACH_ADDR` selects TCP or vsock for guests. Host NFS also persists filehandle identity and mount discovery state so a restarted runner can resume an active kernel mount.

The Docker path reads `DaemonStatus.attach_tcp`, checks the full address against the expected loopback or Docker bridge address and workspace port, resolves the image by build-channel provenance, and runs one exact ID-bearing container with no binds, no `OMNIFS_HOME`, no docker.sock, no SSH agent, and no published ports. The workspace home and filesystem ID labels plus immutable container ID form the ownership proof used by doctor. Normal lifecycle checks also compare the inspected flat command with the stored resolved spec.

`omnifs fs shell --name <id> [--shell <path>] [--command <argv>...]` probes only the runtime stored in the spec. Docker enters the exact labeled container. Libkrun enters the exact helper and VM. Host verifies its mounted phase and reports the ordinary host path.

The libkrun runtime (Apple Silicon macOS only) mirrors that build-channel provenance for its guest disk image instead of a container image: a release binary resolves `ghcr.io/0xff-ai/omnifs-guest:<version>` and pulls it into the workspace cache on first use; a dev binary resolves the local `target/guest-image/omnifs-guest.raw` and never downloads. `[filesystem].guest_image`/`OMNIFS_GUEST_IMAGE` override either default the same way `[filesystem].docker_image`/`OMNIFS_FILESYSTEM_IMAGE` override the Docker image. The CLI launches only its sibling `omnifs-libkrun` helper and passes the fixed configuration through the shared `omnifs-libkrun` crate. Its strict helper record and control reply carry the full resolved spec and random process instance; no external launcher discovery or compatibility path exists.

The resolved libkrun image is an immutable base. Before each launch, the CLI materializes `filesystems/runtime/<id>/libkrun/root.raw` with mode `0600` and passes that root, never the cached or configured base path, as the first virtio-blk device. Atomic temporary roots and `root.raw` are launch-owned state and are removed on rollback, restart, and detach; the base image is preserved unchanged.

The Omnifs VFS wire protocol also serves over local TCP because a container cannot share the host Unix socket. Docker Desktop forwards `host.docker.internal` to host loopback. Native Linux maps that name to the default Docker bridge gateway, so daemon startup binds that detected address when present and loopback otherwise. It never binds `0.0.0.0` or uses host networking. The configured or workspace-derived nonzero port stays stable across daemon replacement. Wire protocol v9 carries the exact resolved spec in the handshake, engine-issued cache lifetimes, cacheable negative lookup answers, terminal `OfflineMiss`, and server-pushed stop control. Older clients are rejected with a named reason. Libkrun maps guest vsock port 1024 through its helper-owned bridge to the fixed Unix endpoint.

### Dev home

`scripts/dev.ts` owns contributor dev state. It renders a dedicated `~/.omnifs-dev` home, builds the CLI with its provider bundle, starts the host-native daemon, ensures stable `dev-host` and `dev-docker` specs, attaches them by name, and opens the developer at `/omnifs` inside the Docker filesystem. Host CLI commands use normal workspace resolution unless `OMNIFS_HOME` is explicit; do not add a Rust-side dev command or dev-session owner.

### Provider bundles

`just build providers` emits the content-addressed provider-store bundle at `target/omnifs-provider-store`. `scripts/dev.ts` embeds it into the natively-built `omnifs` CLI/daemon binary via `OMNIFS_PROVIDER_BUNDLE_DIR`, validates its v2 entries by exact name and id, and invokes `mount add` so each selected embedded artifact is retained lazily; it must not copy the whole store into the dev home or rebuild provider Wasm. Retention may warm Wasmtime's host compilation cache as described below. The filesystem image (`Dockerfile`'s `filesystem-dev`/`filesystem-release` stages, built from or injected into `thin-builder`) needs no provider-store build context: it runs flat `omnifs-thin` arguments supplied by the launcher and contains no engine runtime or provider bundle. Release CLI binaries embed the provider bundle and retain selected artifacts through mount creation.

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
    "filesystems": [],
    "mounts": []
  }
}
```

Inventory and receipt models are typed and sorted before both rendering and serialization. Status owns plural `filesystems` and `mounts`; each mount reports the health of its exact provider pin, and focused mutation receipts add plural `access_paths` where relevant. Filesystems always carry `scope: "all"` and a mount count because they expose the complete namespace. `up` applies the committed pin exactly and never installs or chooses a replacement artifact.

Observation commands exit 0 when collection succeeds and every resource is positive or neutral, including a deliberately stopped daemon, offline mounts while stopped, and unnecessary auth. Inventory never adds a row for an unlaunched default or for a runtime record that is not attached to the daemon. A complete inventory with an actionable or failed row exits 5. When the daemon record says the daemon should be live but its control probe is unavailable, status emits the trustworthy degraded inventory and exits 3. Human, JSON, and JSONL derive the same resource verdict; the exit mapper applies the unreachable override.

`down` asks every attached filesystem to stop, waits up to the daemon's drain bound,
returns the detached and still-attached identities in the typed shutdown result,
then stops the daemon. Busy or slow filesystems are reported as stragglers and remain
for `omnifs doctor`; their presence does not make daemon shutdown unbounded. The CLI
uses a request timeout longer than the drain and then waits until the control surface
and recorded process are both gone. `down` never
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
- Deepen Docker assumptions in daemon architecture; Docker policy belongs in the filesystem command paths only.
- Present macOS host-native integration as macFUSE.
- Make the filesystem (or any other) Docker image own release provider bundles; the CLI binary is the sole owner.
- Assume a fresh worktree already has provider artifacts or wasi-sdk.
- Move generated or cache state into source directories.

## Code

- `crates/omnifs-api/src/lib.rs`
- `crates/omnifs-cli/src/daemon/app.rs`
- `crates/omnifs-cli/src/daemon/server.rs`
- `crates/omnifs-thin/src/fuse.rs`
- `crates/omnifs-vfs/src/beacon.rs`
- `crates/omnifs-cli/src/commands/fs.rs`
- `crates/omnifs-cli/src/fs_container.rs`
- `crates/omnifs-vfs/src/lib.rs`
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
- Daemon-launch or filesystem-attach changes need targeted CLI/daemon tests and live runtime validation for the affected path.
- Contributor workflow changes need CLI tests and, when touching launch behavior, `just dev -y` plus the smoke path in `CONTRIBUTING.md`.
