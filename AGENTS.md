# AGENTS.md

Repository-local guidance for working in `omnifs`.

## Project model

`omnifs` is a projected filesystem that mirrors external services into local paths. The runtime daemon (binary `omnifsd`, crate `crates/omnifs-daemon`) loads providers as `wasm32-wasip2` WASM components and drives them through the `omnifs:provider` WIT interface. The host owns trust, caching, auth, and I/O; providers own meaning (what paths exist, what bytes they hold). Every consumer of the projected tree (shells, scripts, editors, agents, applications) is served by the same mount; nothing in the system may assume a single privileged consumer.

The architecture is a daemon/CLI split (see `docs/design/daemon-cli-split.md`): `omnifsd` runs as the container entrypoint, serves the FUSE mount, and exposes an HTTP control API (`/v1/{ready,version,status,mounts,events}`) on port 7878, published on the host loopback. The `omnifs` CLI owns the Docker lifecycle and host credentials, and talks to the daemon over that API; mounts are pushed as spec payloads (`POST /v1/mounts`), never bind-mounted as config files; the host `OMNIFS_HOME` tree is bind-mounted writable as the daemon's runtime home. `omnifs init` and `omnifs mounts rm` apply live to a running daemon. The CLI does not link wasmtime or fuser.

The runtime FUSE mount is Linux-only today. The host CLI runs on macOS and Linux, and talks to a Linux container in both cases. Do not reintroduce macOS-specific mount behavior, `diskutil`, or macFUSE assumptions unless explicitly requested. `omnifsd` must stay free of container assumptions: Docker is one launch mechanism, and the daemon will later run host-native when NFSv4/FSKit mounts land.

### Architecture direction guardrails

These keep day-to-day changes compatible with where the runtime is going (host-native daemon, additional mount frontends beyond FUSE):

- **Renderer neutrality.** FUSE is one frontend of the projected tree, not its definition. New traversal, caching-policy, coalescing, or invalidation logic must not be expressed in fuser vocabulary or live in `omnifs-fuse` unless it is genuinely kernel-FUSE-specific (inode tables, kernel notifier, reply types). When adding such logic, keep the seam clean so a second frontend can consume the same decision without re-implementing it.
- **Contract evolution discipline.** The WIT contract (`crates/omnifs-wit/wit/provider.wit`, package `omnifs:provider@0.4.0`) has no negotiation story yet: a contract change strands every built provider binary. Treat WIT changes as breaking releases; batch them, call them out explicitly in PRs, and do not widen the contract for one provider's convenience.
- **Capability surface conservatism.** Anything that grants provider WASM more authority (new callout families, new preopens, process or socket effects) changes the security model and needs explicit sign-off, not incidental inclusion.
- **Consumer universality.** Features are judged against every consumer of the mount (the bash-tool compatibility list below), not against one calling pattern. A change that helps one client by breaking `tar` or `find` is wrong.

## Supported workflow

The primary contributor workflow is `omnifs dev`, implemented in `crates/omnifs-cli/src/commands/dev.rs`. It builds the dev image, synthesizes mount configs from built-in provider manifests, validates stored credentials, materializes fixtures, and launches the container directly through the Docker API.

```bash
omnifs dev          # build dev image, materialize fixtures, launch container
omnifs shell        # attach a zsh shell
omnifs logs -f      # follow container output
omnifs status       # inspect mounts, providers, auth state
omnifs down         # stop and remove the container
```

`omnifs dev` is contributor-only and requires a source checkout. It walks up from cwd looking for the workspace `Cargo.toml`, captures `gh auth token`, writes `.secrets/github_token`, downloads the Chinook SQLite fixture into `.secrets/db/test.db`, builds an image tagged `omnifs:<short-sha>-dev`, and starts the container with all built-in providers mounted under `/omnifs/<mount>`.

Do not add alternate local mount recipes unless explicitly requested.

## Build and validation

Use the explicit Rust baseline when changing host or CLI code:

```bash
cargo fmt
cargo nextest run
```

Use the repo checks when touching broad surfaces or provider code:

```bash
just check             # fmt; host workspace clippy/tests; wasm provider/tool checks; npm validate
just providers-check   # wasm32-wasip2 check/clippy for omnifs-provider-* and omnifs-tool-*
just providers-build   # release-build omnifs-provider-* and omnifs-tool-* for wasm32-wasip2
```

The root `justfile` is the human command surface. Run `just` to list grouped commands. The Bun script tree under `scripts/` is layered: thin bins at `scripts/{npm,release}.ts` parse CLI args and dispatch to `scripts/lib/` (changelog, git, npm workspace, release workflow, `Repo` helpers). CI orchestration shells live in `scripts/ci/`, with `scripts/ci/common.sh` factoring out repo-root discovery and `version_pin()` (a thin wrapper over `scripts/toolchain/versions.ts`). Toolchain bootstrap (wasi-sdk, version pin lookup) is at `scripts/toolchain/`. Keep `just` recipes as thin wrappers over Cargo, Bun, Docker, and shell scripts.

Package selection uses cargo `-p` / `--exclude` globs (`omnifs-provider-*`, `omnifs-tool-*`, `test-provider`), not a hand-maintained crate list.

Host crates such as `omnifs-cli` and `omnifs-host` build for the native target. Providers build directly as components with the Rust `wasm32-wasip2` target. `cargo build --target wasm32-wasip2` emits provider component artifacts directly; WIT bindings are generated through the SDK.

Provider clippy and test commands must include `--target wasm32-wasip2` and `-p` globs:

```bash
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider --target wasm32-wasip2 -- -D warnings
cargo test -p 'omnifs-provider-*' -p test-provider --target wasm32-wasip2 --no-run
```

WASM tests compile but cannot execute on the host because there is no WASM runtime in the test harness. Use `--no-run` for target-specific provider compilation checks. Tests that need to run should use `#[cfg(test)]` with host-target-compatible code.

For mount, provider, clone, traversal, or runtime behavior changes, do not stop at Rust-only checks. Validate through the supported runtime path:

```bash
omnifs dev -y
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -n 80 /tmp/omnifs.log'
```

For provider path-surface changes, test the whole shell traversal, not only the intended leaf paths. In the live container, run `ll`, `cd`, and `find` from the provider root through every intermediate directory. Verify that parent directories do not synthesize duplicate root entries, route scaffolding names do not bind as dynamic captures, and control directories do not contain paper/item nodes unless the design explicitly says they should.

CI builds Rust artifacts natively and uses Docker only to assemble the runtime image. Linux CLI artifacts use `cargo-zigbuild` with a GNU glibc 2.17 baseline; Darwin CLI artifacts are cross-linked from Linux through the pinned `rust-cross/cargo-zigbuild` container. Provider and tool WASM artifacts are built by `just providers-build` with WASI SDK pins from `tools/versions.toml`.

`Dockerfile` remains the contributor image path for `omnifs dev`. Release runtime image assembly uses `scripts/ci/build-runtime-image.sh`, which stages the prebuilt Linux CLI and daemon into a small Ubuntu runtime context. Provider and tool WASM artifacts are published as GitHub Release assets and installed into the host `OMNIFS_HOME/providers`; do not make the runtime image the owner of `/root/.omnifs/providers`. Keep `omnifs dev` working when changing Docker-related files.

## Release and npm packaging

User-facing releases ship the host CLI, WASM providers, npm packages, and a GHCR runtime image. Maintainer flow lives in `RELEASING.md`. **CI on `main`** is the single factory for all build artifacts; **`release.yml`** runs after green CI via `workflow_run`, publishes the GitHub Release (`softprops/action-gh-release`), promotes `sha-*` → semver GHCR tags, and publishes npm.

**Version coupling:** for release `X.Y.Z`, npm package version, CLI `CARGO_PKG_VERSION` / `omnifs --version`, and the default runtime image tag all use the **same unprefixed semver** (`0.2.0`, not `v0.2.0`):

- npm: `@0xff-ai/omnifs@X.Y.Z` and matching `@0xff-ai/omnifs-cli-*` optional dependencies
- CLI default image: `ghcr.io/0xff-ai/omnifs:X.Y.Z` (`crates/omnifs-cli/src/session.rs`)
- Git tag / GitHub Release name: `vX.Y.Z` (conventional `v` prefix only here)
- GHCR promote publishes both `X.Y.Z` and `vX.Y.Z`; the CLI default uses the unprefixed tag

