# Refactor ledger

Status: current audit

Scope: Rust production crates, providers, tests, contracts, and architecture
notes at revision bcfecf1994b79e339e3f5f1cdf75ec1ecc400555.

This ledger records refactors that improve semantic ownership, symbol
placement, naming, and lifecycle structure. It is not approval to change a
contract. Each entry names the current owner, the proposed owner, the
invariants that must survive, and a falsifier that can reject the move.

Review update: Sol high-reasoning review completed after the initial audit.
The review rejected R-001 and R-015, downgraded the validated-input and
credential-status claims, and required clearer evidence limits and priority
semantics. A later focused source check also falsified R-022. The changes
below record those reviews.

The running ast-grep task was observed through its existing read-only
artifacts, OUTLINE.md and SIMPLIFICATION_LEDGER.md. Neither artifact, the
task, nor its process was changed.

## Product model

omnifs projects provider-owned meaning into a shared filesystem namespace.
The system has several state and trust boundaries. A refactor is good only
when it reduces duplicate policy without moving an authority across one of
these boundaries.

| Boundary | Current owner | Must not become owned by |
|---|---|---|
| Provider meaning, object identity, canonical bytes, rendering, versions, preload, routes | Provider and SDK | Host, filesystem protocol adapters, or CLI |
| Trust, authorization, callouts, cache storage, effects, and host I/O | Host and engine | Providers |
| Projection semantics, lookup, listing, read behavior, cache facts, and provider terminal effects | omnifs-engine | FUSE, NFS, VFS, or control-plane storage |
| Desired Provider, Credential, Mount, and Filesystem state | SQLite through omnifs-state | API declarations, CLI, progress, or runtime memory |
| Reconciliation, provider preparation, namespace publication, Filesystem lifecycle, and live VFS sessions | omnifs-daemon | CLI or OS protocol adapters |
| Namespace framing, handshake, reconnect, invalidation, and session transport | omnifs-vfs | Engine cache policy or API resource validation |
| OS protocol state, mount options, replies, and teardown | omnifs-fuse and omnifs-nfs | Engine projection policy |
| Resource, progress, Inspector, and local control wire shapes | omnifs-api | Engine internals or SQLite rows |
| Authoring, commands, human output, and daemon spawn | omnifs-cli | Desired-state journal or runtime lifecycle |

The governing rule is one owner for each fact, policy, state transition, and
wire meaning. A second type is justified when it adds a different invariant,
trust level, persistence rule, clock, serialization contract, or failure
boundary.

## Audit method and evidence boundary

The audit used the repository contracts, focused architecture notes, the
ast-grep structural map, the existing simplification ledger, source-level
caller searches, and targeted flow traces.

The traced flows were:

1. declaration and SQLite state through generation preparation into an engine
   mount;
2. action acceptance through durable transition and public wakeup;
3. Filesystem runtime events through durable, status, and progress publication;
4. provider invocation through validation, effect lowering, cache publication,
   invalidation, and Inspector completion;
5. VFS request construction through server dispatch and client response
   extraction;
6. provider commands through the Instance queue, driver, and shutdown.

The audit did not treat source order, a generic noun, or a top-level function
as a defect by itself. A finding needs a duplicated policy, an ownership
contradiction, an impossible state, an accidental public surface, or a
material lifecycle risk.

OUTLINE.md is structural evidence, not semantic proof. SIMPLIFICATION_LEDGER.md
contains the earlier S-001 through S-022 candidates. Related S IDs are kept
below so the two ledgers can be compared without losing history.

Coverage limits: this is a workspace audit, not a downstream-consumer audit.
The providers and generated surfaces were checked through structure and
targeted callers, not every feature combination. Public API findings therefore
remain compatibility-gated until published consumers and feature builds are
checked.

## Naming and placement rules applied

These rules are the semantic checks used in the findings.

1. Name by the invariant and lifecycle owner, not by the shape of the data.
   ResourceIndex is a derived declaration lookup. DesiredRevisionView carries
   the revision that makes the lookup valid. They are not synonyms.
2. Make validation claims precise. If a type itself promises an invariant,
   enforce it with private state and a checked constructor. If validation
   belongs to an outer builder, document that boundary and use Input or Draft
   when that is clearer. Public fields alone do not prove a bug.
3. Make domain restrictions visible in function parameters and enums. A
   credential status mapper should not accept a filesystem action kind and
   then handle the impossible case inside a match.
4. Put protocol constants beside the protocol that interprets them. A
   filesystem attach environment variable is a VFS transport fact, not an API
   resource fact.
5. Keep phase vocabularies separate when their clocks or persistence meanings
   differ. Centralize the projection bundle, not the enums.
6. Keep one canonical owner for a policy. Adapters may translate once at a
   boundary, but they should not reimplement validation, identity, retry,
   ordering, or publication.
7. Use names such as Spec, State, Input, and View when their contracts differ.
   They are heuristics, not a vocabulary law. Config is acceptable when it
   names configuration rather than a validation guarantee.
8. A helper earns a name when it owns a policy, marks an effect boundary, or
   removes meaningful repetition. Caller count is evidence, not a rule. A
   private helper is preferred to a trait when FUSE and NFS share setup but
   not protocol state.
9. A public module is an API promise. If all current callers are inside the
   crate, make the module private unless an external consumer is an explicit
   requirement.
10. An async runtime owner must name admission, cancellation, draining, and
    join behavior. A channel and a reply type are not a lifecycle policy.
11. Keep the semantic facade intact when splitting files. Private modules may
    organize host projection, handles, and protocol glue, but they must not
    create a second namespace owner.
12. Prefer exhaustive domain enums and typed operation pairs. Do not hide a
    wire protocol behind a generic descriptor unless a prototype reduces
    state and keeps request, response, error, lease, and epoch rules visible.

## Ranked index

