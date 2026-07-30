# Plan 001: Add the typed resource domain and durable desired state

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report. Do not improvise. When done, update this plan's status in
> `plans/README.md` unless a reviewer told you that they maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- Cargo.toml crates/omnifs-core crates/omnifs-api crates/omnifs-state`
> If an in-scope file changed, compare the current-state facts below with the
> live code before proceeding. A conflict in ownership or schema is a STOP
> condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: none
- **Category**: architecture, migration
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

Plan and apply need one typed value and one durable revision. The current store
has separate imperative mount and credential operations, a mount-only revision,
and no provider, credential, mount, and attachment desired set. This plan adds
the model and storage without changing the current serving or CLI path.

The result must be usable by the new control API in Plan 002 while the old
mutation path still compiles.

## Current state

- `crates/omnifs-state/migrations/0001_initial.sql:1-94` creates provider
  artifacts, credentials, `mount_state`, mounts, and recovery state.
- `crates/omnifs-state/src/lib.rs:97-104` gives `StateStore` one read pool, one
  writer, one credential refresh wakeup, and one provider import semaphore.
- `crates/omnifs-state/src/writer.rs:31-60` serializes writes through one owned
  SQLite connection. Keep this owner.
- `crates/omnifs-state/src/batch.rs:20-31` mirrors the six current mutation ops.
  Do not extend `StateOp` into the resource model. Add a separate full-set
  transition.
- `crates/omnifs-api/src/mount.rs` and
  `crates/omnifs-api/src/credential.rs` hold current typed control DTOs.
- `crates/omnifs-core/src/fs.rs` holds strict protocol, runtime, location, and
  filesystem identity. Its validation rules are the starting point for the
  normalized attachment spec.
- The repo forbids `serde_json/preserve_order`. Preserve this invariant.

Binding constraints:

- The host owns provider artifacts, credentials, mounts, and state.
- Credentials must not appear in resource snapshots, plans, receipts, Debug,
  logs, or errors.
- Provider config remains provider-owned JSON.
- Domain, wire, stored, and presentation types must stay separate where their
  contracts differ.
- This repo is pre-alpha. Do not add compatibility readers beyond the one
  explicit migration in this plan.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Core/API tests | `cargo nextest run -p omnifs-core -p omnifs-api` | all pass |
| State tests | `cargo nextest run -p omnifs-state` | all pass |
| Host check | `just check host` | exit 0 |

If a Cargo command fails because `sccache` cannot connect to
`127.0.0.1:4226`, rerun it with the repo's `workspace-sccache` permission
profile or request scoped approval. Do not clear `RUSTC_WRAPPER`.

## Scope

**In scope**:

- `Cargo.toml`
- `crates/omnifs-core/Cargo.toml`
- `crates/omnifs-core/src/lib.rs`
- `crates/omnifs-core/src/fs.rs`
- `crates/omnifs-core/src/resource.rs` (new)
- `crates/omnifs-core/src/attachment.rs` (new)
- `crates/omnifs-api/Cargo.toml`
- `crates/omnifs-api/src/lib.rs`
- `crates/omnifs-api/src/resource.rs` (new)
- `crates/omnifs-api/src/attachment.rs` (new)
- `crates/omnifs-state/Cargo.toml`
- `crates/omnifs-state/migrations/0002_resources.sql` (new)
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-state/src/db.rs`
- `crates/omnifs-state/src/row.rs`
- `crates/omnifs-state/src/provider.rs`
- `crates/omnifs-state/src/credential.rs`
- `crates/omnifs-state/src/mount.rs`
- `crates/omnifs-state/src/resource.rs` (new)
- `crates/omnifs-state/src/resource/codec.rs` (new if needed)
- `crates/omnifs-state/src/resource/migration.rs` (new if needed)
- `crates/omnifs-state/src/tests.rs`

**Out of scope**:

- the protobuf schema
- daemon control handlers
- KCL
- provider compilation or generation publication
- filesystem runner ownership
- CLI command changes
- removal of current tables or `StateOp`
- changes to provider authority

## Target types

Add small core value types:

```text
ResourceName
ResourceRevision
ResourceDigest
ResourceKey { kind, name }
AttachmentSpec
AttachmentVersion
```

`ResourceName` must have one documented grammar used by every resource kind.
Use the current `MountName` and `fs::Id` rules as evidence. If those grammars
conflict, choose the stricter common grammar and keep the old types until the
final cleanup plan.

Add API/domain resource types:

```text
ResourceDeclarations { api_version, resources }
ResourceDefinition::Provider
ResourceDefinition::Credential
ResourceDefinition::Mount
ResourceDefinition::Attachment
ProviderDefinition
CredentialDefinition
MountResourceDefinition
AttachmentDefinition
NormalizedResourceSet
ResourceChange
ResourcePlan
ApplyReceipt
ResourcePhase
ResourceStatus
```

