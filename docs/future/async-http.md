# Future redesign: direct WASIp2 HTTP with async components

This is the redesign omnifs should pursue once async components are mature enough to preserve the concurrency we already rely on.

The target end state is clean:

- providers compile as `wasm32-wasip2` components
- providers import `wasi:http` directly
- provider code uses straight-line async I/O instead of the current effect/resume protocol
- the host keeps auth, domain policy, and transport control at the `wasi:http` boundary
- the custom continuation map and `resume(id, result)` machinery disappear

That is still the right direction. The important caveat is what has to be true before we take it on: the runtime has to support concurrent requests on a single provider instance without forcing us back into a custom suspension protocol.

## Why this redesign exists

The current provider boundary works, but it is carrying too much custom machinery:

- providers return `ProviderResponse::{Effect, Batch, Done}`
- the host executes I/O out of band
- the host re-enters the component with `resume(id, result)`
- the SDK keeps pending continuations keyed by request id

That shape solved a real problem. It lets omnifs drop the store between slow operations and keep multiple requests in flight on one provider instance. But it also spreads transport mechanics across the WIT world, the host runtime, and the generated SDK glue.

The future redesign should keep the concurrency benefit and drop the custom protocol.

## What must be true before this becomes viable

This redesign is gated on async components being production-ready for our concurrency model.

More specifically, the runtime needs to support all of these at once:

1. direct provider-side `wasi:http`
2. no custom continuation/resume boundary
3. multiple concurrent requests on one provider instance

Ordinary host-side async is not enough. Wasmtime can already suspend guest execution while an imported host function awaits internally, but that still leaves one active top-level call owning the component instance and store. For omnifs, that would flatten same-instance concurrency into a single active request.

So the trigger is not "host-side async exists." The trigger is:

- async components are mature enough to support the concurrent request shape we need
- `Config::wasm_component_model_async(true)` is a production choice, not an experimental bet
- the runtime story is strong enough that we can remove the continuation protocol without giving up throughput or responsiveness

One subtlety from the investigation is worth keeping explicit: a Cargo feature being enabled by default is not the same thing as the runtime feature being operationally ready. The redesign starts when the runtime model is ready.

## Current baseline and future target

| Concern | Current design | Future redesign |
| --- | --- | --- |
| Target | `wasm32-wasip1` plus preview1 adaptation | `wasm32-wasip2` |
| HTTP boundary | custom `fetch`-style effect | direct `wasi:http` import |
| Slow I/O suspension | custom continuation map + `resume` | runtime-level async components |
| Provider style | synchronous handlers that yield effects | straight-line async provider code |
| Host policy | custom executor around `reqwest` | `wasi:http` hooks / host transport layer |
| Same-instance concurrency | achieved by dropping the store between resumes | achieved by async component runtime |

This is not a new product idea. It is the same intent we explored earlier, but stated as the architecture we want once the runtime can carry its share of the design.

## Why `wasi:http` is still the right boundary

`wasi:http` remains the right long-term interface for outbound provider HTTP.

That gives us:

- a standard component-model HTTP interface instead of a repo-local effect enum
- a clearer contract between provider code and the host
- a better match for the rest of the WASIp2/component ecosystem
- fewer omnifs-specific transport concepts inside provider code

It also forces the right target change:

- providers move to `wasm32-wasip2`
- the preview1 adapter path goes away
- provider bindings are regenerated around the WASIp2 world

This redesign does **not** depend on `wasm32-wasip3`. The target remains `wasm32-wasip2`.

## What `wasi:http` means at the WIT level

This is worth calling out because the future redesign still needs an SDK wrapper.

At the WIT level, `wasi:http` is not just `fetch(url) -> response`. It works through:

- outgoing request resources
- request options
- future incoming response resources
- bodies and streams
- pollables

That is the right interface shape for the platform, but it is too noisy to expose directly in day-to-day provider code. Even in the future design, omnifs should add a small provider-facing HTTP layer that turns the raw binding surface into something closer to:

```rust
let response = http::send(request).await?;
let status = response.status();
let body = response.bytes().await?;
```

The wrapper is an ergonomics layer, not a concurrency mechanism. The concurrency should come from async components, not from reinventing suspension inside the SDK.

## Future provider model

Once the runtime preconditions are met, the provider-facing shape should become much simpler.

### Provider exports

Provider exports should return final results directly instead of transport envelopes.

That means removing continuation-facing concepts such as:

- `correlation-id`
- `single-effect`
- `single-effect-result`
- `effect-result`
- `provider-response`
- exported `resume`

Browse, lifecycle, and related exports should return terminal `action-result` values directly.

### Provider imports

The provider world should import:

- `wasi:http` for outbound HTTP
- repo-local `git`, `kv`, and `cache` host interfaces
- the existing logging interface

The design intent is simple: standardize HTTP, keep repo-specific capabilities repo-specific, and stop tunneling all slow operations through one generic effect enum.

