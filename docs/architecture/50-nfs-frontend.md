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

It does not own projection semantics. It must not decide provider route precedence, cache schema, learned-size authority, mount enumeration at the root, or negative lookup policy.

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

NFS loopback mount startup and teardown are frontend delivery concerns. The CLI launches `omnifs-thin nfs` as a host runner; the daemon serves the projected tree over the fixed local socket and reports readiness through the control plane. The runner attaches and serves until unmount.

Do not describe macFUSE, `diskutil`, or macOS FUSE mounting as current behavior. macOS host-native integration is NFSv4 loopback.

## Client-behavior quirks and the product contract

FUSE gives the daemon per-operation control over what the kernel believes. An NFS mount interposes the OS NFS client, which has its own caching, retry, and recovery behavior. This section catalogs the known gaps between NFS-loopback behavior and the product contract, and how each is handled today. The mount options in `omnifs-nfs/src/mount.rs` are the enforcement point for the mitigated rows; every option there carries its rationale in code.

Mitigated by mount options today:

- **Attribute and lookup staleness.** The client caches attrs and lookups on its own schedule, which fights live and growing projected files. Mitigation: `noac` plus `nonegnamecache` (macOS), `actimeo=0` plus `lookupcache=none` (Linux). Cost: every stat and lookup round-trips to the loopback server. Revisit when tree invalidation can drive cache validity instead of disabling caching wholesale.
- **Hangs against a dead server.** A default NFS mount blocks processes indefinitely when the server dies. Mitigation: `soft`, `timeo=5`, `retrans=1` (Linux) and `intr`, `timeo=5`, `retrans=1`, `retrycnt=0` (macOS) bound the wait; teardown force-unmounts and sweeps state files for daemon crashes.
- **Delegation and callback complexity.** `nocallback` (macOS) disables delegations, so no callback channel or recall handling exists to get wrong.

- **Host metadata traversal.** macOS Spotlight can recursively walk a mounted
  NFS namespace and retain directory references after the serving process dies,
  which makes a dead loopback mount harder to tear down. The macOS mount uses
  `nobrowse`, the export serves a synthetic lookup-only
  `/.metadata_never_index` marker, and the runner asks `mdutil` to disable
  indexing and searching. The marker is frontend-owned and omitted from
  provider listings; the `mdutil` command is best effort because macOS can
  report a disabled NFS volume as having no manageable metadata store.

Deferred by the read-only contract (these arrive with any write path and must be designed for, not discovered):

- **Silly rename.** Open-unlink becomes a client-issued rename to `.nfsXXXX`, visible in listings until last close. A write-capable frontend must decide whether the server hides these names from readdir.
- **AppleDouble spray.** With no xattr support in NFSv4.0, the macOS client materializes `._*` companion files on writes carrying Finder metadata, quarantine flags, and resource forks. These would pollute the projected tree and confuse providers.
- **Write-back and mmap coherence.** Client-side write caching weakens read-after-write visibility across processes; mmap-heavy editors are best effort per the product contract.
- **Locking.** NFSv4.0 mandatory lock state (and its recovery) is protocol machinery the read-only frontend deliberately keeps narrow.

Structurally addressed for the restartable out-of-process frontend:

- **Size pinned across OPEN.** The macOS client latches the file size it held before OPEN for the lifetime of that open, and serves that pinned value even to `fstat` on the open fd, so a cold unknown-length file's reads would stay clamped to the 1-byte size-unknown sentinel. `Export::lookup` (`crates/omnifs-nfs/src/adapter.rs`) probes with a one-byte read whenever `getattr_exact` still reports the sentinel, so the exact size is learned and cached before the client's pre-OPEN attrs are captured; `Export::open_state` repeats the same probe as a backstop for opens that arrive without a fresh lookup. Proven by the engine's `one_byte_probe_read_learns_size_for_next_getattr` (`crates/omnifs-engine/tests/namespace_surface.rs`) plus the adapter's `lookup_probes_unknown_size_file_and_learns_exact_size` (`crates/omnifs-nfs/src/adapter.rs`).
- **Path identity across restarts.** A kernel client holds filehandles across a frontend or daemon restart. The shipped `omnifs-thin nfs` runner persists validated namespace `Path` values with its protocol-local parent/name/scope and inode identity, so a fresh engine can recursively rediscover provider anchors and host descendants without a daemon-local identity table. A daemon disconnect publishes the existing root `NsEvent::InvalidateSubtree`; NFS drains it inline, clears derived sizes and advances `PendingListings`, while preserving path-backed inodes, opens, stateids, leases, clients, and the filehandle generation. The runner also recovers the NFS server address recorded for an active mount and serves the export without remounting. An unknown stateid still returns the protocol error that drives the client's transparent re-open against a still-valid filehandle. FUSE and tests use the same direct `Path` namespace identity, with process-local protocol tables. Proven by the persisted-path and root-invalidation acceptance fixtures.

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
