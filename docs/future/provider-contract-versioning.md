# Provider contract versioning

omnifs ships providers embedded in the CLI binary and refreshes them on the host the moment a user upgrades. That refresh is correct, but it exposes a gap: a mount spec authored against one version of a provider can end up running against a different one, either failing cryptically or drifting silently. This is the design for making that boundary explicit, so a spec carries the provider contract it was built against and omnifs reconciles the two on upgrade.

It builds on the embedded provider bundle (`install_embedded_bundle`, `crates/omnifs-cli/src/provider_bundle.rs`) and the disk-reconcile daemon (`ProviderRegistry::reconcile`, `crates/omnifs-host/src/registry.rs`).

## Why this exists

Three facts about today combine into the gap:

- On upgrade, `install_embedded_bundle` overwrites every built-in `omnifs_provider_*.wasm` and the tool wasm with the new binary's versions (a sentinel-id mismatch forces a full unpack). Providers are version-locked to the binary.
- `ProviderRegistry::reconcile` re-instantiates a mount when `mount_fingerprint` moves, and that fingerprint folds in the provider wasm's size and mtime, so the overwrite is picked up rather than skipped.
- A spec that no longer fits the refreshed provider fails soft but cryptically: `reconcile` records a `MountFailure` (from `Spec::from_file` with `deny_unknown_fields`, from `materialize`, or from instantiation) and keeps serving the other mounts.

So an upgrade can leave a mount dark with a raw serde or materialization error. Worse, a spec that still parses against a changed provider but now means something different loads with no error at all. `deny_unknown_fields` (`crates/omnifs-mount/src/mount_config.rs`) catches an added or removed field; it cannot catch a changed capability set, a changed auth scheme, or a field whose meaning moved.

## The contract

What invalidates a spec is not the provider's code version but its contract: the surface a spec is written against. That surface is `{capabilities, auth, config schema}`, declared in the provider manifest (`omnifs.provider.json`).

omnifs derives a hash over that surface and treats the hash, plus a structural diff of it, as the sole compatibility authority. The provider's cargo version rides along as a provenance label only: every provider is `version.workspace = true`, so the version is workspace-shared, bumps on every release regardless of whether a given provider's contract moved, and cannot answer "did this provider's contract change."

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Compatibility authority | contract hash plus structural diff | the only signal that tracks the actual surface |
| Version role | provenance label | workspace-shared, bumps every release, says nothing per-provider |
| Snapshot home | a `contract` block in the spec | the spec stays self-describing of what it was built against |
| Who evaluates | the CLI, in `omnifs up` | upgrade is interactive (consent, prompts) and CLI-owned |
| Daemon role | refuse a drifted mount | a backstop, not the resolver |
| Orphan prune | out of scope for now | not load-bearing yet |

## The spec contract block

`omnifs init` stamps each mount spec with a `contract` block: a structural snapshot of the surface the spec was built against. It records the config fields (name and required flag), the capability set (kind and value), the auth scheme id, and the cargo-version label. The hash is derived from the block on demand, not stored.

The block describes the provider's contract, not the user's chosen values, so editing config in the spec never desyncs it. It is stored in the spec, rather than computed, because the old contract has to survive the upgrade: once `install_embedded_bundle` overwrites the provider wasm and manifest, the spec is the only place the previous contract remains. That is what makes a structural diff possible at all. A bare hash would record that the contract changed but leave omnifs unable to classify the change, which the auto-migrate path below depends on.

## Upgrade flow

`omnifs up` runs a pre-flight before reconcile. For each mount it derives the live provider contract and diffs it against the stamped block:

| Delta | Action |
|---|---|
| identical | nothing |
| additive config only (new optional field with a default) | auto: fill defaults, re-stamp, no prompt |
| breaking config (new required field, rename, removal) | prompt for the changed fields, prior answers prefilled |
| capability or auth delta | show the delta, require explicit re-consent, re-stamp |
| provider removed | hard error: the provider no longer ships |

Resolved specs are rewritten and reconcile is triggered. Anything that needs consent routes through `omnifs upgrade <mount>`; `omnifs up` auto-handles the safe cases and never blocks silently.

The daemon's materialize path (`materialize`, `crates/omnifs-mount/src/materialize.rs`, invoked by `reconcile`) is the backstop. It hashes the spec's `contract` block, hashes the live provider contract, and refuses on mismatch with a typed `ContractMismatch`, which surfaces as a `MountFailure` while the rest keep serving. Because the CLI clears mismatches before reconcile, this fires only when something drifts behind the CLI's back; it guarantees the daemon never serves a contract the spec was not written against.

## Classification follows the trust boundary

The additive, breaking, and capability split is not arbitrary; it maps to whether the security-relevant surface moved.

- A capability or auth change is exactly what the user consented to at `omnifs init`. It resurfaces for explicit approval and never auto-migrates. This is the security invariant restated: the user approves a provider's reach, and a change to that reach is a new approval.
- An additive config change touches nothing the user reviewed, so it auto-migrates with no friction.
- A breaking config change needs a value omnifs does not have, so it prompts.

So the routing table is a consequence of the trust model, not a set of hand-tuned cases.

## Open question before building

The additive-config branch assumes providers declare their config fields somewhere diffable. The manifest declares `capabilities` and `auth` but may not declare a general config-field schema. If config is validated dynamically inside the provider, the contract is effectively `{capabilities, auth}`, config compatibility cannot be classified statically, and a bad config degrades to a `materialize` refuse and a re-init. Locating or defining the provider config schema (SDK side, manifest side, or the `omnifs init` path) sizes the additive branch, possibly to nothing. Resolve this first.

## What this should not do

- It does not version provider code or gate releases on per-provider semver. The hash detects; the cargo version only labels.
- It does not introduce provider-supplied migration functions. Migration is host-driven (defaults, prompts, re-consent); a provider running migration logic would be new provider authority and is out of scope.
- It does not prune orphaned provider files. A provider dropped from the built-in set leaves stale wasm on disk; a referencing mount surfaces as "provider removed" through the flow above, not as a missing-file error. Pruning is a separate change to provider-dir ownership.
- It does not move the snapshot to a sidecar lockfile. The block lives in the spec so the spec is self-describing; the Cargo.toml and Cargo.lock split was considered and set aside for that reason.

## Later

- Independent provider versions: un-share the workspace version for `providers/*` so the version expresses per-provider contract intent and can guard the hash. A real change to how providers are versioned, deferred.
- Provider-declared migration hints: declarative rename and default rules in the manifest, so more breaking changes auto-migrate without a prompt.
