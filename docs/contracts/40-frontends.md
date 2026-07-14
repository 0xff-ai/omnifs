# Frontend contracts

Status: current-contract
Owns: FUSE and NFS frontend adapter boundaries, protocol state, mount behavior, and frontend-specific validation.

## Read when

Read this before touching `omnifs-thin`, the `omnifs-fuse` or `omnifs-nfs` protocol crates, `omnifs-mtab`, frontend startup, protocol replies, filehandles, stateids, inode tables, kernel notifications, NFS leases, macOS mount readiness, or live mount tests.

## Rules

### Adapter boundary

Frontend crates translate namespace answers into protocol state. They do not decide projection semantics.

A frontend consumes the narrow `omnifs_engine::namespace` surface (`Namespace`, `NodeId`, `Attrs`, `DirPage`, `ReadAnswer`, `NsEvent`, and friends) and nothing else of the engine. It never touches `Tree`, `render`, or `view` directly: the already-policied protocol answer (size, TTL, change counter, direct-I/O, read style) crosses the `Namespace` boundary as plain data. Keep inode numbers, filehandles, stateids, leases, notifications, reply construction, and protocol-specific error mapping in frontend crates. Convert namespace types into protocol replies once at the frontend boundary.

### Frontend registry

The daemon constructs one `TreeNamespace` over the shared mount registry and gives it to `omnifs_vfs_wire::VfsServer`. `VfsServer` owns the fixed local and requested TCP/vsock listeners, attach tokens, listener and connection tasks, readiness, and the deduplicated live attachment snapshot; the daemon owns namespace construction, control serving, durable attach-target records, and process lifetime. Every frontend exposes the complete namespace, so adding or removing a mount changes every frontend together. Frontends never store mount membership, selection, or filtering. Each frontend process owns one protocol surface and its own lifetime; the CLI owns launch and teardown through the frontend backend seam.

### FUSE

FUSE is the Linux frontend protocol. The slim `omnifs-thin fuse` mode can be delivered as a local process, Docker container, or krunkit guest; see `docs/contracts/60-build-validation.md` for image build and publish contracts.

The Docker-hosted FUSE frontend's mount lives entirely inside the container's own mount namespace, so killing the container is an accepted, clean failure mode: the mount disappears with it, with nothing left to unmount host-side, and `omnifs frontend restart fuse --environment docker` creates a fresh container that serves again.

Keep FUSE inode tables, kernel notifications, mount/unmount mechanics, and FUSE reply types in `omnifs-fuse`. Keep shared projection behavior in `omnifs-engine/src/tree`.

### Frontend processes and drivers

Every frontend is a separate slim runner process. `omnifs-thin` contains the protocol mechanics selected by its `fuse` or `nfs` mode, with no engine runtime, Wasmtime runtime, provider bundle, or daemon control plane. It links the `omnifs_engine::Namespace` interface and wire-backed client implementation, while the daemon remains the only process that executes providers and owns shared VFS semantics. A driver only chooses how the CLI delivers that process: `local` on the host, `docker` in a container, or `krunkit` in a guest. `omnifs-vfs-wire` owns serialization, framing, handshake, attach transport and reconnect, readiness signaling, and the client wire cache.

The public frontend identity is `(filesystem, environment, location)`: filesystem is `fuse` or `nfs`; environment is `host`, `docker`, or `krunkit`; location is caller-selected only for host frontends. Low-level code may retain `Driver::Local`, `FrontendDelivery::Local`, and `mount_point` where those names describe launch or protocol mechanics. Public commands, tables, and help use filesystem, environment, and location. `omnifs frontend enable`, `disable`, and `restart` own runner lifecycle; `ls` reports the Inventory observation join. Top-level `up`, `apply`, and `down` mutate only the daemon, so runners remain alive and reconnect across daemon restarts.

### Frontend delivery backend seam

