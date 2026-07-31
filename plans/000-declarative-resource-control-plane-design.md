# Declarative resource control plane design

Status: proposed

Planned against commit `035952bc7` on 2026-07-30.

This document defines the target design. It is not a statement about the
current code. The implementation plans in this directory move the current
system to this design in buildable steps.

## Decision

Omnifs will have one daemon-owned desired state made of four typed resources:

- `Provider`
- `Credential`
- `Mount`
- `Attachment`

The daemon will own resource storage, planning, apply, provider preparation,
namespace publication, and attachment lifecycle. The CLI will own user
interaction, KCL evaluation, OAuth, secret collection, and rendering.

Interactive commands and KCL will use the same typed `PlanResources` and
`ApplyResources` API. There will be one planner, one durable apply point, and
one set of reconcilers.

`ApplyResources` will return after one SQLite transaction and a non-blocking
reconcile wakeup. It will never wait for provider compilation, provider
instantiation, namespace generation drain, image download, process launch,
container launch, VM launch, OS mount, or VFS attachment.

After apply returns, the CLI will open a separate reconciliation stream for
the committed desired revision and wait by default. Human and JSONL output
will show real provider, namespace, image, runtime, and attachment phase
changes as they happen. The daemon owns the work, so it continues if the
client disconnects. This is what "background reconciliation" means here:
background to the transaction and owned by the daemon, not hidden from the
user.

The Wasmtime component cache is required. The daemon will construct one
`ComponentEngine` as soon as it owns the profile control endpoint and can
derive the daemon-state cache path. It will start compiling embedded providers
before SQLite opens, then add desired and retained providers as soon as state
is available. `HostOnline` will share that same engine. Work will use bounded
concurrency and will be deduplicated by provider digest. The daemon will not
retain every compiled `wasmtime::component::Component` in memory.

## Why change

The current mutation path acknowledges too late:

1. `MutationManager::apply_admitted` commits the batch with
   `StateStore::apply_batch`.
2. It then calls `prepare_and_activate`.
3. `GenerationDraft::prepare` loads mounted provider bytes and builds the full
   mount table.
4. Building a provider runtime calls `Component::new`.
5. The RPC reply waits for all of this.

The key code is in:

- `crates/omnifs-daemon/src/manager.rs:368-420`
- `crates/omnifs-daemon/src/manager.rs:535-590`
- `crates/omnifs-daemon/src/generation_builder.rs:117-194`
- `crates/omnifs-engine/src/runtime/wasm.rs:34-37`

The control client has a five second ordinary request deadline. A durable
mutation can therefore commit, spend more than five seconds preparing a
generation, and make the client report a timeout even though its change took
effect.

The current startup path also prepares the complete generation before the
daemon becomes ready. See `crates/omnifs-daemon/src/app.rs:239-271`. This delays
control-plane readiness and does not start work for unused embedded providers.

## Goals

1. No user waits on a silent or deadline-bound Wasmtime compilation. Commands
   may wait by following a detailed server stream after the commit completes.
2. Daemon startup starts provider preparation at the earliest safe point.
3. Thirty providers cause bounded CPU and memory use, not thirty retained
   components or thirty concurrent compilers.
4. Interactive commands remain the simplest way to make one change.
5. KCL gives automation one full desired-state input.
6. Both paths use the same Rust resource types and daemon planner.
7. The daemon owns every attachment process, container, and VM.
8. Desired and observed state are distinct and visible.
9. Secrets never enter KCL, resource output, status, logs, VFS, or TCP.
10. Existing trust, provider authority, filesystem, and VFS boundaries remain
    intact.
11. No normal command reads or writes client-owned filesystem state.
12. Pre-RPC bootstrap code owns only the facts needed to find, start, prove,
    and connect to the daemon.

## Non-goals

The first version will not add:

- Kubernetes-style labels, annotations, owners, finalizer arrays, or condition
  arrays
- Terraform-style state files, saved plans, modules, providers, or field
  managers
- multiple independently owned apply sets
- server-side KCL evaluation
- remote KCL evaluation
- automatic Git or OCI KCL dependency downloads
- OCI, Git, or URL provider artifact sources
- provider artifact garbage collection
- a general job system
- in-process filesystem implementations
- remote daemon control
- write support in projected files

## Vocabulary

### Resource

A durable desired definition stored by the daemon. A resource has a kind, a
validated name, and a typed spec.

### Provider

A named reference to one retained provider artifact digest. Importing artifact
bytes does not create authority. A mount grants the authority that the provider
can use.

### Credential

A non-secret declaration of a credential slot. It names the provider,
authentication scheme, and account label. Secret material is stored separately
and never appears in the resource set.

### Mount

A desired provider projection in the shared namespace. It references a
`Provider` resource and, when needed, a `Credential` resource.

### Attachment

A desired OS-facing exposure of the complete shared namespace. It replaces the
user-facing configured filesystem concept. Its runtime still creates a
filesystem process, container, or VM.

While the daemon runs, a present `Attachment` means "keep this attached." If
the resource is absent, the daemon tears down its runtime. `omnifs down` is the
one exception: it stops live runtimes while preserving desired attachments so
the next daemon start can restore them.

### Filesystem

The protocol implementation that realizes an attachment. FUSE and NFS remain
filesystem adapters. They still run out of process.

### VFS session

One live connection between a filesystem runner and `VfsServer`. Current VFS
code calls this an attachment. It must be renamed to `VfsSession` so the word
attachment has one public meaning.

### Reconcile

Move observed runtime state toward the latest durable desired revision.

### Action

A typed durable operational request that does not change the desired resource
set, such as credential revoke or Attachment restart. An action has a
client-generated ID, one closed kind, one target, current phase, and a
terminal outcome. It is not a fifth resource or a general job abstraction.

### Profile bootstrap

The small pre-RPC contract that lets a client and daemon agree on one profile,
one control socket, and one daemon instance before SQLite or gRPC is usable.
It is process coordination, not desired state.

## Pre-RPC bootstrap

The bootstrap layer cannot disappear completely. A client must find and, when
needed, start the daemon before it can use the daemon-owned resource API. The
daemon must also publish enough exact identity for safe stale-state diagnosis
when the control socket cannot answer.

The current crate carries the right low-level facts, but its central
`Bootstrap<R>` type is broader and more abstract than the target needs:

- `Client` and `Daemon` marker types do not form a real security boundary.
  Any workspace caller can construct either role.
- `omnifs-state` accepts `Bootstrap<Daemon>` even though it needs only the
  daemon-state root.
- daemon logging and `DaemonContext` resolve the profile separately, then use
  a `OnceLock` to detect divergence.
- the name `bootstrap_dir` hides that this is the profile root.

The target keeps the `omnifs-bootstrap` crate but removes `Bootstrap<R>`,
`Client`, `Daemon`, and `PhantomData`. Renaming or deleting the crate would
only move the same pre-RPC Unix and process code into both the CLI and daemon.
The target API centers on one plain resolved value:

```text
Profile
  root
  control_socket
  process_identity_path
  spawn_lock_path

SpawnLock
DaemonIdentity
```

