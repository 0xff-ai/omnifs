# AGENTS.md

`omnifs` projects external services as native frontend filesystems. The host owns trust, caching, auth, and I/O; providers own meaning (what paths exist, what bytes they hold). That one boundary explains most of what follows. Why the system is shaped this way: `docs/design/architecture.md`.

## Vocabulary

Fuller definitions live in `docs/design/architecture.md`.

- **Projection.** A mapping of an external system into paths and bytes, responding to some path access.
- **Canonical.** Bytes returned from the upstream as-is, stored in the canonical cache.
- **Provider.** A sandboxed WASM component (`wasm32-wasip2`) that defines what paths exist and what bytes they hold for one service.
- **Host.** The trusted runtime (daemon, caches, auth, I/O) that owns trust and drives providers.
- **Frontend.** A host-side surface that exposes the projected tree to an operating system.
  - Supported: FUSE (default on Linux), NFSv4 (default on macOS).
- **Object.** Canonical bytes representing an object: identity, canonical bytes, and the files derived from them.
- **Render.** SDK-side assembly of an object's canonical bytes. A provider concern, never a frontend.
- **Callout.** A host-run effect a provider suspends on (for example an HTTP fetch); the host executes it and resumes the provider.
- **Effect.** The single terminal channel a provider returns (cache writes, invalidations).
- **Preload.** Any resource a provider returns other than the one requested.
- **Mount, spec.** A mount projects one provider under a path; the spec (JSON) is the wire and config truth (`Spec`, `Resolved`, `Auth`).

## Invariants

- The host owns trust. Providers are untrusted. A change that weakens this is wrong.
- **Byte boundary.** The host knows only paths, bytes, content types, and file attributes. 
- **No new provider authority without sign-off.** Granting provider WASM more reach (new callout families, preopens, process or socket effects) changes the security model. Treat it as a gated decision (below), never a casual one.
- **Honest about its limits.** The boundary stops confused-deputy and lateral-movement attacks; it does not stop a determined hostile provider exfiltrating through its allowed domains. Do not weaken it, and do not over-claim it in code or docs.

## Responsibilities

- All object reasoning (identity, canonical assembly via render, versioning, preload, revalidation) lives SDK-side.

## How these rules bind

Sections are tiered by how hard they bind and how they change. Read the tier before treating a line as a wall.

- **Invariants.** Three of them, below. A change that breaks one is wrong; if a task seems to require it, stop and surface it. Security gets no alpha pass.
- **Gated decisions.** Allowed, but never silent. Surface the tradeoff and get sign-off in the same change.
- **Direction.** Where the architecture is heading. Strong, but not a wall: `omnifs` is early alpha and breakage is expected, so build with the grain and heed the "don't deepen" notes, while a deliberate, called-out departure is fine.
- **Current shape.** Today's architecture. The baseline to understand and compare against, not a constraint to preserve; expect it to churn.
- **Footguns** are contingent gotchas, each carrying the condition that makes it true. **Conventions** are judgment defaults.

Keep this file current as you work. In the same change: delete a footgun when its stated condition dies, update Current shape when you change the shape, and fix or add a rule when one proves wrong or missing in practice. Edit the file directly; do not let it drift.

### Product thesis: behaves like real files

The projected tree must behave like real files for the standard Linux toolbox, judged against every consumer, not one calling pattern. Every consumer (shells, scripts, editors, agents, applications) is served by the same mount; none may be special-cased. Prove no regression through `tests/smoke/` or a unit test when adding a feature. The toolbox to hold against:

- read: `cat`, `head`, `tail` (incl. `-f`/`-n`/`-c`), `less`, `xxd`, `hexdump`, `od`, `file`
- search and traverse: `grep -r`, `rg`, `find` (incl. `-name`/`-size`/`-type`), `fd`
- stat: `ls -l`/`-h`, `du -sh`, `wc`, `stat`
- copy and archive: `cp`, `mv`, `tar c/x/t`, `rsync`; compare and hash: `diff`, `cmp`, `*sum`; inspect: `jq`, `yq`, `xmllint`; editors: `vim`, `nano` (mmap editors best-effort)

## Agentic development

- If a task specifies a technology, library, or architecture, never substitute an alternative when blocked. Report the blocker, stop, and wait for approval. Completing with a substituted approach without explicit sign-off is forbidden.

