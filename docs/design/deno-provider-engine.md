# Deno as a provider engine

Status: exploratory
Scope: `crates/host/src/runtime` (engine boundary, callout dispatch), `crates/omnifs-sdk` (parallel TS SDK), provider authoring story

## Context

Today every omnifs provider is a `wasm32-wasip2` component implementing the
`omnifs:provider` WIT interface. The host (`CalloutRuntime` in
`crates/host/src/runtime/mod.rs`) instantiates the component, drives it
through `lifecycle` / `browse` / `resume` / `notify`, executes the
provider's `Callout`s on its behalf, and applies terminal-embedded side
effects (preloads, invalidations) at the response boundary.

This document evaluates whether a second engine — running provider code
inside a V8 isolate via Deno's runtime — is worth shipping alongside the
WASM engine, and if so, what shape it has to take to keep the host's
contract intact.

The motivating pull is developer onboarding: TypeScript is a much wider
talent pool than `wit-bindgen` + `wasm32-wasip2` Rust, and a JS/TS
authoring path lowers the barrier to writing simple read-only providers.
The concern is that V8 carries a meaningfully larger sandbox surface, a
larger memory baseline, and an ecosystem (`fetch`, `npm:*`) whose
defining feature is direct outbound I/O — exactly what omnifs's caching
and capability story relies on the host to mediate.

## Non-negotiable invariants

Any second engine must preserve every one of these. They are how the
host stays correct.

1. **Host-mediated I/O.** All HTTP, git open, and (future) WebSocket
   traffic flows through `Callout` → `HttpExecutor` / `GitExecutor` →
   `CalloutResult`. The provider never opens its own sockets. This is
   what lets `CapabilityChecker` enforce domain allowlists, what lets
   `AuthManager` inject credentials, and what lets the host cache
   responses and reason about freshness.
2. **Terminal-boundary side effects.** `apply_terminal_boundary` in
   `runtime/mod.rs:489` consumes `dir-listing.preload`,
   `lookup-entry.preload`, and `event-outcome.invalidate-{paths,prefixes}`
   *before* the terminal is handed back to the caller. Both engines
   must produce these fields in the same shape.
3. **Correlation and cancellation.** Every in-flight call is keyed by a
   `correlation-id`; `cancel(id)` must terminate the continuation
   cleanly without leaking work, sockets, or timers.
4. **Capability gating.** `CapabilityGrants` (domains, git repos,
   memory, `needs_git`) are decided by the host from `InstanceConfig`
   and the provider's declared `RequestedCapabilities`. The engine
   never bypasses them.
5. **Declared handler manifest.** The host needs the provider's
   declared route shape (currently
   `manifest::read_declared_handlers_from_wasm`) for browse-pipeline
   precomputation. A second engine must surface the same `DeclaredHandler`
   list.
6. **Subtree handoff.** When a `#[treeref]` route matches, the WIT
   returns a `tree-ref` and the host resolves it to a bind-mounted
   clone directory via `git::GitExecutor`. Engine-agnostic.

## Decisions

### D1. Introduce a `ProviderEngine` trait, with `WasmEngine` as the only initial impl

The Wasm specifics — `wasmtime::Store`, `Component`, `Linker`,
`Provider::instantiate`, `omnifs_provider_lifecycle`, etc. — currently
live alongside cache, invalidation, activity-table, and capability
logic in `CalloutRuntime`. Before a second engine is feasible, the
boundary needs to be explicit.

```rust
pub trait ProviderEngine: Send + Sync {
    fn initialize(&self, config: &[u8]) -> Result<OpResult>;
    fn call_lookup_child(&self, id: u64, parent: &str, name: &str)
        -> Result<ProviderReturn>;
    fn call_list_children(&self, id: u64, path: &str) -> Result<ProviderReturn>;
    fn call_read_file(&self, id: u64, path: &str) -> Result<ProviderReturn>;
    fn call_resume(&self, id: u64, results: &[CalloutResult])
        -> Result<ProviderReturn>;
    fn call_on_event(&self, id: u64, ev: &ProviderEvent)
        -> Result<ProviderReturn>;
    fn call_close_file(&self, handle: u64) -> Result<()>;
    fn cancel(&self, id: u64);
    fn capabilities(&self) -> RequestedCapabilities;
    fn config_schema(&self) -> Option<String>;
    fn declared_handlers(&self) -> &[DeclaredHandler];
    fn shutdown(&self) -> Result<()>;
}
```

