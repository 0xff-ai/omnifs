# Plan 009: Remove the legacy control plane and finish the migration

> **Post-cutover note**: Complete. The final dry-down also removed the
> temporary legacy scanner and all compatibility migrations, backfills, and
> per-kind desired-state tables.

> **Executor instructions**: This is the deletion and contract-update plan.
> Confirm every new path is live before deleting an old one. Run the broad and
> live gates. Stop if any production caller still uses a legacy symbol. Update
> this plan's status in `plans/README.md` when done unless a reviewer owns the
> index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- AGENTS.md README.md CONTRIBUTING.md docs crates scripts just .github`
> Confirm Plans 001 through 008 are complete and their focused gates pass.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**:
  `plans/003-move-provider-work-to-daemon-reconciliation.md`,
  `plans/005-make-daemon-own-attachments.md`,
  `plans/006-retire-client-filesystem-state-and-narrow-bootstrap.md`,
  `plans/007-add-kcl-plan-and-apply-client.md`,
  `plans/008-switch-to-interactive-resource-porcelain.md`
- **Category**: migration, docs, cleanup
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

The redesign is not complete while old leases, imperative ops, client mutation
journals, client filesystem specs, and old terminology remain as alternate
paths. Two control planes would make ownership and recovery unclear.

This plan removes obsolete code, updates binding contracts to the implemented
system, and runs the full runtime proof.

## Current state at the planned commit

- `docs/contracts/50-control-plane.md:16-20` says the CLI owns filesystem specs,
  runners, `ClientOwnerId`, and the mutation journal.
- The same contract at lines 31-39 names imperative mount/fs grammar and says
  there is no apply.
- Lines 72-75 reject apply and daemon reads of client specs.
- `docs/architecture/00-overview.md:78-84` repeats the old split.
- `AGENTS.md` current shape repeats the mutation lease, client-owned
  filesystems, and live attachment terms.
- `crates/omnifs-api/proto/control/v1/control.proto:17-19` exposes the old
  mutation lease.
- `crates/omnifs-cli/src/mutation.rs` and
  `crates/omnifs-cli/src/client_state.rs` own the journal and client owner.
- `crates/omnifs-cli/src/client_fs_state.rs` owns old specs and paths.
- `crates/omnifs-daemon/src/manager.rs` combines old mutation and serving work.
- `crates/omnifs-state/src/batch.rs` mirrors six imperative ops.

By the time this plan runs, the target resource API and supervisors must be the
only production callers.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Search | `rg` commands in each step | no unintended matches |
| Docs | `just docs-check` | exit 0 |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Full gate | `just check` | exit 0 |
| Live | `just dev -y` and smoke commands | all selected cases pass |

Preserve `sccache`. Build providers before host tests when required.

## Scope

**In scope**:

- old mutation and client state modules under `crates/omnifs-cli`
- old mutation manager and batch modules under daemon/state
- old protobuf methods and messages
- obsolete core IDs and filesystem config types
- Cargo dependencies made unused by deletion
- `AGENTS.md`
- `README.md`
- `CONTRIBUTING.md`
- `docs/contracts/10-system.md`
- `docs/contracts/40-filesystems.md`
- `docs/contracts/50-control-plane.md`
- `docs/contracts/60-build-validation.md`
- `docs/architecture/00-overview.md`
- `docs/architecture/40-auth-boundary.md`
- `docs/architecture/50-nfs-filesystem.md`
- any new focused resource/reconcile architecture note
- `scripts/dev.ts`
- CI, release, smoke, and package files that name old commands or paths
- test fixtures and snapshots for removed surfaces

**Out of scope**:

- new features beyond the accepted design
- compatibility aliases for removed public commands
- provider artifact garbage collection
- remote KCL or provider sources
- schema code generation
- changes to projection or filesystem behavior

## Steps

### Step 1: Prove no production caller uses the old mutation path

Search:

```text
rg -n "BeginMutation|ApplyMutation|DropMutation|MutationOp|MutationManager|StateOp|apply_batch" crates scripts
```

Classify every match:

- obsolete production code to delete
- migration test fixture that may stay
- unrelated word that is safe

Do not delete until `provider`, `mount`, `credential`, `attachment`, `setup`,
KCL apply, provider repair, credential refresh, and credential revoke all have
new owners.

**Verify**:
write a short checklist in the commit message or implementation notes mapping
each old caller to its new owner. Do not add a tracked temporary ledger.

### Step 2: Delete old wire methods and domain DTOs

Remove:

- `BeginMutation`
- `ApplyMutation`
- `DropMutation`
- six imperative wire ops
- lease errors and active lease status
- `ServingOutcome` as a mutation reply concept

Keep resource apply receipts and explicit credential operational RPCs.

Remove obsolete conversion code and tests. Do not reserve compatibility field
numbers unless the checked-in current protocol needs them for wire hygiene.
There is no old-client compatibility promise.

**Verify**:

```text
rg -n "BeginMutation|ApplyMutation|DropMutation|MutationOp|ActiveMutation|LeaseExpired|LeaseNotHeld" crates/omnifs-api crates/omnifs-daemon crates/omnifs-cli
```

returns no production matches. `cargo nextest run -p omnifs-api -p omnifs-daemon
-p omnifs-cli` passes.

### Step 3: Delete mutation lease and journal owners

Remove:

- `crates/omnifs-cli/src/mutation.rs`
- mutation journal fields and files from `client_state`
- `ClientOwnerId` remnants
- old daemon mutation slot and lease
- old `MutationManager` after credential/reconcile work has moved
- old `StateOp` and `apply_batch`
- per-row legacy mutation provenance that no current recovery path reads

Keep durable resource apply receipts and resource-row mutation IDs if the new
retry contract uses them.

Remove dependencies made unused, including file-lock or journal helpers used
only by the deleted path.

**Verify**:

```text
rg -n "mutations.json|MutationManager|MutationSlot|CONTROL_MUTATION_TIMEOUT|StateOp|apply_batch|ClientOwnerId" crates
```

returns no production matches.

### Step 4: Prove active client state is gone and keep legacy data read only

Plan 006 must already have deleted the active spec registry,
`ClientFilesystemState`, per-ID claims, and client runtime-path code. Verify
that no compatibility adapter restored them.

After Step 3 removes the mutation journal, delete `client_dir.rs` and any
remaining helper that creates `<profile>/client`. Remove now-unused
`atomic-write-file` or `fs2` CLI dependencies only when no other CLI caller
remains.

Keep the narrow legacy filesystem scanner used by explicit interactive import
and Doctor reporting. It may read old specs and derive old runtime paths, but
it must not create, update, rename, or remove old specs. Do not recursively
delete users' existing files.

Doctor must distinguish:

- legacy client specs
- daemon-owned Attachment resources
- exact live runtimes
- stray runtimes

Any destructive runtime cleanup still requires a stopped daemon, the
profile-wide spawn lock, fresh exact identity proof, and consent that `--yes`
cannot bypass.

**Verify**:

```text
test ! -e crates/omnifs-cli/src/client_fs_state.rs
test ! -e crates/omnifs-cli/src/client_dir.rs
rg -n "ClientFilesystemState|struct Registry|struct Claim|client_root\\(" crates
```

The searches return no production matches. A fresh lifecycle creates no
client tree; a fixture with legacy specs reports them and does not launch,
edit, or delete them.

### Step 5: Remove the old filesystem configuration domain

`AttachmentSpec` from Plan 001 is the only active exact runtime
configuration. Remove the temporary conversion to and from
`omnifs_core::fs::Spec`.

Replace active uses:

- `fs::Id` becomes the enclosing Attachment resource's `ResourceName`;
- `fs::Spec` becomes `AttachmentSpec`;
- `fs::Protocol` and `fs::Runtime` move to, or are re-exported only from, the
  Attachment domain;
- VFS handshake, thin arguments, runtime driver requests, daemon status, and
  access RPC use Attachment names and exact Attachment specs.

The read-only legacy scanner must use a private strict `LegacyFilesystemSpec`
DTO for old JSON. It converts only after explicit user import. Do not keep the
active `fs::Spec` type just to parse legacy files.

Delete `crates/omnifs-core/src/fs.rs` if no genuine filesystem-protocol type
remains there. Do not rename FUSE or NFS adapters, filesystem processes, or
the product itself. This deletion applies only to the old configured
filesystem domain.

**Verify**:

```text
rg -n "fs::Spec|fs::Id|omnifs_core::fs" crates
```

The search returns no production matches. Deliberate legacy fixture text may
still contain old JSON field names.

### Step 6: Remove old public grammar and examples

Search and remove:

- `omnifs fs`
- `fs create|attach|detach|restart|rm|shell|ls`
- non-interactive mount authoring flags
- claims that setup waits inside the apply RPC
- old mutation recovery output
- old `filesystem` term where it means the public Attachment resource

Keep `filesystem` where it correctly names FUSE/NFS adapters, processes,
protocol behavior, images, or the product's projected filesystem.

Update shell completions and CLI contract tests.

**Verify**:

```text
target/debug/omnifs --help
target/debug/omnifs attachment --help
target/debug/omnifs plan --help
target/debug/omnifs apply --help
```

show only final public grammar.

### Step 7: Update binding contracts and architecture

Update current docs only after code matches them.

`docs/contracts/50-control-plane.md` must state:

- SQLite desired resources
- typed plan/apply RPC
- compare-and-swap and durable receipt semantics
- fast acknowledgement boundary
- separate typed progress stream and terminal revision/action rules
- durable typed action acceptance and restart recovery
- daemon-owned attachment lifecycle
- KCL client role
- interactive porcelain role
- desired versus observed status
- no secrets in resources or replies

`docs/contracts/40-filesystems.md` must state:

- Attachment resource versus filesystem process
- `VfsSession` registry
- daemon runner ownership
- VFS v11 identity
- down and restart behavior

`docs/contracts/10-system.md` and auth architecture must state:

- Credential resource is non-secret
- material is separate
- drain-before-delete and explicit revoke

`docs/contracts/60-build-validation.md` must add:

- KCL target build checks
- provider preparation/cache tests
- resource control tests
- durable credential and Attachment action recovery tests
- attachment live lanes

Update `AGENTS.md` current shape, vocabulary, orientation, and footguns. Remove
stale footguns rather than keeping historical warnings.

Add one current architecture note for resource control and reconciliation if
the overview would become too dense. Do not copy source structs or protobuf
blocks into current docs.

**Verify**:
`just docs-check` exits zero and searches for rejected old claims return no
matches.

### Step 8: Update contributor and CI paths

Update:

- `scripts/dev.ts`
- just recipes
- smoke scripts
- CI command assertions
- release archive smoke
- npm package tests

They must use:

- compiled worktree binary, never bare `omnifs`
- KCL apply for automation
- `attachment` commands for read or operational access
- daemon-owned runtime paths
- required Wasmtime cache

Keep provider-build contention rules and existing test cache paths.

**Verify**:
run the script's check/typecheck command if present, workflow lint through
`just check`, and all focused smoke scripts available on the host.

### Step 9: Run the full static and test gate

Run:

```text
just build providers
just check
```

`just check` must cover formatting, docs/contracts, workflows, providers, host
clippy/tests, and whitespace.

If it fails, fix the root cause. Do not weaken tests, downgrade dependencies,
or bypass `sccache`.

### Step 10: Run live cold-cache and lifecycle acceptance

Use a fresh isolated `OMNIFS_HOME` and the compiled worktree binary.

Prove:

1. The raw `ApplyResources` call returns after desired commit with providers
   still preparing.
2. `omnifs setup` stays open on `WatchProgress`, names each required provider
   and stage, and reaches one terminal revision event.
3. JSONL streams typed events and one terminal result; JSON emits exactly one
   terminal envelope after waiting.
4. a Ctrl-C after commit exits the viewer while daemon work continues, and a
   new revision watch resumes from a current snapshot.
5. Attachment progress reaches ready only after namespace, runtime, OS mount,
   and VFS session readiness.
6. common file tools work through the host attachment.
7. Docker attachment exposes byte-identical data and no credentials.
8. `omnifs down` stops both runtimes and preserves desired attachments.
9. daemon restart restores them.
10. a second restart uses the same Wasmtime cache.
11. a failed provider preparation ends the affected revision stream with a
    stable failure while other resources and control remain available.
12. deleting an attachment stays visible and streams `Deleting` until exact
    teardown completes.
13. an accepted Attachment restart survives daemon replacement and its action
    watch resumes from the new snapshot without a second restart.

Use:

```text
just dev -y
target/debug/omnifs status
```

and the current smoke path in `CONTRIBUTING.md`, updated in Step 7.

Run libkrun conformance only on opted-in Apple Silicon. Report it as not run on
other hosts.

### Step 11: Run final searches

Run:

```text
rg -n "BeginMutation|ApplyMutation|DropMutation|MutationManager|mutations.json|ClientOwnerId" crates docs AGENTS.md README.md CONTRIBUTING.md scripts
rg -n "Bootstrap<|Bootstrap::|client_fs_state|ClientFilesystemState|client_root\\(" crates
rg -n "fs::Spec|fs::Id|omnifs_core::fs" crates
rg -n "omnifs fs|fs attach|fs detach|client/filesystems/specs" docs AGENTS.md README.md CONTRIBUTING.md scripts crates/omnifs-cli
rg -n "cache_dir: Option|Option<.*cache_dir|no.cache|disable.*cache" crates/omnifs-engine crates/omnifs-daemon
git diff --check
git status --short
```

The Bootstrap and active client-state search returns no matches. Only
intentional migration-test, read-only legacy-scanner, or historical plan
matches may remain in the other searches. Current source and current docs must
have no old active-control claims.

## Test plan

This plan mostly deletes and updates. Do not replace deleted tests one for one
when the same contract is already covered by resource, reconcile, and
attachment tests.

Retain or add proof for:

- migrated current profile
- legacy detached client specs not auto-launched
- no old wire methods
- no old public grammar
- KCL automation
- provider preparation under cold cache
- detailed revision and action progress streams
- slow or disconnected stream consumers do not block daemon work
- human, JSONL, JSON, quiet, and non-TTY output contracts
- desired/observed status
- down and restart
- common filesystem tools

## Done criteria

- [ ] There is one resource plan/apply control plane.
- [ ] The mutation lease, imperative ops, and client journal are gone.
- [ ] The daemon owns attachment specs and runtimes.
- [ ] `client_fs_state.rs`, `client_dir.rs`, and active client filesystem paths
  are gone.
- [ ] `AttachmentSpec` and `ResourceName` replace active `fs::Spec` and
  `fs::Id`.
- [ ] `Bootstrap<R>` and role markers are gone; the narrow bootstrap crate
  remains.
- [ ] `omnifs-state` does not depend on `omnifs-bootstrap`.
- [ ] Legacy client filesystem specs are never auto-launched or auto-deleted.
- [ ] Public grammar uses `attachment`, `plan`, `apply`, `config`, and resource
  porcelain.
- [ ] Required Wasmtime caching has no optional path.
- [ ] Current docs describe the implemented system and no rejected old model.
- [ ] `just check` passes.
- [ ] Supported live cold-cache and attachment lanes pass.
- [ ] Final searches have no unintended matches.

## STOP conditions

Stop and report if:

- Any production caller still needs the old lease or `StateOp`.
- Removing old provenance breaks interrupted apply recovery.
- A current profile cannot migrate without losing credential material or mount
  identity.
- Legacy detached specs would be launched or deleted automatically.
- Current code and the proposed binding contract still disagree after prior
  plans.
- Full live validation exposes a projection or filesystem behavior regression.
- The final tree would need two desired-state owners.

## Maintenance notes

- Plans under `plans/` remain proposals and execution history. Current system
  truth belongs in `AGENTS.md`, contracts, architecture notes, code, and tests.
- After this plan, new mutations must be resource edits or explicit operational
  actions. Do not restore hidden imperative authoring flags.
- Any future cache-prune feature must preserve fast raw apply and honest
  streamed compilation progress.

## Git workflow

- Use a branch such as `codex/009-resource-cutover`.
- Use Conventional Commits in logical units, for example:
  - `refactor(control)!: remove mutation lease`
  - `docs(architecture): record resource reconciliation`
- Do not push or open a pull request unless the operator asks.
