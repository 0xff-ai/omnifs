# Control plane contracts

Status: current-contract
Owns: the CLI/daemon split, typed local RPC, profile layout, mounts and credentials, filesystem runners, and contributor dev state.

## Read when

Read this before touching `omnifs-cli`, `omnifs-api`, `omnifs-bootstrap`, `omnifs-daemon`, `omnifs-state`, lifecycle commands, daemon status, mount or credential RPC, filesystem runtimes, logs, or `scripts/dev.ts`.

## Rules

## Ownership boundary

There is no shared workspace store. `omnifs-bootstrap::Profile` resolves one profile root from `OMNIFS_HOME` or `$HOME/.omnifs`, owns only fixed pre-RPC paths, and exposes `SpawnLock` and exact `DaemonIdentity` operations.

The CLI owns user-facing commands, OAuth and static-auth UX, profile config, metrics, daemon spawn, and resource authoring. Interactive provider, mount, credential, and attachment commands edit the complete desired resource set through `PlanResources` and `ApplyResources`, then follow `WatchProgress` or a durable action stream. KCL `plan` and `apply --yes` are the automation path; `credential set --from-env` is the only narrow secret automation command. It sends every write through typed local RPC and keeps no desired-state journal. A strict read-only scanner may report legacy detached filesystem specs; it never launches, edits, or deletes them.

The daemon owns providers, credentials, mounts, desired resources, Attachment runtime state and lifecycle, SQLite state and cache, attach endpoints, live VFS sessions, and raw log bytes. Its durable state is under `<profile>/daemon-state/`: `control-store/state.sqlite3`, `cache/`, `runtime/attachments/`, `staging/`, `logs/`, and the engine projection, Wasmtime, clone, and guest-image caches. The daemon never reads client files or chooses client configuration.

The control protocol is the only CLI-to-daemon API. It is tonic/protobuf gRPC using the checked-in `omnifs.control.v1` schema and generated Rust from `build.rs`, served only on the profile's local Unix socket. It exposes readiness, status and inventory, provider import and metadata, pure resource planning, transactional complete-set apply, typed progress watches, durable actions, Attachment status and access, recovery and repair, shutdown, Inspector subscription, and bounded raw log streaming. `ApplyResources` performs validation and one SQLite transaction, sends a non-blocking reconcile wakeup, and returns before provider preparation or Attachment work. It cannot compile Wasm, initialize a provider, publish or drain a generation, fetch an image, launch a runtime, mount an OS filesystem, or wait for a VFS session.

Each progress subscription starts with a complete snapshot and registers before it reads current state, closing the subscribe-versus-update race. Fanout is bounded and non-blocking. A slow consumer receives a resync snapshot and never delays daemon work. Revision streams include only work that can affect that revision; unused catalog warm-up appears only in current status. Closed provider, serving, credential, Attachment, revision, and action event variants carry closed stage enums, real byte counts, retries, queue counts, and terminal outcomes. They never invent compilation percentages or infer cache hits from time.

Credential and Attachment restart actions use client-generated action IDs and action-generation preconditions. SQLite accepts at most one non-terminal action per target, retains pending work across daemon restart, and returns a durable typed receipt after reply loss. Secret bytes are neither stored nor hashed for dedupe: the first accepted action ID wins, and new material requires a new ID. Credential material may cross only in request payloads on the local control socket. It never crosses filesystem attach/TCP or appears in responses, status, inventory, logs, Debug, Inspector, progress, or receipts.

The daemon listens on the profile's fixed `control.sock`. The profile directory is `0700`; the socket and process identity are `0600`. The VFS namespace is separate: `daemon-state/local.sock` and one profile-derived loopback or Docker-bridge TCP port. TCP has no auth and never binds all interfaces. Both VFS listeners must bind before readiness, and either listener's unexpected exit is fatal.