`CalloutRuntime` keeps cache, invalidation, activity table,
capability checker, `drive_callouts`, and `apply_terminal_boundary`
verbatim, and holds a `Box<dyn ProviderEngine>` instead of a Wasmtime
store directly. This refactor is worth doing on its own merits — it
removes a class of "WASM-isms creep into cache logic" bugs and makes
the contract between the engine and the rest of the host legible —
and is a hard prerequisite for any second engine.

### D2. The Deno engine runs one V8 isolate per mount and exposes a single host module

Each mount instantiates a long-lived isolate that loads the provider's
TypeScript entry. The provider exports a static manifest plus async
handlers that are invoked by the engine when the host requests
lookup / list / read / on-event.

The isolate is booted with permissions stripped:

- `--deny-net --deny-env --deny-read --deny-write --deny-run --deny-ffi`

A single host module — `omnifs:` (or registered `Deno.core` ops) —
provides every escape hatch the provider is allowed to use:

- `omnifs.fetch(req)` → host-mediated HTTP
- `omnifs.gitOpenRepo(req)` → host-mediated git open
- `omnifs.log(level, message)` → host log integration
- (future) `omnifs.ws*` for WebSocket callouts

Providers cannot use `globalThis.fetch`, `node:http`, or any
`npm:*` HTTP transport. The TS SDK exposes `cx.fetch` (and friends)
that wrap the host ops; this is the only legal path to the network.

This is a real cost: it shrinks the JS ecosystem benefit to libraries
that don't own their own transport (zod, date-fns, lodash, schema /
parsing libs). HTTP-shaped libraries like Octokit must be re-wrapped
on top of `cx.fetch` to be usable. We accept this cost; without it,
the host loses caching, capability enforcement, and credentialing.

### D3. The author API uses `await`; the WIT suspend/resume protocol is implicit

Wasm components don't have async, so the WIT exposes suspension as a
visible state — `provider-return { terminal: none, callouts: [..] }`
— and the host calls `resume(id, results)` to continue. JS does have
async, so the natural author API hides this:

```ts
import { defineProvider, file } from "@omnifs/sdk";

export default defineProvider({
  config: ConfigSchema,
  mounts: ["/github/{owner}/{repo}/issues"],
  routes: [
    file(
      "/github/{owner}/{repo}/issues/{n}/title",
      async (cx, { owner, repo, n }) => {
        const issue = await cx.fetch(
          `https://api.github.com/repos/${owner}/${repo}/issues/${n}`,
        );
        return fileContent(issue.title, {
          siblingFiles: { body: issue.body, state: issue.state },
        });
      },
    ),
  ],
});
```

The engine implements suspend/resume below the author's awareness:

1. Each WIT entry point (`call_lookup_child`, etc.) posts a message
   into the isolate and awaits a reply.
2. `cx.fetch` is registered as a host op. When the JS handler calls
   it, the op returns a Promise tied to a host-allocated callout slot
   and yields back to the engine driver loop.
3. The driver collects all pending callouts produced during this slice
   of JS execution, returns
   `ProviderReturn { callouts, terminal: None }` to `CalloutRuntime`.
4. `drive_callouts` runs the callouts (re-using `HttpExecutor`,
   capability checks, auth) and calls back into the engine with results.
5. The engine resolves the corresponding JS Promises and lets the
   handler continue. When the handler `return`s, that's the terminal.

The contract between `CalloutRuntime` and the engine is identical to
the WASM case. Only the engine internals differ.

### D4. The provider's static manifest is read once after `initialize`

`read_declared_handlers_from_wasm` in
`crates/host/src/runtime/manifest.rs` parses route metadata baked
into the wasm binary by SDK macros. The Deno engine reads the
equivalent shape via a one-off `op_get_manifest` call right after
`initialize`. The TS SDK's `defineProvider` builds the manifest from
the route definitions, mirroring what the Rust `#[handlers]` /
`#[provider]` macros emit today.

