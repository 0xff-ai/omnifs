# Plan 004: Extract filesystem runtime drivers from the CLI

> **Executor instructions**: Follow each step and gate. This is a behavior
> preserving extraction. Stop if a change starts moving desired-state policy
> or filesystem protocol logic. Update this plan's status in
> `plans/README.md` when done unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- Cargo.toml crates/omnifs-cli crates/omnifs-thin crates/omnifs-libkrun crates/omnifs-mtab`
> Review all filesystem lifecycle refactors since the planned commit. The
> driver code had active churn immediately before this plan.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-add-resource-domain-and-durable-state.md`
- **Category**: architecture, tech debt
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

The daemon cannot own attachment lifecycle while the host, Docker, and libkrun
drivers live inside `omnifs-cli` and depend on terminal output and
client-owned paths. The low-level runtime code also remains useful to doctor
when the daemon is stopped.

This plan extracts exact probe, launch, stop, and cleanup primitives into one
internal crate. It does not yet change the CLI as the lifecycle owner.

## Current state

- `crates/omnifs-cli/src/filesystem_driver.rs:121-141` is a closed enum over
  host, Docker, and libkrun drivers. Preserve this simple dispatch.
- `crates/omnifs-cli/src/filesystem_driver.rs:22-69` mixes launch inputs with
  `ClientFilesystemState`, `ClientOwnerId`, daemon attach endpoints, and
  terminal `Output`.
- `crates/omnifs-cli/src/client_fs_state.rs:14-141` owns specs, state, runtime
  paths, logs, image cache, and default mount locations under client state.
- `crates/omnifs-cli/src/host_fs.rs` owns exact host runner identity and control.
- `crates/omnifs-cli/src/docker/` owns exact container labels, commands, probe,
  launch, and stop.
- `crates/omnifs-cli/src/libkrun_runner.rs` owns image materialization, helper
  identity, control, seed, and ssh.
- `crates/omnifs-cli/src/guest_image_pull.rs` owns libkrun guest image fetch and
  verification.
- `crates/omnifs-cli/src/commands/fs.rs:478-513` launches and stops through the
  enum, then polls daemon attachment state.

Keep these safety rules:

- exact identity is rechecked before destructive actions
- teardown never signals a PID read from disk
- Docker exact command and labels are verified
- libkrun helper control remains fixed purpose
- guest lockdown checks fail closed
- all filesystems remain out of process

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Runtime unit tests | `cargo nextest run -p omnifs-fs-runtime` | all pass |
| CLI tests | `cargo nextest run -p omnifs-cli` | all pass |
| Thin tests | `cargo nextest run -p omnifs-thin` | all pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Docker live lane | run existing `filesystem_docker` acceptance when available | pass |
| Libkrun lane | `just libkrun-conformance` on opted-in Apple Silicon only | pass |

Do not treat the skipped libkrun CI lane as a pass. Preserve `sccache`.

## Scope

**In scope**:

- `Cargo.toml`
- new `crates/omnifs-fs-runtime/Cargo.toml`
- new `crates/omnifs-fs-runtime/src/lib.rs`
- new modules under `crates/omnifs-fs-runtime/src/`
- `crates/omnifs-cli/Cargo.toml`
- `crates/omnifs-cli/src/filesystem_driver.rs`
- `crates/omnifs-cli/src/host_fs.rs`
- `crates/omnifs-cli/src/docker/`
- `crates/omnifs-cli/src/libkrun_runner.rs`
- `crates/omnifs-cli/src/guest_image_pull.rs`
- `crates/omnifs-cli/src/image.rs`
- narrow helpers in `crates/omnifs-cli/src/process.rs`
- `crates/omnifs-cli/src/client_fs_state.rs`
- `crates/omnifs-cli/src/commands/fs.rs`
- tests that directly cover moved code

**Out of scope**:

- moving normal lifecycle ownership to the daemon
- renaming public `fs` commands
- VFS protocol changes
- changing runtime behavior, paths, images, or defaults
- changing NFS or FUSE protocol code
- changing libkrun device or authority policy
- changing Docker networking or mounts
- desired attachment reconciliation

## Target crate boundary

`omnifs-fs-runtime` owns:

- closed `RuntimeDriver` dispatch
- exact runtime identity types
- exact probe, launch, stop, and stale cleanup operations
- host runner operations
- Docker container operations
- libkrun helper operations
- guest image resolution and materialization
- typed runtime paths supplied by the caller
- typed lifecycle events

It does not own:

- desired resource definitions
- retries or backoff
- which runtime should exist
- daemon control RPC
- terminal rendering
- prompts or consent
- status table rendering
- provider or namespace state

The crate may depend on `omnifs-core`, `omnifs-thin`,
`omnifs-mtab`, `omnifs-libkrun`, Docker client crates, and shared utility
dependencies. It must not depend on `omnifs-cli` or `omnifs-daemon`.

## Steps

### Step 1: Define runtime inputs, paths, events, and errors

Create explicit types:

```text
RuntimePaths
AttachmentRuntimePaths
LaunchRequest
AttachEndpoints
RuntimeDriver
ConfirmedRuntime
RuntimeEvent
RuntimeError
```

The caller supplies all paths. The crate must not resolve `OMNIFS_HOME` or read
client or daemon config.

`RuntimeEvent` should cover the facts a daemon status owner or CLI renderer
needs:

- image reference resolution;
- layer or image download with real completed and total bytes when known;
- image verification and materialization;
- process, container, or VM start;
- OS mount wait and ready;
- VFS session wait and ready;
- stopping and stopped;
- typed failure with a stable stage.

Use a callback or bounded non-blocking sender supplied by the caller. Runtime
work must not wait for a slow event consumer. Do not put terminal strings,
colors, spinners, percentages, or `Output` in the crate. If an operation does
not expose a real total, emit a stage transition without a made-up total.

Use concrete enums, not one trait object per runtime. Keep the current closed
three-variant design.

**Verify**:
`cargo check -p omnifs-fs-runtime` exits zero with a minimal module skeleton.

### Step 2: Move shared identity helpers and host runtime code

Move:

- `ensure_record_matches`
- `ensure_identity_unchanged`
- rollback error composition
- `HostDriver`
- host runtime record scan and exact control

Adjust imports and path inputs without changing behavior. Move their unit tests
with the implementation.

The old CLI modules may temporarily re-export moved types to keep the diff
reviewable, but remove those re-exports by the end of this plan.

**Verify**:
run the moved host tests and current CLI host tests. Exact record mismatch,
busy stop, and corrupt sibling behavior must remain covered.

### Step 3: Move Docker runtime code

Move container naming, labels, command construction, lockdown validation,
exact inspection, launch, stop, and owned-container scan.

Keep current labels and command shape unchanged in this plan. Keep
`ClientOwnerId` until Plan 005 changes VFS identity.

Replace terminal progress with typed runtime events. The CLI adapter renders
those events through its current `Output`. Preserve exact image byte counts
from the source and the stable order of start, mount, and session events.

**Verify**:

- moved Docker unit tests pass
- exact command and label tests have unchanged expected values
- lockdown tests still reject binds and extra environment
- `cargo nextest run -p omnifs-cli` passes

### Step 4: Move libkrun and guest-image code

Move:

- image reference parsing
- OCI fetch and digest verification
- base image cache handling
- per-launch `root.raw` materialization
- seed construction
- helper configuration and exact identity
- launch rollback
- ssh access data and command construction
- owned-helper scan and stop

Do not widen the helper API. Do not change device, socket, firmware, dylib, or
network policy.

Keep private file permissions and immutable-base behavior.

**Verify**:
run all moved libkrun and guest-image unit tests. On Apple Silicon with the
opt-in dependencies, run the narrow existing conformance case.

### Step 5: Move the closed driver dispatch

Move `FilesystemDriver` into the new crate as `RuntimeDriver` or another
attachment-neutral internal name.

