# Catalog and mount-spec boundaries

Status: proposal. Targets `origin/main`. Refactor only; no new product behavior.

## Problem

Three types share the provider/spec domain and none of them owns one thing:

- `omnifs_mount::mounts::Catalog` (`crates/omnifs-mount/src/mounts/mod.rs:186`) is three things fused: a **provider index** (`get/list/latest_by_name/store`, each re-reading `providers/index.json` from disk with **no cache** at `mod.rs:352,363,376`), a **spec resolver** (`resolve_spec`, `apply_metadata(_and_needs)`, `auth_manifest_for`, `provider_path`), and a **spec-path reader** (`spec_path/spec_paths/load_spec/resolve_by_name`). Its two constructors — `new(mounts_dir, providers_dir)` vs `for_providers(providers_dir)` with an empty `mounts_dir` — bake the split identity into the API.
- `omnifs-cli ProviderCatalog` (`crates/omnifs-cli/src/catalog.rs:18`) is a thin facade over `mounts::Catalog` (built `for_providers` only) that delegates resolve/auth/path, adds `provider_templates()`, and leaks the inner catalog via `inner()`.
- `omnifs-cli ProviderTemplates` (`catalog.rs:126`) is the authoring/selection view: `name -> {reference, manifest, auth_manifest}` from the latest installed artifact.

Spec ownership is diffused, not owned:

- **Reads happen in two parallel pipelines.** CLI `Workspace::mounts()` (`workspace.rs:71`) scans `mounts_dir` (`spec_paths_in`) and parses each file into `Vec<MountConfig>`, rebuilt every command. The host `ReconcilePass` (`crates/omnifs-host/src/registry.rs:429,617,627,737`) independently scans `spec_paths_in`, `Spec::from_file`s each, then `materialize` + `into_resolved`. Same scan->load->resolve logic, written twice.
- **Writes happen in three CLI sites, three idioms, one atomic.** `init/mount_file.rs:41` (`to_string_pretty` + `fs::write`), `dev/mod.rs:404` (`to_vec_pretty` + `fs::write`), `upgrade.rs:163` (rewrite via temp + `rename`, the only atomic one). The host never writes specs.
- `Catalog::resolve_by_name` and `Catalog::resolve(path)` have **zero references** (dead).

## What "resolving" is

Resolving is a pure, read-only **join of a spec against the provider index**. Nothing about it persists, and it is recomputed on every call (no cache).

Input: a raw `Spec` (`mounts/mod.rs:26`) — pinned `provider: ProviderRef` (id + meta), `mount` name, optional `auth`/`config`/`capabilities`/`root_mount`.

Steps (`Spec::apply_provider_metadata`, `mounts/mod.rs:78`):
1. Look up the pinned artifact by `spec.provider.id` in the provider index; read its embedded manifest.
2. Fill manifest **defaults into unset fields only**: the default auth scheme (when `auth` is empty) and default `config` values (when `config` is unset). Capabilities are never filled (the spec owns grants explicitly); identity is never touched (`provider.meta.name` is the source).
3. Attach `provider_name` (the slug) -> `Resolved { spec, provider_name }`.

Two depths exist:
- `resolve_spec` / `into_resolved` — hydrate + name. Used by CLI display/auth/credential-target paths.
- `materialize` / `apply_metadata_and_needs` (`materialize.rs:171`) — hydrate **and** extract capability `needs` (the oracle for the required-capabilities check) + parsed `config_schema` (names the host-resource config fields a dynamic grant resolves from) + compute preopen binds. Used by CLI container binds **and** host reconcile.

`require_metadata` toggles strict (artifact must exist; serving/auth) vs best-effort (skip hydration if the artifact is gone; delete/reset/ls display).

Who initiates it:
- CLI, per command, uncached: `status`/`mounts ls` (`mounts.rs:71`, `mount_report.rs` scans), `mounts rm`/`reset` (`mounts.rs:107`, `workspace.rs:98` — to compute the credential target), `auth *` (`auth/mount.rs:125`), `up`/`dev` (`launch.rs:119` `DockerMountMaterializer`), `init` picker (`catalog.rs:162` `configured_mounts`).
- Host/daemon, on reconcile (daemon startup + every `/v1/reconcile`, which the CLI triggers after writing specs): `ReconcilePass` scans, loads, materializes, and updates the in-memory `ProviderRegistry`.

