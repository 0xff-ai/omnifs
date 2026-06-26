# Frontend contracts

Status: current-contract
Owns: FUSE and NFS frontend adapter boundaries, protocol state, mount behavior, and frontend-specific validation.

## Read when

Read this before touching `omnifs-fuse`, `omnifs-nfs`, frontend startup, protocol replies, filehandles, stateids, inode tables, kernel notifications, NFS leases, macOS mount readiness, or live mount tests.

## Rules

### Adapter boundary

Frontend crates translate tree answers into protocol state. They do not decide projection semantics.

Keep inode numbers, filehandles, stateids, leases, notifications, reply construction, and protocol-specific error mapping in frontend crates. Ask `Tree` for provider-neutral projection answers. Convert neutral core/tree types once at the frontend boundary.

### FUSE

FUSE is the Linux frontend, including native Linux and the optional Docker runtime container.

Keep FUSE inode tables, kernel notifications, mount/unmount mechanics, and FUSE reply types in `omnifs-fuse`. Keep shared projection behavior in `omnifs-tree`.

### NFSv4 loopback

macOS host-native integration uses read-only NFSv4.0 loopback. NFS is a frontend protocol boundary, not a provider protocol.

Keep NFS filehandles, stateids, leases, mount state, and NFS protocol errors in `omnifs-nfs`. Preserve read-only behavior for mutation operations. Keep macOS mount readiness and teardown in the NFS path.

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

## Code

- `crates/omnifs-fuse/src`
- `crates/omnifs-nfs/src`
- `crates/omnifs-tree/src`
- `crates/omnifs-daemon/src/frontends.rs`
- `crates/omnifs-cli/src/runtime.rs`
- `crates/omnifs-cli/src/host_teardown.rs`
- `crates/omnifs-cli/tests/lifecycle_acceptance.rs`

## Validation

- Frontend changes should include protocol-specific tests plus shared tree tests when behavior is semantic.
- FUSE-visible behavior changes need targeted FUSE tests and live runtime checks.
- NFS protocol mechanics need NFS protocol/unit tests. Host-native behavior changes need live mount tests.
