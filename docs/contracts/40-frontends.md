# Frontend contracts

Status: current-contract
Owns: FUSE and NFS frontend adapter boundaries, protocol state, mount behavior, and frontend-specific validation.

## Read when

Read this before touching `omnifs-fuse`, `omnifs-nfs`, `omnifs-mtab`, frontend startup, protocol replies, filehandles, stateids, inode tables, kernel notifications, NFS leases, macOS mount readiness, or live mount tests.

## Rules

### Adapter boundary

Frontend crates translate tree answers into protocol state. They do not decide projection semantics.

Keep inode numbers, filehandles, stateids, leases, notifications, reply construction, and protocol-specific error mapping in frontend crates. Ask `Tree` for provider-neutral projection answers. Convert neutral core/tree types once at the frontend boundary.

### FUSE

FUSE is the Linux frontend, including native Linux and the optional Docker runtime container.

Keep FUSE inode tables, kernel notifications, mount/unmount mechanics, and FUSE reply types in `omnifs-fuse`. Keep shared projection behavior in `omnifs-engine/src/tree`.

### NFSv4 loopback

macOS host-native integration uses read-only NFSv4.0 loopback. NFS is a frontend protocol boundary, not a provider protocol.

Keep NFS filehandles, stateids, leases, and NFS protocol errors in `omnifs-nfs`. Preserve read-only behavior for mutation operations. Keep macOS mount readiness and teardown behavior in the NFS/CLI path.

### Mount-table mechanics

Keep `/proc/mounts` parsing, NFS mount state-file schema/IO, and shared platform unmount command construction in `omnifs-mtab`. Frontends and lifecycle code call that crate instead of carrying duplicate parsers, state versions, or unmount argv builders.

### NFS deferral and `NFS4ERR_DELAY`

`omnifs-nfs` uses `NFS4ERR_DELAY` in two distinct ways. Do not conflate them.

**Reactive delay.** When `Tree` returns a transient upstream error (`RateLimited`, `Timeout`, `Network`), the NFS adapter maps it to `NFS4ERR_DELAY` through `tree_status`. The client retry starts fresh; no background work continues past the reply.

**Proactive deferral.** Provider-backed `READDIR` uses `delayed::Listings` with an inline wait budget (`NFS_INLINE_BUDGET`). Past the budget the handler replies `NFS4ERR_DELAY` while the listing task keeps running. On success, `Tree` caches dirents so the retry hits warm cache. Only `READDIR` gets proactive deferral today: successful listings write authoritative dirents into `Tree`; cold `LOOKUP` lacks the same cache-convergence guarantee.

**Concurrent dispatch.** Per-connection RPC dispatch runs each call on its own handler thread; replies carry their own XID. One slow op does not head-of-line block other RPCs on the same TCP connection. Proactive deferral is about not holding a single `READDIR` reply past the inline budget, not about serializing the connection.

**Ownership.** `omnifs-host::singleflight` owns exact-key dedupe (`Group` for block-until-done work such as OAuth refresh; `Deferred` for budgeted proactive deferral). NFS `delayed::Listings` is a `Deferred` over `delayed::Key`. `omnifs-host::inflight::InFlight` owns ancestor-aware namespace coalescing for provider ops; it is not replaced by `Group`. Wait budgets and proactive `DELAY` signalling are NFS frontend policy. `Tree` computes truth and owns cache; it does not know about `NFS4ERR_DELAY` or wait budgets. Reactive `Status::from(&TreeError)` maps transient upstream errors without background continuation. FUSE owns its own blocking tolerance; it has no `DELAY` equivalent.

## Must not

- Call provider WIT directly from a frontend.
- Construct fake provider DTOs to reuse frontend code paths.
- Own root mount discovery, learned-size publication, inline-byte read policy, preload policy, or negative lookup policy.
- Put provider policy or cache schema knowledge in FUSE or NFS.
- Add macOS-specific FUSE behavior.
- Reintroduce macFUSE, `diskutil`, or macOS-specific FUSE mounting.
- Treat container FUSE as the architecture; Docker is one launch mechanism.
- Remove live NFS test serialization casually.
- Claim NFS gives FUSE-equivalent permission isolation.
- Put wait budgets or `DELAY` policy in `omnifs-tree`.
- Assume every `NFS4ERR_DELAY` implies background continuation past the reply.

## Code

- `crates/omnifs-fuse/src`
- `crates/omnifs-nfs/src`
- `crates/omnifs-mtab/src`
- `crates/omnifs-engine/src/tree`
- `crates/omnifs-daemon/src/frontends.rs`
- `crates/omnifs-cli/src/runtime.rs`
- `crates/omnifs-cli/src/host_teardown.rs`
- `crates/omnifs-cli/tests/lifecycle_acceptance.rs`

## Validation

- Frontend changes should include protocol-specific tests plus shared tree tests when behavior is semantic.
- FUSE-visible behavior changes need targeted FUSE tests and live runtime checks.
- NFS protocol mechanics need NFS protocol/unit tests. Host-native behavior changes need live mount tests.