`Profile` owns environment resolution from `OMNIFS_HOME` or `$HOME/.omnifs`
and the three fixed coordination paths. The crate owns:

- private profile-directory creation
- the cross-process daemon spawn lock
- symlink-safe stale control-socket handling and binding
- atomic daemon identity publication
- PID start-time and executable proof
- exact unpublication that cannot remove a replacement daemon's files

It does not own or derive paths for SQLite, Wasmtime, projection data,
attachment runtimes, logs, metrics, KCL, or legacy client files. Their actual
owners receive the profile root and derive their own private layout.

The daemon process resolves `Profile` once. The same value goes to logging,
`DaemonContext`, control binding, and daemon-state construction. This removes
the current second resolution and `RESOLVED_PROFILE` check. `StateStore::open`
and `open_daemon_log` take an explicit daemon-state root, so
`omnifs-state` no longer depends on `omnifs-bootstrap`.

The CLI's daemon launcher owns the start decision. It holds `SpawnLock` across
the control probe, spawn, and readiness result. `RpcClient` needs only the
resolved control socket. Doctor may use `DaemonIdentity` only when RPC cannot
answer, and it must keep the spawn lock while making an exact destructive
repair.

`process.json` remains narrow diagnostic state on disk. It cannot move into
SQLite because it must work when the control store is missing or corrupt. It
never becomes a Resource and never overrides a reachable RPC response.

## Resource shape

The KCL result and Rust authoring type use one root object:

```text
ResourceDeclarations
  apiVersion: "omnifs.dev/v1alpha1"
  resources:
    - kind: Provider
      name: github
      spec: ...
    - kind: Credential
      name: github-default
      spec: ...
    - kind: Mount
      name: github
      spec: ...
    - kind: Attachment
      name: local
      spec: ...
```

Resource identity is `(kind, name)`. Input order has no meaning. Duplicate keys
are invalid. Unknown fields are invalid.

The API will use typed Rust and protobuf variants, not a generic JSON resource
body. Provider-specific mount config remains JSON because the provider
manifest owns that schema. The daemon validates it against retained provider
metadata before apply.

### Provider

Normalized daemon form:

```text
Provider
  name: ResourceName
  artifact: ProviderId
```

KCL authoring supports three client-only source forms:

```text
embedded: "github"
local:
  path: "./github.wasm"
  digest: "blake3:<hex>"
digest: "blake3:<hex>"
```

The CLI resolves `embedded` and `local` to a digest before planning. A local
path never enters daemon state. A local source must include the expected
digest. The existing import RPC verifies the streamed bytes and imports them
idempotently. A speculative `omnifs plan` may retain an otherwise unreferenced
content-addressed artifact so the daemon can run its real metadata and mount
validation. This does not change desired state or grant authority.

The daemon queues provider preparation after every inserted or repaired
artifact. It does not wait for preparation before replying to import.

### Credential

```text
Credential
  name: ResourceName
  provider: ResourceName
  scheme: String
  account: String
```

The resource does not contain tokens, refresh tokens, OAuth client secrets,
environment variable names, file paths, or a sourcing method.

Secret material is a separate request-only write tied to the credential
resource name. Interactive `mount add` may submit a new credential resource and
its secret sidecar in one apply request. KCL apply sends no secret sidecars.
Automation gets one narrow command for secret input, such as:

```text
omnifs credential set github-default --from-env GITHUB_TOKEN
```

The command reads the named environment variable on the client and sends the
secret only in a request on the local control socket. It never prints the
value.

Removing a `Credential` resource schedules local secret deletion. The daemon
first publishes and drains a namespace generation that no longer uses the
secret. Upstream revocation remains a separate explicit action.

A desired set may not leave a Mount reference pointing at a removed
Credential. Interactive removal either refuses while references remain or
clears those Mount references in the same reviewed plan. A mount with no
credential may remain desired in an auth-required phase.

### Mount

```text
Mount
  name: ResourceName
  provider: ResourceName
  credential: ResourceName?
  config: provider-owned JSON object
  limits:
    maxMemoryMb: u32?
    maxFetchBlobBytes: u64?
```

The daemon resolves the provider resource to an exact artifact digest and
validates:

- the provider exists
- any credential exists
- the credential points at the same provider
- the scheme is declared by provider metadata
- provider config is valid
- resource limits and capability grants satisfy host rules

The resolved mount spec remains the runtime grant authority.

The stored Mount resource version covers its declared references and config.
The serving reconciler also computes an effective `MountVersion` from the
resolved provider digest, credential generation, config, limits, and grants.
Changing a Provider resource digest therefore changes every dependent
effective mount even when the Mount row itself did not change.

Credential material keeps its current provider auth fingerprint. If a Provider
resource moves to an artifact whose auth fingerprint no longer matches, the
credential becomes blocked or needs sign-in. The daemon must not inject old
material into the new provider.

### Attachment

Authoring form:

```text
Attachment
  name: ResourceName
  protocol: fuse | nfs
  runtime:
    host:
      location: absolute path?
    docker:
      image: exact image reference?
    libkrun:
      guestImage: exact image reference?
```

The daemon owns platform defaulting. `PlanResources` resolves missing values
into an exact attachment spec before calculating its digest. Apply sends the
same declarations and expected digest; the daemon normalizes again and rejects
any mismatch.

The stored form contains exact protocol, runtime, location, and runtime asset
references. A later environment or profile-config change cannot alter a
running attachment without a new apply.

## One desired set

SQLite is the daemon's desired-state authority. A KCL file is a client input,
not a second state database.

An interactive command:

1. reads the current desired set
2. makes one typed edit in memory
3. asks the daemon to plan that complete set
4. shows the relevant diff and asks for consent
5. applies that exact set against the returned base revision
6. follows that revision's reconciliation stream to a ready, failed, or
   superseded result

A KCL apply:

1. evaluates the file once
2. parses the result into strict Rust authoring types
3. resolves and imports any local or embedded provider artifacts
4. asks the daemon to plan the complete set
5. shows the full diff and asks for consent
6. applies the same authoring value, base revision, and desired digest
7. follows that revision's reconciliation stream

If an interactive command changes daemon state after a KCL file was last
applied, the next KCL plan shows that drift. Applying the file replaces the
complete desired set.

The first version has no field ownership or partial apply sets. This is a local
single-user daemon. Adding those systems now would add conflict rules without
a current user.

## Control API

The local gRPC service adds:

```text
GetResources() -> ResourceSnapshot

PlanResources(
  declarations
) -> ResourcePlan

ApplyResources(
  mutation_id,
  base_revision,
  expected_desired_digest,
  declarations,
  credential_secret_sidecars[]
) -> ApplyReceipt

WatchProgress(
  target: desired_revision | action_id | current
) -> stream ProgressEvent

SetCredentialMaterial(
  action_id,
  base_action_generation,
  credential_name,
  secret_request
) -> CredentialReceipt

RevokeCredential(
  action_id,
  base_action_generation,
  credential_name
) -> CredentialReceipt

RestartAttachment(
  action_id,
  base_action_generation,
  attachment_name
) -> ActionReceipt

GetAttachmentAccess(
  attachment_name,
  shell_request
) -> AttachmentAccess
```

