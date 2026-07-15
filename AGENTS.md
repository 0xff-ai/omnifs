# AGENTS.md

Repository-local guidance for working in `omnifs`, shared by Codex, Claude, and human contributors. `CLAUDE.md` is a symlink to this file. Keep this file self-contained because other agents read it directly and do not expand imports.

## Start here

`omnifs` projects external services as native filesystems. Providers own meaning: what paths exist and what bytes they hold. The host owns trust, auth, callouts, caching, and I/O. Frontends translate one shared projected tree into OS protocol behavior.

The product contract is simple: the projected tree must behave like real files for the standard Linux toolbox, judged against every consumer, not one calling pattern. Shells, scripts, editors, agents, and applications are served by the same mount. Do not special-case one consumer.

## How docs bind

`AGENTS.md` is the always-loaded operating guide. It carries universal rules, routing instructions, validation defaults, and active footguns.

- `docs/contracts/`: binding rules by task area. Read only the contract relevant to the code you are touching.
- `docs/architecture/`: current explanatory model and rationale. Read only the architecture note relevant to the subsystem or boundary you need to understand.
- `docs/future/`: proposals and non-current direction.

If a rule here and a contract disagree, follow current code plus the relevant contract, then update this file in the same change. If architecture prose disagrees with code or contracts, treat it as stale explanation and fix it when practical.

## Rule tiers

- **Invariants.** A change that breaks one is wrong. If a task seems to require it, stop and surface the conflict.
- **Gated decisions.** Allowed only after surfacing the tradeoff and getting explicit sign-off.
- **Direction.** Strong guidance, but a deliberate called-out departure is allowed.
- **Current shape.** Today's implementation. Understand it before changing it, but expect it to churn.
- **Footguns.** Concrete traps that are true only while their stated condition holds. Delete a footgun when the condition dies.
- **Conventions.** Judgment defaults. Follow them unless the code gives a better reason.

## Universal invariants

- The host owns trust. Providers are untrusted, even when built in this repo.
- The host knows paths, bytes, content types, file attributes, cache metadata, capability outcomes, and effects. Object meaning stays SDK/provider-side.
- All object reasoning lives SDK-side: identity, canonical assembly via render, versioning, preload, and revalidation.
- No provider-specific behavior belongs in the host, tree, or frontends.
- `omnifs-engine` owns projection semantics shared by FUSE, NFS, and future frontends.
- Frontends translate namespace answers into protocol state. They consume the narrow `omnifs_engine::namespace` surface, never internal tree/render/view modules directly, and do not decide projection semantics. The daemon is a registry that serves several frontends over one shared namespace.
- Host caching is opaque byte storage. Providers do not add private LRUs or time-based expiration policy.
- Declarations must bind behavior. A permission, capability, schema rule, routing rule, cache contract, or validation guarantee must feed an enforced runtime or build-time decision.

## Gated decisions

Allowed, but never as a side effect. Surface the tradeoff and get sign-off in the same change.

- **Provider WASM authority.** New callout families, preopens, process effects, socket effects, or broader network authority change the security model.
- **Auth or transport model.** Changing auth or transport, such as clone over SSH versus HTTPS/token, changes the operational contract.
- **Strict config parsing where enforced.** CLI config and mount auth/config blocks use strict serde parsing. Loosening existing `deny_unknown_fields` hides misconfiguration.
- **Specified technology substitution.** If a task names a technology, library, or architecture, do not substitute another approach when blocked. Report the blocker and wait for approval.

## Load the right contract

| If touching | Read |
|---|---|
| Trust, byte boundary, provider authority, auth, credentials, sandbox claims | `docs/contracts/10-system.md` |
| Provider SDK, provider macros, objects, routes, WIT, metadata, provider config, endpoints | `docs/contracts/20-provider-sdk.md` |
| Projection tree, cache, attrs, listing, lookup, traversal, learned sizes, live growth | `docs/contracts/30-projection-tree.md` |
| FUSE, NFS, mount protocol behavior, frontend state, protocol replies | `docs/contracts/40-frontends.md` |
| CLI, daemon, typed local control protocol, runtime modes, workspace layout, mount delivery, dev home | `docs/contracts/50-control-plane.md` |
| CI, validation commands, provider artifacts, generated schema, docs checks | `docs/contracts/60-build-validation.md` |
| System model or rationale | `docs/architecture/00-overview.md` |