npm installs the native CLI binary only. Docker is pulled on `omnifs up`, not at `npm install`. Do not bump npm/Cargo versions independently or change the embedded image ref without running through `just release-cut` (see `RELEASING.md`).

`npm/platforms.json` is the single source of truth for platform npm packages, Rust target triples, and npm `os`/`cpu` metadata. `release.yml` reads it directly when staging the four platform packages; do not hand-maintain a second npm publishing matrix in GitHub Actions. `just npm-sync` updates package versions through `npm pkg set` so package manifests keep their existing order and formatting. The npm policy implementation lives in `scripts/lib/npm-workspace.ts` (consumed by both `scripts/npm.ts` and the release workflow). Keep it consolidated in that one module unless it grows a real second responsibility.

The bin shim at `npm/omnifs/bin/omnifs.js` and its `scripts/resolve-binary.js` helper must work entirely from files inside the `@0xff-ai/omnifs` package directory. `npm/platforms.json` lives at the workspace root and is **not** included in the published tarball; if `resolve-binary.js` ever needs that data it must be inlined (with `just npm-validate` cross-checking against `npm/platforms.json`). Same rule applies to any future runtime helper added to the published package.

### Release procedure

Releases go through `just release-cut` followed by a regular PR + merge to `main`. The release workflow fires after green CI on `main`. Prerelease versions (anything containing `-`, e.g. `0.2.0-dev.0`) are auto-detected: GitHub release is marked `prerelease=true, make_latest=false`, npm publishes with dist-tag `dev`, GHCR still gets both `X.Y.Z` and `vX.Y.Z` tags.

**Local verification before cutting** (mandatory; the publish pipeline cannot catch install-time failures):

```bash
# 1. Pack the root npm package as it would publish.
scratch="$(mktemp -d)"; cd "$scratch"
npm pack /Users/raul/W/omnifs/npm/omnifs

# 2. Install the tarball into a scratch prefix, both with and without scripts.
prefix="$scratch/prefix"; mkdir -p "$prefix"
npm install --ignore-scripts --prefix "$prefix" "$scratch"/0xff-ai-omnifs-*.tgz
node "$prefix/node_modules/@0xff-ai/omnifs/bin/omnifs.js" --version    # must succeed

npm install --prefix "$prefix" "$scratch"/0xff-ai-omnifs-*.tgz         # postinstall path
node "$prefix/node_modules/@0xff-ai/omnifs/bin/omnifs.js" --version    # must succeed
```

If either invocation fails (MODULE_NOT_FOUND, missing platform binary, postinstall crash), the published package will fail the same way for every user. Fix before cutting.

**End-to-end cut sequence** once the local check passes:

```bash
# On main, clean tree:
just release-cut 0.2.0-dev.0       # opens release/vX.Y.Z PR
# Watch PR CI; merge via squash + delete branch.
# Release workflow_run fires after green CI on main.
# Verify after publish:
gh release view vX.Y.Z --json isPrerelease,assets
npm view @0xff-ai/omnifs --json | jq '.["dist-tags"]'    # dev tag should point at the new version
docker buildx imagetools inspect ghcr.io/0xff-ai/omnifs:X.Y.Z
```

**Secrets and gates** that must be in place before any cut:

- `NPM_TOKEN` repository secret (Automation type, bypasses 2FA). The npm-platforms and npm-root jobs in `release.yml` pass it as `NODE_AUTH_TOKEN`. Migrate to npm Trusted Publishers (OIDC) per package after the first publish to remove the long-lived secret.
- `id-token: write` permission on the publish jobs (already set) for `--provenance`.
- The `release` label must exist on the GitHub repo; `release.ts publishReleasePr` attaches it to the cut PR and fails if missing.
- `release-cut` requires `cargo set-version`; install with `cargo install cargo-edit` if missing.

**Failure modes seen in practice** (record here when they happen so the next cut avoids them):