`GetResources`, `PlanResources`, and `ApplyResources` carry typed protobuf
messages. No resource request is an untyped JSON document.

### Resource snapshot

```text
ResourceSnapshot
  revision: u64
  desiredDigest: [u8; 32]
  resources: normalized typed resources
```

### Plan

```text
ResourcePlan
  baseRevision: u64
  desiredDigest: [u8; 32]
  normalized: normalized typed resources
  changes:
    - key
    - action: create | update | delete
    - before summary?
    - after summary?
    - destructive: bool
  warnings
```

The plan includes no secrets. A credential material change is shown only as
"credential material will be set" or "credential material will be removed."

### Apply receipt

```text
ApplyReceipt
  mutationId: [u8; 16]
  revision: u64
  desiredDigest: [u8; 32]
  created: u32
  updated: u32
  deleted: u32
  reconciliation: queued | unchanged
```

The receipt does not claim that a provider, mount, or attachment is ready.
For restart, the CLI generates an action ID and sends the Attachment's current
action generation. The daemon durably stores the accepted action, increments
that generation, sends a non-blocking supervisor wakeup, and returns the same
ID. Retrying the same ID and same request returns the first receipt; reusing it
for different input fails. If an old receipt has been pruned, the generation
precondition prevents the same restart from running twice.

```text
ActionReceipt
  actionId: [u8; 16]
  kind: credential_set | credential_revoke | attachment_restart
  target: ResourceKey
  actionGeneration: u64
  phase: accepted | running | retrying | ready | failed
```

The receipt contains no operation payload. `CredentialReceipt` adds only safe
credential health facts to this correlation.

The CLI also generates credential action IDs, and `CredentialReceipt` returns
the accepted ID. Setting material, refreshing a credential, and explicit
upstream revoke do not change the desired resource revision, so their commands
follow that action instead. The local durable credential transition completes
before the receipt. Namespace refresh, drain, and upstream work happen on
daemon-owned tasks and publish action progress.

Credential status carries an action generation. Credential action acceptance
uses the same atomic base-generation check as Attachment restart, so pruning an
old terminal receipt cannot make a lost-reply retry repeat an upstream action.
For set-material, the first accepted action ID wins. Retries never compare,
hash, or reapply later secret bytes; changing material requires a new action
ID.

### Progress stream

`WatchProgress` is one typed server stream with three explicit targets:

- `desired_revision` for apply and interactive resource mutations;
- `action_id` for operational actions such as Attachment restart;
- `current` for `omnifs status --follow`, which runs until canceled.

The command flow is:

```text
CLI                         daemon control               daemon workers
 |                               |                            |
 |-- ApplyResources ------------>|                            |
 |<-- durable revision receipt --|-- non-blocking wakeup ---->|
 |                               |                            |
 |-- WatchProgress(revision) --->|-- subscribe + snapshot     |
 |<================ typed progress events ====================|
 |<================ terminal revision event ==================|
```

The double lines are a long-lived server stream. The stream observes work; it
does not own or cancel it.

The handler registers its bounded receiver before reading the current status,
then sends a complete initial snapshot followed by live changes. This closes
the apply-to-subscribe race without a durable event log.

Each item carries the daemon instance ID, a monotonic stream sequence, the
requested target, and one typed value:

```text
ProgressEvent
  Snapshot
    resources and their current phases
    serving desired and published revisions
    active provider preparations
    active attachment operations
  ProviderPreparation
    provider resource names[]
    catalog name
    digest prefix
    stage enum
    queue rank?
    completed providers
    total providers
  ServingProgress
    desired revision
    stage enum
    provider and mount counts?
  CredentialProgress
    credential name
    action ID
    stage enum
    safe outcome?
  AttachmentProgress
    attachment name
    desired revision or action ID
    runtime
    stage enum
    completed bytes?
    total bytes?
    retry count?
  RevisionReady
  RevisionFailed
    failed or blocked resource summaries
  RevisionSuperseded
    newer desired revision
  ActionCompleted
  ActionFailed
    stable error code and safe detail
  Resync
    complete current snapshot after receiver lag
```

These are closed protobuf variants with closed stage enums, not log records or
free-form field maps. Events report facts the daemon or runtime adapter can
prove.

The first stage enums are:

```text
ProviderStage
  Queued | ReadingArtifact | PreparingComponent | Retrying | Ready | Failed

ServingStage
  WaitingForProvider | BuildingGeneration | Publishing | Draining |
  Ready | Retrying | Failed

CredentialStage
  MaterialStored | WaitingForGeneration | Draining | RevokingUpstream |
  Ready | Retrying | Failed

AttachmentStage
  WaitingForNamespace | ResolvingImage | DownloadingImage | StartingRuntime |
  WaitingForMount | WaitingForSession | Ready | Stopping | Deleting |
  Retrying | Failed
```

Provider preparation names every Provider resource in the watched target that
resolves to the digest, plus the artifact's catalog name and digest prefix.
For global warm-up with no resource, the catalog name and digest are enough.
Queue counters count unique digests, not resource aliases. Stages include
`queued`,
`preparing component`, or `ready`. Wasmtime does not expose honest percent
completion inside `Component::new`, so the UI shows an indeterminate provider
stage instead of a made-up percentage. If the implementation can prove a
cache hit or miss from a supported Wasmtime signal, it may report it. It must
not infer one from elapsed time.

Serving events report waits, generation build, publication, and old-generation
drain. Credential events report only safe lifecycle stages and outcomes.
Attachment events report namespace wait, image fetch with real byte counts
when known, runtime start, OS mount wait, VFS session wait, ready, stop, and
retry. Secret values and provider config never enter event detail.

The stream ends when:

- all resources and deletion tombstones for the target revision are ready;
- a resource reaches a stable failed or blocked phase;
- a newer desired revision supersedes the target;
- the requested operational action succeeds or fails;
- the daemon shuts down; or
- the client disconnects.

A `current` stream has no ready terminal state and runs until shutdown,
disconnect, or user cancellation.

A failure is terminal only when no automatic retry is pending. `Retrying` is a
live stage with its next attempt time and count. When the bounded retry policy
stops, the owner records `Failed` or `Blocked` and the target stream ends
nonzero. The desired state remains applied.

A desired-revision stream includes only resources, provider digests, serving
work, and deletion tombstones that can affect that revision. Preparing an
unused retained or embedded provider appears on the `current` stream but does
not delay or clutter the revision stream. An action stream includes only that
action and its affected resource.

Action IDs have small durable receipt and current-status rows. They are not a
progress-event history or a generic job system. The relevant supervisor still
owns policy, execution, retry, and recovery. A restarted daemon reloads pending
typed actions and `WatchProgress(action_id)` resumes from its current snapshot.
Each target resource has at most one non-terminal action.
Only terminal action receipts may be pruned. An unknown or expired action watch
returns typed `ActionUnavailable` and never repeats the operation. A CLI that
still has the receipt can use its target key to show current resource status.

