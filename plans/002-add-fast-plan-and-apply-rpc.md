# Plan 002: Add fast typed plan, apply, and progress RPCs

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. Stop on any condition listed below. When done, update this plan's
> status in `plans/README.md` unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- crates/omnifs-api crates/omnifs-daemon crates/omnifs-cli`
> Plans are cumulative. First confirm Plan 001 has landed and its resource
> types match the design assumed here. Then inspect any later drift in the
> listed paths.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-add-resource-domain-and-durable-state.md`
- **Category**: bug, architecture
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

The current `ApplyMutation` handler waits for namespace preparation and
generation drain after SQLite commit. A compile or stuck request can exceed
the client's five second deadline and report a timeout after the change
committed.

This plan adds the final control-plane acknowledgement boundary:
`ApplyResources` returns after the desired-state transaction and reconcile
wakeup. It also adds a separate bounded server stream that lets commands wait
on real progress without extending the mutation deadline. It keeps the old RPC
alive only so intermediate commits remain buildable.

## Current state

- `crates/omnifs-api/proto/control/v1/control.proto:17-19` exposes
  `BeginMutation`, `ApplyMutation`, and `DropMutation`.
- `crates/omnifs-api/src/control.rs:18-20` defines five second ordinary requests
  and a 30 second mutation lease.
- `crates/omnifs-cli/src/mutation.rs:226-273` acquires the lease before building
  and applying its operation list.
- `crates/omnifs-daemon/src/manager.rs:346-420` commits state, then waits for
  `prepare_and_activate`.
- `crates/omnifs-state/src/writer.rs` already gives the daemon one durable
  writer. Plan 001 adds full-set compare-and-swap on that writer.
- gRPC conversion code lives in `crates/omnifs-api/src/grpc.rs`.
- Generated protobuf Rust is not checked in.

The new resource handlers must not own `HostOnline`, `ServingCell`, filesystem
drivers, or an OAuth client. The type graph should make expensive runtime work
unavailable to them.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| API tests | `cargo nextest run -p omnifs-api` | all pass |
| Daemon tests | `cargo nextest run -p omnifs-daemon` | all pass |
| CLI tests | `cargo nextest run -p omnifs-cli` | all pass |
| Control integration | `cargo nextest run -p omnifs-itest --test control_plane` | all supported cases pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |

Prebuild providers and set `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1` when a selected
test needs provider artifacts. Preserve `sccache`.

## Scope

**In scope**:

- `crates/omnifs-api/proto/control/v1/control.proto`
- `crates/omnifs-api/src/control.rs`
- `crates/omnifs-api/src/grpc.rs`
- `crates/omnifs-api/src/resource.rs`
- `crates/omnifs-api/src/progress.rs`
- `crates/omnifs-core/src/operation.rs` if action IDs live in core
- `crates/omnifs-api/build.rs` only if protobuf generation needs it
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/control.rs`
- `crates/omnifs-daemon/src/control/mapping.rs`
- `crates/omnifs-daemon/src/control/service.rs`
- `crates/omnifs-daemon/src/control/tests.rs`
- `crates/omnifs-daemon/src/daemon.rs`
- `crates/omnifs-daemon/src/resource_control.rs` (new)
- `crates/omnifs-daemon/src/progress.rs` (new)
- `crates/omnifs-state/src/resource.rs` and its tests from Plan 001
- `crates/omnifs-state/migrations/0003_action_receipts.sql` (new)
- `crates/omnifs-state/src/action.rs` (new)
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-itest/tests/control_plane/main.rs`

**Out of scope**:

- calling the new RPC from public mutation commands
- deleting the old mutation lease or journal
- KCL
- provider preparation and serving generation work
- provider preparation
- filesystem or attachment lifecycle
- provider authority changes
- terminal rendering or spinner implementation

## Target protocol

Add typed protobuf messages and methods:

```text
GetResources
PlanResources
ApplyResources
SetCredentialMaterial
RevokeCredential
WatchProgress
```

Use protobuf `oneof` for resource variants and runtime variants. Keep the
existing one MiB request and response bound.

`ApplyResourcesRequest` contains:

- mutation ID
- base resource revision
- expected desired digest
- complete typed declarations
- request-only credential material sidecars

`ApplyResourcesResponse` contains only a non-secret durable receipt.

`SetCredentialMaterial` and `RevokeCredential` return a non-secret
`CredentialReceipt` with the durable client-generated action ID from the
request. Their local durable transition and action acceptance end before the
reply. Plan 003 attaches the later refresh, drain, and revoke work to that
action.

Keep `ActionReceipt` typed: action ID, closed kind, target `ResourceKey`,
accepted action generation, and current phase. It contains no input payload.

