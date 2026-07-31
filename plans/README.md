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

All numbered plans are complete. A post-cutover dry-down then removed the
temporary compatibility work that the staged rollout needed: the legacy
filesystem scanner, state backfills and incremental migrations, per-kind
desired tables, generated KCL config and schema output, and duplicate planning
and mount models. The numbered plans remain the record of the rollout gates;
`AGENTS.md`, `docs/contracts/`, and `docs/architecture/` describe the final
system.

## Execution order and status

| Plan | Title | Priority | Effort | Depends on | Status |
|---|---|---:|---:|---|---|
| [001](001-add-resource-domain-and-durable-state.md) | Add typed resource domain and durable state | P1 | L | none | DONE |
| [002](002-add-fast-plan-and-apply-rpc.md) | Add fast typed plan, apply, and progress RPCs | P1 | L | 001 | DONE |
| [003](003-move-provider-work-to-daemon-reconciliation.md) | Move provider work to daemon-owned reconciliation | P1 | L | 002 | DONE |
| [004](004-extract-filesystem-runtime-drivers.md) | Extract filesystem runtime drivers | P1 | L | 001 | DONE |
| [005](005-make-daemon-own-attachments.md) | Make the daemon own attachments | P1 | L | 003, 004 | DONE |
| [006](006-retire-client-filesystem-state-and-narrow-bootstrap.md) | Retire client filesystem state and narrow bootstrap | P1 | L | 005 | DONE |
| [007](007-add-kcl-plan-and-apply-client.md) | Add KCL plan and apply | P1 | L | 005 | DONE |
| [008](008-switch-to-interactive-resource-porcelain.md) | Switch to interactive resource porcelain | P1 | L | 005, 006, 007 | DONE |
| [009](009-remove-legacy-control-plane-and-finish-migration.md) | Remove the legacy control plane and finish migration | P1 | L | 003, 006, 007, 008 | DONE |

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
   types, deterministic digests, desired-state storage, and durable receipts.
   The final pre-alpha schema stores the complete desired set in one current
   row and carries no compatibility reader.

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
   daemon profile, and delete `client_fs_state.rs`, its registry, claims,
   paths, config, tests, and the temporary read-only scanner.

7. **Plan 007, add KCL automation.** **DONE.** After every resource has a real progress
   publisher, evaluate KCL on the client, convert its result to strict Rust
   declarations, import exact local provider artifacts, and use the same plan,
   apply, and progress RPCs.

8. **Plan 008, finish public UX.** **DONE.** Add interactive Provider, Mount, Credential,
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

- [x] One strict `ResourceName` grammar
- [x] Typed Provider, Credential, Mount, and Attachment resources
- [x] One sorted, versioned resource-set digest
- [x] One SQLite desired revision
- [x] One atomic compare-and-swap apply
- [x] Durable mutation-ID receipts
- [x] Durable typed action receipts with one non-terminal action per target
- [x] Action-generation preconditions prevent lost-reply duplicates
- [x] No secret fields in resources, plans, receipts, or status

### Control

- [x] `GetResources`
- [x] `PlanResources`
- [x] `ApplyResources`
- [x] `WatchProgress` with desired revision, durable action, and current targets
- [x] Credential actions and Attachment restart return correlated action IDs
- [x] Request-only credential secret sidecars
- [x] Apply returns after commit and non-blocking wakeup
- [x] Progress subscription starts with a complete snapshot
- [x] Slow subscribers resync without blocking reconcile workers
- [x] No compile, generation, network, or runtime work in plan/apply
- [x] Old lease and operation batch removed after cutover

### Providers and serving

- [x] Required Wasmtime cache only
- [x] One required-cache `ComponentEngine` is constructed per daemon
- [x] Embedded preparation starts before SQLite opens
- [x] Desired and retained providers join when SQLite becomes available
- [x] `HostOnline` and preparation share the same engine
- [x] Every embedded and retained digest queued
- [x] Desired providers get priority
- [x] Bounded workers and digest dedupe
- [x] Prepared temporary components dropped
- [x] Only active generation providers stay in memory
- [x] Latest desired revision is the only one allowed to publish
- [x] Last good generation remains active on failure
- [x] Provider and generation phase changes feed the progress stream

### Attachments

- [x] Daemon owns desired specs and lifecycle
- [x] Filesystems stay out of process
- [x] Low-level runtime drivers live outside CLI policy
- [x] Internal live connections renamed `VfsSession`
- [x] VFS v11 removes `ClientOwnerId`
- [x] Per-attachment work serialized and globally bounded
- [x] Deletion tombstone lasts until exact teardown
- [x] `down` stops runtimes but preserves desired rows
- [x] Restart restores desired attachments
- [x] Image, runtime, mount, and VFS session changes feed the progress stream

### Bootstrap and client state

- [x] `omnifs-bootstrap` owns only profile resolution, fixed control paths,
  spawn locking, and exact daemon identity
- [x] `Bootstrap<R>`, `Client`, `Daemon`, and role-specific duplicate methods
  removed
- [x] Daemon resolves one `Profile` for logging, state, and control
- [x] One `DaemonStatePaths` comes from an explicit daemon-state root
- [x] Early engine setup and later state open share that paths value
- [x] `omnifs-state` has no bootstrap dependency
- [x] `client_fs_state.rs`, active JSON registry, and per-ID claims removed
- [x] Normal lifecycle creates no `client/filesystems` paths
- [x] Metrics reads narrow profile config without constructing filesystem state
- [x] No legacy client desired-state reader remains
- [x] Doctor holds the profile spawn lock and exact identity across
  daemon-owned stray runtime repair
- [x] Final cutover removes `client_state.rs`, `client_dir.rs`, owner ID, and
  mutation journal

### KCL and CLI

- [x] KCL evaluator embedded through pinned Rust API
- [x] No `kcl` subprocess
- [x] No implicit remote package fetch
- [x] Strict Rust types remain authoritative
- [x] Local provider paths resolve only on the client
- [x] Directly authored KCL works with `plan` and `apply`
- [x] Interactive provider, mount, credential, and attachment commands
- [x] One shared plan, consent, and receipt path
- [x] Human and JSONL mutations stream progress and wait by default
- [x] JSON emits exactly one terminal envelope after waiting
- [x] Non-TTY progress uses stable lines with no cursor control
- [x] Ctrl-C detaches the viewer, restores the terminal, and reports that
  daemon work continues
- [x] Status can resume current, revision, or durable action streams
- [x] No fake Wasmtime percentage or cache-hit claim
- [x] `attachment` replaces public `fs`
- [x] Setup names active provider and attachment phases while it waits
- [x] Read commands keep structured output
- [x] Secret automation uses one narrow environment-input command

### Cutover

- [x] The current SQLite schema has no compatibility migrations or backfills
- [x] Client-owned detached specs have no production reader
- [x] Client mutation journal removed
- [x] Mutation lease and six imperative ops removed
- [x] Current docs and `AGENTS.md` updated only after code lands
- [x] `just check` passes
- [x] Cold-cache, host, Docker, multi-filesystem, and supported libkrun lanes
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
