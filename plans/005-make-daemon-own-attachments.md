# Plan 005: Make the daemon own desired attachments and runtime lifecycle

> **Executor instructions**: Read this full plan before editing. Follow each
> verification gate. Attachment teardown is destructive, so stop if exact
> identity or recovery behavior is unclear. Update this plan's status in
> `plans/README.md` when done unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- crates/omnifs-core crates/omnifs-api crates/omnifs-state crates/omnifs-daemon crates/omnifs-vfs crates/omnifs-thin crates/omnifs-cli crates/omnifs-itest`
> Confirm Plans 001 through 004 have landed. Map any renamed resource, runtime,
> and reconcile symbols before proceeding.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**:
  `plans/003-move-provider-work-to-daemon-reconciliation.md`,
  `plans/004-extract-filesystem-runtime-drivers.md`
- **Category**: architecture, lifecycle
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

An `Attachment` resource can only be declarative if one long-lived owner keeps
its process, container, or VM in the desired state. The current CLI stores
filesystem specs and owns every launch and stop. The daemon knows only live
VFS connections.

This plan makes the daemon the sole normal lifecycle owner, keeps filesystems
out of process, and renames live VFS attachments to sessions.

## Current state

- `crates/omnifs-cli/src/client_fs_state.rs:1-5` states that specs and launch
  artifacts are CLI-owned and outside daemon state.
- `crates/omnifs-cli/src/commands/fs.rs:118-147` creates a strict local spec
  without launching.
- `crates/omnifs-cli/src/commands/fs.rs:345-430` imperatively attaches,
  detaches, and restarts.
- `crates/omnifs-cli/src/commands/fs.rs:478-540` launches a runtime and polls
  daemon inventory for its VFS connection.
- `crates/omnifs-vfs/src/server.rs:82-117` calls the live connection registry
  `Attachments`.
- `crates/omnifs-vfs/src/lib.rs:48` sets wire protocol version 10.
- The handshake includes `ClientOwnerId`; tests include owner-scoped duplicate
  filesystem IDs.
- `crates/omnifs-daemon/src/daemon.rs:101-115` requires both VFS listeners
  before readiness.
- `crates/omnifs-core/src/fs.rs` owns the exact current spec.

Keep these invariants:

- every filesystem remains a separate process, container, or VM
- both VFS listeners remain readiness-critical
- reconnect overlap accepts only an exact matching identity
- runtime teardown requires fresh exact identity proof
- guests carry no credentials
- Docker TCP attach stays on the verified local address
- `down` drains live filesystems before daemon exit

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| VFS tests | `cargo nextest run -p omnifs-vfs` | all pass |
| Daemon tests | `cargo nextest run -p omnifs-daemon` | all pass |
| CLI tests | `cargo nextest run -p omnifs-cli` | all pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Docker lifecycle | existing `filesystem_docker` lane | pass when Docker is available |
| Multi-filesystem | existing `multi_filesystem` lane | pass |
| Libkrun | `just libkrun-conformance` | pass on opted-in Apple Silicon |

Preserve `sccache`. Serialize live NFS tests with their existing cross-process
lock.

## Scope

**In scope**:

- `Cargo.toml`
- relevant crate `Cargo.toml` files for dependency moves
- `crates/omnifs-core/src/attachment.rs`
- `crates/omnifs-core/src/client_owner_id.rs`
- `crates/omnifs-core/src/lib.rs`
- `crates/omnifs-api/proto/control/v1/control.proto`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-api/src/grpc.rs`
- `crates/omnifs-api/src/attachment.rs`
- `crates/omnifs-state/migrations/0004_attachment_runtime.sql` (new if Plan 001
  did not create the final observed shape)
- `crates/omnifs-state/src/paths.rs`
- `crates/omnifs-state/src/resource.rs`
- `crates/omnifs-state/src/tests.rs`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/daemon.rs`
- `crates/omnifs-daemon/src/attachment_supervisor.rs` (new)
- `crates/omnifs-daemon/src/progress.rs`
- `crates/omnifs-daemon/src/resource_control.rs`
- `crates/omnifs-daemon/src/control/`
- `crates/omnifs-fs-runtime`
- `crates/omnifs-vfs/src/lib.rs`
- `crates/omnifs-vfs/src/client.rs`
- `crates/omnifs-vfs/src/server.rs`
- `crates/omnifs-vfs/src/tests.rs`
- `crates/omnifs-thin/src/lib.rs`
- `crates/omnifs-thin/src/fuse.rs`
- `crates/omnifs-thin/src/nfs.rs`
- current CLI filesystem commands and status adapters
- relevant filesystem integration tests and scripts

**Out of scope**:

- changing FUSE or NFS projection semantics
- running a filesystem in the daemon process
- KCL
- final public command rename from `fs` to `attachment`
- deleting all old CLI state in this plan
- automatic conversion of detached legacy specs
- a generic job or controller framework
- new runtimes

## Target ownership

```text
SQLite Attachment resource
          |
          v
