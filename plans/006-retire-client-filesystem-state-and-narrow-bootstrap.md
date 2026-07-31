# Plan 006: Retire client filesystem state and narrow bootstrap

> **Post-cutover note**: Complete. The temporary read-only legacy scanner was
> removed after the ownership cutover; no client filesystem desired-state
> reader remains.

> **Executor instructions**: Read this plan in full before editing. Plans 001
> through 005 must already be integrated. This plan deletes a former state
> owner, so map every production caller before deleting the module. Run each
> verification gate. If an old client path is still needed by normal
> attachment lifecycle, stop and fix the ownership move in Plan 005 instead of
> preserving a bridge here. Update this plan's status in `plans/README.md` when
> done unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- Cargo.toml crates/omnifs-bootstrap crates/omnifs-cli crates/omnifs-daemon crates/omnifs-state crates/omnifs-fs-runtime crates/omnifs-itest scripts/dev.ts AGENTS.md docs/contracts/50-control-plane.md docs/contracts/60-build-validation.md`
> Confirm Plans 001 through 005 are complete. Compare the current callers
> against the ownership table below. If normal lifecycle still writes
> `client/filesystems`, treat that as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/005-make-daemon-own-attachments.md`
- **Category**: architecture, migration, tech debt
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

Daemon-owned attachments do not remove the old owner by themselves. At the
planned commit, `client_fs_state.rs` still owns desired specs, lifecycle locks,
runtime paths, image caches, config parsing, and metrics lookup. Leaving that
module in place would preserve a second attachment control plane and make
future callers choose between client and daemon paths.

The shared bootstrap crate also reaches into state construction through
`Bootstrap<Daemon>`, while daemon logging and context resolve the profile
twice. This plan removes the false role-generic API, keeps only the pre-RPC
facts that cannot live behind RPC, and deletes client filesystem state as an
active system.

## Current state

At commit `035952bc7`:

- `crates/omnifs-bootstrap/src/lib.rs:23-38` defines `Bootstrap<R>` plus
  `Client` and `Daemon` marker types.
- `crates/omnifs-bootstrap/src/lib.rs:38-124` mixes profile paths, directory
  permissions, process identity reads, and spawn locking.
- `crates/omnifs-bootstrap/src/lib.rs:126-220` splits nearly identical
  operations by marker type. The marker does not enforce process authority
  because any workspace caller can construct either role.
- `crates/omnifs-daemon/src/logging.rs:17-20` resolves a daemon profile for the
  log, while `crates/omnifs-daemon/src/context.rs:28-30` resolves it again for
  runtime state.
- `crates/omnifs-state/src/paths.rs:33-36` accepts
  `&Bootstrap<Daemon>` only to append `daemon-state`.
- `crates/omnifs-cli/src/client_fs_state.rs:14-141` combines filesystem spec
  storage, runtime paths, cache paths, default locations, and profile config.
- `crates/omnifs-cli/src/client_fs_state.rs:144-332` owns a JSON registry,
  per-ID file locks, creates, deletes, and its full error model.
- `crates/omnifs-cli/src/metrics.rs:7-34` reaches metrics config and the
  profile root through `ClientFilesystemState`.
- `crates/omnifs-cli/src/commands/doctor.rs:244-284` uses old per-ID claims
  before runtime repair, in addition to the profile-wide daemon spawn lock.
- `crates/omnifs-cli/src/client_state.rs` and `client_dir.rs` still own the old
  mutation journal. They are transitional after this plan and are deleted in
  Plan 009.

Plan 005 must have changed the normal path before this plan starts:

- Attachment definitions live in daemon SQLite resources.
- The daemon supervisor owns normal launch, stop, retry, and per-name
  serialization.
- Runtime drivers take explicit daemon-owned paths.
- Normal status and access come from daemon RPC.
- No normal command creates or updates a client filesystem spec.

## Target ownership

| Fact | Target owner |
|---|---|
| Profile root and fixed pre-RPC files | `omnifs-bootstrap::Profile` |
| Spawn exclusion and exact daemon identity | `omnifs-bootstrap` |
| SQLite, cache, log, and runtime child paths | `omnifs-state` |
| Desired attachment definitions | daemon resource control and SQLite |
| Per-attachment lifecycle serialization | daemon `AttachmentSupervisor` |
| Runtime identity and probe/launch/stop mechanics | `omnifs-fs-runtime` |
| Attachment default normalization | daemon resource planner |
| CLI metrics preference | narrow CLI profile config |
| Old `client/filesystems` discovery | read-only CLI legacy scanner |
| Old mutation journal | transitional `client_state`, removed in Plan 009 |

