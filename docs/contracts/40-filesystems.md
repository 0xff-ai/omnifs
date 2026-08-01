# Filesystem instance contracts

Status: current-contract
Owns: FUSE and NFS adapter boundaries, protocol state, mounts, and
filesystem-specific validation.

## Read when

Read this before changing `omnifs-thin`, FUSE, NFS, `omnifs-mtab`, startup,
protocol replies or state, mounts, kernel notifications, or live mount tests.

## Rules

### Adapter boundary

Filesystems translate the narrow `omnifs_engine::namespace` surface into
protocol state; they never touch internal tree or view modules. Size, TTL,
change counter, direct I/O, and read style cross as plain data. Inodes,
filehandles, stateids, leases, notifications, replies, and error mapping stay
in filesystem crates and convert once at the edge.

### Compatibility target

One shared projected tree must behave like ordinary read-only files for every
consumer, not one favored access pattern. Changes to namespace or protocol
behavior must preserve:

- reads and tails through `cat`, `head`, `tail`, `less`, `xxd`, `hexdump`,
  `od`, and `file`;
- traversal through `grep -r`, `rg`, `find`, and `fd`;
- metadata through `ls`, `du`, `wc`, and `stat`;
- copy, archive, compare, and hashing through `cp`, `tar`, `rsync`, `diff`,
  `cmp`, and checksum tools;
- structured inspection through `jq`, `yq`, and `xmllint`.

Memory-mapped editors remain best effort. Do not add shell-, editor-, or
agent-specific namespace behavior.

### Filesystem registry

The daemon gives one shared `TreeNamespace` to `VfsServer`, which always binds
one mode-0600 Unix endpoint and one unauthenticated TCP endpoint on loopback or
the verified Docker bridge. It owns both tasks; bind failure or later listener
exit is fatal.

`VfsSession` is observed transport state keyed by `ResourceName`, not
configuration. Reconnect overlap requires matching Filesystem name, full spec,
and runtime instance; conflicting reuse is rejected.

### FUSE

FUSE is the Linux host and guest protocol. Host FUSE uses hidden
`omnifs run-fs`; Docker and libkrun use `omnifs-thin` with the same flat
arguments.

Docker FUSE stays in the container mount namespace. Killing the exact container
removes it; restart creates a fresh runtime for the desired Filesystem.

Keep FUSE inodes, notifications, mount mechanics, and replies in `omnifs-fuse`;
shared projection behavior stays in the engine tree.

### Filesystem runners

Every filesystem has a separate process, container, or VM. Host filesystems use
hidden `omnifs run-fs`; guests use `omnifs-thin`, which has no engine, Wasmtime,
provider bundle, or control plane. Both call the same `omnifs_thin::run`
library entrypoint.
`omnifs-vfs` owns framing, the strict v11 handshake, reconnect, stop, direct
`Path` requests, and ordered invalidations.

`OfflineMiss` is a terminal daemon-lifetime cache-only miss, distinct from
`NotFound` and retryable upstream errors. FUSE maps it to `EIO`; NFS to
`NFS4ERR_IO`.

Disconnect or broadcast lag becomes a root reset on the invalidation stream.
FUSE has one event owner and settles namespace operations before protocol
publication. NFS preserves path-backed protocol identity across reset while
clearing derived sizes, reply cache, and listings. Local generations prevent
late listing or cache completion from repopulating invalidated state.

Filesystem identity is `ResourceName` plus the daemon-resolved
`FilesystemSpec`: protocol, runtime, location, and assets. Every launcher gets
that identity through named arguments; transport never infers it.

Host locations are absolute; Docker and libkrun use `/omnifs`. Host and libkrun
records include Filesystem identity plus a random process instance. Docker
labels name the Filesystem, while command inspection proves its full spec.

`FilesystemSupervisor` serializes lifecycle by name and publishes strict
runtime records and instance-specific mode-0600 controls. Launch, deletion, and
Doctor require identity proof; normal teardown never signals a PID from disk.

Runtime records are recovery evidence, not desired truth. Changing the stored
`FilesystemSpec` shape changes durable identity and the VFS handshake contract,
so it requires explicit storage and protocol review.

Support and default selection are separate facts. A host, protocol, and runtime
tuple is supported only when its launcher, runner, and conformance entry exist.
CLI policy must not reject a supported non-default tuple. Changes to this matrix
must test the exact affected tuple before the broad platform gates.

An exact stop installs the VFS reconnect fence before touching the runtime,
even when no session is currently live. The supervisor first requests graceful
session teardown, then stops and proves the exact runtime absent. Only after
that proof may the server close a busy or stale connection. It waits for the
exact session to disappear before releasing the fence. Never clear durable
runtime identity or a deletion tombstone before both runtime and session
absence are proved.

### Runtime driver ownership

Lifecycle drivers probe and stop only exact runtime identity. The live daemon's
Doctor alone searches runtime roots and offers a destructive repair only when
both desired and observed Filesystem states are absent at diagnosis. Apply
rechecks both states before the effect, and the runtime driver reconfirms exact
identity immediately before it acts.

The private `crates/omnifs-daemon/src/fs_runtime` module owns runtime mechanics,
not desired state; `crates/omnifs-daemon/src/doctor.rs` owns diagnosis and
opaque repair offers. The daemon supplies the Filesystem, daemon paths,
endpoints, and event sink. Runtime events are closed factual variants with
non-blocking delivery. One keyed task owns each Filesystem; no lock spans a
runtime await. The runtime set stays closed until a real fourth implementation
exists.

