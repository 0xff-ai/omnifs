# AGENTS.md

Repository-local guidance for working in `omnifs`, shared by Codex, Claude, and human contributors. `CLAUDE.md` is a symlink to this file. Keep this file self-contained because other agents read it directly and do not expand imports.

## Start here

`omnifs` projects external services as native filesystems. Providers own meaning: what paths exist and what bytes they hold. The host owns trust, auth, callouts, caching, and I/O. Filesystems translate one shared projected tree into OS protocol behavior.

The product contract is simple: the projected tree must behave like real files for the standard Linux toolbox, judged against every consumer, not one calling pattern. Shells, scripts, editors, agents, and applications are served by the same mount. Do not special-case one consumer.

## How docs bind

`AGENTS.md` is the always-loaded operating guide. It carries universal rules, routing instructions, validation defaults, and active footguns.

- `docs/contracts/`: binding rules by task area. Read only the contract relevant to the code you are touching.
- `docs/architecture/`: current explanatory model and rationale. Read only the architecture note relevant to the subsystem or boundary you need to understand.

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
- No provider-specific behavior belongs in the host, tree, or filesystems.
- `omnifs-engine` owns projection semantics shared by FUSE, NFS, and future filesystems.
- Filesystems translate namespace answers into protocol state. They consume the narrow `omnifs_engine::namespace` surface, never internal tree/view modules directly, and do not decide projection semantics. The daemon is a registry that serves several filesystems over one shared namespace.
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
| FUSE, NFS, mount protocol behavior, filesystem state, protocol replies | `docs/contracts/40-filesystems.md` |
| CLI, daemon, typed local control protocol, filesystem runtimes, profile layout, mount and credential RPC, dev home | `docs/contracts/50-control-plane.md` |
| CI, validation commands, provider artifacts, generated schema, docs checks | `docs/contracts/60-build-validation.md` |
| System model or rationale | `docs/architecture/00-overview.md` |

## Load architecture detail when needed

| If you need rationale for | Read |
|---|---|
| File attrs, stat/read behavior, learned sizes, live files, real-tool compatibility | `docs/architecture/10-file-attributes.md` |
| Route precedence, capture validation, lookup/listing authority, exhaustive listings | `docs/architecture/20-route-dispatch-and-listing.md` |
| Object/view/blob cache roles, canonical push, effects, invalidation fences | `docs/architecture/30-cache-and-effects.md` |
| Auth trust boundary, OAuth ownership, credential injection, grants versus needs | `docs/architecture/40-auth-boundary.md` |
| NFSv4 loopback filehandles, stateids, leases, attrs, mount lifecycle | `docs/architecture/50-nfs-filesystem.md` |
| Provider async execution, host imports, callout tracing, same-instance concurrency | `docs/architecture/60-async-provider-runtime.md` |

## Orientation

- `crates/omnifs-core`: shared path, mount and provider content identities, filesystem identity, and file-contract primitives.
- `crates/omnifs-sdk`: provider authoring API, object model, route registration, and dispatch.
- `crates/omnifs-wit/wit/provider.wit`: provider component contract. Guest bindings live at `omnifs_wit::provider`; host (Wasmtime) bindings at `omnifs_wit::host` behind `host-bindings`.
- `crates/omnifs-bootstrap`: profile-root resolution, the fixed control socket, daemon process identity, and the daemon spawn lock.
- `crates/omnifs-state`: daemon-owned SQLx/SQLite state, provider artifacts, mounts, credentials, projection cache, and raw daemon logs.
- `crates/omnifs-daemon`: daemon lifecycle and recovery, local gRPC control, namespace supervision, bounded raw-log streaming, and serialized durable mutations.
- `crates/omnifs-vfs`: shared VFS facade (`Namespace` and plain answers); enable `wire` for framing, handshake, attach/reconnect, readiness, and `VfsServer`.
- `crates/omnifs-engine`: trusted runtime, callouts, auth, cache, pagination, shared projection semantics (`TreeNamespace`), and opaque cache storage.
- `crates/omnifs-fuse` and `crates/omnifs-nfs`: protocol adapters.
- `crates/omnifs-mtab`: `/proc/mounts` parsing, NFS mount state files, and shared platform unmount command construction.
- `crates/omnifs-libkrun`: private fixed-purpose libkrun loader, VM configuration, and shutdown control used only by the Apple Silicon filesystem runtime.
- `crates/omnifs-cli`: daemon process owner, control client, lifecycle, auth commands, dev sessions, and control-plane UX.
- `crates/omnifs-inspector`: Inspector state, replay and live-event sources, terminal lifecycle, and TUI rendering. The CLI owns command dispatch and receipt presentation.
- `crates/omnifs-itest`: host-driven provider and tree conformance tests.
- `scripts/ci/*` and `just/*.just`: maintainer command surface, CI orchestration, runtime image assembly, and generated-artifact checks.
- `providers/*`: product providers. Read `providers/DESIGN.md` and `skills/omnifs-provider-sdk/SKILL.md` before changing provider shape.