`omnifs-bootstrap` remains a crate. It owns the shared facts needed before RPC
can work. The following public names go away:

```text
Bootstrap<R>
Client
Daemon
bootstrap_dir
```

The target public names are:

```text
Profile
SpawnLock
DaemonIdentity
ResolveError
OMNIFS_HOME_ENV
```

`Profile` is one resolved root plus the three fixed paths:

```text
control.sock
process.json
spawn.lock
```

It may expose the existing safe pre-RPC operations for spawn locking, control
socket binding, identity read and publication, and exact cleanup. It must not
derive daemon store, cache, log, attachment, metrics, KCL, or legacy client
paths.

## Complete baseline impact inventory

The executor must account for every row. The file may have moved after prior
plans, but the responsibility must still reach the stated final action.

### Bootstrap callers

| Baseline file | Current use | Exact action |
|---|---|---|
| `crates/omnifs-cli/src/rpc.rs` | stores `Bootstrap<Client>` in every RPC client | store only the control socket path |
| `crates/omnifs-cli/src/commands/daemon_start.rs` | resolves profile, locks spawn, probes socket | use full `Profile`; keep the lock across the start decision |
| `crates/omnifs-cli/src/daemon_teardown.rs` | reads exact process identity and removes stale files | use `Profile` and `DaemonIdentity` |
| `crates/omnifs-cli/src/commands/doctor.rs` | stale daemon cleanup and stopped-daemon exclusion | use `Profile`, `DaemonIdentity`, and `SpawnLock` |
| `crates/omnifs-cli/src/commands/down.rs` | resolves the daemon endpoint | use `Profile` or an exact control socket |
| `crates/omnifs-cli/src/commands/inspect.rs` | obtains the control socket | pass only the socket path |
| `crates/omnifs-cli/src/inventory.rs` | reads identity when RPC cannot answer | use `Profile`; keep RPC authoritative |
| `crates/omnifs-cli/src/client_dir.rs` | finds the transitional journal root | use `Profile::root`; delete the module in Plan 009 |
| `crates/omnifs-cli/src/docker/container.rs` | imports `OMNIFS_HOME_ENV` | keep only the constant import if still needed after runtime extraction |
| `crates/omnifs-daemon/src/context.rs` | stores `Bootstrap<Daemon>` and derives attach paths | store `Profile` plus explicit daemon paths |
| `crates/omnifs-daemon/src/logging.rs` | resolves a second profile and keeps a `OnceLock` | accept `&Profile`; delete the second resolution and guard |
| `crates/omnifs-daemon/src/app.rs` | publishes and removes exact daemon state | call the renamed Profile and identity operations |
| `crates/omnifs-daemon/src/manager.rs` tests | constructs daemon bootstrap fixtures | construct explicit profile and state roots |
| `crates/omnifs-state/src/paths.rs` | appends `daemon-state` to bootstrap root | accept the daemon-state root itself |
| `crates/omnifs-state/src/lib.rs` | exposes bootstrap-shaped open, repair, and log APIs | change all three APIs to explicit root paths |
| `crates/omnifs-state/src/tests.rs` | creates `Bootstrap<Daemon>` fixtures | use direct temporary daemon-state roots |
| CLI and lifecycle path tests | assert generic endpoint paths | assert `Profile` paths and single-resolution flow |

### Client filesystem state callers

