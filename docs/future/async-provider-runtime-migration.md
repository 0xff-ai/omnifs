# Async provider runtime migration

Status: future migration design
Scope: provider WIT, SDK bindings, host provider runtime, host capability imports, inspector events, and validation gates.
Related: `docs/future/async-http.md`, `docs/contracts/10-system.md`, `docs/contracts/20-provider-sdk.md`, `docs/contracts/60-build-validation.md`.

## Decision summary

Omnifs should migrate provider execution from the custom `provider-step` / `continuation.resume` protocol to Wasmtime component async and concurrent host functions. The end state is not "host-side async around the old protocol." The end state is that each provider operation is a concurrent guest task, provider code awaits typed host capabilities directly, and the host receives one terminal `provider-return`.

The first production-shaped migration should use repo-local async host imports for today's capabilities: HTTP fetch, blob fetch/read, git open, archive open, and logging. Direct `wasi:http` remains the long-term HTTP boundary, but it should not be the first dependency of this migration. In Wasmtime v46.0.1 the P3 `wasi-http` module exists, but its source docs still describe it as experimental, unstable, incomplete, and not production-ready. The concurrency mechanism should be proven independently of that HTTP surface.

The provider compilation target remains `wasm32-wasip2` unless the provider toolchain spike proves a target change is required. WASI 0.3 matters here because the component async model and P3 interfaces are becoming real, not because omnifs should immediately rename every provider target.

## Current model

The current provider boundary has four moving parts:

- WIT operations return `provider-step`, either `returned(provider-return)` or `suspended(list<callout>)`.
- The host receives a suspended callout batch, drops the store lock, runs host work, and re-enters the component through `continuation.resume`.
- The SDK macro owns a thread-local `AsyncRuntime` that parks handler futures by correlation id and polls them again after resume.
- `Cx` and `CalloutFuture` implement a positional queue: futures push callouts into a yielded queue, and resumed results are popped in the same order.

This shape is not accidental. It gives omnifs same-instance concurrency without Wasmtime component async: the store is held only for short guest polls, and slow host I/O happens outside the store. It also makes transport mechanics a repo-local protocol spread across WIT, host runtime, SDK runtime, generated macro glue, provider tests, inspector events, and callout tracing.

The migration must preserve the useful property and delete the custom protocol. If the new runtime serializes provider operations while one operation awaits host I/O, the migration has failed even if provider code looks cleaner.

## Validated runtime facts

The Wasmtime v46 investigation established these facts:

- `func_wrap_async` is not enough. It lets the host future await without blocking the host thread, but the imported function still appears blocking to WebAssembly.
- `func_wrap_concurrent` is the relevant host import API. It lets an async host function be asynchronous from the guest's perspective when the guest lowers the import with the async ABI.
- `call_concurrent`, `start_call_concurrent`, `finish_call_concurrent`, and `Store::run_concurrent` are the relevant guest call APIs. They allow multiple calls into one component instance to be in flight at the same time.
- `Config::wasm_component_model_async(true)` is required for the component async ABI. `Config::concurrency_support` defaults to true in v46 and is required by the concurrent APIs.
- A local proof component reached `max_active = 3` for three concurrent calls into one instance, each awaiting a slow host import, and completed in one delay window. Sync-lowered or blocking-wait variants serialized at `max_active = 1`.
- Dropping a future returned by `call_concurrent` does not cancel the guest task. The task remains connected to the store and may keep running until it completes or the store is dropped.

The runtime condition that blocked `docs/future/async-http.md` is now likely solvable. The migration still needs in-repo proof that the Rust provider toolchain, generated bindings, SDK macro, and host driver can use that runtime shape without rebuilding continuations under another name.

## Target model

The target call path is:

```rust
let ret = self.instance.call(op, operation_context).await?;
self.finish_provider_return(&op, ret, op_gen)
```

`Runtime::run_op` still owns cache generation capture, provider operation inspector lifetime, result validation, effect application, and returned-result side effects. It no longer drives `start -> suspend -> dispatch -> resume` rounds.

`Instance` becomes a client handle to a store-owning driver:

```rust
pub struct Instance {
    commands: tokio::sync::mpsc::Sender<DriverCommand>,
}

struct InstanceDriver {
    store: wasmtime::Store<HostState>,
    bindings: Provider,
    services: ProviderHostServices,
    operations: OperationTable,
}
```

The driver is the only place that owns the Wasmtime store. It is also the only place that calls `run_concurrent`. Request handlers enqueue operation commands and await oneshot replies. They never lock the store directly.

The driver should stay alive for the provider lifetime. Each provider operation starts as a concurrent guest task inside the driver's `run_concurrent` scope and completes with one terminal `provider-return`. Host imports run as `func_wrap_concurrent` callbacks and use an `Accessor<HostState>` only for short, non-awaiting store access.

## WIT shape

The WIT surface should stop exposing the transport protocol:

- remove `provider-step`
- remove `callout`
- remove `callout-result`
- remove `callout-results`
- remove `continuation.resume`
- remove `continuation.cancel`

Provider exports should return terminal results:

```wit
namespace.lookup-child(...) -> provider-return
namespace.list-children(...) -> provider-return
namespace.read-file(...) -> provider-return
namespace.open-file(...) -> provider-return
namespace.read-chunk(...) -> provider-return
notify.on-event(...) -> provider-return
```

The exact async WIT syntax is a phase-zero deliverable, because the generated binding shape is load-bearing. The intended model is that exported operations and slow host imports are async component functions. If the current `wit-bindgen` and Wasmtime bindgen stack cannot express the required async exports/imports cleanly, the migration stops at the proof gate.

## Operation context

The host must own operation identity. Provider-supplied integers are fine as provider-local diagnostics, but they must not be the authority for tracing, effect ownership, timeout policy, or host capability correlation.

The preferred shape is a host-created operation context resource:

```wit
interface host-op {
    resource operation-context;
}
```

Provider exports receive a borrowed operation context, and host capability imports require the same borrowed context:

```wit
namespace.lookup-child(ctx: borrow<operation-context>, ...) -> provider-return
host-http.fetch(ctx: borrow<operation-context>, request: http-request) -> result<http-response, callout-error>
host-git.open-repo(ctx: borrow<operation-context>, request: git-open-request) -> result<git-repo-info, callout-error>
```

This is illustrative, not final syntax. The proof gate must confirm that:

- the host can create one context per provider operation;
- the provider can pass a borrow of that context into host imports;
- the provider cannot store a borrow and reuse it after the operation returns;
- host imports can resolve the context to `OperationContext` without trusting a guest-supplied id;
- context cleanup runs when the operation completes, traps, or is abandoned.

`OperationContext` should carry:

- operation id;
- trace id, when present;
- mount name;
- provider name;
- operation descriptor;
- per-operation host import invocation counter;
- deadline and cancellation state;
- stale-result policy;
- any inspector handle needed to emit `provider.end` exactly once.

The operation context is the core replacement for correlation ids. If this shape is not viable, the next best design is a Wasmtime guest-task-id mapping owned by the driver. Do not fall back to plain guest-passed `u64` ids without explicitly accepting the observability and policy risks.

## Host capability imports

The first migration keeps the current capability set but changes the transport shape. Instead of returning generic `callout` variants to the host, providers await typed host imports.

### HTTP

Phase one should expose a repo-local buffered HTTP import that preserves `HttpStack` ownership:

- method and URL validation;
- capability allowlist checks;
- auth preparation and header injection;
- refresh-on-response behavior;
- timeout mapping;
- response header decoding;
- redaction for tracing;
- rate-limit interpretation.

This keeps the migration focused on async component execution. It also avoids binding the first runtime migration to P3 `wasi:http` stream/resource semantics.

Direct `wasi:http` is a later phase. That phase must prove that `WasiHttpHooks` can enforce the same host policy and observability contract, and that its body/resource model does not fight omnifs's blob-cache strategy.

### Blobs

Blob operations stay repo-local. They are host cache capabilities, not generic HTTP:

- `fetch-blob` streams response bodies into the blob cache;
- `read-blob` moves selected bytes across WIT only when the provider needs to parse them;
- blob size limits and staged writes stay in `BlobExecutor`.

This should not be forced through P3 HTTP. The point of blobs is to keep large bytes host-side and serve them through host-controlled file reads.

