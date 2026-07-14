# Architecture overview

Status: current-architecture
Scope: the current explanatory model and rationale for `omnifs`. Binding rules live in `docs/contracts/`; this document explains how the pieces fit together.

`omnifs` projects external services such as GitHub, DNS, arXiv, Docker, Linear, and databases as a filesystem. A trusted host runtime loads each provider as a `wasm32-wasip2` component and drives it through the byte-level `omnifs:provider` WIT interface. Every consumer of the projected tree sees the same mount.

## The spine

The load-bearing decision is where meaning lives.

The host knows paths, bytes, tree structure, content types, file attributes, cache metadata, capability outcomes, and effects. It does not request an object, parse a provider object, render a representation, or derive a path-to-object mapping from payload contents.

The provider SDK owns upstream-specific meaning: identity, canonical assembly, rendering into representations, versioning, preload, revalidation, and route topology. Where provider code needs the host to mutate state, it returns an explicit effect.

This keeps the host reusable across providers and frontends. It also keeps provider compromise bounded by the authority the host resolved for that mount.

## Providers and objects

A provider is one `#[omnifs_sdk::provider]` implementation with synchronous `fn start` registering routes on a `Router`. `file` and `dir` are filesystem nouns; object, direct, blob, stream, collection, children, choices, and tree behavior are SDK faces that lower to byte-level WIT effects.

Object faces fit provider concepts with identity and replayable canonical bytes. `r.object::<O>(template, |o| ..)` and `r.file_object::<O>(template, |o| ..)` bind an `Object` to a path template. Canonical bytes are verbatim upstream bytes or a provider-assembled canonical blob. Derived and representation leaves decode the canonical through the object type and render from it.

Path-oriented routes, `r.dir`, `r.file`, and `r.treeref`, are correct when the domain is not object-shaped. Docker operational state, database browse surfaces, and subtree handoff do not need fake object identity merely to fit the object API.

Identity is layered. The provider computes a logical id from object kind and normalized identity captures. The host stores it in a mount-scoped keyspace, so two mounts with different credentials cannot share private canonical bytes for the same upstream identity.

## Callouts and effects

Provider namespace and notify calls are async component exports that return terminal results. When provider code awaits host work, it calls an async WIT import. The host executes callouts such as HTTP fetches, blob fetches, git clone/open operations, archive opens, and blob reads, then the component future resumes with the typed result.

One provider instance can serve multiple concurrent filesystem operations. Wasmtime's component async runtime owns suspension while the host owns the callout executors, auth injection, capability checks, tracing, and cache-visible effects.

Terminal host mutations travel through effects:

- canonical stores write raw upstream bytes into the object cache.
- filesystem effects write materialized files and directories into the view cache.
- invalidations remove object or listing state.

Errors do not carry effects. New terminal host mutations should be new explicit effect fields, not tunneled through callouts.

## Caches and reads

The host owns storage as opaque bytes.

- The object cache is durable canonical storage, scoped per mount.
- The view cache is derived and disposable. It can be deleted and rebuilt from canonical bytes.
- The blob cache stores large host-resident binary content by handle.

On a warm object read, the host pushes cached canonical bytes into the provider's read operation. The provider decodes and renders from those bytes. There is no provider-to-host canonical-read callout and no host-side render operation.

The object cache has no provider TTL. Entries leave by capacity eviction or explicit invalidation. View leaves can carry freshness derived from stability, because they are derived materialization rather than canonical authority.

## Dispatch and listing

Route dispatch must have one owner for precedence. Lookup, listing, read, and open all need the same route-target resolution model.

Listing honesty matters. A listing is exhaustive only when the provider actually enumerated every entry. A capped listing must stay non-exhaustive unless a real resume cursor exists. `lookup` can resolve a name that did not appear in a non-exhaustive `readdir`.

Literal route prefixes are auto-navigable directories. Capture validators participate in match candidacy, so a parse rejection can fall through to another candidate instead of becoming an accidental read-time error.

## File attributes

Projected files carry explicit size, stability, version, content type, and byte-source evidence. Stat-size and read-termination are separate: read termination must not depend on a guessed stat size.

Unknown and non-zero sizes use truthful sentinel behavior until exact size is learned from real reads. Learned-size publication belongs in shared tree/file-attr policy, not in FUSE or NFS local heuristics.