`WatchProgressRequest` contains one typed target:

```text
desired revision
action ID
current daemon state
```

`WatchProgress` returns a stream of strict `ProgressEvent` variants:

```text
Snapshot
ResourcePhaseChanged
ProviderPreparation
ServingProgress
CredentialProgress
AttachmentProgress
RevisionReady
RevisionFailed
RevisionSuperseded
ActionCompleted
ActionFailed
Resync
```

Every event carries the daemon instance ID, monotonic in-instance sequence,
and its target. Each progress variant has its own stage enum and typed fields.
Events contain names, phases, stable error codes, safe human detail, real
counts, and real byte progress only. They contain no secret, provider config,
local KCL path, or fake percentage. Do not add generic log text or a free-form
field map.

`ProviderPreparation` identifies one unique digest, its catalog name, and the
bounded list of Provider resource names in the watched target that resolve to
it. Queue totals count digests so aliases do not overstate work.

A desired-revision subscription includes only resources, provider digests,
serving work, and deletion tombstones that can affect that revision. Preparing
an unused retained or embedded provider belongs to the `current` stream and
must not delay or clutter the revision stream. An action subscription includes
only that durable action and its affected resource. A `current` subscription
includes all current work and has no ready terminal state.

Do not send the normalized full set back in `PlanResourcesResponse` unless the
CLI needs it to render defaults. If it is sent, keep it typed and under the
same size limit.

Add stable errors for:

- unsupported API version
- invalid resource
- stale base revision
- desired digest mismatch
- mutation ID reuse mismatch
- missing provider artifact
- plan too large
- action unavailable (unknown or expired)
- action ID reuse mismatch

Do not reuse `LEASE_EXPIRED` or `LEASE_NOT_HELD` for these cases.

## Steps

### Step 1: Extend the protobuf schema

Add the methods and typed messages. Do not add generic `bytes json` fields for
whole resources. The only dynamic JSON bytes remain mount provider config.

Ensure no response message contains:

- token bytes
- refresh token bytes
- client secret bytes
- credential material
- environment variable names
- local provider source paths

Apply the same exclusion to every progress event.

Keep old RPC field numbers and messages intact until Plan 009.

**Verify**:
`cargo nextest run -p omnifs-api` compiles generated code and passes strict
round-trip and rejection tests for every new variant.

### Step 2: Add strict gRPC conversions

Implement conversions in `crates/omnifs-api/src/grpc.rs`. Match current helpers
such as `req`, exact-length digest decoding, path-byte handling, and enum
validation.

Conversion order:

1. parse protobuf into wire-domain DTOs
2. validate required variants and exact byte lengths
3. call domain constructors
4. return a typed `FromGrpcError` on any mismatch

Add tests for missing `oneof`, invalid digest size, duplicate resources,
unknown enum values, bad paths, and oversized resource counts.

**Verify**:
`cargo nextest run -p omnifs-api` passes.

### Step 3: Add the typed progress hub

Create `crates/omnifs-daemon/src/progress.rs`.

First add a strict `action_receipts` table and state owner. It stores only a
closed action kind, target resource key, request digest, accepted generation
when used, current phase, safe error, and timestamps. It contains no payload
bytes or secrets. It is a receipt/current-status index for the known
credential and Attachment actions, not a generic queue or event history.
The same migration adds `action_generation` to the non-secret credential
lifecycle row. Plan 005 adds the equivalent field with Attachment observed
state.

Acceptance and phase transitions use the existing single state writer.
Retrying an action ID with the same request returns its receipt; different
input fails. Credential and Attachment observed state each carry an action
generation. Acceptance requires the exact base generation and increments it in
the same transaction as the receipt. Never prune a pending action. Terminal
receipts may use one bounded age or count policy. Owners in Plans 003 and 005
reload pending actions after daemon restart.

Allow at most one non-terminal action per target resource. A different action
for a busy target returns a typed busy error; the same action ID remains an
idempotent retry. This bounds pending rows without a generic scheduler.

The request digest covers only non-secret kind, target, generation, and
operation fields. For set-material, the first accepted action ID wins: a retry
returns its stored receipt without comparing or reapplying supplied secret
bytes. Changing material requires a fresh action ID. Never persist a plain or
unsalted digest of credential material.

The hub owns:

- the latest complete progress snapshot;
- one monotonic sequence scoped to the daemon instance;
- bounded non-blocking live fanout;
- target terminal-state evaluation;
- target-aware filtering for desired revisions, actions, and current state.

It does not own durable truth, provider work, generation work, Attachment
work, retries, or terminal rendering.

