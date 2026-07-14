# Async provider runtime

Status: current-architecture
Scope: provider component execution, async host imports, same-instance concurrency, and callout tracing. Binding rules live in `docs/contracts/`.

## Intended model

Provider code is ordinary async Rust. A handler awaits HTTP, blob, git, archive, or blob-read work through the SDK. That await reaches a WIT async host import. Wasmtime suspends the component future, the host executes the effect, and the guest future resumes with a typed `callout-result`.

The host owns trust and I/O. Providers do not open sockets, read credentials, or bypass capability checks. Suspension lives in Wasmtime's component async runtime rather than an omnifs-specific continuation export.

The provider call site is:

```rust
let pages = omnifs_sdk::cx::join_all(
    urls.into_iter().map(|url| cx.http().get(url).send()),
)
.await?;
```

Current responsibilities:

- `Cx` carries operation identity, provider state, and the host-pushed version token.
- `CalloutFuture` awaits generated WIT async imports directly.
- `CalloutHost` is the host implementation of those imports.
- `Instance` keeps one provider store alive inside Wasmtime `run_concurrent`.
- `Runtime` validates and materializes typed operation payloads together with terminal effects.

## WIT boundary

The provider world imports `omnifs:provider/callouts` and exports async namespace and notify methods. Namespace and notify operations return operation-specific typed result/effects tuples directly.

Not part of the provider protocol:

- `provider-step`
- the `suspended` operation result envelope
- the exported continuation interface
- SDK-managed pending future tables
- host-side operation resume loops

The protocol retains:

- typed callout request and result records
- correlation ids passed into each async host import for tracing and attribution
- terminal effects for canonical stores, filesystem writes, and invalidations

Lifecycle exports remain terminal and synchronous at the WIT level. Because the store is configured for component async, the host still invokes those exports through Wasmtime's concurrent call path.

## Host runtime

`Instance` owns the Wasmtime store on a dedicated driver thread. The driver creates a current-thread Tokio runtime, instantiates the component asynchronously, then enters `Store::run_concurrent`.

Commands sent to the driver are:

- `initialize`
- `start-op`
- `shutdown`
- `manifest-json`
- `close-file`
- `set-callouts`

`start-op` commands create futures inside the concurrent store. While one provider call is suspended on an async host import, the driver can accept another operation and enter the same component instance. This is the central concurrency property.

Lifecycle and close-file calls use Wasmtime's concurrent typed function path. `initialize` passes owned config bytes so it satisfies the concurrent API's static parameter requirement.

## SDK runtime

The SDK owns no custom executor. The provider macro emits async namespace and notify exports that await router dispatch directly. `Cx` contains no yielded or delivered callout queues.

`CalloutFuture` has two states:

- `Ready`, for builder errors or local breaker failures that should not enter the host.
- `Pending`, wrapping the generated WIT async import future.

`join_all` polls sibling futures before yielding, which starts multiple host imports from one provider operation without positional delivery from a host resume batch.

## Callout host and tracing

`CalloutHost` is the single host import implementation for provider effects. It maps each WIT callout to the existing executor:

- HTTP fetches go through `HttpStack`.
- Blob fetches and reads go through `BlobExecutor`.
- Git opens go through `GitExecutor`.
- Archive opens go through `ArchiveExecutor`.

Tracing is preserved at the callout boundary. Every host import creates the same `omnifs_callout` span shape with:

- operation id
- callout index
- callout kind
- executor-specific request fields
- executor-specific outcome fields

The inspector also remains callout-oriented. It begins when the async host import arrives and finishes when the executor returns the `callout-result`.

## Concurrency and blocking

The driver runs one provider instance on a single event-loop thread, so same-instance concurrency depends on every suspension point yielding that thread rather than blocking it.

- **Async callouts yield.** HTTP and archive callouts are `async` and return control to the event loop while the host does the work, so other in-flight ops keep progressing.
- **Synchronous executors are offloaded.** `GitExecutor::open_repo` (which shells out to `git`) and `BlobExecutor::read` (a bounded disk read) are synchronous. `CalloutHost::run` runs them on the Tokio blocking pool via `spawn_blocking`, so a slow clone or read suspends only its own op, not the whole instance. Without this, the blocking call would run inside the host-task future's poll on the event-loop thread and stall every concurrent op.
- **WASI Preview 2 imports still block the instance.** `wasmtime_wasi::p2::add_to_linker_async` binds WASI functions on the legacy `func_wrap_async` path, which holds the store exclusively across the await (Wasmtime `StoreFiberYield::KeepStore`). A provider that blocks on WASI I/O (a preopened-file read, `wasi:io/poll`) therefore serializes the instance for the duration of that wait, unlike an omnifs callout. This is inherent to WASI p2; only a move to WASI p3 (concurrent host bindings) would change it. Providers do upstream work through omnifs callouts, not WASI, so this is rarely on the hot path.

## Test harness

Provider integration tests need deterministic canned HTTP and blob responses. The harness uses `Runtime::new_for_callout_tests`, which captures selected host imports and lets tests answer them.

This is test-only host plumbing, not a provider continuation export. The component suspends on async host imports. The test controller intercepts HTTP and blob fetch imports, records the callout, and waits for the test to provide the corresponding `callout-result`.

Git, archive, and blob-read imports fall through to the real host executors so tests that rely on a real cached git checkout or host blob cache continue to exercise production behavior.

## Direct `wasi:http` option

The current design keeps omnifs-owned callout WIT records. This preserves the host policy surface without a custom continuation protocol.

Direct `wasi:http` remains possible, but it is a separate boundary change. It would move HTTP policy into `wasmtime-wasi-http` hooks and would need a new design for auth injection, domain enforcement, response body streaming, and provider ergonomics.