## Gated decisions

Allowed, but never as a side effect. Surface the tradeoff and get sign-off in the same change.

- **Provider WASM authority.** New callout families, preopens, process or socket effects. Changes the security model the byte boundary rests on.
- **Auth or transport model.** Changing auth or transport (for example, clone over SSH versus HTTPS/token) changes the operational contract. Call it out; never switch silently.
- **Strict config parsing.** Mount specs set `deny_unknown_fields` so typos fail initialization loudly. Loosening it hides misconfiguration; do it only with explicit justification.

## Direction

Where the architecture is heading. Build with the grain; the "don't deepen" notes mark pieces in transition that should not accrete more weight. These are breakable with a call-out, not invariants.

- **Frontend-agnostic seam.** Traversal, caching policy, coalescing, invalidation, learned metadata publication, and root mount enumeration belong in the shared layer (today `omnifs-tree` over `omnifs-host`) that every frontend consumes, not in any one frontend. Two frontends (FUSE, NFSv4) already share it. Don't add this logic to a single frontend, and keep frontend-specific machinery in its own crate: FUSE inode tables, kernel notifier, and reply types in `omnifs-fuse`; NFS filehandles, stateids, and leases in `omnifs-nfs`.
- **Complete extraction, not parallel policy.** When a responsibility moves into `omnifs-tree`, remove the equivalent frontend implementation in the same change. A frontend may adapt `Tree` output to kernel or protocol types, but it must not keep a second copy of projection policy, cache schema knowledge, lookup semantics, root enumeration, size-learning rules, or provider probing.
- **Host-native delivery.** The daemon runs host-native, and the Docker container is one launch mechanism among others. Don't deepen container or Docker assumptions in the daemon.
- **Host owns caching.** The host owns all caching as opaque byte storage and evicts only by capacity or explicit invalidation. Providers do not add their own LRUs or time-based expiration.
- **Hot-path latency.** Warm reads should feel local. Don't turn provider latency into mount latency or add per-op blocking on the hot path. Not yet measured, so this is direction, not a gate.
- **Writes as transactions.** The read model is read-only today. When writes land they are explicit, atomic, and auditable (drafts under a draft namespace, executed by moving a prepared transaction into a control namespace), never a side effect of writing to a projected file. See `docs/future/mutations-via-git.md`.
- **Agent legibility.** The tree should explain itself: predictable naming, honest sizes, correct content types and extensions, self-describing schema and README leaves. Build providers legible rather than retrofitting it. A non-obvious keying scheme is a provider schema smell to fix, not a documentation problem to paper over.

## Current shape

Today's architecture. The baseline to understand and compare against; expect it to change, and update this section in the same change when it does.

- **Topology.** A single `omnifs` binary is both CLI and daemon: the runtime loop lives behind a hidden `omnifs daemon` subcommand that loads provider WASM components (`wasm32-wasip2`) and drives them through the `omnifs:provider` WIT interface. The CLI owns host credentials and the daemon lifecycle and talks to the daemon over an HTTP control API. `omnifs up` reads `[system].runtime` (`docker` or `native`, defaulting to native) and either runs the daemon in a container or spawns `omnifs daemon` as a detached host-native child; there is no separate `omnifsd` binary.
- **Contributor dev session.** `omnifs dev` is a blocking foreground session: the CLI brings up profile-selected db and k8s fixtures with Docker commands, copies provider WASM from `target/wasm32-wasip2/release/`, launches the FUSE runtime container, then opens an interactive shell at `/omnifs` inside it. Mount specs are discovered from `providers/*/dev/mount.json` filtered by `contrib/dev-profiles/`; exit or Ctrl+C tears fixtures and runtime down. `omnifs down` sweeps orphaned fixtures via `~/.omnifs-dev/session.json`.
- **Mount frontends.** Two frontends serve the same projected tree: FUSE (Linux kernel, used by native Linux and by the optional Docker runtime inside the container) and a read-only NFSv4.0 loopback frontend (`crates/omnifs-nfs`, started via `omnifs daemon --frontend nfs`) for non-Linux host-native integration, primarily macOS. Do not reintroduce macFUSE, `diskutil`, or macOS-specific FUSE mount behavior; use the NFS path for non-Linux host integration. Details: `docs/design/nfsv4-loopback-mount.md`.
- **Core pieces.** inode table, router, providers, caches, clone manager. Understand the current shape before changing it; see `docs/design/architecture.md`.
- **A provider in one breath.** One `#[omnifs_sdk::provider]` impl with a synchronous `fn start` that registers routes imperatively on a `Router`; it returns a terminal `op-result` with `effects`, or suspends on callouts the host runs and resumes. There is one route API: `file` and `dir` are the nouns, and the faces under them are the behaviors. `r.object::<O>` / `r.file_object::<O>` bind an `Object` (with `o.file(..).canonical/representation/derive/object/direct/blob/stream` leaves and `o.dir(..).collection/choices/children/tree` child topology); `r.alias` mounts the same object at a second template; raw `r.dir`/`r.file`/`r.treeref` handlers are the escape hatch. The `Object` trait (`crates/omnifs-sdk/src/object.rs`) carries `load`/`decode`/`type Canonical: Format`; `#[omnifs_sdk::object]` emits the impl and forwards `load` to an inherent `async fn`, `#[path_captures]` emits the `impl Key`. Faces, collections, preloads, and `Invalidation` are in the SKILL and `providers/DESIGN.md`; effect shapes are in `architecture.md` sections 2 to 6.

