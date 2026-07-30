# Control plane contracts

Status: current-contract
Owns: the CLI/daemon split, typed local RPC, profile layout, mounts and credentials, filesystem runners, and contributor dev state.

## Read when

Read this before touching `omnifs-cli`, `omnifs-api`, `omnifs-bootstrap`, `omnifs-daemon`, `omnifs-state`, lifecycle commands, daemon status, mount or credential RPC, filesystem runtimes, logs, or `scripts/dev.ts`.

## Rules

## Ownership boundary

There is no shared workspace store. `omnifs-bootstrap` resolves one profile root from `OMNIFS_HOME` or `$HOME/.omnifs`, creates the fixed `control.sock`, writes the narrow `process.json` identity, and serializes daemon spawn with `spawn.lock`.

The CLI owns user-facing commands, OAuth and static-auth UX, client config, `ClientOwnerId` and the single-record mutation journal, filesystem specs and runners, metrics, and daemon spawn. It persists client data under `<profile>/client/` and sends all daemon mutations through typed local RPC.

The daemon owns providers, credentials, mounts, SQLite state and cache, attach endpoints, live filesystem attachments, and raw log bytes. Its durable state is under `<profile>/daemon-state/`: `control-store/state.sqlite3`, `cache/`, `staging/`, `logs/daemon.log`, and the engine projection, Wasmtime, and clone caches. The daemon never reads client files or chooses client configuration.

The control protocol is the only CLI-to-daemon API. It is tonic/protobuf gRPC using the checked-in `omnifs.control.v1` schema and generated Rust from `build.rs`, served only on the profile's local Unix socket. It exposes readiness, status and inventory, provider import and metadata, mount and credential reads, one batched mutation lease (`BeginMutation`/`ApplyMutation`/`DropMutation`), recovery and repair, shutdown, Inspector subscription, and bounded raw log streaming. Unary messages and stream items are each limited to 1 MiB; log tails are limited to 10,000 lines. Ordinary finite calls have a 5-second client deadline, the daemon's single mutation lease is fixed at 30 seconds with no renewal, and shutdown has 15 seconds around its 10-second filesystem drain. Credential material may cross only in request payloads on this local socket. It never crosses filesystem attach/TCP, appears in responses, status, inventory, logs, Debug, or Inspector output.

The daemon listens on the profile's fixed `control.sock`. The profile directory is `0700`; the socket and process identity are `0600`. The VFS namespace is separate: `daemon-state/local.sock` and one profile-derived loopback or Docker-bridge TCP port. TCP has no auth and never binds all interfaces. Both VFS listeners must bind before readiness, and either listener's unexpected exit is fatal.

The process identity is diagnostic metadata only. RPC status and inventory are authoritative when reachable. Doctor owns stale process and filesystem cleanup and requires a stopped daemon, consent, and fresh exact identity proof before destructive repair.

## Command grammar

The public binary is one `omnifs` executable. The hidden `omnifs daemon` subcommand runs the daemon; `omnifs run-fs` dispatches a host filesystem. Public commands are:

- `omnifs status`, `down`, `logs`, `inspect`, `doctor`, `setup`, `skill`, `completions`, and `version`.
- `omnifs mount add|ls|show|update|reauth|revoke|rm`. Add, update, and remove each apply one lease-scoped batch through `BeginMutation`/`ApplyMutation`. `mount add` can upload an exact Wasm artifact or select an embedded provider, then folds a fresh credential submission and the mount create into one batch. `mount update` re-reads the mount under its own lease and applies its patch atomically; there is no version-based compare-and-swap. `mount reauth` and `mount revoke` each apply a single-op credential batch.
- `omnifs credential ls|rm`. List returns non-secret daemon-owned credential status. Remove deletes stored material after showing affected mounts and getting consent; it does not revoke access upstream.
- `omnifs fs create|attach|detach|restart|rm|shell|ls --name <id>`. Specs are client-owned configuration. Attachments are daemon-owned live state joined by `fs ls` and `status`.

Global `--output human|json|jsonl`, `--quiet`, `--no-input`, and `--yes` apply to one invocation after Clap parses it. JSON emits one terminal envelope. JSONL emits stream records followed by one terminal result or error. Clap usage errors exit 2 before output mode applies.

`omnifs setup [--providers NAME] [--no-up] [--no-browser]` composes provider selection, credential UX, mount RPC, daemon start, filesystem creation, and attachment. `--no-up` stops after configuration. There is no `omnifs up`, `omnifs apply`, or offline product mode.

`omnifs down` sends `Shutdown { stop_filesystems: true }`, waits for the bounded drain, reports busy attachments, and then stops the daemon. Daemon spawn and replacement never launch or stop filesystem runners implicitly.

## Mounts, providers, and credentials

Mounts are daemon-owned SQL rows with typed definitions, provider content IDs, versions, auth declarations, and limits. `ListMounts` and `GetMount` are the only mount reads; every write (`CreateMountOp`, `UpdateMountOp`, `RemoveMountOp`) is one op inside an `ApplyMutation` batch, applied in one SQLite transaction. There is no per-mount CAS: the daemon's single mutation lease already serializes every writer, so the batch is the only ordering guarantee a client needs. Every row a batch writes is stamped with that batch's `MutationId` (`last_mutation_id`), which is the sole provenance a client uses to tell whether an interrupted request actually committed.

Provider artifacts live in daemon state. `ImportProvider` accepts a bounded upload or an exact embedded provider name, validates the content digest and metadata, and returns a receipt keyed only by content digest. Provider import carries no mutation identity and never touches the mutation lease: a dropped upload is simply retried, and importing identical bytes twice returns `Unchanged` rather than a second row.

Credentials live in the daemon's SQLite store. The CLI owns browser, device, and static-token UX, then folds the collected material into a `SubmitCredentialOp`/`DeleteCredentialOp`/`RevokeCredentialOp` inside a mutation batch (often alongside a mount op, so a fresh sign-in and the mount that needs it commit or fail together). The daemon injects credentials only into host callouts. It does not expose credential values, file paths, or a reload command; status reports only non-secret health.

## Filesystems and attach

`omnifs_core::fs` owns validated filesystem IDs, protocol, runtime, and resolved location. The CLI stores strict specs under `client/filesystems/specs`, per-ID state under `client/filesystems/state`, host logs under `client/cache/filesystem-<id>.log`, and launch roots under `client/filesystems/runtime`. `fs create` resolves defaults once without launching. Host locations are absolute; Docker and libkrun use `/omnifs`.

The CLI owns host process, Docker, and libkrun launch and teardown. Each runner attaches to the daemon's shared `omnifs_vfs::Namespace` and carries no provider or credential state. Attach rejects an existing confirmed instance. Detach proves mount absence and runtime exit. Remove never detaches an uncertain or running instance. `fs shell` selects only the stored runtime and ID.

`VfsServer` owns listener tasks, readiness, reconnect, server-pushed stop, and attachment records. Wire handshakes carry the exact resolved filesystem spec and daemon instance. A reconnect is accepted only for the same ID and exact spec. A failed reconnect past its deadline enters filesystem-owned teardown.

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
- Make the daemon read client filesystem specs or config, or make the CLI read daemon SQLite tables and logs directly.
- Add a remote control endpoint or TCP authentication mode. TCP attach remains local loopback or the detected Docker bridge without auth.
- Let lifecycle commands remove or detach a filesystem implicitly during daemon replacement.

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