| ID | Area | Finding | Category | Attention | Complexity | Contract risk | Confidence | Status |
|---|---|---|---|---|---|---|---|---|
| R-001 | Control plane to engine | Reject: boundary projections are not proven duplicate policy | refactor | P3 | Low | Low | High | rejected |
| R-002 | Desired revision | Build one derived, exact-revision resource view | refactor | P1 | Medium | Medium | Medium | discovery |
| R-003 | State actions | Share durable action reservation and commit mechanics | refactor | P1 | Medium | High | High | implemented |
| R-004 | Daemon actions | Make one daemon owner pair durable transitions with public wakeups | refactor | P1 | Medium | Medium | Medium | implemented |
| R-005 | Filesystem lifecycle | Centralize phase publication and progress payload construction | refactor | P1 | High | High | High | implemented (safe progress slice) |
| R-006 | Engine lifecycle | Give terminal validation, lowering, publication, and Inspector completion one owner | refactor | P2 | High | Medium | Medium | implemented |
| R-007 | Provider runtime | Define bounded admission, cancellation, drain, and join semantics | redesign | P1 | High | High | High | product-gated |
| R-008 | VFS transport | Move attach environment constants to the VFS owner | refactor | P2 | Low | Medium | High | compatibility-gated |
| R-009 | Engine API | Narrow the accidental public view module | API refactor | P2 | Low | Medium | Medium | compatibility-gated |
| R-010 | Engine runtime input | Clarify the validated-input boundary | refactor | P2 | Medium | Medium | Medium | discovery |
| R-011 | Credential status | Constrain credential status mapping if the broad action type is not intentional | refactor | P2 | Low | Low | Medium | implemented |
| R-012 | Engine observability | Decouple internal observation types from the API Inspector schema | redesign | P3 | Medium | Medium | Medium | conditionally retained |
| R-013 | VFS client | Share response extraction without hiding wire semantics | refactor | P2 | Low | Low | High | implemented |
| R-014 | VFS protocol | Keep request, response, error, server, and client pairing under one private owner | refactor | P2 | High | High | Medium | conditionally retained |
| R-015 | VFS sessions | Reject: Session is a public snapshot, not a drop-in registry entry | refactor | P3 | Low | Low | High | rejected |
| R-016 | Thin launchers | Share FUSE and NFS attach preparation | refactor | P2 | Medium | Medium | Medium | implemented |
| R-017 | VFS listeners | Factor Unix and TCP accept plumbing only if transport behavior stays visible | refactor | P3 | Medium | Low | Medium | deferred |
| R-018 | CLI status | Derive a ResourceRow once before human and structured rendering | refactor | P2 | Low | Low | High | implemented |
| R-019 | Libkrun helper | Use one private schema for fixed launch argument parse and encode | refactor | P2 | Medium | Medium | High | implemented |
| R-020 | Bootstrap | Share the locked bootstrap removal body | refactor | P2 | Low | Low | High | implemented |
| R-021 | CLI auth | Remove the one-caller AuthManifestView wrapper | polish | P3 | Low | Low | High | implemented |
| R-022 | Engine namespace | Reject: reverse TreeError conversion has a runtime caller | polish | P3 | Low | Low | High | rejected |
| R-023 | Provider runtime shape | Prototype a typed operation gateway after R-007 | refactor | P2 | High | High | Medium | conditionally retained |
| R-024 | Engine effects | Avoid parsing the same effect paths twice with a validated effect view | refactor | P2 | Medium | Medium | Medium | conditional |

Attention is review priority, not implementation order. It combines likely
impact and evidence quality. The dependency waves below describe sequence.

## Detailed findings

### R-001: Reject the canonical resolved-mount projection refactor

Category: rejected

Related simplification: S-001

Evidence: API declarations, SQLite rows, daemon ResolvedMount values, engine
RuntimeMountConfig values, and API status records are distinct shapes at
[resource.rs](/Users/raul/W/omnifs/crates/omnifs-api/src/resource.rs:463),
[state/resource.rs](/Users/raul/W/omnifs/crates/omnifs-state/src/resource.rs:25),
[generation_builder.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/generation_builder.rs:45),
[runtime/mod.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/mod.rs:70),
and [mapping.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/control/mapping.rs:6).

Sol review: the earlier entry treated legitimate boundary representations as
duplicated daemon policy. ResolvedMount already owns the daemon's resolved
facts, and the other shapes carry different persistence, trust, runtime, or
wire contracts. No concrete duplicate policy was proven.

Disposition: do not introduce a shared mount record or delete the boundary
constructors. Keep the S-001 field inventory as a guard for future work:
reopen it only when a field has the same writer, invariant, clock, and failure
behavior at both sides of a boundary.

Preserved invariants: API declarations remain separate from SQLite desired
state; engine runtime inputs remain separate from control records; credentials
remain excluded from public and durable projections.

Falsifier for reopening: source tracing proves that one copied field has no
boundary-specific invariant and that its constructors duplicate an enforced
policy rather than translating representations.

Status: rejected.

### R-002: Build one derived, exact-revision resource view

Category: refactor

Related simplifications: S-006, S-007, S-020

Evidence:

- API validation builds provider and credential maps in
  [resource.rs](/Users/raul/W/omnifs/crates/omnifs-api/src/resource.rs:221).
- Daemon validation and credential targeting rebuild related maps in
  [resource_control.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/resource_control.rs:332)
  and :577.
- Provider membership builds name-to-digest and digest-to-mount aliases at
  [resource_control.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/resource_control.rs:757).
- Serving reconciliation rescans declarations in
  [serving_reconciler.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/serving_reconciler.rs:235)
  and :751.
- State validation and generation preparation build more side maps in
  [state/resource.rs](/Users/raul/W/omnifs/crates/omnifs-state/src/resource.rs:375)
  and [generation_builder.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/generation_builder.rs:108).

Current owner: every consumer owns a local map derived from
NormalizedResourceSet::resources(). That repetition may be intentional when
callers need different duplicate, ordering, lifetime, or revision semantics.

Expected owner: if the caller inventory proves shared semantics, a
declaration-only ResourceIndex can live beside NormalizedResourceSet, while a
daemon-only DesiredRevisionView can add the captured revision, loaded provider
metadata, and provider usage. Both remain derived views, never authorities.

Concrete change: measure map construction and lookup costs, then prototype a
private view in one caller. Keep the original normalized slice for
order-sensitive validation. Carry a revision only across an async boundary
that actually needs a stable snapshot.

Naming rule: do not call this a cache or state store. It is a revision-bound
view. Do not persist it.

Deletion inventory: repeated provider maps, credential scans, digest aliases,
and declaration rescans that use identical duplicate and ordering semantics.

Preserved invariants: providers with no mounts remain visible; names sharing a
digest remain distinct; stale views cannot become desired state; SQLite stays
the source of truth.

Compatibility effects: internal if the view stays private. Public API changes
would require a deliberate lifetime and allocation decision.

Falsifier: a consumer needs a different duplicate or ordering rule, observes a
different revision by design, or performs better with a bounded linear scan.

Estimated impact:

- production LOC: 60 to 130 gross lines may move, with fewer reconstruction
  sites;
- test LOC: add exact-revision and no-mount provider cases;
- named declarations: one ResourceIndex and one daemon revision view;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: API normalization tests, daemon credential target
tests, provider membership tests, serving reconciliation tests, and generation
builder tests.

Wave 4 evidence disposition: retained as discovery. The API reference maps in
`validate_references` enforce order-independent declaration checks, while the
daemon maps load provider metadata and validate credential schemes across an
async state boundary. `credential_target` and state sidecar validation use
different target and secret checks; serving reconciliation builds digest
aliases for progress membership and provider scheduling. The generation
builder keeps a revision-bound metadata and credential-id view. These callers
do not share duplicate, ordering, lifetime, or revision semantics, so no one
derived `ResourceIndex` owner is proven. Keep the candidate open for a measured
private view in one caller; do not merge the maps or change SQLite authority.

Status: discovery.

### R-003: Share durable action reservation and commit mechanics

Category: refactor

Related simplification: S-005

Evidence: credential and filesystem action paths duplicate receipt checks,
target validation, pending claims, generation reads, target effects, receipt
writes, and pruning in
 [action.rs](/Users/raul/W/omnifs/crates/omnifs-state/src/action.rs:119)
and :216. Digest framing is also maintained separately at :561 and :578.

Current owner: omnifs-state::action owns both public action paths, but each
path repeats the transaction mechanics.

Expected owner: private state helpers such as reserve_action and commit_action
own the shared durable sequence. Credential and filesystem wrappers keep
target-specific validation and effects.

Concrete change: introduce a private reserved-action value that carries the
receipt identity, target, expected generation, and request digest. Commit must
take an explicit generation update and effect receipt. Keep the two public
accept methods as domain adapters.

Preserved invariants: existing receipt wins; pending claims block duplicates;
generation updates occur at the current transaction point; credential bytes
never enter request digests, receipts, logs, status, or debug output; prune
order stays fixed.