The structural fact that drives the design: **resolve is a join called from almost everywhere, and it belongs to neither "providers only" nor "specs only".**

## Target model

In `omnifs-provider` (it already owns the provider domain — manifests, metadata, validation, and depends only on `omnifs-core`/`omnifs-caps`):

- **`provider::Catalog`** — providers only. The content-addressed `ProviderStore` (today misfiled in `omnifs-mount/src/mounts/store.rs`; nothing in it touches mounts, and all its imports already resolve in `omnifs-provider`) moves here with it. Serves `get/list/latest_by_name/installable`; each `Provider` yields `manifest()` and a derived `auth_manifest()`. Whether it caches the parsed index in memory is an open call — see "Cache".

In `omnifs-mount`:

- **`mount::Registry`** — the sole owner of specs. Reads all specs from `mounts_dir` once into memory; serves reads; owns all writes (atomic) and reloads. Replaces `Workspace::mounts()`, the three scattered writers, and the host's `spec_paths_in` + `Spec::from_file`. Built on the existing host reconcile machinery (see "Basing on the host registry").
- **Resolution is an explicit join**, a free function owned by neither catalog: `resolve(&provider::Catalog, &Spec, require_metadata)` plus the deeper `materialize(&provider::Catalog, Spec, mode)`. Callers that hold both (CLI commands, host reconcile) call it. No `Resolver` builder — one function, one bool.

In `omnifs-cli`:

- **Spec authoring is the CLI's alone.** The CLI reads a `Provider` from `provider::Catalog`, composes a finished `Spec` (pin reference, fill defaults via the pure `Spec::apply_provider_metadata`, gather input), and hands it to `mount::Registry::put`. `apply_provider_metadata` stays a pure transform (it takes a manifest) the CLI drives at author time and `resolve` reuses at hydrate time.
- `ProviderCatalog` (the facade) and `ProviderTemplates` both dissolve into `provider::Catalog` — see "ProviderTemplates is gone".

### Responsibility map

| Concern | Today | Target |
|---|---|---|
| Provider index + content store | `mounts::Catalog` + `mounts::store` (re-read per call), in `omnifs-mount` | `provider::Catalog` + `ProviderStore`, **in `omnifs-provider`** |
| Provider lookup `get/list/latest_by_name` | `mounts::Catalog` | `provider::Catalog` |
| Spec read (scan + parse) | `Workspace::mounts()` AND host `ReconcilePass` | `mount::Registry` (one owner, both consume) |
| Spec write (persist) | 3 CLI sites, 3 idioms | `mount::Registry::put/remove` (atomic, one idiom) |
| Resolve (spec + manifest -> Resolved) | `mounts::Catalog::resolve_spec` + `Resolver` builder | free `resolve(&provider::Catalog, &Spec, bool)` |
| Materialize (needs + binds) | `materialize(&Catalog, ...)` | `materialize(&provider::Catalog, ...)` |
| Spec authoring (compose new spec) | CLI + `Spec::apply_provider_metadata` (mount-side) | CLI composes, calls the pure transform, hands to Registry |
| Authoring/selection view | `ProviderTemplates` (separate type + struct) | gone — `provider::Catalog::installable()` + `Provider::manifest()/auth_manifest()` |

## ProviderTemplates is gone

`ProviderTemplates` (`catalog.rs:126`) is "the latest installed artifact per provider name, bundled with its manifest and auth manifest" — and the latest-per-name logic already exists twice (`catalog.rs:72` and `Catalog::latest_by_name`). Once `provider::Catalog` holds the index, that view is just two accessors on it:

- `templates.by_id(name)` -> `catalog.latest_by_name(name)`, then `Provider::manifest()` / `auth_manifest()` on demand.
- `templates.iter()` / `ids()` / `is_empty()` -> `catalog.installable()` (latest per name).
- `configured_mounts(...)` -> a 3-line CLI helper intersecting `catalog.installable()` names with `registry.iter()` specs (no catalog method needed).

