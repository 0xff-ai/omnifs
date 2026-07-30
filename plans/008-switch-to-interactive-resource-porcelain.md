# Plan 008: Switch public mutations to interactive resource porcelain

> **Executor instructions**: Follow the existing CLI output toolkit and
> transcript workflow. Do not add a second imperative mutation path for
> automation. Stop if a command cannot use the daemon resource planner.
> Update this plan's status in `plans/README.md` when done unless a reviewer
> owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- crates/omnifs-cli crates/omnifs-api crates/omnifs-daemon scripts/dev.ts README.md`
> Confirm Plans 002, 005, and 006 have landed. Read current transcript tests and
> command grammar before editing.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**:
  `plans/005-make-daemon-own-attachments.md`,
  `plans/006-retire-client-filesystem-state-and-narrow-bootstrap.md`,
  `plans/007-add-kcl-plan-and-apply-client.md`
- **Category**: feature, DX
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

Once KCL owns automation, one-resource commands can focus on the interactive
path. They should read the current desired set, make one typed edit, show the
daemon's real plan, and apply it. They must not keep separate flags and
mutation rules that drift from KCL.

This plan adds the final `provider`, `mount`, `credential`, and `attachment`
porcelain. Each mutation commits desired state, then waits on the daemon's
typed progress stream with detailed live status.

## Current state

- `crates/omnifs-cli/src/cli.rs:57-110` exposes mount, credential, setup, and
  `fs` commands, but no provider, plan, apply, config, or attachment command.
- `crates/omnifs-cli/src/commands/mount/` contains provider selection, auth,
  config, token validation, add, update, reauth, revoke, and remove flows.
- `crates/omnifs-cli/src/commands/setup.rs:224-359` offers no-sign-in mounts and
  a recommended filesystem.
- `crates/omnifs-cli/src/provider_resolver.rs` resolves embedded, local, and
  digest selectors through current provider import.
- `crates/omnifs-cli/src/capability.rs` renders provider needs and limits.
- `crates/omnifs-cli/src/ui/` owns prompts, consent, output, style, tables, and
  receipts.
- `crates/omnifs-cli/tests/cli_transcripts.rs` holds human transcript snapshots.
- `scripts/dev.ts:543-610` currently invokes flag-heavy `mount add`.
- `scripts/dev.ts:659-703` creates and attaches client-owned filesystem specs.

Keep the global output contract:

- JSON emits one terminal result or error envelope.
- JSONL emits events then one terminal result or error.
- progress and diagnostics do not pollute structured stdout.
- cancellation exits 130 and prints one settled cancellation.
- `--no-input` rejects prompts.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| CLI tests | `cargo nextest run -p omnifs-cli` | all pass |
| Daemon/API tests | `cargo nextest run -p omnifs-api -p omnifs-daemon` | all pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Docs | `just docs-check` | exit 0 |
| Live setup | `just dev -y` | reaches ready dev session |

Preserve `sccache`.

## Scope

**In scope**:

- `crates/omnifs-cli/src/cli.rs`
- `crates/omnifs-cli/src/main.rs`
- `crates/omnifs-cli/src/commands/mod.rs`
- `crates/omnifs-cli/src/commands/provider.rs` (new)
- `crates/omnifs-cli/src/commands/attachment.rs` (new)
- `crates/omnifs-cli/src/commands/mount/`
- `crates/omnifs-cli/src/commands/credential.rs`
- `crates/omnifs-cli/src/commands/setup.rs`
- `crates/omnifs-cli/src/commands/status.rs`
- `crates/omnifs-cli/src/inventory.rs`
- `crates/omnifs-cli/src/status.rs`
- `crates/omnifs-cli/src/provider_catalog.rs`
- `crates/omnifs-cli/src/provider_resolver.rs`
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-cli/src/ui/`
- CLI contract, transcript, and lifecycle tests
- `scripts/dev.ts`
- `contrib/dev-profiles/` only if the dev input format must change
- focused README command examples

**Out of scope**:

- deleting old RPC and storage code
- changing KCL evaluation
- new provider source types beyond embedded and local Wasm
- OCI, URL, or Git provider imports
- secret values in config or output
- a full-screen TUI
- a durable progress event log or resumable event cursor

## Shared mutation helper

Create one CLI helper used by every porcelain command:

```text
edit_resources_and_wait(edit_fn, optional_secret_sidecars)
```

Flow:

1. read current resources
2. apply one typed edit in memory
3. call daemon plan
4. render the scoped diff
5. ask for consent
6. apply exact base revision and digest
7. render or retain the durable commit receipt
8. open `WatchProgress` for the returned desired revision
9. render typed events until ready, failed, or superseded

The helper does not own resource validation. It only builds typed input and
uses daemon responses.

If state changes after the prompt, apply fails stale. Do not auto-retry after
consent.

The raw RPC still returns before provider, serving, or attachment work. The
command waits through the separate stream. A disconnect or Ctrl-C never
cancels daemon work.

## Steps

### Step 1: Add one shared plan, consent, and progress surface

Reuse the plan renderer from `omnifs plan`. Add a scoped mode that highlights
the resource keys changed by one porcelain command while still showing any
required dependent edits.

Output distinguishes:

- desired state committed;
- unchanged but still reconciling;
- active provider, serving, image, runtime, mount, and VFS session stages;
- terminal ready, failed, or superseded revision;
- operational action accepted and its later terminal outcome.

It never says ready, mounted, compiled, attached, or serving unless a progress
snapshot or event proves that fact.

Use one renderer for KCL apply and porcelain:

- TTY human output may use a bounded transient region with one row per active
  operation, capped by one small constant, and keeps completed items as stable
  lines through the existing progress channel;
- non-TTY human output prints one stable line per phase change and uses no
  cursor control;
- JSONL emits each typed event, then one terminal result or error;
- JSON waits silently on stdout and emits exactly one terminal envelope;
- quiet waits and emits only the terminal receipt.

Only typed JSONL events use structured stdout during the stream. Human progress
and diagnostics never enter JSON or JSONL stdout.

Do not invent percentages. Use real queue counters and byte totals when known.
Show `preparing component` as indeterminate. Summarize active work beyond the
visible row cap by count rather than growing an unbounded terminal region.
The TTY may show elapsed time for an active stage, but elapsed time must not
imply percent complete, a cache hit, or health.

Ctrl-C after commit restores the terminal, exits 130, and prints the committed
revision plus its exact `status --follow` command. A stream error does the same
without making the durable apply outcome unknown.

For credential or restart actions, use
`omnifs status --follow --action <id>`.

JSONL ends cancellation or stream failure with one typed terminal envelope
after prior events. JSON emits only that envelope. Each carries the durable
receipt, `committed: true`, target, follow hint, and stable outcome code.

Interactive mutation commands require a TTY. Under `--no-input` or redirected
input, return a clear error with:

```text
Use `omnifs plan <file>` and `omnifs apply <file> --yes` for automation.
```

The narrow credential secret command remains non-interactive.

**Verify**:
transcript tests cover TTY, no color, cancel, decline, stale state, and
non-interactive refusal.

### Step 2: Add `omnifs provider`

Commands:

```text
omnifs provider add
omnifs provider ls
omnifs provider show <name>
omnifs provider rm <name>
```

`provider add` wizard:

1. choose embedded provider or local Wasm
2. for local Wasm, ask for a path and compute the exact digest
3. import through the existing bounded idempotent RPC
4. display metadata, auth needs, network domains, host resources, and limits
5. state that import grants no authority
6. create a named Provider resource pinned to the digest
7. show the daemon resource plan
8. ask for consent
9. apply and stream provider preparation to its terminal revision result
10. offer to continue into `mount add` without requiring it

Support embedded and local Wasm only. Do not accept a raw URL, Git repository,
or OCI reference.

`provider rm` refuses a plan with remaining mount or credential references. It
removes the named resource, not the retained content-addressed artifact.

Read commands support human and structured output and never prompt.

**Verify**:
tests cover embedded, local, digest mismatch, unchanged artifact, metadata
display, consent, referenced removal, and no authority claim.

### Step 3: Make mount mutations pure porcelain

Final commands:

```text
omnifs mount add
omnifs mount update <name>
omnifs mount reauth <name>
omnifs mount revoke <name>
omnifs mount rm <name>
omnifs mount ls
omnifs mount show <name>
```

`mount add` wizard:

1. select a Provider resource
2. collect provider config from metadata
3. show and resolve host resource fields
4. collect limits
5. if auth is needed, select or create a Credential resource
6. run OAuth or token collection before the plan prompt
7. add credential secret material only as a request sidecar
8. plan the complete desired set
9. show provider grants and resource diff
10. apply and stream the required provider, serving, and mount phases

Remove flag-driven authoring options once KCL plan/apply covers their use. Keep
only selectors needed by read and operational commands.

`reauth` changes secret material and wakes reconcile. `revoke` is explicit
upstream revoke and leaves the declared slot in `NeedsSecret` unless the user
also removes it.

**Verify**:
tests cover no-auth, static token, OAuth, provider config, host fields, existing
credential, new credential sidecar, revoke, remove, cancel, and stale plan.

### Step 4: Finalize credential commands

Commands:

```text
omnifs credential login
omnifs credential set <name> --from-env <variable>
omnifs credential ls
omnifs credential show <name>
omnifs credential rm <name>
omnifs credential revoke <name>
```

Rules:

- `login`, `rm`, and `revoke` are interactive porcelain
- `set --from-env` is the one secret automation path
- no command accepts a token literal flag
- environment variable names may appear in errors; values never do
- removing a resource refuses while Mount references remain, or clears those
  references in the same reviewed resource plan
- removing a resource schedules local material deletion after generation drain
- revoke never implies success if upstream outcome is unknown

Structured output reports non-secret identity, phase, scopes, and outcome only.
`login`, `set`, and `revoke` follow the action ID in their credential receipt
through generation refresh, drain, and any explicit upstream work. They use the
same output and Ctrl-C rules as revision watches.

**Verify**:
secret redaction tests cover Debug, error, human output, JSON, JSONL, logs, and
metrics. Action-stream tests cover login, environment material set, revoke,
success, stable failure, and Ctrl-C.

### Step 5: Replace public `fs` with `attachment`

Final commands:

```text
omnifs attachment add
omnifs attachment ls
omnifs attachment show <name>
omnifs attachment rm <name>
omnifs attachment restart <name>
omnifs attachment shell <name> [-- <argv>...]
```

`attachment add` wizard:

1. show supported platform protocol/runtime pairs
2. choose the recommended pair by default
3. ask only for values the daemon cannot infer
4. plan creation
5. explain that resource presence means desired attached
6. apply and wait through the Attachment lifecycle stream

There is no public `attach` or `detach` verb. Adding the resource requests
attachment; removing it requests teardown.

`restart` follows its returned action ID through stop, start, mount, and
session phases. `shell` uses the access RPC. `rm` follows the desired revision
until exact teardown clears its deletion tombstone or reaches a stable failure.

Delete the public `fs` command and transitional aliases. This repo has no
backward-compatibility obligation.

**Verify**:
CLI contract tests prove the final grammar and that `fs`, `attach`, and
`detach` are not public commands.

### Step 6: Rewrite setup around resources

`omnifs setup` keeps its boot-and-orient flow:

1. start daemon
2. show every embedded provider with honest auth/config labels
3. offer no-sign-in providers
4. import artifacts and create Provider and Mount resources in one desired set
5. offer the platform's recommended Attachment
6. plan the complete set once
7. ask for consent
8. apply once
9. follow that desired revision
10. name each required provider as it prepares
11. report namespace build and publication
12. report Attachment image, runtime, mount, and session stages
13. return only after the revision is ready, failed, or superseded

The `ApplyResources` call itself must not wait for any of those stages. Setup
waits through `WatchProgress`, which has no unary work deadline. Unused catalog
provider warm-up does not block setup's revision.

If an earlier interrupted import or resource change already exists, the plan
shows unchanged state. It must not print the old
"an earlier interrupted command's change..." mutation-journal recovery line.
If the unchanged current revision is still reconciling, setup follows its
remaining work. If it is already ready, the initial snapshot completes the
command at once.

On success, closing output states the ready revision. On stable failure, setup
exits nonzero, states that desired state remains applied, and points to
`omnifs status --follow --revision <n>`.

**Verify**:
add a cold-compiler setup test showing the apply RPC returns inside its normal
deadline, setup stays open, the stream names the compiling provider, and setup
ends only after a terminal revision event. Add failure, Ctrl-C, JSONL, JSON,
non-TTY, unchanged-ready, unchanged-reconciling, and unused-catalog-provider
cases.