Compatibility effects: SQLite transaction behavior is observable through
concurrency and error ordering. Treat this as an internal refactor only after
transaction tests pin those facts.

Falsifier: the two actions require different transaction points, error order,
rollback behavior, or secret exclusion that cannot be represented in an
explicit operation input.

Estimated impact:

- production LOC: 40 to 80 fewer;
- test LOC: add ordering and secret-exclusion assertions;
- named declarations: two public workflows retain their names, one private
  reservation and one commit type are added;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: state action acceptance, duplicate receipt,
generation race, and credential secret-safety tests.

Implementation: added private ActionInput, ActionReservation, and
ActionGenerationUpdate values plus reserve_action and commit_action in
[action.rs](/Users/raul/W/omnifs/crates/omnifs-state/src/action.rs:106).
Credential and filesystem adapters retain their target validation, effects,
generation updates, and transaction boundaries. The shared digest prefix
preserves the existing framing and still excludes credential material.

Checks: cargo test -p omnifs-state passed with 21 unit tests and 0 doctests;
cargo clippy -p omnifs-state -- -D warnings passed; cargo fmt and diff checks
passed.

Status: implemented.

### R-004: Make one daemon owner pair durable transitions with public wakeups

Category: refactor

Related simplification: S-018

Evidence: serving reconciliation pairs durable transitions with public
publication at
 [serving_reconciler.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/serving_reconciler.rs:655),
:813, :856, and :924. Filesystem supervision repeats the pairing at
 [filesystem_supervisor.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/filesystem_supervisor.rs:353),
:840, :1015, :1053, and :1132. The existing bridge is
 [resource_control.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/resource_control.rs:327).

Current owner: individual reconcilers and the filesystem supervisor decide
when to call state transitions and when to publish receipts or wake watchers.

Expected owner: ResourceControl or a private daemon action publication
boundary owns the order: durable SQLite transition first, best-effort
progress receipt and watcher wakeup second. Kind-specific progress remains at
the caller.

Concrete change: add a narrow transition_action wrapper that takes an
explicit action transition and returns the durable receipt. Route current
reconciler paths through it. Do not merge action state with Filesystem phase
state.

Preserved invariants: SQLite commits before public progress; wakeup failure
does not roll back durable state; kind-specific error context and terminal
events remain at their current owners.

Compatibility effects: internal daemon ordering, but progress subscribers can
observe missing or reordered events if the wrapper changes call sites.

Falsifier: a current transition intentionally skips receipt publication,
requires publication before durable state, or needs a different error payload
than the wrapper can carry.

Estimated impact:

- production LOC: 20 to 45 fewer;
- test LOC: add one ordering test for each transition family;
- named declarations: one daemon transition wrapper;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: serving recovery, filesystem retry, action watcher,
and progress snapshot tests.

Implementation: added ResourceControl::transition_action and
transition_action_with_progress in
[resource_control.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/resource_control.rs:332).
They own the durable SQLite transition, optional typed progress publication,
receipt publication, and reconciler wakeup. Filesystem supervisor and serving
reconciler transition sites now use the wrapper. Credential progress remains
at the caller because its event must precede the public action receipt, and
the wrapper exposes that ordering without merging credential and filesystem
phase vocabularies.

Checks: cargo check -p omnifs-daemon, cargo test -p omnifs-daemon --lib
passed with 107 tests, and cargo fmt and diff checks passed.

Status: implemented.

### R-005: Centralize Filesystem phase publication and progress payloads

Category: refactor

Related simplifications: S-003, S-008

Evidence: the repository intentionally has separate persisted,
public-status, progress, runtime-fact, and runner-protocol phase families.
The supervisor repeats publication at
 [filesystem_supervisor.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/filesystem_supervisor.rs:559),
:954, and :1086, and constructs similar progress payloads at :1221, :1254,
and :1305. The supervisor documentation states that it is the sole owner of
Filesystem runtime sequencing and recovery at
 [filesystem_supervisor.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/filesystem_supervisor.rs:1).

Current owner: FilesystemSupervisor owns sequencing, but each transition site
manually assembles two or more projections.

Expected owner: a private supervisor publication bundle, for example
PhasePublication and a small FilesystemProgressInput, owns the repeated
mapping. The existing phase enums remain owned by state, API, runtime, and
runner boundaries.

Concrete change: centralize the durable write, status projection, optional
progress stage, target fanout, and queue sampling where the same transition
meaning is shared. Keep deletion and runtime-event fallback inputs explicit.

Do not create one LifecyclePhase enum for SQLite, API status, progress,
runner IO, and runtime facts. Their clocks and meanings differ.

Preserved invariants: SQLite writes first; status and progress retain their
current serialized meanings; deletion retains its runtime fallback and target
order; runner phases remain protocol facts.

Compatibility effects: public status, progress ordering, recovery, and runner
behavior are sensitive. Use one publication helper, not a new cross-crate
lifecycle protocol.

Falsifier: recovery reads a phase with a different persistence meaning,
progress needs fields status must not expose, or a runtime event can outlive
the daemon transition with different ordering.

Estimated impact:

- production LOC: 80 to 180 fewer;
- test LOC: retain or increase phase projection coverage;
- named declarations: one private publication bundle and one input type;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: filesystem supervisor phase, retry, deletion,
progress fanout, and recovery tests.

Implementation: added private FilesystemProgressInput and
record_filesystem_progress in
[filesystem_supervisor.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/filesystem_supervisor.rs:1244).
The helper owns payload construction, queue sampling, and desired/action
fanout for normal phases, deletion, and runtime events. Durable observations
and ResourcePhase publication remain explicit at transition sites because
their ordering differs between startup, retry, failure, and deletion. This
implements the safe progress-publication slice without creating a shared
LifecyclePhase enum or changing serialized phase meanings.

Checks: cargo test -p omnifs-daemon --lib passed with 107 tests; cargo fmt and
diff checks passed.

Status: implemented (safe progress slice).

### R-006: Give engine terminal publication one owner

Category: refactor

Related simplification: S-019

Evidence: six Runtime::run_* methods repeat provider invocation, typed
validation, Inspector input, staged blob handoff, provider error lowering,
effect lowering, operation-specific lowering, transition merge, fenced cache
publication, and Inspector completion in
 [lifecycle.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/ops/lifecycle.rs:16)
through :369. publish_transition owns the fence and invalidation route at
:371, while validation and effect lowering live in
 [validate.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/ops/validate.rs:11)
and [apply.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/effects/apply.rs:48).

Current owner: each operation method owns the same terminal sequence and
manually invokes the separate lowerers.

Expected owner: an internal TerminalTransaction in the engine lifecycle
module owns validation-before-lowering, staged blob ownership, one projection
publication, fence handling, invalidation routing, and terminal Inspector
completion. Operation-specific lowerers and exceptional commits stay explicit.

Concrete change: prototype a terminal value with accept, lower, commit, and
finish steps. Keep lookup subtree handoff, read NotFound early commit, EOF
learned-size policy, and event terminals as named callbacks or explicit
branches.

Preserved invariants: each WIT operation keeps its typed result; unvalidated
effects cannot publish; staged blobs are either committed or dropped; one
invalidation fence surrounds one projection commit; Inspector outcomes close
on every path.

Compatibility effects: internal engine behavior, but cache ordering and
Inspector records are externally observed through daemon control.