| Baseline file | Current use | Exact action |
|---|---|---|
| `crates/omnifs-cli/src/client_fs_state.rs` | active specs, locks, paths, config, caches | delete the file |
| `crates/omnifs-cli/src/main.rs` | declares the module | remove the declaration; add only narrow config and legacy modules |
| `crates/omnifs-cli/src/commands/fs.rs` | create, read, claim, launch, stop, poll, list, shell | use resource and operational RPCs; delete active registry and lifecycle helpers |
| `crates/omnifs-cli/src/commands/setup.rs` | previews locations and creates or attaches specs | include exact Attachment resources in one plan/apply |
| `crates/omnifs-cli/src/inventory.rs` | joins configured specs with live sessions | read daemon desired and observed Attachment status |
| `crates/omnifs-cli/src/commands/doctor.rs` | owns active state, candidates, claims, and paths | use daemon status plus runtime crate; add a read-only legacy scanner |
| `crates/omnifs-cli/src/metrics.rs` | gets config and profile root through filesystem state | read narrow CLI config from `Profile::root` |
| `crates/omnifs-cli/src/filesystem_driver.rs` | carries client state, owner, spec, UI, and endpoints | remove the CLI copy after Plan 004 extraction |
| `crates/omnifs-cli/src/host_fs.rs` | takes client state for logs and scan roots | remove the CLI copy; use daemon paths through runtime crate |
| `crates/omnifs-cli/src/docker/mod.rs` | reads client config and profile identity | remove the CLI driver; runtime crate gets exact image and profile identity |
| `crates/omnifs-cli/src/docker/container.rs` tests | construct client filesystem config | move runtime tests and delete config fixtures |
| `crates/omnifs-cli/src/libkrun_runner.rs` | reads client config, cache, and runtime root | remove the CLI driver; runtime crate gets exact paths and image |
| `crates/omnifs-cli/src/guest_image_pull.rs` | fills client guest-image cache | move to runtime crate and daemon cache |
| `crates/omnifs-cli/tests/cli_contract.rs` | asserts client spec, state, and mount paths | replace normal cases with resource RPC fixtures; retain named legacy cases |
| `crates/omnifs-cli/tests/lifecycle_acceptance.rs` | stores specs and uses generic Bootstrap | drive Attachment resources and `Profile` |
| `crates/omnifs-itest/src/live.rs` | hard-codes client runner state paths | use daemon attachment runtime paths |
| filesystem Docker/libkrun live tests | hard-code client runtime roots | use daemon paths and resource-driven lifecycle |
| `scripts/dev.ts` | writes `[filesystem]` config and invokes `fs` lifecycle | submit exact Attachment resources and stop writing client filesystem state |

### Related code removed in adjacent plans

Plan 005 removes `ClientOwnerId` from the VFS handshake, thin arguments,
Docker command, host launch, and libkrun seed. Plan 009 removes its final
mutation-lease use, the old mutation journal, `client_state.rs`,
`client_dir.rs`, imperative wire operations, and their file-lock and
atomic-write dependencies. Plan 008 removes the transitional public `fs`
grammar.

Do not recreate any of those concepts to make this plan compile.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Bootstrap tests | `cargo nextest run -p omnifs-bootstrap` | all pass |
| State tests | `cargo nextest run -p omnifs-state` | all pass |
| Daemon tests | `cargo nextest run -p omnifs-daemon` | all pass |
| CLI tests | `cargo nextest run -p omnifs-cli` | all pass |
| Control-plane live test | focused `crates/omnifs-itest/tests/control_plane` command used by the repo | pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Docs | `just docs-check` | exit 0 |

Preserve the global `sccache` wrapper. If a Cargo command cannot reach the
configured `sccache` daemon, request the required scoped permission and rerun
it. Do not disable caching.

## Scope

**In scope**:

- `Cargo.toml`
- `Cargo.lock`
- `crates/omnifs-bootstrap/Cargo.toml`
- `crates/omnifs-bootstrap/src/lib.rs`
- `crates/omnifs-cli/Cargo.toml`
- `crates/omnifs-cli/src/main.rs`
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-cli/src/commands/daemon_start.rs`
- `crates/omnifs-cli/src/commands/doctor.rs`
- `crates/omnifs-cli/src/commands/down.rs`
- `crates/omnifs-cli/src/commands/inspect.rs`
- `crates/omnifs-cli/src/commands/setup.rs`
- current attachment command and inventory modules
- `crates/omnifs-cli/src/daemon_teardown.rs`
- `crates/omnifs-cli/src/metrics.rs`
- `crates/omnifs-cli/src/client_fs_state.rs` (delete)
- `crates/omnifs-cli/src/profile_config.rs` (new, if a separate module stays
  clearer than a metrics-local type)
- `crates/omnifs-cli/src/legacy_filesystems.rs` (new)
- narrow transitional edits to `client_dir.rs` and `client_state.rs`
- `crates/omnifs-state/Cargo.toml`
- `crates/omnifs-state/src/paths.rs`
- `crates/omnifs-state/src/lib.rs`
- `crates/omnifs-state/src/tests.rs`
- `crates/omnifs-daemon/src/context.rs`
- `crates/omnifs-daemon/src/logging.rs`
- `crates/omnifs-daemon/src/app.rs`
- direct daemon tests and fixtures
- `crates/omnifs-itest/src/live.rs`
- direct control-plane and lifecycle integration fixtures
- `scripts/dev.ts`
- `AGENTS.md`
- `docs/contracts/50-control-plane.md`
- `docs/contracts/60-build-validation.md`

**Out of scope**:

- changing resource or apply semantics
- changing provider preparation or namespace publication
- changing FUSE, NFS, or VFS behavior
- adding a system service manager
- moving pre-RPC identity into SQLite
- deleting `process.json` or `spawn.lock`
- deleting or rewriting users' old `client/filesystems` files
- deleting the old mutation journal before Plan 009
- KCL implementation
- final public command cleanup
- a generic path registry or profile service

## Steps

### Step 1: Confirm every `client_fs_state` caller has a final owner

Run:

```text
rg -n "client_fs_state|ClientFilesystemState|ClientConfig|Registry|Claim" crates/omnifs-cli crates/omnifs-itest
```

Classify every production match into one of these groups:

1. desired attachment read or write, which must already use daemon resource
   RPC;
2. normal runtime path or lifecycle, which must already use daemon-owned paths
   and `omnifs-fs-runtime`;
3. profile config or metrics, moved in Step 5;
4. stopped-daemon or legacy diagnosis, moved in Steps 6 and 7;
5. obsolete code or test, deleted with the module.

Do not record this classification in a tracked temporary file. Put it in the
implementation notes or commit body.

**Verify**: no match remains without one listed final owner. If a normal
attachment command still needs the old registry, state root, runtime root,
host log, image cache, or default location, stop and report that Plan 005 is
incomplete.

### Step 2: Replace `Bootstrap<R>` with one plain `Profile`

In `omnifs-bootstrap`:

1. Rename the stored path to `root`.
2. Replace `Bootstrap<R>` with non-generic `Profile`.
3. Delete `Client`, `Daemon`, `PhantomData`, and role-specific duplicate
   constructors.
4. Rename `Instance` to `DaemonIdentity`.
5. Keep `Profile::resolve()` and an explicit-root constructor for tests.
6. Keep the current fail-closed socket, private permission, atomic identity,
   process start-time, executable proof, and exact replacement checks.
7. Keep `SpawnLock` opaque and held by ownership.

Do not add a trait, builder, global singleton, or a second profile type.
Compile-time role markers are not a security boundary and must not be replaced
with new typestate.

Update bootstrap tests to prove:

- exact fixed paths;
- root normalization for an explicit relative root;
- owner-only directory and file modes;
- identity round trip and current-process proof;
- exact cleanup never removes replacement identity;
- stale socket cleanup never follows a symlink;
- a live control socket is never replaced.

**Verify**:

```text
cargo nextest run -p omnifs-bootstrap
rg -n "Bootstrap<|Bootstrap::|PhantomData|struct Client|struct Daemon" crates/omnifs-bootstrap
```

Tests pass and the search returns no matches.

### Step 3: Resolve one profile for the daemon process

Change the daemon entry path so one `Profile` value feeds logging, context,
control publication, and state-root construction.

Target call shape:

```text
Profile::resolve
  -> init daemon tracing with &Profile
  -> DaemonContext::new(Profile)
  -> run daemon with that context
