# Declarative resource control plane

Status: current-architecture

The control plane has one source of desired truth: the normalized Provider,
Credential, Mount, and Attachment resource set in SQLite. Clients read the
current revision, submit a full candidate set to `PlanResources`, and commit
it through `ApplyResources` with the base revision, expected digest, and a
client-generated mutation ID. The durable receipt makes a lost reply safe to
retry. One versioned `resource_state` row stores the canonical complete set,
its digest, and its revision. There are no per-kind desired tables, backfill
readers, or compatibility migrations.

`ApplyResources` ends after validation, one SQLite transaction, and a
non-blocking reconcile wakeup. Provider fetch or compilation, credential
activation, generation publication, runtime launch, OS mounting, and VFS
session waits all happen in daemon workers after the transaction. A client
that wants the ordinary synchronous command experience follows
`WatchProgress`; disconnecting that stream never cancels daemon work.

This split keeps the durable decision small and recoverable. A revision says
what should exist, not that compilation, publication, or mounting already
finished. Reconciliation can resume from SQLite after a client disconnect or
daemon restart without replaying a client journal.

## Reconciliation

The daemon constructs one required-cache `ComponentEngine` and shares it with
provider preparation and `HostOnline`. Bounded preparation starts for embedded
providers before SQLite opens. Desired and retained digests join the same
deduplicated queue after state becomes available. Preparation drops temporary
components, while the active generation retains only the providers it uses.
The cache is required because compilation is normal daemon work, not an
optional optimization. One engine keeps cache identity and Wasmtime settings
consistent across preparation and serving; retaining only the active
generation avoids turning catalog warm-up into an unbounded component store.

The serving reconciler builds only the latest desired revision. A failed build
leaves the last good generation active. `AttachmentSupervisor` separately
reconciles desired Attachment specs into exact out-of-process host, Docker, or
libkrun runtimes. Durable observed rows and deletion tombstones let it adopt,
stop, or replace exact runtime instances after daemon restart.

Revision wakeups are notifications, not a durable work ledger. Every
reconciler reloads current SQLite state and converges on the newest applicable
revision. Provider phase maps are process-local; the required Wasmtime cache is
the compiled-artifact authority, and cache filenames alone prove nothing.
Compilation runs on its own bounded blocking pool. Workers record phase state
before publishing best-effort events so a fresh snapshot remains sufficient
after reconnect.

Any future cache pruning belongs in a daemon worker coordinated with provider
preparation. It cannot run in a control handler, delay raw apply, or replace
honest preparation stages with inferred cache-hit claims.

Each long-lived reconciler owns its spawned work, admission bound,
cancellation, and join path. Shutdown stops new control writes and launches,
drains exact Attachment runtimes and VFS sessions, then joins serving and
provider preparation. Detached work may outlive a client stream, never its
daemon owner.

## Progress and actions

Each progress subscription registers before taking its complete snapshot, so
an update cannot fall between snapshot and subscription. Fanout is bounded and
non-blocking. A slow subscriber receives a resync snapshot. Revision streams
carry only provider, credential, serving, and Attachment work that can affect
that revision; unused catalog warm-up appears only on the current stream.

Credential material changes, upstream revocation, and Attachment restart use
client-generated action IDs plus action-generation preconditions. SQLite
allows one non-terminal action per target and retains accepted actions across
daemon restart. Secret bytes never enter resources, KCL, receipts, progress,
status, logs, hashes, or dedupe keys. For secret actions, the first accepted
action ID owns the supplied material.

## Client role

Interactive commands and KCL automation converge on the same typed plan,
apply, and progress path. KCL runs in process and serves only as temporary
client interchange before strict Rust validation. The CLI owns prompts, local
provider path resolution, secret collection, and rendering. Users author KCL
directly; the CLI does not keep a second KCL renderer or schema asset. It does
not own desired state or filesystem runtime lifecycle.

The narrow bootstrap layer exists only because a client must locate or spawn
the daemon before RPC is available, and Doctor must prove exact process
identity when SQLite is missing or corrupt. `Profile`, `SpawnLock`, and
`DaemonIdentity` cover that boundary. Daemon-state layout and desired resources
do not belong in bootstrap.

## Rejected prior control plane

The former control plane split authority between imperative mutation RPCs,
lease-scoped batches, a client recovery journal, client-owned filesystem specs,
and runtime launch code in the CLI. A request could mix the durable decision
with provider and filesystem work. Recovery then depended on which client
files, daemon rows, and live processes happened to survive.

That design was removed rather than kept as a compatibility path:

- Complete-set resource apply replaced imperative per-kind mutations and the
  mutation lease.
- SQLite receipts replaced the client journal and snapshot handoff.
- Daemon reconciliation replaced client-owned provider and filesystem
  lifecycle.
- Strict `AttachmentSpec` plus durable runtime identity replaced client
  filesystem registries and owner IDs.
- Required shared compilation caching replaced optional or fallback engine
  construction.
- KCL became client input to strict Rust declarations, not another schema or
  state authority.

Do not restore readers, scanners, migrations, aliases, or hidden commands for
that model. If interoperability with an old release ever becomes a product
requirement, design it as an explicit bounded import boundary rather than a
second active control plane.
