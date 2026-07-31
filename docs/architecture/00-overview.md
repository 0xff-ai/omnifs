# Architecture overview

Status: current-architecture
Scope: the current explanatory model and rationale for `omnifs`. Binding rules live in `docs/contracts/`; this document explains how the pieces fit together.

`omnifs` projects external services such as GitHub, DNS, arXiv, Docker, Linear, and databases as a filesystem. A trusted host runtime loads each provider as a `wasm32-wasip2` component and drives it through the byte-level `omnifs:provider` WIT interface. Every filesystem exposes the same projected namespace.

## The spine

The load-bearing decision is where meaning lives.

The host knows paths, bytes, tree structure, content types, file attributes, cache metadata, capability outcomes, and effects. It does not request an object, parse a provider object, render a representation, or derive a path-to-object mapping from payload contents.

The provider SDK owns upstream-specific meaning: identity, canonical assembly, rendering into representations, versioning, preload, revalidation, and route topology. Where provider code needs the host to mutate state, it returns an explicit effect.

This keeps the host reusable across providers and filesystems. It also keeps provider compromise bounded by the authority the host resolved for that mount.

## Providers and objects

A provider is one `#[omnifs_sdk::provider]` implementation with synchronous `fn start` registering routes on a `Router`. `file` and `dir` are filesystem nouns; object, direct, blob, stream, collection, children, choices, and tree behavior are SDK faces that lower to byte-level WIT effects.

Object faces fit provider concepts with identity and replayable canonical bytes. `r.object::<O>(template, |o| ..)` and `r.file_object::<O>(template, |o| ..)` bind an `Object` to a path template. Canonical bytes are verbatim upstream bytes or a provider-assembled canonical blob. Derived and representation leaves decode the canonical through the object type and render from it.

Path-oriented routes, `r.dir`, `r.file`, and `r.treeref`, are correct when the domain is not object-shaped. Docker operational state, database browse surfaces, and subtree handoff do not need fake object identity merely to fit the object API.

Identity is layered. The provider computes a logical id from object kind and normalized identity captures. The host stores it in a mount-scoped keyspace, so two mounts with different credentials cannot share private canonical bytes for the same upstream identity.

## Callouts and effects

Provider namespace and notify calls are async component exports that return terminal results. When provider code awaits host work, it calls an async WIT import. The host executes callouts such as HTTP fetches, blob fetches, git clone/open operations, and blob reads, then the component future resumes with the typed result.

One provider instance can serve multiple concurrent filesystem operations. Wasmtime's component async runtime owns suspension while the host owns the callout executors, auth injection, capability checks, tracing, and cache-visible effects.

Terminal host mutations travel through effects and the operation's typed result:

- canonical stores select object identity and content-addressed body bytes.
- filesystem effects write materialized files, directories, attrs, and listing facts.
- invalidations remove object or listing state.

The operation owner validates and lowers the complete terminal into one projection transition, commits it once, and then exposes the typed result. Errors do not carry effects. New terminal host mutations should be new explicit effect fields, not tunneled through callouts.

## Caches and reads

The host owns storage as opaque facts and bytes.

- One global `BodyStore` stores every complete body by BLAKE3 identity.
- One projection keyspace stores object relations, typed lookup/attr/file/listing facts, blob request references, Git identities, and freshness for an exact spec/provider identity.
- Each projection has a derived process-local memory tier. Provider blob handles and Git tree handles never enter durable rows.

On a warm object read, the host pushes cached canonical bytes into the provider's read operation. The provider decodes and renders from those bytes. There is no provider-to-host canonical-read callout and no host-side render operation.

Online access uses freshness deadlines to decide when provider revalidation is needed. Cache-only access ignores those deadlines and serves complete durable facts. A missing body, partial listing, deferred/live/ranged value, or other provider-dependent fact returns `OfflineMiss`; a corrupt relation fails table construction.

## Dispatch and listing

Route dispatch must have one owner for precedence. Lookup, listing, read, and open all need the same route-target resolution model.

Listing honesty matters. A listing is exhaustive only when the provider actually enumerated every entry. A capped listing must stay non-exhaustive unless a real resume cursor exists. `lookup` can resolve a name that did not appear in a non-exhaustive `readdir`.

Literal route prefixes are auto-navigable directories. Capture validators participate in match candidacy, so a parse rejection can fall through to another candidate instead of becoming an accidental read-time error.

## File attributes

Projected files carry explicit size, stability, version, content type, and byte-source evidence. Stat-size and read-termination are separate: read termination must not depend on a guessed stat size.

Unknown and non-zero sizes use truthful sentinel behavior until exact size is learned from real reads. Learned-size publication belongs in shared tree/file-attr policy, not in FUSE or NFS local heuristics.

## Filesystems

FUSE and NFS are protocol adapters over the same projected tree. Each desired Attachment has a `ResourceName` and fully resolved `AttachmentSpec`; a separate process, container, or VM realizes it. Host filesystems use hidden `omnifs run-fs`; Docker and libkrun guests use the slim `omnifs-thin` binary. Both attach over the Omnifs VFS wire protocol.

FUSE owns inode tables, kernel notifications, mount/unmount mechanics, and FUSE reply construction. NFSv4.0 loopback owns filehandles, stateids, leases, NFS protocol errors, mount readiness, and teardown. Runner and NFS filehandle state live under the Attachment's daemon-owned runtime leaf.

Neither filesystem owns projection semantics, provider WIT calls, cache schema, root enumeration, learned-size rules, preload policy, inline-byte policy, or negative lookup policy.

A filesystem consumes the same `omnifs_vfs::Namespace` through the Omnifs VFS wire protocol. `omnifs-engine` remains the projection owner; `omnifs-vfs` owns the facade and postcard serialization, framing, the strict handshake, attach target resolution and reconnect, server-pushed stop, direct validated `Path` requests, terminal `OfflineMiss`, and ordered invalidation events. The fixed Unix and TCP endpoints serve this one internal protocol. The launcher supplies the Attachment name, exact spec, and runtime instance in every handshake; the daemon rejects conflicting session identity.

## Control plane

There is one `omnifs` binary. The runtime loop lives behind hidden `omnifs daemon`. The CLI owns setup, auth UX, resource authoring, metrics, and daemon spawn. The daemon owns desired resources, providers, credentials, mounts, SQLite state/cache, logs, Attachment runtime lifecycle, live VFS sessions, and namespace serving. The typed local RPC wire types live in `omnifs-api`; the CLI has no direct daemon-store API.

The active profile root comes from `OMNIFS_HOME` or `$HOME/.omnifs`. Daemon state under `daemon-state/` contains `control-store/state.sqlite3`, provider artifacts, desired resources, durable Attachment instances and actions, runtime records, caches, and raw logs. The complete desired set commits through `ApplyResources` in one transaction and reconciles after the reply. Old client filesystem specs remain read-only migration data. There is no client-side resource desired state, snapshot handoff, or offline mode.

The daemon process itself is host-native. `AttachmentSupervisor` starts host, Docker, and libkrun filesystem runtimes out of process and owns their exact lifecycle. On Apple Silicon macOS, the daemon starts the private sibling `omnifs-libkrun`, which loads the signed packaged dylib and firmware and exposes one fixed VM shape. VFS wire v11 separates configured Attachments from live sessions and fences each session with the exact Attachment spec and runtime instance across reconnect and daemon replacement.

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
- NFS filesystem rationale: `docs/architecture/50-nfs-filesystem.md`
- Async provider runtime: `docs/architecture/60-async-provider-runtime.md`
- Provider authoring: `providers/DESIGN.md` and `skills/omnifs-provider-sdk/SKILL.md`