## Load architecture detail when needed

| If you need rationale for | Read |
|---|---|
| File attrs, stat/read behavior, learned sizes, live files, real-tool compatibility | `docs/architecture/10-file-attributes.md` |
| Route precedence, capture validation, lookup/listing authority, exhaustive listings | `docs/architecture/20-route-dispatch-and-listing.md` |
| Object/view/blob cache roles, canonical push, effects, invalidation fences | `docs/architecture/30-cache-and-effects.md` |
| Auth trust boundary, OAuth ownership, credential injection, grants versus needs | `docs/architecture/40-auth-boundary.md` |
| NFSv4 loopback filehandles, stateids, leases, attrs, mount lifecycle | `docs/architecture/50-nfs-frontend.md` |
| Provider async execution, host imports, callout tracing, same-instance concurrency | `docs/architecture/60-async-provider-runtime.md` |

## Orientation

- `crates/omnifs-core`: path, content type, and view primitives.
- `crates/omnifs-sdk`: provider authoring API, object model, route registration, and dispatch.
- `crates/omnifs-wit/wit/provider.wit`: provider component contract.
- `crates/omnifs-workspace`: every byte under `OMNIFS_HOME`: the directory layout, provider/credential identity types, the auth-scheme wire model, provider manifests and the content-addressed provider index, the mount-spec registry (sole owner of on-disk specs) with creation-time inheritance and materialization, and credential stores.
- `crates/omnifs-engine`: trusted runtime, callouts, auth, namespace, cache access, pagination, shared projection semantics, and opaque cache storage.
- `crates/omnifs-vfs-wire`: the Omnifs VFS wire protocol, including serialization, framing, handshake, attach transport and reconnect, readiness signaling, and ordered direct `Path` namespace requests and events.
- `crates/omnifs-fuse` and `crates/omnifs-nfs`: protocol adapters.
- `crates/omnifs-mtab`: `/proc/mounts` parsing, NFS mount state files, and shared platform unmount command construction.
- `crates/omnifs-cli`: daemon process owner, control server, lifecycle, auth commands, dev sessions, and control-plane UX.
- `crates/omnifs-itest`: host-driven provider and tree conformance tests.
- `scripts/ci/*` and `just/*.just`: maintainer command surface, CI orchestration, runtime image assembly, and generated-artifact checks.
- `providers/*`: product providers. Read `providers/DESIGN.md` and `skills/omnifs-provider-sdk/SKILL.md` before changing provider shape.

## Vocabulary

- **Projection.** A mapping of an external system into paths and bytes.
- **Canonical.** Bytes returned from upstream as-is and stored in the canonical cache.
- **Provider.** A sandboxed WASM component (`wasm32-wasip2`) that defines paths, bytes, and object meaning for one service.
- **Upstream.** The external service or data source a provider projects.
- **Host.** The trusted runtime that owns auth, caching, callout execution, namespace state, and I/O.
- **Frontend.** A protocol surface (FUSE or NFSv4 loopback) over the complete shared namespace, served by the separate slim `omnifs-thin` runner in `fuse` or `nfs` mode. It contains no engine runtime, Wasmtime, or provider bundle and attaches over the Omnifs VFS wire protocol. Public identity is filesystem (`fuse` or `nfs`) plus runtime (`host`, `docker`, or `libkrun`) and a runtime-owned or host-selected location. The daemon never runs a frontend in-process.
- **Omnifs VFS wire protocol.** The internal daemon-to-frontend serialization of `omnifs_engine::Namespace` for out-of-process frontends. It is not the provider protocol and does not own projection semantics.
- **Mount.** A configured provider projection rooted into the served filesystem tree.
- **Object.** Provider-side domain identity plus canonical bytes and derived files.
- **Render.** SDK-side assembly of an object's canonical bytes. A provider concern, never a frontend concern.
- **Path.** `omnifs_core::path::Path`, the parsed provider path type used inside SDK and tree policy.
- **Callout.** A host-run effect a provider awaits through an async WIT import, such as HTTP. The host executes it and the component future resumes with the result.
- **Effect.** The single terminal channel a provider returns for cache writes, invalidations, and related host-visible side effects.

