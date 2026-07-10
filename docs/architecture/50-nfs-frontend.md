# NFS frontend

Status: current-architecture
Scope: why the macOS frontend is NFSv4 loopback, what it owns, and what must stay shared with the projection tree. Binding rules live in `docs/contracts/40-frontends.md`.

macOS host-native integration uses a read-only NFSv4.0 loopback frontend. It is a protocol adapter over the same projected tree as FUSE, not a separate product mode and not a provider-facing architecture.

## Boundary

The NFS frontend owns NFS protocol state:

- filehandles
- stateids
- open and read sequencing
- leases
- NFS status mapping
- mount readiness and teardown
- macOS loopback mount options

It does not own projection semantics. It must not decide provider route precedence, cache schema, learned-size authority, root mount enumeration, or negative lookup policy.

## Filehandles

Filehandles are frontend-owned stable identifiers for protocol clients. They should identify projected nodes through tree-owned handles or stable frontend mappings, not by re-deriving provider meaning.

The frontend can keep protocol-local tables as long as they are adapters over tree answers.

## Stateids and leases

NFS open state is a protocol requirement. Stateids and leases guard client protocol sequencing; they are not provider locks and do not imply upstream mutation authority.

Because the current NFS frontend is read-only, write-state handling should remain explicit and narrow. Do not grow write semantics through accidental support for projected file writes.

## Attributes

NFS attributes must express the same file facts as FUSE: type, size, stability-derived freshness, and learned-size behavior. When NFS needs different protocol encoding, keep the semantic decision in tree policy and only translate at the NFS edge.

Unknown and non-zero sizes follow the shared file-attribute contract. NFS must not invent separate placeholder-size behavior.

## Cache and invalidation

Frontend caches are protocol caches. Shared cache records and projection policy belong to host/tree code.

If NFS needs local handle or attr caches, invalidation must flow from tree or host invalidation events. Do not add provider-specific cache invalidation inside NFS.

## Mount lifecycle

NFS loopback mount startup and teardown are frontend delivery concerns. The daemon starts the frontend, serves the projected tree, and reports readiness through the control plane.

Do not describe macFUSE, `diskutil`, or macOS FUSE mounting as current behavior. macOS host-native integration is NFSv4 loopback.

## Client-behavior quirks and the product contract

FUSE gives the daemon per-operation control over what the kernel believes. An NFS mount interposes the OS NFS client, which has its own caching, retry, and recovery behavior. This section catalogs the known gaps between NFS-loopback behavior and the product contract, and how each is handled today. The mount options in `omnifs-nfs/src/mount.rs` are the enforcement point for the mitigated rows; every option there carries its rationale in code.

Mitigated by mount options today:

- **Attribute and lookup staleness.** The client caches attrs and lookups on its own schedule, which fights live and growing projected files. Mitigation: `noac` plus `nonegnamecache` (macOS), `actimeo=0` plus `lookupcache=none` (Linux). Cost: every stat and lookup round-trips to the loopback server. Revisit when tree invalidation can drive cache validity instead of disabling caching wholesale.
- **Hangs against a dead server.** A default NFS mount blocks processes indefinitely when the server dies. Mitigation: `soft`, `timeo=5`, `retrans=1` (Linux) and `intr`, `timeo=5`, `retrans=1`, `retrycnt=0` (macOS) bound the wait; teardown force-unmounts and sweeps state files for daemon crashes.
- **Delegation and callback complexity.** `nocallback` (macOS) disables delegations, so no callback channel or recall handling exists to get wrong.

Deferred by the read-only contract (these arrive with any write path and must be designed for, not discovered):

- **Silly rename.** Open-unlink becomes a client-issued rename to `.nfsXXXX`, visible in listings until last close. A write-capable frontend must decide whether the server hides these names from readdir.
- **AppleDouble spray.** With no xattr support in NFSv4.0, the macOS client materializes `._*` companion files on writes carrying Finder metadata, quarantine flags, and resource forks. These would pollute the projected tree and confuse providers.
- **Write-back and mmap coherence.** Client-side write caching weakens read-after-write visibility across processes; mmap-heavy editors are best effort per the product contract.
- **Locking.** NFSv4.0 mandatory lock state (and its recovery) is protocol machinery the read-only frontend deliberately keeps narrow.

Structurally addressed for the restartable out-of-process frontend (today's caveat: see below):

- **ESTALE across restarts.** A kernel client holds filehandles across a frontend or daemon restart. A restartable out-of-process runner keeps them valid two ways. A frontend restart reloads a persisted filehandle-identity table (`crates/omnifs-nfs/src/persist.rs`, one file in the NFS state dir): the same `generation` so old filehandles keep decoding, the resumed `next_ino`, and per-id `{ scope, parent, name, kind }` that re-resolve lazily by walking the parent chain through `namespace.lookup` (no `NodeId` is persisted; ids are meaningless across processes). The runner also pins the NFS server port and serves the export without remounting when the mount is already active. A daemon restart under a live frontend arrives as a wire reattach (`NsAttachEvent::Reattached`): the adapter drops every cached `NodeId` and re-resolves the surviving identity chains lazily, without closing opens or bumping the filehandle generation. Stateids are not persisted; an unknown stateid returns the protocol error that drives the client's transparent re-open against a still-valid filehandle. The write policy is a debounced write-behind plus a synchronous flush on clean shutdown, so a handle allocated within the debounce window immediately before a `SIGKILL` can be lost (`NFS4ERR_STALE` for that handle only); a handle a client actually holds is durable, because it holds one only after observing the op complete. The in-process daemon frontend does not persist (its mount dies with the daemon). This mechanism (the persisted table, the reattach adapter) is unchanged code, but its live proof depended on the phase-3 `omnifs frontend run --kind nfs` out-of-process test double, which was retired when the runner became the FUSE-only `omnifs-fuse` binary (see `docs/contracts/50-control-plane.md`); the two-leg `wire_reattach` live acceptance test that proved it no longer has a runner to spawn. NFS was never delivered out-of-process in production (`omnifs frontend up` only ever launches the FUSE kind), so no shipped behavior regressed, but the live proof of this catalog row is currently a gap pending a decision on whether NFS keeps an out-of-process test double.

Open, inherent to the transport:

- **Sleep/wake and lease churn.** The v4.0 lease/grace machinery interacts with laptop sleep; live tests serialize for related reasons.
- **No xattr surface at all** (until v4.2 is on the table), independent of writes: `xattr -l` answers differently than FUSE would.

When comparing frontends or debating defaults, this catalog is the checklist: a quirk is either mitigated (name the mount option), deferred (name the contract gate), structurally addressed (name the mechanism and its proof), or open (name the consequence). The conformance target for both frontends is the same product-contract toolbox; NFS earns default status per platform by passing it, not by assertion.

## Rejected shapes

- NFS-specific projection semantics
- provider-specific behavior in NFS protocol handlers
- frontend-owned cache schema
- macFUSE or macOS FUSE as the current integration path
- write behavior hidden behind ordinary projected file writes