## Vocabulary

- **Projection.** A mapping of an external system into paths and bytes.
- **Canonical.** Bytes returned from upstream as-is and stored in the canonical cache.
- **Provider.** A sandboxed WASM component (`wasm32-wasip2`) that defines paths, bytes, and object meaning for one service.
- **Upstream.** The external service or data source a provider projects.
- **Host.** The trusted runtime that owns auth, caching, callout execution, namespace state, and I/O.
- **Filesystem.** One named OS-facing instance over the complete shared namespace. Its strict persisted `fs::Spec` contains ID, protocol (`fuse` or `nfs`), runtime (`host`, `docker`, or `libkrun`), and resolved location. Host filesystems run through hidden `omnifs run-fs`; Docker and libkrun guests use `omnifs-thin`. The daemon never runs a filesystem in-process.
- **Omnifs VFS wire protocol.** The internal daemon-to-filesystem serialization of `omnifs_engine::Namespace` for out-of-process filesystems. It is not the provider protocol and does not own projection semantics.
- **Mount.** A configured provider projection rooted into the served filesystem tree.
- **Object.** Provider-side domain identity plus canonical bytes and derived files.
- **Render.** SDK-side assembly of an object's canonical bytes. A provider concern, never a filesystem concern.
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
- The CLI owns OAuth/static-auth UX, profile config, the legacy mutation journal (`client/mutations.json`), metrics, daemon spawn, and resource authoring. Interactive `provider`, `mount`, `credential`, and `attachment` mutations read the desired set, call `plan`, ask for consent, call `apply`, and follow the typed revision or action stream. Automation uses `omnifs plan <file>` and `omnifs apply <file> --yes`; the narrow `credential set --from-env` path carries secret material without putting it in argv. It talks to the daemon only through typed local RPC. Legacy filesystem specs remain read-only migration input; normal lifecycle never launches from them.
- The control protocol uses tonic/protobuf gRPC over the profile's local Unix socket. Credential material may cross only in request payloads on that socket; it never appears in filesystem attach/TCP traffic, responses, status, inventory, logs, Debug, or Inspector output. Credential health is non-secret operational state. Resource apply returns after validation and durable commit; `WatchProgress` and action watches carry later provider, serving, mount, runtime, and session phases.
- The daemon owns provider artifacts, credentials, mounts, desired resources, Attachment runtime state, SQLite state/cache, attach endpoints, live VFS sessions, and raw log bytes. `PlanResources` is pure. `ApplyResources` validates and commits the complete desired set in one SQLite transaction, sends a non-blocking reconcile wakeup, and returns a durable revision receipt. The legacy mutation slot and storage remain as transitional internals for old mount and credential paths until Plan 009 removes them.
- Provider imports carry no mutation identity and never touch the legacy mutation lease: the daemon dedupes by content digest, so a dropped upload just retries and importing identical bytes twice returns `Unchanged`. Resource porcelain sends typed provider, mount, credential, and Attachment definitions to the planner; the daemon validates metadata, credentials, and grants before serving the namespace. The old lease-scoped batch remains only as transitional storage and RPC plumbing.
- The daemon is the control-plane and lifecycle owner. `AttachmentSupervisor` reconciles durable desired Attachments into out-of-process host, Docker, or libkrun runtimes. `VfsServer` binds one fixed Unix and one fixed TCP endpoint on every start, owns connection tasks and live sessions, and treats either listener's exit as fatal. Host filesystems use hidden `omnifs run-fs`; Docker and libkrun guests keep the slim `omnifs-thin` binary.
- Public filesystem lifecycle is `attachment add|ls|show|rm|restart|shell`; resource presence requests attachment, and removing it requests teardown. Commands wait on typed revision or action progress by default. `attachment shell` executes only the typed argv returned by `GetAttachmentAccess`. The old `fs` grammar is no longer public, while its hidden `run-fs` runner remains valid. Old specs under `client/filesystems/specs` are scanned read-only and require explicit import.
- `down` stops exact daemon-owned runtime instances within a bound and preserves desired Attachment rows. A later daemon start reloads and restores them.
- Daemon-owned runner records, sockets, host logs, libkrun root images and helper state, and guest-image cache live under `daemon-state/`. Each observed Attachment keeps its exact runtime instance before effects so restart and deletion can fence replacements.
- VFS wire protocol v11 carries the Attachment name, exact `AttachmentSpec`, and runtime instance. Internal live connections are sessions, not configured resources. A reconnect is admitted only for the same exact identity; conflicting fields are rejected.
- `omnifs status` shows provider, mount, credential, and Attachment desired and observed phases. `omnifs status --follow`, with `--revision` or `--action`, follows typed progress to a terminal result. VFS sessions remain a deep diagnostic detail. `omnifs doctor` owns legacy and stray reporting and requires a cleanly stopped daemon, consent, and fresh exact identity proof before destructive remediation.
- Global `--output human|json|jsonl`, `--quiet`, `--no-input`, and `--yes` belong to the invocation after Clap parses it. JSON emits exactly one result/error envelope; JSONL emits events followed by one terminal result/error. Clap owns parse failures and exits 2 before output mode applies. Status and list results use plural resource arrays and absolute machine paths. Human reports use `tabled` for wide resource rows and one Inventory-selected closing action.
- Profile-local dogfood metrics are appended only under `$OMNIFS_HOME/metrics/`, controlled by `[metrics] enabled` and `OMNIFS_METRICS`, and never transmitted. The local JSONL writer has no networking dependency and never fails a product operation.
- `omnifs attachment shell <name> [-- <argv>...]` enters one exact guest filesystem. For a host filesystem it verifies the mounted phase and reports the ordinary host path.
- Logs preserve raw bytes. Doctor groups checks by owner, asks once for a deduplicated repair set, continues after independent repair failures, and rescans. Inspector bounds ingestion, keeps stale data on disconnect, derives help from its keymap, and restores terminal plus panic-hook state before printing its session receipt.
- Boot-and-orient onboarding belongs to `omnifs setup`, which starts the daemon, lists embedded providers with honest auth and config labels, creates Provider and Mount resources in one desired set, offers the platform's recommended Attachment, applies once, and follows the typed revision to ready or stable failure.
- `just dev` runs `scripts/dev.ts`, which builds providers and the native CLI, renders KCL desired resources, uses `target/debug/omnifs apply <file> --yes`, waits for the terminal revision, and opens `attachment shell dev-docker` at `/omnifs`.
- The daemon runs on the host. The dev Docker filesystem carries `ai.0xff.omnifs.home` and `ai.0xff.omnifs.fs` labels. Remove it with `target/debug/omnifs attachment rm dev-docker` or stop the whole profile with `target/debug/omnifs down`.
- `Dockerfile`'s `filesystem-dev` stage is the contributor image path. The image entrypoint is only `omnifs-thin`; the launcher passes the flat named arguments. Release image assembly uses `scripts/ci/build-filesystem-image.sh`.
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
- This project is pre-alpha and carries no backward-compatibility obligation. Delete obsolete APIs, wire fields, readers, aliases, and migrations outright unless the task explicitly establishes a current interoperability requirement.
- Prefer extending an existing representative test over adding a situational regression case. Add a new fixture only when it protects a durable concurrency, lifecycle, protocol, security, previously silent failure, or public output invariant that existing coverage cannot express cleanly.
- Dependencies must pay rent. Remove unused direct dependencies in the same change that makes them unused.

