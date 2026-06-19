# AGENTS.md

Repository-local guidance for working in `omnifs`, shared by every coding agent (Claude, Codex) and human contributor. `CLAUDE.md` is a symlink to this file; keep it self-contained, because other agents read it directly and do not expand `@imports`.

`omnifs` projects external services (GitHub, DNS, arXiv, Docker, Linear, Kubernetes, a SQL database) as a Linux filesystem. The host owns trust, caching, auth, and I/O; providers own meaning (what paths exist, what bytes they hold). That one boundary explains most of what follows. Why the system is shaped this way: `docs/design/architecture.md`.

## Vocabulary

The load-bearing terms, disambiguated. Fuller definitions live in `docs/design/architecture.md`.

- **Projection.** Lazy, on-demand mapping of an external system into paths and bytes.
- **Provider.** A sandboxed WASM component (`wasm32-wasip2`) that defines what paths exist and what bytes they hold for one service.
- **Host.** The trusted runtime (daemon, caches, auth, I/O) that owns trust and drives providers.
- **Frontend.** A host-side surface that exposes the projected tree to an operating system (FUSE on Linux; NFSv4 loopback host-native). Distinct from render.
- **Render.** SDK-side assembly of an object's canonical bytes. A provider concern, never a frontend.
- **Object.** The SDK-side unit of meaning: identity, canonical bytes, and the files derived from them.
- **Callout.** A host-run effect a provider suspends on (for example an HTTP fetch); the host executes it and resumes the provider.
- **Effect.** The single terminal channel a provider returns (cache writes, invalidations).
- **Preload.** Any resource a provider returns other than the one requested.
- **Mount, spec.** A mount projects one provider under a path; the spec (JSON) is the wire and config truth (`Spec`, `Resolved`, `Auth`).

## How these rules bind

Sections are tiered by how hard they bind and how they change. Read the tier before treating a line as a wall.

- **Invariants.** Two of them, below. A change that breaks one is wrong; if a task seems to require it, stop and surface it. Security gets no alpha pass.
- **Gated decisions.** Allowed, but never silent. Surface the tradeoff and get sign-off in the same change.
- **Direction.** Where the architecture is heading. Strong, but not a wall: `omnifs` is early alpha and breakage is expected, so build with the grain and heed the "don't deepen" notes, while a deliberate, called-out departure is fine.
- **Current shape.** Today's architecture. The baseline to understand and compare against, not a constraint to preserve; expect it to churn.
- **Footguns** are contingent gotchas, each carrying the condition that makes it true. **Conventions** are judgment defaults.

Keep this file current as you work. In the same change: delete a footgun when its stated condition dies, update Current shape when you change the shape, and fix or add a rule when one proves wrong or missing in practice. Edit the file directly; do not let it drift.

## Invariants

### Security and trust boundary

The host owns trust; providers are untrusted. A change that weakens this is wrong.

- **Byte boundary.** The host knows only paths, bytes, content types, and file attributes. All object reasoning (identity, canonical assembly via render, versioning, preload, revalidation) lives SDK-side.
- **No new provider authority without sign-off.** Granting provider WASM more reach (new callout families, preopens, process or socket effects) changes the security model. Treat it as a gated decision (below), never a casual one.
- **Honest about its limits.** The boundary stops confused-deputy and lateral-movement attacks; it does not stop a determined hostile provider exfiltrating through its allowed domains. Do not weaken it, and do not over-claim it in code or docs.

### Product thesis: behaves like real files

The projected tree must behave like real files for the standard Linux toolbox, judged against every consumer, not one calling pattern. Every consumer (shells, scripts, editors, agents, applications) is served by the same mount; none may be special-cased. Prove no regression through `tests/smoke/` or a unit test when adding a feature. The toolbox to hold against:

- read: `cat`, `head`, `tail` (incl. `-f`/`-n`/`-c`), `less`, `xxd`, `hexdump`, `od`, `file`
- search and traverse: `grep -r`, `rg`, `find` (incl. `-name`/`-size`/`-type`), `fd`
- stat: `ls -l`/`-h`, `du -sh`, `wc`, `stat`
- copy and archive: `cp`, `mv`, `tar c/x/t`, `rsync`; compare and hash: `diff`, `cmp`, `*sum`; inspect: `jq`, `yq`, `xmllint`; editors: `vim`, `nano` (mmap editors best-effort)

