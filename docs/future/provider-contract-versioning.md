# Provider references and upgrade

This document supersedes the previous provider contract snapshot design. A mount
must not carry a copied subset of the provider manifest as its compatibility
authority. A mount references a provider artifact by canonical content id, and
provider upgrade compares the old and new provider manifests by loading the
artifacts themselves.

It builds on the embedded provider bundle (`install_embedded_bundle`,
`crates/omnifs-cli/src/provider_bundle.rs`) and the disk-reconcile daemon
(`ProviderRegistry::reconcile`, `crates/omnifs-host/src/registry.rs`).

## Core model

Provider bytecode is canonically addressed by the BLAKE3 hash of the exact WASM
component bytes. That hash is the provider id.

```rust
pub struct Provider {
    pub id: ProviderId,
    pub meta: ProviderMeta,
    artifact: ProviderArtifact,
}

pub struct ProviderRef {
    pub id: ProviderId,
    pub meta: ProviderMeta,
}

pub struct ProviderMeta {
    pub name: ProviderName,
    pub version: Option<ProviderVersionLabel>,
}
```

`Provider.id` and `ProviderRef.id` are the same kind of value: the BLAKE3
bytecode hash. `ProviderRef` is what the mount spec stores. It references the
provider by id and carries a narrow metadata snapshot so status output and UI
can show a useful label even when the artifact is missing.

`ProviderMeta.name` and `ProviderMeta.version` are not identity. They are
provider-declared metadata read from the embedded manifest and indexed by the
catalog for lookup, display, and upgrade discovery.

The provider manifest is not inlined into the mount spec. The manifest lives
inside the provider artifact and in the catalog's parsed view of that artifact.
The mount stores only the provider reference, auth binding, config, and mount
settings.

## Why the contract model goes away

The old model stamped a `contract` block into the mount spec: config fields,
capabilities, auth scheme, and a version label copied out of the provider
manifest. That is the wrong authority. The provider manifest already owns that
surface, and copying part of it into the mount makes the spec both redundant and
lossy.

The old provider surface is still needed for upgrade classification, but it
does not need to live in the spec. It can be recovered from the old provider
artifact, provided the artifact is retained. The mount stores a reference to
that artifact, not a reserialized fragment of the old manifest.

## Provider storage

Provider bytecode cannot be overwritten in place while a mount references it.
Provider installation may advance a "latest by provider name" index, but the
content-addressed artifact named by `ProviderRef.id` must remain available.
A mounted provider resolves to the artifact it names, not to the newest artifact
that happens to be installed.

If the referenced bytecode is missing, that is an artifact-retention or
corruption error. It is not a compatibility error. A later garbage collector can
remove old artifacts only after proving no mount references their ids.

## Catalog indexes

The catalog resolves artifacts by id and indexes provider metadata for
discovery:

```rust
catalog.get(id) -> Provider
catalog.latest_by_name(name) -> Provider
catalog.versions_by_name(name) -> Vec<Provider>
```

Only `get(id)` is used for normal serving of an existing mount. The name and
version indexes are for `omnifs init`, status/debug surfaces, and explicit
upgrade discovery.

## Normal mount resolution

Normal serving should not compare a mount to the latest provider. It should:

1. Parse and validate the mount spec.
2. Resolve `spec.provider.id` through the provider catalog.
3. Return a `Provider` handle for that exact artifact.
4. Validate config and credential binding against the manifest on that provider
   handle.
5. Materialize the mount from the resolved provider.

A newer provider with the same `ProviderMeta.name` is not an error. It only
means an upgrade candidate exists. Legacy specs that reference providers by
filename or name need an adoption path that resolves the current artifact and
writes the first `ProviderRef`.

## Upgrade flow

Provider upgrade is an explicit transition from an old provider reference to a
new provider candidate. The flow is:

1. Load the old provider artifact named by `mount.spec.provider.id`.
2. Load the old `ProviderManifest` from that artifact.
3. Choose a new provider candidate from the catalog, usually through
   `latest_by_name(old.meta.name)`.