### Worktree and agent handoff

- This repo often moves work across sibling worktrees. Before replaying or integrating, inspect the full source worktree state, not only the last discussed diff.
- Tracked diffs do not include every handoff artifact. Copy required untracked files explicitly, but exclude ignored local state such as `.cache`, `.serena`, `dist`, and `target`.
- Do not infer task ownership from branch names, worktree names, or public branches. Use an explicit local ledger or handoff note for manual multi-agent work.
- Prefer local handoff paths such as `git fetch /path` or `format-patch | git am`. Reserve public branches for integration or review boundaries unless the user explicitly asks to publish.
- Keep transient trackers, test plans, implementation ledgers, and handoff notes outside the tracked repository. During active work the task thread or an ignored local file may coordinate it; after integration, current code, contracts, architecture notes, and Git history are the durable authority.
- Create redesign implementation tasks with the user's **approve for me** permission profile whenever the thread tool exposes that setting. Luna subagents inherit the parent thread's permission profile when no per-agent setting exists; their briefs must require immediate approval requests for necessary escalations rather than treating sandbox or network denial as a product blocker or silently skipping required work.
- Apply the global coordination contract to this repo. Record live NFS locks, provider-build contention, generated-artifact provenance, and other shared runtime resources in the local task ledger before dispatch.

## Validation

Fast sanity for host or CLI code:

```bash
cargo fmt
cargo nextest run
```

Finish source review before broad validation. Tests replace a preceding `cargo check` or `cargo test --no-run` for the same target. Run one broad gate on the final tree, and rerun it only after a relevant code change.

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