AttachmentSupervisor
          |
          +-- host process through omnifs-fs-runtime
          +-- Docker container through omnifs-fs-runtime
          `-- libkrun VM through omnifs-fs-runtime
                       |
                       v
                 VFS session registry
```

The daemon owns policy, retries, and tasks. `omnifs-fs-runtime` owns exact
mechanics. `omnifs-vfs` owns live session transport. CLI commands call daemon
RPC and render results.

## Steps

### Step 1: Add daemon-owned attachment paths and observed state

Extend daemon state paths:

```text
daemon-state/runtime/attachments/<name>/
daemon-state/cache/guest-images/
daemon-state/logs/attachments/<name>.log
```

Add typed path accessors. All private directories are mode 0700 and private
files are mode 0600.

Add or extend `attachment_instances` through the next migration if Plan 001
did not already create its final shape:

```text
name
desired_version nullable
observed_version nullable
phase
runtime_instance nullable
action_generation
last_error_code nullable
last_error_detail nullable
retry_at nullable
deleting
updated_at
```

Do not store OS process handles or open sockets in SQLite. Store only strict
identity records needed for recovery.

**Verify**:
state tests prove private paths, phase round trips, deletion tombstone survival,
and corrupt phase rejection.

### Step 2: Add `AttachmentSupervisor`

Create one daemon-owned supervisor with:

- latest resource revision subscription
- serving revision/status subscription
- per-name serialized state
- a global bounded launch semaphore
- retry timers with capped backoff
- owned tasks and joined shutdown
- exact runtime paths
- exact VFS session observation
- a non-blocking publisher into the shared progress hub
- pending typed Attachment actions reloaded from durable action receipts

State transition:

```text
Pending -> WaitingForNamespace -> Starting -> Ready
Ready -> Stopping -> Starting              (restart)
any transient failure -> Retrying
any unsafe identity conflict -> Failed
desired deletion -> Deleting -> absent
```

For each attachment:

1. read the latest exact desired spec
2. require a suitable serving namespace revision
3. probe only the configured runtime
4. adopt only an exact owned runtime
5. launch if absent
6. wait for an exact VFS session on the attachment's owned daemon task
7. record `Ready`

Record each phase before publishing it. Publish the exact Attachment name,
desired revision, runtime kind, stage, retry count when present, and safe
detail. Adapt `RuntimeEvent` into the same stream, including real image byte
counts. Never hold a lifecycle lock or wait for a subscriber while publishing.

On deletion:

1. keep or create an observed tombstone
2. stop the exact runtime
3. prove the OS mount is absent
4. prove the process, container, or VM is absent
5. remove derived runtime files that are safe to remove
6. clear the tombstone

Do not hold a mutex across runtime awaits. One actor or keyed task should own a
name's sequence.

**Verify**:
fake-runtime tests cover every transition, bounded concurrency, retry,
superseded desired versions, deletion, crash recovery, and joined shutdown.

### Step 3: Rename VFS live attachments to sessions

Within `omnifs-vfs`, rename:

- `AttachmentEntry` to `SessionEntry`
- `AttachmentKey` to `SessionKey`
- `AttachedConnection` to `SessionConnection`
- `AttachmentPhase` to `SessionRegistryPhase`
- `AttachmentState` to `SessionState`
- `Attachments` to `Sessions`
- public attachment-list methods to session-list methods

Do not change reconnect behavior in this step.

Update daemon status internals so `sessions` means live wire connections and
`attachments` means desired resources.

**Verify**:
`rg -n "AttachmentEntry|AttachmentKey|AttachedConnection|struct Attachments" crates/omnifs-vfs`
returns no matches, and `cargo nextest run -p omnifs-vfs` passes.

### Step 4: Remove `ClientOwnerId` from normal runtime identity

