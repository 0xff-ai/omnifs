# Declarative resource control plane plans

Generated on 2026-07-30 against commit `035952bc7`.

Read [the target design](000-declarative-resource-control-plane-design.md)
before starting. Execute plans in the order below unless dependency notes say
otherwise. Each executor must read its assigned plan in full, run its drift
check, honor its STOP conditions, and update the status row when done.

Status values:

- `TODO`
- `IN PROGRESS`
- `DONE`
- `BLOCKED: <one-line reason>`
- `REJECTED: <one-line reason>`

In these plans, daemon-owned reconciliation means work runs outside the
`ApplyResources` transaction and survives client disconnect. It does not mean
the CLI hides the work or returns at once. Mutation commands wait by default
on `WatchProgress` and show typed stages until their revision or action reaches
a terminal result.

## Execution order and status

| Plan | Title | Priority | Effort | Depends on | Status |
|---|---|---:|---:|---|---|
| [001](001-add-resource-domain-and-durable-state.md) | Add typed resource domain and durable state | P1 | L | none | DONE |
| [002](002-add-fast-plan-and-apply-rpc.md) | Add fast typed plan, apply, and progress RPCs | P1 | L | 001 | DONE |
| [003](003-move-provider-work-to-daemon-reconciliation.md) | Move provider work to daemon-owned reconciliation | P1 | L | 002 | DONE |
| [004](004-extract-filesystem-runtime-drivers.md) | Extract filesystem runtime drivers | P1 | L | 001 | DONE |
| [005](005-make-daemon-own-attachments.md) | Make the daemon own attachments | P1 | L | 003, 004 | DONE |
| [006](006-retire-client-filesystem-state-and-narrow-bootstrap.md) | Retire client filesystem state and narrow bootstrap | P1 | L | 005 | DONE |
| [007](007-add-kcl-plan-and-apply-client.md) | Add KCL plan and apply | P1 | L | 005 | TODO |
| [008](008-switch-to-interactive-resource-porcelain.md) | Switch to interactive resource porcelain | P1 | L | 005, 006, 007 | TODO |
| [009](009-remove-legacy-control-plane-and-finish-migration.md) | Remove the legacy control plane and finish migration | P1 | L | 003, 006, 007, 008 | TODO |

## Dependency graph

```text
001 resource types and SQLite
 |
 +--> 002 fast plan/apply RPC
 |     |
 |     `--> 003 provider preparation + serving reconcile --+
 |                                                         |
 `--> 004 extract runtime drivers --> 005 attachments <----+
                                         |
                         +---------------+---------------+
                         |                               |
                         v                               v
              006 client fs + bootstrap          007 KCL client
                         |                               |
                         +---------------+---------------+
                                         |
                                         v
                                008 CLI porcelain
                                         |
                                         v
                                009 cutover + docs
```

Plans 003 and 004 can be developed in separate worktrees after their
dependencies land. After Plan 005, Plans 006 and 007 can proceed in parallel.
Do not merge them out of dependency order. Plan 006 deletes the client
filesystem owner only after the daemon path is live. Plan 008 touches shared
control and CLI files and should run after its prerequisites are integrated.

## Step-by-step execution

1. **Plan 001, define the target state.** Add strict resource and Attachment
   types, deterministic digests, desired-state and receipt tables, and
   migration tests. Keep old callers through explicit temporary conversions.

2. **Plan 002, add the fast commit boundary.** Add typed get, plan, and apply
   RPCs. Make apply end at one SQLite commit plus a non-blocking reconcile
   wakeup. Add the bounded `WatchProgress` server stream. Prove no compiler or
   runtime call is reachable from apply.

3. **Plan 003, move provider work into daemon-owned reconciliation.** Require
   the Wasmtime cache, start bounded preparation at daemon startup, build only
   the latest serving generation, publish typed provider and serving progress,
   and keep the last good generation on failure. CLI commands still wait on
   the separate stream by default.

4. **Plan 004, extract runtime mechanics.** Move host, Docker, libkrun, and
   guest-image code to `omnifs-fs-runtime`. Remove UI, client path, and desired
   state inputs from those mechanics. Keep the CLI as the temporary caller.

5. **Plan 005, switch runtime ownership.** **DONE.** Add `AttachmentSupervisor`, move
   runtime paths to daemon state, switch VFS identity to Attachment plus
   runtime instance, remove client owner identity from runtime surfaces, and
   make current `fs` commands thin daemon RPC and progress-stream adapters.

