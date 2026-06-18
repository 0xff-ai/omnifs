# AGENTS.md

Repository-local guidance for working in `omnifs`.

`omnifs` projects external services (GitHub, DNS, arXiv, Docker, Linear, a SQL database) as a Linux filesystem. The runtime daemon `omnifsd` loads providers as `wasm32-wasip2` WASM components and drives them through the `omnifs:provider` WIT interface; the `omnifs` CLI owns the Docker lifecycle and host credentials and talks to the daemon over an HTTP control API. The host owns trust, caching, auth, and I/O; providers own meaning (what paths exist, what bytes they hold). Every consumer of the projected tree (shells, scripts, editors, agents, applications) is served by the same mount; nothing may assume a privileged consumer. Why the system is shaped this way: `docs/design/architecture.md`.

## Non-negotiables

These are invariants, not preferences. A change that breaks one is wrong; if a task seems to require it, stop and surface it.

- **Byte boundary.** The host knows only paths, bytes, content types, and file attributes; all object reasoning (identity, canonical assembly, rendering, versioning, preload, revalidation) lives SDK-side. Never give provider WASM more authority (new callout families, preopens, process or socket effects) without explicit sign-off; that changes the security model.
- **WIT is flag-day.** The contract (`crates/omnifs-wit/wit/provider.wit`) has no negotiation story: a change strands every built provider. Treat WIT changes as breaking, batch them, and call them out explicitly in the PR; do not widen the contract for one provider's convenience.
- **Bash-tool compatibility.** The projected tree must behave like real files for the standard Linux toolbox, judged against every consumer rather than one calling pattern. A change that regresses any of these is wrong:
  - read: `cat`, `head`, `tail` (incl. `-f`/`-n`/`-c`), `less`, `xxd`, `hexdump`, `od`, `file`
  - search/traverse: `grep -r`, `rg`, `find` (incl. `-name`/`-size`/`-type`), `fd`
  - stat: `ls -l`/`-h`, `du -sh`, `wc`, `stat`
  - copy/archive: `cp`, `mv`, `tar c/x/t`, `rsync`; compare/hash: `diff`, `cmp`, `*sum`; inspect: `jq`, `yq`, `xmllint`; editors: `vim`, `nano` (mmap editors best-effort)

  Prove no regression through `tests/smoke/` or a unit test when introducing a feature.
- **Read-only model.** The read model stays read-only. Writes, when they land, are explicit, atomic, and auditable (drafts under a draft namespace, executed by moving a prepared transaction into a control namespace), never a side effect of writing to a projected file. See `docs/future/mutations-via-git.md`.
- **Linux-only mount, no container assumptions in the daemon.** The runtime FUSE mount is Linux-only; the host CLI runs on macOS and Linux and talks to a Linux container in both cases. Do not reintroduce macFUSE, `diskutil`, or macOS-specific mount behavior unless explicitly requested. Docker is one launch mechanism; `omnifsd` will later run host-native when NFSv4/FSKit mounts land, so keep it free of container assumptions.
- **Renderer neutrality.** FUSE is one frontend of the projected tree, not its definition. Keep new traversal, caching-policy, coalescing, or invalidation logic out of fuser vocabulary and out of `omnifs-fuse` unless it is genuinely kernel-FUSE-specific (inode tables, kernel notifier, reply types), and keep the seam clean so a second frontend can reuse the decision.
- **Host owns caching; providers do not.** The host owns all caching as opaque byte storage and evicts only by capacity or explicit invalidation. Providers must not add their own LRUs or time-based expiration.

## Working in this repo

Use the explicit Rust baseline when changing host or CLI code:

```bash
cargo fmt
cargo nextest run
```

Use the repo checks when touching broad surfaces or provider code: `just check` (fmt, clippy/tests, wasm provider/tool checks, npm validate, docs link check), `just providers-check`, `just providers-build`. The `justfile` is the human command surface; run `just` to list grouped commands, and keep recipes as thin wrappers over Cargo, Bun, Docker, and shell.

Gate on the default members, not the whole workspace. `cargo check` and `cargo nextest run` build the host default members (`omnifs-cli`, `omnifs-host`, `omnifs-daemon`) and their dependencies; that is the host gate. Do not gate with `cargo check --workspace --all-targets`: it forces the wasm guest crates (`providers/*`, `crates/omnifs-tool-*`) onto the host target, where the wit-bindgen guest bindings (the `Guest` trait, the `export!` macro) do not exist, so it fails with `E0404`/`E0432` on `main` too. Those crates are built for wasm via `just providers-build` / `just providers-check`. After editing a `.wit`, incremental builds can serve stale wit-bindgen codegen and surface phantom errors in downstream crates; run `cargo clean -p omnifs-wit` (or a clean build) before trusting a failure.

For mount, provider, clone, traversal, or runtime behavior changes, do not stop at Rust-only checks: validate through the supported runtime path (`omnifs dev`, then exercise the live mount). The contributor runtime is `omnifs dev` / `shell` / `logs` / `status` / `down`. The exact build/provider commands, the live-runtime validation recipe, and the debugging runbook are in `CONTRIBUTING.md`.

## Subsystem map

Read the owning doc before changing a subsystem; these are the source of truth, and the docs cite code rather than transcribe it.