- *Race against main's CI*: when PR #X merges and you immediately `release-cut`, the resulting `release/vX.Y.Z` PR starts CI before main's post-merge CI has saved its caches under `refs/heads/main`. PR CI runs cold on lanes like `cli (linux-x64, darwin)`. Mitigation: wait for the prior main CI to complete before cutting, or accept one cold cycle.
- *Branch name collision*: if a non-versioned branch named `release` exists (locally or remotely), git refuses to create `release/vX.Y.Z` (`cannot lock ref ... 'refs/heads/release' exists`). Delete or rename the conflicting branch first. The PR-branch from the last release-cleanup is the usual culprit; delete remote (`gh pr merge --delete-branch` or `git push origin --delete release`) and `git remote prune origin` locally.

## Auth and cloning

Mount JSON accepts `static-token` with `scheme`, or `oauth`.

Provider auth manifests live in `omnifs.provider.json` and are embedded in WASM as `omnifs.provider-metadata.v1`. Host and CLI derive runtime `AuthManifest` through `ProviderManifest::wasm_auth_manifest()`.

`omnifs-auth` is the OAuth protocol client: `OAuthClient`, device flow, loopback flow, and manual flow. It is not mount auth config, credential storage, or manifest parsing. Its `lib.rs` is mostly re-exports; implementation lives in `client.rs` and `request.rs`.

For the contributor sandbox, `omnifs dev` captures `gh auth token` and exposes it as a read-only mounted secret file at `/run/secrets/github_token` inside the container.

For the normal user path, `omnifs init` plus `omnifs up`, OAuth and static-token credentials live in the file-backed host credential store at `~/.omnifs/credentials.json`.

Git clone currently uses SSH:

- remote format: `git@github.com:<owner>/<repo>.git`
- auth comes from forwarded `SSH_AUTH_SOCK`
- do not mount host private keys into the container

### Runtime trust boundary

Treat the omnifs host runtime as trusted: the host CLI, `omnifsd`, and its Docker container are one trusted control plane for local execution. Do not design credential or layout boundaries around hiding `~/.omnifs` from the container when the runtime needs that state; the container already runs trusted host code and receives host authority such as the SSH agent, selected secrets, preopens, and sometimes Docker access.

The untrusted boundary is provider code. Providers are `wasm32-wasip2` components and must remain constrained by WASI preopens, host-mediated callouts, capability checks, and mount-specific auth materialization. Sharing runtime-home state with the trusted daemon is acceptable; exposing additional filesystem authority directly to provider WASM is not.

### Mount loading and resolved mounts

- Mount config types (`Spec`, `Resolved`, `Catalog`, builtins) live in `omnifs_mount::mounts` (shared by the CLI and the daemon).
- `omnifs_mount::mounts::Spec` is raw user-authored mount JSON.
- `omnifs_mount::mounts::Resolved` is the runtime-ready mount after provider metadata/defaults have been applied.
- Provider-contract types (`ProviderManifest`, manifest parsing, `AuthScheme`) live in `omnifs_provider`.
- Use `resolve_mount_spec(spec, require_metadata)` for strict load versus best-effort delete/reset paths.
- `CredentialTarget` and runtime payload materialization operate on `Resolved`, not raw mount JSON plus a late `apply_metadata` pass.
- Host-managed credentials require `provider_id`, always on `Resolved`, plus `auth.scheme` and optional `auth.account`.
- Static mounts may still use `token_env` or `token_file` for external user-managed secrets. Do not move host-managed credential state into mount JSON.

### Credentials store

- The production credential backend is the file store at `~/.omnifs/credentials.json`.
- `oauth2` 5.0.0 still binds its `reqwest` integration to `reqwest` 0.12. Keep `omnifs-auth` on a direct `reqwest` 0.12 dependency and use the `reqwest-oauth2` alias in `omnifs-host` for OAuth refresh clients until `oauth2` supports `reqwest` 0.13. The workspace `reqwest` dependency is for normal host/CLI HTTP clients and may be newer.
- The file store is intentionally the local runtime credential contract. The trusted daemon reads and writes the same `credentials.json` file through the writable `OMNIFS_HOME` bind, giving omnifs a known path, enumerable keys, atomic writes, and private Unix permissions without depending on a desktop secret service in containers or headless environments.
- `CredentialKey::storage_key()` is the only public wire form: `provider:scheme:account`.
- `CredentialEntry.value` is private; callers use `access_token()`.