Falsifier: callback state becomes more complex than the six methods, or
NotFound, EOF learning, and event commits need incompatible semantics that the
owner hides.

Estimated impact:

- production LOC: about 80 to 180 net fewer after the owner is proven;
- test LOC: add terminal-path matrix tests;
- named declarations: one transaction owner, operation-specific lowerers kept;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: operation validation, effect application,
publication fence, Inspector terminal outcome, EOF, and subtree handoff tests.

Implementation: added private `TerminalTransaction` in
[lifecycle.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/ops/lifecycle.rs:15).
The transaction owns the validate-before-lowering order, staged blob handoff,
base effect lowering, one fenced publication, invalidation routing, and
terminal Inspector completion. Lookup and list subtree handoffs, read
`NotFound`, ranged-EOF learning, and operation-specific cache records remain
explicit at their callers. The semantic owner is `omnifs-engine` lifecycle;
representative callers are `Runtime::run_lookup_child`,
`Runtime::run_list_children`, `Runtime::run_read_file`,
`Runtime::run_open_file`, `Runtime::run_read_chunk`, and `Runtime::run_event`.

Preserved invariants: validation rejects malformed provider returns before
effects lower; staged blob writes are committed only with their transition or
dropped on error; each publication retains its captured epoch fence and
invalidation delivery; every terminal Inspector span records its outcome.

Checks: `cargo fmt --all -- --check`, `cargo check -p omnifs-engine
-p omnifs-vfs`, and `git diff --check` passed. Full and focused tests remain
parent-owned and were not run here.

Status: implemented.

### R-007: Define bounded provider admission, cancellation, drain, and join

Category: redesign

Related simplification: S-022, but this entry is about runtime semantics rather
than command-shape deduplication.

Evidence:

- Instance sends namespace and event commands through an unbounded channel at
  [instance.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/instance.rs:144).
- Six async methods create a reply channel, send a command, and await a reply
  at [instance.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/instance.rs:203)
  through :335.
- The driver stores in-flight operations in FuturesUnordered and accepts more
  commands without an admission limit at
  [instance.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/instance.rs:397).
- The current architecture records that a dropped caller reply does not
  cancel the provider call and that driver shutdown drops remaining futures in
  [60-async-provider-runtime.md](/Users/raul/W/omnifs/docs/architecture/60-async-provider-runtime.md:109).

Current owner: Instance owns the store and driver thread, but queue growth,
caller cancellation, in-flight drain, and shutdown join behavior are implicit
consequences of the channel and FuturesUnordered.

Expected owner: the provider runtime boundary must own an explicit admission
and lifecycle policy. A later gateway may implement it, but Runtime must
remain the owner of typed validation and terminal effects.

Concrete change: make a product decision for maximum queued and in-flight
operations, admission error shape, cancellation before start, cancellation
while suspended on a host callout, shutdown drain order, and driver join
reporting. Then add counters and tests before changing the queue.

Preserved invariants: one Wasmtime store remains on one driver thread; async
host callouts can overlap where the current runtime permits; providers do not
gain trust or direct I/O; WIT operations remain typed.

Compatibility effects: high. Queue bounds and cancellation change latency,
resource use, provider-visible timing, and shutdown behavior.

Falsifier: workload and contract evidence requires unbounded overlap or shows
that cancellation cannot be implemented safely for current Wasmtime calls.

Estimated impact:

- production LOC: unknown until the policy is chosen;
- test LOC: substantial queue, cancellation, and shutdown matrix;
- named declarations: admission policy, cancellation token, and join outcome;
- generated surfaces: none;
- dependencies: none expected.

Preferred tests to add: queue saturation, caller drop before dispatch, caller
drop during callout suspension, driver failure, orderly shutdown, and in-flight
drain.

Status: product-gated.

### R-008: Move attach environment constants to the VFS owner

Category: refactor

Related simplification: S-002

Evidence: omnifs-vfs imports two API constants and declares an optional API
dependency in [Cargo.toml](/Users/raul/W/omnifs/crates/omnifs-vfs/Cargo.toml:12).
The constants are declared in
 [api/lib.rs](/Users/raul/W/omnifs/crates/omnifs-api/src/lib.rs:57), while VFS
uses them in [client.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/client.rs:80)
and [beacon.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/beacon.rs:51).

Current owner: API exports process environment names that describe VFS attach
and readiness transport.

Expected owner: omnifs-vfs::wire owns the names and their transport meaning.
Daemon launchers and integration tests import them from VFS.

Concrete change: move the constants, update workspace callers, remove the
optional API dependency from the wire feature, and keep the string values
unchanged.

Preserved invariants: attach and readiness behavior, environment spelling,
feature behavior, and listener readiness remain unchanged.

Compatibility effects: the public import path changes from omnifs-api to
omnifs-vfs. Check downstream users before deleting the old path.

Falsifier: the API is intentionally the public owner of all process
environment contracts or an external generator consumes the old path.

Estimated impact:

- production LOC: 10 to 25 fewer and one dependency edge removed;
- test LOC: update import sites only;
- named declarations: two constants move;
- generated surfaces: none;
- dependencies: omnifs-vfs no longer needs omnifs-api for wire mode.

Preferred tests to modify: VFS attach, beacon, daemon launch, and itest
readiness tests.

Wave 4 evidence disposition: retained as compatibility-gated. The constants
are still imported by the VFS client and beacon paths, daemon Docker and
libkrun launchers, and integration fixtures. Their string values and public
`omnifs_api` import paths are part of the current attach contract. Moving them
would require edits outside this write set and a downstream-consumer check;
the evidence does not authorize that transport or public API change.

Status: compatibility-gated.

### R-009: Narrow the accidental public engine view module

Category: API refactor

Evidence: [engine/lib.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/lib.rs:36)
declares pub mod view. The module contains cache-internal types such as
BodyId, ByteSource, VersionToken, FileAttrsCache, payload records, and
freshness classifications. A workspace search found only crate::view users
inside omnifs-engine; no current crate imports omnifs_engine::view.

Current owner: engine cache and tree modules own the types, but the module
visibility advertises them as a public API.

Expected owner: view is private to the engine unless a type has an explicit
external contract. Stable public namespace types should be re-exported from a
purpose-named module, not exposed through the cache record module.

Concrete change: run a public API and downstream-consumer check. If no
consumer exists, change the module to pub(crate) and add narrow re-exports
only for types with a documented contract.

Preserved invariants: cache records, provider attrs, and namespace answers do
not change representation or ownership.

Compatibility effects: high for an unknown downstream consumer, low for the
current workspace. This is why the finding is not an immediate deletion.

Falsifier: a supported external consumer, generated binding, or documented
extension imports omnifs_engine::view.

Estimated impact:

- production LOC: none to a few lines;
- test LOC: public API compile tests may need updates;
- named declarations: no semantic types removed;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: public API compile checks and engine integration
tests.

Wave 4 evidence disposition: retained as compatibility-gated. Workspace
searches found only `crate::view` users inside `omnifs-engine`, but the module
is public and exposes cache and projection record types. The audit did not
cover published downstream consumers or feature-specific API builds, so
changing `pub mod view` to private visibility would be an unapproved public
import-path change. Keep the module unchanged until that compatibility
boundary is checked.

Status: compatibility-gated.

### R-010: Clarify the validated-input boundary

Category: audit

Evidence: RuntimeMountConfig is documented as validated state-neutral input
and has public fields in
 [runtime/mod.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/mod.rs:70).