### Git and archive

Git and archive operations stay repo-local host capabilities:

- `git.open-repo` returns a tree reference backed by the host clone/materialization path;
- `archive.open` returns a tree reference backed by host extraction;
- both continue using the shared `TreeRefs` registry.

These are not HTTP, and they should not be hidden behind an HTTP redesign.

### Logging

Logging can remain sync only if it is guaranteed not to block. If any log path can perform I/O, backpressure, or subscriber work that blocks, it must be converted to a non-blocking or concurrent host import. A sync host import inside `run_concurrent` is a progress risk.

## Provider driver

The provider driver owns the store and event loop. It should be introduced as a new module rather than expanding `Instance` into a mixed client/driver type.

Recommended ownership:

```rust
pub struct Instance {
    tx: DriverTx,
}

struct Driver {
    store: Store<HostState>,
    bindings: Provider,
    rx: DriverRx,
    services: ProviderHostServices,
}

struct ProviderHostServices {
    http: Arc<HttpStack>,
    git: GitExecutor,
    blob: BlobExecutor,
    archive: Arc<ArchiveExecutor>,
}
```

`Runtime` remains the owner of provider-level policy and cache application. The driver owns only Wasmtime execution and host import dispatch. Effects are not committed inside host imports. Effects remain terminal data returned from the provider operation and are applied by `Runtime::finish_provider_return`.

The driver loop must satisfy these rules:

- keep one `run_concurrent` scope active while operations are in flight;
- accept new operation commands while existing guest tasks await host imports;
- start guest calls with `start_call_concurrent` or `call_concurrent`;
- await guest completion within a live `run_concurrent` scope;
- send one terminal reply per operation;
- clean operation context after completion, trap, timeout, or driver shutdown;
- drop the whole store on hard provider shutdown.

The driver command queue must be bounded. If the queue is full, callers receive a provider-busy error rather than allocating unbounded guest tasks.

## SDK and macro shape

The SDK should keep provider ergonomics and remove transport emulation.

`Cx` should keep:

- provider state access;
- endpoint helpers;
- cached validator access;
- operation-scoped metadata;
- typed access to host capability wrappers.

`Cx` should lose:

- yielded callout queues;
- delivered result queues;
- `take_yielded_callouts`;
- `push_delivered`;
- positional result matching.

`CalloutFuture` should disappear. HTTP, blob, git, and archive helpers should become wrappers over generated async host imports. `join_all` should either become a thin re-export of normal future fanout or be deleted if providers can use standard utilities directly.

The provider macro should generate async operation exports that call router methods and return `provider-return` directly. It should delete:

- thread-local `AsyncRuntime`;
- generated `continuation` implementation;
- generated `resume`;
- generated `cancel`;
- code paths that return `ProviderStep`.

Provider route registration remains synchronous. Provider operations remain async.

## Initialization and manifest extraction

`manifest-json` should remain boring. It should not require provider config, provider initialization, or host capability services. Build-time metadata injection depends on being able to harvest the manifest from a component without live mount state.

`initialize` should remain terminal unless there is a separately approved reason to allow host capabilities during initialization. The current host builds capability and auth enforcement from mount config, provider manifest, and initialize output. If initialization can call HTTP or git, bootstrapping becomes circular: the host needs initialize output to enforce the call, and the provider wants a call before initialize returns.

The initial design should therefore use a host-state phase:

```rust
enum ServiceState {
    Bootstrapping,
    Ready(Arc<ProviderHostServices>),
    ShuttingDown,
}
```

Slow host imports fail with a provider protocol error in `Bootstrapping`. After `initialize` returns and `Runtime` builds `CapabilityChecker`, `AuthManager`, `HttpStack`, blob, git, and archive services, the driver transitions to `Ready`.

If a future provider needs network during initialization, that is a separate gated decision because it changes the provider authority and startup contract.

## Cancellation, deadlines, and stale results

Cancellation is the most important semantic change.

Today, `AsyncRuntime::cancel` can drop a parked guest future, although the host does not appear to use the WIT `cancel` export in the normal runtime path. Under Wasmtime concurrent calls, dropping the host future does not cancel the guest task. The task can keep running and can still invoke host imports.