A disconnect never cancels reconciliation. A reconnect starts with a new
snapshot and resumes live observation. If the bounded receiver lags, the
server sends `Resync` with current state instead of pretending it delivered
every transition.

Stage transitions publish at once. High-rate byte updates are coalesced to a
small fixed maximum rate per operation, with the final exact count always
published. Provider preparation emits only queue, start, and terminal facts,
so 30 providers produce bounded small events rather than compiler trace noise.

### Deadlines

`PlanResources` and `ApplyResources` keep the ordinary five second client
deadline. They are designed to finish well inside it.

`WatchProgress` is not covered by the five second unary deadline. Its
setup has the ordinary connection deadline, then the stream may remain open
while real work continues. Server and client still enforce bounded frames,
bounded buffering, cancellation on disconnect, and daemon shutdown.

The following operations are forbidden in either handler:

- `Component::new`
- provider instantiation or initialization
- provider route compilation
- namespace generation publication or drain
- OAuth network calls
- upstream revocation
- image pulls
- filesystem process, container, or VM operations
- OS mount or unmount calls
- waits for a VFS session

Provider upload remains a bounded streaming RPC outside apply.

## Apply and retry semantics

The daemon's single SQLite writer remains the durable serialization point. A
30 second prompt lease is no longer needed.

Planning reads one transactionally consistent snapshot and returns its
revision. Apply uses compare-and-swap:

1. Normalize and validate the declarations again.
2. Recompute the desired digest.
3. Require it to match the plan digest.
4. In one writer transaction, require the current revision to equal the base
   revision.
5. Apply the typed diff.
6. Increase the global resource revision once if desired state changed.
7. Store an apply receipt keyed by mutation ID.
8. Commit.
9. Send a watch-channel wakeup without waiting for a reconciler.
10. Reply.

The CLI treats that reply as the durable acknowledgement point, then calls
`WatchProgress(desired_revision)`. Waiting belongs to the stream, not the
mutation RPC. A stream failure can therefore never make the commit outcome
unknown.

If the desired digest already equals the current desired digest, apply returns
`unchanged` even if the base revision is stale.

Retries reuse the same mutation ID. A small durable receipt table makes a
repeated request return its first result. The table is bounded by age or count.
Reusing one mutation ID with different input is an error.

The desired digest is not canonical JSON. It is a BLAKE3 hash over an explicit
version tag and sorted, length-delimited typed fields. Provider IDs, credential
fields, existing canonical mount config bytes, and exact attachment fields
feed that encoder.

## Reconciler topology

```text
                         local control socket
                                  |
                         +--------v---------+
                         | ResourceControl  |
                         | plan / apply     |
                         +--------+---------+
                                  |
                         SQLite commit point
                                  |
                    watch resource revision
                 +----------------+----------------+
                 |                                 |
        +--------v-----------+            +--------v-----------+
        | ServingReconciler  |            | AttachmentSupervisor|
        | providers + mounts |            | process/container/VM|
        +----+-----------+---+            +----+------------+---+
             |           |                     |            |
      +------v-----+  +--v---------------+  +--v------+  +--v---------+
      | Provider   |  | ServingCell      |  | runtime |  | VFS session|
      | Preparer   |  | generation swap  |  | driver  |  | registry   |
      +------+-----+  +------------------+  +---------+  +------------+
             |
      required Wasmtime
      disk cache
```

The daemon owns every task and joins it during shutdown.

All three owners publish typed state changes to one bounded
`ProgressEvents` hub. The hub owns current snapshots and live fanout for
`WatchProgress`; it does not own reconcile policy or durable truth.
SQLite resource and observed-state rows remain the recovery source. A slow
subscriber receives a resync snapshot instead of applying backpressure to
provider compilation or attachment lifecycle.

### ResourceControl

Owns normalization, cross-resource validation, plan construction, apply
compare-and-swap, receipt recovery, and reconcile wakeups. It does not own
runtime work.

### ProviderPreparer

Owns every intentional cold provider compilation. Active generation loads
occur only after this owner has marked a digest ready in the required durable
cache.

Rules:

- start after the sole required-cache `ComponentEngine::new`, before SQLite
  opens and before the first serving reconcile
- enqueue every embedded and retained artifact
- enqueue every newly imported or repaired artifact
- deduplicate by `ProviderId`
- prioritize providers referenced by desired mounts
- use a small bounded worker count
- run synchronous Wasmtime preparation through `spawn_blocking`
- call `ComponentEngine::prepare`, then drop the returned component
- mark a digest ready only after `Component::new` succeeds with the required
  cache enabled
- retain only small phase/error records in memory
- never retain all compiled components
- expose a wait/observe handle to the serving reconciler, not to RPC handlers
- emit provider name, digest prefix, queue position, stage, duration, and
  terminal outcome to `ProgressEvents`

On restart, the preparer checks every digest again. A warm `Component::new`
loads from the durable cache. This gives one uniform path and detects cache
damage.

The preparer belongs to the process-level daemon owner so embedded preparation
continues while SQLite is in recovery. Once state opens or is repaired, the
same preparer accepts desired and retained artifact work. Every exit path joins
it, including startup that never reaches full readiness.

### ServingReconciler

Owns provider readiness dependencies and namespace generation ordering.

It processes one desired revision at a time:

1. Read the latest complete serving snapshot.
2. Wait for required provider digests to reach `Ready`.
3. Build a generation outside the control request on an owned daemon task.
4. Before publish, re-read the current desired revision.
5. If the revision changed, discard the stale build and start from the latest.
6. Publish the generation.
7. Drain the old generation.
8. Record the serving revision and per-resource phase.

Only the latest desired revision may publish. The last good generation remains
active while a newer revision prepares or fails.

The reconciler emits each wait dependency, generation-build start and finish,
publication, and drain result. It does not emit one event per filesystem
request or provider call.

Credential refresh and revoke wakeups carry their action correlation. The
reconciler publishes safe credential phase changes and ends the action only
after the new generation is active and required drain or revoke work has a
stable result.

Daemon control readiness does not wait for the first full generation. Startup
publishes a cheap empty or preparing namespace, binds both required VFS
listeners, and starts reconcilers. A later generation publication emits the
normal root invalidation.

### AttachmentSupervisor

Owns one desired attachment per name and all runtime actions needed to realize
it.

It waits until the serving reconciler has published a suitable namespace
revision, then probes or launches the exact runtime. Work is bounded across
attachments and serialized per attachment.

The supervisor and runtime event adapter emit real image byte progress when
the source reports content length, plus runtime start, mount wait, session
wait, retry, stop, and ready transitions.

An attachment phase is one of:

```text
Pending
WaitingForNamespace
Starting
Ready
Stopping
Retrying
Failed
Deleting
```

The supervisor persists enough identity to recover after a daemon crash.
Runtime drivers still use exact identity checks before stop or cleanup.

