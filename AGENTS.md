# AGENTS.md

Repository guide for agents and contributors working on `omnifs`.
`CLAUDE.md` is a symlink to this file, so keep it self-contained.

## Start here

`omnifs` is a filesystem projection system. It projects external services into
one shared virtual namespace. Providers own meaning: what paths exist and what
bytes they hold. The host owns trust, authorization, callouts, caching, and
I/O. FUSE and NFS expose the namespace to the OS as one or more filesystems.

Before changing the repository:

1. Read `docs/contracts/00-index.md`, then the one contract for the area.
2. Read `docs/architecture/00-overview.md` only when you need system rationale;
   it routes to focused architecture notes.
3. Trace the production call path and its tests before deciding.

Contracts bind behavior. Architecture notes explain the current design and
rejected prior designs. Source code is the final check on current shape. If
code and a contract disagree, resolve the conflict and update the contract in
the same change.

## Rule tiers

- **Invariant:** breaking it is wrong. Stop if the task appears to require it.
- **Gated decision:** surface the tradeoff and get explicit approval first.
- **Direction:** follow it unless the code gives a concrete reason not to.
- **Footgun:** a current trap. Remove the note when its condition disappears.

## Universal invariants

- The host owns trust; all providers, including embedded providers, are
  untrusted.
- Providers and the SDK own object identity, canonical assembly, rendering,
  versioning, preload, revalidation, and route topology. The host knows only
  paths, bytes, attributes, cache facts, capabilities, and effects.
- `omnifs-engine` owns shared projection semantics. Filesystems consume only
  the narrow namespace surface and own protocol state, not projection policy.
- Host caching is opaque byte and fact storage. Providers do not add private
  LRUs or expiration policy.
- SQLite is the sole desired-state authority for Provider, Credential, Mount,
  and Filesystem resources; the CLI has no desired-state journal.
  `ApplyResources` ends after validation, one SQLite transaction, and a
  non-blocking reconcile wakeup; daemon workers reconcile after the reply.
- The daemon owns provider preparation, namespace publication, Filesystem
  lifecycle, and live VFS sessions. Filesystems stay out of process.
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
  signal; do not weaken tests, fixtures, coverage, or strict parsing to hide it.
- Keep one fact under one owner. Delete duplicate DTOs, compatibility aliases,
  bridge layers, and one-caller forwarding helpers when the direct path exists.
- This project is pre-alpha and has no backward-compatibility obligation.
  Delete obsolete readers, migrations, wire fields, aliases, and APIs unless a
  current interoperability requirement says otherwise.
- Add an abstraction only for two real callers or one volatile external
  boundary. Prefer parsed domain types over strings, maps, or raw JSON.
- Public APIs need current callers and enforced invariants. Dependencies must
  pay for themselves; remove a direct dependency when its final use disappears.
- Treat WIT, protobuf, VFS wire, shared domain type, and public constructor
  changes as repository-wide migrations. Update definitions, generated
  bindings, constructors, patterns, callers, fixtures, and tests together, then
  validate in dependency order per `docs/contracts/60-build-validation.md`.
- Before changing a shared lookup, attribute, cache, or identity type, inspect
  its callers and relevant history; confirm the existing owner before adding a
  type or cache.
- Preserve user changes in dirty worktrees. Do not use destructive Git commands
  or rewrite history without explicit approval.
- Before stacked Git work, inspect the base, merge state, ancestry, and
  overlapping worktrees. Do not rebase or replay until the target relationship
  is explicit.
- Delegate only disjoint write sets. One agent owns each shared protocol,
  schema, or public API migration. A handoff states files changed, focused
  checks run, and pending work; the parent runs the combined gate.
- Use Conventional Commits when asked to commit. Do not push or open a pull
  request without explicit approval.

## Execution preflight

Before the first broad build, test, Git write, or networked command:

1. Confirm the active worktree and all write targets are writable. A sibling
   worktree does not inherit the main worktree's permissions.
2. Check artifacts before treating a missing file as a code failure:
   provider-backed host tests need `just build providers`; local libkrun use
   needs `just guest-image`.
