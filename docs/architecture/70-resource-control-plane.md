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

## Reconciliation

The daemon constructs one required-cache `ComponentEngine` and shares it with
provider preparation and `HostOnline`. Bounded preparation starts for embedded
providers before SQLite opens. Desired and retained digests join the same
deduplicated queue after state becomes available. Preparation drops temporary
components, while the active generation retains only the providers it uses.

The serving reconciler builds only the latest desired revision. A failed build
leaves the last good generation active. `AttachmentSupervisor` separately
reconciles desired Attachment specs into exact out-of-process host, Docker, or
libkrun runtimes. Durable observed rows and deletion tombstones let it adopt,
stop, or replace exact runtime instances after daemon restart.

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