## Avoid these frames

- Do not call the current daemon `omnifsd`. There is one `omnifs` binary with a hidden `omnifs daemon` subcommand.
- Do not describe macFUSE, `diskutil`, or macOS FUSE mounting as current integration paths. macOS host-native integration is NFSv4 loopback.
- Do not alias `omnifs_core::path::Path` as `ProtocolPath` or another local name. Import it as `Path`; alias `std::path::Path` as `StdPath` when both are needed.
- Do not claim the sandbox prevents all exfiltration. It reduces confused-deputy and lateral-movement risk, but an allowed provider can still exfiltrate through its allowed domains.
- Do not frame agents, editors, or shells as separate product modes. They are consumers of the same mount.

## Current shape

- A single `omnifs` binary is both CLI and daemon. The runtime loop lives behind hidden `omnifs daemon`.
- The CLI owns credentials, lifecycle, and user-facing commands. It talks to the daemon through the typed local control protocol and the on-disk workspace under `OMNIFS_HOME`.
- The control protocol wire types live in `omnifs-api`. Credential material is never transmitted on the wire; credential health is non-secret operational state.
- Mount specs are one file per mount under `mounts/`, and a spec file's stem is its mount name. Only this directory is a local Git repository: `HEAD` is desired state and `refs/omnifs/applied` is the last revision that reached daemon readiness. `mounts::Registry` owns spec parsing, naming, and atomic writes; `mounts::Repository` owns the shared lock, Git operations, revision validation, and immutable snapshots under cache storage. The daemon receives one snapshot path and revision at startup and never invokes Git or reads the mutable worktree. `omnifs up --offline` observes and snapshots the committed `HEAD` without mutating dirty specs, skips provider, credential, network, and runtime startup, serves only validated durable facts, never advances `refs/omnifs/applied`, and replaces a live daemon when its mode differs.
- Provider storage is an internal append-only cache. `mount add` resolves an existing Wasm path, a directory containing one Wasm, an embedded provider name, or an exact/unique lowercase digest prefix, validates and retains that artifact, and commits its exact pin. Existing pins change only through an explicit desired-spec edit; `omnifs up` and its exact `apply` alias validate and apply the committed pin without installing or choosing another artifact.
- The daemon is a pure namespace server and control-plane owner; `omnifs_vfs_wire::VfsServer` owns the fixed local, requested TCP/vsock listeners, attach tokens, listener and connection tasks, readiness, and live attachment observation. The daemon never builds, mounts, supervises, or unmounts a renderer and has no `--frontend` or named attach-socket flags. Every filesystem frontend is a separate slim `omnifs-thin` runner, selected with `fuse` or `nfs`. It contains protocol mechanics only: no engine runtime, no Wasmtime, no provider bundle, no daemon control plane. It attaches over the Omnifs VFS wire protocol.
- The CLI owns frontend launch and teardown through three frontend runtimes: `host` (local process), `docker` (container), and `libkrun` (libkrun microVM on macOS). Docker and libkrun serve FUSE only. Public commands, tables, and help use filesystem, runtime, and location.
- Frontends are durable, independent access surfaces over all mounts. `omnifs frontend enable`, `disable`, and `restart` own runner lifecycle; `ls` reports observations only. `omnifs up` and its exact `apply` alias start or replace only the daemon, and `down` stops only the daemon. A daemon restart never replaces a live frontend process, and every runner reconnects through the wire protocol. `setup` may launch the platform defaults once as imperative actions: Linux = host FUSE; macOS = host NFS plus Docker FUSE. No frontend desired state is persisted. The daemon always binds the fixed host socket `$OMNIFS_HOME/frontends/local.sock` (dir 0700, socket 0600; filesystem permissions are the auth). TCP (docker) and vsock (libkrun) listeners are bound on request and token-authenticated.
- Wire protocol v5 carries the connecting frontend's identity (kind + guest-side mount point, display-only) and the terminal `OfflineMiss` namespace error. v4-or-lower clients are rejected outright with a named reason. Namespace identities are validated `Path` values and disconnects publish root invalidation on the ordered event stream. Runtime is labeled by listener ownership at the daemon (UDS host, TCP docker, vsock libkrun); the guest never self-reports.
- Host runners persist frontend discovery state per location under `cache/frontends/<kind>/<blake3-of-location>` leaves; the NFS filehandle-identity table lives in the same leaf. Corrupt records degrade individually and never hide healthy siblings.
- `omnifs status` joins daemon attachments, runner records, mounts with exact provider-pin health, and daemon state into one plural `Inventory`. Frontends are observed facts only; Inventory never synthesizes defaults or stopped desired rows. Human output is a workspace strip plus responsive resource tables; JSON/JSONL use the invocation-level `--output` envelope. A deliberately stopped, internally consistent workspace exits 0; actionable complete inventory exits 5; an expected but unreachable daemon exits 3.
- Global `--output human|json|jsonl`, `--quiet`, `--no-input`, and `--yes` belong to the invocation. JSON emits exactly one result/error envelope; JSONL emits events followed by one terminal result/error. Status and list results use plural resource arrays and absolute machine paths.
- `omnifs frontend shell <filesystem> --runtime <docker|libkrun>` enters one exact observed guest frontend. Host frontends are ordinary mounted paths and need no Omnifs-owned subshell.
- Guided onboarding belongs to `omnifs setup`, which composes provider configuration, `up`, and imperative platform-default frontend enables without persisting frontend desired state. `up` still starts only the daemon, and `frontend enable` still starts one explicit runner. `down`, `status`, and `shell` read the daemon and observed attachment/runner inventory.
- `just dev` is the supported contributor runtime entrypoint: it runs `scripts/dev.ts` to build provider WASM into a content-addressed provider-store bundle, build the omnifs CLI natively, start a host-native daemon with pinned dev mounts and credentials rendered into `~/.omnifs-dev`, imperatively enable the host and Docker frontends, and open a shell inside the Docker frontend at `/omnifs`. Uniform across OSes: the daemon is always host-native; a frontend location is how a contributor browses.
- The daemon runs on the host, not in a container: `omnifs status`, `omnifs down`, and the daemon log (`~/.omnifs-dev/cache/daemon.log`) all work directly, no `docker exec` needed. The frontend container is discoverable by its `ai.0xff.omnifs.home` label (`docker ps --filter label=ai.0xff.omnifs.home=~/.omnifs-dev`); reach its location with `docker exec -it -w /omnifs <name> /bin/sh` (it ships no zsh) and disable it with `omnifs frontend disable fuse --runtime docker`.
- `Dockerfile`'s `frontend-dev` stage is the contributor image path for `just dev` (the same target `just frontend-image` builds). There is no daemon-in-a-container image or stage: the daemon only ever runs host-native. The frontend image runs `omnifs-thin fuse`, built by the `thin-builder` Dockerfile stage with no provider-store build context. Release frontend image assembly uses `scripts/ci/build-frontend-image.sh`, injecting a prebuilt `omnifs-thin` binary as the `omnifs-thin-bin` build context; release CLI binaries (the separate full `omnifs` binary) embed the provider bundle, and selected mounts retain their exact artifacts through `mount add`.
- A provider is one `#[omnifs_sdk::provider]` impl with synchronous `fn start` registering routes on a `Router`. `r.object::<O>` and `r.file_object::<O>` bind objects; `r.alias` mounts the same object at another template; `r.dir`, `r.file`, and `r.treeref` are the path-oriented face for non-object routes.
- Provider namespace and notify exports are async component functions. SDK callout futures await host imports directly; the host uses Wasmtime component async with `run_concurrent` so one provider instance can have multiple filesystem operations in flight.