The migration must define cancellation before deleting continuations:

- A caller timeout marks its operation context abandoned.
- Host imports check the operation context before starting expensive work and after awaiting.
- An abandoned operation may finish, but its terminal result is ignored.
- Effects from an abandoned operation must not commit.
- Inspector output should end the provider operation with timeout or cancelled outcome.
- Provider shutdown drops the store, which is the hard cancellation boundary.

This means the operation context owns an `OperationState`:

```rust
enum OperationState {
    Active,
    Abandoned(AbandonReason),
    Completing,
    Completed,
}
```

Only `Active` operations can start new expensive host work. Only an operation that reaches `Completing` from `Active` may return effects to `Runtime` for commit. A late guest return after abandonment is observed, logged at debug level, and discarded.

## Backpressure and fairness

Real same-instance concurrency makes backpressure mandatory. The current continuation protocol naturally throttles some paths because each operation needs explicit host resume rounds. The new model can start many guest tasks and many host imports at once.

Required limits:

- per-provider operation queue capacity;
- per-provider active guest task limit;
- per-provider host import limit;
- per-upstream or per-authority HTTP concurrency, where the existing endpoint/rate-limit model can supply the key;
- blob materialization concurrency;
- git clone concurrency;
- archive extraction concurrency.

The first implementation does not need a perfect scheduler. It does need bounded queues and predictable errors. Unbounded `mpsc`, unbounded spawned tasks, or unlimited concurrent host imports should be rejected in review.

## Effects and cache fences

Effect application remains terminal. Host imports can produce bytes, blobs, tree refs, and response payloads, but they do not mutate the browse cache directly. Providers still return `effects` with the final `provider-return`, and `Runtime::finish_provider_return` validates and applies them.

The existing generation fence remains the right concept: capture `op_gen` before starting the operation, reject writes whose anchor was invalidated after the operation began, and apply accepted effects atomically from the host's perspective.

The migration needs explicit tests for concurrent returns:

- two reads of the same object returning in reverse order;
- invalidation while a provider operation is awaiting host I/O;
- stale listing write after a newer listing was committed;
- canonical write after object invalidation;
- abandoned operation that returns effects after timeout.

Passing Rust checks is not enough here. These are cache contract tests.

## Observability

Callout tracing remains a contract, but the events move to a different owner.

Kept:

- `provider.start`;
- `callout.start`;
- `callout.end`;
- `subtree.start`;
- `subtree.end`;
- `clone.start`;
- `clone.end`;
- `cache.event`;
- `provider.end`.

Removed:

- `provider.suspend`;
- `provider.resume`.

`provider.suspend` and `provider.resume` describe the custom protocol, not the user-visible provider operation. They should disappear with the protocol.

`callout.start` and `callout.end` should be emitted by host capability import wrappers. The event name can stay `callout` for wire compatibility, but the implementation should treat it as "host capability invocation." The old `callout_index` was a batch index. In the new model it should become a monotonic per-operation invocation index, or the inspector schema should be versioned and the field renamed.

Tracing spans with target `omnifs_callout` should move from the batch dispatcher to the host service import wrappers and existing executor methods. Redaction stays in the host layer.

## Test strategy

The current test harness has an easy lever: inspect `TestOp::callouts()` and call `resume(...)` with canned results. That lever disappears. Tests should move to fake host services.

Required harness shape:

```rust
let host = FakeProviderHostServices::new()
    .http_response(...)
    .git_tree(...)
    .blob(...)
    .archive(...);

let runtime = harness.with_host_services(host).build().await?;
let result = runtime.namespace().read_file(path).await?;
```

The fake services should capture requests and expose assertions:

- which host imports were invoked;
- operation id and trace id attached to each import;
- ordering and concurrency when relevant;
- denied, timeout, network, and rate-limit outcomes;
- redacted summaries used by inspector events.

Tests that currently assert the exact callout batch should be rewritten to assert host capability invocations and final filesystem behavior. Keep tests for observable behavior and policy decisions. Delete tests that only pin the old suspend/resume mechanics.

## Execution plan

### Phase 0: In-repo proof gates

Output: a small in-repo spike that proves the exact component async binding shape before broad migration work starts.

Tasks:

1. Add a test-only provider or fixture component using the same provider build path as product providers.
2. Upgrade a spike branch to Wasmtime v46 with component async features enabled.
3. Generate bindings for one async export and one async host import.
4. Register the import with `func_wrap_concurrent`.
5. Drive three same-instance calls through `run_concurrent`.
6. Assert `max_active == 3` and elapsed time is one delay window, not three.
7. Prove operation context correlation through host imports without trusting a guest-passed integer.
8. Add negative tests for `func_wrap_async` or sync-lowered imports if keeping them nearby is useful for regression evidence.

Exit criteria:

- provider builds through `just providers build`;
- host test passes through the repo test harness;
- operation context resource or task mapping works;
- no unbounded driver task pattern is required;
- no reliance on P3 `wasi:http`.

Stop criteria:

- generated bindings cannot express async exports/imports cleanly;
- host imports cannot correlate to host-owned operation context;
- new provider calls cannot start while one call awaits an import;
- the only working path requires blocking waits inside the guest.

### Phase 1: Runtime dependency and feature upgrade

Output: Wasmtime v46 is the host runtime baseline, but product providers still use the old protocol.

Tasks:

1. Upgrade `wasmtime` to v46 with `async`, `component-model`, `component-model-async`, `cache`, `cranelift`, `winch`, and `runtime` features as appropriate.
2. Upgrade `wasmtime-wasi` to v46 and keep required P2 support.
3. Add `wasmtime-wasi-http` only in the spike or behind a future gate; do not make P3 HTTP required for the first runtime migration.
4. Keep old sync provider calls working while dependency fallout is handled.
5. Update any cache, config, or linker API changes caused by the dependency upgrade.

Validation:

- `just fmt-check`;
- `just host clippy`;
- `just host test`;
- `just providers build`;
- `all_providers_initialize_and_seal`.

### Phase 2: Async driver behind a compatibility layer

Output: one provider can run through the new store-owning driver while the old WIT surface still exists for the rest of the system.

Tasks:

1. Introduce `Instance` as a client handle and `Driver` as the store owner.
2. Move store access into the driver.
3. Run the driver on a bounded command queue.
4. Add operation context creation, cleanup, deadlines, and per-operation host import counters.
5. Keep old `start_op` and `resume` paths temporarily only if needed to make the driver land safely.
6. Add a feature-gated or test-only async operation path for the proof provider.

Validation:

- targeted driver tests for queue bounds, shutdown, timeout, and operation context cleanup;
- concurrency test with one slow host import and later same-instance calls;
- inspector test showing `provider.start`, `callout.start`, `callout.end`, `provider.end`.

Cleanup rule:

Any compatibility path added in this phase must have a named deletion phase. Do not leave the old and new drivers as peer production paths.

### Phase 3: WIT and SDK async import surface

Output: provider WIT and SDK expose async host capabilities and terminal operation returns.

Tasks:

1. Change WIT exports from `provider-step` to `provider-return`.
2. Add host capability import interfaces for HTTP, blob, git, archive, and operation context.
3. Regenerate host and guest bindings.
4. Update the provider macro to generate async operation exports.
5. Delete generated continuation implementation.
6. Replace `CalloutFuture` with generated async import wrappers.
7. Simplify `Cx` around state, operation metadata, endpoint helpers, and host capability wrappers.
8. Move rate-limit policy to the host side or explicitly preserve the SDK-side breaker until the host-side equivalent exists.

Validation:

- SDK WIT-boundary tests rewritten around fake host imports;
- provider macro tests for generated export shape;
- provider checks and builds;
- host initialization and seal tests.

### Phase 4: Migrate product providers

Output: product providers compile and run against terminal async operations.

Tasks:

1. Port providers one at a time, starting with the simplest HTTP-only provider.
2. Replace callout-specific tests with fake host service tests.
3. Preserve provider route topology and object model.
4. Keep provider-facing helpers such as `cx.http()`, `cx.endpoint()`, `cx.git()`, and `cx.archives()` stable where possible.
5. Remove any provider-local batching that existed only to optimize suspension rounds.

Validation per provider:

- `just providers check`;
- `just providers build`;
- provider-specific integration tests;
- host seal test;
- at least one runtime smoke covering read/list behavior through the mounted tree.