### Step 7: Update status and inventory language

Status shows resources and their phases. Use `Attachment` for desired OS
exposure and `VFS session` only in doctor or deep diagnostics.

For each resource show:

- desired revision
- observed revision
- phase
- concise error when failed

Keep wide tables under the existing `tabled` and inventory rendering
conventions. Preserve structured plural arrays.

Add:

```text
omnifs status --follow
omnifs status --follow --revision <revision>
omnifs status --follow --action <action-id>
```

The first watches current work until canceled. Revision and action watches end
at their typed terminal result. Make `--revision` and `--action` mutually
exclusive. An action watch resumes from durable current action state after a
daemon restart.

**Verify**:
transcripts cover empty, preparing, ready, failed, deleting, current follow,
revision follow, action follow, and a pending action resumed by a replacement
daemon.

### Step 8: Move contributor automation to KCL

Rewrite `scripts/dev.ts` to:

1. build providers and the CLI
2. render one temporary or profile-local KCL desired config
3. use `omnifs apply <path> --yes`
4. set any dev credential through `credential set --from-env`
5. rely on apply's terminal revision result before shell access
6. call `attachment shell dev-docker` or the typed access flow
7. run `down` for teardown

Do not invoke interactive provider, mount, credential, or attachment porcelain
from the script.

Keep fixture startup before apply for host resource paths and sockets.

**Verify**:
`just dev -y --no-shell` or the current equivalent reaches ready state on a
supported host.

### Step 9: Run final gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-api -p omnifs-daemon -p omnifs-cli
just check host
just test host
just docs-check
git diff --check
```

Then run:

```text
just dev -y
target/debug/omnifs status
```

Exercise one host and one Docker attachment on a supported machine.

## Test plan

Use semantic tests for edits and transcript tests for output. Required
transcripts:

- provider add
- mount add with and without auth
- credential secret automation
- attachment add and remove
- setup with cold daemon-owned preparation
- no-input refusal
- plan decline and cancel
- durable commit receipt followed by streamed phases
- stable reconcile failure after a successful commit
- Ctrl-C detach after commit
- status phases

Run transcripts with color and with ANSI disabled. Verify structured stdout has
one terminal envelope for JSON and typed progress plus one terminal envelope
for JSONL.

## Done criteria

- [ ] All interactive mutations use one shared resource edit, plan, consent,
  and apply path.
- [ ] Automation uses KCL plan/apply, not hidden authoring flags.
- [ ] Provider add supports embedded and local Wasm and grants no authority.
- [ ] No CLI accepts a token literal flag.
- [ ] `attachment` replaces public `fs`.
- [ ] Resource presence, not an `attached` boolean, controls lifecycle.
- [ ] Raw mutation RPCs never wait for compilation or Attachment work.
- [ ] Human and JSONL mutation commands stream detailed progress and wait by
  default.
- [ ] JSON and quiet wait without incremental output.
- [ ] Setup applies once, then waits on its revision stream to a terminal
  result.
- [ ] Status shows desired and observed phases.
- [ ] `scripts/dev.ts` uses KCL apply.
- [ ] All Step 9 and supported live commands pass.

## STOP conditions

Stop and report if:

- A porcelain command needs validation or diff logic not available through the
  daemon planner.
- A non-interactive mutation cannot be expressed through KCL.
- A command cannot identify one desired revision or action whose terminal
  state it can watch.
- Secret automation would require a token literal in argv or KCL.
- Removing `fs` would leave an internal hidden runner command without a safe
  replacement. Hidden `run-fs` itself remains valid.
- A live script needs to read daemon SQLite or runtime files directly.

## Maintenance notes

- Keep interactive commands small. When an option becomes complex, add it to
  the resource model and KCL, not a private imperative flag.
- Read commands remain automation-friendly even though mutation porcelain is
  interactive.
- Keep the commit receipt distinct from streamed readiness. A progress failure
  does not roll back or obscure the committed desired revision.
- Plan 009 removes the old lease, journal, tables, and stale docs.

## Git workflow

- Use a branch such as `codex/008-resource-porcelain`.
- Use Conventional Commits, for example
  `feat(cli): add interactive resource commands`.
- Do not push or open a pull request unless the operator asks.