Use enums for variants and phases. Reject unknown fields on Serde authoring
types. Do not put protobuf, terminal rendering, SQLite row fields, or KCL source
paths on these domain types.

The normalized `ProviderDefinition` contains only an exact `ProviderId`. Local
and embedded source selectors are client authoring types added in Plan 007.

The normalized `AttachmentSpec` contains exact protocol, runtime, location, and
runtime asset references. Its constructor owns platform and pair validation.

## Storage shape

Add strict typed tables:

```text
resource_state
  singleton
  revision
  desired_digest
  initialized
  updated_at

provider_resources
  name
  provider_digest
  revision
  last_mutation_id
  updated_at

credential_resources
  name
  provider_name
  scheme
  account
  revision
  last_mutation_id
  updated_at

mount_resources
  name
  canonical
  version
  provider_name
  credential_name nullable
  revision
  last_mutation_id
  updated_at

attachment_resources
  name
  canonical
  version
  revision
  last_mutation_id
  updated_at

apply_receipts
  mutation_id
  input_digest
  result_revision
  result_digest
  changed
  created_at
```

Use foreign keys for resource references where SQLite can enforce them. Do not
put secret bytes in `credential_resources`.

Do not add a generic `resources(kind, name, json)` table. Each resource has a
different stored contract and transition rules.

Keep provider WASM in the current `providers` table.

## Steps

### Step 1: Add core resource and attachment value types

Implement strict constructors, parsing, display, Serde, and errors for the core
types. Make invalid states hard to construct, but do not use typestate or deep
generic wrappers.

`AttachmentSpec` must reject:

- unsupported protocol/runtime pairs
- non-absolute host locations
- a caller-selected guest location
- host-only fields on Docker or libkrun
- Docker-only or libkrun-only asset fields on another runtime

Keep `omnifs_core::fs` unchanged for current callers. Add explicit conversion
between exact `AttachmentSpec` and current `fs::Spec`. This conversion is
temporary. Plan 009 removes active `fs::Spec` and `fs::Id` after VFS and
runtime callers use Attachment types.

**Verify**:
`cargo nextest run -p omnifs-core` returns success, including new round-trip and
rejection tests.

### Step 2: Add typed resource DTOs and deterministic digesting

Implement the API/domain types in `omnifs-api`.

Normalize every set by:

1. validating `apiVersion`
2. rejecting duplicate `(kind, name)` keys
3. sorting by kind tag then resource name
4. validating all references
5. producing a `NormalizedResourceSet`

Compute `ResourceDigest` with BLAKE3 over:

- a fixed `omnifs-resource-set-v1` domain tag
- sorted resource kind tags
- length-delimited resource names and string fields
- exact `ProviderId` bytes
- credential fields
- existing canonical mount config bytes or a pinned equivalent
- exact attachment fields

Do not hash the text emitted by KCL or `serde_json::to_vec` of the whole set.
Add a test that permutation of the input list does not change the digest.

Keep planning as a pure function:

```text
plan(current: &NormalizedResourceSet, desired: &NormalizedResourceSet)
    -> Vec<ResourceChange>
```

Cover create, update, delete, unchanged, and secret-impact flags. The planner
does no I/O.

**Verify**:
`cargo nextest run -p omnifs-api` returns success, including order-independent
digest and diff tests.

### Step 3: Add the resource schema migration

Create `0002_resources.sql`. Use SQLite `STRICT` tables, length checks for IDs
and digests, foreign keys, and explicit check constraints for enum text.

Do not edit `0001_initial.sql`. Existing test databases must migrate through
both files.

Insert the singleton `resource_state` row with revision zero, the empty-set
digest, and `initialized = 0`.

Add indexes for:

- provider digest lookup
- credential provider lookup
- mount provider and credential lookup
- receipt creation time

**Verify**:
`cargo nextest run -p omnifs-state opens_migrates_and_joins_the_writer` returns
success.

### Step 4: Add one exact resource snapshot read

Add `StateStore::resource_snapshot`. Like
`StateStore::serving_snapshot`, it must read the singleton revision and every
resource table in one read transaction.

Decode stored rows into stored types first, then convert to domain types with
validation. A corrupt row must fail the whole snapshot with table and resource
context.

The snapshot must contain no credential material.

**Verify**:
add a state test that writes all four kinds, reads one snapshot, and confirms
the revision, digest, exact definitions, sort order, and absence of secret
fields. Run `cargo nextest run -p omnifs-state`.

### Step 5: Add full-set compare-and-swap apply

Add a separate `StateStore::apply_resources` path. It must run in one
`StateWriter` job and one SQLite transaction.

Input:

```text
mutation_id
base_revision
expected_desired_digest
normalized_desired_set
credential secret sidecars
```

Required behavior:

- return the stored receipt when the same mutation ID and input digest repeat
- reject mutation ID reuse with different input
- return unchanged when current desired digest already matches
- otherwise require exact base revision
- apply the typed diff without deleting credential material that an active
  generation may still hold
- advance the global revision once
- stamp every changed desired row with the same revision and mutation ID
- store the receipt in the transaction
- roll back every row on any error

Credential secret sidecars are request-only values with redacted `Debug`.
Persist them through the existing secret type and lifecycle table. If the
current credential schema cannot safely bind a resource name without serving
changes, add a narrow mapping column/table and leave final drain-before-delete
work to Plan 003.

Do not persist a plain or unsalted digest of secret bytes in
`apply_receipts.input_digest`. Digest the non-secret declarations and sidecar
targets only. The first accepted mutation ID wins; retry returns its stored
receipt and never compares or reapplies later secret bytes. New material needs
a fresh mutation or credential action ID.

Bound receipt retention in the same writer owner. Use a clear count or age
policy with tests. Do not start a cleanup task.

**Verify**:
`cargo nextest run -p omnifs-state` passes tests for success, stale base,
unchanged desired state, duplicate retry, duplicate mismatch, rollback, and
secret redaction.

### Step 6: Backfill current daemon state once

When `resource_state.initialized = 0`, run one writer transaction that:

1. creates a Provider resource for every provider digest referenced by a
   current mount or non-deleted credential
2. creates a Credential resource for every non-deleted credential
3. creates a Mount resource for every current mount
4. leaves Attachment resources empty
5. computes the resulting desired digest
6. sets revision to the current mount revision or one, whichever is greater
7. marks the state initialized

Naming must be deterministic and tested:

- use the provider metadata name when unique
- suffix a short provider digest when one metadata name maps to several
  retained digests
- use a valid, unique account-based credential name when possible
- otherwise use `credential-<stable digest prefix>`
- extend a prefix on collision rather than accepting one

Do not convert client filesystem specs. The daemon must not read them.

The backfill is a one-way migration, not a permanent legacy reader.

**Verify**:
add a test database with two provider versions, mounts, active and deleted
credentials, then open the new store twice. Both opens must produce the same
resource names and digest, with no duplicate rows.

### Step 7: Add final local gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-core -p omnifs-api -p omnifs-state
just check host
git diff --check
```

All commands must exit zero.

## Test plan

Use existing patterns:

- core value-type tests beside their modules
- API JSON tests in `crates/omnifs-api/src/mount.rs` as a shape example
- SQLite transaction tests in `crates/omnifs-state/src/tests.rs:295-569`
- serving snapshot test in `crates/omnifs-state/src/tests.rs:670-726`

Add tests for:

- every name and attachment validation boundary
- strict unknown-field rejection
- resource set permutation
- duplicate key and dangling reference rejection
- every diff action
- full transaction rollback
- receipt dedupe
- deterministic backfill
- secret absence and redacted Debug
- corrupt stored resource decoding

## Done criteria

- [ ] All four resource definitions have strict Rust domain types.
- [ ] `ResourceDigest` is stable across resource order.
- [ ] SQLite has typed desired tables and one global resource revision.
- [ ] `StateStore` can read and compare-and-swap a full desired set.
- [ ] Apply receipts make mutation retries durable.
- [ ] Resource reads, plans, receipts, and Debug contain no secret material.
- [ ] Existing state backfills once with pinned deterministic names.
- [ ] Current imperative store and serving code still compile.
- [ ] No file outside the in-scope list changed.
- [ ] All commands in Step 7 pass.

## STOP conditions

Stop and report if:

- `MountName` and `fs::Id` cannot share a resource-name grammar without
  invalidating common current names.
- Backfill cannot map an existing mount to exactly one retained provider.
- Credential material would need to enter a resource DTO to preserve current
  behavior.
- Full-set apply would require provider compilation, provider instantiation,
  filesystem work, or network I/O.
- The migration needs to read `<profile>/client`.
- Any change requires enabling `serde_json/preserve_order`.
- An in-scope schema or owner has changed since `035952bc7` in a way that
  invalidates the target tables.

## Maintenance notes

- Review the digest encoder as a stored contract. Future fields must update its
  version or exact encoding tests.
- Keep apply policy out of SQL row codecs.
- Keep the old mutation path only until Plan 009. Do not add new features to
  it.
- The attachment stored shape will become VFS identity in Plan 005, so changes
  to it need explicit protocol review.

## Git workflow

- Use a branch such as `codex/001-resource-state`.
- Use Conventional Commits, for example
  `feat(control): add durable resource state`.
- Do not push or open a pull request unless the operator asks.