The downstream `DeclaredHandler` consumers (browse pipeline, listing
exhaustiveness) are unchanged.

### D5. Cancellation drains both halves of the in-flight state

`cancel(id)` must clean up two things:

1. The JS-side Promise (and anything it's awaiting). Implemented by
   tracking the set of host-op tokens issued for `id`, rejecting them
   with a cancellation error, and letting `await` propagate the
   rejection.
2. The host-side callout in flight. The existing `InFlight` table
   already does this for the WASM engine; the Deno engine reuses it.

If a handler ignores the rejection and keeps doing CPU work, we fall
back to `Deno.core.terminateExecution` and discard the isolate state
for that correlation. This is a hard cancel, not graceful, and is the
last resort.

### D6. Cache and invalidation logic stays engine-agnostic

`apply_preloads`, `apply_event_outcome`, `apply_terminal_boundary`,
`InvalidationState`, the L2 redb cache, and the FUSE notifier all
live above the engine boundary in `CalloutRuntime`. Both engines
produce the same `OpResult` / `ProviderReturn` shapes; the host
applies side effects identically.

This is the property that lets us add an engine without rewriting
caching.

## Things that get harder

- **Sandbox surface.** WASI's syscall surface is small and audited.
  V8 has had escape CVEs every year. For a host that mounts
  third-party providers, this is a real downgrade. Mitigations:
  enforce `--deny-*` permissions, disable raw `Deno.core` ops the
  provider doesn't need, run the isolate in a separate OS process if
  the threat model demands it.
- **Memory baseline.** A cold Wasmtime instance is ~1 MB; a cold V8
  isolate is tens of MB. With many mounts this adds up. V8 startup
  snapshots help but add build complexity.
- **Startup cost.** V8 cold start is in the tens of ms; relevant for
  test cycles and for `omnifs status`-style commands that touch many
  mounts.
- **Reproducibility / supply chain.** WASM components are
  content-addressed binaries. TS providers depend on package
  resolution (`deno.lock`, registries). Pinning is doable but the
  surface is wider.
- **Ecosystem expectations vs reality.** Authors will reach for
  `npm:octokit` and discover it can't talk to the network. Clear
  documentation and SDK-side wrappers for the most common services
  are the only mitigation.

## Things that stay the same

- WIT terminal shapes (`OpResult`, `ProviderReturn`, `LookupResult`,
  `ListResult`, `EventOutcome`).
- Cache layer (L1, L2 redb, FUSE notifier integration, invalidation
  prefixes).
- Capability checks and auth injection.
- Subtree handoff via `tree-ref` and the cloner.
- `CalloutRuntime::drive_callouts` loop and
  `apply_terminal_boundary`.

## Suggested incremental path

1. **Land the `ProviderEngine` trait.** Pure refactor of
   `CalloutRuntime` to delegate WIT-export calls through a trait,
   with `WasmEngine` as the sole implementation. No behavior change.
   This is worth doing whether or not Deno ships.
2. **Minimal Deno engine.** Implement `omnifs.fetch` and `omnifs.log`
   only; skip git, WS, reconcile, mutations, and tree-ref handoff.
3. **Port one provider.** Either rewrite `providers/dns` (simple,
   no HTTP) as TS to validate the lifecycle/manifest path, or pick
   a small read-only GitHub route to validate the fetch path.
4. **Parity test.** Same mount, same fixtures, run both engines and
   assert identical terminals (including preloads and invalidations).
   This is where the design either holds or leaks.
5. **Decide.** If parity holds, expand to git/WS/reconcile. If not,
   the leak tells you what part of the contract was implicit.

## Recommendation

Step 1 is unconditionally good. Steps 2–4 are an experiment, not a
commitment. The honest case for shipping a Deno engine is *developer
onboarding*, not capability — and that case is only worth the V8
sandbox/memory cost if there are providers that wouldn't otherwise be
written. Hold on the experiment until there's a concrete provider in
hand whose author wouldn't ship a Rust version.
