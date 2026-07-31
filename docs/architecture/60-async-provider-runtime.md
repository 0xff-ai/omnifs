# Async provider runtime

Status: current-architecture
Scope: provider component execution, async host imports, same-instance
concurrency, and callout tracing.

Read when: changing provider instance lifecycle, async WIT calls, host callouts,
same-instance scheduling, cancellation, shutdown, or callout tracing.

Binding contracts: `docs/contracts/10-system.md`,
`docs/contracts/20-provider-sdk.md`, and
`docs/contracts/60-build-validation.md`.

## Intended model

Provider handlers await SDK HTTP, blob, or Git work. The await reaches a WIT
async import; Wasmtime suspends the component while the trusted host performs
I/O and returns a typed result. Providers gain no direct socket or credential
access.

The provider call site is:

```rust
let pages = omnifs_sdk::cx::join_all(
    urls.into_iter().map(|url| cx.http().get(url).send()),
)
.await?;
```

| Owner | Responsibility |
|---|---|
| `Cx` | Operation identity, provider state, host-pushed version |
| `CalloutFuture` | Direct WIT import future |
| `CalloutHost` | Trusted host implementation |
| `Instance` | One provider store in `run_concurrent` |
| `Runtime` | Validate typed payload and terminal effects |

## WIT boundary

The provider world imports `omnifs:provider/callouts` and exports async
namespace and notify methods. Each operation returns its typed result and
effects directly.

The protocol retains typed callout records, operation IDs for host attribution,
and terminal effects. It has no `provider-step`, suspended envelope,
continuation export, SDK pending table, or host resume loop. Inspector
correlation follows tracing parents rather than a parallel trace ID.

Lifecycle exports are terminal and synchronous in WIT but use Wasmtime's
concurrent call path because the store has component async enabled.

## Host runtime

`Instance` owns the Wasmtime store on a dedicated driver thread. It creates a
current-thread Tokio runtime, instantiates asynchronously, then enters
`Store::run_concurrent`.

The driver accepts three kinds of commands:

- lifecycle and host-state commands, including initialization, callout
  installation, and shutdown
- namespace operations for lookup, listing, whole-file reads, open, ranged
  reads, and close
- provider event delivery

Namespace and event commands create futures inside the concurrent store. While
one provider call is suspended on an async host import, the driver can poll
another operation on the same component instance. Lifecycle and close
operations use Wasmtime's concurrent typed function path.

## SDK runtime

The SDK owns no executor. Its macro emits async namespace and notify exports
that await router dispatch; `Cx` has no yielded or delivered callout queues.

`CalloutFuture` is `Ready` for local builder or breaker failures and `Pending`
for a generated WIT import future.

`join_all` polls siblings before yielding, starting independent imports without
positional host resume batches.

## Callout host and tracing

`CalloutHost` is the single host import implementation for provider effects.
HTTP fetch and blob fetch use the asynchronous HTTP stack. Git open shells out
to `git`, so `CalloutHost` runs it on Tokio's blocking pool. Cached body reads
occur through `MountResources` and `BodyStore`; they do not make another host
callout.

Each async command captures its tracing parent and instruments the component
future on the driver thread. Every import creates one Inspector span with
operation ID, callout index and kind, redacted summary, and terminal outcome.

Executor-specific child spans on the `omnifs_callout` target retain detailed,
redacted request and response diagnostics for daemon logging. They do not emit
a second Inspector lifecycle.

`InspectorLayer` alone converts span callbacks into typed records, bounded
history, and the control stream. `InspectorLine` owns the JSONL unit for
subscription, tee, recording, replay, and plain output. Dropping a future
closes its span with an internal outcome unless a specific result was recorded.

## Concurrency and blocking

One provider instance runs on one event-loop thread, so concurrency depends on
each suspension point yielding:

- **Async callouts yield.** HTTP returns control to the event loop while host
  work runs, so other operations progress.
- **Git opens are offloaded.** `GitExecutor::open_repo` shells out to `git` and
  blocks. `CalloutHost::run` sends it to Tokio's blocking pool, so a slow clone
  suspends only its own operation.
- **WASI Preview 2 imports still block the instance.** With the pinned Wasmtime
  46.0.1 implementation, `wasmtime_wasi::p2::add_to_linker_async` binds WASI
  functions on the legacy `func_wrap_async` path, which holds the store
  exclusively across the await (`StoreFiberYield::KeepStore`). A provider that
  blocks on WASI I/O therefore serializes the instance for that wait. Only a
  move to concurrent WASI host bindings changes this property.

## Queue, cancellation, and shutdown

`Instance` sends commands through an unbounded channel. The driver keeps
namespace and event calls in `FuturesUnordered`; the channel provides no
backpressure limit.

Dropping a caller after its command was sent drops the reply receiver but does
not explicitly cancel the provider call. Driver shutdown stops command
dispatch, invokes the provider lifecycle shutdown export, and then drops
remaining in-flight call futures as the driver exits. A driver failure closes
the command and reply channels and surfaces as an engine error to callers.

These are current mechanics, not a provider-visible scheduling guarantee.
Changes to queue bounds, caller cancellation, draining, or shutdown order need
an explicit runtime design because they affect every operation on one provider
instance.

## Test harness

`load_mount_table_for_callout_tests` keeps the production construction path but
captures HTTP and blob imports for deterministic responses. This is host test
plumbing, not a provider continuation protocol. Git imports still use the real
executor.

## Direct `wasi:http` option

Omnifs-owned callout records preserve host policy without a custom continuation
protocol. Direct `wasi:http` would move policy into `wasmtime-wasi-http` hooks
and needs a separate design for auth, domains, body streaming, and SDK use.