## Subsystem map

Read the owning doc before changing a subsystem; these are the source of truth, and they cite code rather than transcribe it.

| Area | Source of truth |
|---|---|
| Architecture invariants, decisions, rejected directions | `docs/design/architecture.md` |
| Provider authoring (read before writing or changing a provider) | `skills/omnifs-provider-sdk/SKILL.md`, `providers/DESIGN.md`, `crates/omnifs-sdk/src/lib.rs` rustdocs |
| Caching model | `architecture.md` section 3, `docs/design/object-cache-primary.md` |
| File attributes (size, stability, read-mode, byte source) | `docs/design/file-attributes.md` |
| Path dispatch and listing honesty | `architecture.md` section 6, `docs/design/path-dispatch-and-listing.md` |
| Mount frontends (FUSE, NFSv4 loopback) | `docs/design/nfsv4-loopback-mount.md`, `crates/omnifs-daemon/src/frontends.rs` |
| Auth, credentials, mount loading | `docs/design/host-auth.md` |
| Daemon and CLI split, control API | `docs/design/daemon-cli-split.md` |
| Build, runtime, debugging | `CONTRIBUTING.md` |
| Release and npm packaging | `RELEASING.md` |
| Roadmap and open opportunities | `docs/future/` |

## Validation

Baseline for host or CLI code:

```bash
cargo fmt
cargo nextest run
```

- **WASM toolchain (one-time).** Building provider and tool WASM (`just providers-build`, `just providers-check`, and the host integration tests, which rebuild providers on demand) needs the wasi-sdk. Install it with `just wasi-sdk`; it is idempotent (a matching `.cache/wasi-sdk/.version` stamp skips) and downloads the version pinned in `tools/versions.toml`. A fresh checkout or git worktree has no toolchain until you run it.
- **Provider or broad-surface changes.** Add `just check` (fmt, clippy and tests, wasm provider and tool checks, npm validate, docs link check), `just providers-check`, and `just providers-build`. The `justfile` is the human command surface (run `just` to list); keep recipes as thin wrappers over Cargo, Bun, Docker, and shell.
- **Mount, provider, clone, traversal, or runtime behavior.** Rust checks are not enough: validate through the live runtime (`omnifs dev`, then exercise the mount). The contributor runtime is `omnifs dev`, `shell`, `logs`, `status`, `down`; exact commands, the validation recipe, and the debugging runbook are in `CONTRIBUTING.md`.

Gate on the default members (`omnifs-cli`, `omnifs-host`, `omnifs-daemon`) and their dependencies. If validation cannot run, say so and describe the next best check.

## Footguns

Contingent gotchas. Each carries the condition that makes it true; delete the entry in the same change when its condition dies.