6. **Plan 006, delete the former client owner and narrow bootstrap.** **DONE.** Replace
   `Bootstrap<R>` with `Profile`, pass an explicit root into state, resolve one
   daemon profile, add a read-only legacy scanner, and delete
   `client_fs_state.rs`, its registry, claims, paths, config, and tests.

7. **Plan 007, add KCL automation.** After every resource has a real progress
   publisher, evaluate KCL on the client, convert its result to strict Rust
   declarations, import exact local provider artifacts, and use the same plan,
   apply, and progress RPCs.

8. **Plan 008, finish public UX.** Add interactive Provider, Mount, Credential,
   and Attachment porcelain. Remove the transitional imperative `fs` grammar.
   Keep read commands and the narrow secret input scriptable.

9. **Plan 009, remove the old control plane.** Delete mutation leases,
   imperative wire operations, client owner and journal files, shared client
   file helpers, old active `fs::Spec` and `fs::Id`, compatibility adapters,
   stale docs, and old CI or dev paths. Run the full cold-cache and live
   lifecycle proof.

Each step has its own buildable state and STOP conditions. Do not merge a
deletion step before its replacement path passes the focused tests named in
that plan.

## Principal implementation checklist

### Desired state

- [ ] One strict `ResourceName` grammar
- [ ] Typed Provider, Credential, Mount, and Attachment resources
- [ ] One sorted, versioned resource-set digest
- [ ] One SQLite desired revision
- [ ] One atomic compare-and-swap apply
- [ ] Durable mutation-ID receipts
- [ ] Durable typed action receipts with one non-terminal action per target
- [ ] Action-generation preconditions prevent lost-reply duplicates
- [ ] No secret fields in resources, plans, receipts, or status

### Control

- [ ] `GetResources`
- [ ] `PlanResources`
- [ ] `ApplyResources`
- [ ] `WatchProgress` with desired revision, durable action, and current targets
- [ ] Credential actions and Attachment restart return correlated action IDs
- [ ] Request-only credential secret sidecars
- [ ] Apply returns after commit and non-blocking wakeup
- [ ] Progress subscription starts with a complete snapshot
- [ ] Slow subscribers resync without blocking reconcile workers
- [ ] No compile, generation, network, or runtime work in plan/apply
- [ ] Old lease and operation batch removed after cutover

### Providers and serving

- [ ] Required Wasmtime cache only
- [ ] One required-cache `ComponentEngine` is constructed per daemon
- [ ] Embedded preparation starts before SQLite opens
- [ ] Desired and retained providers join when SQLite becomes available
- [ ] `HostOnline` and preparation share the same engine
- [ ] Every embedded and retained digest queued
- [ ] Desired providers get priority
- [ ] Bounded workers and digest dedupe
- [ ] Prepared temporary components dropped
- [ ] Only active generation providers stay in memory
- [ ] Latest desired revision is the only one allowed to publish
- [ ] Last good generation remains active on failure
- [ ] Provider and generation phase changes feed the progress stream

### Attachments

- [ ] Daemon owns desired specs and lifecycle
- [ ] Filesystems stay out of process
- [ ] Low-level runtime drivers live outside CLI policy
- [ ] Internal live connections renamed `VfsSession`
- [ ] VFS v11 removes `ClientOwnerId`
- [ ] Per-attachment work serialized and globally bounded
- [ ] Deletion tombstone lasts until exact teardown
- [ ] `down` stops runtimes but preserves desired rows
- [ ] Restart restores desired attachments
- [ ] Image, runtime, mount, and VFS session changes feed the progress stream

### Bootstrap and client state

- [ ] `omnifs-bootstrap` owns only profile resolution, fixed control paths,
  spawn locking, and exact daemon identity
- [ ] `Bootstrap<R>`, `Client`, `Daemon`, and role-specific duplicate methods
  removed
- [ ] Daemon resolves one `Profile` for logging, state, and control
- [ ] One `DaemonStatePaths` comes from an explicit daemon-state root
- [ ] Early engine setup and later state open share that paths value
- [ ] `omnifs-state` has no bootstrap dependency
- [ ] `client_fs_state.rs`, active JSON registry, and per-ID claims removed
- [ ] Normal lifecycle creates no `client/filesystems` paths
- [ ] Metrics reads narrow profile config without constructing filesystem state
- [ ] Legacy client specs have one named read-only scanner
- [ ] Doctor holds the profile spawn lock and exact identity across legacy
  runtime repair