`ProviderTemplate` (the `{reference, manifest, auth_manifest}` struct) disappears: `reference` is `Provider::reference()`, `manifest` is `Provider::manifest()`, `auth_manifest` is `manifest.wasm_auth_manifest()`. Manifests stay lazily read (one read per displayed provider, same as today) — no eager bundling.

## Cache

The user's model says `provider::Catalog` keeps the index in memory. That cache is the only part of this design that *adds* complexity: it forces a `reload()` after every install (`init`/`setup`/`dev` install then resolve in one command) and introduces staleness between the CLI's and daemon's copies. `index.json` is a few KB; today's per-call `read_index` is not a measured bottleneck for either the one-shot CLI or the daemon. Lazy default: **keep reading `index.json` per call, skip the cache and the `reload()` machinery**. Add the in-memory index only when a profiler shows the daemon's index reads are hot — at which point `reload()` lands at the install sites named in Risks.

## The resolve-join decision (the crux)

Because resolve needs both catalogs, putting it on either one re-creates today's straddle. Pick the free-function form:

```rust
// omnifs-mount::resolve  (owns neither catalog's state)
pub fn resolve(
    providers: &provider::Catalog,
    spec: &Spec,
    require_metadata: bool,
) -> Result<Resolved, Error>;
```

`materialize` keeps the same shape it has today, just renamed parameter type:

```rust
pub fn materialize(
    providers: &provider::Catalog,
    spec: Spec,
    mode: MaterializationMode,
) -> Result<MaterializedMount, MaterializeError>;
```

Drop the `Resolver<'a>` builder (`mounts/mod.rs:437`) — it carries a single `require_metadata` bool, which is just an argument. Neither catalog gains a resolve method. The CLI command or host reconcile pass is the place that holds a `&provider::Catalog` and a `&Spec` and joins them — which is also where pinning already forces the two to meet.

## API sketches

```rust
// omnifs-provider, module `provider`: providers only. ProviderStore moves here too.
pub struct Catalog {
    providers_dir: PathBuf,
    // ponytail: no in-memory `index` field by default — read index.json per call (it is tiny).
    // Add a cached `index: Index` + `reload()` only when the daemon's reads are measured hot.
}

impl Catalog {
    pub fn open(providers_dir: impl AsRef<Path>) -> Self;
    pub fn get(&self, id: &ProviderId) -> Result<Option<Provider>, StoreError>;
    pub fn latest_by_name(&self, name: &ProviderName) -> Result<Option<Provider>, StoreError>;
    pub fn list(&self) -> Result<Vec<Provider>, StoreError>;
    pub fn installable(&self) -> Result<Vec<Provider>, StoreError>; // latest per name (was ProviderTemplates)
}
// Provider::manifest() / auth_manifest() lazily read the by-hash wasm; ProviderStore is the on-disk format.
```

```rust
// omnifs-mount, module `mount`: the sole spec owner.
pub struct Registry {
    mounts_dir: PathBuf,
    specs: BTreeMap<mount::Name, Spec>, // all mounts/*.json, held in memory
}

impl Registry {
    pub fn load(mounts_dir: impl AsRef<Path>) -> Result<Self, Error>; // scan + parse every spec
    pub fn get(&self, name: &mount::Name) -> Option<&Spec>;
    pub fn iter(&self) -> impl Iterator<Item = (&mount::Name, &Spec)> + '_;
    pub fn put(&mut self, spec: Spec) -> Result<(), Error>;     // persist atomically (temp + rename) + update cache
    pub fn remove(&mut self, name: &mount::Name) -> Result<bool, Error>;
    pub fn reload(&mut self) -> Result<(), Error>;             // re-scan disk (daemon reconcile)
}
```

```rust
// CLI authoring: compose, then hand to the registry. The CLI is the only spec author.
let provider = providers.latest_by_name("github")?.ok_or(NotInstalled)?; // provider::Catalog
let mut spec = Spec { provider: provider.reference(), mount: name, ..Spec::bare() };
spec.apply_provider_metadata(&provider.manifest()?)?; // pure transform, author time
registry.put(spec)?;                                  // single owner persists atomically
```

