# omnifs-host

Host runtime for the [omnifs](https://github.com/0xff-ai/omnifs) virtual filesystem. Loads `wasm32-wasip2` provider components in a `wasmtime` sandbox and bridges their path-first handlers to a Linux FUSE mount, including capability enforcement, callout execution (HTTP, git), and a two-tier (in-memory + redb-backed) browse cache.

This crate is the engine inside `omnifs-cli`. Embed it directly to host omnifs providers from a custom binary.

## What it does

- **Component loading**: validates the `omnifs.provider-manifest.v1` custom section against the host's WIT world, instantiates the component, and initializes it with the JSON config during `ProviderRuntime::new`, caching the returned capabilities for sandbox grants and timer setup.
- **FUSE bridge**: implements `lookup_child`, `list_children`, `read_file` (plus mutation paths) on top of the provider's path handlers. Subtree handoffs (`#[treeref]`) resolve to bind-mounted clones on disk.
- **Callout dispatcher**: when a provider suspends with a list of HTTP / git callouts, the host runs them, then calls `resume(id, results)`. No fire-and-forget; everything is request/response.
- **Capacity-bounded cache**: listings, lookups, and file content land in capacity-bounded caches; entries leave on capacity eviction or explicit invalidation from `event-outcome` fields or the FUSE notifier. No TTLs.

## Platform

Linux only today (FUSE via `fuser`). macOS and Windows targets wait on the macFUSE / WinFsp story.

## Install

```toml
[dependencies]
omnifs-host = "0.1"
```

## Status

Pre-1.0. Library API may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
