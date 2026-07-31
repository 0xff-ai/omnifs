# Plan 007: Add the KCL plan and apply client

> **Executor instructions**: Start with the feasibility gate. Do not add KCL
> through a subprocess or substitute another language if the named Rust API is
> blocked. Stop and report under the conditions below. Update this plan's
> status in `plans/README.md` when done unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 035952bc7..HEAD -- Cargo.toml Cargo.lock crates/omnifs-api crates/omnifs-cli npm .github/workflows`
> Confirm Plans 001 through 005 have landed. Use the final Attachment resource
> shape and the real provider, serving, credential, and Attachment progress
> publishers.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/005-make-daemon-own-attachments.md`
- **Category**: feature, DX, dependency
- **Planned at**: commit `035952bc7`, 2026-07-30

## Why this matters

Interactive mutation commands should stay short and guided. Automation needs a
different entry point that can express the complete desired set, show drift,
and apply it without duplicating daemon policy.

KCL is a client authoring language only. This plan embeds the official Rust
evaluator, converts its result to strict Rust resource declarations, and calls
the same daemon plan/apply API used by interactive commands.

## Current state

- The workspace uses Rust 1.95.0 and edition 2024.
- `crates/omnifs-cli/src/cli.rs` owns top-level command grammar and dispatch.
- `crates/omnifs-cli/src/rpc.rs` owns typed local control calls.
- `crates/omnifs-cli/src/ui/output.rs` owns output channels and structured
  envelopes.
- `crates/omnifs-cli/tests/cli_transcripts.rs` and
  `crates/omnifs-cli/tests/cli_contract.rs` own CLI output and grammar tests.
- Plan 002 adds `GetResources`, `PlanResources`, and `ApplyResources`.
- Provider upload already streams exact bytes, verifies a caller digest, and is
  idempotent.

Current official KCL 0.12 docs show an in-process Rust API:

```rust
let api = kcl_lang::API::default();
let result = api.exec_program(&ExecProgramArgs {
    k_filename_list: vec!["main.k".to_string()],
    ..Default::default()
})?;
```

References:

- <https://github.com/kcl-lang/kcl-lang.io/blob/main/versioned_docs/version-0.12/reference/xlang-api/rust-api.md>
- <https://github.com/kcl-lang/kcl>
- <https://github.com/kcl-lang/kcl-lang.io/blob/main/versioned_docs/version-0.12/user_docs/guides/package-management/4-how-to/8-kcl_mod.md>

The docs show Git, OCI, and local package dependencies. Omnifs must not fetch
remote dependencies during plan or apply.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| KCL crate tests | `cargo nextest run -p omnifs-kcl` | all pass |
| CLI tests | `cargo nextest run -p omnifs-cli` | all pass |
| Host check | `just check host` | exit 0 |
| Host tests | `just test host` | exit 0 |
| Package builds | use current Linux and Darwin release build commands from CI | all target builds pass |

Network access may be needed once to fetch the pinned KCL Git revision. Request
scoped approval if the sandbox blocks it. Do not replace KCL or vendor an
unreviewed copy to bypass access. Preserve `sccache`.

## Scope

**In scope**:

- `Cargo.toml`
- `Cargo.lock`
- new `crates/omnifs-kcl/Cargo.toml`
- new `crates/omnifs-kcl/src/lib.rs`
- new `crates/omnifs-kcl/src/evaluator.rs`
- new `crates/omnifs-kcl/src/source.rs`
- new `crates/omnifs-kcl/assets/omnifs.k`
- `crates/omnifs-cli/Cargo.toml`
- `crates/omnifs-cli/src/cli.rs`
- `crates/omnifs-cli/src/main.rs`
- `crates/omnifs-cli/src/commands/mod.rs`
- new `crates/omnifs-cli/src/commands/config.rs`
- new `crates/omnifs-cli/src/commands/plan.rs`
- new `crates/omnifs-cli/src/commands/apply.rs`
- `crates/omnifs-cli/src/provider_resolver.rs`
- `crates/omnifs-cli/src/rpc.rs`
- `crates/omnifs-cli/src/ui/`
- CLI tests and snapshots
- release/package manifests only as needed for the embedded library