MountBuildInput is documented as already validated and has public fields in
 [registry.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/registry.rs:34).
MountTable::build_durable_mount still validates provider bytes and manifest
input at [registry.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/registry.rs:124).
The daemon constructs the values in
 [generation_builder.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/generation_builder.rs:529),
while itest, FUSE, NFS, and engine tests construct them directly.

Current owner: the daemon supplies a prepared build input, while the engine
checks provider-specific build facts. The source does not prove that every
field of either public type is meant to be constructor-enforced.

Expected owner: the docs and constructors should state which invariants the
daemon guarantees and which checks the engine repeats. A private validated
type is only warranted if callers must not construct the value directly.

Concrete change: inventory the fields and checks before changing names or
visibility. If the type itself owns an invariant, add a checked constructor
and private fields. Otherwise rename or revise the docs to say that the
builder validates the input. Do not prescribe a migration from public fields
without a downstream API decision.

Preserved invariants: provider manifest and config checks, canonical bytes,
credential binding, and unavailable-provider states retain their current
boundaries.

Compatibility effects: public field construction in itest, FUSE, NFS, and
possible downstream code makes this an API review, not a proven correctness
bug.

Falsifier: the documented validation boundary is deliberate and supported
callers rely on open construction.

Estimated impact:

- production LOC: unknown until the invariant inventory;
- test LOC: add only the constructor or documentation tests that the chosen
  boundary needs;
- named declarations: possible checked constructor or renamed input type;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: registry validation, generation builder, FUSE/NFS
support registries, and itest mount input tests.

Wave 4 evidence disposition: retained as discovery. `RuntimeMountConfig` and
`MountBuildInput` have public fields and are constructed directly by the
daemon generation builder, `omnifs-itest`, FUSE tests, NFS tests, and engine
tests. `MountTable::build_durable_mount` still owns provider-byte and manifest
validation, while the daemon owns prepared metadata and credential binding.
The callers prove an open construction surface and a split validation
boundary, but not a safe checked-constructor migration. Keep names,
visibility, and construction behavior unchanged pending an API decision.

Status: discovery.

### R-011: Constrain credential status mapping only if the broad action type is accidental

Category: refactor

Evidence before the change: `credential_action_status` accepted `ActionKind`
and mapped `RestartFilesystem` to `Blocked`. The only production callers were
the credential material replay/accept paths and credential revoke path at the
three `ResourceControl` call sites; nearby tests covered only credential
actions. A workspace search found no filesystem receipt projection calling the
mapper.

Current owner: the private credential receipt projection owns one
`CredentialActionKind` domain enum and maps it with
[`credential_action_status`](/Users/raul/W/omnifs/crates/omnifs-daemon/src/resource_control.rs:594).
Filesystem restart status remains in the filesystem action path.

Expected owner: the credential-only action kind feeds the mapper; the public
API `ActionKind` remains unchanged at the receipt boundary.

Concrete change: search all feature paths and receipt projections, then narrow
the private mapper only after proving that filesystem actions cannot reach it.

Preserved invariants: current credential active, blocked, deleted, pending,
and unknown mappings remain unchanged.

Compatibility effects: private daemon code, but action receipts and progress
projections can be cross-domain at the API boundary.

Falsifier: a real receipt or recovery path intentionally asks this helper to
map RestartFilesystem, or generated control code relies on the broad enum.

Estimated impact:

- production LOC: 0 to 15;
- test LOC: one domain-separation or defensive-branch test;
- named declarations: one private `CredentialActionKind` enum;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: resource_control credential status and action
receipt projection tests.

Implementation: narrowed the mapper to the private `CredentialActionKind`
(`SetMaterial` or `Revoke`) and kept one `credential_action_status` policy
owner in [resource_control.rs](/Users/raul/W/omnifs/crates/omnifs-daemon/src/resource_control.rs:594).
The three production callers are the credential material replay/accept paths
and credential revoke path; filesystem restart remains in its own method and
cannot enter this mapper. The representative unit test covers ready, failed,
running, and revocation terminal projections. The semantic owner remains
daemon credential receipt projection, and all existing status meanings are
unchanged.

Checks: `cargo fmt --all -- --check`, `cargo check -p omnifs-cli
-p omnifs-daemon`, and `git diff --check` passed. Focused and full tests remain
parent-owned.

Status: implemented.

### R-012: Decouple internal observation types from the API Inspector schema

Category: redesign

Related simplification: S-004

Evidence: engine internals import API event and Inspector types from
 [log_redaction.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/log_redaction.rs:1),
 [tree/read.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/tree/read.rs:25),
and [inspect.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/inspect.rs:9).
The API event types also serve JSONL, CLI, and control consumers.

Current owner: API owns the public Inspector schema, while engine code uses it
as an internal observation vocabulary.

Expected owner: engine owns a small internal observation and redaction
vocabulary. A daemon or Inspector adapter maps it to API events at the control
boundary.

Concrete change: first measure schema churn and feature-isolation pain. Only
then prototype engine::observe types and a single adapter. Keep secret
exclusion in the engine and preserve JSONL field names.

Preserved invariants: Inspector serialization, redaction, operation identity,
and terminal outcome meanings do not change.

Compatibility effects: medium. An adapter can add code and a new mapping
surface if the schema is already stable.

Falsifier: engine must emit the exact API schema directly, or the proposed
internal types have one caller and no dependency or churn benefit.

Estimated impact:

- production LOC: 20 fewer to 40 more;
- test LOC: schema and redaction tests must remain;
- named declarations: possible internal observation types and one adapter;
- generated surfaces: Inspector JSONL remains unchanged;
- dependencies: potential removal of engine to API event coupling.

Preferred tests to modify: redaction, Inspector serialization, and engine
feature-isolation tests.

Disposition: conditionally retained. A private one-to-one mirror of the API
Inspector enums and events was prototyped and rejected in this wave: the
`Inspector` still stores and publishes `InspectorRecord`, and no second engine
consumer or distinct invariant would justify the added mapping layer. The
current API event types remain the canonical observation vocabulary at this
boundary.

Falsifier for reopening: schema churn or feature isolation creates a second
engine-owned consumer with a distinct lifecycle, redaction, or trust invariant,
and a mapping can live at one control boundary without duplicating the public
record store or changing JSONL field names.

Checks: `cargo fmt --all -- --check`, `cargo check -p omnifs-engine
-p omnifs-vfs`, and `git diff --check` passed. No Inspector adapter code
remains; full and focused tests remain parent-owned and were not run here.

Status: conditionally retained.

### R-013: Share VFS response extraction without hiding wire semantics

Category: refactor

Related simplification: S-011

Evidence: WireResponse variants are defined in
 [vfs/lib.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/lib.rs:99).
WireNamespace repeats a call, response match, and mismatch error across
methods at [client.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/client.rs:376)
through :504. The request-side error mapping already has a small helper at
 [vfs/lib.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/lib.rs:85).

Current owner: each client method pairs its request with a visible response
match.

Expected owner: a private expect_* method or a small macro owns only the
repeated extraction and mismatch construction. The call site still names the
request and expected response.

Concrete change: add narrow helpers such as expect_read only if they retain
request ID, peer context, and mismatch error details. Do not introduce a
generic boxed operation layer for this low-level cleanup.

Preserved invariants: postcard variants, request IDs, reconnect state, and
corrupt-peer errors stay unchanged.