Libkrun uses the same thin binary and VFS protocol as Docker, replacing TCP with
one explicit virtio-vsock device. Three fixed ports carry guest-initiated
attach, guest readiness, and host-initiated keyed SSH. A mode-0600 bridge maps
attach to the daemon Unix socket and closes both legs on daemon replacement so
the guest reconnects. There is no network device or egress. Guest FUSE is
guest-only; host-visible macOS remains NFS loopback.

Private sibling `omnifs-libkrun` alone owns libkrun. It loads absolute packaged
library and firmware paths and accepts one strict fixed-VM configuration: two
raw disks, 2 vCPUs, 2048 MiB, serial, three vsock ports, no GPU or network. It
never searches `PATH`, invokes another launcher, or exposes generic policy or
REST. Detached teardown requires identity-matched Ping and Shutdown; only
rollback may kill its directly owned unreaped child.

Each launch copies the immutable guest image to mode-0600
`<profile>/daemon-state/runtime/filesystems/<name>/libkrun/root.raw`; libkrun
never mutates the base. Rollback, replacement, restart, and deletion remove
launch copies but preserve the base and SSH key.

Every guest runner owns a fail-closed lockdown check. Docker allows no binds and
only `OMNIFS_ATTACH_ADDR` plus image-default env; flat arguments carry identity.
Libkrun audits the exact seed keys and proves loopback-only networking with no
`tsi_hijack` argument in live conformance. Either runner fails before launch
success on any violation.

Libkrun SSH uses a persistent per-Filesystem ed25519 key. The seed carries only
the public key; the guest enables its SSH socket only when that key is present.

### NFSv4 loopback

macOS host integration is read-only NFSv4.0 loopback, a filesystem boundary,
not a provider protocol.

Startup excludes the macOS mount from Spotlight with `nobrowse`, a synthetic
lookup-only `/.metadata_never_index`, and best-effort `mdutil`. The marker never
enters provider listings. A nonzero `mdutil` result is accepted only when macOS
reports indexing and search disabled. This is mount policy, not namespace or
provider behavior.

Keep filehandles, stateids, leases, and NFS errors in `omnifs-nfs`. Mutation
operations remain read-only; macOS readiness and teardown stay in NFS/CLI code.

The shared NFS entrypoint attaches through VFS; host delivery uses hidden
`omnifs run-fs --protocol nfs`. Runner records and persistent filehandles live
under `daemon-state/runtime/filesystems/<name>`. Restart reuses the recorded
server address rather than binding a new port without remounting. Corrupt
leaves degrade individually.

### Mount-table mechanics

`omnifs-mtab` owns `/proc/mounts`, NFS state-file I/O, and platform unmount
commands. Filesystem and lifecycle code add no duplicate parser, schema, or argv
builder.

Per-Filesystem mtab files hold discovery and teardown state. Mount records carry
mount point, address, and PID; host records add Filesystem identity, process
group, and control socket. The colocated NFS filehandle table is protocol
identity owned by `omnifs-nfs`. Records degrade independently.

### NFS deferral and `NFS4ERR_DELAY`

NFS uses `NFS4ERR_DELAY` in two distinct ways:

| Mode | Trigger | Work after reply | Owner |
|---|---|---|---|
| Reactive | Namespace `RateLimited`, `Timeout`, or `Network` | None; retry starts fresh | `Status::from(&NsError)` |
| Proactive | Provider-backed `READDIR` exceeds `NFS_INLINE_BUDGET` | `PendingListings` completes and warms authoritative dirents | `omnifs-nfs::delayed` |

Cold `LOOKUP` has no proactive deferral because it lacks the same
cache-convergence guarantee. Each RPC has its own handler thread and XID, so a
slow call does not block other calls on the connection.

The engine owns truth and cache, not delay budgets. `PendingListings` owns
path-keyed slots, completion, generation, and wait budget. OAuth refresh
single-flight remains in `omnifs-auth`. FUSE has no `DELAY` equivalent.

## Must not

- Call provider WIT directly from a filesystem.
- Construct fake provider DTOs to reuse filesystem code paths.
- Own root enumeration, learned size, inline reads, preload, or negative lookup
  policy.
- Put provider policy or cache schema knowledge in FUSE or NFS.
- Add macOS FUSE behavior or restore macFUSE or `diskutil` mounting.
- Treat container FUSE as the daemon architecture; it is only an
  out-of-process filesystem runtime.
- Remove live NFS test serialization casually.
- Claim NFS gives FUSE-equivalent permission isolation.
- Put wait budgets or `DELAY` policy in `omnifs-engine`.
- Assume every `NFS4ERR_DELAY` implies background continuation past the reply.

## Code

- `crates/omnifs-fuse/src`
- `crates/omnifs-nfs/src`
- `crates/omnifs-mtab/src`
- `crates/omnifs-engine/src/namespace` (the surface filesystems consume)
- `crates/omnifs-engine/src/tree`
- `crates/omnifs-vfs/src/server.rs` (`VfsServer`)
- `crates/omnifs-libkrun/src`
- `crates/omnifs-daemon/src/filesystem_supervisor.rs`
- `crates/omnifs-daemon/src/fs_runtime`
- `crates/omnifs-daemon/src/doctor.rs`
- `crates/omnifs-thin/src/host_control.rs` and `lifecycle.rs`
- `crates/omnifs-cli/tests/lifecycle_acceptance.rs`

## Validation

- Filesystem changes need protocol tests plus shared tree tests for semantics.
- FUSE-visible behavior changes need targeted FUSE tests and live runtime checks.
- NFS mechanics need protocol/unit tests; host behavior needs live mounts.
- Libkrun changes need local-only `just libkrun-conformance`; it never runs in
  hosted CI.