The process identity is diagnostic metadata only. RPC status and inventory are authoritative when reachable. Doctor owns stale process and filesystem cleanup and requires a stopped daemon, consent, and fresh exact identity proof before destructive repair.

## Command grammar

The public binary is one `omnifs` executable. The hidden `omnifs daemon` subcommand runs the daemon; `omnifs run-fs` dispatches a host filesystem. Public commands are:

- `omnifs status`, `down`, `logs`, `inspect`, `doctor`, `setup`, `skill`, `completions`, and `version`.
- `omnifs provider add|ls|show|rm` imports embedded or local Wasm, pins a named Provider resource, and never grants authority by import alone.
- `omnifs mount add|ls|show|update|reauth|revoke|rm` is interactive resource porcelain. It selects a Provider, collects typed config, host resources, limits, and Credential references, shows the planner's diff, applies the complete desired set, and follows typed revision progress. `reauth` changes secret material; `revoke` performs explicit upstream revocation and leaves the declared slot in `NeedsSecret`.
- `omnifs credential login|ls|show|rm|revoke` plus `omnifs credential set <name> --from-env <variable>` keep values out of argv and output. `set --from-env` is the one secret automation path; login, removal, and revoke are interactive and follow durable action progress.
- `omnifs attachment add|ls|show|rm|restart|shell <name>` is the public filesystem lifecycle. Resource presence requests attachment; removal requests teardown. Restart follows its action stream and shell uses typed daemon access. The old `fs` grammar is retired publicly, while hidden `run-fs` remains an internal runner.

Global `--output human|json|jsonl`, `--quiet`, `--no-input`, and `--yes` apply to one invocation after Clap parses it. JSON emits one terminal envelope. JSONL emits stream records followed by one terminal result or error. Clap usage errors exit 2 before output mode applies.

`omnifs setup` starts the daemon, lists every embedded provider with honest auth and config labels, offers no-sign-in providers, creates Provider and Mount resources plus the recommended Attachment in one desired set, and asks for one plan consent. It then follows the returned revision until ready, failed, or superseded. The apply RPC does not wait for compilation or runtime work. There is no `omnifs up` or offline product mode; `omnifs plan <file>` and `omnifs apply <file> --yes` are the automation surface.

`omnifs down` rejects new writes, asks `AttachmentSupervisor` to stop every exact observed runtime, waits for the bounded drain, reports stragglers, and stops the daemon without deleting desired Attachment rows. The next daemon reloads and restores desired Attachments.

## Mounts, providers, and credentials

SQLite is the sole desired-state authority. Provider, Mount, Credential, and Attachment definitions commit as one complete set with an exact base revision and desired digest. The durable apply receipt makes retries safe across lost replies. Public reads may use resource snapshots or typed resource-specific status calls; there is no lease-scoped mutation batch, imperative authoring RPC, or client recovery journal.

Provider artifacts live in daemon state. `ImportProvider` accepts a bounded upload or an exact embedded provider name, validates the content digest and metadata, and returns a receipt keyed only by content digest. A dropped upload is simply retried, and importing identical bytes twice returns `Unchanged` rather than a second row.

Credentials live in the daemon's SQLite store. The CLI owns browser, device, and static-token UX, then submits secret material as a request sidecar while the desired Credential resource is planned and applied. The daemon injects credentials only into host callouts. It does not expose credential values, file paths, or a reload command; status reports only non-secret health. Login, set, and revoke follow the returned action through generation refresh, drain, and explicit upstream work.

## Filesystems and attach

`omnifs_core::AttachmentSpec` owns the exact protocol, runtime, resolved location, and runtime asset references. SQLite Attachment resources are desired truth. Durable observed rows retain the exact observed spec, version, runtime instance, phase, retry state, action generation, and deletion tombstone until teardown is proved. Host locations are absolute; Docker and libkrun use `/omnifs`.