Its constructor takes a validated exact spec and caller-owned runtime paths.
Launch takes exact attach endpoints and event sink. Probe and stop take exact
identity inputs.

Keep one `match` on runtime in the constructor and dispatch through enum methods
elsewhere, matching the current pattern.

**Verify**:
add one unit test per runtime variant for dispatch and invalid input. Run
`cargo nextest run -p omnifs-fs-runtime`.

### Step 6: Adapt the CLI without behavior change

Make current `fs create|attach|detach|restart|rm|shell|ls` use the extracted
crate.

`ClientFilesystemState` still supplies the same client paths in this plan.
The CLI still owns runtime policy, waits for VFS attachment, and renders
progress.

Do not change command names, flags, receipts, or output snapshots.

Delete the moved implementation modules from `omnifs-cli` once all callers use
the new crate.

**Verify**:

```text
cargo nextest run -p omnifs-fs-runtime -p omnifs-cli
```

All existing CLI transcript snapshots must remain unchanged.

### Step 7: Check dependency direction and unused dependencies

Run:

```text
cargo tree -p omnifs-fs-runtime
```

Confirm it has no dependency on `omnifs-cli`, `omnifs-daemon`,
`omnifs-engine`, terminal UI crates, or KCL.

Remove direct dependencies from `omnifs-cli/Cargo.toml` when the moved code made
them unused. Do not leave both crates directly depending on a package used only
by the new runtime crate.

**Verify**:
`just check host` exits zero with no unused-dependency or clippy failures.

### Step 8: Run final gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-fs-runtime -p omnifs-cli -p omnifs-thin
just check host
just test host
git diff --check
```

When Docker is available, also run the existing
`crates/omnifs-itest/tests/filesystem_docker` acceptance lane.

## Test plan

Move tests with their owners. Do not delete a test merely because its module
moved.

Required retained proof:

- exact host record and control identity
- Docker label and flat command identity
- Docker lockdown
- guest image digest and layer validation
- libkrun private files and immutable base
- launch rollback behavior
- stale and corrupt record isolation
- CLI output unchanged

Add tests only for the new boundary:

- caller-supplied path use
- typed event order
- real byte progress and unknown-total behavior
- slow or dropped event receiver does not stop runtime work
- no UI dependency
- closed runtime dispatch

## Done criteria

- [ ] `omnifs-fs-runtime` owns all low-level host, Docker, and libkrun lifecycle
  code.
- [ ] The new crate resolves no profile root and reads no client config.
- [ ] The new crate has no terminal UI, daemon, engine, or KCL dependency.
- [ ] The current CLI still owns lifecycle and behaves the same.
- [ ] Current CLI transcript snapshots are unchanged.
- [ ] Exact identity and lockdown tests remain.
- [ ] Moved direct dependencies were removed from the CLI.
- [ ] All Step 8 commands pass.

## STOP conditions

Stop and report if:

- A runtime operation cannot be separated from terminal rendering without
  changing behavior.
- Exact doctor cleanup and normal lifecycle require different identity rules.
- Moving code requires a new provider, filesystem protocol, or VFS authority.
- The extraction starts making the runtime crate decide desired state, retry,
  or user consent.
- Libkrun helper policy must widen to fit the new boundary.
- Existing Docker or libkrun live behavior changes.
- A proposed shared crate would depend on both CLI and daemon.

## Maintenance notes

- Plan 005 moves policy and task ownership to the daemon. Keep this crate at
  the mechanism boundary.
- Runtime events are facts, not user-facing prose.
- If a fourth runtime appears, revisit the closed enum then. Do not add a plugin
  trait in advance.
- Doctor may use exact scan/cleanup operations, but it must not become a second
  normal lifecycle owner.

## Git workflow

- Use a branch such as `codex/004-fs-runtime`.
- Use Conventional Commits, for example
  `refactor(runtime): extract filesystem lifecycle drivers`.
- Do not push or open a pull request unless the operator asks.