- **Gate on the default members, not the workspace.** `cargo check` and `cargo nextest run` build the host default members and their dependencies; that is the host gate. Never gate with `cargo check --workspace --all-targets`: it forces the wasm guest crates (`providers/*`, `crates/omnifs-tool-*`) onto the host target, where the wit-bindgen guest bindings (the `Guest` trait, the `export!` macro) do not exist, so it fails with `E0404`/`E0432` on `main` too. Those crates build for wasm via `just providers-build` / `just providers-check`. Condition: holds while the guest crates are non-default members.
- **Stale wit-bindgen codegen after a `.wit` edit.** Incremental builds can serve stale codegen and surface phantom errors downstream; run `cargo clean -p omnifs-wit` (or a clean build) before trusting a failure. Condition: holds while wit-bindgen generates guest bindings through the build.
- **Host integration tests rebuild providers, and rebuilding under nextest contends.** The `omnifs-host` integration tests (`runtime_test`, `pagination_test`, `object_cache_test`, the archive tests) call `ensure_providers_built`, which shells out to `just providers-build`. Two failure modes: a tree without the wasi-sdk cannot build at all (clang cannot find `libLLVM.dylib`), and nextest's process-per-test means many parallel processes each invoke the build and contend on the cargo target lock (`Blocking waiting for file lock`, intermittent LLVM errors). The reliable flow is build once, then skip in the tests: `just wasi-sdk` (one-time), `just providers-build`, then `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run …`. CI sets that env in its workflow and stages prebuilt WASM under `target/wasm32-wasip2/release/`. Condition: holds while the integration tests rebuild providers on demand.
- **Host checks may depend on generated provider artifacts.** Some host-side crates embed or inspect provider outputs at build time. If a host check fails because generated provider artifacts are missing, build the provider artifacts first or point the check at an existing artifact directory. Condition: holds while host crates consume generated provider bundles during build.
- **An object mounts at its `r.object` template, not at root.** The template is where the anchor projects, and a directory-shaped object's faces hang under it. Mounting an object at a captured-root template (`"/{paper}"` when the real anchor is `/papers/{paper}/{version}`) projects the whole tree in the wrong place. Mount at the real path, or use a detached `object(..)` handle plus `r.object`/`r.alias` to place it. Condition: holds while objects mount by template path.
- **Most seal-time checks fail only at component init, not `cargo check`.** `cargo check --target wasm32-wasip2` compiles a provider whose route tree is incoherent (a representation face with no canonical, two canonical faces, a collection child with no `r.object::<C>`, an alias missing key captures). The seal runs inside the host's `initialize`, so the gate is the host integration test `all_providers_initialize_and_seal` (`crates/omnifs-host/tests/runtime_test.rs`); run it after a route-surface change. Per repo convention there are no in-crate `routes_are_valid()` provider tests. Condition: holds while seal runs at component init and providers carry no unit tests.

## Conventions

Judgment defaults, not absolutes.

### General architecture hygiene

- **One fact, one owner.** Each piece of policy or truth has one authoritative type, function, config field, or document. Do not mirror it in another public surface unless the mirror is generated or mechanically checked.
- **Declarations must bind behavior.** A declaration that sounds like a permission, capability, schema rule, routing rule, cache contract, or validation guarantee must be enforced by the owning layer. If it is only commentary or intent, make it prose instead of API.
- **Keep ownership separate from placement.** Do not make a type or block own something merely because the path is nested there. Ownership follows the invariant and data source, not filesystem adjacency, module adjacency, or call-site convenience.
- **Model the boundary, not the workaround.** If several call sites need side parameters, special cases, fake variants, or bypass paths, stop and fix the missing domain boundary instead of standardizing the workaround.
- **One dispatcher owns precedence.** Any ordered decision tree that affects externally visible behavior must have one implementation. Other paths may lower the selected target, but must not reimplement the ordering.
- **Public API is a contract, not a sketchpad.** Exported types, enum variants, macro arguments, route verbs, and trait methods must have current users and clear invariants. Keep future possibilities private until they are proven by a real call site.
- **Abstractions need at least two honest pressures.** Add a trait, backend enum, strategy object, or generic helper only when there are multiple real implementations or a single implementation behind a genuinely volatile external boundary.
- **Prefer parsed forms after parse boundaries.** Once input has been validated into a domain type, keep carrying that type. Do not fall back to strings, maps, or JSON values for internal policy unless the format itself is the domain.
- **The final call site must read like the model.** If the main path still reads as registration plumbing, conversion choreography, or helper scaffolding around the real idea, the refactor is not done.
- **Delete bridge layers when the direct path exists.** Transitional adapters, duplicated DTOs, compatibility aliases, and one-caller forwarding helpers must disappear in the same change that makes them unnecessary.