`AttachmentSupervisor` owns bounded host process, Docker, and libkrun launch and teardown through `omnifs-fs-runtime`. It persists exact identity before effects, adopts only an exact current runtime and session, serializes work per Attachment, and keeps pending restart actions durable across daemon restart. Each runner remains out of process and attaches to the shared `omnifs_vfs::Namespace` without provider or credential state.

`VfsServer` owns listener tasks, readiness, reconnect, server-pushed stop, and live session records. VFS wire v11 handshakes carry the Attachment name, exact `AttachmentSpec`, and runtime instance. The daemon admits only the expected exact session and rejects conflicting identity. Configured Attachments and live VFS sessions are separate domains.

## Logs, Inspector, metrics, and dev

The daemon appends raw bytes to `daemon-state/logs/daemon.log`; `omnifs logs` reads or follows them through `StreamLogs`. Inspector events use the typed subscription stream. The CLI preserves raw log bytes and bounds tail and stream frames.

`omnifs-inspector` owns Inspector state, replay and live-event sources, terminal
lifecycle, and TUI rendering. `omnifs-cli` resolves the profile endpoint,
dispatches the command, and renders the final session receipt. Inspector
restores both terminal and prior panic-hook state before returning to the CLI.

CLI dogfood metrics are local client files under `<profile>/metrics/`, controlled by `[metrics].enabled` and `OMNIFS_METRICS`; they are never transmitted and cannot fail a command. Daemon logs and cache data remain daemon-owned.

`scripts/dev.ts` owns contributor state. It uses a dedicated profile such as `~/.omnifs-dev`, builds the provider bundle and native CLI, renders a temporary or profile-local KCL desired config, sets any dev credential through `target/debug/omnifs credential set --from-env`, invokes `target/debug/omnifs apply <file> --yes`, waits for the terminal revision, and opens `target/debug/omnifs attachment shell dev-docker -- ...` at `/omnifs`. It does not invoke interactive porcelain or create a daemon container.

## Must not

- Reintroduce `omnifs-workspace`, a shared workspace API, client-side mount desired state, Git refs, immutable mount snapshots, `daemon.json`, or JSON credential storage.
- Add `up`, an imperative mutation path separate from the resource planner, or an offline product mode. `omnifs apply` is the KCL complete-set apply command.
- Send credentials through filesystem attach/TCP or expose them in RPC replies, status, inventory, logs, tracing, metrics, Debug, Inspector, or receipts.
- Make the daemon read legacy detached specs or config, make normal lifecycle write a client-owned filesystem tree, or make the CLI read daemon SQLite tables and logs directly.
- Add a remote control endpoint or TCP authentication mode. TCP attach remains local loopback or the detected Docker bridge without auth.
- Clear observed Attachment identity or a deletion tombstone before exact runtime and session teardown is proved.

## Code

- `crates/omnifs-bootstrap/src/lib.rs`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-cli/src/legacy_filesystems.rs`
- `crates/omnifs-inspector/src/lib.rs`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/control.rs` and `crates/omnifs-daemon/src/control/`
- `crates/omnifs-daemon/src/daemon.rs`
- `crates/omnifs-daemon/src/log_stream.rs`
- `crates/omnifs-daemon/src/resource_control.rs`
- `crates/omnifs-daemon/src/progress.rs`
- `crates/omnifs-daemon/src/attachment_supervisor.rs`
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-state/src/resource.rs`
- `crates/omnifs-state/src/action.rs`
- `crates/omnifs-vfs/src/frame.rs`
- `crates/omnifs-vfs/src/server.rs`
- `crates/omnifs-vfs/src/serving.rs`
- `scripts/dev.ts`

## Validation

- Run `just docs-check` for documentation-only changes.
- For control protocol changes, run the typed request/reply and lifecycle tests in `crates/omnifs-api`, `crates/omnifs-cli`, `crates/omnifs-daemon`, and `crates/omnifs-itest/tests/control_plane`.
- For filesystem behavior, use `just dev -y`, `target/debug/omnifs status`, and the relevant live smoke path.
