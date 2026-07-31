# Architecture overview

Status: current-architecture
Scope: the current explanatory model and rationale for `omnifs`.

Read when: orienting to the whole system or deciding which focused
architecture note and binding contract owns a change.

Binding contracts: start with `docs/contracts/00-index.md`.

`omnifs` is a filesystem projection system. It projects external services into
one shared virtual namespace. A trusted host loads untrusted providers as
`wasm32-wasip2` components; FUSE and NFS expose the same namespace to the OS as
one or more filesystems.

## Ownership

| Owner | Owns | Does not own |
|---|---|---|
| Provider and SDK | Upstream meaning, object identity, canonical assembly, rendering, versioning, preload, revalidation, and routes | Storage, credentials, host I/O, or OS protocol state |
| `omnifs-engine` | Projection semantics, cache facts, attrs, lookup, listing, reads, and provider execution | Provider-specific object meaning or OS protocol state |
| `omnifs-vfs` | Namespace facade, framing, handshake, reconnect, stop, and invalidation delivery | Projection policy |
| FUSE and NFS | Inodes or filehandles, protocol state, replies, mounts, and teardown | Provider calls, cache schema, or shared tree policy |
| Daemon | Desired state, SQLite, credentials, provider preparation, reconciliation, runtimes, and live VFS sessions | Client prompts or provider meaning |
| CLI | Setup, auth UX, resource authoring, output, metrics, and daemon spawn | Desired-state storage or runtime lifecycle |

The key decision is that providers own meaning while the host owns trust and
effects. The host sees paths, bytes, attributes, cache facts, capabilities, and
effects. It never parses canonical provider objects or renders their
representations.

## Namespace flow

1. FUSE or NFS sends a validated path request through the VFS wire protocol.
2. `TreeNamespace` applies shared dispatch, lookup, listing, attr, and read
   policy.
3. A durable fact may answer directly. Online access also applies freshness;
   cache-only access returns complete facts and known entries from partial
   listings. Provider-dependent misses return `OfflineMiss`.
4. Provider execution may await host HTTP, blob-fetch, or Git callouts.
   Wasmtime suspends the component while the host enforces grants, injects
   credentials, performs I/O, and records tracing.
5. A successful provider terminal contains a typed result and explicit effects.
   The engine validates both, commits one projection transition, updates memory,
   emits invalidations, then exposes the result. Errors carry no effects.
6. The filesystem adapter translates the namespace answer into FUSE or NFS
   protocol state.

Warm object reads push cached canonical bytes into the provider, which decodes
and renders them. There is no host-side render or canonical-read callout.

Lookup, listing, read, and open share one route-precedence model. A listing is
exhaustive only when the provider or a closed literal route shape proves the
child set complete. Absence from a partial listing is not `NotFound`.

File facts keep size, stability, version evidence, content type, and byte
source together. Unknown lengths use a one-byte stat compatibility hint until
a complete observation proves the exact size; the hint never bounds reads.

## Resource flow

1. The CLI reads the current revision, plans a complete Provider, Credential,
   Mount, and Filesystem set, and calls `ApplyResources`.
2. The daemon validates and commits that set plus its receipt in one SQLite
   transaction, then wakes reconcilers and returns.
3. Daemon workers prepare providers, publish the latest valid generation,
   process credential actions, and reconcile Filesystems. A failed build leaves
   the last good generation active.
4. `FilesystemSupervisor` realizes each desired Filesystem as an exact
   out-of-process runtime and waits for its identity-matched VFS session.

Progress streams observe durable work but do not own or cancel it. SQLite is
the only desired-state authority; the CLI keeps no journal or fallback reader.

## Runtime variants

| Runtime | Filesystem process | Attach transport | Visible mount |
|---|---|---|---|
| Host | Hidden `omnifs run-fs` | Unix VFS | Host |
| Docker | `omnifs-thin` in a container | Verified bridge TCP VFS | Container |
| libkrun | `omnifs-thin` in a fixed microVM | Vsock bridge to Unix VFS | Guest |

The daemon itself always runs on the host. macOS host-native integration is
read-only NFSv4.0 loopback; libkrun FUSE remains guest-visible.

## Trust

Provider metadata declares auth and capability needs. The resolved mount spec
is the runtime grant authority. Credentials stay in daemon-owned state and are
injected only into allowed host callouts. Filesystem runners receive no
credentials.

The sandbox limits authority but cannot prevent all exfiltration through an
allowed destination. Over-grant detection is not yet enforced.

## Rejected directions

Rejected designs include host-side object meaning or rendering, provider-owned
caches, fake cursors or exhaustive claims, provider-specific filesystem policy,
macFUSE as the macOS path, a second public daemon binary, and projected writes
that trigger upstream mutations.

## Focused notes

| Topic | Read |
|---|---|
| Binding rules | `docs/contracts/00-index.md` |
| File attributes | `docs/architecture/10-file-attributes.md` |
| Dispatch and listing | `docs/architecture/20-route-dispatch-and-listing.md` |
| Cache and effects | `docs/architecture/30-cache-and-effects.md` |
| Auth | `docs/architecture/40-auth-boundary.md` |
| NFS | `docs/architecture/50-nfs-filesystem.md` |
| Async runtime | `docs/architecture/60-async-provider-runtime.md` |
| Resource control | `docs/architecture/70-resource-control-plane.md` |
| Provider authoring | `providers/DESIGN.md`, `skills/omnifs-provider-sdk/SKILL.md` |