Compatibility effects: internal VFS client representation.

Falsifier: a method needs special post-match behavior or a helper hides which
wire variants pair.

Estimated impact:

- production LOC: 20 to 35 fewer;
- test LOC: preserve mismatch and reconnect tests;
- named declarations: four to six private extraction helpers;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: VFS client mismatch, request ID, and reconnect tests.

Implementation: added private expect_response! extraction in
[client.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/client.rs:314).
Each call site still names its request and expected WireResponse variant, and
the existing mismatch error, request IDs, reconnect flow, and post-processing
remain unchanged.

Checks: cargo test -p omnifs-vfs passed; cargo test -p omnifs-vfs --features
wire passed with 33 tests; cargo clippy -p omnifs-vfs --features wire
--all-targets -- -D warnings passed.

Status: implemented.

### R-014: Keep VFS operation pairing under one private owner

Category: refactor

Related simplification: S-021

Evidence: request and response variants are separate in
 [vfs/lib.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/lib.rs:58) and :99.
WireRequest::error_response adds a third mapping at :85. The server dispatches
requests at [server.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/server.rs:1231),
while the client calls and extracts responses at
 [client.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/client.rs:376).

Current owner: adding an operation requires coordinated edits in the wire
enums, server match, error mapping, and client match.

Expected owner: a private descriptor or operation table can record the
request, response, error, server invocation, lease, and epoch pairing. Public
postcard enums remain the wire owner.

Concrete change: build a typed prototype for one operation. Keep explicit
matches if the descriptor adds boxed futures, lifetime plumbing, or hides
protocol state.

Preserved invariants: wire enum ordering, handshake, reconnect, request IDs,
leases, epochs, and error responses remain byte-for-byte compatible.

Compatibility effects: high if the descriptor leaks into the wire protocol.
The safe candidate is private and internal.

Falsifier: operation-specific epoch, lease, budget, or cancellation behavior
does not fit without opaque branches, or the descriptor is larger than the
maintenance it removes.

Estimated impact:

- production LOC: 20 to 80 net fewer, with a possible 100 to 180 line
  descriptor;
- test LOC: add postcard and operation-pair compile tests;
- named declarations: one private descriptor family;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: wire compatibility, server dispatch, client
response, handshake, and reconnect tests.

Disposition: conditionally retained. The wire enums remain the canonical
postcard representation and the server/client pairings stay explicit. A
private descriptor is not safe to add in this wave: request-specific lease and
epoch handling lives in `server::dispatch` and `client::ManagerState`, while
error responses and reconnect handling carry distinct failure semantics. A
descriptor would either duplicate those policies or require boxed futures/type
erasure that hides the protocol state and risks changing transport behavior.

Falsifier for reopening: a prototype for one operation can preserve request
IDs, exact postcard variant order, lease acquisition, epoch capture, reconnect
retry, and mismatch errors without type erasure or opaque continuation state;
then add wire-compatibility and reconnect tests before moving another pair.

Checks: `cargo fmt --all -- --check`, `cargo check -p omnifs-engine
-p omnifs-vfs`, and `git diff --check` passed. No VFS descriptor code changed;
full and focused tests remain parent-owned and were not run here.

Status: conditionally retained.

### R-015: Reject the session identity consolidation

Category: rejected

Related simplification: S-012

Evidence: Session is a public snapshot and replacement value at
 [server.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/server.rs:58).
SessionEntry stores only the fields needed beside its SessionKey at :91.
The key owns the filesystem identity, while the entry owns connection
counting and exact spec/runtime checks at :164, :274, :303, :349, and :378.
Snapshot reconstruction at :397 and :459 restores the public value from these
separate owners.

Sol review: replacing the entry fields with Session would duplicate the key's
filesystem identity and conflate a public snapshot with the registry's
internal keyed state. The earlier entry treated two related representations as
one interchangeable identity.

Disposition: keep SessionKey, SessionEntry, and Session separate. Do not
consolidate them unless the registry contract changes so one value owns both
the key and the public snapshot semantics.

Preserved invariants: replacement fences, connection ownership, stop behavior,
keyed lookup, and public snapshots.

Falsifier for reopening: a source trace proves the key and entry identity have
the same owner, lifetime, and mutation rules and that storing Session would
not duplicate the key.

Status: rejected.

### R-016: Share FUSE and NFS attach preparation

Category: refactor

Related simplification: S-010

Evidence: [thin/fuse.rs](/Users/raul/W/omnifs/crates/omnifs-thin/src/fuse.rs:13)
and [thin/nfs.rs](/Users/raul/W/omnifs/crates/omnifs-thin/src/nfs.rs:13)
repeat argument parsing, runtime setup, lifecycle preparation, preflight,
VFS attach, and readiness. Their differences begin at protocol-specific mount
options and thread behavior.

Current owner: two thin launchers each own shared attach preparation and their
own protocol mount.

Expected owner: a private prepare_attach or AttachedRunner in omnifs-thin
owns shared setup. FUSE and NFS retain protocol-specific mount options,
threads, replies, and teardown.

Concrete change: trace lifetimes and readiness order, then extract only the
shared preparation. Do not introduce a generic mount trait.

Preserved invariants: argument parsing, ready-port validation, AttachTarget
resolution, Tokio runtime lifetime, preflight, attach order, logging, and join
behavior.

Compatibility effects: launcher error context and thread timing can be
observable.

Falsifier: runtime handles, lifecycle objects, or readiness order differ
between protocols, or the helper needs many protocol-specific option flags.

Estimated impact:

- production LOC: 35 to 65 fewer;
- test LOC: preserve both protocol live paths;
- named declarations: one private preparation value;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: thin FUSE and NFS attach, readiness, and teardown
tests.

Implementation: added private AttachedRunner and prepare_attach in
[lifecycle.rs](/Users/raul/W/omnifs/crates/omnifs-thin/src/lifecycle.rs:39).
FUSE and NFS now share readiness-port resolution, attach-target resolution,
runtime and lifecycle setup, protocol preflight, attach ordering, and attach
logging. Each launcher retains its own protocol mount options, readiness
behavior, thread, and teardown path.

Checks: cargo check -p omnifs-thin, cargo test -p omnifs-thin passed with 6
tests, and cargo fmt and diff checks passed.

Status: implemented.

### R-017: Factor Unix and TCP accept plumbing only if transport behavior stays visible

Category: refactor

Related simplification: S-013

Evidence: Unix and TCP listener branches duplicate accept, connection setup,
spawn, and error handling in
 [server.rs](/Users/raul/W/omnifs/crates/omnifs-vfs/src/server.rs:897)
through :963.

Current owner: each transport loop owns bind, readiness, accept, and session
spawn details.

Expected owner: at most a concrete private accept helper owns the shared
stream-to-session setup. Bind, readiness, transport labels, and cancellation
stay at the transport call sites.

Concrete change: prototype a helper with concrete stream bounds. Prefer
duplication when a generic adapter hides Unix versus TCP behavior.

Preserved invariants: fixed listener readiness, error context, cancellation,
and process shutdown.

Compatibility effects: internal server structure.

Falsifier: generic bounds make cancellation, retry, or readiness less clear.

Estimated impact:

- production LOC: 30 to 50 fewer if the helper remains concrete;
- test LOC: unchanged;
- named declarations: one private helper;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: both listener readiness and accept failure tests.