Frontend delivery sits behind the CLI's `FrontendBackend` seam (`crates/omnifs-cli/src/frontend_backend.rs`): frontend lifecycle commands launch, probe, and tear down frontends through that trait, never against a specific runtime's client library directly. The drivers are `local`, `docker`, and `krunkit`; protocol kind and delivery driver remain separate facts.

Krunkit is a libkrun microVM on macOS. It ships the same frontend binary and Omnifs VFS wire protocol as the Docker driver; only the attach transport changes, from TCP to vsock. Three vsock devices, each multiplexed by port on the guest's single virtio-vsock CID: attach (guest-initiated, proxied by krunkit onto the daemon's `POST /v1/frontend/attach-target/vsock` socket), a readiness beacon (guest-initiated, dialed by `omnifs-fuse` once its FUSE mount is serving — see `crates/omnifs-vfs-wire/src/beacon.rs`), and ssh (host-initiated, krunkit's explicit `connect` vsock mode, into the guest image's socket-activated dropbear, reached via `ssh -o ProxyCommand='socat - UNIX-CONNECT:<path>'`). No `virtio-net` device is ever configured: the frontend carries no credentials and needs no egress. Its purpose is dropping the Docker Desktop dependency, not changing mount semantics: the guest FUSE mount stays reachable only from inside the guest, exactly as it is inside a Docker container today. The host-visible macOS surface remains the NFSv4 loopback frontend; a backend must never claim host visibility for its guest FUSE mount.

The fail-closed lockdown check every backend's launch path runs immediately after start, killing the guest on violation, is part of the backend contract, not a Docker particular. Docker asserts no mounts and an env set of exactly the two attach vars plus the image's own defaults. Krunkit asserts, against the live process's own argv (`ps`, since macOS has no `/proc`): no `virtio-net` device, exactly the expected device count, the two disk devices (root + seed), and the three vsock devices at their expected socket paths — plus a seed audit that the per-launch seed ISO's staging directory carries exactly its four expected `KEY=VALUE` keys before it is burned, since only the attach token among them is sensitive. Both backends fail the launch (never report success) on any violation.

The krunkit guest's ssh access is keyed, not passworded: `launch` generates a per-workspace ed25519 keypair under `<config_dir>/krunkit/` on first use (persists across launches) and embeds the public half in the seed as `OMNIFS_SSH_PUBKEY`. The guest installs it into root's `authorized_keys` and starts the ssh socket only when the seed carries a key; an omitted key (as in the boot smoke test, `scripts/guest-image/smoke.sh`) leaves ssh disabled for that launch, loudly logged in the guest journal rather than a silent hang.

### NFSv4 loopback

macOS host-native integration uses read-only NFSv4.0 loopback. NFS is a frontend protocol boundary, not a provider protocol.

The macOS NFS mount is excluded from Spotlight as part of frontend startup. The
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

The `omnifs-thin nfs` mode attaches through the Omnifs VFS wire protocol. Frontend discovery records and the persistent filehandle table live under per-location state leaves (`cache/frontends/<kind>/<blake3-of-location>`); restarting an active frontend location must reuse the recorded server address for that leaf, never silently bind a new port and skip remounting. Corrupt leaves degrade individually.

### Mount-table mechanics

Keep `/proc/mounts` parsing, NFS mount state-file schema/IO, and shared platform unmount command construction in `omnifs-mtab`. Frontends and lifecycle code call that crate instead of carrying duplicate parsers, state versions, or unmount argv builders.

The `omnifs-mtab` state files under a per-location leaf are frontend *discovery and teardown* state (location, address, pid), shared by frontend runners and the CLI. The NFS filehandle-identity table (`omnifs-nfs/src/persist.rs`, persisted so a restarted out-of-process frontend decodes handles a kernel client still holds) is *protocol identity*, not frontend discovery, so it stays in `omnifs-nfs` with the filehandles, stateids, and inode table. It lives in the same location leaf (`cache/frontends/nfs/<hash>`) alongside the mtab files and mirrors their discipline (version field, unknown version is an error, atomic write, 0600 mode), but its schema and IO are NFS-crate-owned. Discovery records degrade individually; healthy siblings are never hidden.