4. Load the new candidate's `ProviderManifest`.
5. Compare the manifest surfaces directly: config metadata, capabilities,
   limits, auth declaration, and any future declarative migration hints.
6. Ask the user to approve what is changing and to provide any missing values.
7. Rewrite the mount with migrated config, the chosen credential binding, and
   the new `ProviderRef`.

The comparison is best effort and host-driven. Additive config with defaults can
be migrated mechanically. Required or renamed config needs user input.
Capability, scalar limit, or auth changes require explicit user approval
because they change the provider authority or runtime ceiling the mount was
initialized with. Provider-supplied imperative migration code remains out of
scope unless separately approved as a new provider authority.

## What dies

The mount-level contract system should be removed, not renamed:

- `Spec.contract`
- `omnifs_mount::contract`
- `Spec::stamp_contract`
- `Catalog::live_contract_for`
- CLI contract preflight code
- `MaterializeError::ContractMismatch`

The daemon backstop changes shape. It should fail when the referenced artifact
is missing or cannot validate the mount, not when the mount is older than the
latest provider. The CLI owns the interactive upgrade path.

Provider-manifest WIT or SDK evidence, such as an internal
`ContractEvidence` field, is a separate concept. It records what host/provider
interface a provider was built against. It must not be used as the mount
compatibility snapshot, and it may need a clearer name to avoid reviving the
deleted contract model.

## Required capabilities and over-grant

The manifest declares only what provider access *needs*
(`caps::AccessNeed`); it is never a runtime grant source. A mount spec's
`capabilities` block carries the explicit *grants* (`caps::Grants`), seeded from
the manifest's needs at `omnifs init` (`Grants::from_needs`) and owned by the
spec thereafter. The host resolves those grants into the enforcement allowlist
(`CapabilityChecker::from_config`, resolving dynamic markers such as a unix
socket from the mount endpoint). So the spec, not the provider's own
declaration, bounds the grant.

**Required capabilities (enforced).** At provider start `materialize` rejects a
spec whose grants do not satisfy every capability the manifest declares the
provider needs (`Grants::satisfies` returning `caps::Missing`, surfaced as
`MaterializeError::MissingCapabilities`). The check covers the four
access-control kinds (domains, git repos, unix sockets, preopened paths); a
missing grant would otherwise surface as a denied callout at the provider's
first request, so the mount fails fast at start instead. Narrowing a glob
(`git@github.com:*` to a concrete repo) still satisfies; a dynamic need is met
only by a dynamic grant of the same kind. Scalar resource limits (memory, blob
bytes) live in manifest `limits` and mount-spec `limits`; they are not inputs to
`Grants::satisfies` and fail as resource exhaustion, not access denial. The
check never weakens the trust boundary, it only guarantees a provider is not
under-provisioned, so it is not a gated change.

Limit changes still require explicit re-consent during upgrade because they
change runtime ceilings, but they are reported separately from capability
changes.

**Over-grant (deferred).** Nothing enforces `spec grants ⊆ manifest needs`, so a
hand-authored or hand-edited spec can grant a provider more reach (extra HTTP
domains, git repos, unix sockets, filesystem preopens, or auth schemes) than its
pinned manifest declares. This predates the content-addressed provider work;
pinning did not introduce it. Adding an over-grant check would change the
security model (a gated decision), so the upgrade flow deliberately ships without
it, preserving the user's ability to grant capabilities beyond the manifest. A
later slice should decide the over-grant surface (the confused-deputy-risky
preopen/socket/auth surface is the first candidate; network egress may stay
user-grantable) and add the check. Until then the gap is recorded in
`docs/architecture/00-overview.md`.

## Open implementation details

The direction above is fixed; these are implementation details to settle in the
first slice:

- The final wire shape for `Spec.provider`.
- The concrete provider version label source. If the manifest lacks a provider
  version field, add a label field rather than making version a gate.
- The provider artifact store layout and retention rule.
- The CLI policy for surfacing available upgrades during `omnifs up` versus a
  dedicated `omnifs upgrade <mount>` command.
