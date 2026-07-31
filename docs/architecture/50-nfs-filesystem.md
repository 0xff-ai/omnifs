# NFS filesystem architecture

Status: current-architecture
Scope: why the macOS filesystem is NFSv4 loopback, what it owns, and what stays
shared with the projection tree.

Read when: changing NFS protocol state, mount options, filehandles, attrs,
lookup caching, mount lifecycle, restart behavior, or macOS client workarounds.

Binding contracts: `docs/contracts/40-filesystems.md` and
`docs/contracts/50-control-plane.md`.

macOS host-native integration uses read-only NFSv4.0 loopback over the same
projected tree as FUSE.

## Ownership

| Concern | NFS owns | Shared owner |
|---|---|---|
| Identity | Stable filehandles and protocol-local mappings | Tree handles and path meaning |
| Open state | Stateids, leases, sequencing | No provider locks or mutation authority |
| Attributes | NFS encoding | Tree size, stability, freshness, and learned-size policy |
| Cache | Bounded process-local attr and lookup answers | Engine facts, lifetimes, and invalidation |
| Lifecycle | Mount readiness, options, serving, unmount | Daemon desired state and runtime supervision |
| Errors | NFS status mapping | Namespace error meaning |

NFS never owns route precedence, cache schema, root enumeration, learned-size
authority, or negative lookup policy. Its current write-state surface stays
explicit and narrow.

## Cache

Mount options disable kernel attr and lookup caches, so NFS keeps one bounded
process-local cache of plain engine answers. The engine sets positive and
negative lifetimes; listings may seed positive entries; namespace invalidation
evicts them. Zero-TTL answers cross the wire. The cache is neither durable nor
aware of projection schema.

## Mount lifecycle

`FilesystemSupervisor` launches hidden `omnifs run-fs` with the resolved
Filesystem identity and attach endpoint. The process owns preflight, attach,
startup cancellation, serving, and unmount; its instance-bound control socket
handles daemon stop and exact Doctor repair.

macFUSE, `diskutil`, and macOS FUSE are not current behavior.

## Client behavior

The OS NFS client adds caching, retry, and recovery behavior. Each known gap has
one status:

| Status | Gap | Current handling |
|---|---|---|
| Mitigated | Attr and lookup staleness | macOS `noac,nonegnamecache`; Linux `actimeo=0,lookupcache=none`; NFS-local cache absorbs engine-leased repeats |
| Mitigated | Dead-server hangs | Bounded soft retry options; teardown force-unmounts and sweeps crash state |
| Mitigated | Delegation callbacks | macOS `nocallback` disables delegations |
| Mitigated | Spotlight traversal | `nobrowse`, lookup-only `/.metadata_never_index`, and best-effort `mdutil`; marker stays out of provider listings |
| Deferred by read-only | `.nfsXXXX` silly rename | A write design needs an explicit listing-visibility rule |
| Deferred by read-only | AppleDouble `._*` files | A write design needs a guard against provider-tree pollution |
| Deferred by read-only | Write-back, mmap, locking | Design coherence and NFSv4.0 lock recovery before writes |
| Addressed | Size pinned across `OPEN` | `Export::lookup` probes one byte before pre-open attrs; `open_state` repeats as a backstop |
| Addressed | Filehandles across restart | Persist validated `Path` plus protocol identity and server address; root reset clears derived state without remounting |
| Open | Sleep/wake lease churn | Live tests serialize |
| Open | No NFSv4.0 xattrs | `xattr` differs from FUSE until a later protocol version |

Size probing is proved by
`one_byte_probe_read_learns_size_for_next_getattr` and
`lookup_probes_unknown_size_file_and_learns_exact_size`. Mount options and
their rationale live in `omnifs-nfs/src/mount.rs`.

Use this status model when changing defaults: name the option for a mitigation,
the contract gate for a deferral, the mechanism and proof for an addressed gap,
or the consequence for an open gap.

## Rejected shapes

- NFS-specific projection semantics
- provider-specific behavior in NFS protocol handlers
- filesystem-owned cache schema
- macFUSE or macOS FUSE as the current integration path
- write behavior hidden behind ordinary projected file writes
