# omnifs: target architecture and in-place delivery plan

This document describes the clean shape omnifs should have and the sequenced, in-place work to reach it. Every change is an independently-shippable PR on `main` that keeps the branch green and the live runtime working; there is no fork and no cutover. Part one is the target architecture. Part two is the delivery plan.

## What omnifs is

omnifs projects external systems (APIs, services, datasets, live feeds) into the local filesystem as directories and files, so the standard Unix toolbox, any agent, and any application can read them with no SDK and no API client. The division of labor is the load-bearing idea: **providers own meaning** (what paths exist and what bytes they hold), the **host owns trust** (auth, capability enforcement, caching, callout execution, I/O), and **frontends** translate one shared projected tree into operating-system filesystem behavior.

## Vocabulary

The terms used throughout this document.

- **Projection.** The lazy, on-demand mapping of an external system into paths and bytes: materialize the slice you touch, when you touch it, rather than syncing ahead of need.
- **Upstream.** The external service or data source a provider projects.
- **Provider.** A sandboxed WASM component (`wasm32-wasip2`) that defines the paths, bytes, and object meaning for one upstream. Untrusted by default, even when built in this repo.
- **Host.** The trusted runtime that owns auth, capability enforcement, caching, callout execution, namespace state, and I/O.
- **Frontend.** A host-side surface that exposes the projected tree to an OS: FUSE on Linux, NFSv4 loopback on macOS.
- **Mount.** A configured provider projection rooted into the served tree.
- **Mount spec (`Spec`).** The on-disk definition of one mount (which provider, its config, its auth, its capability grants), one file per mount, owned atomically by `mount::Registry`.
- **Worldview.** A scoped, auditable projection bound to a consumer (an agent, a human, a team): which subtrees that consumer sees and under what read and write policy. An authority concept; section 4 explains why it is not yet a type.
- **Serving context.** The set of mounts a single request is served against. Today there is exactly one, holding every configured mount, with no scope or policy. The non-authority precursor to a worldview.
- **Path.** A validated, absolute, UTF-8 protocol path used inside the SDK and host (`omnifs_core::path::Path`), distinct from a host filesystem path.
- **Leaf.** A file node in the projected tree, as opposed to a directory.
- **Object.** A typed value a provider assembles from one replayable canonical payload; the unit of provider-side domain meaning.
- **Canonical.** The bytes an upstream returned, captured verbatim and stored as the durable source of truth for an object.
- **Render, representation, computed.** Render is SDK-side assembly of an object's bytes into another content type. A *representation* is a render registered in a render table (for example a Markdown view of a JSON issue); a *computed* leaf is an arbitrary derived file with no render-table entry.
- **Subtree.** A leaf or subtree served from a real host directory (a git clone, an extracted archive) rather than from provider bytes; a provider declares one with the `treeref` route verb.
- **Collection.** A typed, listable set of objects; the single way a directory of objects is listed.
- **Key, LogicalId, Facet.** Identity machinery: `Key` is identity plus route context; `LogicalId` is the cache key (object kind plus ordered identity captures); a `Facet` is a path capture deliberately excluded from identity, so two routes can share one cached object.
- **Callout.** Work the host runs on a provider's behalf (an HTTP request, a git clone, a blob fetch) while the provider awaits the result. Every callout is request/response; there are no fire-and-forget callouts.
- **Effect.** The single channel a provider return uses to mutate host state: write canonical bytes, write files, invalidate cache entries. *Materialization* is the host applying those effects.
- **Route, Router, seal.** A provider registers routes (path templates bound to objects or files) on a `Router` during `start`; `seal` validates the route tree once; dispatch is read-only after seal.
- **WIT contract.** The component interface (`crates/omnifs-wit/wit/provider.wit`) defining callouts, effects, and the provider's exported functions: the boundary between the trusted host and the untrusted provider.
- **Capability: `Need`, `Grant`, `Allowlist`.** A provider's manifest declares the capabilities it *needs* (a domain, a git repo, a unix socket); a mount spec *grants* them; the host resolves grants into a runtime *allowlist* enforced before every callout. Declaration, authorization, and enforcement as three named layers.
- **Sandbox.** The WASM component boundary plus the capability allowlist: a provider can reach only what its grants permit.
- **Object cache, view cache, generation fence.** The object cache is the durable store of canonical bytes (an object's home); the view cache is a disposable, restart-wiped tier of derived attributes and listings; the generation fence rejects any write that predates an invalidation.
- **Async provider runtime.** Providers run as async Wasmtime components; a per-instance driver thread drives the store so multiple operations on one instance can suspend on callouts concurrently.
- **Reconcile.** The daemon loop that brings running mounts into line with the on-disk specs (add, remove, upgrade).

## The shape of the problem

The engine is sound. The provider contract (WIT callouts and effects), the sandbox, the cache coherence model, the typed SDK object model, and the capability language are well-designed and tested, and they have grown stronger over time: providers now run on an async Wasmtime component runtime, provider metadata is harvested host-side, and the host-native mount works on macOS (NFSv4 loopback) and Linux (FUSE).

The debt is not crate count and not the engine. It is three things:

- **Conceptual entropy.** Single concepts wear several names across crate boundaries: "verbatim upstream bytes" is four types, "a directory bound in from the host" is three, and two object byte-faces blur. Tracing a value across the system means relearning its type at every seam.
- **A handful of misplaced boundaries.** Host-side cache types live in the wasm-safe leaf and leak into every provider; the OAuth engine depends upward on the crates that should depend on it; the projection core depends on the host runtime it pretends to sit beneath.
- **Control-plane shape, not size.** The CLI is large but mostly justified; its problem is module hygiene (a junk-drawer module, an inverted control-address dependency, a two-constructor pattern).

The work is to fix those in place, with one concept per name, one invariant per crate, and no abstraction that does not earn its keep.

## Constraints and north star

- **Stack is fixed:** Rust, and providers as `wasm32-wasip2` WASM components.
- **The provider contract may change.** Where a cleanup needs the contract to move, the change lands in place: the WIT, the metadata section, and all in-repo providers move together in one PR. There is no version negotiation and no old-artifact compatibility to maintain, which is how the repo already ships breaking provider-contract changes; the only discipline is to not change it gratuitously and never to leave a provider broken across a PR boundary.
- **Deliver today's behavior, cleanly,** with concepts and seams ready for the roadmap but no speculative machinery built ahead of need.
- **Simple core, clear seams.** Minimal core concepts; extension points only where a second real pressure exists.
- **Three bars, merge-blocking:** one concept / one name / one owner; each crate owns one invariant; no abstraction without two pressures.

---

# Part one: target architecture

For each piece: **leave** (it is correct, keep it), **refactor** (change shape in place), or **delete** (remove).

## 1. Leave, refactor, delete

### Leave

- **WIT callout/effects contract shape.** Callouts are request/response async host imports (no fire-and-forget), `effects` is the single terminal host-mutation channel, and the data records (`canonical-store`, `fs-write`, `invalidation`, `file-attrs`, `dir-listing`) are clean. The shape stays; one variant (`capability-need`) changes where a unification needs it (section 9). The package label `omnifs:provider@0.4.0` is cosmetic and is not a compatibility guarantee.
- **The async provider runtime.** A per-instance driver thread owns a `current_thread` runtime and drives the store through `Store::run_concurrent` over a `FuturesUnordered` with no store mutex; `call_concurrent` invokes exported functions; `spawn_blocking` offloads slow git and blob callouts; `Instance` is a lock-free `Clone` handle. Same-instance concurrency is real and covered by an integration test. This is the latency answer at the runtime layer.
- **Capability language.** `Need` / `Grant<T>` / `Allowlist` in `omnifs-caps`, with the matching primitives owned once so the start-time satisfiability check and the runtime allowlist cannot drift, and a `Need::kind()`/`Need::value()` canonical projection. The cleanest vocabulary in the system; the model the rest should imitate.
- **Object model.** `Object` / `Key` / `LogicalId` / `Facet`: identity, route-context, and identity-neutral axis as three separated ideas, with one canonical payload serving every facet view.
- **Cache shape.** Durable object store (canonical's sole home), restart-wiped view tier, per-mount generation fence. "Canonical is never copied into the view cache" is load-bearing.
- **Auth and credentials.** `omnifs-creds` is a 0600 atomic locked file store; `omnifs-auth` drives the OAuth flows; the store is the only runtime credential source.
- **Host-side static metadata.** The `#[provider]`/`#[config]` macros emit a native `provider_metadata()` accessor; `omnifs-embed-metadata` links each provider as a native library and injects `omnifs.provider-metadata.v1` without instantiating WASM. `ProviderManifest` is the single wire type with a structured `config: ConfigMetadata`.
- **Two-phase Router/seal.** Register in `start`, validate at `seal`, read-only dispatch after.
- **Self-contained mount Spec.** `mount::Registry` is the sole atomic owner; `Spec.auth` is an `Option` (at-most-one is structural); `config_raw` is a plain `Option<Value>`; reads go through one `pinned_manifest` path.

### Refactor

- **Frontend dispatch.** The runtime interior is async, but every FUSE and NFS callback enters the runtime with `block_on` per op (`crates/omnifs-fuse/src/{filesystem,lookup,read,listing}.rs`), so end-to-end concurrency bottlenecks at the renderers. Make frontend dispatch async-first over the existing runtime.
- **The projection tree's framing.** `omnifs-tree` is the right cut but oversells independence: it depends on and drives the host `Runtime`, and carries a transitional private `wit_types` import. Describe it honestly as projection policy that drives the host Runtime, and have the host re-export a result type so tree drops its direct `omnifs-wit` dependency.
- **Shared renderer state.** FUSE and NFS each reimplement a `NodeEntry`, the live-follow size map, and the invalidation drain. Move a shared `RendererEntry` and a drain helper into `omnifs-tree`, which already owns the live-follow pump; each frontend keeps only its protocol-specific identity and teardown.
- **Provider SDK ergonomics.** Keep the deep model; clean the authoring surface (section 7).
- **Control-plane and CLI shape.** Fix module ownership (section 9).

### Delete

- **`Workspace<Role>`** (`omnifs-home`): a phantom type parameter that gates no method; callers route around it.
- **Dead view-cache methods** (`view::Cache::{invalidate, invalidate_entries_if, is_fresh}`): no production callers; one duplicates another.
- **The empty inspector allowlist** (`ALLOWLISTED_QUERY_KEYS = &[]`): a false affordance in a redaction path; collapse to "strip all params."
- **`DirProjection` as a public listing path:** `Collection` is the one listing surface.
- **Vestigial wrappers:** the host `Artifact` (one call site), `OAuthRequestConfig` (one caller), the six `callout_*` one-liners, three duplicate `From<RequestTokenError>` impls, the hand-rolled `CredentialEntryWire` serde, and the SDK macro's `unreachable!` blob arms and duplicated placeholder string.
- **The duplicate provider source of truth:** drive `omnifs-embed-metadata`'s provider set from `cargo metadata` instead of a hand-maintained list parallel to the Dockerfile's discovery.

## 2. Invariants the work must preserve

Each is enforced and tested today and is merge-blocking for every slice.

- Capability satisfaction is checked at provider start, fail-fast; a literal grant does not satisfy a dynamic need.
- Every callout passes the allowlist before dispatch: plain HTTP denied, private and link-local IPs denied for HTTPS, exact-path unix-socket matching.
- Provider returns carry no trailing callouts; error returns carry no effects, validated before any state mutation.
- Only `UnixSocket` and `PreopenedPath` may be dynamic, enforced at the manifest parse boundary and in the SDK macro.
- Credentials never appear on the wire, and the store is the only runtime credential source. Credential types must never derive `ToSchema`.
- Credential storage is 0600, atomic, advisory-locked; the parent directory is 0700.
- Reserved `@`-prefixed names cannot originate from provider data.
- Canonical identity bytes are never copied into the view cache.
- The view cache is wiped unconditionally on restart.
- NFS binds loopback only; filehandles carry a process-random generation prefix.
- Strict serde is enforced where present; loosening it is a gated decision.

Operational-plane gaps to close as part of the work:

- **Top-level mount `Spec` parsing is not strict** (no `deny_unknown_fields`), so a typo'd top-level key is silently ignored. Make it strict (a gated decision: surface and confirm).
- **`UpgradePlan::diff` under-binds re-consent:** it compares only `auth.default` and ignores endpoint, scope, inject-domain, and config field-type changes. Make the diff bind the full surface it gates.
- **`Path::from_validated` is checked only in debug builds.** Keep the unchecked fast path; document it as unsafe-by-convention.
- **`mount_fingerprint` uses `DefaultHasher`.** Switch to a stable hash (BLAKE3 over the spec JSON) for hygiene. This is cleanliness, not a correctness fix: `DefaultHasher` is deterministic per binary and the fingerprint map is in-memory only.

## 3. The cleaned conceptual model

One concept, one name, one owner. The clean axis (the trust/meaning split and the `Need`/`Grant` language) stays; the muddy axis (bytes in transit) is unified.

### Canonical noun catalog

| Concept | Owner | Name |
|---|---|---|
| Validated protocol path | `omnifs-core` | `Path` (+ `Segment`) |
| Provider identity triad | `omnifs-core` | `ProviderId` / `ProviderName` / `ProviderRef` |
| Credential identity | `omnifs-core` | `CredentialId` (keyed on name slug) |
| Content type | `omnifs-core` | `ContentType` |
| On-disk workspace layout | `omnifs-core` | `WorkspaceLayout` |
| Byte provenance, learned size, stability | `omnifs-view` | `ByteSource` / `FileAttrs` / `Stability` / `DirentsPayload` |
| Capability language | `omnifs-caps` | `AccessNeed` / `ResourceLimit` / `Grant<T>` / `Grants` / `Allowlist` |
| OAuth scheme and flow-config value types | `omnifs-auth-model` | `OauthScheme`, the flow configs |
| Object model | `omnifs-sdk` | `Object` / `Key` / `LogicalId` / `Facet` |
| Verbatim upstream bytes | one concept, role-named projections | `Canonical` |
| A leaf served from a real host directory | `omnifs-tree` | `Subtree` (authored via the `treeref` verb) |
| A listable set of objects | `omnifs-sdk` | `Collection` |
| Provider execution | `omnifs-host` | per-instance driver thread + `call_concurrent` |
| Protocol interaction directions | `omnifs-wit` | `Callout` (request/response) / `Effect` (single terminal mutation) |
| Renderer-side node identity | `omnifs-tree` | `RendererEntry` |
| The mount set a request is served against | `omnifs-mount` | `ServingContext` (section 4) |

### Conflations to resolve

- **"Canonical bytes" is one concept, not four types.** Today it is `sdk::object::Canonical`, `ByteSource::Canonical`, `cache::object::Canonical` + `StoredObject.canonical`, and the host accessor. The provider-author-facing name is `Canonical`; every other appearance is an explicitly role-named projection of it, never an independently-named struct.
- **"A directory bound in" is `Subtree`, one type.** Today one concept wears three names: `treeref` (the route verb), `Subtree` (the tree node body and the WIT `ListChildrenResult`/`LookupChildResult` wire variants), and `Backing` (the tree read result and fuse inode body). The owner is `omnifs-tree`; the author-facing route verb stays `treeref` (a `treeref` declares a `Subtree`), and the stray `Backing` occurrences are renamed up to `Subtree`. The WIT already uses `Subtree`, so this is contract-neutral. No `Backing`.
- **Object byte-faces are `canonical`, `representation`, `computed`.** `representation` is a render registered in the render table; rename `derive` to `computed` (an arbitrary derived file with no render-table entry) so the distinction is predictable.
- **`Need` splits into `AccessNeed` and `ResourceLimit`.** It currently unions access capabilities (checked by `Grants::satisfies`) and resource limits (excluded from satisfaction). The split changes the WIT `capability-need` variant (`provider.wit:421-428`), the SDK macro that emits it, and the manifest, so it lands as one PR that moves those together and rebuilds all providers; it also deletes the unreachable blob-byte arms in the macro.
- **Smaller:** `Grant<T>` moves from `#[serde(untagged)]` to a tagged form and drops the always-true `DynamicMarker.dynamic` bool; `Spec.mount` becomes `mount::Name` with the parse pushed to `from_file`; the two `Artifact` types and the vestigial `ProviderWasm` collapse to one; the daemon's `proc_mounts::MountInfo` is renamed to stop shadowing `api::MountInfo`.

### `omnifs-view`: cache types out of the wasm-safe leaf

The host-side cache-coordination types (`FileAttrsCache`/`FileAttrs`, `DirentsPayload`, `ByteSource`, `Stability`) live in `omnifs-core`, the wasm-safe protocol leaf, only because the dependency graph forbade a better owner. They leak into `omnifs-sdk` and every provider transitively. Extract `omnifs-view` at the bottom layer, depending only on `omnifs-core`. `omnifs-cache`, `omnifs-host`, `omnifs-tree`, and the frontends depend on it; `omnifs-sdk` and providers must not.

## 4. The serving context, and why "Worldview" waits

A worldview (see Vocabulary) is an authority concept: a scoped, auditable, transactional projection bound to a consumer, defining which subtrees that consumer sees and under what read and write policy. A type called `Worldview` that ships now but exposes every mount, filters nothing, and enforces no audit would be a policy-looking object a reader could mistake for tenant isolation or audit proof while the host still serves everything. That is a trust-boundary risk.

So the serving path takes a `&ServingContext`: the set of mounts a request is served against. Today there is one, it holds every configured mount, it is read-only, and it carries no scope, policy, or audit claim. It is honestly a serving context. Threading it through the serving path now means adding scope later is an addition, not a signature change.

The name `Worldview`, and the scope/policy/audit it implies, is introduced only when at least one scope or audit rule is enforced on every `list`/`lookup`/`read`/`open` path. That is a roadmap concern, out of this program's scope. `ServingContext` lives in `omnifs-mount`; no new crate for an unenforced concept.

## 5. Target crate shape

Each crate owns one invariant. The crate count is roughly flat; the structure is mostly sound and the fix is correct boundaries, not consolidation. Three boundaries move:

- **Extract `omnifs-view`** from `omnifs-core`: a new bottom-layer crate depending only on `omnifs-core`. `omnifs-sdk` and providers stop depending on it.
- **Add `omnifs-auth-model`** (a bottom-layer leaf holding the OAuth scheme and flow-config value types) and **demote `omnifs-auth`** to a leaf by removing its dependencies on `omnifs-mount` and `omnifs-provider`.
- **Fold `omnifs-home` into `omnifs-core`** as `omnifs_core::home`, dropping `Workspace<Role>`.

Plus: the host re-exports a result type so `omnifs-tree` drops its direct `omnifs-wit` dependency; `ServingContext` lives in `omnifs-mount`; `daemon` and `nfs` remain optional cargo features on the CLI.

Resulting layering, bottom-up: `omnifs-core`, `omnifs-caps`, `omnifs-wit`, `omnifs-api`, `omnifs-inspector`, `omnifs-auth-model`, `omnifs-view`; then `omnifs-creds`, `omnifs-auth`, `omnifs-cache`; then `omnifs-provider`, `omnifs-sdk-macros`; then `omnifs-mount`, `omnifs-sdk`; then `omnifs-host`; then `omnifs-tree`; then `omnifs-fuse` and `omnifs-nfs`; then `omnifs-daemon`; then `omnifs-cli`. Alongside: `omnifs-embed-metadata`, `providers/*`, `omnifs-itest`, and the `scripts/dev.ts` Bun contributor flow.

The trust-enforcement seam stays whole in `omnifs-host` plus `omnifs-caps`; fragmenting it would recreate the "where is trust enforced" ambiguity the invariants forbid.

## 6. Provider SDK

The deep model is kept; the authoring surface is cleaned. Providers already work against the current SDK and move with each SDK change in the same PR (co-edit producer with consumers). GitHub, Linear, and Docker are tracers: an SDK-surface change updates those three first and gates on their conformance plus a live smoke run before it is considered done.

- **Faces:** `canonical`, `representation` (render-table content-type view), `computed` (arbitrary derived leaf), plus the byte-source and directory faces.
- **`Collection<C>` is the only way to list objects,** with eager derived leaves; `DirProjection` is removed and Linear migrates to `Collection<Issue>`.
- **Two-stage registration** replaces the `Rc<RefCell<Option<Rc<...>>>>` late-binding cell: declarations hold unresolved data until `seal`, which constructs a plain `Rc<ResolvedChildView>`. The parallel `collections` and `collection_handlers` lists, matched today by string equality, merge into one declaration type.
- **Conformance and tracers gate** every authoring-surface change: the kernel-free `ConformanceTree` harness plus the tracer providers' live smoke.

## 7. Host, tree, frontends

- **Async runtime:** keep as is.
- **Frontend async-first dispatch:** replace per-op `block_on` in FUSE and NFS so the async interior is not wrapped in a synchronous exterior. This is the single open piece of the async story and it is narrow.
- **Tree:** projection policy that drives the host Runtime; public surface stays WIT-free; the private `wit_types` import is replaced by a host-re-exported result type.
- **Shared renderer state:** `RendererEntry` and the live-follow size map move into `omnifs-tree`. Diff the two `NodeEntry` structs field-by-field before promoting, since NFS carries filehandle-generation and stateid concerns FUSE does not; the shared type holds the common core and each frontend extends it.
- **Effect materialization:** the single-terminal-channel framing is structural (a return is terminal). Confirm multi-effect ordering and partial-failure on the success path, and that an invalidation effect deletes from the durable object store rather than only tombstoning the in-memory fence (section 12).

## 8. Control plane and CLI

The fix is module ownership, not new crates.

- **Move `daemon_addr()`** out of the inspector debugging module into a `control_addr` module the inspector imports; it is the control-plane address authority and lifecycle code depends on it.
- **Dissolve `session.rs`:** Docker constants and the materializer tests go to `launch`, `MountConfig` to `workspace`, utilities fold into callers.
- **Shape the remaining mass:** `runtime.rs`, `launch_backend.rs` (collapse the two-constructor `resolve`/`from_config` into one taking explicit overrides), `launch.rs`.
- **Keep `daemon`/`nfs` as optional features.**
- **Command surface:** init-centric (`omnifs init`/`up`/`status`, `init --reauth`), with mount management read as composing the default serving context.
- **daemon/REST boundary stays;** `omnifs-api` is the justified micro-crate that keeps CLI and daemon wire-compatible without coupling them. Fix the `MountInfo.provider_id`-holds-a-name wart at the next API major.

## 9. The provider contract

The async contract shape stays: callouts as async host imports, `effects` as the single terminal mutation channel, the data records. Where a concept unification needs the contract to change, it changes. The one case here is splitting `capability-need` into access and resource-limit arms (`provider.wit:421-428`). It lands in place: the WIT, the SDK macro and manifest, and every in-repo provider move together in one PR, rebuilt and green. (The `subtree`/`backing` unification keeps the WIT's existing `Subtree` variant, so it touches no contract.)

There is no version negotiation, and none is needed while every provider lives in this repo and rebuilds with the host. The package label `omnifs:provider@0.4.0` is cosmetic and carries no compatibility promise. Versioning and negotiation become relevant only when providers ship out of tree; that is a future concern, not a constraint on this work. The only discipline: do not change the contract gratuitously, and never leave a provider broken across a PR boundary.

---

# Part two: delivery

## 10. Acceptance criteria

Merge-blocking.

- **One concept, one name, one owner.** A repo-wide search for the retired names (`Backing`, a second `Artifact`, `DirProjection`, `Workspace<Role>`, `ProviderWasm`) returns zero hits; "canonical" is one concept plus role-named projections.
- **Each crate owns one invariant.** `cargo tree` shows `omnifs-sdk` and providers free of `omnifs-view`, `omnifs-auth` with no edge to `mount` or `provider`, `omnifs-tree` with no direct `omnifs-wit` dependency; trust enforcement lives only in `omnifs-host` and `omnifs-caps`.
- **No abstraction without two pressures.** No phantom type parameter that gates no method; no public method with no production caller; no permanently-empty allowlist constant; no authority-shaped type that enforces nothing.

## 11. Delivery phases

Every slice is an independently-shippable PR on `main`: small, producer co-edited with consumers in one pass, no compatibility shims beyond what the PR needs, `main` green and the live runtime working at every merge.

**Gate (every slice):** committed on `main`; a non-vacuous structural assertion passes (the specific `cargo tree` / grep / type check named below, so a blocked or no-op slice fails rather than passing vacuously); the kernel-free conformance harness and any slice-specific WIT-boundary test are green; slices touching the authoring surface also pass the tracer providers' conformance and live smoke. Gates are verified by exit code.

### Phase A: ground truth and dead-weight

- Capture the product-contract behaviors of section 12 as itests or recorded live-runtime checks, so later refactors have a regression net.
- Land the pure deletions (no contract impact): `Workspace<Role>`, the dead view-cache methods, the empty inspector allowlist, the vestigial wrappers, the SDK macro residue.
- Gate: deleted symbols return zero grep hits; conformance and host gate green; characterization checks committed and passing.

### Phase B: concept unification

- Contract-neutral renames: `Backing` renamed up to `Subtree` (the read-path and fuse occurrences; the WIT and SDK already use `Subtree`); `Canonical` role-naming; `representation`/`computed`; `Spec.mount: mount::Name`; single `Artifact`; the `proc_mounts::MountInfo` rename; tagged `Grant<T>` (an on-disk spec format change); `DirProjection` off the public surface with Linear migrated to `Collection<Issue>`.
- One contract-touching slice (the WIT changes): the `Need` to `AccessNeed`/`ResourceLimit` split across the WIT `capability-need` variant, the SDK macro, the manifest, and all providers.
- Make top-level `Spec` parsing strict (gated; surface and confirm first).
- Gate: zero-hit grep for retired names; every provider rebuilds and passes conformance, and the tracer providers pass live smoke; for the contract-touching slice, the `provider.wit` change is intentional and the providers move in the same PR.

### Phase C: boundary moves

- Extract `omnifs-view` from core; add `omnifs-auth-model` and demote `omnifs-auth`; fold `omnifs-home` into core; host re-exports a result type so `omnifs-tree` drops its direct `omnifs-wit` dependency.
- Gate: `cargo tree` assertions (sdk and providers free of view; auth no edge to mount or provider; tree no wit edge).

### Phase D: frontend async dispatch and shared renderer state

- Move `RendererEntry` and the shared drain into tree; replace per-op `block_on` in FUSE and NFS with async-first dispatch over the existing runtime.
- Gate: a concurrency check shows the frontends no longer serialize; the NFS loopback-only bind test, FUSE notify, and `tail -f` live growth are green; the `NodeEntry` field-by-field diff is recorded in the PR.

### Phase E: control plane and CLI

- `daemon_addr` to `control_addr`; dissolve `session.rs`; collapse the `launch_backend` two-constructor; init-centric command surface; thread `&ServingContext` through the serving path (default-only, no policy).
- Fix the `UpgradePlan::diff` under-binding and the `mount_fingerprint` stable hash.
- Gate: `omnifs init github && omnifs up && cat` round-trips host-native; an upgrade test detects an endpoint, scope, or config-type change as re-consent; the serving path carries `&ServingContext` with behavior unchanged.

Sequencing: A is first and unblocks everything. B is the bulk of the work, wide and largely parallelizable; its one contract-touching slice (the `Need` split) rebuilds all providers in the same PR. C, D, and E are largely independent once B lands. Provider tracers ride every authoring-surface slice rather than trailing in a batch.

## 12. Behaviors to validate before refactoring

Coverage of these product-contract behaviors is thin, and Phase A turns them into a regression net before any refactor moves the code that implements them:

- **Exhaustive listing:** where a single `ls`/`find` is guaranteed to page through the provider and see every entry (`crates/omnifs-host/src/pagination.rs`).
- **Effect ordering and atomicity on the success path,** and whether an invalidation effect deletes from the durable object store or only the in-memory fence.
- **Live growth:** `tail -f` streams, and `ls -l` shows learned sizes, end to end.
- **OAuth loopback listener:** loopback-only bind, state/CSRF validation, port handling.
- **Inspector redaction:** what is captured and redacted (Authorization headers, tokens in query strings), beyond the empty allowlist branch.
- **Reconcile state machine:** the two-phase serial-then-parallel loop, its triggers, runtime add/remove/upgrade transitions, and failure isolation.
- **Acyclicity:** confirm the crate graph with `cargo tree`.

## 13. Risks and non-goals

- **Merge discipline.** Keep slices small, co-edit producers with consumers in one pass, and rebase often against an active trunk.
- **The one contract-touching slice** (the `Need` split) changes the WIT and rebuilds all in-repo providers in the same PR; no old-artifact compatibility is kept, which is fine while every provider ships from this tree. Never leave a provider broken across a PR boundary.
- **Tracer discipline.** The nine providers move with each SDK change; skipping the GitHub/Linear/Docker gate on an authoring-surface slice surfaces provider-specific failures late.
- **`ServingContext` must not quietly grow policy.** If a later slice adds a scope filter, it graduates to `Worldview` with enforcement and tests on every `list`/`lookup`/`read`/`open` path.
- **The async risk is small and scoped:** the runtime concurrency is done; only frontend `block_on` removal remains.

Non-goals (the cleaned base picks these up later): writes-as-transactions, replica-as-asset enumeration and export, provider-push liveness and the subscribe callout family, per-consumer scoping and the policy plane (the `Worldview` graduation), agent-legibility files, and provider-contract versioning and negotiation (only relevant once providers ship out of tree). None is scaffolded with empty seams.
