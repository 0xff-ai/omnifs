# Declarative resource control plane

Status: current-architecture
Scope: why desired resources commit separately from daemon reconciliation,
runtime work, progress, and actions.

Read when: changing resource planning or apply, SQLite desired state, provider
preparation, serving generations, Filesystem reconciliation, progress streams,
durable actions, or client and daemon ownership.

Binding contracts: `docs/contracts/50-control-plane.md` and
`docs/contracts/60-build-validation.md`.

SQLite holds one normalized Provider, Credential, Mount, and Filesystem set.
Clients plan a complete candidate and call `ApplyResources` with its base
revision, digest, and mutation ID. One versioned `resource_state` row stores
the set, digest, and revision; a durable receipt makes reply-loss retries safe.
There are no per-kind desired tables or compatibility readers.

`ApplyResources` validates, commits one transaction, wakes reconcilers, and
returns. Daemon workers perform provider preparation, credential activation,
generation publication, runtime launch, mounting, and VFS waits. Clients follow
`WatchProgress`; disconnecting never cancels work.

A revision states desired existence, not completed runtime work. Reconciliation
resumes from SQLite without a client journal.

## Reconciliation

One required-cache `ComponentEngine` serves preparation and `HostOnline`.
Embedded preparation starts within a bound before SQLite opens; desired and
retained digests later join the same deduplicated queue. Preparation drops
temporary components, while the active generation retains only its providers.
One engine keeps cache identity and Wasmtime settings consistent.

The serving reconciler builds only the latest desired revision. A failed build
leaves the last good generation active. `FilesystemSupervisor` separately
reconciles desired Filesystem specs into exact out-of-process host, Docker, or
libkrun runtimes. Durable observed rows and deletion tombstones let it adopt,
stop, or replace exact runtime instances after daemon restart.

Wakeups are notifications, not a work ledger. Reconcilers reload SQLite and
converge on the newest revision. Provider phases are process-local; the
Wasmtime cache, not filenames, proves compiled artifacts. Compilation uses a
bounded blocking pool. Workers record state before best-effort events so a new
snapshot repairs stream loss.

Cache pruning belongs in a daemon worker coordinated with preparation, never a
control handler or inferred progress stage.

Each long-lived reconciler owns its spawned work, admission bound,
cancellation, and join path. Shutdown stops new control writes and launches,
drains exact Filesystem runtimes and VFS sessions, then joins serving and
provider preparation. Detached work may outlive a client stream, never its
daemon owner.

## Progress and actions

Subscriptions register before reading their full snapshot. Fanout is bounded;
slow consumers receive a resync snapshot. Revision streams include only work
that can affect that revision; unused warm-up appears only in current status.

Credential material changes, upstream revocation, and Filesystem restart use
client-generated action IDs plus action-generation preconditions. SQLite
allows one non-terminal action per target and retains accepted actions across
daemon restart. Secret bytes never enter resources, KCL, receipts, progress,
status, logs, hashes, or dedupe keys. For secret actions, the first accepted
action ID owns the supplied material.

## Client role

Interactive commands and KCL use the same typed plan, apply, and progress path.
KCL runs in process as temporary interchange before strict Rust validation.
The CLI owns prompts, local provider paths, secrets, and rendering, but no
desired state, KCL schema copy, or filesystem lifecycle.

The narrow bootstrap layer exists only because a client must locate or spawn
the daemon before RPC is available, and Doctor must prove exact process
identity when SQLite is missing or corrupt. `Profile`, `SpawnLock`, and
`DaemonIdentity` cover that boundary. Daemon-state layout and desired resources
do not belong in bootstrap.

## Rejected prior control plane

The former model split authority among imperative RPCs, leased batches, a
client journal, client filesystem specs, and CLI runtime launch. It mixed the
durable decision with runtime work and made recovery depend on several owners.

That design was removed rather than kept as a compatibility path:

- Complete-set resource apply replaced imperative per-kind mutations and the
  mutation lease.
- SQLite receipts replaced the client journal and snapshot handoff.
- Daemon reconciliation replaced client-owned provider and filesystem
  lifecycle.
- Strict `FilesystemSpec` plus durable runtime identity replaced client
  filesystem registries and owner IDs.
- Required shared compilation caching replaced optional or fallback engine
  construction.
- KCL became client input to strict Rust declarations, not another schema or
  state authority.

Compatibility readers, scanners, migrations, aliases, or hidden commands would
restore that split authority. Any future interoperability with an old release
fits an explicit bounded import boundary, not a second active control plane.