### SDK and provider architecture

- **Providers use SDK constructs; a hand-rolled mechanism is an SDK gap to close.** A provider that reaches past the SDK to do its own HTTP, status mapping, caching, retry, or projection plumbing is evidence the SDK is not yet flexible enough. Extending the SDK so the construct fits is the first solution, not a per-provider workaround. When a provider needs behavior the construct does not offer, add a defaulted hook or a runtime-configurable shape to the shared type (as `Endpoint`/`EndpointHooks` do for base URLs and response classification) rather than dropping to a lower-level API in the provider. Surface the SDK change with the provider that motivated it.
- **Object blocks derive from canonical bytes.** An object block may declare representation leaves and projected leaves that can be served from the object's canonical payload. Do not put ordinary file or directory handlers inside object blocks; register non-canonical child routes explicitly in `start()`, or change the SDK object projection contract if the leaf genuinely needs the loaded object.
- **One file contract owns file truth.** Size, stability, read mode, content type, byte source, and version evidence must have one authoritative type per path. Do not add parallel provider-facing and wire-facing file structs that can disagree; if lowering needs another shape, keep it private and mechanical.
- **No false policy APIs.** A macro argument, manifest field, config key, capability, or doc sentence that claims enforcement must feed an enforced runtime or build-time decision. If it only type-checks, documents intent, or mirrors another source of truth, delete it until it becomes real policy.
- **Route dispatch has one owner for precedence.** Lookup, listing, read, and open must share the same route-target resolution rules. Do not copy precedence, static-sibling, object-leaf, or implicit-prefix logic across operation-specific dispatch functions; put the rule in a single resolved route/target model and lower per operation.
- **Fix structural gaps at the shared layer.** If multiple providers need empty handlers, duplicate routes, placeholder types, or boilerplate solely to satisfy SDK traversal or dispatch behavior, the shared model is missing a rule. Fix the shared layer instead of normalizing local scaffolding.
- **Avoid second shapes for one concept.** Do not add a parallel registration verb, block type, enum arm, or object shape for one provider's special case if an existing route target can express it directly. Preserve the user-visible path, but collapse the internal model.
- **Effects stay with the operation that earned them.** If an operation already has sibling bytes, canonical data, invalidations, or preloads in hand, return those effects from that same terminal. Do not invent a second route shape or protocol path merely to gain access to an effects channel.
- **Use typed paths inside SDK policy.** Once a path has crossed the parse boundary, carry `omnifs_core::path::Path` or segments through routing, object expansion, and effect assembly. Do not split and join provider paths as strings except at WIT or display boundaries.
- **Keep route topology visible in `start()`.** A provider's main path surface should be readable from its `start()` method. Avoid one-caller route-registration helpers or hidden DSLs unless they name a reusable domain subtree used in more than one place.
- **Config and schema must be wired.** Every config field in `#[omnifs_sdk::config]` and `omnifs.provider.json` must affect runtime behavior or be deleted. Defaults in schema, Rust config, endpoint types, and manifests must describe the same value.
- **Config schema changes are whole-contract changes.** Adding, removing, or renaming a provider config key must update every producer and consumer of that config: runtime parsing, manifests, generated defaults, docs, examples, fixtures, and integration specs. Grep the old key across the repo before calling the schema change done.
- **No speculative public SDK surface.** Do not export marker formats, tuple impls, handles, builder states, enum variants, or methods for anticipated providers. Add public SDK surface when a provider or host path uses it and the invariant is clear.
- **No fake extensibility enums.** A one-variant enum, backend selector, transport trait, or strategy trait is not a boundary. Use the concrete type until a second implementation exists and the call sites prove the abstraction.
- **Provider test seams belong at host or SDK boundaries.** Product providers should not grow local fake transports or in-crate unit tests for callout behavior. Put reusable protocol logic in the SDK and verify provider behavior through host integration tests, provider fixtures, or the live runtime path.
- **Dependencies must pay rent.** Before adding or keeping a direct dependency, confirm the crate uses it directly rather than through an SDK re-export or macro side effect. Delete unused direct deps in the same change that makes them unused.