Wave 4 evidence disposition: retained as deferred. The Unix and TCP loops
share stream-to-session setup, but each call site still owns its transport
label, accept error, listener lifetime, and shutdown break. A generic helper
would need to preserve those distinctions and could hide fixed-listener
readiness behavior. No helper was added without a concrete transport-neutral
contract; keep both loops visible until a focused prototype proves that shape.

Status: deferred.

### R-018: Derive a ResourceRow once before CLI rendering

Category: refactor

Related simplification: S-014

Evidence: StatusResult::new derives status lookup, defaults, desired state,
observed state, and detail at
 [status.rs](/Users/raul/W/omnifs/crates/omnifs-cli/src/commands/status.rs:208).
render_resources repeats lookup and defaults before formatting at :261.

Current owner: status result construction and human rendering each derive the
same row facts.

Expected owner: one ResourceRow::from_snapshot derives data. Human and
structured output only format or map it.

Concrete change: build rows once, then pass the same rows to both output
edges. Keep display-only labels at the formatter.

Preserved invariants: human and structured defaults, missing-resource behavior,
phase text, and output field names.

Compatibility effects: CLI output must remain byte-compatible where promised.

Falsifier: human and structured output intentionally use different defaults or
one format needs a live lookup the other must not perform.

Estimated impact:

- production LOC: 20 to 45 fewer;
- test LOC: preserve snapshot and structured output tests;
- named declarations: one row derivation owner;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: status human, JSON, missing-resource, and default
phase tests.

Implementation: added derive_resource_rows in
[status.rs](/Users/raul/W/omnifs/crates/omnifs-cli/src/commands/status.rs:245).
Structured and human output now consume the same derived rows. The row keeps
the private ResourceKind needed to route structured results and sort human
output while serde output remains unchanged.

Checks: cargo check -p omnifs-cli, cargo test -p omnifs-cli passed with 213
tests, and cargo fmt and diff checks passed.

Status: implemented.

### R-019: Use one private schema for fixed Libkrun launch argument parse and encode

Category: refactor

Related simplification: S-015

Evidence: Config::parse checks a fixed positional argument layout and usage
text at [lib.rs](/Users/raul/W/omnifs/crates/omnifs-libkrun/src/lib.rs:304).
Config::arguments lists the same flags separately at :374, and tests
reconstruct the list at :615.

Current owner: parser, encoder, and tests each maintain the same positional
schema.

Expected owner: a small private fixed flag specification drives strict parse
and encode. It is not a general command parser.

Concrete change: define the flag names, order, and value slots once. Preserve
strict length, order, unknown-argument, and error-index behavior.

Preserved invariants: flat launch argument contract and usage/error semantics.

Compatibility effects: helper launchers may depend on exact errors or order.

Falsifier: platform-specific layouts differ or a schema table makes strict
positional checks less readable.

Estimated impact:

- production and test-support LOC: 25 to 50 fewer;
- test LOC: retain flat wire contract tests;
- named declarations: one private fixed schema;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: parse, encode, round-trip, and bad-argument tests.

Implementation: added the private FIXED_ARGUMENT_FLAGS schema and
parse_fixed_arguments in
[lib.rs](/Users/raul/W/omnifs/crates/omnifs-libkrun/src/lib.rs:47).
Strict positional parsing and encoding now use one flag order and preserve the
existing usage text, length checks, order checks, and flat launch contract.

Checks: cargo test -p omnifs-libkrun passed with 10 tests, and cargo fmt and
diff checks passed.

Status: implemented.

### R-020: Share the locked bootstrap removal body

Category: refactor

Related simplification: S-016

Evidence: remove_daemon_bootstrap_if and remove_published_bootstrap_if both
lock, read, compare identity, remove the socket, remove the identity file, and
return in [bootstrap/lib.rs](/Users/raul/W/omnifs/crates/omnifs-bootstrap/src/lib.rs:144)
and :185.

Current owner: two public methods duplicate the cleanup body.

Expected owner: a private locked cleanup function. Public methods retain their
distinct lock entry and wrapper names.

Concrete change: extract only the body after lock acquisition. Do not add a
second lock or change removal order.

Preserved invariants: lock scope, expected identity comparison, socket and
identity cleanup, errors, and return values.

Compatibility effects: internal implementation.

Falsifier: the methods differ in file ownership, lock semantics, or cleanup
order despite their current shape.

Estimated impact:

- production LOC: 15 to 30 fewer;
- test LOC: unchanged;
- named declarations: one private cleanup helper;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: bootstrap replacement and cleanup tests.

Implementation: added the private remove_bootstrap_if_locked helper in
[bootstrap/lib.rs](/Users/raul/W/omnifs/crates/omnifs-bootstrap/src/lib.rs:175).
Both public methods keep their distinct lock acquisition and call the helper
after the lock is held.

Checks: cargo test -p omnifs-bootstrap passed with 7 unit tests and 0 doctests;
cargo fmt --check and just check also passed.

Status: implemented.

### R-021: Remove the one-caller AuthManifestView wrapper

Category: polish

Related simplification: S-017

Evidence before the change: `AuthManifestView` was defined in
 [manifest_view.rs](/Users/raul/W/omnifs/crates/omnifs-cli/src/auth/manifest_view.rs:5)
and had one current caller in
 [mount.rs](/Users/raul/W/omnifs/crates/omnifs-cli/src/auth/mount.rs:43).
The wrapper delegated to `AuthManifest` and added one default-scheme policy;
its `mount_scheme` argument was always `None`.

Current owner: the named `static_token_scheme_key` policy function in
`manifest_view.rs` owns default and ambiguity handling for the CLI mount auth
flow. It has one production caller and no independent state.

Expected owner: the mount auth selection boundary or this named policy
function, not a one-caller view wrapper.

Concrete change: inline the policy or use one function named for the scheme
selection rule. Reintroduce a type if a second caller or a stable test seam
appears.

Preserved invariants: ambiguity errors and current default choice.

Compatibility effects: private CLI structure.

Falsifier: another caller appears, or the wrapper is a deliberate auth
boundary with a documented external contract.

Estimated impact:

- production LOC: 20 to 35 fewer;
- test LOC: preserve malformed and ambiguous manifest tests;
- named declarations: one wrapper removed or one policy function retained;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: CLI mount auth selection tests.

Implementation: removed the one-caller `AuthManifestView` state wrapper and
kept the default/ambiguity policy in the named
`static_token_scheme_key` function at
[manifest_view.rs](/Users/raul/W/omnifs/crates/omnifs-cli/src/auth/manifest_view.rs:5).
`Auth::static_token_scheme` is its only production caller, and the former
`mount_scheme` input was always `None`. The function still returns an explicit
requested key, the sole declared static scheme, the fixed `static-token`
fallback, and the existing ambiguity error. The semantic owner remains CLI
mount auth selection; no serialized or user-visible output changed.

Checks: `cargo fmt --all -- --check`, `cargo check -p omnifs-cli
-p omnifs-daemon`, and `git diff --check` passed. Focused and full tests remain
parent-owned.

Status: implemented.

### R-022: Reject the unused reverse TreeError conversion finding

Category: polish

Related simplification: S-009

Evidence: the forward conversion remains at
 [implementation.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/namespace/implementation.rs:122).
The reverse conversion is a current runtime path: host-child resolution returns
NsError and converts it into TreeError at
 [resolve.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/tree/resolve.rs:71).

Disposition: keep both conversions. The earlier workspace search missed the
feature-gated runtime caller, so deleting the reverse impl would break the
engine build.

