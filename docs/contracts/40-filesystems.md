# Filesystem instance contracts

Status: current-contract
Owns: FUSE and NFS filesystem adapter boundaries, protocol state, mount behavior, and filesystem-specific validation.

## Read when

Read this before touching `omnifs-thin`, the `omnifs-fuse` or `omnifs-nfs` protocol crates, `omnifs-mtab`, filesystem startup, protocol replies, filehandles, stateids, inode tables, kernel notifications, NFS leases, macOS mount readiness, or live mount tests.

## Rules

### Adapter boundary

Filesystem crates translate namespace answers into protocol state. They do not decide projection semantics.

A filesystem consumes the narrow `omnifs_engine::namespace` surface (`Namespace`, validated `Path`, `Attrs`, `DirPage`, `ReadAnswer`, `NsEvent`, and friends) and nothing else of the engine. It never touches internal tree or view modules directly: the already-policied protocol answer (size, TTL, change counter, direct-I/O, read style) crosses the `Namespace` boundary as plain data. Keep inode numbers, filehandles, stateids, leases, notifications, reply construction, and protocol-specific error mapping in filesystem crates. Convert namespace types into protocol replies once at the filesystem boundary.

### Filesystem registry

The daemon constructs one `TreeNamespace` over the shared mount registry and gives it to `omnifs_vfs::VfsServer`. `VfsServer` binds one fixed Unix endpoint and one fixed TCP endpoint on every start, owns their tasks, and records the exact `omnifs_core::fs::Spec` supplied in each handshake. The Unix endpoint is mode `0600`; TCP binds only loopback or the verified Docker bridge address and has no auth. Any listener exit after readiness is fatal.

The attachment registry keys logical filesystems by `fs::Id`. Reconnect overlap may add another connection only when every resolved spec field matches. A connection that reuses an ID with a different protocol, runtime, or location is rejected.

### FUSE

FUSE is the Linux host and guest filesystem protocol. Host FUSE runs through hidden `omnifs run-fs --protocol fuse --runtime host`; Docker and libkrun run the slim `omnifs-thin` binary with the same flat named arguments.

The Docker-hosted FUSE mount lives entirely inside the container's own mount namespace. Killing the exact container removes that mount with it, and `omnifs fs restart --name <id>` creates a fresh container for the same persisted spec.

Keep FUSE inode tables, kernel notifications, mount/unmount mechanics, and FUSE reply types in `omnifs-fuse`. Keep shared projection behavior in `omnifs-engine/src/tree`.

### Filesystem runners

Every attached filesystem has a separate process, container, or VM. Host filesystems run through the full `omnifs` binary's hidden `run-fs` command. Guest filesystems use `omnifs-thin`, which contains no engine runtime, Wasmtime runtime, provider bundle, or daemon control plane. Both call the same `omnifs-thin` library entrypoint. `omnifs-vfs` owns serialization, framing, the strict protocol-v9 handshake, attach and reconnect, server-pushed stop, direct `Path` requests, and ordered invalidation events.

`NsError::OfflineMiss` is a terminal daemon-lifetime cache-only miss, distinct from `NotFound` and from retryable upstream errors. FUSE maps it to `EIO`; NFS maps it to `NFS4ERR_IO`.

Disconnects and broadcast lag are represented by a root reset on the same event stream as ordinary subtree invalidations. FUSE keeps one background event owner and settles each namespace operation before publishing protocol state. NFS preserves path-backed filehandles, opens, stateids, leases, and clients across root refresh while resetting derived sizes, its bounded protocol reply cache, and its listing state. `PendingListings` and the reply cache advance local generations on every subtree invalidation, so a late completion cannot populate fresh state.

The public identity is the persisted, fully resolved `fs::Spec`: ID, protocol, runtime, and location. Every launcher supplies all four through named flags. Transport never infers identity. `omnifs up` and `apply` replace only the daemon so runners reconnect. Explicit `down` asks the daemon to stop and drain attached filesystems before it exits, while preserving every spec.

Host locations are absolute and default to an ID-bearing workspace path. Docker and libkrun always use `/omnifs`. Host and libkrun runtime records and controls include the full resolved spec plus a separate random process instance ID. Docker labels carry the ID, while exact inspection verifies its full flat command against the stored spec.

Host lifecycle takes an exclusive per-ID claim, publishes strict `runner.json`, and serves an instance-specific mode-0600 control socket. Launch confirmation, detach, and doctor require an identity-matched `Ping`; normal teardown sends `Stop` and never signals a PID read from disk. Mount startup cancellation does not exit until the mount operation has joined and the mount is absent. Status joins persisted specs with daemon attachment rows. Commands that act on a runtime probe only the runtime named by the spec; unreported strays and stale records belong to doctor.

### Filesystem runtime and runner ownership

