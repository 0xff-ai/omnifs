---
title: Glossary
description: Crisp definitions of omnifs terms — provider, mount, callout, terminal, projection, EffectiveConfig, and more.
---

Definitions of the terms used throughout the omnifs docs and codebase. Where a term has a
fuller treatment, the entry links to the relevant Concepts page.

## Core model

**provider**
A WebAssembly component that projects an external domain (GitHub, DNS, arXiv, and so on)
into the filesystem namespace. Providers implement the `omnifs:provider` WIT interface and
run sandboxed inside the host. See [Provider model](/concepts/provider-model/).

**mount**
One configured provider instance, described by a mount config and exposed at a mount root.
The CLI stores mount configs under `~/.omnifs/config/mounts/`. See
[Mount schema](/reference/mount-schema/).

**mount root**
The top-level path a mount occupies, for example `/github` or `/dns`. The provider's
namespace lives beneath it.

**host**
The omnifs runtime. It owns the FUSE mount, loads provider components, runs all I/O on
their behalf, manages caching, and enforces auth and network policy. See
[How it works](/introduction/how-it-works/).

## Browse and dispatch

**router**
The host component that maps a requested path to the provider handler that owns it,
applying route precedence and per-segment validators. See
[Path dispatch](/concepts/path-dispatch/).

**auto-navigable directory**
A directory that exists implicitly because some registered route has a literal-segment
prefix at that depth. Providers do not write stub handlers for these intermediate
navigation nodes.

**exhaustive listing**
A directory listing the host can treat as complete. A listing is non-exhaustive
(`exhaustive=false`) whenever a sibling route at the next depth has a capture or rest
segment, meaning more children may exist than were listed.

**treeref**
A subtree-handoff route. When it matches, the provider returns a `TreeRef` that the host
resolves to a bind-mounted clone directory (for example a GitHub repo cloned on demand).

**subtree (typed dispatch)**
A typed subtree handler block (`#[subtree] impl B { ... }`). The host parses the prefix
captures, constructs the subtree type, and dispatches the remaining path suffix through
the subtree's own inner route registry.

## Provider protocol

**callout**
A request a provider makes for the host to perform an external operation — fetch an HTTP
endpoint, open a Git repo, and so on. Providers never touch the network or Git directly;
they describe the work and the host executes it. Callouts are strictly request/response.
See [Callout runtime](/concepts/callout-runtime/).

**continuation**
The suspended provider state kept while the host services callouts, keyed by a correlation
ID so the provider can resume from where it left off.

**resume**
The host call that re-enters a provider with the results of its callouts, driving the
stored continuation forward. (`resume(id, results)`.)

**terminal**
A final result a provider returns instead of suspending — the end of a browse, lookup, or
read operation rather than another callout batch.

**op-result**
The terminal result value wrapped in a `provider-return`. The outcome of `lookup_child`,
`list_children`, or `read_file`.

## Data and caching

**projection**
The set of file attributes a provider declares for a projected file — `Size`, `Bytes`,
`ReadMode`, `Stability`, and optional version evidence — via the SDK's `Projection` API.
See [File attributes](/concepts/file-attributes/).

**sibling files**
Additional files derivable from the same upstream payload as the requested file. Providers
return them together (`with_sibling_files(..)`) so a later stat or read of a sibling avoids
a round trip.

**preload**
Content for named child paths attached to a listing or directory lookup so the host caches
it alongside the listing. Used for nested children that are not direct siblings.

**on-event / event-outcome**
An `on-event` handler reacts to a change and returns an `event-outcome` record carrying
`invalidate-paths` and `invalidate-prefixes`. The host applies these invalidations at the
response boundary. There are no TTLs; entries leave the cache only by capacity eviction or
explicit invalidation. See [Caching](/concepts/caching/).

## Configuration and auth

**EffectiveConfig**
The mount config after the provider's embedded metadata has been merged into the parsed
JSON. `provider_id` is always present. Credential materialization operates on this, not on
raw mount JSON. See [Mount schema](/reference/mount-schema/).

**LoadedMount**
The result of `ProviderCatalog::load_mount()` — a wrapper around the `EffectiveConfig` for
a mount.

**AuthManifest**
The runtime auth description for a provider. The host and CLI derive it from the provider's
embedded `omnifs.provider.json` metadata via `ProviderManifest::wasm_auth_manifest()`.
See [Auth and credentials](/concepts/auth-credentials/).

**CredentialKey**
The address of a host-managed credential. Its public wire form, `storage_key()`, is
`provider:scheme:account` (for example `github:pat:default`).

## Runtime internals

**inode table**
The host's mapping of filesystem inodes to provider paths, backing FUSE's stat and lookup
operations.

**clone manager**
The host subsystem that clones Git repos on demand (over SSH) and serves their working
trees as bind-mounted passthrough directories behind `treeref` handoffs. See [Cloning](/concepts/cloning/).
