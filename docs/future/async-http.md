# Future redesign: direct WASI HTTP

Status: future

The current provider runtime already uses Wasmtime component async. Provider handlers await omnifs-owned async host imports, and one provider instance can have multiple filesystem operations in flight. See `docs/architecture/60-async-provider-runtime.md` for the current design.

This note describes a possible boundary change: replacing the repo-local HTTP callout records with direct `wasi:http` imports.

## Current baseline

Current provider HTTP uses:

- SDK request builders such as `cx.endpoint(..).get(..).send_checked().await`
- generated async imports in `omnifs:provider/callouts`
- host executors that enforce domain capability checks, auth injection, timeout behavior, redaction, tracing, and error mapping
- terminal namespace returns with effects

There is no provider-step or continuation export in the current protocol. The custom suspension mechanism is gone.

## Future target

The possible future target is:

- providers still compile as `wasm32-wasip2` components
- provider code still uses SDK request builders
- the SDK lowers HTTP requests to `wasi:http`
- the host enforces policy through `wasmtime-wasi-http` hooks and transport plumbing
- non-HTTP host effects, such as git, blob cache, and archive opens, remain repo-local imports unless a standard interface replaces them

This is an HTTP boundary migration, not an async-runtime migration.

## Why it may still be worth doing

Direct `wasi:http` would align provider HTTP with the component ecosystem and remove one repo-local HTTP request/response envelope.

The practical benefits would be:

- less custom WIT for HTTP specifically
- a clearer split between standard HTTP resources and omnifs-specific effects
- a path toward streaming bodies through standard resource types instead of buffering every HTTP response into an omnifs record

## Open design questions

The migration is not automatic just because component async works.

The host must still answer these questions:

- How does auth injection attach to `wasi:http` outgoing requests without exposing credentials to providers?
- Where do domain allowlists, Unix socket grants, method restrictions, and timeout policy live?
- How do redacted trace fields map from `wasi:http` resources and streams back to the current `omnifs_callout` observability surface?
- How do provider SDK helpers present simple buffered responses while preserving an escape hatch for streaming large bodies?
- What remains in omnifs-owned WIT for blob fetches, git opens, archive opens, and blob reads?
- How do provider integration tests script `wasi:http` responses without bypassing the same host policy layer production uses?

## Non-goals

- Do not reintroduce provider continuations or SDK-managed resume queues.
- Do not move credential storage into providers.
- Do not make providers own arbitrary network authority.
- Do not replace a working buffered HTTP executor with lower-level transport code unless the boundary migration needs it.

## Execution outline

1. Prototype one provider route using `wasi:http` behind the existing SDK endpoint API.
2. Map the current `HttpStack` policy decisions to `wasmtime-wasi-http` hooks or an equivalent host adapter.
3. Preserve `omnifs_callout` span fields or define a reviewed replacement span contract.
4. Add integration tests that prove auth injection, denied domains, response status mapping, large-body behavior, and captured test responses.
5. Migrate SDK HTTP and endpoint helpers while keeping provider call sites stable.
6. Remove HTTP-specific variants from the omnifs callout interface only after all providers and tests use the standard path.

## References

- Current async provider runtime: `docs/architecture/60-async-provider-runtime.md`
- Provider SDK contract: `docs/contracts/20-provider-sdk.md`
- System trust contract: `docs/contracts/10-system.md`
- WASI HTTP roadmap: https://wasi.dev/roadmap