- [ ] Final cutover removes `client_state.rs`, `client_dir.rs`, owner ID, and
  mutation journal

### KCL and CLI

- [ ] KCL evaluator embedded through pinned Rust API
- [ ] No `kcl` subprocess
- [ ] No implicit remote package fetch
- [ ] Strict Rust types remain authoritative
- [ ] Local provider paths resolve only on the client
- [ ] `config init`, `config export`, `plan`, and `apply`
- [ ] Interactive provider, mount, credential, and attachment commands
- [ ] One shared plan, consent, and receipt path
- [ ] Human and JSONL mutations stream progress and wait by default
- [ ] JSON emits exactly one terminal envelope after waiting
- [ ] Non-TTY progress uses stable lines with no cursor control
- [ ] Ctrl-C detaches the viewer, restores the terminal, and reports that
  daemon work continues
- [ ] Status can resume current, revision, or durable action streams
- [ ] No fake Wasmtime percentage or cache-hit claim
- [ ] `attachment` replaces public `fs`
- [ ] Setup names active provider and attachment phases while it waits
- [ ] Read commands keep structured output
- [ ] Secret automation uses one narrow environment-input command

### Cutover

- [ ] Existing daemon mounts and credentials migrate deterministically
- [ ] Legacy detached client specs never auto-launch
- [ ] Legacy client specs are never edited or deleted by migration
- [ ] Client mutation journal removed
- [ ] Mutation lease and six imperative ops removed
- [ ] Current docs and `AGENTS.md` updated only after code lands
- [ ] `just check` passes
- [ ] Cold-cache, host, Docker, multi-filesystem, and supported libkrun lanes
  pass

## Core acceptance test

The most important end-to-end proof is:

1. Start a fresh daemon with an empty Wasmtime cache.
2. With a store-open test seam, prove embedded preparation starts first; then
   release state and prove retained providers join the same bounded queue.
3. Apply Provider, Mount, and Attachment resources.
4. At the protocol level, confirm `ApplyResources` returns inside the ordinary
   control deadline while provider status is still `Preparing`.
5. At the CLI level, confirm the command stays open on `WatchProgress`, names
   each active provider and stage, and ends only when the target revision is
   ready or failed.
6. Confirm JSONL emits typed progress records and one terminal result.
7. Confirm the attachment serves real files.
8. Run `down` and prove desired attachments remain.
9. Restart and prove the same cache is reused and attachments recover.

If this test cannot pass, the redesign has not solved the original problem.

## Considered and rejected

- Retain every compiled `Component` in a supervisor: rejected because memory
  grows with the full provider catalog. The durable cache holds prepared
  artifacts; only active mounts retain components.
- Let cache configuration remain optional: rejected. Production startup fails
  if the private Wasmtime cache cannot open.
- Run compilation inside `ApplyResources` so one RPC can wait: rejected because
  a transport deadline or disconnect would again hide the durable commit
  outcome. The CLI waits on a separate stream owned by the daemon.
- Keep client-owned filesystem specs and add daemon reconciliation beside them:
  rejected because it creates two lifecycle owners.
- Delete the bootstrap crate and copy its logic into CLI and daemon: rejected
  because the fixed socket, spawn lock, and exact process identity are a real
  shared pre-RPC contract. The generic role API is removed instead.
- Keep `ClientFilesystemState` as a profile path facade after specs move:
  rejected because it would preserve a false owner for config, caches, logs,
  runtime records, and defaults.
- Use KCL JSON as persisted or canonical desired state: rejected. It is an
  in-memory client interchange. Rust types and SQLite rows own the contract.
- Put secrets in KCL with environment or file references: rejected. Secret
  material stays outside resources and crosses only request-side local RPC.
- Add Kubernetes metadata, condition arrays, field managers, or Terraform state:
  rejected until local multi-owner use makes them necessary.
- Make every porcelain command support full non-interactive authoring flags:
  rejected because KCL is the automation surface. Read commands and the narrow
  secret input remain scriptable.
- Auto-convert every old detached filesystem spec to an Attachment: rejected
  because resource presence means desired attached and would start runtimes the
  user may have meant to keep stopped.
