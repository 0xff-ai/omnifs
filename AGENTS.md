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
- `omnifs-tree` owns projection semantics shared by FUSE, NFS, and future frontends.
- Frontends translate tree answers into protocol state. They do not decide projection semantics.
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
| CLI, daemon, REST API, runtime modes, workspace layout, mount delivery, dev home | `docs/contracts/50-control-plane.md` |
| CI, validation commands, provider artifacts, generated OpenAPI/schema, docs checks | `docs/contracts/60-build-validation.md` |
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
- `crates/omnifs-host`: trusted runtime, callouts, auth, namespace, cache access, pagination, and archive execution.
- `crates/omnifs-tree`: shared projection semantics consumed by every frontend.
- `crates/omnifs-fuse` and `crates/omnifs-nfs`: protocol adapters.
- `crates/omnifs-mtab`: `/proc/mounts` parsing, NFS mount state files, and shared platform unmount command construction.
- `crates/omnifs-daemon`: control server, app context, frontend startup, and daemon runtime.
- `crates/omnifs-cli`: setup, lifecycle, auth commands, dev sessions, and control-plane UX.
- `crates/omnifs-cache`: opaque byte and record storage owned by the host.
- `crates/omnifs-itest`: host-driven provider and tree conformance tests.
- `scripts/ci/*` and `just/*.just`: maintainer command surface, CI orchestration, runtime image assembly, and generated-artifact checks.
- `providers/*`: product providers. Read `providers/DESIGN.md` and `skills/omnifs-provider-sdk/SKILL.md` before changing provider shape.

## Vocabulary

- **Projection.** A mapping of an external system into paths and bytes.
- **Canonical.** Bytes returned from upstream as-is and stored in the canonical cache.
- **Provider.** A sandboxed WASM component (`wasm32-wasip2`) that defines paths, bytes, and object meaning for one service.
- **Upstream.** The external service or data source a provider projects.
- **Host.** The trusted runtime that owns auth, caching, callout execution, namespace state, and I/O.
- **Frontend.** A host-side surface that exposes the projected tree to an OS. Supported frontends are FUSE on Linux and NFSv4 loopback on macOS.
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
- The CLI owns setup, credentials, lifecycle, and user-facing commands. It talks to the daemon through the REST API and the on-disk workspace under `OMNIFS_HOME`.
- The REST API schema lives in `omnifs-api`. Credentials are never transmitted on the wire.
- Mount specs are one file per mount under `mounts/`, and a spec file's stem is its mount name. `mounts::Registry` (in `omnifs-workspace`) is the sole spec owner: the CLI is the only author and writes through it atomically, the daemon reads through it on reconcile. `provider::Catalog` (in `omnifs-workspace`) is the provider index over the content-addressed store. A spec inherits its provider-manifest defaults (auth scheme and config) at creation time, so it is self-contained: reading or serving it is a plain parse, with no read-time resolution step.
- Runtime modes are host-native and Docker. Docker runs the Linux daemon and exposes FUSE inside the container.
- Linux defaults to FUSE. macOS defaults to read-only NFSv4.0 loopback through `omnifs-nfs`.
- `omnifs setup` picks the default runtime (Docker on macOS, native on Linux/WSL; macOS native is experimental) and records it at `[system].runtime`. `omnifs up --runtime <docker|native>` overrides it for one launch without persisting; `down`/`status`/`shell` read the running backend from the daemon and launch record, never from `[system].runtime`.
- `just dev` is the supported contributor runtime entrypoint: it runs `scripts/dev.ts` to build provider WASM into a content-addressed provider-store bundle, build the runtime image from that bundle, render pinned dev mounts and credentials into `~/.omnifs-dev`, start profile-selected fixtures, launch the FUSE runtime container, and open a shell at `/omnifs`.
- Contributor dev commands run inside the container. From the host, use `docker exec omnifs omnifs status`, `docker exec -it -w /omnifs omnifs /bin/zsh`, `docker logs omnifs`, and `docker rm -f omnifs`.
- `Dockerfile` is the contributor image path for `just dev`. The dev image consumes the host-built provider-store bundle from `target/omnifs-provider-store` instead of compiling providers inside the image, while the dev provider store retains artifacts by content id for runtime mount pinning. Release runtime image assembly uses `scripts/ci/build-runtime-image.sh`; release CLI binaries embed the provider bundle and unpack it into `OMNIFS_HOME/providers`.
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