- **WIT coordination.** When you change the WIT contract (`crates/omnifs-wit/wit/provider.wit`), rebuild all providers and update the affected docs in the same change. Breakage is expected in alpha; the rule is keeping providers and docs in step, not preserving the contract.
- **Small, local changes.** Keep changes small and local. When a refactor touches clone, routing, or traversal, compare against pre-refactor behavior before accepting the new result.
- **`omnifs-tree` owns projection semantics.** If the behavior answers "what projected node exists here?", "what bytes or attrs does this node have?", "what cache entry should be published?", "what root children exist?", or "what provider probe is needed?", put it behind a `Tree` API. Do not make FUSE and NFS each rediscover that answer through registry access, direct provider calls, or cache-key inspection.
- **Frontend crates translate, they do not decide.** `omnifs-fuse` and `omnifs-nfs` may own inode numbers, filehandles, stateids, leases, kernel notifications, reply construction, and protocol-specific error mapping. They must not own provider WIT calls, projection cache formats, root mount discovery, learned-size publication, inline-byte read policy, preload policy, or negative lookup policy.
- **Cache schema stays below frontend adapters.** Frontends may request a cached result through a shared API, but they should not match on cache payload variants, build projection cache keys, or know which cache entry shape stores attrs versus bytes. If a frontend needs that knowledge, add a narrow `Tree` or host method and delete the frontend-side schema branch.
- **Learned sizes live with file attrs.** Rules that infer, preserve, reject, or publish exact sizes from complete reads, EOF-ranged reads, inline bytes, or live files belong on `FileAttrsCache` or `Tree`. A frontend can copy returned attrs into its own protocol metadata, but it must not reimplement size-learning predicates or decide whether a learned size is authoritative.
- **Root enumeration is a tree operation.** The synthetic mount root is part of the projected tree, even when a frontend exposes protocol-specific aliases such as the NFS export name. Frontends should ask `Tree` for root children and only handle protocol quirks after the tree answer is known.
- **Negative lookups are shared semantics or no semantics.** Do not add per-frontend negative probe caches, dotfile exceptions, or lookup suppression lists. If negative lookup behavior is needed for correctness or performance, model it once in `omnifs-tree` or host lookup policy with invalidation rules and cross-frontend tests.
- **Live growth needs one owner.** Follow-mode reads, growing sizes, EOF discovery, and invalidation for live files must be governed by a shared live-file model. Frontend pumps are acceptable only as protocol delivery machinery; they must not own the semantic rules for when a live file grows, when EOF is learned, or when cached attrs change.
- **No fake provider DTOs in frontends.** Do not construct placeholder WIT records, fake provider files, or schema-shaped values merely to reuse frontend code paths. Use neutral core/tree types for shared state, then convert once at the frontend boundary.
- **No extraction without deletion.** A refactor that adds a shared `Tree` method is incomplete until the matching FUSE and NFS branches, helpers, tests, and direct dependencies disappear or are explicitly justified as protocol machinery. Check `cargo tree -p omnifs-fuse -e normal --depth 1` and `cargo tree -p omnifs-nfs -e normal --depth 1` when moving responsibilities out of frontends.
- **Name the owning type before writing a helper.** A helper whose first argument is always the same struct or enum usually belongs on that type. A helper that repeatedly takes the same context pair, such as `(Runtime, Path)` or `(mount, path)`, usually wants a small adapter object with methods.
- **Do not mirror constructors locally.** If two modules need to build the same domain value (`Node`, `Entry`, `EntryMeta`, mount options, protocol targets), put the constructor or projection method on the domain type. Local `provider_*`, `*_node`, `*_entry`, and `*_meta` builders are allowed only when they add protocol-specific state that the shared type must not know.
- **Predicates belong with the invariant.** Questions such as "is this ranged?", "is this exact?", "should this preserve a learned size?", or "is this synthetic?" belong on the type that owns the data. Do not scatter `matches!` ladders across adapters for enums owned in this repo.
- **A defended free function is a design smell.** If a comment explains why a free function cannot be a method, reread it as evidence for the missing owner. Introduce the owner when it is real; keep the free function only for stateless parsing, wire encoding, tiny OS adapters, or orphan-rule boundaries.
- **Post-refactor shape scan.** After moving behavior into a shared layer, grep for the old nouns and helper names in sibling crates. Remove local mirrors, direct dependency leaks, one-call wrappers, duplicate predicates, and helper tests that only pin the old plumbing.
- **Test harness duplication stays quarantined.** Tests may seed narrow fixture shapes when exposing production constructors would widen the API for no runtime value, but do not let test helpers justify duplicate production policy.
- **Project what you fetched.** If a handler holds an upstream payload, emit every derivable sibling and child instead of returning one field and forcing later refetches.
- **Provider maps use `hashbrown::HashMap`.** It keeps provider internals predictable across WASI targets.
- **Naming.** Reuse source-of-truth terms; do not invent names for public surfaces unless the rename is explicit; keep host internals out of SDK and WIT naming.
- **Path type naming.** Refer to `omnifs_core::path::Path` as `Path`; never alias it (no `ProtocolPath`). A module that also needs the stdlib path type imports `std::path::Path as StdPath`, so the bare `Path` always means the omnifs path. Modules that use only stdlib paths need no alias.
- **Abstraction fit.** Do not reuse an existing abstraction if it changes the behavior model; semantic fit matters more than code reuse.
- **Protocol changes.** Write the exact interaction trace first and reject extra hops on hot paths; if something is conceptually one-way, fix the boundary rather than forcing it through request and response machinery.
- **`From`/`TryFrom` at type boundaries** over `foo_to_bar` free functions for true one-to-one mappings. The exceptions and the existing conversion hubs are in `CONTRIBUTING.md`.
- **CLI presentation versus schema types.** `omnifs-mount` types (`Spec`, `Resolved`, `Auth`) are wire and config truth: keep human-facing labels and formatting out of them, format capability display at the use site through `crates/omnifs-cli/src/capability.rs`, and keep `*_to_json` DTO serialization separate from schema conversions.
- **Simplest honest flow.** Prefer the simpler end-to-end flow over the purer local abstraction, single-phase over multi-phase on the hot path, and data kept near where it is produced and consumed. Once the direct path exists, remove bridge-style dispatch layers and transitional glue.

