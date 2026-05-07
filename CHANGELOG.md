# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

- FUSE filesystem on Linux that projects external services into local paths, with macOS and Windows planned.
- WASM-based provider architecture using the WIT Component Model (`wasmtime`); each provider is a `wasm32-wasip2` component implementing the `omnifs:provider` WIT interface.
- Per-provider capability declarations (HTTP domains, auth types, memory limits, git/websocket/streaming flags) enforced by the host runtime.
- GitHub provider mounted at `/github`, projecting repos, issues, PRs, CI runs, diffs, and source trees as files. Source trees are bind-mounted clones (cloned on demand via SSH); issues and PRs project per-item directories with title, body, state, and comments as separate files.
- Git-backed reconciliation scaffolding via a custom remote helper (read path live; mutation path WIP per the design docs).
- arXiv provider mounted at `/arxiv`, projecting per-paper subtrees (`paper.pdf`, `source.tar.gz`, `metadata.json`, `links.json`, `versions/v{n}/`) under `/arxiv/papers/{id}` and the `/categories/{cat}/{ym|new|updated|by-author}`, `/authors/{author}/{...|by-category}`, and `/search/{query}` scopes.
- DNS provider mounted at `/dns`, projecting record types (A/AAAA/MX/NS/TXT/CNAME/SOA/SRV plus `_all` and `_raw`) over DNS-over-HTTPS, with resolver-scoped queries via `/dns/@{resolver}/...` and reverse lookups via `/dns/_reverse/{ip}`.
- Path-first SDK with attribute macros: `#[dir]`, `#[file]`, `#[treeref]`, `#[bind]`, `#[mutate]` inside `#[handlers] impl ...` blocks; `#[subtree] impl B { ... }` for typed subtree dispatch (mounted at any number of `#[bind("...")]` sites); `#[config]` and `#[provider(mounts(...))]` at the top level. Path patterns support literal segments, bare captures (`{name}`), prefix captures (`v{ver}`), and rest captures (`{*tail}`).
- Auto-navigable intermediate directories: any literal-segment prefix of a registered route is a directory without an explicit handler, so providers don't write no-op stubs for navigation nodes. Listings carry `exhaustive=false` whenever a sibling capture or rest segment lives at depth+1, preventing the host's negative cache from short-circuiting valid dynamic-capture lookups.
- Parse-function fallthrough in route matching: when the highest-precedence pattern's parse function rejects a candidate path, the dispatcher falls through to the next-most-specific candidate. Per-segment validators participate in match candidacy rather than acting as a post-match check.
- Two-tier (L0 in-memory, L2 redb-backed) browse cache with negative caching, projected-file extraction from listings, and `event-outcome` driven invalidation; sibling preload on lookup terminals so the host caches projected siblings alongside the primary entry.
- Cross-listing PR preload and hybrid issue/PR pagination in the GitHub provider.
- HTTP SDK gains POST + raw / JSON bodies and adopts `http` crate request/response types.
- Docker Compose dev workflow (`compose.yaml`, `just dev`, `just shell`) with self-starting published image.
- `docs/design/path-dispatch-and-listing.md` as the perennial reference for routing precedence, listing semantics, and the `lookup`/`readdir` authority split; `docs/design/projected-file-sizes.md` documenting the `direct_io` redesign.

### Changed

- SDK and host runtime are redesigned around path-first handlers and callouts. Effect-based handler signatures are replaced by free-function path handlers that return either a terminal `op-result` or a list of `callout`s for the host to execute and resume. The previous effect-style API is removed.
- Provider configuration moved from TOML to JSON. The host parses each mount's JSON config into `InstanceConfig` and re-serializes the provider-specific `"config"` object as JSON bytes for `initialize()`; providers deserialize via `serde_json::from_slice` (the SDK's `#[config]` macro wires this up automatically).
- Subtree handoff is now declared with `#[treeref("...")]` (renamed from the previous `#[subtree]`) so `#[subtree]` is free for typed-subtree-dispatch impl blocks. The WIT result variant is still spelled `subtree`.
- Dir and file handlers can co-exist on identical rest-captured templates; the parent dir handler's projection authoritatively decides the child's kind at lookup.
- Handler `cx` parameter is now optional, and the `DirCx` lifetime is dropped.

### Fixed

- Bind exact-match lookups no longer return a bare `Lookup::entry` with an implied `exhaustive: true` over an empty sibling set. The host's lookup-side cache treated the bare entry as "the bind has no children" and wrote an exhaustive empty `Dirents` at the bind site, causing subsequent `readdir` to short-circuit before the typed subtree's `list_children` ran.
- Parent listings that returned exhaustive empty no longer poison dynamic-capture child lookups. With the auto-navigable rule and the SDK's exhaustive-flag computation, listings under parents with capture children at depth+1 are correctly non-exhaustive, so the host's negative cache leaves room for the capture handler to dispatch.
- GitHub route projections normalized to honor sibling preload, projected-files, and cross-listing PR preload paths.
