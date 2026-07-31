# Control plane contracts

Status: current-contract
Owns: the CLI/daemon split, typed local RPC, profile layout, mounts and credentials, filesystem runners, and contributor dev state.

## Read when

Read this before touching `omnifs-cli`, `omnifs-api`, `omnifs-bootstrap`, `omnifs-daemon`, `omnifs-state`, lifecycle commands, daemon status, mount or credential RPC, filesystem runtimes, logs, or `scripts/dev.ts`.

## Rules

## Ownership boundary

There is no shared workspace store. `omnifs-bootstrap` resolves one profile root from `OMNIFS_HOME` or `$HOME/.omnifs`, creates the fixed `control.sock`, writes the narrow `process.json` identity, and serializes daemon spawn with `spawn.lock`.

The CLI owns user-facing commands, OAuth and static-auth UX, client config, the legacy single-record mutation journal, metrics, daemon spawn, and resource authoring. It persists remaining client data under `<profile>/client/` and sends all daemon mutations through typed local RPC. Legacy filesystem specs are read-only migration input and never start a runtime.

The daemon owns providers, credentials, mounts, desired resources, Attachment runtime state and lifecycle, SQLite state and cache, attach endpoints, live VFS sessions, and raw log bytes. Its durable state is under `<profile>/daemon-state/`: `control-store/state.sqlite3`, `cache/`, `runtime/attachments/`, `staging/`, `logs/`, and the engine projection, Wasmtime, clone, and guest-image caches. The daemon never reads client files or chooses client configuration.

The control protocol is the only CLI-to-daemon API. It is tonic/protobuf gRPC using the checked-in `omnifs.control.v1` schema and generated Rust from `build.rs`, served only on the profile's local Unix socket. It exposes readiness, status and inventory, provider import and metadata, pure resource planning, transactional complete-set apply, typed progress watches, durable actions, Attachment status and access, the transitional mutation lease, recovery and repair, shutdown, Inspector subscription, and bounded raw log streaming. `ApplyResources` performs validation and one SQLite transaction, sends a non-blocking reconcile wakeup, and returns. Provider preparation, serving publication, runtime launch, mount, and VFS waits occur only in daemon reconcilers. Credential material may cross only in request payloads on this local socket. It never crosses filesystem attach/TCP or appears in responses, status, inventory, logs, Debug, Inspector, progress, or receipts.

The daemon listens on the profile's fixed `control.sock`. The profile directory is `0700`; the socket and process identity are `0600`. The VFS namespace is separate: `daemon-state/local.sock` and one profile-derived loopback or Docker-bridge TCP port. TCP has no auth and never binds all interfaces. Both VFS listeners must bind before readiness, and either listener's unexpected exit is fatal.

The process identity is diagnostic metadata only. RPC status and inventory are authoritative when reachable. Doctor owns stale process and filesystem cleanup and requires a stopped daemon, consent, and fresh exact identity proof before destructive repair.

## Command grammar

The public binary is one `omnifs` executable. The hidden `omnifs daemon` subcommand runs the daemon; `omnifs run-fs` dispatches a host filesystem. Public commands are:

- `omnifs status`, `down`, `logs`, `inspect`, `doctor`, `setup`, `skill`, `completions`, and `version`.
- `omnifs mount add|ls|show|update|reauth|revoke|rm`. Add, update, and remove each apply one lease-scoped batch through `BeginMutation`/`ApplyMutation`. `mount add` can upload an exact Wasm artifact or select an embedded provider, then folds a fresh credential submission and the mount create into one batch. `mount update` re-reads the mount under its own lease and applies its patch atomically; there is no version-based compare-and-swap. `mount reauth` and `mount revoke` each apply a single-op credential batch.
- `omnifs credential ls|rm`. List returns non-secret daemon-owned credential status. Remove deletes stored material after showing affected mounts and getting consent; it does not revoke access upstream.
- `omnifs fs create|attach|detach|restart|rm|shell|ls --name <id>` is the transitional Attachment grammar. Create and ensure-present commit desired resources, detach and remove delete them, restart submits a durable action, shell uses typed daemon access, and list reads desired and observed status. Mutation commands follow their revision or action stream by default.

Global `--output human|json|jsonl`, `--quiet`, `--no-input`, and `--yes` apply to one invocation after Clap parses it. JSON emits one terminal envelope. JSONL emits stream records followed by one terminal result or error. Clap usage errors exit 2 before output mode applies.