| Area | Source of truth |
|---|---|
| Architecture invariants, decisions, rejected directions | `docs/design/architecture.md` |
| Provider authoring (read before writing or changing a provider) | `skills/omnifs-provider-sdk/SKILL.md`, `providers/DESIGN.md`, `crates/omnifs-sdk/src/lib.rs` rustdocs |
| Caching model | `architecture.md` §3, `docs/design/object-cache-primary.md` |
| File attributes (size, stability, read-mode, byte source) | `docs/design/file-attributes.md` |
| Path dispatch and listing honesty | `architecture.md` §6, `docs/design/path-dispatch-and-listing.md` |
| Auth, credentials, mount loading | `docs/design/host-auth.md` |
| Daemon/CLI split and control API | `docs/design/daemon-cli-split.md` |
| Build, runtime, debugging | `CONTRIBUTING.md` |
| Release and npm packaging | `RELEASING.md` |
| Roadmap and open opportunities | `docs/future/` |

A provider in one breath: one `#[omnifs_sdk::provider]` impl with a synchronous `fn start` that registers routes imperatively on a `Router`; it returns a terminal `op-result` with `effects`, or suspends on `callout`s the host runs and resumes. The registration verbs, macros, and effect shapes are in the SKILL and `architecture.md` §2-§6. Mount specs are JSON, not TOML, and config deserialization sets `deny_unknown_fields` (do not loosen it; typos must fail initialization loudly).

## Conventions

Decision rules, not absolutes; apply judgment.

- Keep changes small and local, and preserve the current architecture (inode table, router, providers, caches, clone manager) unless the task explicitly changes it. When a refactor touches clone, routing, or traversal, compare against the pre-refactor behavior before accepting the new result.
- Do not silently change the auth or transport model. Switching clone transport from SSH to HTTPS/token changes the operational contract, so call it out explicitly.
- Providers must project all data they have already fetched: if a handler holds an upstream payload, emit every derivable sibling and child instead of returning one field and forcing later refetches.
- Use `hashbrown::HashMap` for provider-internal maps; it keeps provider internals predictable across WASI targets.
- Reuse source-of-truth terms; do not invent names for public surfaces unless the rename is explicit, and keep host internals out of SDK/WIT naming. Do not reuse an existing abstraction if it changes the behavior model; semantic fit matters more than code reuse. For protocol changes, write the exact interaction trace first and reject extra hops on hot paths; if something is conceptually one-way, fix the boundary rather than forcing it through request/response machinery.
- Prefer `From`/`TryFrom` at type boundaries over `foo_to_bar` free functions for true one-to-one mappings. The exceptions (orphan rules, extra context, callout extraction, conditional mappings) and the existing conversion hubs are in `CONTRIBUTING.md`.
- CLI presentation vs schema types: `omnifs-mount` types (`Spec`, `Resolved`, `Auth`) are wire/config truth, so keep human-facing labels and formatting out of them; format capability display at the use site through `crates/omnifs-cli/src/capability.rs`; keep `*_to_json` DTO serialization separate from schema conversions.
- Prefer the simpler end-to-end flow over the purer local abstraction, single-phase over multi-phase on the hot path, and data kept near where it is produced and consumed (split it into a second mechanism only when the separation buys something concrete). Once the direct path exists, remove bridge-style dispatch layers and transitional glue rather than letting them harden.
- Agent legibility: prefer designs where the tree explains itself (predictable naming, honest sizes, correct content types and extensions). A non-obvious keying scheme is a provider schema smell to fix, not a documentation problem to paper over.

### Test quality

Prefer tests that protect behavior the project depends on: user-visible workflows, domain invariants, security/auth boundaries, persistence and wire-format compatibility, or regressions that are easy to reintroduce. Avoid tests whose only value is confirming library or plumbing behavior (serde/clap/url working as documented, a wrapper forwarding fields, a builder storing what it was passed, an in-memory fake round-tripping data, brittle wording checks). Before adding a narrow test, be able to name the regression it catches and why that regression matters. Providers carry no in-crate `#[cfg(test)]` modules; verify provider behavior through host-driven integration tests and the live runtime path.

## Documentation

Docs are organized by what changes at what rate, and they point at code rather than copy it.

- `docs/design/architecture.md` is the invariants-and-decisions home: the load-bearing rules, their rationale, and the rejected directions (the "never" list). Change it when an invariant or a deliberate constraint changes, not when an implementation detail moves.
- `docs/design/*` are subsystem detail; `docs/future/*` is roadmap (incl. `engine-roadmap.md`); `docs/internal/*` is gitignored strategy. The WIT and the code are the source of truth for the contract.
- Docs cite symbols and files; they do not transcribe WIT blocks, struct definitions, or crate layouts. A transcribed code shape is a known drift liability: if you must include one, treat it as something to re-verify on the next contract change. Most of the docs that went stale did so by mirroring code.

Two triggers, because these drift the docs fastest:

- A WIT change updates `architecture.md` (the affected sections, e.g. §2 object model, §4 effects, §5 read path, §8 file attributes) and any affected `design/*` doc in the same PR (it is already a flag-day break; the docs ride with it).
- A crate, type, or registration-verb rename updates the docs that name it. Grep `docs/` for the old name before calling the rename done.

`just docs-check` (in `just check` and CI preflight) fails on any doc that references a nonexistent `docs/` path, catching dangling doc-to-doc links automatically. It does not check code paths; that is what the cite-don't-transcribe doctrine above is for.