`omnifs fs create` resolves platform defaults once and atomically writes one strict spec under `$OMNIFS_HOME/filesystems/specs`. It never launches a runtime. Duplicate IDs fail until `omnifs fs rm --name <id>` removes a proven-detached spec.

`attach`, `detach`, `restart`, `rm`, and `shell` select only by `--name` and hold the per-ID claim. Attach is not idempotent: it rejects an attached or confirmed running instance. Every runtime launch has the same success postcondition: the OS mount completed and the exact spec appears in daemon attachments. Detach succeeds only after the mount is absent and the owned process, container, or VM has exited. Remove never detaches implicitly and refuses attached, running, or uncertain state.

`omnifs fs ls` lists every persisted spec and joins exact daemon attachment state. A configured spec may be detached indefinitely. A failed daemon probe yields `unknown`; it must not invent a stopped or attached runtime fact.

Host, Docker, and libkrun lifecycle owners probe and stop only exact ID-bearing runtime state. Doctor alone searches for state outside safe daemon attachments. Destructive cleanup holds the same per-ID claim as lifecycle commands and requires a cleanly stopped daemon, a fresh exact identity proof before and after consent, and interactive confirmation that `--yes` cannot bypass.

Libkrun is a libkrun microVM on Apple Silicon macOS. It ships the same filesystem binary and Omnifs VFS wire protocol as the Docker runtime; only the attach transport changes, from TCP to vsock. Three fixed vsock ports share one explicit virtio-vsock device: attach (guest-initiated, proxied by libkrun onto the daemon's fixed Unix attach socket), a readiness beacon (guest-initiated, dialed by `omnifs-fuse` once its FUSE mount is serving; see `crates/omnifs-vfs/src/beacon.rs`), and ssh (host-initiated, libkrun's connect mode, into the guest image's socket-activated dropbear, reached through `socat`). The helper owns a mode-0600 attach bridge between libkrun and the daemon target. When daemon replacement closes the target leg, the bridge closes the guest leg so the wire client reconnects through a fresh bridge connection to the restored target. No `virtio-net` device is ever configured, and the helper disables implicit vsock before adding that device with a zero TSI mask. The filesystem carries no credentials and needs no egress. The guest FUSE mount stays reachable only from inside the guest. The host-visible macOS surface remains the NFSv4 loopback filesystem; a guest runtime must never claim host visibility for its FUSE mount.

The private `omnifs-libkrun` sibling is the only libkrun process owner. It loads the absolute packaged `libexec/omnifs/libkrun.1.dylib`, uses the packaged EFI firmware, and accepts one strict typed configuration shared with the CLI. That shape fixes two raw block devices, 2 vCPUs, 2048 MiB, serial output, three vsock ports, no GPU, and no network. It has no generic device, library, firmware, feature, or socket policy surface. It never searches `PATH`, invokes an external launcher, or exposes a REST control API. Its strict record carries the full resolved spec, PID, and random instance ID. Detached teardown requires an identity-matched control Ping and Shutdown and never signals a PID read from disk. Only launch rollback may kill the unreaped child handle it directly owns.

The resolved guest image is an immutable base artifact. Each launch copies it into the filesystem ID's runtime directory as `libkrun/root.raw`, restricts that copy to mode `0600`, and passes only the copy to libkrun; the base image is never opened read-write or mutated. `root.raw` and any temporary copy are launch-owned artifacts removed by rollback, stale replacement, restart, and detach, while the base image and persistent SSH key survive.

The fail-closed lockdown check every guest runner owns is part of the runner contract, not a Docker detail. Docker asserts no binds and an env set containing only `OMNIFS_ATTACH_ADDR` plus image defaults; the exact ID, protocol, runtime, and location arrive as flat command arguments. Libkrun audits the seed's exact key set, including `OMNIFS_FS_ID`, before burning it and proves the guest-visible result in the live conformance lane: only loopback networking and no `tsi_hijack` kernel argument. Both runners fail launch before reporting success on any violation.

The libkrun guest's ssh access is keyed, not passworded: launch generates a per-filesystem ed25519 keypair under the ID's runtime directory on first use and embeds the public half in the seed as `OMNIFS_SSH_PUBKEY`. The guest installs it into root's `authorized_keys` and starts the ssh socket only when the seed carries a key; an omitted key leaves ssh disabled for that launch.

### NFSv4 loopback

macOS host-native integration uses read-only NFSv4.0 loopback. NFS is a filesystem protocol boundary, not a provider protocol.

The macOS NFS mount is excluded from Spotlight as part of filesystem startup. The
mount requests `nobrowse`, and the NFS export exposes a synthetic,
lookup-only `/.metadata_never_index` marker at its root without adding that
entry to provider listings. The runner also invokes the host `mdutil` control
when available; macOS may return a non-zero status for an NFS export with no
local metadata store even while reporting that indexing and searching are
disabled, which is an accepted success state. This policy prevents a host
indexer from recursively traversing provider-backed paths and holding the
mount during teardown; it does not special-case Spotlight in namespace or
provider semantics.

Keep NFS filehandles, stateids, leases, and NFS protocol errors in `omnifs-nfs`. Preserve read-only behavior for mutation operations. Keep macOS mount readiness and teardown behavior in the NFS/CLI path.

The shared NFS filesystem entrypoint attaches through the Omnifs VFS wire protocol. Host delivery reaches it through hidden `omnifs run-fs --protocol nfs`. Runner records and the persistent filehandle table live under the filesystem ID's state leaf (`cache/filesystems/<id>`). Restarting an active filesystem must reuse the recorded server address for that leaf, never silently bind a new port and skip remounting. Corrupt leaves degrade individually.

### Mount-table mechanics

Keep `/proc/mounts` parsing, NFS mount state-file schema/IO, and shared platform unmount command construction in `omnifs-mtab`. Filesystems and lifecycle code call that crate instead of carrying duplicate parsers, state versions, or unmount argv builders.

The `omnifs-mtab` state files under a per-ID leaf are filesystem discovery and teardown state. Mount records carry mount point, address, and pid; the host runner record carries the exact spec, random process instance, process group, and control socket. The NFS filehandle-identity table (`omnifs-nfs/src/persist.rs`) is protocol identity, so it stays in `omnifs-nfs` with the filehandles, stateids, and inode table. It lives in the same ID leaf alongside the mtab files and mirrors their write discipline, but its schema and IO are NFS-crate-owned. Records degrade individually; healthy siblings are never hidden.

### NFS deferral and `NFS4ERR_DELAY`

The NFS filesystem uses `NFS4ERR_DELAY` in two distinct ways. Do not conflate them.

**Reactive delay.** When the namespace returns a transient upstream error (`RateLimited`, `Timeout`, `Network`), the NFS adapter maps it to `NFS4ERR_DELAY` through `Status::from(&NsError)`. The client retry starts fresh; no background work continues past the reply.

**Proactive deferral.** Provider-backed `READDIR` uses the NFS-local `delayed::PendingListings` table with an inline wait budget (`NFS_INLINE_BUDGET`). Past the budget the handler replies `NFS4ERR_DELAY` while the listing task keeps running. On success, the engine namespace caches dirents so the retry hits warm cache. Only `READDIR` gets proactive deferral today: successful listings write authoritative dirents into the engine namespace cache; cold `LOOKUP` lacks the same cache-convergence guarantee.

**Concurrent dispatch.** Per-connection RPC dispatch runs each call on its own handler thread; replies carry their own XID. One slow op does not head-of-line block other RPCs on the same TCP connection. Proactive deferral is about not holding a single `READDIR` reply past the inline budget, not about serializing the connection.

**Ownership.** `async_singleflight::Group` owns exact-key OAuth refresh dedupe in `omnifs-auth`. `omnifs-nfs::delayed::PendingListings` owns exact-path listing slots, detached completion, the mutex-owned generation, and the inline wait budget for proactive `DELAY` signalling. The engine namespace computes truth and owns cache; it does not know about `NFS4ERR_DELAY` or wait budgets. Reactive `Status::from(&NsError)` maps transient upstream errors without background continuation. FUSE owns its own blocking tolerance; it has no `DELAY` equivalent.

## Must not

- Call provider WIT directly from a filesystem.
- Construct fake provider DTOs to reuse filesystem code paths.
- Own mount enumeration at the root, learned-size publication, inline-byte read policy, preload policy, or negative lookup policy.
- Put provider policy or cache schema knowledge in FUSE or NFS.
- Add macOS-specific FUSE behavior.
- Reintroduce macFUSE, `diskutil`, or macOS-specific FUSE mounting.
- Treat container FUSE as a filesystem runtime attached to the host-native daemon, never as the daemon architecture.
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
- `crates/omnifs-cli/src/host_fs.rs`, `fs_container.rs`, `docker.rs`, and `libkrun_runner.rs`
- `crates/omnifs-thin/src/host_control.rs` and `lifecycle.rs`
- `crates/omnifs-cli/tests/lifecycle_acceptance.rs`

## Validation

- Filesystem changes should include protocol-specific tests plus shared tree tests when behavior is semantic.
- FUSE-visible behavior changes need targeted FUSE tests and live runtime checks.
- NFS protocol mechanics need NFS protocol/unit tests. Host-native behavior changes need live mount tests.
- Libkrun runtime changes need the local-only `just libkrun-conformance` lane (`docs/contracts/60-build-validation.md`); it can never run in GitHub-hosted CI.