### NFS deferral and `NFS4ERR_DELAY`

The NFS mode of `omnifs-thin` uses `NFS4ERR_DELAY` in two distinct ways. Do not conflate them.

**Reactive delay.** When the namespace returns a transient upstream error (`RateLimited`, `Timeout`, `Network`), the NFS adapter maps it to `NFS4ERR_DELAY` through `Status::from(&NsError)`. The client retry starts fresh; no background work continues past the reply.

**Proactive deferral.** Provider-backed `READDIR` uses `delayed::Listings` with an inline wait budget (`NFS_INLINE_BUDGET`). Past the budget the handler replies `NFS4ERR_DELAY` while the listing task keeps running. On success, `Tree` caches dirents so the retry hits warm cache. Only `READDIR` gets proactive deferral today: successful listings write authoritative dirents into `Tree`; cold `LOOKUP` lacks the same cache-convergence guarantee.

**Concurrent dispatch.** Per-connection RPC dispatch runs each call on its own handler thread; replies carry their own XID. One slow op does not head-of-line block other RPCs on the same TCP connection. Proactive deferral is about not holding a single `READDIR` reply past the inline budget, not about serializing the connection.

**Ownership.** `async_singleflight::Group` owns exact-key OAuth refresh dedupe in `omnifs-auth`. `omnifs_engine::singleflight::Deferred` owns budgeted proactive deferral; NFS `delayed::Listings` is a `Deferred` over `delayed::Key`. `omnifs_engine::coalesce::Coalesce` owns covering-key namespace coalescing for provider ops. Wait budgets and proactive `DELAY` signalling are NFS frontend policy. The engine namespace computes truth and owns cache; it does not know about `NFS4ERR_DELAY` or wait budgets. Reactive `Status::from(&NsError)` maps transient upstream errors without background continuation. FUSE owns its own blocking tolerance; it has no `DELAY` equivalent.

## Must not

- Call provider WIT directly from a frontend.
- Construct fake provider DTOs to reuse frontend code paths.
- Own mount enumeration at the root, learned-size publication, inline-byte read policy, preload policy, or negative lookup policy.
- Put provider policy or cache schema knowledge in FUSE or NFS.
- Add macOS-specific FUSE behavior.
- Reintroduce macFUSE, `diskutil`, or macOS-specific FUSE mounting.
- Treat container FUSE as the architecture; the Docker-hosted frontend is one optional delivery mechanism attached to a host-native daemon.
- Remove live NFS test serialization casually.
- Claim NFS gives FUSE-equivalent permission isolation.
- Put wait budgets or `DELAY` policy in `omnifs-engine`.
- Assume every `NFS4ERR_DELAY` implies background continuation past the reply.

## Code

- `crates/omnifs-fuse/src`
- `crates/omnifs-nfs/src`
- `crates/omnifs-mtab/src`
- `crates/omnifs-engine/src/namespace` (the surface frontends consume)
- `crates/omnifs-engine/src/tree`
- `crates/omnifs-vfs-wire/src/server.rs` (`VfsServer`)
- `crates/omnifs-cli/src/frontend_backend.rs`
- `crates/omnifs-cli/src/runtime.rs`
- `crates/omnifs-cli/src/host_teardown.rs`
- `crates/omnifs-cli/tests/lifecycle_acceptance.rs`

## Validation

- Frontend changes should include protocol-specific tests plus shared tree tests when behavior is semantic.
- FUSE-visible behavior changes need targeted FUSE tests and live runtime checks.
- NFS protocol mechanics need NFS protocol/unit tests. Host-native behavior changes need live mount tests.
- Krunkit driver changes need the local-only `just krunkit-conformance` lane (`docs/contracts/60-build-validation.md`); it can never run in GitHub-hosted CI.