## Product contract

The mount must behave like real files for the standard toolbox:

- read: `cat`, `head`, `tail` including `-f`, `-n`, and `-c`, `less`, `xxd`, `hexdump`, `od`, `file`
- search and traverse: `grep -r`, `rg`, `find` including `-name`, `-size`, and `-type`, `fd`
- stat: `ls -l`, `ls -h`, `du -sh`, `wc`, `stat`
- copy and archive: `cp`, `mv`, `tar c`, `tar x`, `tar t`, `rsync`
- compare and hash: `diff`, `cmp`, `*sum`
- inspect and edit: `jq`, `yq`, `xmllint`, `vim`, `nano`; mmap editors are best effort

When a feature touches mount behavior, prove no regression through `crates/omnifs-itest`, a relevant `*smoke*` test, a focused unit test, or the live runtime path described in `CONTRIBUTING.md`.

## Working rules

### Ground yourself

- Trace the real flow before deciding. Read the files a change touches, their call sites, and the owning docs.
- Simplicity after comprehension is good. Simplicity that skips the flow ships confident wrong fixes.
- When investigating a failure, identify the root cause before proposing or making fixes.
- Preserve the original failure signal until the underlying mechanism is understood. Do not weaken tests, fixtures, coverage, or scenarios to make a failure disappear unless explicitly asked.