## Gated decisions

Allowed, but never as a side effect. Surface the tradeoff and get sign-off in the same change.

- **Provider WASM authority.** New callout families, preopens, process or socket effects. Changes the security model the byte boundary rests on.
- **Auth or transport model.** Changing auth or transport (for example, clone over SSH versus HTTPS/token) changes the operational contract. Call it out; never switch silently.
- **Strict config parsing.** Mount specs set `deny_unknown_fields` so typos fail initialization loudly. Loosening it hides misconfiguration; do it only with explicit justification.

## Direction

Where the architecture is heading. Build with the grain; the "don't deepen" notes mark pieces in transition that should not accrete more weight. These are breakable with a call-out, not invariants.

- **Frontend-agnostic seam.** Traversal, caching policy, coalescing, and invalidation belong in the shared layer (today `omnifs-host`) that every frontend consumes, not in any one frontend. Two frontends (FUSE, NFSv4) already share it. Don't add this logic to a single frontend, and keep frontend-specific machinery in its own crate: FUSE inode tables, kernel notifier, and reply types in `omnifs-fuse`; NFS filehandles, stateids, and leases in `omnifs-nfs`.
- **Host-native delivery.** The daemon runs host-native, and the Docker container is one launch mechanism among others. Don't deepen container or Docker assumptions in the daemon.
- **Host owns caching.** The host owns all caching as opaque byte storage and evicts only by capacity or explicit invalidation. Providers do not add their own LRUs or time-based expiration.
- **Hot-path latency.** Warm reads should feel local. Don't turn provider latency into mount latency or add per-op blocking on the hot path. Not yet measured, so this is direction, not a gate.
- **Writes as transactions.** The read model is read-only today. When writes land they are explicit, atomic, and auditable (drafts under a draft namespace, executed by moving a prepared transaction into a control namespace), never a side effect of writing to a projected file. See `docs/future/mutations-via-git.md`.
- **Agent legibility.** The tree should explain itself: predictable naming, honest sizes, correct content types and extensions, self-describing schema and README leaves. Build providers legible rather than retrofitting it. A non-obvious keying scheme is a provider schema smell to fix, not a documentation problem to paper over.

## Current shape

Today's architecture. The baseline to understand and compare against; expect it to change, and update this section in the same change when it does.

- **Topology.** A single `omnifs` binary is both CLI and daemon: the runtime loop lives behind a hidden `omnifs daemon` subcommand that loads provider WASM components (`wasm32-wasip2`) and drives them through the `omnifs:provider` WIT interface. The CLI owns host credentials and the daemon lifecycle and talks to the daemon over an HTTP control API. `omnifs up` reads `[system].runtime` (`docker` or `native`, defaulting to native) and either runs the daemon in a container or spawns `omnifs daemon` as a detached host-native child; there is no separate `omnifsd` binary.
- **Mount frontends.** Two frontends serve the same projected tree: FUSE (Linux kernel, used by native Linux and by the optional Docker runtime inside the container) and a read-only NFSv4.0 loopback frontend (`crates/omnifs-nfs`, started via `omnifs daemon --frontend nfs`) for non-Linux host-native integration, primarily macOS. Do not reintroduce macFUSE, `diskutil`, or macOS-specific FUSE mount behavior; use the NFS path for non-Linux host integration. Details: `docs/design/nfsv4-loopback-mount.md`.
- **Core pieces.** inode table, router, providers, caches, clone manager. Understand the current shape before changing it; see `docs/design/architecture.md`.
- **A provider in one breath.** One `#[omnifs_sdk::provider]` impl with a synchronous `fn start` that registers routes imperatively on a `Router`; it returns a terminal `op-result` with `effects`, or suspends on callouts the host runs and resumes. Registration verbs, macros, and effect shapes are in the SKILL and `architecture.md` sections 2 to 6.

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

## Conventions

Judgment defaults, not absolutes.

- **WIT coordination.** When you change the WIT contract (`crates/omnifs-wit/wit/provider.wit`), rebuild all providers and update the affected docs in the same change. Breakage is expected in alpha; the rule is keeping providers and docs in step, not preserving the contract.
- **Small, local changes.** Keep changes small and local. When a refactor touches clone, routing, or traversal, compare against pre-refactor behavior before accepting the new result.
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