Container startup requires:

- host `gh auth token` works so `omnifs dev` can capture and write `.secrets/github_token`
- host `SSH_AUTH_SOCK` is set
- host SSH agent has a usable GitHub key loaded

Useful host checks:

```bash
test -s .secrets/github_token || gh auth token > .secrets/github_token
ssh-add -L
ssh -T git@github.com
```

Useful container checks:

```bash
cat /tmp/omnifs.log
ssh -F /dev/null -T git@github.com
```

## Runtime debugging

- Runtime log file is `/tmp/omnifs.log` inside the container.
- `omnifs logs` or `docker logs omnifs` shows stdout/stderr from the entrypoint. Runtime FUSE traces still go to `/tmp/omnifs.log` inside the container.
- Clone failures should surface in the runtime log with `git clone` stderr.
- FUSE `access(...)` warnings are expected noise unless they correlate with a real failure.
- Use `omnifs status` inside the container for fast mount/config/provider/cache triage.
- Do not assume `docker exec` inherits the entrypoint environment. Verify live runtime paths instead of inferring them from defaults.

When a repo path returns `Input/output error`, check:

1. `omnifs logs` or `docker logs omnifs`
2. SSH auth inside the container
3. whether the mount is still present in `/proc/mounts`

When debugging hangs or slow paths, start with user-visible probes before theory:

1. `cd /github/<owner>`
2. `cat /dns/@google/<domain>/MX`
3. `tail -n 80 /tmp/omnifs.log`

The runtime image uses Ubuntu 25.10 and `zsh`. Interactive shells should have `ls` aliased to `ls --color=auto` and `ll` aliased to `ls -lrt`. If changing shell behavior, prefer putting it in the image rather than generating per-session shell config.

## Provider architecture

Each provider is a WASM component implementing the `omnifs:provider` WIT interface. **Before writing or modifying a provider, read `skills/omnifs-provider-sdk/SKILL.md`** (the operational guide with the flavour decision table), `providers/DESIGN.md` (the flavour doctrine with per-provider rationale), and the crate-level rustdocs in `crates/omnifs-sdk/src/lib.rs`.

A provider is one `#[omnifs_sdk::provider(..)]` impl block containing optional `type Config`/`type State` aliases and a synchronous `fn start` that registers routes **imperatively** on a `Router<State>` (there are no per-route attribute macros). The registration verbs:

- `r.dir(t).handler(h)`: a directory route family (one handler serves lookup, list, and read-file intents via `DirCx`)
- `r.file(t).handler(h)`: a file route family
- `r.treeref(t).handler(h)`: a subtree-handoff route returning a `TreeRef` the host resolves to a bind-mounted tree
- `r.object::<O>(t, |o| ..)` / `r.file_object::<O>(t, |o| ..)`: attach an Object `O` (object-shaped providers); the key's `load` supplies the canonical payload, and the block declares representations, projected leaves (`o.file("title").project(..)`, `.lazy()` for deferred), and dynamic children
- `r.attach(prefix, &handle)`: mount a detached `object(..)` handle under a prefix

The supporting macros are top-level only: `#[omnifs_sdk::provider(metadata = "..", resources(git, memory_mb, endpoints = [..]), events(timer(..)))]`, `#[omnifs_sdk::object(kind = "..", key = KeyType, canonical = .., parse = .., stability = ..)]`, `#[omnifs_sdk::config]`, `#[omnifs_sdk::path_captures]` (typed multi-segment keys; `Facet<T>` fields are route context excluded from identity; `Option<T>` for keys shared across routes; `#[flatten]` for key composition), and `#[derive(omnifs_sdk::Endpoint)]`. There is no `#[handlers]`, `#[dir]`, `#[file]`, `#[treeref]`, `#[bind]`, `#[mutate]`, or `#[subtree]` attribute, and no `r.bind`/`r.subtree` registration verbs.

The host browse surface is byte-level:

- `lookup_child(id, parent_path, name)`: resolves one child entry
- `list_children(id, path, cached_validator, cursor)`: lists a directory
- `read_file(id, path, content_type, cached_canonical)`: reads file bytes; `cached_canonical` is the object the host pushes for re-rendering (see Caching model)
- `open_file` / `read_chunk` / `close_file`: ranged/streamed reads

Subtree handoff folds into `lookup_child` and `list_children`: a `treeref` route returns the `lookup-result::subtree(tree-ref)` / `list-result::subtree(tree-ref)` terminal and the host resolves the handle to a bind-mounted clone directory.

Dispatch (the SDK router under `crates/omnifs-sdk/src/router/`; ADR-0001 §8 is the design rationale):

- Any registered route's literal-segment prefix is an auto-navigable directory; authors do not write no-op stub handlers for intermediate navigation nodes.
- Per-segment validators (capture parse functions) participate in match candidacy. A parse rejection falls through to the next-most-specific candidate route, not to ENOENT.
- `lookup` is the authoritative name oracle; `readdir` may be non-exhaustive. An `exhaustive` listing means "these are the names I am aware of," and `lookup(parent, name)` may resolve a name absent from the latest `readdir`.
- A directory listing merges the literal sibling routes registered at that depth with the handler's enumeration; an auto-navigable listing is `exhaustive=false` whenever a capture sibling exists at the next depth.
- The provider macro seals the router after `start`: overlapping leaf claims fail initialization.

Providers return either a terminal `op-result`, wrapped in a `provider-return` with `effects`, or suspend with a list of `callout`s (HTTP fetch, git open, fetch-blob, open-archive, read-blob). The host runs the batch and calls `resume(id, results)`. Providers' suspended futures are resumed by the SDK's async runtime; callouts are strictly request/response, and there are no fire-and-forget callouts.

Host-side mutations travel as `effects` on the terminal, not as separate callouts: `canonical` (store object bytes + path index leaves), `fs` (materialize files/dirs into the view cache), and `invalidations` (`object` / `listing`). See the Caching model section. `on-event` handlers return a normal `provider-step`; their `effects.invalidations` are applied at the response boundary. Today only `timer` events dispatch to a handler (declared via `events(timer(Duration, Self::method))`); other provider events return empty effects. There is no `preload` field and no `event-outcome` record.

Mount specs are JSON, not TOML. The host parses each mount's JSON config into `omnifs_mount::mounts::Spec`, resolves it to `omnifs_mount::mounts::Resolved`, and preserves the provider-specific `config` object as a `serde_json::Value` before re-serializing it to JSON bytes for the `initialize()` call. Providers receive the raw config payload as JSON bytes and deserialize through `serde_json::from_slice`; the SDK's `#[omnifs_sdk::config]` macro wires this up (and sets `deny_unknown_fields`, so mount JSON typos fail initialization loudly; do not loosen this).

## Caching model

The host owns all caching as plain byte storage; it never reasons about objects. There are no TTLs; entries leave by capacity eviction or by explicit invalidation (SDK `invalidate_object`, `invalidate_listing_path`, and `invalidate_listing_prefix` effects or the FUSE notifier). Providers must not add their own LRUs or time-based expiration. The full design is `docs/design/object-cache-primary.md`; read it before changing cache code. The key points:

- **Three host caches, named by role.** **Object cache**: durable primary (`object.redb`, global, mount-prefixed keys); holds canonical upstream bytes keyed by logical object id; object-shaped providers only (a provider that emits no `canonical-store` effect self-selects out). **View cache**: derived, non-durable (`view.redb`, deleted and recreated on every startup); the rendered representations/fields/dirents shell tools read; recomputes from the object cache with no upstream refetch. **Blob cache**: large binary served by handle (+ archive trees), unchanged.
- **Effects, not preload-vs-sibling.** A provider return carries `effects { canonical, fs, invalidations }`. `canonical-store { id, validator, bytes, view-leaves }` stores object bytes and teaches the host the exact full paths that map to that logical id. `fs` writes materialized files/dirs into the view cache and can carry the same id for object leaves. There is no `FileContent::with_sibling_files`, `Lookup::with_sibling_files`, `Projection::preload`, or `event-outcome`; those are gone. "Preload" is still the umbrella concept (any resource returned other than the one requested), expressed as additional `canonical-store`/`fs` effects.
- **The read path.** On a view miss the host resolves the path to a logical object id by exact map lookup (never a prefix probe) and pushes the cached canonical (`canonical-input { id, bytes, validator }`) into `read-file`; the SDK renders without an upstream call. The SDK self-checks the pushed id against its route-derived id before rendering. Identity representations (`byte-source::canonical`) live only in the object cache and are never copied into the view cache.
- **Fences.** A per-mount generation + tombstone fence rejects a write (canonical-store or a rendered view result) derived from data read before a concurrent invalidation. Object overwrite evicts the id's prior derived view leaves (version coherence); leaf invalidation cascades to the whole object via the reverse leaf set.