```

The exact function split may follow current crate visibility, but the profile
must resolve once. Delete `logging::RESOLVED_PROFILE` and
`verify_resolved_profile`; they become unnecessary when both consumers borrow
the same resolved value.

Keep this startup order:

1. resolve the profile;
2. construct one `DaemonStatePaths` from the profile's daemon-state root;
3. open the daemon log;
4. load embedded providers;
5. create private startup directories;
6. bind the control socket;
7. publish exact daemon identity;
8. create the progress hub and start control service;
9. construct the required `ComponentEngine` and start embedded preparation;
10. open state from the same `DaemonStatePaths` and enqueue retained providers;
11. open the rest of `HostOnline` and start reconcilers.

This preserves Plan 003's early preparation order. Do not move SQLite back in
front of embedded preparation while simplifying bootstrap.

On any failure after binding but before publication completes, remove only the
socket this start created. Normal shutdown still unpublishes only the exact
current identity.

**Verify**:

```text
rg -n "RESOLVED_PROFILE|verify_resolved_profile|for_daemon" crates/omnifs-daemon
cargo nextest run -p omnifs-daemon
```

The search returns no matches and daemon startup, corrupt-store recovery,
replacement-safe cleanup, and control readiness tests pass.

### Step 4: Remove bootstrap from the state crate

Make the state crate's path owner explicit:

```text
DaemonStatePaths::new(daemon_state_root)
StateStore::open(paths, options)
StateStore::recreate_control_store(paths, options)
open_daemon_log(&paths)
```

Use the `DaemonStatePaths` added in Plan 003 as the one owner of child layout
such as `control-store`, `cache`, `staging`, `logs`, and the pre-SQLite
Wasmtime path. Delete the old endpoint-based constructor. It must not receive a
`Profile` or rebuild the profile root. The daemon composition root computes
`profile.root().join("daemon-state")` once, constructs one paths value, and
passes clones or borrows down. This proves early `ComponentEngine` setup and
later `StateStore` open use the same cache.

Remove `omnifs-bootstrap` from `crates/omnifs-state/Cargo.toml`.

**Verify**:

```text
cargo tree -p omnifs-state | rg "omnifs-bootstrap"
cargo nextest run -p omnifs-state -p omnifs-daemon
```

The dependency search returns no output. State path, repair rollback, private
permissions, daemon log, and daemon recovery tests pass.

### Step 5: Move CLI profile config and metrics to narrow owners

Remove `ClientConfig`, `ClientMetrics`, and `ClientFilesystemAssets` from
`client_fs_state.rs`.

Keep only a strict CLI preference type for settings that still exist. In this
design, that is `[metrics].enabled`. Put it in `profile_config.rs` if Doctor
and metrics both need it, or keep it private to `metrics.rs` if there is only
one real caller.

Remove `[filesystem].docker_image` and `[filesystem].guest_image` from profile
config. Attachment resources store exact image references after daemon
normalization. The existing image environment overrides may remain at their
current explicit authoring boundary, but the daemon must persist the resolved
value before reconciliation.

Update `scripts/dev.ts` to submit exact attachment image values or set the
existing explicit image override before creating the Attachment. It must not
write `[filesystem]` config for normal lifecycle.

Make metrics take a `Profile` root or resolve it directly through
`Profile::resolve`; it must not construct any filesystem state or create a
`client/` directory. Metrics remain best effort and write only
`<profile>/metrics/cli.jsonl`.

Doctor's config probe must report a strict parse error, including the obsolete
`[filesystem]` section, instead of silently treating it as current.

**Verify**:

```text
rg -n "ClientConfig|ClientMetrics|ClientFilesystemAssets|\\[filesystem\\]|docker_image|guest_image" crates/omnifs-cli scripts/dev.ts
cargo nextest run -p omnifs-cli
```

Only intentional image environment or Attachment field names remain. Metrics
tests prove no `client/` directory is created.

### Step 6: Replace active client spec reads with daemon resource reads

Remove the old registry from:

- inventory collection;
- setup;
- attachment list, restart, access, create, and delete paths;
- status next-action selection;
- normal Doctor checks.

All desired and observed Attachment facts come from the daemon API introduced
by Plans 001, 002, and 005. A stopped daemon does not make old client specs
current desired state. Status may report the daemon as stopped and separately
report that legacy files exist, but it must not merge them into the active
resource list.

Remove every `Registry` and `Claim` use from normal code. The daemon supervisor
serializes normal lifecycle by attachment name.

**Verify**:

```text
rg -n "\\.registry\\(\\)|Registry|Claim" crates/omnifs-cli/src
cargo nextest run -p omnifs-cli
```

No active registry or claim match remains. CLI resource and lifecycle tests
pass.

### Step 7: Add one read-only legacy filesystem scanner

Create `crates/omnifs-cli/src/legacy_filesystems.rs`. Its name and docs must
state that it handles old profile data only.

It may:

- derive the old `client/filesystems/specs`, `state`, and `runtime` roots from
  a supplied profile root;
- list old spec filenames;
- strictly parse one old `fs::Spec` for display or an explicit later import;
- supply exact old runtime paths to stopped-daemon Doctor probes;
- report corrupt or mismatched files without aborting unrelated findings.

It must not:

- create any old directory;
- write, update, rename, or remove a spec;
- decide current desired state;
- launch a runtime;
- run during daemon startup or attachment reconciliation;
- expose a general registry or claim API.

Doctor may still stop or clean an exact legacy runtime through
`omnifs-fs-runtime`, but it must hold the profile-wide `SpawnLock`, prove the
daemon is stopped before and after its probe, recheck exact runtime identity,
and get the existing user consent. Hold the spawn lock across the full repair.
This replaces the old combination of a per-ID client claim plus spawn lock.

Add an explicit legacy finding with a future import hint. Plan 008 adds the
final interactive import command. Do not auto-import or auto-delete.

Tests must prove:

- absent old directories return an empty result without creating them;
- valid specs are found in sorted order;
- unknown fields and filename mismatches are reported;
- scanning does not change file content, mode, or modification time;
- normal inventory does not treat a legacy detached spec as desired;
- a Doctor repair cannot overlap daemon start;
- replacement runtime identity is never touched.

**Verify**:

```text
cargo nextest run -p omnifs-cli
git diff -- crates/omnifs-cli/src/legacy_filesystems.rs
```

The module contains no file write, create, rename, or remove operation.

### Step 8: Delete `client_fs_state.rs`

Delete:

- `ClientFilesystemState`;
- client filesystem `Registry`;
- `Claim`;
- the client filesystem error enum;
- active spec, state, runtime, cache, log, and default-location path helpers;
- the module declaration and tests.

Remove direct dependencies that became unused. Do not delete
`client_state.rs` or `client_dir.rs` yet if the old mutation journal still
uses them. Narrow their module docs so they no longer claim to support
filesystem state. Plan 009 deletes both with the journal.

Update integration fixtures to use daemon attachment paths for current
lifecycle and direct fixture paths for deliberate legacy cases.

**Verify**:

```text
test ! -e crates/omnifs-cli/src/client_fs_state.rs
rg -n "client_fs_state|ClientFilesystemState|ClientFilesystemAssets|struct Registry|struct Claim" crates
```

The file is absent and the search returns no production matches.

### Step 9: Update Bootstrap callers and final path checks

Update CLI RPC, daemon start, down, teardown, Inspector, inventory, Doctor, and
tests to use `Profile`, `DaemonIdentity`, or the exact control socket path.

Use the full `Profile` only where a caller needs a spawn lock or identity.
Pass a `PathBuf` control socket to `RpcClient`. Do not let ordinary RPC methods
gain access to process identity or spawn operations.

Run:

```text
rg -n "Bootstrap<|Bootstrap::|omnifs_bootstrap::\\{[^}]*Client|omnifs_bootstrap::\\{[^}]*Daemon|\\bInstance\\b" crates
rg -n "client/filesystems" crates/omnifs-cli crates/omnifs-daemon crates/omnifs-state crates/omnifs-itest
```

The first search returns no production matches. The second returns only the
named legacy scanner, legacy fixtures, and comments that state the path is
legacy. No daemon production module may match.

### Step 10: Update current contracts

Update current docs to match the code that now exists:

- `AGENTS.md` orientation must describe `omnifs-bootstrap` as the small pre-RPC
  profile, spawn lock, and exact daemon identity crate.
- `AGENTS.md` current shape must state that desired Attachment specs and
  runtime paths are daemon-owned and `client/filesystems` is legacy only.
- `docs/contracts/50-control-plane.md` must state that the daemon owns
  Attachment resources and lifecycle, and must describe the remaining
  bootstrap files.
- `docs/contracts/60-build-validation.md` must replace
  `Bootstrap<Client>`/`Bootstrap<Daemon>` with `Profile`, state that the daemon
  resolves it once, and name the no-client-filesystem-state checks.

Do not document the old and new models as two supported choices. Keep
historical rationale in proposal or plan files only.

**Verify**:

```text
just docs-check
rg -n "Bootstrap<Client>|Bootstrap<Daemon>|CLI-owned filesystem specs|client/filesystems/specs" AGENTS.md docs/contracts docs/architecture
```

The docs gate passes. Any remaining search match must say the path is legacy,
not active state.

### Step 11: Run final gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-bootstrap -p omnifs-state -p omnifs-daemon -p omnifs-cli
just check host
just test host
just docs-check
git diff --check
git status --short
```