### Provider implementation style

Provider code should become ordinary async Rust:

- call HTTP directly
- await the response
- shape filesystem results
- return the final `ActionResult`

That is the main readability win of the redesign.

## Future host model

The host remains responsible for policy, not just transport.

That part of the current design is solid and should survive the redesign.

### Runtime configuration

The future host runtime should use:

- `wasm_component_model(true)`
- ordinary async support
- async component support once mature enough for the target concurrency shape
- async instantiation and guest calls

The key change is that concurrency should become a runtime property, not an omnifs-specific resume protocol.

### HTTP policy and auth

The host should keep central control over:

- domain allowlists
- header restrictions
- auth injection
- timeout behavior
- error mapping

The right place for that in the future design is the `wasi:http` host layer, via `WasiHttpView` and `WasiHttpHooks`.

That preserves one of the strongest parts of the current system: providers describe what they need, but they do not own raw secret handling or unrestricted network policy.

### Store and concurrency model

This is the hard requirement the redesign must satisfy.

Today, omnifs gets same-instance concurrency by briefly entering the component, receiving an effect request, dropping the store, doing I/O, and resuming later. The future design must match the observable behavior without reproducing that protocol ourselves.

In practical terms, the runtime has to let a provider instance participate in multiple concurrent filesystem requests while provider code is suspended in async imports or awaits. If the runtime model still serializes top-level calls per instance, then the redesign is not ready, no matter how nice the code looks.

That is the central readiness check.

## What this redesign should not do

Two side investigations were useful because they narrowed the real work.

### This is not a host-only `wasi:http` refactor

Rewriting the current host HTTP executor to use `wasmtime-wasi-http` internally, while keeping the continuation boundary intact, is not the redesign this note describes.

That path might share some future policy code, but by itself it does not:

- remove continuations
- simplify provider code
- improve concurrency

It is at most a preparatory refactor.

### This is not a `reqwest` to `hyper` project

The current executor is a thin buffered client wrapper. A standalone migration from `reqwest` to `hyper` would mostly trade convenience for lower-level plumbing.

The real design question is the provider boundary and the concurrency model, not the brand name of the host HTTP client.

If this redesign happens, the host will naturally move closer to the transport shape used by `wasmtime-wasi-http` anyway. That is a consequence of the architecture, not a separate goal.

## Redesign outline

Once the runtime condition is met, the implementation sequence should look like this:

1. Switch providers and tests from `wasm32-wasip1` to `wasm32-wasip2`.
2. Redefine the WIT world around direct imports and terminal returns.
3. Regenerate host and SDK bindings.
4. Add a provider-facing async HTTP helper over the raw `wasi:http` bindings.
5. Move host HTTP policy to the `wasi:http` hook layer.
6. Convert the host runtime to async component instantiation and calls.
7. Port providers from effect dispatch to direct async imports.
8. Delete continuation storage, `resume`, and effect-result plumbing.

The dependency order matters. The runtime and WIT boundary need to change before provider code gets simpler.

## Readiness checklist for revisiting this

We should pick this back up when the answers to these questions are all comfortably "yes":

- Can a single provider instance handle multiple concurrent filesystem requests without our custom continuation protocol?
- Is async component support documented and implemented as a production path rather than a partial feature?
- Can provider-side `wasi:http` be used without collapsing same-instance concurrency?
- Can the host still enforce auth injection and outbound HTTP policy cleanly at the new boundary?
- Can we express provider code as ordinary async Rust without rebuilding our own resume system in the SDK?

That is the moment this redesign moves from "future note" to "real migration plan."

## References

- [Wasmtime async docs](https://docs.wasmtime.dev/api/wasmtime/)
- [Wasmtime `Config` docs](https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html)
- [WASIp2 docs](https://docs.wasmtime.dev/api/wasmtime_wasi/p2/index.html)
- [WASI HTTP p2 docs](https://docs.wasmtime.dev/api/wasmtime_wasi_http/p2/index.html)
- [WASI HTTP hooks source docs](https://docs.wasmtime.dev/api/src/wasmtime_wasi_http/p2/mod.rs.html)
- [WASI HTTP impl docs](https://docs.wasmtime.dev/api/wasmtime_wasi_http/struct.WasiHttpImpl.html)
- [Rust `wasm32-wasip2` target docs](https://doc.rust-lang.org/beta/rustc/platform-support/wasm32-wasip2.html)
- [NVD CVE-2026-27195](https://nvd.nist.gov/vuln/detail/CVE-2026-27195)
- [Wasmtime security advisory](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-xjhv-v822-pf94)

## Closing take

The right way to read this note is not "we decided against async HTTP." The better read is: this is the async HTTP redesign we want, and we now understand the price of doing it too early.

When async components are ready to preserve omnifs's concurrency model, this becomes a cleanup with real upside. Until then, the current continuation boundary is the mechanism that holds the design together, and this note exists so we can pick the future path back up without redoing the investigation from scratch.