Use a latest-snapshot channel plus bounded broadcast, or an equivalent design.
Subscription must follow this order:

1. subscribe to live events;
2. read one snapshot and its sequence watermark;
3. send the snapshot;
4. discard queued events at or below the watermark;
5. forward newer events;
6. on receiver lag, read and send a new `Resync` snapshot.

A slow client must never block or fill a reconciler queue. A disconnected
client drops only its receiver.

Publish stage transitions immediately. Coalesce high-rate byte updates to a
small fixed maximum rate per operation and always publish the final exact
count. Keep that rate in one testable constant. Do not coalesce terminal
events.

At this stage, snapshots may contain only desired resource and observed-state
facts available from Plan 001. Plans 003 and 005 add provider, generation, and
Attachment publishers.

Terminal evaluation must use the snapshot, not depend on seeing every event.
An unchanged apply may target a revision already ready and end from its first
snapshot. If that revision is still active, it waits like a changed apply.

**Verify**:
unit tests cover subscribe-versus-update races, monotonic sequences, lagged
resync, disconnect, desired-revision terminal states, action terminal states,
current watch non-termination, target filtering, unchanged-ready completion,
unused catalog work not blocking a desired revision, byte-progress
coalescing, and final exact byte delivery.

### Step 4: Add a resource control owner

Create `crates/omnifs-daemon/src/resource_control.rs`.

It owns:

- `Arc<StateStore>`
- normalization and daemon-side validation services that do no runtime work
- a `watch::Sender<ResourceRevision>` or equivalent coalescing wakeup
- a progress-hub handle for desired-state snapshot changes
- shutdown admission state

It does not own:

- `HostOnline`
- `ServingCell`
- `ComponentEngine`
- `GenerationDraft`
- a filesystem runtime driver
- network clients

Public internal methods:

```text
snapshot()
plan(declarations)
apply(request)
set_credential_material(request)
revoke_credential(request)
subscribe_revisions()
shutdown()
```

`plan` must:

- read one current resource snapshot
- normalize platform-owned attachment defaults
- validate provider digests against retained artifact metadata
- validate mount references and provider metadata
- calculate a pure typed diff
- return base revision and desired digest

It must not initialize a provider or check upstream state.

`apply` must:

- repeat all normalization and validation
- recompute the digest
- call Plan 001's single state transaction
- send the watch wakeup with `send_replace` or an equivalent non-blocking,
  coalescing action
- return immediately

No queue send in this path may await available reconcile capacity.

**Verify**:
unit tests construct `ResourceControl` without any engine or serving object and
cover plan, changed apply, unchanged apply, stale conflict, retry, and shutdown
admission.

### Step 5: Wire the control service

Make `ControlServer` hold a resource-control handle once state is ready. Follow
the current ready/recovery split: read methods should return the same typed
not-ready or recovery error when the store is not available.

Map expected domain errors to stable `ControlErrorCode` values. Attach useful
resource keys but no secret or raw KCL text.

The gRPC handler must do no work after `ResourceControl::apply` returns.

Add `WatchProgress`. Register the live receiver before loading the initial
snapshot. Stream setup uses the ordinary connection deadline, but the stream
itself has no five second unary deadline. Bound each frame and the per-client
buffer. Client cancellation drops the stream task without canceling daemon
work.

**Verify**:
add service-boundary tests in
`crates/omnifs-daemon/src/control/tests.rs` for:

- plan and apply round trip
- stale plan
- same mutation retry
- malformed request
- not-ready state
- recovery-required state
- response secret absence
- initial progress snapshot
- apply-to-subscribe race
- lagged subscriber resync
- stream disconnect while work continues

Run `cargo nextest run -p omnifs-daemon`.

### Step 6: Add a CLI RPC client surface

Add private client methods in `crates/omnifs-cli/src/rpc.rs`:

```text
resources()
plan_resources()
apply_resources()
set_credential_material()
revoke_credential()
watch_progress(target)
```

Use the ordinary five second unary deadline for finite calls. Do not add a
longer "compile" deadline. `watch_progress` uses that deadline only for stream
setup and first snapshot, then reads until a typed terminal event,
disconnection, or caller cancellation. Convert stale conflicts into an error
that later commands can turn into "state changed, review a new plan."

Do not switch public commands in this plan.

**Verify**:
add client tests or control fixture calls proving the exact timeout and typed
response mapping, including credential action IDs. Run
`cargo nextest run -p omnifs-cli`.

### Step 7: Prove the acknowledgement boundary

Add a focused daemon test with a reconcile subscriber that never consumes its
wakeup, or a fake subscriber that remains pending.

The test must prove:

- apply commits and replies
- the desired revision advances
- no reconcile consumer needs to run
- dropping the client wait after commit does not cancel the writer job
- retrying the mutation ID returns the stored receipt
- a separate progress subscriber can observe the committed revision
- canceling that subscriber does not cancel daemon work

Prefer a structural proof as well: `resource_control.rs` must not import
`omnifs_engine`, `generation_builder`, `filesystem_driver`, or the future
runtime crate.

Add a grep assertion to the final done criteria, but do not add a brittle test
that scans source at runtime.

**Verify**:
`cargo nextest run -p omnifs-daemon resource` passes without provider artifacts.

### Step 8: Extend the control-plane integration fixture

In `crates/omnifs-itest/tests/control_plane/main.rs`, start an isolated daemon,
read the empty set, plan one retained embedded provider and mount, apply, and
read the advanced desired revision.

The raw apply call must not wait for serving readiness. Then open
`WatchProgress` for the returned revision, assert the initial snapshot reports
the desired state as pending, then disconnect. Prove a second watcher resumes
from a current snapshot. Plan 003 adds the real provider and serving publishers
and owns the first end-to-end `RevisionReady` assertion; Plan 005 adds the
Attachment terminal path.

**Verify**:

```text
just build providers
OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run -p omnifs-itest --test control_plane
```

### Step 9: Run local gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-api -p omnifs-state -p omnifs-daemon -p omnifs-cli
just check host
just test host
git diff --check
```

All commands must exit zero.

## Test plan

Follow:

- generated control conversion tests in `crates/omnifs-api/src/grpc.rs`
- daemon service tests in `crates/omnifs-daemon/src/control/tests.rs`
- store transaction tests from Plan 001
- isolated profile fixture in
  `crates/omnifs-itest/tests/control_plane/main.rs`

Required cases:

- every new wire variant
- invalid and oversized inputs
- no-secret response shape
- exact base compare-and-swap
- unchanged desired state
- duplicate mutation recovery
- reconcile not running
- store not ready
- store in recovery
- daemon shutdown admission
- progress initial snapshot
- progress sequence monotonicity
- subscribe/update race
- slow-subscriber resync
- client disconnect without work cancellation
- desired revision, durable action, and current targets
- pending action recovery after daemon restart

## Done criteria

- [ ] `GetResources`, `PlanResources`, and `ApplyResources` exist as typed RPCs.
- [ ] `WatchProgress` exists as a typed bounded server stream.
- [ ] Apply replies after SQLite commit and non-blocking wakeup.
- [ ] `resource_control.rs` has no engine, generation, Wasmtime, or runtime
  driver dependency.
- [ ] `rg -n "Component::new|prepare_and_activate|GenerationDraft|FilesystemDriver" crates/omnifs-daemon/src/resource_control.rs`
  returns no matches.
- [ ] Plan and apply use the ordinary five second deadline.
- [ ] Progress uses the ordinary setup deadline and no unary work deadline.
- [ ] Every subscription starts with a complete snapshot.
- [ ] Slow or disconnected subscribers never block or cancel reconciliation.
- [ ] Retrying a mutation ID returns a durable receipt.
- [ ] Credential receipts correlate later work with a durable action ID.
- [ ] Typed action acceptance and current status survive daemon restart.
- [ ] No response type can carry credential material.
- [ ] Old RPCs still compile but public commands have not moved yet.
- [ ] All Step 9 commands pass.

## STOP conditions

Stop and report if:

- Plan 001 did not provide one atomic full-set compare-and-swap.
- Correct planning requires provider instantiation or Wasmtime compilation.
- Apply needs to wait on a bounded worker queue to avoid losing its wakeup.
- The protobuf set cannot fit under one MiB for a representative 30-provider,
  30-mount, and several-attachment profile.
- Progress events require an unbounded queue or durable event log.
- Credential secret material must appear in a response to preserve a current
  feature.
- A stale apply cannot be rejected at the SQLite transaction boundary.
- Any proposed handler needs direct access to current client files.

## Maintenance notes

- The desired revision watch is a notification, not a work ledger. Reconcilers
  must always reload the latest state.
- Do not make a successful apply receipt claim that resources are ready.
- Do not make progress delivery part of durable apply acknowledgement.
- Do not block workers on progress subscribers or invent progress data.
- Keep old and new APIs from calling each other. Both may coexist briefly, but
  one must not become an adapter over the other until public callers switch.
- Plan 003 will consume the revision subscription and remove runtime work from
  `MutationManager`.

## Git workflow

- Use a branch such as `codex/002-resource-rpc`.
- Use Conventional Commits, for example
  `feat(control): add fast resource plan and apply`.
- Do not push or open a pull request unless the operator asks.
