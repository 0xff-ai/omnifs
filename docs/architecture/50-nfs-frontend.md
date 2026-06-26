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

## Rejected shapes

- NFS-specific projection semantics
- provider-specific behavior in NFS protocol handlers
- frontend-owned cache schema
- macFUSE or macOS FUSE as the current integration path
- write behavior hidden behind ordinary projected file writes