### Phase 5: Delete the continuation protocol

Output: the old protocol is gone.

Tasks:

1. Remove `provider-step`.
2. Remove `callout` and `callout-result`.
3. Remove `continuation` WIT interface.
4. Delete SDK `AsyncRuntime`.
5. Delete `CalloutFuture`.
6. Delete `Runtime::dispatch_callouts`.
7. Delete `Instance::resume`.
8. Delete `TestOp::callouts` and `TestOp::resume`.
9. Remove inspector `provider.suspend` and `provider.resume` events or version the inspector wire schema.
10. Update `docs/contracts/20-provider-sdk.md` and `AGENTS.md` current shape.
11. Update `docs/future/async-http.md` so it points to the new migration state rather than describing the runtime blocker as unresolved.

Validation:

- grep for `ProviderStep`, `Suspended`, `resume`, `CalloutFuture`, `dispatch_callouts`, and `provider.suspend`;
- `just providers check`;
- `just providers build`;
- `just providers validate`;
- `just host clippy`;
- `just host test`;
- live runtime smoke.

### Phase 6: Decide on P3 `wasi:http`

Output: either a separate HTTP-boundary migration plan or a deliberate deferral.

Tasks:

1. Reassess `wasmtime-wasi-http` P3 maturity on the exact Wasmtime version in use.
2. Prove `WasiHttpHooks` can enforce omnifs auth, allowlist, timeout, redaction, and error mapping.
3. Compare P3 body/resource flow with `fetch-blob` and host blob-cache requirements.
4. Decide whether buffered HTTP should become P3 `wasi:http`, remain repo-local, or support both behind one SDK helper.

Exit criteria:

- no loss of host policy enforcement;
- no loss of callout tracing;
- no forced guest-side handling of large bodies that should remain host-side;
- no provider-specific behavior leaks into the host.

## Validation matrix

| Risk | Required proof |
| --- | --- |
| Same-instance calls serialize | Three concurrent calls into one provider instance complete in one delay window |
| Host import blocks the store | Slow import does not prevent a later operation from starting |
| Guest-provided correlation is trusted | Host imports resolve a host-created operation context |
| Dropped caller commits stale effects | Timed-out operation returning effects is discarded |
| Cache invalidation race | Concurrent stale return cannot resurrect invalidated listing or canonical bytes |
| Provider flood | Bounded queue and active-task limit return provider-busy errors |
| Inspector regression | `callout.start/end` still emit with operation id, kind, summary, and outcome |
| Auth/capability regression | HTTP import enforces the same allowlist and credential injection as `HttpStack` |
| Blob regression | Large response can still stay host-side and be served through blob-backed reads |
| Toolchain mismatch | Product provider builds through `just providers build` with generated async bindings |

## Non-goals

- This is not a direct move to P3 `wasi:http`.
- This is not a `reqwest` to `hyper` migration.
- This is not a provider object-model rewrite.
- This is not a frontend change.
- This is not permission to add new provider authority without the gated-decision process.
- This is not a compatibility layer that keeps the old continuation protocol indefinitely.

## Open questions

1. Can the chosen `wit-bindgen` version generate ergonomic Rust guest bindings for async exports and async imports in the provider macro?
2. Is `borrow<operation-context>` the right WIT shape for host-owned operation identity, or should the driver use Wasmtime guest task ids instead?
3. What exact timeout outcome should the inspector wire use: `timeout`, `cancelled`, or existing `internal` with a message?
4. Should rate-limit breaker state move entirely host-side during phase three, or should that wait for the P3 HTTP decision?
5. Which provider should be the first product migration candidate?

## Recommended first patch series

1. Add an ignored or test-only async component proof under the host or itest crate.
2. Upgrade Wasmtime in that branch and prove `func_wrap_concurrent` with generated bindings.
3. Add operation context resource proof.
4. Introduce the driver module behind the proof test only.
5. Port one fake provider operation end to end.
6. Only then start deleting WIT continuation types.

The migration is worth doing if those first patches stay small and the call path becomes clearer. If the proof requires a large compatibility framework before a single real provider operation works, the design is preserving transition-era structure and should be revisited before product providers move.