## Design invariants

### Bash tool compatibility

`omnifs` paths must behave like real files for the standard Linux toolbox. Every code path is judged against this list. A change that makes any of these regress is wrong.

- **Read content**: `cat`, `head`, `tail` including `-f`, `-n`, `-c`, `less`, `more`, `xxd`, `hexdump`, `od`, `file`
- **Search and traversal**: `grep` including `-r`, `rg`, `find` including `-name`, `-size`, `-type`, `fd`
- **Stat-based**: `ls` including `-l`, `-h`, `du` including `-sh`, `wc` including `-l`, `-c`, `-m`, `stat`
- **Copy and archive**: `cp`, `mv`, `tar` with `c`, `x`, `t`, `rsync`
- **Compare and hash**: `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum`
- **Inspection**: `jq`, `yq`, `xmllint`
- **Editors**: `vim`, `neovim`, `nano`. Editors that mmap, including some `code` configurations, are best-effort but should not break.

When introducing a feature, prove it does not regress these tools through the smoke harness in `tests/smoke/` or a unit test.

### File attributes

Projected files declare `Size`, `Bytes`, `ReadMode`, `Stability`, and optional version evidence through the SDK's projection API. The host wires `st_size`, FUSE flags/direct I/O, cache layers, durable version-keyed content, learned-size promotion, and post-read invalidation from those attributes.

The full design lives in `docs/design/file-attributes.md`, including enum definitions, the structural rule that `Volatile` requires `Ranged`, legal combinations, and byte-source to handler pairing. Read it before changing the projection API or adding a new file handler shape.

### Agent legibility

The projected tree's consumers include agents reading it with `ls`/`cat`/`grep`. Prefer designs where the tree explains itself: predictable naming, honest sizes, correct content types and extensions. When a directory's keying scheme is non-obvious, that is a provider schema smell to fix, not a documentation problem to paper over.

## Codebase expectations

- Keep changes small and local.
- Preserve the current architecture unless the task explicitly changes it: inode table, router, providers, caches, and clone manager.
- Do not silently change the auth model or transport model.
- If switching clone transport from SSH to HTTPS/token, call that out explicitly because it changes the operational contract.
- When a refactor touches clone, routing, or traversal behavior, compare against the pre-refactor behavior before accepting the new result.
- Preserve existing repo-tree passthrough and ownership semantics unless intentionally changing the contract.
- Providers must project all data they have already fetched. If a handler has an upstream payload in hand, emit every sibling file and child that can be derived from it instead of returning only the requested field and forcing later refetches.
- Use `hashbrown::HashMap` for provider-internal maps. It keeps provider internals predictable across WASI targets.

### CLI presentation vs schema types

- `omnifs-mount` types (`Spec`, `Resolved`, `Auth`, `AuthKind`) are wire/config truth. Do not add human-facing CLI labels or terminal formatting there.
- Provider capability display uses the schema `CapabilityEntry` enum directly. Format at use site through `crates/omnifs-cli/src/capability.rs`, with `capability_label` and `capability_value`. Do not introduce parallel CLI view-model structs for the same schema type.
- Status/JSON output helpers such as `*_to_json` are DTO serialization, not domain types. Keep them separate from schema conversions.

### Rust type conversions

Prefer `From` and `TryFrom` at type boundaries instead of `foo_to_bar` free functions when the conversion is a true one-to-one mapping.

Keep free functions when:

- orphan rules block a cross-crate impl, for example `credential_entry_from_token` from oauth2 token to `omnifs_creds::CredentialEntry`
- extra context is required, for example `io_context_into(context, err)` or `projected_file_from_projection(..., parent, name)`
- the helper is callout-specific extraction for `CalloutFuture`, meaning `fn(CalloutResult) -> Result<T>`. Do not use `TryFrom<CalloutResult>` for single-variant unwraps.
- the mapping is conditional or formatting-only, for example HTTP `status_error` with 429 and `retry-after` handling

When orphan rules block `From<A> for B`, use a local newtype in the owning crate, for example `WitHeaders(&HeaderMap)` in `omnifs-sdk/src/http.rs`, rather than a conversion helper that hides the same logic.

In `omnifs-sdk`, `Result<T>` aliases `core::result::Result<T, ProviderError>`. `TryFrom` impls must return `std::result::Result<_, ProviderError>` explicitly.

Host-only error enums may be supersets of WIT/guest types. For example, host `ExtractError` adds `SandboxTrapped` and setup failures. Do not re-export guest bindgen types as the host public error; map with `From` at the boundary.

Existing conversion hubs: `host/runtime/wit_conversions.rs`, `omnifs-sdk/file_attrs.rs`, `omnifs-sdk/browse.rs`, `host/runtime/{blob,git,archive}.rs`.

## Test quality

Prefer tests that protect behavior the project actually depends on: user-visible workflows, domain invariants, security/auth boundaries, persistence and wire-format compatibility, or regressions that would be easy to reintroduce.

Avoid adding tests whose main value is only to confirm local plumbing or library behavior, such as:

- serde/clap/url/http/std helpers working as documented
- a wrapper forwarding fields unchanged
- a builder storing the exact values just passed to it
- an in-memory fake round-tripping data without exercising caller behavior
- brittle presentation text checks for wording that is not contractual

Small unit tests are fine when they guard a non-obvious rule, an edge case with real product meaning, or a boundary that is hard to exercise elsewhere. Before adding a narrow test, be able to say what regression it would catch and why that regression matters.

Providers do not carry in-crate `#[cfg(test)]` modules; verify provider behavior through host-driven integration tests and the live runtime path.

## Design judgment

- Prefer the simpler end-to-end flow, not the purer local abstraction.
- Bias toward single-phase designs over multi-phase orchestration on the hot path.
- Keep data near the point where it is naturally produced and immediately consumed; split it into a second mechanism only when that separation buys something concrete.
- Do not defend abstraction boundaries that add complexity in the common case.
- Once the direct path exists, remove bridge-style dispatch layers and other transitional glue instead of letting them harden into architecture.

## Protocol and contract guardrails

- Reuse source-of-truth terms. Do not invent new names for public surfaces unless the rename is explicit.
- Keep public contracts at the right layer. Host internals must not leak into SDK/WIT naming or semantics.
- Do not reuse an existing abstraction if it changes the behavior model. Semantic fit matters more than code reuse.
- For protocol changes, write the exact interaction trace first and reject extra hops on hot paths.
- If something is conceptually one-way, stop before making it `await`-shaped. Fix the boundary instead of forcing it through request/response machinery.
- WIT changes are flag-day breaking for every built provider until a versioning/negotiation story exists; batch them and call them out explicitly.

## Mutation protocol

Mutations are not implemented yet. The read model stays read-only permanently; writes, when they land, are explicit and reviewable, never implicit side effects of writing to projected files.

If adding them, prefer:

- read model remains read-only
- drafts live under a draft namespace
- execution is triggered by an explicit, atomic act (moving a prepared transaction directory into a control namespace, or a git-shaped commit/push flow), producing an auditable record that can be reverted

Do not make projected issue/PR files directly writable as an implicit mutation mechanism.

## Known follow-ups

- Incomplete fuse module split, `attrs.rs` and `trace.rs`, was reverted. Revisit only if `fuse.rs` growth becomes a maintenance problem.
- Non-timer provider events (`webhook-received`, `file-changed`, `auth-refreshed`) are currently swallowed with empty effects in the provider macro; surface them before building on event-driven freshness.