## Two-process coherence

The CLI and the daemon are separate runtimes. A `mount::Registry` is a **per-process in-memory mirror of the on-disk spec directory**, not a shared singleton. The disk stays the source of truth:

- The CLI mutates specs through its `Registry` (which persists), then calls `/v1/reconcile`.
- The daemon's `Registry` learns of the change via `reload()` inside reconcile, then the reconcile pass diffs desired specs against built mounts.

"All reads and writes go through the Registry" holds within a process; cross-process coherence is the reconcile API, unchanged. The same applies to `provider::Catalog`: its in-memory index must `reload()` after an install (today's per-call read masks this; caching makes the reload explicit — see risks).

## Basing on the host registry

`crates/omnifs-host/src/registry.rs` already maintains the daemon's in-memory world: `ProviderRegistry` (built mounts), `ReconcilePass` (desired -> built diff), `mount_fingerprint`, failures. Two of its pieces generalize into `mount::Registry`:

- The desired-state read (`spec_paths_in` + `Spec::from_file`, `registry.rs:429,617`) -> `Registry::load`/`reload`.
- The built-vs-desired diff stays host-side in `ProviderRegistry` (it is runtime-shaped: live providers, fingerprints), consuming `Registry` for the desired set and `provider::Catalog` + `resolve`/`materialize` for hydration.

So `mount::Registry` is the desired-state owner extracted out of `registry.rs`; `ProviderRegistry` remains the running-state owner that consumes it.

## Refactor in slices

Each slice compiles and passes tests on its own; behavior is preserved until the naming/cache changes in slices 4-5.

1. **Make the join explicit.** Add `resolve(&Catalog, &Spec, require_metadata)` as a free function wrapping today's `Resolver`. Repoint `ProviderCatalog::resolve_mount_spec` and the host to it. No type moves yet. Delete the dead `resolve_by_name` / `resolve(path)`.
2. **Introduce `mount::Registry` (read side).** `load/get/iter/reload` over `mounts_dir`. Repoint `Workspace::mounts()` and the host `spec_paths_in` + `Spec::from_file` to it. Both pipelines now share one reader.
3. **Move writes into the Registry.** `put/remove`, atomic (temp + rename). Repoint `init/mount_file`, `dev::write_dev_mounts`, `upgrade::apply_additive_upgrade`. One serialization idiom; init/dev gain atomicity for free.
4. **Move + split the provider catalog.** Move `ProviderStore` and the provider-index half of `Catalog` into `omnifs-provider` as `provider::Catalog`; drop `mounts_dir`, the spec-path methods (now in `Registry`), and the `new`/`for_providers` duality; add `installable()`. Rename `materialize`'s parameter type. No in-memory cache (see Cache).
5. **Dissolve the CLI facade and `ProviderTemplates`.** Delete both `ProviderCatalog` and `ProviderTemplates`; consumers call `provider::Catalog::installable()`/`latest_by_name()` and `Provider::manifest()` directly, and `configured_mounts` becomes a small CLI helper. Host reconcile consumes `Registry` + `provider::Catalog` + `resolve` directly.

## Risks and open questions

- **Cache is deferred** (see "Cache"). The lazy default has no in-memory index, so there is no reload or staleness to manage. If it is added later, the install -> resolve sequences (`init`, `setup`, `dev`) are where `reload()` lands.
- **Write/read race + locking.** `ProviderStore` already uses a `LOCK_FILE`; specs do not. With one writer (the CLI Registry) and one reader (the daemon, via reconcile) coordinating through disk, decide whether `Registry::put` needs the same discipline.
- **`Resolved` stays a mount-crate type.** It is the join product (a data type), not owned by either catalog; that is fine.
- **`materialize` is the third join consumer.** Its signature churns (parameter type rename only). Keep CLI container-binds and host reconcile both fed.
- **`MountConfig` (CLI, `session.rs`).** Today the CLI's in-memory spec form is `Vec<MountConfig>`. Decide whether `Registry` returns `Spec` (and `MountConfig` becomes a thin CLI wrapper) or absorbs `MountConfig`'s source-path bookkeeping.