`omnifs setup` starts the daemon, lists every embedded provider with an honest auth label, then offers two quick-start confirms: mount every provider that needs no sign-in in one atomic batch, and attach the platform's recommended filesystem. It never selects providers on the caller's behalf and never starts an OAuth flow; a provider that needs a sign-in or a config value is left for `omnifs mount add`. There is no `omnifs up`, `omnifs apply`, or offline product mode.

`omnifs down` rejects new writes, asks `AttachmentSupervisor` to stop every exact observed runtime, waits for the bounded drain, reports stragglers, and stops the daemon without deleting desired Attachment rows. The next daemon reloads and restores desired Attachments.

## Mounts, providers, and credentials

Mounts are daemon-owned SQL rows with typed definitions, provider content IDs, versions, auth declarations, and limits. `ListMounts` and `GetMount` are the only mount reads; every write (`CreateMountOp`, `UpdateMountOp`, `RemoveMountOp`) is one op inside an `ApplyMutation` batch, applied in one SQLite transaction. There is no per-mount CAS: the daemon's single mutation lease already serializes every writer, so the batch is the only ordering guarantee a client needs. Every row a batch writes is stamped with that batch's `MutationId` (`last_mutation_id`), which is the sole provenance a client uses to tell whether an interrupted request actually committed.

Provider artifacts live in daemon state. `ImportProvider` accepts a bounded upload or an exact embedded provider name, validates the content digest and metadata, and returns a receipt keyed only by content digest. Provider import carries no mutation identity and never touches the mutation lease: a dropped upload is simply retried, and importing identical bytes twice returns `Unchanged` rather than a second row.

Credentials live in the daemon's SQLite store. The CLI owns browser, device, and static-token UX, then folds the collected material into a `SubmitCredentialOp`/`DeleteCredentialOp`/`RevokeCredentialOp` inside a mutation batch (often alongside a mount op, so a fresh sign-in and the mount that needs it commit or fail together). The daemon injects credentials only into host callouts. It does not expose credential values, file paths, or a reload command; status reports only non-secret health.

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

`scripts/dev.ts` owns contributor state. It uses a dedicated profile such as `~/.omnifs-dev`, builds the provider bundle and native CLI, writes client config, invokes `mount add` for selected providers, starts the host daemon, creates `dev-host` and `dev-docker` specs, attaches them, and opens `/omnifs` in the Docker runner. It does not create a Git mount repository or a daemon container.

## Must not

- Reintroduce `omnifs-workspace`, a shared workspace API, client-side mount desired state, Git refs, immutable mount snapshots, `daemon.json`, or JSON credential storage.
- Add `up`, `apply`, `offline`, or a second reconcile path.
- Send credentials through filesystem attach/TCP or expose them in RPC replies, status, inventory, logs, tracing, metrics, Debug, Inspector, or receipts.
- Make the daemon read legacy client filesystem specs or config, make normal lifecycle write `client/filesystems`, or make the CLI read daemon SQLite tables and logs directly.
- Add a remote control endpoint or TCP authentication mode. TCP attach remains local loopback or the detected Docker bridge without auth.
- Clear observed Attachment identity or a deletion tombstone before exact runtime and session teardown is proved.

## Code

- `crates/omnifs-bootstrap/src/lib.rs`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-cli/src/mutation.rs`
- `crates/omnifs-cli/src/client_state.rs`
- `crates/omnifs-cli/src/client_fs_state.rs`
- `crates/omnifs-inspector/src/lib.rs`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/control.rs` and `crates/omnifs-daemon/src/control/`
- `crates/omnifs-daemon/src/daemon.rs`
- `crates/omnifs-daemon/src/log_stream.rs`
- `crates/omnifs-daemon/src/manager.rs`
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-state/src/batch.rs`
- `crates/omnifs-vfs/src/frame.rs`
- `crates/omnifs-vfs/src/server.rs`
- `crates/omnifs-vfs/src/serving.rs`
- `scripts/dev.ts`

## Validation

- Run `just docs-check` for documentation-only changes.
- For control protocol changes, run the typed request/reply and lifecycle tests in `crates/omnifs-api`, `crates/omnifs-cli`, `crates/omnifs-daemon`, and `crates/omnifs-itest/tests/control_plane`.
- For filesystem behavior, use `just dev -y`, `target/debug/omnifs status`, and the relevant live smoke path.