The daemon is now the sole normal owner. Change VFS protocol v10 to v11.

Handshake identity:

```text
protocol version
attachment name
exact AttachmentSpec
random runtime instance ID
```

Server session key:

```text
attachment name
```

Reconnect overlap requires:

- same attachment name
- same exact `AttachmentSpec`
- same runtime instance, or the current explicit replacement rule during
  supervisor restart

Define the replacement rule in one server method and test it. A new instance
must not silently replace a live old instance unless the supervisor started the
transition.

Remove `ClientOwnerId` from:

- thin CLI args
- Docker command
- host runner launch
- libkrun seed
- VFS client and server

The old mutation lease may still use `ClientOwnerId` until Plan 009. Do not
keep it in any runtime or VFS type, but do not delete the core type or client
owner file while that lease has a production caller.

Keep profile and attachment labels needed for exact Docker discovery.

**Verify**:

- v11 rejects v10
- conflicting specs reject
- unapproved instance replacement rejects
- reconnect overlap for one exact runtime succeeds
- `rg -n "ClientOwnerId|client_owner" crates/omnifs-vfs crates/omnifs-thin crates/omnifs-fs-runtime crates/omnifs-daemon` returns no runtime or VFS matches

### Step 5: Add attachment operational RPCs

Add:

```text
GetAttachmentStatus
RestartAttachment
GetAttachmentAccess
```

Resource create/update/delete continues through `PlanResources` and
`ApplyResources`.

`GetAttachmentStatus` returns the current `action_generation`.
`RestartAttachment` takes that base generation and a client-generated action
ID. In one state-writer transaction it:

1. returns the stored receipt when the same ID and request repeat;
2. rejects action ID reuse with different input;
3. requires the exact base generation;
4. increments the Attachment action generation;
5. stores a pending typed action receipt;
6. commits.

It then sends a non-blocking supervisor wakeup and returns `ActionReceipt`.
The RPC does not wait for teardown, launch, mount, or session. The CLI follows
`WatchProgress(action_id)` until `ActionCompleted` or `ActionFailed`.

The supervisor records action phase and terminal outcome through the state
writer. On daemon restart it resumes a pending accepted restart before
reporting the action terminal. Once a terminal receipt is pruned, the advanced
action generation still prevents a lost-reply retry from restarting twice.

`GetAttachmentAccess` returns:

- a verified host path for host runtime, or
- a typed command description for Docker/libkrun shell access

It returns no shell command string assembled for direct shell evaluation. The
CLI builds `std::process::Command` argv from typed fields.

**Verify**:
API and daemon tests cover unknown attachment, not-ready access, restart
coalescing, lost-reply retry, action ID reuse mismatch, stale action generation,
daemon restart recovery, action progress, disconnect without cancellation,
and secret-free responses.

### Step 6: Switch current `fs` commands to daemon ownership

Keep the public `fs` grammar temporarily so the intermediate tree remains
usable.

Change behavior:

- `fs create` edits desired resources through plan/apply and therefore queues
  attachment realization
- `fs attach` becomes idempotent ensure-present for transition only
- `fs detach` removes the desired attachment
- `fs restart` calls the operational RPC
- `fs rm` becomes the same desired deletion and is removed in Plan 008
- `fs shell` uses `GetAttachmentAccess`
- `fs ls` reads daemon desired and observed status

The underlying apply or restart RPC returns before runtime work. The human
command then follows the desired revision or action ID and waits by default.
Render typed phase changes as stable lines in non-TTY output. A Ctrl-C stops
only the watch and states that daemon work continues, with the exact revision
or action follow target. The public rename and polished renderer arrive in
Plan 008.

Stop writing new files under `client/filesystems`.

Detect legacy specs and report them as legacy config with an explicit import
hint. Do not auto-create Attachment resources because old specs may have been
deliberately detached.

**Verify**:
update CLI tests and transcripts for commit receipt, streamed runtime stages,
terminal ready/failure, and Ctrl-C detach semantics. Add a test that a legacy
detached spec is not launched.

### Step 7: Make `down` preserve desired attachments

On shutdown:

1. reject new apply and operational actions
2. tell `AttachmentSupervisor` to stop all current runtimes
3. wait for the existing bounded drain
4. report any exact stragglers
5. preserve `attachment_resources`
6. join the supervisor
7. continue generation and state shutdown

On next daemon start, the supervisor reloads desired attachments and restores
them.