Run the focused control-plane live test and one fresh-profile Attachment
lifecycle supported on the host.

In the fresh profile, assert:

```text
test ! -e "$OMNIFS_HOME/client/filesystems"
```

Do not run that assertion against a real user profile.

## Test plan

Retain and adapt the current bootstrap safety tests. Add only tests that prove
the new ownership boundaries:

- one daemon profile resolution feeds log, state, and control paths;
- live socket replacement fails closed;
- exact identity cleanup cannot remove a replacement;
- `omnifs-state` opens from an explicit daemon-state root;
- state has no bootstrap dependency;
- normal lifecycle creates no client filesystem tree;
- legacy scanning is read-only and deterministic;
- legacy detached specs do not become desired Attachments;
- stopped-daemon repair uses the spawn lock and exact runtime identity;
- metrics do not construct client filesystem state.

Use current CLI path tests, daemon corrupt-store recovery tests, bootstrap
identity tests, and Doctor repair tests as structural patterns. Delete tests
that only prove the removed active JSON registry.

## Done criteria

- [ ] `Bootstrap<R>`, `Client`, `Daemon`, and `PhantomData` are absent from
  `omnifs-bootstrap`.
- [ ] `Profile`, `SpawnLock`, and `DaemonIdentity` cover only pre-RPC facts.
- [ ] The daemon resolves one profile value for logging, state, and control.
- [ ] One explicit `DaemonStatePaths` feeds early engine setup, logging, state,
  and runtime paths.