Deleting an attachment is asynchronous. The desired row is removed at apply,
but an observed tombstone remains until the OS mount is absent and the exact
process, container, or VM has stopped. Status shows `Deleting` during this
period.

`RestartAttachment` is an operational action. It does not change desired
state. `attachment shell` is also operational and asks the daemon for the
runtime-specific access command or host location.

## Provider preparation and serving

Provider preparation and active serving have different memory needs:

```text
All embedded and retained providers
        |
        | bounded prepare, one digest at a time per worker
        v
Wasmtime compiled-component cache on disk
        |
        | load only providers needed by active mounts
        v
Active generation components in memory
```

Preparing 30 providers causes up to the configured worker limit in memory at
once. After preparation, only components used by the active serving generation
remain in memory.

There is no cache-disabled production mode. `ComponentEngine::new(cache_dir)`
already requires a cache path. Daemon startup must fail if it cannot create and
open the private cache. The daemon constructs it once and shares its clone with
the preparer and `HostOnline`.

The serving reconciler never asks an unprepared provider to start. Generation
construction may still call `Component::new`, but it does so only after
preparation, with the same required cache, and on the daemon reconcile path.
An unexpected cache miss can therefore cost reconcile time,
never control-request time. The old serving generation remains active.

## Attachment runtime ownership

The current CLI modules mix lifecycle policy, runtime drivers, and terminal
output. The target split is:

```text
omnifs-cli
  prompts, OAuth, KCL, plan display, receipts, shell exec

omnifs-daemon
  desired state, attachment policy, retries, phases, task ownership

omnifs-fs-runtime
  exact host/Docker/libkrun probe, launch, stop, cleanup primitives
  no terminal rendering and no desired-state policy

omnifs-thin / omnifs-fuse / omnifs-nfs
  out-of-process filesystem protocol implementations
```

The new internal runtime crate is justified because normal daemon control and
stopped-daemon doctor repair both need the same exact identity checks.

`ClientOwnerId` has no purpose after one daemon owns all live runtimes. VFS
protocol v11 will replace it with the attachment name plus a random runtime
instance ID. `VfsServer` will key sessions by attachment name and exact spec,
and will retain its reconnect-overlap rules.

## Removing client filesystem state

`crates/omnifs-cli/src/client_fs_state.rs` is not a target abstraction. It is a
temporary collection of facts that gain different owners when attachments
become daemon resources.

| Current `client_fs_state` fact | Final owner |
|---|---|
| JSON filesystem spec registry | SQLite `attachment_resources` |
| Per-ID `Claim` lock | daemon per-attachment serialization |
| Stopped-daemon repair exclusion | profile-wide `SpawnLock` plus exact runtime identity |
| Host runner records and sockets | daemon `AttachmentRuntimePaths` |
| Host filesystem logs | daemon attachment log paths |
| libkrun root image, helper record, keys, and sockets | daemon attachment runtime paths |
| Guest image cache | daemon cache paths |
| Default host mount location | daemon attachment normalization |
| Docker and guest image defaults in `[filesystem]` | exact normalized `Attachment` fields |
| Metrics setting lookup | narrow CLI profile config reader |

The migration order matters:

1. Extract low-level runtime drivers so they take explicit paths.
2. Make the daemon own Attachment resources, runtime paths, and lifecycle.
3. Stop every normal command from writing `client/filesystems`.
4. Move metrics config lookup out of `ClientFilesystemState`.
5. Replace normal inventory and Doctor inputs with daemon resource and runtime
   status.
6. Add a read-only legacy scanner for old
   `client/filesystems/specs/*.json`.
7. Delete `client_fs_state.rs`, its registry, claims, errors, and tests.
8. Delete the remaining `client_state.rs` mutation journal and shared
   `client_dir.rs` helpers when the old mutation API is removed.

The daemon never reads legacy client specs during startup or reconciliation.
The CLI reads them only to report legacy state or after the user chooses an
explicit import. It does not update, attach, or delete them. The profile-wide
spawn lock is enough for stopped-daemon Doctor repair because normal runtime
operations now require the daemon and two Doctor processes cannot hold that
lock at once.

The final system creates no `client/` tree. An old tree may remain on disk as
untouched legacy data. KCL files live where the user puts them, and the first
KCL version has no package cache because remote packages are out of scope.

## Deletion ledger

This ledger separates code that disappears from code that only moves. The
implementation is incomplete while an old owner remains reachable in
production.

### Code removed outright

| Current file or symbol | Exact final action |
|---|---|
| `omnifs-bootstrap::Bootstrap<R>` | replace with non-generic `Profile` |
| `omnifs-bootstrap::{Client, Daemon}` | delete both role markers |
| `Bootstrap::{for_client, for_daemon}` | replace with one `Profile::resolve` |
| `Bootstrap::bootstrap_dir` | replace with `Profile::root` |
| `omnifs_bootstrap::Instance` | rename to `DaemonIdentity` |
| `omnifs-daemon::logging::RESOLVED_PROFILE` and `verify_resolved_profile` | delete after one profile value is passed to logging and context |
| `omnifs-state` dependency on `omnifs-bootstrap` | delete; state takes its root path |
| `crates/omnifs-cli/src/client_fs_state.rs` | delete the whole file |
| `ClientFilesystemState` path and config facade | delete |
| client JSON `Registry`, `Claim`, and their error enum | delete |
| `client/filesystems/specs/*.json` as active desired state | stop reading and writing |
| per-filesystem `.locks/<id>.lock` | stop creating; daemon serialization replaces it |
| `client/filesystems/state` and `runtime` as active paths | stop creating; daemon runtime paths replace them |
| `client/cache/filesystem-<id>.log` and client guest-image cache | stop creating; daemon paths replace them |
| `[filesystem].docker_image` and `[filesystem].guest_image` client config | delete; exact Attachment fields replace them |
| `commands/fs.rs` attach polling, runtime confirmation, launch, stop, and client-owner helpers | delete after daemon operational RPCs exist |
| client-side join of configured specs with live VFS state in `inventory.rs` | delete; daemon desired and observed status replaces it |
| setup's sequential create-and-attach loop | delete; one resource apply queues Attachments |
| Doctor's active client registry and per-ID claim use | delete |
| VFS, host, Docker, and libkrun `ClientOwnerId` fields | delete when daemon-owned sessions land |
| `omnifs_core::ClientOwnerId` and `client/owner-id` | delete with the last old mutation-lease caller |
| `client/mutations.json`, mutation lock, and owner journal code | delete with the old mutation API |
| old `BeginMutation`, `ApplyMutation`, `DropMutation`, lease slot, and six imperative ops | delete after resource apply cutover |
| public `fs create\|attach\|detach\|rm` grammar | delete after `attachment` porcelain lands |

### Code moved, then deleted from the CLI

These mechanics remain necessary. Plan 004 moves them to
`omnifs-fs-runtime`; later plans delete the old CLI copies:

| Current CLI code | Final location |
|---|---|
| `filesystem_driver.rs` closed runtime dispatch and exact identity helpers | `omnifs-fs-runtime` |
| `host_fs.rs` host process probe, launch, stop, and stale cleanup | `omnifs-fs-runtime` |
| `docker/` container labels, exact command, inspection, launch, and stop | `omnifs-fs-runtime` |
| `libkrun_runner.rs` helper, VM, seed, image materialization, and stop | `omnifs-fs-runtime` |
| `guest_image_pull.rs` OCI fetch, digest proof, and cache materialization | `omnifs-fs-runtime` |

The extracted code loses `ClientFilesystemState`, `ClientOwnerId`, terminal
`Output`, client config, and desired-state decisions. The daemon supplies exact
paths, exact Attachment specs, attach endpoints, and a typed event sink.

### Code that must stay

The redesign must not delete safety or protocol work under the label of
simplification:

- profile root resolution;
- fixed `control.sock`, `process.json`, and `spawn.lock`;
- private path permissions;
- symlink-safe control socket binding;
- PID start-time and executable proof;
- replacement-safe daemon identity cleanup;
- exact host, Docker, and libkrun runtime identity checks;
- runtime teardown proof;
- FUSE, NFS, `omnifs-thin`, and VFS protocol implementations;
- daemon recovery when SQLite is missing or corrupt;
- read-only discovery of legacy client specs until explicit import is no
  longer needed.

### Complexity removed

The final flow removes these coordination problems:

1. No client desired-state database exists to join with daemon live state.
2. No command takes a client spec lock, then a daemon spawn lock, then probes a
   runtime, then polls VFS attachment.
3. No client owner ID crosses host, Docker, libkrun seed, and VFS handshake
   boundaries.
4. No runtime default can come from a client config file after the daemon has
   normalized a different value.
5. No ordinary RPC client carries spawn and process-identity capabilities.
6. State storage no longer depends on the process bootstrap abstraction.
7. Daemon logging and runtime cannot resolve different profiles.
8. Public Attachment resources and internal VFS sessions no longer use the
   same word.
9. The raw apply RPC never waits for runtime work. CLI setup and apply wait on
   the separate progress stream by default.
10. Doctor has one stopped-daemon exclusion lock and one exact runtime proof,
    not a second active spec registry.

## Exact target code changes by crate

| Crate or area | Required code change |
|---|---|
| `omnifs-bootstrap` | keep the crate; replace role-generic bootstrap with `Profile`, `SpawnLock`, and `DaemonIdentity` |
| `omnifs-core` | add strict resource and Attachment types; remove `ClientOwnerId`; delete active `fs::Spec` and `fs::Id` after handshakes and runtime drivers use `AttachmentSpec` plus `ResourceName` |
| `omnifs-api` | add resource plan/apply, typed progress, and Attachment operational RPCs; remove old lease and imperative messages |
| `omnifs-state` | own `DaemonStatePaths` and resource tables; build paths from one explicit daemon-state root; drop the bootstrap dependency |
| `omnifs-engine` | keep required-cache `ComponentEngine::new`; let `HostOnline` accept the one engine already created by the daemon |
| `omnifs-daemon` | resolve one Profile, start embedded preparation before SQLite, own all reconcilers and Attachment runtimes, and publish desired, observed, and progress state |
| `omnifs-fs-runtime` | own exact host, Docker, and libkrun mechanics with caller-supplied paths and no UI or desired-state policy |
| `omnifs-vfs` | rename live attachments to sessions and replace owner-scoped identity with Attachment name plus runtime instance |
| `omnifs-cli` | retain prompts, auth, KCL, streaming progress rendering, daemon launch, shell execution, metrics, and read-only legacy import; delete active filesystem state and lifecycle |
| `scripts/dev.ts` | submit exact Provider, Mount, and Attachment resources; stop writing filesystem config or client specs |
| tests and docs | replace client-spec fixtures with resource fixtures; keep deliberate legacy fixtures read only; update current contracts after each ownership cutover |

## Storage

SQLite holds authoritative small state. Disk holds caches, large derived
runtime files, logs, and control sockets.

```text
$OMNIFS_HOME/
|
+-- control.sock                 fixed local gRPC socket
+-- process.json                 exact daemon identity for pre-RPC diagnosis
+-- spawn.lock                   cross-process daemon start and repair lock
+-- config.toml                  optional CLI preferences, such as metrics
+-- metrics/
+|   `-- cli.jsonl               best-effort local CLI metrics
+-- client/                      untouched legacy data only, never created
+|   `-- filesystems/specs/      explicit import or Doctor reporting only
+`-- daemon-state/
    |
    +-- control-store/
    |   +-- state.sqlite3
    |   +-- state.sqlite3-wal
    |   `-- state.sqlite3-shm
    |
    +-- cache/
    |   +-- wasmtime/            compiled provider components
    |   +-- projection/          opaque projection cache
    |   +-- git/                 clone cache
    |   `-- guest-images/        immutable libkrun image bases
    |
    +-- runtime/
    |   `-- attachments/
    |       `-- <name>/
    |           +-- host/        runner record and control socket
    |           +-- nfs/         protocol-local filehandle state
    |           `-- libkrun/     helper record, root.raw, ssh key, sockets
    |
    +-- logs/
    |   +-- daemon.log
    |   `-- attachments/
    |       `-- <name>.log
    |
    +-- staging/                 bounded atomic-write staging
    +-- local.sock               VFS Unix listener
    `-- control and helper sockets under exact runtime leaves
```

SQLite contains:

```text
providers                 retained artifact metadata and WASM bytes
resource_state            global desired revision and digest
provider_resources        name -> provider digest
credential_resources      non-secret slot declarations
credentials               secret bytes and lifecycle state
mount_resources           typed desired mounts
attachment_resources      exact desired attachments
attachment_instances      observed runtime identity and deletion state
action_receipts           bounded typed action acceptance and current outcome
serving_state             desired and published namespace revisions
apply_receipts            bounded mutation-id dedupe receipts
```

Provider WASM remains in SQLite in this change. Moving it to a file store is a
separate storage project with no bearing on the control timeout.

Runtime files are not desired configuration. They are records and artifacts
needed to recover, prove identity, connect, or tear down a live runtime. Deleting
them blindly is unsafe, but the daemon can rebuild them after it has proved the
corresponding process is absent.

`control.sock`, `process.json`, and `spawn.lock` are neither runtime files nor
SQLite state. They are the fixed pre-RPC bootstrap surface. The daemon and CLI
must be able to use them when SQLite cannot open.

## KCL client

KCL is an optional client authoring language. A profile does not require an
`omnifs.k` file.

Commands:

```text
omnifs config init > omnifs.k
omnifs config export --format kcl > omnifs.k
omnifs plan omnifs.k
omnifs apply omnifs.k
```

`config export` reads the current daemon resource set. It never exports secret
material.

The CLI embeds the KCL evaluator through its Rust API. Current official KCL
0.12 documentation shows `kcl_lang::API::exec_program` returning an
`ExecProgramResult` with JSON/YAML output:

- <https://github.com/kcl-lang/kcl-lang.io/blob/main/versioned_docs/version-0.12/reference/xlang-api/rust-api.md>
- <https://github.com/kcl-lang/kcl>

The evaluator adapter is private and pinned to an exact KCL commit. Evaluation
runs in `spawn_blocking`. Its JSON result is an in-memory interchange value.
Rust strict types are the runtime authority. Omnifs neither persists nor hashes
the evaluator's JSON text.

Omnifs ships a small KCL schema package so an exported file can use:

```kcl
import omnifs

