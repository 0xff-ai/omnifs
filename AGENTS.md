# AGENTS.md

Repository guide for agents and contributors working on `omnifs`.
`CLAUDE.md` is a symlink to this file, so keep it self-contained.

## Start here

`omnifs` projects external services as native filesystems. Providers own
meaning: what paths exist and what bytes they hold. The host owns trust,
authorization, callouts, caching, and I/O. FUSE and NFS translate one shared
projected namespace into OS protocol behavior.

Before changing code:

1. Read `docs/contracts/00-index.md`, then the one contract for the area.
2. Read `docs/architecture/00-overview.md` only when you need system rationale;
   it routes to focused architecture notes.
3. Trace the production call path and its tests before deciding.

Contracts are binding. Architecture notes explain the current design and
record rejected prior designs. Source code is the final check on current shape.
If code and a contract disagree, resolve the conflict and update the contract
in the same change.

## Rule tiers

- **Invariant:** breaking it is wrong. Stop if the task appears to require it.
- **Gated decision:** surface the tradeoff and get explicit approval first.
- **Direction:** follow it unless the code gives a concrete reason not to.
- **Footgun:** a current trap. Remove the note when its condition disappears.

## Universal invariants

- The host owns trust. Providers are untrusted, including embedded providers.
- Providers and the SDK own object identity, canonical assembly, rendering,
  versioning, preload, revalidation, and route topology.
- The host knows paths, bytes, attributes, cache facts, capabilities, and
  effects. It never gains provider-specific object meaning.
- `omnifs-engine` owns shared projection semantics. Filesystems consume only
  the narrow namespace surface and own protocol state, not projection policy.
- Host caching is opaque byte and fact storage. Providers do not add private
  LRUs or expiration policy.
- SQLite is the sole desired-state authority for Provider, Credential, Mount,
  and Attachment resources. The CLI has no desired-state journal.
- `ApplyResources` ends after validation, one SQLite transaction, and a
  non-blocking reconcile wakeup. Runtime work happens in daemon workers.
- The daemon owns provider preparation, namespace publication, Attachment
  lifecycle, and live VFS sessions. Filesystem processes stay out of process.
- Credentials and other secret bytes never enter resources, KCL, status,
  progress, receipts, logs, Debug output, Inspector, or dedupe hashes.
- A declaration must bind behavior. Permissions, capabilities, schema rules,
  cache contracts, and validation claims must feed an enforced decision.

## Gated decisions

Get explicit approval before changing:

- Provider WASM authority, including callout families, preopens, process
  effects, socket effects, or broader network access.
- Authentication or transport models.
- Existing strict `deny_unknown_fields` parsing.
- A technology, library, or architecture named by the task.

## Work rules

- Diagnose root causes before changing code. Preserve the original failure
  signal until you understand it.
- Do not weaken tests, fixtures, coverage, or strict parsing to make a failure
  disappear.
- Keep one fact under one owner. Delete duplicate DTOs, compatibility aliases,
  bridge layers, and one-caller forwarding helpers when the direct path exists.
- This project is pre-alpha and has no backward-compatibility obligation.
  Delete obsolete readers, migrations, wire fields, aliases, and APIs unless a
  current interoperability requirement says otherwise.
- Add an abstraction only for two real callers or one volatile external
  boundary. Prefer parsed domain types over strings, maps, or raw JSON.
- Public APIs need current callers and enforced invariants.
- Dependencies must pay for themselves. Remove direct dependencies when their
  final use disappears.
- Preserve user changes in dirty worktrees. Do not use destructive Git
  commands or rewrite history without explicit approval.
- Use Conventional Commits when asked to commit. Do not push or open a pull
  request without explicit approval.

## Orientation

- `omnifs-core`: shared identities, paths, and filesystem primitives.
- `omnifs-api`: resource domain and typed local control protocol.
- `omnifs-bootstrap`: pre-RPC profile, socket, spawn lock, and daemon identity.
- `omnifs-state`: SQLite desired state, actions, observations, and caches.
- `omnifs-daemon`: reconciliation, local control, Attachments, and VFS serving.
- `omnifs-engine`: trusted provider runtime and projection semantics.
- `omnifs-vfs`: namespace facade, wire protocol, reconnect, and sessions.
- `omnifs-fuse`, `omnifs-nfs`, `omnifs-mtab`: OS protocol adapters and mount
  mechanics.
- `omnifs-fs-runtime`, `omnifs-libkrun`, `omnifs-thin`: out-of-process
  filesystem lifecycle.
- `omnifs-cli`, `omnifs-inspector`: user commands, output, and inspection.
- `omnifs-sdk`, `omnifs-wit`, `providers/`: provider authoring and components.
- `omnifs-itest`: host, provider, filesystem, and live conformance tests.

## Validation

Run the narrowest meaningful check while iterating. Before a push or handoff,
run `just check`. Detailed gates and live-lane requirements live in
`docs/contracts/60-build-validation.md`.

- Host or CLI sanity: `cargo fmt` and focused `cargo nextest run`.
- Fresh worktree: `just build providers` before host tests that need WASM.
- Documentation changes: `just docs-check`.
- Provider manifest changes: `just schema`.
- Mount, runtime, provider, clone, or traversal changes require the relevant
  live path. Rust checks alone are not enough.
- Always use `target/debug/omnifs` or `target/release/omnifs`, never bare
  `omnifs`.
- Preserve the configured compiler cache wrapper. If sandboxing blocks its
  local service, rerun with scoped escalation instead of disabling it.

## Active footguns

- Both fixed VFS listeners are part of readiness. Failure to bind either, or
  either listener exiting later, is fatal.
- `cargo check --workspace --all-targets` builds WASM guests for the host and
  is not the host gate. Use `just check host` and `just test host`.
- `omnifs-wit` guest bindings (`provider`) and host bindings (`host`) must
  coexist. Cargo feature unification makes feature-alternate modules unsafe.
- After WIT changes, stale generated bindings may require
  `cargo clean -p omnifs-wit` or a clean build.
- Some engine integration tests build providers. Prebuild them and set
  `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1`, or use `just test host`.
- Integration fixtures share only the explicit compiled Wasmtime cache.
  Runtime state remains private to each fixture.
- Provider metadata lives in one custom WASM section. Missing metadata usually
  means the artifact is stale; rebuild providers.
- Never enable `serde_json/preserve_order`. Mount versions depend on canonical
  map ordering; `omnifs_state::mount::tests::canonical_config_text_is_pinned`
  guards this.
- Live NFS mount tests use a cross-process lock. Do not parallelize them.

## Documentation

- Keep contracts about enforced current behavior.
- Keep architecture notes about the current model, its rationale, and short
  descriptions of rejected prior designs that must not return.
- Do not keep completed implementation plans, migration playbooks, temporary
  ledgers, or campaign checklists in the repository.
- Do not copy source structs, WIT blocks, or protobuf messages into prose.
  Name the owning symbols and files.
- Update a contract when ownership or behavior changes. Update architecture
  when the model or rationale changes. Delete stale footguns.
- Run `just docs-check` after documentation changes.