### Shape code around owners

- One fact has one owner: one authoritative type, function, config field, or document.
- Keep ownership separate from placement. Ownership follows the invariant and data source, not nearby files.
- Model the boundary, not the workaround. If call sites need side parameters, fake variants, or bypass paths, fix the missing domain boundary.
- Public API is a contract, not a sketchpad. Exported types, enum variants, macro arguments, route verbs, and trait methods need current users and clear invariants.
- Add an abstraction only for two honest pressures or one genuinely volatile external boundary.
- Prefer parsed forms after parse boundaries. Do not fall back to strings, maps, or JSON values for internal policy unless the format itself is the domain.
- Delete bridge layers when the direct path exists. Transitional adapters, duplicate DTOs, compatibility aliases, and one-caller forwarding helpers should not harden.
- Dependencies must pay rent. Remove unused direct dependencies in the same change that makes them unused.

### Worktree and agent handoff

- This repo often moves work across sibling worktrees. Before replaying or integrating, inspect the full source worktree state, not only the last discussed diff.
- Tracked diffs do not include every handoff artifact. Copy required untracked files explicitly, but exclude ignored local state such as `.cache`, `.serena`, `dist`, and `target`.
- Do not infer task ownership from branch names, worktree names, or public branches. Use an explicit local ledger or handoff note for manual multi-agent work.
- Prefer local handoff paths such as `git fetch /path` or `format-patch | git am`. Reserve public branches for integration or review boundaries unless the user explicitly asks to publish.
- `docs/future/codebase-simplification-tracker.md` is the operational source of truth for redesign work. Update it immediately when a redesign is recorded, assigned, starts implementation, produces a stable handoff, starts integration, finishes integration, is blocked, or is superseded. Do not report a redesign lifecycle transition until the tracker records it.
- Create redesign implementation tasks with the user's **approve for me** permission profile whenever the thread tool exposes that setting. Luna subagents inherit the parent thread's permission profile when no per-agent setting exists; their briefs must require immediate approval requests for necessary escalations rather than treating sandbox or network denial as a product blocker or silently skipping required work.

## Validation

Fast sanity for host or CLI code:

```bash
cargo fmt
cargo nextest run
```

Use the right wider gate for the change:

- **Before a push or PR handoff.** Run `just check`; it composes formatting, justfile and docs checks, workflow linting, provider gates, host clippy and tests, and whitespace validation. CI keeps the scoped lanes separate for parallelism.
- **WASM toolchain.** Provider WASM builds need wasi-sdk. Provider build and check recipes install the pinned version when needed.
- **Fresh worktree or missing artifacts.** Run `just build providers` before treating missing provider artifacts as product failures.
- **Host gate.** Use `just check host` and `just test host`; both exclude provider/test-provider WASM crates from host-target builds.
- **Provider or broad-surface change.** Run the affected provider, host, generated-artifact, and docs gates explicitly.
- **Mount, provider, clone, traversal, or runtime behavior.** Rust checks are not enough. Validate through the live runtime with `just dev -y`, `omnifs status`, and the smoke path in `CONTRIBUTING.md`.
- **Route-surface change.** Run the host integration path that initializes and compiles provider routers, especially `all_providers_initialize_and_compile`.
- **Control protocol change.** Run the focused typed request/reply and lifecycle tests for the daemon, CLI, Inspector, and existing control-plane fixtures.
- **Provider manifest schema change.** Run `just schema` and keep the checked-in schema synchronized.
- **Documentation-heavy change.** Run `just docs-check` locally. It is not a CI gate and does not block a merge, so run it yourself when you touch `docs/`.

Do not use `cargo check --workspace --all-targets` as the host gate. If validation cannot run, say exactly what failed, what was skipped, and the next best check.

## Footguns

- **A restored attach listener is part of daemon readiness.** Docker and libkrun runners retain their processes across daemon replacement; startup restores each durable TCP/vsock address and token through `VfsServer` before publishing readiness. A restored listener failure aborts startup and leaves the predecessor daemon record as stale diagnostic evidence.

- **Bare `omnifs` on PATH may be the stale npm release.** A global `@0xff-ai/omnifs` shim under the node/fnm tree can shadow the worktree binary and serve a stale published build with retired behavior, such as the pre-host-native Docker-daemon model. When operating the daemon, mounts, or any CLI command from this worktree, always run the compiled `target/debug/omnifs` or `target/release/omnifs`, never bare `omnifs`; a stale shim answering `omnifs status` or `omnifs shell` with errors like a missing `omnifs` Docker container is this footgun, not a real regression.
- **Default members, not workspace.** `cargo check --workspace --all-targets` forces WASM guest crates onto the host target and fails on `main` too. Guest crates build through `just build providers` and `just check providers`.
- **Stale wit-bindgen after `.wit` edits.** Incremental builds can serve stale codegen. Run `cargo clean -p omnifs-wit` or a clean build before trusting downstream errors.
- **Provider rebuild contention under nextest.** Some `omnifs-engine` integration tests shell out to `just build providers`. Reliable flow: `just build providers`, then `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run ...` (or `just test host`, which sets that flag for you).
- **Host checks may need generated provider artifacts.** If a host check fails because provider WASM is missing, build providers first or point the check at an existing artifact directory.
- **Provider metadata is compiled directly into provider Wasm.** The `#[provider]` macro assembles the manifest JSON at compile time and emits exactly one `omnifs.provider-metadata.v1` custom section. Missing or invalid metadata means the provider artifact is stale or malformed, so rebuild it with `just build providers`. The host reads and validates the section pre-instantiation; it never instantiates a component to discover metadata.
- **Host resource bindings must stay per-field.** `HostFile` and `HostSocket` are string config fields with host-resource bindings. Keep the binding on the field metadata; hiding it in the type shape makes host resource lookup miss it.
- **Object anchors mount at their `r.object` template.** Mount directory-shaped objects at the real anchor path, or use a detached object handle plus `r.object`/`r.alias`.
- **Router compile errors are component-init errors.** `cargo check --target wasm32-wasip2` can type-check incoherent route trees. Provider route validity is proved when initialization consumes the registration builder through `Router::compile`.
- **Live NFS mount tests are serialized for a reason.** macOS live NFS tests use a cross-process TCP lock. Do not parallelize or remove that guard casually.

## Documentation

- Update `docs/contracts/` when a boundary, ownership rule, gated decision, or validation contract changes.
- Update `docs/architecture/` when the current explanatory model or rationale changes.
- Keep task instructions next to the owning surface until a repeated pattern justifies a guide namespace.
- Keep `docs/future/` clearly non-current.
- Do not transcribe WIT blocks, struct definitions, or crate layouts into docs. Cite symbols and files instead.
- Grep docs for old names when renaming a crate, type, route verb, command, generated artifact, or doc path.
- `just docs-check` fails on nonexistent `docs/` links and enforces the `docs/contracts/` theme-file template. It does not check code paths.