**Out of scope**:

- server-side KCL
- invoking a `kcl` subprocess
- remote KCL URLs
- implicit Git or OCI package fetch
- secrets in KCL
- saved plan files
- partial apply or targets
- resource schema code generation
- changing daemon validation rules
- changing provider authority

## Input contract

The default file is `omnifs.k` when no path is supplied and that file exists in
the current directory. Otherwise require an explicit path.

The evaluated result contains one root `config` value:

```text
config
  apiVersion
  resources
```

`omnifs config init` and `config export` emit:

```kcl
import omnifs

config = omnifs.Config {
    apiVersion = "omnifs.dev/v1alpha1"
    resources = [...]
}
```

The KCL schema improves editor and evaluator feedback. Strict Rust parsing and
daemon validation remain authoritative.

## Steps

### Step 1: Run a contained KCL feasibility spike

Pin one exact KCL Git commit in the workspace. Do not use a moving branch.

In a temporary or new private crate, prove:

- `API::exec_program` evaluates one file in process
- the result exposes JSON output
- source filename and line/column errors are available
- evaluation can run inside `spawn_blocking`
- the built-in `omnifs` KCL schema can be supplied without a system `kcl`
  install
- local imports can be rooted at the input package
- missing remote dependencies fail without an automatic fetch
- the CLI release binary builds for the repo's Linux and Darwin targets
- no unplanned dynamic library is required at runtime
- KCL's license is compatible with the workspace distribution

Record the exact commit and why it was selected in code comments or the crate
README only if the repo normally tracks such dependency notes.

If the built-in package cannot be injected cleanly, choose the documented
fallback: accept a plain root KCL object and rely on strict Rust parsing. Do not
build a custom KCL package manager.

**Verify**:
one small `omnifs-kcl` test evaluates a fixture and returns a typed resource
declaration.

### Step 2: Build a private evaluator adapter

Create `omnifs-kcl` as the only crate that imports KCL internals.

API:

```text
evaluate(path, options) -> EvaluatedConfig
render_config(normalized_resources) -> String
```

`EvaluatedConfig` contains:

- strict authoring resource declarations
- client-only provider source declarations
- source path for diagnostics

It does not expose raw KCL runtime values to the CLI.

Evaluation rules:

- canonicalize the input file and work directory
- run on `spawn_blocking`
- bound source size and JSON result size before parsing
- parse JSON into `#[serde(deny_unknown_fields)]` authoring types
- preserve KCL source locations in errors when available
- never include full source text in a daemon error or metric
- never evaluate a URL
- never invoke a package download command

Treat KCL and local imports as trusted user-authored code. Do not claim a
sandbox.

**Verify**:
fixtures cover valid config, syntax error, schema error, Rust unknown field,
duplicate resource, missing file, oversized result, local import, and rejected
URL.

### Step 3: Add the embedded Omnifs KCL schema

Define KCL schemas for:

- root config
- Provider authoring source
- Credential
- Mount
- Attachment

Keep provider config open as a dictionary because provider metadata owns its
schema.

Do not generate Rust types from this file. Add paired fixtures that prove the
schema and Rust types agree on every example and rejection that both can
express.

Provide the schema through an embedded package or the feasibility fallback.
Do not require package files beside the installed binary unless current npm and
archive packaging can include and locate them on every target.

**Verify**:
`cargo nextest run -p omnifs-kcl` passes schema fixtures.

### Step 4: Resolve provider authoring sources

Add client-only source variants:

```text
embedded { name }
local { path, expected_digest }
digest { provider_id }
```

Resolution:

- `embedded` uses the exact embedded provider import RPC
- `local` resolves relative to the KCL file, hashes bytes, requires the declared
  digest, and uses the existing bounded streaming import
- `digest` requires the artifact already be retained

Import is allowed before planning because it is content-addressed, idempotent,
inert, and grants no authority. It must not wait for daemon provider
preparation.

If a later plan or apply fails, leave the unreferenced artifact retained. Do
not add garbage collection here.

The normalized resource set sent to the daemon contains only provider digests.
Never send or store local source paths.

**Verify**:
tests cover digest mismatch, changed file between hash and upload, embedded
import, already-retained digest, repaired artifact, and plan failure after
import.

### Step 5: Add `omnifs plan`

Command:

```text
omnifs plan [path]
```

Flow:

1. start the daemon if needed
2. evaluate KCL once
3. resolve/import provider artifacts
4. call `PlanResources`
5. render create/update/delete rows and counts
6. exit zero for a valid plan, including an empty plan

Plan never asks for consent and never applies desired state.

Human output marks destructive credential and attachment deletion. Color is
optional. JSON and JSONL use versioned structured result envelopes with base
revision, digest, changes, warnings, and counts.

Progress and diagnostics go to stderr. Primary structured data goes to stdout.

**Verify**:
CLI tests cover TTY, no color, redirected output, JSON, empty diff, errors, and
no prompt.

### Step 6: Add `omnifs apply`

Command:

```text
omnifs apply [path]
```

Flow:

1. evaluate KCL once
2. resolve/import provider artifacts
3. call `PlanResources`
4. show the exact plan
5. ask for confirmation on an interactive TTY
6. in non-interactive use, require explicit `--yes`
7. call `ApplyResources` with the same declarations, base revision, digest, and
   a fresh mutation ID
8. print or retain the durable receipt
9. call `WatchProgress` for the receipt's desired revision
10. wait for `RevisionReady`, `RevisionFailed`, or `RevisionSuperseded`

Do not evaluate the KCL file a second time after consent.

If apply reports a stale revision, do not auto-replan after consent. Tell the
user to review a new plan.

Human output starts with the durable commit, then renders server events:

```text
✓ desired  revision <n> committed
⟳ provider <name>  preparing component (<completed>/<total>)
⟳ serving          building generation
✓ revision <n> ready in <duration>
```

The first line is true after apply. Later lines come only from the progress
stream. Never claim ready from the apply receipt.

Output rules:

- human waits and renders detailed progress through the existing progress
  channel, with commit and terminal receipts on the result channel;
- JSONL emits versioned progress events followed by one terminal result or
  error;
- JSON waits without incremental stdout and emits exactly one terminal
  envelope containing the final snapshot;
- quiet waits and emits only the terminal receipt;
- redirected human output uses stable lines and no cursor control.

Only typed JSONL events use structured stdout during the stream. Human progress
and diagnostics must not leak into JSON or JSONL stdout.

Ctrl-C after commit stops only the client watch, exits 130, and prints the
committed revision plus `omnifs status --follow --revision <n>`. Daemon work
continues. If stream setup or transport fails, report the same durable commit
and follow command. A stable reconcile failure exits nonzero and says the
desired state remains applied.

For JSONL, cancellation or stream failure is the one terminal typed envelope
after prior events. For JSON, it is the only envelope. Both include the durable
receipt and follow target without unstructured progress text. The receipt owns
the committed revision, while the terminal envelope owns the stable outcome.

An unchanged apply follows the current revision if it is still reconciling and
finishes from the first snapshot if it is already ready.

**Verify**:
transcript tests cover consent yes/no/cancel, non-TTY without `--yes`, stale
state, changed local provider bytes, unchanged ready and reconciling apply,
human progress, JSONL event order, one-envelope JSON, redirected output,
stream reconnect guidance, structured cancellation, stable failure,
superseded revision, and Ctrl-C detach behavior.

### Step 7: Add `config init` and `config export`

Commands:

```text
omnifs config init
omnifs config export --format kcl
```

Both write KCL source to stdout and diagnostics to stderr.

`init` prints a minimal commented config with no resources.

`export` calls `GetResources` and renders deterministic KCL:

- resources sorted by kind and name
- exact provider digests
- exact normalized attachment values
- provider config values escaped safely
- no secret values or secret source hints

Round-trip every export through the evaluator and compare normalized typed
resources.

Do not write a file unless a later explicit command asks for a destination.
Shell redirection remains the simple path.

**Verify**:
export round-trip tests include all resource variants and awkward strings.

### Step 8: Check packaging and dependency cost

Run the repo's actual release build surfaces for:

- Linux x64
- Linux arm64 where supported
- Darwin x64 cross-build
- Darwin arm64 native build

Confirm the installed binary needs no separate `kcl` program and no unshipped
KCL dynamic library or data directory.

Record the binary size change in the implementation summary. Do not set an
arbitrary pass/fail threshold unless the repo has one.

**Verify**:
all selected package build commands exit zero and the built binary evaluates a
small KCL file in a clean temp profile.

### Step 9: Run final gates

Run:

```text
cargo fmt --all -- --check
cargo nextest run -p omnifs-kcl -p omnifs-api -p omnifs-cli
just check host
just test host
git diff --check
```

All commands must exit zero.

## Test plan

Use current CLI output and transcript infrastructure. Add KCL fixtures under
the new crate, not in user docs.

Required cases:

- valid expressions and comprehensions
- syntax and type errors with locations
- unknown resource fields
- result size bound
- local-only import behavior
- no implicit remote fetch
- provider source resolution
- export round trip
- plan output in all output modes
- apply consent and stale state
- apply waits on typed revision progress after commit
- human, JSONL, JSON, quiet, and redirected progress contracts
- stream failure and Ctrl-C preserve the known commit outcome
- source evaluated once
- secret-free configs and output

## Done criteria

- [ ] KCL evaluates in process through one private Rust adapter.
- [ ] The dependency is pinned to an exact commit.
- [ ] No system `kcl` program is required.
- [ ] Plan and apply never evaluate a URL or fetch a remote KCL package.
- [ ] KCL JSON is internal and is neither persisted nor used as the desired
  digest.
- [ ] Rust strict resource types remain authoritative.
- [ ] Local provider paths resolve client-side and never enter daemon state.
- [ ] `plan` never changes desired state.
- [ ] `apply` evaluates once and uses daemon plan/apply.
- [ ] `apply` follows its desired revision and waits by default.
- [ ] Structured modes preserve the global output contract while waiting.
- [ ] `config export` contains no secrets and round-trips.
- [ ] Release targets build and run without missing KCL runtime files.
- [ ] All Step 9 commands pass.

## STOP conditions

Stop and report if:

- The current KCL Rust API cannot return JSON without invoking a subprocess.
- KCL cannot build for one of the repo's release targets.
- The evaluator requires an unshipped dynamic library or tool at runtime.
- KCL's license is not compatible with the distribution.
- Remote dependency resolution occurs implicitly and cannot be disabled or
  prevented.
- A built-in schema package requires a custom package manager or broad runtime
  file extraction.
- Provider local paths would need to enter daemon desired state.
- KCL needs access to credential material.
- The implementation would duplicate daemon normalization or planning policy
  in the client.

## Maintenance notes

- Keep all KCL imports behind `omnifs-kcl`; its Rust API is an external seam.
- A future KCL upgrade must rerun release-target and export-round-trip tests.
- Do not add generated Rust/KCL schemas until hand maintenance causes measured
  drift.
- Remote modules and OCI provider sources remain separate design decisions.

## Git workflow

- Use a branch such as `codex/007-kcl-client`.
- Use Conventional Commits, for example
  `feat(cli): add KCL plan and apply`.
- Do not push or open a pull request unless the operator asks.