3. Use one narrow command to separate code from environment failures.
   Permission errors from `sccache`, Cargo or Git locks, cache databases,
   keyrings, or network access are environment failures.
4. Rerun a required sandbox-blocked operation with scoped escalation. Do not
   disable `sccache` or widen validation. Serialize commands that write the
   same lockfile, provider artifacts, Git index, or live filesystem state.

## Orientation

- `omnifs-core`: shared identities, paths, and filesystem primitives.
- `omnifs-api`: resource domain and typed local control protocol.
- `omnifs-bootstrap`: pre-RPC profile, socket, spawn lock, and daemon identity.
- `omnifs-state`: SQLite desired state, actions, observations, and caches.
- `omnifs-daemon`: reconciliation, local control, Filesystems, VFS serving,
  runtime mechanics, and Doctor.
- `omnifs-engine`: trusted provider runtime and projection semantics.
- `omnifs-vfs`: namespace facade, wire protocol, reconnect, and sessions.
- `omnifs-fuse`, `omnifs-nfs`, `omnifs-mtab`: OS protocol adapters and mount
  mechanics.
- `omnifs-libkrun`, `omnifs-thin`: out-of-process filesystem helpers.
- `omnifs-cli`, `omnifs-inspector`: user commands, output, and inspection.
- `omnifs-sdk`, `omnifs-wit`, `providers/`: provider authoring and components.
- `omnifs-itest`: host, provider, filesystem, and live conformance tests.

## Validation

Run the narrowest meaningful check while iterating. Before a push or handoff,
run `just check`. Detailed gates and live-lane requirements live in
`docs/contracts/60-build-validation.md`.

- Host or CLI sanity: `cargo fmt` and focused `cargo nextest run`; use
  `just test fast` for the default quick host lane.
- Documentation changes: `just docs-check`.
- Provider manifest changes: `just schema`.
- Mount, runtime, provider, clone, or traversal changes require the relevant
  live path. Rust checks alone are not enough.
- Always use `target/debug/omnifs` or `target/release/omnifs`, never bare
  `omnifs`.

## Active footguns

- Both fixed VFS listeners are part of readiness. Failure to bind either, or
  either listener exiting later, is fatal.
- `cargo check --workspace --all-targets` builds WASM guests for the host and
  is not the host gate. Use `just check host` and `just test host`.
- `omnifs-wit` guest bindings (`provider`) and host bindings (`host`) must
  coexist. Cargo feature unification makes feature-alternate modules unsafe.
- After WIT changes, stale generated bindings may require
  `cargo clean -p omnifs-wit` or a clean build.
- Host integration tests reuse the built provider sidecars and serialize an
  automatic build only when an artifact is missing.
- Integration fixtures share only the explicit compiled Wasmtime cache.
  Runtime state remains private to each fixture.
- Provider metadata lives in one custom WASM section. Missing metadata usually
  means the artifact is stale; rebuild providers.
- Never enable `serde_json/preserve_order`. Mount versions depend on canonical
  map ordering; `omnifs_state::mount::tests::canonical_config_text_is_pinned`
  guards this.
- Live NFS mount tests use a cross-process lock. Do not parallelize them.

## Documentation

- Contracts contain enforced current behavior. Architecture notes contain the
  current model, rationale, and rejected prior designs. Do not keep completed
  plans, migration playbooks, temporary ledgers, or campaign checklists.
- Update contracts with ownership or behavior; update architecture with the
  model or rationale; delete stale footguns.
- Start each focused architecture note with `Status`, `Scope`, `Read when`, and
  `Binding contracts`.
- Architecture notes explain a choice, its owner, its effects, its failure
  boundary, and why rejected designs failed. Put binding `must`, `never`, and
  `do not` rules in contracts.
- Use a table when a subsystem has several runtimes, protocols, states, or
  authority sources. Use prose for rationale and causal flow.
- Do not copy source structs, WIT blocks, protobuf messages, volatile enums,
  command flags, or dependency internals into architecture prose. Name the
  source owner and proof test when exact detail matters; pin external behavior
  to its version.
- Run `just docs-check` after documentation changes.
