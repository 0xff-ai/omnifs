# Plan 003: Move provider work to daemon-owned reconciliation

> **Executor instructions**: Follow this plan in order. Run each verification
> command before proceeding. Stop on the listed conditions and report the
> evidence. Update this plan's row in `plans/README.md` when done unless a
> reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- crates/omnifs-engine crates/omnifs-daemon crates/omnifs-state crates/omnifs-itest`
> Confirm Plans 001 and 002 have landed. Inspect changed startup, generation,
> and resource-control code before using the symbol names below.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/002-add-fast-plan-and-apply-rpc.md`
- **Category**: performance, correctness, architecture
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

The daemon must start preparing every provider as soon as its required
Wasmtime cache is open, but it must not hold every compiled component in
memory. Namespace publication must use prepared artifacts and run outside
control requests.

This plan adds a bounded provider preparer and one ordered serving reconciler.
The last good namespace stays active while a newer desired revision prepares.

Here, "background" describes ownership and transaction scope only. The daemon
owns the work outside `ApplyResources`, and the work survives a client
disconnect. A CLI command still follows `WatchProgress` and waits by default,
with each active provider and serving stage reported as it changes.

## Current state

- `crates/omnifs-engine/src/runtime/wasm.rs:19-31` requires a cache directory
  and fails engine creation when cache setup fails.
- `ComponentEngine::load` at lines 34-37 calls synchronous `Component::new`.
- `crates/omnifs-daemon/src/app.rs:239-271` opens `HostOnline`, loads durable
  state, prepares the complete generation, publishes it, and only then creates
  the daemon.
- `crates/omnifs-daemon/src/generation_builder.rs:143-186` loads providers for
  mounted resources and builds a complete `MountTable`.
- `crates/omnifs-daemon/src/manager.rs:535-590` prepares, publishes, drains,
  and marks serving inside the mutation manager.
- `crates/omnifs-daemon/src/provider_bundle.rs:22-79` owns validated embedded
  artifact bytes and metadata.
- Provider import is content-addressed and already returns without a mutation
  lease.

The current generation model is valid. Keep one immutable complete generation.
Do not change filesystem projection semantics.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Engine tests | `cargo nextest run -p omnifs-engine` | all pass |
| Daemon tests | `cargo nextest run -p omnifs-daemon` | all pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Route init | run the existing `all_providers_initialize_and_compile` test filter | all providers pass |

Run `just build providers` before provider-dependent tests, then use
`OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1`. Preserve `sccache`.

## Scope

**In scope**:

- `crates/omnifs-engine/src/runtime/wasm.rs`
- `crates/omnifs-engine/src/runtime/host.rs`
- focused engine tests or `test_support`
- `crates/omnifs-daemon/src/app.rs`
- `crates/omnifs-daemon/src/daemon.rs`
- `crates/omnifs-daemon/src/generation_builder.rs`
- `crates/omnifs-daemon/src/manager.rs`
- `crates/omnifs-daemon/src/provider_bundle.rs`
- `crates/omnifs-daemon/src/provider_preparer.rs` (new)
- `crates/omnifs-daemon/src/serving_reconciler.rs` (new)
- `crates/omnifs-daemon/src/progress.rs`
- `crates/omnifs-daemon/src/resource_control.rs`
- `crates/omnifs-daemon/src/control/service.rs`
- `crates/omnifs-daemon/src/control/tests.rs`
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-state/src/paths.rs`
- `crates/omnifs-state/src/resource.rs`
- `crates/omnifs-state/src/provider.rs`
- `crates/omnifs-state/src/tests.rs`
- relevant daemon/engine integration tests

**Out of scope**:

- keeping all provider `Component` values in a supervisor map
- changing Wasmtime cache from required to optional
- a new cache implementation
- per-mount mutable generations
- filesystem runner ownership
- KCL or public CLI command changes
- provider authority or WIT changes

## Invariants

1. Production engine construction always has a daemon-state cache directory.
   The path can be derived without opening SQLite.
2. All intentional cold compilation runs in `ProviderPreparer`.
3. Provider work is deduplicated by `ProviderId`.
4. Concurrent preparation never exceeds its configured bound.
5. A successful prepare drops its temporary `Component`.
6. Only providers used by the active generation remain in memory after build.
7. Only the latest desired revision can publish.
8. A failed build leaves the last good generation active.
9. The raw apply RPC never waits for either supervisor.
10. A progress subscriber never delays either supervisor.
11. A desired-revision stream waits only for work that can affect that
    revision. Unused catalog warm-up never blocks it.
12. Every task has one owner, cancellation, join, and shutdown order.

## Steps

### Step 1: Give the engine an explicit prepare operation

Add a method such as:

```text
ComponentEngine::prepare(provider_id, component_bytes)
```

It may call `Component::new`, but its contract is to populate or verify the
required compiled-component cache and then drop the component. Keep
`ComponentEngine::load` for active generation construction. That later load
still calls `Component::new`, but only on an owned daemon reconcile task after
the same digest reached `Ready` in the required cache.

Do not add an optional cache path, a no-cache constructor, or a fallback
engine.

Run synchronous preparation through a blocking boundary at the daemon layer,
not by hiding Tokio inside the engine.

Add an engine test that:

- creates a private temporary cache
- prepares a valid component
- drops the result
- creates a fresh engine for the same cache
- loads the component successfully
- confirms cache files exist

Do not assert speed with a wall-clock threshold.

**Verify**:
`cargo nextest run -p omnifs-engine runtime` passes.

### Step 2: Add `ProviderPreparer`

Create a daemon-owned supervisor with:

- a bounded command queue or revision/watch input
- a small worker semaphore
- a map keyed by `ProviderId`
- phases `Queued`, `Preparing`, `Retrying`, `Ready`, `Failed`
- a watch/notify handle per digest
- a root cancellation token or shutdown command
- owned join handles
- a non-blocking publisher into the Plan 002 progress hub

Choose the worker limit from one central constant or daemon option. Use a
conservative cap, such as `min(available_parallelism, 4)`, with a minimum of
one. The exact value must have a testable helper.

Queue policy:

1. desired providers referenced by mounts
2. other retained provider artifacts
3. embedded provider artifacts not already seen

Deduplicate identical digests across all three sources. A repaired import must
reset `Failed` or `Ready` to `Queued` for that digest.

Each worker:

1. records and publishes `Queued` with catalog name, target resource aliases,
   digest prefix, and queue position
2. loads exact bytes by digest
3. records and publishes `PreparingComponent`
4. runs `ComponentEngine::prepare` through `spawn_blocking`
5. drops the temporary component
6. records `Ready` or `Failed` before publishing the matching event
7. wakes waiters

Wasmtime does not expose progress within `Component::new`. Report that stage
as indeterminate. Never derive a percentage or cache hit from elapsed time.
Report a cache hit or miss only if Wasmtime gives the host a supported,
reliable signal. Record real queue totals and completed-digest counts. Those
counters match the deduplication key.

Do not clone 128 MiB byte vectors per waiter. One owned job carries the bytes or
loads them once.

Expose:

```text
enqueue(provider_id, priority)
wait_ready(provider_id)
status(provider_id)
shutdown()
```

Only reconcilers may call `wait_ready`. Control handlers use status only.

**Verify**:
with a private fake compiler boundary, test dedupe, priority, max in-flight
work, failure isolation, repair retry, event order, provider identity, a slow
subscriber, cancellation, and joined shutdown.

### Step 3: Start preparation at the earliest safe point

Add `omnifs_state::DaemonStatePaths`, constructed from an explicit
daemon-state root without an open SQLite connection. It owns the existing
control-store, cache, staging, and log child layout, including
`wasmtime_cache()`. Its preparation method creates private directories but
does not open or mutate SQLite. Keep the current `StateStore` entry point
temporarily, but make it construct this same type internally and test that its
cache path is identical. Plan 006 passes the paths value directly and removes
the bootstrap dependency.

Refactor the top-level daemon startup:

1. resolve the profile and load the embedded provider bundle
2. prepare fixed directories, bind control, and publish exact process identity
3. create the progress hub and start the control server
4. derive and create the private Wasmtime cache directory without SQLite
5. construct one required-cache `ComponentEngine`
6. create `ProviderPreparer`, enqueue every embedded provider, and start
   bounded preparation
7. open or recover `StateStore` while embedded preparation continues
8. enqueue desired and other retained provider digests as soon as state opens
9. open `HostOnline` with a clone of the same `ComponentEngine`
10. create the initial cheap namespace
11. bind/start `VfsServer`
12. expose full daemon readiness
13. start or wake `ServingReconciler`

Do not construct a second Wasmtime engine. Adjust `HostOnline` construction so
the daemon gives it the already configured `ComponentEngine`. Cache
configuration remains required at the sole engine constructor.

Create the cache directory with the existing private-directory rules before
engine construction. A missing, unsafe, or unusable required cache is a startup
failure, not a reason to fall back to uncached compilation.

The preparer is a top-level process task, not a child of ready state. It must
stay alive while SQLite is in recovery so embedded work can finish, accept the
current store after repair, and join on every exit path even if the daemon
never reaches ready.

Preparation events may begin before control readiness. The progress hub must
hold a complete current snapshot so the first later subscriber sees the
already-active work. Do not depend on replaying startup events.

The initial namespace may be an empty mount table or an explicit preparing
namespace. It must use existing `Namespace` semantics and emit a root
invalidation when the first desired generation publishes.

Both Unix and TCP VFS listeners remain part of daemon readiness. Do not weaken
that current invariant.

Startup must still fail if:

- SQLite cannot open
- required cache setup fails
- either VFS listener cannot bind
- the initial namespace cannot be constructed

Startup must not fail merely because one provider cannot compile. Report that
provider as failed and keep control/status available.

**Verify**:
add startup tests showing control and listener readiness while a fake provider
compiler remains blocked. Add a separate startup-order test proving embedded
preparation begins while a fake store open is blocked, then retained work joins
after the store becomes available. Prove only one `ComponentEngine` is
constructed.

### Step 4: Add one ordered `ServingReconciler`

Move generation sequencing out of request handling into a new daemon owner.

Inputs:

- latest desired revision watch from `ResourceControl`
- provider preparation changes
- credential refresh and revoke wakeups with action IDs
- pending credential actions loaded from durable action receipts at startup
- provider repair wakeups
- shutdown
- a non-blocking publisher into the shared progress hub

Algorithm:

1. coalesce wakeups
2. read the latest complete desired serving snapshot
3. resolve each Mount's Provider resource to an exact artifact digest
4. recompute the effective mount version from resolved provider and credential
   facts
5. register or prioritize all required provider digests
6. wait until required providers are ready, or record a resource failure
7. prepare the complete generation off the control path
8. read current desired revision before publish
9. discard the build if stale
10. publish
11. drain the retired generation with the current bound
12. activate pending credential refreshes
13. mark serving revision and resource phases
14. loop immediately if desired advanced

Only one generation build may be active. Do not start one build per revision.

Keep the current credential refresh and revoke safety:

- revoked or deleting material is excluded from new generations
- provider digest changes recheck the stored auth runtime fingerprint before
  any material can be injected
- old admission closes where required
- secret deletion or upstream revoke finishes only after the old generation
  drains

An operational build failure updates status and schedules a bounded retry. It
does not mark the desired transaction uncommitted.

Update the authoritative phase or snapshot first, then publish its event. Emit
only coarse serving transitions:

- waiting for each required provider digest;
- all required providers ready;
- building the generation;
- generation built;
- publishing the generation;
- published revision;
- draining the retired generation;
- drain completed or degraded;
- stable failure, retry, or superseded revision.

For a credential action, publish the safe slot phase, generation refresh,
drain, and revoke stages under its action ID. End it with `ActionCompleted` or
`ActionFailed`; never expose material or provider responses that may contain
secrets. Record each phase and terminal outcome before publishing it. After a
daemon restart, resume pending credential actions from their stored lifecycle
state and action receipt.

Add the credential action generation to non-secret credential status. Accept
set and revoke only through the action transaction from Plan 002, including its
base-generation check.

Do not emit filesystem requests, provider calls, or fake build percentages.
When a newer desired revision wins, publish `RevisionSuperseded` for the older
target and stop that target's stream. Do not silently retarget the watcher.

**Verify**:
add deterministic tests for coalescing, stale-build discard, last-good
preservation, drain timeout, retry, credential action correlation and terminal
events, restart recovery, refresh, revoke, and shutdown.

### Step 5: Keep waiting on the progress stream, not apply

Inspect `ResourceControl::apply` and its gRPC handler after the refactor.
Confirm the raw `ResourceControl::apply` path still has no wait on:

- provider status
- serving revision
- generation build
- generation drain

Provider import may enqueue preparation after its content-addressed transaction
but must reply without waiting.

Old `ApplyMutation` may still use the current manager until Plan 009. Do not
route the new API through it.

`WatchProgress(desired_revision)` must remain open while required provider or
serving work runs. Its terminal rules are:

- `RevisionReady` after every resource and deletion tombstone owned by that
  revision reaches ready;
- `RevisionFailed` after a required resource reaches a stable failed or blocked
  phase;
- `RevisionSuperseded` when a newer desired revision replaces the target.

Unreferenced retained or embedded provider preparation appears on
`WatchProgress(current)` only. It does not delay a desired revision. A watcher
disconnect drops only its receiver, and a reconnect starts from the current
snapshot.

`Retrying` is not terminal. Publish its attempt count and next-attempt time.
Emit `Failed` only after no automatic retry remains.

**Verify**:
the Plan 002 acknowledgement test still passes with the fake compiler blocked
forever.

### Step 6: Publish desired and observed status

Extend daemon inventory and resource status so users can see:

- desired resource revision
- serving namespace revision
- provider preparation phase
- mount observed phase
- last stable error code and detail

Use one phase and optional error, not a generic condition array.

Do not expose provider bytes, config secrets, credential material, or raw KCL.

The progress snapshot and ordinary status read must derive from the same phase
facts. Do not build a separate stream-only state model.

**Verify**:
daemon control tests cover desired-ahead-of-serving, provider failed, stale
generation retained, and later ready states.

### Step 7: Add a cold-cache integration case

Create an isolated profile and empty Wasmtime cache. Arrange for a provider
compile to take longer than the control request deadline using a test seam, not
an arbitrary large real provider.

Prove:

- daemon control becomes ready
- apply returns success within its normal deadline
- a revision watcher first reports the named provider as preparing
- the same watcher reports generation build and publication
- the watcher ends with `RevisionReady`
- ordinary status agrees with each sampled phase
- a daemon restart uses the same cache directory and reaches ready
- a disconnected first watcher does not stop compilation, and a second watcher
  resumes from a complete snapshot

The test must not use a fragile timing assertion beyond the existing request
deadline distinction.

**Verify**:
run the focused test with provider artifacts prebuilt.

### Step 8: Run gates

Run:

```text
just build providers
OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run -p omnifs-engine -p omnifs-state -p omnifs-daemon
OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run all_providers_initialize_and_compile
cargo fmt --all -- --check
just check host
just test host
git diff --check
```

All commands must exit zero.

## Test plan

Use:

- engine runtime tests near `crates/omnifs-engine/src/runtime/wasm.rs`
- daemon task-owner patterns in current manager and VFS server
- current generation drain cases in `crates/omnifs-daemon/src/manager.rs`
- provider bundle test in
  `crates/omnifs-daemon/src/provider_bundle.rs:81-95`

Required proof:

- durable cache required
- all embedded and retained digests enqueued
- desired digests prioritized
- max compiler concurrency bounded
- no all-provider component retention
- stale revision never publishes
- apply succeeds while compiler is blocked
- revision stream stays open while compiler is blocked
- revision stream names the active provider and serving stage
- unused catalog preparation does not delay or appear in the revision stream
- current stream can show unused catalog preparation
- slow progress subscribers never delay workers
- provider failure is local
- last good generation survives
- every task joins

## Done criteria

- [ ] Embedded provider preparation starts immediately after required engine
  construction and before SQLite opens.
- [ ] Retained provider preparation joins as soon as SQLite opens.
- [ ] `HostOnline` and `ProviderPreparer` share one configured engine.
- [ ] Every embedded and retained provider digest is queued.
- [ ] Preparation is bounded and deduplicated by digest.
- [ ] Prepared temporary components are dropped.
- [ ] The production engine has no cache-disabled path.
- [ ] Only active generation providers retain components.
- [ ] Serving work runs only in `ServingReconciler`.
- [ ] Only the latest desired revision publishes.
- [ ] Raw apply returns while provider compilation is blocked.
- [ ] The CLI can wait for the same work through `WatchProgress`.
- [ ] Provider and serving phases stream with exact names and honest counters.
- [ ] No provider compilation percentage is invented.
- [ ] Unused catalog warm-up does not block a desired revision.
- [ ] Pending credential actions resume and retain their action correlation
  after daemon restart.
- [ ] Status separates desired revision from serving revision.
- [ ] All Step 8 commands pass.

## STOP conditions

Stop and report if:

- Wasmtime does not make the compiled cache durable by the time
  `Component::new` returns.
- A cache hit cannot be used safely by a fresh `ComponentEngine` with the same
  config and cache directory.
- Constructing an empty or preparing namespace changes projection semantics or
  requires a provider.
- `MountTable` cannot build only active mount providers without retaining every
  prepared component.
- Credential deletion or revocation would erase material before an old
  generation drains.
- A provider preparation failure forces the control socket or VFS listeners to
  stop.
- The only proposed design uses detached tasks or an unbounded queue.

## Maintenance notes

- The in-memory phase map is process state. The durable cache is the compiled
  artifact authority.
- Never infer that a cache directory entry is valid from its filename alone.
  Let Wasmtime validate through the required engine.
- Keep compilation on the blocking pool and bound it separately from async
  request work.
- Write phase state before publishing its event. A reconnect must recover from
  the snapshot without an event log.
- Progress delivery is best-effort detail over authoritative snapshots. Never
  await a subscriber from a worker or reconciler.
- Future cache pruning must coordinate with this supervisor and cannot run in a
  control handler.

## Git workflow

- Use a branch such as `codex/003-provider-reconcile`.
- Use Conventional Commits, for example
  `refactor(daemon): reconcile providers and serving asynchronously`.
- Do not push or open a pull request unless the operator asks.