## Frontends

FUSE and NFS are protocol adapters over the same projected tree. Every frontend is a separate slim `omnifs-thin` runner selected with `fuse` or `nfs`; it contains protocol mechanics only and attaches to the daemon over the Omnifs VFS wire protocol. The CLI owns launch and teardown through drivers (`local`, `docker`, `krunkit`); the daemon is only a namespace server and attachment registry and never mounts or supervises a frontend.

FUSE owns inode tables, kernel notifications, mount/unmount mechanics, and FUSE reply construction. NFSv4.0 loopback (macOS host-native) owns filehandles, stateids, leases, NFS protocol errors, mount readiness, and teardown. Mount discovery and NFS filehandle state live under per-mount leaves under `cache/frontends/<kind>/<hash>`.

Neither frontend owns projection semantics, provider WIT calls, cache schema, root enumeration, learned-size rules, preload policy, inline-byte policy, or negative lookup policy.

A frontend (always out-of-process) consumes the same `omnifs_engine::Namespace` through the Omnifs VFS wire protocol. `omnifs-engine` remains the semantic owner; `omnifs-vfs-wire` owns postcard serialization, framing, the handshake, attach target resolution and reconnect, readiness signaling, and its client-side wire cache. Unix sockets (local), token-authenticated TCP (docker), and vsock (krunkit) are attach transports for this one internal protocol. The VFS wire protocol is separate from the provider WIT contract and does not define another projection model. Delivery is labeled by listener ownership at the daemon (UDS local, TCP docker, vsock krunkit); the guest never self-reports delivery.

## Control plane

There is one `omnifs` binary. The runtime loop lives behind hidden `omnifs daemon`. The CLI owns setup, credentials, lifecycle commands, and user-facing UX. The daemon owns runtime serving and exposes a typed local control protocol whose wire types live in `omnifs-api`.

Mount desired state is the Git `HEAD` of `$OMNIFS_HOME/mounts`; no other workspace state is versioned. The CLI writes specs through `mounts::Registry`, records desired-state commits through `mounts::Repository`, and applies one complete revision through `omnifs up` or its exact `apply` alias. The daemon receives a revision-named immutable snapshot at process start, loads it completely before readiness, and exposes no mount mutation or reconcile API.

The daemon has one runtime mode: host-native. It is a pure namespace server and attachment registry. Docker and krunkit deliver only FUSE frontends (as separate processes); they are not daemon runtime modes. Contributor dev sessions run through `scripts/dev.ts`, which writes a dedicated `~/.omnifs-dev` home and starts the daemon on the host directly.

## Auth and sandbox

Providers never hold stored tokens. Provider metadata declares auth needs and capability needs. The host resolves mount config, credential bindings, and capability grants, then injects auth on host-run callouts.

The sandbox reduces confused-deputy and lateral-movement risk. It does not claim to prevent all exfiltration: a provider with allowed network destinations can still use those destinations maliciously.

The resolved mount spec is the runtime grant authority. Required capabilities are enforced at mount materialization. Over-grant detection remains a future policy decision.

## Rejected directions

These directions were explicitly ruled out and should not return without a new gated decision:

- host-side object semantics or host-side rendering.
- provider-owned content caches or TTLs.
- fake resumable cursors or exhaustive claims over truncated listings.
- `canonical-read` callouts.
- provider-specific behavior in host, tree, FUSE, or NFS.
- macFUSE, `diskutil`, or macOS-specific FUSE mounting.
- a separate public `omnifsd` binary name.
- writable projected files that execute upstream mutations as a side effect of writes.

## Where to go next

- Binding task-area rules: `docs/contracts/00-index.md`
- File attribute rationale: `docs/architecture/10-file-attributes.md`
- Route dispatch rationale: `docs/architecture/20-route-dispatch-and-listing.md`
- Cache and effects rationale: `docs/architecture/30-cache-and-effects.md`
- Auth boundary rationale: `docs/architecture/40-auth-boundary.md`
- NFS frontend rationale: `docs/architecture/50-nfs-frontend.md`
- Async provider runtime: `docs/architecture/60-async-provider-runtime.md`
- Provider authoring: `providers/DESIGN.md` and `skills/omnifs-provider-sdk/SKILL.md`
- Roadmap and non-current ideas: `docs/future/`