config = omnifs.Config {
    apiVersion = "omnifs.dev/v1alpha1"
    resources = [
        omnifs.Provider {
            name = "arxiv"
            source = {embedded = "arxiv"}
        },
        omnifs.Mount {
            name = "arxiv"
            provider = "arxiv"
        },
        omnifs.Attachment {
            name = "local"
        },
    ]
}
```

The KCL feasibility plan must first prove how to provide this built-in package
through the current Rust API on every release target. If a built-in import is
not clean, `config init` may emit a plain root object and rely on strict Rust
validation for v1. It must not add a second schema owner through generated Rust
types.

KCL files and local imports run with the user's authority and are trusted input.
Omnifs will not claim to sandbox KCL. `plan` and `apply` will not fetch Git or
OCI KCL packages. KCL supports those dependency forms, but users must vendor
any local dependency before Omnifs evaluates it.

## CLI behavior

### Interactive porcelain

Mutation commands are guided, interactive commands:

```text
omnifs provider add
omnifs provider rm
omnifs mount add
omnifs mount update
omnifs mount rm
omnifs credential login
omnifs credential rm
omnifs credential revoke
omnifs attachment add
omnifs attachment rm
omnifs attachment restart
omnifs attachment shell
```

They ask only for facts they cannot infer. Before consent, each command renders
the same resource diff returned by `PlanResources`.

The interactive mutation commands need no full flag-driven twin. Automation
uses `plan` and `apply`. Read commands remain scriptable and keep structured
output. Secret automation keeps the narrow `credential set --from-env` path.

### Plan output

Human output uses stable create/update/delete markers and text labels:

```text
Plan

  + create  Provider/arxiv
  + create  Mount/arxiv
  + create  Attachment/local

  3 to create, 0 to update, 0 to delete
```

Deleting a credential with local material or deleting an attachment is marked
as destructive. Color is optional and never the sole signal.

### Apply output

```text
✓ desired  revision 12 committed
⟳ provider github    preparing component (1/3)
✓ provider arxiv     ready (2/3)
✓ provider dns       ready (3/3)
⟳ serving            building generation for 3 mounts
✓ serving            published revision 12
⟳ attachment local   waiting for NFS session
✓ attachment local   ready at /Users/me/omnifs