### Test quality

- Prefer tests that protect behavior the project depends on: user-visible workflows, domain invariants, security and auth boundaries, persistence and wire-format compatibility, or regressions that are easy to reintroduce.
- Avoid tests whose only value is confirming library or plumbing behavior (serde, clap, url working as documented; a wrapper forwarding fields; a builder storing what it was passed; an in-memory fake round-tripping data; brittle wording checks).
- Before adding a narrow test, name the regression it catches and why that regression matters.
- Providers carry no in-crate `#[cfg(test)]` modules; verify provider behavior through host-driven integration tests and the live runtime path.

## Documentation

Docs are organized by what changes at what rate, and they cite code rather than copy it.

- **`docs/design/architecture.md`** is the invariants-and-decisions home: the load-bearing rules, their rationale, and the rejected directions. Change it when an invariant or a deliberate constraint changes, not when an implementation detail moves.
- **Layout.** `docs/design/*` is subsystem detail; `docs/future/*` is roadmap; `docs/internal/*` is gitignored strategy. The WIT and the code are the source of truth for the contract.
- **Cite, don't transcribe.** Docs cite symbols and files; they do not transcribe WIT blocks, struct definitions, or crate layouts. A transcribed code shape is a drift liability. Most docs that went stale did so by mirroring code.

Two triggers that drift the docs fastest:

- **A WIT or effects change** updates `architecture.md` (the affected sections) and any affected `design/*` doc in the same change.
- **A crate, type, or registration-verb rename** updates the docs that name it. Grep `docs/` for the old name before calling the rename done.

`just docs-check` (in `just check` and CI preflight) fails on any doc that references a nonexistent `docs/` path. It does not check code paths; that is what the cite-don't-transcribe doctrine is for.

## Editing this file

Repository-local guidance for working in `omnifs`, shared by every coding agent (Claude, Codex) and human contributor. `CLAUDE.md` is a symlink to this file; keep it self-contained, because other agents read it directly and do not expand `@imports`.