- **Both attach endpoints are part of daemon readiness.** Startup binds the fixed Unix path and the profile's nonzero TCP port before publishing readiness. Failure of either bind aborts startup; exit of either listener after readiness is fatal.

- **Bare `omnifs` on PATH may be the stale npm release.** A global `@0xff-ai/omnifs` shim under the node/fnm tree can shadow the worktree binary and serve a stale published build with retired behavior, such as the pre-host-native Docker-daemon model. When operating the daemon, mounts, or any CLI command from this worktree, always run the compiled `target/debug/omnifs` or `target/release/omnifs`, never bare `omnifs`; a stale shim answering `omnifs status` or `omnifs shell` with errors like a missing `omnifs` Docker container is this footgun, not a real regression.
- **Default members, not workspace.** `cargo check --workspace --all-targets` forces WASM guest crates onto the host target and fails on `main` too. Guest crates build through `just build providers` and `just check providers`.
- **`omnifs-wit` guest and host are separate modules.** SDK/providers use `omnifs_wit::provider` (`wit-bindgen`); engine/itest use `omnifs_wit::host` (wasmtime, behind `host-bindings`). Do not make those feature alternates of one module again: Cargo unification would replace guest `Guest` traits with host structs and break providers under whole-workspace checks.
- **Stale wit-bindgen after `.wit` edits.** Incremental builds can serve stale codegen. Run `cargo clean -p omnifs-wit` or a clean build before trusting downstream errors.
- **Provider rebuild contention under nextest.** Some `omnifs-engine` integration tests shell out to `just build providers`. Reliable flow: `just build providers`, then `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run ...` (or `just test host`, which sets that flag for you).
- **Integration fixtures share only compiled Wasmtime artifacts.** `RuntimeHarness` must bind `HostContext` to `omnifs_engine::test_support::wasm_cache_dir`; nextest runs each test in a separate process, so falling back to a per-fixture temporary cache recompiles identical components. Keep all runtime data private to each fixture, and make CI cache the same explicit compiled-component directory.
- **Host checks may need generated provider artifacts.** If a host check fails because provider WASM is missing, build providers first or point the check at an existing artifact directory.
- **Provider metadata is compiled directly into provider Wasm.** The `#[provider]` macro assembles the manifest JSON at compile time and emits exactly one `omnifs.provider-metadata.v1` custom section. Missing or invalid metadata means the provider artifact is stale or malformed, so rebuild it with `just build providers`. The host reads and validates the section pre-instantiation; it never instantiates a component to discover metadata.
- **Host resource bindings must stay per-field.** `HostFile` and `HostSocket` are string config fields with host-resource bindings. Keep the binding on the field metadata; hiding it in the type shape makes host resource lookup miss it.
- **Object anchors mount at their `r.object` template.** Mount directory-shaped objects at the real anchor path, or use a detached object handle plus `r.object`/`r.alias`.
- **Router compile errors are component-init errors.** `cargo check --target wasm32-wasip2` can type-check incoherent route trees. Provider route validity is proved when initialization consumes the registration builder through `Router::compile`.
- **Live NFS mount tests are serialized for a reason.** macOS live NFS tests use a cross-process TCP lock. Do not parallelize or remove that guard casually.
- **Never enable `serde_json/preserve_order`.** Canonical mount bytes embed a mount's config as `serde_json` text, and `MountVersion` is a hash over those bytes. `serde_json::Map` is a `BTreeMap` only while that feature is off; turning it on anywhere in the workspace switches to insertion order, silently changing every mount version and invalidating stored CAS tokens. `omnifs_state::mount::tests::canonical_config_text_is_pinned` fails if this happens.

## Documentation

- Update `docs/contracts/` when a boundary, ownership rule, gated decision, or validation contract changes.
- Update `docs/architecture/` when the current explanatory model or rationale changes.
- Keep task instructions next to the owning surface until a repeated pattern justifies a guide namespace.
- Keep tracked documentation about the current system. Carry proposals and campaign planning in the active task or issue, then update current contracts and architecture when the decision lands.
- Do not transcribe WIT blocks, struct definitions, or crate layouts into docs. Cite symbols and files instead.
- Grep docs for old names when renaming a crate, type, route verb, command, generated artifact, or doc path.
- `just docs-check` fails on nonexistent `docs/` links and enforces the `docs/contracts/` theme-file template. It does not check code paths.

## After a change

- Run the narrowest meaningful validation and report exact commands.
- Update the relevant contract when a boundary, contract, or user-visible behavior changed.
- Delete stale footguns in this file when their condition no longer holds.
- Update current shape when the implementation shape changes.
- Fix or add a rule here when the work proves one wrong or missing.