Daemon replacement must not start a second runtime before it has either adopted
or rejected the exact current one.

**Verify**:
an integration test applies one attachment, reaches ready, runs down, confirms
the runtime stopped and desired row remained, restarts the daemon, and confirms
the attachment returns.

### Step 8: Move normal runtime assets from client to daemon paths

Update runtime driver call sites to use daemon-owned paths for:

- runner records and sockets
- host logs
- NFS protocol-local state
- libkrun root image copy
- libkrun helper record and sockets
- ssh keys
- guest image cache

Do not move files by broad recursive shell operations. Add explicit,
identity-checked migration or leave legacy files untouched and report them to
doctor.

The CLI may keep its metrics and KCL cache under client state.

**Verify**:
fresh-profile tests assert no `client/filesystems` tree is created by a normal
attachment lifecycle.

### Step 9: Update live acceptance

Update these tests to resource-driven lifecycle:

- `crates/omnifs-itest/tests/filesystem_docker/main.rs`
- `crates/omnifs-itest/tests/filesystem_libkrun/main.rs`
- `crates/omnifs-itest/tests/multi_filesystem/main.rs`
- `crates/omnifs-itest/tests/wire_reattach/main.rs`
- `crates/omnifs-itest/tests/attach_tcp/main.rs`

Keep current product checks:

- byte identity
- no credentials in guests
- lockdown
- daemon kill/restart
- reconnect
- two filesystems over one namespace
- down ordering

**Verify**:
run every supported live lane on the host platform.

### Step 10: Run final gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-core -p omnifs-api -p omnifs-state -p omnifs-fs-runtime -p omnifs-vfs -p omnifs-thin -p omnifs-daemon -p omnifs-cli
just check host
just test host
git diff --check
```

Then run supported Docker, multi-filesystem, and libkrun live lanes.

## Test plan

Model fake-runtime supervisor tests after state-machine facts, not helper
layout. Cover:

- desired add, update, delete
- namespace wait
- exact adopt
- conflicting runtime
- retry and capped backoff
- restart
- restart action progress and terminal event
- deletion finalization
- daemon crash/restart
- down preservation
- queue bounds and shutdown

Retain all current runtime identity, lockdown, and filesystem conformance tests.

## Done criteria

- [ ] Daemon is the sole normal attachment lifecycle owner.
- [ ] Filesystems remain out of process.
- [ ] Public desired attachments and internal VFS sessions have distinct names.
- [ ] VFS protocol v11 has no `ClientOwnerId`.
- [ ] Runtime work is bounded and serialized per attachment.
- [ ] Raw apply and restart RPCs return receipts without waiting for runtime
  work.
- [ ] Current `fs` mutation commands wait through the progress stream by
  default.
- [ ] Accepted restart actions and their current phase survive daemon restart.
- [ ] Stale action generations cannot repeat a lost-reply restart.
- [ ] Attachment progress names the runtime stage and uses real byte counts.
- [ ] Slow or disconnected watchers never delay or cancel lifecycle work.
- [ ] Deletion remains visible until exact teardown is proved.
- [ ] `down` stops runtimes and preserves desired attachment rows.
- [ ] Restart restores desired attachments.
- [ ] Normal lifecycle creates no new client filesystem state.
- [ ] All Step 10 and supported live gates pass.

## STOP conditions

Stop and report if:

- A filesystem would need to run in the daemon process.
- Removing `ClientOwnerId` makes two profiles able to claim the same session or
  runtime.
- Exact runtime replacement cannot be fenced during daemon restart.
- Deletion would need to clear desired and observed identity before teardown is
  proved.
- The daemon must read legacy client specs to start normally.
- `down` cannot stop runtimes without deleting desired attachment rows.
- A guest would need credentials, host networking, new mounts, or broader
  libkrun authority.
- Live NFS locking or serialization would need to be weakened.

## Maintenance notes

- Resource presence means desired attached. Do not add a separate `attached`
  boolean.
- Runtime records are recovery data, not desired truth.
- Doctor may inspect legacy and stray state, but normal lifecycle stays in the
  daemon.
- Plan 008 removes the transitional `fs attach|detach|rm` grammar and exposes
  the final `attachment` command.

## Git workflow

- Use a branch such as `codex/005-daemon-attachments`.
- Use Conventional Commits, for example
  `feat(daemon): reconcile attachment runtimes`.
- Do not push or open a pull request unless the operator asks.