## After a change

- Run the narrowest meaningful validation and report exact commands.
- Update the relevant contract when a boundary, contract, or user-visible behavior changed.
- Delete stale footguns in this file when their condition no longer holds.
- Update current shape when the implementation shape changes.
- Fix or add a rule here when the work proves one wrong or missing.

# Writing style

Write in flowing technical prose, the way a sharp senior engineer talks in chat - direct, conversational, and confident. Not documentation, not a report, not a slide deck.

Rules:

1. **Answer exactly what was asked, at the length it deserves - err short.** A yes/no or confirmation question gets 2-4 sentences. A "which one should I pick" gets a few paragraphs. Only a genuinely multi-part design question earns a long answer. Before sending, cut any paragraph that doesn't change what the reader does next: background they didn't ask for, restating their situation back to them, generic advice ("monitor it", "measure first") they'd already know. Seven paragraphs where three would do is a style failure even if every paragraph is well-written.
2. **Every paragraph and every bullet carries a complete argument** - claim, mechanism, and consequence together. Never state a fact without saying why it matters in the same breath. Not "MoR increases scan cost, latency, and metadata overhead" but "MoR is cheap to write, but every read has to reconcile delete files against data files, so scans get slower and flakier until something compacts them - and now that's your problem to operate."
3. **Match the form to the content - and vary it.** A long answer whose every block has the same shape (all paragraphs, all bold-lead paragraphs, all bullets) is monotonous and hard to scan; real explanations mix forms because the content mixes kinds. Pick per part:
 - **Distinct sections or comparison axes** (cost vs ops, "how generation works" vs "conventions") -> short bold headings on their own line, like "**The API reference is generated, not hand-written**" or "**Cost:**". A multi-axis comparison in undifferentiated paragraphs is a style failure just like a fragmented list is.
 - **A genuine sequence** (pipeline stages, diagnostic steps, ranked guesses) -> a numbered list, each item opening with a short bolded lead phrase and continuing in full sentences (1-4 of them).
 - **Genuinely parallel, enumerable facts** (the four config files involved, the three limits that apply) -> a plain bullet list; items may be a single full sentence when the facts are simple, and that's fine.
 - **Reasoning, causality, narrative** -> paragraphs.
 Shortening never means flattening: when rule 1 says cut, cut sentences within the structure - don't collapse headings, lists, and sections into uniform paragraphs.
4. **Don't shred connected reasoning into bullets.** If items connect with "because"/"so"/"but", those connections are the content - write prose. And never a bolded label followed by a clipped noun phrase posing as a bullet.
5. **Open with the verdict and its central caveat in one or two plain sentences.** Not a bolded headline.
6. **Conversational but not dramatic.** Use contractions (it's, you'd, don't). Say "so" and "but", not "therefore" and "however". Never write scaffolding like "The deciding mechanism is", "It is worth noting", "Importantly". No theatrical labels or hype adjectives: no "**The poison**", "the trap", "brutally expensive", "the killer feature", "sharp edge", "absurdly cheap". State the actual problem in plain words - "this rewrites gigabytes to change megabytes" beats any dramatic framing.
 - No staccato, short dramatic sentences. Let sentences breathe with commas, dependent clauses, and ideas linked together.
 - No cheesy setup phrases that introduce a point instead of stating it. Never write "here's the thing", "here's the kicker", "the part nobody warns you about", "what nobody tells you", "the dirty secret", "the truth is", "plot twist", "the reality is", "here's what's wild". State the claim directly.
 - No contrastive "not just X, but Y" structure or its variants ("it's not just X, it's Y", "not only X but also Y"). State the point directly instead of negating one framing to elevate another.
7. **No compression.** No dropped articles, no strings of abstract nouns where one concrete mechanism explains more. Shortness comes from cutting low-value content (rule 1), never from clipping sentences.
8. **End with a bottom line only when the answer weighed a real decision.** One plain-prose sentence: the call plus the condition that would flip it. Short factual or confirmation answers just end - no formulaic closer.