## Validation

Fast sanity for host or CLI code:

```bash
cargo fmt
cargo nextest run
```

Use the right wider gate for the change:

- **Before a push or PR handoff.** Run the relevant CI-shaped lanes directly rather than relying on a local aggregate: formatting/npm preflight, provider gates for WASM changes, and host gates for host-target changes.
- **WASM toolchain.** Provider WASM builds need wasi-sdk. Install the pinned version with `just providers wasi-sdk`.
- **Fresh worktree or missing artifacts.** Run `RUSTC_WRAPPER= just providers build` before treating missing provider artifacts as product failures.
- **Host gate.** Use `just host clippy` and `just host test`; both exclude provider/test-provider WASM crates from host-target builds.
- **Provider or broad-surface change.** Run the affected provider, host, generated-artifact, and docs gates explicitly.
- **Mount, provider, clone, traversal, or runtime behavior.** Rust checks are not enough. Validate through the live runtime with `just dev -y`, `docker exec omnifs /bin/zsh -lc 'omnifs status'`, and the smoke path in `CONTRIBUTING.md`.
- **Route-surface change.** Run the host integration path that initializes and seals providers, especially `all_providers_initialize_and_seal`.
- **Control API change.** Run `just openapi` to regenerate the checked-in spec, then run the daemon OpenAPI parity test.
- **Provider manifest schema change.** Run `just schema` and keep the checked-in schema synchronized.
- **Documentation-heavy change.** Run `just docs-check` locally. It is not a CI gate and does not block a merge, so run it yourself when you touch `docs/`.

Do not use `cargo check --workspace --all-targets` as the host gate. If validation cannot run, say exactly what failed, what was skipped, and the next best check.

## Footguns

- **Default members, not workspace.** `cargo check --workspace --all-targets` forces WASM guest crates onto the host target and fails on `main` too. Guest crates build through `just providers build` and `just providers check`.
- **Stale wit-bindgen after `.wit` edits.** Incremental builds can serve stale codegen. Run `cargo clean -p omnifs-wit` or a clean build before trusting downstream errors.
- **Provider rebuild contention under nextest.** Some `omnifs-host` integration tests shell out to `just providers build`. Reliable flow: `just providers wasi-sdk`, `just providers build`, then `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1 cargo nextest run ...` (or `just host test`, which sets that flag for you).
- **Host checks may need generated provider artifacts.** If a host check fails because provider WASM is missing, build providers first or point the check at an existing artifact directory.
- **Provider metadata is injected at build time, not compiled into the Wasm.** The `#[provider]` macro emits the typed `Provider::METADATA` const plus a native `provider_metadata()` accessor (non-wasm only); `omnifs-embed-metadata` (run by `just providers build`) links the provider crates, converts each const into the host `ProviderManifest`, and injects the JSON as the `omnifs.provider-metadata.v1` custom section. Missing metadata means the harvester did not run, build providers with `just providers build`. The host reads the section pre-instantiation; it never instantiates a component to read it.
- **Host resource bindings must stay per-field.** `HostFile` and `HostSocket` are string config fields with host-resource bindings. Keep the binding on the field metadata; hiding it in the type shape makes host resource lookup miss it.
- **Raw mount specs are not strict today.** CLI config and mount auth/config blocks are strict, but raw top-level mount `Spec` parsing currently allows unknown keys. Do not claim strict mount-spec parsing unless the code and tests change.
- **Object anchors mount at their `r.object` template.** Mount directory-shaped objects at the real anchor path, or use a detached object handle plus `r.object`/`r.alias`.
- **Seal-time route errors are component-init errors.** `cargo check --target wasm32-wasip2` can compile incoherent route trees. Provider route validity is proved when the host initializes and seals the provider.
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