- [ ] Embedded preparation still starts before SQLite opens.
- [ ] `omnifs-state` has no dependency on `omnifs-bootstrap`.
- [ ] `client_fs_state.rs`, `ClientFilesystemState`, `Registry`, and `Claim`
  are absent.
- [ ] Desired and observed Attachment data comes only from daemon RPC and
  SQLite.
- [ ] Normal runtime paths, logs, and image caches are daemon-owned.
- [ ] Normal lifecycle creates no `client/filesystems` path.
- [ ] Old client filesystem data is read only by the named legacy scanner.
- [ ] No old spec is auto-imported, launched, edited, or deleted.
- [ ] The mutation journal remains only as an explicit transitional owner for
  Plan 009.
- [ ] All Step 11 checks pass.

## STOP conditions

Stop and report if:

- A normal attachment command still needs a client spec, claim, state path,
  runtime path, log path, image cache, or default-location helper.
- The daemon would need to read `client/filesystems` during startup or
  reconciliation.
- Removing the per-ID claim makes normal lifecycle concurrent outside the
  daemon supervisor.
- Doctor cannot make legacy runtime repair safe with the spawn lock and exact
  runtime identity.
- State needs any pre-RPC process identity fact rather than an explicit
  daemon-state path.
- Resolving the daemon profile once would require a global mutable singleton.
- A proposed simplification removes fail-closed socket handling, PID reuse
  proof, executable proof, or replacement-safe cleanup.
- Existing profile config cannot be rejected or migrated without silently
  changing an Attachment's exact image.
- Any step would recursively remove a user's legacy files.

## Maintenance notes

- Do not remove `omnifs-bootstrap` unless another process supervisor makes its
  pre-RPC contract unnecessary. Moving its code into CLI and daemon modules is
  not removal.
- Keep `process.json` diagnostic only. Reachable RPC remains authoritative.
- Do not add daemon-state child paths to `Profile`; the state and runtime
  owners derive them.
- `legacy_filesystems.rs` is a migration reader, not a new state owner. It
  should have no write methods.
- Plan 008 adds the final explicit interactive import path.
- Plan 009 removes the old mutation journal, `client_state.rs`, and
  `client_dir.rs`. After that plan, a fresh profile has no active `client/`
  tree at all.

## Git workflow

- Use a branch such as `codex/006-profile-and-client-fs-cutover`.
- Use Conventional Commits. Suitable logical commits include
  `refactor(bootstrap)!: replace role-generic profile state` and
  `refactor(cli)!: remove client filesystem state`.
- Do not push or open a pull request unless the operator asks.