✓ revision 12 ready in 4.8s
```

The first line is the durable apply receipt. Later lines come only from the
progress stream. The CLI must not print "mounted," "ready," or "serving"
merely because the desired transaction committed.

In an interactive TTY, a bounded transient region may show one line per active
operation, capped by one small constant. Provider compiler rows are already
bounded by the worker limit. Extra active work is summarized by count.
Completed items become stable lines. With no TTY, no color, or redirected
output, every phase change is one stable line with no cursor control. Fast work
under the animation delay prints only its completed line. Known discrete
totals use counters. Byte progress appears only when the source reports a real
total. Component preparation remains indeterminate. The TTY may show locally
measured elapsed time for an active stage, but it must not turn elapsed time
into completion, cache, or health claims.

Output modes keep their existing contracts:

- `human` waits and shows live text through the existing progress channel by
  default, while commit and terminal receipts use the normal result channel;
- `jsonl` writes one versioned progress event per line, then one terminal
  result or error;
- `json` waits without incremental stdout and writes exactly one terminal
  envelope containing the final resource snapshot;
- `quiet` waits but prints only the terminal receipt;
- unstructured text never contaminates JSON or JSONL stdout.

Ctrl-C stops only the client-side watch after apply has committed. It restores
the terminal, exits 130, and prints that revision 12 is still reconciling in
the daemon plus the exact follow command:

```text
omnifs status --follow --revision 12
```

The first version does not need a separate detach flag. Cancellation already
detaches safely, and `status --follow` reconnects from a fresh snapshot.
For an operational action, the equivalent hint is
`omnifs status --follow --action <id>`.

Structured modes carry the same fact without loose text. JSONL ends with one
typed canceled envelope after its progress events. JSON emits one canceled
envelope. Both include the durable receipt and a typed follow hint. The receipt
owns the target revision or action ID, while the envelope owns the stable
outcome. A stream transport failure uses the same shape with its stable error
code. A terminal `RevisionFailed`, `RevisionSuperseded`, or `ActionFailed`
exits nonzero; ready or completed exits zero.

### Status

Status shows desired and observed facts:

```text
RESOURCE          DESIRED  OBSERVED  PHASE
Provider/arxiv    r12      r12       ready
Mount/arxiv       r12      r12       ready
Attachment/local  r12      r11       waiting for namespace
```

Each resource has one phase, an observed revision, and an optional stable error
code plus human detail. The first version will not expose a generic condition
array.

`omnifs status --follow` uses `WatchProgress(current)`.
`omnifs status --follow --revision <n>` follows one revision to its terminal
state. `omnifs status --follow --action <id>` follows one durable operational
action. `--revision` and `--action` are mutually exclusive. All
three start with a complete current snapshot, so reconnect never depends on an
in-memory event cursor.

Pending actions survive daemon restart through their durable typed state. If a
terminal receipt has expired, action follow reports `ActionUnavailable` and does
not repeat the action. A caller with the original receipt can then inspect its
target resource.

## Failure and recovery rules

### Apply

- A validation error changes nothing.
- A stale base revision changes nothing.
- A client timeout after commit is recovered by retrying the same mutation ID.
- Apply receipt storage is durable.
- Reconcile failure never changes the acknowledged desired set.
- A progress-stream setup or transport failure does not make the commit
  outcome unknown; the CLI prints the committed revision and reconnect command.
- A stable reconcile failure exits nonzero and says that desired state remains
  applied. It does not claim rollback.
- Ctrl-C detaches observation and does not cancel daemon work.

### Provider preparation

- A failed provider is `Failed` with its digest and error.
- Retrying is bounded and uses backoff.
- Import repair wakes preparation.
- A failed provider does not stop other provider preparation.
- A failed new generation leaves the last good generation active.
- Phase and error changes reach every current progress subscriber without
  blocking the worker.

### Serving

- Only the latest desired revision may publish.
- A stale completed build is dropped.
- A stuck retired generation degrades serving health but does not hold a
  control request open.
- Credential refresh and revoke retain their current drain-before-delete safety.

### Actions

- Acceptance commits the typed action and generation before replying.
- The wakeup is non-blocking because the durable row is the work ledger.
- A lost reply is recovered with the same action ID.
- Pending credential and Attachment actions resume after daemon restart.
- One target cannot have two non-terminal actions.
- Stream disconnect or Ctrl-C never cancels an accepted action.
- Stable action failure records a terminal outcome and does not change desired
  resources.

### Attachments

- Exact identity is checked again immediately before any destructive action.
- Launch failure records phase and retry time.
- Removal remains visible as `Deleting` until teardown is proved.
- The daemon adopts only an exact, owned runtime record.
- Unknown or conflicting runtime state goes to `Failed` and requires doctor.
- `down` stops runtimes, joins supervisors, and preserves desired attachment
  rows.

### Shutdown order

1. Stop accepting new apply and operational action requests.
2. Stop attachment launches and request runtime teardown.
3. Drain VFS sessions for the existing bounded period.
4. Stop serving reconcile and provider preparation admission.
5. Retire and drain the active namespace generation.
6. Join every owned task.
7. Close SQLite.
8. Remove control identity and sockets.

No task may outlive the state, host engine, serving cell, or runtime path owner
it uses.

## Security and trust

The redesign does not change provider authority:

- provider WASM remains untrusted
- artifact import grants no authority
- mounts hold resolved grants
- the host owns callouts and credential injection
- filesystem runners receive no provider credentials
- KCL runs only in the trusted CLI
- secrets cross only request-side local control RPC
- VFS TCP remains local attach traffic and carries no secrets

`ApplyResources` responses, progress events, resource snapshots, plans, status,
logs, debug output, and Inspector events must not contain secret material.

## Transition

This is a pre-alpha one-release API cutover. There will be no long-lived dual
control plane.

During implementation, old tables and RPCs may remain only long enough to keep
each intermediate commit buildable. The final plan removes them.

Daemon SQL migration will preserve:

- provider artifact rows
- mount definitions
- credential material and lifecycle state

It will create deterministic resource names for old provider and credential
identities and backfill mount references. Migration tests must pin the mapping.

Legacy client filesystem specs cannot be converted automatically because a
configured detached filesystem does not mean "desired attached." The new CLI
will detect them, leave them untouched, and offer an explicit interactive
import into `Attachment` resources. It will never attach them silently.

The `omnifs-bootstrap` crate remains, but only as the pre-RPC profile and
daemon identity boundary. The generic `Bootstrap<R>` surface disappears.
`client_fs_state.rs` disappears after Attachment lifecycle moves to the
daemon. The remaining client mutation journal and its shared file helpers
disappear with the old mutation API.

## Test strategy

### Pure model tests

- resource name and key validation
- order-independent desired digest
- duplicate key rejection
- cross-resource reference validation
- attachment normalization by platform
- create/update/delete diff
- stale apply rejection
- same mutation retry
- same digest unchanged apply
- action ID retry, reuse mismatch, and stale generation

### State tests

- one transaction replaces the full desired set
- any failed row rolls back the full apply
- one revision increase per changed apply
- credential material never appears in snapshots or receipts
- credential deletion waits for serving drain
- attachment deletion tombstone survives restart
- pending typed actions survive restart
- secret material never enters action request digests
- migration mapping is deterministic
- daemon state opens from an explicit daemon-state root with no bootstrap
  dependency

### Reconciler tests

- apply reply does not call or wait on the compiler
- embedded preparation starts while store open is blocked
- retained preparation joins after state opens
- preparation and `HostOnline` share one configured engine
- a progress subscriber receives an initial snapshot before live changes
- a slow subscriber gets a resync snapshot and never blocks reconcile work
- provider work is deduplicated by digest
- provider worker concurrency stays within its bound
- referenced providers run before unused providers
- a stale generation never publishes
- a failed generation preserves the old generation
- shutdown cancels admission and joins tasks
- attachment launch waits for namespace readiness
- removal reaches absent state before clearing its tombstone
- credential and restart actions emit correlated terminal events

### Protocol tests

- every new protobuf type round-trips through strict domain conversion
- unknown and missing variants fail
- control messages stay within the one MiB bound
- no response type has a secret field
- progress events round-trip through strict typed variants and stay within
  frame bounds
- desired-revision, action-ID, and current watch targets terminate as specified
- VFS v11 rejects old and conflicting handshakes
- reconnect overlap accepts only the same attachment and exact spec
- daemon profile resolution feeds logging, state, and control from one value

### CLI transcript tests

- TTY plan, consent, and apply receipt
- no-color output
- redirected and structured output
- non-TTY interactive mutation refusal with a KCL hint
- KCL parse, type, and daemon validation errors
- secret input never appears in stdout or stderr
- setup prints the commit receipt, streams real reconcile phases, and ends with
  a terminal readiness or failure receipt
- JSONL streams events and JSON emits one terminal envelope
- Ctrl-C reports that committed work continues and restores terminal state
- non-TTY output has stable lines and no cursor escapes
- legacy client filesystem specs are reported but never changed or launched

### Live tests

- raw `ApplyResources` with a cold Wasmtime cache returns before provider
  compilation completes
- setup with a cold cache names each active provider preparation and waits on
  the progress stream
- status follow moves from preparing to ready
- daemon restart reuses the durable Wasmtime cache
- Docker and libkrun attachment lifecycle
- `down` stops runtimes and preserves desired attachments
- next daemon start restores them
- host and guest filesystems still expose one byte-identical namespace
- a fresh profile creates no `client/filesystems` tree

## Measures

The implementation should record:

- plan and apply transaction latency
- desired revision and serving revision lag
- provider preparation queue depth
- provider preparation duration and outcome by digest
- active provider preparation worker count
- generation build, publish, and drain duration
- attachment reconcile phase and retry count
- credential and Attachment action duration and outcome

No metric contains provider config values, credential values, or KCL source.

## Decisions deferred

The following require real use before design:

- multiple KCL state owners or field managers
- saved binary plan artifacts
- provider artifact garbage collection
- OCI provider import
- remote KCL packages
- resource schema code generation
- attachment pause or suspend
- per-resource apply targeting
- durable progress-event history, resumable cursors, or a generic event bus

## Acceptance criteria

The redesign is complete when all of these are true:

- `ApplyResources` has no path to `Component::new` or runtime launch code.
- A cold provider compile can exceed five seconds without timing out apply.
- Human and JSONL commands can wait for that compile through a detailed
  progress stream with no unary RPC deadline.
- Progress names the active provider or runtime stage and never invents a
  percentage.
- Daemon startup starts bounded embedded preparation before SQLite opens, adds
  retained digests when state opens, and finishes both before their first
  serving use.
- The production Wasmtime cache has no disabled or optional mode.
- Only active mount providers retain components in memory.
- SQLite is the only desired-state authority.
- Interactive commands and KCL use the same daemon plan and apply functions.
- The daemon owns host, Docker, and libkrun attachment lifecycle.
- Public `Attachment` and internal `VfsSession` have distinct names.
- `client_fs_state.rs`, `Bootstrap<R>`, and the bootstrap role markers are
  absent.
- `omnifs-state` has no dependency on `omnifs-bootstrap`.
- Normal commands create no `client/filesystems` paths.
- KCL and every response remain secret-free.
- Current control, filesystem, auth, provider, and live runtime gates pass.