Falsifier for reopening: the host-child resolver changes to return TreeError
directly and a feature-aware search finds no other NsError to TreeError
conversion.

Status: rejected.

### R-023: Prototype a typed provider operation gateway after R-007

Category: refactor

Related simplification: S-022

Evidence: Instance defines multiple transport aliases and command variants at
 [instance.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/runtime/instance.rs:94).
Six async methods repeat oneshot creation and reply mapping at :203 through
:335; the driver repeats generated WIT call and reply routing arms at :400
through :505. Synchronous initialize, callout setup, shutdown, and close paths
are separate at :337 through :370.

Current owner: each operation method and driver arm owns a piece of queue and
reply pairing.

Expected owner: after the runtime policy is chosen, a private typed gateway
may own admission, operation spans, cancellation, and typed reply routing.
Generated WIT calls remain operation-specific. Synchronous lifecycle calls
remain separate.

Concrete change: prototype one typed operation family. Keep the current
exhaustive command enum if the gateway needs type erasure or becomes a
continuation protocol.

Preserved invariants: WIT result types, provider trust boundary, lifecycle
ordering, and operation-specific spans.

Compatibility effects: high if a generic result or continuation crosses the
WIT boundary.

Falsifier: generated bindings cannot share routing without boxing or type
erasure, or the gateway only moves match arms without reducing state.

Estimated impact:

- production LOC: 100 to 220 gross lines may move or disappear;
- test LOC: add typed routing and cancellation tests;
- named declarations: one gateway and operation input family;
- generated surfaces: WIT bindings unchanged;
- dependencies: none expected.

Preferred tests to modify: instance command routing, provider operation
concurrency, lifecycle, and shutdown tests.

Disposition: conditionally retained after product-gated R-007. This wave did
not implement a typed provider gateway because queue admission, cancellation,
drain, and join policy remain undecided. The current exhaustive command enum,
operation-specific WIT calls, request IDs, and shutdown paths therefore stay
visible. A gateway is safe only after R-007 chooses those runtime semantics and
tests prove that typed routing does not become a continuation protocol.

Falsifier for reopening: R-007 supplies bounded admission and cancellation
rules, and a one-operation prototype preserves WIT result types, provider
trust boundaries, operation spans, and shutdown ordering without type erasure
or hidden retry state.

Status: conditionally retained.

### R-024: Avoid parsing the same effect paths twice with a validated effect view

Category: refactor

Evidence: ReturnValidator::effects parses canonical leaves, filesystem write
paths, and invalidation paths in
 [validate.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/ops/validate.rs:119).
EffectApplier::lower_effects parses the same raw strings again in
 [apply.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/effects/apply.rs:57),
:80, and :126. The lifecycle path validates and then lowers in
 [lifecycle.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/ops/lifecycle.rs:34).

Current owner: validation and lowering each parse and enforce part of the
same raw effect representation. This is partly intentional defense in depth,
so it is not a deletion finding by itself.

Expected owner: a checked internal ValidatedEffects view owns parsed paths and
validated attrs between validation and lowering. A raw-input wrapper can
retain the lowerer's defensive check for independent tests or future callers.

Concrete change: prototype a borrowed validated effect representation. Keep
the raw lowerer boundary until all callers use the checked view; do not remove
lowerer errors merely because the normal lifecycle validates first.

Preserved invariants: malformed provider effects remain rejected; reserved
leaves, duplicate IDs, byte limits, and path errors retain their current
messages or stable classifications.

Compatibility effects: internal engine errors and tests may observe which
stage rejects malformed data.

Falsifier: lowering has legitimate independent raw callers, or a validated
view duplicates more fields than the parse work it removes.

Estimated impact:

- production LOC: 10 to 50 fewer parsing and conversion lines, or neutral if
  defense in depth remains;
- test LOC: add validator-to-lowerer handoff tests;
- named declarations: one borrowed validated effect view;
- generated surfaces: none;
- dependencies: none.

Preferred tests to modify: operation validation, malformed effects, reserved
leaf, duplicate ID, and effect lowering tests.

Status: conditional.

## Suggested dependency order

| Wave | Findings | Reason |
|---|---|---|
| 0 | R-020 (completed) | Small local candidate with clear falsifiers |
| 1 | R-003 (completed), R-005 (safe progress slice completed), R-013 (completed) | Durable ordering and narrow helpers with strong source evidence |
| 2 | R-004, R-016, R-018, R-019 (completed); R-002 (discovery retained), R-008 (compatibility-gated), R-024 (conditional) | Safe owner and duplicate-policy slices completed; remaining items still need caller, downstream, or prototype evidence |
| 3 | R-006 (completed); R-012, R-014, R-023 (conditionally retained) | The terminal owner is proven; observation and protocol abstractions remain conditional, and the provider gateway waits for R-007 |
| 4 | R-011, R-021 (completed); R-007 (product-gated), R-009 (compatibility-gated), R-010 (discovery retained), R-017 (deferred) | Low-risk internal cleanups completed; remaining items need product, downstream, input-boundary, or transport evidence |

R-001, R-015, and R-022 are excluded because source review rejected them. Do
not start a later wave by introducing a shared type whose owner is still
ambiguous in an earlier wave.

## Non-findings and rejected moves

The following were inspected and deliberately not reported as refactors:

- Provider attributes, engine cached attributes, VFS attributes, and FUSE/NFS
  replies. They represent different trust, cache, wire, and OS protocol
  boundaries.
- API declarations versus SQLite desired state. The duplication is an
  intentional control-plane boundary, not two authorities.
- ResolvedMount, RuntimeMountConfig, MountBuildInput, and API mount records.
  Sol's review found legitimate boundary projections, not one duplicated
  daemon policy. Keep the field inventory as a future falsifier.
- SessionKey, SessionEntry, and public Session. The key, registry entry, and
  snapshot have different identity and lifetime roles.
- TreeError and NsError. The reverse conversion is used by the host-child
  resolver at [resolve.rs](/Users/raul/W/omnifs/crates/omnifs-engine/src/tree/resolve.rs:71);
  the prior unused-code finding was false.
- Runtime facts versus durable Filesystem observations. They use different
  clocks and recovery meanings.
- BodyStore, ProjectionStore, and generation-local MountResources. Their
  lifetimes and ownership differ.
- EngineNamespace as a single semantic facade. Its implementation is large,
  but the contract names it as the sole projection owner. Splitting it into
  fake semantic layers would duplicate authority. Private file organization
  can be considered only after a state-owner map.
- RuntimeDriver backend variants. FUSE, NFS, and libkrun launch behavior is an
  adapter boundary; a trait is not justified without two stable callers and a
  shared invariant.
- A universal lifecycle enum for state, API, progress, runtime, and runner
  phases. This was rejected because the phase families have different clocks,
  persistence, and wire meanings.
- A generic WIT terminal or continuation protocol. The current typed export and
  effect contract is a binding invariant.
- Broad renaming of ordinary variables or top-level functions without a
  changed owner, invariant, or caller contract.

## Maintenance rule

Before implementing an entry, rerun the caller, writer, serializer, generated
binding, feature, and test searches named in its falsifier. If the falsifier
fires, mark the entry rejected or compatibility-gated instead of widening the
refactor. After implementation, update the relevant contract or architecture
note when ownership or behavior changes, then run the narrowest focused tests
and just docs-check.
