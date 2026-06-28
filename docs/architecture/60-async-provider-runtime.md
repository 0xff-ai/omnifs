# Async provider runtime

Status: current-architecture
Scope: provider component execution, async host imports, same-instance concurrency, and callout tracing. Binding rules live in `docs/contracts/`.

## Intended model

Provider code is ordinary async Rust. A handler awaits HTTP, blob, git, archive, or blob-read work through the SDK. That await reaches a WIT async host import. Wasmtime suspends the component future, the host executes the effect, and the guest future resumes with a typed `callout-result`.

The host still owns trust and I/O. Providers do not open sockets, read credentials, or bypass capability checks. The async change moves suspension from an omnifs-specific continuation export into Wasmtime's component async runtime.

The target call site is unchanged for provider authors:

```rust
let pages = omnifs_sdk::cx::join_all(
    urls.into_iter().map(|url| cx.http().get(url).send()),
)
.await?;
```

The implementation responsibility changes:

- `Cx` carries operation identity, provider state, and the host-pushed version token.
- `CalloutFuture` awaits generated WIT async imports directly.
- `CalloutHost` is the host implementation of those imports.
- `Instance` keeps one provider store alive inside Wasmtime `run_concurrent`.
- `Runtime` validates and materializes terminal provider returns.

## WIT boundary

The provider world imports `omnifs:provider/callouts` and exports async namespace and notify methods. Namespace operations return terminal `provider-return` values directly.

Removed from the provider protocol:

- `provider-step`
- the `suspended` operation result envelope
- the exported continuation interface
- SDK-managed pending future tables
- host-side operation resume loops

Kept in the protocol:

- typed callout request and result records
- correlation ids, now passed into each async host import for tracing and attribution
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

`start-op` commands create futures inside the concurrent store. While one provider call is suspended on an async host import, the driver can accept another operation and enter the same component instance. This is the central concurrency property the migration needed to prove.

Lifecycle and close-file calls use Wasmtime's concurrent typed function path. `initialize` passes owned config bytes so it satisfies the concurrent API's static parameter requirement.

## SDK runtime

The SDK no longer owns a custom executor. The provider macro emits async namespace and notify exports that await router dispatch directly. `Cx` no longer contains yielded or delivered callout queues.

`CalloutFuture` has two states:

- `Ready`, for builder errors or local breaker failures that should not enter the host.
- `Pending`, wrapping the generated WIT async import future.

`join_all` is still useful. It polls sibling futures before yielding, which starts multiple host imports from one provider operation. It no longer depends on positional delivery from a host resume batch.

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

## Test harness

Provider integration tests still need deterministic canned HTTP and blob responses. The harness uses `Runtime::new_for_callout_tests`, which captures selected host imports and lets tests answer them.

This is test-only host plumbing. It does not reintroduce a provider continuation export. The component still suspends on async host imports. The test controller intercepts HTTP and blob fetch imports, records the callout, and waits for the test to provide the corresponding `callout-result`.

Git, archive, and blob-read imports fall through to the real host executors so tests that rely on a real cached git checkout or host blob cache continue to exercise production behavior.

## Remaining direct `wasi:http` question

This migration keeps omnifs-owned callout WIT records. That is deliberate. It removes our custom continuation protocol while preserving the existing host policy surface.

A later direct `wasi:http` migration is still possible, but it is a separate boundary change. It would move HTTP policy into `wasmtime-wasi-http` hooks and would need a new design for auth injection, domain enforcement, response body streaming, and provider ergonomics.
